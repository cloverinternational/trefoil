//! HTTP transport proxy for configured remote MCP servers.

use crate::AppState;
use axum::{
    body::{Body, Bytes},
    extract::{Path, Query, State},
    http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Router,
};
use futures_util::StreamExt;
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::Row;
use std::collections::HashMap;
use subtle::ConstantTimeEq;

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

fn scope_allows(scopes: Option<&str>, required: &str) -> bool {
    let Some(scopes) = scopes else {
        return true;
    };
    let granted: std::collections::HashSet<&str> = scopes.split_whitespace().collect();
    granted.contains(required) || (required == "mcp:read" && granted.contains("mcp:write"))
}

fn resource_matches(resource: &str, name: &str) -> bool {
    let expected = format!("/mcp/{name}");
    resource == expected
        || url::Url::parse(resource)
            .ok()
            .is_some_and(|url| url.path() == expected)
}

fn required_scope(method: Option<&str>) -> &'static str {
    if method == Some("tools/call") {
        "mcp:write"
    } else {
        "mcp:read"
    }
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
        // A batch containing a write operation requires write scope, even if
        // a later read operation is the last entry in the batch.
        if method.is_none() || name == "tools/call" {
            method = Some(name.to_owned());
        }
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
) -> Result<(String, bool, HashMap<String, String>, i32, String, i64, i64), Response> {
    if !valid_name(name) {
        return Err(error(StatusCode::BAD_REQUEST, "Invalid MCP server name"));
    }
    let Some(pool) = &state.inner.database else {
        return Err(error(StatusCode::NOT_FOUND, "MCP server not found"));
    };
    let row =
        sqlx::query("SELECT id, upstream_url, enabled, mode, headers, auth_scheme, rate_limit_cooldown_ms, quota_cooldown_ms FROM mcp_servers WHERE name=$1")
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
    Ok((
        url,
        enabled,
        headers,
        row.try_get("id").unwrap_or_default(),
        row.try_get("auth_scheme").unwrap_or_default(),
        i64::from(
            row.try_get::<i32, _>("rate_limit_cooldown_ms")
                .unwrap_or(60_000),
        ),
        i64::from(
            row.try_get::<i32, _>("quota_cooldown_ms")
                .unwrap_or(86_400_000),
        ),
    ))
}

#[derive(Debug, Clone)]
struct UpstreamKey {
    id: i32,
    key: String,
}

fn select_round_robin(last: Option<i32>, keys: &[UpstreamKey]) -> Option<&UpstreamKey> {
    if keys.is_empty() {
        return None;
    }
    let index = last
        .and_then(|id| keys.iter().position(|key| key.id == id))
        .map_or(0, |index| (index + 1) % keys.len());
    keys.get(index)
}

