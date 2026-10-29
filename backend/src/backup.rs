//! Portable database backup export.
//!
//! The full form deliberately uses PostgreSQL's JSON representation for rows:
//! this keeps the archive independent of Rust's database type mappings while
//! preserving every column (including columns added by later migrations).

use axum::{
    body::{Body, Bytes},
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Router,
};
use flate2::{write::GzEncoder, Compression};
use serde::Deserialize;
use sqlx::Row;
use std::collections::HashMap;
use std::io::Write;
use tar::Builder;

use crate::{
    auth::{resolve_principal, Principal},
    AppState,
};

const TABLES: &[&str] = &[
    "request_usage",
    "provider_cooldowns",
    "debug_logs",
    "inference_errors",
    "provider_performance",
    "quota_state",
    "meter_snapshots",
    "quota_snapshots",
    "mcp_request_usage",
    "mcp_debug_logs",
    "responses",
    "conversations",
    "response_items",
];

#[derive(Clone, Copy, PartialEq)]
enum Col {
    Text,
    Number,
    Bool,
    Timestamp,
}

/// Plexus drizzle (PostgreSQL) columns per operational table, in schema
/// order: `(property, column, kind)`. Generated from the pinned reference.
type ColumnSpec = (&'static str, &'static str, Col);
const PLEXUS_COLUMNS: &[(&str, &[ColumnSpec])] = &[
    (
        "request_usage",
        &[
            ("id", "id", Col::Number),
            ("requestId", "request_id", Col::Text),
            ("clientRequestId", "client_request_id", Col::Text),
            ("date", "date", Col::Text),
            ("sourceIp", "source_ip", Col::Text),
            ("apiKey", "api_key", Col::Text),
            ("attribution", "attribution", Col::Text),
            ("incomingApiType", "incoming_api_type", Col::Text),
            ("provider", "provider", Col::Text),
            ("attemptCount", "attempt_count", Col::Number),
            ("retryHistory", "retry_history", Col::Text),
            ("incomingModelAlias", "incoming_model_alias", Col::Text),
            ("canonicalModelName", "canonical_model_name", Col::Text),
            ("selectedModelName", "selected_model_name", Col::Text),
            ("finalAttemptProvider", "final_attempt_provider", Col::Text),
            ("finalAttemptModel", "final_attempt_model", Col::Text),
            ("upstreamModel", "upstream_model", Col::Text),
            (
                "allAttemptedProviders",
                "all_attempted_providers",
                Col::Text,
            ),
            ("outgoingApiType", "outgoing_api_type", Col::Text),
            ("reasoningEffort", "reasoning_effort", Col::Text),
            ("requestedServiceTier", "requested_service_tier", Col::Text),
            (
                "requestedServiceTierRaw",
                "requested_service_tier_raw",
                Col::Text,
            ),
            ("serviceTier", "service_tier", Col::Text),
            ("serviceTierRaw", "service_tier_raw", Col::Text),
            ("tokensInput", "tokens_input", Col::Number),
            ("tokensOutput", "tokens_output", Col::Number),
            ("tokensReasoning", "tokens_reasoning", Col::Number),
            ("tokensCached", "tokens_cached", Col::Number),
            ("tokensCacheWrite", "tokens_cache_write", Col::Number),
            ("costInput", "cost_input", Col::Number),
            ("costOutput", "cost_output", Col::Number),
            ("costCached", "cost_cached", Col::Number),
            ("costCacheWrite", "cost_cache_write", Col::Number),
            ("costTotal", "cost_total", Col::Number),
            ("costSource", "cost_source", Col::Text),
            ("costMetadata", "cost_metadata", Col::Text),
            ("startTime", "start_time", Col::Number),
            ("durationMs", "duration_ms", Col::Number),
            ("ttftMs", "ttft_ms", Col::Number),
            ("tokensPerSec", "tokens_per_sec", Col::Number),
            ("isStreamed", "is_streamed", Col::Bool),
            ("isPassthrough", "is_passthrough", Col::Bool),
            ("isRaw", "is_raw", Col::Number),
            ("requestMethod", "request_method", Col::Text),
            ("requestPath", "request_path", Col::Text),
            ("responseStatus", "response_status", Col::Text),
            ("tokensEstimated", "tokens_estimated", Col::Number),
            ("createdAt", "created_at", Col::Number),
            ("toolsDefined", "tools_defined", Col::Number),
            ("messageCount", "message_count", Col::Number),
            (
                "parallelToolCallsEnabled",
                "parallel_tool_calls_enabled",
                Col::Number,
            ),
            ("toolCallsCount", "tool_calls_count", Col::Number),
            ("finishReason", "finish_reason", Col::Text),
            ("isVisionFallthrough", "is_vision_fallthrough", Col::Bool),
            ("isDescriptorRequest", "is_descriptor_request", Col::Bool),
            (
                "visionFallthroughModel",
                "vision_fallthrough_model",
                Col::Text,
            ),
            ("kwhUsed", "kwh_used", Col::Number),
            (
                "providerReportedCost",
                "provider_reported_cost",
                Col::Number,
            ),
        ],
    ),
    (
        "provider_cooldowns",
        &[
            ("provider", "provider", Col::Text),
            ("model", "model", Col::Text),
            ("expiry", "expiry", Col::Number),
            ("consecutiveFailures", "consecutive_failures", Col::Number),
            ("createdAt", "created_at", Col::Number),
            ("lastError", "last_error", Col::Text),
        ],
    ),
    (
        "debug_logs",
        &[
            ("id", "id", Col::Number),
            ("requestId", "request_id", Col::Text),
            ("apiKey", "api_key", Col::Text),
            ("rawRequest", "raw_request", Col::Text),
            ("transformedRequest", "transformed_request", Col::Text),
            ("rawResponse", "raw_response", Col::Text),
            ("transformedResponse", "transformed_response", Col::Text),
            ("rawResponseSnapshot", "raw_response_snapshot", Col::Text),
            (
                "transformedResponseSnapshot",
                "transformed_response_snapshot",
                Col::Text,
            ),
            ("requestHeaders", "request_headers", Col::Text),
            ("responseHeaders", "response_headers", Col::Text),
            ("responseStatus", "response_status", Col::Number),
            ("createdAt", "created_at", Col::Number),
        ],
    ),
    (
        "inference_errors",
        &[
            ("id", "id", Col::Number),
            ("requestId", "request_id", Col::Text),
            ("date", "date", Col::Text),
            ("apiKey", "api_key", Col::Text),
            ("errorMessage", "error_message", Col::Text),
            ("errorStack", "error_stack", Col::Text),
            ("details", "details", Col::Text),
            ("createdAt", "created_at", Col::Number),
        ],
    ),
    (
        "provider_performance",
        &[
            ("id", "id", Col::Number),
            ("provider", "provider", Col::Text),
            ("model", "model", Col::Text),
            ("canonicalModelName", "canonical_model_name", Col::Text),
            ("requestId", "request_id", Col::Text),
            ("timeToFirstTokenMs", "time_to_first_token_ms", Col::Number),
            ("totalTokens", "total_tokens", Col::Number),
            ("durationMs", "duration_ms", Col::Number),
            ("tokensPerSec", "tokens_per_sec", Col::Number),
            ("e2eTokensPerSec", "e2e_tokens_per_sec", Col::Number),
            ("failureCount", "failure_count", Col::Number),
            ("successCount", "success_count", Col::Number),
            ("createdAt", "created_at", Col::Number),
        ],
    ),
    (
        "quota_state",
        &[
            ("keyName", "key_name", Col::Text),
            ("quotaName", "quota_name", Col::Text),
            ("limitType", "limit_type", Col::Text),
            ("currentUsage", "current_usage", Col::Number),
            ("lastUpdated", "last_updated", Col::Number),
            ("windowStart", "window_start", Col::Number),
        ],
    ),
    (
        "meter_snapshots",
        &[
            ("id", "id", Col::Number),
            ("checkerId", "checker_id", Col::Text),
            ("checkerType", "checker_type", Col::Text),
            ("provider", "provider", Col::Text),
            ("meterKey", "meter_key", Col::Text),
            ("kind", "kind", Col::Text),
            ("unit", "unit", Col::Text),
            ("label", "label", Col::Text),
            ("group", "group", Col::Text),
            ("scope", "scope", Col::Text),
            ("limit", "limit", Col::Number),
            ("used", "used", Col::Number),
            ("remaining", "remaining", Col::Number),
            ("utilizationState", "utilization_state", Col::Text),
            ("utilizationPercent", "utilization_percent", Col::Number),
            ("status", "status", Col::Text),
            ("periodValue", "period_value", Col::Number),
            ("periodUnit", "period_unit", Col::Text),
            ("periodCycle", "period_cycle", Col::Text),
            ("resetsAt", "resets_at", Col::Number),
            ("success", "success", Col::Bool),
            ("errorMessage", "error_message", Col::Text),
            ("checkedAt", "checked_at", Col::Number),
            ("createdAt", "created_at", Col::Number),
        ],
    ),
    (
        "quota_snapshots",
        &[
            ("id", "id", Col::Number),
            ("provider", "provider", Col::Text),
            ("checkerId", "checker_id", Col::Text),
            ("groupId", "group_id", Col::Text),
            ("windowType", "window_type", Col::Text),
            ("description", "description", Col::Text),
            ("checkedAt", "checked_at", Col::Number),
            ("limit", "limit", Col::Number),
            ("used", "used", Col::Number),
            ("remaining", "remaining", Col::Number),
            ("utilizationPercent", "utilization_percent", Col::Number),
            ("unit", "unit", Col::Text),
            ("resetsAt", "resets_at", Col::Number),
            ("status", "status", Col::Text),
            ("success", "success", Col::Number),
            ("errorMessage", "error_message", Col::Text),
            ("createdAt", "created_at", Col::Number),
        ],
    ),
    (
        "mcp_request_usage",
        &[
            ("id", "id", Col::Number),
            ("requestId", "request_id", Col::Text),
            ("createdAt", "created_at", Col::Timestamp),
            ("startTime", "start_time", Col::Number),
            ("durationMs", "duration_ms", Col::Number),
            ("serverName", "server_name", Col::Text),
            ("upstreamUrl", "upstream_url", Col::Text),
            ("method", "method", Col::Text),
            ("jsonrpcMethod", "jsonrpc_method", Col::Text),
            ("toolName", "tool_name", Col::Text),
            ("apiKey", "api_key", Col::Text),
            ("attribution", "attribution", Col::Text),
            ("sourceIp", "source_ip", Col::Text),
            ("responseStatus", "response_status", Col::Number),
            ("isStreamed", "is_streamed", Col::Bool),
            ("hasDebug", "has_debug", Col::Bool),
            ("errorCode", "error_code", Col::Text),
            ("errorMessage", "error_message", Col::Text),
        ],
    ),
    (
        "mcp_debug_logs",
        &[
            ("id", "id", Col::Number),
            ("requestId", "request_id", Col::Text),
            ("rawRequestHeaders", "raw_request_headers", Col::Text),
            ("rawRequestBody", "raw_request_body", Col::Text),
            ("rawResponseHeaders", "raw_response_headers", Col::Text),
            ("rawResponseBody", "raw_response_body", Col::Text),
            ("createdAt", "created_at", Col::Timestamp),
        ],
    ),
    (
        "responses",
        &[
            ("id", "id", Col::Text),
            ("object", "object", Col::Text),
            ("createdAt", "created_at", Col::Number),
            ("completedAt", "completed_at", Col::Number),
            ("status", "status", Col::Text),
            ("model", "model", Col::Text),
            ("outputItems", "output_items", Col::Text),
            ("instructions", "instructions", Col::Text),
            ("temperature", "temperature", Col::Number),
            ("topP", "top_p", Col::Number),
            ("maxOutputTokens", "max_output_tokens", Col::Number),
            ("topLogprobs", "top_logprobs", Col::Number),
            ("parallelToolCalls", "parallel_tool_calls", Col::Number),
            ("toolChoice", "tool_choice", Col::Text),
            ("tools", "tools", Col::Text),
            ("textConfig", "text_config", Col::Text),
            ("reasoningConfig", "reasoning_config", Col::Text),
            ("usageInputTokens", "usage_input_tokens", Col::Number),
            ("usageOutputTokens", "usage_output_tokens", Col::Number),
            (
                "usageReasoningTokens",
                "usage_reasoning_tokens",
                Col::Number,
            ),
            ("usageCachedTokens", "usage_cached_tokens", Col::Number),
            ("usageTotalTokens", "usage_total_tokens", Col::Number),
            ("previousResponseId", "previous_response_id", Col::Text),
            ("conversationId", "conversation_id", Col::Text),
            ("store", "store", Col::Number),
            ("background", "background", Col::Number),
            ("truncation", "truncation", Col::Text),
            ("incompleteDetails", "incomplete_details", Col::Text),
            ("error", "error", Col::Text),
            ("safetyIdentifier", "safety_identifier", Col::Text),
            ("serviceTier", "service_tier", Col::Text),
            ("promptCacheKey", "prompt_cache_key", Col::Text),
            ("promptCacheRetention", "prompt_cache_retention", Col::Text),
            ("metadata", "metadata", Col::Text),
            ("plexusProvider", "plexus_provider", Col::Text),
            ("plexusTargetModel", "plexus_target_model", Col::Text),
            ("plexusApiType", "plexus_api_type", Col::Text),
            ("plexusCanonicalModel", "plexus_canonical_model", Col::Text),
        ],
    ),
    (
        "conversations",
        &[
            ("id", "id", Col::Text),
            ("createdAt", "created_at", Col::Number),
            ("updatedAt", "updated_at", Col::Number),
            ("items", "items", Col::Text),
            ("metadata", "metadata", Col::Text),
            ("plexusAccountId", "plexus_account_id", Col::Text),
        ],
    ),
    (
        "response_items",
        &[
            ("id", "id", Col::Text),
            ("responseId", "response_id", Col::Text),
            ("itemIndex", "item_index", Col::Number),
            ("itemType", "item_type", Col::Text),
            ("itemData", "item_data", Col::Text),
        ],
    ),
];

