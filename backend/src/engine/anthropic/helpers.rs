//! Minimal ports of the helpers the Anthropic transformer imports from
//! elsewhere in Plexus:
//! - `transformers/utils.ts`: `countTokens`, `getThinkLevel`, `formatBase64`
//! - `services/pi-ai/reasoning.ts`: `normalizeEffort`, `clampEffortToWindow`,
//!   `splitReasoningSuffix` (alias part only), `EFFORT_LADDER`
//! - `services/pi-ai/catalog.ts`: `getCatalogModel('anthropic', id)` reduced
//!   to the `thinkingLevelMap.off` lookup `thinking-clamp.ts` performs
//! - `utils/api-format.ts`: `getApiBaseType`
//!
//! Plus a few JS-semantics helpers (`+`, `trimStart`, Bun TypeErrors).
use crate::engine::js::{to_number, to_string};
use crate::engine::transformer::EngineError;
use serde_json::Value;

/// pi-ai effort ladder, lowest → highest (`EFFORT_LADDER`).
pub const EFFORT_LADDER: [&str; 6] = ["minimal", "low", "medium", "high", "xhigh", "max"];

/// `normalizeEffort`: `None` for non-strings and unknown values; `"off"` for
/// none/off/disabled.
pub fn normalize_effort(raw: Option<&Value>) -> Option<&'static str> {
    normalize_effort_str(raw?.as_str()?)
}

pub fn normalize_effort_str(raw: &str) -> Option<&'static str> {
    match js_trim(raw).to_lowercase().as_str() {
        "none" | "off" | "disabled" => Some("off"),
        "minimal" => Some("minimal"),
        "low" => Some("low"),
        "medium" => Some("medium"),
        "high" => Some("high"),
        "xhigh" => Some("xhigh"),
        "max" | "maximum" => Some("max"),
        _ => None,
    }
}

/// `clampEffortToWindow(effort, floor, ceiling)`.
pub fn clamp_effort_to_window(effort: &'static str, floor: Option<&str>, ceiling: Option<&str>) -> &'static str {
    let pos = |e: &str| EFFORT_LADDER.iter().position(|x| *x == e);
    let Some(mut idx) = pos(effort) else { return effort };
    if let Some(f) = floor.and_then(pos) {
        if f > idx { idx = f; }
    }
    if let Some(c) = ceiling.and_then(pos) {
        if c < idx { idx = c; }
    }
    EFFORT_LADDER[idx]
}

/// `splitReasoningSuffix(alias).alias`: strips a trailing recognised
/// `:effort` suffix; unknown suffixes stay on the alias.
pub fn split_reasoning_suffix_alias(model_alias: &str) -> &str {
    let Some(idx) = model_alias.rfind(':') else { return model_alias };
    if idx == 0 || idx == model_alias.len() - 1 { return model_alias; }
    if normalize_effort_str(&model_alias[idx + 1..]).is_none() { return model_alias; }
    &model_alias[..idx]
}

/// `getThinkLevel(thinking_budget)`.
pub fn get_think_level(thinking_budget: f64) -> &'static str {
    if thinking_budget <= 0.0 { "none" } else if thinking_budget <= 1024.0 { "low" } else if thinking_budget <= 8192.0 { "medium" } else { "high" }
}

/// `getApiBaseType(apiType)`.
pub fn get_api_base_type(api_type: &str) -> String {
    js_trim(api_type).to_lowercase().split(':').next().unwrap_or("").to_owned()
}

/// Anthropic models in the pinned pi-ai 1.2.0 builtin catalog (and the
/// persisted pi.dev overlay snapshot), with their `thinkingLevelMap.off`
/// state: `Some(true)` = `off` maps to a level, `Some(false)` = `off: null`
/// (thinking cannot be disabled), `None` = no `off` key / no map.
const ANTHROPIC_CATALOG: &[(&str, Option<bool>)] = &[
    ("claude-fable-5", Some(false)),
    ("claude-fable-5-1", Some(false)),
    ("claude-haiku-4-5", None),
    ("claude-haiku-4-5-20251001", None),
    ("claude-haiku-5-5", Some(false)),
    ("claude-opus-4-5", None),
    ("claude-opus-4-5-20251101", None),
    ("claude-opus-4-6", None),
    ("claude-opus-4-7", None),
    ("claude-opus-4-8", None),
    ("claude-opus-5", Some(false)),
    ("claude-opus-5-5", Some(false)),
    ("claude-sonnet-4-5", None),
    ("claude-sonnet-4-5-20250929", None),
    ("claude-sonnet-4-6", None),
    ("claude-sonnet-5", None),
    ("claude-sonnet-5-5", Some(false)),
];

