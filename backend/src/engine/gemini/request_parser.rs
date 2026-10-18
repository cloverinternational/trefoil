//! `transformers/gemini/request-parser.ts`: client Gemini request → unified.
use super::part_mapper::convert_gemini_parts_to_unified;
use super::utils::*;
use crate::engine::js::*;
use crate::engine::transformer::EngineResult;
use serde_json::{json, Map, Value};

/// `contentParts.length === 1 && first.type === 'text' ? first.text : …`.
fn simplify_content(parts: Vec<Value>, empty: Value) -> Value {
    if parts.len() == 1 && get_str(&parts[0], &["type"]) == Some("text") {
        return parts[0].get("text").cloned().unwrap_or(Value::Null);
    }
    if parts.is_empty() {
        empty
    } else {
        Value::Array(parts)
    }
}

/// `for (const x of v)` over a JSON value: arrays yield items, strings yield
/// code points, anything else is not iterable.
fn iterate(v: &Value, expr: &str) -> EngineResult<Vec<Value>> {
    match v {
        Value::Array(items) => Ok(items.clone()),
        Value::String(s) => Ok(s.chars().map(|c| Value::String(c.to_string())).collect()),
        _ => Err(type_error(format!("{expr} is not iterable"))),
    }
}

fn map_tools(tools: &[Value]) -> EngineResult<Vec<Value>> {
    let mut unified = Vec::new();
    for tool in tools {
        let fds = read(Some(tool), "functionDeclarations")?;
        if truthy(fds) {
            for fd in iterate(fds.unwrap_or(&Value::Null), "tool.functionDeclarations")? {
                if fd.is_null() {
                    return Err(read_error(Some(&fd), "name"));
                }
                unified.push(json!({"type": "function", "function": obj(vec![
                    ("name", fd.get("name").cloned()),
                    ("description", fd.get("description").cloned()),
                    ("parametersJsonSchema", fd.get("parametersJsonSchema").cloned()),
                    ("parameters", fd.get("parameters").cloned()),
                ])}));
            }
        }
        if truthy(tool.get("googleSearch")) {
            unified.push(json!({"type": "googleSearch", "googleSearch": {}}));
        }
        if truthy(tool.get("codeExecution")) {
            unified.push(json!({"type": "codeExecution", "codeExecution": {}}));
        }
        if truthy(tool.get("urlContext")) {
            unified.push(json!({"type": "urlContext", "urlContext": {}}));
        }
        if matches!(
            get_str(tool, &["type"]),
            Some("web_search_20250305" | "web_search" | "openrouter:web_search")
        ) {
            unified.push(json!({"type": "googleSearch", "googleSearch": {}}));
        }
    }
    Ok(unified)
}

/// Unified message under construction (`UnifiedMessage`).
struct Thinking {
    content: String,
    signature: Option<Value>,
}

