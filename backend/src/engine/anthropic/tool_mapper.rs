//! `transformers/anthropic/tool-mapper.ts`.
use super::helpers::type_error;
use crate::engine::js::{coalesce, get, get_str, obj, truthy, Obj};
use crate::engine::transformer::EngineResult;
use serde_json::{json, Value};

/// Anthropic built-in tool type strings that must be passed through as-is
/// (server-side tools recognised by their `type`, e.g. web search); they must
/// NOT be mapped to the custom function schema.
pub const ANTHROPIC_BUILTIN_TOOL_TYPES: &[&str] = &["web_search_20250305"];

fn is_builtin(t: &Value) -> bool {
    truthy(t.get("type")) && get_str(t, &["type"]).is_some_and(|ty| ANTHROPIC_BUILTIN_TOOL_TYPES.contains(&ty))
}

/// Own enumerable properties of a JS value as a spread source (`{...v}`).
fn spread_entries(v: &Value) -> Obj {
    match v {
        Value::Object(map) => map.clone(),
        Value::Array(items) => items.iter().enumerate().map(|(i, x)| (i.to_string(), x.clone())).collect(),
        Value::String(s) => s.chars().enumerate().map(|(i, c)| (i.to_string(), Value::String(c.to_string()))).collect(),
        _ => Obj::new(),
    }
}

/// `convertAnthropicToolsToUnified`: `{ name, description, input_schema }` →
/// `{ type: "function", function: { name, description, parameters } }`.
///
/// Built-in Anthropic tools pass through verbatim. Every other field (e.g.
/// `eager_input_streaming`, `cache_control`, `type`) is carried on
/// `_anthropicExtras` so Anthropic-native tool options survive a
/// same-format (messages → messages) round-trip.
pub fn convert_anthropic_tools_to_unified(tools: &[Value]) -> EngineResult<Vec<Value>> {
    tools.iter().map(|t| {
        if t.is_null() { return Err(type_error(Some(t), "t.type")); }
        if is_builtin(t) { return Ok(t.clone()); }
        let mut rest = spread_entries(t);
        for k in ["name", "description", "input_schema"] { rest.shift_remove(k); }
        let mut tool = json!({"type": "function", "function": obj(vec![
            ("name", t.get("name").cloned()),
            ("description", t.get("description").cloned()),
            ("parameters", t.get("input_schema").cloned()),
        ])});
        if !rest.is_empty() { tool["_anthropicExtras"] = Value::Object(rest); }
        Ok(tool)
    }).collect()
}

/// `convertUnifiedToolsToAnthropic`: unified function tools →
/// `{ name, description, input_schema }`, re-attaching `_anthropicExtras`.
/// Built-in Anthropic tools pass through unchanged.
pub fn convert_unified_tools_to_anthropic(tools: &[Value]) -> EngineResult<Vec<Value>> {
    tools.iter().map(|t| {
        if t.is_null() { return Err(type_error(Some(t), "t.type")); }
        if is_builtin(t) { return Ok(t.clone()); }
        let f = |k: &str| get(t, &["function", k]);
        let mut result = Obj::new();
        result.insert("name".into(), coalesce(f("name"), None).unwrap_or_else(|| json!("")));
        result.insert("description".into(), coalesce(f("description"), None).unwrap_or_else(|| json!("")));
        result.insert("input_schema".into(), coalesce(f("parameters"), None).unwrap_or_else(|| json!({})));
        if let Some(extras) = t.get("_anthropicExtras").filter(|e| truthy(Some(e)) && (e.is_object() || e.is_array())) {
            for (k, v) in spread_entries(extras) { result.insert(k, v); }
        }
        Ok(Value::Object(result))
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::js::stringify;

    #[test]
    fn round_trips_extras_and_builtins() {
        let tools = vec![
            json!({"name": "get", "description": "d", "input_schema": {"type": "object"}, "cache_control": {"type": "ephemeral"}, "eager_input_streaming": true}),
            json!({"type": "web_search_20250305", "name": "web_search", "max_uses": 3}),
            json!({"name": "bare"}),
        ];
        let unified = convert_anthropic_tools_to_unified(&tools).unwrap();
        assert_eq!(stringify(&unified[0]), r#"{"type":"function","function":{"name":"get","description":"d","parameters":{"type":"object"}},"_anthropicExtras":{"cache_control":{"type":"ephemeral"},"eager_input_streaming":true}}"#);
        assert_eq!(unified[1], tools[1]);
        assert_eq!(unified[2], json!({"type": "function", "function": {"name": "bare"}}));
        let back = convert_unified_tools_to_anthropic(&unified).unwrap();
        assert_eq!(stringify(&back[0]), r#"{"name":"get","description":"d","input_schema":{"type":"object"},"cache_control":{"type":"ephemeral"},"eager_input_streaming":true}"#);
        assert_eq!(back[1], tools[1]);
        assert_eq!(back[2], json!({"name": "bare", "description": "", "input_schema": {}}));
    }
}
