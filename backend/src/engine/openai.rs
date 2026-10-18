//! `transformers/openai.ts` (Chat Completions).
use super::image_rendering::compose_content;
use super::js::*;
use super::sse::{self, SseEvent};
use super::transformer::*;
use super::usage::normalize_openai_chat;
use serde_json::{json, Value};

pub const GEMINI_MALFORMED_FUNCTION_CALL_CODE: &str = "MALFORMED_FUNCTION_CALL";

pub struct OpenAITransformer;

/// Unified usage → Chat Completions usage payload (`buildChatUsagePayload`).
pub fn chat_usage_payload(usage: &Value) -> Value {
    let input = get_f64(usage, &["input_tokens"]).unwrap_or(0.0);
    let cached = get(usage, &["cached_tokens"]).filter(|v| truthy(Some(v))).and_then(Value::as_f64).unwrap_or(0.0);
    json!({
        "prompt_tokens": num(input + cached),
        "completion_tokens": usage.get("output_tokens").cloned().unwrap_or(Value::Null),
        "total_tokens": usage.get("total_tokens").cloned().unwrap_or(Value::Null),
        "prompt_tokens_details": if truthy(usage.get("cached_tokens")) { json!({"cached_tokens": usage["cached_tokens"]}) } else { Value::Null },
        "reasoning_tokens": usage.get("reasoning_tokens").cloned().unwrap_or(Value::Null),
    })
}

impl Transformer for OpenAITransformer {
    fn name(&self) -> &'static str { "chat" }
    fn default_endpoint(&self) -> &'static str { "/chat/completions" }

    fn parse_request(&self, input: &Value) -> EngineResult<Value> {
        let messages = match input.get("messages") {
            Some(Value::Array(msgs)) => Value::Array(msgs.iter().map(|msg| {
                if get_str(msg, &["role"]) == Some("assistant") && !nullish(msg.get("reasoning_content")) {
                    let mut rest = msg.as_object().cloned().unwrap_or_default();
                    let reasoning = rest.shift_remove("reasoning_content");
                    let reasoning_text = reasoning.as_ref().and_then(Value::as_str).unwrap_or("").to_owned();
                    let content = match rest.get("content") {
                        Some(Value::String(s)) => if s.is_empty() { json!([]) } else { json!([{"type": "text", "text": s}]) },
                        Some(Value::Array(a)) => Value::Array(a.clone()),
                        _ => json!([]),
                    };
                    rest.insert("content".into(), content);
                    let thinking = if truthy(msg.get("thinking")) { msg["thinking"].clone() } else { json!({"content": reasoning_text}) };
                    rest.insert("thinking".into(), thinking);
                    Value::Object(rest)
                } else { msg.clone() }
            }).collect()),
            other => other.cloned().unwrap_or(Value::Null),
        };
        let positive = |v: Option<&Value>| v.and_then(Value::as_f64).filter(|n| *n > 0.0);
        let max_tokens = positive(input.get("max_completion_tokens")).or_else(|| positive(input.get("max_tokens")));
        let mut out = obj(vec![
            ("messages", if input.get("messages").is_some() { Some(messages) } else { None }),
            ("model", input.get("model").cloned()),
            ("max_tokens", max_tokens.map(num)),
            ("temperature", input.get("temperature").cloned()),
            ("stream", input.get("stream").cloned()),
            ("tools", input.get("tools").cloned()),
            ("tool_choice", input.get("tool_choice").cloned()),
            ("reasoning", input.get("reasoning").cloned()),
        ]);
        let _ = &mut out;
        Ok(out)
    }

