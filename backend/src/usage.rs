//! PostgreSQL-backed usage management endpoints (initial subset).

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{
        sse::{Event, Sse},
        IntoResponse, Response,
    },
    Json, Router,
};
use futures_util::stream;
use serde::Deserialize;
use serde_json::Value;
use sqlx::Row;
use std::{convert::Infallible, time::Duration};
use tokio::sync::broadcast;

use crate::{
    auth::{resolve_principal, Principal},
    AppState,
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/v0/management/events", axum::routing::get(events))
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
        .route(
            "/v0/management/logs/reset",
            axum::routing::delete(reset_logs),
        )
}

#[derive(Deserialize, Debug, Clone)]
pub struct UsageEvent {
    pub kind: String,
    pub id: i64,
    pub api_key: Option<String>,
    pub data: Value,
}

pub fn publish_event(state: &AppState, event: UsageEvent) {
    if let Some(sender) = &state.inner.usage_events {
        let _ = sender.send(event);
    }
}

// Unscoped activity must never leak to limited principals. Heartbeats are
// generated separately and contain no request data.
fn event_visible_to(principal: &Principal, event: &UsageEvent) -> bool {
    match principal {
        Principal::Admin => true,
        Principal::Limited { key_name, .. } => event.api_key.as_ref() == Some(key_name),
    }
}

async fn events(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let principal = match admin(&state, &headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let Some(sender) = &state.inner.usage_events else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "usage event stream unavailable",
        )
            .into_response();
    };
    let receiver = sender.subscribe();
    let stream = stream::unfold(
        (
            receiver,
            principal,
            tokio::time::interval_at(
                tokio::time::Instant::now() + Duration::from_secs(10),
                Duration::from_secs(10),
            ),
        ),
        |(mut receiver, principal, mut interval)| async move {
            loop {
                tokio::select! {
                    result = receiver.recv() => { let event = match result { Ok(event) => event, Err(broadcast::error::RecvError::Lagged(_)) => UsageEvent { kind: "ping".into(), id: chrono::Utc::now().timestamp_millis(), api_key: None, data: serde_json::json!("subscriber lagged") }, Err(broadcast::error::RecvError::Closed) => return None }; if !event_visible_to(&principal, &event) { continue; } let sse = Event::default().event(event.kind).id(event.id.to_string()).json_data(event.data).unwrap_or_else(|_| Event::default().event("ping").data("pong")); return Some((Ok::<Event, Infallible>(sse), (receiver, principal, interval))); }
                    _ = interval.tick() => { let id = chrono::Utc::now().timestamp_millis(); return Some((Ok::<Event, Infallible>(Event::default().event("ping").id(id.to_string()).data("pong")), (receiver, principal, interval))); }
                }
            }
        },
    );
    let mut response = Sse::new(stream).into_response();
    response
        .headers_mut()
        .insert("access-control-allow-origin", "*".parse().unwrap());
    response
        .headers_mut()
        .insert("cache-control", "no-cache".parse().unwrap());
    response
        .headers_mut()
        .insert("connection", "keep-alive".parse().unwrap());
    response
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

async fn reset_logs(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    if let Some(pool) = &state.inner.database {
        let mut transaction = match pool.begin().await {
            Ok(transaction) => transaction,
            Err(_) => return reset_error(),
        };
        for statement in [
            "DELETE FROM request_usage",
            "DELETE FROM inference_errors",
            "DELETE FROM debug_logs",
        ] {
            if sqlx::query(statement)
                .execute(&mut *transaction)
                .await
                .is_err()
            {
                let _ = transaction.rollback().await;
                return reset_error();
            }
        }
        if transaction.commit().await.is_err() {
            return reset_error();
        }
    }
    Json(serde_json::json!({
        "success": true,
        "message": "All logs have been reset successfully"
    }))
    .into_response()
}

fn reset_error() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({"error":"Failed to reset logs"})),
    )
        .into_response()
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

#[derive(Deserialize, Debug)]
struct SummaryQuery {
    range: Option<String>,
    #[serde(rename = "startDate")]
    start_date: Option<String>,
    #[serde(rename = "endDate")]
    end_date: Option<String>,
    breakdowns: Option<String>,
    #[serde(rename = "breakdownLimit")]
    breakdown_limit: Option<i64>,
}

fn summary_empty(range: &str) -> Value {
    serde_json::json!({"range":range,"series":[],"stats":{"totalRequests":0,"totalErrors":0,"totalTokens":0,"inputTokens":0,"outputTokens":0,"reasoningTokens":0,"cachedTokens":0,"cacheWriteTokens":0,"totalCost":0.0,"avgDurationMs":0.0,"totalDurationMs":0,"avgTtftMs":0.0,"avgTokensPerSec":0.0,"successRate":0.0},"today":{"requests":0,"inputTokens":0,"outputTokens":0,"reasoningTokens":0,"cachedTokens":0,"cacheWriteTokens":0,"totalCost":0.0}})
}

