use crate::AppState;
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Router,
};
use base64::Engine;
use sha2::{Digest, Sha256};

static OPENAPI: &str = include_str!("assets/openapi.json");

pub fn router() -> Router<AppState> {
    Router::new().route(
        "/.well-known/plexus/openapi.json",
        axum::routing::get(openapi),
    )
}

async fn openapi(State(_state): State<AppState>, headers: HeaderMap) -> Response {
    let hash =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(OPENAPI.as_bytes()));
    let etag = format!("\"{hash}\"");
    if headers.get("if-none-match").and_then(|v| v.to_str().ok()) == Some(etag.as_str()) {
        return (StatusCode::NOT_MODIFIED, [("etag", etag)]).into_response();
    }
    (
        [
            ("content-type", "application/json; charset=utf-8"),
            ("cache-control", "public, max-age=300, must-revalidate"),
        ],
        OPENAPI,
    )
        .into_response()
}