fn plexus_columns(table: &str) -> &'static [ColumnSpec] {
    PLEXUS_COLUMNS
        .iter()
        .find(|(t, _)| *t == table)
        .map(|(_, c)| *c)
        .unwrap_or(&[])
}

/// `normaliseForCsv` + `csvEscape` for one Trefoil row value, as Plexus
/// (PostgreSQL dialect) writes it.
fn plexus_csv_value(value: Option<&serde_json::Value>, kind: Col) -> String {
    use serde_json::Value;
    let text = match value {
        None | Some(Value::Null) => return "\\N".into(),
        Some(Value::Number(n)) if kind == Col::Bool => {
            if n.as_f64() == Some(1.0) {
                "true".into()
            } else {
                "false".into()
            }
        }
        Some(Value::Number(n)) => crate::engine::js::js_number_text(n.as_f64().unwrap_or(0.0)),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::String(s)) if kind == Col::Timestamp => chrono::DateTime::parse_from_rfc3339(s)
            .or_else(|_| chrono::DateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f%#z"))
            .map(|d| {
                d.with_timezone(&chrono::Utc)
                    .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
            })
            .or_else(|_| {
                chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f").map(|d| {
                    d.and_utc()
                        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
                })
            })
            .unwrap_or_else(|_| s.clone()),
        Some(Value::String(s)) => s.clone(),
        Some(other) => crate::engine::js::stringify(other),
    };
    csv_escape(&Value::String(text))
}

