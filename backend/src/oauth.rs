//! OAuth management discovery surface.
//!
//! Plexus discovers these descriptors dynamically from its pinned pi-ai
//! provider registry. Trefoil has no pi-ai dependency, so this list mirrors
//! the reference registry at the pinned Plexus revision and must be refreshed
//! when that dependency's OAuth provider set changes.

use crate::{
    auth::{resolve_principal, Principal},
    AppState,
};
use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json, Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::{distributions::Alphanumeric, Rng};
use serde::Deserialize;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};

const SUPPORTED_PROVIDERS: &[&str] = &[
    "anthropic",
    "github-copilot",
    "kimi-coding",
    "openai-codex",
    "openrouter",
    "xai",
];

#[derive(Debug, Clone, Copy)]
struct OAuthCapabilities {
    callback: bool,
    manual_code: bool,
}

fn capabilities(provider: &str) -> Option<OAuthCapabilities> {
    match provider {
        "anthropic" | "openai-codex" => Some(OAuthCapabilities {
            callback: true,
            manual_code: false,
        }),
        "github-copilot" | "kimi-coding" | "openrouter" | "xai" => Some(OAuthCapabilities {
            callback: false,
            manual_code: true,
        }),
        _ => None,
    }
}

pub(crate) fn is_supported_provider(provider: &str) -> bool {
    SUPPORTED_PROVIDERS.contains(&provider)
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/v0/management/oauth/providers",
            axum::routing::get(providers),
        )
        .route(
            "/v0/management/oauth/credentials/status",
            axum::routing::get(credential_status),
        )
        .route(
            "/v0/management/oauth/credentials",
            axum::routing::delete(delete_credentials),
        )
        .route(
            "/v0/management/oauth/sessions/:id",
            axum::routing::get(get_session),
        )
        .route(
            "/v0/management/oauth/sessions",
            axum::routing::post(create_session),
        )
        .route(
            "/v0/management/oauth/sessions/:id/cancel",
            axum::routing::post(cancel_session),
        )
        .route(
            "/v0/management/oauth/sessions/:id/prompt",
            axum::routing::post(prompt_session),
        )
        .route(
            "/v0/management/oauth/sessions/:id/manual-code",
            axum::routing::post(manual_code),
        )
        .route(
            "/v0/management/oauth/sessions/:id/callback",
            axum::routing::get(callback),
        )
        .route("/v0/management/oauth/models", axum::routing::get(models))
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthSession {
    pub id: String,
    pub provider_id: String,
    pub account_id: String,
    pub status: String,
    pub auth_info: Option<serde_json::Value>,
    pub prompt: Option<serde_json::Value>,
    pub progress: Vec<serde_json::Value>,
    pub error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub expires_at: i64,
    #[serde(skip)]
    pub pkce_verifier: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateSession {
    provider_id: String,
    account_id: String,
}

#[derive(Debug, Deserialize)]
struct SessionInput {
    value: String,
}

#[derive(Debug, Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}
fn session_response(session: OAuthSession) -> Response {
    Json(serde_json::json!({"data": session})).into_response()
}
fn session_id() -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(32)
        .map(char::from)
        .collect()
}

fn openrouter_authorization_url(origin: &str, session_id: &str, verifier: &str) -> Option<String> {
    let mut callback = url::Url::parse(origin).ok()?;
    if !matches!(callback.scheme(), "http" | "https") || callback.host_str().is_none() || !callback.username().is_empty() || callback.password().is_some() { return None; }
    callback.set_path(&format!("/v0/management/oauth/sessions/{session_id}/callback"));
    callback.set_query(None);
    callback.query_pairs_mut().append_pair("state", session_id);
    callback.set_fragment(None);
    let mut authorize = url::Url::parse("https://openrouter.ai/auth").ok()?;
    authorize.query_pairs_mut().append_pair("callback_url", callback.as_str())
        .append_pair("code_challenge", &URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())))
        .append_pair("code_challenge_method", "S256");
    Some(authorize.into())
}

