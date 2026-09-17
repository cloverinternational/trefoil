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
use serde_json::Value;
use sqlx::Row;
use std::collections::HashMap;

const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

pub fn router() -> Router<AppState> {
    Router::new().route(
        "/mcp/:name",
        axum::routing::post(post).get(get).delete(delete),
    )
}

fn error(status: StatusCode, message: &str) -> Response {
    (
        status,
        axum::Json(serde_json::json!({
            "error": { "message": message, "type": "mcp_proxy_error", "code": status.as_u16() }
        })),
    )
        .into_response()
}

fn jsonrpc_error(code: i32, message: &str, id: Value) -> Response {
    (
        StatusCode::BAD_REQUEST,
        axum::Json(serde_json::json!({
            "jsonrpc": "2.0", "error": { "code": code, "message": message }, "id": id
        })),
    )
        .into_response()
}

/// Validate JSON-RPC envelopes before forwarding them upstream.
fn validate_jsonrpc(body: &[u8]) -> Result<(Option<String>, Option<String>), Response> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|_| jsonrpc_error(-32700, "Parse error", Value::Null))?;
    let entries: Vec<&Value> = match &value {
        Value::Array(items) if !items.is_empty() => items.iter().collect(),
        Value::Array(_) => return Err(jsonrpc_error(-32600, "Invalid Request", Value::Null)),
        _ => vec![&value],
    };
    let mut method = None;
    let mut tool = None;
    for entry in entries {
        let Some(object) = entry.as_object() else {
            return Err(jsonrpc_error(-32600, "Invalid Request", Value::Null));
        };
        if object.get("jsonrpc") != Some(&Value::String("2.0".into())) {
            return Err(jsonrpc_error(-32600, "Invalid Request", Value::Null));
        }
        let Some(name) = object.get("method").and_then(Value::as_str) else {
            return Err(jsonrpc_error(-32600, "Invalid Request", Value::Null));
        };
        if let Some(params) = object.get("params") {
            if !(params.is_object() || params.is_array()) {
                return Err(jsonrpc_error(
                    -32602,
                    "Invalid params",
                    object.get("id").cloned().unwrap_or(Value::Null),
                ));
            }
        }
        method = Some(name.to_owned());
        if name == "tools/call" {
            tool = object
                .get("params")
                .and_then(|p| p.get("name"))
                .and_then(Value::as_str)
                .map(str::to_owned);
        }
    }
    Ok((method, tool))
}

async fn record_request(
    pool: &sqlx::PgPool,
    request_id: &str,
    started: i64,
    server: &str,
    upstream: &str,
    method: &str,
    jsonrpc_method: Option<&str>,
    tool_name: Option<&str>,
    status: u16,
) {
    let duration = chrono::Utc::now().timestamp_millis() - started;
    let _ = sqlx::query("INSERT INTO mcp_request_usage (request_id,start_time,duration_ms,server_name,upstream_url,method,jsonrpc_method,tool_name,response_status) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9) ON CONFLICT (request_id) DO UPDATE SET duration_ms=EXCLUDED.duration_ms,response_status=EXCLUDED.response_status")
        .bind(request_id).bind(started).bind(duration).bind(server).bind(upstream).bind(method)
        .bind(jsonrpc_method).bind(tool_name).bind(i32::from(status)).execute(pool).await;
}

fn valid_name(name: &str) -> bool {
    name.len() >= 2
        && name.len() <= 63
        && name
            .as_bytes()
            .first()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name.as_bytes().iter().enumerate().all(|(i, c)| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-' || (*c == b'_' && i > 0)
        })
}

