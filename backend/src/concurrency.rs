//! Concurrency management: empty live snapshot plus PostgreSQL timeline.

use axum::{
    extract::{Query, State},
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
    Router::new().route(
        "/v0/management/concurrency",
        axum::routing::get(concurrency),
    )
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Params {
    mode: Option<String>,
    time_range: Option<String>,
    group_by: Option<String>,
    start_date: Option<String>,
    end_date: Option<String>,
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

async fn concurrency(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<Params>,
) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    if params.mode.as_deref().unwrap_or("live") == "live" {
        // Trefoil has no in-memory concurrency reservation tracker yet.
        return Json(serde_json::json!({"data":[]})).into_response();
    }
    let Some(pool) = &state.inner.database else {
        return Json(serde_json::json!({"data":[]})).into_response();
    };
    let now = chrono::Utc::now().timestamp_millis();
    let (start, end) = if params.time_range.as_deref() == Some("custom")
        && params.start_date.is_some()
        && params.end_date.is_some()
    {
        let start = parse_date_ms(params.start_date.as_deref().unwrap());
        let end = parse_date_ms(params.end_date.as_deref().unwrap());
        match (start, end) {
            (Ok(start), Ok(end)) => (start, end),
            _ => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"error":"Invalid date format"})),
                )
                    .into_response()
            }
        }
    } else {
        let duration = match params.time_range.as_deref().unwrap_or("hour") {
            "day" => 86_400_000,
            "week" => 604_800_000,
            "month" => 2_592_000_000,
            _ => 3_600_000,
        };
        (now - duration, now)
    };
    let duration = end - start;
    let mut bucket = if duration <= 1_800_000 {
        60_000
    } else if duration <= 86_400_000 {
        300_000
    } else if duration <= 604_800_000 {
        3_600_000
    } else {
        21_600_000
    };
    if (duration + bucket - 1) / bucket > 100 {
        bucket = (duration + 99) / 100;
    }
    let group_by = params.group_by.as_deref().unwrap_or("provider");
    let column = if group_by == "model" {
        "canonical_model_name"
    } else {
        "provider"
    };
    let sql=format!("SELECT (floor(start_time::double precision / $1) * $1)::bigint AS timestamp, {column} AS key, count(*)::bigint AS count FROM request_usage WHERE {column} IS NOT NULL AND start_time >= $2 AND start_time <= $3 GROUP BY {column}, timestamp ORDER BY timestamp");
    let rows = sqlx::query(&sql)
        .bind(bucket)
        .bind(start)
        .bind(end)
        .fetch_all(pool)
        .await;
    let Ok(rows) = rows else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error":"Failed to read concurrency timeline"})),
        )
            .into_response();
    };
    let data: Vec<_> = rows
        .into_iter()
        .map(|row| {
            let timestamp = row.try_get::<i64, _>("timestamp").unwrap_or_default();
            let key = row.try_get::<String, _>("key").unwrap_or_default();
            let count = row.try_get::<i64, _>("count").unwrap_or_default();
            if group_by == "model" {
                serde_json::json!({"timestamp":timestamp,"model":key,"count":count})
            } else {
                serde_json::json!({"timestamp":timestamp,"provider":key,"count":count})
            }
        })
        .collect();
    Json(serde_json::json!({"data":data})).into_response()
}

fn parse_date_ms(value: &str) -> Result<i64, ()> {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|value| value.timestamp_millis())
        .or_else(|_| {
            chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d").map(|date| {
                date.and_hms_opt(0, 0, 0)
                    .unwrap()
                    .and_utc()
                    .timestamp_millis()
            })
        })
        .map_err(|_| ())
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
    async fn live_mode_returns_empty_snapshot() {
        let response = router()
            .with_state(fixture())
            .oneshot(
                Request::get("/v0/management/concurrency")
                    .header("x-admin-key", "admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
    #[tokio::test]
    async fn rejects_non_admin() {
        for (key, status) in [
            (Some("secret"), StatusCode::FORBIDDEN),
            (None, StatusCode::UNAUTHORIZED),
        ] {
            let mut r = Request::get("/v0/management/concurrency");
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

    #[test]
    fn custom_date_parser_accepts_rfc3339_and_date_only() {
        assert_eq!(parse_date_ms("1970-01-01").unwrap(), 0);
        assert_eq!(parse_date_ms("1970-01-01T00:00:01Z").unwrap(), 1_000);
        assert!(parse_date_ms("not-a-date").is_err());
    }
}