fn authorization_url(provider: &str, session_id: &str, verifier: &str) -> Option<String> {
    let endpoint = match provider {
        "anthropic" => "https://claude.ai/oauth/authorize",
        "github-copilot" => "https://github.com/login/oauth/authorize",
        "kimi-coding" => "https://auth.kimi.com/oauth/authorize",
        "openai-codex" => "https://auth.openai.com/oauth/authorize",
        "openrouter" => "https://openrouter.ai/auth",
        "xai" => "https://accounts.x.ai/oauth/authorize",
        _ => return None,
    };
    if provider == "openrouter" {
        let origin = std::env::var("TREFOIL_PUBLIC_URL").unwrap_or_else(|_| format!("http://127.0.0.1:{}", std::env::var("PORT").unwrap_or_else(|_| "8080".into())));
        return openrouter_authorization_url(&origin, session_id, verifier);
    }
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let mut url = url::Url::parse(endpoint).ok()?;
    let redirect = format!("/v0/management/oauth/sessions/{session_id}/callback");
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", crate::oauth_transport::provider_spec(provider)?.client_id)
        .append_pair("redirect_uri", &redirect)
        .append_pair("state", session_id)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256");
    if provider == "anthropic" {
        url.query_pairs_mut().append_pair("code", "true").append_pair("scope", "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload");
    }
    Some(url.to_string())
}

async fn start_copilot_session(state: AppState, account: String) -> Response {
    let Some(pool)=state.inner.database.as_ref() else {
        return (StatusCode::SERVICE_UNAVAILABLE,Json(serde_json::json!({"error":"PostgreSQL is required for device login"}))).into_response();
    };
    if crate::credential_crypto::CredentialKey::from_env().is_err() {
        return (StatusCode::SERVICE_UNAVAILABLE,Json(serde_json::json!({"error":"OAuth credential encryption is not configured"}))).into_response();
    }
    let device=match crate::copilot_oauth::start().await {Ok(device)=>device,Err(message)=>return (StatusCode::BAD_GATEWAY,Json(serde_json::json!({"error":message}))).into_response()};
    let now=now_ms();let id=session_id();
    let session=OAuthSession{id:id.clone(),provider_id:"github-copilot".into(),account_id:account,status:"waiting".into(),auth_info:Some(serde_json::json!({"url":device.verification_uri,"userCode":device.user_code,"instructions":format!("Enter code {} at the verification URL.",device.user_code)})),prompt:None,progress:vec![],error:None,created_at:now,updated_at:now,expires_at:now.saturating_add(device.expires_in.as_millis() as i64),pkce_verifier:None};
    if crate::database::upsert_oauth_session(pool,&session).await.is_err(){return (StatusCode::INTERNAL_SERVER_ERROR,Json(serde_json::json!({"error":"Unable to persist device session"}))).into_response();}
    state.inner.oauth_sessions.write().unwrap().insert(id.clone(),session.clone());
    let account_id=session.account_id.clone();
    tokio::spawn(async move {
        let mut interval=device.interval;
        loop {
            if !state.inner.oauth_sessions.read().unwrap().get(&id).is_some_and(|s|s.status=="waiting"&&s.expires_at>now_ms()){break;}
            tokio::time::sleep(interval).await;
            if !state.inner.oauth_sessions.read().unwrap().get(&id).is_some_and(|s|s.status=="waiting"&&s.expires_at>now_ms()){break;}
            match crate::copilot_oauth::poll(&device.secret,interval).await {
                Ok(crate::copilot_oauth::Poll::Pending)=>{},
                Ok(crate::copilot_oauth::Poll::SlowDown(next))=>interval=next,
                Ok(crate::copilot_oauth::Poll::Complete(github_token))=>{
                    let Some(pool)=state.inner.database.as_ref() else {break};
                    if !matches!(crate::database::claim_oauth_exchange(pool,&id).await,Ok(true)){break;}
                    if let Some(current)=state.inner.oauth_sessions.write().unwrap().get_mut(&id){if current.status!="waiting"{break;}current.status="exchanging".into();}
                    match crate::copilot_oauth::exchange(&github_token).await {
                        Ok((access,expires))=>{
                            if matches!(crate::database::complete_oauth_exchange(pool,&id,"github-copilot",&account_id,&access,&github_token,expires).await,Ok(true)) {
                                let _=update_session(&state,&id,"manual-code",Some("complete".into())).await;
                            }
                        }
                        Err(message)=>{let _=oauth_failed(&state,&id,&message,StatusCode::BAD_GATEWAY).await;}
                    }
                    break;
                }
                Err(message)=>{let _=oauth_failed(&state,&id,&message,StatusCode::BAD_GATEWAY).await;break;}
            }
        }
        let expired={let mut sessions=state.inner.oauth_sessions.write().unwrap();sessions.get_mut(&id).and_then(|session| {
            if session.status=="waiting" && session.expires_at<=now_ms() {session.status="expired".into();session.auth_info=None;session.error=Some("Device authorization expired".into());Some(session.clone())} else {None}
        })};
        if let (Some(session),Some(pool))=(expired,state.inner.database.as_ref()){let _=crate::database::upsert_oauth_session(pool,&session).await;}
    });
    session_response(session)
}

