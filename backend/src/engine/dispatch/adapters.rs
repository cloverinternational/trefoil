//! `types/provider-adapter.ts` + `transformers/adapters/*.ts`.
//!
//! Adapters rewrite the outbound provider payload (`pre_dispatch`), the raw
//! non-streaming response (`post_dispatch`, identity for every built-in) and
//! raw SSE lines. `options` is the config entry's `options` JSON.
use super::util::{del, sha256_hex, strict_eq};
use crate::engine::js::*;
use crate::engine::routing::RouteResult;
use serde_json::{json, Value};

pub trait ProviderAdapter: Send + Sync {
    /// Registry key (config `adapter[].name`).
    fn name(&self) -> &'static str;
    fn pre_dispatch(&self, payload: Value, options: Option<&Value>) -> Value;
    fn post_dispatch(&self, response: Value, _options: Option<&Value>) -> Value {
        response
    }
    /// `preDispatchStreamChunk?`; `None` when the adapter does not define it.
    fn pre_dispatch_stream_chunk(&self, _line: &str, _options: Option<&Value>) -> Option<String> {
        None
    }
    /// `postDispatchStreamChunk?`; `None` when the adapter does not define it.
    fn post_dispatch_stream_chunk(&self, _line: &str, _options: Option<&Value>) -> Option<String> {
        None
    }
}

/// `ResolvedAdapter`.
#[derive(Clone)]
pub struct ResolvedAdapter {
    pub adapter: &'static dyn ProviderAdapter,
    pub options: Option<Value>,
}

impl std::fmt::Debug for ResolvedAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedAdapter")
            .field("adapter", &self.adapter.name())
            .field("options", &self.options)
            .finish()
    }
}

/// `ADAPTER_REGISTRY` lookup.
pub fn adapter_by_name(name: &str) -> Option<&'static dyn ProviderAdapter> {
    static ALL: [&dyn ProviderAdapter; 9] = [
        &ReasoningContent,
        &SuppressDeveloperRole,
        &ModelOverride,
        &ReasoningRewrite,
        &WebSearchCoercion,
        &StripUnsupportedToolSearch,
        &SuppressUnsupportedGpt5Options,
        &NormalizeAnthropicToolIds,
        &MuseCodeCompat,
    ];
    ALL.iter().copied().find(|a| a.name() == name)
}

fn with_key(payload: Value, key: &str, value: Value) -> Value {
    let mut map = payload.as_object().cloned().unwrap_or_default();
    map.insert(key.into(), value);
    Value::Object(map)
}

