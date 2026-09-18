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
    AdapterEntry, AppState, ProviderConfig, QuotaCheckerConfig, RawPassthroughConfig,
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

const PROBE_API_TYPES: [&str; 8] = [
    "chat",
    "messages",
    "gemini",
    "responses",
    "embeddings",
    "images",
    "speech",
    "oauth",
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
    let base = url::Url::parse(&provider.api_base_url)
        .map_err(|_| "Provider API base URL is invalid".to_string())?;
    if !matches!(base.scheme(), "http" | "https") || base.host_str().is_none() {
        return Err("Provider API base URL is invalid".into());
    }
    let (path, payload) = match api_type {
        "messages" => (
            "/messages",
            serde_json::json!({"model":model,"max_tokens":100,"system":"You are a helpful assistant.","messages":[{"role":"user","content":"Just respond with the word acknowledged"}]}),
        ),
        "gemini" => (
            "/v1beta/models/{model}:generateContent",
            serde_json::json!({"contents":[{"role":"user","parts":[{"text":"Just respond with the word acknowledged"}]}],"generationConfig":{"maxOutputTokens":100}}),
        ),
        "responses" => (
            "/responses",
            serde_json::json!({"model":model,"input":"Just respond with the word acknowledged","instructions":"You are a helpful assistant."}),
        ),
        "embeddings" => (
            "/embeddings",
            serde_json::json!({"model":model,"input":["Hello world"]}),
        ),
        "images" => (
            "/images/generations",
            serde_json::json!({"model":model,"prompt":"A tiny red square","n":1}),
        ),
        "speech" => (
            "/audio/speech",
            serde_json::json!({"model":model,"input":"Hello world"}),
        ),
        _ => (
            "/chat/completions",
            serde_json::json!({"model":model,"messages":[{"role":"user","content":"Just respond with the word acknowledged"}],"max_tokens":100}),
        ),
    };
    let path = path.replace("{model}", model);
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

async fn fetch_models(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if let Err(error) = require_admin(&state, &headers) {
        return error.into_response();
    }
    let Some(url) = body.get("url").and_then(Value::as_str) else {
        return ApiError::validation(vec![ValidationIssue::root("url is required")])
            .into_response();
    };
    let Ok(parsed) = url::Url::parse(url) else {
        return ApiError::Simple(StatusCode::BAD_REQUEST, "Invalid URL".into()).into_response();
    };
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || matches!(parsed.host_str(), Some("localhost" | "127.0.0.1" | "::1"))
    {
        return ApiError::Simple(StatusCode::BAD_REQUEST, "URL is not allowed".into())
            .into_response();
    }
    let mut request = reqwest::Client::new().get(url);
    if let Some(key) = body.get("apiKey").and_then(Value::as_str) {
        request = request.bearer_auth(key);
    }
    match request
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
    {
        Ok(response) if response.status().is_success() => {
            let payload = response
                .json::<Value>()
                .await
                .unwrap_or_else(|_| serde_json::json!({"data":[]}));
            Json(payload).into_response()
        }
        Ok(_) => ApiError::Simple(StatusCode::BAD_GATEWAY, "Provider request failed".into())
            .into_response(),
        Err(error) if error.is_timeout() => ApiError::Simple(
            StatusCode::GATEWAY_TIMEOUT,
            "Request timed out after 10 seconds".into(),
        )
        .into_response(),
        Err(error) => ApiError::Simple(StatusCode::BAD_GATEWAY, error.to_string()).into_response(),
    }
}

enum ApiError {
    Auth(StatusCode, &'static str, &'static str),
    Simple(StatusCode, String),
    Validation(Vec<ValidationIssue>),
}

struct ValidationIssue {
    message: String,
    path: Vec<String>,
}

impl ValidationIssue {
    fn root(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            path: vec![],
        }
    }
    fn at(path: &str, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            path: vec![path.to_string()],
        }
    }
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
    fn validation(issues: Vec<ValidationIssue>) -> Self {
        Self::Validation(issues)
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
            ApiError::Validation(issues) => {
                let details: Vec<Value> = issues
                    .iter()
                    .map(|i| {
                        serde_json::json!({
                            "code": "custom",
                            "path": i.path,
                            "message": i.message,
                        })
                    })
                    .collect();
                let body = serde_json::json!({ "error": "Validation failed", "details": details });
                (StatusCode::BAD_REQUEST, Json(body)).into_response()
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

/// Wire shape accepted by PUT/PATCH bodies. Mirrors the load-bearing subset
/// of `ProviderConfigSchema`.
#[derive(Debug, Deserialize, Default)]
struct ProviderBody {
    display_name: Option<String>,
    api_base_url: Option<String>,
    api_key: Option<String>,
    oauth_provider: Option<String>,
    oauth_account: Option<String>,
    enabled: Option<bool>,
    disable_cooldown: Option<bool>,
    stall_cooldown: Option<bool>,
    allow_100_percent_utilization: Option<bool>,
    discount: Option<f64>,
    #[serde(rename = "estimateTokens")]
    estimate_tokens: Option<bool>,
    #[serde(rename = "useClaudeMasking")]
    use_claude_masking: Option<bool>,
    #[serde(rename = "geminiThinkingEnabled")]
    gemini_thinking_enabled: Option<bool>,
    auto_compat: Option<bool>,
    /// Accepts the legacy string/array-of-string form or the normalized
    /// `AdapterEntry[]` form, matching `AdapterConfigSchema`'s preprocess step.
    adapter: Option<AdapterInput>,
    raw_passthrough: Option<RawPassthroughConfig>,
    quota_checker: Option<QuotaCheckerConfig>,
    #[serde(rename = "retryPolicy")]
    retry_policy: Option<crate::state::RetryPolicy>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum AdapterInput {
    Single(AdapterInputEntry),
    Many(Vec<AdapterInputEntry>),
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum AdapterInputEntry {
    Name(String),
    Entry {
        name: String,
        #[serde(default)]
        options: serde_json::Map<String, Value>,
        #[serde(default = "super_true")]
        enabled: bool,
    },
}

fn super_true() -> bool {
    true
}

fn normalize_adapter(input: AdapterInput) -> Vec<AdapterEntry> {
    let entries = match input {
        AdapterInput::Single(e) => vec![e],
        AdapterInput::Many(es) => es,
    };
    entries
        .into_iter()
        .map(|e| match e {
            AdapterInputEntry::Name(name) => AdapterEntry {
                name,
                options: Default::default(),
                enabled: true,
            },
            AdapterInputEntry::Entry {
                name,
                options,
                enabled,
            } => AdapterEntry {
                name,
                options,
                enabled,
            },
        })
        .collect()
}

/// Mirrors `isValidUrlOrOAuth`: an `oauth://` placeholder or a parseable URL.
fn is_valid_url_or_oauth(value: &str) -> bool {
    if ProviderConfig::is_oauth_placeholder_url(value) {
        return true;
    }
    url::Url::parse(value).is_ok()
}

/// Validates a `ProviderBody` against the load-bearing subset of
/// `ProviderConfigSchema`'s refinements, and builds a `ProviderConfig` from
/// it merged over `existing` (None for a from-scratch PUT).
fn validate_and_build(
    body: ProviderBody,
    existing: Option<&ProviderConfig>,
) -> Result<ProviderConfig, Vec<ValidationIssue>> {
    let mut issues = Vec::new();

    let api_base_url = body
        .api_base_url
        .clone()
        .or_else(|| existing.map(|e| e.api_base_url.clone()));
    let api_base_url = match api_base_url {
        Some(url) if is_valid_url_or_oauth(&url) => url,
        Some(_) => {
            issues.push(ValidationIssue::at(
                "api_base_url",
                "api_base_url must be a valid URL or oauth://",
            ));
            String::new()
        }
        None => {
            issues.push(ValidationIssue::at("api_base_url", "Required"));
            String::new()
        }
    };

    let is_oauth = ProviderConfig::is_oauth_placeholder_url(&api_base_url);
    let api_key = body
        .api_key
        .clone()
        .or_else(|| existing.and_then(|e| e.api_key.clone()));
    let oauth_provider = body
        .oauth_provider
        .clone()
        .or_else(|| existing.and_then(|e| e.oauth_provider.clone()));
    let oauth_account = body
        .oauth_account
        .clone()
        .or_else(|| existing.and_then(|e| e.oauth_account.clone()));

    if api_key.is_none() && !is_oauth {
        issues.push(ValidationIssue::root(
            "'api_key' must be specified for provider",
        ));
    }
    if is_oauth && oauth_provider.is_none() {
        issues.push(ValidationIssue::root(
            "'oauth_provider' must be specified when using oauth://",
        ));
    }
    if let Some(provider) = oauth_provider.as_deref() {
        if !crate::oauth::is_supported_provider(provider) {
            issues.push(ValidationIssue::root(
                "'oauth_provider' must be a supported OAuth provider",
            ));
        }
    }
    if is_oauth && oauth_account.is_none() {
        issues.push(ValidationIssue::root(
            "'oauth_account' must be specified when using oauth://",
        ));
    }

    let raw_passthrough = body
        .raw_passthrough
        .or_else(|| existing.and_then(|e| e.raw_passthrough.clone()));
    if let Some(rp) = &raw_passthrough {
        if rp.enabled && is_oauth {
            issues.push(ValidationIssue::root(
                "raw_passthrough currently supports static API-key providers only",
            ));
        }
    }

    if !issues.is_empty() {
        return Err(issues);
    }

    let adapter = match body.adapter {
        Some(a) => normalize_adapter(a),
        None => existing.map(|e| e.adapter.clone()).unwrap_or_default(),
    };

    Ok(ProviderConfig {
        display_name: body
            .display_name
            .or_else(|| existing.and_then(|e| e.display_name.clone())),
        api_base_url,
        api_key,
        oauth_provider,
        oauth_account,
        enabled: body
            .enabled
            .unwrap_or_else(|| existing.map(|e| e.enabled).unwrap_or(true)),
        disable_cooldown: body
            .disable_cooldown
            .unwrap_or_else(|| existing.map(|e| e.disable_cooldown).unwrap_or(false)),
        stall_cooldown: body
            .stall_cooldown
            .unwrap_or_else(|| existing.map(|e| e.stall_cooldown).unwrap_or(false)),
        allow_100_percent_utilization: body.allow_100_percent_utilization.unwrap_or_else(|| {
            existing
                .map(|e| e.allow_100_percent_utilization)
                .unwrap_or(false)
        }),
        discount: body.discount.or_else(|| existing.and_then(|e| e.discount)),
        estimate_tokens: body
            .estimate_tokens
            .unwrap_or_else(|| existing.map(|e| e.estimate_tokens).unwrap_or(false)),
        use_claude_masking: body
            .use_claude_masking
            .unwrap_or_else(|| existing.map(|e| e.use_claude_masking).unwrap_or(false)),
        gemini_thinking_enabled: body
            .gemini_thinking_enabled
            .unwrap_or_else(|| existing.map(|e| e.gemini_thinking_enabled).unwrap_or(false)),
        auto_compat: body
            .auto_compat
            .unwrap_or_else(|| existing.map(|e| e.auto_compat).unwrap_or(false)),
        adapter,
        model_autosync: existing
            .map(|e| e.model_autosync.clone())
            .unwrap_or_default(),
        raw_passthrough,
        quota_checker: body
            .quota_checker
            .or_else(|| existing.and_then(|e| e.quota_checker.clone())),
        retry_policy: body
            .retry_policy
            .or_else(|| existing.map(|e| e.retry_policy.clone()))
            .unwrap_or_default(),
    })
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

fn check_raw_passthrough_slug(config: &ProviderConfig, slug: &str) -> Result<(), ApiError> {
    if config.raw_passthrough.as_ref().is_some_and(|rp| rp.enabled) && !is_slug_safe(slug) {
        return Err(ApiError::Simple(
            StatusCode::BAD_REQUEST,
            "Raw passthrough requires a single slug-safe provider ID".to_string(),
        ));
    }
    Ok(())
}

async fn list_providers(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(e) = require_admin(&state, &headers) {
        return e.into_response();
    }
    let providers = state.inner.providers.read().unwrap();
    Json(providers.clone()).into_response()
}

async fn get_provider(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(slug): Path<String>,
) -> Response {
    if let Err(e) = require_admin(&state, &headers) {
        return e.into_response();
    }
    let providers = state.inner.providers.read().unwrap();
    match providers.get(&slug) {
        Some(config) => Json(config.clone()).into_response(),
        None => ApiError::Simple(
            StatusCode::NOT_FOUND,
            format!("Provider '{slug}' not found"),
        )
        .into_response(),
    }
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
    let body: ProviderBody = match serde_json::from_value(raw) {
        Ok(b) => b,
        Err(_) => return ApiError::validation(vec![]).into_response(),
    };
    let config = match validate_and_build(body, None) {
        Ok(c) => c,
        Err(issues) => return ApiError::validation(issues).into_response(),
    };
    if let Err(e) = check_raw_passthrough_slug(&config, &slug) {
        return e.into_response();
    }

    if let Some(pool) = &state.inner.database {
        if let Err(error) = crate::database::upsert_provider(pool, &slug, &config).await {
            return ApiError::Simple(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
                .into_response();
        }
    }
    state
        .inner
        .providers
        .write()
        .unwrap()
        .insert(slug.clone(), config);
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
    if !raw.is_object() {
        return ApiError::Simple(
            StatusCode::BAD_REQUEST,
            "Object body is required".to_string(),
        )
        .into_response();
    }

    let Some(existing) = state.inner.providers.read().unwrap().get(&slug).cloned() else {
        return ApiError::Simple(
            StatusCode::NOT_FOUND,
            format!("Provider '{slug}' not found"),
        )
        .into_response();
    };

    let body: ProviderBody = match serde_json::from_value(raw) {
        Ok(b) => b,
        Err(_) => return ApiError::validation(vec![]).into_response(),
    };
    let merged = match validate_and_build(body, Some(&existing)) {
        Ok(c) => c,
        Err(issues) => return ApiError::validation(issues).into_response(),
    };
    if let Err(e) = check_raw_passthrough_slug(&merged, &slug) {
        return e.into_response();
    }

    if let Some(pool) = &state.inner.database {
        if let Err(error) = crate::database::upsert_provider(pool, &slug, &merged).await {
            return ApiError::Simple(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
                .into_response();
        }
    }
    state
        .inner
        .providers
        .write()
        .unwrap()
        .insert(slug.clone(), merged);
    Json(serde_json::json!({ "success": true, "slug": slug })).into_response()
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
        assert!(messages.contains(&"'oauth_account' must be specified when using oauth://"));
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
