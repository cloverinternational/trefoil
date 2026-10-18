//! `transformers/responses/stream-formatter.ts`: unified chunks → client
//! Responses API SSE (`response.created` … `response.completed` /
//! `response.incomplete` / `response.failed`, with `sequence_number`s).
use super::js_ext::{js_trim, js_trim_start, strict_eq};
use super::tool_mapper::{
    custom_tool_input, resolve_output_tool_call, ResolvedToolType, ResponsesToolState,
};
use super::utils::{build_responses_usage_payload, generate_item_id, generate_response_id};
use crate::engine::js::*;
use crate::engine::sse;
use crate::engine::transformer::StreamFormatter;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Mutex};

/// Per-tool-call bookkeeping (`toolOutputIndexMap` … `toolNameMap`).
#[derive(Debug, Clone)]
struct ToolEntry {
    key: Value,
    output_index: u64,
    call_id: Value,
    item_id: String,
    args: String,
    flat_name: Value,
}

/// `normalizeToolArgs`: a delta that is itself a complete JSON object
/// snapshot extending what we have replaces the buffer; otherwise append.
pub fn normalize_tool_args(previous: &str, delta: &str) -> String {
    if delta.is_empty() {
        return previous.to_owned();
    }
    let trimmed = js_trim(delta);
    if trimmed.starts_with('{')
        && trimmed.ends_with('}')
        && parse(trimmed).is_some()
        && (previous.is_empty() || trimmed.starts_with(js_trim_start(previous)))
    {
        return trimmed.to_owned();
    }
    format!("{previous}{delta}")
}

/// State of one `formatResponsesStream` pipe. Consults the owning
/// transformer's tool state on every chunk, like the TS closure does.
pub struct ResponsesStreamFormatter {
    state: Option<Arc<Mutex<ResponsesToolState>>>,
    finished: bool,
    has_sent_created: bool,
    has_sent_in_progress: bool,
    response_id: Value,
    response_model: Value,
    response_created_at: Value,
    message_item_sent: bool,
    message_item_id: String,
    message_text: String,
    message_part_added: bool,
    message_output_index: Option<u64>,
    reasoning_item_sent: bool,
    reasoning_item_id: String,
    reasoning_text: String,
    reasoning_output_index: Option<u64>,
    reasoning_summary_text: String,
    reasoning_content_index: u64,
    reasoning_summary_index: u64,
    reasoning_summary_part_added: bool,
    last_usage: Option<Value>,
    sequence_number: u64,
    next_output_index: u64,
    used_output_indices: HashSet<u64>,
    output_items_by_index: BTreeMap<u64, Value>,
    tools: Vec<ToolEntry>,
}

impl ResponsesStreamFormatter {
    pub fn new(state: Option<Arc<Mutex<ResponsesToolState>>>) -> Self {
        Self {
            state,
            finished: false,
            has_sent_created: false,
            has_sent_in_progress: false,
            response_id: json!(""),
            response_model: json!(""),
            response_created_at: json!(0),
            message_item_sent: false,
            message_item_id: String::new(),
            message_text: String::new(),
            message_part_added: false,
            message_output_index: None,
            reasoning_item_sent: false,
            reasoning_item_id: String::new(),
            reasoning_text: String::new(),
            reasoning_output_index: None,
            reasoning_summary_text: String::new(),
            reasoning_content_index: 0,
            reasoning_summary_index: 0,
            reasoning_summary_part_added: false,
            last_usage: None,
            sequence_number: 0,
            next_output_index: 0,
            used_output_indices: HashSet::new(),
            output_items_by_index: BTreeMap::new(),
            tools: Vec::new(),
        }
    }

    fn resolve(&self, flat_name: &Value) -> super::tool_mapper::ResolvedToolCall {
        match &self.state {
            Some(s) => {
                let guard = s.lock().unwrap_or_else(|e| e.into_inner());
                resolve_output_tool_call(Some(flat_name), Some(&guard))
            }
            None => resolve_output_tool_call(Some(flat_name), None),
        }
    }

    /// `sendEvent`: `{...data, sequence_number}` as `event: <type>` SSE.
    fn send(&mut self, out: &mut Vec<String>, mut data: Value) {
        data["sequence_number"] = json!(self.sequence_number);
        self.sequence_number += 1;
        let event = data.get("type").and_then(Value::as_str).map(str::to_owned);
        out.push(sse::encode(event.as_deref(), &stringify(&data)));
    }