#[derive(Deserialize, Default)]
struct BackupQuery {
    full: Option<bool>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/v0/management/backup", axum::routing::get(export))
        .route("/v0/management/restore", axum::routing::post(restore))
}

const MAX_BACKUP_BYTES: usize = 256 * 1024 * 1024;
const MAX_ENTRY_BYTES: u64 = 128 * 1024 * 1024;
const MAX_UNCOMPRESSED_BACKUP_BYTES: u64 = 512 * 1024 * 1024;

async fn restore(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    match resolve_principal(
        &state,
        headers.get("x-admin-key").and_then(|v| v.to_str().ok()),
    ) {
        Some(Principal::Admin) => {}
        Some(Principal::Limited { .. }) => {
            return restore_auth_error(
                StatusCode::FORBIDDEN,
                "Admin privileges required",
                "forbidden",
                403,
            )
        }
        None => {
            return restore_auth_error(StatusCode::UNAUTHORIZED, "Unauthorized", "auth_error", 401)
        }
    }
    if body.len() > MAX_BACKUP_BYTES {
        return restore_error(StatusCode::PAYLOAD_TOO_LARGE, "Backup is too large");
    }
    let Some(pool) = &state.inner.database else {
        return restore_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Restore requires a database",
        );
    };
    let result = if headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .contains("json")
    {
        let value: serde_json::Value = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(_) => {
                return json_response(
                    StatusCode::BAD_REQUEST,
                    serde_json::json!({
                        "error": {
                            "message": "Body is not valid JSON but content-type is set to 'application/json'",
                            "type": "api_error"
                        }
                    }),
                )
            }
        };
        if value.get("plexus_backup") != Some(&serde_json::Value::Bool(true)) {
            return restore_error(
                StatusCode::BAD_REQUEST,
                "Invalid backup: missing plexus_backup field",
            );
        }
        restore_config(pool, &state, &value)
            .await
            .map(|_| serde_json::json!({"success":true,"config":true}))
    } else {
        restore_archive(pool, &state, &body)
            .await
            .map(|n| serde_json::json!({"success":true,"restored":n}))
    };
    match result {
        Ok(value) => axum::Json(value).into_response(),
        Err(error) if error == "missing data" => restore_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "undefined is not an object (evaluating 'data.data.providers')",
        ),
        Err(error) => restore_error(StatusCode::BAD_REQUEST, &format!("Invalid backup: {error}")),
    }
}

