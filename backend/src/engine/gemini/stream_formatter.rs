//! `transformers/gemini/stream-formatter.ts`: unified chunks → client Gemini SSE.
use super::utils::js_plus;
use crate::engine::js::*;
use crate::engine::sse;
use crate::engine::transformer::StreamFormatter;
use serde_json::{json, Map, Value};

/// `PendingGeminiToolCall`.
#[derive(Default, Debug, Clone)]
struct PendingToolCall {
    id: Option<String>,
    name: Option<String>,
    arguments_text: String,
    /// `structuredArguments` (`Some(Null)` is an explicit `null`).
    structured_arguments: Option<Value>,
    thought_signature: Option<Value>,
    emitted: bool,
}

/// `getToolCallKeys`.
fn tool_call_keys(tc: &Value, fallback: String) -> Vec<String> {
    let mut keys = Vec::new();
    if let Some(i) = tc.get("index").and_then(Value::as_f64) {
        keys.push(format!("index:{}", js_num_string(i)));
    }
    if let Some(id) = get_str(tc, &["id"]).filter(|s| !s.trim().is_empty()) {
        keys.push(format!("id:{id}"));
    }
    if keys.is_empty() {
        keys.push(fallback);
    }
    keys
}

/// `resolveToolCallArguments`.
fn resolve_arguments(state: &PendingToolCall, allow_empty_object: bool) -> Option<Value> {
    if let Some(v) = &state.structured_arguments {
        return Some(v.clone());
    }
    let raw = state.arguments_text.trim();
    if raw.is_empty() {
        return if allow_empty_object {
            Some(json!({}))
        } else {
            None
        };
    }
    parse(raw)
}

/// `buildGeminiFunctionCallPart`.
fn function_call_part(state: &PendingToolCall, allow_empty_object: bool) -> Option<Value> {
    let name = state.name.as_ref()?;
    let args = resolve_arguments(state, allow_empty_object)?;
    let mut fc = Map::new();
    fc.insert("name".into(), json!(name));
    fc.insert("args".into(), args);
    if let Some(id) = &state.id {
        fc.insert("id".into(), json!(id));
    }
    let mut part = Map::new();
    part.insert("functionCall".into(), Value::Object(fc));
    if let Some(sig) = state.thought_signature.as_ref().filter(|s| truthy(Some(s))) {
        part.insert("thoughtSignature".into(), sig.clone());
    }
    Some(Value::Object(part))
}

#[derive(Default)]
pub struct GeminiStreamFormatter {
    states: Vec<PendingToolCall>,
    /// `Map<string, PendingGeminiToolCall>` in insertion order (key → state index).
    keys: Vec<(String, usize)>,
    has_sent_error: bool,
    /// The TS stream errored (a throw inside `transform`); nothing more is emitted.
    errored: bool,
}

fn or_zero(v: Option<&Value>) -> Value {
    or(v, Some(&json!(0))).unwrap_or(Value::Null)
}

fn or_empty(v: Option<&Value>) -> Value {
    or(v, Some(&json!(""))).unwrap_or(Value::Null)
}

impl GeminiStreamFormatter {
    pub fn new() -> Self {
        Self::default()
    }

    fn key_index(&self, key: &str) -> Option<usize> {
        self.keys.iter().find(|(k, _)| k == key).map(|(_, i)| *i)
    }

    fn set_key(&mut self, key: String, idx: usize) {
        match self.keys.iter_mut().find(|(k, _)| *k == key) {
            Some(entry) => entry.1 = idx,
            None => self.keys.push((key, idx)),
        }
    }