    fn created_at_or_now(&self) -> Value {
        if truthy(Some(&self.response_created_at)) {
            self.response_created_at.clone()
        } else {
            json!(now_secs())
        }
    }

    fn response_id_or_undefined(&self) -> Option<Value> {
        Some(self.response_id.clone()).filter(|v| truthy(Some(v)))
    }

    fn ensure_created(&mut self, out: &mut Vec<String>, chunk: &Value) {
        if self.has_sent_created {
            return;
        }
        self.response_id =
            or(chunk.get("id"), None).unwrap_or_else(|| Value::String(generate_response_id()));
        self.response_model =
            or(chunk.get("model"), Some(&self.response_model)).unwrap_or_default();
        self.response_created_at =
            or(chunk.get("created"), None).unwrap_or_else(|| json!(now_secs()));
        let data = json!({
            "type": "response.created",
            "response": {
                "id": self.response_id,
                "object": "response",
                "created_at": self.response_created_at,
                "status": "in_progress",
                "model": self.response_model,
                "output": [],
            },
        });
        self.send(out, data);
        self.has_sent_created = true;
    }

    fn reserve_output_index(&mut self) -> u64 {
        while self.used_output_indices.contains(&self.next_output_index) {
            self.next_output_index += 1;
        }
        let index = self.next_output_index;
        self.used_output_indices.insert(index);
        self.next_output_index += 1;
        index
    }

    fn ensure_in_progress(&mut self, out: &mut Vec<String>) {
        if self.has_sent_in_progress {
            return;
        }
        let data = json!({
            "type": "response.in_progress",
            "response": {
                "id": self.response_id,
                "object": "response",
                "created_at": self.response_created_at,
                "status": "in_progress",
                "model": self.response_model,
                "output": [],
            },
        });
        self.send(out, data);
        self.has_sent_in_progress = true;
    }

    fn ensure_message_item(&mut self, out: &mut Vec<String>) {
        if self.message_item_sent {
            return;
        }
        if self.message_output_index.is_none() {
            self.message_output_index = Some(self.reserve_output_index());
        }
        let index = self.message_output_index.unwrap_or(0);
        self.message_item_id = generate_item_id("msg");
        let added = json!({
            "type": "response.output_item.added",
            "output_index": index,
            "item": {"id": self.message_item_id, "type": "message", "status": "in_progress", "role": "assistant", "content": []},
        });
        self.send(out, added);
        if !self.message_part_added {
            let part = json!({
                "type": "response.content_part.added",
                "output_index": index,
                "item_id": self.message_item_id,
                "content_index": 0,
                "part": {"type": "output_text", "annotations": [], "logprobs": [], "text": ""},
            });
            self.send(out, part);
            self.message_part_added = true;
        }
        self.message_item_sent = true;
    }

    fn ensure_reasoning_item(&mut self, out: &mut Vec<String>) {
        if self.reasoning_item_sent {
            return;
        }
        let index = self.reserve_output_index();
        self.reasoning_output_index = Some(index);
        self.reasoning_item_id = generate_item_id("rs");
        let added = json!({
            "type": "response.output_item.added",
            "output_index": index,
            "item": {"id": self.reasoning_item_id, "type": "reasoning", "status": "in_progress", "content": [], "summary": []},
        });
        self.send(out, added);
        self.reasoning_item_sent = true;
    }

    fn tool_position(&self, key: &Value) -> Option<usize> {
        self.tools
            .iter()
            .position(|t| strict_eq(Some(&t.key), Some(key)))
    }

    fn ensure_tool_item(&mut self, out: &mut Vec<String>, key: &Value, tool_call: &Value) {
        if self.tool_position(key).is_some() {
            return;
        }
        let output_index = self.reserve_output_index();
        let call_id = or(tool_call.get("id"), None)
            .unwrap_or_else(|| Value::String(generate_item_id("call")));
        let item_id = generate_item_id("fc");
        let flat_name = or(get(tool_call, &["function", "name"]), tool_call.get("name"))
            .filter(|v| truthy(Some(v)))
            .unwrap_or_else(|| json!(""));
        self.tools.push(ToolEntry {
            key: key.clone(),
            output_index,
            call_id: call_id.clone(),
            item_id: item_id.clone(),
            args: String::new(),
            flat_name: flat_name.clone(),
        });

        // Namespace/custom split-back for the streamed "added" event.
        let resolved = self.resolve(&flat_name);
        let item = if resolved.kind == ResolvedToolType::CustomToolCall {
            obj(vec![
                ("id", Some(json!(item_id))),
                ("type", Some(json!("custom_tool_call"))),
                ("status", Some(json!("in_progress"))),
                ("call_id", Some(call_id)),
                ("name", resolved.name),
                ("input", Some(json!(""))),
            ])
        } else {
            obj(vec![
                ("id", Some(json!(item_id))),
                ("type", Some(json!("function_call"))),
                ("status", Some(json!("in_progress"))),
                ("call_id", Some(call_id)),
                ("name", resolved.name),
                ("namespace", resolved.namespace.filter(|n| truthy(Some(n)))),
                ("arguments", Some(json!(""))),
            ])
        };
        self.send(out, json!({"type": "response.output_item.added", "output_index": output_index, "item": item}));
    }

