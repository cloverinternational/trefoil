//! `transformers/anthropic/thinking-clamp.ts`.
use super::helpers::{anthropic_catalog_off, clamp_effort_to_window, js_trim, normalize_effort, split_reasoning_suffix_alias};
use crate::engine::js::{truthy, Obj};
use serde_json::{json, Value};

/// Whether an Anthropic model supports disabling thinking.
///
/// Models like Claude Opus 5 and Claude Fable 5 have
/// `thinkingLevelMap.off === null`, meaning thinking cannot be disabled. For
/// these models any client attempt to turn thinking off (e.g.
/// `thinking: { type: 'disabled' }` or `output_config: { effort: 'off' }`)
/// must be clamped to the model's lowest supported thinking mode: adaptive
/// thinking with low effort.
pub fn anthropic_model_supports_disable(model_id: Option<&str>) -> bool {
    let Some(model_id) = model_id.filter(|m| !m.is_empty()) else { return true };
    let clean = js_trim(model_id).to_lowercase();
    let clean = split_reasoning_suffix_alias(&clean);
    let clean = clean.strip_prefix("anthropic/").unwrap_or(clean);
    let base = strip_date_suffix(clean);
    let model = anthropic_catalog_off(clean).or_else(|| anthropic_catalog_off(base));
    if let Some(Some(off_supported)) = model { return off_supported; }
    if clean.contains("opus-5") || clean.contains("fable-5") { return false; }
    true
}

/// `id.replace(/-\d{8}$/, '')`.
fn strip_date_suffix(id: &str) -> &str {
    let b = id.as_bytes();
    if b.len() >= 9 && b[b.len() - 9] == b'-' && b[b.len() - 8..].iter().all(u8::is_ascii_digit) { &id[..id.len() - 9] } else { id }
}

fn is_object(v: Option<&Value>) -> bool { truthy(v) && matches!(v, Some(Value::Object(_) | Value::Array(_))) }

/// Clamp Anthropic thinking and effort values to valid ranges.
///
/// Anthropic's Messages API accepts only `low`, `medium`, `high`, `xhigh`,
/// `max` for `output_config.effort` and rejects `minimal`/`off` with a 400.
/// 1. Clamps any `output_config.effort` to `['low', 'max']`.
/// 2. If a model cannot disable thinking, maps disabled thinking or effort
///    `off` to adaptive thinking with effort `low`.
/// 3. If a model can disable thinking and effort is `off`, removes
///    `output_config.effort` and sets `thinking: { type: 'disabled' }`.
/// 4. Ensures `output_config.effort` is not sent with disabled thinking.
///
/// Non-object payloads are returned untouched.
pub fn clamp_anthropic_effort_and_thinking(mut payload: Value, model_id: Option<&str>) -> Value {
    let Some(map) = payload.as_object_mut() else { return payload };

    let raw_model = map.get("model").and_then(Value::as_str).map(js_trim).filter(|m| !m.is_empty()).map(str::to_owned);
    let model = raw_model.as_deref().or(model_id);
    let supports_disable = anthropic_model_supports_disable(model);

    let has_output_config = is_object(map.get("output_config"));
    let output_config: Option<Obj> = if has_output_config { Some(spread_obj(&map["output_config"])) } else { None };
    let raw_effort = if has_output_config { map["output_config"].get("effort").cloned() } else { None };
    let has_effort = raw_effort.is_some();
    let normalized = if has_effort { normalize_effort(raw_effort.as_ref()) } else { None };

    let thinking = if is_object(map.get("thinking")) { map.get("thinking").cloned() } else { None };
    let thinking_disabled = thinking.as_ref().and_then(|t| t.get("type")).and_then(Value::as_str) == Some("disabled");

    let clamped = || normalized.map(|n| clamp_effort_to_window(n, Some("low"), Some("max"))).unwrap_or("low");

    if !supports_disable {
        // Model cannot disable thinking (e.g. Claude Opus 5, Fable 5)
        if thinking_disabled || normalized == Some("off") {
            let mut t = Obj::new();
            t.insert("type".into(), json!("adaptive"));
            if let Some(display) = thinking.as_ref().and_then(|t| t.get("display")).filter(|d| d.is_string()) {
                t.insert("display".into(), display.clone());
            }
            map.insert("thinking".into(), Value::Object(t));
            let mut oc = output_config.unwrap_or_default();
            oc.insert("effort".into(), json!("low"));
            map.insert("output_config".into(), Value::Object(oc));
        } else if has_effort && has_output_config {
            map["output_config"]["effort"] = json!(clamped());
        }
    } else if normalized == Some("off") {
        if has_output_config { remove_effort(map); }
        map.insert("thinking".into(), json!({"type": "disabled"}));
    } else if thinking_disabled {
        if has_output_config && truthy(map["output_config"].get("effort")) { remove_effort(map); }
    } else if has_effort && has_output_config {
        map["output_config"]["effort"] = json!(clamped());
    }

    payload
}

