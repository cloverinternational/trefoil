//! `transformers/gemini/utils/*` (thought-signature, json-schema, model-tail)
//! plus small JS-semantics helpers the Gemini modules share.
use crate::engine::js::*;
use crate::engine::transformer::{EngineError, EngineResult};
use serde_json::{json, Map, Value};

// ---------------------------------------------------------------------------
// utils/thought-signature.ts
// ---------------------------------------------------------------------------

/// `/^[A-Za-z0-9+/]+={0,2}$/`
fn matches_base64_pattern(s: &str) -> bool {
    let body = s.trim_end_matches('=');
    let pad = s.len() - body.len();
    !body.is_empty() && pad <= 2 && body.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/')
}

/// `isValidThoughtSignature`: undefined/empty signatures are valid (no thinking).
pub fn is_valid_thought_signature(signature: Option<&str>) -> bool {
    let Some(sig) = signature.filter(|s| !s.is_empty()) else { return true };
    // `signature.length` is UTF-16 length; non-ASCII input fails the pattern anyway.
    if sig.encode_utf16().count() % 4 != 0 { return false; }
    matches_base64_pattern(sig)
}

/// `isValidThoughtSignature` for an arbitrary JS value (non-strings: `!sig`
/// short-circuits falsy values; other non-strings have no usable `length`
/// and fail).
pub fn is_valid_thought_signature_value(signature: Option<&Value>) -> bool {
    if !truthy(signature) { return true; }
    match signature { Some(Value::String(s)) => is_valid_thought_signature(Some(s)), _ => false }
}

/// `sanitizeThoughtSignature`.
pub fn sanitize_thought_signature(signature: Option<&str>) -> Option<&str> {
    let sig = signature.filter(|s| !s.is_empty())?;
    if is_valid_thought_signature(Some(sig)) { Some(sig) } else { None }
}

// ---------------------------------------------------------------------------
// utils/json-schema.ts
// ---------------------------------------------------------------------------

