//! Ported contract: `/v0/management/providers/*` CRUD from Plexus
//! (`packages/backend/src/routes/management/config.ts`, the "Providers"
//! section, mounted under the admin-only scope in
//! `packages/backend/src/routes/management.ts`).
//!
//! Scoped to the load-bearing subset of `ProviderConfigSchema`
//! (`packages/backend/src/config.ts`) — see `state::ProviderConfig` for the
//! fields covered and which are deferred to a later parity slice.
//!
//! All routes require the `Admin` principal (403 for a limited key, 401 for
//! no/bad credential), same as `backend/src/keys.rs`.

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json, Router,
};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::time::Instant;

use crate::auth::{resolve_principal, Principal};
use crate::state::{
    AppState, ProviderConfig,
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/v0/management/providers",
            axum::routing::get(list_providers),
        )
        .route(
            "/v0/management/providers/*slug",
            axum::routing::get(get_provider)
                .put(put_provider)
                .patch(patch_provider)
                .delete(delete_provider),
        )
        .route(
            "/v0/management/providers/fetch-models",
            axum::routing::post(fetch_models),
        )
        .route("/v0/management/test", axum::routing::post(manual_probe))
}

#[derive(Debug, Deserialize)]
struct ProbeBody {
    provider: Option<String>,
    model: Option<String>,
    #[serde(rename = "apiType")]
    api_type: Option<String>,
}

const PROBE_API_TYPES: [&str; 7] = [
    "chat",
    "messages",
    "gemini",
    "responses",
    "embeddings",
    "images",
    "speech",
];

/// Run a small, direct request against one configured provider/model.  This
/// intentionally does not use alias resolution, failover, cooldowns, or API
/// key quotas: it is the admin UI's connectivity test for a target itself.
async fn manual_probe(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ProbeBody>,
) -> Response {
    if let Err(error) = require_admin(&state, &headers) {
        return error.into_response();
    }
    let (Some(provider_name), Some(model)) = (body.provider, body.model) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "success": false,
                "error": "Both provider and model are required"
            })),
        )
            .into_response();
    };
    if provider_name.trim().is_empty() || model.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "success": false,
                "error": "Both provider and model are required"
            })),
        )
            .into_response();
    }
    let api_type = body.api_type.unwrap_or_else(|| "chat".into());
    if api_type == "oauth" {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({
            "success": false,
            "error": "OAuth probes are not supported by the test endpoint. Use the OAuth session flow instead."
        }))).into_response();
    }
    if api_type == "transcriptions" {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({
            "success": false,
            "error": "Cannot test transcriptions API via test endpoint - requires file upload. Use the actual /v1/audio/transcriptions endpoint to test."
        }))).into_response();
    }
    if !PROBE_API_TYPES.contains(&api_type.as_str()) {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({
            "success": false,
            "error": format!("Invalid API type: {api_type}. Must be one of: {}", PROBE_API_TYPES.join(", "))
        }))).into_response();
    }

    let started = Instant::now();
    let result = probe_request(&state, &provider_name, &model, &api_type).await;
    let duration_ms = started.elapsed().as_millis() as u64;
    let mut output = serde_json::json!({
        "success": result.is_ok(), "durationMs": duration_ms, "apiType": api_type
    });
    match result {
        Ok(response) => {
            output["response"] = Value::String(probe_response_text(&response, &api_type));
        }
        Err(error) => output["error"] = Value::String(error),
    }
    Json(output).into_response()
}