fn restore_error(status: StatusCode, message: &str) -> Response {
    json_response(status, serde_json::json!({"error": message}))
}

fn restore_auth_error(status: StatusCode, message: &str, error_type: &str, code: u16) -> Response {
    json_response(
        status,
        serde_json::json!({
            "error": { "message": message, "type": error_type, "code": code }
        }),
    )
}

fn json_response(status: StatusCode, value: serde_json::Value) -> Response {
    Response::builder()
        .status(status)
        .header("content-type", "application/json; charset=utf-8")
        .body(Body::from(
            serde_json::to_vec(&value).expect("JSON value is serializable"),
        ))
        .expect("valid JSON response")
}

async fn restore_archive(
    pool: &sqlx::PgPool,
    state: &AppState,
    input: &[u8],
) -> Result<HashMap<String, u64>, String> {
    use flate2::read::GzDecoder;
    let mut archive = tar::Archive::new(GzDecoder::new(input));
    let mut files = HashMap::new();
    let mut total_size = 0u64;
    for item in archive.entries().map_err(|e| e.to_string())? {
        let mut entry = item.map_err(|e| e.to_string())?;
        let path = entry.path().map_err(|e| e.to_string())?.to_path_buf();
        let name = path.to_str().ok_or("invalid archive path")?.to_string();
        if !valid_archive_entry(
            &path,
            entry.header().entry_type(),
            entry.size(),
            total_size,
            files.contains_key(&name),
        ) {
            return Err("unsafe, unknown, duplicate, or oversized archive entry".into());
        }
        let mut data = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut data).map_err(|e| e.to_string())?;
        total_size += data.len() as u64;
        files.insert(name, data);
    }
    let manifest: serde_json::Value = serde_json::from_slice(
        files
            .get("manifest.json")
            .ok_or("manifest.json is required")?,
    )
    .map_err(|_| "invalid manifest")?;
    if manifest.get("plexus_backup") != Some(&serde_json::Value::Bool(true))
        || manifest.get("version").and_then(|v| v.as_i64()) != Some(1)
    {
        return Err("unsupported backup version".into());
    }
    let config: serde_json::Value =
        serde_json::from_slice(files.get("config.json").ok_or("config.json is required")?)
            .map_err(|_| "invalid config")?;
    let mut parsed = HashMap::new();
    for table in TABLES {
        if let Some(data) = files.get(&format!("{table}.csv.gz")) {
            parsed.insert(*table, parse_csv(data)?);
        }
    }
    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
    for table in TABLES.iter().rev() {
        sqlx::query(&format!("DELETE FROM {table}"))
            .execute(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
    }
    let mut counts = HashMap::new();
    for table in TABLES {
        let rows: Vec<_> = parsed
            .remove(table)
            .unwrap_or_default()
            .into_iter()
            .map(|row| plexus_row_to_db(table, row))
            .collect();
        for row in &rows {
            sqlx::query(&format!(
                "INSERT INTO {table} SELECT * FROM jsonb_populate_record(NULL::{table}, $1::jsonb)"
            ))
            .bind(serde_json::to_string(row).unwrap())
            .execute(&mut *tx)
            .await
            .map_err(|e| format!("{table}: {e}"))?;
        }
        counts.insert((*table).to_string(), rows.len() as u64);
    }
    tx.commit().await.map_err(|e| e.to_string())?;
    // `config.json` holds the bare config; Plexus wraps it in an envelope before
    // restoring. Tolerate a file that is already wrapped.
    let envelope = if config.get("data").is_some() {
        config
    } else {
        serde_json::json!({ "data": config })
    };
    restore_config(pool, state, &envelope).await?;
    Ok(counts)
}

