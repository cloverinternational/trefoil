//! Runtime system log endpoints.

use crate::{
    auth::{resolve_principal, Principal},
    AppState,
};
use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json, Router,
};
use serde::Deserialize;

pub fn router() -> Router<AppState> {
    Router::new().route("/v0/system/logs/recent", axum::routing::get(recent))
}
#[derive(Deserialize)]
struct Params {
    limit: Option<usize>,
    offset: Option<usize>,
}
async fn recent(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(p): Query<Params>,
) -> Response {
    match resolve_principal(
        &state,
        headers.get("x-admin-key").and_then(|v| v.to_str().ok()),
    ) {
        Some(Principal::Admin) => {}
        Some(Principal::Limited { .. }) => {
            return (StatusCode::FORBIDDEN, "forbidden").into_response()
        }
        None => return (StatusCode::UNAUTHORIZED, "unauthorized").into_response(),
    }
    let limit = p.limit.unwrap_or(100).clamp(1, 1000);
    let offset = p.offset.unwrap_or(0);
    Json(serde_json::json!({"data":[],"total":0,"limit":limit,"offset":offset})).into_response()
}
