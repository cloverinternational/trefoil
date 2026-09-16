//! PostgreSQL-backed usage management endpoints (initial subset).

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json, Router,
};
use serde::Deserialize;
use serde_json::Value;
use sqlx::Row;

use crate::{
    auth::{resolve_principal, Principal},
    AppState,
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/v0/management/usage",
            axum::routing::get(list_usage).delete(clear_usage),
        )
        .route(
            "/v0/management/usage/summary",
            axum::routing::get(summary_usage),
        )
        .route(
            "/v0/management/usage/:request_id",
            axum::routing::delete(delete_usage),
        )
}

#[derive(Deserialize)]
struct UsageQuery {
    limit: Option<i64>,
    offset: Option<i64>,
    api_key: Option<String>,
}

fn admin(state: &AppState, headers: &HeaderMap) -> Result<Principal, Response> {
    let key = headers.get("x-admin-key").and_then(|v| v.to_str().ok());
    resolve_principal(state, key).ok_or_else(|| (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error":{"message":"Unauthorized","type":"auth_error","code":401}}))).into_response())
}

/// Authenticate any principal, then require Admin specifically -- matches
/// Plexus's `isLimited(request)` check on DELETE /v0/management/usage(/:id),
/// which 403s a limited key rather than allowing it to wipe usage logs.
fn require_admin(state: &AppState, headers: &HeaderMap) -> Result<(), Response> {
    match admin(state, headers)? {
        Principal::Admin => Ok(()),
        Principal::Limited { .. } => Err((
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error":{"message":"Admin privileges required","type":"forbidden","code":403}})),
        )
            .into_response()),
    }
}

async fn list_usage(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<UsageQuery>,
) -> Response {
    let principal = match admin(&state, &headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let scope = match principal {
        Principal::Admin => query.api_key,
        Principal::Limited { key_name, .. } => Some(key_name),
    };
    let Some(pool) = &state.inner.database else {
        return Json(serde_json::json!({"data":[],"total":0,"limit":query.limit.unwrap_or(50),"offset":query.offset.unwrap_or(0)})).into_response();
    };
    let limit = query.limit.unwrap_or(50).clamp(1, 500);
    let offset = query.offset.unwrap_or(0).max(0);
    let total: i64 = if let Some(key) = &scope {
        sqlx::query_scalar("SELECT count(*) FROM request_usage WHERE api_key = $1")
            .bind(key)
            .fetch_one(pool)
            .await
            .unwrap_or(0)
    } else {
        sqlx::query_scalar("SELECT count(*) FROM request_usage")
            .fetch_one(pool)
            .await
            .unwrap_or(0)
    };
    let rows = if let Some(key) = &scope {
        sqlx::query("SELECT request_id, date, api_key, provider, incoming_model_alias, response_status, start_time, duration_ms, tokens_input, tokens_output, cost_total FROM request_usage WHERE api_key = $1 ORDER BY start_time DESC LIMIT $2 OFFSET $3").bind(key).bind(limit).bind(offset).fetch_all(pool).await
    } else {
        sqlx::query("SELECT request_id, date, api_key, provider, incoming_model_alias, response_status, start_time, duration_ms, tokens_input, tokens_output, cost_total FROM request_usage ORDER BY start_time DESC LIMIT $1 OFFSET $2").bind(limit).bind(offset).fetch_all(pool).await
    };
    let Ok(rows) = rows else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error":"Failed to read usage"})),
        )
            .into_response();
    };
    let data: Vec<Value> = rows.into_iter().map(|row| serde_json::json!({
        "requestId": row.try_get::<String, _>("request_id").unwrap_or_default(), "date": row.try_get::<String, _>("date").unwrap_or_default(),
        "apiKey": row.try_get::<Option<String>, _>("api_key").unwrap_or(None), "provider": row.try_get::<Option<String>, _>("provider").unwrap_or(None),
        "incomingModelAlias": row.try_get::<Option<String>, _>("incoming_model_alias").unwrap_or(None), "responseStatus": row.try_get::<Option<String>, _>("response_status").unwrap_or(None),
        "startTime": row.try_get::<i64, _>("start_time").unwrap_or_default(), "durationMs": row.try_get::<Option<i64>, _>("duration_ms").unwrap_or(None),
        "tokensInput": row.try_get::<Option<i32>, _>("tokens_input").unwrap_or(None), "tokensOutput": row.try_get::<Option<i32>, _>("tokens_output").unwrap_or(None), "costTotal": row.try_get::<Option<f32>, _>("cost_total").unwrap_or(None)
    })).collect();
    Json(serde_json::json!({"data":data,"total":total,"limit":limit,"offset":offset}))
        .into_response()
}

async fn clear_usage(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    if let Some(pool) = &state.inner.database {
        if sqlx::query("DELETE FROM request_usage")
            .execute(pool)
            .await
            .is_err()
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error":"Failed to clear usage"})),
            )
                .into_response();
        }
    }
    Json(serde_json::json!({"success":true})).into_response()
}

async fn summary_usage(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let principal = match admin(&state, &headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let scope = match principal {
        Principal::Admin => None,
        Principal::Limited { key_name, .. } => Some(key_name),
    };
    let Some(pool) = &state.inner.database else {
        return Json(serde_json::json!({"requests":0,"errors":0,"inputTokens":0,"outputTokens":0,"totalCost":0})).into_response();
    };
    // Matches Plexus's aggregateSelection.errors definition exactly: a row
    // counts as an error when response_status is NULL or anything other
    // than the literal string 'success'. Accepted streaming responses are
    // initially successful; full terminal stream inspection is not yet ported.
    let query = if scope.is_some() {
        sqlx::query("SELECT count(*)::bigint AS requests, count(*) FILTER (WHERE response_status IS NULL OR response_status != 'success')::bigint AS errors, coalesce(sum(tokens_input),0)::bigint AS input_tokens, coalesce(sum(tokens_output),0)::bigint AS output_tokens, coalesce(sum(cost_total),0)::real AS total_cost FROM request_usage WHERE api_key = $1").bind(scope.unwrap()).fetch_one(pool).await
    } else {
        sqlx::query("SELECT count(*)::bigint AS requests, count(*) FILTER (WHERE response_status IS NULL OR response_status != 'success')::bigint AS errors, coalesce(sum(tokens_input),0)::bigint AS input_tokens, coalesce(sum(tokens_output),0)::bigint AS output_tokens, coalesce(sum(cost_total),0)::real AS total_cost FROM request_usage").fetch_one(pool).await
    };
    let Ok(row) = query else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error":"Failed to read usage summary"})),
        )
            .into_response();
    };
    Json(serde_json::json!({"requests":row.try_get::<i64,_>("requests").unwrap_or(0),"errors":row.try_get::<i64,_>("errors").unwrap_or(0),"inputTokens":row.try_get::<i64,_>("input_tokens").unwrap_or(0),"outputTokens":row.try_get::<i64,_>("output_tokens").unwrap_or(0),"totalCost":row.try_get::<f32,_>("total_cost").unwrap_or(0.0)})).into_response()
}

async fn delete_usage(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(request_id): Path<String>,
) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    let Some(pool) = &state.inner.database else {
        return Json(serde_json::json!({"success":true})).into_response();
    };
    match crate::database::delete_request_usage(pool, &request_id).await {
        Ok(()) => Json(serde_json::json!({"success":true})).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error":error.to_string()})),
        )
            .into_response(),
    }
}