    fn format_event(&mut self, chunk: &Value, event_name: &Value) -> String {
        let name = to_string(Some(event_name));
        let mut data = Map::new();
        let typed = |data: &mut Map<String, Value>, t: &str, index: bool| {
            data.insert("type".into(), json!(t));
            if index {
                data.insert("index".into(), json!(0));
            }
        };
        match name.as_str() {
            "message_start" => {
                data.insert("type".into(), json!("message_start"));
                let id = if truthy(chunk.get("id")) {
                    chunk["id"].clone()
                } else {
                    json!(format!("msg_{}", now_ms()))
                };
                let usage = match chunk.get("usage").filter(|u| truthy(Some(u))) {
                    Some(u) => {
                        json!({"input_tokens": or_zero(u.get("input_tokens")), "output_tokens": or_zero(u.get("output_tokens"))})
                    }
                    None => json!({"input_tokens": 0, "output_tokens": 0}),
                };
                data.insert(
                    "message".into(),
                    obj(vec![
                        ("id", Some(id)),
                        ("type", Some(json!("message"))),
                        (
                            "role",
                            Some(
                                or(get(chunk, &["delta", "role"]), Some(&json!("assistant")))
                                    .unwrap_or(Value::Null),
                            ),
                        ),
                        ("model", chunk.get("model").cloned()),
                        ("content", Some(json!([]))),
                        ("usage", Some(usage)),
                    ]),
                );
            }
            "text_start" | "text_end" | "thinking_start" | "thinking_end" | "toolcall_start"
            | "toolcall_end" => typed(&mut data, &name, true),
            "text_delta" => {
                data.insert("type".into(), json!("text_delta"));
                data.insert("delta".into(), or_empty(get(chunk, &["delta", "content"])));
                data.insert("index".into(), json!(0));
            }
            "thinking_delta" => {
                data.insert("type".into(), json!("thinking_delta"));
                data.insert(
                    "delta".into(),
                    or_empty(get(chunk, &["delta", "reasoning_content"])),
                );
                data.insert("index".into(), json!(0));
            }
            "toolcall_delta" => {
                data.insert("type".into(), json!("toolcall_delta"));
                let tc = get(chunk, &["delta", "tool_calls", "0"]).filter(|t| truthy(Some(t)));
                let delta = match tc {
                    Some(tc) => {
                        json!({"name": or_empty(get(tc, &["function", "name"])), "args": or_empty(get(tc, &["function", "arguments"])), "id": or_empty(tc.get("id"))})
                    }
                    None => json!(""),
                };
                data.insert("delta".into(), delta);
                data.insert("index".into(), json!(0));
            }
            "message_end" => typed(&mut data, "message_end", false),
            "usage" => {
                data.insert("type".into(), json!("usage"));
                if let Some(u) = chunk.get("usage").filter(|u| truthy(Some(u))) {
                    data.insert("usage".into(), obj(vec![
                        ("prompt_tokens", Some(js_plus(u.get("input_tokens"), Some(&or_zero(u.get("cached_tokens")))))),
                        ("completion_tokens", u.get("output_tokens").cloned()),
                        ("total_tokens", u.get("total_tokens").cloned()),
                        ("prompt_tokens_details", Some(json!({"cached_tokens": or_zero(u.get("cached_tokens")), "cache_write_tokens": 0, "audio_tokens": 0, "video_tokens": 0}))),
                        ("cost_details", Some(json!({
                            "upstream_inference_cost": or_zero(u.get("upstream_inference_cost")),
                            "upstream_inference_prompt_cost": or_zero(u.get("upstream_inference_prompt_cost")),
                            "upstream_inference_completions_cost": or_zero(u.get("upstream_inference_completions_cost")),
                        }))),
                        ("completion_tokens_details", Some(json!({"reasoning_tokens": or_zero(u.get("reasoning_tokens")), "image_tokens": 0}))),
                    ]));
                }
            }
            "error" => {
                self.has_sent_error = true;
                data.insert(
                    "error".into(),
                    obj(vec![
                        (
                            "code",
                            Some(
                                or(get(chunk, &["error", "statusCode"]), Some(&json!(503)))
                                    .unwrap_or(Value::Null),
                            ),
                        ),
                        ("status", Some(json!("UNAVAILABLE"))),
                        ("message", get(chunk, &["error", "message"]).cloned()),
                    ]),
                );
            }
            "done" => typed(&mut data, "done", false),
            _ => {}
        }
        sse::encode(Some(&name), &stringify(&Value::Object(data)))
    }

