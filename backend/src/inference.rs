//! Initial OpenAI-compatible inference surface.
//!
//! Model discovery plus chat, legacy completions, and embeddings pass-through
//! for HTTP(S) providers. Shared authentication, request quotas, key access
//! policy, cooldown admission, request IDs, and PostgreSQL usage accounting are
//! enforced. Provider-specific transforms, full failover, pricing, and stream
//! terminal inspection remain intentionally deferred parity slices.

use axum::{
    body::Body,
    extract::State,
    http::HeaderMap,
    response::{IntoResponse, Response},
    Json, Router,
};
use serde_json::Value;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::{
    auth::{resolve_principal, Principal},
    AppState,
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/v1/models", axum::routing::get(list_models))
        .route(
            "/v1/chat/completions",
            axum::routing::post(chat_completions),
        )
        .route("/v1/embeddings", axum::routing::post(embeddings))
        .route("/v1/completions", axum::routing::post(completions))
        .route("/completions", axum::routing::post(completions))
}

async fn completions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut body): Json<Value>,
) -> Response {
    let principal = match authenticate(&state, &headers) {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    let Some(model) = body.get("model").and_then(Value::as_str).map(str::to_owned) else {
        return error_response(
            axum::http::StatusCode::BAD_REQUEST,
            "model is required",
            "invalid_request_error",
        );
    };
    if body.get("prompt").is_none() {
        return error_response(
            axum::http::StatusCode::BAD_REQUEST,
            "prompt is required",
            "invalid_request_error",
        );
    }
    let usage_key_name = match &principal {
        Principal::Limited { key_name, .. } => Some(key_name.as_str()),
        Principal::Admin => None,
    };
    if let Err(response) = enforce_request_quotas(&state, &principal).await {
        return response;
    }
    let key_config = if let Principal::Limited { config, .. } = &principal {
        if !model_allowed(config, &model) {
            return error_response(
                axum::http::StatusCode::FORBIDDEN,
                &format!("Key is not allowed to access model '{model}' for completions"),
                "access_denied",
            );
        }
        Some(config)
    } else {
        None
    };
    let (provider_slug, upstream_model) = resolve_target(&state, &model, key_config);
    if key_config.is_some_and(|config| !provider_allowed(config, &provider_slug)) {
        return error_response(
            axum::http::StatusCode::FORBIDDEN,
            &format!("Key is not allowed to access any provider configured for model '{model}'"),
            "access_denied",
        );
    }
    if target_is_on_cooldown(&state, &provider_slug, &upstream_model) {
        return error_response(
            axum::http::StatusCode::BAD_GATEWAY,
            &format!("Provider {provider_slug}/{upstream_model} is on cooldown"),
            "upstream_error",
        );
    }
    let Some(provider) = state
        .inner
        .providers
        .read()
        .unwrap()
        .get(&provider_slug)
        .cloned()
    else {
        return error_response(
            axum::http::StatusCode::NOT_FOUND,
            "No provider configured for model",
            "model_not_found",
        );
    };
    let Ok(base) = dispatch_provider_url(&provider.api_base_url) else {
        return error_response(
            axum::http::StatusCode::BAD_GATEWAY,
            "Provider API base URL is invalid",
            "upstream_error",
        );
    };
    let request_id = new_request_id();
    let start_time = now_ms();
    body["model"] = Value::String(upstream_model);
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            return error_response(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                &error.to_string(),
                "server_error",
            )
        }
    };
    let mut request = client
        .post(format!(
            "{}/completions",
            base.as_str().trim_end_matches('/')
        ))
        .json(&body);
    if let Some(key) = provider.api_key {
        request = request.bearer_auth(key);
    }
    match request.send().await {
        Ok(upstream) => {
            let status = upstream.status();
            if body.get("stream").and_then(Value::as_bool).unwrap_or(false) {
                record_basic_usage(
                    &state,
                    &request_id,
                    usage_key_name,
                    &provider_slug,
                    &model,
                    "completions",
                    if status.is_success() {
                        "success"
                    } else {
                        "error"
                    },
                    true,
                    start_time,
                )
                .await;
                let content_type = upstream
                    .headers()
                    .get("content-type")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or("text/event-stream");
                return axum::http::Response::builder()
                    .status(status)
                    .header("content-type", content_type)
                    .header("cache-control", "no-cache")
                    .header("x-request-id", &request_id)
                    .body(Body::from_stream(upstream.bytes_stream()))
                    .expect("valid streaming response")
                    .into_response();
            }
            let payload = upstream.json::<Value>().await.unwrap_or_else(|_| {
                serde_json::json!({"error":{"message":"Invalid upstream response","type":"upstream_error"}})
            });
            record_response_usage(
                &state,
                &request_id,
                usage_key_name,
                &provider_slug,
                &model,
                "completions",
                status.is_success(),
                start_time,
                &payload,
                true,
            )
            .await;
            response_with_request_id(status, payload, &request_id)
        }
        Err(error) => {
            record_basic_usage(
                &state,
                &request_id,
                usage_key_name,
                &provider_slug,
                &model,
                "completions",
                "error",
                false,
                start_time,
            )
            .await;
            error_with_request_id(
                axum::http::StatusCode::BAD_GATEWAY,
                &error.to_string(),
                "upstream_error",
                &request_id,
            )
        }
    }
}

