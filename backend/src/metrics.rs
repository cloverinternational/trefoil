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

pub fn compute_active_time_ms(windows: &[(i64, i64)], gap_ms: i64) -> i64 {
    let Some(&(start, duration)) = windows.first() else { return 0 };
    let mut session_start = start;
    let mut session_end = start.saturating_add(duration.max(0));
    let mut total = 0;
    for &(start, duration) in &windows[1..] {
        let end = start.saturating_add(duration.max(0));
        if start.saturating_sub(session_end) > gap_ms {
            total += session_end - session_start;
            session_start = start;
        }
        session_end = session_end.max(end);
    }
    total + session_end - session_start
}

fn json_charset(mut response: Response) -> Response {
    response.headers_mut().insert("content-type", "application/json; charset=utf-8".parse().unwrap());
    response
}

async fn metrics(State(state): State<AppState>, headers: HeaderMap) -> Response {
    match resolve_principal(
        &state,
        headers.get("x-admin-key").and_then(|v| v.to_str().ok()),
    ) {
        Some(Principal::Admin) => {}
        Some(Principal::Limited { .. }) => return json_charset(forbidden()),
        None => return json_charset(unauthorized()),
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
    let mut today_cost = 0f64;
    let mut today = (0i64, 0i64, 0i64, 0i64, 0i64, 0i64, 0i64, 0i64);
    let mut active_time = 0i64;

    if let Some(pool) = &state.inner.database {
        if let Ok(row) = sqlx::query("SELECT count(*)::bigint AS requests, count(*) FILTER (WHERE response_status IS NULL OR response_status != 'success')::bigint AS errors, coalesce(sum(tokens_input),0)::bigint AS input, coalesce(sum(tokens_output),0)::bigint AS output, coalesce(sum(tokens_reasoning),0)::bigint AS reasoning, coalesce(sum(tokens_cached),0)::bigint AS cached, coalesce(sum(tokens_cache_write),0)::bigint AS cache_write, coalesce(sum(cost_total),0)::double precision AS cost, coalesce(sum(duration_ms),0)::bigint AS duration, coalesce(avg(duration_ms),0)::double precision AS average FROM request_usage").fetch_one(pool).await {
            total = row.try_get("requests").unwrap_or(0); errors = row.try_get("errors").unwrap_or(0);
            input = row.try_get("input").unwrap_or(0); output = row.try_get("output").unwrap_or(0);
            cached = row.try_get("cached").unwrap_or(0); cache_write = row.try_get("cache_write").unwrap_or(0);
            duration = row.try_get("duration").unwrap_or(0); average = row.try_get("average").unwrap_or(0.0);
        }
        let midnight = chrono::Local::now()
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_local_timezone(chrono::Local)
            .earliest()
            .map(|midnight| midnight.timestamp_millis())
            .unwrap_or_else(|| chrono::Utc::now().timestamp_millis());
        if let Ok(row) = sqlx::query("SELECT count(*)::bigint AS requests, count(*) FILTER (WHERE response_status IS NULL OR response_status != 'success')::bigint AS errors, coalesce(sum(tokens_input),0)::bigint AS input, coalesce(sum(tokens_output),0)::bigint AS output, coalesce(sum(tokens_reasoning),0)::bigint AS reasoning, coalesce(sum(tokens_cached),0)::bigint AS cached, coalesce(sum(tokens_cache_write),0)::bigint AS cache_write, coalesce(sum(cost_total),0)::double precision AS cost FROM request_usage WHERE start_time >= $1").bind(midnight).fetch_one(pool).await {
            today = (row.try_get("requests").unwrap_or(0), row.try_get("errors").unwrap_or(0), row.try_get("input").unwrap_or(0), row.try_get("output").unwrap_or(0), row.try_get("reasoning").unwrap_or(0), row.try_get("cached").unwrap_or(0), row.try_get("cache_write").unwrap_or(0), row.try_get("duration").unwrap_or(0)); today_cost = row.try_get("cost").unwrap_or(0.0);
        }
        if let Ok(rows) = sqlx::query("SELECT start_time, coalesce(duration_ms,0)::bigint AS duration FROM request_usage WHERE duration_ms IS NOT NULL ORDER BY start_time").fetch_all(pool).await {
            let windows: Vec<_> = rows.into_iter().filter_map(|r| Some((r.try_get("start_time").ok()?, r.try_get("duration").ok()?))).collect();
            active_time = compute_active_time_ms(&windows, 15 * 60 * 1000);
        }
        body.push_str(&dimension_metrics(pool).await);
        body.push_str(&performance_metrics(pool).await);
    }
    body.push_str(&cooldown_metrics(&state));

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
    body.push_str(&block("plexus_active_time_ms", "gauge", "Total active time in milliseconds, computed using a session-based model (15-min gap threshold, same as Google Analytics). This represents the wall-clock time during which at least one request was in-flight."));
    body.push_str(&line("plexus_active_time_ms", &[], active_time));
    body.push_str(&block("plexus_duration_ms_today", "gauge", "Total inference duration in milliseconds for requests since local midnight."));
    body.push_str(&line("plexus_duration_ms_today", &[], today.7));
    body.push_str(&block(
        "plexus_cost_today_usd",
        "gauge",
        "Request cost in USD since local midnight.",
    ));
    body.push_str(&line("plexus_cost_today_usd", &[], today_cost));
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

async fn performance_metrics(pool: &sqlx::PgPool) -> String {
    let mut body = String::new();
    let Ok(rows) = sqlx::query("SELECT provider, coalesce(canonical_model_name, model) AS model, avg(time_to_first_token_ms)::double precision AS avg_ttft, min(time_to_first_token_ms)::double precision AS min_ttft, max(time_to_first_token_ms)::double precision AS max_ttft, avg(tokens_per_sec)::double precision AS avg_tps, min(tokens_per_sec)::double precision AS min_tps, max(tokens_per_sec)::double precision AS max_tps, count(*)::bigint AS samples, coalesce(sum(success_count),0)::bigint AS successes, coalesce(sum(failure_count),0)::bigint AS failures FROM provider_performance GROUP BY provider, coalesce(canonical_model_name, model)").fetch_all(pool).await else { return body };
    for (name, help) in [
        ("plexus_perf_avg_ttft_ms", "Average time to first token."),
        ("plexus_perf_min_ttft_ms", "Minimum time to first token."),
        ("plexus_perf_max_ttft_ms", "Maximum time to first token."),
        (
            "plexus_perf_avg_tokens_per_sec",
            "Average token throughput.",
        ),
        (
            "plexus_perf_min_tokens_per_sec",
            "Minimum token throughput.",
        ),
        (
            "plexus_perf_max_tokens_per_sec",
            "Maximum token throughput.",
        ),
        ("plexus_perf_sample_count", "Performance sample count."),
        ("plexus_perf_success_count", "Successful samples."),
        ("plexus_perf_failure_count", "Failed samples."),
    ] {
        body.push_str(&block(name, "gauge", help));
    }
    for row in rows {
        let provider: String = row.try_get("provider").unwrap_or_else(|_| "unknown".into());
        let model: String = row.try_get("model").unwrap_or_else(|_| "unknown".into());
        let labels = [("provider", provider.as_str()), ("model", model.as_str())];
        for (name, col) in [
            ("plexus_perf_avg_ttft_ms", "avg_ttft"),
            ("plexus_perf_min_ttft_ms", "min_ttft"),
            ("plexus_perf_max_ttft_ms", "max_ttft"),
            ("plexus_perf_avg_tokens_per_sec", "avg_tps"),
            ("plexus_perf_min_tokens_per_sec", "min_tps"),
            ("plexus_perf_max_tokens_per_sec", "max_tps"),
        ] {
            body.push_str(&line(
                name,
                &labels,
                row.try_get::<f64, _>(col).unwrap_or(0.0),
            ));
        }
        for (name, col) in [
            ("plexus_perf_sample_count", "samples"),
            ("plexus_perf_success_count", "successes"),
            ("plexus_perf_failure_count", "failures"),
        ] {
            body.push_str(&line(
                name,
                &labels,
                row.try_get::<i64, _>(col).unwrap_or(0),
            ));
        }
    }
    body
}

fn cooldown_metrics(state: &AppState) -> String {
    let now = chrono::Utc::now().timestamp_millis();
    let mut body = String::new();
    let providers = state.inner.providers.read().unwrap();
    let entries: Vec<_> = state
        .inner
        .cooldowns
        .read()
        .unwrap()
        .iter()
        .filter(|((provider, _), entry)| {
            entry.expiry_ms > now && !providers.get(provider).is_some_and(|p| p.disable_cooldown)
        })
        .map(|((provider, model), entry)| {
            (
                provider.clone(),
                model.clone(),
                entry.expiry_ms - now,
                entry.consecutive_failures,
            )
        })
        .collect();
    for (name, help) in [
        ("plexus_cooldown_active", "Whether a cooldown is active."),
        (
            "plexus_cooldown_time_remaining_ms",
            "Cooldown milliseconds remaining.",
        ),
        (
            "plexus_cooldown_consecutive_failures",
            "Consecutive cooldown failures.",
        ),
    ] {
        body.push_str(&block(name, "gauge", help));
    }
    for (provider, model, remaining, failures) in entries {
        let labels = [("provider", provider.as_str()), ("model", model.as_str())];
        body.push_str(&line("plexus_cooldown_active", &labels, 1));
        body.push_str(&line(
            "plexus_cooldown_time_remaining_ms",
            &labels,
            remaining,
        ));
        body.push_str(&line(
            "plexus_cooldown_consecutive_failures",
            &labels,
            failures,
        ));
    }
    body
}

async fn dimension_metrics(pool: &sqlx::PgPool) -> String {
    let mut body = String::new();
    let Ok(rows) = sqlx::query("SELECT coalesce(provider,'unknown') AS provider, count(*)::bigint AS requests, count(*) FILTER (WHERE response_status IS NULL OR response_status != 'success')::bigint AS errors, coalesce(sum(tokens_input),0)::bigint + coalesce(sum(tokens_output),0)::bigint + coalesce(sum(tokens_cached),0)::bigint + coalesce(sum(tokens_cache_write),0)::bigint AS tokens, coalesce(sum(cost_total),0)::double precision AS cost, coalesce(avg(duration_ms),0)::double precision AS avg_latency, coalesce(avg(ttft_ms),0)::double precision AS avg_ttft, coalesce(avg(tokens_per_sec),0)::double precision AS avg_tps, coalesce(sum(duration_ms),0)::bigint AS duration FROM request_usage GROUP BY provider ORDER BY provider").fetch_all(pool).await else { return body };
    body.push_str(&block(
        "plexus_provider_requests_total",
        "counter",
        "Total requests routed to each provider.",
    ));
    body.push_str(&block(
        "plexus_provider_errors_total", "counter", "Total non-success responses from each provider."));
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
    for (name, kind, help) in [("plexus_provider_avg_latency_ms", "gauge", "All-time average end-to-end latency in milliseconds per provider."), ("plexus_provider_avg_ttft_ms", "gauge", "All-time average time-to-first-token in milliseconds per provider."), ("plexus_provider_avg_tokens_per_sec", "gauge", "All-time average token throughput in tokens/second per provider."), ("plexus_provider_duration_ms_total", "counter", "Total inference duration in milliseconds per provider.")] { body.push_str(&block(name, kind, help)); }
    for row in rows {
        let provider: String = row.try_get("provider").unwrap_or_else(|_| "unknown".into());
        let labels = [("provider", provider.as_str())];
        body.push_str(&line(
            "plexus_provider_requests_total",
            &labels,
            row.try_get::<i64, _>("requests").unwrap_or(0),
        ));
        body.push_str(&line(
            "plexus_provider_errors_total", &labels, row.try_get::<i64, _>("errors").unwrap_or(0)));
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
        for (name, col) in [("plexus_provider_avg_latency_ms", "avg_latency"), ("plexus_provider_avg_ttft_ms", "avg_ttft"), ("plexus_provider_avg_tokens_per_sec", "avg_tps")] { body.push_str(&line(name, &labels, row.try_get::<f64, _>(col).unwrap_or(0.0))); }
        body.push_str(&line("plexus_provider_duration_ms_total", &labels, row.try_get::<i64, _>("duration").unwrap_or(0)));
    }
    body
}

#[cfg(test)]
mod tests {
    use super::{compute_active_time_ms, escape};

    #[test]
    fn prometheus_label_escaping_is_spec_compliant() {
        assert_eq!(escape("a\\b\"c\nd"), "a\\\\b\\\"c\\nd");
    }

    #[test]
    fn active_time_coalesces_overlaps_and_splits_sessions() {
        assert_eq!(compute_active_time_ms(&[(0, 100), (50, 100), (1_000, 10)], 100), 160);
        assert_eq!(compute_active_time_ms(&[], 100), 0);
    }
}

#[cfg(test)]
mod runtime_tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    #[tokio::test]
    async fn metrics_exposes_live_cooldowns_and_excludes_expired_or_disabled() {
        let state = AppState::new(Some("admin".into()), Default::default());
        state.inner.providers.write().unwrap().insert(
            "disabled".into(),
            crate::state::ProviderConfig {
                disable_cooldown: true,
                ..Default::default()
            },
        );
        let now = chrono::Utc::now().timestamp_millis();
        for (provider, expiry_ms) in [
            ("active", now + 60_000),
            ("expired", now - 1),
            ("disabled", now + 60_000),
        ] {
            state.inner.cooldowns.write().unwrap().insert(
                (provider.into(), "model".into()),
                crate::state::CooldownEntry {
                    expiry_ms,
                    consecutive_failures: 3,
                    last_error: None,
                },
            );
        }
        let response = router()
            .with_state(state)
            .oneshot(
                Request::builder()
                    .uri("/v0/management/metrics")
                    .header("x-admin-key", "admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/plain"));
        let text = String::from_utf8(
            response
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .to_vec(),
        )
        .unwrap();
        assert!(text.contains("plexus_cooldown_active{provider=\"active\",model=\"model\"} 1"));
        assert!(text.contains(
            "plexus_cooldown_consecutive_failures{provider=\"active\",model=\"model\"} 3"
        ));
        assert!(!text.contains("provider=\"expired\"") && !text.contains("provider=\"disabled\""));
        assert!(text.contains("plexus_cost_today_usd 0"));
    }
}