async fn probe_request(
    state: &AppState,
    provider_name: &str,
    model: &str,
    api_type: &str,
) -> Result<Value, String> {
    let provider = state
        .inner
        .providers
        .read()
        .unwrap()
        .get(provider_name)
        .cloned()
        .ok_or_else(|| format!("Provider '{provider_name}' not found"))?;
    if !provider.models.is_empty() && !provider.models.contains_key(model) {
        return Err("Model is not in the provider's configured inventory".to_string());
    }
    let upstream_model = provider.models.get(model)
        .and_then(|entry| entry.upstream_id.as_deref())
        .filter(|id| !id.is_empty())
        .unwrap_or(model);
    let base = url::Url::parse(provider.api_url_for(api_type))
        .map_err(|_| "Provider API base URL is invalid".to_string())?;
    if !matches!(base.scheme(), "http" | "https") || base.host_str().is_none() {
        return Err("Provider API base URL is invalid".into());
    }
    let (path, payload) = match api_type {
        "messages" => (
            "/messages",
            serde_json::json!({"model":upstream_model,"max_tokens":100,"system":"You are a helpful assistant.","messages":[{"role":"user","content":"Just respond with the word acknowledged"}]}),
        ),
        "gemini" => (
            "/v1beta/models/{model}:generateContent",
            serde_json::json!({"contents":[{"role":"user","parts":[{"text":"Just respond with the word acknowledged"}]}],"generationConfig":{"maxOutputTokens":100}}),
        ),
        "responses" => (
            "/responses",
            serde_json::json!({"model":upstream_model,"input":"Just respond with the word acknowledged","instructions":"You are a helpful assistant."}),
        ),
        "embeddings" => (
            "/embeddings",
            serde_json::json!({"model":upstream_model,"input":["Hello world"]}),
        ),
        "images" => (
            "/images/generations",
            serde_json::json!({"model":upstream_model,"prompt":"A tiny red square","n":1}),
        ),
        "speech" => (
            "/audio/speech",
            serde_json::json!({"model":upstream_model,"input":"Hello world"}),
        ),
        _ => (
            "/chat/completions",
            serde_json::json!({"model":upstream_model,"messages":[{"role":"user","content":"Just respond with the word acknowledged"}],"max_tokens":100}),
        ),
    };
    let path = path.replace("{model}", upstream_model);
    let endpoint = format!("{}{}", base.as_str().trim_end_matches('/'), path);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())?;
    let mut request = client.post(endpoint).json(&payload);
    if let Some(key) = provider.api_key {
        if api_type == "messages" {
            request = request.header("x-api-key", key);
        } else if api_type == "gemini" {
            request = request.header("x-goog-api-key", key);
        } else {
            request = request.bearer_auth(key);
        }
    }
    let response = request.send().await.map_err(|e| {
        if e.is_timeout() {
            "Request timed out after 30 seconds".into()
        } else {
            e.to_string()
        }
    })?;
    let status = response.status();
    let payload = response.json::<Value>().await.unwrap_or(Value::Null);
    if !status.is_success() {
        return Err(payload
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("Provider request failed")
            .to_string());
    }
    Ok(payload)
}

fn probe_response_text(response: &Value, api_type: &str) -> String {
    if api_type == "images" {
        return format!(
            "Success ({} image(s) created)",
            response
                .get("data")
                .and_then(Value::as_array)
                .map_or(0, Vec::len)
        );
    }
    if api_type == "embeddings" {
        return format!(
            "Success ({} embedding(s))",
            response
                .get("data")
                .and_then(Value::as_array)
                .map_or(0, Vec::len)
        );
    }
    response
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(Value::as_str)
        .map(|s| s.chars().take(100).collect())
        .unwrap_or_else(|| "Success".into())
}

fn public_discovery_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            !(a == 0 || a == 10 || a == 127 || a >= 224 || a == 169 && b == 254
                || a == 172 && (16..=31).contains(&b) || a == 192 && b == 168
                || a == 100 && (64..=127).contains(&b) || a == 192 && b == 0 && c == 0
                || a == 198 && (b == 18 || b == 19) || a == 198 && b == 51 && c == 100
                || a == 203 && b == 0 && c == 113)
        }
        // Do not support IPv6 here until transition/tunnel address ranges are
        // reviewed end to end (e.g. 6to4 can encode a private IPv4 target).
        std::net::IpAddr::V6(_) => false,
    }
}