async fn embeddings(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let principal = match authenticate(&state, &headers) {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    let Some(model) = body.get("model").and_then(Value::as_str).map(str::to_owned) else {
        return error_response(
            axum::http::StatusCode::BAD_REQUEST,
            "model is required",
            "invalid_request_error",
        );
    };
    if body.get("input").is_none() {
        return error_response(
            axum::http::StatusCode::BAD_REQUEST,
            "input is required",
            "invalid_request_error",
        );
    }
    let usage_key_name = match &principal {
        Principal::Limited { key_name, .. } => Some(key_name.as_str()),
        Principal::Admin => None,
    };
    if let Err(response) = enforce_request_quotas(&state, &principal).await {
        return response;
    }
    let key_config = if let Principal::Limited { config, .. } = &principal {
        if !model_allowed(config, &model) {
            return error_response(
                axum::http::StatusCode::FORBIDDEN,
                &format!("Key is not allowed to access model '{model}' for embeddings"),
                "access_denied",
            );
        }
        Some(config)
    } else {
        None
    };
    let (provider_slug, upstream_model) = resolve_target(&state, &model, key_config);
    if key_config.is_some_and(|config| !provider_allowed(config, &provider_slug)) {
        return error_response(
            axum::http::StatusCode::FORBIDDEN,
            &format!("Key is not allowed to access any provider configured for model '{model}'"),
            "access_denied",
        );
    }
    if target_is_on_cooldown(&state, &provider_slug, &upstream_model) {
        return error_response(
            axum::http::StatusCode::BAD_GATEWAY,
            &format!("Provider {provider_slug}/{upstream_model} is on cooldown"),
            "upstream_error",
        );
    }
    let Some(provider) = state
        .inner
        .providers
        .read()
        .unwrap()
        .get(&provider_slug)
        .cloned()
    else {
        return error_response(
            axum::http::StatusCode::NOT_FOUND,
            "No provider configured for model",
            "model_not_found",
        );
    };
    let Ok(base) = dispatch_provider_url(&provider.api_base_url) else {
        return error_response(
            axum::http::StatusCode::BAD_GATEWAY,
            "Provider API base URL is invalid",
            "upstream_error",
        );
    };
    let request_id = format!(
        "trefoil-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default()
    );
    let start_time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default();
    let mut request_body = body;
    request_body["model"] = Value::String(upstream_model);
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            return error_response(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                &error.to_string(),
                "server_error",
            )
        }
    };
    let mut request = client
        .post(format!(
            "{}/embeddings",
            base.as_str().trim_end_matches('/')
        ))
        .json(&request_body);
    if let Some(key) = provider.api_key {
        request = request.bearer_auth(key);
    }
    match request.send().await {
        Ok(response) => {
            let status = response.status();
            let payload = response.json::<Value>().await.unwrap_or_else(|_| serde_json::json!({"error":{"message":"Invalid upstream response","type":"upstream_error"}}));
            if let Some(pool) = &state.inner.database {
                let duration_ms = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|value| value.as_millis() as i64)
                    .unwrap_or(start_time)
                    - start_time;
                let input_tokens = payload["usage"]["prompt_tokens"]
                    .as_i64()
                    .map(|value| value as i32);
                let _ = crate::database::record_request_usage(
                    pool,
                    &request_id,
                    usage_key_name,
                    Some(&provider_slug),
                    Some(&model),
                    "embeddings",
                    if status.is_success() {
                        "success"
                    } else {
                        "error"
                    },
                    false,
                    start_time,
                )
                .await;
                let _ = crate::database::update_request_usage_metrics(
                    pool,
                    &request_id,
                    input_tokens,
                    Some(0),
                    None,
                    duration_ms,
                )
                .await;
            }
            let mut response = (status, Json(payload)).into_response();
            if let Ok(value) = request_id.parse() {
                response.headers_mut().insert("x-request-id", value);
            }
            response
        }
        Err(error) => {
            if let Some(pool) = &state.inner.database {
                let _ = crate::database::record_request_usage(
                    pool,
                    &request_id,
                    usage_key_name,
                    Some(&provider_slug),
                    Some(&model),
                    "embeddings",
                    "error",
                    false,
                    start_time,
                )
                .await;
            }
            let mut response = error_response(
                axum::http::StatusCode::BAD_GATEWAY,
                &error.to_string(),
                "upstream_error",
            );
            if let Ok(value) = request_id.parse() {
                response.headers_mut().insert("x-request-id", value);
            }
            response
        }
    }
}

