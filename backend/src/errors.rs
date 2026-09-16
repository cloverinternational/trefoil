//! PostgreSQL-backed inference error management.

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json, Router,
};
use serde::Deserialize;
use sqlx::Row;

use crate::{
    auth::{resolve_principal, Principal},
    AppState,
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/v0/management/errors",
            axum::routing::get(list_errors).delete(delete_all_errors),
        )
        .route(
            "/v0/management/errors/:request_id",
            axum::routing::delete(delete_error),
        )
}

#[derive(Deserialize)]
struct ErrorQuery {
    limit: Option<i64>,
    offset: Option<i64>,
}

#[allow(clippy::result_large_err)]
fn principal(state: &AppState, headers: &HeaderMap) -> Result<Principal, Response> {
    let key = headers
        .get("x-admin-key")
        .and_then(|value| value.to_str().ok());
    resolve_principal(state, key).ok_or_else(|| {
        (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "error": { "message": "Unauthorized", "type": "auth_error", "code": 401 }
            })),
        )
            .into_response()
    })
}

async fn list_errors(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ErrorQuery>,
) -> Response {
    let principal = match principal(&state, &headers) {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    let limit = query.limit.unwrap_or(50);
    let offset = query.offset.unwrap_or(0);
    let scope = match principal {
        Principal::Admin => None,
        Principal::Limited { key_name, .. } => Some(key_name),
    };
    let Some(pool) = &state.inner.database else {
        return Json(serde_json::json!({
            "data": [], "total": 0, "limit": limit, "offset": offset
        }))
        .into_response();
    };
    let rows = if let Some(key) = scope.as_deref() {
        sqlx::query("SELECT id, request_id, date, error_message, error_stack, details, created_at, api_key FROM inference_errors WHERE api_key = $1 ORDER BY created_at DESC LIMIT $2 OFFSET $3")
            .bind(key).bind(limit).bind(offset).fetch_all(pool).await
    } else {
        sqlx::query("SELECT id, request_id, date, error_message, error_stack, details, created_at, api_key FROM inference_errors ORDER BY created_at DESC LIMIT $1 OFFSET $2")
            .bind(limit).bind(offset).fetch_all(pool).await
    };
    let total: Result<i64, sqlx::Error> = if let Some(key) = scope.as_deref() {
        sqlx::query_scalar("SELECT count(*) FROM inference_errors WHERE api_key = $1")
            .bind(key)
            .fetch_one(pool)
            .await
    } else {
        sqlx::query_scalar("SELECT count(*) FROM inference_errors")
            .fetch_one(pool)
            .await
    };
    let (Ok(rows), Ok(total)) = (rows, total) else {
        return Json(serde_json::json!({
            "data": [], "total": 0, "limit": limit, "offset": offset
        }))
        .into_response();
    };
    let data: Vec<_> = rows
        .into_iter()
        .map(|row| {
            serde_json::json!({
                "id": row.try_get::<i32, _>("id").unwrap_or_default(),
                "requestId": row.try_get::<String, _>("request_id").unwrap_or_default(),
                "date": row.try_get::<String, _>("date").unwrap_or_default(),
                "errorMessage": row.try_get::<Option<String>, _>("error_message").unwrap_or(None),
                "errorStack": row.try_get::<Option<String>, _>("error_stack").unwrap_or(None),
                "details": row.try_get::<Option<String>, _>("details").unwrap_or(None),
                "createdAt": row.try_get::<i64, _>("created_at").unwrap_or_default(),
                "apiKey": row.try_get::<Option<String>, _>("api_key").unwrap_or(None),
            })
        })
        .collect();
    Json(serde_json::json!({ "data": data, "total": total, "limit": limit, "offset": offset }))
        .into_response()
}

async fn delete_all_errors(State(state): State<AppState>, headers: HeaderMap) -> Response {
    match principal(&state, &headers) {
        Ok(Principal::Admin) => {}
        Ok(Principal::Limited { .. }) => {
            return (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({
                    "error": { "message": "Admin privileges required", "type": "forbidden", "code": 403 }
                })),
            )
                .into_response();
        }
        Err(response) => return response,
    }
    if let Some(pool) = &state.inner.database {
        if sqlx::query("DELETE FROM inference_errors")
            .execute(pool)
            .await
            .is_err()
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "Failed to delete error logs" })),
            )
                .into_response();
        }
    }
    Json(serde_json::json!({ "success": true })).into_response()
}

async fn delete_error(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(request_id): Path<String>,
) -> Response {
    let principal = match principal(&state, &headers) {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    let Some(pool) = &state.inner.database else {
        return match principal {
            Principal::Admin => Json(serde_json::json!({ "success": true })).into_response(),
            Principal::Limited { .. } => not_found(),
        };
    };
    if let Principal::Limited { key_name, .. } = &principal {
        let owner: Result<Option<Option<String>>, _> = sqlx::query_scalar(
            "SELECT api_key FROM inference_errors WHERE request_id = $1 LIMIT 1",
        )
        .bind(&request_id)
        .fetch_optional(pool)
        .await;
        if !matches!(owner, Ok(Some(Some(owner))) if owner == *key_name) {
            return not_found();
        }
    }
    match sqlx::query("DELETE FROM inference_errors WHERE request_id = $1")
        .bind(&request_id)
        .execute(pool)
        .await
    {
        Ok(_) => Json(serde_json::json!({ "success": true })).into_response(),
        Err(_) => not_found(),
    }
}

fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "error": "Error log not found or could not be deleted" })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::KeyConfig;
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use std::collections::HashMap;
    use tower::ServiceExt;

    fn fixture() -> AppState {
        let mut keys = HashMap::new();
        keys.insert(
            "limited".into(),
            KeyConfig {
                secret: "secret".into(),
                ..Default::default()
            },
        );
        AppState::new(Some("admin".into()), keys)
    }

    #[tokio::test]
    async fn empty_list_and_admin_idempotent_delete_shapes() {
        let list = router()
            .with_state(fixture())
            .oneshot(
                Request::get("/v0/management/errors?limit=5&offset=2")
                    .header("x-admin-key", "admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&list.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(
            body,
            serde_json::json!({ "data": [], "total": 0, "limit": 5, "offset": 2 })
        );

        let deleted = router()
            .with_state(fixture())
            .oneshot(
                Request::delete("/v0/management/errors/missing")
                    .header("x-admin-key", "admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(deleted.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn limited_delete_all_is_forbidden_and_missing_id_is_hidden() {
        let all = router()
            .with_state(fixture())
            .oneshot(
                Request::delete("/v0/management/errors")
                    .header("x-admin-key", "secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(all.status(), StatusCode::FORBIDDEN);
        let one = router()
            .with_state(fixture())
            .oneshot(
                Request::delete("/v0/management/errors/missing")
                    .header("x-admin-key", "secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(one.status(), StatusCode::NOT_FOUND);
    }
}
