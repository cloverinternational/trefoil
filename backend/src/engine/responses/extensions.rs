//! Responses API extensions: `services/dispatch/responses-extensions.ts`
//! plus the extension lists / defaults from `@plexus/shared` `provider.ts`
//! and the `utils/api-format.ts` helpers they use.
//!
//! Extensions (namespace tools, custom tools, Codex lite items) are forwarded
//! verbatim only when the target accepts every extension the body carries;
//! anything else takes the transform pipeline (see `tool_mapper`).
use crate::engine::js::*;
use serde_json::Value;

/// `RESPONSES_EXTENSIONS`.
pub const RESPONSES_EXTENSIONS: [&str; 7] = [
    "namespace_tools",
    "namespaced_calls",
    "dotted_calls",
    "custom_tools",
    "custom_calls",
    "additional_tools",
    "tool_search",
];

/// `RESPONSES_LITE_EXTENSIONS`: every extension except `namespace_tools` and
/// `dotted_calls` (the Codex lite wire contract).
pub const RESPONSES_LITE_EXTENSIONS: [&str; 5] = ["namespaced_calls", "custom_tools", "custom_calls", "additional_tools", "tool_search"];

/// Plain Responses targets with no known endpoint.
pub const BASE_RESPONSES_EXTENSIONS: [&str; 1] = ["custom_tools"];
/// Muse Code's own endpoint.
pub const META_RESPONSES_EXTENSIONS: [&str; 3] = ["namespace_tools", "namespaced_calls", "dotted_calls"];
/// OpenAI's public /v1/responses.
pub const OPENAI_RESPONSES_EXTENSIONS: [&str; 4] = ["namespace_tools", "namespaced_calls", "custom_tools", "custom_calls"];
/// The ChatGPT Codex backend: every extension except `dotted_calls`.
pub const CODEX_RESPONSES_EXTENSIONS: [&str; 6] = ["namespace_tools", "namespaced_calls", "custom_tools", "custom_calls", "additional_tools", "tool_search"];

/// Tool `type` declared by each tool-declaration extension.
fn extension_tool_type(extension: &str) -> Option<&'static str> {
    match extension {
        "namespace_tools" => Some("namespace"),
        "custom_tools" => Some("custom"),
        "tool_search" => Some("tool_search"),
        _ => None,
    }
}

/// `LITE_ALLOWED_TOOL_TYPES`: tool `type`s the responses:lite contract
/// accepts in `tools` (`function`, `custom`, `tool_search`), in Set order.
pub fn lite_allowed_tool_types() -> Vec<&'static str> {
    let mut out = vec!["function"];
    for t in RESPONSES_LITE_EXTENSIONS.iter().filter_map(|e| extension_tool_type(e)) {
        if !out.contains(&t) { out.push(t); }
    }
    out
}

/// `getApiBaseType`.
pub fn get_api_base_type(api_type: &str) -> String {
    api_type.trim().to_lowercase().split(':').next().unwrap_or("").to_owned()
}

/// `getApiSubtype`.
pub fn get_api_subtype(api_type: &str) -> Option<String> {
    let normalized = api_type.trim().to_lowercase();
    let sep = normalized.find(':')?;
    Some(normalized[sep + 1..].to_owned()).filter(|s| !s.is_empty())
}

/// `!!value && typeof value === 'object'` (arrays included).
fn is_object(v: &Value) -> bool { v.is_object() || v.is_array() }

/// Insertion-ordered `Set<ResponsesExtension>`.
fn add(found: &mut Vec<&'static str>, ext: &'static str) {
    if !found.contains(&ext) { found.push(ext); }
}

/// Names of every `namespace` tool declared on `tools` or in `additional_tools` items.
fn declared_namespaces(body: &Value) -> Vec<String> {
    let mut decls: Vec<&Value> = get_arr(body, &["tools"]).map(|t| t.iter().collect()).unwrap_or_default();
    if let Some(input) = get_arr(body, &["input"]) {
        for item in input {
            if is_object(item) && get_str(item, &["type"]) == Some("additional_tools") {
                if let Some(tools) = get_arr(item, &["tools"]) { decls.extend(tools.iter()); }
            }
        }
    }
    let mut out: Vec<String> = Vec::new();
    for tool in decls {
        if is_object(tool) && get_str(tool, &["type"]) == Some("namespace") {
            if let Some(name) = get_str(tool, &["name"]) {
                if !out.iter().any(|n| n == name) { out.push(name.to_owned()); }
            }
        }
    }
    out
}

