//! `transformers/anthropic/stream-formatter.ts`: unified chunks → client
//! Anthropic SSE.
//!
//! Block lifecycle state machine:
//! 1. `message_start` — initial message metadata
//! 2. `content_block_start` — begin a text/thinking/tool_use block
//! 3. `content_block_delta` — progressive deltas
//! 4. `content_block_stop` — end the current block (on type change)
//! 5. `message_delta` — final usage and stop reason (deferred to `finish`)
//! 6. `message_stop` — stream complete
use super::helpers::{or_zero, strict_eq};
use crate::engine::js::{coalesce, get, now_ms, obj, or, stringify, truthy};
use crate::engine::sse;
use crate::engine::transformer::StreamFormatter;
use serde_json::{json, Value};

#[derive(Clone, Copy, PartialEq, Debug)]
enum BlockType { Text, Thinking, ToolUse }

#[derive(Default)]
pub struct AnthropicStreamFormatter {
    has_sent_start: bool,
    has_sent_finish: bool,
    next_block_index: i64,
    active_block_type: Option<BlockType>,
    active_block_index: Option<i64>,
    /// `info?.id ?? null` of the active tool_use block (`None` = null).
    active_tool_call_id: Option<Value>,
    last_usage: Option<Value>,
    pending_finish_reason: Option<Value>,
    /// Synthetic safeguard verdict carried on the terminal unified chunk
    /// (first seen wins; `Some(Value::Null)` is a seen `null`).
    pending_safeguard_results: Option<Value>,
}

fn send_event(out: &mut Vec<String>, event: &str, data: &Value) {
    out.push(sse::encode(Some(event), &stringify(data)));
}

/// `mapping[finish_reason] || finish_reason`.
fn map_finish_reason(fr: &Value) -> Value {
    match fr.as_str() {
        Some("stop") => json!("end_turn"),
        Some("length") => json!("max_tokens"),
        Some("tool_calls") => json!("tool_use"),
        Some("content_filter") => json!("stop_sequence"),
        _ => fr.clone(),
    }
}

impl AnthropicStreamFormatter {
    pub fn new() -> Self { Self::default() }

    fn close_current_block(&mut self, out: &mut Vec<String>) {
        if let (Some(_), Some(index)) = (self.active_block_type, self.active_block_index) {
            send_event(out, "content_block_stop", &json!({"type": "content_block_stop", "index": index}));
            self.active_block_type = None;
            self.active_block_index = None;
            self.active_tool_call_id = None;
        }
    }

    fn start_block(&mut self, out: &mut Vec<String>, kind: BlockType, id: Option<&Value>, name: Option<&Value>) {
        self.close_current_block(out);
        let index = self.next_block_index;
        self.next_block_index += 1;
        self.active_block_index = Some(index);
        self.active_block_type = Some(kind);
        self.active_tool_call_id = if kind == BlockType::ToolUse { id.filter(|v| !v.is_null()).cloned() } else { None };
        let content_block = match kind {
            BlockType::Text => json!({"type": "text", "text": ""}),
            BlockType::Thinking => json!({"type": "thinking", "thinking": ""}),
            BlockType::ToolUse => obj(vec![
                ("type", Some(json!("tool_use"))),
                ("id", id.cloned()),
                ("name", name.cloned()),
                ("input", Some(json!({}))),
            ]),
        };
        send_event(out, "content_block_start", &json!({"type": "content_block_start", "index": index, "content_block": content_block}));
    }

    fn delta_event(&self, out: &mut Vec<String>, delta: Value) {
        let index = self.active_block_index.map(Value::from).unwrap_or(Value::Null);
        send_event(out, "content_block_delta", &json!({"type": "content_block_delta", "index": index, "delta": delta}));
    }
}

