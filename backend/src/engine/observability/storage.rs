//! `services/observability/usage-storage.ts` (`UsageStorageService`) over
//! Trefoil's Postgres schema (`backend/migrations/*.sql`).
//!
//! SQL is built by pure functions (unit-tested) and executed with `sqlx`.
//! Columns Plexus writes that the live schema lacks (Trefoil's migrations
//! currently have no `upstream_model` / `*service_tier*` columns) are
//! detected once through `information_schema` and skipped instead of failing
//! the whole write.
use super::hooks::*;
use super::record::{
    column_for, js_stringify, js_stringify_pretty, parse_int, ColumnKind, UsageRecord,
    REQUEST_USAGE_FIELDS,
};
use crate::engine::js::{self, now_iso, now_ms, num, truthy, Obj};
use serde_json::Value;
use sqlx::{postgres::PgRow, PgPool, Row};
use std::collections::HashSet;
use std::sync::{Arc, OnceLock};

/// A positional SQL argument.
#[derive(Debug, Clone, PartialEq)]
pub enum SqlArg {
    Text(Option<String>),
    Int(Option<i64>),
    Real(Option<f64>),
}

/// SQL text plus its `$n` arguments.
#[derive(Debug, Clone, PartialEq)]
pub struct SqlStatement {
    pub sql: String,
    pub args: Vec<SqlArg>,
}

/// JS value → column argument (`null` → NULL; booleans → 1/0 for numbers).
pub fn sql_arg(v: &Value, kind: ColumnKind) -> SqlArg {
    match kind {
        ColumnKind::Text => SqlArg::Text(match v {
            Value::Null => None,
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => n.as_f64().map(super::record::js_number_string),
            Value::Bool(b) => Some(b.to_string()),
            other => Some(js_stringify(other)),
        }),
        ColumnKind::Int | ColumnKind::BigInt => SqlArg::Int(match v {
            Value::Bool(b) => Some(*b as i64),
            Value::Null => None,
            other => js::to_number(Some(other))
                .filter(|n| n.is_finite())
                .map(|n| n.round() as i64),
        }),
        ColumnKind::Real => SqlArg::Real(match v {
            Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            Value::Null => None,
            other => js::to_number(Some(other)).filter(|n| !n.is_nan()),
        }),
    }
}

/// Which `request_usage` columns exist (`None` = assume every Plexus column).
#[derive(Debug, Clone, Default)]
pub struct ColumnSet(pub Option<HashSet<String>>);

impl ColumnSet {
    pub fn all() -> Self {
        Self(None)
    }
    pub fn has(&self, column: &str) -> bool {
        match &self.0 {
            None => column != "permission_group",
            Some(s) => s.contains(column),
        }
    }
}

const BOOL_FIELDS: [&str; 6] = [
    "isStreamed",
    "isPassthrough",
    "isRaw",
    "parallelToolCallsEnabled",
    "isVisionFallthrough",
    "isDescriptorRequest",
];

