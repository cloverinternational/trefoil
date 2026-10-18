//! `transformers/anthropic/stream-transformer.ts`: provider Anthropic SSE
//! stream → unified chunks.
//!
//! Anthropic streams are a state machine of events (`message_start`,
//! `content_block_start`, `content_block_delta`, `message_delta`,
//! `message_stop`, `error`). This transformer converts them to unified
//! chunks while accumulating state (message id, model, text, thinking flag)
//! for thinking-token imputation. Events whose processing throws in the TS
//! (e.g. a `content_block_delta` without `delta`) are logged and dropped
//! there; here they yield no chunk, keeping any state mutated before the
//! throw point.
use super::helpers::{count_tokens, js_max0_sub, js_num, js_plus, or_zero, strict_eq};
use crate::engine::js::{get, get_str, now_secs, obj, or, parse, to_string, truthy};
use crate::engine::sse::SseEvent;
use crate::engine::transformer::StreamTransformer;
use serde_json::{json, Value};

#[derive(Default)]
pub struct AnthropicStreamTransformer {
    accumulated_text: String,
    seen_thinking: bool,
    message_id: Option<Value>,
    model: Option<Value>,
    /// Anthropic content block index → 0-based sequential tool call index.
    /// Anthropic's block index counts ALL content blocks (text, thinking,
    /// tool_use), but OpenAI tool call indices count only tool calls.
    block_index_to_tool_call_index: Vec<(Option<Value>, i64)>,
    next_tool_call_index: i64,
}

impl AnthropicStreamTransformer {
    pub fn new() -> Self {
        Self::default()
    }

    /// `Map.get` with SameValueZero keys (`undefined` is a valid key).
    fn tool_index_for(&self, key: Option<&Value>) -> Option<i64> {
        self.block_index_to_tool_call_index
            .iter()
            .find(|(k, _)| match (k, key) {
                (None, None) => true,
                (Some(a), Some(b)) => strict_eq(a, b),
                _ => false,
            })
            .map(|(_, v)| *v)
    }

    fn set_tool_index(&mut self, key: Option<&Value>, value: i64) {
        let key = key.cloned();
        if let Some(entry) = self
            .block_index_to_tool_call_index
            .iter_mut()
            .find(|(k, _)| match (k, &key) {
                (None, None) => true,
                (Some(a), Some(b)) => strict_eq(a, b),
                _ => false,
            })
        {
            entry.1 = value;
        } else {
            self.block_index_to_tool_call_index.push((key, value));
        }
    }