async fn fetch_models(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if let Err(error) = require_admin(&state, &headers) {
        return error.into_response();
    }
    let Some(url) = body.get("url").and_then(Value::as_str) else {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error":{"message":"Invalid request body","type":"validation_error","code":400}}))).into_response();
    };
    // Never forward a caller-supplied credential to a caller-supplied host.
    // Credentialed discovery needs a separately verified saved-provider flow.
    if body.get("apiKey").is_some() {
        return ApiError::Simple(StatusCode::BAD_REQUEST, "Credentialed model discovery is unavailable".into()).into_response();
    }
    let Ok(parsed) = url::Url::parse(url) else {
        return ApiError::Simple(StatusCode::BAD_REQUEST, "Invalid URL".into()).into_response();
    };
    // Credential-free discovery is limited to a reviewed public catalog.
    // Saved-provider credential discovery requires a separate scoped design.
    if parsed.scheme() != "https" || parsed.host_str().is_none()
        || !parsed.username().is_empty() || parsed.password().is_some()
        || parsed.port().is_some_and(|port| port != 443)
        || parsed.query().is_some() || parsed.fragment().is_some()
        || parsed.host_str() != Some("openrouter.ai")
        || parsed.path() != "/api/v1/models"
    {
        return ApiError::Simple(StatusCode::BAD_REQUEST, "URL is not allowed".into())
            .into_response();
    }
    let host = parsed.host_str().unwrap();
    let Ok(Ok(addresses)) = tokio::time::timeout(std::time::Duration::from_secs(3), tokio::net::lookup_host((host, 443))).await else {
        return ApiError::Simple(StatusCode::BAD_GATEWAY, "Unable to resolve provider host".into()).into_response();
    };
    let addresses: Vec<_> = addresses.collect();
    let approved = addresses.iter().find(|address| public_discovery_ip(address.ip()) && address.is_ipv4()).copied();
    if approved.is_none() || addresses.iter().any(|address| address.is_ipv4() && !public_discovery_ip(address.ip())) {
        return ApiError::Simple(StatusCode::BAD_REQUEST, "URL is not allowed".into()).into_response();
    }
    let Ok(client) = reqwest::Client::builder().no_proxy().redirect(reqwest::redirect::Policy::none())
        .resolve(host, approved.unwrap()).timeout(std::time::Duration::from_secs(10)).build() else {
        return ApiError::Simple(StatusCode::BAD_GATEWAY, "Unable to configure provider request".into()).into_response();
    };
    let request = client.get(url);
    match request
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
    {
        Ok(response) if response.status().is_success() => {
            if response.content_length().is_some_and(|size| size > 2_000_000) {
                return ApiError::Simple(StatusCode::BAD_GATEWAY, "Model catalog is too large".into()).into_response();
            }
            let mut response = response;
            let mut bytes = Vec::new();
            loop {
                let chunk = match response.chunk().await {
                    Ok(Some(chunk)) => chunk,
                    Ok(None) => break,
                    Err(_) => return ApiError::Simple(StatusCode::BAD_GATEWAY, "Unable to read model catalog".into()).into_response(),
                };
                if bytes.len().saturating_add(chunk.len()) > 2_000_000 {
                    return ApiError::Simple(StatusCode::BAD_GATEWAY, "Model catalog is too large".into()).into_response();
                }
                bytes.extend_from_slice(&chunk);
            }
            if bytes.is_empty() {
                return ApiError::Simple(StatusCode::BAD_GATEWAY, "Unable to read model catalog".into()).into_response();
            }
            let Ok(payload) = serde_json::from_slice::<Value>(&bytes) else {
                return ApiError::Simple(StatusCode::BAD_GATEWAY, "Invalid model catalog".into()).into_response();
            };
            let Some(data) = payload.get("data").and_then(Value::as_array) else {
                return ApiError::Simple(StatusCode::BAD_GATEWAY, "Invalid model catalog".into()).into_response();
            };
            if data.len() > 20_000 || data.iter().any(|model| model.get("id").and_then(Value::as_str).is_none_or(|id| id.is_empty() || id.len() > 200)) {
                return ApiError::Simple(StatusCode::BAD_GATEWAY, "Invalid model catalog".into()).into_response();
            }
            let sanitized: Vec<Value> = data.iter().map(|model| serde_json::json!({"id":model["id"]})).collect();
            Json(serde_json::json!({"data":sanitized})).into_response()
        }
        Ok(_) => ApiError::Simple(StatusCode::BAD_GATEWAY, "Provider request failed".into())
            .into_response(),
        Err(error) if error.is_timeout() => ApiError::Simple(
            StatusCode::GATEWAY_TIMEOUT,
            "Request timed out after 10 seconds".into(),
        )
        .into_response(),
        Err(_) => ApiError::Simple(StatusCode::BAD_GATEWAY, "Unable to fetch provider models".into()).into_response(),
    }
}

