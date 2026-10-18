//! `types/usage.ts` (`UsageRecord`) plus the JS number helpers the usage
//! pipeline relies on (`Number(x.toFixed(n))`, `parseFloat`, `parseInt`).
//!
//! The TypeScript code mutates a `Partial<UsageRecord>` in place and persists
//! it with Drizzle, which skips `undefined` members on insert/update. The
//! record is therefore kept as a camelCase JSON object: an absent key is
//! `undefined` (not written), `null` is written as SQL NULL.
use crate::engine::js::{self, Obj};
use serde_json::Value;

/// Storage kind of a `request_usage` column (Trefoil/Plexus Postgres schema).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnKind {
    /// `text`
    Text,
    /// `integer` (int4)
    Int,
    /// `bigint` (int8)
    BigInt,
    /// `real` (float4)
    Real,
}

/// Every `UsageRecord` field that maps to a `request_usage` column, in the
/// Drizzle schema order (`drizzle/schema/postgres/request-usage.ts`).
/// Fields without a column (`hasDebug`, `hasError`) are never written.
pub const REQUEST_USAGE_FIELDS: &[(&str, &str, ColumnKind)] = &[
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
    ("incomingModelAlias", "incoming_model_alias", ColumnKind::Text),
    ("canonicalModelName", "canonical_model_name", ColumnKind::Text),
    ("selectedModelName", "selected_model_name", ColumnKind::Text),
    ("finalAttemptProvider", "final_attempt_provider", ColumnKind::Text),
    ("finalAttemptModel", "final_attempt_model", ColumnKind::Text),
    ("upstreamModel", "upstream_model", ColumnKind::Text),
    ("allAttemptedProviders", "all_attempted_providers", ColumnKind::Text),
    ("outgoingApiType", "outgoing_api_type", ColumnKind::Text),
    ("reasoningEffort", "reasoning_effort", ColumnKind::Text),
    ("requestedServiceTier", "requested_service_tier", ColumnKind::Text),
    ("requestedServiceTierRaw", "requested_service_tier_raw", ColumnKind::Text),
    ("serviceTier", "service_tier", ColumnKind::Text),
    ("serviceTierRaw", "service_tier_raw", ColumnKind::Text),
    ("tokensInput", "tokens_input", ColumnKind::Int),
    ("tokensOutput", "tokens_output", ColumnKind::Int),
    ("tokensReasoning", "tokens_reasoning", ColumnKind::Int),
    ("tokensCached", "tokens_cached", ColumnKind::Int),
    ("tokensCacheWrite", "tokens_cache_write", ColumnKind::Int),
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
    ("tokensEstimated", "tokens_estimated", ColumnKind::Int),
    ("createdAt", "created_at", ColumnKind::BigInt),
    ("toolsDefined", "tools_defined", ColumnKind::Int),
    ("messageCount", "message_count", ColumnKind::Int),
    ("parallelToolCallsEnabled", "parallel_tool_calls_enabled", ColumnKind::Int),
    ("toolCallsCount", "tool_calls_count", ColumnKind::Int),
    ("finishReason", "finish_reason", ColumnKind::Text),
    ("isVisionFallthrough", "is_vision_fallthrough", ColumnKind::Int),
    ("isDescriptorRequest", "is_descriptor_request", ColumnKind::Int),
    ("visionFallthroughModel", "vision_fallthrough_model", ColumnKind::Text),
    ("kwhUsed", "kwh_used", ColumnKind::Real),
    ("providerReportedCost", "provider_reported_cost", ColumnKind::Real),
];

/// Column name for a record field.
pub fn column_for(field: &str) -> Option<(&'static str, ColumnKind)> {
    REQUEST_USAGE_FIELDS.iter().find(|(f, _, _)| *f == field).map(|(_, c, k)| (*c, *k))
}

/// `Partial<UsageRecord>` as a camelCase JSON object (absent = `undefined`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UsageRecord(pub Obj);