async fn active_upstream_keys(
    state: &AppState,
    server_id: i32,
) -> Result<Vec<UpstreamKey>, Response> {
    let Some(pool) = &state.inner.database else {
        return Ok(Vec::new());
    };
    let rows = sqlx::query("SELECT id, key FROM mcp_keys WHERE mcp_server_id=$1 AND is_active=true AND (cooldown_until IS NULL OR cooldown_until <= NOW()) ORDER BY id")
        .bind(server_id).fetch_all(pool).await
        .map_err(|_| error(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error"))?;
    Ok(rows
        .into_iter()
        .map(|row| UpstreamKey {
            id: row.try_get("id").unwrap_or_default(),
            key: row.try_get("key").unwrap_or_default(),
        })
        .collect())
}

async fn cooldown_key(state: &AppState, key_id: i32, cooldown_ms: i64) {
    if let Some(pool) = &state.inner.database {
        let _ = sqlx::query("UPDATE mcp_keys SET cooldown_until=$1, updated_at=NOW() WHERE id=$2")
            .bind(chrono::Utc::now().naive_utc() + chrono::Duration::milliseconds(cooldown_ms))
            .bind(key_id)
            .execute(pool)
            .await;
    }
}

fn inject_upstream_auth(
    headers: &HashMap<String, String>,
    scheme: &str,
    key: &str,
) -> HashMap<String, String> {
    if scheme.trim().is_empty() || key.is_empty() {
        return headers.clone();
    }
    let mut output = headers.clone();
    let normalized = scheme.to_ascii_lowercase();
    let header_name = if normalized == "bearer" {
        "authorization"
    } else {
        normalized.as_str()
    };
    output.retain(|name, _| name.to_ascii_lowercase() != header_name);
    output.insert(
        if normalized == "bearer" {
            "Authorization".into()
        } else {
            scheme.into()
        },
        if normalized == "bearer" {
            format!("Bearer {key}")
        } else {
            key.into()
        },
    );
    output
}

#[derive(Debug, Default)]
struct AuthContext {
    oauth_scopes: Option<String>,
}

async fn authenticate(
    state: &AppState,
    name: &str,
    incoming: &HeaderMap,
) -> Result<AuthContext, Response> {
    let Some(pool) = &state.inner.database else {
        return Err(error(
            StatusCode::UNAUTHORIZED,
            "MCP authentication required",
        ));
    };
    let bearer = incoming
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| {
            let (scheme, token) = value.split_once(' ')?;
            scheme.eq_ignore_ascii_case("bearer").then_some(token)
        })
        .filter(|value| !value.is_empty());

    // OAuth access tokens are scoped to the MCP resource they were issued for.
    // Check them before the legacy MCP-key path, while retaining Bearer API-key
    // compatibility for installations that use MCP keys as bearer credentials.
    if let Some(token) = bearer {
        let hash = hex::encode(Sha256::digest(token.as_bytes()));
        let now = chrono::Utc::now().timestamp_millis();
        if let Some(row) = sqlx::query(
            "SELECT access_token, resource, scope FROM mcp_oauth_tokens WHERE access_token_hash=$1 AND revoked_at IS NULL AND access_token_expires_at>$2",
        )
        .bind(&hash)
        .bind(now)
        .fetch_optional(pool)
        .await
        .map_err(|_| error(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error"))?
        {
            let stored: String = row.try_get("access_token").unwrap_or_default();
            let resource: String = row.try_get("resource").unwrap_or_default();
            let resource_matches = resource_matches(&resource, name);
            let token_matches = Sha256::digest(token.as_bytes())
                .ct_eq(Sha256::digest(stored.as_bytes()).as_ref());
            if resource_matches && bool::from(token_matches) {
                return Ok(AuthContext { oauth_scopes: row.try_get("scope").unwrap_or(None) });
            }
            return Err(error(StatusCode::UNAUTHORIZED, "Invalid MCP credentials"));
        }
    }

    let credential = bearer
        .or_else(|| incoming.get("x-api-key").and_then(|v| v.to_str().ok()))
        .filter(|value| !value.is_empty());
    let Some(credential) = credential else {
        return Err(error(
            StatusCode::UNAUTHORIZED,
            "MCP authentication required",
        ));
    };
    let digest = Sha256::digest(credential.as_bytes());
    let rows = sqlx::query("SELECT k.key FROM mcp_keys k JOIN mcp_servers s ON s.id=k.mcp_server_id WHERE s.name=$1 AND k.is_active=true")
        .bind(name).fetch_all(pool).await
        .map_err(|_| error(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error"))?;
    let mut matched = false;
    for row in rows {
        let key: String = row.try_get("key").unwrap_or_default();
        matched |= bool::from(digest.ct_eq(Sha256::digest(key.as_bytes()).as_ref()));
    }
    if matched {
        Ok(AuthContext::default())
    } else {
        Err(error(StatusCode::UNAUTHORIZED, "Invalid MCP credentials"))
    }
}

fn safe_configured_header(name: &str) -> bool {
    !matches!(
        name.to_ascii_lowercase().as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "trailers"
            | "transfer-encoding"
            | "upgrade"
            | "host"
            | "content-length"
    )
}

fn strip_zero_ttl(value: &mut Value) -> bool {
    let messages = match value {
        Value::Array(items) => items.iter_mut().collect::<Vec<_>>(),
        _ => vec![value],
    };
    let mut changed = false;
    for message in messages {
        let Some(result) = message.get_mut("result").and_then(Value::as_object_mut) else {
            continue;
        };
        if result.get("tools").is_some_and(Value::is_array)
            && result
                .get("ttlMs")
                .and_then(Value::as_i64)
                .is_some_and(|ttl| ttl <= 0)
        {
            result.remove("ttlMs");
            changed = true;
        }
    }
    changed
}

fn sanitize_sse_lines(buffer: &mut String, chunk: &[u8]) -> Bytes {
    buffer.push_str(&String::from_utf8_lossy(chunk));
    let mut output = String::new();
    while let Some(index) = buffer.find('\n') {
        let line = buffer[..index].to_owned();
        *buffer = buffer[index + 1..].to_owned();
        let data = line
            .strip_prefix("data: ")
            .or_else(|| line.strip_prefix("data:"));
        if let Some(data) = data {
            if let Ok(mut value) = serde_json::from_str::<Value>(data) {
                if strip_zero_ttl(&mut value) {
                    output.push_str("data: ");
                    output.push_str(&value.to_string());
                    output.push('\n');
                    continue;
                }
            }
        }
        output.push_str(&line);
        output.push('\n');
    }
    Bytes::from(output)
}

fn safe_client_header(name: &str) -> bool {
    safe_configured_header(name)
        && !matches!(
            name.to_ascii_lowercase().as_str(),
            "authorization" | "cookie" | "set-cookie" | "x-api-key" | "proxy-authorization"
        )
}

fn copy_request_headers(
    mut request: reqwest::RequestBuilder,
    incoming: &HeaderMap,
    configured: &HashMap<String, String>,
) -> reqwest::RequestBuilder {
    // Forward transport metadata and ordinary client headers, but never leak
    // gateway credentials to the upstream. Configured headers are applied after.
    for (name, value) in incoming {
        if safe_client_header(name.as_str()) {
            request = request.header(name, value);
        }
    }
    for (key, value) in configured {
        if safe_configured_header(key) {
            request = request.header(key, value);
        }
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
    if let Some(ref body) = body {
        request = request.body(body.clone());
        let has_content_type = headers.keys().any(|name| name == header::CONTENT_TYPE)
            || configured
                .keys()
                .any(|name| name.eq_ignore_ascii_case("content-type"));
        if !has_content_type {
            request = request.header(header::CONTENT_TYPE, "application/json");
        }
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
    let is_event_stream = response_headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().starts_with("text/event-stream"));
    let is_tools_list = body.as_ref().is_some_and(|body| {
        serde_json::from_slice::<Value>(body).is_ok_and(|value| match value {
            Value::Array(items) => items
                .iter()
                .any(|item| item.get("method") == Some(&Value::String("tools/list".into()))),
            _ => value.get("method") == Some(&Value::String("tools/list".into())),
        })
    });
    let sanitize = is_event_stream && is_tools_list;
    let mut sse_buffer = String::new();
    let mut response = if is_event_stream {
        Response::new(Body::from_stream(upstream.bytes_stream().map(
            move |chunk| {
                chunk
                    .map(|bytes| {
                        if sanitize {
                            sanitize_sse_lines(&mut sse_buffer, &bytes)
                        } else {
                            bytes
                        }
                    })
                    .map_err(std::io::Error::other)
            },
        )))
    } else {
        // The reference proxy buffers ordinary JSON responses so it can apply
        // the tools/list compatibility fix without forwarding a stale
        // Content-Length. Non-JSON bodies remain byte-for-byte unchanged.
        let bytes = match upstream.bytes().await {
            Ok(bytes) => bytes,
            Err(_) => return error(StatusCode::BAD_GATEWAY, "MCP upstream response failed"),
        };
        let body = if is_tools_list {
            match serde_json::from_slice::<Value>(&bytes) {
                Ok(mut value) => {
                    if strip_zero_ttl(&mut value) {
                        serde_json::to_vec(&value).unwrap_or_else(|_| bytes.to_vec())
                    } else {
                        bytes.to_vec()
                    }
                }
                _ => bytes.to_vec(),
            }
        } else {
            bytes.to_vec()
        };
        Response::new(Body::from(body))
    };
    *response.status_mut() =
        StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    for (name, value) in &response_headers {
        if safe_configured_header(name.as_str()) {
            response.headers_mut().append(name, value.clone());
        }
    }
    // Streamable HTTP clients require these proxy-safe SSE headers.  The
    // reference proxy adds them when the upstream did not, while preserving
    // an explicit upstream Cache-Control value.
    let is_event_stream = response_headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().starts_with("text/event-stream"));
    if is_event_stream {
        if !response.headers().contains_key(header::CACHE_CONTROL) {
            response
                .headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        }
        response.headers_mut().insert(
            HeaderName::from_static("x-accel-buffering"),
            HeaderValue::from_static("no"),
        );
    }
    if !is_event_stream && is_tools_list {
        // A transformed buffered body can no longer use the upstream length.
        response.headers_mut().remove(header::CONTENT_LENGTH);
    }
    response
}

async fn post(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let auth = match authenticate(&state, &name, &headers).await {
        Ok(auth) => auth,
        Err(response) => return response,
    };
    let (url, _, configured, server_id, auth_scheme, rate_cooldown, quota_cooldown) =
        match config(&state, &name).await {
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
    if !scope_allows(
        auth.oauth_scopes.as_deref(),
        required_scope(jsonrpc_method.as_deref()),
    ) {
        return error(
            StatusCode::FORBIDDEN,
            "MCP OAuth scope does not permit this operation",
        );
    }
    let started = chrono::Utc::now().timestamp_millis();
    let request_id = format!("mcp-{started}-{}", rand::random::<u64>());
    let keys = match active_upstream_keys(&state, server_id).await {
        Ok(keys) => keys,
        Err(response) => return response,
    };
    let attempts = keys.len().max(1);
    let mut response = None;
    for attempt in 0..attempts {
        let last = state
            .inner
            .mcp_key_round_robin
            .read()
            .unwrap()
            .get(&server_id)
            .copied();
        let key = select_round_robin(last, &keys);
        if let Some(key) = key {
            state
                .inner
                .mcp_key_round_robin
                .write()
                .unwrap()
                .insert(server_id, key.id);
        }
        let request_headers = key.map_or_else(
            || configured.clone(),
            |key| inject_upstream_auth(&configured, &auth_scheme, &key.key),
        );
        let current = forward(
            reqwest::Method::POST,
            url.clone(),
            headers.clone(),
            request_headers,
            Some(body.clone()),
        )
        .await;
        let status = current.status();
        if !matches!(
            status,
            StatusCode::TOO_MANY_REQUESTS | StatusCode::PAYMENT_REQUIRED
        ) || key.is_none()
        {
            response = Some(current);
            break;
        }
        cooldown_key(
            &state,
            key.unwrap().id,
            if status == StatusCode::TOO_MANY_REQUESTS {
                rate_cooldown
            } else {
                quota_cooldown
            },
        )
        .await;
        response = Some(current);
        if attempt + 1 == attempts {
            break;
        }
    }
    let response = response.expect("MCP request always attempts once");
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
    if let Err(response) = authenticate(&state, &name, &headers).await {
        return response;
    }
    let (mut url, _, configured, _, _, _, _) = match config(&state, &name).await {
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
    if let Err(response) = authenticate(&state, &name, &headers).await {
        return response;
    }
    let (url, _, configured, _, _, _, _) = match config(&state, &name).await {
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

#[cfg(test)]
mod tests {
    use super::{
        inject_upstream_auth, safe_client_header, safe_configured_header, select_round_robin,
        validate_jsonrpc, UpstreamKey,
    };
    use axum::http::{header, HeaderMap, HeaderName, HeaderValue};
    use serde_json::json;
    use std::collections::HashMap;

    #[test]
    fn strips_non_positive_tool_ttl_but_preserves_positive_ttl() {
        let mut zero = json!({"result":{"tools":[],"ttlMs":0}});
        assert!(super::strip_zero_ttl(&mut zero));
        assert!(zero["result"].get("ttlMs").is_none());

        let mut positive = json!({"result":{"tools":[],"ttlMs":1000}});
        assert!(!super::strip_zero_ttl(&mut positive));
        assert_eq!(positive["result"]["ttlMs"], 1000);
    }

    #[test]
    fn rejects_hop_by_hop_and_routing_headers() {
        for name in [
            "Connection",
            "Host",
            "Content-Length",
            "Proxy-Authorization",
            "Transfer-Encoding",
            "Trailers",
        ] {
            assert!(!safe_configured_header(name));
        }
    }

    #[test]
    fn permits_static_auth_and_mcp_headers() {
        assert!(safe_configured_header("authorization"));
        assert!(safe_configured_header("x-api-key"));
        assert!(safe_configured_header("mcp-session-id"));
        assert!(!safe_client_header("authorization"));
        assert!(!safe_client_header("x-api-key"));
        assert!(safe_client_header("mcp-session-id"));
    }

    #[test]
    fn selects_keys_in_stable_round_robin_order() {
        let keys = vec![
            UpstreamKey {
                id: 1,
                key: "first".into(),
            },
            UpstreamKey {
                id: 2,
                key: "second".into(),
            },
        ];
        assert_eq!(select_round_robin(None, &keys).unwrap().id, 1);
        assert_eq!(select_round_robin(Some(1), &keys).unwrap().id, 2);
        assert_eq!(select_round_robin(Some(2), &keys).unwrap().id, 1);
    }

    #[test]
    fn injects_configured_auth_without_leaking_old_auth() {
        let mut headers = HashMap::from([(String::from("authorization"), String::from("old"))]);
        headers.insert("content-type".into(), "application/json".into());
        let result = inject_upstream_auth(&headers, "bearer", "new");
        assert_eq!(
            result.get("Authorization"),
            Some(&String::from("Bearer new"))
        );
        assert!(!result.contains_key("authorization"));
        assert_eq!(
            result.get("content-type"),
            Some(&String::from("application/json"))
        );
    }

    #[test]
    fn rejects_empty_batches_and_non_object_entries_before_dispatch() {
        for body in [json!([]), json!(["not-json-rpc"])] {
            let error = validate_jsonrpc(&serde_json::to_vec(&body).unwrap()).unwrap_err();
            assert_eq!(error.status(), axum::http::StatusCode::BAD_REQUEST);
        }
    }

    #[test]
    fn rejects_wrong_version_missing_method_and_scalar_params() {
        for body in [
            json!({"jsonrpc":"1.0","id":1,"method":"tools/list"}),
            json!({"jsonrpc":"2.0","id":1}),
            json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":true}),
        ] {
            let error = validate_jsonrpc(&serde_json::to_vec(&body).unwrap()).unwrap_err();
            assert_eq!(error.status(), axum::http::StatusCode::BAD_REQUEST);
        }
    }

    #[test]
    fn extracts_method_and_tool_without_accepting_invalid_tool_params() {
        let body = json!({
            "jsonrpc":"2.0", "id":1, "method":"tools/call",
            "params":{"name":"lookup"}
        });
        assert_eq!(
            validate_jsonrpc(&serde_json::to_vec(&body).unwrap()).unwrap(),
            (Some("tools/call".into()), Some("lookup".into()))
        );

        let invalid = json!({
            "jsonrpc":"2.0", "id":1, "method":"tools/call",
            "params":{"name":42}
        });
        assert_eq!(
            validate_jsonrpc(&serde_json::to_vec(&invalid).unwrap()).unwrap(),
            (Some("tools/call".into()), None)
        );
    }

    #[test]
    fn event_stream_headers_preserve_cache_control_and_disable_buffering() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream; charset=utf-8"),
        );
        let is_event_stream = headers
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.to_ascii_lowercase().starts_with("text/event-stream"));
        assert!(is_event_stream);
        assert!(!headers.contains_key(header::CACHE_CONTROL));
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        headers.insert(
            HeaderName::from_static("x-accel-buffering"),
            HeaderValue::from_static("no"),
        );
        assert_eq!(headers[header::CACHE_CONTROL], "no-cache");
        assert_eq!(headers["x-accel-buffering"], "no");
    }

    #[test]
    fn enforces_oauth_read_write_scopes_and_legacy_unscoped_tokens() {
        assert!(super::scope_allows(Some("mcp:read"), "mcp:read"));
        assert!(!super::scope_allows(Some("mcp:read"), "mcp:write"));
        assert!(super::scope_allows(Some("mcp:write"), "mcp:read"));
        assert!(super::scope_allows(Some("mcp:write"), "mcp:write"));
        assert!(super::scope_allows(Some("mcp:read mcp:write"), "mcp:read"));
        assert!(super::scope_allows(Some("mcp:read mcp:read"), "mcp:read"));
        assert!(!super::scope_allows(Some("mcp:admin"), "mcp:read"));
        assert!(super::scope_allows(None, "mcp:write"));
        assert_eq!(super::required_scope(Some("tools/call")), "mcp:write");
        assert_eq!(super::required_scope(Some("tools/list")), "mcp:read");
        assert_eq!(super::required_scope(None), "mcp:read");
    }

    #[test]
    fn matches_oauth_resources_by_exact_path_or_relative_resource() {
        assert!(super::resource_matches("/mcp/demo", "demo"));
        assert!(super::resource_matches(
            "https://example.test/mcp/demo",
            "demo"
        ));
        assert!(!super::resource_matches(
            "https://example.test/mcp/other",
            "demo"
        ));
        assert!(!super::resource_matches("not a URL", "demo"));
        assert!(!super::resource_matches("", "demo"));
    }
}
