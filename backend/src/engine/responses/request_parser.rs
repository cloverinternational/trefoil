//! `transformers/responses/request-parser.ts`: client Responses request →
//! unified request.
use super::js_ext::{join, spread_iter, stringify_opt};
use super::tool_mapper::{
    convert_tool_choice_for_chat_completions, convert_tools_for_unified, custom_tool_arguments_for_model,
    resolve_history_tool_name, ResponsesToolState,
};
use crate::engine::js::*;
use crate::engine::transformer::{EngineError, EngineResult};
use serde_json::{json, Value};

/// `normalizeInput`: a plain string becomes one user message item.
pub fn normalize_input(input: &Value) -> Value {
    match input {
        Value::String(text) => json!([{"type": "message", "role": "user", "content": [{"type": "input_text", "text": text}]}]),
        other => other.clone(),
    }
}

/// `mapInputRole`.
pub fn map_input_role(role: Option<&Value>) -> &'static str {
    match role.and_then(Value::as_str) {
        Some("system") | Some("developer") => "system",
        Some("assistant") => "assistant",
        Some("tool") => "tool",
        _ => "user",
    }
}

/// `typeof x === 'object' && x !== null` (arrays included).
fn is_object_like(v: &Value) -> bool { v.is_object() || v.is_array() }

/// `convertContentParts`: Responses content parts → Chat parts (a lone
/// input/output text part collapses to its text; `None` = undefined text).
pub fn convert_content_parts(raw_parts: &[Value]) -> Option<Value> {
    let parts: Vec<&Value> = raw_parts.iter().filter(|p| is_object_like(p)).collect();
    if parts.len() == 1 && matches!(get_str(parts[0], &["type"]), Some("input_text") | Some("output_text")) {
        return parts[0].get("text").cloned();
    }
    Some(Value::Array(
        parts
            .into_iter()
            .map(|part| match get_str(part, &["type"]) {
                Some("input_text") | Some("output_text") | Some("summary_text") => obj(vec![("type", Some(json!("text"))), ("text", part.get("text").cloned())]),
                Some("input_image") => json!({"type": "image_url", "image_url": obj(vec![
                    ("url", part.get("image_url").cloned()),
                    ("detail", part.get("detail").cloned()),
                ])}),
                _ => part.clone(),
            })
            .collect(),
    ))
}

/// `normalizeMessageContent`: string, parts array, or null (`None` = undefined).
pub fn normalize_message_content(content: Option<&Value>) -> Option<Value> {
    match content {
        Some(Value::String(s)) => Some(Value::String(s.clone())),
        Some(Value::Array(parts)) => convert_content_parts(parts),
        _ => Some(Value::Null),
    }
}

fn message(role: &str, content: Option<Value>) -> Value { obj(vec![("role", Some(json!(role))), ("content", content)]) }

fn tool_call_message(call_id: Option<&Value>, name: Option<Value>, arguments: Option<Value>) -> Value {
    let tool_call = obj(vec![
        ("id", call_id.cloned()),
        ("type", Some(json!("function"))),
        ("function", Some(obj(vec![("name", name), ("arguments", arguments)]))),
    ]);
    json!({"role": "assistant", "content": null, "tool_calls": [tool_call]})
}