async fn create_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateSession>,
) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    if body.provider_id.trim().is_empty() || body.account_id.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":"providerId and accountId are required"})),
        )
            .into_response();
    }
    if !is_supported_provider(&body.provider_id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":"Unsupported OAuth provider"})),
        )
            .into_response();
    }
    if body.provider_id == "github-copilot" { return start_copilot_session(state,body.account_id).await; }
    let now = now_ms();
    let id = session_id();
    let verifier: String = rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(64)
        .map(char::from)
        .collect();
    let Some(auth_url) = authorization_url(&body.provider_id, &id, &verifier) else {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error":"Invalid OAuth public callback URL configuration"}))).into_response();
    };
    let session = OAuthSession {
        id: id.clone(),
        provider_id: body.provider_id.clone(),
        account_id: body.account_id,
        status: "waiting".into(),
        auth_info: Some(
            serde_json::json!({"url": auth_url, "instructions":"Open the authorization URL to continue."}),
        ),
        prompt: None,
        progress: vec![serde_json::json!({"status":"waiting"})],
        error: None,
        created_at: now,
        updated_at: now,
        expires_at: now + 20 * 60 * 1000,
        pkce_verifier: Some(verifier.clone()),
    };
    state
        .inner
        .oauth_sessions
        .write()
        .unwrap()
        .insert(id, session.clone());
    state
        .inner
        .oauth_pkce_verifiers
        .write()
        .unwrap()
        .insert(session.id.clone(), verifier);
    if let Some(pool) = &state.inner.database {
        if crate::database::upsert_oauth_session(pool, &session)
            .await
            .is_err()
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error":"Unable to persist OAuth session"})),
            )
                .into_response();
        }
    }
    session_response(session)
}

fn claim_exchange(state: &AppState, id: &str) -> Result<(OAuthSession, String), Response> {
    let mut sessions = state.inner.oauth_sessions.write().unwrap();
    let Some(session) = sessions.get_mut(id) else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error":"OAuth session not found"})),
        )
            .into_response());
    };
    if session.status != "waiting" || session.expires_at <= now_ms() {
        return Err((
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error":"OAuth session is no longer active"})),
        )
            .into_response());
    }
    let verifier = state
        .inner
        .oauth_pkce_verifiers
        .write()
        .unwrap()
        .remove(id)
        .or_else(|| session.pkce_verifier.take());
    let Some(verifier) = verifier else {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":"OAuth verifier unavailable"})),
        )
            .into_response());
    };
    session.pkce_verifier = None;
    session.status = "exchanging".into();
    session.updated_at = now_ms();
    Ok((session.clone(), verifier))
}