enum ApiError {
    Auth(StatusCode, &'static str, &'static str),
    Simple(StatusCode, String),
}

impl ApiError {
    fn unauthorized() -> Self {
        Self::Auth(StatusCode::UNAUTHORIZED, "Unauthorized", "auth_error")
    }
    fn forbidden() -> Self {
        Self::Auth(
            StatusCode::FORBIDDEN,
            "Admin privileges required",
            "forbidden",
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self {
            ApiError::Auth(status, message, error_type) => {
                let body = serde_json::json!({
                    "error": { "message": message, "type": error_type, "code": status.as_u16() }
                });
                (status, Json(body)).into_response()
            }
            ApiError::Simple(status, message) => {
                let body = serde_json::json!({ "error": message });
                (status, Json(body)).into_response()
            }
        }
    }
}

fn require_admin(state: &AppState, headers: &HeaderMap) -> Result<(), ApiError> {
    let provided_key = headers.get("x-admin-key").and_then(|v| v.to_str().ok());
    match resolve_principal(state, provided_key) {
        Some(Principal::Admin) => Ok(()),
        Some(Principal::Limited { .. }) => Err(ApiError::forbidden()),
        None => Err(ApiError::unauthorized()),
    }
}

/// Mirrors `validateRawProviderSlug`: `/^[a-z0-9][a-z0-9._-]{0,62}$/`.
fn is_slug_safe(slug: &str) -> bool {
    let mut chars = slug.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    let rest_ok = chars
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '_' || c == '-');
    rest_ok && slug.len() <= 63
}

