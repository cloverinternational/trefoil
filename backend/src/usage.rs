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
use std::{collections::HashMap, convert::Infallible, time::Duration};
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
            tokio::time::interval_at(tokio::time::Instant::now() + Duration::from_secs(1), Duration::from_secs(1)),
            std::collections::VecDeque::<Event>::new(),
        ),
        |(mut receiver, principal, mut interval, mut progress, mut pending)| async move {
            loop {
                if let Some(ev) = pending.pop_front() { return Some((Ok::<Event, Infallible>(ev), (receiver, principal, interval, progress, pending))); }
                tokio::select! {
                    result = receiver.recv() => { let event = match result { Ok(event) => event, Err(broadcast::error::RecvError::Lagged(_)) => UsageEvent { kind: "ping".into(), id: chrono::Utc::now().timestamp_millis(), api_key: None, data: serde_json::json!("subscriber lagged") }, Err(broadcast::error::RecvError::Closed) => return None }; if !event_visible_to(&principal, &event) { continue; } let kind = if event.kind == "created" { "completed".to_owned() } else { event.kind }; let sse = Event::default().event(kind).id(event.id.to_string()).json_data(event.data).unwrap_or_else(|_| Event::default().event("ping").data("pong")); return Some((Ok::<Event, Infallible>(sse), (receiver, principal, interval, progress, pending))); }
                    _ = interval.tick() => { let id = chrono::Utc::now().timestamp_millis(); return Some((Ok::<Event, Infallible>(Event::default().event("ping").id(id.to_string()).data("pong")), (receiver, principal, interval, progress, pending))); }
                    _ = progress.tick() => {
                        // Periodic progress updates for in-flight requests (`getProgressUpdates`, every 1s).
                        let scope = match &principal { Principal::Limited { key_name, .. } => Some(key_name.clone()), Principal::Admin => None };
                        for update in crate::engine::dispatcher::stall_inspector::progress_updates() {
                            if scope.as_ref().is_some_and(|k| update.get("apiKey").and_then(serde_json::Value::as_str) != Some(k.as_str())) { continue; }
                            pending.push_back(Event::default().event("progress").id(chrono::Utc::now().timestamp_millis().to_string()).data(crate::engine::js::stringify(&update)));
                        }
                        if let Some(ev) = pending.pop_front() { return Some((Ok::<Event, Infallible>(ev), (receiver, principal, interval, progress, pending))); }
                    }
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

/// `parseInt(text)` prefix parse (`None` = NaN).
pub(crate) fn parse_int(text: &str) -> Option<i64> {
    let t = text.trim_start();
    let (sign, rest) = match t.as_bytes().first() { Some(b'-') => (-1, &t[1..]), Some(b'+') => (1, &t[1..]), _ => (1, t) };
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse::<i64>().ok().map(|n| sign * n)
}

const USAGE_FIELDS: &[&str] = &["requestId", "clientRequestId", "date", "sourceIp", "apiKey", "attribution", "incomingApiType", "provider", "attemptCount", "retryHistory", "incomingModelAlias", "canonicalModelName", "selectedModelName", "finalAttemptProvider", "finalAttemptModel", "allAttemptedProviders", "upstreamModel", "outgoingApiType", "reasoningEffort", "requestedServiceTier", "requestedServiceTierRaw", "serviceTier", "serviceTierRaw", "tokensInput", "tokensOutput", "tokensReasoning", "tokensCached", "tokensCacheWrite", "tokensEstimated", "costInput", "costOutput", "costCached", "costCacheWrite", "costTotal", "costSource", "costMetadata", "startTime", "durationMs", "ttftMs", "tokensPerSec", "kwhUsed", "isStreamed", "isPassthrough", "responseStatus", "toolsDefined", "messageCount", "parallelToolCallsEnabled", "toolCallsCount", "finishReason", "hasDebug", "hasError"];

/// Process-wide engine usage storage (`UsageStorageService`).
pub(crate) fn usage_storage(state: &AppState) -> Option<std::sync::Arc<crate::engine::observability::UsageStorage>> {
    if let Some(storage) = state.inner.engine_usage.0.get() { return Some(storage.clone()); }
    crate::engine::dispatcher::engine::Engine::new(state).usage
}

fn admin(state: &AppState, headers: &HeaderMap) -> Result<Principal, Response> {
    let key = headers.get("x-admin-key").and_then(|v| v.to_str().ok());
    resolve_principal(state, key).ok_or_else(|| (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error":{"message":"Unauthorized","type":"auth_error","code":401}}))).into_response())
}

async fn reset_logs(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    crate::system_logs::event(&state,"info","[Backup] Starting logs reset (request logs, error logs, debug trace logs)",[("module".into(),serde_json::json!("backup"))]);
    if let Some(pool) = &state.inner.database {
        let mut transaction = match pool.begin().await {
            Ok(transaction) => transaction,
            Err(_) => return reset_error(),
        };
        for statement in [
            "DELETE FROM request_usage",
            "DELETE FROM inference_errors",
            "DELETE FROM debug_logs",
            "DELETE FROM mcp_debug_logs",
            "DELETE FROM mcp_request_usage",
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
    crate::system_logs::event(&state,"info","[Backup] All logs reset successfully",[("module".into(),serde_json::json!("backup"))]);
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
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let principal = match admin(&state, &headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let q = |k: &str| query.get(k).filter(|v| !v.is_empty()).cloned();
    let limit = parse_int(query.get("limit").filter(|v| !v.is_empty()).map(String::as_str).unwrap_or("50"));
    let offset = parse_int(query.get("offset").filter(|v| !v.is_empty()).map(String::as_str).unwrap_or("0"));
    let sort_by = query.get("sortBy").filter(|v| matches!(v.as_str(), "date" | "apiKey" | "provider" | "incomingModelAlias" | "costTotal" | "durationMs")).cloned();
    let sort_asc = query.get("sortDir").map(String::as_str) == Some("asc");
    let requested: Vec<&str> = query.get("fields").map(String::as_str).unwrap_or("").split(',').map(str::trim).filter(|f| USAGE_FIELDS.contains(f)).collect();
    let mut filters = crate::engine::observability::storage::UsageFilters {
        start_date: q("startDate"), end_date: q("endDate"), request_id: q("requestId"), client_request_id: q("clientRequestId"),
        api_key: q("apiKey"), incoming_api_type: q("incomingApiType"), provider: q("provider"), incoming_model_alias: q("incomingModelAlias"),
        selected_model_name: q("selectedModelName"), outgoing_api_type: q("outgoingApiType"), response_status: q("responseStatus"),
        min_duration_ms: q("minDurationMs").and_then(|v| parse_int(&v)), max_duration_ms: q("maxDurationMs").and_then(|v| parse_int(&v)),
        ..Default::default()
    };
    if let Principal::Limited { key_name, .. } = &principal {
        filters.api_key = Some(key_name.clone());
        filters.api_key_exact = true;
    }
    let limit_json = limit.map(Value::from).unwrap_or(Value::Null);
    let offset_json = offset.map(Value::from).unwrap_or(Value::Null);
    let Some(storage) = usage_storage(&state) else {
        return Json(serde_json::json!({"data":[],"total":0,"limit":limit_json,"offset":offset_json})).into_response();
    };
    let pagination = crate::engine::observability::storage::Pagination { limit: limit.unwrap_or(0), offset: offset.unwrap_or(0), sort_by, sort_asc };
    match storage.get_usage(&filters, &pagination).await {
        Ok((data, total)) => {
            let data: Vec<Value> = if requested.is_empty() { data } else {
                data.into_iter().map(|record| {
                    let mut out = serde_json::Map::new();
                    for f in &requested { if let Some(v) = record.get(*f) { out.insert((*f).into(), v.clone()); } }
                    Value::Object(out)
                }).collect()
            };
            Json(serde_json::json!({"data":data,"total":total,"limit":limit_json,"offset":offset_json})).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error":e.to_string()}))).into_response(),
    }
}

async fn clear_usage(State(state): State<AppState>, headers: HeaderMap, Query(query): Query<HashMap<String, String>>) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    // `olderThanDays` → `date <= (now - days).toISOString()`.
    let before = query.get("olderThanDays").filter(|v| !v.is_empty()).and_then(|v| parse_int(v)).map(|days| {
        let ms = local_shift(chrono::Utc::now().timestamp_millis(), 0, days);
        chrono::DateTime::from_timestamp_millis(ms).map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)).unwrap_or_default()
    });
    if let Some(pool) = &state.inner.database {
        let result = match &before {
            Some(before) => sqlx::query("DELETE FROM request_usage WHERE date <= $1").bind(before).execute(pool).await,
            None => sqlx::query("DELETE FROM request_usage").execute(pool).await,
        };
        if result.is_err() {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error":"Failed to delete usage logs"}))).into_response();
        }
    }
    Json(serde_json::json!({"success":true})).into_response()
}

const SUMMARY_CACHE_TTL_MS: i64 = 10_000;
const HISTORICAL_SUMMARY_CACHE_TTL_MS: i64 = 60_000;
const MAX_CUSTOM_RANGE_MS: i64 = 366 * 24 * 60 * 60 * 1000;
const MAX_SUMMARY_RESPONSE_BYTES: usize = 128 * 1024;
const MAX_BASIC_SUMMARY_RESPONSE_BYTES: usize = 16 * 1024;
const MAX_SUMMARY_CACHE_ENTRIES: usize = 100;
const BREAKDOWN_DIMENSIONS: [&str; 4] = ["provider", "modelAlias", "apiKey", "status"];
const SUMMARY_EXCLUSIONS: [&str; 2] = ["directModels", "probe"];

type SummaryCache = std::sync::Mutex<indexmap::IndexMap<String, (i64, Value)>>;
fn summary_cache() -> &'static SummaryCache {
    static CACHE: std::sync::OnceLock<SummaryCache> = std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

fn bad(message: impl Into<String>) -> Response {
    (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": message.into()}))).into_response()
}

/// `new Date(text).getTime()` (date-only = UTC, date-time without offset = local).
fn js_date_ms(text: &str) -> Option<i64> {
    use chrono::{DateTime, NaiveDate, NaiveDateTime, TimeZone};
    let t = text.trim();
    if let Ok(d) = DateTime::parse_from_rfc3339(t) { return Some(d.timestamp_millis()); }
    if let Ok(d) = NaiveDate::parse_from_str(t, "%Y-%m-%d") { return d.and_hms_opt(0, 0, 0).map(|v| v.and_utc().timestamp_millis()); }
    for fmt in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M", "%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%d %H:%M"] {
        if let Ok(d) = NaiveDateTime::parse_from_str(t, fmt) { return chrono::Local.from_local_datetime(&d).earliest().map(|v| v.timestamp_millis()); }
    }
    DateTime::parse_from_rfc2822(t).ok().map(|d| d.timestamp_millis())
}

/// Wall-clock local arithmetic like `Date#setHours(h - n)` / `setDate(d - n)`.
fn local_shift(ms: i64, hours: i64, days: i64) -> i64 {
    use chrono::TimeZone;
    let Some(local) = chrono::Local.timestamp_millis_opt(ms).single() else { return ms };
    let naive = local.naive_local() - chrono::Duration::hours(hours) - chrono::Duration::days(days);
    chrono::Local.from_local_datetime(&naive).earliest().map(|v| v.timestamp_millis()).unwrap_or(ms - (hours * 3_600_000 + days * 86_400_000))
}

fn local_midnight(ms: i64) -> i64 {
    use chrono::TimeZone;
    let Some(local) = chrono::Local.timestamp_millis_opt(ms).single() else { return ms };
    let naive = local.date_naive().and_hms_opt(0, 0, 0).unwrap();
    chrono::Local.from_local_datetime(&naive).earliest().map(|v| v.timestamp_millis()).unwrap_or(ms)
}

const AGGREGATE_SQL: &str = "(COUNT(*))::text AS requests, \
 (COALESCE(SUM(CASE WHEN response_status IS NULL OR response_status != 'success' THEN 1 ELSE 0 END), 0))::text AS errors, \
 (COALESCE(SUM(tokens_input), 0))::text AS input_tokens, (COALESCE(SUM(tokens_output), 0))::text AS output_tokens, \
 (COALESCE(SUM(tokens_reasoning), 0))::text AS reasoning_tokens, (COALESCE(SUM(tokens_cached), 0))::text AS cached_tokens, \
 (COALESCE(SUM(tokens_cache_write), 0))::text AS cache_write_tokens, (COALESCE(SUM(cost_total), 0))::text AS total_cost, \
 (COALESCE(SUM(COALESCE(duration_ms, 0)) * 1.0 / NULLIF(COUNT(*), 0), 0))::text AS avg_duration_ms, \
 (COALESCE(SUM(duration_ms), 0))::text AS total_duration_ms, (COALESCE(SUM(COALESCE(ttft_ms, 0)), 0))::text AS total_ttft_ms, \
 (COALESCE(SUM(COALESCE(tokens_per_sec, 0)), 0))::text AS total_tokens_per_sec, \
 (COALESCE(SUM(COALESCE(ttft_ms, 0)) * 1.0 / NULLIF(COUNT(*), 0), 0))::text AS avg_ttft_ms, \
 (COALESCE(SUM(COALESCE(tokens_per_sec, 0)) * 1.0 / NULLIF(COUNT(*), 0), 0))::text AS avg_tokens_per_sec";

/// `SummaryAggregate` (`toSummaryAggregate`).
#[derive(Default, Clone, Copy)]
struct Agg { requests: f64, errors: f64, input: f64, output: f64, reasoning: f64, cached: f64, cache_write: f64, total_tokens: f64, total_cost: f64, avg_duration: f64, total_duration: f64, total_ttft: f64, total_tps: f64, avg_ttft: f64, avg_tps: f64 }

/// `Number(text)` for a postgres text value (`null` → 0).
fn pg_num(row: &sqlx::postgres::PgRow, column: &str) -> f64 {
    row.try_get::<Option<String>, _>(column).ok().flatten().and_then(|t| t.trim().parse::<f64>().ok()).unwrap_or(0.0)
}

fn agg_from_row(row: &sqlx::postgres::PgRow) -> Agg {
    let g = |c| pg_num(row, c);
    let (input, output, reasoning, cached, cache_write) = (g("input_tokens"), g("output_tokens"), g("reasoning_tokens"), g("cached_tokens"), g("cache_write_tokens"));
    Agg { requests: g("requests"), errors: g("errors"), input, output, reasoning, cached, cache_write, total_tokens: input + output + reasoning + cached + cache_write, total_cost: g("total_cost"), avg_duration: g("avg_duration_ms"), total_duration: g("total_duration_ms"), total_ttft: g("total_ttft_ms"), total_tps: g("total_tokens_per_sec"), avg_ttft: g("avg_ttft_ms"), avg_tps: g("avg_tokens_per_sec") }
}

fn success_rate(a: &Agg) -> f64 { if a.requests > 0.0 { ((a.requests - a.errors) / a.requests) * 100.0 } else { 0.0 } }

fn summary_group(name: String, a: &Agg) -> Value {
    use crate::engine::js::num;
    serde_json::json!({"name": name, "requests": num(a.requests), "errors": num(a.errors), "inputTokens": num(a.input), "outputTokens": num(a.output), "reasoningTokens": num(a.reasoning), "cachedTokens": num(a.cached), "cacheWriteTokens": num(a.cache_write), "totalTokens": num(a.total_tokens), "totalCost": num(a.total_cost), "avgDurationMs": num(a.avg_duration), "totalDurationMs": num(a.total_duration), "avgTtftMs": num(a.avg_ttft), "avgTokensPerSec": num(a.avg_tps), "successRate": num(success_rate(a))})
}

fn subtract(t: &Agg, i: &Agg) -> Agg {
    let rem = t.requests - i.requests;
    let avg = |a: f64, b: f64| if t.requests > i.requests { (a - b).max(0.0) / rem } else { 0.0 };
    Agg { requests: rem.max(0.0), errors: (t.errors - i.errors).max(0.0), input: (t.input - i.input).max(0.0), output: (t.output - i.output).max(0.0), reasoning: (t.reasoning - i.reasoning).max(0.0), cached: (t.cached - i.cached).max(0.0), cache_write: (t.cache_write - i.cache_write).max(0.0), total_tokens: (t.total_tokens - i.total_tokens).max(0.0), total_cost: (t.total_cost - i.total_cost).max(0.0), avg_duration: avg(t.total_duration, i.total_duration), total_duration: (t.total_duration - i.total_duration).max(0.0), total_ttft: (t.total_ttft - i.total_ttft).max(0.0), total_tps: (t.total_tps - i.total_tps).max(0.0), avg_ttft: avg(t.total_ttft, i.total_ttft), avg_tps: avg(t.total_tps, i.total_tps) }
}

struct SummaryParams { range: String, start: i64, end: i64, now: i64, step_ms: i64, key: Option<String>, breakdowns: Vec<&'static str>, exclusions: Vec<&'static str>, limit: i64 }

async fn build_summary(pool: &sqlx::PgPool, p: &SummaryParams) -> Result<Value, String> {
    use crate::engine::js::num;
    let err = |e: sqlx::Error| e.to_string();
    let key_filter = if p.key.is_some() { " AND api_key = $3" } else { "" };
    let range_where = format!("start_time >= $1 AND start_time <= $2{key_filter}");
    fn bind<'q>(sql: &'q str, p: &SummaryParams) -> sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments> {
        let q = sqlx::query(sql).bind(p.start).bind(p.end);
        match &p.key { Some(k) => q.bind(k.clone()), None => q }
    }
    let bucket = format!("FLOOR(start_time::double precision / {0}) * {0}", p.step_ms);
    let series_sql = format!("SELECT ({bucket})::text AS bucket_start_ms, {AGGREGATE_SQL} FROM request_usage WHERE {range_where} GROUP BY {bucket} ORDER BY {bucket}");
    let series_rows = bind(&series_sql, p).fetch_all(pool).await.map_err(err)?;
    let stats_sql = format!("SELECT {AGGREGATE_SQL} FROM request_usage WHERE {range_where}");
    let stats_row = bind(&stats_sql, p).fetch_one(pool).await.map_err(err)?;
    let today_start = local_midnight(p.now);
    let today_sql = format!("SELECT (COUNT(*))::text AS requests, (COALESCE(SUM(tokens_input), 0))::text AS input_tokens, (COALESCE(SUM(tokens_output), 0))::text AS output_tokens, (COALESCE(SUM(tokens_reasoning), 0))::text AS reasoning_tokens, (COALESCE(SUM(tokens_cached), 0))::text AS cached_tokens, (COALESCE(SUM(tokens_cache_write), 0))::text AS cache_write_tokens, (COALESCE(SUM(cost_total), 0))::text AS total_cost FROM request_usage WHERE start_time >= $1 AND start_time <= $2{key_filter}");
    let today_q = sqlx::query(&today_sql).bind(today_start).bind(p.now);
    let today_row = match &p.key { Some(k) => today_q.bind(k.clone()), None => today_q }.fetch_one(pool).await.map_err(err)?;
    let stats = agg_from_row(&stats_row);

    let mut grouped = serde_json::Map::new();
    for dim in &p.breakdowns {
        let mut where_sql = range_where.clone();
        if *dim == "modelAlias" && p.exclusions.contains(&"directModels") { where_sql.push_str(" AND (incoming_model_alias IS NULL OR incoming_model_alias NOT LIKE 'direct/%')"); }
        if *dim == "apiKey" && p.exclusions.contains(&"probe") { where_sql.push_str(" AND (api_key IS NULL OR api_key != 'probe')"); }
        let field = match *dim { "provider" => "COALESCE(provider, 'unknown')", "modelAlias" => "COALESCE(incoming_model_alias, selected_model_name, 'unknown')", "status" => "COALESCE(response_status, 'unknown')", _ => "api_key" };
        let rows_sql = format!("SELECT {field} AS group_key, (COUNT(*) OVER ())::text AS total_dimensions, {AGGREGATE_SQL} FROM request_usage WHERE {where_sql} GROUP BY {field} ORDER BY COUNT(*) DESC, {field} LIMIT {}", p.limit);
        let rows = bind(&rows_sql, p).fetch_all(pool).await.map_err(err)?;
        let mut items: Vec<Value> = Vec::new();
        let mut included = Agg::default();
        for row in &rows {
            let key: Option<String> = row.try_get("group_key").ok().flatten();
            let name = if *dim == "apiKey" {
                match key.as_deref() { Some("probe") => "probe".to_string(), Some(k) if !k.is_empty() => if k.chars().count() > 8 { format!("{}...", k.chars().take(8).collect::<String>()) } else { k.to_string() }, _ => "unknown".to_string() }
            } else { key.unwrap_or_else(|| "unknown".into()) };
            let a = agg_from_row(row);
            items.push(summary_group(name, &a));
            included = Agg { requests: included.requests + a.requests, errors: included.errors + a.errors, input: included.input + a.input, output: included.output + a.output, reasoning: included.reasoning + a.reasoning, cached: included.cached + a.cached, cache_write: included.cache_write + a.cache_write, total_tokens: included.total_tokens + a.total_tokens, total_cost: included.total_cost + a.total_cost, total_duration: included.total_duration + a.total_duration, total_ttft: included.total_ttft + a.total_ttft, total_tps: included.total_tps + a.total_tps, avg_duration: 0.0, avg_ttft: 0.0, avg_tps: 0.0 };
        }
        let total_sql = format!("SELECT {AGGREGATE_SQL} FROM request_usage WHERE {where_sql}");
        let total_row = bind(&total_sql, p).fetch_one(pool).await.map_err(err)?;
        let grouped_total = agg_from_row(&total_row);
        let total_dimensions = rows.first().map(|r| pg_num(r, "total_dimensions")).unwrap_or(0.0);
        if total_dimensions > items.len() as f64 { items.push(summary_group("Other".into(), &subtract(&grouped_total, &included))); }
        grouped.insert((*dim).into(), serde_json::json!({"items": items, "totalDimensions": num(total_dimensions), "truncated": total_dimensions > p.limit as f64}));
    }

    let series: Vec<Value> = series_rows.iter().map(|row| {
        let a = agg_from_row(row);
        serde_json::json!({"bucketStartMs": num(pg_num(row, "bucket_start_ms")), "requests": num(a.requests), "errors": num(a.errors), "inputTokens": num(a.input), "outputTokens": num(a.output), "reasoningTokens": num(a.reasoning), "cachedTokens": num(a.cached), "cacheWriteTokens": num(a.cache_write), "tokens": num(a.total_tokens), "totalCost": num(a.total_cost), "avgDurationMs": num(a.avg_duration), "avgTtftMs": num(a.avg_ttft), "avgTokensPerSec": num(a.avg_tps)})
    }).collect();
    let t = |c| num(pg_num(&today_row, c));
    let mut response = serde_json::json!({
        "range": p.range,
        "series": series,
        "stats": {"totalRequests": num(stats.requests), "totalErrors": num(stats.errors), "totalTokens": num(stats.total_tokens), "inputTokens": num(stats.input), "outputTokens": num(stats.output), "reasoningTokens": num(stats.reasoning), "cachedTokens": num(stats.cached), "cacheWriteTokens": num(stats.cache_write), "totalCost": num(stats.total_cost), "avgDurationMs": num(stats.avg_duration), "totalDurationMs": num(stats.total_duration), "avgTtftMs": num(stats.avg_ttft), "avgTokensPerSec": num(stats.avg_tps), "successRate": num(success_rate(&stats))},
        "today": {"requests": t("requests"), "inputTokens": t("input_tokens"), "outputTokens": t("output_tokens"), "reasoningTokens": t("reasoning_tokens"), "cachedTokens": t("cached_tokens"), "cacheWriteTokens": t("cache_write_tokens"), "totalCost": t("total_cost")},
    });
    if !p.breakdowns.is_empty() { response["grouped"] = Value::Object(grouped); }
    let bytes = crate::engine::js::stringify(&response).len();
    let limit = if p.breakdowns.is_empty() { MAX_BASIC_SUMMARY_RESPONSE_BYTES } else { MAX_SUMMARY_RESPONSE_BYTES };
    if bytes > limit { return Err(format!("413:Usage summary response exceeds the {limit} byte size limit")); }
    Ok(response)
}

async fn summary_usage(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let principal = match admin(&state, &headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let q = |k: &str| query.get(k).filter(|v| !v.is_empty()).cloned();
    let range = q("range").unwrap_or_else(|| "day".into());
    let (start_str, end_str) = (q("startDate"), q("endDate"));
    if range == "custom" {
        let (Some(s), Some(e)) = (&start_str, &end_str) else { return bad("startDate and endDate are required for custom range") };
        let (Some(s), Some(e)) = (js_date_ms(s), js_date_ms(e)) else { return bad("Invalid date format") };
        if e < s { return bad("endDate must be after startDate"); }
    } else if !matches!(range.as_str(), "hour" | "day" | "week" | "month") {
        return bad("Invalid range");
    }
    let mut breakdowns: Vec<String> = Vec::new();
    for b in query.get("breakdowns").map(String::as_str).unwrap_or("").split(',').map(str::trim).filter(|v| !v.is_empty()) {
        if !breakdowns.iter().any(|x| x == b) { breakdowns.push(b.to_string()); }
    }
    if let Some(invalid) = breakdowns.iter().find(|b| !BREAKDOWN_DIMENSIONS.contains(&b.as_str())) { return bad(format!("Unsupported breakdown: {invalid}")); }
    if breakdowns.len() > 3 { return bad("At most 3 breakdowns are supported"); }
    let mut exclusions: Vec<String> = Vec::new();
    for x in query.get("exclude").map(String::as_str).unwrap_or("").split(',').map(str::trim).filter(|v| !v.is_empty()) {
        if !exclusions.iter().any(|e| e == x) { exclusions.push(x.to_string()); }
    }
    if let Some(invalid) = exclusions.iter().find(|x| !SUMMARY_EXCLUSIONS.contains(&x.as_str())) { return bad(format!("Unsupported exclusion: {invalid}")); }
    // `Number(query.breakdownLimit)`; undefined → 10.
    let parsed_limit = match query.get("breakdownLimit") {
        None => Some(10.0),
        Some(v) => { let t = v.trim(); if t.is_empty() { Some(0.0) } else { t.parse::<f64>().ok() } }
    };
    let limit = match parsed_limit { Some(n) if n.fract() == 0.0 && (1.0..=50.0).contains(&n) => n as i64, _ => return bad("breakdownLimit must be an integer between 1 and 50") };

    let current = chrono::Utc::now().timestamp_millis();
    let now = current - current.rem_euclid(60_000);
    let (start, end) = if range == "custom" {
        let (s, e) = (js_date_ms(start_str.as_deref().unwrap()).unwrap(), js_date_ms(end_str.as_deref().unwrap()).unwrap());
        if e > current { return bad("endDate cannot be in the future"); }
        if e - s > MAX_CUSTOM_RANGE_MS { return bad("custom range cannot exceed 12 months"); }
        (s, e)
    } else {
        let start = match range.as_str() { "hour" => local_shift(now, 1, 0), "day" => local_shift(now, 24, 0), "week" => local_shift(now, 0, 7), _ => local_shift(now, 0, 30) };
        (start, now)
    };
    let step_seconds: i64 = if range == "custom" {
        let duration_ms = (end - start) as f64;
        let minutes = duration_ms / 60_000.0;
        let seconds = duration_ms / 1000.0;
        let mut step = if minutes <= 30.0 { 60 } else if minutes <= 24.0 * 60.0 { 300 } else if minutes <= 7.0 * 24.0 * 60.0 { 3600 } else { 21600 };
        if (seconds / step as f64).ceil() > 100.0 { step = (seconds / 100.0).ceil() as i64; }
        step
    } else { match range.as_str() { "hour" => 60, "day" => 3600, _ => 86_400 } };
    let normalized: Vec<&'static str> = BREAKDOWN_DIMENSIONS.iter().copied().filter(|d| breakdowns.iter().any(|b| b == d)).collect();
    let norm_excl: Vec<&'static str> = SUMMARY_EXCLUSIONS.iter().copied().filter(|d| exclusions.iter().any(|b| b == d)).collect();
    let key = match &principal { Principal::Admin => None, Principal::Limited { key_name, .. } => Some(key_name.clone()) };
    let iso = |ms: i64| chrono::DateTime::from_timestamp_millis(ms).map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)).unwrap_or_default();
    let cache_key = format!("{:x}:", std::sync::Arc::as_ptr(&state.inner) as usize) + &crate::engine::js::stringify(&serde_json::json!({"scopeKey": key.as_ref().map(|k| format!("limited:{k}")).unwrap_or_else(|| "admin".into()), "range": range, "startDate": iso(start), "endDate": iso(end), "breakdowns": normalized, "exclusions": norm_excl, "breakdownLimit": limit}));
    let mk = |status: StatusCode, cache: &str, body: Value| { let mut r = (status, Json(body)).into_response(); if !cache.is_empty() { r.headers_mut().insert("x-usage-summary-cache", cache.parse().unwrap()); } r };
    {
        let cache = summary_cache().lock().unwrap();
        if let Some((expires, body)) = cache.get(&cache_key) {
            if *expires > chrono::Utc::now().timestamp_millis() { return mk(StatusCode::OK, "HIT", body.clone()); }
        }
    }
    let Some(pool) = &state.inner.database else {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error":"Database not initialized"}))).into_response();
    };
    let params = SummaryParams { range: range.clone(), start, end, now, step_ms: step_seconds * 1000, key, breakdowns: normalized, exclusions: norm_excl, limit };
    match build_summary(pool, &params).await {
        Ok(body) => {
            let mut cache = summary_cache().lock().unwrap();
            let now_ms = chrono::Utc::now().timestamp_millis();
            cache.retain(|_, (expires, _)| *expires > now_ms);
            while cache.len() >= MAX_SUMMARY_CACHE_ENTRIES { cache.shift_remove_index(0); }
            let ttl = if range == "custom" && end < now - 5 * 60 * 1000 { HISTORICAL_SUMMARY_CACHE_TTL_MS } else { SUMMARY_CACHE_TTL_MS };
            cache.insert(cache_key, (now_ms + ttl, body.clone()));
            mk(StatusCode::OK, "MISS", body)
        }
        Err(e) => match e.strip_prefix("413:") {
            Some(msg) => mk(StatusCode::PAYLOAD_TOO_LARGE, "", serde_json::json!({"error": msg})),
            None => mk(StatusCode::INTERNAL_SERVER_ERROR, "", serde_json::json!({"error": e})),
        },
    }
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