    fn chunk_for(&mut self, data: &Value) -> Option<Value> {
        // `data.type` on null throws (caught → dropped).
        if data.is_null() {
            return None;
        }
        match get_str(data, &["type"])? {
            "message_start" => {
                let message = data.get("message").filter(|m| !m.is_null())?;
                self.message_id = message.get("id").cloned();
                self.model = message.get("model").cloned();
                let usage = message.get("usage").filter(|u| truthy(Some(u))).map(|u| {
                    let input = or_zero(u.get("input_tokens"));
                    let output = or_zero(u.get("output_tokens"));
                    json!({
                        "input_tokens": input,
                        "output_tokens": output,
                        "total_tokens": js_plus(&input, &output),
                        "reasoning_tokens": 0,
                        "cached_tokens": or_zero(u.get("cache_read_input_tokens")),
                        "cache_creation_tokens": or_zero(u.get("cache_creation_input_tokens")),
                    })
                });
                Some(obj(vec![
                    ("id", self.message_id.clone()),
                    ("model", self.model.clone()),
                    ("created", Some(json!(now_secs()))),
                    ("delta", Some(json!({"role": "assistant"}))),
                    ("usage", usage),
                ]))
            }
            "content_block_delta" => {
                let delta = data.get("delta").filter(|d| !d.is_null())?;
                match get_str(delta, &["type"]) {
                    Some("text_delta") => {
                        self.accumulated_text
                            .push_str(&to_string(delta.get("text")));
                        Some(json!({"delta": obj(vec![("content", delta.get("text").cloned())])}))
                    }
                    Some("thinking_delta") => {
                        self.seen_thinking = true;
                        Some(json!({"delta": obj(vec![
                            ("reasoning_content", delta.get("thinking").cloned()),
                            ("thinking", Some(obj(vec![("content", delta.get("thinking").cloned())]))),
                        ])}))
                    }
                    Some("signature_delta") => Some(
                        json!({"delta": {"thinking": obj(vec![("signature", delta.get("signature").cloned())])}}),
                    ),
                    Some("input_json_delta") => {
                        let index = self
                            .tool_index_for(data.get("index"))
                            .map(|i| json!(i))
                            .or_else(|| data.get("index").cloned());
                        Some(json!({"delta": {"tool_calls": [obj(vec![
                            ("index", index),
                            ("function", Some(obj(vec![("arguments", delta.get("partial_json").cloned())]))),
                        ])]}}))
                    }
                    _ => None,
                }
            }
            "content_block_start" => {
                let block = data.get("content_block").filter(|b| !b.is_null())?;
                match get_str(block, &["type"]) {
                    Some("tool_use") => {
                        let idx = self.next_tool_call_index;
                        self.next_tool_call_index += 1;
                        self.set_tool_index(data.get("index"), idx);
                        Some(json!({"delta": {"tool_calls": [obj(vec![
                            ("index", Some(json!(idx))),
                            ("id", block.get("id").cloned()),
                            ("type", Some(json!("function"))),
                            ("function", Some(obj(vec![("name", block.get("name").cloned()), ("arguments", Some(json!("")))]))),
                        ])]}}))
                    }
                    Some("thinking") => {
                        self.seen_thinking = true;
                        Some(
                            json!({"delta": {"thinking": {"content": or(block.get("thinking"), Some(&json!(""))).unwrap_or(Value::Null)}}}),
                        )
                    }
                    _ => None,
                }
            }
            "message_delta" => {
                // Usage update and finish reason.
                let usage_v = data.get("usage");
                let input = or_zero(get(data, &["usage", "input_tokens"]));
                let total_output = or_zero(get(data, &["usage", "output_tokens"]));
                let (real_output, imputed) = if self.seen_thinking {
                    let real = js_num(count_tokens(&self.accumulated_text));
                    let imputed = js_max0_sub(&total_output, &real);
                    (real, imputed)
                } else {
                    (total_output.clone(), json!(0))
                };
                let delta = data.get("delta").filter(|d| !d.is_null())?;
                let finish = match delta.get("stop_reason") {
                    Some(Value::String(s)) if s == "end_turn" => Some(json!("stop")),
                    Some(Value::String(s)) if s == "tool_use" => Some(json!("tool_calls")),
                    other => other.cloned(),
                };
                let usage = usage_v.filter(|u| truthy(Some(u))).map(|u| {
                    json!({
                        "input_tokens": input,
                        "output_tokens": real_output,
                        "total_tokens": js_plus(&input, &total_output),
                        "reasoning_tokens": imputed,
                        "cached_tokens": or_zero(u.get("cache_read_input_tokens")),
                        "cache_creation_tokens": or_zero(u.get("cache_creation_input_tokens")),
                    })
                });
                Some(obj(vec![("finish_reason", finish), ("usage", usage)]))
            }
            "error" => {
                // Anthropic mid-stream error event: surface it as a unified
                // error chunk instead of silently truncating the client stream.
                Some(obj(vec![
                    ("id", self.message_id.clone()),
                    ("model", self.model.clone()),
                    ("created", Some(json!(now_secs()))),
                    ("event", Some(json!("error"))),
                    ("delta", Some(json!({}))),
                    (
                        "error",
                        Some(json!({
                            "statusCode": 500,
                            "code": or(get(data, &["error", "type"]), Some(&json!("error"))),
                            "message": or(get(data, &["error", "message"]), Some(&json!("Upstream error"))),
                        })),
                    ),
                ]))
            }
            _ => None,
        }
    }
}