/// `saveRequest(record)` → `INSERT … ON CONFLICT (request_id) DO UPDATE SET`
/// over exactly the defined (non-`undefined`) record fields, booleans as
/// 0/1 and `createdAt || Date.now()`. Trefoil's analytics-only
/// `permission_group` is resolved from `api_keys` when `apiKey` is written.
pub fn build_save_request(record: &UsageRecord, columns: &ColumnSet, now: i64) -> SqlStatement {
    let mut values: Vec<(&'static str, SqlArg)> = Vec::new();
    for (field, column, kind) in REQUEST_USAGE_FIELDS {
        if *field == "createdAt" || !columns.has(column) {
            continue;
        }
        let Some(v) = record.get(field) else { continue };
        let v = if BOOL_FIELDS.contains(field) {
            if let Value::Bool(b) = v {
                Value::from(*b as i64)
            } else {
                v.clone()
            }
        } else {
            v.clone()
        };
        values.push((column, sql_arg(&v, *kind)));
    }
    let created_at = if record.truthy("createdAt") {
        record.get("createdAt").cloned().unwrap_or(Value::Null)
    } else {
        Value::from(now)
    };
    values.push(("created_at", sql_arg(&created_at, ColumnKind::BigInt)));

    let mut cols: Vec<String> = Vec::new();
    let mut placeholders: Vec<String> = Vec::new();
    let mut args = Vec::new();
    let mut api_key_param = None;
    for (i, (column, arg)) in values.into_iter().enumerate() {
        cols.push(column.to_owned());
        placeholders.push(format!("${}", i + 1));
        if column == "api_key" {
            api_key_param = Some(i + 1);
        }
        args.push(arg);
    }
    if let (Some(p), true) = (api_key_param, columns.has("permission_group")) {
        cols.push("permission_group".into());
        placeholders.push(format!(
            "(SELECT permission_group FROM api_keys WHERE name = ${p} LIMIT 1)"
        ));
    }
    let set: Vec<String> = cols.iter().map(|c| format!("{c} = EXCLUDED.{c}")).collect();
    SqlStatement {
        sql: format!(
            "INSERT INTO request_usage ({}) VALUES ({}) ON CONFLICT (request_id) DO UPDATE SET {}",
            cols.join(", "),
            placeholders.join(", "),
            set.join(", ")
        ),
        args,
    }
}

/// `emitStarted(record)` → pending row insert (`durationMs: null`, `responseStatus: 'pending'`).
pub fn build_emit_started(
    record: &UsageRecord,
    columns: &ColumnSet,
    now: i64,
    now_iso: &str,
) -> SqlStatement {
    let text = |k: &str| SqlArg::Text(record.str_or_none(k));
    let flag = |k: &str| SqlArg::Int(Some(record.truthy(k) as i64));
    let start = if record.truthy("startTime") {
        sql_arg(
            &record.get("startTime").cloned().unwrap_or(Value::Null),
            ColumnKind::BigInt,
        )
    } else {
        SqlArg::Int(Some(now))
    };
    let date = record
        .str_or_none("date")
        .unwrap_or_else(|| now_iso.to_owned());
    let entries: Vec<(&str, SqlArg)> = vec![
        ("request_id", SqlArg::Text(Some(record.request_id()))),
        ("client_request_id", text("clientRequestId")),
        ("date", SqlArg::Text(Some(date))),
        ("source_ip", text("sourceIp")),
        ("api_key", text("apiKey")),
        ("attribution", text("attribution")),
        ("incoming_api_type", text("incomingApiType")),
        ("provider", text("provider")),
        ("incoming_model_alias", text("incomingModelAlias")),
        ("canonical_model_name", text("canonicalModelName")),
        ("selected_model_name", text("selectedModelName")),
        ("outgoing_api_type", text("outgoingApiType")),
        ("reasoning_effort", text("reasoningEffort")),
        ("start_time", start),
        ("duration_ms", SqlArg::Int(None)),
        ("response_status", SqlArg::Text(Some("pending".into()))),
        ("is_streamed", flag("isStreamed")),
        ("is_passthrough", flag("isPassthrough")),
        ("is_raw", flag("isRaw")),
        ("request_method", text("requestMethod")),
        ("request_path", text("requestPath")),
        ("created_at", SqlArg::Int(Some(now))),
    ];
    let mut cols = Vec::new();
    let mut placeholders = Vec::new();
    let mut args = Vec::new();
    let mut api_key_param = None;
    for (column, arg) in entries {
        if !columns.has(column) {
            continue;
        }
        args.push(arg);
        cols.push(column.to_owned());
        placeholders.push(format!("${}", args.len()));
        if column == "api_key" {
            api_key_param = Some(args.len());
        }
    }
    if let (Some(p), true) = (api_key_param, columns.has("permission_group")) {
        cols.push("permission_group".into());
        placeholders.push(format!(
            "(SELECT permission_group FROM api_keys WHERE name = ${p} LIMIT 1)"
        ));
    }
    SqlStatement {
        sql: format!(
            "INSERT INTO request_usage ({}) VALUES ({})",
            cols.join(", "),
            placeholders.join(", ")
        ),
        args,
    }
}

/// `emitUpdated(record)` → partial `UPDATE` of the pending row, or `None`
/// when Plexus would skip the DB write (no requestId, or none of provider /
/// canonicalModelName / reasoningEffort present).
pub fn build_emit_updated(record: &UsageRecord, columns: &ColumnSet) -> Option<SqlStatement> {
    let request_id = record.str_or_none("requestId")?;
    if !(record.truthy("provider")
        || record.truthy("canonicalModelName")
        || record.get("reasoningEffort").is_some())
    {
        return None;
    }
    let mut set: Vec<(&str, &Value)> = Vec::new();
    let truthy_fields = ["provider", "canonicalModelName", "selectedModelName"];
    for f in truthy_fields {
        if record.truthy(f) {
            set.push((f, &record.0[f]));
        }
    }
    for f in [
        "reasoningEffort",
        "requestedServiceTier",
        "requestedServiceTierRaw",
        "serviceTier",
        "serviceTierRaw",
    ] {
        if let Some(v) = record.get(f) {
            set.push((f, v));
        }
    }
    if record.truthy("incomingModelAlias") {
        set.push(("incomingModelAlias", &record.0["incomingModelAlias"]));
    }
    if record.truthy("apiKey") {
        set.push(("apiKey", &record.0["apiKey"]));
    }
    if let Some(v) = record.get("attribution") {
        set.push(("attribution", v));
    }
    let mut parts = Vec::new();
    let mut args = Vec::new();
    for (field, v) in set {
        let Some((column, kind)) = column_for(field) else {
            continue;
        };
        if !columns.has(column) {
            continue;
        }
        args.push(sql_arg(v, kind));
        parts.push(format!("{column} = ${}", args.len()));
    }
    if parts.is_empty() {
        return None;
    }
    args.push(SqlArg::Text(Some(request_id)));
    Some(SqlStatement {
        sql: format!(
            "UPDATE request_usage SET {} WHERE request_id = ${}",
            parts.join(", "),
            args.len()
        ),
        args,
    })
}

/// `normalizeErrorDetails(details)`: strings verbatim, objects JSON with an
/// object `providerResponse` pretty-stringified (2-space) first.
pub fn normalize_error_details(details: Option<&Value>) -> Option<String> {
    let d = details.filter(|d| truthy(Some(d)))?;
    match d {
        Value::String(s) => Some(s.clone()),
        Value::Object(map) => {
            let mut out = map.clone();
            if let Some(pr) = map
                .get("providerResponse")
                .filter(|p| !p.is_null() && !p.is_string())
            {
                out.insert(
                    "providerResponse".into(),
                    Value::String(js_stringify_pretty(pr, 2)),
                );
            }
            Some(js_stringify(&Value::Object(out)))
        }
        // `{...array}` spreads indices into an object.
        Value::Array(items) => {
            let out: Obj = items
                .iter()
                .enumerate()
                .map(|(i, v)| (i.to_string(), v.clone()))
                .collect();
            Some(js_stringify(&Value::Object(out)))
        }
        other => Some(js_stringify(other)),
    }
}

/// Row values written by `updatePerformanceMetrics` (pure part).
#[derive(Debug, Clone, PartialEq)]
pub struct PerformanceRow {
    pub time_to_first_token_ms: Option<f64>,
    pub total_tokens: Option<f64>,
    pub duration_ms: Option<f64>,
    pub tokens_per_sec: Option<f64>,
    pub e2e_tokens_per_sec: Option<f64>,
    pub success_count: i64,
    pub failure_count: i64,
}

pub fn performance_row(s: &PerformanceSample) -> PerformanceRow {
    let mut tps = None;
    let mut e2e = None;
    let out = s.output_tokens.filter(|n| *n != 0.0 && !n.is_nan());
    if s.success && out.is_some() && s.duration_ms > 0.0 {
        let out = out.unwrap_or(0.0);
        let ttft = s
            .time_to_first_token_ms
            .filter(|t| *t != 0.0 && !t.is_nan());
        let streaming = match ttft {
            Some(t) => s.duration_ms - t,
            None => s.duration_ms,
        };
        tps = if streaming > 0.0 {
            Some((out / streaming) * 1000.0)
        } else {
            None
        };
        e2e = Some((out / s.duration_ms) * 1000.0);
    }
    PerformanceRow {
        time_to_first_token_ms: if s.success {
            s.time_to_first_token_ms
        } else {
            None
        },
        total_tokens: if s.success { s.output_tokens } else { None },
        duration_ms: if s.success { Some(s.duration_ms) } else { None },
        tokens_per_sec: tps,
        e2e_tokens_per_sec: if s.success { e2e } else { None },
        success_count: s.success as i64,
        failure_count: (!s.success) as i64,
    }
}

/// `PLEXUS_PROVIDER_PERFORMANCE_RETENTION_LIMIT` (default 100).
pub fn performance_retention_limit(env: Option<&str>) -> i64 {
    match env.filter(|s| !s.is_empty()).map(parse_int) {
        None => 100,
        Some(Some(n)) if n >= 1 => n,
        Some(_) => 100,
    }
}

/// `UsageFilters`.
#[derive(Debug, Clone, Default)]
pub struct UsageFilters {
    pub request_id: Option<String>,
    pub client_request_id: Option<String>,
    pub start_date: Option<String>,
    pub end_date: Option<String>,
    pub api_key: Option<String>,
    /// `true` = `apiKeyMatch: 'exact'`; default substring (`LIKE %x%`).
    pub api_key_exact: bool,
    pub incoming_api_type: Option<String>,
    pub provider: Option<String>,
    pub incoming_model_alias: Option<String>,
    pub selected_model_name: Option<String>,
    pub outgoing_api_type: Option<String>,
    pub min_duration_ms: Option<i64>,
    pub max_duration_ms: Option<i64>,
    pub response_status: Option<String>,
}

/// `PaginationOptions` (`sort_by` one of date/apiKey/provider/incomingModelAlias/costTotal/durationMs).
#[derive(Debug, Clone)]
pub struct Pagination {
    pub limit: i64,
    pub offset: i64,
    pub sort_by: Option<String>,
    pub sort_asc: bool,
}

const USAGE_SELECT: &[(&str, &str, ColumnKind)] = &[
    ("requestId", "request_id", ColumnKind::Text),
    ("clientRequestId", "client_request_id", ColumnKind::Text),
    ("date", "date", ColumnKind::Text),
    ("sourceIp", "source_ip", ColumnKind::Text),
    ("apiKey", "api_key", ColumnKind::Text),
    ("attribution", "attribution", ColumnKind::Text),
    ("incomingApiType", "incoming_api_type", ColumnKind::Text),
    ("provider", "provider", ColumnKind::Text),
    ("attemptCount", "attempt_count", ColumnKind::Int),
    ("retryHistory", "retry_history", ColumnKind::Text),
    (
        "incomingModelAlias",
        "incoming_model_alias",
        ColumnKind::Text,
    ),
    (
        "canonicalModelName",
        "canonical_model_name",
        ColumnKind::Text,
    ),
    ("selectedModelName", "selected_model_name", ColumnKind::Text),
    (
        "finalAttemptProvider",
        "final_attempt_provider",
        ColumnKind::Text,
    ),
    ("finalAttemptModel", "final_attempt_model", ColumnKind::Text),
    ("upstreamModel", "upstream_model", ColumnKind::Text),
    (
        "allAttemptedProviders",
        "all_attempted_providers",
        ColumnKind::Text,
    ),
    ("outgoingApiType", "outgoing_api_type", ColumnKind::Text),
    ("reasoningEffort", "reasoning_effort", ColumnKind::Text),
    (
        "requestedServiceTier",
        "requested_service_tier",
        ColumnKind::Text,
    ),
    (
        "requestedServiceTierRaw",
        "requested_service_tier_raw",
        ColumnKind::Text,
    ),
    ("serviceTier", "service_tier", ColumnKind::Text),
    ("serviceTierRaw", "service_tier_raw", ColumnKind::Text),
    ("tokensInput", "tokens_input", ColumnKind::Int),
    ("tokensOutput", "tokens_output", ColumnKind::Int),
    ("tokensReasoning", "tokens_reasoning", ColumnKind::Int),
    ("tokensCached", "tokens_cached", ColumnKind::Int),
    ("tokensCacheWrite", "tokens_cache_write", ColumnKind::Int),
    ("tokensEstimated", "tokens_estimated", ColumnKind::Int),
    ("costInput", "cost_input", ColumnKind::Real),
    ("costOutput", "cost_output", ColumnKind::Real),
    ("costCached", "cost_cached", ColumnKind::Real),
    ("costCacheWrite", "cost_cache_write", ColumnKind::Real),
    ("costTotal", "cost_total", ColumnKind::Real),
    ("costSource", "cost_source", ColumnKind::Text),
    ("costMetadata", "cost_metadata", ColumnKind::Text),
    ("startTime", "start_time", ColumnKind::BigInt),
    ("durationMs", "duration_ms", ColumnKind::BigInt),
    ("ttftMs", "ttft_ms", ColumnKind::Real),
    ("tokensPerSec", "tokens_per_sec", ColumnKind::Real),
    ("isStreamed", "is_streamed", ColumnKind::Int),
    ("isPassthrough", "is_passthrough", ColumnKind::Int),
    ("isRaw", "is_raw", ColumnKind::Int),
    ("requestMethod", "request_method", ColumnKind::Text),
    ("requestPath", "request_path", ColumnKind::Text),
    ("responseStatus", "response_status", ColumnKind::Text),
    ("toolsDefined", "tools_defined", ColumnKind::Int),
    ("messageCount", "message_count", ColumnKind::Int),
    (
        "parallelToolCallsEnabled",
        "parallel_tool_calls_enabled",
        ColumnKind::Int,
    ),
    ("toolCallsCount", "tool_calls_count", ColumnKind::Int),
    ("finishReason", "finish_reason", ColumnKind::Text),
    ("kwhUsed", "kwh_used", ColumnKind::Real),
];

/// `getUsage(filters, pagination)` → (data query, count query, shared filter args;
/// the data query additionally binds limit and offset as the last two args).
pub fn build_usage_query(
    f: &UsageFilters,
    p: &Pagination,
    columns: &ColumnSet,
) -> (SqlStatement, SqlStatement) {
    let mut conds: Vec<String> = Vec::new();
    let mut args: Vec<SqlArg> = Vec::new();
    let mut push = |cond: &str, arg: SqlArg, conds: &mut Vec<String>| {
        args.push(arg);
        conds.push(cond.replace('?', &format!("${}", args.len())));
    };
    let t = |s: &Option<String>| s.as_ref().filter(|v| !v.is_empty()).cloned();
    if let Some(v) = t(&f.request_id) {
        push("request_id = ?", SqlArg::Text(Some(v)), &mut conds);
    }
    if let Some(v) = t(&f.client_request_id) {
        push("client_request_id = ?", SqlArg::Text(Some(v)), &mut conds);
    }
    if let Some(v) = t(&f.start_date) {
        push("date >= ?", SqlArg::Text(Some(v)), &mut conds);
    }
    if let Some(v) = t(&f.end_date) {
        push("date <= ?", SqlArg::Text(Some(v)), &mut conds);
    }
    if let Some(v) = t(&f.incoming_api_type) {
        push("incoming_api_type = ?", SqlArg::Text(Some(v)), &mut conds);
    }
    if let Some(v) = t(&f.api_key) {
        if f.api_key_exact {
            push("api_key = ?", SqlArg::Text(Some(v)), &mut conds);
        } else {
            push(
                "api_key LIKE ?",
                SqlArg::Text(Some(format!("%{v}%"))),
                &mut conds,
            );
        }
    }
    if let Some(v) = t(&f.provider) {
        push(
            "provider LIKE ?",
            SqlArg::Text(Some(format!("%{v}%"))),
            &mut conds,
        );
    }
    if let Some(v) = t(&f.incoming_model_alias) {
        push(
            "incoming_model_alias LIKE ?",
            SqlArg::Text(Some(format!("%{v}%"))),
            &mut conds,
        );
    }
    if let Some(v) = t(&f.selected_model_name) {
        push(
            "selected_model_name LIKE ?",
            SqlArg::Text(Some(format!("%{v}%"))),
            &mut conds,
        );
    }
    if let Some(v) = t(&f.outgoing_api_type) {
        push("outgoing_api_type = ?", SqlArg::Text(Some(v)), &mut conds);
    }
    if let Some(v) = f.min_duration_ms {
        push("duration_ms >= ?", SqlArg::Int(Some(v)), &mut conds);
    }
    if let Some(v) = f.max_duration_ms {
        push("duration_ms <= ?", SqlArg::Int(Some(v)), &mut conds);
    }
    if let Some(v) = t(&f.response_status) {
        push("response_status = ?", SqlArg::Text(Some(v)), &mut conds);
    }
    let where_sql = if conds.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", conds.join(" AND "))
    };

    let sort_col = match p.sort_by.as_deref() {
        Some("apiKey") => "api_key",
        Some("provider") => "provider",
        Some("incomingModelAlias") => "incoming_model_alias",
        Some("costTotal") => "cost_total",
        Some("durationMs") => "duration_ms",
        _ => "date",
    };
    let dir = if p.sort_asc { "ASC" } else { "DESC" };
    let select: Vec<String> = USAGE_SELECT
        .iter()
        .map(|(_, c, k)| {
            if columns.has(c) {
                c.to_string()
            } else {
                let ty = match k {
                    ColumnKind::Text => "text",
                    ColumnKind::Int => "int4",
                    ColumnKind::BigInt => "int8",
                    ColumnKind::Real => "float4",
                };
                format!("NULL::{ty} AS {c}")
            }
        })
        .collect();
    let data_sql = format!(
        "SELECT {}, EXISTS(SELECT 1 FROM debug_logs dl WHERE dl.request_id = request_usage.request_id) AS has_debug, EXISTS(SELECT 1 FROM inference_errors ie WHERE ie.request_id = request_usage.request_id) AS has_error FROM request_usage{where_sql} ORDER BY {sort_col} {dir}, date DESC LIMIT ${} OFFSET ${}",
        select.join(", "), args.len() + 1, args.len() + 2
    );
    let mut data_args = args.clone();
    data_args.push(SqlArg::Int(Some(p.limit)));
    data_args.push(SqlArg::Int(Some(p.offset)));
    (
        SqlStatement {
            sql: data_sql,
            args: data_args,
        },
        SqlStatement {
            sql: format!("SELECT count(*) FROM request_usage{where_sql}"),
            args,
        },
    )
}