async fn list_providers(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(e) = require_admin(&state, &headers) {
        return e.into_response();
    }
    if let Some(pool) = &state.inner.database {
        let Ok(current) = crate::database::load_providers(pool).await else {
            return ApiError::Simple(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error".into()).into_response();
        };
        *state.inner.providers.write().unwrap() = current;
    }
    let providers = state.inner.providers.read().unwrap();
    let mut public = serde_json::Map::new();
    for (slug, config) in providers.iter() {
        public.insert(slug.clone(), provider_public(&state, config));
    }
    Json(Value::Object(public)).into_response()
}

/// Served JSON: the repository view with the OAuth account resolved the way
/// Plexus `resolveOAuthAccountId` does at read time.
fn provider_public(state: &AppState, config: &ProviderConfig) -> Value {
    let mut view = config.public_view();
    if let Some(map) = view.as_object_mut() {
        match config.oauth_provider.as_deref().and_then(|kind| resolve_oauth_account(state, kind, config.oauth_account.as_deref())) {
            Some(account) => { map.insert("oauth_account".into(), Value::String(account)); }
            None => { map.shift_remove("oauth_account"); }
        }
    }
    view
}

fn oauth_accounts(state: &AppState, kind: &str) -> Vec<String> {
    state.inner.oauth_accounts.read().unwrap().iter().filter(|(p, _)| p == kind).map(|(_, a)| a.clone()).collect()
}

/// `resolveOAuthAccountId`: the linked credential, else the well-known
/// `legacy` account, else the single account for this provider type.
fn resolve_oauth_account(state: &AppState, kind: &str, linked: Option<&str>) -> Option<String> {
    let accounts = oauth_accounts(state, kind);
    if let Some(linked) = linked.filter(|l| accounts.iter().any(|a| a == l)) { return Some(linked.to_owned()); }
    if accounts.iter().any(|a| a == "legacy") { return Some("legacy".into()); }
    (accounts.len() == 1).then(|| accounts[0].clone())
}

/// `saveProvider`'s credential link: the slug-keyed account, else a
/// grandfathered `oauth_account`, else the existing compatible link.
fn link_oauth_account(state: &AppState, slug: &str, parsed: &Value, existing: Option<&ProviderConfig>) -> Option<String> {
    let kind = parsed.get("oauth_provider").and_then(Value::as_str).filter(|k| !k.is_empty())?;
    let accounts = oauth_accounts(state, kind);
    if accounts.iter().any(|a| a == slug) { return Some(slug.to_owned()); }
    let legacy = parsed.get("oauth_account").and_then(Value::as_str).map(str::trim).filter(|a| !a.is_empty());
    if let Some(legacy) = legacy {
        return (legacy != slug && accounts.iter().any(|a| a == legacy)).then(|| legacy.to_owned());
    }
    existing.filter(|e| e.oauth_provider.as_deref() == Some(kind)).and_then(|e| e.oauth_account.clone()).filter(|a| accounts.iter().any(|x| x == a))
}

fn provider_etag(config: &ProviderConfig) -> String { format!("\"{}\"", config.version) }

async fn refresh_oauth_accounts(state: &AppState) {
    if let Some(pool) = &state.inner.database {
        if let Ok(rows) = sqlx::query_as::<_, (String, String)>("SELECT oauth_provider_type, account_id FROM oauth_credentials").fetch_all(pool).await {
            *state.inner.oauth_accounts.write().unwrap() = rows;
        }
    }
}

async fn get_provider(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(slug): Path<String>,
) -> Response {
    if let Err(e) = require_admin(&state, &headers) {
        return e.into_response();
    }
    if let Some(pool) = &state.inner.database {
        match crate::database::load_provider(pool, &slug).await {
            Ok(Some(config)) => { state.inner.providers.write().unwrap().insert(slug.clone(), config); }
            Ok(None) => { state.inner.providers.write().unwrap().remove(&slug); }
            Err(_) => return ApiError::Simple(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error".into()).into_response(),
        }
        refresh_oauth_accounts(&state).await;
    }
    let config = state.inner.providers.read().unwrap().get(&slug).cloned();
    match config {
        Some(config) => {
            let mut response = Json(provider_public(&state, &config)).into_response();
            response.headers_mut().insert(axum::http::header::ETAG, provider_etag(&config).parse().unwrap());
            response
        }
        None => ApiError::Simple(StatusCode::NOT_FOUND, format!("Provider '{slug}' not found")).into_response(),
    }
}

/// Plexus `ProviderConfigSchema` + `validateProviderQuotaChecker` + raw slug
/// check, then the repository round trip. `Err` is the finished 400.
async fn validate_provider(state: &AppState, slug: &str, body: &Value, existing: Option<&ProviderConfig>) -> Result<ProviderConfig, Response> {
    let parsed = crate::provider_schema::provider_config().safe_parse(body)
        .map_err(|details| (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "Validation failed", "details": details}))).into_response())?;
    let custom_types: Vec<String> = match &state.inner.database {
        Some(pool) => sqlx::query_scalar::<_, String>("SELECT trim(id) FROM custom_checkers WHERE enabled = true").fetch_all(pool).await.unwrap_or_default(),
        None => vec![],
    };
    crate::provider_schema::validate_quota_checker(&parsed, &custom_types)
        .map_err(|details| (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "Validation failed", "details": details}))).into_response())?;
    if parsed.pointer("/raw_passthrough/enabled") == Some(&Value::Bool(true)) && !is_slug_safe(slug) {
        return Err(ApiError::Simple(StatusCode::BAD_REQUEST, "Raw passthrough requires a single slug-safe provider ID".to_string()).into_response());
    }
    refresh_oauth_accounts(state).await;
    let account = link_oauth_account(state, slug, &parsed, existing);
    let view = crate::provider_view::repository_view(&parsed, account.as_deref());
    let mut config = crate::provider_view::from_view(view, existing.map(|e| e.version).unwrap_or(1));
    // The served view resolves the account at read time; keep the link.
    config.oauth_account = account;
    Ok(config)
}

