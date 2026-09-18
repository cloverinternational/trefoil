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

fn token_endpoint(provider: &str) -> Option<&'static str> {
    match provider {
        "anthropic" => Some("https://console.anthropic.com/v1/oauth/token"),
        "github-copilot" => Some("https://github.com/login/oauth/access_token"),
        "kimi-coding" => Some("https://auth.kimi.com/oauth/token"),
        "openai-codex" => Some("https://auth.openai.com/oauth/token"),
        "openrouter" => Some("https://openrouter.ai/api/v1/auth/keys"),
        "xai" => Some("https://accounts.x.ai/oauth/token"),
        _ => None,
    }
}

fn authorization_url(provider: &str, session_id: &str, verifier: &str) -> Option<String> {
    let endpoint = match provider {
        "anthropic" => "https://console.anthropic.com/oauth/authorize",
        "github-copilot" => "https://github.com/login/oauth/authorize",
        "kimi-coding" => "https://auth.kimi.com/oauth/authorize",
        "openai-codex" => "https://auth.openai.com/oauth/authorize",
        "openrouter" => "https://openrouter.ai/auth",
        "xai" => "https://accounts.x.ai/oauth/authorize",
        _ => return None,
    };
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let mut url = url::Url::parse(endpoint).ok()?;
    let redirect = format!("/v0/management/oauth/sessions/{session_id}/callback");
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", "trefoil")
        .append_pair("redirect_uri", &redirect)
        .append_pair("state", session_id)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256");
    Some(url.to_string())
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
    let now = now_ms();
    let id = session_id();
    let verifier: String = rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(64)
        .map(char::from)
        .collect();
    let auth_url = authorization_url(&body.provider_id, &id, &verifier).unwrap();
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
    if !capabilities(&session.provider_id).is_some_and(|caps| caps.callback) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":"OAuth callback flow is not supported by this provider"})),
        )
            .into_response();
    }
    let verifier = match state
        .inner
        .oauth_pkce_verifiers
        .write()
        .unwrap()
        .remove(&id)
    {
        Some(verifier) => verifier,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error":"OAuth verifier unavailable"})),
            )
                .into_response()
        }
    };
    let Some(endpoint) = token_endpoint(&session.provider_id) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":"Unsupported OAuth provider"})),
        )
            .into_response();
    };
    let redirect = format!("/v0/management/oauth/sessions/{id}/callback");
    let result = reqwest::Client::new()
        .post(endpoint)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("client_id", "trefoil"),
            ("redirect_uri", redirect.as_str()),
            ("code_verifier", verifier.as_str()),
        ])
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await;
    let response = match result {
        Ok(response) => response,
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
    let status = response.status();
    let payload = response
        .json::<serde_json::Value>()
        .await
        .unwrap_or_default();
    if !status.is_success() {
        return oauth_failed(
            &state,
            &id,
            "OAuth token exchange rejected",
            StatusCode::BAD_GATEWAY,
        )
        .await;
    }
    let Some(access) = payload.get("access_token").and_then(|v| v.as_str()) else {
        return oauth_failed(
            &state,
            &id,
            "OAuth response omitted access token",
            StatusCode::BAD_GATEWAY,
        )
        .await;
    };
    let refresh = payload
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let expires = now_ms()
        + payload
            .get("expires_in")
            .and_then(|v| v.as_i64())
            .unwrap_or(3600)
            * 1000;
    if let Some(pool) = &state.inner.database {
        if crate::database::upsert_oauth_credential(
            pool,
            &session.provider_id,
            &session.account_id,
            access,
            refresh,
            expires,
        )
        .await
        .is_err()
        {
            return oauth_failed(
                &state,
                &id,
                "Unable to persist OAuth credentials",
                StatusCode::INTERNAL_SERVER_ERROR,
            )
            .await;
        }
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
                session.prompt = None;
            }
            "prompt" => {
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
    update_session(&state, &id, "cancel", None).await
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
    update_session(&state, &id, "manual-code", Some(body.value)).await
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
}