    /// Typed image_generation_call carries → native output items (full base64).
    fn emit_image_generation_call_items(&mut self, out: &mut Vec<String>, image_calls: &[Value]) {
        for image_call in image_calls {
            let output_index = self.reserve_output_index();
            let item_id = or(image_call.get("id"), None)
                .unwrap_or_else(|| Value::String(generate_item_id("ig")));
            let done_item = json!({
                "id": item_id,
                "type": "image_generation_call",
                "status": or(image_call.get("status"), Some(&json!("completed"))),
                "result": image_call["result"],
            });
            let mut added_item = done_item.clone();
            added_item["status"] = json!("in_progress");
            added_item["result"] = Value::Null;
            self.send(out, json!({"type": "response.output_item.added", "output_index": output_index, "item": added_item}));
            self.send(out, json!({"type": "response.output_item.done", "output_index": output_index, "item": done_item}));
            self.output_items_by_index.insert(output_index, done_item);
        }
    }

    /// Typed client-executed tool-call carries → native output items, untouched.
    fn emit_client_tool_call_items(&mut self, out: &mut Vec<String>, calls: &[Value]) {
        for call in calls {
            let output_index = self.reserve_output_index();
            let added_item = spread(call, &json!({"status": "in_progress"}));
            self.send(out, json!({"type": "response.output_item.added", "output_index": output_index, "item": added_item}));
            self.send(out, json!({"type": "response.output_item.done", "output_index": output_index, "item": call}));
            self.output_items_by_index
                .insert(output_index, call.clone());
        }
    }

    /// `finalizeOutputItems`: closes every open item (stamping `item_status`)
    /// and returns all items ordered by output index.
    fn finalize_output_items(&mut self, out: &mut Vec<String>, item_status: &str) -> Vec<Value> {
        if let (true, Some(index)) = (self.reasoning_item_sent, self.reasoning_output_index) {
            let content = if self.reasoning_text.is_empty() {
                json!([])
            } else {
                json!([{"type": "reasoning_text", "text": self.reasoning_text}])
            };
            let summary = if self.reasoning_summary_text.is_empty() {
                json!([])
            } else {
                json!([{"type": "summary_text", "text": self.reasoning_summary_text}])
            };
            let item = json!({"id": self.reasoning_item_id, "type": "reasoning", "status": item_status, "content": content, "summary": summary});
            if !self.reasoning_text.is_empty() {
                let e = json!({"type": "response.reasoning_text.done", "output_index": index, "item_id": self.reasoning_item_id, "content_index": self.reasoning_content_index, "text": self.reasoning_text});
                self.send(out, e);
            }
            if !self.reasoning_summary_text.is_empty() {
                let e = json!({"type": "response.reasoning_summary_text.done", "output_index": index, "item_id": self.reasoning_item_id, "summary_index": self.reasoning_summary_index, "text": self.reasoning_summary_text});
                self.send(out, e);
                if self.reasoning_summary_part_added {
                    let e = json!({"type": "response.reasoning_summary_part.done", "output_index": index, "item_id": self.reasoning_item_id, "summary_index": self.reasoning_summary_index, "part": {"type": "summary_text", "text": self.reasoning_summary_text}});
                    self.send(out, e);
                }
            }
            self.send(
                out,
                json!({"type": "response.output_item.done", "output_index": index, "item": item}),
            );
            self.output_items_by_index.insert(index, item);
        }

        if self.message_item_sent {
            let index = self.message_output_index.unwrap_or(0);
            let item = json!({
                "id": self.message_item_id,
                "type": "message",
                "status": item_status,
                "role": "assistant",
                "content": [{"type": "output_text", "annotations": [], "logprobs": [], "text": self.message_text}],
            });
            let e = json!({"type": "response.output_text.done", "output_index": index, "item_id": self.message_item_id, "content_index": 0, "logprobs": [], "text": self.message_text});
            self.send(out, e);
            let e = json!({"type": "response.content_part.done", "output_index": index, "item_id": self.message_item_id, "content_index": 0, "part": {"type": "output_text", "annotations": [], "logprobs": [], "text": self.message_text}});
            self.send(out, e);
            self.send(
                out,
                json!({"type": "response.output_item.done", "output_index": index, "item": item}),
            );
            self.output_items_by_index.insert(index, item);
        }

        for entry in self.tools.clone() {
            let resolved = self.resolve(&entry.flat_name);
            let item = if resolved.kind == ResolvedToolType::CustomToolCall {
                obj(vec![
                    ("id", Some(json!(entry.item_id))),
                    ("type", Some(json!("custom_tool_call"))),
                    ("status", Some(json!(item_status))),
                    ("call_id", Some(entry.call_id)),
                    ("name", resolved.name),
                    (
                        "input",
                        Some(Value::String(custom_tool_input(Some(&Value::String(
                            entry.args,
                        ))))),
                    ),
                ])
            } else {
                obj(vec![
                    ("id", Some(json!(entry.item_id))),
                    ("type", Some(json!("function_call"))),
                    ("status", Some(json!(item_status))),
                    ("call_id", Some(entry.call_id)),
                    ("name", resolved.name),
                    ("namespace", resolved.namespace.filter(|n| truthy(Some(n)))),
                    ("arguments", Some(Value::String(entry.args))),
                ])
            };
            self.send(out, json!({"type": "response.output_item.done", "output_index": entry.output_index, "item": item}));
            self.output_items_by_index.insert(entry.output_index, item);
        }

        self.output_items_by_index.values().cloned().collect()
    }