async fn callback(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<CallbackQuery>,
) -> Response {
    // The state parameter binds the browser redirect to the server-side session.
    if query.state.as_deref() != Some(id.as_str()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":"Invalid OAuth state"})),
        )
            .into_response();
    }
    if let Some(error) = query.error {
        return oauth_failed(
            &state,
            &id,
            &format!("Authorization was denied: {error}"),
            StatusCode::BAD_REQUEST,
        )
        .await;
    }
    let Some(code) = query.code.filter(|code| !code.trim().is_empty()) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":"Authorization code is required"})),
        )
            .into_response();
    };
    let session = match state.inner.oauth_sessions.read().unwrap().get(&id).cloned() {
        Some(session) if session.expires_at > now_ms() && session.status == "waiting" => session,
        Some(_) => {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({"error":"OAuth session is no longer active"})),
            )
                .into_response()
        }
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error":"OAuth session not found"})),
            )
                .into_response()
        }
    };
    if session.provider_id != "openrouter" && !capabilities(&session.provider_id).is_some_and(|caps| caps.callback) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":"OAuth callback flow is not supported by this provider"})),
        )
            .into_response();
    }
    let (session, verifier) = match claim_exchange(&state, &id) {
        Ok(claim) => claim,
        Err(response) => return response,
    };
    // Claim durable state before contacting the provider. Only one process
    // may exchange a session, even if several loaded the same waiting snapshot.
    if let Some(pool) = &state.inner.database {
        match crate::database::claim_oauth_exchange(pool, &id).await {
            Ok(true) => {},
            Ok(false) => return (StatusCode::CONFLICT, Json(serde_json::json!({"error":"OAuth session is no longer active"}))).into_response(),
            Err(_) => return oauth_failed(&state, &id, "Unable to persist OAuth session", StatusCode::INTERNAL_SERVER_ERROR).await,
        }
    }
    let redirect = format!("/v0/management/oauth/sessions/{id}/callback");
    let transport = crate::oauth_transport::ReqwestTransport;
    let token = match crate::oauth_transport::exchange_with_state(
        &transport,
        &session.provider_id,
        &code,
        &verifier,
        &redirect,
        Some(&id),
    )
    .await
    {
        Ok(token) => token,
        Err(crate::oauth_transport::OAuthTransportError::Http { .. }) => {
            return oauth_failed(
                &state,
                &id,
                "OAuth token exchange rejected",
                StatusCode::BAD_GATEWAY,
            )
            .await;
        }
        Err(_) => {
            return oauth_failed(
                &state,
                &id,
                "OAuth token request failed",
                StatusCode::BAD_GATEWAY,
            )
            .await
        }
    };
    if !state.inner.oauth_sessions.read().unwrap().get(&id)
        .is_some_and(|current| current.status == "exchanging" && current.expires_at > now_ms()) {
        return (StatusCode::CONFLICT, Json(serde_json::json!({"error":"OAuth session is no longer active"}))).into_response();
    }
    if state.inner.database.is_none() {
        return oauth_failed(&state, &id, "PostgreSQL is required to persist OAuth credentials", StatusCode::SERVICE_UNAVAILABLE).await;
    }
    let access = token.access_token;
    let refresh = token.refresh_token;
    let expires = if session.provider_id == "openrouter" && token.expires_in == 0 {
        i64::MAX
    } else {
        now_ms().saturating_add(token.expires_in.saturating_mul(1000))
    };
    if let Some(pool) = &state.inner.database {
        match crate::database::complete_oauth_exchange(pool, &id, &session.provider_id, &session.account_id, &access, &refresh, expires).await {
            Ok(true) => {},
            Ok(false) => return (StatusCode::CONFLICT, Json(serde_json::json!({"error":"OAuth session is no longer active"}))).into_response(),
            Err(_) => return oauth_failed(&state, &id, "Unable to persist OAuth credentials", StatusCode::INTERNAL_SERVER_ERROR).await,
        }
    }
    state
        .inner
        .oauth_pkce_verifiers
        .write()
        .unwrap()
        .remove(&id);
    if let Some(current) = state.inner.oauth_sessions.write().unwrap().get_mut(&id) {
        current.pkce_verifier = None;
    }
    update_session(&state, &id, "manual-code", Some("complete".into())).await
}

