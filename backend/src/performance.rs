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
use sqlx::Row;

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
    let Some(pool) = &state.inner.database else {
        return (StatusCode::SERVICE_UNAVAILABLE,Json(serde_json::json!({"error":"Database unavailable"}))).into_response();
    };
    let rows=sqlx::query("SELECT provider,model,coalesce(avg(time_to_first_token_ms),0)::float8 avg_ttft_ms,coalesce(min(time_to_first_token_ms),0)::float4 min_ttft_ms,coalesce(max(time_to_first_token_ms),0)::float4 max_ttft_ms,coalesce(avg(tokens_per_sec),0)::float8 avg_tokens_per_sec,coalesce(min(tokens_per_sec),0)::float4 min_tokens_per_sec,coalesce(max(tokens_per_sec),0)::float4 max_tokens_per_sec,coalesce(avg(e2e_tokens_per_sec),0)::float8 avg_e2e_tokens_per_sec,coalesce(min(e2e_tokens_per_sec),0)::float4 min_e2e_tokens_per_sec,coalesce(max(e2e_tokens_per_sec),0)::float4 max_e2e_tokens_per_sec,count(*)::bigint sample_count,count(time_to_first_token_ms)::bigint ttft_sample_count,count(tokens_per_sec)::bigint tps_sample_count,count(e2e_tokens_per_sec)::bigint e2e_sample_count,coalesce(sum(success_count),0)::bigint success_count,coalesce(sum(failure_count),0)::bigint failure_count,coalesce(max(created_at),0)::bigint last_updated FROM provider_performance WHERE ($1::text IS NULL OR provider=$1) AND ($2::text IS NULL OR model=$2) GROUP BY provider,model ORDER BY avg(tokens_per_sec) DESC NULLS LAST")
        .bind(q.provider.as_deref()).bind(q.model.as_deref()).fetch_all(pool).await;
    let Ok(rows) = rows else {
        return (StatusCode::INTERNAL_SERVER_ERROR,Json(serde_json::json!({"error":"Failed to read provider performance"}))).into_response();
    };
    let mut data: Vec<_> = rows.into_iter().map(map_row).collect();
    if let Some(model) = q.model.as_deref() {
        let providers:Result<Vec<Option<String>>,_>=sqlx::query_scalar("SELECT provider FROM request_usage WHERE canonical_model_name=$1 AND ($2::text IS NULL OR provider=$2) GROUP BY provider").bind(model).bind(q.provider.as_deref()).fetch_all(pool).await;
        let Ok(providers) = providers else {
            return (StatusCode::INTERNAL_SERVER_ERROR,Json(serde_json::json!({"error":"Failed to read provider candidates"}))).into_response();
        };
        for provider in providers.into_iter().flatten() {
            if !data.iter().any(|row| row["provider"] == provider) {
                data.push(empty_row(&provider, model));
            }
        }
    }
    Json(serde_json::Value::Array(data)).into_response()
}
fn map_row(row: sqlx::postgres::PgRow) -> serde_json::Value {
    let provider = row.try_get::<String, _>("provider").unwrap_or_default();
    let model = row.try_get::<String, _>("model").unwrap_or_default();
    serde_json::json!({"provider":provider,"model":model,"target_model":model,"avg_ttft_ms":row.try_get::<f64,_>("avg_ttft_ms").unwrap_or(0.0),"min_ttft_ms":row.try_get::<f32,_>("min_ttft_ms").unwrap_or(0.0),"max_ttft_ms":row.try_get::<f32,_>("max_ttft_ms").unwrap_or(0.0),"avg_tokens_per_sec":row.try_get::<f64,_>("avg_tokens_per_sec").unwrap_or(0.0),"min_tokens_per_sec":row.try_get::<f32,_>("min_tokens_per_sec").unwrap_or(0.0),"max_tokens_per_sec":row.try_get::<f32,_>("max_tokens_per_sec").unwrap_or(0.0),"avg_e2e_tokens_per_sec":row.try_get::<f64,_>("avg_e2e_tokens_per_sec").unwrap_or(0.0),"min_e2e_tokens_per_sec":row.try_get::<f32,_>("min_e2e_tokens_per_sec").unwrap_or(0.0),"max_e2e_tokens_per_sec":row.try_get::<f32,_>("max_e2e_tokens_per_sec").unwrap_or(0.0),"sample_count":row.try_get::<i64,_>("sample_count").unwrap_or(0),"ttft_sample_count":row.try_get::<i64,_>("ttft_sample_count").unwrap_or(0),"tps_sample_count":row.try_get::<i64,_>("tps_sample_count").unwrap_or(0),"e2e_sample_count":row.try_get::<i64,_>("e2e_sample_count").unwrap_or(0),"success_count":row.try_get::<i64,_>("success_count").unwrap_or(0),"failure_count":row.try_get::<i64,_>("failure_count").unwrap_or(0),"last_updated":row.try_get::<i64,_>("last_updated").unwrap_or(0)})
}
fn empty_row(provider: &str, model: &str) -> serde_json::Value {
    serde_json::json!({"provider":provider,"model":model,"target_model":model,"avg_ttft_ms":0,"min_ttft_ms":0,"max_ttft_ms":0,"avg_tokens_per_sec":0,"min_tokens_per_sec":0,"max_tokens_per_sec":0,"avg_e2e_tokens_per_sec":0,"min_e2e_tokens_per_sec":0,"max_e2e_tokens_per_sec":0,"sample_count":0,"ttft_sample_count":0,"tps_sample_count":0,"e2e_sample_count":0,"success_count":0,"failure_count":0,"last_updated":0})
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