/// `real` (float4) as postgres-js reads it (shortest decimal text → number).
fn f32_to_js(v: f32) -> f64 {
    format!("{v}").parse().unwrap_or(v as f64)
}

fn row_value(row: &PgRow, column: &str, kind: ColumnKind) -> Value {
    match kind {
        ColumnKind::Text => row
            .try_get::<Option<String>, _>(column)
            .ok()
            .flatten()
            .map(Value::String)
            .unwrap_or(Value::Null),
        ColumnKind::Int => row
            .try_get::<Option<i32>, _>(column)
            .ok()
            .flatten()
            .map(Value::from)
            .unwrap_or(Value::Null),
        ColumnKind::BigInt => row
            .try_get::<Option<i64>, _>(column)
            .ok()
            .flatten()
            .map(Value::from)
            .unwrap_or(Value::Null),
        ColumnKind::Real => row
            .try_get::<Option<f32>, _>(column)
            .ok()
            .flatten()
            .map(|f| num(f32_to_js(f)))
            .unwrap_or(Value::Null),
    }
}

/// Row → `UsageRecord` JSON exactly as `getUsage` maps it.
fn map_usage_row(row: &PgRow) -> Value {
    let mut out = Obj::new();
    for (field, column, kind) in USAGE_SELECT {
        out.insert((*field).into(), row_value(row, column, *kind));
    }
    let def = |out: &mut Obj, k: &str, d: Value| {
        if out.get(k).is_none_or(Value::is_null) {
            out.insert(k.into(), d);
        }
    };
    def(&mut out, "incomingApiType", Value::from(""));
    def(&mut out, "attemptCount", Value::from(1));
    def(&mut out, "responseStatus", Value::from(""));
    for k in [
        "isStreamed",
        "isPassthrough",
        "isRaw",
        "parallelToolCallsEnabled",
    ] {
        let b = truthy(out.get(k));
        out.insert(k.into(), Value::Bool(b));
    }
    out.insert(
        "hasDebug".into(),
        Value::Bool(
            row.try_get::<Option<bool>, _>("has_debug")
                .ok()
                .flatten()
                .unwrap_or(false),
        ),
    );
    out.insert(
        "hasError".into(),
        Value::Bool(
            row.try_get::<Option<bool>, _>("has_error")
                .ok()
                .flatten()
                .unwrap_or(false),
        ),
    );
    // Re-order to the TS literal order (hasDebug/hasError sit after responseStatus/tokensPerSec).
    let order = [
        "requestId",
        "clientRequestId",
        "date",
        "sourceIp",
        "apiKey",
        "attribution",
        "incomingApiType",
        "provider",
        "attemptCount",
        "retryHistory",
        "incomingModelAlias",
        "canonicalModelName",
        "selectedModelName",
        "finalAttemptProvider",
        "finalAttemptModel",
        "upstreamModel",
        "allAttemptedProviders",
        "outgoingApiType",
        "reasoningEffort",
        "requestedServiceTier",
        "requestedServiceTierRaw",
        "serviceTier",
        "serviceTierRaw",
        "tokensInput",
        "tokensOutput",
        "tokensReasoning",
        "tokensCached",
        "tokensCacheWrite",
        "tokensEstimated",
        "costInput",
        "costOutput",
        "costCached",
        "costCacheWrite",
        "costTotal",
        "costSource",
        "costMetadata",
        "startTime",
        "durationMs",
        "isStreamed",
        "responseStatus",
        "ttftMs",
        "tokensPerSec",
        "hasDebug",
        "hasError",
        "isPassthrough",
        "isRaw",
        "requestMethod",
        "requestPath",
        "toolsDefined",
        "messageCount",
        "parallelToolCallsEnabled",
        "toolCallsCount",
        "finishReason",
        "kwhUsed",
    ];
    let mut ordered = Obj::new();
    for k in order {
        if let Some(v) = out.remove(k) {
            ordered.insert(k.into(), v);
        }
    }
    Value::Object(ordered)
}