impl UsageRecord {
    pub fn new() -> Self { Self(Obj::new()) }
    /// `{ requestId }` — the minimal record the routes start from.
    pub fn with_request_id(request_id: &str) -> Self {
        let mut r = Self::new();
        r.set("requestId", request_id);
        r
    }
    /// Wrap an object value (non-objects give an empty record).
    pub fn from_value(v: Value) -> Self {
        match v { Value::Object(m) => Self(m), _ => Self::new() }
    }
    pub fn to_value(&self) -> Value { Value::Object(self.0.clone()) }

    /// `record.key` (`None` = undefined).
    pub fn get(&self, key: &str) -> Option<&Value> { self.0.get(key) }
    /// `record.key = value`.
    pub fn set(&mut self, key: &str, value: impl Into<Value>) { self.0.insert(key.to_owned(), value.into()); }
    /// `record.key = value` where `None` is `undefined` (the key is dropped,
    /// which is what Drizzle and `JSON.stringify` observe).
    pub fn set_opt(&mut self, key: &str, value: Option<Value>) {
        match value { Some(v) => { self.0.insert(key.to_owned(), v); } None => { self.0.remove(key); } }
    }
    /// `record.key = n` for a JS number (NaN → `null`, like `JSON.stringify`).
    pub fn set_num(&mut self, key: &str, n: f64) { self.set(key, js::num(n)); }
    /// `record.key = n ?? null`.
    pub fn set_num_or_null(&mut self, key: &str, n: Option<f64>) {
        self.set(key, n.map(js::num).unwrap_or(Value::Null));
    }
    pub fn str(&self, key: &str) -> Option<&str> { self.0.get(key).and_then(Value::as_str) }
    pub fn f64(&self, key: &str) -> Option<f64> { self.0.get(key).and_then(Value::as_f64) }
    pub fn truthy(&self, key: &str) -> bool { js::truthy(self.0.get(key)) }
    /// `record.key || 0` as a number.
    pub fn num_or_zero(&self, key: &str) -> f64 {
        if self.truthy(key) { js::to_number(self.0.get(key)).unwrap_or(f64::NAN) } else { 0.0 }
    }
    /// `record.key` as an owned string when it is a non-empty string (`x || null`).
    pub fn str_or_none(&self, key: &str) -> Option<String> {
        if self.truthy(key) { Some(js::to_string(self.0.get(key))) } else { None }
    }
    pub fn request_id(&self) -> String { self.str("requestId").unwrap_or_default().to_owned() }
}

/// `Number(x.toFixed(digits))` with exact JS rounding (round half away
/// from zero on the exact binary value; ties are only possible for values
/// whose exact expansion terminates at `digits + 1`).
pub fn to_fixed_num(x: f64, digits: usize) -> f64 {
    if !x.is_finite() || x.abs() >= 1e21 { return x; }
    let neg = x < 0.0;
    let s = format!("{:.40}", x.abs());
    let (int_part, frac) = s.split_once('.').unwrap_or((s.as_str(), ""));
    let keep = &frac[..digits.min(frac.len())];
    let rest = &frac[digits.min(frac.len())..];
    let round_up = rest.as_bytes().first().is_some_and(|d| *d >= b'5');
    let mut digits_str: Vec<u8> = format!("{int_part}{keep}").into_bytes();
    if round_up {
        let mut i = digits_str.len();
        loop {
            if i == 0 { digits_str.insert(0, b'1'); break; }
            i -= 1;
            if digits_str[i] == b'9' { digits_str[i] = b'0'; } else { digits_str[i] += 1; break; }
        }
    }
    let all = String::from_utf8(digits_str).unwrap_or_default();
    let split = all.len() - keep.len();
    let text = if keep.is_empty() { all } else { format!("{}.{}", &all[..split], &all[split..]) };
    let v: f64 = text.parse().unwrap_or(f64::NAN);
    if neg { -v } else { v }
}