/// `resolveDottedPath(obj, path)`.
pub fn resolve_dotted_path<'a>(obj: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = obj;
    for seg in path.split('.') {
        cur = match cur {
            Value::Object(m) => m.get(seg)?,
            Value::Array(a) => a.get(seg.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(cur)
}

// ── model_override ────────────────────────────────────────────────────────

pub struct ModelOverride;
impl ProviderAdapter for ModelOverride {
    fn name(&self) -> &'static str {
        "model_override"
    }
    fn pre_dispatch(&self, payload: Value, options: Option<&Value>) -> Value {
        let Some(rules) = options
            .and_then(|o| o.get("rules"))
            .and_then(Value::as_array)
        else {
            return payload;
        };
        for rule in rules {
            if !strict_eq(payload.get("model"), rule.get("model")) {
                continue;
            }
            let matched = get_arr(rule, &["conditions"]).is_some_and(|conds| {
                conds.iter().any(|c| {
                    let actual =
                        get_str(c, &["field"]).and_then(|f| resolve_dotted_path(&payload, f));
                    match c.get("value") {
                        Some(v) => strict_eq(actual, Some(v)),
                        None => actual.is_some(),
                    }
                })
            });
            if matched {
                let mut map = payload.as_object().cloned().unwrap_or_default();
                match rule.get("rewriteTo") {
                    Some(m) => {
                        map.insert("model".into(), m.clone());
                    }
                    None => {
                        del(&mut map, "model");
                    }
                }
                return Value::Object(map);
            }
        }
        payload
    }
}

// ── muse_code_compat ──────────────────────────────────────────────────────

/// `isCustomTool(tool)`.
pub fn is_custom_tool(tool: &Value) -> bool {
    tool.is_object() && get_str(tool, &["type"]).is_some_and(|t| t.to_lowercase() == "custom")
}

/// `isMuseTarget(route)`.
pub fn is_muse_target(route: &RouteResult) -> bool {
    if crate::engine::config::oauth_provider_of(&route.provider, &route.config) == "meta" {
        return true;
    }
    let urls: Vec<&Value> = match route.config.get("api_base_url") {
        Some(v @ Value::String(_)) => vec![v],
        Some(Value::Object(m)) => m.values().collect(),
        _ => vec![],
    };
    urls.iter().any(|u| {
        u.as_str()
            .is_some_and(|s| s.to_lowercase().contains("api.meta.ai"))
    })
}

/// `isMuseSubscriptionTarget(route)`: whether a dispatch carries a Muse Code
/// *subscription* credential, the only case Meta's builtin-tool check applies
/// to. A direct `api.meta.ai` provider with a pay-as-you-go key speaks the
/// same wire contract but is not gated, so it must not receive the
/// declaration.
pub fn is_muse_subscription_target(route: &RouteResult) -> bool {
    if get_str(&route.config, &["oauth_provider"]) == Some("meta") {
        return true;
    }
    if crate::engine::config::oauth_provider_of(&route.provider, &route.config) != "meta" {
        return false;
    }
    match route.config.get("api_base_url") {
        Some(Value::String(s)) => crate::engine::config::is_oauth_placeholder_url(s),
        Some(Value::Object(m)) => m
            .values()
            .filter_map(Value::as_str)
            .any(crate::engine::config::is_oauth_placeholder_url),
        _ => false,
    }
}

pub struct MuseCodeCompat;
impl ProviderAdapter for MuseCodeCompat {
    fn name(&self) -> &'static str {
        "muse_code_compat"
    }
    fn pre_dispatch(&self, payload: Value, options: Option<&Value>) -> Value {
        let mut next = payload;
        if next.get("tool_choice").is_some() {
            if let Some(m) = next.as_object_mut() {
                del(m, "tool_choice");
            }
        }
        if let Some(tools) = get_arr(&next, &["tools"]).filter(|t| !t.is_empty()) {
            let kept: Vec<Value> = tools
                .iter()
                .filter(|t| !is_custom_tool(t))
                .cloned()
                .collect();
            if kept.len() != tools.len() {
                next = with_key(next, "tools", Value::Array(kept));
            }
        }
        if options.and_then(|o| o.get("inject_builtin_tool")) == Some(&Value::Bool(true)) {
            next = super::muse_builtin_tool::inject_muse_builtin_tool(next);
        }
        next
    }
}

// ── reasoning_content ─────────────────────────────────────────────────────

pub struct ReasoningContent;
impl ProviderAdapter for ReasoningContent {
    fn name(&self) -> &'static str {
        "reasoning_content"
    }
    fn pre_dispatch(&self, payload: Value, _options: Option<&Value>) -> Value {
        let Some(messages) = get_arr(&payload, &["messages"]) else {
            return payload;
        };
        let messages: Vec<Value> = messages
            .iter()
            .map(|msg| {
                if get_str(msg, &["role"]) != Some("assistant") {
                    return msg.clone();
                }
                let value = msg
                    .get("reasoning")
                    .or_else(|| get(msg, &["thinking", "content"]));
                let Some(value) = value.cloned() else {
                    return msg.clone();
                };
                let mut rest = msg.as_object().cloned().unwrap_or_default();
                del(&mut rest, "reasoning");
                del(&mut rest, "thinking");
                rest.insert("reasoning_content".into(), value);
                Value::Object(rest)
            })
            .collect();
        with_key(payload, "messages", Value::Array(messages))
    }
    fn pre_dispatch_stream_chunk(&self, line: &str, _options: Option<&Value>) -> Option<String> {
        Some(if line.starts_with("data:") {
            line.replace("\"reasoning\":", "\"reasoning_content\":")
        } else {
            line.to_owned()
        })
    }
}