impl StreamTransformer for AnthropicStreamTransformer {
    fn on_event(&mut self, event: &SseEvent) -> Vec<Value> {
        if event.data == "[DONE]" {
            return vec![];
        }
        let Some(data) = parse(&event.data) else {
            return vec![];
        };
        self.chunk_for(&data).into_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::js::stringify;

    fn ev(data: Value) -> SseEvent {
        SseEvent {
            event: None,
            data: stringify(&data),
            id: None,
        }
    }

    #[test]
    fn converts_event_sequence() {
        let mut t = AnthropicStreamTransformer::new();
        let start = t.on_event(&ev(json!({"type": "message_start", "message": {"id": "msg_1", "model": "claude", "usage": {"input_tokens": 10, "output_tokens": 1, "cache_read_input_tokens": 2}}})));
        assert_eq!(start[0]["id"], json!("msg_1"));
        assert_eq!(start[0]["delta"], json!({"role": "assistant"}));
        assert_eq!(
            stringify(&start[0]["usage"]),
            r#"{"input_tokens":10,"output_tokens":1,"total_tokens":11,"reasoning_tokens":0,"cached_tokens":2,"cache_creation_tokens":0}"#
        );

        assert_eq!(t.on_event(&ev(json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": ""}})))[0], json!({"delta": {"thinking": {"content": ""}}}));
        assert_eq!(t.on_event(&ev(json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "hm"}})))[0],
            json!({"delta": {"reasoning_content": "hm", "thinking": {"content": "hm"}}}));
        assert_eq!(t.on_event(&ev(json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "sig"}})))[0],
            json!({"delta": {"thinking": {"signature": "sig"}}}));
        assert_eq!(t.on_event(&ev(json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "Hello world!"}})))[0],
            json!({"delta": {"content": "Hello world!"}}));
        assert_eq!(stringify(&t.on_event(&ev(json!({"type": "content_block_start", "index": 2, "content_block": {"type": "tool_use", "id": "t1", "name": "get", "input": {}}})))[0]),
            r#"{"delta":{"tool_calls":[{"index":0,"id":"t1","type":"function","function":{"name":"get","arguments":""}}]}}"#);
        assert_eq!(t.on_event(&ev(json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "{\"a\""}})))[0],
            json!({"delta": {"tool_calls": [{"index": 0, "function": {"arguments": "{\"a\""}}]}}));
        // Unknown block index falls back to the raw Anthropic index.
        assert_eq!(t.on_event(&ev(json!({"type": "content_block_delta", "index": 7, "delta": {"type": "input_json_delta", "partial_json": "x"}})))[0]["delta"]["tool_calls"][0]["index"], json!(7));
        assert!(t
            .on_event(&ev(json!({"type": "content_block_stop", "index": 2})))
            .is_empty());
        let end = t.on_event(&ev(json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 30}})));
        assert_eq!(
            stringify(&end[0]),
            r#"{"finish_reason":"tool_calls","usage":{"input_tokens":0,"output_tokens":6,"total_tokens":30,"reasoning_tokens":24,"cached_tokens":0,"cache_creation_tokens":0}}"#
        );
        assert!(t.on_event(&ev(json!({"type": "message_stop"}))).is_empty());
        assert!(t
            .on_event(&SseEvent {
                event: None,
                data: "[DONE]".into(),
                id: None
            })
            .is_empty());
        assert!(t
            .on_event(&ev(json!({"type": "content_block_delta"})))
            .is_empty());
    }

    #[test]
    fn error_event() {
        let mut t = AnthropicStreamTransformer::new();
        t.on_event(&ev(
            json!({"type": "message_start", "message": {"id": "msg_1", "model": "claude"}}),
        ));
        let e = t.on_event(&ev(json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}})));
        assert_eq!(e[0]["event"], json!("error"));
        assert_eq!(e[0]["id"], json!("msg_1"));
        assert_eq!(
            stringify(&e[0]["error"]),
            r#"{"statusCode":500,"code":"overloaded_error","message":"Overloaded"}"#
        );
        let e = t.on_event(&ev(json!({"type": "error"})));
        assert_eq!(
            e[0]["error"],
            json!({"statusCode": 500, "code": "error", "message": "Upstream error"})
        );
    }
}