fn valid_archive_entry(
    path: &std::path::Path,
    entry_type: tar::EntryType,
    size: u64,
    total_size: u64,
    duplicate: bool,
) -> bool {
    let Some(name) = path.to_str() else {
        return false;
    };
    entry_type.is_file()
        && path.components().count() == 1
        && (name == "manifest.json"
            || name == "config.json"
            || TABLES.iter().any(|table| {
                name == format!("{table}.csv.gz") || (name == format!("{table}.csv") && size == 0)
            }))
        && size <= MAX_ENTRY_BYTES
        && total_size.saturating_add(size) <= MAX_UNCOMPRESSED_BACKUP_BYTES
        && !duplicate
}

/// Maps a Plexus CSV row (drizzle property names, `"true"`/`"false"`
/// booleans) onto Trefoil's columns; snake_case headers pass through.
fn plexus_row_to_db(
    table: &str,
    row: serde_json::Map<String, serde_json::Value>,
) -> serde_json::Map<String, serde_json::Value> {
    let columns = plexus_columns(table);
    row.into_iter()
        .map(|(key, value)| {
            let Some((_, column, kind)) = columns
                .iter()
                .find(|(prop, column, _)| *prop == key || *column == key)
            else {
                return (key, value);
            };
            let value = match (kind, &value) {
                (Col::Bool, serde_json::Value::Bool(b))
                    if !(table == "meter_snapshots" && *column == "success") =>
                {
                    serde_json::Value::from(*b as i32)
                }
                // Plexus on SQLite exports its timestamp-mode integer columns
                // as ISO strings; Trefoil stores them as epoch milliseconds.
                (Col::Number, serde_json::Value::String(text)) => {
                    match chrono::DateTime::parse_from_rfc3339(text) {
                        Ok(moment) => serde_json::Value::from(moment.timestamp_millis()),
                        Err(_) => value,
                    }
                }
                _ => value,
            };
            ((*column).to_owned(), value)
        })
        .collect()
}