/// `normalizeJsonSchemaTypes`: lowercases string `type` members recursively.
pub fn normalize_json_schema_types(schema: &Value) -> Value {
    match schema {
        Value::Array(items) => Value::Array(items.iter().map(normalize_json_schema_types).collect()),
        Value::Object(map) => {
            let mut out = Map::new();
            for (k, v) in map {
                let nv = match v {
                    Value::String(s) if k == "type" => Value::String(s.to_lowercase()),
                    Value::Object(_) | Value::Array(_) => normalize_json_schema_types(v),
                    other => other.clone(),
                };
                out.insert(k.clone(), nv);
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

// ---------------------------------------------------------------------------
// utils/model-tail.ts
// ---------------------------------------------------------------------------

const TEXT_ONLY_MODEL_PART_KEYS: [&str; 3] = ["text", "thought", "thoughtSignature"];

/// `isTextOnlyModelContent`.
pub fn is_text_only_model_content(content: &Value) -> bool {
    if get_str(content, &["role"]) != Some("model") { return false; }
    let Some(parts) = get_arr(content, &["parts"]).filter(|p| !p.is_empty()) else { return false };
    parts.iter().all(|part| match part.as_object() {
        Some(map) if map.contains_key("text") => map.keys().all(|k| TEXT_ONLY_MODEL_PART_KEYS.contains(&k.as_str())),
        _ => false,
    })
}

fn user_placeholder() -> Value { json!({"role": "user", "parts": [{"text": "."}]}) }

/// `appendUserAfterTextOnlyModelTail`: returns `None` when the input is
/// returned unchanged (same reference in TS), otherwise the new array.
pub fn append_user_after_text_only_model_tail(contents: &[Value]) -> Option<Vec<Value>> {
    let last = contents.last()?;
    if !is_text_only_model_content(last) { return None; }
    let mut out = contents.to_vec();
    out.push(user_placeholder());
    Some(out)
}

/// `ensureContentsEndWithUser`: mutating variant; true when a turn was appended.
pub fn ensure_contents_end_with_user(contents: &mut Vec<Value>) -> bool {
    match contents.last() {
        Some(last) if is_text_only_model_content(last) => { contents.push(user_placeholder()); true }
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// JS helpers
// ---------------------------------------------------------------------------

/// A `TypeError` thrown by the TS source (surfaced as an engine error).
pub fn type_error(message: impl Into<String>) -> EngineError { EngineError::new(message) }

/// `Cannot read properties of undefined|null (reading 'key')` for a nullish receiver.
pub fn read_error(receiver: Option<&Value>, key: &str) -> EngineError {
    let what = if receiver.is_none() { "undefined" } else { "null" };
    type_error(format!("Cannot read properties of {what} (reading '{key}')"))
}

/// `v.key` with JS semantics: throws on a nullish receiver; primitives have no
/// JSON members (`undefined`).
pub fn read<'a>(v: Option<&'a Value>, key: &str) -> EngineResult<Option<&'a Value>> {
    match v {
        None | Some(Value::Null) => Err(read_error(v, key)),
        Some(Value::Object(map)) => Ok(map.get(key)),
        Some(Value::Array(items)) => Ok(key.parse::<usize>().ok().and_then(|i| items.get(i))),
        Some(_) => Ok(None),
    }
}

/// `JSON.stringify(v)` where `undefined` yields `undefined`.
pub fn stringify_opt(v: Option<&Value>) -> Option<Value> { v.map(|v| Value::String(stringify(v))) }

/// `JSON.parse(v)`: strings parse as-is, other values are coerced with `String(v)`.
pub fn json_parse_js(v: Option<&Value>) -> EngineResult<Value> {
    let text = match v { Some(Value::String(s)) => s.clone(), other => to_string(other) };
    serde_json::from_str(&text).map_err(|e| EngineError::new(format!("JSON Parse error: {e}")))
}

fn is_stringish(v: Option<&Value>) -> bool { matches!(v, Some(Value::String(_) | Value::Array(_) | Value::Object(_))) }

/// JS binary `a + b` over JSON values (NaN/Infinity serialize as `null`).
pub fn js_plus(a: Option<&Value>, b: Option<&Value>) -> Value {
    if is_stringish(a) || is_stringish(b) {
        return Value::String(format!("{}{}", to_string(a), to_string(b)));
    }
    let n = |v: Option<&Value>| to_number(v).unwrap_or(f64::NAN);
    num(n(a) + n(b))
}

/// `'call_' + Math.random().toString(36).substring(7)`.
pub fn random_call_id() -> String { format!("call_{}", random_base36(6)) }

/// `String.prototype.toLowerCase` on a value that must be a string.
pub fn lower(v: &Value, expr: &str) -> EngineResult<String> {
    match v { Value::String(s) => Ok(s.to_lowercase()), _ => Err(type_error(format!("{expr}.toLowerCase is not a function"))) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thought_signature_validation() {
        assert!(is_valid_thought_signature(None));
        assert!(is_valid_thought_signature(Some("")));
        assert!(is_valid_thought_signature(Some("aGVsbG8=")));
        assert!(is_valid_thought_signature(Some("SGVsbG8gV29ybGQh")));
        assert!(is_valid_thought_signature(Some("aGVsbG80")));
        assert!(is_valid_thought_signature(Some("YWJjZGVm")));
        assert!(!is_valid_thought_signature(Some("aGVsbG8!")));
        assert!(!is_valid_thought_signature(Some("hello world")));
        assert!(!is_valid_thought_signature(Some("abc@def")));
        assert!(!is_valid_thought_signature(Some("abcde")));
        assert!(!is_valid_thought_signature(Some("abcdefg")));
        assert!(is_valid_thought_signature(Some("aGVsbG8+")));
        assert!(!is_valid_thought_signature(Some("aGVsbG8===")));
        assert!(!is_valid_thought_signature(Some("====")));
        assert_eq!(sanitize_thought_signature(None), None);
        assert_eq!(sanitize_thought_signature(Some("aGVsbG8=")), Some("aGVsbG8="));
        assert_eq!(sanitize_thought_signature(Some("invalid!")), None);
        assert_eq!(sanitize_thought_signature(Some("abcde")), None);
    }

    #[test]
    fn model_tail() {
        assert!(is_text_only_model_content(&json!({"role": "model", "parts": [{"text": "stored"}]})));
        assert!(is_text_only_model_content(&json!({"role": "model", "parts": [{"text": "s", "thought": true, "thoughtSignature": "aGVsbG8="}]})));
        assert!(!is_text_only_model_content(&json!({"role": "user", "parts": [{"text": "hi"}]})));
        assert!(!is_text_only_model_content(&json!({"role": "model", "parts": []})));
        assert!(!is_text_only_model_content(&json!({"role": "model", "parts": [{"functionCall": {"name": "lookup", "args": {}}}]})));
        assert!(!is_text_only_model_content(&json!({"role": "model", "parts": [{"inlineData": {"mimeType": "image/png", "data": "abcd"}}]})));
        assert!(!is_text_only_model_content(&json!({"role": "model", "parts": [{"text": "stored", "media_resolution": "low"}]})));

        let contents = vec![json!({"role": "user", "parts": [{"text": "Remember cobalt"}]}), json!({"role": "model", "parts": [{"text": "stored"}]})];
        let out = append_user_after_text_only_model_tail(&contents).unwrap();
        assert_eq!(out.len(), 3);
        assert_eq!(out[2], json!({"role": "user", "parts": [{"text": "."}]}));
        let user_tail = vec![json!({"role": "user", "parts": [{"text": "hi"}]})];
        assert!(append_user_after_text_only_model_tail(&user_tail).is_none());
        let fc_tail = vec![json!({"role": "model", "parts": [{"functionCall": {"name": "lookup", "args": {}}}]})];
        assert!(append_user_after_text_only_model_tail(&fc_tail).is_none());

        let mut c = vec![json!({"role": "model", "parts": [{"text": "stored"}]})];
        assert!(ensure_contents_end_with_user(&mut c));
        assert_eq!(c.len(), 2);
        let mut u = user_tail.clone();
        assert!(!ensure_contents_end_with_user(&mut u));
        assert_eq!(u.len(), 1);
    }

    #[test]
    fn json_schema_and_plus() {
        assert_eq!(
            normalize_json_schema_types(&json!({"type": "OBJECT", "properties": {"a": {"type": "STRING"}}, "items": [{"type": "INTEGER"}], "n": null})),
            json!({"type": "object", "properties": {"a": {"type": "string"}}, "items": [{"type": "integer"}], "n": null})
        );
        assert_eq!(js_plus(Some(&json!(100)), Some(&json!(20))), json!(120));
        assert_eq!(js_plus(None, Some(&json!(0))), Value::Null);
        assert_eq!(js_plus(Some(&Value::Null), Some(&json!(0))), json!(0));
        assert_eq!(js_plus(Some(&json!("5")), Some(&json!(1))), json!("51"));
    }
}
