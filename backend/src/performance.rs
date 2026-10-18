//! PostgreSQL-backed provider performance management.
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
    Router::new().route(
        "/v0/management/performance",
        axum::routing::get(get_performance).delete(delete_performance),
    )
}
#[derive(Deserialize)]
struct PerformanceQuery {
    provider: Option<String>,
    model: Option<String>,
}
#[allow(clippy::result_large_err)]
fn require_admin(state: &AppState, headers: &HeaderMap) -> Result<(), Response> {
    let key = headers.get("x-admin-key").and_then(|v| v.to_str().ok());
    match resolve_principal(state,key) {
        Some(Principal::Admin)=>Ok(()),
        Some(Principal::Limited{..})=>Err((StatusCode::FORBIDDEN,Json(serde_json::json!({"error":{"message":"Admin privileges required","type":"forbidden","code":403}}))).into_response()),
        None=>Err((StatusCode::UNAUTHORIZED,Json(serde_json::json!({"error":{"message":"Unauthorized","type":"auth_error","code":401}}))).into_response()),
    }
}
async fn get_performance(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<PerformanceQuery>,
) -> Response {
    if let Err(r) = require_admin(&state, &headers) {
        return r;
    }
    // `usageStorage.getProviderPerformance(provider, model)`.
    let Some(storage) = crate::usage::usage_storage(&state) else {
        return Json(serde_json::Value::Array(Vec::new())).into_response();
    };
    let rows = storage.get_provider_performance(q.provider.as_deref(), q.model.as_deref()).await;
    Json(serde_json::Value::Array(rows.iter().map(|row| row.to_json()).collect())).into_response()
}
async fn delete_performance(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<PerformanceQuery>,
) -> Response {
    if let Err(r) = require_admin(&state, &headers) {
        return r;
    }
    let Some(model) = q.model else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":"Model parameter is required"})),
        )
            .into_response();
    };
    if let Some(pool) = &state.inner.database {
        if sqlx::query(
            "DELETE FROM provider_performance WHERE coalesce(canonical_model_name,model)=$1",
        )
        .bind(&model)
        .execute(pool)
        .await
        .is_err()
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error":"Failed to delete performance data"})),
            )
                .into_response();
        }
    }
    Json(serde_json::json!({"success":true,"model":model})).into_response()
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::KeyConfig;
    use axum::{body::Body, http::Request};
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
    #[ignore = "performance store requires PostgreSQL"]
    async fn empty_and_delete_contracts() {
        let get = router()
            .with_state(fixture())
            .oneshot(
                Request::get("/v0/management/performance")
                    .header("x-admin-key", "admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(get.status(), StatusCode::OK);
        let missing = router()
            .with_state(fixture())
            .oneshot(
                Request::delete("/v0/management/performance")
                    .header("x-admin-key", "admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::BAD_REQUEST);
        let deleted = router()
            .with_state(fixture())
            .oneshot(
                Request::delete("/v0/management/performance?model=ghost")
                    .header("x-admin-key", "admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(deleted.status(), StatusCode::OK);
    }
    #[tokio::test]
    async fn admin_only() {
        for (key, status) in [
            (Some("secret"), StatusCode::FORBIDDEN),
            (None, StatusCode::UNAUTHORIZED),
        ] {
            let mut r = Request::get("/v0/management/performance");
            if let Some(key) = key {
                r = r.header("x-admin-key", key)
            }
            let response = router()
                .with_state(fixture())
                .oneshot(r.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), status);
        }
    }
}