/// `convertInputItemsToMessages`: Responses input items → unified messages.
pub fn convert_input_items_to_messages(items: &[Value], mut state: Option<&mut ResponsesToolState>) -> EngineResult<Vec<Value>> {
    let mut messages = Vec::new();
    for item in items {
        if !item.is_object() && !item.is_array() { continue; }
        match get_str(item, &["type"]) {
            Some("message") => messages.push(message(map_input_role(item.get("role")), normalize_message_content(item.get("content")))),
            Some("function_call") => {
                let flat = resolve_history_tool_name(item.get("name"), item.get("namespace"), state.as_deref());
                messages.push(tool_call_message(item.get("call_id"), flat, item.get("arguments").cloned()));
            }
            Some("custom_tool_call") => {
                let flat = resolve_history_tool_name(item.get("name"), item.get("namespace"), state.as_deref());
                if let Some(s) = state.as_deref_mut() { s.custom_tool_names.add(flat.as_ref()); }
                let args = custom_tool_arguments_for_model(item.get("input"));
                messages.push(tool_call_message(item.get("call_id"), flat, Some(Value::String(args))));
            }
            Some("function_call_output") | Some("custom_tool_call_output") => {
                let output = item.get("output");
                let content = match output {
                    Some(Value::String(s)) => Some(Value::String(s.clone())),
                    _ => {
                        let text = output.and_then(|o| o.get("text"));
                        if truthy(text) { text.cloned() } else { stringify_opt(output).map(Value::String) }
                    }
                };
                messages.push(obj(vec![("role", Some(json!("tool"))), ("tool_call_id", item.get("call_id").cloned()), ("content", content)]));
            }
            Some("reasoning") => {
                let summary = item.get("summary");
                // `item.summary && item.summary.length > 0`
                let non_empty = truthy(summary)
                    && match summary {
                        Some(Value::Array(a)) => !a.is_empty(),
                        Some(Value::String(s)) => !s.is_empty(),
                        _ => false,
                    };
                if non_empty {
                    let Some(Value::Array(parts)) = summary else {
                        return Err(EngineError::new(
                            "item.summary.filter is not a function. (In 'item.summary.filter((part) => part !== null && typeof part === \"object\")', 'item.summary.filter' is undefined)",
                        ));
                    };
                    let texts: Vec<Option<&Value>> = parts.iter().filter(|p| is_object_like(p)).map(|p| p.get("text")).collect();
                    messages.push(message("assistant", Some(Value::String(join(&texts, "\n")))));
                }
            }
            // Already lifted into the tool list in parseRequest; not a message.
            Some("additional_tools") => {}
            _ => {
                if truthy(item.get("role")) {
                    messages.push(message(map_input_role(item.get("role")), normalize_message_content(item.get("content"))));
                }
            }
        }
    }
    Ok(messages)
}

/// `parseResponsesRequest`: client Responses body → unified request. Resets
/// and repopulates the per-request tool state.
pub fn parse_responses_request(input: &Value, mut state: Option<&mut ResponsesToolState>) -> EngineResult<Value> {
    if !truthy(input.get("model")) { return Err(EngineError::new("Missing required field: model")); }
    if !truthy(input.get("input")) { return Err(EngineError::new("Missing required field: input")); }
    if let Some(s) = state.as_deref_mut() { s.clear(); }

    let normalized = normalize_input(&input["input"]);
    let Value::Array(normalized) = normalized else {
        return Err(EngineError::new(
            "normalizedInput.filter is not a function. (In 'normalizedInput.filter((item) => item?.type === \"additional_tools\" && Array.isArray(item.tools))', 'normalizedInput.filter' is undefined)",
        ));
    };

    // Codex CLI "lite" mode: tool definitions in `additional_tools` items.
    let lifted: Vec<Value> = normalized
        .iter()
        .filter(|item| get_str(item, &["type"]) == Some("additional_tools") && get_arr(item, &["tools"]).is_some())
        .flat_map(|item| item["tools"].as_array().cloned().unwrap_or_default())
        .collect();

    let mut all_tools: Vec<Value> = match input.get("tools") {
        Some(t) if truthy(Some(t)) => spread_iter(t)?,
        _ => Vec::new(),
    };
    all_tools.extend(lifted);
    // Tools first: history items resolve against the namespace/custom names.
    let tools = convert_tools_for_unified(&all_tools, state.as_deref_mut())?;
    let mut messages = convert_input_items_to_messages(&normalized, state)?;

    if truthy(input.get("instructions")) {
        messages.insert(0, json!({"role": "system", "content": input["instructions"]}));
    }

    let response_format = get(input, &["text", "format"]).filter(|f| truthy(Some(f))).map(|format| {
        obj(vec![
            ("type", format.get("type").cloned()),
            ("json_schema", format.get("schema").cloned()),
            ("name", format.get("name").cloned()),
            ("description", format.get("description").cloned()),
            ("strict", format.get("strict").cloned()),
        ])
    });

    Ok(obj(vec![
        ("requestId", input.get("requestId").cloned()),
        ("model", input.get("model").cloned()),
        ("messages", Some(Value::Array(messages))),
        ("max_tokens", input.get("max_output_tokens").cloned()),
        ("temperature", input.get("temperature").cloned()),
        ("stream", input.get("stream").cloned()),
        ("tools", if tools.is_empty() { None } else { Some(Value::Array(tools)) }),
        ("tool_choice", Some(convert_tool_choice_for_chat_completions(input.get("tool_choice")))),
        ("reasoning", input.get("reasoning").cloned()),
        ("include", input.get("include").cloned()),
        ("prompt_cache_key", input.get("prompt_cache_key").cloned()),
        ("text", input.get("text").cloned()),
        ("parallel_tool_calls", input.get("parallel_tool_calls").cloned()),
        ("response_format", response_format),
        ("metadata", input.get("metadata").cloned()),
        ("incomingApiType", Some(json!("responses"))),
        ("originalBody", Some(input.clone())),
    ]))
}
