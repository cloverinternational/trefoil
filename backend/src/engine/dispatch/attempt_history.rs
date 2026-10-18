//! `services/dispatch/dispatcher-types.ts` + `attempt-history.ts`.
//!
//! The failure-reason / summary formatters the TS injects are always the
//! dispatcher's own (`formatFailureReason`, `compactProviderErrorSummary`),
//! so they are called directly here.
use super::errors::{compact_str, error_status_code, format_failure_reason};
use super::util::del;
use crate::engine::js::*;
use crate::engine::routing::RouteResult;
use serde_json::{json, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptStatus { Success, Failed, Skipped }

impl AttemptStatus {
    pub fn as_str(self) -> &'static str {
        match self { Self::Success => "success", Self::Failed => "failed", Self::Skipped => "skipped" }
    }
}

/// `RetryAttemptRecord`.
#[derive(Debug, Clone, PartialEq)]
pub struct RetryAttemptRecord {
    pub index: usize,
    pub provider: String,
    pub model: String,
    /// Post-adapter model actually dispatched (`providerPayload.model`).
    pub upstream_model: Option<String>,
    pub api_type: Option<String>,
    pub status: AttemptStatus,
    pub reason: String,
    pub status_code: Option<f64>,
    pub retryable: Option<bool>,
    pub provider_response_headers: Option<Value>,
}

impl RetryAttemptRecord {
    /// JSON in the TS literal key order (`undefined` members dropped).
    pub fn to_json(&self) -> Value {
        obj(vec![
            ("index", Some(json!(self.index))),
            ("provider", Some(json!(self.provider))),
            ("model", Some(json!(self.model))),
            ("upstreamModel", self.upstream_model.as_ref().map(|s| json!(s))),
            ("apiType", self.api_type.as_ref().map(|s| json!(s))),
            ("status", Some(json!(self.status.as_str()))),
            ("reason", Some(json!(self.reason))),
            ("statusCode", self.status_code.map(num)),
            ("retryable", self.retryable.map(Value::Bool)),
            ("providerResponseHeaders", self.provider_response_headers.clone()),
        ])
    }
}

/// `JSON.stringify(retryHistory)`.
pub fn retry_history_json(history: &[RetryAttemptRecord]) -> String {
    stringify(&Value::Array(history.iter().map(RetryAttemptRecord::to_json).collect()))
}

fn record(history: &[RetryAttemptRecord], route: &RouteResult, status: AttemptStatus, reason: String) -> RetryAttemptRecord {
    RetryAttemptRecord {
        index: history.len() + 1, provider: route.provider.clone(), model: route.model.clone(), upstream_model: None,
        api_type: None, status, reason, status_code: None, retryable: None, provider_response_headers: None,
    }
}

/// `appendSkippedAttempt`.
pub fn append_skipped_attempt(history: &mut Vec<RetryAttemptRecord>, route: &RouteResult, reason: &str, api_type: Option<&str>) {
    let mut r = record(history, route, AttemptStatus::Skipped, reason.to_owned());
    r.api_type = api_type.map(str::to_owned);
    r.retryable = Some(false);
    history.push(r);
}

/// `appendSuccessAttempt`.
pub fn append_success_attempt(history: &mut Vec<RetryAttemptRecord>, route: &RouteResult, api_type: Option<&str>, upstream_model: Option<&str>) {
    let mut r = record(history, route, AttemptStatus::Success, "Request completed successfully".into());
    r.upstream_model = upstream_model.map(str::to_owned);
    r.api_type = api_type.map(str::to_owned);
    r.retryable = Some(false);
    history.push(r);
}

/// `appendFailureAttempt` (with the dispatcher's `formatFailureReason`).
pub fn append_failure_attempt(
    history: &mut Vec<RetryAttemptRecord>,
    route: &RouteResult,
    error: &Value,
    api_type: Option<&str>,
    retryable: Option<bool>,
    upstream_model: Option<&str>,
) {
    let mut r = record(history, route, AttemptStatus::Failed, format_failure_reason(error, false));
    r.upstream_model = upstream_model.map(str::to_owned);
    r.api_type = api_type.map(str::to_owned);
    r.status_code = error_status_code(error).and_then(Value::as_f64);
    r.retryable = retryable;
    r.provider_response_headers = get(error, &["routingContext", "providerResponseHeaders"]).cloned();
    history.push(r);
}

/// `resolveUpstreamPricing` result.
#[derive(Debug, Clone, PartialEq)]
pub struct UpstreamPricing {
    pub pricing: Option<Value>,
    pub pricing_model: String,
    pub pricing_fallback: bool,
}

