//! OAuth metadata endpoints for MCP clients.

use crate::AppState;
use axum::extract::Form;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::{
    extract::{Path, Query, State},
    response::{IntoResponse, Response},
    Json, Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::{distributions::Alphanumeric, Rng};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use sqlx::Row;

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/.well-known/oauth-authorization-server",
            axum::routing::get(authorization_server),
        )
        .route(
            "/.well-known/oauth-protected-resource/mcp/:name",
            axum::routing::get(protected_resource),
        )
        .route("/oauth/register", axum::routing::post(register_client))
        .route("/oauth/token", axum::routing::post(token))
        .route(
            "/oauth/authorize",
            axum::routing::get(authorize).post(authorize),
        )
}

async fn authorization_server(State(_state): State<AppState>, _headers: HeaderMap) -> Response {
    Json(serde_json::json!({
        "issuer":"/",
        "authorization_endpoint":"/oauth/authorize",
        "token_endpoint":"/oauth/token",
        "registration_endpoint":"/oauth/register",
        "response_types_supported":["code"],
        "grant_types_supported":["authorization_code","refresh_token"],
        "code_challenge_methods_supported":["S256"]
    }))
    .into_response()
}

async fn protected_resource(State(_state): State<AppState>, Path(name): Path<String>) -> Response {
    Json(serde_json::json!({"resource":format!("/mcp/{name}"),"authorization_servers":["/"]}))
        .into_response()
}

#[derive(Deserialize)]
struct RegistrationRequest {
    client_name: Option<String>,
    redirect_uris: Vec<String>,
    grant_types: Option<Vec<String>>,
    response_types: Option<Vec<String>>,
    scope: Option<String>,
    token_endpoint_auth_method: Option<String>,
}

