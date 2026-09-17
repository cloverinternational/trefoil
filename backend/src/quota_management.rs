//! Administrative quota-state management.

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json, Router,
};
use serde_json::Value;

use crate::{
    auth::{resolve_principal, Principal},
    AppState,
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/v0/management/quota/clear",
            axum::routing::post(clear_quota),
        )
        .route(
            "/v0/management/quota/status/:key",
            axum::routing::get(quota_status),
        )
        .route(
            "/v0/management/quota/recompute",
            axum::routing::post(recompute_quota),
        )
}

async fn recompute_quota(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    let Some(key) = body
        .get("key")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
    else {
        return error(
            StatusCode::BAD_REQUEST,
            "Missing required field(s): key, quota",
            "invalid_request_error",
        );
    };
    let Some(quota) = body
        .get("quota")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
    else {
        return error(
            StatusCode::BAD_REQUEST,
            "Missing required field(s): key, quota",
            "invalid_request_error",
        );
    };
    if !state.inner.keys.read().unwrap().contains_key(key) {
        return error(
            StatusCode::NOT_FOUND,
            &format!("Key not found: {key}"),
            "not_found_error",
        );
    }
    let config = state.inner.keys.read().unwrap().get(key).cloned().unwrap();
    let attached: Vec<String> = if config.quota_names.is_empty() {
        state
            .inner
            .settings
            .read()
            .unwrap()
            .get("default_quotas")
            .and_then(Value::as_array)
            .map(|v| {
                v.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    } else {
        config.quota_names.clone()
    };
    if !attached.iter().any(|name| name == quota) {
        return error(
            StatusCode::BAD_REQUEST,
            "Quota is not attached to key",
            "invalid_request_error",
        );
    }
    let Some(definition) = state.inner.user_quotas.read().unwrap().get(quota).cloned() else {
        return error(
            StatusCode::BAD_REQUEST,
            "Quota is not attached to key",
            "invalid_request_error",
        );
    };
    if definition.quota_type == "rolling" && definition.limit_type != "cost" {
        return error(
            StatusCode::BAD_REQUEST,
            "unsupported_quota_type: Cannot recompute a leaky rolling quota",
            "invalid_request_error",
        );
    }
    let Some(pool) = &state.inner.database else {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "PostgreSQL is required",
            "server_error",
        );
    };
    let usage: f64 = match definition.limit_type.as_str() { "requests" => sqlx::query_scalar("SELECT count(*)::double precision FROM request_usage WHERE api_key=$1 AND response_status='success'").bind(key).fetch_one(pool).await.unwrap_or(0.0), "tokens" => sqlx::query_scalar("SELECT coalesce(sum(coalesce(tokens_input,0)+coalesce(tokens_output,0)+coalesce(tokens_reasoning,0)),0)::double precision FROM request_usage WHERE api_key=$1 AND response_status='success'").bind(key).fetch_one(pool).await.unwrap_or(0.0), "cost" => sqlx::query_scalar("SELECT coalesce(sum(coalesce(cost_total,0)),0)::double precision FROM request_usage WHERE api_key=$1 AND response_status='success'").bind(key).fetch_one(pool).await.unwrap_or(0.0), _ => 0.0 };
    let now = chrono::Utc::now().timestamp_millis();
    if sqlx::query("INSERT INTO quota_state (key_name,quota_name,limit_type,current_usage,last_updated) VALUES ($1,$2,$3,$4,$5) ON CONFLICT(key_name,quota_name) DO UPDATE SET current_usage=EXCLUDED.current_usage,last_updated=EXCLUDED.last_updated").bind(key).bind(quota).bind(&definition.limit_type).bind(usage).bind(now).execute(pool).await.is_err() { return error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to recompute quota", "server_error"); }
    Json(serde_json::json!({"success":true,"key":key,"quota":quota,"usage":usage,"windowStartMs":now,"message":format!("Quota '{quota}' recomputed successfully")})).into_response()
}

#[allow(clippy::result_large_err)]
fn require_admin(state: &AppState, headers: &HeaderMap) -> Result<(), Response> {
    let key = headers.get("x-admin-key").and_then(|v| v.to_str().ok());
    match resolve_principal(state,key){
        Some(Principal::Admin)=>Ok(()),
        Some(Principal::Limited{..})=>Err((StatusCode::FORBIDDEN,Json(serde_json::json!({"error":{"message":"Admin privileges required","type":"forbidden","code":403}}))).into_response()),
        None=>Err((StatusCode::UNAUTHORIZED,Json(serde_json::json!({"error":{"message":"Unauthorized","type":"auth_error","code":401}}))).into_response()),
    }
}

async fn clear_quota(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    let key = body
        .get("key")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty());
    let Some(key) = key else {
        return error(
            StatusCode::BAD_REQUEST,
            "Missing required field: key",
            "invalid_request_error",
        );
    };
    let quota = body
        .get("quota")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty());
    if let Some(quota) = quota {
        let keys = state.inner.keys.read().unwrap();
        let Some(config) = keys.get(key) else {
            return error(
                StatusCode::NOT_FOUND,
                &format!("Key not found: {key}"),
                "not_found_error",
            );
        };
        let attached: Vec<String> = if config.quota_names.is_empty() {
            state
                .inner
                .settings
                .read()
                .unwrap()
                .get("default_quotas")
                .and_then(Value::as_array)
                .map(|v| {
                    v.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default()
        } else {
            config.quota_names.clone()
        };
        if !attached.iter().any(|attached| attached == quota) {
            return error(
                StatusCode::BAD_REQUEST,
                &format!("Quota '{quota}' is not attached to key '{key}'"),
                "invalid_request_error",
            );
        }
    }
    if let Some(pool) = &state.inner.database {
        let result = if let Some(quota) = quota {
            sqlx::query("DELETE FROM quota_state WHERE key_name=$1 AND quota_name=$2")
                .bind(key)
                .bind(quota)
                .execute(pool)
                .await
        } else {
            sqlx::query("DELETE FROM quota_state WHERE key_name=$1")
                .bind(key)
                .execute(pool)
                .await
        };
        if let Err(error) = result {
            return error_response(&error.to_string());
        }
    }
    Json(serde_json::json!({"success":true,"key":key,"quota":quota,"message":quota.map(|q|format!("Quota '{q}' reset successfully")).unwrap_or_else(||"Quota reset successfully".into())})).into_response()
}
fn error(status: StatusCode, message: &str, error_type: &str) -> Response {
    (
        status,
        Json(serde_json::json!({"error":{"message":message,"type":error_type}})),
    )
        .into_response()
}
fn error_response(message: &str) -> Response {
    error(StatusCode::INTERNAL_SERVER_ERROR, message, "server_error")
}

async fn quota_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(key): Path<String>,
) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    let key_config = {
        let keys = state.inner.keys.read().unwrap();
        let Some(config) = keys.get(&key) else {
            return error(
                StatusCode::NOT_FOUND,
                &format!("Key not found: {key}"),
                "not_found_error",
            );
        };
        config.clone()
    };
    let (names, source) = if key_config.quota_names.is_empty() {
        let names = state
            .inner
            .settings
            .read()
            .unwrap()
            .get("default_quotas")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        (names, "default")
    } else {
        (key_config.quota_names, "assigned")
    };
    let definitions = state.inner.user_quotas.read().unwrap().clone();
    let now = chrono::Utc::now().timestamp_millis();
    let mut snapshots = Vec::new();
    for name in names {
        let Some(definition) = definitions.get(&name) else {
            continue;
        };
        let state_row: Option<(f32, i64, Option<i64>, String)> = if let Some(pool) =
            &state.inner.database
        {
            sqlx::query_as("SELECT current_usage,last_updated,window_start,limit_type FROM quota_state WHERE key_name=$1 AND quota_name=$2")
                .bind(&key).bind(&name).fetch_optional(pool).await.unwrap_or(None)
        } else {
            None
        };
        let current = state_row
            .as_ref()
            .map(|row| f64::from(row.0))
            .unwrap_or(0.0);
        let (current, resets_at_ms) =
            quota_usage_and_reset(definition, state_row.as_ref(), current, now);
        let remaining = (definition.limit - current).max(0.0);
        let allowed = current < definition.limit;
        let mut scope = serde_json::Map::new();
        for (key, values) in [
            ("allowedModels", &definition.allowed_models),
            ("allowedProviders", &definition.allowed_providers),
            ("excludedModels", &definition.excluded_models),
            ("excludedProviders", &definition.excluded_providers),
        ] {
            if values.as_ref().is_some_and(|values| !values.is_empty()) {
                scope.insert(key.into(), serde_json::json!(values));
            }
        }
        let global = scope.is_empty();
        let mut snapshot = serde_json::json!({
            "name": name, "limitType": definition.limit_type,
            "limit": definition.limit, "currentUsage": current,
            "remaining": remaining, "allowed": allowed,
            "resetsAt": iso_ms(resets_at_ms), "scope": scope,
            "global": global, "shared": definition.shared.unwrap_or(false),
            "source": source,
        });
        if let Some(warn_at) = definition.warn_at {
            snapshot["warnAt"] = serde_json::json!(warn_at);
        }
        snapshots.push(snapshot);
    }
    let constrained = snapshots.iter().min_by(|a, b| {
        ratio(a)
            .partial_cmp(&ratio(b))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Json(serde_json::json!({
        "key": key,
        "quotas": snapshots,
        "quota_name": constrained.map(|v| v["name"].clone()).unwrap_or(Value::Null),
        "allowed": constrained.and_then(|v| v["allowed"].as_bool()).unwrap_or(true),
        "current_usage": constrained.map(|v| v["currentUsage"].clone()).unwrap_or(serde_json::json!(0)),
        "limit": constrained.map(|v| v["limit"].clone()).unwrap_or(Value::Null),
        "remaining": constrained.map(|v| v["remaining"].clone()).unwrap_or(Value::Null),
        "resets_at": constrained.map(|v| v["resetsAt"].clone()).unwrap_or(Value::Null),
    })).into_response()
}

fn ratio(value: &Value) -> f64 {
    let limit = value["limit"].as_f64().unwrap_or(0.0);
    if limit <= 0.0 {
        0.0
    } else {
        value["remaining"].as_f64().unwrap_or(0.0) / limit
    }
}

fn iso_ms(value: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(value)
        .map(|value| value.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_default()
}

fn quota_usage_and_reset(
    definition: &crate::state::QuotaDefinition,
    row: Option<&(f32, i64, Option<i64>, String)>,
    stored: f64,
    now: i64,
) -> (f64, i64) {
    if definition.quota_type == "rolling" {
        let duration = duration_ms(definition.duration.as_deref()).unwrap_or(0);
        if matches!(definition.limit_type.as_str(), "requests" | "tokens") {
            let last_updated = row.map(|row| row.1).unwrap_or(now);
            let leak_rate = if duration > 0 {
                definition.limit / duration as f64
            } else {
                0.0
            };
            let current = (stored - (now - last_updated).max(0) as f64 * leak_rate).max(0.0);
            let reset = if definition.limit > 0.0 {
                now + ((current / definition.limit) * duration as f64) as i64
            } else {
                now
            };
            return (current, reset);
        }
        let start = align_period(now, duration);
        let current = if row.and_then(|row| row.2) == Some(start) {
            stored
        } else {
            0.0
        };
        return (current, start + duration);
    }
    let start = calendar_start(&definition.quota_type, now);
    let current = if row.and_then(|row| row.2) == Some(start) {
        stored
    } else {
        0.0
    };
    (current, calendar_end(&definition.quota_type, start))
}

fn duration_ms(value: Option<&str>) -> Option<i64> {
    let value = value?.trim();
    let split = value.len().checked_sub(1)?;
    let number: i64 = value[..split].parse().ok()?;
    number.checked_mul(match &value[split..] {
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => return None,
    })
}
fn align_period(now: i64, duration: i64) -> i64 {
    if duration > 0 {
        now - now.rem_euclid(duration)
    } else {
        now
    }
}
fn calendar_start(kind: &str, now: i64) -> i64 {
    use chrono::{Datelike, TimeZone};
    let dt = chrono::Utc.timestamp_millis_opt(now).single().unwrap();
    match kind {
        "daily" => dt
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp_millis(),
        "weekly" => (dt.date_naive()
            - chrono::Duration::days(dt.weekday().num_days_from_monday() as i64))
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_utc()
        .timestamp_millis(),
        "monthly" => chrono::Utc
            .with_ymd_and_hms(dt.year(), dt.month(), 1, 0, 0, 0)
            .single()
            .unwrap()
            .timestamp_millis(),
        _ => now,
    }
}
fn calendar_end(kind: &str, start: i64) -> i64 {
    if kind == "daily" {
        start + 86_400_000
    } else if kind == "weekly" {
        start + 604_800_000
    } else {
        use chrono::{Datelike, TimeZone};
        let dt = chrono::Utc.timestamp_millis_opt(start).single().unwrap();
        let (y, m) = if dt.month() == 12 {
            (dt.year() + 1, 1)
        } else {
            (dt.year(), dt.month() + 1)
        };
        chrono::Utc
            .with_ymd_and_hms(y, m, 1, 0, 0, 0)
            .single()
            .unwrap()
            .timestamp_millis()
    }
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
            "key".into(),
            KeyConfig {
                secret: "secret".into(),
                quota_names: vec!["q1".into()],
                ..Default::default()
            },
        );
        AppState::new(Some("admin".into()), keys)
    }
    async fn post(body: &str) -> Response {
        router()
            .with_state(fixture())
            .oneshot(
                Request::post("/v0/management/quota/clear")
                    .header("x-admin-key", "admin")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_owned()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }
    #[tokio::test]
    async fn validates_and_clears() {
        assert_eq!(post("{}").await.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            post(r#"{"key":"missing","quota":"q1"}"#).await.status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            post(r#"{"key":"key","quota":"other"}"#).await.status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            post(r#"{"key":"key","quota":"q1"}"#).await.status(),
            StatusCode::OK
        );
        assert_eq!(post(r#"{"key":"anything"}"#).await.status(), StatusCode::OK);
    }
}