fn parse_csv(data: &[u8]) -> Result<Vec<serde_json::Map<String, serde_json::Value>>, String> {
    use flate2::read::GzDecoder;
    use std::io::Read;
    let mut text = String::new();
    GzDecoder::new(data)
        .take(MAX_ENTRY_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(|_| "invalid compressed CSV")?;
    if text.len() as u64 > MAX_ENTRY_BYTES {
        return Err("compressed CSV expands beyond entry limit".into());
    }
    let mut reader = csv::Reader::from_reader(text.as_bytes());
    let headers = reader.headers().map_err(|_| "invalid CSV headers")?.clone();
    let mut rows = Vec::new();
    for record in reader.records() {
        let record = record.map_err(|_| "invalid CSV row")?;
        if record.len() != headers.len() {
            return Err("CSV column mismatch".into());
        }
        let mut row = serde_json::Map::new();
        for (key, value) in headers.iter().zip(record.iter()) {
            row.insert(
                key.into(),
                if value == "\\N" {
                    serde_json::Value::Null
                } else {
                    serde_json::from_str(value)
                        .unwrap_or_else(|_| serde_json::Value::String(value.into()))
                },
            );
        }
        rows.push(row);
    }
    Ok(rows)
}

async fn restore_config(
    pool: &sqlx::PgPool,
    state: &AppState,
    envelope: &serde_json::Value,
) -> Result<(), String> {
    let data = envelope.get("data").ok_or("missing data")?;
    // Plexus restores raw configs through `saveProvider` (no schema parse);
    // credentials are restored afterwards, so no account link exists yet.
    let raw_providers: serde_json::Map<String, serde_json::Value> =
        serde_json::from_value(data.get("providers").cloned().ok_or("missing providers")?)
            .map_err(|_| "invalid providers")?;
    let providers: HashMap<String, crate::state::ProviderConfig> = raw_providers
        .iter()
        .map(|(slug, raw)| {
            (
                slug.clone(),
                crate::provider_view::from_view(
                    crate::provider_view::repository_view(raw, None),
                    1,
                ),
            )
        })
        .collect();
    // Like providers, aliases are raw Plexus repository configs: normalize
    // them through the alias view instead of strict struct deserialization.
    let raw_aliases: serde_json::Map<String, serde_json::Value> =
        serde_json::from_value(data.get("models").cloned().ok_or("missing models")?)
            .map_err(|_| "invalid models")?;
    let aliases: HashMap<String, crate::state::ModelConfig> = raw_aliases
        .iter()
        .map(|(slug, raw)| {
            (
                slug.clone(),
                crate::alias_view::from_view(crate::alias_view::repository_view(raw)),
            )
        })
        .collect();
    let keys: HashMap<String, crate::state::KeyConfig> =
        serde_json::from_value(data.get("keys").cloned().ok_or("missing keys")?)
            .map_err(|_| "invalid keys")?;

    // A restore is a replacement, not an upsert.  Clear records that are not
    // present in the backup so removed providers, models, and keys do not
    // survive a full restore.
    crate::database::delete_all_aliases(pool)
        .await
        .map_err(|e| e.to_string())?;
    crate::database::delete_all_providers(pool)
        .await
        .map_err(|e| e.to_string())?;
    crate::database::delete_all_keys(pool)
        .await
        .map_err(|e| e.to_string())?;
    for (name, config) in &keys {
        crate::database::upsert_key(pool, name, config)
            .await
            .map_err(|e| e.to_string())?;
    }
    for (name, config) in &providers {
        crate::database::upsert_provider(pool, name, config)
            .await
            .map_err(|e| e.to_string())?;
    }
    for (name, config) in &aliases {
        crate::database::upsert_alias(pool, name, config)
            .await
            .map_err(|e| e.to_string())?;
    }
    *state.inner.keys.write().unwrap() = keys;
    *state.inner.providers.write().unwrap() = providers;
    *state.inner.aliases.write().unwrap() = aliases;
    restore_extras(pool, state, data).await
}

/// The rest of a Plexus config backup: user quotas, settings, MCP servers and
/// keys, and OAuth credentials. Every section is optional (older Plexus
/// revisions omit `user_quotas`/`mcp_keys`); present sections are upserted.
async fn restore_extras(
    pool: &sqlx::PgPool,
    state: &AppState,
    data: &serde_json::Value,
) -> Result<(), String> {
    let section = |name: &str| data.get(name).filter(|v| !v.is_null());

    if let Some(raw) = section("user_quotas") {
        let quotas: HashMap<String, crate::state::QuotaDefinition> =
            serde_json::from_value(raw.clone()).map_err(|e| format!("invalid user_quotas: {e}"))?;
        for (name, quota) in &quotas {
            crate::database::upsert_user_quota(pool, name, quota)
                .await
                .map_err(|e| e.to_string())?;
        }
        state.inner.user_quotas.write().unwrap().extend(quotas);
    }

    if let Some(raw) = section("settings") {
        let settings = raw.as_object().ok_or("invalid settings")?;
        crate::database::upsert_settings_bulk(pool, settings)
            .await
            .map_err(|e| e.to_string())?;
        let mut live = state.inner.settings.write().unwrap();
        for (key, value) in settings {
            live.insert(key.clone(), value.clone());
        }
    }

    if let Some(raw) = section("mcp_servers") {
        for (name, config) in raw.as_object().ok_or("invalid mcp_servers")? {
            crate::mcp::save_server(state, name, config)
                .await
                .map_err(|_| format!("could not restore MCP server {name}"))?;
        }
    }
    if let Some(raw) = section("mcp_keys") {
        let keys = raw
            .as_array()
            .ok_or("invalid mcp_keys")?
            .iter()
            .map(|entry| {
                Ok((
                    entry
                        .get("serverName")
                        .and_then(|v| v.as_str())
                        .ok_or("mcp_keys entry is missing serverName")?
                        .to_owned(),
                    entry
                        .get("key")
                        .and_then(|v| v.as_str())
                        .ok_or("mcp_keys entry is missing key")?
                        .to_owned(),
                    entry
                        .get("isActive")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(true),
                ))
            })
            .collect::<Result<Vec<_>, &str>>()?;
        crate::mcp::restore_keys(pool, &keys).await?;
    }

    if let Some(raw) = section("oauth_credentials") {
        for entry in raw.as_array().ok_or("invalid oauth_credentials")? {
            let text = |field: &str| entry.get(field).and_then(|v| v.as_str());
            let (Some(provider), Some(account), Some(access)) = (
                text("provider_type"),
                text("account_id"),
                text("access_token"),
            ) else {
                return Err("oauth_credentials entry is missing required fields".into());
            };
            let mut credentials = crate::oauth_login::OAuthCredentials::new(
                access,
                text("refresh_token").unwrap_or_default(),
                entry
                    .get("expires_at")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0),
            );
            credentials.enterprise_url = text("enterprise_url").map(str::to_owned);
            crate::database::upsert_oauth_credential(
                pool,
                provider,
                account,
                &credentials.access,
                &credentials.stored_refresh(),
                credentials.expires,
            )
            .await
            .map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// Decrypted OAuth credentials in Plexus's backup shape. GitHub Copilot's
/// enterprise domain is stored as a `<domain>\n` prefix on the refresh token
/// (see `OAuthCredentials::stored_refresh`); split it back out for the backup.
async fn export_oauth_credentials(pool: &sqlx::PgPool) -> Result<Vec<serde_json::Value>, String> {
    let credentials = crate::database::load_oauth_credentials(pool)
        .await
        .map_err(|e| e.to_string())?;
    Ok(credentials
        .into_iter()
        .map(|(provider, account, access, refresh, expires_at)| {
            let (enterprise_url, refresh) = match refresh.split_once('\n') {
                Some((domain, token)) if provider == "github-copilot" => {
                    (Some(domain.to_owned()), token.to_owned())
                }
                _ => (None, refresh),
            };
            serde_json::json!({
                "provider_type": provider,
                "account_id": account,
                "access_token": access,
                "refresh_token": refresh,
                "enterprise_url": enterprise_url,
                "expires_at": expires_at,
            })
        })
        .collect())
}

async fn export(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<BackupQuery>,
) -> Response {
    match resolve_principal(
        &state,
        headers.get("x-admin-key").and_then(|v| v.to_str().ok()),
    ) {
        Some(Principal::Admin) => {}
        Some(Principal::Limited { .. }) => {
            return restore_auth_error(
                StatusCode::FORBIDDEN,
                "Admin privileges required",
                "forbidden",
                403,
            )
        }
        None => {
            return restore_auth_error(StatusCode::UNAUTHORIZED, "Unauthorized", "auth_error", 401)
        }
    }
    let Some(pool) = &state.inner.database else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Backup requires a database",
        )
            .into_response();
    };
    crate::system_logs::event(
        &state,
        "info",
        if query.full.unwrap_or(false) {
            "[Backup] Starting full backup export"
        } else {
            "[Backup] Starting config-only backup export"
        },
        [("module".into(), serde_json::json!("backup"))],
    );
    let (mcp_servers, mcp_keys) = match crate::mcp::export_config(pool).await {
        Ok(exported) => exported,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to create backup: {error}"),
            )
                .into_response()
        }
    };
    let oauth_credentials = match export_oauth_credentials(pool).await {
        Ok(credentials) => credentials,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to create backup: {error}"),
            )
                .into_response()
        }
    };
    let config = serde_json::json!({
        "providers": state.inner.providers.read().unwrap().clone(),
        "models": state.inner.aliases.read().unwrap().clone(),
        "keys": state.inner.keys.read().unwrap().clone(),
        "user_quotas": state.inner.user_quotas.read().unwrap().clone(),
        "mcp_servers": mcp_servers, "mcp_keys": mcp_keys,
        "settings": state.inner.settings.read().unwrap().clone(),
        "oauth_credentials": oauth_credentials,
    });
    if !query.full.unwrap_or(false) {
        return json_response(
            StatusCode::OK,
            serde_json::json!({
                "plexus_backup": true, "version": 1,
                "created_at": chrono::Utc::now().to_rfc3339(), "dialect": "postgres", "data": config
            }),
        );
    }
    match build_archive(pool, config).await {
        Ok(bytes) => (
            [
                ("content-type", "application/gzip"),
                ("content-disposition", "attachment; filename=backup.tar.gz"),
            ],
            bytes,
        )
            .into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to create backup: {error}"),
        )
            .into_response(),
    }
}