async fn register_client(
    State(state): State<AppState>,
    Json(request): Json<RegistrationRequest>,
) -> Response {
    if request.redirect_uris.is_empty()
        || request.redirect_uris.iter().any(|uri| {
            uri.trim().is_empty()
                || url::Url::parse(uri)
                    .map(|parsed| parsed.host_str().is_none())
                    .unwrap_or(true)
        })
    {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error":"redirect_uris must contain at least one non-empty URI"}))).into_response();
    }
    let Some(pool) = &state.inner.database else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error":"PostgreSQL is required"})),
        )
            .into_response();
    };
    let client_id = format!(
        "trefoil-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let now = chrono::Utc::now().timestamp_millis();
    let grants = request
        .grant_types
        .unwrap_or_else(|| vec!["authorization_code".into()]);
    let responses = request
        .response_types
        .unwrap_or_else(|| vec!["code".into()]);
    let result = sqlx::query("INSERT INTO mcp_oauth_clients (client_id,client_name,redirect_uris,grant_types,response_types,scope,token_endpoint_auth_method,status,created_at,updated_at) VALUES ($1,$2,$3,$4,$5,$6,$7,'active',$8,$8)").bind(&client_id).bind(&request.client_name).bind(serde_json::to_string(&request.redirect_uris).unwrap()).bind(serde_json::to_string(&grants).unwrap()).bind(serde_json::to_string(&responses).unwrap()).bind(&request.scope).bind(request.token_endpoint_auth_method.as_deref().unwrap_or("none")).bind(now).execute(pool).await;
    match result { Ok(_) => (StatusCode::CREATED, Json(serde_json::json!({"client_id":client_id,"client_id_issued_at":now / 1000,"client_name":request.client_name,"redirect_uris":request.redirect_uris,"grant_types":grants,"response_types":responses,"token_endpoint_auth_method":"none"}))).into_response(), Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error":"Unable to register OAuth client"}))).into_response() }
}

#[derive(Deserialize)]
struct TokenRequest {
    grant_type: Option<String>,
    code: Option<String>,
    client_id: Option<String>,
    redirect_uri: Option<String>,
    code_verifier: Option<String>,
    _refresh_token: Option<String>,
}

async fn token(State(state): State<AppState>, Form(request): Form<TokenRequest>) -> Response {
    let Some(pool) = &state.inner.database else {
        return oauth_error("server_error", "PostgreSQL is required");
    };
    if request.grant_type.as_deref() == Some("refresh_token") {
        return oauth_error(
            "unsupported_grant_type",
            "Refresh token rotation is not implemented yet",
        );
    }
    if request.grant_type.as_deref() != Some("authorization_code") {
        return oauth_error(
            "unsupported_grant_type",
            "grant_type must be authorization_code",
        );
    }
    let (Some(code), Some(client_id), Some(redirect_uri), Some(verifier)) = (
        request.code,
        request.client_id,
        request.redirect_uri,
        request.code_verifier,
    ) else {
        return oauth_error(
            "invalid_request",
            "code, client_id, redirect_uri, and code_verifier are required",
        );
    };
    let code_hash = hex::encode(Sha256::digest(code.as_bytes()));
    let row = sqlx::query("SELECT code,client_id,redirect_uri,resource,scope,key_name,code_challenge,code_challenge_method,expires_at,consumed_at FROM mcp_oauth_authorization_codes WHERE code_hash=$1")
        .bind(code_hash).fetch_optional(pool).await;
    let Ok(Some(row)) = row else {
        return oauth_error("invalid_grant", "Authorization code is invalid");
    };
    let valid = row.try_get::<String, _>("code").ok().as_deref() == Some(code.as_str())
        && row.try_get::<String, _>("client_id").ok().as_deref() == Some(client_id.as_str())
        && row.try_get::<String, _>("redirect_uri").ok().as_deref() == Some(redirect_uri.as_str())
        && row.try_get::<i64, _>("expires_at").unwrap_or(0) > chrono::Utc::now().timestamp_millis()
        && row
            .try_get::<Option<i64>, _>("consumed_at")
            .unwrap_or(Some(0))
            .is_none();
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let valid = valid
        && row
            .try_get::<String, _>("code_challenge")
            .unwrap_or_default()
            == challenge
        && row
            .try_get::<String, _>("code_challenge_method")
            .unwrap_or_default()
            == "S256";
    if !valid {
        return oauth_error("invalid_grant", "Authorization code is invalid");
    }
    let now = chrono::Utc::now().timestamp_millis();
    if sqlx::query("UPDATE mcp_oauth_authorization_codes SET consumed_at=$1 WHERE code=$2 AND consumed_at IS NULL").bind(now).bind(&code).execute(pool).await.map(|r|r.rows_affected()).unwrap_or(0) != 1 { return oauth_error("invalid_grant", "Authorization code was already consumed"); }
    let access = random_token();
    let refresh = random_token();
    let access_expiry = now + 3_600_000;
    let refresh_expiry = now + 30 * 86_400_000;
    let access_hash = hex::encode(Sha256::digest(access.as_bytes()));
    let refresh_hash = hex::encode(Sha256::digest(refresh.as_bytes()));
    let result = sqlx::query("INSERT INTO mcp_oauth_tokens (access_token_hash,access_token,refresh_token_hash,refresh_token,client_id,key_name,resource,scope,access_token_expires_at,refresh_token_expires_at,created_at,updated_at) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$11)")
        .bind(access_hash).bind(&access).bind(refresh_hash).bind(&refresh).bind(client_id).bind(row.try_get::<String,_>("key_name").unwrap_or_default()).bind(row.try_get::<String,_>("resource").unwrap_or_default()).bind(row.try_get::<Option<String>,_>("scope").unwrap_or(None)).bind(access_expiry).bind(refresh_expiry).bind(now).execute(pool).await;
    if result.is_err() {
        return oauth_error("server_error", "Unable to issue OAuth token");
    }
    Json(serde_json::json!({"access_token":access,"refresh_token":refresh,"token_type":"Bearer","expires_in":3600,"scope":row.try_get::<Option<String>,_>("scope").unwrap_or(None)})).into_response()
}

#[derive(Deserialize, Clone)]
struct AuthorizationRequest {
    response_type: Option<String>,
    client_id: Option<String>,
    redirect_uri: Option<String>,
    state: Option<String>,
    scope: Option<String>,
    code_challenge: Option<String>,
    code_challenge_method: Option<String>,
    resource: Option<String>,
    key_name: Option<String>,
}

async fn authorize(
    State(state): State<AppState>,
    Query(request): Query<AuthorizationRequest>,
    headers: HeaderMap,
) -> Response {
    let Some(pool) = &state.inner.database else {
        return oauth_error_status(
            "server_error",
            "PostgreSQL is required",
            StatusCode::INTERNAL_SERVER_ERROR,
        );
    };
    let (Some(client_id), Some(redirect_uri), Some(challenge), Some(resource)) = (
        request.client_id.as_deref(),
        request.redirect_uri.as_deref(),
        request.code_challenge.as_deref(),
        request.resource.as_deref(),
    ) else {
        return oauth_error(
            "invalid_request",
            "client_id, redirect_uri, code_challenge, and resource are required",
        );
    };
    if request.response_type.as_deref() != Some("code")
        || request.code_challenge_method.as_deref() != Some("S256")
        || url::Url::parse(resource).is_err()
    {
        return oauth_error(
            "invalid_request",
            "response_type=code and S256 PKCE are required",
        );
    }
    let Ok(Some(client)) =
        sqlx::query("SELECT redirect_uris,status,scope FROM mcp_oauth_clients WHERE client_id=$1")
            .bind(client_id)
            .fetch_optional(pool)
            .await
    else {
        return oauth_error("invalid_request", "Unknown OAuth client");
    };
    if client.try_get::<String, _>("status").unwrap_or_default() != "active" {
        return oauth_error("unauthorized_client", "OAuth client is disabled");
    }
    let redirects: Vec<String> = serde_json::from_str(
        &client
            .try_get::<String, _>("redirect_uris")
            .unwrap_or_default(),
    )
    .unwrap_or_default();
    if !redirects.iter().any(|uri| uri == redirect_uri) {
        return oauth_error("invalid_request", "redirect_uri is not registered");
    }
    let allowed = client.try_get::<Option<String>, _>("scope").unwrap_or(None);
    if allowed.as_ref().is_some_and(|a| {
        request
            .scope
            .as_deref()
            .unwrap_or("")
            .split_whitespace()
            .any(|s| !a.split_whitespace().any(|x| x == s))
    }) {
        return oauth_error("invalid_scope", "Requested scope is not allowed");
    }
    if headers
        .get("accept")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("text/html"))
    {
        return (StatusCode::OK, [("content-type", "text/html; charset=utf-8")], "<h1>Authorize access</h1><p>Submit this request with an authenticated x-admin-key and Accept: application/json.</p>").into_response();
    }
    use crate::auth::{resolve_principal, Principal};
    let key_name = match resolve_principal(
        &state,
        headers.get("x-admin-key").and_then(|v| v.to_str().ok()),
    ) {
        Some(Principal::Admin) => request
            .key_name
            .filter(|n| state.inner.keys.read().unwrap().contains_key(n)),
        Some(Principal::Limited { key_name, .. }) => Some(key_name),
        None => None,
    };
    let Some(key_name) = key_name else {
        return oauth_error(
            "access_denied",
            "An authenticated API-key session and key_name are required",
        );
    };
    let code = random_token();
    let now = chrono::Utc::now().timestamp_millis();
    let hash = hex::encode(Sha256::digest(code.as_bytes()));
    let result = sqlx::query("INSERT INTO mcp_oauth_authorization_codes (code_hash,code,client_id,redirect_uri,resource,scope,key_name,code_challenge,code_challenge_method,expires_at,created_at) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,'S256',$9,$10)").bind(hash).bind(&code).bind(client_id).bind(redirect_uri).bind(resource).bind(&request.scope).bind(key_name).bind(challenge).bind(now + 600_000).bind(now).execute(pool).await;
    if result.is_err() {
        return oauth_error_status(
            "server_error",
            "Unable to store authorization code",
            StatusCode::INTERNAL_SERVER_ERROR,
        );
    }
    let mut callback = url::Url::parse(redirect_uri).unwrap();
    callback.query_pairs_mut().append_pair("code", &code);
    if let Some(state) = request.state {
        callback.query_pairs_mut().append_pair("state", &state);
    }
    let location = callback.to_string();
    if headers
        .get("accept")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("application/json"))
    {
        return Json(serde_json::json!({"redirect_to": location})).into_response();
    }
    (
        StatusCode::FOUND,
        [(
            axum::http::header::LOCATION,
            HeaderValue::from_str(&location).unwrap(),
        )],
    )
        .into_response()
}

fn oauth_error(error: &str, description: &str) -> Response {
    oauth_error_status(error, description, StatusCode::BAD_REQUEST)
}
fn oauth_error_status(error: &str, description: &str, status: StatusCode) -> Response {
    (
        status,
        Json(serde_json::json!({"error":error,"error_description":description})),
    )
        .into_response()
}
fn random_token() -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(48)
        .map(char::from)
        .collect()
}
