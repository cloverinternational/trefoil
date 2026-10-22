//! `transformers/anthropic/response-formatter.ts`: unified response →
//! client Anthropic response.
use super::helpers::{or_zero, type_error};
use crate::engine::image_rendering::compose_content;
use crate::engine::js::{get, get_arr, obj, parse, random_base36, to_string, truthy};
use crate::engine::transformer::{EngineError, EngineResult};
use serde_json::{json, Value};

/// `formatAnthropicResponse`.
///
/// - Rebuilds Anthropic content blocks (text, thinking, tool_use)
/// - Turns annotations/citations into a synthetic `web_search` server tool
///   call plus its `web_search_tool_result`
/// - Sets `stop_reason` to `tool_use` when tool calls are present
pub fn format_anthropic_response(response: &Value) -> EngineResult<Value> {
    let mut content: Vec<Value> = Vec::new();

    // Support Annotations/Citations
    if let Some(annotations) = get_arr(response, &["annotations"]).filter(|a| !a.is_empty()) {
        let tool_id = format!("srvtoolu_{}", random_base36(9));
        content.push(json!({"type": "server_tool_use", "id": tool_id, "name": "web_search", "input": {"query": ""}}));
        let results: Vec<Value> = annotations
            .iter()
            .map(|a| {
                obj(vec![
                    ("type", Some(json!("web_search_result"))),
                    ("url", get(a, &["url_citation", "url"]).cloned()),
                    ("title", get(a, &["url_citation", "title"]).cloned()),
                ])
            })
            .collect();
        content.push(
            json!({"type": "web_search_tool_result", "tool_use_id": tool_id, "content": results}),
        );
    }

    if truthy(response.get("reasoning_content")) {
        content.push(json!({"type": "thinking", "thinking": response["reasoning_content"]}));
    }

    // Messages-format clients see image_generation_call output as size-guarded
    // markdown composed after the authored text; unified `content` stays pure.
    let text_content = compose_content(
        response.get("content"),
        response.get("image_generation_calls"),
    );
    if truthy(text_content.as_ref()) {
        content.push(json!({"type": "text", "text": text_content}));
    }

    let tool_calls = response.get("tool_calls").filter(|t| truthy(Some(t)));
    if let Some(calls) = tool_calls {
        let Some(calls) = calls.as_array() else {
            return Err(EngineError::new("response.tool_calls is not iterable"));
        };
        for tc in calls {
            let func = tc.get("function").filter(|f| !f.is_null());
            let Some(func) = func else {
                // The try block's TypeError is caught, then the catch block
                // reads `toolCall.function.arguments` again and throws.
                return Err(type_error(
                    tc.get("function"),
                    "toolCall.function.arguments",
                ));
            };
            let args = func.get("arguments");
            let input = if truthy(args) && args.is_some_and(|a| a.is_object() || a.is_array()) {
                args.cloned().unwrap_or(Value::Null)
            } else {
                let text = if truthy(args) {
                    to_string(args)
                } else {
                    "{}".to_owned()
                };
                // Robust Tool Argument Parsing: wrap in a safe state if JSON fails
                parse(&text).unwrap_or_else(|| obj(vec![("raw_arguments", args.cloned())]))
            };
            content.push(obj(vec![
                ("type", Some(json!("tool_use"))),
                ("id", tc.get("id").cloned()),
                ("name", func.get("name").cloned()),
                ("input", Some(input)),
            ]));
        }
    }

    let stop_reason = if tool_calls
        .and_then(Value::as_array)
        .is_some_and(|c| !c.is_empty())
    {
        "tool_use"
    } else {
        "end_turn"
    };
    let usage = |k: &str| or_zero(get(response, &["usage", k]));

    Ok(obj(vec![
        ("id", response.get("id").cloned()),
        ("type", Some(json!("message"))),
        ("role", Some(json!("assistant"))),
        ("model", response.get("model").cloned()),
        ("content", Some(Value::Array(content))),
        ("stop_reason", Some(json!(stop_reason))),
        ("stop_sequence", Some(Value::Null)),
        (
            "usage",
            Some(json!({
                "input_tokens": usage("input_tokens"),
                "output_tokens": usage("output_tokens"),
                "thinkingTokens": usage("reasoning_tokens"),
                "cache_read_input_tokens": usage("cached_tokens"),
                "cache_creation_input_tokens": usage("cache_creation_tokens"),
            })),
        ),
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::js::stringify;

    #[test]
    fn formats_blocks_and_usage() {
        let unified = json!({
            "id": "msg_1", "model": "claude", "content": "hi", "reasoning_content": "think",
            "tool_calls": [
                {"id": "t1", "type": "function", "function": {"name": "a", "arguments": "{\"x\":1}"}},
                {"id": "t2", "type": "function", "function": {"name": "b", "arguments": "{bad"}},
                {"id": "t3", "type": "function", "function": {"name": "c", "arguments": ""}},
            ],
            "usage": {"input_tokens": 5, "output_tokens": 7, "reasoning_tokens": 2, "cached_tokens": 1, "cache_creation_tokens": 0},
        });
        let out = format_anthropic_response(&unified).unwrap();
        assert_eq!(
            stringify(&out),
            stringify(&json!({
                "id": "msg_1", "type": "message", "role": "assistant", "model": "claude",
                "content": [
                    {"type": "thinking", "thinking": "think"},
                    {"type": "text", "text": "hi"},
                    {"type": "tool_use", "id": "t1", "name": "a", "input": {"x": 1}},
                    {"type": "tool_use", "id": "t2", "name": "b", "input": {"raw_arguments": "{bad"}},
                    {"type": "tool_use", "id": "t3", "name": "c", "input": {}},
                ],
                "stop_reason": "tool_use", "stop_sequence": null,
                "usage": {"input_tokens": 5, "output_tokens": 7, "thinkingTokens": 2, "cache_read_input_tokens": 1, "cache_creation_input_tokens": 0},
            }))
        );
    }

    #[test]
    fn annotations_become_web_search_blocks() {
        let out = format_anthropic_response(&json!({"id": "m", "content": "", "annotations": [{"type": "url_citation", "url_citation": {"url": "https://a", "title": "A"}}]})).unwrap();
        let id = out["content"][0]["id"].as_str().unwrap();
        assert!(id.starts_with("srvtoolu_") && id.len() == 18);
        assert_eq!(
            out["content"][1],
            json!({"type": "web_search_tool_result", "tool_use_id": id, "content": [{"type": "web_search_result", "url": "https://a", "title": "A"}]})
        );
        assert_eq!(out["content"].as_array().unwrap().len(), 2);
        assert_eq!(out["stop_reason"], json!("end_turn"));
    }
}
