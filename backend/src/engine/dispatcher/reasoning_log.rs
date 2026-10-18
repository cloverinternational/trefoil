//! `getReasoningLogValue` from `services/pi-ai/reasoning.ts` (usage log
//! `reasoningEffort`).
use crate::engine::dispatch::pi_ai::{budget_to_effort, normalize_effort, split_reasoning_suffix};
use crate::engine::js::*;
use serde_json::Value;

#[derive(Default)]
struct Intent<'a> {
    effort: Option<&'a Value>,
    budget_tokens: Option<&'a Value>,
    enabled: Option<Value>,
    adaptive: Option<Value>,
}

/// `reasoningIntentToLogValue`.
fn intent_to_log_value(i: &Intent<'_>) -> Option<String> {
    if i.enabled == Some(Value::Bool(false)) { return Some("off".into()); }
    let raw = i.effort.and_then(Value::as_str).map(|s| s.trim().to_lowercase());
    if raw.as_deref() == Some("auto") { return Some("auto".into()); }
    if let Some(e) = normalize_effort(i.effort) { return Some(e.into()); }
    if let Some(Value::Number(n)) = i.budget_tokens {
        let b = n.as_f64().unwrap_or(f64::NAN);
        if b == -1.0 { return Some("auto".into()); }
        return Some(budget_to_effort(b).into());
    }
    if i.adaptive == Some(Value::Bool(true)) || i.enabled == Some(Value::Bool(true)) { return Some("on".into()); }
    None
}

/// `getReasoningLogValue(request, payload)`.
pub fn get_reasoning_log_value(request: Option<&Value>, payload: &Value) -> Option<String> {
    let empty = Value::Object(Default::default());
    let source = if payload.is_object() || payload.is_array() { payload } else { &empty };
    let mut generic_on = false;
    let mut resolve = |v: Option<String>| -> Option<String> {
        match v.as_deref() {
            Some("on") => { generic_on = true; None }
            Some("off") if generic_on => None,
            _ => v,
        }
    };
    if let Some(thinking) = source.get("thinking").filter(|t| truthy(Some(t)) && (t.is_object() || t.is_array())) {
        let kind = thinking.get("type").and_then(Value::as_str).map(str::to_lowercase);
        let v = resolve(intent_to_log_value(&Intent {
            effort: get(source, &["output_config", "effort"]),
            budget_tokens: thinking.get("budget_tokens"),
            enabled: match kind.as_deref() { Some("disabled") => Some(Value::Bool(false)), Some(_) => Some(Value::Bool(true)), None => None },
            adaptive: Some(Value::Bool(kind.as_deref() == Some("adaptive"))),
        }));
        if v.is_some() { return v; }
    }
    if let Some(r) = source.get("reasoning").filter(|t| truthy(Some(t)) && (t.is_object() || t.is_array())) {
        let effort = if nullish(r.get("effort")) { r.get("level") } else { r.get("effort") };
        let v = resolve(intent_to_log_value(&Intent { effort, budget_tokens: r.get("max_tokens"), enabled: r.get("enabled").cloned(), adaptive: r.get("adaptive").cloned() }));
        if v.is_some() { return v; }
    }
    let chat_raw = if nullish(source.get("reasoning_effort")) { source.get("reasoning_level") } else { source.get("reasoning_effort") };
    if let Some(s) = chat_raw.and_then(Value::as_str) {
        let lower = s.trim().to_lowercase();
        if lower == "auto" { return Some("auto".into()); }
        if let Some(e) = normalize_effort(Some(&Value::String(lower))) { return Some(e.into()); }
    }
    if let Some(tc) = get(source, &["generationConfig", "thinkingConfig"]).filter(|t| truthy(Some(t)) && (t.is_object() || t.is_array())) {
        let budget = if nullish(tc.get("thinkingBudget")) { tc.get("thinking_budget") } else { tc.get("thinkingBudget") };
        let level = if nullish(tc.get("thinkingLevel")) { tc.get("thinking_level") } else { tc.get("thinkingLevel") };
        let enabled = if budget.and_then(Value::as_f64) == Some(0.0) { Some(Value::Bool(false)) }
            else if tc.get("includeThoughts") == Some(&Value::Bool(true)) || tc.get("include_thoughts") == Some(&Value::Bool(true)) { Some(Value::Bool(true)) }
            else { None };
        let v = resolve(intent_to_log_value(&Intent { effort: level, budget_tokens: budget, enabled, adaptive: None }));
        if v.is_some() { return v; }
    }
    let effort = { let e = get(source, &["output_config", "effort"]); if nullish(e) { source.get("thinking_level") } else { e } };
    let budget = if nullish(source.get("budget_tokens")) { source.get("thinking_budget") } else { source.get("budget_tokens") };
    let enabled = if nullish(source.get("enable_thinking")) { get(source, &["chat_template_kwargs", "enable_thinking"]).cloned() } else { source.get("enable_thinking").cloned() };
    let v = resolve(intent_to_log_value(&Intent { effort, budget_tokens: budget, enabled, adaptive: None }));
    if v.is_some() { return v; }
    if let Some(model) = source.get("model").and_then(Value::as_str) {
        if let (_, Some(intent)) = split_reasoning_suffix(model) {
            let effort = intent.effort.map(|e| Value::String(e.into()));
            let budget = intent.budget_tokens.map(num);
            let v = resolve(intent_to_log_value(&Intent { effort: effort.as_ref(), budget_tokens: budget.as_ref(), enabled: intent.enabled.map(Value::Bool), adaptive: Some(Value::Bool(intent.adaptive)) }));
            if v.is_some() { return v; }
        }
    }
    let r = request.and_then(|r| r.get("reasoning"));
    let v = resolve(intent_to_log_value(&Intent {
        effort: r.and_then(|r| r.get("effort")),
        budget_tokens: r.and_then(|r| r.get("max_tokens")),
        enabled: r.and_then(|r| r.get("enabled")).cloned(),
        adaptive: r.and_then(|r| r.get("adaptive")).cloned(),
    }));
    if v.is_some() { return v; }
    generic_on.then(|| "on".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn log_values() {
        assert_eq!(get_reasoning_log_value(None, &json!({"reasoning_effort": "High"})).as_deref(), Some("high"));
        assert_eq!(get_reasoning_log_value(None, &json!({"thinking": {"type": "enabled"}, "reasoning_effort": "low"})).as_deref(), Some("low"));
        assert_eq!(get_reasoning_log_value(None, &json!({"thinking": {"type": "enabled"}})).as_deref(), Some("on"));
        assert_eq!(get_reasoning_log_value(None, &json!({"thinking": {"type": "enabled", "budget_tokens": 2000}})).as_deref(), Some("low"));
        assert_eq!(get_reasoning_log_value(None, &json!({"model": "m:high"})).as_deref(), Some("high"));
        assert_eq!(get_reasoning_log_value(None, &json!({"model": "m"})), None);
    }
}