    fn format_content(&mut self, chunk: &Value) -> Option<String> {
        let mut parts: Vec<Value> = Vec::new();
        let mut completed: Vec<Value> = Vec::new();
        let delta = chunk.get("delta");
        let dget = |k: &str| delta.and_then(|d| d.get(k));

        if truthy(dget("content")) {
            parts.push(json!({"text": dget("content")}));
        }
        if truthy(dget("reasoning_content")) {
            parts.push(json!({"text": dget("reasoning_content"), "thought": true}));
        }

        if truthy(dget("tool_calls")) {
            let Some(tcs) = dget("tool_calls").and_then(Value::as_array) else {
                self.errored = true;
                return None;
            };
            for (position, tc) in tcs.iter().enumerate() {
                if tc.is_null() {
                    self.errored = true;
                    return None;
                }
                let keys = tool_call_keys(tc, format!("anon:{position}"));
                let idx = match keys.iter().find_map(|k| self.key_index(k)) {
                    Some(i) => i,
                    None => {
                        self.states.push(PendingToolCall::default());
                        self.states.len() - 1
                    }
                };
                for key in keys {
                    self.set_key(key, idx);
                }
                let state = &mut self.states[idx];
                if let Some(id) = get_str(tc, &["id"]).filter(|s| !s.trim().is_empty()) {
                    state.id = Some(id.to_owned());
                }
                if let Some(name) =
                    get_str(tc, &["function", "name"]).filter(|s| !s.trim().is_empty())
                {
                    state.name = Some(name.to_owned());
                }
                match get(tc, &["function", "arguments"]) {
                    Some(Value::String(s)) => state.arguments_text.push_str(s),
                    Some(other) => state.structured_arguments = Some(other.clone()),
                    None => {}
                }
                let sig = [
                    get(tc, &["thinking", "signature"]),
                    tc.get("thought_signature"),
                    get(tc, &["extra_content", "google", "thought_signature"]),
                    dget("thinking").and_then(|t| t.get("signature")),
                    dget("thought_signature"),
                ]
                .into_iter()
                .find(|s| truthy(*s))
                .flatten();
                if let Some(sig) = sig {
                    state.thought_signature = Some(sig.clone());
                }
                if let Some(part) = function_call_part(state, false) {
                    if !state.emitted {
                        state.emitted = true;
                        completed.push(part);
                    }
                }
            }
        }

        if truthy(chunk.get("finish_reason")) {
            let mut seen: Vec<usize> = Vec::new();
            for (_, idx) in &self.keys {
                if !seen.contains(idx) {
                    seen.push(*idx);
                }
            }
            for idx in seen {
                let state = &mut self.states[idx];
                if state.emitted {
                    continue;
                }
                if let Some(part) = function_call_part(state, true) {
                    state.emitted = true;
                    completed.push(part);
                }
            }
        }
        parts.extend(completed);

        let finish = match chunk.get("finish_reason") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => Some(s.to_uppercase()),
            Some(_) => {
                self.errored = true;
                return None;
            } // `.toUpperCase` throws
        };
        let finish = finish
            .map(|f| {
                if f == "TOOL_CALLS" {
                    "STOP".to_owned()
                } else {
                    f
                }
            })
            .filter(|f| !f.is_empty());
        if parts.is_empty() && finish.is_none() {
            return None;
        }

        let usage_metadata = chunk.get("usage").filter(|u| truthy(Some(u))).map(|u| {
            obj(vec![
                (
                    "promptTokenCount",
                    Some(js_plus(
                        u.get("input_tokens"),
                        Some(&or_zero(u.get("cached_tokens"))),
                    )),
                ),
                ("candidatesTokenCount", u.get("output_tokens").cloned()),
                ("totalTokenCount", u.get("total_tokens").cloned()),
                (
                    "thoughtsTokenCount",
                    u.get("reasoning_tokens")
                        .filter(|r| truthy(Some(r)))
                        .cloned(),
                ),
                (
                    "cachedContentTokenCount",
                    u.get("cached_tokens").filter(|c| truthy(Some(c))).cloned(),
                ),
            ])
        });
        let parts = if parts.is_empty() {
            vec![json!({"text": ""})]
        } else {
            parts
        };
        let mut gemini_chunk = obj(vec![
            (
                "candidates",
                Some(
                    json!([{"content": {"role": "model", "parts": parts}, "finishReason": finish, "index": 0}]),
                ),
            ),
            ("usageMetadata", usage_metadata),
        ]);
        if truthy(chunk.get("model")) {
            gemini_chunk["modelVersion"] = chunk["model"].clone();
        }
        if truthy(chunk.get("id")) {
            gemini_chunk["responseId"] = chunk["id"].clone();
        }
        Some(sse::data(&stringify(&gemini_chunk)))
    }
}

impl StreamFormatter for GeminiStreamFormatter {
    fn on_chunk(&mut self, chunk: &Value) -> Vec<String> {
        if self.has_sent_error || self.errored {
            return vec![];
        }
        if let Some(event) = chunk.get("event").filter(|e| truthy(Some(e))) {
            return vec![self.format_event(chunk, event)];
        }
        self.format_content(chunk).into_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::sse::SseParser;

    fn run(chunks: &[Value]) -> Vec<(Option<String>, Value)> {
        let mut f = GeminiStreamFormatter::new();
        let mut text = String::new();
        for c in chunks {
            for s in f.on_chunk(c) {
                text.push_str(&s);
            }
        }
        for s in f.finish() {
            text.push_str(&s);
        }
        SseParser::new()
            .feed(&text)
            .into_iter()
            .map(|e| (e.event, parse(&e.data).unwrap()))
            .collect()
    }

    fn fc_parts(out: &[(Option<String>, Value)]) -> Vec<Value> {
        out.iter()
            .flat_map(|(_, d)| {
                get_arr(d, &["candidates", "0", "content", "parts"])
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|p| p.get("functionCall").is_some())
            })
            .collect()
    }

