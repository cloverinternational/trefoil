//! `services/dispatch/responses-extensions.ts` plus the `@plexus/shared`
//! pieces it uses (`RESPONSES_EXTENSIONS`, `RESPONSES_LITE_EXTENSIONS`,
//! `getDefaultResponsesExtensions`).
use super::util::{base_type, hostname};
use crate::engine::config::api_format;
use crate::engine::js::*;
use crate::engine::routing::RouteResult;
use serde_json::Value;
use std::collections::BTreeSet;

pub const RESPONSES_EXTENSIONS: [&str; 7] =
    ["namespace_tools", "namespaced_calls", "dotted_calls", "custom_tools", "custom_calls", "additional_tools", "tool_search"];
/// Lite wire contract: everything but `namespace_tools` / `dotted_calls`.
pub const RESPONSES_LITE_EXTENSIONS: [&str; 5] = ["namespaced_calls", "custom_tools", "custom_calls", "additional_tools", "tool_search"];
const BASE_RESPONSES_EXTENSIONS: [&str; 1] = ["custom_tools"];
const META_RESPONSES_EXTENSIONS: [&str; 3] = ["namespace_tools", "namespaced_calls", "dotted_calls"];
const OPENAI_RESPONSES_EXTENSIONS: [&str; 4] = ["namespace_tools", "namespaced_calls", "custom_tools", "custom_calls"];
const CODEX_RESPONSES_EXTENSIONS: [&str; 6] =
    ["namespace_tools", "namespaced_calls", "custom_tools", "custom_calls", "additional_tools", "tool_search"];

/// Tool `type`s the responses:lite contract accepts in `tools`
/// (`function` + the lite extensions' declaration types).
pub const LITE_ALLOWED_TOOL_TYPES: [&str; 3] = ["function", "custom", "tool_search"];

fn responses_host(api_base_url: Option<&Value>) -> String {
    let url = match api_base_url {
        Some(Value::String(s)) => Some(s.as_str()),
        Some(other) => get_str(other, &["responses"]),
        None => None,
    };
    url.filter(|u| !u.is_empty()).and_then(hostname).unwrap_or_default()
}

/// `getDefaultResponsesExtensions({ oauthProvider, apiBaseUrl })`.
pub fn default_responses_extensions(oauth_provider: Option<&str>, api_base_url: Option<&Value>) -> Vec<&'static str> {
    if oauth_provider == Some("openai-codex") { return CODEX_RESPONSES_EXTENSIONS.to_vec(); }
    if oauth_provider == Some("meta") { return META_RESPONSES_EXTENSIONS.to_vec(); }
    match responses_host(api_base_url).as_str() {
        "api.meta.ai" => META_RESPONSES_EXTENSIONS.to_vec(),
        "api.openai.com" => OPENAI_RESPONSES_EXTENSIONS.to_vec(),
        _ => BASE_RESPONSES_EXTENSIONS.to_vec(),
    }
}

fn declares_custom_tool(tool: &Value) -> bool {
    if !tool.is_object() { return false; }
    match get_str(tool, &["type"]) {
        Some("custom") => true,
        Some("namespace") => get_arr(tool, &["tools"]).is_some_and(|t| t.iter().any(|s| get_str(s, &["type"]) == Some("custom"))),
        _ => false,
    }
}

fn declared_namespaces(body: &Value) -> BTreeSet<String> {
    let mut decls: Vec<&Value> = get_arr(body, &["tools"]).map(|t| t.iter().collect()).unwrap_or_default();
    if let Some(input) = get_arr(body, &["input"]) {
        for item in input {
            if get_str(item, &["type"]) == Some("additional_tools") {
                if let Some(tools) = get_arr(item, &["tools"]) { decls.extend(tools.iter()); }
            }
        }
    }
    decls.into_iter()
        .filter(|t| get_str(t, &["type"]) == Some("namespace"))
        .filter_map(|t| get_str(t, &["name"]).map(str::to_owned))
        .collect()
}

fn is_dotted_call(item: &Value, namespaces: &BTreeSet<String>) -> bool {
    let Some(name) = get_str(item, &["name"]) else { return false };
    match name.find('.') { Some(dot) if dot > 0 => namespaces.contains(&name[..dot]), _ => false }
}