async fn oauth_failed(state: &AppState, id: &str, message: &str, status: StatusCode) -> Response {
    let _ = update_session(state, id, "fail", Some(message.to_string())).await;
    (status, Json(serde_json::json!({"error":message}))).into_response()
}

async fn get_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    match state.inner.oauth_sessions.read().unwrap().get(&id).cloned() {
        Some(session) => session_response(session),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error":"OAuth session not found"})),
        )
            .into_response(),
    }
}

async fn update_session(
    state: &AppState,
    id: &str,
    action: &str,
    value: Option<String>,
) -> Response {
    let snapshot = {
        let mut sessions = state.inner.oauth_sessions.write().unwrap();
        let Some(session) = sessions.get_mut(id) else {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error":"OAuth session not found"})),
            )
                .into_response();
        };
        if session.expires_at <= now_ms()
            && !matches!(
                session.status.as_str(),
                "complete" | "cancelled" | "failed" | "expired"
            )
        {
            session.status = "expired".into();
        }
        if matches!(
            session.status.as_str(),
            "complete" | "cancelled" | "failed" | "expired"
        ) {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({"error":"OAuth session is no longer active"})),
            )
                .into_response();
        }
        match action {
            "cancel" => {
                session.status = "cancelled".into();
                session.pkce_verifier = None;
                session.prompt = None;
            }
            "prompt" => {
                if session.status == "exchanging" {
                    return (StatusCode::CONFLICT, Json(serde_json::json!({"error":"OAuth exchange is already in progress"}))).into_response();
                }
                session.status = "waiting".into();
                session.prompt = None;
                session
                    .progress
                    .push(serde_json::json!({"prompt":value,"status":"waiting"}));
            }
            "manual-code" => {
                session.status = "complete".into();
                session.auth_info = None;
                session
                    .progress
                    .push(serde_json::json!({"status":"complete"}));
            }
            "fail" => {
                session.status = "failed".into();
                session.prompt = None;
                session.error = value;
                session.progress.push(serde_json::json!({
                    "status": "failed",
                    "error": session.error.clone()
                }));
            }
            _ => unreachable!(),
        }
        session.updated_at = now_ms();
        session.clone()
    };
    if let Some(pool) = &state.inner.database {
        if crate::database::upsert_oauth_session(pool, &snapshot)
            .await
            .is_err()
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error":"Unable to persist OAuth session"})),
            )
                .into_response();
        }
    }
    session_response(snapshot)
}

async fn cancel_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    let response = update_session(&state, &id, "cancel", None).await;
    if response.status().is_success() {
        // A cancelled flow can never reach its callback. Do not retain the
        // one-shot verifier until the session's normal expiry cleanup.
        state
            .inner
            .oauth_pkce_verifiers
            .write()
            .unwrap()
            .remove(&id);
    }
    response
}
async fn prompt_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<SessionInput>,
) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    if body.value.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":"value is required"})),
        )
            .into_response();
    }
    update_session(&state, &id, "prompt", Some(body.value)).await
}
async fn manual_code(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<SessionInput>,
) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    if body.value.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":"value is required"})),
        )
            .into_response();
    }
    let supported = state
        .inner
        .oauth_sessions
        .read()
        .unwrap()
        .get(&id)
        .and_then(|session| capabilities(&session.provider_id))
        .is_some_and(|caps| caps.manual_code);
    if !supported {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":"OAuth manual-code flow is not supported by this provider"})),
        )
            .into_response();
    }
    let provider = state.inner.oauth_sessions.read().unwrap().get(&id).map(|s|s.provider_id.clone());
    if provider.as_deref() == Some("openrouter") {
        let code = match url::Url::parse(body.value.trim()) {
            Ok(url) => url.query_pairs().find(|(key,_)|key == "code").map(|(_,value)|value.into_owned()),
            Err(_) => Some(body.value.trim().to_owned()),
        };
        return callback(State(state), Path(id.clone()), Query(CallbackQuery { code, state:Some(id), error:None })).await;
    }
    (StatusCode::NOT_IMPLEMENTED, Json(serde_json::json!({"error":"Manual provider code exchange is not implemented; no credentials were saved"}))).into_response()
}

