//! Dispatch-local JS-semantics helpers not covered by `engine::js`.
use crate::engine::js::Obj;
use serde_json::Value;

/// `getApiBaseType`.
pub fn base_type(api: &str) -> String { crate::engine::config::api_format::base_type(api) }

/// `delete obj[key]` keeping the remaining insertion order (serde_json's
/// `Map::remove` is `swap_remove` under `preserve_order`).
pub fn del(map: &mut Obj, key: &str) -> Option<Value> { map.shift_remove(key) }

/// `a === b` for possibly-undefined JSON values: primitives by value
/// (numbers numerically), objects/arrays never equal (distinct references).
pub fn strict_eq(a: Option<&Value>, b: Option<&Value>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(Value::Null), Some(Value::Null)) => true,
        (Some(Value::Bool(x)), Some(Value::Bool(y))) => x == y,
        (Some(Value::String(x)), Some(Value::String(y))) => x == y,
        (Some(Value::Number(x)), Some(Value::Number(y))) => x.as_f64() == y.as_f64(),
        _ => false,
    }
}

/// `typeof v === 'object' && v !== null` (arrays included).
pub fn is_object_like(v: Option<&Value>) -> bool { matches!(v, Some(Value::Object(_)) | Some(Value::Array(_))) }

/// `{ ...v }` for an object (anything else spreads to `{}` for our inputs).
pub fn obj_of(v: Option<&Value>) -> Obj { v.and_then(Value::as_object).cloned().unwrap_or_default() }

/// `new URL(url).hostname` lower-cased; `None` when the URL does not parse.
pub fn hostname(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    Some(parsed.host_str().unwrap_or("").trim_start_matches('[').trim_end_matches(']').to_lowercase())
}

/// `createHash('sha256').update(bytes).digest('hex')`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

/// JS whitespace (`String.prototype.trim`).
pub fn is_js_ws(c: char) -> bool { c.is_whitespace() || c == '\u{feff}' }
pub fn js_trim(s: &str) -> &str { s.trim_matches(is_js_ws) }

/// `parseFloat(s)`: longest numeric prefix after leading whitespace.
pub fn parse_float(s: &str) -> f64 {
    let t = s.trim_start_matches(is_js_ws);
    let b = t.as_bytes();
    let mut i = 0;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') { i += 1; }
    if t[i..].starts_with("Infinity") {
        return if b.first() == Some(&b'-') { f64::NEG_INFINITY } else { f64::INFINITY };
    }
    let digits_start = i;
    while i < b.len() && b[i].is_ascii_digit() { i += 1; }
    let mut had_digits = i > digits_start;
    if i < b.len() && b[i] == b'.' {
        let frac = i + 1;
        let mut j = frac;
        while j < b.len() && b[j].is_ascii_digit() { j += 1; }
        if j > frac || had_digits { had_digits = had_digits || j > frac; i = j; }
    }
    if !had_digits { return f64::NAN; }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        let mut j = i + 1;
        if j < b.len() && (b[j] == b'+' || b[j] == b'-') { j += 1; }
        let exp = j;
        while j < b.len() && b[j].is_ascii_digit() { j += 1; }
        if j > exp { i = j; }
    }
    t[..i].parse::<f64>().unwrap_or(f64::NAN)
}

/// `Date.parse` for the HTTP-date / ISO forms a `Retry-After` can carry.
pub fn date_parse_ms(s: &str) -> Option<f64> {
    let s = s.trim();
    if let Ok(d) = chrono::DateTime::parse_from_rfc2822(s) { return Some(d.timestamp_millis() as f64); }
    if let Ok(d) = chrono::DateTime::parse_from_rfc3339(s) { return Some(d.timestamp_millis() as f64); }
    None
}

/// `Object.keys(obj).find((k) => k.toLowerCase() === name.toLowerCase())`.
pub fn find_key_ci<'a>(map: &'a Obj, name: &str) -> Option<&'a String> {
    let target = name.to_lowercase();
    map.keys().find(|k| k.to_lowercase() == target)
}

/// String-valued header map (`Record<string, string>`), insertion ordered.
pub type Headers = Obj;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn helpers() {
        assert_eq!(parse_float(" 1.5s"), 1.5);
        assert_eq!(parse_float("120"), 120.0);
        assert!(parse_float("abc").is_nan());
        assert_eq!(parse_float(".5"), 0.5);
        assert_eq!(parse_float("2e3x"), 2000.0);
        assert!(strict_eq(Some(&json!(1)), Some(&json!(1.0))));
        assert!(!strict_eq(Some(&json!({})), Some(&json!({}))));
        assert_eq!(hostname("https://API.Anthropic.com/v1").as_deref(), Some("api.anthropic.com"));
        assert_eq!(hostname("not a url"), None);
    }
}
