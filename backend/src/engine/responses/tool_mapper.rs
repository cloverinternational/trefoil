//! `transformers/responses/tool-mapper.ts`: Codex CLI tool extensions
//! (namespace tools, custom/freeform tools) flattened for providers and
//! split back for the client.
use super::js_ext::{iterate, js_trim, object_values, prop, strict_eq, ValueSet};
use super::utils::generate_item_id;
use crate::engine::js::*;
use crate::engine::transformer::EngineResult;
use serde_json::{json, Value};

/// One `namespaceMap` entry: the raw `namespace` / sub-tool `name` values.
#[derive(Debug, Clone, PartialEq)]
pub struct NamespaceEntry {
    pub namespace: Option<Value>,
    pub name: Option<Value>,
}

/// `ResponsesToolState`: per-request tool bookkeeping shared between
/// `parseRequest` and `formatResponse` / `formatStream` of one transformer.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResponsesToolState {
    /// Flat name → `{namespace, name}`, insertion ordered (JS `Map`).
    pub namespace_map: Vec<(String, NamespaceEntry)>,
    pub custom_tool_names: ValueSet,
    /// Every flat function name the model is given.
    pub tool_names: ValueSet,
}

impl ResponsesToolState {
    pub fn new() -> Self { Self::default() }

    /// `clearResponsesToolState`.
    pub fn clear(&mut self) {
        self.namespace_map.clear();
        self.custom_tool_names.clear();
        self.tool_names.clear();
    }

    /// `namespaceMap.get(flatName)`.
    pub fn namespace_get(&self, flat: &str) -> Option<&NamespaceEntry> {
        self.namespace_map.iter().find(|(k, _)| k == flat).map(|(_, e)| e)
    }

    /// `namespaceMap.has(flatName)`.
    pub fn namespace_has(&self, flat: &str) -> bool { self.namespace_get(flat).is_some() }

    /// `namespaceMap.set(flatName, entry)` (existing keys keep their position).
    pub fn namespace_set(&mut self, flat: String, entry: NamespaceEntry) {
        match self.namespace_map.iter_mut().find(|(k, _)| *k == flat) {
            Some((_, e)) => *e = entry,
            None => self.namespace_map.push((flat, entry)),
        }
    }
}

/// `createResponsesToolState`.
pub fn create_responses_tool_state() -> ResponsesToolState { ResponsesToolState::default() }

/// `clearResponsesToolState`.
pub fn clear_responses_tool_state(state: &mut ResponsesToolState) { state.clear(); }

/// The flat name of the only namespaced sub-tool called `name`, if exactly
/// one exists (OpenAI returns default-namespace calls without a namespace).
fn unique_namespaced_tool_name(name: Option<&Value>, state: &ResponsesToolState) -> Option<String> {
    let mut found: Option<&String> = None;
    for (flat, entry) in &state.namespace_map {
        if !strict_eq(entry.name.as_ref(), name) { continue; }
        if found.is_some() { return None; }
        found = Some(flat);
    }
    found.cloned()
}

/// `${a}__${b}` with JS template-literal stringification.
fn flat_join(a: Option<&Value>, b: Option<&Value>) -> String { format!("{}__{}", to_string(a), to_string(b)) }

/// `resolveHistoryToolName`: flat tool name for a function_call /
/// custom_tool_call history item. Accepts `{namespace, name}` (Codex),
/// `name: "<namespace>.<name>"` (Muse Code), or a bare sub-tool name.
pub fn resolve_history_tool_name(name: Option<&Value>, namespace: Option<&Value>, state: Option<&ResponsesToolState>) -> Option<Value> {
    if truthy(namespace) { return Some(Value::String(flat_join(namespace, name))); }
    let Some(state) = state else { return name.cloned() };
    let Some(name_str) = name.and_then(Value::as_str) else { return name.cloned() };
    if state.tool_names.has(name) { return name.cloned(); }
    if let Some(dot) = name_str.find('.').filter(|d| *d > 0) {
        let dotted = format!("{}__{}", &name_str[..dot], &name_str[dot + 1..]);
        if state.namespace_has(&dotted) { return Some(Value::String(dotted)); }
    }
    Some(unique_namespaced_tool_name(name, state).map(Value::String).unwrap_or_else(|| Value::String(name_str.to_owned())))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedToolType {
    FunctionCall,
    CustomToolCall,
}

/// `ResolvedToolCall`.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedToolCall {
    pub kind: ResolvedToolType,
    /// Flat name the model used, after resolving provider-side spellings.
    pub flat_name: Option<Value>,
    pub name: Option<Value>,
    pub namespace: Option<Value>,
}