async fn build_archive(
    pool: &sqlx::PgPool,
    config: serde_json::Value,
) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
    let mut compressed = GzEncoder::new(Vec::new(), Compression::default());
    {
        let mut archive = Builder::new(&mut compressed);
        let manifest = serde_json::json!({"plexus_backup": true, "version": 1, "created_at": chrono::Utc::now().to_rfc3339(), "dialect": "postgres", "tables": TABLES});
        append_bytes(
            &mut archive,
            "manifest.json",
            serde_json::to_vec_pretty(&manifest)?.as_slice(),
        )?;
        append_bytes(
            &mut archive,
            "config.json",
            serde_json::to_vec_pretty(&config)?.as_slice(),
        )?;
        for table in TABLES {
            let rows = sqlx::query(&format!(
                "SELECT row_to_json(t)::text AS row FROM {table} t"
            ))
            .fetch_all(pool)
            .await?;
            let mut objects = Vec::with_capacity(rows.len());
            for row in rows {
                objects.push(serde_json::from_str::<
                    serde_json::Map<String, serde_json::Value>,
                >(&row.try_get::<String, _>("row")?)?);
            }
            // `Object.keys(rows[0])`: the Plexus schema's property names and
            // order; lines joined by "\n" with no trailing newline.
            let mut csv = String::new();
            if !objects.is_empty() {
                let columns = plexus_columns(table);
                let mut lines = vec![columns
                    .iter()
                    .map(|(prop, _, _)| *prop)
                    .collect::<Vec<_>>()
                    .join(",")];
                for object in &objects {
                    lines.push(
                        columns
                            .iter()
                            .map(|(_, column, kind)| plexus_csv_value(object.get(*column), *kind))
                            .collect::<Vec<_>>()
                            .join(","),
                    );
                }
                csv = lines.join("\n");
            }
            if csv.is_empty() {
                append_bytes(&mut archive, &format!("{table}.csv"), &[])?;
                continue;
            }
            let mut csv_compressed = GzEncoder::new(Vec::new(), Compression::default());
            csv_compressed.write_all(csv.as_bytes())?;
            let csv_bytes = csv_compressed.finish()?;
            append_bytes(&mut archive, &format!("{table}.csv.gz"), &csv_bytes)?;
        }
        archive.finish()?;
    }
    Ok(compressed.finish()?)
}

