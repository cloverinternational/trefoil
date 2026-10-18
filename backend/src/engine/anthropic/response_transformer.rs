//! `transformers/anthropic/response-transformer.ts`: provider Anthropic
//! response → unified response.
use super::helpers::{count_tokens, js_max0_sub, js_num, js_plus, or_zero, stringify_opt};
use crate::engine::js::{get, get_str, obj, or, to_string};
use crate::engine::transformer::{EngineError, EngineResult};
use serde_json::{json, Value};

/// `transformAnthropicResponse`.
///
/// Combines text and thinking blocks, rebuilds tool calls, and imputes
/// thinking tokens: when thinking content is present but the provider does
/// not report thinking tokens separately, text tokens are estimated with
/// `countTokens` and the remainder of `output_tokens` is attributed to
/// reasoning.
pub fn transform_anthropic_response(response: &Value) -> EngineResult<Value> {
    let blocks: &[Value] = match or(response.get("content"), None) {
        None => &[],
        Some(Value::Array(_)) => response["content"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or(&[]),
        // `for…of` over a string yields characters (no block types).
        Some(Value::String(_)) => &[],
        Some(_) => return Err(EngineError::new("contentBlocks is not iterable")),
    };
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls: Vec<Value> = Vec::new();

    for block in blocks {
        if block.is_null() {
            return Err(super::helpers::type_error(Some(block), "block.type"));
        }
        match get_str(block, &["type"]) {
            Some("text") => text.push_str(&to_string(block.get("text"))),
            Some("thinking") => reasoning.push_str(&to_string(block.get("thinking"))),
            Some("tool_use") => tool_calls.push(obj(vec![
                ("id", block.get("id").cloned()),
                ("type", Some(json!("function"))),
                (
                    "function",
                    Some(obj(vec![
                        ("name", block.get("name").cloned()),
                        ("arguments", stringify_opt(block.get("input"))),
                    ])),
                ),
            ])),
            _ => {}
        }
    }

    // Anthropic input_tokens is already the uncached portion (unlike
    // Gemini/OpenAI prompt counts, which include cache).
    let usage = |k: &str| or_zero(get(response, &["usage", k]));
    let input_tokens = usage("input_tokens");
    let cache_read = usage("cache_read_input_tokens");
    let cache_creation = usage("cache_creation_input_tokens");
    let total_output = usage("output_tokens");

    // TOKEN IMPUTATION LOGIC
    let (real_output, imputed) = if !reasoning.is_empty() {
        let real = count_tokens(&text);
        (js_num(real), js_max0_sub(&total_output, &js_num(real)))
    } else {
        (total_output.clone(), json!(0))
    };

    Ok(obj(vec![
        ("id", response.get("id").cloned()),
        ("model", response.get("model").cloned()),
        (
            "content",
            Some(if text.is_empty() {
                Value::Null
            } else {
                json!(text)
            }),
        ),
        (
            "reasoning_content",
            Some(if reasoning.is_empty() {
                Value::Null
            } else {
                json!(reasoning)
            }),
        ),
        (
            "tool_calls",
            if tool_calls.is_empty() {
                None
            } else {
                Some(Value::Array(tool_calls))
            },
        ),
        (
            "finishReason",
            Some(or(response.get("stop_reason"), None).unwrap_or(Value::Null)),
        ),
        (
            "usage",
            Some(json!({
                "input_tokens": input_tokens,
                "output_tokens": real_output,
                "total_tokens": js_plus(&input_tokens, &total_output),
                "reasoning_tokens": imputed,
                "cached_tokens": cache_read,
                "cache_creation_tokens": cache_creation,
            })),
        ),
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::js::stringify;

    #[test]
    fn transforms_with_imputation() {
        let resp = json!({
            "id": "msg_1", "model": "claude", "stop_reason": "tool_use",
            "content": [
                {"type": "thinking", "thinking": "let me think"},
                {"type": "text", "text": "Hello world!"},
                {"type": "tool_use", "id": "t1", "name": "get", "input": {"a": 1}},
            ],
            "usage": {"input_tokens": 10, "output_tokens": 50, "cache_read_input_tokens": 3},
        });
        let out = transform_anthropic_response(&resp).unwrap();
        assert_eq!(
            stringify(&out),
            stringify(&json!({
                "id": "msg_1", "model": "claude", "content": "Hello world!", "reasoning_content": "let me think",
                "tool_calls": [{"id": "t1", "type": "function", "function": {"name": "get", "arguments": "{\"a\":1}"}}],
                "finishReason": "tool_use",
                "usage": {"input_tokens": 10, "output_tokens": 6, "total_tokens": 60, "reasoning_tokens": 44, "cached_tokens": 3, "cache_creation_tokens": 0},
            }))
        );
    }

    #[test]
    fn empty_response() {
        let out = transform_anthropic_response(&json!({"id": "m"})).unwrap();
        assert_eq!(
            stringify(&out),
            r#"{"id":"m","content":null,"reasoning_content":null,"finishReason":null,"usage":{"input_tokens":0,"output_tokens":0,"total_tokens":0,"reasoning_tokens":0,"cached_tokens":0,"cache_creation_tokens":0}}"#
        );
    }
}
