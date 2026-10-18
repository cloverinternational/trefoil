//! Pure error helpers from `services/dispatch/dispatcher.ts`
//! (`compactProviderErrorSummary`, `formatClientProviderError`,
//! `extractFailureReason`, `formatFailureReason`, `isQuotaExhaustedError`,
//! `handleProviderError`'s error shaping) and `utils/sanitize-headers.ts`.
//!
//! Errors are JSON values shaped like the TypeScript error objects:
//! `{ message, status?, statusCode?, code?, piAiResponse?, routingContext? }`.
use super::util::{date_parse_ms, js_trim, parse_float, Headers};
use crate::engine::js::*;
use crate::engine::routing::RouteResult;
use crate::engine::transformer::EngineError;
use serde_json::{json, Value};

pub const PROVIDER_ERROR_SUMMARY_LIMIT: usize = 500;

/// `utils/constants.ts` `QUOTA_ERROR_PATTERNS`.
pub const QUOTA_ERROR_PATTERNS: [&str; 11] = [
    "insufficient_quota", "credit balance is too low", "used up your points", "quota exceeded", "out of credits",
    "balance too low", "insufficient balance", "insufficient funds", "account balance", "quota limit", "usage limit",
];
/// `utils/constants.ts` `DEADLINE_EXPIRED_PATTERNS`.
pub const DEADLINE_EXPIRED_PATTERNS: [&str; 3] = ["deadline expired", "deadline_exceeded", "deadline exceeded"];

/// An `EngineError` as the TS error-like value (`routingContext` carries the
/// status/code when present).
pub fn engine_error_value(err: &EngineError) -> Value {
    let mut rc = Obj::new();
    if let Some(s) = err.status { rc.insert("statusCode".into(), json!(s)); }
    if let Some(c) = &err.code { rc.insert("code".into(), json!(c)); }
    if rc.is_empty() { json!({"message": err.message}) } else { json!({"message": err.message, "routingContext": rc}) }
}

/// `compactProviderErrorSummary(value)`.
pub fn compact_provider_error_summary(value: Option<&Value>) -> String {
    let raw = match value {
        Some(Value::String(s)) => s.clone(),
        None | Some(Value::Null) => String::new(),
        other => to_string(other),
    };
    compact_str(&raw)
}

/// `compactProviderErrorSummary` for a string input.
pub fn compact_str(raw: &str) -> String {
    let trimmed = js_trim(raw);
    let text = if trimmed.is_empty() { "Unknown provider error" } else { trimmed };
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= PROVIDER_ERROR_SUMMARY_LIMIT { return text.to_owned(); }
    let head: String = chars[..PROVIDER_ERROR_SUMMARY_LIMIT].iter().collect();
    format!("{head}... [truncated {} chars]", chars.len() - PROVIDER_ERROR_SUMMARY_LIMIT)
}

/// `formatClientProviderError(statusCode, errorText)`.
pub fn format_client_provider_error(status_code: u16, error_text: &str) -> String {
    let reason = extract_failure_reason(Some(&Value::String(error_text.to_owned())))
        .or_else(|| (!error_text.is_empty()).then(|| error_text.to_owned()))
        .unwrap_or_else(|| "Unknown provider error".into());
    format!("Provider failed: {status_code} {}", compact_str(&reason))
}