// ── suppress_developer_role ───────────────────────────────────────────────

pub struct SuppressDeveloperRole;
impl ProviderAdapter for SuppressDeveloperRole {
    fn name(&self) -> &'static str {
        "suppress_developer_role"
    }
    fn pre_dispatch(&self, payload: Value, _options: Option<&Value>) -> Value {
        let Some(messages) = get_arr(&payload, &["messages"]) else {
            return payload;
        };
        let messages: Vec<Value> = messages
            .iter()
            .map(|msg| {
                if get_str(msg, &["role"]) != Some("developer") {
                    return msg.clone();
                }
                with_key(msg.clone(), "role", json!("system"))
            })
            .collect();
        with_key(payload, "messages", Value::Array(messages))
    }
    fn pre_dispatch_stream_chunk(&self, line: &str, _options: Option<&Value>) -> Option<String> {
        Some(if line.starts_with("data:") {
            line.replace("\"role\":\"developer\"", "\"role\":\"system\"")
        } else {
            line.to_owned()
        })
    }
}

// ── suppress_unsupported_gpt5_options ─────────────────────────────────────

pub const GPT5_UNSUPPORTED_OPTIONS: [&str; 11] = [
    "temperature",
    "top_p",
    "logprobs",
    "top_logprobs",
    "frequency_penalty",
    "presence_penalty",
    "logit_bias",
    "truncation",
    "max_output_tokens",
    "max_completion_tokens",
    "safety_identifier",
];

/// `stripUnsupportedGpt5Options(payload)`.
pub fn strip_unsupported_gpt5_options(payload: Value) -> Value {
    let mut map = payload.as_object().cloned().unwrap_or_default();
    for k in GPT5_UNSUPPORTED_OPTIONS {
        del(&mut map, k);
    }
    Value::Object(map)
}

pub struct SuppressUnsupportedGpt5Options;
impl ProviderAdapter for SuppressUnsupportedGpt5Options {
    fn name(&self) -> &'static str {
        "suppress_unsupported_gpt5_options"
    }
    fn pre_dispatch(&self, payload: Value, _options: Option<&Value>) -> Value {
        strip_unsupported_gpt5_options(payload)
    }
}

// ── normalize_anthropic_tool_ids ──────────────────────────────────────────

fn is_anthropic_id_char(c: u16) -> bool {
    c < 128 && ((c as u8).is_ascii_alphanumeric() || c == b'_' as u16 || c == b'-' as u16)
}

/// `sanitizeAnthropicToolId(id)`: deterministic, idempotent mapping onto
/// `^[a-zA-Z0-9_-]+$` (≤ 64 chars) with a sha256 suffix over UTF-16LE.
pub fn sanitize_anthropic_tool_id(id: Option<&Value>) -> String {
    if let Some(Value::String(s)) = id {
        if !s.is_empty() && s.encode_utf16().all(is_anthropic_id_char) {
            return s.clone();
        }
    }
    let source = match id {
        Some(Value::String(s)) => s.clone(),
        other => to_string(other),
    };
    let bytes: Vec<u8> = source.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let hash8 = &sha256_hex(&bytes)[..8];
    let s = match id {
        Some(Value::String(s)) if !s.is_empty() => s,
        _ => return format!("tool_{hash8}"),
    };
    let replaced: String = s
        .encode_utf16()
        .map(|c| {
            if is_anthropic_id_char(c) {
                c as u8 as char
            } else {
                '_'
            }
        })
        .collect();
    let prefix: String = replaced.chars().take(64 - 8 - 1).collect();
    format!("{prefix}_{hash8}")
}

