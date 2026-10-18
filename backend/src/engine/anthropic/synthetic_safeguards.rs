//! `transformers/anthropic/synthetic-safeguards.ts`.
//!
//! Synthetic Claude Code auto-mode `safeguard_results` for translated
//! (non-native Messages) model aliases. When an inbound `/v1/messages`
//! request carries `safeguards` but the alias routes to a
//! Responses/Chat/Gemini target, there is no upstream classifier verdict to
//! relay. With the alias-level `synthetic_safeguard_approval` toggle enabled
//! (opt-in, default off), Plexus answers with a synthetic
//! `evaluated`/`not_flagged` verdict per observed tool use plus an
//! `explanation` stating no real classifier ran — no evaluation is performed.
use super::helpers::{get_api_base_type, js_trim};
use crate::engine::js::{get, get_arr};
use serde_json::{json, Map, Value};

pub const SYNTHETIC_SAFEGUARD_EXPLANATION: &str = "Synthetic Plexus approval; no classifier evaluation was performed.";
pub const SYNTHETIC_SAFEGUARD_TYPE: &str = "dangerous_tool_use";

/// `getRequestedSafeguardTypes(originalBody)`.
pub fn get_requested_safeguard_types(original_body: Option<&Value>) -> Vec<String> {
    let Some(body) = original_body.filter(|b| b.is_object() || b.is_array()) else { return vec![] };
    let Some(safeguards) = body.get("safeguards").and_then(Value::as_array) else { return vec![] };
    safeguards.iter()
        .filter(|e| e.is_object() || e.is_array())
        .filter_map(|e| e.get("type").and_then(Value::as_str))
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
        .collect()
}

/// `buildSyntheticSafeguardResults(toolIds, requestedTypes)`.
pub fn build_synthetic_safeguard_results(tool_ids: &[String], requested_types: &[String]) -> Vec<Value> {
    let mut tool_uses = Map::new();
    for id in tool_ids.iter().filter(|id| !id.is_empty()) {
        if tool_uses.contains_key(id) { continue; }
        tool_uses.insert(id.clone(), json!({"type": "evaluated", "outcome": "not_flagged", "explanation": SYNTHETIC_SAFEGUARD_EXPLANATION}));
    }
    requested_types.iter().map(|t| json!({"type": t, "status": {"type": "available", "tool_uses": tool_uses}})).collect()
}

/// `SafeguardSynthesisGate`.
#[derive(Debug, Clone, Default)]
pub struct SafeguardSynthesisGate<'a> {
    pub incoming_api_type: Option<&'a str>,
    pub original_body: Option<&'a Value>,
    pub alias_toggle: bool,
    pub outgoing_api_type: Option<&'a str>,
    pub bypass_transformation: bool,
    pub has_client_error: bool,
    pub has_existing_results: bool,
}

/// `shouldSynthesizeSafeguards(gate)`.
pub fn should_synthesize_safeguards(gate: &SafeguardSynthesisGate) -> bool {
    if gate.incoming_api_type.unwrap_or("").to_lowercase() != "messages" { return false; }
    if !gate.alias_toggle { return false; }
    if gate.bypass_transformation { return false; }
    if gate.has_client_error { return false; }
    if gate.has_existing_results { return false; }
    // The outgoing target must be known and translated: a missing/empty type
    // means routing metadata was lost, so refuse to synthesize rather than
    // risk approving on an unverified path. Native Messages targets relay the
    // upstream verdict verbatim; never synthesize.
    let Some(outgoing) = gate.outgoing_api_type.filter(|t| !js_trim(t).is_empty()) else { return false };
    if get_api_base_type(outgoing) == "messages" { return false; }
    !get_requested_safeguard_types(gate.original_body).is_empty()
}

/// `resolveSyntheticSafeguardToggle(canonicalModel)`: `models` is
/// `getConfig().models` (the alias map). Default off.
pub fn resolve_synthetic_safeguard_toggle(canonical_model: Option<&str>, models: Option<&Value>) -> bool {
    let Some(name) = canonical_model.filter(|m| !m.is_empty()) else { return false };
    models.and_then(|m| get(m, &[name, "synthetic_safeguard_approval"])) == Some(&Value::Bool(true))
}