async fn put_provider(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(slug): Path<String>,
    Json(raw): Json<Value>,
) -> Response {
    if let Err(e) = require_admin(&state, &headers) {
        return e.into_response();
    }
    let existing = match &state.inner.database {
        Some(pool) => crate::database::load_provider(pool, &slug).await.ok().flatten(),
        None => None,
    };
    let config = match validate_provider(&state, &slug, &raw, existing.as_ref()).await {
        Ok(config) => config,
        Err(response) => return response,
    };
    let Some(pool) = state.inner.database.clone() else {
        return ApiError::Simple(StatusCode::SERVICE_UNAVAILABLE, "Database unavailable".into()).into_response();
    };
    if crate::database::upsert_provider(&pool, &slug, &config).await.is_err() {
        return ApiError::Simple(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error".into()).into_response();
    }
    if let Ok(Some(saved)) = crate::database::load_provider(&pool, &slug).await {
        state.inner.providers.write().unwrap().insert(slug.clone(), saved);
    }
    Json(serde_json::json!({ "success": true, "slug": slug })).into_response()
}

async fn patch_provider(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(slug): Path<String>,
    Json(raw): Json<Value>,
) -> Response {
    if let Err(e) = require_admin(&state, &headers) {
        return e.into_response();
    }
    let Some(body) = raw.as_object() else {
        return ApiError::Simple(StatusCode::BAD_REQUEST, "Object body is required".to_string()).into_response();
    };
    let Some(pool) = state.inner.database.clone() else {
        return ApiError::Simple(StatusCode::SERVICE_UNAVAILABLE, "Database unavailable".into()).into_response();
    };
    let existing = match crate::database::load_provider(&pool, &slug).await {
        Ok(Some(provider)) => provider,
        Ok(None) => return ApiError::Simple(StatusCode::NOT_FOUND, format!("Provider '{slug}' not found")).into_response(),
        Err(_) => return ApiError::Simple(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error".into()).into_response(),
    };
    // Trefoil extension: optimistic concurrency via If-Match / expectedVersion.
    // Plexus strips unknown keys, so `expectedVersion` never reaches the schema.
    let expected_version = body.get("expectedVersion").and_then(Value::as_i64);
    let header_version = match headers.get(axum::http::header::IF_MATCH) {
        None => None,
        Some(value) => match value.to_str().ok().and_then(|s| s.strip_prefix('"').and_then(|s| s.strip_suffix('"')).and_then(|n| n.parse::<i64>().ok())) {
            Some(version) if version > 0 => Some(version),
            _ => return ApiError::Simple(StatusCode::BAD_REQUEST, "Invalid If-Match provider ETag".into()).into_response(),
        },
    };
    if header_version.is_some() && expected_version.is_some() && header_version != expected_version {
        return ApiError::Simple(StatusCode::BAD_REQUEST, "If-Match conflicts with expectedVersion".into()).into_response();
    }
    refresh_oauth_accounts(&state).await;
    let mut merged = provider_public(&state, &existing);
    let map = merged.as_object_mut().expect("provider view is an object");
    for (key, value) in body { map.insert(key.clone(), value.clone()); }
    // Explicit null clears these saved settings on PATCH; omission preserves them.
    for key in ["pi_ai_provider", "pi_ai_quirks", "responses_extensions"] {
        if body.get(key) == Some(&Value::Null) { map.shift_remove(key); }
    }
    let config = match validate_provider(&state, &slug, &merged, Some(&existing)).await {
        Ok(config) => config,
        Err(response) => return response,
    };
    let revision = header_version.or(expected_version).unwrap_or(existing.version);
    let next_version = match crate::database::update_provider_if_version(&pool, &slug, &config, revision).await {
        Ok(Some(version)) => version,
        Ok(None) => return ApiError::Simple(StatusCode::CONFLICT, "Provider changed; reload before saving".into()).into_response(),
        Err(_) => return ApiError::Simple(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error".into()).into_response(),
    };
    let config = ProviderConfig { version: next_version, ..config };
    let etag = provider_etag(&config);
    state.inner.providers.write().unwrap().insert(slug.clone(), config);
    let mut response = Json(serde_json::json!({ "success": true, "slug": slug })).into_response();
    response.headers_mut().insert(axum::http::header::ETAG, etag.parse().unwrap());
    response
}

async fn delete_provider(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(slug): Path<String>,
) -> Response {
    if let Err(e) = require_admin(&state, &headers) {
        return e.into_response();
    }
    // Plexus's `cascade=true` query param also deletes model aliases that
    // reference this provider; there's no model-alias store yet in this
    // slice, so `cascade` is a no-op here until aliases are ported.
    if let Some(pool) = &state.inner.database {
        if let Err(error) = crate::database::delete_provider(pool, &slug).await {
            return ApiError::Simple(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
                .into_response();
        }
    }
    state.inner.providers.write().unwrap().remove(&slug);
    Json(serde_json::json!({ "success": true, "provider": slug })).into_response()
}

// Serialize impl for HashMap<String, ProviderConfig> uses ProviderConfig's
// own Serialize derive via serde_json::Value machinery — no extra glue
// needed beyond importing HashMap for the state type above.
#[allow(unused_imports)]
use HashMap as _Unused;

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn admin_state() -> AppState {
        AppState::new(Some("correct-admin-key".to_string()), HashMap::new())
    }

    async fn call(
        state: AppState,
        method: &str,
        uri: &str,
        admin_header: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut req = Request::builder().uri(uri).method(method);
        if let Some(h) = admin_header {
            req = req.header("x-admin-key", h);
        }
        let body = match body {
            Some(v) => {
                req = req.header("content-type", "application/json");
                Body::from(serde_json::to_vec(&v).unwrap())
            }
            None => Body::empty(),
        };
        let response = router()
            .with_state(state)
            .oneshot(req.body(body).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        (status, json)
    }

    #[tokio::test]
    async fn put_rejects_missing_api_key() {
        let (status, body) = call(
            admin_state(),
            "PUT",
            "/v0/management/providers/test-provider",
            Some("correct-admin-key"),
            Some(serde_json::json!({ "api_base_url": "https://api.example.com" })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "Validation failed");
        assert!(body["details"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["message"] == "'api_key' must be specified for provider"));
    }

    #[tokio::test]
    #[ignore = "provider writes require PostgreSQL; covered by parity/provider-*-e2e.sh"]
    async fn put_accepts_valid_minimal_provider() {
        let state = admin_state();
        let (status, body) = call(
            state.clone(),
            "PUT",
            "/v0/management/providers/test-provider",
            Some("correct-admin-key"),
            Some(serde_json::json!({
                "api_base_url": "https://api.example.com",
                "api_key": "sk-x",
                "adapter": "openai"
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["success"], true);
        assert_eq!(body["slug"], "test-provider");

        let (status, body) = call(
            state,
            "GET",
            "/v0/management/providers/test-provider",
            Some("correct-admin-key"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["enabled"], true);
        assert_eq!(
            body["adapter"],
            serde_json::json!([{ "name": "openai", "options": {}, "enabled": true }])
        );
    }

    #[tokio::test]
    async fn put_oauth_placeholder_requires_oauth_provider_and_account() {
        let (status, body) = call(
            admin_state(),
            "PUT",
            "/v0/management/providers/oauth-test",
            Some("correct-admin-key"),
            Some(serde_json::json!({ "api_base_url": "oauth://", "adapter": "openai" })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let messages: Vec<&str> = body["details"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["message"].as_str().unwrap())
            .collect();
        assert!(messages.contains(&"'oauth_provider' must be specified when using oauth://"));
        assert!(!messages.contains(&"'oauth_account' must be specified when using oauth://"));
    }

    #[tokio::test]
    async fn put_rejects_invalid_api_base_url() {
        let (status, body) = call(
            admin_state(),
            "PUT",
            "/v0/management/providers/bad-url",
            Some("correct-admin-key"),
            Some(serde_json::json!({
                "api_base_url": "not-a-url",
                "api_key": "sk-x",
                "adapter": "openai"
            })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            body["details"][0]["message"],
            "api_base_url must be a valid URL or oauth://"
        );
    }

    #[tokio::test]
    async fn probe_rejects_unsupported_oauth_mode_without_dispatching() {
        let (status, body) = call(
            admin_state(),
            "POST",
            "/v0/management/test",
            Some("correct-admin-key"),
            Some(serde_json::json!({
                "provider": "missing",
                "model": "model",
                "apiType": "oauth"
            })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            body["error"],
            "OAuth probes are not supported by the test endpoint. Use the OAuth session flow instead."
        );
    }

    #[tokio::test]
    async fn put_raw_passthrough_requires_slug_safe_id() {
        let (status, body) = call(
            admin_state(),
            "PUT",
            "/v0/management/providers/provider%2Fmodel",
            Some("correct-admin-key"),
            Some(serde_json::json!({
                "api_base_url": "https://api.example.com",
                "api_key": "sk-x",
                "adapter": "openai",
                "raw_passthrough": { "enabled": true, "base_url": "https://api.example.com" }
            })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            body["error"],
            "Raw passthrough requires a single slug-safe provider ID"
        );
    }

    #[tokio::test]
    #[ignore = "provider writes require PostgreSQL; covered by parity/provider-*-e2e.sh"]
    async fn put_raw_passthrough_roundtrips_with_default_auth() {
        let state = admin_state();
        let (status, _) = call(
            state.clone(),
            "PUT",
            "/v0/management/providers/openrouter",
            Some("correct-admin-key"),
            Some(serde_json::json!({
                "api_base_url": "https://openrouter.ai/api/v1",
                "api_key": "sk-x",
                "adapter": "openai",
                "raw_passthrough": { "enabled": true, "base_url": "https://openrouter.ai/api" }
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (_, body) = call(
            state,
            "GET",
            "/v0/management/providers/openrouter",
            Some("correct-admin-key"),
            None,
        )
        .await;
        assert_eq!(
            body["raw_passthrough"],
            serde_json::json!({ "enabled": true, "base_url": "https://openrouter.ai/api", "auth": "bearer" })
        );
    }

    #[tokio::test]
    #[ignore = "provider writes require PostgreSQL; covered by parity/provider-*-e2e.sh"]
    async fn patch_nonexistent_provider_returns_404() {
        let (status, body) = call(
            admin_state(),
            "PATCH",
            "/v0/management/providers/nope",
            Some("correct-admin-key"),
            Some(serde_json::json!({ "enabled": false })),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"], "Provider 'nope' not found");
    }

    #[tokio::test]
    async fn patch_rejects_non_object_body() {
        let (status, body) = call(
            admin_state(),
            "PATCH",
            "/v0/management/providers/nope",
            Some("correct-admin-key"),
            Some(serde_json::json!([1, 2, 3])),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "Object body is required");
    }

    #[tokio::test]
    async fn delete_returns_success_with_provider_name() {
        let state = admin_state();
        call(
            state.clone(),
            "PUT",
            "/v0/management/providers/gone",
            Some("correct-admin-key"),
            Some(serde_json::json!({
                "api_base_url": "https://api.example.com",
                "api_key": "sk-x",
                "adapter": "openai"
            })),
        )
        .await;
        let (status, body) = call(
            state,
            "DELETE",
            "/v0/management/providers/gone",
            Some("correct-admin-key"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            serde_json::json!({ "success": true, "provider": "gone" })
        );
    }

    #[tokio::test]
    async fn limited_key_gets_403() {
        let mut keys = HashMap::new();
        keys.insert(
            "limited-key".to_string(),
            crate::state::KeyConfig {
                secret: "sk-limited".to_string(),
                ..Default::default()
            },
        );
        let state = AppState::new(Some("correct-admin-key".to_string()), keys);
        let (status, body) = call(
            state,
            "GET",
            "/v0/management/providers",
            Some("sk-limited"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["error"]["type"], "forbidden");
    }

    #[tokio::test]
    async fn missing_credential_gets_401() {
        let (status, _) = call(admin_state(), "GET", "/v0/management/providers", None, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
}