/// `resolveUpstreamPricing(finalRoute, upstreamModel)`.
pub fn resolve_upstream_pricing(route: &RouteResult, upstream_model: Option<&str>) -> UpstreamPricing {
    let route_pricing = || route.model_config.as_ref().and_then(|m| m.get("pricing")).cloned();
    let normalized = upstream_model.filter(|s| !s.is_empty()).unwrap_or(&route.model);
    if normalized == route.model {
        return UpstreamPricing { pricing: route_pricing(), pricing_model: route.model.clone(), pricing_fallback: false };
    }
    if let Some(Value::Object(models)) = route.config.get("models") {
        if let Some(p) = models.get(normalized).and_then(|m| m.get("pricing")).filter(|p| truthy(Some(p))) {
            return UpstreamPricing { pricing: Some(p.clone()), pricing_model: normalized.to_owned(), pricing_fallback: false };
        }
    }
    UpstreamPricing { pricing: route_pricing(), pricing_model: route.model.clone(), pricing_fallback: true }
}

fn put(map: &mut Obj, key: &str, value: Option<Value>) {
    match value { Some(v) => { map.insert(key.into(), v); } None => { del(map, key); } }
}

/// `attachAttemptMetadata`: sets `response.plexus` and back-fills the
/// winning success entry's `upstreamModel`.
pub fn attach_attempt_metadata(
    response: &mut Value,
    attempted_providers: &[String],
    history: &mut [RetryAttemptRecord],
    route: &RouteResult,
    api_type: &str,
    upstream_model: Option<&str>,
) {
    let response_api_type = get(response, &["plexus", "apiType"]).cloned();
    let normalized = upstream_model.filter(|s| !s.is_empty());
    let resolved = resolve_upstream_pricing(route, normalized);
    if let Some(up) = normalized {
        if let Some(entry) = history.iter_mut().rev().find(|e| {
            e.status == AttemptStatus::Success && e.provider == route.provider && e.model == route.model
                && e.upstream_model.as_deref().is_none_or(str::is_empty)
        }) {
            entry.upstream_model = Some(up.to_owned());
        }
    }
    let mut plexus = response.get("plexus").and_then(Value::as_object).cloned().unwrap_or_default();
    put(&mut plexus, "attemptCount", Some(json!(attempted_providers.len())));
    put(&mut plexus, "finalAttemptProvider", Some(json!(route.provider)));
    put(&mut plexus, "finalAttemptModel", Some(json!(route.model)));
    put(&mut plexus, "upstreamModel", normalized.map(|s| json!(s)));
    put(&mut plexus, "allAttemptedProviders", Some(json!(stringify(&json!(attempted_providers)))));
    put(&mut plexus, "retryHistory", Some(json!(retry_history_json(history))));
    put(&mut plexus, "canonicalModel", route.canonical_model.as_ref().map(|s| json!(s)));
    put(&mut plexus, "provider", Some(json!(route.provider)));
    put(&mut plexus, "model", Some(json!(route.model)));
    put(&mut plexus, "apiType", Some(if truthy(response_api_type.as_ref()) { response_api_type.unwrap() } else { json!(api_type) }));
    put(&mut plexus, "pricing", resolved.pricing);
    put(&mut plexus, "pricingModel", Some(json!(resolved.pricing_model)));
    put(&mut plexus, "pricingFallback", Some(json!(resolved.pricing_fallback)));
    put(&mut plexus, "providerDiscount", route.config.get("discount").cloned());
    put(&mut plexus, "config", Some(obj(vec![("estimateTokens", route.config.get("estimateTokens").cloned())])));
    if let Some(map) = response.as_object_mut() { map.insert("plexus".into(), Value::Object(plexus)); }
}

/// `describeAttemptTag`.
fn describe_attempt_tag(entry: Option<&RetryAttemptRecord>) -> Option<String> {
    let entry = entry?;
    match entry.status {
        AttemptStatus::Skipped => Some("skipped".into()),
        AttemptStatus::Success => None,
        AttemptStatus::Failed => Some(match entry.status_code {
            Some(c) if c == 200.0 => "empty".into(),
            Some(c) => js_num_string(c),
            None => if entry.reason.to_lowercase().contains("stall") { "stall".into() } else { "network".into() },
        }),
    }
}

/// `formatAttemptedProvidersSummary`.
pub fn format_attempted_providers_summary(attempted: &[String], history: &[RetryAttemptRecord]) -> String {
    if attempted.is_empty() { return "none".into(); }
    let mut queues: Vec<(String, std::collections::VecDeque<&RetryAttemptRecord>)> = Vec::new();
    for entry in history.iter().filter(|e| e.status != AttemptStatus::Skipped) {
        let key = format!("{}/{}", entry.provider, entry.model);
        match queues.iter_mut().find(|(k, _)| *k == key) {
            Some((_, q)) => q.push_back(entry),
            None => queues.push((key, std::collections::VecDeque::from([entry]))),
        }
    }
    attempted.iter().map(|provider| {
        let next = queues.iter_mut().find(|(k, _)| k == provider).and_then(|(_, q)| q.pop_front());
        match describe_attempt_tag(next) { Some(tag) => format!("{provider} ({tag})"), None => provider.clone() }
    }).collect::<Vec<_>>().join(", ")
}