/// One `getProviderPerformance` aggregate row (snake_case JSON shape).
/// Plexus on postgres-js returns the int8 aggregates (`sample_count`,
/// `success_count`, `failure_count`, `last_updated`) as strings; here they
/// are numbers, which is what every selector coerces them to.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderPerformanceRow {
    pub provider: Option<String>,
    pub model: String,
    pub target_model: String,
    pub avg_ttft_ms: f64,
    pub min_ttft_ms: f64,
    pub max_ttft_ms: f64,
    pub avg_tokens_per_sec: f64,
    pub min_tokens_per_sec: f64,
    pub max_tokens_per_sec: f64,
    pub avg_e2e_tokens_per_sec: f64,
    pub min_e2e_tokens_per_sec: f64,
    pub max_e2e_tokens_per_sec: f64,
    pub sample_count: f64,
    pub success_count: f64,
    pub failure_count: f64,
    pub last_updated: f64,
}

impl ProviderPerformanceRow {
    /// Zero row for a canonical-model provider seen only in `request_usage`.
    pub fn empty(provider: Option<String>, model: &str) -> Self {
        Self {
            provider,
            model: model.into(),
            target_model: model.into(),
            avg_ttft_ms: 0.0,
            min_ttft_ms: 0.0,
            max_ttft_ms: 0.0,
            avg_tokens_per_sec: 0.0,
            min_tokens_per_sec: 0.0,
            max_tokens_per_sec: 0.0,
            avg_e2e_tokens_per_sec: 0.0,
            min_e2e_tokens_per_sec: 0.0,
            max_e2e_tokens_per_sec: 0.0,
            sample_count: 0.0,
            success_count: 0.0,
            failure_count: 0.0,
            last_updated: 0.0,
        }
    }
    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "provider": self.provider, "model": self.model, "target_model": self.target_model,
            "avg_ttft_ms": num(self.avg_ttft_ms), "min_ttft_ms": num(self.min_ttft_ms), "max_ttft_ms": num(self.max_ttft_ms),
            "avg_tokens_per_sec": num(self.avg_tokens_per_sec), "min_tokens_per_sec": num(self.min_tokens_per_sec), "max_tokens_per_sec": num(self.max_tokens_per_sec),
            "avg_e2e_tokens_per_sec": num(self.avg_e2e_tokens_per_sec), "min_e2e_tokens_per_sec": num(self.min_e2e_tokens_per_sec), "max_e2e_tokens_per_sec": num(self.max_e2e_tokens_per_sec),
            "sample_count": num(self.sample_count), "success_count": num(self.success_count), "failure_count": num(self.failure_count), "last_updated": num(self.last_updated),
        })
    }
}

