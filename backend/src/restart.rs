//! Graceful restart request contract.

use crate::{
    auth::{resolve_principal, Principal},
    AppState,
};
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json, Router,
};

pub fn router() -> Router<AppState> {
    Router::new().route("/v0/management/restart", axum::routing::post(restart))
}
async fn restart(State(state): State<AppState>, headers: HeaderMap) -> Response {
    match resolve_principal(&state,headers.get("x-admin-key").and_then(|v|v.to_str().ok())){Some(Principal::Admin)=>Json(serde_json::json!({"success":true,"message":"Server is restarting"})).into_response(),Some(Principal::Limited{..})=>(StatusCode::FORBIDDEN,Json(serde_json::json!({"error":{"message":"Admin privileges required","type":"forbidden","code":403}}))).into_response(),None=>(StatusCode::UNAUTHORIZED,Json(serde_json::json!({"error":{"message":"Unauthorized","type":"auth_error","code":401}}))).into_response()}
}