/// `normalizeAnthropicToolIds(payload)`: rewrites in place, returns the count.
pub fn normalize_anthropic_tool_ids(payload: &mut Value) -> usize {
    let Some(messages) = payload.get_mut("messages").and_then(Value::as_array_mut) else {
        return 0;
    };
    let mut count = 0;
    for message in messages {
        let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        for block in content {
            let Some(b) = block.as_object_mut() else {
                continue;
            };
            let t = b
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            if matches!(t.as_str(), "tool_use" | "server_tool_use" | "mcp_tool_use")
                && b.contains_key("id")
            {
                let sanitized = sanitize_anthropic_tool_id(b.get("id"));
                if !strict_eq(Some(&json!(sanitized)), b.get("id")) {
                    b.insert("id".into(), json!(sanitized));
                    count += 1;
                }
            }
            let tuid = b.get("tool_use_id");
            if tuid.is_some_and(Value::is_string)
                || (matches!(t.as_str(), "tool_result" | "mcp_tool_result") && tuid.is_some())
            {
                let sanitized = sanitize_anthropic_tool_id(tuid);
                if !strict_eq(Some(&json!(sanitized)), b.get("tool_use_id")) {
                    b.insert("tool_use_id".into(), json!(sanitized));
                    count += 1;
                }
            }
        }
    }
    count
}

pub struct NormalizeAnthropicToolIds;
impl ProviderAdapter for NormalizeAnthropicToolIds {
    fn name(&self) -> &'static str {
        "normalize_anthropic_tool_ids"
    }
    fn pre_dispatch(&self, mut payload: Value, _options: Option<&Value>) -> Value {
        normalize_anthropic_tool_ids(&mut payload);
        payload
    }
}

// ── reasoning_rewrite ─────────────────────────────────────────────────────

/// JS relational comparison (`a < b` etc.) for JSON values.
fn js_cmp(a: Option<&Value>, b: Option<&Value>) -> Option<std::cmp::Ordering> {
    if let (Some(Value::String(x)), Some(Value::String(y))) = (a, b) {
        return Some(x.encode_utf16().cmp(y.encode_utf16()));
    }
    let n = |v: Option<&Value>| match v {
        None => f64::NAN,
        Some(Value::Array(_)) | Some(Value::Object(_)) => {
            to_number(Some(&json!(to_string(v)))).unwrap_or(f64::NAN)
        }
        other => to_number(other).unwrap_or(f64::NAN),
    };
    n(a).partial_cmp(&n(b))
}

fn evaluate_match(source: Option<&Value>, cond: &Value) -> bool {
    use std::cmp::Ordering::*;
    let v = cond.get("value");
    match get_str(cond, &["op"]) {
        Some("present") => source.is_some(),
        Some("absent") => source.is_none(),
        Some("eq") => strict_eq(source, v),
        Some("neq") => !strict_eq(source, v),
        Some("gt") => js_cmp(source, v) == Some(Greater),
        Some("gte") => matches!(js_cmp(source, v), Some(Greater | Equal)),
        Some("lt") => js_cmp(source, v) == Some(Less),
        Some("lte") => matches!(js_cmp(source, v), Some(Less | Equal)),
        Some("in") => get_arr(cond, &["values"])
            .is_some_and(|vals| vals.iter().any(|x| strict_eq(source, Some(x)))),
        _ => false,
    }
}

fn compute_target_value(source: Option<&Value>, rewrite: &Value) -> Option<Value> {
    let v = rewrite.get("value")?;
    let Value::Object(o) = v else {
        return Some(v.clone());
    };
    let Some(from) = o.get("from") else {
        return Some(v.clone());
    };
    match from.as_str() {
        Some("source") => source.cloned(),
        Some("map") => {
            let map = o.get("values").filter(|m| truthy(Some(m)))?;
            map.as_object()?.get(&to_string(source)).cloned()
        }
        Some("boolean") => {
            if truthy(source) {
                o.get("truthy").cloned()
            } else {
                o.get("falsy").cloned()
            }
        }
        _ => Some(v.clone()),
    }
}