fn auth_error(status: axum::http::StatusCode, message: &'static str) -> Response {
    (
        status,
        Json(serde_json::json!({
            "error": { "message": message, "type": "auth_error", "code": status.as_u16() }
        })),
    )
        .into_response()
}

#[allow(clippy::result_large_err)] // Route handlers return Axum responses directly on auth failure.
fn authenticate(state: &AppState, headers: &HeaderMap) -> Result<Principal, Response> {
    let key = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .or_else(|| headers.get("x-api-key").and_then(|v| v.to_str().ok()))
        .or_else(|| headers.get("x-admin-key").and_then(|v| v.to_str().ok()));
    resolve_principal(state, key)
        .ok_or_else(|| auth_error(axum::http::StatusCode::UNAUTHORIZED, "Unauthorized"))
}

async fn list_models(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if authenticate(&state, &headers).is_err() {
        return auth_error(axum::http::StatusCode::UNAUTHORIZED, "Unauthorized");
    }
    // Only alias slugs are listed here, matching Plexus's /v1/models: direct
    // provider/model syntax (e.g. "openai/gpt-4o") is intentionally excluded
    // from discovery since it's meant for debugging only, not client-facing
    // model selection.
    let mut data = Vec::new();
    for slug in state.inner.aliases.read().unwrap().keys() {
        data.push(serde_json::json!({ "id": slug, "object": "model", "owned_by": "trefoil" }));
    }
    data.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    Json(serde_json::json!({ "object": "list", "data": data })).into_response()
}