    #[test]
    fn usage_event() {
        let out = run(&[
            json!({"id": "resp_123", "model": "m", "created": 1, "event": "usage", "delta": {}, "usage": {
                "input_tokens": 100, "output_tokens": 50, "total_tokens": 150, "reasoning_tokens": 10, "cached_tokens": 20,
                "cache_creation_tokens": 0, "upstream_inference_cost": 0.001, "upstream_inference_prompt_cost": 0.0004, "upstream_inference_completions_cost": 0.0006
            }}),
        ]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0.as_deref(), Some("usage"));
        assert_eq!(
            out[0].1,
            json!({"type": "usage", "usage": {
                "prompt_tokens": 120, "completion_tokens": 50, "total_tokens": 150,
                "prompt_tokens_details": {"cached_tokens": 20, "cache_write_tokens": 0, "audio_tokens": 0, "video_tokens": 0},
                "cost_details": {"upstream_inference_cost": 0.001, "upstream_inference_prompt_cost": 0.0004, "upstream_inference_completions_cost": 0.0006},
                "completion_tokens_details": {"reasoning_tokens": 10, "image_tokens": 0}
            }})
        );
    }

    #[test]
    fn lifecycle_events() {
        let out = run(&[
            json!({"id": "r", "model": "m", "event": "message_start", "delta": {"role": "assistant"}}),
            json!({"event": "text_delta", "delta": {"content": "hi"}}),
            json!({"event": "toolcall_delta", "delta": {"tool_calls": [{"id": "c", "function": {"name": "n", "arguments": "{}"}}]}}),
            json!({"event": "toolcall_delta", "delta": {}}),
            json!({"event": "message_delta", "delta": {}}),
            json!({"event": "done", "delta": {}}),
        ]);
        assert_eq!(
            out[0].1,
            json!({"type": "message_start", "message": {"id": "r", "type": "message", "role": "assistant", "model": "m", "content": [], "usage": {"input_tokens": 0, "output_tokens": 0}}})
        );
        assert_eq!(
            out[1].1,
            json!({"type": "text_delta", "delta": "hi", "index": 0})
        );
        assert_eq!(
            out[2].1,
            json!({"type": "toolcall_delta", "delta": {"name": "n", "args": "{}", "id": "c"}, "index": 0})
        );
        assert_eq!(
            out[3].1,
            json!({"type": "toolcall_delta", "delta": "", "index": 0})
        );
        assert_eq!(out[4], (Some("message_delta".into()), json!({})));
        assert_eq!(out[5], (Some("done".into()), json!({"type": "done"})));
    }

    #[test]
    fn error_event_stops_stream() {
        let out = run(&[
            json!({"event": "error", "error": {"statusCode": 503, "code": "MALFORMED_FUNCTION_CALL", "message": "retry"}}),
            json!({"delta": {"content": "late"}}),
        ]);
        assert_eq!(
            out,
            vec![(
                Some("error".into()),
                json!({"error": {"code": 503, "status": "UNAVAILABLE", "message": "retry"}})
            )]
        );
    }