async fn config(
    state: &AppState,
    name: &str,
) -> Result<(String, bool, HashMap<String, String>), Response> {
    if !valid_name(name) {
        return Err(error(StatusCode::BAD_REQUEST, "Invalid MCP server name"));
    }
    let Some(pool) = &state.inner.database else {
        return Err(error(StatusCode::NOT_FOUND, "MCP server not found"));
    };
    let row =
        sqlx::query("SELECT upstream_url, enabled, mode, headers FROM mcp_servers WHERE name=$1")
            .bind(name)
            .fetch_optional(pool)
            .await
            .map_err(|_| error(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error"))?;
    let Some(row) = row else {
        return Err(error(StatusCode::NOT_FOUND, "MCP server not found"));
    };
    let enabled: bool = row.try_get("enabled").unwrap_or(true);
    let mode: String = row.try_get("mode").unwrap_or_else(|_| "remote_http".into());
    let url: String = row.try_get("upstream_url").unwrap_or_default();
    if !enabled || mode != "remote_http" {
        return Err(error(StatusCode::NOT_FOUND, "MCP server is disabled"));
    }
    if url::Url::parse(&url)
        .ok()
        .filter(|u| matches!(u.scheme(), "http" | "https") && u.host_str().is_some())
        .is_none()
    {
        return Err(error(StatusCode::BAD_GATEWAY, "Invalid MCP upstream URL"));
    }
    let raw: Option<String> = row.try_get("headers").ok().flatten();
    let headers = raw
        .and_then(|s| serde_json::from_str::<HashMap<String, String>>(&s).ok())
        .unwrap_or_default();
    Ok((url, enabled, headers))
}

fn copy_request_headers(
    mut request: reqwest::RequestBuilder,
    incoming: &HeaderMap,
    configured: &HashMap<String, String>,
) -> reqwest::RequestBuilder {
    // Authentication is deliberately never copied from the client. MCP transport
    // metadata is safe and required for session-based upstreams.
    for key in ["content-type", "accept", "mcp-session-id", "last-event-id"] {
        if let Some(value) = incoming.get(key) {
            if let Ok(value) = value.to_str() {
                request = request.header(key, value);
            }
        }
    }
    for (key, value) in configured {
        request = request.header(key, value);
    }
    request
}

async fn forward(
    method: reqwest::Method,
    url: String,
    headers: HeaderMap,
    configured: HashMap<String, String>,
    body: Option<Bytes>,
) -> Response {
    let client = match reqwest::Client::builder().timeout(REQUEST_TIMEOUT).build() {
        Ok(c) => c,
        Err(_) => {
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Unable to create HTTP client",
            )
        }
    };
    let mut request = copy_request_headers(client.request(method, url), &headers, &configured);
    if let Some(body) = body {
        request = request.body(body);
    }
    let upstream = match request.send().await {
        Ok(response) => response,
        Err(e) if e.is_timeout() => {
            return error(
                StatusCode::GATEWAY_TIMEOUT,
                "MCP upstream request timed out",
            )
        }
        Err(_) => return error(StatusCode::BAD_GATEWAY, "MCP upstream request failed"),
    };
    let status = upstream.status();
    let response_headers = upstream.headers().clone();
    let mut response = Response::new(Body::from_stream(
        upstream
            .bytes_stream()
            .map(|chunk| chunk.map_err(std::io::Error::other)),
    ));
    *response.status_mut() =
        StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    for key in [
        header::CONTENT_TYPE.as_str(),
        header::CACHE_CONTROL.as_str(),
        "mcp-session-id",
        "connection",
    ] {
        if let Some(value) = response_headers.get(key) {
            if let Ok(value) = HeaderValue::from_bytes(value.as_bytes()) {
                response.headers_mut().insert(key, value);
            }
        }
    }
    response
}

async fn post(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let (url, _, configured) = match config(&state, &name).await {
        Ok(v) => v,
        Err(e) => return e,
    };
    if body.is_empty() {
        return error(StatusCode::BAD_REQUEST, "Request body is required");
    }
    let (jsonrpc_method, tool_name) = match validate_jsonrpc(&body) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let started = chrono::Utc::now().timestamp_millis();
    let request_id = format!("mcp-{started}-{}", rand::random::<u64>());
    let response = forward(
        reqwest::Method::POST,
        url.clone(),
        headers,
        configured,
        Some(body),
    )
    .await;
    if let Some(pool) = &state.inner.database {
        record_request(
            pool,
            &request_id,
            started,
            &name,
            &url,
            "POST",
            jsonrpc_method.as_deref(),
            tool_name.as_deref(),
            response.status().as_u16(),
        )
        .await;
    }
    response
}

async fn get(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let (mut url, _, configured) = match config(&state, &name).await {
        Ok(v) => v,
        Err(e) => return e,
    };
    if !query.is_empty() {
        let mut parsed = url::Url::parse(&url).unwrap();
        parsed.query_pairs_mut().extend_pairs(query);
        url = parsed.to_string();
    }
    let started = chrono::Utc::now().timestamp_millis();
    let request_id = format!("mcp-{started}-{}", rand::random::<u64>());
    let response = forward(reqwest::Method::GET, url.clone(), headers, configured, None).await;
    if let Some(pool) = &state.inner.database {
        record_request(
            pool,
            &request_id,
            started,
            &name,
            &url,
            "GET",
            None,
            None,
            response.status().as_u16(),
        )
        .await;
    }
    response
}

async fn delete(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Response {
    let (url, _, configured) = match config(&state, &name).await {
        Ok(v) => v,
        Err(e) => return e,
    };
    let started = chrono::Utc::now().timestamp_millis();
    let request_id = format!("mcp-{started}-{}", rand::random::<u64>());
    let response = forward(
        reqwest::Method::DELETE,
        url.clone(),
        headers,
        configured,
        None,
    )
    .await;
    if let Some(pool) = &state.inner.database {
        record_request(
            pool,
            &request_id,
            started,
            &name,
            &url,
            "DELETE",
            None,
            None,
            response.status().as_u16(),
        )
        .await;
    }
    response
}