/// `getProviderPerformance(provider?, model?)` aggregate query.
pub fn build_provider_performance(provider: Option<&str>, model: Option<&str>) -> SqlStatement {
    let mut conds = Vec::new();
    let mut args = Vec::new();
    if let Some(p) = provider.filter(|p| !p.is_empty()) {
        args.push(SqlArg::Text(Some(p.into())));
        conds.push(format!("pp.provider = ${}", args.len()));
    }
    if let Some(m) = model.filter(|m| !m.is_empty()) {
        args.push(SqlArg::Text(Some(m.into())));
        conds.push(format!("pp.model = ${}", args.len()));
    }
    let where_sql = if conds.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", conds.join(" AND "))
    };
    SqlStatement {
        sql: format!(
            "SELECT pp.provider AS provider, pp.model AS model, \
             AVG(pp.time_to_first_token_ms) AS avg_ttft_ms, MIN(pp.time_to_first_token_ms) AS min_ttft_ms, MAX(pp.time_to_first_token_ms) AS max_ttft_ms, \
             AVG(pp.tokens_per_sec) AS avg_tokens_per_sec, MIN(pp.tokens_per_sec) AS min_tokens_per_sec, MAX(pp.tokens_per_sec) AS max_tokens_per_sec, \
             AVG(pp.e2e_tokens_per_sec) AS avg_e2e_tokens_per_sec, MIN(pp.e2e_tokens_per_sec) AS min_e2e_tokens_per_sec, MAX(pp.e2e_tokens_per_sec) AS max_e2e_tokens_per_sec, \
             COUNT(*) AS sample_count, SUM(pp.success_count) AS success_count, SUM(pp.failure_count) AS failure_count, MAX(pp.created_at) AS last_updated \
             FROM provider_performance pp LEFT JOIN request_usage ru ON pp.request_id = ru.request_id{where_sql} \
             GROUP BY pp.provider, pp.model ORDER BY AVG(pp.tokens_per_sec) DESC"
        ),
        args,
    }
}

fn bind_args<'q>(
    mut q: sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>,
    args: &[SqlArg],
) -> sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments> {
    for a in args {
        q = match a {
            SqlArg::Text(v) => q.bind(v.clone()),
            SqlArg::Int(v) => q.bind(*v),
            SqlArg::Real(v) => q.bind(*v),
        };
    }
    q
}

/// Optional collaborators of the storage service.
#[derive(Clone, Default)]
pub struct StorageHooks {
    pub events: Option<Arc<dyn UsageEvents>>,
    pub key_context: Option<Arc<dyn KeyNameContext>>,
    pub debug: Option<Arc<dyn DebugHooks>>,
}

enum TelemetryTask {
    Started(UsageRecord),
    Updated(UsageRecord),
}

/// `UsageStorageService` (write side + selector reads) on a `PgPool`.
/// DB failures are swallowed like Plexus (which logs them); reads used by
/// selectors return empty results on error.
pub struct UsageStorage {
    pool: PgPool,
    hooks: StorageHooks,
    columns: tokio::sync::OnceCell<ColumnSet>,
    telemetry: OnceLock<tokio::sync::mpsc::UnboundedSender<TelemetryTask>>,
}

impl UsageStorage {
    pub fn new(pool: PgPool, hooks: StorageHooks) -> Self {
        Self {
            pool,
            hooks,
            columns: tokio::sync::OnceCell::new(),
            telemetry: OnceLock::new(),
        }
    }
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Live `request_usage` columns (cached; falls back to the Plexus set).
    pub async fn columns(&self) -> &ColumnSet {
        self.columns.get_or_init(|| async {
            let rows = sqlx::query("SELECT column_name::text AS column_name FROM information_schema.columns WHERE table_schema = current_schema() AND table_name = 'request_usage'")
                .fetch_all(&self.pool).await;
            match rows {
                Ok(rows) if !rows.is_empty() => ColumnSet(Some(rows.iter().filter_map(|r| r.try_get::<String, _>("column_name").ok()).collect())),
                _ => ColumnSet::all(),
            }
        }).await
    }

    fn emit(&self, kind: UsageEventKind, record: &Value) {
        if let Some(e) = &self.hooks.events {
            e.emit(kind, record);
        }
    }

    async fn exec(&self, st: &SqlStatement) -> Result<(), sqlx::Error> {
        bind_args(sqlx::query(&st.sql), &st.args)
            .execute(&self.pool)
            .await
            .map(|_| ())
    }

    /// `emitStarted(record)`.
    pub async fn emit_started(&self, record: &UsageRecord) {
        let st = build_emit_started(record, self.columns().await, now_ms(), &now_iso());
        let _ = self.exec(&st).await;
        let mut event = record.clone();
        event.set("responseStatus", "pending");
        self.emit(UsageEventKind::Started, &event.to_value());
    }

    /// `emitUpdated(record)`.
    pub async fn emit_updated(&self, record: &UsageRecord) {
        if let Some(st) = build_emit_updated(record, self.columns().await) {
            let _ = self.exec(&st).await;
        }
        self.emit(UsageEventKind::Updated, &record.to_value());
    }