async fn models(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<CredentialStatusQuery>,
) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    if query.provider_id.as_deref().is_none_or(str::is_empty) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":"providerId is required"})),
        )
            .into_response();
    }
    Json(serde_json::json!({"data": [], "source":"catalog"})).into_response()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CredentialStatusQuery {
    provider_id: Option<String>,
    account_id: Option<String>,
}

#[allow(clippy::result_large_err)]
fn require_admin(state: &AppState, headers: &HeaderMap) -> Result<(), Response> {
    match resolve_principal(
        state,
        headers.get("x-admin-key").and_then(|value| value.to_str().ok()),
    ) {
        Some(Principal::Admin) => Ok(()),
        Some(Principal::Limited { .. }) => Err((
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": { "message": "Admin privileges required", "type": "forbidden", "code": 403 }
            })),
        )
            .into_response()),
        None => Err((
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "error": { "message": "Unauthorized", "type": "auth_error", "code": 401 }
            })),
        )
            .into_response()),
    }
}

async fn providers(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    let data = serde_json::json!([
        { "id": "anthropic", "name": "Anthropic (Claude Pro/Max)", "usesCallbackServer": true },
        { "id": "github-copilot", "name": "GitHub Copilot", "usesCallbackServer": false },
        { "id": "kimi-coding", "name": "Kimi Code (subscription)", "usesCallbackServer": false },
        { "id": "openai-codex", "name": "OpenAI (ChatGPT Plus/Pro)", "usesCallbackServer": true },
        { "id": "openrouter", "name": "OpenRouter OAuth", "usesCallbackServer": false },
        { "id": "xai", "name": "xAI (Grok/X subscription)", "usesCallbackServer": false },
    ]);
    Json(serde_json::json!({ "data": data, "total": 6 })).into_response()
}

async fn credential_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<CredentialStatusQuery>,
) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    let mut details = Vec::new();
    for (field, value) in [
        ("providerId", query.provider_id.as_deref()),
        ("accountId", query.account_id.as_deref()),
    ] {
        if value.is_none_or(str::is_empty) {
            details.push(query_issue(field, value.is_some()));
        }
    }
    if !details.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "Invalid query parameters", "details": details })),
        )
            .into_response();
    }
    let provider_id = query.provider_id.unwrap();
    let account_id = query.account_id.unwrap();
    let ready = if let Some(pool) = &state.inner.database {
        sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM oauth_credentials WHERE oauth_provider_type = $1 AND account_id = $2)",
        )
        .bind(provider_id)
        .bind(account_id)
        .fetch_one(pool)
        .await
        .unwrap_or(false)
    } else {
        false
    };
    Json(serde_json::json!({ "data": { "ready": ready } })).into_response()
}

fn query_issue(field: &str, present: bool) -> serde_json::Value {
    if present {
        serde_json::json!({
            "origin": "string", "code": "too_small", "minimum": 1,
            "inclusive": true, "path": [field],
            "message": "Too small: expected string to have >=1 characters"
        })
    } else {
        serde_json::json!({
            "expected": "string", "code": "invalid_type", "path": [field],
            "message": "Invalid input: expected string, received undefined"
        })
    }
}

async fn delete_credentials(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    let object = body.as_object();
    let mut details = Vec::new();
    for field in ["providerId", "accountId"] {
        match object.and_then(|object| object.get(field)) {
            Some(serde_json::Value::String(value)) if !value.is_empty() => {}
            Some(serde_json::Value::String(_)) => details.push(serde_json::json!({
                "origin": "string", "code": "too_small", "minimum": 1,
                "inclusive": true, "path": [field],
                "message": "Too small: expected string to have >=1 characters"
            })),
            Some(value) => details.push(serde_json::json!({
                "expected": "string", "code": "invalid_type", "path": [field],
                "message": format!("Invalid input: expected string, received {}", json_type(value))
            })),
            None => details.push(query_issue(field, false)),
        }
    }
    if !details.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "Invalid request body", "details": details })),
        )
            .into_response();
    }
    let object = object.unwrap();
    let deleted = if let Some(pool) = &state.inner.database {
        sqlx::query(
            "DELETE FROM oauth_credentials WHERE oauth_provider_type = $1 AND account_id = $2",
        )
        .bind(object["providerId"].as_str().unwrap())
        .bind(object["accountId"].as_str().unwrap())
        .execute(pool)
        .await
        .map(|result| result.rows_affected() > 0)
        .unwrap_or(false)
    } else {
        false
    };
    if !deleted {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "OAuth credentials not found" })),
        )
            .into_response();
    }
    Json(serde_json::json!({ "data": { "deleted": true } })).into_response()
}