/// Muse Code's `name: "<namespace>.<tool>"` spelling, against a declared namespace.
fn is_dotted_call(item: &Value, namespaces: &[String]) -> bool {
    let Some(name) = get_str(item, &["name"]) else { return false };
    match name.find('.') {
        Some(dot) if dot > 0 => namespaces.iter().any(|n| n == &name[..dot]),
        _ => false,
    }
}

/// A `custom` tool declaration, directly or nested in a namespace.
fn declares_custom_tool(tool: &Value) -> bool {
    if !is_object(tool) { return false; }
    match get_str(tool, &["type"]) {
        Some("custom") => true,
        Some("namespace") => get_arr(tool, &["tools"]).is_some_and(|subs| subs.iter().any(|s| is_object(s) && get_str(s, &["type"]) == Some("custom"))),
        _ => false,
    }
}

/// `detectResponsesExtensions`: every extension present on a request body,
/// in detection (Set insertion) order.
pub fn detect_responses_extensions(body: &Value) -> Vec<&'static str> {
    let mut found = Vec::new();
    if !is_object(body) { return found; }

    if let Some(tools) = get_arr(body, &["tools"]) {
        for tool in tools {
            if !is_object(tool) { continue; }
            match get_str(tool, &["type"]) {
                Some("namespace") => add(&mut found, "namespace_tools"),
                Some("tool_search") => add(&mut found, "tool_search"),
                _ => {}
            }
            if declares_custom_tool(tool) { add(&mut found, "custom_tools"); }
        }
    }

    if let Some(input) = get_arr(body, &["input"]) {
        let namespaces = declared_namespaces(body);
        for item in input {
            if !is_object(item) { continue; }
            let Some(ty) = get_str(item, &["type"]) else { continue };
            match ty {
                "function_call" => {
                    if item.get("namespace").is_some_and(Value::is_string) { add(&mut found, "namespaced_calls"); }
                    else if is_dotted_call(item, &namespaces) { add(&mut found, "dotted_calls"); }
                }
                "custom_tool_call" | "custom_tool_call_output" => add(&mut found, "custom_calls"),
                "additional_tools" => {
                    add(&mut found, "additional_tools");
                    if get_arr(item, &["tools"]).is_some_and(|t| t.iter().any(declares_custom_tool)) { add(&mut found, "custom_tools"); }
                }
                t if t.starts_with("tool_search") => add(&mut found, "tool_search"),
                _ => {}
            }
        }
    }
    found
}

/// `responsesHost`: lower-cased hostname of the provider's Responses URL.
fn responses_host(api_base_url: Option<&Value>) -> String {
    let url = match api_base_url {
        Some(Value::String(s)) => Some(Value::String(s.clone())),
        Some(v) if v.is_object() => v.get("responses").cloned(),
        _ => None,
    };
    let Some(url) = url.filter(|u| truthy(Some(u))) else { return String::new() };
    let text = to_string(Some(&url));
    url::Url::parse(&text).ok().and_then(|u| u.host_str().map(str::to_lowercase)).unwrap_or_default()
}

/// `getDefaultResponsesExtensions`: what a plain `responses` endpoint accepts
/// verbatim when the admin hasn't set `responses_extensions`.
pub fn get_default_responses_extensions(oauth_provider: Option<&str>, api_base_url: Option<&Value>) -> Vec<&'static str> {
    if oauth_provider == Some("openai-codex") { return CODEX_RESPONSES_EXTENSIONS.to_vec(); }
    if oauth_provider == Some("meta") { return META_RESPONSES_EXTENSIONS.to_vec(); }
    match responses_host(api_base_url).as_str() {
        "api.meta.ai" => META_RESPONSES_EXTENSIONS.to_vec(),
        "api.openai.com" => OPENAI_RESPONSES_EXTENSIONS.to_vec(),
        _ => BASE_RESPONSES_EXTENSIONS.to_vec(),
    }
}