async fn chat_completions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut body): Json<Value>,
) -> Response {
    let principal = match authenticate(&state, &headers) {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    let usage_key_name = match &principal {
        Principal::Limited { key_name, .. } => Some(key_name.as_str()),
        Principal::Admin => None,
    };
    let request_id = format!(
        "trefoil-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default()
    );
    let start_time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default();
    let Some(model) = body.get("model").and_then(Value::as_str).map(str::to_owned) else {
        return error_response(
            axum::http::StatusCode::BAD_REQUEST,
            "model is required",
            "invalid_request_error",
        );
    };
    if body.get("messages").and_then(Value::as_array).is_none() {
        return error_response(
            axum::http::StatusCode::BAD_REQUEST,
            "messages must be an array",
            "invalid_request_error",
        );
    }
    if let Err(response) = enforce_request_quotas(&state, &principal).await {
        return response;
    }
    let key_config = if let Principal::Limited { config, .. } = &principal {
        if !model_allowed(config, &model) {
            return error_response(
                axum::http::StatusCode::FORBIDDEN,
                &format!("Key is not allowed to access model '{model}' for chat"),
                "access_denied",
            );
        }
        Some(config)
    } else {
        None
    };
    let (provider_slug, upstream_model) = resolve_target(&state, &model, key_config);
    if let Some(config) = key_config {
        if !provider_allowed(config, &provider_slug) {
            return error_response(
                axum::http::StatusCode::FORBIDDEN,
                &format!(
                    "Key is not allowed to access any provider configured for model '{model}'"
                ),
                "access_denied",
            );
        }
    }
    if target_is_on_cooldown(&state, &provider_slug, &upstream_model) {
        return error_response(
            axum::http::StatusCode::BAD_GATEWAY,
            &format!("Provider {provider_slug}/{upstream_model} is on cooldown"),
            "upstream_error",
        );
    }
    let Some(provider) = state
        .inner
        .providers
        .read()
        .unwrap()
        .get(&provider_slug)
        .cloned()
    else {
        return error_response(
            axum::http::StatusCode::NOT_FOUND,
            "No provider configured for model",
            "model_not_found",
        );
    };
    if provider.api_base_url.is_empty() {
        return error_response(
            axum::http::StatusCode::BAD_GATEWAY,
            "Provider has no API base URL",
            "upstream_error",
        );
    }
    let Ok(provider_url) = dispatch_provider_url(&provider.api_base_url) else {
        return error_response(
            axum::http::StatusCode::BAD_GATEWAY,
            "Provider API base URL is invalid",
            "upstream_error",
        );
    };
    body["model"] = Value::String(upstream_model);
    let endpoint = format!(
        "{}/chat/completions",
        provider_url.as_str().trim_end_matches('/')
    );
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            return error_response(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                &error.to_string(),
                "server_error",
            )
        }
    };
    let mut request = client.post(&endpoint).json(&body);
    if let Some(api_key) = provider.api_key {
        request = request.bearer_auth(api_key);
    }
    match request.send().await {
        Ok(upstream) => {
            let status = upstream.status();
            if body.get("stream").and_then(Value::as_bool).unwrap_or(false) {
                if let Some(pool) = &state.inner.database {
                    let _ = crate::database::record_request_usage(
                        pool,
                        &request_id,
                        usage_key_name,
                        Some(&provider_slug),
                        Some(&model),
                        "chat",
                        if status.is_success() {
                            "success"
                        } else {
                            "error"
                        },
                        true,
                        start_time,
                    )
                    .await;
                }
                let content_type = upstream
                    .headers()
                    .get("content-type")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or("text/event-stream");
                return axum::http::Response::builder()
                    .status(status)
                    .header("content-type", content_type)
                    .header("cache-control", "no-cache")
                    .header("x-request-id", &request_id)
                    .body(Body::from_stream(upstream.bytes_stream()))
                    .expect("valid streaming response")
                    .into_response();
            }
            let payload = upstream.json::<Value>().await.unwrap_or_else(|_| serde_json::json!({
                "error": { "message": "Invalid response from upstream provider", "type": "upstream_error" }
            }));
            if let Some(pool) = &state.inner.database {
                let input_tokens = payload["usage"]["prompt_tokens"]
                    .as_i64()
                    .map(|value| value as i32);
                let output_tokens = payload["usage"]["completion_tokens"]
                    .as_i64()
                    .map(|value| value as i32);
                let total_cost = payload["usage"]["total_cost"]
                    .as_f64()
                    .map(|value| value as f32);
                let duration_ms = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|value| value.as_millis() as i64)
                    .unwrap_or(start_time)
                    - start_time;
                let _ = crate::database::record_request_usage(
                    pool,
                    &request_id,
                    usage_key_name,
                    Some(&provider_slug),
                    Some(&model),
                    "chat",
                    if status.is_success() {
                        "success"
                    } else {
                        "error"
                    },
                    false,
                    start_time,
                )
                .await;
                let _ = crate::database::update_request_usage_metrics(
                    pool,
                    &request_id,
                    input_tokens,
                    output_tokens,
                    total_cost,
                    duration_ms,
                )
                .await;
            }
            let mut response = (status, Json(payload)).into_response();
            if let Ok(value) = request_id.parse() {
                response.headers_mut().insert("x-request-id", value);
            }
            response
        }
        Err(error) => {
            record_basic_usage(
                &state,
                &request_id,
                usage_key_name,
                &provider_slug,
                &model,
                "chat",
                "error",
                false,
                start_time,
            )
            .await;
            error_with_request_id(
                axum::http::StatusCode::BAD_GATEWAY,
                &error.to_string(),
                "upstream_error",
                &request_id,
            )
        }
    }
}

