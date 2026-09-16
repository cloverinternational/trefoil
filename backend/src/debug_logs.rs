//! PostgreSQL-backed debug-log management.

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
            "/v0/management/debug/logs",
            axum::routing::get(list).delete(delete_all),
        )
        .route(
            "/v0/management/debug/logs/:request_id",
            axum::routing::get(get).delete(delete_one),
        )
}
#[derive(Deserialize)]
struct Page {
    limit: Option<i64>,
    offset: Option<i64>,
}
#[allow(clippy::result_large_err)]
fn principal(state: &AppState, headers: &HeaderMap) -> Result<Principal, Response> {
    resolve_principal(state,headers.get("x-admin-key").and_then(|v|v.to_str().ok())).ok_or_else(||(StatusCode::UNAUTHORIZED,Json(serde_json::json!({"error":{"message":"Unauthorized","type":"auth_error","code":401}}))).into_response())
}
async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(page): Query<Page>,
) -> Response {
    let p = match principal(&state, &headers) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let limit = page.limit.unwrap_or(50);
    let offset = page.offset.unwrap_or(0);
    let scope = match p {
        Principal::Admin => None,
        Principal::Limited { key_name, .. } => Some(key_name),
    };
    let Some(pool) = &state.inner.database else {
        return Json(serde_json::json!({"data":[],"total":0,"limit":limit,"offset":offset}))
            .into_response();
    };
    let rows = if let Some(key) = scope.as_deref() {
        sqlx::query("SELECT request_id,created_at,response_status FROM debug_logs WHERE api_key=$1 ORDER BY created_at DESC LIMIT $2 OFFSET $3").bind(key).bind(limit).bind(offset).fetch_all(pool).await
    } else {
        sqlx::query("SELECT request_id,created_at,response_status FROM debug_logs ORDER BY created_at DESC LIMIT $1 OFFSET $2").bind(limit).bind(offset).fetch_all(pool).await
    };
    let total: Result<i64, _> = if let Some(key) = scope.as_deref() {
        sqlx::query_scalar("SELECT count(*) FROM debug_logs WHERE api_key=$1")
            .bind(key)
            .fetch_one(pool)
            .await
    } else {
        sqlx::query_scalar("SELECT count(*) FROM debug_logs")
            .fetch_one(pool)
            .await
    };
    let (Ok(rows), Ok(total)) = (rows, total) else {
        return Json(serde_json::json!({"data":[],"total":0,"limit":limit,"offset":offset}))
            .into_response();
    };
    let data:Vec<_>=rows.into_iter().map(|r|serde_json::json!({"requestId":r.try_get::<String,_>("request_id").unwrap_or_default(),"createdAt":r.try_get::<i64,_>("created_at").unwrap_or_default(),"responseStatus":r.try_get::<Option<i32>,_>("response_status").unwrap_or(None)})).collect();
    Json(serde_json::json!({"data":data,"total":total,"limit":limit,"offset":offset}))
        .into_response()
}
async fn get(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let p = match principal(&state, &headers) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let Some(pool) = &state.inner.database else {
        return not_found("Log not found");
    };
    if let Principal::Limited { key_name, .. } = &p {
        let owner: Result<Option<Option<String>>, _> =
            sqlx::query_scalar("SELECT api_key FROM debug_logs WHERE request_id=$1 LIMIT 1")
                .bind(&id)
                .fetch_optional(pool)
                .await;
        if !matches!(owner,Ok(Some(Some(owner)))if owner==*key_name) {
            return not_found("Log not found");
        }
    }
    let row=sqlx::query("SELECT request_id,created_at,raw_request,transformed_request,raw_response,transformed_response,raw_response_snapshot,transformed_response_snapshot,request_headers,response_headers,response_status FROM debug_logs WHERE request_id=$1 LIMIT 1").bind(&id).fetch_optional(pool).await;
    let Ok(Some(r)) = row else {
        return not_found("Log not found");
    };
    Json(serde_json::json!({"requestId":r.try_get::<String,_>("request_id").unwrap_or_default(),"createdAt":r.try_get::<i64,_>("created_at").unwrap_or_default(),"rawRequest":r.try_get::<Option<String>,_>("raw_request").unwrap_or(None),"transformedRequest":r.try_get::<Option<String>,_>("transformed_request").unwrap_or(None),"rawResponse":r.try_get::<Option<String>,_>("raw_response").unwrap_or(None),"transformedResponse":r.try_get::<Option<String>,_>("transformed_response").unwrap_or(None),"rawResponseSnapshot":r.try_get::<Option<String>,_>("raw_response_snapshot").unwrap_or(None),"transformedResponseSnapshot":r.try_get::<Option<String>,_>("transformed_response_snapshot").unwrap_or(None),"requestHeaders":r.try_get::<Option<String>,_>("request_headers").unwrap_or(None),"responseHeaders":r.try_get::<Option<String>,_>("response_headers").unwrap_or(None),"responseStatus":r.try_get::<Option<i32>,_>("response_status").unwrap_or(None)})).into_response()
}
async fn delete_all(State(state): State<AppState>, headers: HeaderMap) -> Response {
    match principal(&state,&headers){Ok(Principal::Admin)=>{},Ok(Principal::Limited{..})=>return(StatusCode::FORBIDDEN,Json(serde_json::json!({"error":{"message":"Admin privileges required","type":"forbidden","code":403}}))).into_response(),Err(r)=>return r}
    if let Some(pool) = &state.inner.database {
        if sqlx::query("DELETE FROM debug_logs")
            .execute(pool)
            .await
            .is_err()
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error":"Failed to delete logs"})),
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
    let p = match principal(&state, &headers) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let Some(pool) = &state.inner.database else {
        return match p {
            Principal::Admin => Json(serde_json::json!({"success":true})).into_response(),
            _ => not_found("Log not found or could not be deleted"),
        };
    };
    if let Principal::Limited { key_name, .. } = &p {
        let owner: Result<Option<Option<String>>, _> =
            sqlx::query_scalar("SELECT api_key FROM debug_logs WHERE request_id=$1 LIMIT 1")
                .bind(&id)
                .fetch_optional(pool)
                .await;
        if !matches!(owner,Ok(Some(Some(owner)))if owner==*key_name) {
            return not_found("Log not found or could not be deleted");
        }
    }
    match sqlx::query("DELETE FROM debug_logs WHERE request_id=$1")
        .bind(id)
        .execute(pool)
        .await
    {
        Ok(_) => Json(serde_json::json!({"success":true})).into_response(),
        Err(_) => not_found("Log not found or could not be deleted"),
    }
}
fn not_found(message: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({"error":message})),
    )
        .into_response()
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
    async fn empty_and_missing_shapes() {
        let r = router()
            .with_state(fixture())
            .oneshot(
                Request::get("/v0/management/debug/logs?limit=5&offset=2")
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
                Request::get("/v0/management/debug/logs/x")
                    .header("x-admin-key", "admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        let r = router()
            .with_state(fixture())
            .oneshot(
                Request::delete("/v0/management/debug/logs/x")
                    .header("x-admin-key", "admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
    }
    #[tokio::test]
    async fn delete_all_limited_forbidden() {
        let r = router()
            .with_state(fixture())
            .oneshot(
                Request::delete("/v0/management/debug/logs")
                    .header("x-admin-key", "secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
    }
}
