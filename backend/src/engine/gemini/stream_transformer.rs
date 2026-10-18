//! `transformers/gemini/stream-transformer.ts`: provider SSE → unified chunks
//! with block lifecycle events (`*_start` / `*_delta` / `*_end`, `usage`, `done`).
use super::malformed::detect_gemini_malformed_function_call;
use crate::engine::js::*;
use crate::engine::sse::SseEvent;
use crate::engine::transformer::StreamTransformer;
use crate::engine::usage::normalize_gemini;
use serde_json::{json, Value};

#[derive(Default)]
pub struct GeminiStreamTransformer {
    active_block: Option<&'static str>,
    has_sent_message_start: bool,
    message_has_function_calls: bool,
    streamed_tool_call_count: u64,
}

/// `{ input_tokens, output_tokens, total_tokens, reasoning_tokens, cached_tokens }`.
fn stream_usage(usage_metadata: &Value) -> Value {
    let u = normalize_gemini(usage_metadata);
    json!({
        "input_tokens": u["input_tokens"], "output_tokens": u["output_tokens"], "total_tokens": u["total_tokens"],
        "reasoning_tokens": u["reasoning_tokens"], "cached_tokens": u["cached_tokens"],
    })
}

fn event_chunk(id: Option<&Value>, model: Option<&Value>, event: String, delta: Value) -> Value {
    obj(vec![
        ("id", id.cloned()),
        ("model", model.cloned()),
        ("created", Some(json!(now_ms()))),
        ("event", Some(Value::String(event))),
        ("delta", Some(delta)),
    ])
}

impl GeminiStreamTransformer {
    pub fn new() -> Self { Self::default() }

    /// The body of the TS `try` block. Returning early models a throw caught
    /// by the TS handler: chunks enqueued before it are kept.
    fn process(&mut self, data: &Value, out: &mut Vec<Value>) {
        let candidate = get(data, &["candidates", "0"]).filter(|c| truthy(Some(c)));
        let (id, model) = (data.get("responseId"), data.get("modelVersion"));

        // Usage-only chunk (no candidate).
        let Some(candidate) = candidate else {
            if let Some(um) = data.get("usageMetadata").filter(|u| truthy(Some(u))) {
                let mut chunk = event_chunk(id, model, "usage".into(), json!({}));
                chunk["usage"] = stream_usage(um);
                out.push(chunk);
            }
            return;
        };

        let malformed = detect_gemini_malformed_function_call(data);
        let parts = or(get(candidate, &["content", "parts"]), None).unwrap_or_else(|| json!([]));

        if !self.has_sent_message_start {
            out.push(event_chunk(id, model, "message_start".into(), json!({"role": "assistant"})));
            self.has_sent_message_start = true;
            self.message_has_function_calls = false;
            self.streamed_tool_call_count = 0;
        }

        // `for (const part of parts)`: strings iterate characters (no-ops);
        // other non-iterables throw.
        let parts = match parts {
            Value::Array(items) => items,
            Value::String(_) => Vec::new(),
            _ => return,
        };
        for part in &parts {
            if part.is_null() { return; }
            if truthy(part.get("text")) {
                let is_thinking = part.get("thought") == Some(&Value::Bool(true));
                let new_block = if is_thinking { "thinking" } else { "text" };
                if self.active_block != Some(new_block) {
                    if let Some(active) = self.active_block {
                        out.push(event_chunk(id, model, format!("{active}_end"), json!({})));
                    }
                    out.push(event_chunk(id, model, format!("{new_block}_start"), json!({"role": "assistant"})));
                    self.active_block = Some(new_block);
                }
                let text = part["text"].clone();
                let delta = obj(vec![
                    ("role", Some(json!("assistant"))),
                    ("reasoning_content", if is_thinking { Some(text.clone()) } else { None }),
                    ("content", if is_thinking { None } else { Some(text) }),
                ]);
                out.push(event_chunk(id, model, format!("{new_block}_delta"), delta));
            }

            if truthy(part.get("functionCall")) {
                let fc = &part["functionCall"];
                self.message_has_function_calls = true;
                let index = self.streamed_tool_call_count;
                self.streamed_tool_call_count += 1;
                // Closes any active block, including a previous toolcall block
                // (which then stays active without a new toolcall_start).
                if let Some(active) = self.active_block {
                    out.push(event_chunk(id, model, format!("{active}_end"), json!({})));
                }
                if self.active_block != Some("toolcall") {
                    out.push(event_chunk(id, model, "toolcall_start".into(), json!({"role": "assistant"})));
                    self.active_block = Some("toolcall");
                }
                let call_id = if truthy(fc.get("id")) { fc["id"].clone() } else { json!(format!("call_{}", index + 1)) };
                let delta = json!({"role": "assistant", "tool_calls": [{
                    "index": index,
                    "id": call_id,
                    "type": "function",
                    "function": obj(vec![("name", fc.get("name").cloned()), ("arguments", fc.get("args").map(|a| json!(stringify(a))))]),
                }]});
                out.push(event_chunk(id, model, "toolcall_delta".into(), delta));
            }
        }

        let Some(finish) = candidate.get("finishReason").filter(|f| truthy(Some(f))) else { return };
        if let Some(active) = self.active_block.take() {
            out.push(event_chunk(id, model, format!("{active}_end"), json!({})));
        }
        if let Some(m) = malformed {
            let mut chunk = event_chunk(id, model, "error".into(), json!({}));
            chunk["error"] = m.client_error();
            out.push(chunk);
            self.message_has_function_calls = false;
            self.streamed_tool_call_count = 0;
            return;
        }
        let Some(finish) = finish.as_str() else { return }; // `.toLowerCase` throws
        let mut finish_reason = finish.to_lowercase();
        if self.message_has_function_calls && finish_reason == "stop" { finish_reason = "tool_calls".into(); }
        out.push(obj(vec![
            ("id", id.cloned()),
            ("model", model.cloned()),
            ("created", Some(json!(now_ms()))),
            ("finish_reason", Some(Value::String(finish_reason))),
            ("usage", data.get("usageMetadata").filter(|u| truthy(Some(u))).map(stream_usage)),
        ]));
        self.message_has_function_calls = false;
        self.streamed_tool_call_count = 0;
    }
}