fn error_response(status: axum::http::StatusCode, message: &str, error_type: &str) -> Response {
    (
        status,
        Json(serde_json::json!({ "error": { "message": message, "type": error_type } })),
    )
        .into_response()
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

fn new_request_id() -> String {
    format!(
        "trefoil-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default()
    )
}

fn model_allowed(config: &crate::state::KeyConfig, model: &str) -> bool {
    !config.excluded_models.iter().any(|value| value == model)
        && (config.allowed_models.is_empty()
            || config.allowed_models.iter().any(|value| value == model))
}

fn response_with_request_id(
    status: axum::http::StatusCode,
    payload: Value,
    request_id: &str,
) -> Response {
    let mut response = (status, Json(payload)).into_response();
    if let Ok(value) = request_id.parse() {
        response.headers_mut().insert("x-request-id", value);
    }
    response
}

fn error_with_request_id(
    status: axum::http::StatusCode,
    message: &str,
    error_type: &str,
    request_id: &str,
) -> Response {
    let mut response = error_response(status, message, error_type);
    if let Ok(value) = request_id.parse() {
        response.headers_mut().insert("x-request-id", value);
    }
    response
}

#[allow(clippy::too_many_arguments)] // Keeps route call sites explicit about every accounting field.
async fn record_basic_usage(
    state: &AppState,
    request_id: &str,
    key_name: Option<&str>,
    provider: &str,
    model: &str,
    api_type: &str,
    status: &str,
    streamed: bool,
    start_time: i64,
) {
    if let Some(pool) = &state.inner.database {
        let _ = crate::database::record_request_usage(
            pool,
            request_id,
            key_name,
            Some(provider),
            Some(model),
            api_type,
            status,
            streamed,
            start_time,
        )
        .await;
    }
}

#[allow(clippy::too_many_arguments)] // Extends record_basic_usage with response-derived metrics.
async fn record_response_usage(
    state: &AppState,
    request_id: &str,
    key_name: Option<&str>,
    provider: &str,
    model: &str,
    api_type: &str,
    success: bool,
    start_time: i64,
    payload: &Value,
    include_output_tokens: bool,
) {
    record_basic_usage(
        state,
        request_id,
        key_name,
        provider,
        model,
        api_type,
        if success { "success" } else { "error" },
        false,
        start_time,
    )
    .await;
    if let Some(pool) = &state.inner.database {
        let input_tokens = payload["usage"]["prompt_tokens"]
            .as_i64()
            .map(|value| value as i32);
        let output_tokens = include_output_tokens
            .then(|| payload["usage"]["completion_tokens"].as_i64())
            .flatten()
            .map(|value| value as i32)
            .or_else(|| (!include_output_tokens).then_some(0));
        let total_cost = payload["usage"]["total_cost"]
            .as_f64()
            .map(|value| value as f32);
        let _ = crate::database::update_request_usage_metrics(
            pool,
            request_id,
            input_tokens,
            output_tokens,
            total_cost,
            now_ms() - start_time,
        )
        .await;
    }
}

fn dispatch_provider_url(value: &str) -> Result<url::Url, ()> {
    let parsed = url::Url::parse(value).map_err(|_| ())?;
    if matches!(parsed.scheme(), "http" | "https") && parsed.host_str().is_some() {
        Ok(parsed)
    } else {
        Err(())
    }
}

async fn enforce_request_quotas(state: &AppState, principal: &Principal) -> Result<(), Response> {
    let Principal::Limited { key_name, config } = principal else {
        return Ok(());
    };
    let quota_names = if config.quota_names.is_empty() {
        state
            .inner
            .settings
            .read()
            .unwrap()
            .get("default_quotas")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    } else {
        config.quota_names.clone()
    };
    let Some(pool) = &state.inner.database else {
        return Ok(());
    };
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default();
    let windows = crate::database::request_quota_windows(pool, &quota_names, now_ms)
        .await
        .map_err(|error| {
            error_response(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                &error.to_string(),
                "server_error",
            )
        })?;
    for (quota, limit, start, end) in windows {
        match crate::database::reserve_request_quota(pool, key_name, &quota, limit, start, now_ms)
            .await
        {
            Ok(None) => {}
            Ok(Some(current_usage)) => {
                return Err(quota_exceeded_response(&quota, limit, current_usage, end));
            }
            Err(error) => {
                return Err(error_response(
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    &error.to_string(),
                    "server_error",
                ));
            }
        }
    }
    Ok(())
}

/// Matches Plexus's `buildQuotaExceededBody` shape exactly (only the single
/// most-constrained quota is reported here; Trefoil doesn't yet aggregate
/// multiple simultaneously-blocking quotas into `blocking_quotas`, so that
/// array always has exactly one entry -- a documented partial-parity gap).
fn quota_exceeded_response(
    quota_name: &str,
    limit: i32,
    current_usage: i64,
    resets_at_ms: i64,
) -> Response {
    let resets_at = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(resets_at_ms)
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_default();
    (
        axum::http::StatusCode::TOO_MANY_REQUESTS,
        Json(serde_json::json!({
            "error": {
                "message": format!("Quota exceeded: {quota_name} limit of {limit} reached"),
                "type": "quota_exceeded",
                "quota_name": quota_name,
                "current_usage": current_usage,
                "limit": limit,
                "resets_at": resets_at,
                "blocking_quotas": [{
                    "quotaName": quota_name,
                    "limitType": "requests",
                    "limit": limit,
                    "currentUsage": current_usage,
                    "remaining": (limit as i64 - current_usage).max(0),
                    "resetsAt": resets_at,
                }],
            }
        })),
    )
        .into_response()
}

fn resolve_target(
    state: &AppState,
    model: &str,
    key_config: Option<&crate::state::KeyConfig>,
) -> (String, String) {
    if let Some((provider, name)) = model.split_once('/') {
        if state.inner.providers.read().unwrap().contains_key(provider) {
            return (provider.to_string(), name.to_string());
        }
    }
    if state.inner.providers.read().unwrap().contains_key(model) {
        return (model.to_string(), model.to_string());
    }
    if let Some(alias) = state.inner.aliases.read().unwrap().get(model) {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis() as i64)
            .unwrap_or_default();
        let providers = state.inner.providers.read().unwrap();
        let cooldowns = state.inner.cooldowns.read().unwrap();
        if let Some(target) = alias
            .target_groups
            .iter()
            .flat_map(|group| group.targets.iter())
            .filter(|target| target.enabled && target.provider.is_some() && target.model.is_some())
            .filter(|target| {
                let provider = target.provider.as_deref().unwrap_or_default();
                let model = target.model.as_deref().unwrap_or_default();
                providers.contains_key(provider)
                    && key_config.is_none_or(|config| provider_allowed(config, provider))
                    && !cooldown_is_active(&cooldowns, provider, model, now_ms)
            })
            .find(|_| true)
        {
            return (
                target.provider.clone().unwrap(),
                target.model.clone().unwrap(),
            );
        }
    }
    (model.to_string(), model.to_string())
}