impl StreamFormatter for AnthropicStreamFormatter {
    fn on_chunk(&mut self, chunk: &Value) -> Vec<String> {
        let mut out = Vec::new();
        if self.has_sent_finish { return out; }

        if get(chunk, &["event"]).and_then(Value::as_str) == Some("error") {
            send_event(&mut out, "error", &json!({"type": "error", "error": obj(vec![
                ("type", Some(json!("api_error"))),
                ("message", get(chunk, &["error", "message"]).cloned()),
            ])}));
            self.has_sent_finish = true;
            return out;
        }

        // Accumulate usage
        if truthy(chunk.get("usage")) { self.last_usage = chunk.get("usage").cloned(); }

        // Carry a safeguard verdict attached upstream (real or synthetic).
        // First-seen wins: an early real verdict must not be overwritten by a
        // later synthetic terminal injection.
        if let Some(results) = chunk.get("safeguard_results") {
            if self.pending_safeguard_results.is_none() { self.pending_safeguard_results = Some(results.clone()); }
        }

        // 1. Message start
        if !self.has_sent_start {
            let usage = |k: &str| or_zero(get(chunk, &["usage", k]));
            let id = or(chunk.get("id"), None).unwrap_or_else(|| json!(format!("msg_{}", now_ms())));
            let message = obj(vec![
                ("id", Some(id)),
                ("type", Some(json!("message"))),
                ("role", Some(json!("assistant"))),
                ("model", chunk.get("model").cloned()),
                ("content", Some(json!([]))),
                ("stop_reason", Some(Value::Null)),
                ("stop_sequence", Some(Value::Null)),
                ("usage", Some(json!({
                    "input_tokens": usage("input_tokens"),
                    "output_tokens": usage("output_tokens"),
                    "thinkingTokens": usage("reasoning_tokens"),
                    "cache_read_input_tokens": usage("cached_tokens"),
                    "cache_creation_input_tokens": usage("cache_creation_tokens"),
                }))),
            ]);
            send_event(&mut out, "message_start", &json!({"type": "message_start", "message": message}));
            self.has_sent_start = true;
        }

        if let Some(delta) = chunk.get("delta").filter(|d| truthy(Some(d))) {
            // Thinking
            let thinking_text = or(get(delta, &["thinking", "content"]), delta.get("reasoning_content"));
            if truthy(thinking_text.as_ref()) {
                if self.active_block_type != Some(BlockType::Thinking) { self.start_block(&mut out, BlockType::Thinking, None, None); }
                self.delta_event(&mut out, json!({"type": "thinking_delta", "thinking": thinking_text}));
            }

            if let Some(signature) = get(delta, &["thinking", "signature"]).filter(|s| truthy(Some(s))) {
                if self.active_block_type != Some(BlockType::Thinking) { self.start_block(&mut out, BlockType::Thinking, None, None); }
                self.delta_event(&mut out, json!({"type": "signature_delta", "signature": signature}));
            }

            // Text
            if let Some(text) = delta.get("content").filter(|c| truthy(Some(c))) {
                if self.active_block_type != Some(BlockType::Text) { self.start_block(&mut out, BlockType::Text, None, None); }
                self.delta_event(&mut out, json!({"type": "text_delta", "text": text}));
            }

            // Tool calls
            if let Some(calls) = delta.get("tool_calls").filter(|t| truthy(Some(t))).and_then(Value::as_array) {
                for tc in calls {
                    let tool_call_id = tc.get("id");
                    let id_differs = match (tool_call_id, &self.active_tool_call_id) {
                        (Some(id), Some(active)) => !strict_eq(id, active),
                        (Some(id), None) => !id.is_null(),
                        (None, _) => true,
                    };
                    let should_start = self.active_block_type != Some(BlockType::ToolUse)
                        || self.active_block_index.is_none()
                        // Empty IDs identify continuation chunks.
                        || (truthy(tool_call_id) && id_differs);
                    if should_start {
                        if !truthy(tool_call_id) { continue; }
                        self.start_block(&mut out, BlockType::ToolUse, tool_call_id, get(tc, &["function", "name"]));
                    }
                    if let Some(args) = get(tc, &["function", "arguments"]).filter(|a| truthy(Some(a))) {
                        if self.active_block_type == Some(BlockType::ToolUse) && self.active_block_index.is_some() {
                            self.delta_event(&mut out, json!({"type": "input_json_delta", "partial_json": args}));
                        }
                    }
                }
            }
        }

        // Capture finish reason; completion events are deferred to `finish`
        // so subsequent chunks (like usage) can still be processed.
        if let Some(fr) = chunk.get("finish_reason").filter(|f| truthy(Some(f))) {
            self.close_current_block(&mut out);
            self.pending_finish_reason = Some(map_finish_reason(fr));
        }
        out
    }