    fn transform_request(&self, request: &Value) -> EngineResult<Value> {
        let messages = match get(request, &["systemInstruction", "content"]).filter(|c| truthy(Some(c))) {
            Some(content) => {
                let mut msgs = vec![json!({"role": "system", "content": content})];
                msgs.extend(get_arr(request, &["messages"]).cloned().unwrap_or_default());
                Value::Array(msgs)
            }
            None => request.get("messages").cloned().unwrap_or(Value::Null),
        };
        let normalized_tools: Option<Vec<Value>> = get_arr(request, &["tools"]).filter(|t| !t.is_empty()).map(|tools| tools.iter().map(|t| {
            if get_str(t, &["type"]) != Some("function") || !truthy(t.get("function")) { return t.clone(); }
            let f = &t["function"];
            let parameters = coalesce(f.get("parametersJsonSchema"), f.get("parameters"));
            json!({"type": "function", "function": obj(vec![
                ("name", f.get("name").cloned()),
                ("description", f.get("description").cloned()),
                ("parameters", parameters),
                ("strict", f.get("strict").cloned()),
            ])})
        }).collect());
        let original = request.get("originalBody");
        let same_api = truthy(original) && get_str(request, &["incomingApiType"]).is_some_and(|t| t.eq_ignore_ascii_case("chat"));
        let mut out = if same_api { original.and_then(Value::as_object).cloned().unwrap_or_default() } else { Obj::new() };
        set_opt(&mut out, "model", request.get("model").cloned());
        if request.get("model").is_none() { out.shift_remove("model"); }
        out.insert("messages".into(), messages);
        if request.get("messages").is_none() && !truthy(get(request, &["systemInstruction", "content"])) { out.shift_remove("messages"); }
        let caller_mct = original.and_then(|o| o.get("max_completion_tokens")).is_some();
        let caller_mt = original.and_then(|o| o.get("max_tokens")).is_some();
        if let Some(mt) = request.get("max_tokens") {
            out.shift_remove("max_tokens");
            out.shift_remove("max_completion_tokens");
            let key = if caller_mct { "max_completion_tokens" } else { "max_tokens" };
            out.insert(key.into(), mt.clone());
        }
        assign(&mut out, "temperature", request.get("temperature"));
        assign(&mut out, "stream", request.get("stream"));
        assign(&mut out, "tools", normalized_tools.filter(|t| !t.is_empty()).map(Value::Array).as_ref());
        assign(&mut out, "tool_choice", request.get("tool_choice"));
        let _ = caller_mt;
        if let Some(rf) = request.get("response_format").filter(|v| truthy(Some(v))) {
            if get_str(rf, &["type"]) == Some("json_schema") && truthy(rf.get("json_schema")) {
                out.insert("response_format".into(), json!({"type": "json_schema", "json_schema": obj(vec![
                    ("name", Some(coalesce(rf.get("name"), Some(&json!("response_schema"))).unwrap_or(Value::Null))),
                    ("description", rf.get("description").cloned()),
                    ("schema", rf.get("json_schema").cloned()),
                    ("strict", Some(coalesce(rf.get("strict"), Some(&json!(true))).unwrap_or(Value::Null))),
                ])}));
            } else {
                out.insert("response_format".into(), rf.clone());
            }
        }
        if truthy(request.get("reasoning")) { out.insert("reasoning".into(), request["reasoning"].clone()); }
        if let Some(p) = request.get("parallel_tool_calls") { out.insert("parallel_tool_calls".into(), p.clone()); }
        Ok(Value::Object(out))
    }

    fn transform_response(&self, response: &Value) -> EngineResult<Value> {
        let choice = get(response, &["choices", "0"]);
        let message = choice.and_then(|c| c.get("message"));
        let usage = response.get("usage").filter(|u| truthy(Some(u))).map(normalize_openai_chat);
        Ok(obj(vec![
            ("id", response.get("id").cloned()),
            ("model", response.get("model").cloned()),
            ("created", response.get("created").cloned()),
            ("content", Some(or(message.and_then(|m| m.get("content")), None).unwrap_or(Value::Null))),
            ("reasoning_content", Some(coalesce(message.and_then(|m| m.get("reasoning_content")), message.and_then(|m| m.get("reasoning"))).filter(|v| !v.is_null()).unwrap_or(Value::Null))),
            ("tool_calls", message.and_then(|m| m.get("tool_calls")).cloned()),
            ("usage", usage),
            ("finishReason", Some(or(choice.and_then(|c| c.get("finish_reason")), None).unwrap_or(Value::Null))),
        ]))
    }