/// JS `Number.prototype.toString()` (shortest round-trip digits; exponent
/// form below 1e-6 and from 1e21, e.g. `3e-7`, `0.000003`, `1e+21`).
pub fn js_number_string(x: f64) -> String {
    if x.is_nan() { return "NaN".into(); }
    if x.is_infinite() { return if x > 0.0 { "Infinity".into() } else { "-Infinity".into() }; }
    if x == 0.0 { return "0".into(); }
    let sign = if x < 0.0 { "-" } else { "" };
    let sci = format!("{:e}", x.abs());
    let (mantissa, exp) = sci.split_once('e').unwrap_or((sci.as_str(), "0"));
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let e: i64 = exp.parse().unwrap_or(0);
    let k = digits.len() as i64;
    let n = e + 1;
    let body = if k <= n && n <= 21 {
        format!("{digits}{}", "0".repeat((n - k) as usize))
    } else if 0 < n && n <= 21 {
        format!("{}.{}", &digits[..n as usize], &digits[n as usize..])
    } else if -6 < n && n <= 0 {
        format!("0.{}{digits}", "0".repeat((-n) as usize))
    } else {
        let rest = if k > 1 { format!(".{}", &digits[1..]) } else { String::new() };
        let es = if n > 0 { format!("+{}", n - 1) } else { format!("{}", n - 1) };
        format!("{}{rest}e{es}", &digits[..1])
    };
    format!("{sign}{body}")
}

/// `JSON.stringify(v)` / `JSON.stringify(v, null, indent)` with JS number
/// formatting (serde_json would print `3e-6` / `10.0` where JS prints
/// `0.000003` / `10`). Keys keep insertion order.
pub fn js_stringify(v: &Value) -> String { let mut out = String::new(); write_json(v, None, 0, &mut out); out }
pub fn js_stringify_pretty(v: &Value, indent: usize) -> String { let mut out = String::new(); write_json(v, Some(indent), 0, &mut out); out }

fn write_json(v: &Value, indent: Option<usize>, depth: usize, out: &mut String) {
    let nl = |out: &mut String, d: usize| if let Some(i) = indent { out.push('\n'); out.push_str(&" ".repeat(i * d)); };
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => match (n.as_i64(), n.as_u64(), n.as_f64()) {
            (Some(i), _, _) => out.push_str(&i.to_string()),
            (_, Some(u), _) => out.push_str(&u.to_string()),
            (_, _, Some(f)) if f.is_finite() => out.push_str(&js_number_string(f)),
            _ => out.push_str("null"),
        },
        Value::String(s) => out.push_str(&serde_json::to_string(s).unwrap_or_default()),
        Value::Array(items) => {
            if items.is_empty() { out.push_str("[]"); return; }
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 { out.push(','); }
                nl(out, depth + 1);
                write_json(item, indent, depth + 1, out);
            }
            nl(out, depth);
            out.push(']');
        }
        Value::Object(map) => {
            if map.is_empty() { out.push_str("{}"); return; }
            out.push('{');
            for (i, (k, item)) in map.iter().enumerate() {
                if i > 0 { out.push(','); }
                nl(out, depth + 1);
                out.push_str(&serde_json::to_string(k).unwrap_or_default());
                out.push(':');
                if indent.is_some() { out.push(' '); }
                write_json(item, indent, depth + 1, out);
            }
            nl(out, depth);
            out.push('}');
        }
    }
}

/// JS `parseFloat(String(v))`: longest leading decimal literal (or
/// `Infinity`), NaN when there is none.
pub fn parse_float(v: Option<&Value>) -> f64 {
    let s = match v {
        Some(Value::Number(n)) => return n.as_f64().unwrap_or(f64::NAN),
        other => js::to_string(other),
    };
    parse_float_str(&s)
}