    fn telemetry_sender(
        self: &Arc<Self>,
    ) -> Option<&tokio::sync::mpsc::UnboundedSender<TelemetryTask>> {
        if self.telemetry.get().is_none() {
            let handle = tokio::runtime::Handle::try_current().ok()?;
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<TelemetryTask>();
            if self.telemetry.set(tx).is_ok() {
                let weak = Arc::downgrade(self);
                handle.spawn(async move {
                    while let Some(task) = rx.recv().await {
                        let Some(this) = weak.upgrade() else { break };
                        match task {
                            TelemetryTask::Started(r) => this.emit_started(&r).await,
                            TelemetryTask::Updated(r) => this.emit_updated(&r).await,
                        }
                    }
                });
            }
        }
        self.telemetry.get()
    }

    /// `emitStartedAsync(record)`: non-blocking, FIFO with `emit_updated_async`.
    pub fn emit_started_async(self: &Arc<Self>, record: UsageRecord) {
        if let Some(tx) = self.telemetry_sender() {
            let _ = tx.send(TelemetryTask::Started(record));
        }
    }
    /// `emitUpdatedAsync(record)`.
    pub fn emit_updated_async(self: &Arc<Self>, record: UsageRecord) {
        if let Some(tx) = self.telemetry_sender() {
            let _ = tx.send(TelemetryTask::Updated(record));
        }
    }

    async fn update_vision_metadata(&self, request_id: &str, metadata: &VisionMetadata) {
        let _ = sqlx::query("UPDATE request_usage SET is_vision_fallthrough = $1, is_descriptor_request = $2, vision_fallthrough_model = $3 WHERE request_id = $4")
            .bind(metadata.is_vision_fallthrough as i32).bind(metadata.is_descriptor_request as i32).bind(metadata.vision_fallthrough_model.clone()).bind(request_id)
            .execute(&self.pool).await;
    }

    /// `recordSuccessfulAttempt(provider, model, canonical, requestId, metadata?)`.
    pub async fn record_successful_attempt(
        &self,
        provider: &str,
        model: &str,
        canonical: Option<&str>,
        request_id: &str,
        metadata: Option<&VisionMetadata>,
    ) {
        if let Some(m) = metadata {
            self.update_vision_metadata(request_id, m).await;
        }
        self.update_performance_metrics(&PerformanceSample {
            provider: provider.into(),
            model: model.into(),
            canonical_model_name: canonical.map(str::to_owned),
            time_to_first_token_ms: None,
            output_tokens: None,
            duration_ms: 0.0,
            request_id: request_id.into(),
            success: true,
        })
        .await;
    }

    /// `recordFailedAttempt(provider, model, canonical, requestId, metadata?)`.
    pub async fn record_failed_attempt(
        &self,
        provider: &str,
        model: &str,
        canonical: Option<&str>,
        request_id: &str,
        metadata: Option<&VisionMetadata>,
    ) {
        if let Some(m) = metadata {
            self.update_vision_metadata(request_id, m).await;
        }
        self.update_performance_metrics(&PerformanceSample {
            provider: provider.into(),
            model: model.into(),
            canonical_model_name: canonical.map(str::to_owned),
            time_to_first_token_ms: None,
            output_tokens: None,
            duration_ms: 0.0,
            request_id: request_id.into(),
            success: false,
        })
        .await;
    }

    /// `getProviderPerformance(provider?, model?)`.
    pub async fn get_provider_performance(
        &self,
        provider: Option<&str>,
        model: Option<&str>,
    ) -> Vec<ProviderPerformanceRow> {
        let st = build_provider_performance(provider, model);
        let Ok(rows) = bind_args(sqlx::query(&st.sql), &st.args)
            .fetch_all(&self.pool)
            .await
        else {
            return Vec::new();
        };
        let f8 = |r: &PgRow, c: &str| r.try_get::<Option<f64>, _>(c).ok().flatten().unwrap_or(0.0);
        let f4 = |r: &PgRow, c: &str| {
            r.try_get::<Option<f32>, _>(c)
                .ok()
                .flatten()
                .map(f32_to_js)
                .unwrap_or(0.0)
        };
        let i8 =
            |r: &PgRow, c: &str| r.try_get::<Option<i64>, _>(c).ok().flatten().unwrap_or(0) as f64;
        let mut out: Vec<ProviderPerformanceRow> = rows
            .iter()
            .map(|r| {
                let model: String = r.try_get("model").unwrap_or_default();
                ProviderPerformanceRow {
                    provider: r.try_get("provider").ok(),
                    target_model: model.clone(),
                    model,
                    avg_ttft_ms: f8(r, "avg_ttft_ms"),
                    min_ttft_ms: f4(r, "min_ttft_ms"),
                    max_ttft_ms: f4(r, "max_ttft_ms"),
                    avg_tokens_per_sec: f8(r, "avg_tokens_per_sec"),
                    min_tokens_per_sec: f4(r, "min_tokens_per_sec"),
                    max_tokens_per_sec: f4(r, "max_tokens_per_sec"),
                    avg_e2e_tokens_per_sec: f8(r, "avg_e2e_tokens_per_sec"),
                    min_e2e_tokens_per_sec: f4(r, "min_e2e_tokens_per_sec"),
                    max_e2e_tokens_per_sec: f4(r, "max_e2e_tokens_per_sec"),
                    sample_count: i8(r, "sample_count"),
                    success_count: i8(r, "success_count"),
                    failure_count: i8(r, "failure_count"),
                    last_updated: i8(r, "last_updated"),
                }
            })
            .collect();
        if let Some(model) = model.filter(|m| !m.is_empty()) {
            let mut sql =
                "SELECT provider FROM request_usage WHERE canonical_model_name = $1".to_owned();
            let mut q_args = vec![SqlArg::Text(Some(model.into()))];
            if let Some(p) = provider.filter(|p| !p.is_empty()) {
                sql.push_str(" AND provider = $2");
                q_args.push(SqlArg::Text(Some(p.into())));
            }
            sql.push_str(" GROUP BY provider");
            let Ok(usage_rows) = bind_args(sqlx::query(&sql), &q_args)
                .fetch_all(&self.pool)
                .await
            else {
                return Vec::new();
            };
            let existing: HashSet<Option<String>> =
                out.iter().map(|r| r.provider.clone()).collect();
            for r in usage_rows {
                let p: Option<String> = r.try_get("provider").ok().flatten();
                if !existing.contains(&p) {
                    out.push(ProviderPerformanceRow::empty(p, model));
                }
            }
        }
        out
    }

    /// `getUsage(filters, pagination)` → `(data, total)`; errors propagate (Plexus rethrows).
    pub async fn get_usage(
        &self,
        filters: &UsageFilters,
        pagination: &Pagination,
    ) -> Result<(Vec<Value>, i64), sqlx::Error> {
        let (data, count) = build_usage_query(filters, pagination, self.columns().await);
        let rows = bind_args(sqlx::query(&data.sql), &data.args)
            .fetch_all(&self.pool)
            .await?;
        let total: i64 = bind_args(sqlx::query(&count.sql), &count.args)
            .fetch_one(&self.pool)
            .await?
            .try_get(0)?;
        Ok((rows.iter().map(map_usage_row).collect(), total))
    }
}