fn push_tool_ids(ids: &mut Vec<String>, calls: &[Value]) {
    for call in calls {
        if let Some(id) = call.get("id").and_then(Value::as_str).filter(|id| !id.is_empty()) {
            if !ids.iter().any(|x| x == id) { ids.push(id.to_owned()); }
        }
    }
}

/// `collectUnifiedToolIds(toolCalls)`.
pub fn collect_unified_tool_ids(tool_calls: Option<&Value>) -> Vec<String> {
    let mut ids = Vec::new();
    if let Some(calls) = tool_calls.and_then(Value::as_array) { push_tool_ids(&mut ids, calls); }
    ids
}

/// `collectStreamToolIds(chunks)`.
pub fn collect_stream_tool_ids(chunks: &[Value]) -> Vec<String> {
    let mut ids = Vec::new();
    for chunk in chunks {
        if let Some(calls) = get_arr(chunk, &["delta", "tool_calls"]) { push_tool_ids(&mut ids, calls); }
    }
    ids
}

/// `wrapUnifiedStreamWithSyntheticSafeguards`: feed every unified chunk
/// through [`SyntheticSafeguardStream::process`] so the terminal
/// `finish_reason` chunk carries synthetic `safeguard_results`. Tool IDs are
/// collected from `delta.tool_calls` as they pass; error chunks never
/// synthesize.
#[derive(Debug, Clone)]
pub struct SyntheticSafeguardStream {
    requested_types: Vec<String>,
    tool_ids: Vec<String>,
    /// Set once ANY chunk carries a real upstream `safeguard_results`: a
    /// later terminal chunk without results must not overwrite it.
    seen_real_results: bool,
}

impl SyntheticSafeguardStream {
    pub fn new(requested_types: Vec<String>) -> Self { Self { requested_types, tool_ids: Vec::new(), seen_real_results: false } }

