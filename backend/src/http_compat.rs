//! Fastify request/response conventions from pinned Plexus `index.ts`.
//!
//! - Body parsing: `application/json` (empty or malformed bodies are 400s in
//!   Plexus's error shape), `text/plain` (string body), urlencoded (form
//!   object via `@fastify/formbody`), multipart and gzip/octet-stream (passed
//!   through); any other media type with a body is 415.
//! - Unknown routes (and unregistered methods) are 404 `{"error":"Not Found"}`.
//! - `/health` answers `OK`; `/` and `/ui` redirect to `/ui/`, which serves the
//!   built frontend when `TREFOIL_UI_DIR` (default `frontend/dist`) exists.
use axum::{
    body::{to_bytes, Body},
    extract::Request,
    http::{header, HeaderValue, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Map, Value};

const BODY_LIMIT: usize = 30 * 1024 * 1024;

fn api_error(status: StatusCode, message: &str) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
        Json(json!({"error": {"message": message, "type": "api_error"}})),
    )
        .into_response()
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, Json(json!({"error": "Not Found"}))).into_response()
}

/// `fast-querystring` parse: repeated keys collect into arrays.
fn parse_form(text: &str) -> Value {
    let mut out = Map::new();
    for (key, value) in url::form_urlencoded::parse(text.as_bytes()) {
        let value = Value::String(value.into_owned());
        match out.get_mut(key.as_ref()) {
            Some(Value::Array(items)) => items.push(value),
            Some(existing) => {
                let first = existing.take();
                *existing = Value::Array(vec![first, value]);
            }
            None => {
                out.insert(key.into_owned(), value);
            }
        }
    }
    Value::Object(out)
}

pub async fn body_parser(request: Request, next: Next) -> Response {
    let path = request.uri().path();
    if path.starts_with("/raw/")
        || matches!(
            *request.method(),
            Method::GET | Method::HEAD | Method::OPTIONS
        )
    {
        return next.run(request).await;
    }
    let content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.split(';')
                .next()
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase()
        });
    let Some(content_type) = content_type else {
        return next.run(request).await;
    };
    if content_type.starts_with("multipart/")
        || matches!(
            content_type.as_str(),
            "application/gzip" | "application/x-gzip" | "application/octet-stream"
        )
    {
        return next.run(request).await;
    }
    let (mut parts, body) = request.into_parts();
    let bytes = match to_bytes(body, BODY_LIMIT).await {
        Ok(bytes) => bytes,
        Err(_) => return (StatusCode::PAYLOAD_TOO_LARGE, [(header::CONTENT_TYPE, "application/json; charset=utf-8")], Json(json!({"error": {"message": "Your input exceeds the context window of this model. Please adjust your input and try again.", "type": "invalid_request_error", "code": "context_length_exceeded"}}))).into_response(),
    };
    let rewritten = match content_type.as_str() {
        "application/json" => {
            if bytes.is_empty() {
                return api_error(
                    StatusCode::BAD_REQUEST,
                    "Body cannot be empty when content-type is set to 'application/json'",
                );
            }
            if serde_json::from_slice::<serde::de::IgnoredAny>(&bytes).is_err() {
                return api_error(
                    StatusCode::BAD_REQUEST,
                    "Body is not valid JSON but content-type is set to 'application/json'",
                );
            }
            None
        }
        "text/plain" => Some(Value::String(String::from_utf8_lossy(&bytes).into_owned())),
        "application/x-www-form-urlencoded" => Some(parse_form(&String::from_utf8_lossy(&bytes))),
        _ if bytes.is_empty() => None,
        _ => return api_error(StatusCode::UNSUPPORTED_MEDIA_TYPE, "Unsupported Media Type"),
    };
    let bytes = match rewritten {
        Some(value) => {
            parts.headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
            let encoded = serde_json::to_vec(&value).unwrap_or_default();
            parts
                .headers
                .insert(header::CONTENT_LENGTH, HeaderValue::from(encoded.len()));
            encoded.into()
        }
        None => bytes,
    };
    next.run(Request::from_parts(parts, Body::from(bytes)))
        .await
}

/// Fastify answers an unregistered method on a known path with 404 too, and
/// labels JSON replies `application/json; charset=utf-8`.
pub async fn method_not_allowed_as_404(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    if response.status() == StatusCode::METHOD_NOT_ALLOWED {
        response = not_found();
    }
    if response
        .headers()
        .get(header::CONTENT_TYPE)
        .is_some_and(|v| v == "application/json")
    {
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json; charset=utf-8"),
        );
    }
    response
}

fn ui_dir() -> std::path::PathBuf {
    std::env::var("TREFOIL_UI_DIR")
        .map(Into::into)
        .unwrap_or_else(|_| std::path::PathBuf::from("frontend/dist"))
}

fn mime(path: &std::path::Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
    {
        "html" => "text/html; charset=utf-8",
        "js" => "application/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "ico" => "image/x-icon",
        "svg" => "image/svg+xml",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "webmanifest" => "application/manifest+json",
        _ => "application/octet-stream",
    }
}

async fn serve_file(path: std::path::PathBuf) -> Response {
    match tokio::fs::read(&path).await {
        Ok(bytes) => (
            [
                (header::CONTENT_TYPE, mime(&path)),
                (header::CACHE_CONTROL, "no-store"),
            ],
            bytes,
        )
            .into_response(),
        Err(_) => (StatusCode::NOT_FOUND, "Not Found").into_response(),
    }
}

pub async fn health() -> Response {
    ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], "OK").into_response()
}
pub async fn root_redirect() -> Response {
    (StatusCode::FOUND, [(header::LOCATION, "/ui/")]).into_response()
}
pub async fn ui_index() -> Response {
    serve_file(ui_dir().join("index.html")).await
}

/// SPA routes (no extension) resolve to the shell; assets come from disk.
pub async fn fallback(request: Request) -> Response {
    let path = request.uri().path().to_owned();
    if path.starts_with("/v1") || path.starts_with("/v0") {
        return not_found();
    }
    if path == "/ui" {
        return root_redirect().await;
    }
    if let Some(file) = path.strip_prefix("/ui/") {
        // `/ui/:filename` is single-segment; deeper paths hit the SPA fallback.
        if file.is_empty() || file.contains('/') || !file.contains('.') {
            return ui_index().await;
        }
        return serve_file(ui_dir().join(file)).await;
    }
    not_found()
}
