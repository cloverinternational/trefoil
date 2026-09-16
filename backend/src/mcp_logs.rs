//! PostgreSQL-backed MCP request-log management.
use crate::{
    auth::{resolve_principal, Principal},
    AppState,
};
use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json, Router,
};
use serde::Deserialize;
use sqlx::Row;
pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/v0/management/mcp-logs",
            axum::routing::get(list).delete(delete_all),
        )
        .route(
            "/v0/management/mcp-logs/:request_id",
            axum::routing::delete(delete_one),
        )
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Params {
    limit: Option<i64>,
    offset: Option<i64>,
    server_name: Option<String>,
    api_key: Option<String>,
    older_than_days: Option<String>,
}
#[allow(clippy::result_large_err)]
fn admin(state: &AppState, h: &HeaderMap) -> Result<(), Response> {
    match resolve_principal(state,h.get("x-admin-key").and_then(|v|v.to_str().ok())){Some(Principal::Admin)=>Ok(()),Some(Principal::Limited{..})=>Err((StatusCode::FORBIDDEN,Json(serde_json::json!({"error":{"message":"Admin privileges required","type":"forbidden","code":403}}))).into_response()),None=>Err((StatusCode::UNAUTHORIZED,Json(serde_json::json!({"error":{"message":"Unauthorized","type":"auth_error","code":401}}))).into_response())}
}
async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(p): Query<Params>,
) -> Response {
    if let Err(r) = admin(&state, &headers) {
        return r;
    }
    let limit = p.limit.unwrap_or(20);
    let offset = p.offset.unwrap_or(0);
    let Some(pool) = &state.inner.database else {
        return Json(serde_json::json!({"data":[],"total":0,"limit":limit,"offset":offset}))
            .into_response();
    };
    let rows=sqlx::query("SELECT request_id,created_at,start_time,duration_ms,server_name,upstream_url,method,jsonrpc_method,tool_name,api_key,attribution,source_ip,response_status,is_streamed,has_debug,error_code,error_message FROM mcp_request_usage WHERE ($1::text IS NULL OR server_name LIKE '%'||$1||'%') AND ($2::text IS NULL OR api_key LIKE '%'||$2||'%') ORDER BY start_time DESC LIMIT $3 OFFSET $4").bind(p.server_name.as_deref()).bind(p.api_key.as_deref()).bind(limit).bind(offset).fetch_all(pool).await;
    let total:Result<i64,_>=sqlx::query_scalar("SELECT count(*) FROM mcp_request_usage WHERE ($1::text IS NULL OR server_name LIKE '%'||$1||'%') AND ($2::text IS NULL OR api_key LIKE '%'||$2||'%')").bind(p.server_name.as_deref()).bind(p.api_key.as_deref()).fetch_one(pool).await;
    let (Ok(rows), Ok(total)) = (rows, total) else {
        return Json(serde_json::json!({"data":[],"total":0,"limit":limit,"offset":offset}))
            .into_response();
    };
    let data:Vec<_>=rows.into_iter().map(|r|{let created=r.try_get::<chrono::NaiveDateTime,_>("created_at").ok().map(|v|v.and_utc().to_rfc3339_opts(chrono::SecondsFormat::Millis,true)).unwrap_or_default();serde_json::json!({"request_id":r.try_get::<String,_>("request_id").unwrap_or_default(),"created_at":created,"start_time":r.try_get::<i64,_>("start_time").unwrap_or_default(),"duration_ms":r.try_get::<Option<i64>,_>("duration_ms").unwrap_or(None),"server_name":r.try_get::<String,_>("server_name").unwrap_or_default(),"upstream_url":r.try_get::<String,_>("upstream_url").unwrap_or_default(),"method":r.try_get::<String,_>("method").unwrap_or_default(),"jsonrpc_method":r.try_get::<Option<String>,_>("jsonrpc_method").unwrap_or(None),"tool_name":r.try_get::<Option<String>,_>("tool_name").unwrap_or(None),"api_key":r.try_get::<Option<String>,_>("api_key").unwrap_or(None),"attribution":r.try_get::<Option<String>,_>("attribution").unwrap_or(None),"source_ip":r.try_get::<Option<String>,_>("source_ip").unwrap_or(None),"response_status":r.try_get::<Option<i32>,_>("response_status").unwrap_or(None),"is_streamed":r.try_get::<i32,_>("is_streamed").unwrap_or(0)!=0,"has_debug":r.try_get::<i32,_>("has_debug").unwrap_or(0)!=0,"error_code":r.try_get::<Option<String>,_>("error_code").unwrap_or(None),"error_message":r.try_get::<Option<String>,_>("error_message").unwrap_or(None)})}).collect();
    Json(serde_json::json!({"data":data,"total":total,"limit":limit,"offset":offset}))
        .into_response()
}
async fn delete_all(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(p): Query<Params>,
) -> Response {
    if let Err(r) = admin(&state, &headers) {
        return r;
    }
    if let Some(pool) = &state.inner.database {
        let result = if let Some(days) = p.older_than_days.and_then(|v| v.parse::<i64>().ok()) {
            let cutoff = chrono::Utc::now().timestamp_millis() - days * 86_400_000;
            sqlx::query("DELETE FROM mcp_request_usage WHERE start_time<$1")
                .bind(cutoff)
                .execute(pool)
                .await
        } else {
            sqlx::query("DELETE FROM mcp_request_usage")
                .execute(pool)
                .await
        };
        if result.is_err() {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error":"Failed to delete MCP logs"})),
            )
                .into_response();
        }
    }
    Json(serde_json::json!({"success":true})).into_response()
}
async fn delete_one(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(r) = admin(&state, &headers) {
        return r;
    }
    if let Some(pool) = &state.inner.database {
        if sqlx::query("DELETE FROM mcp_request_usage WHERE request_id=$1")
            .bind(id)
            .execute(pool)
            .await
            .is_err()
        {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error":"Log not found or could not be deleted"})),
            )
                .into_response();
        }
    }
    Json(serde_json::json!({"success":true})).into_response()
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::KeyConfig;
    use axum::{body::Body, http::Request};
    use std::collections::HashMap;
    use tower::ServiceExt;
    fn fixture() -> AppState {
        let mut k = HashMap::new();
        k.insert(
            "limited".into(),
            KeyConfig {
                secret: "secret".into(),
                ..Default::default()
            },
        );
        AppState::new(Some("admin".into()), k)
    }
    #[tokio::test]
    async fn empty_and_idempotent_shapes() {
        let r = router()
            .with_state(fixture())
            .oneshot(
                Request::get("/v0/management/mcp-logs?limit=5&offset=2")
                    .header("x-admin-key", "admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let r = router()
            .with_state(fixture())
            .oneshot(
                Request::delete("/v0/management/mcp-logs/x")
                    .header("x-admin-key", "admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
    }
    #[tokio::test]
    async fn limited_is_forbidden() {
        let r = router()
            .with_state(fixture())
            .oneshot(
                Request::get("/v0/management/mcp-logs")
                    .header("x-admin-key", "secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
    }
}
