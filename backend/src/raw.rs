//! Authenticated raw provider passthrough.

use crate::{
    auth::{resolve_principal, Principal},
    AppState,
};
use axum::{
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, Method, StatusCode},
    response::{IntoResponse, Response},
    Router,
};

pub fn router() -> Router<AppState> {
    Router::new().route("/raw/:provider/*path", axum::routing::any(raw))
}

async fn raw(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((provider, path)): Path<(String, String)>,
    method: Method,
    body: Body,
) -> Response {
    let principal = match resolve_principal(
        &state,
        headers
            .get("x-admin-key")
            .or_else(|| headers.get("authorization"))
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer ").or(Some(v))),
    ) {
        Some(value) => value,
        None => return (StatusCode::UNAUTHORIZED, "unauthorized").into_response(),
    };
    if let Principal::Limited { config, .. } = principal {
        if !config.allow_raw_passthrough {
            return (StatusCode::FORBIDDEN, "raw passthrough disabled").into_response();
        }
    }
    let Some(config) = state
        .inner
        .providers
        .read()
        .unwrap()
        .get(&provider)
        .cloned()
    else {
        return (StatusCode::NOT_FOUND, "provider not found").into_response();
    };
    if !config
        .raw_passthrough
        .as_ref()
        .is_some_and(|passthrough| passthrough.enabled)
    {
        return (
            StatusCode::NOT_FOUND,
            "raw passthrough is not enabled for provider",
        )
            .into_response();
    }
    let passthrough_base = config
        .raw_passthrough
        .as_ref()
        .map(|passthrough| passthrough.base_url.as_str())
        .filter(|value| !value.is_empty())
        .unwrap_or(config.api_base_url.as_str());
    let Ok(base) = url::Url::parse(passthrough_base) else {
        return (StatusCode::BAD_GATEWAY, "invalid provider URL").into_response();
    };
    if !matches!(base.scheme(), "http" | "https") || base.host_str().is_none() {
        return (StatusCode::BAD_GATEWAY, "invalid provider URL").into_response();
    };
    let bytes = match axum::body::to_bytes(body, 16 * 1024 * 1024).await {
        Ok(v) => v,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid body").into_response(),
    };
    let client = reqwest::Client::new();
    let url = format!(
        "{}/{}",
        passthrough_base.trim_end_matches('/'),
        path.trim_start_matches('/')
    );
    let mut request = client.request(method, url).body(bytes.to_vec());
    for (name, value) in &headers {
        if name.as_str().eq_ignore_ascii_case("content-type")
            || name.as_str().eq_ignore_ascii_case("accept")
            || name.as_str().eq_ignore_ascii_case("x-request-id")
        {
            request = request.header(name, value);
        }
    }
    if let Some(key) = config.api_key {
        request = request.bearer_auth(key)
    };
    match request.send().await {
        Ok(response) => {
            let status = response.status();
            let bytes = response.bytes().await.unwrap_or_default();
            (status, bytes).into_response()
        }
        Err(_) => (StatusCode::BAD_GATEWAY, "upstream request failed").into_response(),
    }
}