/// `metadata` argument of `recordSuccessfulAttempt` / `recordFailedAttempt`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VisionMetadata {
    pub is_vision_fallthrough: bool,
    pub is_descriptor_request: bool,
    pub vision_fallthrough_model: Option<String>,
}

#[async_trait::async_trait]
impl UsageSink for UsageStorage {
    fn register_in_flight(&self, request_id: &str, api_key: Option<&str>, is_streamed: bool) {
        crate::engine::dispatcher::stall_inspector::register_in_flight(
            request_id,
            api_key,
            is_streamed,
        );
    }
    fn deregister_in_flight(&self, request_id: &str) {
        crate::engine::dispatcher::stall_inspector::deregister_in_flight(request_id);
    }

    async fn save_request(&self, record: &UsageRecord) {
        let st = build_save_request(record, self.columns().await, now_ms());
        if self.exec(&st).await.is_ok() {
            let v = record.to_value();
            self.emit(UsageEventKind::Created, &v);
            self.emit(UsageEventKind::Completed, &v);
        }
    }

    async fn save_error(
        &self,
        request_id: &str,
        error: &ErrorInfo,
        details: Option<&Value>,
        api_key: Option<&str>,
    ) {
        // 1. explicit apiKey, 2. request context, 3. request_usage row.
        let mut effective = api_key.filter(|k| !k.is_empty()).map(str::to_owned);
        if effective.is_none() {
            effective = self
                .hooks
                .key_context
                .as_ref()
                .and_then(|c| c.current_key_name())
                .filter(|k| !k.is_empty());
        }
        if effective.is_none() {
            if let Ok(Some(row)) =
                sqlx::query("SELECT api_key FROM request_usage WHERE request_id = $1 LIMIT 1")
                    .bind(request_id)
                    .fetch_optional(&self.pool)
                    .await
            {
                effective = row
                    .try_get::<Option<String>, _>("api_key")
                    .ok()
                    .flatten()
                    .filter(|k| !k.is_empty());
            }
        }
        let message = if error.message.is_empty() {
            "Error".to_owned()
        } else {
            error.message.clone()
        };
        let res = sqlx::query("INSERT INTO inference_errors (request_id, date, api_key, error_message, error_stack, details, created_at) VALUES ($1, $2, $3, $4, $5, $6, $7)")
            .bind(request_id).bind(now_iso()).bind(effective).bind(message).bind(error.stack.clone().filter(|s| !s.is_empty()))
            .bind(normalize_error_details(details)).bind(now_ms())
            .execute(&self.pool).await;
        if res.is_ok() {
            if let Some(d) = &self.hooks.debug {
                d.mark_force_persist(request_id);
            }
        }
    }

