//! Administrative MCP OAuth client/token controls.

use crate::{
    auth::{resolve_principal, Principal},
    AppState,
};
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json, Router,
};
use sqlx::Row;

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/v0/management/mcp-oauth/clients",
            axum::routing::get(list_clients),
        )
        .route(
            "/v0/management/mcp-oauth/clients/:client_id",
            axum::routing::delete(delete_client).patch(update_client),
        )
        .route(
            "/v0/management/mcp-oauth/clients/:client_id/revoke",
            axum::routing::post(revoke_client),
        )
        .route(
            "/v0/management/mcp-oauth/tokens/:id/revoke",
            axum::routing::post(revoke_token),
        )
}
fn admin(state: &AppState, headers: &HeaderMap) -> Result<(), Response> {
    match resolve_principal(
        state,
        headers.get("x-admin-key").and_then(|v| v.to_str().ok()),
    ) {
        Some(Principal::Admin) => Ok(()),
        Some(Principal::Limited { .. }) => Err((
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error":"Admin privileges required"})),
        )
            .into_response()),
        None => Err((
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error":"Unauthorized"})),
        )
            .into_response()),
    }
}
async fn list_clients(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(r) = admin(&state, &headers) {
        return r;
    }
    let Some(pool) = &state.inner.database else {
        return Json(serde_json::json!({"clients":[]})).into_response();
    };
    let Ok(rows)=sqlx::query("SELECT client_id,client_name,status,created_at FROM mcp_oauth_clients ORDER BY created_at DESC").fetch_all(pool).await else{return (StatusCode::INTERNAL_SERVER_ERROR,Json(serde_json::json!({"error":"Internal server error"}))).into_response()};
    Json(serde_json::json!({"clients":rows.into_iter().map(|r|serde_json::json!({"clientId":r.try_get::<String,_>("client_id").unwrap_or_default(),"clientName":r.try_get::<Option<String>,_>("client_name").unwrap_or(None),"status":r.try_get::<String,_>("status").unwrap_or_default(),"createdAt":r.try_get::<i64,_>("created_at").unwrap_or_default()})).collect::<Vec<_>>() })).into_response()
}
async fn update_client(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(client_id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    if let Err(r) = admin(&state, &headers) {
        return r;
    }
    let Some(status) = body
        .get("status")
        .and_then(|v| v.as_str())
        .filter(|v| *v == "active" || *v == "disabled")
    else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":"Invalid status"})),
        )
            .into_response();
    };
    let Some(pool) = &state.inner.database else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error":"OAuth client not found"})),
        )
            .into_response();
    };
    match sqlx::query("UPDATE mcp_oauth_clients SET status=$1,updated_at=$2 WHERE client_id=$3")
        .bind(status)
        .bind(chrono::Utc::now().timestamp_millis())
        .bind(&client_id)
        .execute(pool)
        .await
    {
        Ok(r) if r.rows_affected() > 0 => {
            Json(serde_json::json!({"success":true,"clientId":client_id,"status":status}))
                .into_response()
        }
        _ => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error":"OAuth client not found"})),
        )
            .into_response(),
    }
}
async fn revoke_client(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(client_id): Path<String>,
) -> Response {
    if let Err(r) = admin(&state, &headers) {
        return r;
    }
    let Some(pool) = &state.inner.database else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error":"OAuth client not found"})),
        )
            .into_response();
    };
    let revoked=sqlx::query("UPDATE mcp_oauth_tokens SET revoked_at=$1,updated_at=$1 WHERE client_id=$2 AND revoked_at IS NULL").bind(chrono::Utc::now().timestamp_millis()).bind(&client_id).execute(pool).await;
    match revoked{Ok(r)=>Json(serde_json::json!({"success":true,"clientId":client_id,"revokedCount":r.rows_affected()})).into_response(),Err(_)=>(StatusCode::INTERNAL_SERVER_ERROR,Json(serde_json::json!({"error":"Internal server error"}))).into_response()}
}
async fn delete_client(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(client_id): Path<String>,
) -> Response {
    let _ = revoke_client(
        State(state.clone()),
        headers.clone(),
        Path(client_id.clone()),
    )
    .await;
    let Some(pool) = &state.inner.database else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error":"OAuth client not found"})),
        )
            .into_response();
    };
    match sqlx::query("DELETE FROM mcp_oauth_clients WHERE client_id=$1")
        .bind(&client_id)
        .execute(pool)
        .await
    {
        Ok(r) if r.rows_affected() > 0 => {
            Json(serde_json::json!({"success":true,"clientId":client_id})).into_response()
        }
        _ => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error":"OAuth client not found"})),
        )
            .into_response(),
    }
}
async fn revoke_token(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(r) = admin(&state, &headers) {
        return r;
    }
    let Ok(id) = id.parse::<i32>() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":"Invalid token id"})),
        )
            .into_response();
    };
    let Some(pool) = &state.inner.database else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error":"Active OAuth token not found"})),
        )
            .into_response();
    };
    match sqlx::query("UPDATE mcp_oauth_tokens SET revoked_at=$1,updated_at=$1 WHERE id=$2 AND revoked_at IS NULL").bind(chrono::Utc::now().timestamp_millis()).bind(id).execute(pool).await{Ok(r) if r.rows_affected()>0=>Json(serde_json::json!({"success":true,"revokedCount":r.rows_affected()})).into_response(),_=>(StatusCode::NOT_FOUND,Json(serde_json::json!({"error":"Active OAuth token not found"}))).into_response()}
}
