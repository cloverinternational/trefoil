//! `services/dispatch/adapter-resolver.ts`.
use super::adapters::{adapter_by_name, is_muse_target, ResolvedAdapter};
use super::util::base_type;
use crate::engine::config::{is_oauth_placeholder_url, oauth_provider_of};
use crate::engine::js::*;
use crate::engine::routing::RouteResult;
use regex::Regex;
use serde_json::{json, Value};
use std::sync::OnceLock;

/// `resolveAdapters(route, effectiveApiType)`: implicit adapters, then
/// provider-level, then model-level `adapter` entries; `enabled: false`
/// removes earlier instances; unknown names are skipped.
pub fn resolve_adapters(route: &RouteResult, effective_api_type: Option<&str>) -> Vec<ResolvedAdapter> {
    let mut entries: Vec<Value> = resolve_implicit_adapters(route, effective_api_type);
    let arr = |v: Option<&Value>| v.and_then(Value::as_array).cloned().unwrap_or_default();
    entries.extend(arr(route.config.get("adapter")));
    entries.extend(arr(route.model_config.as_ref().and_then(|m| m.get("adapter"))));
    let mut resolved: Vec<ResolvedAdapter> = Vec::new();
    for entry in entries {
        let name = to_string(entry.get("name"));
        if entry.get("enabled") == Some(&Value::Bool(false)) {
            resolved.retain(|r| r.adapter.name() != name);
            continue;
        }
        let Some(adapter) = adapter_by_name(&name) else { continue };
        resolved.push(ResolvedAdapter { adapter, options: entry.get("options").cloned() });
    }
    resolved
}

fn implicit(name: &str) -> Value { json!({"name": name, "options": {}, "enabled": true}) }

fn resolve_implicit_adapters(route: &RouteResult, effective_api_type: Option<&str>) -> Vec<Value> {
    let mut out = Vec::new();
    if is_gpt5_model(&route.model) { out.push(implicit("suppress_unsupported_gpt5_options")); }
    if get_str(&route.config, &["pi_ai_provider"]) == Some("openrouter") { out.push(implicit("strip_unsupported_tool_search")); }
    if is_muse_target(route) { out.push(implicit("muse_code_compat")); }
    if let Some(api) = effective_api_type.filter(|a| !a.is_empty()) {
        if base_type(api) == "messages" && is_anthropic_target_provider(route, Some(api)) {
            out.push(implicit("normalize_anthropic_tool_ids"));
        }
    }
    out
}

/// `isAnthropicTargetProvider(route, effectiveApiType)`.
pub fn is_anthropic_target_provider(route: &RouteResult, effective_api_type: Option<&str>) -> bool {
    if route.config.get("useClaudeMasking") == Some(&Value::Bool(true)) { return true; }
    let lowered: Vec<String> = select_dispatch_urls(route.config.get("api_base_url"), effective_api_type).iter().map(|u| u.to_lowercase()).collect();
    if lowered.iter().any(|u| u.contains("anthropic.com")) { return true; }
    lowered.iter().any(|u| is_oauth_placeholder_url(u)) && oauth_provider_of(&route.provider, &route.config) == "anthropic"
}

/// `selectDispatchUrls(base, effectiveApiType)`.
pub fn select_dispatch_urls(base: Option<&Value>, effective_api_type: Option<&str>) -> Vec<String> {
    if let Some(Value::String(s)) = base { return vec![s.clone()]; }
    let empty = Obj::new();
    let map = base.and_then(Value::as_object).unwrap_or(&empty);
    let Some(api) = effective_api_type.filter(|a| !a.is_empty()) else {
        return map.values().filter_map(Value::as_str).map(str::to_owned).collect();
    };
    let key = api.to_lowercase();
    let selected = or(map.get(&key), map.get(&base_type(&key)));
    match selected { Some(Value::String(s)) if !s.is_empty() => vec![s], _ => vec![] }
}

/// `isGpt5Model(model)`.
pub fn is_gpt5_model(model: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)(?:^|/)gpt-5(?:[.-]|$)").expect("valid regex")).is_match(model)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(model: &str, config: Value, model_config: Option<Value>) -> RouteResult {
        RouteResult { provider: "p".into(), model: model.into(), config, model_config, incoming_model_alias: None, canonical_model: None }
    }
    fn names(r: &[ResolvedAdapter]) -> Vec<&'static str> { r.iter().map(|a| a.adapter.name()).collect() }

    #[test]
    fn gpt5_regex() {
        for m in ["gpt-5", "gpt-5.2", "gpt-5-mini", "openai/gpt-5.5", "GPT-5"] { assert!(is_gpt5_model(m), "{m}"); }
        for m in ["gpt-55", "chatgpt-5", "my-gpt-5x", "gpt-4o"] { assert!(!is_gpt5_model(m), "{m}"); }
    }

    #[test]
    fn resolution_order_and_tombstones() {
        let r = route("gpt-5", json!({
            "api_base_url": "https://api.anthropic.com/v1", "pi_ai_provider": "openrouter",
            "adapter": [{"name": "reasoning_content"}, {"name": "nope"}, {"name": "suppress_unsupported_gpt5_options", "enabled": false}]
        }), Some(json!({"adapter": [{"name": "model_override", "options": {"rules": []}}]})));
        assert_eq!(names(&resolve_adapters(&r, Some("messages"))), vec!["strip_unsupported_tool_search", "normalize_anthropic_tool_ids", "reasoning_content", "model_override"]);
        assert_eq!(names(&resolve_adapters(&r, None)), vec!["strip_unsupported_tool_search", "reasoning_content", "model_override"]);
        let plain = route("m", json!({"api_base_url": "https://x"}), None);
        assert!(resolve_adapters(&plain, Some("chat")).is_empty());
        let meta = route("m", json!({"api_base_url": {"responses": "https://api.meta.ai/v1"}}), None);
        assert_eq!(names(&resolve_adapters(&meta, Some("responses"))), vec!["muse_code_compat"]);
    }

    #[test]
    fn anthropic_target_scoping() {
        let r = route("m", json!({"api_base_url": {"chat": "https://api.anthropic.com/v1", "messages": "https://proxy.example/v1"}}), None);
        assert!(!is_anthropic_target_provider(&r, Some("messages")));
        assert!(is_anthropic_target_provider(&r, None));
        assert!(is_anthropic_target_provider(&r, Some("chat:x")));
        let mut o = route("m", json!({"api_base_url": "oauth://x", "oauth_provider": "anthropic"}), None);
        assert!(is_anthropic_target_provider(&o, Some("messages")));
        o.config = json!({"api_base_url": "oauth://x"});
        assert!(!is_anthropic_target_provider(&o, Some("messages")));
    }
}