/// `buildAllTargetsFailedError` → error-like value (`message`, `cause`,
/// `routingContext`).
pub fn build_all_targets_failed_error(last_error: &Value, attempted: &[String], history: &[RetryAttemptRecord]) -> Value {
    let summary = format_attempted_providers_summary(attempted, history);
    let reason = format_failure_reason(last_error, false);
    let base = compact_str(&reason);
    let mut rc = last_error.get("routingContext").and_then(Value::as_object).cloned().unwrap_or_default();
    rc.insert("allAttemptedProviders".into(), json!(attempted));
    rc.insert("attemptCount".into(), json!(attempted.len()));
    rc.insert("retryHistory".into(), json!(retry_history_json(history)));
    let status = get(last_error, &["routingContext", "statusCode"]).filter(|v| truthy(Some(v))).cloned().unwrap_or(json!(500));
    rc.insert("statusCode".into(), status);
    json!({
        "message": format!("All targets failed: {summary}. Last error: {base}"),
        "cause": last_error,
        "routingContext": rc,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(p: &str, m: &str) -> RouteResult {
        RouteResult { provider: p.into(), model: m.into(), config: json!({}), model_config: None, incoming_model_alias: None, canonical_model: None }
    }

    #[test]
    fn history_records_and_json_order() {
        let mut h = Vec::new();
        append_skipped_attempt(&mut h, &route("a", "x"), "quota_exceeded:q", Some("chat"));
        let err = json!({"message": "boom", "routingContext": {"statusCode": 503, "providerResponse": "busy"}});
        append_failure_attempt(&mut h, &route("b", "y"), &err, Some("chat"), Some(true), None);
        append_success_attempt(&mut h, &route("c", "z"), Some("chat"), None);
        assert_eq!(
            h[1].to_json(),
            json!({"index": 2, "provider": "b", "model": "y", "apiType": "chat", "status": "failed", "reason": "busy", "statusCode": 503, "retryable": true})
        );
        assert_eq!(stringify(&h[0].to_json()), r#"{"index":1,"provider":"a","model":"x","apiType":"chat","status":"skipped","reason":"quota_exceeded:q","retryable":false}"#);
    }

    #[test]
    fn summary_tags() {
        let mut h = Vec::new();
        append_failure_attempt(&mut h, &route("a", "x"), &json!({"routingContext": {"statusCode": 500}, "message": "e"}), None, None, None);
        append_failure_attempt(&mut h, &route("b", "y"), &json!({"message": "Stream stall detected"}), None, None, None);
        append_failure_attempt(&mut h, &route("c", "z"), &json!({"message": "ECONNRESET"}), None, None, None);
        append_failure_attempt(&mut h, &route("d", "w"), &json!({"routingContext": {"statusCode": 200}, "message": "empty"}), None, None, None);
        let attempted: Vec<String> = ["a/x", "b/y", "c/z", "d/w", "e/v"].iter().map(|s| s.to_string()).collect();
        assert_eq!(format_attempted_providers_summary(&attempted, &h), "a/x (500), b/y (stall), c/z (network), d/w (empty), e/v");
        assert_eq!(format_attempted_providers_summary(&[], &h), "none");
        let err = build_all_targets_failed_error(&json!({"message": "last", "routingContext": {"statusCode": 429}}), &attempted[..1], &h[..1]);
        assert_eq!(err["message"], "All targets failed: a/x (500). Last error: last");
        assert_eq!(err["routingContext"]["statusCode"], 429);
        assert_eq!(err["routingContext"]["attemptCount"], 1);
    }

    #[test]
    fn skipped_entries_do_not_consume_slots() {
        let mut h = Vec::new();
        append_skipped_attempt(&mut h, &route("a", "x"), "cooldown", None);
        append_failure_attempt(&mut h, &route("a", "x"), &json!({"routingContext": {"statusCode": 502}}), None, None, None);
        assert_eq!(format_attempted_providers_summary(&["a/x".into()], &h), "a/x (502)");
    }

    #[test]
    fn attach_metadata_backfills_upstream_model() {
        let mut h = Vec::new();
        append_success_attempt(&mut h, &route("p", "m"), Some("chat"), None);
        let mut r = route("p", "m");
        r.config = json!({"models": {"m2": {"pricing": {"input": 1}}}, "discount": 0.1});
        let mut resp = json!({"plexus": {"apiType": "messages", "keep": 1}});
        attach_attempt_metadata(&mut resp, &["p/m".into()], &mut h, &r, "chat", Some("m2"));
        assert_eq!(h[0].upstream_model.as_deref(), Some("m2"));
        assert_eq!(resp["plexus"]["apiType"], "messages");
        assert_eq!(resp["plexus"]["pricing"], json!({"input": 1}));
        assert_eq!(resp["plexus"]["pricingModel"], "m2");
        assert_eq!(resp["plexus"]["keep"], 1);
        assert_eq!(resp["plexus"]["config"], json!({}));
    }
}