/// `extractFailureReason(value)`.
pub fn extract_failure_reason(value: Option<&Value>) -> Option<String> {
    let value = value?;
    if let Value::String(s) = value {
        let trimmed = js_trim(s);
        if trimmed.is_empty() { return None; }
        if (trimmed.starts_with('{') && trimmed.ends_with('}')) || trimmed.starts_with('[') {
            return match parse(trimmed) {
                Some(parsed) => extract_failure_reason(Some(&parsed)).or_else(|| Some(trimmed.to_owned())),
                None => Some(trimmed.to_owned()),
            };
        }
        return Some(trimmed.to_owned());
    }
    let record = value.as_object()?;
    let nested_error = record.get("error").filter(|e| e.is_object());
    let nested_rc = record.get("routingContext").filter(|e| e.is_object());
    let candidates = [
        record.get("errorMessage"),
        nested_error.and_then(|e| e.get("errorMessage")),
        record.get("message"),
        nested_error.and_then(|e| e.get("message")),
        record.get("providerResponse"),
        record.get("rawResponseText"),
        nested_rc.and_then(|e| e.get("providerResponse")),
        nested_rc.and_then(|e| e.get("rawResponseText")),
    ];
    for candidate in candidates {
        if let Some(found) = extract_failure_reason(candidate) { return Some(found); }
    }
    if let Some(Value::String(history)) = record.get("retryHistory") {
        if let Some(Value::Array(entries)) = parse(history) {
            for entry in entries.iter().rev() {
                if let Some(found) = extract_failure_reason(entry.get("reason")) { return Some(found); }
            }
        }
    }
    None
}

/// `error?.routingContext?.statusCode ?? error?.status ?? error?.statusCode`.
pub fn error_status_code(error: &Value) -> Option<&Value> {
    let rc = get(error, &["routingContext", "statusCode"]);
    let a = if nullish(rc) { error.get("status").filter(|v| !v.is_null()) } else { rc };
    if a.is_some() { a } else { error.get("statusCode") }
}

/// `formatFailureReason(error, includeStatusCode)`.
pub fn format_failure_reason(error: &Value, include_status_code: bool) -> String {
    let extracted = extract_failure_reason(get(error, &["routingContext", "providerResponse"]))
        .or_else(|| extract_failure_reason(get(error, &["routingContext", "rawResponseText"])))
        .or_else(|| extract_failure_reason(error.get("piAiResponse")))
        .or_else(|| extract_failure_reason(Some(error)))
        .or_else(|| error.get("message").filter(|m| truthy(Some(m))).map(|m| to_string(Some(m))))
        .unwrap_or_else(|| "Unknown provider error".into());
    if include_status_code {
        if let Some(Value::Number(n)) = error_status_code(error) {
            return compact_str(&format!("HTTP {}: {extracted}", js_num_string(n.as_f64().unwrap_or(0.0))));
        }
    }
    compact_str(&extracted)
}

/// `isQuotaExhaustedError(errorText)` (dispatcher private helper).
pub fn is_quota_exhausted_error(error_text: &str) -> bool {
    const PATTERNS: [&str; 22] = [
        "insufficient fund", "insufficient_quota", "insufficient balance", "insufficient_balance", "quota exceeded",
        "out of credits", "credit balance is too low", "credit_balance_too_low", "account is out of credits",
        "used up your points", "usage limit", "free plan", "your credit balance", "remaining quota",
        "payment required", "billing", "no credits", "topup", "top up", "top_up", "rate limit", "rate_limit",
    ];
    let lower = error_text.to_lowercase();
    PATTERNS.iter().any(|p| lower.contains(p))
}

fn contains_any(text: &str, patterns: &[&str]) -> bool {
    let lower = text.to_lowercase();
    patterns.iter().any(|p| lower.contains(&p.to_lowercase()))
}

/// Inputs of `handleProviderError` that it reads off the fetch `Response`.
pub struct ProviderErrorInput<'a> {
    pub status: u16,
    pub error_text: &'a str,
    /// `response.headers.get('retry-after')`.
    pub retry_after: Option<&'a str>,
    /// `extractResponseHeaders(response)` (lower-cased names, fetch order).
    pub response_headers: Headers,
    pub url: Option<&'a str>,
    pub request_headers: Option<&'a Headers>,
    pub target_api_type: Option<&'a str>,
    /// `Date.now()`.
    pub now_ms: f64,
}