/// `{...v}` for an object (arrays spread their indices).
fn spread_obj(v: &Value) -> Obj {
    match v {
        Value::Object(m) => m.clone(),
        Value::Array(items) => items.iter().enumerate().map(|(i, x)| (i.to_string(), x.clone())).collect(),
        _ => Obj::new(),
    }
}

/// `delete outputConfig.effort; if (!Object.keys(outputConfig).length) delete payload.output_config`.
fn remove_effort(map: &mut Obj) {
    let empty = match map.get_mut("output_config") {
        Some(Value::Object(oc)) => { oc.shift_remove("effort"); oc.is_empty() }
        Some(Value::Array(items)) => items.is_empty(),
        _ => false,
    };
    if empty { map.shift_remove("output_config"); }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clamp(v: Value) -> Value { clamp_anthropic_effort_and_thinking(v, None) }

    #[test]
    fn supports_disable() {
        for m in ["claude-opus-5", "claude-opus-5-20260301", "anthropic/claude-opus-5", "claude-opus-5:off", "claude-fable-5", "claude-fable-5-1", "custom-opus-5", "claude-sonnet-5-5", "claude-haiku-5-5"] {
            assert!(!anthropic_model_supports_disable(Some(m)), "{m}");
        }
        for m in ["claude-sonnet-4-6", "claude-3-7-sonnet", "claude-haiku-4-5", "claude-opus-4-6", "claude-sonnet-5"] {
            assert!(anthropic_model_supports_disable(Some(m)), "{m}");
        }
        assert!(anthropic_model_supports_disable(None));
    }

    #[test]
    fn cannot_disable_models() {
        let r = clamp(json!({"model": "claude-opus-5", "thinking": {"type": "disabled"}}));
        assert_eq!(r, json!({"model": "claude-opus-5", "thinking": {"type": "adaptive"}, "output_config": {"effort": "low"}}));
        let r = clamp(json!({"model": "claude-opus-5", "output_config": {"effort": "off"}}));
        assert_eq!(r["thinking"], json!({"type": "adaptive"}));
        assert_eq!(r["output_config"], json!({"effort": "low"}));
        let r = clamp(json!({"model": "claude-opus-5", "thinking": {"type": "adaptive"}, "output_config": {"effort": "minimal"}}));
        assert_eq!(r["thinking"], json!({"type": "adaptive"}));
        assert_eq!(r["output_config"], json!({"effort": "low"}));
        for e in ["low", "medium", "high", "max"] {
            let r = clamp(json!({"model": "claude-opus-5", "thinking": {"type": "adaptive"}, "output_config": {"effort": e}}));
            assert_eq!(r["output_config"], json!({"effort": e}));
        }
        let r = clamp(json!({"model": "claude-opus-5", "thinking": {"type": "disabled", "display": "raw"}}));
        assert_eq!(r["thinking"], json!({"type": "adaptive", "display": "raw"}));
        let r = clamp(json!({"model": "claude-opus-5", "output_config": {"effort": "turbo"}}));
        assert_eq!(r["output_config"], json!({"effort": "low"}));
    }

    #[test]
    fn can_disable_models() {
        let r = clamp(json!({"model": "claude-sonnet-4-6", "output_config": {"effort": "minimal"}}));
        assert_eq!(r["output_config"], json!({"effort": "low"}));
        let r = clamp(json!({"model": "claude-sonnet-4-6", "output_config": {"effort": "off"}}));
        assert_eq!(r, json!({"model": "claude-sonnet-4-6", "thinking": {"type": "disabled"}}));
        let r = clamp(json!({"model": "claude-sonnet-4-6", "thinking": {"type": "disabled"}, "output_config": {"effort": "high"}}));
        assert_eq!(r, json!({"model": "claude-sonnet-4-6", "thinking": {"type": "disabled"}}));
        let r = clamp(json!({"model": "claude-sonnet-4-6", "thinking": {"type": "disabled"}}));
        assert_eq!(r, json!({"model": "claude-sonnet-4-6", "thinking": {"type": "disabled"}}));
        let r = clamp(json!({"model": "claude-sonnet-4-6", "output_config": {"effort": "high"}}));
        assert_eq!(r["output_config"], json!({"effort": "high"}));
        let r = clamp(json!({"model": "claude-sonnet-4-6", "output_config": {"effort": "MINIMAL"}}));
        assert_eq!(r["output_config"], json!({"effort": "low"}));
        // Other output_config members survive and keep their order.
        let r = clamp(json!({"model": "claude-sonnet-4-6", "output_config": {"effort": "off", "format": 1}}));
        assert_eq!(crate::engine::js::stringify(&r), r#"{"model":"claude-sonnet-4-6","output_config":{"format":1},"thinking":{"type":"disabled"}}"#);
    }

    #[test]
    fn passthrough_guards() {
        assert_eq!(clamp(Value::Null), Value::Null);
        assert_eq!(clamp(json!("string")), json!("string"));
    }
}