    pub fn process(&mut self, chunk: &mut Value) {
        if !(chunk.is_object() || chunk.is_array()) { return; }
        if chunk.get("safeguard_results").is_some() { self.seen_real_results = true; }
        if let Some(calls) = get_arr(chunk, &["delta", "tool_calls"]) { push_tool_ids(&mut self.tool_ids, calls); }
        let terminal = chunk.get("finish_reason").and_then(Value::as_str).is_some_and(|f| !f.is_empty());
        let is_error = chunk.get("event").and_then(Value::as_str) == Some("error");
        if !self.seen_real_results && terminal && !is_error && chunk.get("safeguard_results").is_none() {
            if let Some(map) = chunk.as_object_mut() {
                map.insert("safeguard_results".into(), Value::Array(build_synthetic_safeguard_results(&self.tool_ids, &self.requested_types)));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body() -> Value {
        json!({"model": "luna-alias", "messages": [{"role": "user", "content": "hi"}], "safeguards": [{"type": "dangerous_tool_use", "classifier_context": {"v": 1}}]})
    }

    #[test]
    fn requested_types_and_results() {
        assert_eq!(get_requested_safeguard_types(Some(&body())), vec!["dangerous_tool_use"]);
        assert!(get_requested_safeguard_types(Some(&json!({"model": "x"}))).is_empty());
        assert!(get_requested_safeguard_types(Some(&Value::Null)).is_empty());
        let r = build_synthetic_safeguard_results(&["toolu_a".into(), "toolu_b".into(), "toolu_a".into(), "".into()], &["dangerous_tool_use".into()]);
        assert_eq!(crate::engine::js::stringify(&r[0]), format!(
            r#"{{"type":"dangerous_tool_use","status":{{"type":"available","tool_uses":{{"toolu_a":{{"type":"evaluated","outcome":"not_flagged","explanation":"{e}"}},"toolu_b":{{"type":"evaluated","outcome":"not_flagged","explanation":"{e}"}}}}}}}}"#,
            e = SYNTHETIC_SAFEGUARD_EXPLANATION));
        assert_eq!(build_synthetic_safeguard_results(&[], &["dangerous_tool_use".into()])[0]["status"]["tool_uses"], json!({}));
        assert_eq!(collect_unified_tool_ids(Some(&json!([{"id": "a"}, {"id": "a"}, {"id": ""}]))), vec!["a"]);
        assert_eq!(collect_stream_tool_ids(&[json!({"delta": {"tool_calls": [{"id": "a"}]}}), json!({"delta": {"tool_calls": [{"id": "b"}]}})]), vec!["a", "b"]);
    }

    #[test]
    fn gate() {
        let b = body();
        let base = SafeguardSynthesisGate { incoming_api_type: Some("messages"), original_body: Some(&b), alias_toggle: true, outgoing_api_type: Some("responses"), ..Default::default() };
        assert!(should_synthesize_safeguards(&base));
        assert!(!should_synthesize_safeguards(&SafeguardSynthesisGate { alias_toggle: false, ..base.clone() }));
        let other = json!({"model": "x"});
        assert!(!should_synthesize_safeguards(&SafeguardSynthesisGate { original_body: Some(&other), ..base.clone() }));
        assert!(!should_synthesize_safeguards(&SafeguardSynthesisGate { outgoing_api_type: Some("messages"), ..base.clone() }));
        assert!(!should_synthesize_safeguards(&SafeguardSynthesisGate { bypass_transformation: true, ..base.clone() }));
        assert!(!should_synthesize_safeguards(&SafeguardSynthesisGate { has_client_error: true, ..base.clone() }));
        assert!(!should_synthesize_safeguards(&SafeguardSynthesisGate { has_existing_results: true, ..base.clone() }));
        assert!(!should_synthesize_safeguards(&SafeguardSynthesisGate { incoming_api_type: Some("chat"), ..base.clone() }));
        for t in [None, Some(""), Some("   "), Some("MESSAGES")] {
            assert!(!should_synthesize_safeguards(&SafeguardSynthesisGate { outgoing_api_type: t, ..base.clone() }));
        }
        assert!(should_synthesize_safeguards(&SafeguardSynthesisGate { outgoing_api_type: Some("responses:lite"), ..base.clone() }));
    }

    #[test]
    fn toggle() {
        let models = json!({"a": {"synthetic_safeguard_approval": true}, "b": {"synthetic_safeguard_approval": "true"}});
        assert!(resolve_synthetic_safeguard_toggle(Some("a"), Some(&models)));
        assert!(!resolve_synthetic_safeguard_toggle(Some("b"), Some(&models)));
        assert!(!resolve_synthetic_safeguard_toggle(None, Some(&models)));
    }

    #[test]
    fn stream_wrapper() {
        let real = Value::Array(build_synthetic_safeguard_results(&["toolu_a".into()], &["dangerous_tool_use".into()]));
        let mut w = SyntheticSafeguardStream::new(vec!["dangerous_tool_use".into()]);
        let mut c1 = json!({"id": "m", "delta": {"tool_calls": [{"id": "toolu_a"}]}, "safeguard_results": real});
        let mut c2 = json!({"id": "m", "delta": {}, "finish_reason": "tool_calls"});
        w.process(&mut c1);
        w.process(&mut c2);
        assert_eq!(c1["safeguard_results"], real);
        assert!(c2.get("safeguard_results").is_none());

        let mut w = SyntheticSafeguardStream::new(vec!["dangerous_tool_use".into()]);
        let mut c1 = json!({"id": "m", "delta": {"tool_calls": [{"id": "toolu_x"}]}});
        let mut err = json!({"event": "error", "finish_reason": "error"});
        let mut c2 = json!({"id": "m", "delta": {}, "finish_reason": "tool_calls"});
        w.process(&mut c1);
        w.process(&mut err);
        w.process(&mut c2);
        assert!(err.get("safeguard_results").is_none());
        assert_eq!(c2["safeguard_results"][0]["status"]["tool_uses"]["toolu_x"]["outcome"], json!("not_flagged"));
    }
}