/// `detectResponsesExtensions(body)`.
pub fn detect_responses_extensions(body: &Value) -> BTreeSet<&'static str> {
    let mut found = BTreeSet::new();
    if !body.is_object() { return found; }
    if let Some(tools) = get_arr(body, &["tools"]) {
        for tool in tools.iter().filter(|t| t.is_object()) {
            match get_str(tool, &["type"]) {
                Some("namespace") => { found.insert("namespace_tools"); }
                Some("tool_search") => { found.insert("tool_search"); }
                _ => {}
            }
            if declares_custom_tool(tool) { found.insert("custom_tools"); }
        }
    }
    if let Some(input) = get_arr(body, &["input"]) {
        let namespaces = declared_namespaces(body);
        for item in input {
            let Some(t) = get_str(item, &["type"]) else { continue };
            if t == "function_call" {
                if get_str(item, &["namespace"]).is_some() { found.insert("namespaced_calls"); }
                else if is_dotted_call(item, &namespaces) { found.insert("dotted_calls"); }
            } else if t == "custom_tool_call" || t == "custom_tool_call_output" {
                found.insert("custom_calls");
            } else if t == "additional_tools" {
                found.insert("additional_tools");
                if get_arr(item, &["tools"]).is_some_and(|ts| ts.iter().any(declares_custom_tool)) { found.insert("custom_tools"); }
            } else if t.starts_with("tool_search") {
                found.insert("tool_search");
            }
        }
    }
    found
}

/// `resolveSupportedResponsesExtensions(route, targetApiType)`.
pub fn resolve_supported_responses_extensions(route: &RouteResult, target_api_type: &str) -> BTreeSet<String> {
    if api_format::subtype(target_api_type).as_deref() == Some("lite") {
        return RESPONSES_LITE_EXTENSIONS.iter().map(|s| s.to_string()).collect();
    }
    if let Some(explicit) = route.config.get("responses_extensions").filter(|v| truthy(Some(v))) {
        return explicit.as_array().map(|a| a.iter().map(|v| to_string(Some(v))).collect()).unwrap_or_default();
    }
    default_responses_extensions(get_str(&route.config, &["oauth_provider"]), route.config.get("api_base_url"))
        .into_iter().map(str::to_owned).collect()
}

/// `hasUnsupportedResponsesExtensions(body, route, targetApiType)`.
pub fn has_unsupported_responses_extensions(body: Option<&Value>, route: &RouteResult, target_api_type: &str) -> bool {
    if base_type(target_api_type) != "responses" { return false; }
    let Some(body) = body else { return false };
    let detected = detect_responses_extensions(body);
    if detected.is_empty() { return false; }
    let supported = resolve_supported_responses_extensions(route, target_api_type);
    detected.iter().any(|e| !supported.contains(*e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn route(config: Value) -> RouteResult {
        RouteResult { provider: "p".into(), model: "m".into(), config, model_config: None, incoming_model_alias: None, canonical_model: None }
    }

    #[test]
    fn detection_and_support() {
        let body = json!({
            "tools": [{"type": "namespace", "name": "fs", "tools": [{"type": "custom"}]}, {"type": "function"}],
            "input": [{"type": "function_call", "name": "fs.read"}, {"type": "custom_tool_call"}, {"type": "tool_search_call"}]
        });
        let found: Vec<&str> = detect_responses_extensions(&body).into_iter().collect();
        assert_eq!(found, vec!["custom_calls", "custom_tools", "dotted_calls", "namespace_tools", "tool_search"]);
        let plain = route(json!({"api_base_url": "https://example.com/v1"}));
        assert!(has_unsupported_responses_extensions(Some(&body), &plain, "responses"));
        assert!(!has_unsupported_responses_extensions(Some(&json!({"tools": [{"type": "custom"}]})), &plain, "responses"));
        assert!(!has_unsupported_responses_extensions(Some(&body), &plain, "chat"));
        let meta = route(json!({"api_base_url": {"responses": "https://api.meta.ai/v1"}}));
        assert!(!has_unsupported_responses_extensions(Some(&json!({"tools": [{"type": "namespace", "name": "a"}]})), &meta, "responses"));
        let explicit = route(json!({"responses_extensions": []}));
        assert!(has_unsupported_responses_extensions(Some(&json!({"tools": [{"type": "custom"}]})), &explicit, "responses"));
        assert!(has_unsupported_responses_extensions(Some(&json!({"tools": [{"type": "namespace"}]})), &plain, "responses:lite"));
    }
}