    fn terminal_response(
        &self,
        status: &str,
        output: Vec<Value>,
        extra: Vec<(&str, Option<Value>)>,
    ) -> Value {
        let mut pairs = vec![
            ("id", self.response_id_or_undefined()),
            ("object", Some(json!("response"))),
            ("created_at", Some(self.created_at_or_now())),
            ("status", Some(json!(status))),
            ("model", Some(self.response_model.clone())),
            ("output", Some(Value::Array(output))),
        ];
        pairs.extend(extra);
        pairs.push((
            "usage",
            build_responses_usage_payload(self.last_usage.as_ref()),
        ));
        obj(pairs)
    }

    fn complete(&mut self, out: &mut Vec<String>) {
        let output = self.finalize_output_items(out, "completed");
        let response = self.terminal_response("completed", output, vec![]);
        self.send(
            out,
            json!({"type": "response.completed", "response": response}),
        );
        self.finished = true;
    }
}

impl StreamFormatter for ResponsesStreamFormatter {
    fn on_chunk(&mut self, chunk: &Value) -> Vec<String> {
        let mut out = Vec::new();
        if self.finished {
            return out;
        }
        self.ensure_created(&mut out, chunk);
        self.ensure_in_progress(&mut out);

        if get_str(chunk, &["event"]) == Some("error") {
            if truthy(chunk.get("usage")) {
                self.last_usage = chunk.get("usage").cloned();
            }
            let incomplete = chunk
                .get("incomplete_details")
                .filter(|d| truthy(Some(d)))
                .cloned();
            let output = self.finalize_output_items(
                &mut out,
                if incomplete.is_some() {
                    "incomplete"
                } else {
                    "completed"
                },
            );
            let empty = json!({});
            let err = or(chunk.get("error"), Some(&empty)).unwrap_or_default();
            if let Some(details) = incomplete {
                let response = self.terminal_response(
                    "incomplete",
                    output,
                    vec![("incomplete_details", Some(details))],
                );
                self.send(
                    &mut out,
                    json!({"type": "response.incomplete", "response": response}),
                );
            } else {
                let error = json!({
                    "code": or(err.get("code"), Some(&json!("server_error"))),
                    "message": or(err.get("message"), Some(&json!("The model response failed to complete."))),
                });
                let response =
                    self.terminal_response("failed", output, vec![("error", Some(error))]);
                self.send(
                    &mut out,
                    json!({"type": "response.failed", "response": response}),
                );
            }
            self.finished = true;
            return out;
        }

        if truthy(chunk.get("usage")) {
            self.last_usage = chunk.get("usage").cloned();
        }

        let empty = json!({});
        let delta = chunk
            .get("delta")
            .filter(|d| truthy(Some(d)))
            .unwrap_or(&empty);
        let reasoning_delta = delta.get("reasoning_content").and_then(Value::as_str);
        let summary_delta = get_str(delta, &["thinking", "content"]);

        // Typed image carries re-emit natively; the paired markdown content
        // delta on the same chunk is skipped below.
        let typed_image_calls: Vec<Value> = match chunk.get("image_generation_calls") {
            Some(Value::Array(calls)) => calls
                .iter()
                .filter(|c| {
                    truthy(Some(c)) && get_str(c, &["result"]).is_some_and(|r| !r.is_empty())
                })
                .cloned()
                .collect(),
            _ => Vec::new(),
        };
        if !typed_image_calls.is_empty() {
            self.emit_image_generation_call_items(&mut out, &typed_image_calls);
        }

        if let Some(Value::Array(calls)) = chunk.get("client_tool_calls") {
            if !calls.is_empty() {
                let calls = calls.clone();
                self.emit_client_tool_call_items(&mut out, &calls);
            }
        }

        if let Some(rd) = reasoning_delta.filter(|d| !d.is_empty()) {
            self.ensure_reasoning_item(&mut out);
            self.reasoning_text.push_str(rd);
            let e = json!({"type": "response.reasoning_text.delta", "output_index": self.reasoning_output_index, "item_id": self.reasoning_item_id, "content_index": self.reasoning_content_index, "delta": rd});
            self.send(&mut out, e);
        }

        if let Some(sd) = summary_delta.filter(|d| !d.is_empty()) {
            self.ensure_reasoning_item(&mut out);
            if !self.reasoning_summary_part_added {
                let e = json!({"type": "response.reasoning_summary_part.added", "output_index": self.reasoning_output_index, "item_id": self.reasoning_item_id, "summary_index": self.reasoning_summary_index, "part": {"type": "summary_text", "text": ""}});
                self.send(&mut out, e);
                self.reasoning_summary_part_added = true;
            }
            self.reasoning_summary_text.push_str(sd);
            let e = json!({"type": "response.reasoning_summary_text.delta", "output_index": self.reasoning_output_index, "item_id": self.reasoning_item_id, "summary_index": self.reasoning_summary_index, "delta": sd});
            self.send(&mut out, e);
        }

        if let Some(content) = delta
            .get("content")
            .and_then(Value::as_str)
            .filter(|c| !c.is_empty())
        {
            if typed_image_calls.is_empty() {
                self.ensure_message_item(&mut out);
                self.message_text.push_str(content);
                let e = json!({"type": "response.output_text.delta", "output_index": self.message_output_index, "item_id": self.message_item_id, "content_index": 0, "delta": content, "logprobs": []});
                self.send(&mut out, e);
            }
        }

        if let Some(Value::Array(tool_calls)) = delta.get("tool_calls") {
            for tool_call in tool_calls {
                if tool_call.is_null() {
                    // `toolCall.index` throws: the TS stream closes here.
                    self.finished = true;
                    return out;
                }
                let key = coalesce(tool_call.get("index"), None).unwrap_or_else(|| json!(0));
                self.ensure_tool_item(&mut out, &key, tool_call);
                if let Some(args) = get_str(tool_call, &["function", "arguments"]) {
                    let Some(pos) = self.tool_position(&key) else {
                        continue;
                    };
                    let output_index = self.tools[pos].output_index;
                    let item_id = self.tools[pos].item_id.clone();
                    self.tools[pos].args = normalize_tool_args(&self.tools[pos].args, args);
                    // Custom tool input can't be unwrapped from partial JSON;
                    // it is emitted once, complete, at finalization.
                    let flat_name = self.tools[pos].flat_name.clone();
                    let flat_name = if truthy(Some(&flat_name)) {
                        flat_name
                    } else {
                        json!("")
                    };
                    if self.resolve(&flat_name).kind != ResolvedToolType::CustomToolCall {
                        let e = json!({"type": "response.function_call_arguments.delta", "output_index": output_index, "item_id": item_id, "delta": args});
                        self.send(&mut out, e);
                    }
                }
            }
        }

        if truthy(chunk.get("finish_reason")) && !truthy(chunk.get("delta")) {
            self.complete(&mut out);
        }
        out
    }

    fn finish(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        if self.finished {
            return out;
        }
        if !self.has_sent_created {
            let synthetic =
                json!({"model": self.response_model, "created": self.response_created_at});
            self.ensure_created(&mut out, &synthetic);
        }
        self.complete(&mut out);
        out
    }
}
