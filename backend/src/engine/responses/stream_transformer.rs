//! `transformers/responses/stream-transformer.ts`: provider Responses SSE
//! events → unified chunks.
use super::utils::{
    client_tool_call_key, is_client_executed_tool_item, to_unified_image_generation_call,
};
use crate::engine::image_rendering::image_generation_call_markdown;
use crate::engine::js::*;
use crate::engine::sse::SseEvent;
use crate::engine::transformer::StreamTransformer;
use crate::engine::usage::normalize_openai_responses;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

/// State of one `transformResponsesStream` pipe.
#[derive(Debug)]
pub struct ResponsesStreamTransformer {
    response_model: Value,
    response_id: Value,
    /// Responses output indexes → Chat tool-call indexes (keyed by the JS
    /// number's string form, matching `Map<number, number>` semantics).
    tool_call_index_by_output_index: HashMap<String, u64>,
    tool_call_index_by_item_id: HashMap<String, u64>,
    next_tool_call_index: u64,
    has_function_call: bool,
    rendered_image_item_ids: HashSet<String>,
    rendered_client_tool_call_ids: HashSet<String>,
    /// Chat tool-call indexes that already got a start chunk (id/name).
    function_call_indexes_with_started_chunks: HashSet<u64>,
    /// Chat tool-call indexes whose arguments were already emitted (streamed
    /// deltas or a completed event), so later `.done`/`completed` copies are skipped.
    function_call_indexes_with_handled_arguments: HashSet<u64>,
}

impl Default for ResponsesStreamTransformer {
    fn default() -> Self {
        Self {
            response_model: json!(""),
            response_id: json!(""),
            tool_call_index_by_output_index: HashMap::new(),
            tool_call_index_by_item_id: HashMap::new(),
            next_tool_call_index: 0,
            has_function_call: false,
            rendered_image_item_ids: HashSet::new(),
            rendered_client_tool_call_ids: HashSet::new(),
            function_call_indexes_with_started_chunks: HashSet::new(),
            function_call_indexes_with_handled_arguments: HashSet::new(),
        }
    }
}

impl ResponsesStreamTransformer {
    pub fn new() -> Self {
        Self::default()
    }

    fn tool_call_index(&mut self, data: &Value) -> u64 {
        let output_index = data
            .get("output_index")
            .and_then(Value::as_f64)
            .map(js_num_string);
        let item_id = get_str(data, &["item_id"])
            .or_else(|| get_str(data, &["item", "id"]))
            .map(str::to_owned);
        let index = output_index
            .as_ref()
            .and_then(|o| self.tool_call_index_by_output_index.get(o).copied())
            .or_else(|| {
                item_id
                    .as_ref()
                    .and_then(|i| self.tool_call_index_by_item_id.get(i).copied())
            })
            .unwrap_or_else(|| {
                let i = self.next_tool_call_index;
                self.next_tool_call_index += 1;
                i
            });
        if let Some(o) = output_index {
            self.tool_call_index_by_output_index.insert(o, index);
        }
        if let Some(i) = item_id {
            self.tool_call_index_by_item_id.insert(i, index);
        }
        index
    }

    /// `emitCompletedFunctionCallArguments`: emits the full arguments of a
    /// function call that finished without streamed deltas, preceded by the
    /// call's id/name when no start chunk was sent (completion-only calls).
    fn emit_completed_function_call_arguments(
        &mut self,
        data: &Value,
        arguments: Value,
        item: Option<&Value>,
        out: &mut Vec<Value>,
    ) {
        let mut merged = data.clone();
        if let (Some(item), Some(m)) = (item, merged.as_object_mut()) {
            m.insert("item".to_owned(), item.clone());
        }
        let index = self.tool_call_index(&merged);
        if self
            .function_call_indexes_with_handled_arguments
            .contains(&index)
        {
            return;
        }
        let has_started_chunk = self
            .function_call_indexes_with_started_chunks
            .contains(&index);
        let mut function = vec![("arguments", Some(arguments))];
        let mut tc = vec![("index", Some(json!(index)))];
        if !has_started_chunk {
            // A completion-only call needs the item to supply its id/name.
            let Some(item) = item else { return };
            tc.push(("id", item.get("call_id").cloned()));
            tc.push(("type", Some(json!("function"))));
            function.insert(0, ("name", item.get("name").cloned()));
        }
        tc.push(("function", Some(obj(function))));
        out.push(self.chunk(json!({"tool_calls": [obj(tc)]}), None));
        self.function_call_indexes_with_started_chunks.insert(index);
        self.function_call_indexes_with_handled_arguments
            .insert(index);
    }

    /// `{id, model, created: now, delta, ...extra, finish_reason: null}`.
    fn chunk(&self, delta: Value, extra: Option<(&str, Value)>) -> Value {
        let mut c = obj(vec![
            ("id", Some(self.response_id.clone())),
            ("model", Some(self.response_model.clone())),
            ("created", Some(json!(now_secs()))),
            ("delta", Some(delta)),
        ]);
        if let Some((k, v)) = extra {
            c[k] = v;
        }
        c["finish_reason"] = Value::Null;
        c
    }