/// `setDottedPath(obj, path, value)` (`undefined` writes are skipped).
pub fn set_dotted_path(obj: &mut Value, path: &str, value: Option<Value>) {
    let Some(value) = value else { return };
    let segments: Vec<&str> = path.split('.').collect();
    let (last, parents) = segments.split_last().expect("split yields one segment");
    let mut cur = obj;
    for seg in parents {
        let next = match cur {
            Value::Object(m) => {
                let entry = m.entry(seg.to_string()).or_insert(Value::Null);
                if !(entry.is_object() || entry.is_array()) {
                    *entry = json!({});
                }
                entry
            }
            Value::Array(a) => match seg.parse::<usize>().ok().and_then(|i| a.get_mut(i)) {
                Some(entry) => {
                    if !(entry.is_object() || entry.is_array()) {
                        *entry = json!({});
                    }
                    entry
                }
                None => return,
            },
            _ => return,
        };
        cur = next;
    }
    match cur {
        Value::Object(m) => {
            m.insert(last.to_string(), value);
        }
        Value::Array(a) => {
            if let Some(slot) = last.parse::<usize>().ok().and_then(|i| a.get_mut(i)) {
                *slot = value;
            }
        }
        _ => {}
    }
}

/// `removeDottedPath(obj, path)` (array leaves become holes → `null`).
pub fn remove_dotted_path(obj: &mut Value, path: &str) {
    let segments: Vec<&str> = path.split('.').collect();
    let (last, parents) = segments.split_last().expect("split yields one segment");
    let mut cur = obj;
    for seg in parents {
        let next = match cur {
            Value::Object(m) => m.get_mut(*seg),
            Value::Array(a) => seg.parse::<usize>().ok().and_then(|i| a.get_mut(i)),
            _ => None,
        };
        match next {
            Some(n) if n.is_object() || n.is_array() => cur = n,
            _ => return,
        }
    }
    match cur {
        Value::Object(m) => {
            del(m, last);
        }
        Value::Array(a) => {
            if let Some(slot) = last.parse::<usize>().ok().and_then(|i| a.get_mut(i)) {
                *slot = Value::Null;
            }
        }
        _ => {}
    }
}

pub struct ReasoningRewrite;
impl ProviderAdapter for ReasoningRewrite {
    fn name(&self) -> &'static str {
        "reasoning_rewrite"
    }
    fn pre_dispatch(&self, payload: Value, options: Option<&Value>) -> Value {
        let Some(rules) = options
            .and_then(|o| o.get("rules"))
            .and_then(Value::as_array)
            .filter(|r| !r.is_empty())
        else {
            return payload;
        };
        let mut result: Option<Value> = None;
        for rule in rules {
            let current = result.as_ref().unwrap_or(&payload);
            let source_path = get_str(rule, &["source"]).unwrap_or("undefined");
            let source = resolve_dotted_path(current, source_path).cloned();
            let fire = match rule.get("when").filter(|w| truthy(Some(w))) {
                Some(when) => evaluate_match(source.as_ref(), when),
                None => source.is_some(),
            };
            if !fire {
                continue;
            }
            let obj = result.get_or_insert_with(|| payload.clone());
            for rewrite in get_arr(rule, &["rewrites"])
                .map(Vec::as_slice)
                .unwrap_or(&[])
            {
                let target = get_str(rewrite, &["target"]).unwrap_or("undefined");
                set_dotted_path(obj, target, compute_target_value(source.as_ref(), rewrite));
            }
            if let Some(strip) = get_arr(rule, &["strip"]) {
                for p in strip {
                    remove_dotted_path(obj, &to_string(Some(p)));
                }
            }
        }
        result.unwrap_or(payload)
    }
}

// ── strip_unsupported_tool_search ─────────────────────────────────────────

/// `isToolSearchShorthand(tool)`.
pub fn is_tool_search_shorthand(tool: &Value) -> bool {
    get_str(tool, &["type"]).is_some_and(|t| t.to_lowercase().starts_with("tool_search_tool_"))
}

pub struct StripUnsupportedToolSearch;
impl ProviderAdapter for StripUnsupportedToolSearch {
    fn name(&self) -> &'static str {
        "strip_unsupported_tool_search"
    }
    fn pre_dispatch(&self, payload: Value, _options: Option<&Value>) -> Value {
        let Some(tools) = get_arr(&payload, &["tools"]).filter(|t| !t.is_empty()) else {
            return payload;
        };
        let filtered: Vec<Value> = tools
            .iter()
            .filter(|t| !is_tool_search_shorthand(t))
            .cloned()
            .collect();
        if filtered.len() == tools.len() {
            return payload;
        }
        with_key(payload, "tools", Value::Array(filtered))
    }
}

