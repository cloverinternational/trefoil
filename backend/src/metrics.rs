//! Prometheus text exposition for the management API.
//!
//! This intentionally derives metrics from the durable usage table rather than
//! maintaining a second counter store.  Consequently a restart does not reset
//! the counters and clearing usage has the same meaning everywhere.

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Router,
};
use sqlx::Row;

use crate::{
    auth::{resolve_principal, Principal},
    AppState,
};

pub fn router() -> Router<AppState> {
    Router::new().route("/v0/management/metrics", axum::routing::get(metrics))
}

fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

fn line(name: &str, labels: &[(&str, &str)], value: impl std::fmt::Display) -> String {
    let labels = if labels.is_empty() {
        String::new()
    } else {
        format!(
            "{{{}}}",
            labels
                .iter()
                .map(|(k, v)| format!(r#"{}="{}""#, k, escape(v)))
                .collect::<Vec<_>>()
                .join(",")
        )
    };
    format!("{name}{labels} {value}\n")
}

fn block(name: &str, kind: &str, help: &str) -> String {
    format!("# HELP {name} {help}\n# TYPE {name} {kind}\n")
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        axum::Json(serde_json::json!({
            "error": { "message": "Unauthorized", "type": "auth_error", "code": 401 }
        })),
    )
        .into_response()
}

fn forbidden() -> Response {
    (
        StatusCode::FORBIDDEN,
        axum::Json(serde_json::json!({
            "error": { "message": "Admin privileges required", "type": "forbidden", "code": 403 }
        })),
    )
        .into_response()
}

async fn metrics(State(state): State<AppState>, headers: HeaderMap) -> Response {
    match resolve_principal(
        &state,
        headers.get("x-admin-key").and_then(|v| v.to_str().ok()),
    ) {
        Some(Principal::Admin) => {}
        Some(Principal::Limited { .. }) => return forbidden(),
        None => return unauthorized(),
    }

    let mut body = String::new();
    let mut total = 0i64;
    let mut errors = 0i64;
    let mut input = 0i64;
    let mut output = 0i64;
    let mut cached = 0i64;
    let mut cache_write = 0i64;
    let mut duration = 0i64;
    let mut average = 0f64;
    let mut today = (0i64, 0i64, 0i64, 0i64, 0i64, 0i64, 0i64);

    if let Some(pool) = &state.inner.database {
        if let Ok(row) = sqlx::query("SELECT count(*)::bigint AS requests, count(*) FILTER (WHERE response_status IS NULL OR response_status != 'success')::bigint AS errors, coalesce(sum(tokens_input),0)::bigint AS input, coalesce(sum(tokens_output),0)::bigint AS output, coalesce(sum(tokens_cached),0)::bigint AS cached, coalesce(sum(tokens_cache_write),0)::bigint AS cache_write, coalesce(sum(duration_ms),0)::bigint AS duration, coalesce(avg(duration_ms),0)::double precision AS average FROM request_usage").fetch_one(pool).await {
            total = row.try_get("requests").unwrap_or(0); errors = row.try_get("errors").unwrap_or(0);
            input = row.try_get("input").unwrap_or(0); output = row.try_get("output").unwrap_or(0);
            cached = row.try_get("cached").unwrap_or(0); cache_write = row.try_get("cache_write").unwrap_or(0);
            duration = row.try_get("duration").unwrap_or(0); average = row.try_get("average").unwrap_or(0.0);
        }
        let midnight = chrono::Local::now()
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp_millis();
        if let Ok(row) = sqlx::query("SELECT count(*)::bigint AS requests, count(*) FILTER (WHERE response_status IS NULL OR response_status != 'success')::bigint AS errors, coalesce(sum(tokens_input),0)::bigint AS input, coalesce(sum(tokens_output),0)::bigint AS output, coalesce(sum(tokens_reasoning),0)::bigint AS reasoning, coalesce(sum(tokens_cached),0)::bigint AS cached, coalesce(sum(tokens_cache_write),0)::bigint AS cache_write FROM request_usage WHERE start_time >= $1").bind(midnight).fetch_one(pool).await {
            today = (row.try_get("requests").unwrap_or(0), row.try_get("errors").unwrap_or(0), row.try_get("input").unwrap_or(0), row.try_get("output").unwrap_or(0), row.try_get("reasoning").unwrap_or(0), row.try_get("cached").unwrap_or(0), row.try_get("cache_write").unwrap_or(0));
        }
        body.push_str(&dimension_metrics(pool).await);
    }

    body.push_str(&block(
        "plexus_requests_total",
        "counter",
        "Total number of requests processed by Plexus.",
    ));
    body.push_str(&line("plexus_requests_total", &[], total));
    body.push_str(&block(
        "plexus_errors_total",
        "counter",
        "Total number of non-success responses.",
    ));
    body.push_str(&line("plexus_errors_total", &[], errors));
    body.push_str(&block(
        "plexus_tokens_total",
        "counter",
        "Total tokens processed by token type.",
    ));
    for (kind, value) in [
        ("input", input),
        ("output", output),
        ("cached", cached),
        ("cache_write", cache_write),
    ] {
        body.push_str(&line("plexus_tokens_total", &[("type", kind)], value));
    }
    body.push_str(&block(
        "plexus_requests_today",
        "gauge",
        "Requests processed since local midnight.",
    ));
    body.push_str(&line("plexus_requests_today", &[], today.0));
    body.push_str(&block(
        "plexus_errors_today",
        "gauge",
        "Non-success responses since local midnight.",
    ));
    body.push_str(&line("plexus_errors_today", &[], today.1));
    body.push_str(&block(
        "plexus_tokens_today",
        "gauge",
        "Tokens processed since local midnight.",
    ));
    for (kind, value) in [
        ("input", today.2),
        ("output", today.3),
        ("reasoning", today.4),
        ("cached", today.5),
        ("cache_write", today.6),
    ] {
        body.push_str(&line("plexus_tokens_today", &[("type", kind)], value));
    }
    body.push_str(&block(
        "plexus_duration_ms_total",
        "counter",
        "Total inference duration in milliseconds.",
    ));
    body.push_str(&line("plexus_duration_ms_total", &[], duration));
    body.push_str(&block(
        "plexus_avg_duration_ms",
        "gauge",
        "Average inference duration in milliseconds.",
    ));
    body.push_str(&line("plexus_avg_duration_ms", &[], average));
    body.push_str("\n");
    (
        StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        body,
    )
        .into_response()
}

async fn dimension_metrics(pool: &sqlx::PgPool) -> String {
    let mut body = String::new();
    let Ok(rows) = sqlx::query("SELECT coalesce(provider,'unknown') AS provider, count(*)::bigint AS requests, coalesce(sum(tokens_input),0)::bigint + coalesce(sum(tokens_output),0)::bigint + coalesce(sum(tokens_cached),0)::bigint + coalesce(sum(tokens_cache_write),0)::bigint AS tokens, coalesce(sum(cost_total),0)::double precision AS cost FROM request_usage GROUP BY provider ORDER BY provider").fetch_all(pool).await else { return body };
    body.push_str(&block(
        "plexus_provider_requests_total",
        "counter",
        "Total requests routed to each provider.",
    ));
    body.push_str(&block(
        "plexus_provider_tokens_total",
        "counter",
        "Total tokens sent to each provider.",
    ));
    body.push_str(&block(
        "plexus_provider_cost_usd_total",
        "counter",
        "Cumulative cost in USD for each provider.",
    ));
    for row in rows {
        let provider: String = row.try_get("provider").unwrap_or_else(|_| "unknown".into());
        let labels = [("provider", provider.as_str())];
        body.push_str(&line(
            "plexus_provider_requests_total",
            &labels,
            row.try_get::<i64, _>("requests").unwrap_or(0),
        ));
        body.push_str(&line(
            "plexus_provider_tokens_total",
            &labels,
            row.try_get::<i64, _>("tokens").unwrap_or(0),
        ));
        body.push_str(&line(
            "plexus_provider_cost_usd_total",
            &labels,
            row.try_get::<f64, _>("cost").unwrap_or(0.0),
        ));
    }
    body
}

#[cfg(test)]
mod tests {
    use super::escape;

    #[test]
    fn prometheus_label_escaping_is_spec_compliant() {
        assert_eq!(escape("a\\b\"c\nd"), "a\\\\b\\\"c\\nd");
    }
}