    /// `responseId || fallback || ''`.
    fn or_fallback(own: &Value, fallback: Option<&Value>) -> Value {
        or(Some(own), fallback)
            .filter(|v| truthy(Some(v)))
            .unwrap_or_else(|| json!(""))
    }

    fn handle(&mut self, data: &Value) -> Vec<Value> {
        let mut out = Vec::new();
        let ty = get_str(data, &["type"]);
        let item = data.get("item");

        if ty == Some("response.created") && truthy(data.get("response")) {
            let r = &data["response"];
            self.response_model = or(r.get("model"), Some(&json!(""))).unwrap_or_default();
            self.response_id = or(r.get("id"), Some(&json!(""))).unwrap_or_default();
            let created = if truthy(r.get("created_at")) {
                r["created_at"].clone()
            } else {
                json!(now_secs())
            };
            out.push(json!({
                "id": self.response_id,
                "model": self.response_model,
                "created": created,
                "delta": {"role": "assistant"},
                "finish_reason": null,
            }));
            return out;
        }

        match ty {
            Some("response.output_text.delta") => {
                out.push(self.chunk(obj(vec![("content", data.get("delta").cloned())]), None))
            }
            Some("response.reasoning_summary_text.delta") => out.push(self.chunk(
                json!({"thinking": obj(vec![("content", data.get("delta").cloned())])}),
                None,
            )),
            Some("response.reasoning_text.delta") => out.push(self.chunk(
                obj(vec![("reasoning_content", data.get("delta").cloned())]),
                None,
            )),
            Some("response.function_call_arguments.delta") => {
                self.has_function_call = true;
                let index = self.tool_call_index(data);
                self.function_call_indexes_with_handled_arguments
                    .insert(index);
                let tc = json!({"index": index, "function": obj(vec![("arguments", data.get("delta").cloned())])});
                out.push(self.chunk(json!({"tool_calls": [tc]}), None));
            }
            Some("response.output_item.added")
                if get_str(data, &["item", "type"]) == Some("function_call") =>
            {
                self.has_function_call = true;
                let index = self.tool_call_index(data);
                self.function_call_indexes_with_started_chunks.insert(index);
                let item = &data["item"];
                let tc = obj(vec![
                    ("index", Some(json!(index))),
                    ("id", item.get("call_id").cloned()),
                    ("type", Some(json!("function"))),
                    (
                        "function",
                        Some(obj(vec![
                            ("name", item.get("name").cloned()),
                            ("arguments", Some(json!(""))),
                        ])),
                    ),
                ]);
                out.push(self.chunk(json!({"tool_calls": [tc]}), None));
            }
            Some("response.function_call_arguments.done") => {
                let args = coalesce(data.get("arguments"), Some(&json!(""))).unwrap_or_default();
                self.emit_completed_function_call_arguments(data, args, None, &mut out);
            }
            Some("response.output_item.done")
                if get_str(data, &["item", "type"]) == Some("function_call") =>
            {
                self.has_function_call = true;
                let item = &data["item"];
                let args = coalesce(item.get("arguments"), Some(&json!(""))).unwrap_or_default();
                self.emit_completed_function_call_arguments(data, args, Some(item), &mut out);
            }
            Some("response.output_item.done")
                if get_str(data, &["item", "type"]) == Some("image_generation_call") =>
            {
                let item = &data["item"];
                // Completed items only; partial-image previews are out of scope.
                if let Some(markdown) = image_generation_call_markdown(item) {
                    if let Some(id) = item.get("id").and_then(Value::as_str) {
                        self.rendered_image_item_ids.insert(id.to_owned());
                    }
                    out.push(self.chunk(
                        json!({"content": markdown}),
                        Some((
                            "image_generation_calls",
                            json!([to_unified_image_generation_call(item)]),
                        )),
                    ));
                }
            }
            Some("response.output_item.done") if is_client_executed_tool_item(item) => {
                // Carried typed for responses-facing formatters; deliberately
                // does NOT set has_function_call (chat clients can't render it).
                if let Some(key) = client_tool_call_key(item) {
                    self.rendered_client_tool_call_ids.insert(key);
                }
                out.push(self.chunk(
                    json!({}),
                    Some((
                        "client_tool_calls",
                        json!([item.cloned().unwrap_or(Value::Null)]),
                    )),
                ));
            }
            Some("response.completed") => {
                let response = data.get("response");
                let usage = response
                    .and_then(|r| r.get("usage"))
                    .filter(|u| truthy(Some(u)))
                    .map(normalize_openai_responses);
                let output: &[Value] = match response.and_then(|r| r.get("output")) {
                    None | Some(Value::Null) => &[],
                    Some(Value::Array(items)) => items,
                    // `.some` / iteration throw: the whole event is dropped.
                    Some(_) => return out,
                };
                let completed_has_function_call = output
                    .iter()
                    .any(|i| get_str(i, &["type"]) == Some("function_call"));
                for item in output {
                    if get_str(item, &["type"]) != Some("image_generation_call") {
                        continue;
                    }
                    if item
                        .get("id")
                        .and_then(Value::as_str)
                        .is_some_and(|id| self.rendered_image_item_ids.contains(id))
                    {
                        continue;
                    }
                    let Some(markdown) = image_generation_call_markdown(item) else {
                        continue;
                    };
                    out.push(self.chunk(
                        json!({"content": markdown}),
                        Some((
                            "image_generation_calls",
                            json!([to_unified_image_generation_call(item)]),
                        )),
                    ));
                }
                for (output_index, item) in output.iter().enumerate() {
                    if get_str(item, &["type"]) == Some("function_call") {
                        let args =
                            coalesce(item.get("arguments"), Some(&json!(""))).unwrap_or_default();
                        let fallback = obj(vec![
                            ("output_index", Some(json!(output_index))),
                            ("item_id", item.get("id").cloned()),
                        ]);
                        self.emit_completed_function_call_arguments(
                            &fallback,
                            args,
                            Some(item),
                            &mut out,
                        );
                        continue;
                    }
                    if !is_client_executed_tool_item(Some(item)) {
                        continue;
                    }
                    if client_tool_call_key(Some(item))
                        .is_some_and(|k| self.rendered_client_tool_call_ids.contains(&k))
                    {
                        continue;
                    }
                    out.push(self.chunk(json!({}), Some(("client_tool_calls", json!([item])))));
                }
                let finish = if self.has_function_call || completed_has_function_call {
                    "tool_calls"
                } else {
                    "stop"
                };
                out.push(obj(vec![
                    ("id", Some(self.response_id.clone())),
                    ("model", Some(self.response_model.clone())),
                    ("created", Some(json!(now_secs()))),
                    ("delta", Some(json!({}))),
                    ("finish_reason", Some(json!(finish))),
                    ("usage", usage),
                ]));
            }
            Some("response.failed") => {
                let response = data.get("response");
                let err =
                    or(response.and_then(|r| r.get("error")), Some(&json!({}))).unwrap_or_default();
                let usage = response
                    .and_then(|r| r.get("usage"))
                    .filter(|u| truthy(Some(u)))
                    .map(normalize_openai_responses);
                out.push(obj(vec![
                    ("id", Some(Self::or_fallback(&self.response_id, response.and_then(|r| r.get("id"))))),
                    ("model", Some(Self::or_fallback(&self.response_model, response.and_then(|r| r.get("model"))))),
                    ("created", Some(json!(now_secs()))),
                    ("event", Some(json!("error"))),
                    ("delta", Some(json!({}))),
                    ("error", Some(json!({
                        "statusCode": 500,
                        "code": or(err.get("code"), Some(&json!("response_failed"))),
                        "message": or(err.get("message"), Some(&json!("The model response failed to complete."))),
                    }))),
                    ("usage", usage),
                ]));
            }
            Some("response.incomplete") => {
                let response = data.get("response");
                let details = coalesce(
                    response.and_then(|r| r.get("incomplete_details")),
                    Some(&json!({"reason": "unknown"})),
                )
                .unwrap_or_default();
                let reason = or(details.get("reason"), Some(&json!("unknown"))).unwrap_or_default();
                let usage = response
                    .and_then(|r| r.get("usage"))
                    .filter(|u| truthy(Some(u)))
                    .map(normalize_openai_responses);
                let finish = if reason.as_str() == Some("content_filter") {
                    "content_filter"
                } else {
                    "length"
                };
                out.push(obj(vec![
                    ("id", Some(Self::or_fallback(&self.response_id, response.and_then(|r| r.get("id"))))),
                    ("model", Some(Self::or_fallback(&self.response_model, response.and_then(|r| r.get("model"))))),
                    ("created", Some(json!(now_secs()))),
                    ("event", Some(json!("error"))),
                    ("delta", Some(json!({}))),
                    ("finish_reason", Some(json!(finish))),
                    ("incomplete_details", Some(details)),
                    ("error", Some(json!({
                        "statusCode": 500,
                        "code": reason,
                        "message": format!("Response ended incomplete: {}", to_string(Some(&reason))),
                    }))),
                    ("usage", usage),
                ]));
            }
            Some("error") => {
                out.push(json!({
                    "id": self.response_id,
                    "model": self.response_model,
                    "created": now_secs(),
                    "event": "error",
                    "delta": {},
                    "error": {
                        "statusCode": 500,
                        "code": or(data.get("code"), Some(&json!("error"))),
                        "message": or(data.get("message"), Some(&json!("Upstream error"))),
                    },
                }));
            }
            _ => {}
        }
        out
    }
}

impl StreamTransformer for ResponsesStreamTransformer {
    fn on_event(&mut self, event: &SseEvent) -> Vec<Value> {
        if event.data == "[DONE]" {
            return vec![];
        }
        // Unparseable chunks are logged and skipped in the TS source.
        let Some(data) = parse(&event.data) else {
            return vec![];
        };
        self.handle(&data)
    }
}