/// `resolveSupportedResponsesExtensions`: extensions the route's target
/// accepts verbatim. `route_config` is the TS `route.config` (provider
/// config: `responses_extensions`, `oauth_provider`, `api_base_url`).
/// `responses:lite` targets get the fixed lite contract; otherwise an
/// explicit list (even `[]`) wins over the provider default.
pub fn resolve_supported_responses_extensions(route_config: &Value, target_api_type: &str) -> Vec<String> {
    if get_api_subtype(target_api_type).as_deref() == Some("lite") {
        return RESPONSES_LITE_EXTENSIONS.iter().map(|s| (*s).to_owned()).collect();
    }
    if let Some(explicit) = route_config.get("responses_extensions").filter(|e| truthy(Some(e))) {
        let mut out: Vec<String> = Vec::new();
        for e in explicit.as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
            let s = to_string(Some(e));
            if !out.contains(&s) { out.push(s); }
        }
        return out;
    }
    get_default_responses_extensions(get_str(route_config, &["oauth_provider"]), route_config.get("api_base_url"))
        .into_iter()
        .map(str::to_owned)
        .collect()
}

/// `hasUnsupportedResponsesExtensions`: true when a Responses-bound body
/// carries an extension the target doesn't accept verbatim.
pub fn has_unsupported_responses_extensions(body: &Value, route_config: &Value, target_api_type: &str) -> bool {
    if get_api_base_type(target_api_type) != "responses" { return false; }
    let detected = detect_responses_extensions(body);
    if detected.is_empty() { return false; }
    let supported = resolve_supported_responses_extensions(route_config, target_api_type);
    detected.iter().any(|e| !supported.iter().any(|s| s == e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg(extra: Value) -> Value { spread(&json!({"api_base_url": "https://api.test/v1", "models": {}}), &extra) }

    #[test]
    fn detection() {
        let body = json!({"tools": [{"type": "namespace", "name": "crm", "tools": []}, {"type": "custom", "name": "apply_patch"}, {"type": "tool_search"}, {"type": "function", "name": "ls"}]});
        assert_eq!(detect_responses_extensions(&body), ["namespace_tools", "custom_tools", "tool_search"]);
        let body = json!({"input": [{"type": "additional_tools", "tools": []}, {"type": "custom_tool_call", "name": "exec"}, {"type": "custom_tool_call_output", "call_id": "c"}, {"type": "tool_search_call"}, {"type": "function_call", "name": "list", "namespace": "crm"}]});
        assert_eq!(detect_responses_extensions(&body), ["additional_tools", "custom_calls", "tool_search", "namespaced_calls"]);
        let body = json!({"tools": [{"type": "namespace", "name": "muse", "tools": []}], "input": [{"type": "function_call", "name": "muse.bash"}, {"type": "function_call", "name": "other.bash"}]});
        assert_eq!(detect_responses_extensions(&body), ["namespace_tools", "dotted_calls"]);
        assert!(detect_responses_extensions(&json!("x")).is_empty());
        assert_eq!(lite_allowed_tool_types(), ["function", "custom", "tool_search"]);
    }

    #[test]
    fn support_resolution() {
        assert_eq!(resolve_supported_responses_extensions(&cfg(json!({})), "responses"), ["custom_tools"]);
        assert_eq!(resolve_supported_responses_extensions(&cfg(json!({"oauth_provider": "openai-codex"})), "responses").len(), 6);
        assert_eq!(resolve_supported_responses_extensions(&cfg(json!({"api_base_url": {"responses": "https://API.OpenAI.com/v1"}})), "responses"), OPENAI_RESPONSES_EXTENSIONS);
        assert_eq!(resolve_supported_responses_extensions(&cfg(json!({"responses_extensions": ["namespace_tools"]})), "responses:lite"), RESPONSES_LITE_EXTENSIONS);
        assert!(resolve_supported_responses_extensions(&cfg(json!({"responses_extensions": []})), "responses").is_empty());
        let ns = json!({"tools": [{"type": "namespace", "name": "crm", "tools": []}]});
        assert!(has_unsupported_responses_extensions(&ns, &cfg(json!({})), "responses"));
        assert!(!has_unsupported_responses_extensions(&ns, &cfg(json!({"responses_extensions": ["namespace_tools"]})), "responses"));
        assert!(!has_unsupported_responses_extensions(&ns, &cfg(json!({})), "chat"));
    }
}