    #[test]
    fn buffers_partial_tool_call_arguments() {
        let out = run(&[
            json!({"id": "resp_123", "model": "gemini-2.0-flash", "delta": {"tool_calls": [{"index": 0, "id": "call_1", "type": "function", "function": {"name": "manage_todo_list", "arguments": "{\"items\":["}}]}, "finish_reason": null}),
            json!({"id": "resp_123", "model": "gemini-2.0-flash", "delta": {"tool_calls": [{"index": 0, "function": {"arguments": "\"buy milk\"]}"}}]}, "finish_reason": null}),
            json!({"id": "resp_123", "model": "gemini-2.0-flash", "delta": {}, "finish_reason": "stop"}),
        ]);
        let parts = fc_parts(&out);
        assert_eq!(
            parts,
            vec![
                json!({"functionCall": {"name": "manage_todo_list", "args": {"items": ["buy milk"]}, "id": "call_1"}})
            ]
        );
        let finish = out
            .iter()
            .find(|(_, d)| truthy(get(d, &["candidates", "0", "finishReason"])))
            .unwrap();
        assert_eq!(finish.1["candidates"][0]["finishReason"], "STOP");
        assert_eq!(
            finish.1["candidates"][0]["content"]["parts"],
            json!([{"text": ""}])
        );
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn interleaved_parallel_tool_calls() {
        let out = run(&[
            json!({"delta": {"tool_calls": [{"index": 0, "id": "call_1", "type": "function", "function": {"name": "get_date", "arguments": "{\"timezone\":\""}}]}, "finish_reason": null}),
            json!({"delta": {"tool_calls": [{"index": 1, "id": "call_2", "type": "function", "function": {"name": "add_tasks", "arguments": "{\"tasks\":[\"buy milk\""}}]}, "finish_reason": null}),
            json!({"delta": {"tool_calls": [{"index": 1, "function": {"arguments": ",\"clean kitchen\"]}"}}]}, "finish_reason": null}),
            json!({"delta": {"tool_calls": [{"index": 0, "function": {"arguments": "UTC\"}"}}]}, "finish_reason": "stop"}),
        ]);
        let parts = fc_parts(&out);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["functionCall"]["name"], "add_tasks");
        assert_eq!(
            parts[0]["functionCall"]["args"],
            json!({"tasks": ["buy milk", "clean kitchen"]})
        );
        assert_eq!(parts[1]["functionCall"]["name"], "get_date");
        assert_eq!(parts[1]["functionCall"]["args"], json!({"timezone": "UTC"}));
        assert_eq!(
            out.last().unwrap().1["candidates"][0]["finishReason"],
            "STOP"
        );
    }

    #[test]
    fn openai_compat_ids_and_signatures() {
        let out = run(&[
            json!({"id": "resp_real", "model": "gemini-3.5-flash", "delta": {"role": "assistant", "tool_calls": [{"id": "xjcm5z4x", "type": "function", "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}, "extra_content": {"google": {"thought_signature": "sig-abc"}}}]}, "finish_reason": null}),
            json!({"id": "resp_real", "model": "gemini-3.5-flash", "delta": {"role": "assistant", "tool_calls": [{"id": "pc5lscvl", "type": "function", "function": {"name": "get_weather", "arguments": "{\"city\":\"Tokyo\"}"}}]}, "finish_reason": null}),
            json!({"id": "resp_real", "model": "gemini-3.5-flash", "delta": {"role": "assistant"}, "finish_reason": "stop"}),
        ]);
        let parts = fc_parts(&out);
        assert_eq!(
            parts,
            vec![
                json!({"functionCall": {"name": "get_weather", "args": {"city": "Paris"}, "id": "xjcm5z4x"}, "thoughtSignature": "sig-abc"}),
                json!({"functionCall": {"name": "get_weather", "args": {"city": "Tokyo"}, "id": "pc5lscvl"}}),
            ]
        );
        assert_eq!(out[0].1["modelVersion"], "gemini-3.5-flash");
        assert_eq!(out[0].1["responseId"], "resp_real");
        assert_eq!(out[0].1["candidates"][0]["finishReason"], Value::Null);
    }

    #[test]
    fn content_usage_and_finish() {
        let out = run(&[
            json!({"id": "resp_123", "model": "gemini-2.0-flash", "delta": {"content": "Response", "reasoning_content": "r"}, "finish_reason": "tool_calls",
            "usage": {"input_tokens": 100, "output_tokens": 50, "total_tokens": 150, "reasoning_tokens": 25, "cached_tokens": 20}}),
        ]);
        assert_eq!(
            out[0].1,
            json!({
                "candidates": [{"content": {"role": "model", "parts": [{"text": "Response"}, {"text": "r", "thought": true}]}, "finishReason": "STOP", "index": 0}],
                "usageMetadata": {"promptTokenCount": 120, "candidatesTokenCount": 50, "totalTokenCount": 150, "thoughtsTokenCount": 25, "cachedContentTokenCount": 20},
                "modelVersion": "gemini-2.0-flash", "responseId": "resp_123"
            })
        );
        let out = run(&[
            json!({"delta": {"content": "x"}, "finish_reason": "stop", "usage": {"input_tokens": 1, "output_tokens": 2, "total_tokens": 3, "reasoning_tokens": 0, "cached_tokens": 0}}),
        ]);
        assert_eq!(
            out[0].1["usageMetadata"],
            json!({"promptTokenCount": 1, "candidatesTokenCount": 2, "totalTokenCount": 3})
        );
        assert!(run(&[json!({"delta": {}})]).is_empty());
    }
}