async fn summary_usage(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<SummaryQuery>,
) -> Response {
    let principal = match admin(&state, &headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let scope = match principal {
        Principal::Admin => None,
        Principal::Limited { key_name, .. } => Some(key_name),
    };
    let range = query.range.as_deref().unwrap_or("day");
    if !matches!(range, "hour" | "day" | "week" | "month" | "custom") {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error":"Invalid range"}))).into_response();
    }
    let (start, end) = if range == "custom" {
        let (Some(start), Some(end)) = (query.start_date.as_deref(), query.end_date.as_deref()) else {
            return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error":"startDate and endDate are required for custom range"}))).into_response();
        };
        let parse = |s: &str| chrono::DateTime::parse_from_rfc3339(s).ok().map(|v| v.timestamp_millis());
        let (Some(start), Some(end)) = (parse(start), parse(end)) else {
            return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error":"Invalid date format"}))).into_response();
        };
        if end < start { return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error":"endDate must be after startDate"}))).into_response(); }
        if end > chrono::Utc::now().timestamp_millis() { return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error":"endDate cannot be in the future"}))).into_response(); }
        if end - start > 366 * 24 * 60 * 60 * 1000 { return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error":"custom range cannot exceed 12 months"}))).into_response(); }
        (start, end)
    } else {
        let end = chrono::Utc::now().timestamp_millis();
        let duration = match range { "hour" => 60*60*1000, "day" => 24*60*60*1000, "week" => 7*24*60*60*1000, _ => 30*24*60*60*1000 };
        (end - duration, end)
    };
    let step = match range { "hour" => 60_000, "day" => 3_600_000, "week" | "month" => 86_400_000, _ => ((end-start)/100).max(60_000) };
    let breakdowns: Vec<&str> = query.breakdowns.as_deref().unwrap_or("").split(',').map(str::trim).filter(|v| !v.is_empty()).collect();
    if breakdowns.iter().any(|v| !matches!(*v, "provider" | "modelAlias" | "apiKey" | "status")) || breakdowns.len() > 3 {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error":"Unsupported or excessive breakdown"}))).into_response();
    }
    let limit = query.breakdown_limit.unwrap_or(10);
    if !(1..=50).contains(&limit) { return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error":"breakdownLimit must be an integer between 1 and 50"}))).into_response(); }
    let Some(pool) = &state.inner.database else {
        return Json(summary_empty(range)).into_response();
    };
    let aggregate_sql = "SELECT count(*)::bigint requests, count(*) FILTER (WHERE response_status IS NULL OR response_status != 'success')::bigint errors, coalesce(sum(tokens_input),0)::bigint input_tokens, coalesce(sum(tokens_output),0)::bigint output_tokens, coalesce(sum(tokens_reasoning),0)::bigint reasoning_tokens, coalesce(sum(tokens_cached),0)::bigint cached_tokens, coalesce(sum(tokens_cache_write),0)::bigint cache_write_tokens, coalesce(sum(cost_total),0)::double precision total_cost, coalesce(sum(duration_ms),0)::bigint total_duration_ms, coalesce(avg(duration_ms),0)::double precision avg_duration_ms, coalesce(sum(ttft_ms),0)::double precision total_ttft_ms, coalesce(avg(ttft_ms),0)::double precision avg_ttft_ms, coalesce(avg(tokens_per_sec),0)::double precision avg_tokens_per_sec FROM request_usage WHERE start_time >= $1 AND start_time <= $2 AND ($3::text IS NULL OR api_key = $3)";
    let Ok(row) = sqlx::query(aggregate_sql).bind(start).bind(end).bind(scope.as_deref()).fetch_one(pool).await else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error":"Failed to read usage summary"})),
        )
            .into_response();
    };
    let n = |name| row.try_get::<i64,_>(name).unwrap_or(0);
    let f = |name| row.try_get::<f64,_>(name).unwrap_or(0.0);
    let requests=n("requests"); let errors=n("errors");
    let total_tokens=n("input_tokens")+n("output_tokens")+n("reasoning_tokens")+n("cached_tokens")+n("cache_write_tokens");
    let stats=serde_json::json!({"totalRequests":requests,"totalErrors":errors,"totalTokens":total_tokens,"inputTokens":n("input_tokens"),"outputTokens":n("output_tokens"),"reasoningTokens":n("reasoning_tokens"),"cachedTokens":n("cached_tokens"),"cacheWriteTokens":n("cache_write_tokens"),"totalCost":f("total_cost"),"avgDurationMs":f("avg_duration_ms"),"totalDurationMs":n("total_duration_ms"),"avgTtftMs":f("avg_ttft_ms"),"avgTokensPerSec":f("avg_tokens_per_sec"),"successRate":if requests > 0 {(requests-errors) as f64 * 100.0 / requests as f64} else {0.0}});
    let series_rows = sqlx::query("SELECT (floor(start_time::double precision / $4) * $4)::bigint bucket, count(*)::bigint requests, count(*) FILTER (WHERE response_status IS NULL OR response_status != 'success')::bigint errors, coalesce(sum(tokens_input),0)::bigint input_tokens, coalesce(sum(tokens_output),0)::bigint output_tokens, coalesce(sum(tokens_reasoning),0)::bigint reasoning_tokens, coalesce(sum(tokens_cached),0)::bigint cached_tokens, coalesce(sum(tokens_cache_write),0)::bigint cache_write_tokens, coalesce(sum(cost_total),0)::double precision total_cost, coalesce(avg(duration_ms),0)::double precision avg_duration_ms, coalesce(avg(ttft_ms),0)::double precision avg_ttft_ms, coalesce(avg(tokens_per_sec),0)::double precision avg_tokens_per_sec FROM request_usage WHERE start_time >= $1 AND start_time <= $2 AND ($3::text IS NULL OR api_key = $3) GROUP BY bucket ORDER BY bucket").bind(start).bind(end).bind(scope.as_deref()).bind(step).fetch_all(pool).await.unwrap_or_default();
    let series: Vec<Value> = series_rows.iter().map(|r| { let i=|x|r.try_get::<i64,_>(x).unwrap_or(0); let x=|x|r.try_get::<f64,_>(x).unwrap_or(0.0); serde_json::json!({"bucketStartMs":i("bucket"),"requests":i("requests"),"errors":i("errors"),"inputTokens":i("input_tokens"),"outputTokens":i("output_tokens"),"reasoningTokens":i("reasoning_tokens"),"cachedTokens":i("cached_tokens"),"cacheWriteTokens":i("cache_write_tokens"),"tokens":i("input_tokens")+i("output_tokens")+i("reasoning_tokens")+i("cached_tokens")+i("cache_write_tokens"),"totalCost":x("total_cost"),"avgDurationMs":x("avg_duration_ms"),"avgTtftMs":x("avg_ttft_ms"),"avgTokensPerSec":x("avg_tokens_per_sec")}) }).collect();
    let today_start = chrono::Local::now().date_naive().and_hms_opt(0,0,0).unwrap().and_utc().timestamp_millis();
    let today_row = sqlx::query(aggregate_sql).bind(today_start).bind(end).bind(scope.as_deref()).fetch_one(pool).await.ok();
    let today = today_row.map(|r| serde_json::json!({"requests":r.try_get::<i64,_>("requests").unwrap_or(0),"inputTokens":r.try_get::<i64,_>("input_tokens").unwrap_or(0),"outputTokens":r.try_get::<i64,_>("output_tokens").unwrap_or(0),"reasoningTokens":r.try_get::<i64,_>("reasoning_tokens").unwrap_or(0),"cachedTokens":r.try_get::<i64,_>("cached_tokens").unwrap_or(0),"cacheWriteTokens":r.try_get::<i64,_>("cache_write_tokens").unwrap_or(0),"totalCost":r.try_get::<f64,_>("total_cost").unwrap_or(0.0)})).unwrap_or_else(|| summary_empty(range)["today"].clone());
    Json(serde_json::json!({"range":range,"series":series,"stats":stats,"today":today})).into_response()
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

#[cfg(test)]
mod event_tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    #[tokio::test]
    async fn limited_stream_filters_foreign_and_unscoped_events_and_cleans_up() {
        let keys = [(
            "limited".into(),
            crate::state::KeyConfig {
                secret: "secret".into(),
                ..Default::default()
            },
        )]
        .into_iter()
        .collect();
        let state = AppState::new(Some("admin".into()), keys);
        let response = router()
            .with_state(state.clone())
            .oneshot(
                Request::builder()
                    .uri("/v0/management/events")
                    .header("x-admin-key", "secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        for (id, key) in [(1, None), (2, Some("other")), (3, Some("limited"))] {
            publish_event(
                &state,
                UsageEvent {
                    kind: "usage".into(),
                    id,
                    api_key: key.map(str::to_owned),
                    data: serde_json::json!({"marker": id}),
                },
            );
        }
        let mut body = response.into_body();
        let frame = tokio::time::timeout(Duration::from_secs(1), body.frame())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let text = std::str::from_utf8(frame.data_ref().unwrap()).unwrap();
        assert!(text.contains("id: 3"), "{text}");
        assert!(!text.contains("id: 1") && !text.contains("id: 2"));
        drop(body);
        assert_eq!(
            state.inner.usage_events.as_ref().unwrap().receiver_count(),
            0
        );
    }
}
