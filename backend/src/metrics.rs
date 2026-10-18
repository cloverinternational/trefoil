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
    let Some(&(start, duration)) = windows.first() else {
        return 0;
    };
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
    response.headers_mut().insert(
        "content-type",
        "application/json; charset=utf-8".parse().unwrap(),
    );
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
    let now_ms = chrono::Utc::now().timestamp_millis();
    let owner = std::sync::Arc::as_ptr(&state.inner) as usize;
    if let Some((cached_owner, expires, body)) = metrics_cache().lock().unwrap().clone() {
        if cached_owner == owner && now_ms < expires { return exposition_response(body, "HIT"); }
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
        if let Ok(row) = sqlx::query("SELECT count(*)::bigint AS requests, count(*) FILTER (WHERE response_status != 'success')::bigint AS errors, coalesce(sum(tokens_input),0)::bigint AS input, coalesce(sum(tokens_output),0)::bigint AS output, coalesce(sum(tokens_reasoning),0)::bigint AS reasoning, coalesce(sum(tokens_cached),0)::bigint AS cached, coalesce(sum(tokens_cache_write),0)::bigint AS cache_write, coalesce(sum(cost_total),0)::double precision AS cost, coalesce(sum(duration_ms),0)::bigint AS duration, coalesce(avg(duration_ms),0)::double precision AS average FROM request_usage").fetch_one(pool).await {
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
        if let Ok(row) = sqlx::query("SELECT count(*)::bigint AS requests, count(*) FILTER (WHERE response_status != 'success')::bigint AS errors, coalesce(sum(tokens_input),0)::bigint AS input, coalesce(sum(tokens_output),0)::bigint AS output, coalesce(sum(tokens_reasoning),0)::bigint AS reasoning, coalesce(sum(tokens_cached),0)::bigint AS cached, coalesce(sum(tokens_cache_write),0)::bigint AS cache_write, coalesce(sum(cost_total),0)::double precision AS cost, coalesce(sum(duration_ms),0)::bigint AS duration FROM request_usage WHERE start_time >= $1").bind(midnight).fetch_one(pool).await {
            today = (row.try_get("requests").unwrap_or(0), row.try_get("errors").unwrap_or(0), row.try_get("input").unwrap_or(0), row.try_get("output").unwrap_or(0), row.try_get("reasoning").unwrap_or(0), row.try_get("cached").unwrap_or(0), row.try_get("cache_write").unwrap_or(0), row.try_get("duration").unwrap_or(0)); today_cost = row.try_get("cost").unwrap_or(0.0);
        }
        if let Ok(rows) = sqlx::query("SELECT start_time, coalesce(duration_ms,0)::bigint AS duration FROM request_usage ORDER BY start_time").fetch_all(pool).await {
            let windows: Vec<_> = rows.into_iter().filter_map(|r| Some((r.try_get("start_time").ok()?, r.try_get("duration").ok()?))).collect();
            active_time = compute_active_time_ms(&windows, 15 * 60 * 1000);
        }
        body.push_str(&performance_metrics(&state).await);
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
    body.push_str(&block(
        "plexus_duration_ms_today",
        "gauge",
        "Total inference duration in milliseconds for requests since local midnight.",
    ));
    body.push_str(&line("plexus_duration_ms_today", &[], today.7));
    body.push_str(&block(
        "plexus_cost_today_usd",
        "gauge",
        "Request cost in USD since local midnight.",
    ));
    body.push_str(&line("plexus_cost_today_usd", &[], today_cost));
    if let Some(pool) = &state.inner.database {
        body.push_str(&dimension_metrics(pool).await);
    }
    let targets = crate::concurrency::snapshot(&state);
    let mut providers = std::collections::BTreeMap::<String, usize>::new();
    for ((provider, model), count) in targets {
        *providers.entry(provider.clone()).or_default() += count;
        body.push_str(&line(
            "plexus_concurrency_active_by_target",
            &[("provider", provider.as_str()), ("model", model.as_str())],
            count,
        ));
    }
    for (provider, count) in providers {
        body.push_str(&line(
            "plexus_concurrency_active",
            &[("provider", provider.as_str())],
            count,
        ));
    }
    body.push('\n');
    let rendered = render_exposition(&body);
    *metrics_cache().lock().unwrap() = Some((owner, chrono::Utc::now().timestamp_millis() + metrics_cache_ttl_ms(), rendered.clone()));
    exposition_response(rendered, "MISS")
}

/// Scrape cache (`PLEXUS_METRICS_CACHE_TTL_MS`, default 15s), keyed by app state.
type MetricsCache = std::sync::Mutex<Option<(usize, i64, String)>>;
fn metrics_cache() -> &'static MetricsCache {
    static CACHE: std::sync::OnceLock<MetricsCache> = std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

fn metrics_cache_ttl_ms() -> i64 {
    std::env::var("PLEXUS_METRICS_CACHE_TTL_MS").ok().and_then(|v| crate::usage::parse_int(&v)).filter(|n| *n > 0).unwrap_or(15_000)
}

fn exposition_response(body: String, cache: &'static str) -> Response {
    (
        StatusCode::OK,
        [
            (axum::http::header::CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8"),
            (axum::http::HeaderName::from_static("x-metrics-cache"), cache),
        ],
        body,
    )
        .into_response()
}

/// Provider performance gauges from `usageStorage.getProviderPerformance()`,
/// merged per (provider, model) label pair with sample-weighted averages.
async fn performance_metrics(state: &AppState) -> String {
    let mut body = String::new();
    let Some(storage) = crate::usage::usage_storage(state) else { return body };
    struct Perf { provider: String, model: String, avg_ttft: f64, min_ttft: f64, max_ttft: f64, avg_tps: f64, min_tps: f64, max_tps: f64, samples: f64, successes: f64, failures: f64 }
    let mut rows: Vec<Perf> = Vec::new();
    for r in storage.get_provider_performance(None, None).await {
        let provider = r.provider.clone().unwrap_or_else(|| "unknown".into());
        let model = if r.model.is_empty() { "unknown".to_string() } else { r.model.clone() };
        if let Some(e) = rows.iter_mut().find(|e| e.provider == provider && e.model == model) {
            let total = e.samples + r.sample_count;
            let (w1, w2) = if total > 0.0 { (e.samples / total, r.sample_count / total) } else { (0.5, 0.5) };
            e.avg_ttft = e.avg_ttft * w1 + r.avg_ttft_ms * w2;
            e.min_ttft = e.min_ttft.min(r.min_ttft_ms);
            e.max_ttft = e.max_ttft.max(r.max_ttft_ms);
            e.avg_tps = e.avg_tps * w1 + r.avg_tokens_per_sec * w2;
            e.min_tps = e.min_tps.min(r.min_tokens_per_sec);
            e.max_tps = e.max_tps.max(r.max_tokens_per_sec);
            e.samples = total;
            e.successes += r.success_count;
            e.failures += r.failure_count;
        } else {
            rows.push(Perf { provider, model, avg_ttft: r.avg_ttft_ms, min_ttft: r.min_ttft_ms, max_ttft: r.max_ttft_ms, avg_tps: r.avg_tokens_per_sec, min_tps: r.min_tokens_per_sec, max_tps: r.max_tokens_per_sec, samples: r.sample_count, successes: r.success_count, failures: r.failure_count });
        }
    }
    for (name, help) in [
        ("plexus_perf_avg_ttft_ms", "Average time to first token."),
        ("plexus_perf_min_ttft_ms", "Minimum time to first token."),
        ("plexus_perf_max_ttft_ms", "Maximum time to first token."),
        ("plexus_perf_avg_tokens_per_sec", "Average token throughput."),
        ("plexus_perf_min_tokens_per_sec", "Minimum token throughput."),
        ("plexus_perf_max_tokens_per_sec", "Maximum token throughput."),
        ("plexus_perf_sample_count", "Performance sample count."),
        ("plexus_perf_success_count", "Successful samples."),
        ("plexus_perf_failure_count", "Failed samples."),
    ] {
        body.push_str(&block(name, "gauge", help));
    }
    for r in rows {
        let labels = [("provider", r.provider.as_str()), ("model", r.model.as_str())];
        for (name, value) in [
            ("plexus_perf_avg_ttft_ms", r.avg_ttft), ("plexus_perf_min_ttft_ms", r.min_ttft), ("plexus_perf_max_ttft_ms", r.max_ttft),
            ("plexus_perf_avg_tokens_per_sec", r.avg_tps), ("plexus_perf_min_tokens_per_sec", r.min_tps), ("plexus_perf_max_tokens_per_sec", r.max_tps),
            ("plexus_perf_sample_count", r.samples), ("plexus_perf_success_count", r.successes), ("plexus_perf_failure_count", r.failures),
        ] {
            body.push_str(&line(name, &labels, crate::engine::js::js_number_text(value)));
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
    let Ok(rows) = sqlx::query("SELECT coalesce(provider,'unknown') AS provider, count(*)::bigint AS requests, count(*) FILTER (WHERE response_status != 'success')::bigint AS errors, coalesce(sum(tokens_input),0)::bigint + coalesce(sum(tokens_output),0)::bigint + coalesce(sum(tokens_cached),0)::bigint + coalesce(sum(tokens_cache_write),0)::bigint AS tokens, coalesce(sum(cost_total),0)::double precision AS cost, coalesce(avg(duration_ms),0)::double precision AS avg_latency, coalesce(avg(ttft_ms),0)::double precision AS avg_ttft, coalesce(avg(tokens_per_sec),0)::double precision AS avg_tps, coalesce(sum(duration_ms),0)::bigint AS duration FROM request_usage WHERE provider IS NOT NULL GROUP BY provider ORDER BY provider").fetch_all(pool).await else { return body };
    body.push_str(&block(
        "plexus_provider_requests_total",
        "counter",
        "Total requests routed to each provider.",
    ));
    body.push_str(&block(
        "plexus_provider_errors_total",
        "counter",
        "Total non-success responses from each provider.",
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
    for (name, kind, help) in [
        (
            "plexus_provider_avg_latency_ms",
            "gauge",
            "All-time average end-to-end latency in milliseconds per provider.",
        ),
        (
            "plexus_provider_avg_ttft_ms",
            "gauge",
            "All-time average time-to-first-token in milliseconds per provider.",
        ),
        (
            "plexus_provider_avg_tokens_per_sec",
            "gauge",
            "All-time average token throughput in tokens/second per provider.",
        ),
        (
            "plexus_provider_duration_ms_total",
            "counter",
            "Total inference duration in milliseconds per provider.",
        ),
    ] {
        body.push_str(&block(name, kind, help));
    }
    for row in rows {
        let provider: String = row.try_get("provider").unwrap_or_else(|_| "unknown".into());
        let labels = [("provider", provider.as_str())];
        body.push_str(&line(
            "plexus_provider_requests_total",
            &labels,
            row.try_get::<i64, _>("requests").unwrap_or(0),
        ));
        body.push_str(&line(
            "plexus_provider_errors_total",
            &labels,
            row.try_get::<i64, _>("errors").unwrap_or(0),
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
        for (name, col) in [
            ("plexus_provider_avg_latency_ms", "avg_latency"),
            ("plexus_provider_avg_ttft_ms", "avg_ttft"),
            ("plexus_provider_avg_tokens_per_sec", "avg_tps"),
        ] {
            body.push_str(&line(
                name,
                &labels,
                row.try_get::<f64, _>(col).unwrap_or(0.0),
            ));
        }
        body.push_str(&line(
            "plexus_provider_duration_ms_total",
            &labels,
            row.try_get::<i64, _>("duration").unwrap_or(0),
        ));
    }
    let Ok(rows) = sqlx::query("SELECT coalesce(incoming_model_alias,'unknown') AS model_alias, count(*)::bigint AS requests, coalesce(sum(tokens_input),0)::bigint + coalesce(sum(tokens_output),0)::bigint + coalesce(sum(tokens_cached),0)::bigint + coalesce(sum(tokens_cache_write),0)::bigint AS tokens, coalesce(sum(duration_ms),0)::bigint AS duration FROM request_usage WHERE incoming_model_alias IS NOT NULL GROUP BY incoming_model_alias ORDER BY incoming_model_alias").fetch_all(pool).await else { return body };
    for (name, kind, help) in [
        (
            "plexus_model_alias_requests_total",
            "counter",
            "Total requests received per incoming model alias (the model name the client sent).",
        ),
        (
            "plexus_model_alias_tokens_total",
            "counter",
            "Total tokens (input + output + cached + cache_write) per incoming model alias.",
        ),
        (
            "plexus_model_alias_duration_ms_total",
            "counter",
            "Total inference duration in milliseconds per incoming model alias.",
        ),
    ] {
        body.push_str(&block(name, kind, help));
    }
    for row in rows {
        let model: String = row
            .try_get("model_alias")
            .unwrap_or_else(|_| "unknown".into());
        let l = [("model_alias", model.as_str())];
        body.push_str(&line(
            "plexus_model_alias_requests_total",
            &l,
            row.try_get::<i64, _>("requests").unwrap_or(0),
        ));
        body.push_str(&line(
            "plexus_model_alias_tokens_total",
            &l,
            row.try_get::<i64, _>("tokens").unwrap_or(0),
        ));
        body.push_str(&line(
            "plexus_model_alias_duration_ms_total",
            &l,
            row.try_get::<i64, _>("duration").unwrap_or(0),
        ));
    }
    let Ok(rows) = sqlx::query("SELECT coalesce(api_key,'unknown') AS api_key, count(*)::bigint AS requests, coalesce(sum(tokens_input),0)::bigint + coalesce(sum(tokens_output),0)::bigint + coalesce(sum(tokens_cached),0)::bigint + coalesce(sum(tokens_cache_write),0)::bigint AS tokens FROM request_usage WHERE api_key IS NOT NULL GROUP BY api_key ORDER BY api_key").fetch_all(pool).await else { return body };
    for (name, help) in [
        (
            "plexus_api_key_requests_total",
            "Total requests per API key (hashed or named key identifier).",
        ),
        ("plexus_api_key_tokens_total", "Total tokens per API key."),
    ] {
        body.push_str(&block(name, "counter", help));
    }
    for row in rows {
        let key: String = row.try_get("api_key").unwrap_or_else(|_| "unknown".into());
        let l = [("api_key", key.as_str())];
        body.push_str(&line(
            "plexus_api_key_requests_total",
            &l,
            row.try_get::<i64, _>("requests").unwrap_or(0),
        ));
        body.push_str(&line(
            "plexus_api_key_tokens_total",
            &l,
            row.try_get::<i64, _>("tokens").unwrap_or(0),
        ));
    }
    let Ok(rows) = sqlx::query("SELECT coalesce(api_key,'unknown') AS api_key, coalesce(attribution,'') AS attribution, count(*)::bigint AS requests, coalesce(sum(tokens_input),0)::bigint + coalesce(sum(tokens_output),0)::bigint + coalesce(sum(tokens_cached),0)::bigint + coalesce(sum(tokens_cache_write),0)::bigint AS tokens, coalesce(sum(cost_total),0)::double precision AS cost, count(*) FILTER (WHERE response_status != 'success')::bigint AS errors FROM request_usage WHERE api_key IS NOT NULL GROUP BY api_key, attribution ORDER BY api_key, attribution").fetch_all(pool).await else { return body };
    for (name, help) in [("plexus_api_key_attribution_requests_total", "Total requests per API key and attribution suffix (the part after the colon in the key, e.g. \"mysecret:copilot\" → attribution=\"copilot\")."), ("plexus_api_key_attribution_tokens_total", "Total tokens per API key and attribution suffix."), ("plexus_api_key_attribution_cost_usd_total", "Cumulative cost in USD per API key and attribution suffix."), ("plexus_api_key_attribution_errors_total", "Total non-success responses per API key and attribution suffix.")] { body.push_str(&block(name,"counter",help)); }
    for row in rows {
        let key: String = row.try_get("api_key").unwrap_or_else(|_| "unknown".into());
        let attr: String = row.try_get("attribution").unwrap_or_default();
        let l = [("api_key", key.as_str()), ("attribution", attr.as_str())];
        body.push_str(&line(
            "plexus_api_key_attribution_requests_total",
            &l,
            row.try_get::<i64, _>("requests").unwrap_or(0),
        ));
        body.push_str(&line(
            "plexus_api_key_attribution_tokens_total",
            &l,
            row.try_get::<i64, _>("tokens").unwrap_or(0),
        ));
        body.push_str(&line(
            "plexus_api_key_attribution_cost_usd_total",
            &l,
            row.try_get::<f64, _>("cost").unwrap_or(0.0),
        ));
        body.push_str(&line(
            "plexus_api_key_attribution_errors_total",
            &l,
            row.try_get::<i64, _>("errors").unwrap_or(0),
        ));
    }
    for (name, help) in [
        (
            "plexus_in_flight_requests",
            "Number of requests currently in-flight (started but not yet completed), per provider.",
        ),
        (
            "plexus_in_flight_requests_by_model",
            "Number of requests currently in-flight, per canonical model name.",
        ),
    ] {
        body.push_str(&block(name, "gauge", help));
    }
    if let Ok(rows)=sqlx::query("SELECT coalesce(provider,'unknown') AS provider, count(*)::bigint AS count FROM request_usage WHERE duration_ms IS NULL AND start_time >= $1 GROUP BY provider ORDER BY provider").bind(chrono::Utc::now().timestamp_millis()-3_600_000).fetch_all(pool).await { for row in rows { let p:String=row.try_get("provider").unwrap_or_else(|_|"unknown".into()); body.push_str(&line("plexus_in_flight_requests",&[("provider",p.as_str())],row.try_get::<i64,_>("count").unwrap_or(0))); } }
    if let Ok(rows)=sqlx::query("SELECT coalesce(canonical_model_name,'unknown') AS model, count(*)::bigint AS count FROM request_usage WHERE duration_ms IS NULL AND start_time >= $1 GROUP BY canonical_model_name ORDER BY model").bind(chrono::Utc::now().timestamp_millis()-3_600_000).fetch_all(pool).await { for row in rows { let m:String=row.try_get("model").unwrap_or_else(|_|"unknown".into()); body.push_str(&line("plexus_in_flight_requests_by_model",&[("model",m.as_str())],row.try_get::<i64,_>("count").unwrap_or(0))); } }
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
        assert_eq!(
            compute_active_time_ms(&[(0, 100), (50, 100), (1_000, 10)], 100),
            160
        );
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

// Exposition metadata and order follow the pinned Plexus contract.
const METRIC_DEFINITIONS: &[(&str, &str, &str)] = &[
    ("plexus_requests_total","counter","Total number of requests processed by Plexus since the database was created."),
    ("plexus_errors_total","counter","Total number of non-success responses (errors) across all providers."),
    ("plexus_tokens_total","counter","Total tokens processed, broken down by token type (input, output, cached, cache_write)."),
    ("plexus_requests_today","gauge","Number of requests processed since local midnight."),
    ("plexus_errors_today","gauge","Number of non-success responses since local midnight."),
    ("plexus_cost_today_usd","gauge","Total cost in USD for requests processed since local midnight."),
    ("plexus_tokens_today","gauge","Tokens processed since local midnight, broken down by token type."),
    ("plexus_duration_ms_total","counter","Total inference duration in milliseconds across all requests since the database was created."),
    ("plexus_avg_duration_ms","gauge","All-time average end-to-end latency in milliseconds per request."),
    ("plexus_active_time_ms","gauge","Total active time in milliseconds, computed using a session-based model (15-min gap threshold, same as Google Analytics). This represents the wall-clock time during which at least one request was in-flight."),
    ("plexus_duration_ms_today","gauge","Total inference duration in milliseconds for requests since local midnight."),
    ("plexus_provider_requests_total","counter","Total requests routed to each provider."),
    ("plexus_provider_errors_total","counter","Total non-success responses from each provider."),
    ("plexus_provider_tokens_total","counter","Total tokens (input + output + cached + cache_write) sent to each provider."),
    ("plexus_provider_cost_usd_total","counter","Cumulative cost in USD for each provider."),
    ("plexus_provider_avg_latency_ms","gauge","All-time average end-to-end latency in milliseconds per provider."),
    ("plexus_provider_avg_ttft_ms","gauge","All-time average time-to-first-token in milliseconds per provider."),
    ("plexus_provider_avg_tokens_per_sec","gauge","All-time average token throughput in tokens/second per provider."),
    ("plexus_provider_duration_ms_total","counter","Total inference duration in milliseconds per provider."),
    ("plexus_model_alias_requests_total","counter","Total requests received per incoming model alias (the model name the client sent)."),
    ("plexus_model_alias_tokens_total","counter","Total tokens (input + output + cached + cache_write) per incoming model alias."),
    ("plexus_model_alias_duration_ms_total","counter","Total inference duration in milliseconds per incoming model alias."),
    ("plexus_api_key_requests_total","counter","Total requests per API key (hashed or named key identifier)."),
    ("plexus_api_key_tokens_total","counter","Total tokens per API key."),
    ("plexus_api_key_attribution_requests_total","counter","Total requests per API key and attribution suffix (the part after the colon in the key, e.g. \"mysecret:copilot\" → attribution=\"copilot\")."),
    ("plexus_api_key_attribution_tokens_total","counter","Total tokens per API key and attribution suffix."),
    ("plexus_api_key_attribution_cost_usd_total","counter","Cumulative cost in USD per API key and attribution suffix."),
    ("plexus_api_key_attribution_errors_total","counter","Total non-success responses per API key and attribution suffix."),
    ("plexus_in_flight_requests","gauge","Number of requests currently in-flight (started but not yet completed), per provider."),
    ("plexus_in_flight_requests_by_model","gauge","Number of requests currently in-flight, per canonical model name."),
    ("plexus_cooldown_active","gauge","1 if the provider/model combination is currently in a cooldown, 0 otherwise. Active cooldowns indicate recent consecutive failures."),
    ("plexus_cooldown_time_remaining_ms","gauge","Milliseconds remaining in the current cooldown period for this provider/model pair."),
    ("plexus_cooldown_consecutive_failures","gauge","Number of consecutive failures that triggered the current cooldown."),
    ("plexus_concurrency_active","gauge","Number of requests currently tracked by the in-memory ConcurrencyTracker per provider."),
    ("plexus_concurrency_active_by_target","gauge","Number of requests currently tracked by the in-memory ConcurrencyTracker per provider/model target."),
    ("plexus_perf_avg_ttft_ms","gauge","Average time-to-first-token in milliseconds per provider and target model, aggregated over all recorded samples."),
    ("plexus_perf_min_ttft_ms","gauge","Minimum time-to-first-token in milliseconds per provider and model."),
    ("plexus_perf_max_ttft_ms","gauge","Maximum time-to-first-token in milliseconds per provider and model."),
    ("plexus_perf_avg_tokens_per_sec","gauge","Average token throughput in tokens/second per provider and model."),
    ("plexus_perf_min_tokens_per_sec","gauge","Minimum token throughput in tokens/second per provider and model."),
    ("plexus_perf_max_tokens_per_sec","gauge","Maximum token throughput in tokens/second per provider and model."),
    ("plexus_perf_sample_count","gauge","Number of requests sampled in the performance aggregate for this provider and model."),
    ("plexus_perf_success_count","gauge","Number of successful requests in the performance aggregate for this provider and model."),
    ("plexus_perf_failure_count","gauge","Number of failed requests in the performance aggregate for this provider and model."),
];

fn render_exposition(samples: &str) -> String {
    let mut output = String::new();
    for (index, (name, kind, help)) in METRIC_DEFINITIONS.iter().enumerate() {
        if index > 0 {
            output.push('\n');
        }
        output.push_str(&block(name, kind, help));
        for sample in samples.lines().filter(|line| !line.starts_with('#')) {
            if sample
                .strip_prefix(name)
                .is_some_and(|rest| rest.starts_with('{') || rest.starts_with(' '))
            {
                output.push_str(sample);
                output.push('\n');
            }
        }
    }
    output
}

#[cfg(test)]
mod exposition_contract_tests {
    use super::*;
    #[test]
    fn rendering_groups_samples_in_reference_order_without_losing_values() {
        let rendered = render_exposition("plexus_provider_requests_total{provider=\"a\"} 3\nplexus_requests_total 7\nplexus_provider_requests_total{provider=\"b\"} 4\n");
        assert!(rendered.starts_with("# HELP plexus_requests_total Total number of requests processed by Plexus since the database was created.\n"));
        assert_eq!(rendered.matches("plexus_requests_total 7\n").count(), 1);
        assert!(rendered.contains("plexus_provider_requests_total{provider=\"a\"} 3\nplexus_provider_requests_total{provider=\"b\"} 4\n"));
        assert_eq!(
            rendered
                .lines()
                .filter(|line| line.starts_with("# TYPE "))
                .count(),
            44
        );
    }
}

#[cfg(test)]
mod concurrency_gauge_tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    #[tokio::test]
    async fn gauges_measure_active_permits_and_disappear_on_release() {
        let state = AppState::new(Some("admin".into()), Default::default());
        let first = crate::concurrency::acquire(&state, "provider", "model/a");
        let second = crate::concurrency::acquire(&state, "provider", "model/b");
        let response = router()
            .with_state(state.clone())
            .oneshot(
                Request::get("/v0/management/metrics")
                    .header("x-admin-key", "admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
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
        assert!(text.contains("plexus_concurrency_active{provider=\"provider\"} 2\n"));
        assert!(text.contains(
            "plexus_concurrency_active_by_target{provider=\"provider\",model=\"model/a\"} 1\n"
        ));
        drop(first);
        drop(second);
        assert!(crate::concurrency::snapshot(&state).is_empty());
    }
}