/// `resolveOutputToolCall`: splits a model-generated flat name back into the
/// client's declared shape (namespace-qualified function calls; custom tools
/// as custom_tool_call carrying only the sub-tool name).
pub fn resolve_output_tool_call(raw_flat_name: Option<&Value>, state: Option<&ResponsesToolState>) -> ResolvedToolCall {
    let flat_name: Option<Value> = match state {
        Some(s) if !s.tool_names.has(raw_flat_name) => unique_namespaced_tool_name(raw_flat_name, s).map(Value::String).or_else(|| raw_flat_name.cloned()),
        _ => raw_flat_name.cloned(),
    };
    let namespaced = state.and_then(|s| flat_name.as_ref().and_then(Value::as_str).and_then(|f| s.namespace_get(f)));
    if state.is_some_and(|s| s.custom_tool_names.has(flat_name.as_ref())) {
        let name = coalesce(namespaced.and_then(|n| n.name.as_ref()), flat_name.as_ref());
        return ResolvedToolCall { kind: ResolvedToolType::CustomToolCall, flat_name, name, namespace: None };
    }
    if let Some(entry) = namespaced {
        return ResolvedToolCall { kind: ResolvedToolType::FunctionCall, name: entry.name.clone(), namespace: entry.namespace.clone(), flat_name };
    }
    ResolvedToolCall { kind: ResolvedToolType::FunctionCall, name: flat_name.clone(), flat_name, namespace: None }
}

/// `customToolArgumentsForModel`: `JSON.stringify({ input })` with non-string
/// input itself JSON-stringified (undefined input → `{}`).
pub fn custom_tool_arguments_for_model(input: Option<&Value>) -> String {
    let inner = match input {
        Some(Value::String(s)) => Some(Value::String(s.clone())),
        other => other.map(|v| Value::String(stringify(v))),
    };
    stringify(&obj(vec![("input", inner)]))
}

/// `customToolInput`: unwraps a model's JSON arguments back into the raw
/// string a custom_tool_call carries (raw patch, `{input}`, `{command:[…]}`,
/// first string property, else the raw text).
pub fn custom_tool_input(raw_arguments: Option<&Value>) -> String {
    let raw = match raw_arguments {
        Some(Value::String(s)) => s,
        None | Some(Value::Null) => return String::new(),
        other => return to_string(other),
    };
    if js_trim(raw).starts_with("*** Begin Patch") { return raw.clone(); }
    let Some(parsed) = parse(raw) else { return raw.clone() };
    match &parsed {
        Value::String(s) => return s.clone(),
        Value::Object(map) => {
            if let Some(Value::String(s)) = map.get("input") { return s.clone(); }
            if let Some(Value::Array(cmd)) = map.get("command") {
                if let Some(Value::String(last)) = cmd.last() { return last.clone(); }
            }
            for v in object_values(map) {
                if let Value::String(s) = v { return s.clone(); }
            }
        }
        Value::Array(items) => {
            // Arrays are objects too: `candidate.input` / `.command` are
            // undefined, and Object.values yields the elements in order.
            for v in items {
                if let Value::String(s) = v { return s.clone(); }
            }
        }
        _ => {}
    }
    raw.clone()
}

fn custom_tool_parameters() -> Value { json!({"type": "object", "properties": {"input": {"type": "string"}}, "required": ["input"]}) }

/// `for (const x of v || [])`.
fn iter_or_empty(v: Option<&Value>, expr: &str) -> EngineResult<Vec<Value>> {
    if !truthy(v) { return Ok(Vec::new()); }
    iterate(v, expr)
}