// ── web_search_coercion ───────────────────────────────────────────────────

/// `isWebSearchTool(tool)`.
pub fn is_web_search_tool(tool: &Value) -> bool {
    matches!(
        get_str(tool, &["type"]),
        Some("web_search_20250305" | "web_search" | "openrouter:web_search" | "googleSearch")
    )
}

/// `buildTargetTool(target, maxUses)`.
pub fn build_target_tool(target: &str, max_uses: Option<&Value>) -> Value {
    match target {
        "anthropic" => obj(vec![
            ("type", Some(json!("web_search_20250305"))),
            ("name", Some(json!("web_search"))),
            ("max_uses", max_uses.cloned()),
        ]),
        "openai" => json!({"type": "web_search"}),
        "openrouter" => json!({"type": "openrouter:web_search"}),
        _ => json!({"googleSearch": {}}),
    }
}

pub struct WebSearchCoercion;
impl ProviderAdapter for WebSearchCoercion {
    fn name(&self) -> &'static str {
        "web_search_coercion"
    }
    fn pre_dispatch(&self, payload: Value, options: Option<&Value>) -> Value {
        let Some(target) = options
            .and_then(|o| o.get("target"))
            .filter(|t| truthy(Some(t)))
        else {
            return payload;
        };
        let Some(tools) = get_arr(&payload, &["tools"]).filter(|t| !t.is_empty()) else {
            return payload;
        };
        let Some(target) = target
            .as_str()
            .filter(|t| matches!(*t, "anthropic" | "openai" | "openrouter" | "google"))
        else {
            return payload;
        };
        let max_uses = options
            .and_then(|o| o.get("max_uses"))
            .filter(|v| v.is_number());
        let mut did = false;
        let tools: Vec<Value> = tools
            .iter()
            .map(|t| {
                if is_web_search_tool(t) {
                    did = true;
                    build_target_tool(target, max_uses)
                } else {
                    t.clone()
                }
            })
            .collect();
        if !did {
            return payload;
        }
        with_key(payload, "tools", Value::Array(tools))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_override() {
        let opts = json!({"rules": [
            {"model": "deepseek-r1", "rewriteTo": "deepseek-r1-fast", "conditions": [{"field": "reasoning.enabled", "value": false}, {"field": "reasoning.effort", "value": "none"}]},
            {"model": "deepseek-r1-fast", "rewriteTo": "deepseek-r1", "conditions": [{"field": "reasoning.enabled", "value": true}]},
            {"model": "x", "rewriteTo": "y", "conditions": [{"field": "tools"}]}
        ]});
        let a = ModelOverride.pre_dispatch(
            json!({"model": "deepseek-r1", "reasoning": {"effort": "none"}}),
            Some(&opts),
        );
        assert_eq!(a["model"], "deepseek-r1-fast");
        let b = ModelOverride.pre_dispatch(
            json!({"model": "deepseek-r1-fast", "reasoning": {"enabled": true}}),
            Some(&opts),
        );
        assert_eq!(b["model"], "deepseek-r1");
        assert_eq!(
            ModelOverride.pre_dispatch(json!({"model": "x", "tools": null}), Some(&opts))["model"],
            "y"
        );
        assert_eq!(
            ModelOverride.pre_dispatch(json!({"model": "x"}), Some(&opts))["model"],
            "x"
        );
        assert_eq!(
            ModelOverride.pre_dispatch(json!({"model": "x"}), None)["model"],
            "x"
        );
    }

    #[test]
    fn muse_and_tool_search_and_gpt5() {
        let p = json!({"model": "m", "tool_choice": "auto", "tools": [{"type": "function", "name": "a"}, {"type": "Custom", "name": "b"}]});
        assert_eq!(
            MuseCodeCompat.pre_dispatch(p, None),
            json!({"model": "m", "tools": [{"type": "function", "name": "a"}]})
        );
        let p = json!({"tools": [{"type": "tool_search_tool_bm25_20251119"}, {"type": "web_search_20250305"}]});
        assert_eq!(
            StripUnsupportedToolSearch.pre_dispatch(p, None),
            json!({"tools": [{"type": "web_search_20250305"}]})
        );
        let p = json!({"model": "gpt-5", "temperature": 1, "top_p": 1, "input": []});
        assert_eq!(
            SuppressUnsupportedGpt5Options.pre_dispatch(p, None),
            json!({"model": "gpt-5", "input": []})
        );
    }

    #[test]
    fn muse_injects_builtin_tool_only_when_asked() {
        use crate::engine::dispatch::muse_builtin_tool::has_muse_builtin_namespace;
        let payload = json!({"tools": [{"type": "function", "name": "read"}]});
        assert_eq!(MuseCodeCompat.pre_dispatch(payload.clone(), None), payload);
        assert_eq!(
            MuseCodeCompat.pre_dispatch(
                payload.clone(),
                Some(&json!({"inject_builtin_tool": false}))
            ),
            payload
        );
        let injected =
            MuseCodeCompat.pre_dispatch(payload, Some(&json!({"inject_builtin_tool": true})));
        assert!(has_muse_builtin_namespace(
            injected["tools"].as_array().unwrap()
        ));
        // Custom tools are still dropped before the declaration is appended.
        let both = MuseCodeCompat.pre_dispatch(
            json!({"tools": [{"type": "custom", "name": "c"}, {"type": "function", "name": "f"}]}),
            Some(&json!({"inject_builtin_tool": true})),
        );
        let names: Vec<&str> = both["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        assert_eq!(names, ["f", "muse"]);
    }

    #[test]
    fn muse_subscription_target_detection() {
        let route = |config: Value| RouteResult {
            provider: "p".into(),
            model: "m".into(),
            config,
            model_config: None,
            incoming_model_alias: None,
            canonical_model: None,
        };
        assert!(is_muse_subscription_target(&route(
            json!({"api_base_url": "oauth://plexus", "oauth_provider": "meta"})
        )));
        // Pay-as-you-go key on the same host is not gated.
        assert!(!is_muse_subscription_target(&route(
            json!({"api_base_url": "https://api.meta.ai/v1"})
        )));
        // A provider slugged `meta` is a subscription target only on an
        // OAuth placeholder URL.
        let mut meta = route(json!({"api_base_url": {"responses": "oauth://plexus"}}));
        meta.provider = "meta".into();
        assert!(is_muse_subscription_target(&meta));
        meta.config = json!({"api_base_url": "https://api.meta.ai/v1"});
        assert!(!is_muse_subscription_target(&meta));
    }

    #[test]
    fn reasoning_content_and_developer_role() {
        let p = json!({"messages": [{"role": "assistant", "content": "x", "reasoning": "r", "thinking": {"content": "t"}}, {"role": "assistant", "thinking": {"content": "t"}}, {"role": "developer", "content": "d"}]});
        let out = ReasoningContent.pre_dispatch(p.clone(), None);
        assert_eq!(
            out["messages"][0],
            json!({"role": "assistant", "content": "x", "reasoning_content": "r"})
        );
        assert_eq!(
            out["messages"][1],
            json!({"role": "assistant", "reasoning_content": "t"})
        );
        let out = SuppressDeveloperRole.pre_dispatch(p, None);
        assert_eq!(out["messages"][2]["role"], "system");
        assert_eq!(
            ReasoningContent
                .pre_dispatch_stream_chunk("data: {\"reasoning\":1}", None)
                .unwrap(),
            "data: {\"reasoning_content\":1}"
        );
        assert_eq!(
            SuppressDeveloperRole
                .pre_dispatch_stream_chunk("event: x", None)
                .unwrap(),
            "event: x"
        );
    }

    #[test]
    fn tool_id_sanitizer() {
        assert_eq!(
            sanitize_anthropic_tool_id(Some(&json!("toolu_01ABC"))),
            "toolu_01ABC"
        );
        let s = sanitize_anthropic_tool_id(Some(&json!("functions.WebSearch:3")));
        assert!(
            s.starts_with("functions_WebSearch_3_")
                && s.len() == "functions_WebSearch_3_".len() + 8
        );
        assert_eq!(sanitize_anthropic_tool_id(Some(&json!(s.clone()))), s);
        assert_ne!(
            sanitize_anthropic_tool_id(Some(&json!("a.b"))),
            sanitize_anthropic_tool_id(Some(&json!("a:b")))
        );
        assert!(sanitize_anthropic_tool_id(Some(&json!(""))).starts_with("tool_"));
        assert!(sanitize_anthropic_tool_id(Some(&json!(42))).starts_with("tool_"));
        assert!(sanitize_anthropic_tool_id(Some(&json!("x".repeat(100) + "."))).len() == 64);
        let mut p = json!({"messages": [{"role": "assistant", "content": [{"type": "tool_use", "id": "call_x|fc_y"}]}, {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "call_x|fc_y"}, {"type": "text"}]}]});
        assert_eq!(normalize_anthropic_tool_ids(&mut p), 2);
        assert_eq!(
            p["messages"][0]["content"][0]["id"],
            p["messages"][1]["content"][0]["tool_use_id"]
        );
    }

    #[test]
    fn tool_id_hash_matches_node() {
        // node: createHash('sha256').update(Buffer.from('a.b','utf16le')).digest('hex').slice(0,8)
        let bytes: Vec<u8> = "a.b".encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert_eq!(
            sanitize_anthropic_tool_id(Some(&json!("a.b"))),
            format!("a_b_{}", &sha256_hex(&bytes)[..8])
        );
    }

    #[test]
    fn reasoning_rewrite() {
        let opts = json!({"rules": [
            {"source": "reasoning.enabled", "rewrites": [{"target": "enable_thinking", "value": {"from": "source"}}, {"target": "thinking.type", "value": {"from": "boolean", "truthy": "enabled", "falsy": "disabled"}}], "strip": ["reasoning"]},
            {"source": "reasoning.effort", "when": {"op": "in", "values": ["low", "high"]}, "rewrites": [{"target": "budget_tokens", "value": {"from": "map", "values": {"low": 1024, "high": 32768}}}]},
            {"source": "max_tokens", "when": {"op": "gt", "value": 100}, "rewrites": [{"target": "chat_template_kwargs.big", "value": true}]}
        ]});
        let out = ReasoningRewrite.pre_dispatch(
            json!({"reasoning": {"enabled": false}, "max_tokens": 50}),
            Some(&opts),
        );
        assert_eq!(
            out,
            json!({"max_tokens": 50, "enable_thinking": false, "thinking": {"type": "disabled"}})
        );
        let out = ReasoningRewrite.pre_dispatch(
            json!({"reasoning": {"effort": "high"}, "max_tokens": 500}),
            Some(&opts),
        );
        assert_eq!(
            out,
            json!({"reasoning": {"effort": "high"}, "max_tokens": 500, "budget_tokens": 32768, "chat_template_kwargs": {"big": true}})
        );
        let untouched = json!({"x": 1});
        assert_eq!(
            ReasoningRewrite.pre_dispatch(untouched.clone(), Some(&opts)),
            untouched
        );
    }

    #[test]
    fn web_search() {
        let p = json!({"tools": [{"type": "web_search"}, {"type": "function"}]});
        let out = WebSearchCoercion.pre_dispatch(
            p.clone(),
            Some(&json!({"target": "anthropic", "max_uses": 3})),
        );
        assert_eq!(
            out["tools"][0],
            json!({"type": "web_search_20250305", "name": "web_search", "max_uses": 3})
        );
        assert_eq!(
            WebSearchCoercion.pre_dispatch(p.clone(), Some(&json!({"target": "google"})))["tools"]
                [0],
            json!({"googleSearch": {}})
        );
        assert_eq!(
            WebSearchCoercion.pre_dispatch(p.clone(), Some(&json!({"target": "bogus"}))),
            p
        );
    }
}