    fn format_response(&self, response: &Value) -> EngineResult<Value> {
        let mut message = obj(vec![
            ("role", Some(json!("assistant"))),
            ("content", compose_content(response.get("content"), response.get("image_generation_calls"))),
            ("reasoning_content", response.get("reasoning_content").cloned()),
            ("tool_calls", response.get("tool_calls").cloned()),
        ]);
        if get_arr(response, &["annotations"]).is_some_and(|a| !a.is_empty()) { message["annotations"] = response["annotations"].clone(); }
        let created = if truthy(response.get("created")) { response["created"].clone() } else { json!(now_secs()) };
        Ok(obj(vec![
            ("id", response.get("id").cloned()),
            ("object", Some(json!("chat.completion"))),
            ("created", Some(created)),
            ("model", response.get("model").cloned()),
            ("choices", Some(json!([{"index": 0, "message": message, "finish_reason": if truthy(response.get("tool_calls")) { "tool_calls" } else { "stop" }}]))),
            ("usage", response.get("usage").filter(|u| truthy(Some(u))).map(chat_usage_payload)),
        ]))
    }

    fn stream_transformer(&self) -> Option<Box<dyn StreamTransformer>> { Some(Box::new(OpenAIStreamTransformer)) }
    fn stream_formatter(&self) -> Option<Box<dyn StreamFormatter>> { Some(Box::new(OpenAIStreamFormatter::default())) }

    fn extract_usage(&self, data: &str) -> Option<Value> {
        let data = parse(data)?;
        let u = data.get("usage").filter(|u| truthy(Some(u)))?;
        let n = normalize_openai_chat(u);
        Some(json!({"input_tokens": n["input_tokens"], "output_tokens": n["output_tokens"], "cached_tokens": n["cached_tokens"], "cache_creation_tokens": n["cache_creation_tokens"], "reasoning_tokens": n["reasoning_tokens"]}))
    }
}

/// `out.key = value` where an undefined value deletes the member.
fn assign(out: &mut Obj, key: &str, value: Option<&Value>) {
    match value { Some(v) => { out.insert(key.into(), v.clone()); } None => { out.shift_remove(key); } }
}

pub struct OpenAIStreamTransformer;

impl StreamTransformer for OpenAIStreamTransformer {
    fn on_event(&mut self, event: &SseEvent) -> Vec<Value> {
        if event.data == "[DONE]" { return vec![]; }
        let Some(data) = parse(&event.data) else { return vec![] };
        let choice = get(&data, &["choices", "0"]);
        let delta = choice.and_then(|c| c.get("delta"));
        let usage = data.get("usage").filter(|u| truthy(Some(u))).map(normalize_openai_chat);
        let finish = or(choice.and_then(|c| c.get("finish_reason")), data.get("finish_reason"))
            .filter(|v| truthy(Some(v)))
            .unwrap_or(if delta.is_some() { Value::Null } else { json!("stop") });
        vec![obj(vec![
            ("id", data.get("id").cloned()),
            ("model", data.get("model").cloned()),
            ("created", data.get("created").cloned()),
            ("delta", Some(obj(vec![
                ("role", delta.and_then(|d| d.get("role")).cloned()),
                ("content", delta.and_then(|d| d.get("content")).cloned()),
                ("reasoning_content", coalesce(delta.and_then(|d| d.get("reasoning_content")), delta.and_then(|d| d.get("reasoning")))),
                ("tool_calls", delta.and_then(|d| d.get("tool_calls")).cloned()),
            ]))),
            ("finish_reason", Some(finish)),
            ("usage", usage),
        ])]
    }
}

#[derive(Default)]
pub struct OpenAIStreamFormatter {
    has_sent_error: bool,
    suppress_done: bool,
    has_sent_finish: bool,
    forwarded_trailing_usage: bool,
    last_id: Option<Value>,
    last_model: Option<Value>,
}