fn json_type(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::KeyConfig;
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use std::collections::HashMap;
    use tower::ServiceExt;

    #[test]
    fn pkce_verifier_is_persisted_but_not_serialized() {
        let session = OAuthSession {
            id: "session".into(),
            provider_id: "anthropic".into(),
            account_id: "default".into(),
            status: "waiting".into(),
            auth_info: None,
            prompt: None,
            progress: vec![],
            error: None,
            created_at: 1,
            updated_at: 1,
            expires_at: 2,
            pkce_verifier: Some("deterministic-verifier".into()),
        };
        let json = serde_json::to_value(&session).unwrap();
        assert_eq!(
            session.pkce_verifier.as_deref(),
            Some("deterministic-verifier")
        );
        assert!(json.get("pkceVerifier").is_none());
    }

    #[test]
    fn openrouter_authorization_uses_callback_url_and_pkce_without_fake_client_id() {
        let url = url::Url::parse(&openrouter_authorization_url("https://gateway.example/base?discard=yes", "session", "verifier").unwrap()).unwrap();
        let query: HashMap<_,_> = url.query_pairs().into_owned().collect();
        assert_eq!(query["callback_url"], "https://gateway.example/v0/management/oauth/sessions/session/callback?state=session");
        assert_eq!(query["code_challenge_method"], "S256");
        assert!(!query.contains_key("client_id"));
        assert!(!query.contains_key("redirect_uri"));
        assert!(openrouter_authorization_url("file:///private", "session", "verifier").is_none());
    }

    #[test]
    fn exchange_claim_is_atomic_and_consumes_persisted_verifier() {
        let state = fixture();
        state.inner.oauth_sessions.write().unwrap().insert(
            "claim".into(),
            OAuthSession {
                id: "claim".into(),
                provider_id: "openrouter".into(),
                account_id: "default".into(),
                status: "waiting".into(),
                auth_info: None,
                prompt: None,
                progress: vec![],
                error: None,
                created_at: now_ms(),
                updated_at: now_ms(),
                expires_at: now_ms() + 60000,
                pkce_verifier: Some("verifier".into()),
            },
        );
        let (session, verifier) = claim_exchange(&state, "claim").unwrap();
        assert_eq!(verifier, "verifier");
        assert_eq!(session.status, "exchanging");
        assert!(session.pkce_verifier.is_none());
        assert_eq!(
            claim_exchange(&state, "claim").unwrap_err().status(),
            StatusCode::CONFLICT
        );
    }

    #[test]
    fn callback_claim_removes_in_memory_verifier() {
        let state = fixture();
        state
            .inner
            .oauth_pkce_verifiers
            .write()
            .unwrap()
            .insert("session".into(), "deterministic-verifier".into());
        assert_eq!(
            state
                .inner
                .oauth_pkce_verifiers
                .write()
                .unwrap()
                .remove("session")
                .as_deref(),
            Some("deterministic-verifier")
        );
        assert!(state
            .inner
            .oauth_pkce_verifiers
            .read()
            .unwrap()
            .get("session")
            .is_none());
    }

    fn fixture() -> AppState {
        let mut keys = HashMap::new();
        keys.insert(
            "limited".into(),
            KeyConfig {
                secret: "secret".into(),
                ..Default::default()
            },
        );
        AppState::new(Some("admin".into()), keys)
    }

    #[tokio::test]
    async fn lists_pinned_oauth_provider_descriptors() {
        let response = router()
            .with_state(fixture())
            .oneshot(
                Request::get("/v0/management/oauth/providers")
                    .header("x-admin-key", "admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(body["total"], 6);
        assert_eq!(body["data"][0]["id"], "anthropic");
        assert_eq!(body["data"][3]["usesCallbackServer"], true);
        assert!(body["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|provider| { provider.get("capabilities").is_none() }));
        assert!(!body["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["id"] == "radius"));
    }

    #[tokio::test]
    async fn oauth_provider_discovery_is_admin_only() {
        for (credential, expected) in [
            (Some("secret"), StatusCode::FORBIDDEN),
            (None, StatusCode::UNAUTHORIZED),
        ] {
            let mut request = Request::get("/v0/management/oauth/providers");
            if let Some(value) = credential {
                request = request.header("x-admin-key", value);
            }
            let response = router()
                .with_state(fixture())
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), expected);
        }
    }

    #[tokio::test]
    async fn credential_status_defaults_false_and_validates_queries() {
        let response = router()
            .with_state(fixture())
            .oneshot(
                Request::get("/v0/management/oauth/credentials/status?providerId=anthropic&accountId=default")
                    .header("x-admin-key", "admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(body, serde_json::json!({ "data": { "ready": false } }));

        let invalid = router()
            .with_state(fixture())
            .oneshot(
                Request::get("/v0/management/oauth/credentials/status")
                    .header("x-admin-key", "admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
        let body: serde_json::Value =
            serde_json::from_slice(&invalid.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(body["details"].as_array().unwrap().len(), 2);
        assert_eq!(
            body["details"][0]["path"],
            serde_json::json!(["providerId"])
        );
    }

    #[tokio::test]
    async fn delete_credentials_validates_body_and_returns_not_found() {
        let missing = router()
            .with_state(fixture())
            .oneshot(
                Request::delete("/v0/management/oauth/credentials")
                    .header("x-admin-key", "admin")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"providerId":"anthropic","accountId":"missing"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);

        let invalid = router()
            .with_state(fixture())
            .oneshot(
                Request::delete("/v0/management/oauth/credentials")
                    .header("x-admin-key", "admin")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
        let body: serde_json::Value =
            serde_json::from_slice(&invalid.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(body["details"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn cancelling_session_cleans_up_pkce_verifier_and_is_terminal() {
        let state = fixture();
        let session = OAuthSession {
            id: "cancel-me".into(),
            provider_id: "anthropic".into(),
            account_id: "default".into(),
            status: "waiting".into(),
            auth_info: None,
            prompt: None,
            progress: vec![],
            error: None,
            created_at: now_ms(),
            updated_at: now_ms(),
            expires_at: now_ms() + 60_000,
            pkce_verifier: Some("one-shot".into()),
        };
        state
            .inner
            .oauth_sessions
            .write()
            .unwrap()
            .insert(session.id.clone(), session);
        state
            .inner
            .oauth_pkce_verifiers
            .write()
            .unwrap()
            .insert("cancel-me".into(), "one-shot".into());

        let response = router()
            .with_state(state.clone())
            .oneshot(
                Request::post("/v0/management/oauth/sessions/cancel-me/cancel")
                    .header("x-admin-key", "admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            state
                .inner
                .oauth_sessions
                .read()
                .unwrap()
                .get("cancel-me")
                .unwrap()
                .status,
            "cancelled"
        );
        assert!(!state
            .inner
            .oauth_pkce_verifiers
            .read()
            .unwrap()
            .contains_key("cancel-me"));

        let callback = router()
            .with_state(state.clone())
            .oneshot(
                Request::get(
                    "/v0/management/oauth/sessions/cancel-me/callback?state=cancel-me&code=late-code",
                )
                .body(Body::empty())
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(callback.status(), StatusCode::CONFLICT);

        let repeated = router()
            .with_state(state)
            .oneshot(
                Request::post("/v0/management/oauth/sessions/cancel-me/cancel")
                    .header("x-admin-key", "admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(repeated.status(), StatusCode::CONFLICT);
    }
}