fn provider_allowed(config: &crate::state::KeyConfig, provider: &str) -> bool {
    !config
        .excluded_providers
        .iter()
        .any(|value| value == provider)
        && (config.allowed_providers.is_empty()
            || config
                .allowed_providers
                .iter()
                .any(|value| value == provider))
}

fn target_is_on_cooldown(state: &AppState, provider: &str, model: &str) -> bool {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default();
    cooldown_is_active(
        &state.inner.cooldowns.read().unwrap(),
        provider,
        model,
        now_ms,
    )
}

fn cooldown_is_active(
    cooldowns: &std::collections::HashMap<(String, String), crate::state::CooldownEntry>,
    provider: &str,
    model: &str,
    now_ms: i64,
) -> bool {
    ["", model].iter().any(|candidate_model| {
        cooldowns
            .get(&(provider.to_owned(), (*candidate_model).to_owned()))
            .is_some_and(|entry| entry.expiry_ms > now_ms)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{CooldownEntry, KeyConfig};
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use std::collections::HashMap;
    use tower::ServiceExt;

    #[tokio::test]
    async fn models_requires_authentication() {
        let state = AppState::new(Some("admin".into()), HashMap::new());
        let response = router()
            .with_state(state)
            .oneshot(Request::get("/v1/models").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn models_lists_aliases_only_not_raw_providers() {
        let state = AppState::new(Some("admin".into()), HashMap::new());
        state
            .inner
            .providers
            .write()
            .unwrap()
            .insert("openai".into(), Default::default());
        state
            .inner
            .aliases
            .write()
            .unwrap()
            .insert("gpt-4o-alias".into(), Default::default());
        let request = Request::get("/v1/models")
            .header("Authorization", "Bearer admin")
            .body(Body::empty())
            .unwrap();
        let response = router().with_state(state).oneshot(request).await.unwrap();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: Value = serde_json::from_slice(&body).unwrap();
        let ids: Vec<&str> = json["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["gpt-4o-alias"]);
    }

    #[tokio::test]
    async fn embeddings_and_completions_require_authentication() {
        let state = AppState::new(Some("admin".into()), HashMap::new());
        for path in ["/v1/embeddings", "/v1/completions", "/completions"] {
            let response = router()
                .with_state(state.clone())
                .oneshot(
                    Request::post(path)
                        .header("content-type", "application/json")
                        .body(Body::from(r#"{"model":"m","input":"x","prompt":"x"}"#))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), axum::http::StatusCode::UNAUTHORIZED);
        }
    }

    #[tokio::test]
    async fn embeddings_accepts_x_api_key_and_enforces_model_policy() {
        let mut keys = HashMap::new();
        keys.insert(
            "limited".into(),
            KeyConfig {
                secret: "secret".into(),
                allowed_models: vec!["allowed".into()],
                ..Default::default()
            },
        );
        let state = AppState::new(Some("admin".into()), keys);
        let response = router()
            .with_state(state)
            .oneshot(
                Request::post("/v1/embeddings")
                    .header("x-api-key", "secret")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"model":"blocked","input":"x"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::FORBIDDEN);
        let body: Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(body["error"]["type"], "access_denied");
    }

    #[test]
    fn cooldown_matching_checks_model_specific_and_provider_wide_entries() {
        let mut cooldowns = HashMap::new();
        cooldowns.insert(
            ("openai".into(), "gpt-4o".into()),
            CooldownEntry {
                expiry_ms: 2_000,
                consecutive_failures: 1,
                last_error: None,
            },
        );
        assert!(cooldown_is_active(&cooldowns, "openai", "gpt-4o", 1_000));
        assert!(!cooldown_is_active(&cooldowns, "openai", "gpt-4o", 2_000));
        assert!(!cooldown_is_active(&cooldowns, "openai", "gpt-4.1", 1_000));

        cooldowns.insert(
            ("openai".into(), "".into()),
            CooldownEntry {
                expiry_ms: 3_000,
                consecutive_failures: 2,
                last_error: None,
            },
        );
        assert!(cooldown_is_active(&cooldowns, "openai", "gpt-4.1", 1_000));
        assert!(!cooldown_is_active(
            &cooldowns,
            "anthropic",
            "claude",
            1_000
        ));
    }

    #[test]
    fn dispatch_provider_url_requires_http_or_https_with_a_host() {
        assert!(dispatch_provider_url("https://api.example.test/v1").is_ok());
        assert!(dispatch_provider_url("http://localhost:1234/v1").is_ok());
        assert!(dispatch_provider_url("not a URL").is_err());
        assert!(dispatch_provider_url("oauth://").is_err());
        assert!(dispatch_provider_url("file:///tmp/provider").is_err());
    }
}