pub fn parse_float_str(s: &str) -> f64 {
    let t = s.trim_start();
    let b = t.as_bytes();
    let mut i = 0;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') { i += 1; }
    if t[i..].starts_with("Infinity") {
        return if b.first() == Some(&b'-') { f64::NEG_INFINITY } else { f64::INFINITY };
    }
    let start_digits = i;
    while i < b.len() && b[i].is_ascii_digit() { i += 1; }
    let mut had = i > start_digits;
    if i < b.len() && b[i] == b'.' {
        let j = i + 1;
        let mut k = j;
        while k < b.len() && b[k].is_ascii_digit() { k += 1; }
        if k > j || had { had = had || k > j; i = k; }
    }
    if !had { return f64::NAN; }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        let mut k = i + 1;
        if k < b.len() && (b[k] == b'+' || b[k] == b'-') { k += 1; }
        let ds = k;
        while k < b.len() && b[k].is_ascii_digit() { k += 1; }
        if k > ds { i = k; }
    }
    t[..i].parse::<f64>().unwrap_or(f64::NAN)
}

/// JS `parseInt(s, 10)` (leading sign + digits, NaN when none).
pub fn parse_int(s: &str) -> Option<i64> {
    let t = s.trim_start();
    let (neg, rest) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() { return None; }
    let n: i64 = digits.parse().unwrap_or(i64::MAX);
    Some(if neg { -n } else { n })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn to_fixed_matches_js() {
        assert_eq!(to_fixed_num(0.0007217243274280318, 8), 0.00072172);
        // Exact tie (1/512 = 0.001953125): JS rounds up, banker's rounding would not.
        assert_eq!(to_fixed_num(1.0 / 512.0, 8), 0.00195313);
        assert_eq!(to_fixed_num(-1.0 / 512.0, 8), -0.00195313);
        assert_eq!(to_fixed_num(0.000025, 8), 0.000025);
        assert_eq!(to_fixed_num(20.0, 8), 20.0);
        assert_eq!(to_fixed_num(0.999999999, 8), 1.0);
        assert_eq!(to_fixed_num(1.23456789012, 10), 1.2345678901);
        assert!(to_fixed_num(f64::NAN, 8).is_nan());
    }

    #[test]
    fn js_number_and_stringify() {
        assert_eq!(js_number_string(0.000003), "0.000003");
        assert_eq!(js_number_string(3e-7), "3e-7");
        assert_eq!(js_number_string(1.5e-7), "1.5e-7");
        assert_eq!(js_number_string(0.00072172), "0.00072172");
        assert_eq!(js_number_string(123.456), "123.456");
        assert_eq!(js_number_string(1e21), "1e+21");
        assert_eq!(js_number_string(-2.5), "-2.5");
        assert_eq!(js_number_string(100.0), "100");
        let v: Value = serde_json::from_str(r#"{"a":10.0,"b":[0.000001,{}],"c":"x\n"}"#).unwrap();
        assert_eq!(js_stringify(&v), r#"{"a":10,"b":[0.000001,{}],"c":"x\n"}"#);
        assert_eq!(js_stringify_pretty(&json!({"error": {"code": 400, "list": []}}), 2), "{\n  \"error\": {\n    \"code\": 400,\n    \"list\": []\n  }\n}");
    }

    #[test]
    fn parse_float_matches_js() {
        assert_eq!(parse_float(Some(&json!("0.000003"))), 0.000003);
        assert_eq!(parse_float(Some(&json!("1e-6abc"))), 1e-6);
        assert_eq!(parse_float(Some(&json!(".5"))), 0.5);
        assert_eq!(parse_float(Some(&json!("-1"))), -1.0);
        assert!(parse_float(Some(&json!(""))).is_nan());
        assert!(parse_float(None).is_nan());
        assert_eq!(parse_float(Some(&json!(2.5))), 2.5);
        assert_eq!(parse_int("  42abc"), Some(42));
        assert_eq!(parse_int("abc"), None);
    }

    #[test]
    fn record_undefined_vs_null() {
        let mut r = UsageRecord::with_request_id("r1");
        r.set_opt("outgoingApiType", None);
        r.set("finishReason", Value::Null);
        assert!(r.get("outgoingApiType").is_none());
        assert_eq!(r.get("finishReason"), Some(&Value::Null));
        assert_eq!(column_for("tokensCacheWrite"), Some(("tokens_cache_write", ColumnKind::Int)));
        assert_eq!(column_for("hasDebug"), None);
    }
}
