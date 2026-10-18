//! `services/models/model-behaviors.ts`: alias-level `advanced` behaviors.
use super::util::del;
use crate::engine::js::*;
use serde_json::Value;

/// `applyModelBehaviors(payload, behaviors, { incomingApiType, canonicalModel })`.
/// Unknown behavior types are ignored (TS logs a warning).
pub fn apply_model_behaviors(payload: Value, behaviors: Option<&Value>, incoming_api_type: &str, _canonical_model: &str) -> Value {
    let Some(list) = behaviors.and_then(Value::as_array).filter(|b| !b.is_empty()) else { return payload };
    let mut result = payload.as_object().cloned().unwrap_or_default();
    for behavior in list {
        if behavior.get("enabled") == Some(&Value::Bool(false)) { continue; }
        if get_str(behavior, &["type"]) == Some("strip_adaptive_thinking") && incoming_api_type == "messages" {
            if let Some(thinking) = result.get("thinking").filter(|t| t.is_object()) {
                if get_str(thinking, &["type"]) == Some("adaptive") { del(&mut result, "thinking"); }
            }
        }
    }
    Value::Object(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn strip_adaptive() {
        let b = json!([{"type": "strip_adaptive_thinking"}]);
        let p = json!({"model": "m", "thinking": {"type": "adaptive"}, "x": 1});
        assert_eq!(apply_model_behaviors(p.clone(), Some(&b), "messages", "a"), json!({"model": "m", "x": 1}));
        assert_eq!(apply_model_behaviors(p.clone(), Some(&b), "chat", "a"), p);
        let disabled = json!([{"type": "strip_adaptive_thinking", "enabled": false}]);
        assert_eq!(apply_model_behaviors(p.clone(), Some(&disabled), "messages", "a"), p);
        let enabled = json!({"model": "m", "thinking": {"type": "enabled", "budget_tokens": 5}});
        assert_eq!(apply_model_behaviors(enabled.clone(), Some(&b), "messages", "a"), enabled);
    }
}