/// `convertToolsForUnified`: function tools reformatted, built-in tools
/// passed through, namespace tools flattened to `${namespace}__${name}`,
/// custom tools exposed as single-`input: string` function tools.
pub fn convert_tools_for_unified(tools: &[Value], mut state: Option<&mut ResponsesToolState>) -> EngineResult<Vec<Value>> {
    let mut result = Vec::new();
    let push_custom = |result: &mut Vec<Value>, state: &mut Option<&mut ResponsesToolState>, name: Option<&Value>, description: Option<&Value>| {
        if let Some(s) = state.as_deref_mut() {
            s.custom_tool_names.add(name);
            s.tool_names.add(name);
        }
        result.push(json!({"type": "function", "function": obj(vec![
            ("name", name.cloned()),
            ("description", or(description, Some(&json!("")))),
            ("parameters", Some(custom_tool_parameters())),
        ])}));
    };

    for tool in tools {
        let tool_type = prop(Some(tool), "tool.type")?;
        if tool_type.and_then(Value::as_str) == Some("namespace") {
            for sub in &iter_or_empty(tool.get("tools"), "tool.tools || []")? {
                let sub_name = prop(Some(sub), "subTool.name")?;
                let flat = flat_join(tool.get("name"), sub_name);
                if let Some(s) = state.as_deref_mut() {
                    s.namespace_set(flat.clone(), NamespaceEntry { namespace: tool.get("name").cloned(), name: sub_name.cloned() });
                }
                if get_str(sub, &["type"]) == Some("custom") {
                    push_custom(&mut result, &mut state, Some(&Value::String(flat)), sub.get("description"));
                    continue;
                }
                if let Some(s) = state.as_deref_mut() { s.tool_names.add(Some(&Value::String(flat.clone()))); }
                let default_params = json!({"type": "object", "properties": {}, "additionalProperties": false});
                result.push(json!({"type": "function", "function": obj(vec![
                    ("name", Some(Value::String(flat))),
                    ("description", or(sub.get("description"), Some(&json!("")))),
                    ("parameters", or(sub.get("parameters"), Some(&default_params))),
                    ("strict", sub.get("strict").cloned()),
                ])}));
            }
            continue;
        }
        if tool_type.and_then(Value::as_str) == Some("custom") {
            push_custom(&mut result, &mut state, tool.get("name"), tool.get("description"));
            continue;
        }
        if tool_type.and_then(Value::as_str) != Some("function") {
            result.push(tool.clone());
            continue;
        }
        if let Some(s) = state.as_deref_mut() { s.tool_names.add(tool.get("name")); }
        result.push(json!({"type": "function", "function": obj(vec![
            ("name", tool.get("name").cloned()),
            ("description", tool.get("description").cloned()),
            ("parameters", tool.get("parameters").cloned()),
            ("strict", tool.get("strict").cloned()),
        ])}));
    }
    Ok(result)
}

/// `convertToolsForChatCompletions`: function tools only, reformatted.
pub fn convert_tools_for_chat_completions(tools: &[Value]) -> EngineResult<Vec<Value>> {
    let mut out = Vec::new();
    for tool in tools {
        if prop(Some(tool), "tool.type")?.and_then(Value::as_str) != Some("function") { continue; }
        out.push(json!({"type": "function", "function": obj(vec![
            ("name", tool.get("name").cloned()),
            ("description", tool.get("description").cloned()),
            ("parameters", tool.get("parameters").cloned()),
            ("strict", tool.get("strict").cloned()),
        ])}));
    }
    Ok(out)
}

/// `convertToolChoiceForChatCompletions`.
pub fn convert_tool_choice_for_chat_completions(tool_choice: Option<&Value>) -> Value {
    match tool_choice {
        Some(Value::String(s)) => Value::String(s.clone()),
        Some(tc) if get_str(tc, &["type"]) == Some("function") => json!({"type": "function", "function": obj(vec![("name", tc.get("name").cloned())])}),
        _ => json!("auto"),
    }
}