/// `getCatalogModel('anthropic', id)` reduced to the off-key lookup:
/// outer `None` = model not in the catalog.
pub fn anthropic_catalog_off(model_id: &str) -> Option<Option<bool>> {
    ANTHROPIC_CATALOG.iter().find(|(id, _)| *id == model_id).map(|(_, off)| *off)
}

/// `countTokens(text)`: the regex `/\w+|[^\w\s]|\s+/g` over UTF-16 code
/// units (JS `\w` is ASCII-only, `\s` is the JS whitespace set). Each
/// `\w+` run longer than 4 adds `floor((len - 1) / 4)`; the CJK branch only
/// ever sees single-code-unit tokens, so it never changes the count.
pub fn count_tokens(text: &str) -> f64 {
    #[derive(PartialEq, Clone, Copy)]
    enum Run { None, Word, Space }
    let mut total = 0f64;
    let mut run = Run::None;
    let mut word_len = 0usize;
    let flush_word = |len: usize, total: &mut f64| {
        if len > 4 { *total += ((len - 1) / 4) as f64; }
    };
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            if run != Run::Word {
                run = Run::Word;
                word_len = 0;
                total += 1.0;
            }
            word_len += 1;
        } else {
            if run == Run::Word { flush_word(word_len, &mut total); }
            if is_js_whitespace(ch) {
                if run != Run::Space { total += 1.0; }
                run = Run::Space;
            } else {
                // One token per UTF-16 code unit (astral chars are two).
                total += ch.len_utf16() as f64;
                run = Run::None;
            }
        }
    }
    if run == Run::Word { flush_word(word_len, &mut total); }
    total
}

/// `formatBase64(data, media_type)`.
pub fn format_base64(data: Option<&Value>, media_type: Option<&Value>) -> Result<String, EngineError> {
    let data = match data {
        Some(Value::String(s)) => {
            let mut d = s.as_str();
            if d.contains("base64") {
                d = d.rsplit("base64").next().unwrap_or("");
                d = d.strip_prefix(',').unwrap_or(d);
            }
            d.to_owned()
        }
        // Arrays have `.includes`; without a 'base64' element they reach the
        // template literal, otherwise `.split` is not a function.
        Some(Value::Array(items)) if !items.iter().any(|i| i.as_str() == Some("base64")) => to_string(data),
        Some(Value::Array(_)) => return Err(EngineError::new("data.split is not a function.")),
        None | Some(Value::Null) => return Err(type_error(data, "data.includes")),
        Some(_) => return Err(EngineError::new("data.includes is not a function.")),
    };
    Ok(format!("data:{};base64,{data}", to_string(media_type)))
}

/// JS `WhiteSpace` + `LineTerminator` (`\s`, `trim`).
pub fn is_js_whitespace(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\u{0B}' | '\u{0C}' | '\r' | ' ' | '\u{A0}' | '\u{1680}' | '\u{2000}'..='\u{200A}' | '\u{2028}' | '\u{2029}' | '\u{202F}' | '\u{205F}' | '\u{3000}' | '\u{FEFF}')
}

pub fn js_trim(s: &str) -> &str { s.trim_matches(is_js_whitespace) }
pub fn js_trim_start(s: &str) -> &str { s.trim_start_matches(is_js_whitespace) }

/// `a || 0` (owned).
pub fn or_zero(v: Option<&Value>) -> Value {
    if crate::engine::js::truthy(v) { v.cloned().unwrap_or(Value::Null) } else { Value::from(0) }
}