impl StreamTransformer for GeminiStreamTransformer {
    fn on_event(&mut self, event: &SseEvent) -> Vec<Value> {
        let mut out = Vec::new();
        if event.data == "[DONE]" {
            if let Some(active) = self.active_block.take() {
                out.push(event_chunk(Some(&json!("")), Some(&json!("")), format!("{active}_end"), json!({})));
            }
            out.push(event_chunk(Some(&json!("")), Some(&json!("")), "done".into(), json!({})));
            self.message_has_function_calls = false;
            self.streamed_tool_call_count = 0;
            return out;
        }
        let Some(data) = parse(&event.data) else { return out };
        self.process(&data, &mut out);
        out
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn run(events: &[&str]) -> Vec<Value> {
        let mut t = GeminiStreamTransformer::new();
        let mut out = Vec::new();
        for e in events { out.extend(t.on_event(&SseEvent { event: None, data: (*e).to_owned(), id: None })); }
        out.extend(t.finish());
        out
    }

    fn events(chunks: &[Value]) -> Vec<String> {
        chunks.iter().map(|c| c.get("event").and_then(Value::as_str).map(str::to_owned).unwrap_or_else(|| format!("finish:{}", c["finish_reason"].as_str().unwrap_or("")))).collect()
    }

    #[test]
    fn text_lifecycle_and_finish() {
        let c = run(&[r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"Hello"}]},"finishReason":"STOP","index":0}],"responseId":"resp_123","modelVersion":"gemini-2.0-flash"}"#, "[DONE]"]);
        assert_eq!(events(&c), vec!["message_start", "text_start", "text_delta", "text_end", "finish:stop", "done"]);
        assert_eq!(c[0]["delta"], json!({"role": "assistant"}));
        assert_eq!(c[0]["id"], "resp_123");
        assert_eq!(c[2]["delta"], json!({"role": "assistant", "content": "Hello"}));
        assert!(c[4].get("delta").is_none() && c[4].get("usage").is_none());
        assert_eq!(c[5]["id"], "");
    }

    #[test]
    fn thinking_then_text() {
        let c = run(&[
            r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"Thinking process","thought":true}]},"finishReason":null,"index":0}],"responseId":"r"}"#,
            r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"Final answer"}]},"finishReason":"STOP","index":0}],"responseId":"r"}"#,
        ]);
        assert_eq!(events(&c), vec!["message_start", "thinking_start", "thinking_delta", "thinking_end", "text_start", "text_delta", "text_end", "finish:stop"]);
        assert_eq!(c[2]["delta"]["reasoning_content"], "Thinking process");
    }

    #[test]
    fn tool_calls() {
        let c = run(&[r#"{"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"get_weather","args":{"city":"San Francisco"}}},{"functionCall":{"name":"b","args":{}}}]},"finishReason":"STOP","index":0}],"responseId":"resp_123"}"#]);
        assert_eq!(events(&c), vec!["message_start", "toolcall_start", "toolcall_delta", "toolcall_end", "toolcall_delta", "toolcall_end", "finish:tool_calls"]);
        assert_eq!(c[2]["delta"]["tool_calls"][0], json!({"index": 0, "id": "call_1", "type": "function", "function": {"name": "get_weather", "arguments": "{\"city\":\"San Francisco\"}"}}));
        assert_eq!(c[4]["delta"]["tool_calls"][0]["id"], "call_2");
        assert_eq!(c[4]["delta"]["tool_calls"][0]["index"], 1);
    }

    #[test]
    fn tool_calls_finish_survives_trailing_empty_text_chunk() {
        let c = run(&[
            r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"search_web","args":{"query":"top news headlines today"},"id":"zq87ju01"},"thoughtSignature":"EjQK"}],"role":"model"},"index":0}],"usageMetadata":{"promptTokenCount":1385,"candidatesTokenCount":19,"totalTokenCount":1404},"modelVersion":"gemini-3.1-flash-lite-preview","responseId":"Q"}"#,
            r#"{"candidates":[{"content":{"parts":[{"text":""}],"role":"model"},"finishReason":"STOP","index":0}],"usageMetadata":{"promptTokenCount":1385,"candidatesTokenCount":19,"totalTokenCount":1404},"modelVersion":"gemini-3.1-flash-lite-preview","responseId":"Q"}"#,
        ]);
        let delta = c.iter().find(|x| x["event"] == "toolcall_delta").unwrap();
        assert_eq!(delta["delta"]["tool_calls"][0]["id"], "zq87ju01");
        let finish = c.iter().rev().find(|x| x.get("finish_reason").is_some()).unwrap();
        assert_eq!(finish["finish_reason"], "tool_calls");
        assert_eq!(finish["usage"], json!({"input_tokens": 1385, "output_tokens": 19, "total_tokens": 1404, "reasoning_tokens": 0, "cached_tokens": 0}));
    }

    #[test]
    fn usage_recitation_empty_parts_and_usage_only() {
        let c = run(&[r#"{"candidates":[{"content":{"parts":[{"text":"Response"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":100,"candidatesTokenCount":50,"totalTokenCount":150,"thoughtsTokenCount":25}}"#]);
        let u = &c.iter().find(|x| x.get("usage").is_some()).unwrap()["usage"];
        assert_eq!((u["input_tokens"].clone(), u["output_tokens"].clone(), u["total_tokens"].clone(), u["reasoning_tokens"].clone()), (json!(100), json!(50), json!(150), json!(25)));
        let c = run(&[r#"{"candidates":[{"content":{"parts":[{"text":"Citation"}]},"finishReason":"RECITATION"}]}"#]);
        assert_eq!(c.last().unwrap()["finish_reason"], "recitation");
        let c = run(&[r#"{"candidates":[{"content":{"role":"model","parts":[]},"finishReason":"STOP","index":0}]}"#]);
        assert_eq!(events(&c), vec!["message_start", "finish:stop"]);
        let c = run(&[r#"{"usageMetadata":{"promptTokenCount":123,"candidatesTokenCount":456,"totalTokenCount":579},"responseId":"resp_123","modelVersion":"gemini-1.5-flash"}"#]);
        assert_eq!(events(&c), vec!["usage"]);
        assert_eq!(c[0]["usage"]["input_tokens"], 123);
        assert_eq!(c[0]["usage"]["output_tokens"], 456);
        assert!(run(&["not json"]).is_empty());
    }

    #[test]
    fn malformed_function_call_emits_error_event() {
        let c = run(&[r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"Formattingcall:default_api:bash{command:bun run format}"}]},"finishReason":"MALFORMED_FUNCTION_CALL","finishMessage":"x","index":0}],"responseId":"resp_malformed","modelVersion":"gemini-3.6-flash"}"#]);
        let err = c.iter().find(|x| x["event"] == "error").unwrap();
        assert_eq!(err["error"]["statusCode"], 503);
        assert_eq!(err["error"]["code"], "MALFORMED_FUNCTION_CALL");
        assert!(err["error"]["message"].as_str().unwrap().contains("please retry your request"));
        assert!(!c.iter().any(|x| x.get("finish_reason").is_some()));
    }
}