/// `buildToolOutputItem`: one Chat-style tool call → Responses output item
/// (namespace split-back; custom tools → custom_tool_call with unwrapped input).
pub fn build_tool_output_item(tool_call: &Value, state: Option<&ResponsesToolState>) -> EngineResult<Value> {
    let function = prop(Some(tool_call), "toolCall.function")?;
    let name = prop(function, "toolCall.function.name")?;
    let resolved = resolve_output_tool_call(name, state);
    let arguments = function.and_then(|f| f.get("arguments"));
    if resolved.kind == ResolvedToolType::CustomToolCall {
        return Ok(obj(vec![
            ("type", Some(json!("custom_tool_call"))),
            ("id", Some(Value::String(generate_item_id("fc")))),
            ("status", Some(json!("completed"))),
            ("call_id", tool_call.get("id").cloned()),
            ("name", resolved.name),
            ("input", Some(Value::String(custom_tool_input(arguments)))),
        ]));
    }
    Ok(obj(vec![
        ("type", Some(json!("function_call"))),
        ("id", Some(Value::String(generate_item_id("fc")))),
        ("status", Some(json!("completed"))),
        ("call_id", tool_call.get("id").cloned()),
        ("name", resolved.name),
        ("namespace", resolved.namespace.filter(|n| truthy(Some(n)))),
        ("arguments", arguments.cloned()),
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_tool_input_shapes() {
        let patch = "*** Begin Patch\n*** End Patch";
        assert_eq!(custom_tool_input(Some(&json!(patch))), patch);
        assert_eq!(custom_tool_input(Some(&json!(stringify(&json!({"input": patch}))))), patch);
        assert_eq!(custom_tool_input(Some(&json!(stringify(&json!({"command": ["apply_patch", patch]}))))), patch);
        assert_eq!(custom_tool_input(Some(&json!(r#"{"a":1,"b":"x"}"#))), "x");
        assert_eq!(custom_tool_input(Some(&json!(r#"{"b":"late","1":"first"}"#))), "first");
        assert_eq!(custom_tool_input(Some(&json!("\"s\""))), "s");
        assert_eq!(custom_tool_input(Some(&json!("not json"))), "not json");
        assert_eq!(custom_tool_input(None), "");
        assert_eq!(custom_tool_input(Some(&json!(5))), "5");
        assert_eq!(custom_tool_arguments_for_model(Some(&json!("ls"))), r#"{"input":"ls"}"#);
        assert_eq!(custom_tool_arguments_for_model(Some(&json!({"a": 1}))), r#"{"input":"{\"a\":1}"}"#);
        assert_eq!(custom_tool_arguments_for_model(None), "{}");
    }

    #[test]
    fn namespace_and_custom_resolution() {
        let mut state = ResponsesToolState::new();
        let tools = vec![
            json!({"type": "namespace", "name": "functions", "tools": [{"type": "custom", "name": "exec", "description": "Run code"}, {"type": "function", "name": "wait", "parameters": {"type": "object", "properties": {}}}]}),
            json!({"type": "custom", "name": "apply_patch"}),
            json!({"type": "web_search"}),
            json!({"type": "function", "name": "ls"}),
        ];
        let out = convert_tools_for_unified(&tools, Some(&mut state)).unwrap();
        assert_eq!(stringify(&out[0]), r#"{"type":"function","function":{"name":"functions__exec","description":"Run code","parameters":{"type":"object","properties":{"input":{"type":"string"}},"required":["input"]}}}"#);
        assert_eq!(stringify(&out[1]), r#"{"type":"function","function":{"name":"functions__wait","description":"","parameters":{"type":"object","properties":{}}}}"#);
        assert_eq!(out[3], json!({"type": "web_search"}));
        assert_eq!(out[4], json!({"type": "function", "function": {"name": "ls"}}));

        let r = resolve_output_tool_call(Some(&json!("exec")), Some(&state));
        assert_eq!((r.kind, r.name), (ResolvedToolType::CustomToolCall, Some(json!("exec"))));
        let r = resolve_output_tool_call(Some(&json!("functions__wait")), Some(&state));
        assert_eq!((r.kind, r.name, r.namespace), (ResolvedToolType::FunctionCall, Some(json!("wait")), Some(json!("functions"))));
        assert_eq!(resolve_history_tool_name(Some(&json!("exec")), None, Some(&state)), Some(json!("functions__exec")));
        assert_eq!(resolve_history_tool_name(Some(&json!("functions.wait")), None, Some(&state)), Some(json!("functions__wait")));
        assert_eq!(resolve_history_tool_name(Some(&json!("x")), Some(&json!("ns")), None), Some(json!("ns__x")));
        assert_eq!(resolve_history_tool_name(Some(&json!("x.y")), None, Some(&state)), Some(json!("x.y")));
        assert!(convert_tools_for_unified(&[Value::Null], None).is_err());
        assert_eq!(convert_tool_choice_for_chat_completions(Some(&json!({"type": "function", "name": "f"}))), json!({"type": "function", "function": {"name": "f"}}));
        assert_eq!(convert_tool_choice_for_chat_completions(None), json!("auto"));
    }
}