fn is_empty_delta(delta: Option<&Value>) -> bool {
    match delta { None | Some(Value::Null) => true, Some(d) => !truthy(d.get("role")) && !truthy(d.get("content")) && !truthy(d.get("reasoning_content")) && !truthy(d.get("tool_calls")) }
}

fn created_or_now(chunk: &Value) -> Value { if truthy(chunk.get("created")) { chunk["created"].clone() } else { json!(now_secs()) } }

impl StreamFormatter for OpenAIStreamFormatter {
    fn on_chunk(&mut self, chunk: &Value) -> Vec<String> {
        if self.has_sent_error { return vec![]; }
        if let Some(id) = chunk.get("id").filter(|v| !v.is_null()) { self.last_id = Some(id.clone()); }
        if let Some(m) = chunk.get("model").filter(|v| !v.is_null()) { self.last_model = Some(m.clone()); }
        let usage_payload = chunk.get("usage").filter(|u| truthy(Some(u))).map(chat_usage_payload);
        if self.has_sent_finish {
            let trailing = !self.forwarded_trailing_usage && get_str(chunk, &["event"]) != Some("error") && truthy(chunk.get("usage")) && is_empty_delta(chunk.get("delta"));
            if !trailing { return vec![]; }
            self.forwarded_trailing_usage = true;
            let payload = obj(vec![("id", chunk.get("id").cloned()), ("object", Some(json!("chat.completion.chunk"))), ("created", Some(created_or_now(chunk))), ("model", chunk.get("model").cloned()), ("choices", Some(json!([]))), ("usage", usage_payload)]);
            return vec![sse::data(&stringify(&payload))];
        }
        if get_str(chunk, &["event"]) == Some("error") {
            if truthy(chunk.get("finish_reason")) {
                let payload = obj(vec![("id", chunk.get("id").cloned()), ("object", Some(json!("chat.completion.chunk"))), ("created", Some(created_or_now(chunk))), ("model", chunk.get("model").cloned()),
                    ("choices", Some(json!([{"index": 0, "delta": {}, "finish_reason": chunk["finish_reason"]}]))), ("usage", usage_payload)]);
                self.has_sent_finish = true;
                return vec![sse::data(&stringify(&payload))];
            }
            let malformed = get_str(chunk, &["error", "code"]) == Some(GEMINI_MALFORMED_FUNCTION_CALL_CODE);
            let code = if malformed { json!("upstream_malformed_function_call") } else { or(get(chunk, &["error", "code"]), None).unwrap_or(json!("upstream_error")) };
            let payload = obj(vec![("error", Some(obj(vec![("message", get(chunk, &["error", "message"]).cloned()), ("type", Some(json!("server_error"))), ("code", Some(code))]))), ("usage", usage_payload)]);
            self.has_sent_error = true;
            self.has_sent_finish = true;
            if malformed { self.suppress_done = true; }
            return vec![sse::data(&stringify(&payload))];
        }
        if truthy(chunk.get("finish_reason")) { self.has_sent_finish = true; }
        let choice = obj(vec![("index", Some(json!(0))), ("delta", chunk.get("delta").cloned()), ("finish_reason", chunk.get("finish_reason").cloned())]);
        let payload = obj(vec![("id", chunk.get("id").cloned()), ("object", Some(json!("chat.completion.chunk"))), ("created", Some(created_or_now(chunk))), ("model", chunk.get("model").cloned()), ("choices", Some(json!([choice]))), ("usage", usage_payload)]);
        vec![sse::data(&stringify(&payload))]
    }

    fn finish(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        if !self.has_sent_finish {
            let payload = obj(vec![
                ("id", Some(self.last_id.clone().unwrap_or_else(|| json!(format!("chatcmpl_{}", uuid()))))),
                ("object", Some(json!("chat.completion.chunk"))),
                ("created", Some(json!(now_secs()))),
                ("model", self.last_model.clone()),
                ("choices", Some(json!([{"index": 0, "delta": {}, "finish_reason": "stop"}]))),
            ]);
            out.push(sse::data(&stringify(&payload)));
        }
        if !self.suppress_done { out.push(sse::data("[DONE]")); }
        out
    }
}
