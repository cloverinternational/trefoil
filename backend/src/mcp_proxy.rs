//! HTTP transport proxy for configured remote MCP servers.

use crate::AppState;
use axum::{
    body::{Body, Bytes},
    extract::{Path, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Router,
};
use futures_util::StreamExt;
use sqlx::Row;
use std::collections::HashMap;

const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/mcp/:name", axum::routing::post(post).get(get).delete(delete))
}

fn error(status: StatusCode, message: &str) -> Response {
    (status, axum::Json(serde_json::json!({
        "error": { "message": message, "type": "mcp_proxy_error", "code": status.as_u16() }
    }))).into_response()
}

fn valid_name(name: &str) -> bool {
    name.len() >= 2 && name.len() <= 63
        && name.as_bytes().first().is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name.as_bytes().iter().enumerate().all(|(i, c)|
            c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-' || (*c == b'_' && i > 0))
}

async fn config(state: &AppState, name: &str) -> Result<(String, bool, HashMap<String, String>), Response> {
    if !valid_name(name) { return Err(error(StatusCode::BAD_REQUEST, "Invalid MCP server name")); }
    let Some(pool) = &state.inner.database else { return Err(error(StatusCode::NOT_FOUND, "MCP server not found")); };
    let row = sqlx::query("SELECT upstream_url, enabled, mode, headers FROM mcp_servers WHERE name=$1")
        .bind(name).fetch_optional(pool).await.map_err(|_| error(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error"))?;
    let Some(row) = row else { return Err(error(StatusCode::NOT_FOUND, "MCP server not found")); };
    let enabled: bool = row.try_get("enabled").unwrap_or(true);
    let mode: String = row.try_get("mode").unwrap_or_else(|_| "remote_http".into());
    let url: String = row.try_get("upstream_url").unwrap_or_default();
    if !enabled || mode != "remote_http" { return Err(error(StatusCode::NOT_FOUND, "MCP server is disabled")); }
    if url::Url::parse(&url).ok().filter(|u| matches!(u.scheme(), "http" | "https") && u.host_str().is_some()).is_none() {
        return Err(error(StatusCode::BAD_GATEWAY, "Invalid MCP upstream URL"));
    }
    let raw: Option<String> = row.try_get("headers").ok().flatten();
    let headers = raw.and_then(|s| serde_json::from_str::<HashMap<String, String>>(&s).ok()).unwrap_or_default();
    Ok((url, enabled, headers))
}

fn copy_request_headers(mut request: reqwest::RequestBuilder, incoming: &HeaderMap, configured: &HashMap<String, String>) -> reqwest::RequestBuilder {
    // Authentication is deliberately never copied from the client. MCP transport
    // metadata is safe and required for session-based upstreams.
    for key in ["content-type", "accept", "mcp-session-id", "last-event-id"] {
        if let Some(value) = incoming.get(key) { if let Ok(value) = value.to_str() { request = request.header(key, value); } }
    }
    for (key, value) in configured { request = request.header(key, value); }
    request
}

async fn forward(method: reqwest::Method, url: String, headers: HeaderMap, configured: HashMap<String, String>, body: Option<Bytes>) -> Response {
    let client = match reqwest::Client::builder().timeout(REQUEST_TIMEOUT).build() { Ok(c) => c, Err(_) => return error(StatusCode::INTERNAL_SERVER_ERROR, "Unable to create HTTP client") };
    let mut request = copy_request_headers(client.request(method, url), &headers, &configured);
    if let Some(body) = body { request = request.body(body); }
    let upstream = match request.send().await {
        Ok(response) => response,
        Err(e) if e.is_timeout() => return error(StatusCode::GATEWAY_TIMEOUT, "MCP upstream request timed out"),
        Err(_) => return error(StatusCode::BAD_GATEWAY, "MCP upstream request failed"),
    };
    let status = upstream.status();
    let response_headers = upstream.headers().clone();
    let mut response = Response::new(Body::from_stream(upstream.bytes_stream().map(|chunk| chunk.map_err(std::io::Error::other))));
    *response.status_mut() = StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    for key in [header::CONTENT_TYPE.as_str(), header::CACHE_CONTROL.as_str(), "mcp-session-id", "connection"] {
        if let Some(value) = response_headers.get(key) { if let Ok(value) = HeaderValue::from_bytes(value.as_bytes()) { response.headers_mut().insert(key, value); } }
    }
    response
}

async fn post(State(state): State<AppState>, Path(name): Path<String>, headers: HeaderMap, body: Bytes) -> Response {
    let (url, _, configured) = match config(&state, &name).await { Ok(v) => v, Err(e) => return e };
    if body.is_empty() { return error(StatusCode::BAD_REQUEST, "Request body is required"); }
    forward(reqwest::Method::POST, url, headers, configured, Some(body)).await
}

async fn get(State(state): State<AppState>, Path(name): Path<String>, headers: HeaderMap, Query(query): Query<HashMap<String, String>>) -> Response {
    let (mut url, _, configured) = match config(&state, &name).await { Ok(v) => v, Err(e) => return e };
    if !query.is_empty() { let mut parsed = url::Url::parse(&url).unwrap(); parsed.query_pairs_mut().extend_pairs(query); url = parsed.to_string(); }
    forward(reqwest::Method::GET, url, headers, configured, None).await
}

async fn delete(State(state): State<AppState>, Path(name): Path<String>, headers: HeaderMap) -> Response {
    let (url, _, configured) = match config(&state, &name).await { Ok(v) => v, Err(e) => return e };
    forward(reqwest::Method::DELETE, url, headers, configured, None).await
}