/// `parseGeminiRequest`.
pub fn parse_gemini_request(input: &Value) -> EngineResult<Value> {
    let contents = or(read(Some(input), "contents")?, None).unwrap_or_else(|| json!([]));
    let tools = or(input.get("tools"), None).unwrap_or_else(|| json!([]));
    let model = or(input.get("model"), None).unwrap_or_else(|| json!(""));
    let gc = or(input.get("generationConfig"), None).unwrap_or_else(|| json!({}));
    let system_instruction = input.get("systemInstruction");

    // Key order follows the TS object literal (`messages` is reassigned at the end).
    let mut out = Map::new();
    out.insert("messages".into(), json!([]));
    out.insert("model".into(), model);
    set_opt(&mut out, "max_tokens", gc.get("maxOutputTokens").cloned());
    set_opt(&mut out, "temperature", gc.get("temperature").cloned());
    out.insert("stream".into(), json!(truthy(input.get("stream"))));

    // Gap 1: systemInstruction (inbound); thought parts are dropped.
    if truthy(system_instruction) && truthy(get(system_instruction.unwrap(), &["parts"])) {
        let mut noop = |_: &Value, _: Option<&Value>| {};
        let parts = convert_gemini_parts_to_unified(
            &system_instruction.unwrap()["parts"],
            Some(&mut noop),
        )?;
        out.insert(
            "systemInstruction".into(),
            json!({"role": "system", "content": simplify_content(parts, json!([]))}),
        );
    }

    // Response format.
    if get_str(&gc, &["responseMimeType"]) == Some("application/json") {
        let schema = gc.get("responseJsonSchema").filter(|s| truthy(Some(s)));
        out.insert(
            "response_format".into(),
            obj(vec![
                (
                    "type",
                    Some(json!(if schema.is_some() {
                        "json_schema"
                    } else {
                        "json_object"
                    })),
                ),
                ("json_schema", schema.map(normalize_json_schema_types)),
            ]),
        );
    }

    // Thinking config (camelCase and snake_case variants).
    if truthy(gc.get("thinkingConfig")) {
        let tc = &gc["thinkingConfig"];
        let level = coalesce(tc.get("thinkingLevel"), tc.get("thinking_level"));
        let budget = coalesce(tc.get("thinkingBudget"), tc.get("thinking_budget"));
        let budget_is = |n: f64| budget.as_ref().and_then(Value::as_f64) == Some(n);
        let enabled = if budget_is(0.0) {
            Some(json!(false))
        } else {
            coalesce(tc.get("includeThoughts"), tc.get("include_thoughts"))
        };
        let effort = match level.as_ref().filter(|l| truthy(Some(l))) {
            Some(l) => Some(Value::String(lower(l, "thinkingLevel")?)),
            None => None,
        };
        out.insert(
            "reasoning".into(),
            obj(vec![
                ("enabled", enabled),
                ("max_tokens", budget.clone()),
                ("effort", effort),
                (
                    "adaptive",
                    if budget_is(-1.0) {
                        Some(json!(true))
                    } else {
                        None
                    },
                ),
            ]),
        );
    }

    // Gap 3: toolConfig.
    if truthy(input.get("toolConfig")) {
        out.insert(
            "toolConfig".into(),
            obj(vec![
                (
                    "mode",
                    get(input, &["toolConfig", "functionCallingConfig", "mode"]).cloned(),
                ),
                (
                    "functionCallingPreference",
                    get(
                        input,
                        &[
                            "toolConfig",
                            "functionCallingConfig",
                            "functionCallingPreference",
                        ],
                    )
                    .cloned(),
                ),
            ]),
        );
    }

    // Gap 4 & 5: function declarations and Google built-in tools.
    if let Some(list) = tools.as_array().filter(|t| !t.is_empty()) {
        let unified = map_tools(list)?;
        if !unified.is_empty() {
            out.insert("tools".into(), Value::Array(unified));
        }
    }

    // Contents → messages.
    let mut messages: Vec<Value> = Vec::new();
    if let Some(contents) = contents.as_array() {
        for content in contents {
            let role = if read(Some(content), "role")?.and_then(Value::as_str) == Some("model") {
                "assistant"
            } else {
                "user"
            };
            let Some(parts) = content.get("parts").filter(|p| truthy(Some(p))) else {
                continue;
            };

            let mut thinking: Option<Thinking> = None;
            let mut on_thinking = |text: &Value, signature: Option<&Value>| {
                let t = thinking.get_or_insert_with(|| Thinking {
                    content: String::new(),
                    signature: None,
                });
                t.content.push_str(&to_string(Some(text)));
                // Gap 6: only base64-valid signatures are kept.
                if truthy(signature) && is_valid_thought_signature_value(signature) {
                    t.signature = signature.cloned();
                }
            };
            let content_parts = convert_gemini_parts_to_unified(parts, Some(&mut on_thinking))?;
            let parts = parts.as_array().cloned().unwrap_or_default();

            let mut tool_calls: Option<Vec<Value>> = None;
            for part in &parts {
                if !truthy(part.get("functionCall")) {
                    continue;
                }
                let fc = &part["functionCall"];
                let name = fc.get("name");
                let id = if truthy(name) {
                    name.cloned().unwrap_or(Value::Null)
                } else {
                    json!(random_call_id())
                };
                let mut tc = obj(vec![
                    ("id", Some(id)),
                    ("type", Some(json!("function"))),
                    (
                        "function",
                        Some(obj(vec![
                            (
                                "name",
                                Some(or(name, Some(&json!("unknown"))).unwrap_or(Value::Null)),
                            ),
                            ("arguments", stringify_opt(fc.get("args"))),
                        ])),
                    ),
                ]);
                if truthy(part.get("thoughtSignature")) {
                    tc["thought_signature"] = part["thoughtSignature"].clone();
                }
                tool_calls.get_or_insert_with(Vec::new).push(tc);
            }

            let has_content_parts = !content_parts.is_empty();
            let mut message = Map::new();
            message.insert("role".into(), json!(role));
            message.insert(
                "content".into(),
                simplify_content(content_parts, Value::Null),
            );
            if let Some(t) = thinking {
                message.insert(
                    "thinking".into(),
                    obj(vec![
                        ("content", Some(Value::String(t.content))),
                        ("signature", t.signature),
                    ]),
                );
            }
            if let Some(tcs) = tool_calls {
                message.insert("tool_calls".into(), Value::Array(tcs));
            }

            let function_responses: Vec<&Value> = parts
                .iter()
                .filter(|p| truthy(p.get("functionResponse")))
                .collect();
            if !function_responses.is_empty() {
                for fr in function_responses {
                    let name = get(fr, &["functionResponse", "name"]);
                    messages.push(obj(vec![
                        ("role", Some(json!("tool"))),
                        (
                            "content",
                            stringify_opt(get(fr, &["functionResponse", "response"])),
                        ),
                        (
                            "tool_call_id",
                            Some(or(name, Some(&json!("unknown_tool"))).unwrap_or(Value::Null)),
                        ),
                        ("name", name.cloned()),
                    ]));
                }
                if has_content_parts {
                    messages.push(Value::Object(message));
                }
            } else {
                messages.push(Value::Object(message));
            }
        }
    }

    // Merge consecutive assistant messages (parallel tool calls arrive as
    // separate model contents).
    let mut merged: Vec<Value> = Vec::new();
    for msg in messages {
        let prev_is_assistant = merged
            .last()
            .is_some_and(|p| get_str(p, &["role"]) == Some("assistant"));
        if !(prev_is_assistant && get_str(&msg, &["role"]) == Some("assistant")) {
            merged.push(msg);
            continue;
        }
        let prev = merged
            .last_mut()
            .and_then(Value::as_object_mut)
            .expect("assistant message object");
        if let Some(tcs) = get_arr(&msg, &["tool_calls"]).filter(|t| !t.is_empty()) {
            if !truthy(prev.get("tool_calls")) {
                prev.insert("tool_calls".into(), json!([]));
            }
            if let Some(Value::Array(p)) = prev.get_mut("tool_calls") {
                p.extend(tcs.iter().cloned());
            }
        }
        let mc = msg.get("content");
        if truthy(mc) && truthy(prev.get("content")) {
            let merged_content = match (prev.get("content"), mc) {
                (Some(Value::String(a)), Some(Value::String(b))) => {
                    Some(Value::String(format!("{a}{b}")))
                }
                (Some(Value::Array(a)), Some(Value::Array(b))) => {
                    Some(Value::Array(a.iter().chain(b.iter()).cloned().collect()))
                }
                _ => None,
            };
            if let Some(c) = merged_content {
                prev.insert("content".into(), c);
            }
        } else if truthy(mc) && !truthy(prev.get("content")) {
            prev.insert("content".into(), mc.cloned().unwrap_or(Value::Null));
        }
        if truthy(msg.get("thinking")) {
            if !truthy(prev.get("thinking")) {
                prev.insert("thinking".into(), msg["thinking"].clone());
            } else {
                let pt = prev.get_mut("thinking").expect("thinking");
                let joined = format!(
                    "{}{}",
                    to_string(Some(
                        &or(pt.get("content"), Some(&json!(""))).unwrap_or(Value::Null)
                    )),
                    to_string(Some(
                        &or(get(&msg, &["thinking", "content"]), Some(&json!("")))
                            .unwrap_or(Value::Null)
                    )),
                );
                if let Some(m) = pt.as_object_mut() {
                    m.insert("content".into(), Value::String(joined));
                    if truthy(get(&msg, &["thinking", "signature"])) {
                        m.insert("signature".into(), msg["thinking"]["signature"].clone());
                    }
                }
            }
        }
    }

    // Propagate the first thought signature to unsigned sibling tool calls.
    for msg in merged.iter_mut() {
        if get_str(msg, &["role"]) != Some("assistant") {
            continue;
        }
        let Some(Value::Array(tcs)) = msg.get_mut("tool_calls") else {
            continue;
        };
        if tcs.len() < 2 {
            continue;
        }
        let shared = tcs
            .iter()
            .find(|tc| truthy(tc.get("thought_signature")))
            .map(|tc| tc["thought_signature"].clone());
        if let Some(sig) = shared {
            for tc in tcs.iter_mut() {
                if !truthy(tc.get("thought_signature")) {
                    if let Some(m) = tc.as_object_mut() {
                        m.insert("thought_signature".into(), sig.clone());
                    }
                }
            }
        }
    }

    out.insert("messages".into(), Value::Array(merged));
    Ok(Value::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_req(v: Value) -> Value {
        parse_gemini_request(&v).unwrap()
    }

    #[test]
    fn system_instruction() {
        let r = parse_req(
            json!({"systemInstruction": {"role": "system", "parts": [{"text": "You are a helpful assistant."}]}, "contents": [{"role": "user", "parts": [{"text": "Hello"}]}], "model": "gemini-2.0-flash"}),
        );
        assert_eq!(
            r["systemInstruction"],
            json!({"role": "system", "content": "You are a helpful assistant."})
        );
        let r = parse_req(
            json!({"systemInstruction": {"parts": [{"text": "a"}, {"text": "b"}]}, "contents": [], "model": "m"}),
        );
        assert!(r["systemInstruction"]["content"].is_array());
        let r = parse_req(
            json!({"contents": [{"role": "user", "parts": [{"text": "Hello"}]}], "model": "gemini-2.0-flash"}),
        );
        assert!(r.get("systemInstruction").is_none());
        assert_eq!(
            r,
            json!({"messages": [{"role": "user", "content": "Hello"}], "model": "gemini-2.0-flash", "stream": false})
        );
    }

    #[test]
    fn tool_config_and_tools() {
        for mode in ["auto", "none", "any"] {
            let r = parse_req(
                json!({"contents": [], "model": "m", "toolConfig": {"functionCallingConfig": {"mode": mode}}}),
            );
            assert_eq!(r["toolConfig"], json!({"mode": mode}));
        }
        let r = parse_req(
            json!({"contents": [], "toolConfig": {"functionCallingConfig": {"mode": "auto", "functionCallingPreference": "auto"}}}),
        );
        assert_eq!(r["toolConfig"]["functionCallingPreference"], "auto");
        assert_eq!(r["model"], "");
        let r = parse_req(json!({"contents": [], "tools": [
            {"functionDeclarations": [{"name": "my_function", "description": "A function", "parametersJsonSchema": {"type": "object"}, "parameters": {"type": "object"}}]},
            {"googleSearch": {}}, {"codeExecution": {}}, {"urlContext": {}}, {"type": "web_search"}
        ]}));
        let tools = r["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 5);
        assert_eq!(
            tools[0]["function"]["parametersJsonSchema"]["type"],
            "object"
        );
        assert_eq!(tools[0]["function"]["parameters"]["type"], "object");
        assert_eq!(
            tools
                .iter()
                .map(|t| t["type"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec![
                "function",
                "googleSearch",
                "codeExecution",
                "urlContext",
                "googleSearch"
            ]
        );
    }

    #[test]
    fn thinking_config() {
        let r = parse_req(
            json!({"contents": [], "generationConfig": {"thinkingConfig": {"include_thoughts": true, "thinking_level": "MEDIUM"}}}),
        );
        assert_eq!(r["reasoning"], json!({"enabled": true, "effort": "medium"}));
        let r = parse_req(
            json!({"contents": [], "generationConfig": {"thinkingConfig": {"include_thoughts": true, "thinking_budget": 8192}}}),
        );
        assert_eq!(r["reasoning"], json!({"enabled": true, "max_tokens": 8192}));
        let r = parse_req(
            json!({"contents": [], "generationConfig": {"thinkingConfig": {"includeThoughts": true, "thinkingBudget": 4096}}}),
        );
        assert_eq!(r["reasoning"], json!({"enabled": true, "max_tokens": 4096}));
        let r = parse_req(
            json!({"contents": [], "generationConfig": {"thinkingConfig": {"thinkingBudget": -1}}}),
        );
        assert_eq!(r["reasoning"], json!({"max_tokens": -1, "adaptive": true}));
        let r = parse_req(
            json!({"contents": [], "generationConfig": {"thinkingConfig": {"thinkingBudget": 0, "includeThoughts": true}}}),
        );
        assert_eq!(r["reasoning"], json!({"enabled": false, "max_tokens": 0}));
        let r = parse_req(json!({"contents": [], "generationConfig": {}}));
        assert!(r.get("reasoning").is_none());
    }

    #[test]
    fn response_format() {
        let r = parse_req(
            json!({"contents": [], "generationConfig": {"responseMimeType": "application/json", "responseJsonSchema": {"type": "OBJECT"}, "maxOutputTokens": 10, "temperature": 0.5}}),
        );
        assert_eq!(
            r["response_format"],
            json!({"type": "json_schema", "json_schema": {"type": "object"}})
        );
        assert_eq!(r["max_tokens"], 10);
        assert_eq!(r["temperature"], 0.5);
        let r = parse_req(
            json!({"contents": [], "generationConfig": {"responseMimeType": "application/json"}}),
        );
        assert_eq!(r["response_format"], json!({"type": "json_object"}));
    }

    #[test]
    fn contents_function_calls_and_merging() {
        let r = parse_req(json!({"model": "m", "stream": true, "contents": [
            {"role": "user", "parts": [{"text": "weather?"}]},
            {"role": "model", "parts": [{"text": "plan", "thought": true, "thoughtSignature": "bad!"}, {"text": "more", "thought": true, "thoughtSignature": "aGVsbG8="}, {"functionCall": {"name": "a", "args": {"x": 1}}, "thoughtSignature": "c2ln"}]},
            {"role": "model", "parts": [{"functionCall": {"name": "b", "args": {}}}]},
            {"role": "user", "parts": [{"functionResponse": {"name": "a", "response": {"ok": true}}}, {"functionResponse": {"response": 1}}]},
            {"role": "model", "parts": [{"text": "x"}]},
            {"role": "model", "parts": [{"text": "y"}]},
            {"role": "model", "parts": [{"functionCall": {"args": null}}, {"functionResponse": {"name": "z"}}]}
        ]}));
        assert_eq!(r["stream"], true);
        let m = r["messages"].as_array().unwrap();
        assert_eq!(m.len(), 6);
        assert_eq!(
            m[1],
            json!({"role": "assistant", "content": null,
            "thinking": {"content": "planmore", "signature": "aGVsbG8="},
            "tool_calls": [
                {"id": "a", "type": "function", "function": {"name": "a", "arguments": "{\"x\":1}"}, "thought_signature": "c2ln"},
                {"id": "b", "type": "function", "function": {"name": "b", "arguments": "{}"}, "thought_signature": "c2ln"}
            ]})
        );
        assert_eq!(
            m[2],
            json!({"role": "tool", "content": "{\"ok\":true}", "tool_call_id": "a", "name": "a"})
        );
        assert_eq!(
            m[3],
            json!({"role": "tool", "content": "1", "tool_call_id": "unknown_tool"})
        );
        assert_eq!(m[4], json!({"role": "assistant", "content": "xy"}));
        // functionResponse with no content parts drops the model message (and its tool call).
        assert_eq!(
            m[5],
            json!({"role": "tool", "tool_call_id": "z", "name": "z"})
        );
    }

    #[test]
    fn unnamed_function_call_gets_random_id() {
        let r = parse_req(
            json!({"contents": [{"role": "model", "parts": [{"functionCall": {"args": {"a": 1}}}]}]}),
        );
        let tc = &r["messages"][0]["tool_calls"][0];
        assert!(tc["id"].as_str().unwrap().starts_with("call_"));
        assert_eq!(tc["function"]["name"], "unknown");
    }
}