    async fn update_performance_metrics(&self, s: &PerformanceSample) {
        let limit = performance_retention_limit(
            std::env::var("PLEXUS_PROVIDER_PERFORMANCE_RETENTION_LIMIT")
                .ok()
                .as_deref(),
        );
        let row = performance_row(s);
        let inserted = sqlx::query("INSERT INTO provider_performance (provider, model, canonical_model_name, request_id, time_to_first_token_ms, total_tokens, duration_ms, tokens_per_sec, e2e_tokens_per_sec, success_count, failure_count, created_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)")
            .bind(&s.provider).bind(&s.model).bind(s.canonical_model_name.clone()).bind(&s.request_id)
            .bind(row.time_to_first_token_ms).bind(row.total_tokens.map(|n| n.round() as i64)).bind(row.duration_ms)
            .bind(row.tokens_per_sec).bind(row.e2e_tokens_per_sec).bind(row.success_count as i32).bind(row.failure_count as i32).bind(now_ms())
            .execute(&self.pool).await;
        if inserted.is_err() {
            return;
        }
        let _ = sqlx::query("DELETE FROM provider_performance WHERE provider = $1 AND model = $2 AND id NOT IN (SELECT id FROM (SELECT id FROM provider_performance WHERE provider = $1 AND model = $2 ORDER BY created_at DESC LIMIT $3) sub)")
            .bind(&s.provider).bind(&s.model).bind(limit)
            .execute(&self.pool).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn save_request_writes_defined_fields_only() {
        let r = UsageRecord::from_value(json!({
            "requestId": "r1", "date": "2026-01-01T00:00:00.000Z", "apiKey": "k", "isStreamed": true, "isPassthrough": false,
            "tokensInput": 10, "costTotal": 0.5, "finishReason": null, "hasDebug": true, "startTime": 5, "durationMs": 12.0
        }));
        let st = build_save_request(&r, &ColumnSet::all(), 99);
        assert_eq!(st.sql, "INSERT INTO request_usage (request_id, date, api_key, tokens_input, cost_total, start_time, duration_ms, is_streamed, is_passthrough, finish_reason, created_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) ON CONFLICT (request_id) DO UPDATE SET request_id = EXCLUDED.request_id, date = EXCLUDED.date, api_key = EXCLUDED.api_key, tokens_input = EXCLUDED.tokens_input, cost_total = EXCLUDED.cost_total, start_time = EXCLUDED.start_time, duration_ms = EXCLUDED.duration_ms, is_streamed = EXCLUDED.is_streamed, is_passthrough = EXCLUDED.is_passthrough, finish_reason = EXCLUDED.finish_reason, created_at = EXCLUDED.created_at");
        assert_eq!(st.args[7], SqlArg::Int(Some(1)));
        assert_eq!(st.args[8], SqlArg::Int(Some(0)));
        assert_eq!(st.args[9], SqlArg::Text(None));
        assert_eq!(st.args[10], SqlArg::Int(Some(99)));
        // Missing columns are skipped; permission_group follows apiKey when available.
        let cols = ColumnSet(Some(
            [
                "request_id",
                "date",
                "api_key",
                "created_at",
                "permission_group",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        ));
        let st = build_save_request(&r, &cols, 99);
        assert_eq!(st.sql, "INSERT INTO request_usage (request_id, date, api_key, created_at, permission_group) VALUES ($1, $2, $3, $4, (SELECT permission_group FROM api_keys WHERE name = $3 LIMIT 1)) ON CONFLICT (request_id) DO UPDATE SET request_id = EXCLUDED.request_id, date = EXCLUDED.date, api_key = EXCLUDED.api_key, created_at = EXCLUDED.created_at, permission_group = EXCLUDED.permission_group");
    }

    #[test]
    fn emit_started_and_updated() {
        let r = UsageRecord::from_value(
            json!({"requestId": "r1", "apiKey": "", "incomingApiType": "chat", "isStreamed": true, "startTime": 7}),
        );
        let st = build_emit_started(&r, &ColumnSet::all(), 100, "2026-01-01T00:00:00.000Z");
        assert!(st.sql.starts_with(
            "INSERT INTO request_usage (request_id, client_request_id, date, source_ip, api_key,"
        ));
        assert_eq!(st.args[1], SqlArg::Text(None));
        assert_eq!(
            st.args[2],
            SqlArg::Text(Some("2026-01-01T00:00:00.000Z".into()))
        );
        assert_eq!(st.args[4], SqlArg::Text(None));
        assert_eq!(st.args[6], SqlArg::Text(Some("chat".into())));
        assert_eq!(st.args[13], SqlArg::Int(Some(7)));
        assert_eq!(st.args[14], SqlArg::Int(None));
        assert_eq!(st.args[15], SqlArg::Text(Some("pending".into())));
        assert_eq!(st.args[16], SqlArg::Int(Some(1)));
        assert_eq!(
            build_emit_updated(
                &UsageRecord::from_value(json!({"requestId": "r1", "selectedModelName": "m"})),
                &ColumnSet::all()
            ),
            None
        );
        let up = build_emit_updated(&UsageRecord::from_value(json!({"requestId": "r1", "provider": "p", "selectedModelName": "m", "serviceTier": null, "attribution": "a"})), &ColumnSet::all()).unwrap();
        assert_eq!(up.sql, "UPDATE request_usage SET provider = $1, selected_model_name = $2, service_tier = $3, attribution = $4 WHERE request_id = $5");
        assert_eq!(up.args[2], SqlArg::Text(None));
        let only_effort = build_emit_updated(
            &UsageRecord::from_value(json!({"requestId": "r1", "reasoningEffort": null})),
            &ColumnSet::all(),
        )
        .unwrap();
        assert_eq!(
            only_effort.sql,
            "UPDATE request_usage SET reasoning_effort = $1 WHERE request_id = $2"
        );
    }

    #[test]
    fn error_details_normalization() {
        let d = json!({"apiType": "chat", "provider": "test-provider", "providerResponse": {"error": {"message": "structured provider error", "code": 400}}});
        let out: Value = serde_json::from_str(&normalize_error_details(Some(&d)).unwrap()).unwrap();
        assert!(out["providerResponse"].is_string());
        assert_eq!(
            serde_json::from_str::<Value>(out["providerResponse"].as_str().unwrap()).unwrap(),
            json!({"error": {"message": "structured provider error", "code": 400}})
        );
        assert_eq!(out["providerResponse"].as_str().unwrap(), "{\n  \"error\": {\n    \"message\": \"structured provider error\",\n    \"code\": 400\n  }\n}");
        assert_eq!(
            normalize_error_details(Some(&json!({"providerResponse": "raw"}))),
            Some(r#"{"providerResponse":"raw"}"#.into())
        );
        assert_eq!(
            normalize_error_details(Some(&json!("text"))),
            Some("text".into())
        );
        assert_eq!(normalize_error_details(Some(&json!(""))), None);
        assert_eq!(normalize_error_details(None), None);
        assert_eq!(
            normalize_error_details(Some(&json!(["a"]))),
            Some(r#"{"0":"a"}"#.into())
        );
        assert_eq!(
            normalize_error_details(Some(&json!(true))),
            Some("true".into())
        );
    }

    #[test]
    fn performance_accounting() {
        let base = PerformanceSample {
            provider: "p".into(),
            model: "m".into(),
            canonical_model_name: None,
            time_to_first_token_ms: Some(200.0),
            output_tokens: Some(100.0),
            duration_ms: 1200.0,
            request_id: "r".into(),
            success: true,
        };
        let row = performance_row(&base);
        assert_eq!(row.tokens_per_sec, Some(100.0));
        assert!((row.e2e_tokens_per_sec.unwrap() - 83.33333333333333).abs() < 1e-9);
        assert_eq!((row.success_count, row.failure_count), (1, 0));
        let fail = performance_row(&PerformanceSample {
            success: false,
            ..base.clone()
        });
        assert_eq!(
            fail,
            PerformanceRow {
                time_to_first_token_ms: None,
                total_tokens: None,
                duration_ms: None,
                tokens_per_sec: None,
                e2e_tokens_per_sec: None,
                success_count: 0,
                failure_count: 1
            }
        );
        let attempt = performance_row(&PerformanceSample {
            time_to_first_token_ms: None,
            output_tokens: None,
            duration_ms: 0.0,
            ..base.clone()
        });
        assert_eq!(
            (
                attempt.duration_ms,
                attempt.tokens_per_sec,
                attempt.success_count
            ),
            (Some(0.0), None, 1)
        );
        let no_ttft = performance_row(&PerformanceSample {
            time_to_first_token_ms: Some(0.0),
            ..base
        });
        assert!((no_ttft.tokens_per_sec.unwrap() - 83.33333333333333).abs() < 1e-9);
        assert_eq!(performance_retention_limit(None), 100);
        assert_eq!(performance_retention_limit(Some("3")), 3);
        assert_eq!(performance_retention_limit(Some("0")), 100);
        assert_eq!(performance_retention_limit(Some("x")), 100);
    }

    #[test]
    fn usage_and_performance_queries() {
        let f = UsageFilters {
            provider: Some("open".into()),
            selected_model_name: Some("gpt".into()),
            start_date: Some("a".into()),
            end_date: Some("b".into()),
            ..Default::default()
        };
        let (data, count) = build_usage_query(
            &f,
            &Pagination {
                limit: 1000,
                offset: 0,
                sort_by: None,
                sort_asc: false,
            },
            &ColumnSet(Some(
                ["request_id", "date"]
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
            )),
        );
        assert_eq!(count.sql, "SELECT count(*) FROM request_usage WHERE date >= $1 AND date <= $2 AND provider LIKE $3 AND selected_model_name LIKE $4");
        assert_eq!(count.args[2], SqlArg::Text(Some("%open%".into())));
        assert!(data.sql.contains("NULL::text AS upstream_model"));
        assert!(data
            .sql
            .ends_with("ORDER BY date DESC, date DESC LIMIT $5 OFFSET $6"));
        assert_eq!(data.args.len(), 6);
        let (data, _) = build_usage_query(
            &UsageFilters {
                api_key: Some("k".into()),
                api_key_exact: true,
                ..Default::default()
            },
            &Pagination {
                limit: 5,
                offset: 10,
                sort_by: Some("costTotal".into()),
                sort_asc: true,
            },
            &ColumnSet::all(),
        );
        assert!(data
            .sql
            .contains("WHERE api_key = $1 ORDER BY cost_total ASC, date DESC LIMIT $2 OFFSET $3"));
        let perf = build_provider_performance(Some("p"), Some("m"));
        assert!(perf.sql.contains("WHERE pp.provider = $1 AND pp.model = $2 GROUP BY pp.provider, pp.model ORDER BY AVG(pp.tokens_per_sec) DESC"));
        assert_eq!(
            ProviderPerformanceRow::empty(Some("p".into()), "m").to_json()["sample_count"],
            json!(0)
        );
    }
}