/// A JS number result as JSON (`NaN`/`Infinity` → `null`, like `JSON.stringify`).
pub fn js_num(n: f64) -> Value {
    if n.is_finite() { crate::engine::js::num(n) } else { Value::Null }
}

/// JS binary `+` over JSON values: string concatenation when either side
/// is (or primitive-converts to) a string, numeric addition otherwise.
pub fn js_plus(a: &Value, b: &Value) -> Value {
    let stringy = |v: &Value| matches!(v, Value::String(_) | Value::Array(_) | Value::Object(_));
    if stringy(a) || stringy(b) {
        return Value::String(format!("{}{}", to_string(Some(a)), to_string(Some(b))));
    }
    match (to_number(Some(a)), to_number(Some(b))) {
        (Some(x), Some(y)) => js_num(x + y),
        _ => Value::Null,
    }
}

/// `Math.max(0, a - b)` (NaN → `null`).
pub fn js_max0_sub(a: &Value, b: &Value) -> Value {
    match (to_number(Some(a)), to_number(Some(b))) {
        (Some(x), Some(y)) if !(x - y).is_nan() => js_num(0f64.max(x - y)),
        _ => Value::Null,
    }
}

/// JS `===` for JSON values (numbers compare numerically).
pub fn strict_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        (Value::Object(_), _) | (Value::Array(_), _) => false,
        _ => a == b,
    }
}

/// Bun/JSC TypeError for a property read on `null`/`undefined`.
pub fn type_error(v: Option<&Value>, expr: &str) -> EngineError {
    let what = if v.is_none() { "undefined" } else { "null" };
    EngineError::new(format!("{what} is not an object (evaluating '{expr}')"))
}

/// TypeError for `for…of` / `.map` over a non-iterable value.
pub fn not_iterable(expr: &str) -> EngineError { EngineError::new(format!("{expr} is not iterable")) }

/// `JSON.parse(v)` with JS string coercion; SyntaxError on failure.
pub fn json_parse(v: Option<&Value>) -> Result<Value, EngineError> {
    let text = match v { Some(Value::String(s)) => s.clone(), other => to_string(other) };
    serde_json::from_str(&text).map_err(|e| EngineError::new(format!("JSON Parse error: {e}")))
}

/// `JSON.stringify(v)` where `undefined` stays `undefined`.
pub fn stringify_opt(v: Option<&Value>) -> Option<Value> {
    v.map(|v| Value::String(crate::engine::js::stringify(v)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn count_tokens_matches_js() {
        assert_eq!(count_tokens(""), 0.0);
        // "Hello" (1 + floor(4/4)=1), " ", "world" (2), "!" → 6
        assert_eq!(count_tokens("Hello world!"), 6.0);
        assert_eq!(count_tokens("a  b"), 3.0);
        assert_eq!(count_tokens("你好"), 2.0);
        assert_eq!(count_tokens("😀"), 2.0);
        assert_eq!(count_tokens("internationalization"), 1.0 + 4.0);
    }

    #[test]
    fn reasoning_helpers() {
        assert_eq!(normalize_effort(Some(&json!(" MAXIMUM "))), Some("max"));
        assert_eq!(normalize_effort(Some(&json!(3))), None);
        assert_eq!(clamp_effort_to_window("minimal", Some("low"), Some("max")), "low");
        assert_eq!(split_reasoning_suffix_alias("claude-opus-5:off"), "claude-opus-5");
        assert_eq!(split_reasoning_suffix_alias("a:b"), "a:b");
        assert_eq!(get_think_level(1024.0), "low");
        assert_eq!(get_api_base_type(" Responses:lite "), "responses");
        assert_eq!(format_base64(Some(&json!("data:image/png;base64,QUJD")), Some(&json!("image/png"))).unwrap(), "data:image/png;base64,QUJD");
        assert_eq!(format_base64(Some(&json!("QUJD")), None).unwrap(), "data:undefined;base64,QUJD");
        assert_eq!(js_plus(&json!(1), &json!(2)), json!(3));
        assert_eq!(js_plus(&json!("1"), &json!(2)), json!("12"));
    }
}