fn append_bytes<W: Write>(
    archive: &mut Builder<W>,
    name: &str,
    data: &[u8],
) -> Result<(), std::io::Error> {
    let mut header = tar::Header::new_gnu();
    header.set_size(data.len() as u64);
    header.set_mode(0o600);
    header.set_cksum();
    archive.append_data(&mut header, name, data)
}

#[cfg(test)]
mod tests {
    use super::{plexus_row_to_db, valid_archive_entry, StatusCode};
    use std::path::Path;

    #[test]
    fn sqlite_iso_timestamps_in_numeric_columns_become_epoch_millis() {
        let row = |checked_at: &str| {
            let mut row = serde_json::Map::new();
            row.insert("checkedAt".into(), checked_at.into());
            row.insert("provider".into(), "2026-10-29T22:58:06.000Z".into());
            plexus_row_to_db("meter_snapshots", row)
        };
        let converted = row("2026-10-29T22:58:06.000Z");
        assert_eq!(converted["checked_at"], 1_793_314_686_000_i64);
        // Text columns keep date-looking strings untouched.
        assert_eq!(converted["provider"], "2026-10-29T22:58:06.000Z");
        // Non-date strings in numeric columns pass through for the database to judge.
        assert_eq!(row("not a date")["checked_at"], "not a date");
    }

    #[test]
    fn rejects_traversal_links_duplicates_and_oversized_entries() {
        assert!(valid_archive_entry(
            Path::new("config.json"),
            tar::EntryType::Regular,
            10,
            0,
            false
        ));
        for path in ["../config.json", "/config.json", "nested/config.json"] {
            assert!(!valid_archive_entry(
                Path::new(path),
                tar::EntryType::Regular,
                10,
                0,
                false
            ));
        }
        assert!(!valid_archive_entry(
            Path::new("config.json"),
            tar::EntryType::Symlink,
            10,
            0,
            false
        ));
        assert!(!valid_archive_entry(
            Path::new("config.json"),
            tar::EntryType::Regular,
            10,
            0,
            true
        ));
        assert!(!valid_archive_entry(
            Path::new("config.json"),
            tar::EntryType::Regular,
            super::MAX_ENTRY_BYTES + 1,
            0,
            false
        ));
        assert!(!valid_archive_entry(
            Path::new("config.json"),
            tar::EntryType::Regular,
            1,
            super::MAX_UNCOMPRESSED_BACKUP_BYTES,
            false
        ));
    }

    #[test]
    fn restore_errors_use_reference_json_envelope() {
        let response = super::restore_error(StatusCode::BAD_REQUEST, "bad backup");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response.headers()["content-type"],
            "application/json; charset=utf-8"
        );
    }

    #[tokio::test]
    async fn restore_error_envelope_contains_message() {
        let response = super::restore_error(StatusCode::UNAUTHORIZED, "Unauthorized");
        let body = response.into_body();
        let bytes = http_body_util::BodyExt::collect(body)
            .await
            .expect("error response body");
        let value: serde_json::Value = serde_json::from_slice(&bytes.to_bytes()).unwrap();
        assert_eq!(value, serde_json::json!({"error": "Unauthorized"}));
    }

    #[test]
    fn config_restore_rejects_missing_backup_marker_before_database_work() {
        assert_ne!(
            serde_json::json!({"some": "data"}).get("plexus_backup"),
            Some(&serde_json::Value::Bool(true))
        );
    }
}

fn csv_escape(value: &serde_json::Value) -> String {
    let text = match value {
        serde_json::Value::Null => "\\N".to_string(),
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    if text == "\\N" {
        return "\"\\N\"".into();
    }
    if text.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", text.replace('"', "\"\""))
    } else {
        text
    }
}