    fn finish(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        // Robust termination: ensure message_delta and message_stop are sent.
        if self.has_sent_start && !self.has_sent_finish {
            if let (Some(_), Some(index)) = (self.active_block_type, self.active_block_index) {
                send_event(&mut out, "content_block_stop", &json!({"type": "content_block_stop", "index": index}));
            }
            let mut delta = serde_json::Map::new();
            delta.insert("stop_reason".into(), or(self.pending_finish_reason.as_ref(), Some(&json!("end_turn"))).unwrap_or(Value::Null));
            delta.insert("stop_sequence".into(), Value::Null);
            if let Some(results) = &self.pending_safeguard_results { delta.insert("safeguard_results".into(), results.clone()); }
            let usage = |k: &str| coalesce(self.last_usage.as_ref().and_then(|u| get(u, &[k])), Some(&json!(0))).unwrap_or(Value::Null);
            send_event(&mut out, "message_delta", &json!({
                "type": "message_delta",
                "delta": delta,
                "usage": {
                    "input_tokens": usage("input_tokens"),
                    "output_tokens": usage("output_tokens"),
                    "thinkingTokens": usage("reasoning_tokens"),
                    "cache_read_input_tokens": usage("cached_tokens"),
                    "cache_creation_input_tokens": usage("cache_creation_tokens"),
                },
            }));
            send_event(&mut out, "message_stop", &json!({"type": "message_stop"}));
            self.has_sent_finish = true;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(chunks: &[Value]) -> String {
        let mut f = AnthropicStreamFormatter::new();
        let mut out: Vec<String> = chunks.iter().flat_map(|c| f.on_chunk(c)).collect();
        out.extend(f.finish());
        out.concat()
    }

    #[test]
    fn full_lifecycle() {
        let text = run(&[
            json!({"id": "m1", "model": "claude", "delta": {"role": "assistant"}, "usage": {"input_tokens": 5, "output_tokens": 0}}),
            json!({"delta": {"reasoning_content": "hm", "thinking": {"content": "hm"}}}),
            json!({"delta": {"thinking": {"signature": "sig"}}}),
            json!({"delta": {"content": "Hi"}}),
            json!({"delta": {"tool_calls": [{"index": 0, "id": "t1", "type": "function", "function": {"name": "get", "arguments": ""}}]}}),
            json!({"delta": {"tool_calls": [{"index": 0, "function": {"arguments": "{}"}}]}}),
            json!({"delta": {}, "finish_reason": "tool_calls", "usage": {"input_tokens": 5, "output_tokens": 9, "reasoning_tokens": 2, "cached_tokens": 1, "cache_creation_tokens": 0}}),
        ]);
        let expected = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":5,\"output_tokens\":0,\"thinkingTokens\":0,\"cache_read_input_tokens\":0,\"cache_creation_input_tokens\":0}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"hm\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"get\",\"input\":{}}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{}\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":2}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",\"stop_sequence\":null},\"usage\":{\"input_tokens\":5,\"output_tokens\":9,\"thinkingTokens\":2,\"cache_read_input_tokens\":1,\"cache_creation_input_tokens\":0}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        );
        assert_eq!(text, expected);
    }

    #[test]
    fn error_chunk_ends_stream() {
        let text = run(&[json!({"event": "error", "error": {"message": "boom"}}), json!({"delta": {"content": "x"}})]);
        assert_eq!(text, "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"api_error\",\"message\":\"boom\"}}\n\n");
        assert_eq!(run(&[]), "");
    }

    #[test]
    fn continuation_without_active_tool_is_skipped_and_default_id() {
        let text = run(&[json!({"model": "m", "delta": {"tool_calls": [{"index": 0, "function": {"arguments": "{}"}}]}})]);
        assert!(text.contains("\"id\":\"msg_"));
        assert!(!text.contains("content_block_start"));
        assert!(text.contains("\"stop_reason\":\"end_turn\""));
    }

    fn message_delta(text: &str) -> Value {
        let line = text.split('\n').find(|l| l.starts_with("data: ") && l.contains("message_delta")).unwrap();
        serde_json::from_str(&line["data: ".len()..]).unwrap()
    }

    #[test]
    fn safeguard_results_first_seen_wins() {
        use crate::engine::anthropic::synthetic_safeguards::{build_synthetic_safeguard_results, SYNTHETIC_SAFEGUARD_EXPLANATION};
        let real = Value::Array(build_synthetic_safeguard_results(&["toolu_a".into()], &["dangerous_tool_use".into()]));
        let synthetic = json!([{"type": "other"}]);
        let text = run(&[
            json!({"id": "m", "model": "luna", "created": 1, "delta": {"tool_calls": [{"id": "toolu_a"}]}, "safeguard_results": real}),
            json!({"id": "m", "model": "luna", "created": 1, "delta": {}, "finish_reason": "tool_calls", "safeguard_results": synthetic}),
        ]);
        assert_eq!(message_delta(&text)["delta"]["safeguard_results"], real);

        let text = run(&[
            json!({"id": "m", "model": "luna", "created": 1, "delta": {"role": "assistant"}}),
            json!({"id": "m", "model": "luna", "created": 1, "delta": {}, "finish_reason": "tool_calls",
                "safeguard_results": build_synthetic_safeguard_results(&["toolu_x".into()], &["dangerous_tool_use".into()])}),
        ]);
        let d = message_delta(&text);
        assert_eq!(d["delta"]["safeguard_results"][0]["status"]["tool_uses"]["toolu_x"],
            json!({"type": "evaluated", "outcome": "not_flagged", "explanation": SYNTHETIC_SAFEGUARD_EXPLANATION}));
    }
}