/// What `CooldownManager.markProviderFailure` is called with.
#[derive(Debug, Clone, PartialEq)]
pub struct CooldownDecision {
    /// `undefined` → the cooldown manager's default duration.
    pub duration_ms: Option<f64>,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProviderErrorOutcome {
    /// The thrown error (`message` + `routingContext`).
    pub error: Value,
    pub is_quota_400: bool,
    pub is_deadline_expired: bool,
    pub is_caller_error: bool,
    /// `Some` when a cooldown must be recorded (`!isCallerError`).
    pub cooldown: Option<CooldownDecision>,
}

/// `handleProviderError` minus its side effects. `provider_cooldown` is
/// `parseCooldownDurationForProvider(resolveCooldownProviderType(route), errorText, 'HTTP')`,
/// consulted only for 429/503 without a usable `Retry-After`.
pub fn shape_provider_error(
    route: &RouteResult,
    input: ProviderErrorInput<'_>,
    provider_cooldown: &dyn Fn(&str) -> Option<f64>,
) -> ProviderErrorOutcome {
    let status = input.status;
    let is_quota_400 = status == 400 && contains_any(input.error_text, &QUOTA_ERROR_PATTERNS);
    let is_deadline_expired = contains_any(input.error_text, &DEADLINE_EXPIRED_PATTERNS);
    let is_caller_error = status == 413 || status == 422 || (status == 400 && !is_quota_400) || is_deadline_expired;

    let cooldown = (!is_caller_error).then(|| {
        let mut duration: Option<f64> = None;
        if status == 429 || status == 503 {
            if let Some(header) = input.retry_after.filter(|h| !h.is_empty()) {
                let seconds = parse_float(header);
                if seconds.is_finite() && seconds > 0.0 {
                    duration = Some((seconds * 1000.0).ceil());
                } else if let Some(date) = date_parse_ms(header).filter(|d| *d > input.now_ms) {
                    duration = Some(date - input.now_ms);
                }
            }
            if duration.is_none_or(|d| d == 0.0) { duration = provider_cooldown(input.error_text); }
        }
        let reason = format_failure_reason(
            &json!({"routingContext": {"providerResponse": input.error_text, "statusCode": status}}),
            true,
        );
        CooldownDecision { duration_ms: duration, reason }
    });

    let mut rc = Obj::new();
    rc.insert("provider".into(), json!(route.provider));
    rc.insert("targetModel".into(), json!(route.model));
    set_opt(&mut rc, "targetApiType", input.target_api_type.map(|s| json!(s)));
    set_opt(&mut rc, "url", input.url.map(|s| json!(s)));
    rc.insert("headers".into(), Value::Object(sanitize_headers(input.request_headers.unwrap_or(&Headers::new()))));
    rc.insert("statusCode".into(), json!(status));
    rc.insert("providerResponse".into(), json!(input.error_text));
    rc.insert("providerResponseHeaders".into(), Value::Object(input.response_headers));
    rc.insert("cooldownTriggered".into(), json!(!is_caller_error));
    let error = json!({"message": format_client_provider_error(status, input.error_text), "routingContext": rc});
    ProviderErrorOutcome { error, is_quota_400, is_deadline_expired, is_caller_error, cooldown }
}

const SENSITIVE_HEADERS: [&str; 7] = ["authorization", "x-admin-key", "x-auth-token", "x-api-key", "x-goog-api-key", "api-key", "cookie"];

/// `sanitizeHeaders(headers)` (string or string-array values; others dropped
/// only when `undefined`, i.e. never for JSON).
pub fn sanitize_headers(headers: &Headers) -> Obj {
    let mut out = Obj::new();
    for (key, value) in headers {
        let lower = key.to_lowercase();
        let sensitive = SENSITIVE_HEADERS.iter().any(|h| lower == *h || lower.contains(h));
        let v = if !sensitive {
            value.clone()
        } else {
            match value {
                Value::Array(items) => Value::Array(items.iter().map(|v| json!(mask_secret(&to_string(Some(v))))).collect()),
                other => json!(mask_secret(&to_string(Some(other)))),
            }
        };
        out.insert(key.clone(), v);
    }
    out
}

fn utf16_slice(s: &[u16], from: usize, to: usize) -> String { String::from_utf16_lossy(&s[from.min(s.len())..to.min(s.len())]) }

fn mask_secret(value: &str) -> String {
    let units: Vec<u16> = value.encode_utf16().collect();
    if units.len() <= 8 { return "***".into(); }
    if value.starts_with("Bearer ") {
        let token = &units[7..];
        if token.len() <= 8 { return "Bearer ***".into(); }
        return format!("Bearer {}...{}", utf16_slice(token, 0, 4), utf16_slice(token, token.len() - 4, token.len()));
    }
    format!("{}...{}", utf16_slice(&units, 0, 4), utf16_slice(&units, units.len() - 4, units.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route() -> RouteResult {
        RouteResult { provider: "p".into(), model: "m".into(), config: json!({}), model_config: None, incoming_model_alias: None, canonical_model: None }
    }

    #[test]
    fn summaries_and_reasons() {
        assert_eq!(compact_str("  "), "Unknown provider error");
        let long = "x".repeat(510);
        assert_eq!(compact_str(&long), format!("{}... [truncated 10 chars]", "x".repeat(500)));
        assert_eq!(extract_failure_reason(Some(&json!(r#"{"error":{"message":"bad key"}}"#))).as_deref(), Some("bad key"));
        assert_eq!(extract_failure_reason(Some(&json!("{not json}"))).as_deref(), Some("{not json}"));
        assert_eq!(
            extract_failure_reason(Some(&json!({"retryHistory": r#"[{"reason":"a"},{"reason":"b"}]"#}))).as_deref(),
            Some("b")
        );
        assert_eq!(format_client_provider_error(429, r#"{"error":{"message":"slow down"}}"#), "Provider failed: 429 slow down");
        let err = json!({"message": "x", "routingContext": {"statusCode": 502, "providerResponse": "upstream down"}});
        assert_eq!(format_failure_reason(&err, true), "HTTP 502: upstream down");
        assert_eq!(format_failure_reason(&err, false), "upstream down");
        assert_eq!(format_failure_reason(&json!({}), false), "Unknown provider error");
    }

    #[test]
    fn provider_error_shaping() {
        let none = |_: &str| None;
        let base = |status, text: &'static str, retry: Option<&'static str>| ProviderErrorInput {
            status, error_text: text, retry_after: retry, response_headers: Headers::new(), url: Some("u"),
            request_headers: None, target_api_type: Some("chat"), now_ms: 0.0,
        };
        let out = shape_provider_error(&route(), base(400, "invalid param", None), &none);
        assert!(out.is_caller_error && out.cooldown.is_none());
        assert_eq!(out.error["message"], "Provider failed: 400 invalid param");
        assert_eq!(out.error["routingContext"]["cooldownTriggered"], false);
        let out = shape_provider_error(&route(), base(400, "Insufficient Balance", None), &none);
        assert!(out.is_quota_400 && out.cooldown.is_some());
        let out = shape_provider_error(&route(), base(429, "rate", Some("2.5")), &none);
        assert_eq!(out.cooldown.unwrap(), CooldownDecision { duration_ms: Some(2500.0), reason: "HTTP 429: rate".into() });
        let out = shape_provider_error(&route(), base(503, "x", None), &|_| Some(7.0));
        assert_eq!(out.cooldown.unwrap().duration_ms, Some(7.0));
        let out = shape_provider_error(&route(), base(500, "Deadline Exceeded", None), &none);
        assert!(out.is_deadline_expired && out.cooldown.is_none());
    }

    #[test]
    fn header_masking() {
        let mut h = Headers::new();
        h.insert("Authorization".into(), json!("Bearer sk-1234567890abcd"));
        h.insert("x-api-key".into(), json!("short"));
        h.insert("Accept".into(), json!("application/json"));
        let s = sanitize_headers(&h);
        assert_eq!(s["Authorization"], "Bearer sk-1...abcd");
        assert_eq!(s["x-api-key"], "***");
        assert_eq!(s["Accept"], "application/json");
    }
}
