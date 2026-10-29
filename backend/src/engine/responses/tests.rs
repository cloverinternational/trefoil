//! Ports of the most important `transformers/__tests__/responses-*.test.ts` cases.
use super::*;
use crate::engine::openai::OpenAITransformer;
use crate::engine::sse::SseParser;
use serde_json::json;

const TINY_IMAGE_B64: &str = "aGVsbG8=";
fn tiny_markdown() -> String {
    format!("![generated image](data:image/png;base64,{TINY_IMAGE_B64})")
}

fn transform_events(events: &[Value]) -> Vec<Value> {
    let mut parser = SseParser::new();
    let mut st = ResponsesStreamTransformer::new();
    let mut chunks = Vec::new();
    for e in events {
        let text = format!(
            "event: {}\ndata: {}\n\n",
            e["type"].as_str().unwrap_or(""),
            stringify(e)
        );
        for ev in parser.feed(&text) {
            chunks.extend(st.on_event(&ev));
        }
    }
    chunks.extend(st.finish());
    chunks
}

fn format_chunks(t: &ResponsesTransformer, chunks: &[Value]) -> (String, Vec<Value>) {
    let mut f = t.stream_formatter().unwrap();
    let mut out = String::new();
    for c in chunks {
        out.extend(f.on_chunk(c));
    }
    out.extend(f.finish());
    let events = out
        .split("\n\n")
        .filter(|b| !b.trim().is_empty())
        .map(|b| {
            parse(
                b.lines()
                    .find(|l| l.starts_with("data: "))
                    .unwrap()
                    .trim_start_matches("data: "),
            )
            .unwrap()
        })
        .collect();
    (out, events)
}

fn find(events: &[Value], f: impl Fn(&Value) -> bool) -> &Value {
    events.iter().find(|e| f(e)).expect("event")
}

fn tool_call_chunks(name: &str, args: &str) -> Vec<Value> {
    vec![
        json!({"id": "resp_1", "model": "m", "created": 1, "delta": {"tool_calls": [{"index": 0, "id": "call_1", "function": {"name": name, "arguments": ""}}]}, "finish_reason": null}),
        json!({"id": "resp_1", "model": "m", "created": 1, "delta": {"tool_calls": [{"index": 0, "function": {"arguments": args}}]}, "finish_reason": null}),
        json!({"id": "resp_1", "model": "m", "created": 1, "delta": null, "finish_reason": "tool_calls"}),
    ]
}

const USER_MESSAGE: &str =
    r#"{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}"#;
fn user_message() -> Value {
    parse(USER_MESSAGE).unwrap()
}
fn codex_lite_additional_tools() -> Value {
    json!({"type": "additional_tools", "role": "developer", "tools": [{"type": "namespace", "name": "functions", "tools": [
        {"type": "custom", "name": "exec", "description": "Run code"},
        {"type": "function", "name": "wait", "parameters": {"type": "object", "properties": {}}},
    ]}]})
}

// ---- stream transformation -------------------------------------------------

#[test]
fn parallel_function_calls_stay_distinct_when_interleaved() {
    let events = vec![
        json!({"type": "response.created", "response": {"id": "resp_1", "model": "gpt-5", "created_at": 1234567890}}),
        json!({"type": "response.output_item.added", "output_index": 4, "item": {"id": "fc_first", "type": "function_call", "call_id": "call_first", "name": "add_task"}}),
        json!({"type": "response.output_item.added", "output_index": 9, "item": {"id": "fc_second", "type": "function_call", "call_id": "call_second", "name": "add_task"}}),
        json!({"type": "response.function_call_arguments.delta", "output_index": 9, "item_id": "fc_second", "delta": "{\"title\":\"second\"}"}),
        json!({"type": "response.function_call_arguments.delta", "output_index": 4, "item_id": "fc_first", "delta": "{\"title\":\"first\"}"}),
    ];
    let chunks = transform_events(&events);
    let tcs: Vec<&Value> = chunks
        .iter()
        .filter(|c| c["delta"].get("tool_calls").is_some())
        .collect();
    assert_eq!(tcs.len(), 4);
    assert_eq!(
        tcs[0]["delta"]["tool_calls"],
        json!([{"index": 0, "id": "call_first", "type": "function", "function": {"name": "add_task", "arguments": ""}}])
    );
    assert_eq!(tcs[1]["delta"]["tool_calls"][0]["index"], json!(1));
    assert_eq!(
        tcs[2]["delta"]["tool_calls"],
        json!([{"index": 1, "function": {"arguments": "{\"title\":\"second\"}"}}])
    );
    assert_eq!(
        tcs[3]["delta"]["tool_calls"],
        json!([{"index": 0, "function": {"arguments": "{\"title\":\"first\"}"}}])
    );
    assert_eq!(
        (
            tcs[0]["id"].clone(),
            tcs[0]["model"].clone(),
            tcs[0]["finish_reason"].clone()
        ),
        (json!("resp_1"), json!("gpt-5"), Value::Null)
    );
    assert_eq!(
        stringify(&chunks[0]),
        r#"{"id":"resp_1","model":"gpt-5","created":1234567890,"delta":{"role":"assistant"},"finish_reason":null}"#
    );
}

#[test]
fn finish_reason_tracks_function_calls() {
    let created = json!({"type": "response.created", "response": {"id": "resp_1", "model": "gpt-4o", "created_at": 1234567890}});
    let chunks = transform_events(&[
        created.clone(),
        json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "function_call", "call_id": "call_1", "name": "get_date", "arguments": ""}}),
        json!({"type": "response.function_call_arguments.delta", "output_index": 0, "item_id": "fc_1", "delta": "{\"timezone\":\"UTC\"}"}),
        json!({"type": "response.completed", "response": {"usage": {"input_tokens": 10, "output_tokens": 5, "total_tokens": 15}}}),
    ]);
    let last = chunks.last().unwrap();
    assert_eq!(last["finish_reason"], json!("tool_calls"));
    assert_eq!(last["usage"]["total_tokens"], json!(15));
    let chunks = transform_events(&[
        created.clone(),
        json!({"type": "response.output_text.delta", "delta": "Done"}),
        json!({"type": "response.completed", "response": {}}),
    ]);
    assert_eq!(chunks.last().unwrap()["finish_reason"], json!("stop"));
    assert!(chunks.last().unwrap().get("usage").is_none());
    let chunks = transform_events(&[
        created,
        json!({"type": "response.completed", "response": {"output": [{"type": "function_call"}]}}),
    ]);
    assert_eq!(chunks.last().unwrap()["finish_reason"], json!("tool_calls"));
}

/// `(index, arguments)` of every tool-call delta, in order.
fn tool_call_arguments(chunks: &[Value]) -> Vec<(u64, String)> {
    chunks
        .iter()
        .filter_map(|c| c["delta"]["tool_calls"].as_array())
        .flatten()
        .map(|tc| {
            (
                tc["index"].as_u64().unwrap(),
                tc["function"]["arguments"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

#[test]
fn forwards_completed_arguments_when_no_argument_deltas_were_streamed() {
    let item = json!({"id": "fc_example", "type": "function_call", "status": "completed", "call_id": "call_example", "name": "example_lookup", "arguments": "{\"limit\":100,\"offset\":50}"});
    let chunks = transform_events(&[
        json!({"type": "response.output_item.added", "sequence_number": 1, "output_index": 0, "item": {"id": "fc_example", "type": "function_call", "status": "in_progress", "call_id": "call_example", "name": "example_lookup", "arguments": ""}}),
        json!({"type": "response.function_call_arguments.done", "sequence_number": 2, "item_id": "fc_example", "output_index": 0, "arguments": "{\"limit\":100,\"offset\":50}"}),
        json!({"type": "response.output_item.done", "sequence_number": 3, "output_index": 0, "item": item}),
        json!({"type": "response.completed", "sequence_number": 4, "response": {"id": "resp_example", "model": "example-model", "status": "completed", "output": [item], "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}}}),
    ]);
    assert_eq!(
        tool_call_arguments(&chunks),
        vec![
            (0, String::new()),
            (0, "{\"limit\":100,\"offset\":50}".to_owned())
        ]
    );
    assert_eq!(chunks.last().unwrap()["finish_reason"], json!("tool_calls"));
}

#[test]
fn does_not_repeat_completed_arguments_after_forwarding_argument_deltas() {
    let item = json!({"id": "fc_1", "type": "function_call", "call_id": "call_1", "name": "example_lookup", "arguments": "{\"limit\":100,\"offset\":50}"});
    let chunks = transform_events(&[
        json!({"type": "response.created", "response": {"id": "resp_1", "model": "gpt-4o", "created_at": 1234567890}}),
        json!({"type": "response.output_item.added", "output_index": 0, "item": {"id": "fc_1", "type": "function_call", "call_id": "call_1", "name": "example_lookup", "arguments": ""}}),
        json!({"type": "response.function_call_arguments.delta", "output_index": 0, "item_id": "fc_1", "delta": "{\"limit\":100,\"offset\":50}"}),
        json!({"type": "response.output_item.done", "output_index": 0, "item": item}),
        json!({"type": "response.completed", "response": {"output": [item]}}),
    ]);
    assert_eq!(
        tool_call_arguments(&chunks),
        vec![
            (0, String::new()),
            (0, "{\"limit\":100,\"offset\":50}".to_owned())
        ]
    );
}

#[test]
fn completion_only_fallback_arguments_stay_separate_from_parallel_call_deltas() {
    let chunks = transform_events(&[
        json!({"type": "response.created", "response": {"id": "resp_1", "model": "gpt-4o", "created_at": 1234567890}}),
        json!({"type": "response.output_item.added", "output_index": 0, "item": {"id": "fc_1", "type": "function_call", "call_id": "call_1", "name": "first_tool", "arguments": ""}}),
        json!({"type": "response.output_item.added", "output_index": 1, "item": {"id": "fc_2", "type": "function_call", "call_id": "call_2", "name": "second_tool", "arguments": ""}}),
        json!({"type": "response.function_call_arguments.delta", "output_index": 1, "item_id": "fc_2", "delta": "{\"which\":\"second\"}"}),
        json!({"type": "response.output_item.done", "output_index": 0, "item": {"id": "fc_1", "type": "function_call", "call_id": "call_1", "name": "first_tool", "arguments": "{\"which\":\"first\"}"}}),
        json!({"type": "response.output_item.done", "output_index": 1, "item": {"id": "fc_2", "type": "function_call", "call_id": "call_2", "name": "second_tool", "arguments": "{\"which\":\"second\"}"}}),
    ]);
    assert_eq!(
        tool_call_arguments(&chunks),
        vec![
            (0, String::new()),
            (1, String::new()),
            (1, "{\"which\":\"second\"}".to_owned()),
            (0, "{\"which\":\"first\"}".to_owned()),
        ]
    );
}

#[test]
fn completion_only_call_without_start_chunk_carries_id_and_name() {
    let chunks = transform_events(&[
        json!({"type": "response.created", "response": {"id": "resp_1", "model": "gpt-4o", "created_at": 1234567890}}),
        json!({"type": "response.completed", "response": {"output": [{"id": "fc_1", "type": "function_call", "call_id": "call_1", "name": "only_tool", "arguments": "{}"}]}}),
    ]);
    let tcs: Vec<&Value> = chunks
        .iter()
        .filter(|c| c["delta"].get("tool_calls").is_some())
        .collect();
    assert_eq!(tcs.len(), 1);
    assert_eq!(
        tcs[0]["delta"]["tool_calls"],
        json!([{"index": 0, "id": "call_1", "type": "function", "function": {"name": "only_tool", "arguments": "{}"}}])
    );
    assert_eq!(chunks.last().unwrap()["finish_reason"], json!("tool_calls"));
}

fn reasoning_summary_events() -> Vec<Value> {
    vec![
        json!({"type": "response.created", "response": {"id": "resp_reasoning_1", "model": "gpt-5", "created_at": 1234567890}}),
        json!({"type": "response.output_item.added", "output_index": 0, "item": {"id": "rs_1", "type": "reasoning", "content": [], "summary": []}}),
        json!({"type": "response.reasoning_summary_part.added", "output_index": 0, "item_id": "rs_1", "summary_index": 0, "part": {"type": "summary_text", "text": ""}}),
        json!({"type": "response.reasoning_summary_text.delta", "output_index": 0, "item_id": "rs_1", "summary_index": 0, "delta": "Checking the "}),
        json!({"type": "response.reasoning_summary_text.delta", "output_index": 0, "item_id": "rs_1", "summary_index": 0, "delta": "file listing"}),
        json!({"type": "response.reasoning_summary_text.done", "output_index": 0, "item_id": "rs_1", "summary_index": 0, "text": "Checking the file listing"}),
        json!({"type": "response.output_item.done", "output_index": 0, "item": {"id": "rs_1", "type": "reasoning", "content": [], "summary": []}}),
        json!({"type": "response.output_item.added", "output_index": 1, "item": {"id": "fc_1", "type": "function_call", "call_id": "call_1", "name": "ls", "arguments": ""}}),
        json!({"type": "response.function_call_arguments.delta", "output_index": 1, "item_id": "fc_1", "delta": "{\"path\":\".\"}"}),
        json!({"type": "response.output_item.done", "output_index": 1, "item": {"id": "fc_1", "type": "function_call", "call_id": "call_1", "name": "ls", "arguments": "{\"path\":\".\"}"}}),
        json!({"type": "response.completed", "response": {"usage": {"input_tokens": 10, "output_tokens": 20, "total_tokens": 30, "output_tokens_details": {"reasoning_tokens": 12}}}}),
    ]
}

#[test]
fn reasoning_summary_round_trips_through_format_stream() {
    let chunks = transform_events(&reasoning_summary_events());
    let thinking: Vec<&Value> = chunks
        .iter()
        .filter_map(|c| get(c, &["delta", "thinking", "content"]))
        .collect();
    assert_eq!(
        thinking,
        vec![&json!("Checking the "), &json!("file listing")]
    );

    let (_, events) = format_chunks(&ResponsesTransformer::new(), &chunks);
    let deltas: Vec<&Value> = events
        .iter()
        .filter(|e| e["type"] == "response.reasoning_summary_text.delta")
        .map(|e| &e["delta"])
        .collect();
    assert_eq!(
        deltas,
        vec![&json!("Checking the "), &json!("file listing")]
    );
    let completed = find(&events, |e| e["type"] == "response.completed");
    let types: Vec<&Value> = completed["response"]["output"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| &i["type"])
        .collect();
    assert_eq!(types, vec![&json!("reasoning"), &json!("function_call")]);
    assert_eq!(
        completed["response"]["output"][0]["summary"],
        json!([{"type": "summary_text", "text": "Checking the file listing"}])
    );
    assert_eq!(
        completed["response"]["usage"]["output_tokens_details"]["reasoning_tokens"],
        json!(12)
    );
    // sequence numbers are contiguous from 0
    for (i, e) in events.iter().enumerate() {
        assert_eq!(e["sequence_number"], json!(i));
    }
    assert_eq!(events[0]["type"], "response.created");
    assert_eq!(events[1]["type"], "response.in_progress");
}

#[test]
fn error_events_become_unified_error_chunks() {
    let created = json!({"type": "response.created", "response": {"id": "resp_1", "model": "gpt-5", "created_at": 1234567890}});
    let chunks = transform_events(&[
        created.clone(),
        json!({"type": "response.failed", "response": {"id": "resp_1", "status": "failed", "error": {"code": "server_error", "message": "The model encountered an error."}}}),
    ]);
    let err = find(&chunks, |c| c["event"] == "error");
    assert_eq!(err["error"]["message"], "The model encountered an error.");
    assert!(err.get("usage").is_none());
    assert!(!chunks.iter().any(|c| c["finish_reason"] == "stop"));

    let chunks = transform_events(&[
        created.clone(),
        json!({"type": "response.incomplete", "response": {"incomplete_details": {"reason": "max_output_tokens"}, "usage": {"input_tokens": 20, "output_tokens": 8, "total_tokens": 28}}}),
    ]);
    let err = find(&chunks, |c| c["event"] == "error");
    assert_eq!(
        (err["finish_reason"].clone(), err["error"]["code"].clone()),
        (json!("length"), json!("max_output_tokens"))
    );
    assert_eq!(
        err["incomplete_details"],
        json!({"reason": "max_output_tokens"})
    );
    assert_eq!(err["usage"]["total_tokens"], json!(28));

    let chunks = transform_events(&[
        created.clone(),
        json!({"type": "response.incomplete", "response": {"incomplete_details": {"reason": "content_filter"}}}),
    ]);
    assert_eq!(
        find(&chunks, |c| c["event"] == "error")["finish_reason"],
        json!("content_filter")
    );

    let chunks = transform_events(&[
        created.clone(),
        json!({"type": "response.incomplete", "response": {}}),
    ]);
    let err = find(&chunks, |c| c["event"] == "error");
    assert_eq!(err["incomplete_details"], json!({"reason": "unknown"}));
    assert_eq!(
        err["error"],
        json!({"statusCode": 500, "code": "unknown", "message": "Response ended incomplete: unknown"})
    );

    let chunks = transform_events(&[created, json!({"type": "error", "message": "boom"})]);
    assert_eq!(
        find(&chunks, |c| c["event"] == "error")["error"],
        json!({"statusCode": 500, "code": "error", "message": "boom"})
    );
}

#[test]
fn image_generation_call_stream_rendering_and_dedupe() {
    let created = json!({"type": "response.created", "response": {"id": "resp_1", "model": "gpt-5", "created_at": 1}});
    let item = json!({"id": "ig_1", "type": "image_generation_call", "status": "completed", "result": TINY_IMAGE_B64});
    let chunks = transform_events(&[
        created.clone(),
        json!({"type": "response.image_generation_call.partial_image", "partial_image_b64": "xx"}),
        json!({"type": "response.output_item.done", "output_index": 0, "item": item}),
        json!({"type": "response.completed", "response": {"output": [item]}}),
    ]);
    let images: Vec<&Value> = chunks
        .iter()
        .filter(|c| c.get("image_generation_calls").is_some())
        .collect();
    assert_eq!(images.len(), 1);
    assert_eq!(images[0]["delta"]["content"], json!(tiny_markdown()));
    assert_eq!(
        images[0]["image_generation_calls"],
        json!([{"id": "ig_1", "status": "completed", "result": TINY_IMAGE_B64}])
    );
    let keys: Vec<&String> = images[0].as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        [
            "id",
            "model",
            "created",
            "delta",
            "image_generation_calls",
            "finish_reason"
        ]
    );

    // completed-only item still renders, before the terminal chunk
    let chunks = transform_events(&[
        created,
        json!({"type": "response.completed", "response": {"output": [item]}}),
    ]);
    assert_eq!(chunks[1]["delta"]["content"], json!(tiny_markdown()));
    assert_eq!(chunks[2]["finish_reason"], json!("stop"));

    // formatStream re-emits the typed item natively, skipping the markdown
    let (_, events) = format_chunks(&ResponsesTransformer::new(), &chunks);
    assert!(!events
        .iter()
        .any(|e| e["type"] == "response.output_text.delta"));
    let added = find(&events, |e| e["type"] == "response.output_item.added");
    assert_eq!(
        added["item"],
        json!({"id": "ig_1", "type": "image_generation_call", "status": "in_progress", "result": null})
    );
    let completed = find(&events, |e| e["type"] == "response.completed");
    assert_eq!(
        completed["response"]["output"],
        json!([{"id": "ig_1", "type": "image_generation_call", "status": "completed", "result": TINY_IMAGE_B64}])
    );
}

#[test]
fn client_executed_tool_calls_carry_without_tool_calls_finish() {
    let created = json!({"type": "response.created", "response": {"id": "resp_1", "model": "gpt-5", "created_at": 1}});
    let item = json!({"type": "tool_search_call", "id": "ts_1", "call_id": "call_ts", "execution": "client", "status": "completed", "arguments": {"query": "x"}});
    let chunks = transform_events(&[
        created,
        json!({"type": "response.output_item.done", "output_index": 0, "item": item}),
        json!({"type": "response.completed", "response": {"output": [item]}}),
    ]);
    let carried: Vec<&Value> = chunks
        .iter()
        .filter(|c| c.get("client_tool_calls").is_some())
        .collect();
    assert_eq!(carried.len(), 1);
    assert_eq!(carried[0]["client_tool_calls"], json!([item]));
    assert_eq!(chunks.last().unwrap()["finish_reason"], json!("stop"));

    let (_, events) = format_chunks(&ResponsesTransformer::new(), &chunks);
    let added = find(&events, |e| e["type"] == "response.output_item.added");
    assert_eq!(added["item"]["status"], "in_progress");
    assert_eq!(
        added["item"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        ["type", "id", "call_id", "execution", "status", "arguments"]
    );
    let completed = find(&events, |e| e["type"] == "response.completed");
    assert_eq!(completed["response"]["output"], json!([item]));

    // unary round trip
    let unified = ResponsesTransformer::new().transform_response(&json!({"id": "resp_1", "object": "response", "model": "gpt-5", "created_at": 1, "output": [item]})).unwrap();
    assert_eq!(unified["client_tool_calls"], json!([item]));
    let formatted = ResponsesTransformer::new()
        .format_response(&unified)
        .unwrap();
    assert_eq!(formatted["output"][0], item);
}

// ---- formatStream ------------------------------------------------------------

#[test]
fn format_stream_full_event_shape_for_text() {
    let chunks = vec![
        json!({"id": "chatcmpl_1", "model": "gpt-4o", "created": 1234567890, "delta": {"role": "assistant", "content": "hel"}, "finish_reason": null}),
        json!({"id": "chatcmpl_1", "model": "gpt-4o", "created": 1234567890, "delta": {"content": "lo"}, "finish_reason": null}),
        json!({"id": "chatcmpl_1", "model": "gpt-4o", "created": 1234567890, "delta": {}, "finish_reason": "stop", "usage": {"input_tokens": 2571, "output_tokens": 416, "total_tokens": 17963, "reasoning_tokens": 0, "cached_tokens": 14976, "cache_creation_tokens": 0}}),
    ];
    let (raw, events) = format_chunks(&ResponsesTransformer::new(), &chunks);
    let types: Vec<&str> = events.iter().map(|e| e["type"].as_str().unwrap()).collect();
    assert_eq!(
        types,
        [
            "response.created",
            "response.in_progress",
            "response.output_item.added",
            "response.content_part.added",
            "response.output_text.delta",
            "response.output_text.delta",
            "response.output_text.done",
            "response.content_part.done",
            "response.output_item.done",
            "response.completed",
        ]
    );
    assert!(raw.starts_with("event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"chatcmpl_1\",\"object\":\"response\",\"created_at\":1234567890,\"status\":\"in_progress\",\"model\":\"gpt-4o\",\"output\":[]},\"sequence_number\":0}\n\n"));
    let msg_id = events[2]["item"]["id"].as_str().unwrap().to_owned();
    assert!(msg_id.starts_with("msg_"));
    assert_eq!(
        stringify(&events[4]),
        format!(
            r#"{{"type":"response.output_text.delta","output_index":0,"item_id":"{msg_id}","content_index":0,"delta":"hel","logprobs":[],"sequence_number":4}}"#
        )
    );
    let completed = &events[9]["response"];
    assert_eq!(completed["output"][0]["content"][0]["text"], "hello");
    assert_eq!(completed["usage"]["input_tokens"], json!(17547));
    assert_eq!(
        completed["usage"]["input_tokens_details"]["cached_tokens"],
        json!(14976)
    );
}

#[test]
fn format_stream_terminal_events_for_errors() {
    let base = json!({"id": "resp_1", "model": "m", "created": 1, "delta": {"content": "partial"}, "finish_reason": null});
    let failed = json!({"id": "resp_1", "model": "m", "created": 1, "event": "error", "delta": {}, "error": {"statusCode": 500, "code": "server_error", "message": "boom"}});
    let (_, events) = format_chunks(&ResponsesTransformer::new(), &[base.clone(), failed]);
    let last = events.last().unwrap();
    assert_eq!(last["type"], "response.failed");
    assert_eq!(
        last["response"]["error"],
        json!({"code": "server_error", "message": "boom"})
    );
    assert_eq!(last["response"]["output"][0]["status"], "completed");
    assert!(!events.iter().any(|e| e["type"] == "response.completed"));

    let incomplete = json!({"id": "resp_1", "model": "m", "created": 1, "event": "error", "delta": {}, "finish_reason": "length", "incomplete_details": {"reason": "max_output_tokens"}, "error": {"statusCode": 500, "code": "max_output_tokens", "message": "x"}, "usage": {"input_tokens": 1, "output_tokens": 2, "total_tokens": 3}});
    let (_, events) = format_chunks(&ResponsesTransformer::new(), &[base, incomplete]);
    let last = events.last().unwrap();
    assert_eq!(last["type"], "response.incomplete");
    assert_eq!(last["response"]["status"], "incomplete");
    assert_eq!(
        last["response"]["incomplete_details"],
        json!({"reason": "max_output_tokens"})
    );
    assert_eq!(last["response"]["output"][0]["status"], "incomplete");
    assert_eq!(last["response"]["usage"]["output_tokens"], json!(2));
    let keys: Vec<&String> = last["response"].as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        [
            "id",
            "object",
            "created_at",
            "status",
            "model",
            "output",
            "incomplete_details",
            "usage"
        ]
    );
}

#[test]
fn format_stream_tool_argument_snapshots() {
    let t = ResponsesTransformer::new();
    let mk = |args: &str| json!({"id": "resp_1", "model": "gpt-5.6", "created": 1, "delta": {"tool_calls": [{"index": 0, "function": {"arguments": args}}]}, "finish_reason": null});
    let start = json!({"id": "resp_1", "model": "gpt-5.6", "created": 1, "delta": {"tool_calls": [{"index": 0, "id": "call_1", "function": {"name": "run", "arguments": ""}}]}, "finish_reason": null});
    let end = json!({"id": "resp_1", "model": "gpt-5.6", "created": 1, "delta": null, "finish_reason": "tool_calls"});
    let (_, events) = format_chunks(
        &t,
        &[
            start.clone(),
            mk("{\"command\":\"before "),
            mk("{}"),
            mk(" after\"}"),
            end.clone(),
        ],
    );
    let done = find(&events, |e| {
        e["type"] == "response.output_item.done" && e["item"]["type"] == "function_call"
    });
    assert_eq!(
        parse(done["item"]["arguments"].as_str().unwrap()).unwrap(),
        json!({"command": "before {} after"})
    );
    let (_, events) = format_chunks(
        &t,
        &[
            start,
            mk(" {\"command\":\"fu"),
            mk("{\"command\":\"full\"}"),
            end,
        ],
    );
    let done = find(&events, |e| {
        e["type"] == "response.output_item.done" && e["item"]["type"] == "function_call"
    });
    assert_eq!(
        parse(done["item"]["arguments"].as_str().unwrap()).unwrap(),
        json!({"command": "full"})
    );
}

// ---- Codex tools: parse / format -------------------------------------------

#[test]
fn namespace_tools_flatten_and_split_back() {
    let t = ResponsesTransformer::new();
    let unified = t.parse_request(&json!({"model": "gpt-5.6", "input": [user_message()], "tools": [{"type": "namespace", "name": "crm", "tools": [{"type": "function", "name": "list_open_orders", "description": "List open orders", "parameters": {"type": "object", "properties": {}}}]}]})).unwrap();
    assert_eq!(
        unified["tools"],
        json!([{"type": "function", "function": {"name": "crm__list_open_orders", "description": "List open orders", "parameters": {"type": "object", "properties": {}}}}])
    );

    let formatted = t.format_response(&json!({"id": "resp_1", "model": "gpt-5.6", "created": 1, "content": "", "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "crm__list_open_orders", "arguments": "{}"}}]})).unwrap();
    let item = &formatted["output"][0];
    assert_eq!(
        (
            item["type"].clone(),
            item["name"].clone(),
            item["namespace"].clone(),
            item["arguments"].clone()
        ),
        (
            json!("function_call"),
            json!("list_open_orders"),
            json!("crm"),
            json!("{}")
        )
    );
    assert_eq!(
        item.as_object().unwrap().keys().collect::<Vec<_>>(),
        [
            "type",
            "id",
            "status",
            "call_id",
            "name",
            "namespace",
            "arguments"
        ]
    );

    let (_, events) = format_chunks(&t, &tool_call_chunks("crm__list_open_orders", "{}"));
    let added = find(&events, |e| {
        e["type"] == "response.output_item.added" && e["item"]["type"] == "function_call"
    });
    assert_eq!(
        (
            added["item"]["name"].clone(),
            added["item"]["namespace"].clone()
        ),
        (json!("list_open_orders"), json!("crm"))
    );

    let unified = ResponsesTransformer::new().parse_request(&json!({"model": "gpt-5.6", "input": [{"type": "function_call", "call_id": "call_1", "name": "list_open_orders", "namespace": "crm", "arguments": "{}"}], "tools": [{"type": "namespace", "name": "crm", "tools": [{"type": "function", "name": "list_open_orders", "parameters": {}}]}]})).unwrap();
    assert_eq!(
        unified["messages"][0]["tool_calls"][0]["function"]["name"],
        "crm__list_open_orders"
    );
}

#[test]
fn custom_tools_round_trip() {
    let patch = "*** Begin Patch\n*** Update File: foo.ts\n*** End Patch";
    let t = ResponsesTransformer::new();
    let unified = t.parse_request(&json!({"model": "gpt-5.6", "input": [{"type": "custom_tool_call", "call_id": "call_1", "name": "apply_patch", "input": patch}, {"type": "custom_tool_call_output", "call_id": "call_1", "output": "patch applied"}], "tools": [{"type": "custom", "name": "apply_patch", "description": "Apply a patch"}]})).unwrap();
    assert_eq!(
        unified["tools"],
        json!([{"type": "function", "function": {"name": "apply_patch", "description": "Apply a patch", "parameters": {"type": "object", "properties": {"input": {"type": "string"}}, "required": ["input"]}}}])
    );
    assert_eq!(
        parse(
            unified["messages"][0]["tool_calls"][0]["function"]["arguments"]
                .as_str()
                .unwrap()
        )
        .unwrap(),
        json!({"input": patch})
    );
    assert_eq!(
        unified["messages"][1],
        json!({"role": "tool", "tool_call_id": "call_1", "content": "patch applied"})
    );

    let formatted = t.format_response(&json!({"id": "r", "model": "m", "created": 1, "content": "", "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "apply_patch", "arguments": stringify(&json!({"input": patch}))}}]})).unwrap();
    assert_eq!(
        (
            formatted["output"][0]["type"].clone(),
            formatted["output"][0]["input"].clone()
        ),
        (json!("custom_tool_call"), json!(patch))
    );

    let (_, events) = format_chunks(
        &t,
        &tool_call_chunks("apply_patch", &stringify(&json!({"input": patch}))),
    );
    assert!(!events
        .iter()
        .any(|e| e["type"] == "response.function_call_arguments.delta"));
    let done = find(&events, |e| {
        e["type"] == "response.output_item.done" && e["item"]["type"] == "custom_tool_call"
    });
    assert_eq!(done["item"]["input"], json!(patch));

    // provider-side transformResponse → client-side formatResponse
    let unified = ResponsesTransformer::new().transform_response(&json!({"id": "resp_1", "object": "response", "model": "gpt-5.6", "created_at": 1, "output": [{"type": "custom_tool_call", "call_id": "call_1", "name": "apply_patch", "input": patch}]})).unwrap();
    let formatted = t.format_response(&unified).unwrap();
    assert_eq!(formatted["output"][0]["input"], json!(patch));
}

#[test]
fn codex_lite_nested_custom_tools() {
    let t = ResponsesTransformer::new();
    let unified = t.parse_request(&json!({"model": "m", "input": [
        codex_lite_additional_tools(), user_message(),
        {"type": "custom_tool_call", "call_id": "call_1", "name": "exec", "input": "text(\"hi\")"},
        {"type": "custom_tool_call_output", "call_id": "call_1", "output": [{"type": "input_text", "text": "a"}]},
    ]})).unwrap();
    let exec = unified["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["function"]["name"] == "functions__exec")
        .unwrap();
    assert_eq!(
        exec["function"]["parameters"],
        json!({"type": "object", "properties": {"input": {"type": "string"}}, "required": ["input"]})
    );
    let messages = unified["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 3);
    assert_eq!(
        messages[1]["tool_calls"][0]["function"],
        json!({"name": "functions__exec", "arguments": "{\"input\":\"text(\\\"hi\\\")\"}"})
    );
    assert_eq!(
        messages[2]["content"],
        json!("[{\"type\":\"input_text\",\"text\":\"a\"}]")
    );

    for name in ["functions__exec", "exec"] {
        let formatted = t.format_response(&json!({"id": "r", "model": "m", "created": 1, "content": "", "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": name, "arguments": "{\"input\":\"ls\"}"}}]})).unwrap();
        let item = &formatted["output"][0];
        assert_eq!(
            (
                item["type"].clone(),
                item["name"].clone(),
                item["input"].clone()
            ),
            (json!("custom_tool_call"), json!("exec"), json!("ls"))
        );
        assert!(item.get("namespace").is_none());
    }
    let (_, events) = format_chunks(&t, &tool_call_chunks("functions__wait", "{}"));
    let done = find(&events, |e| {
        e["type"] == "response.output_item.done" && e["item"].get("call_id").is_some()
    });
    assert_eq!(
        (
            done["item"]["type"].clone(),
            done["item"]["name"].clone(),
            done["item"]["namespace"].clone()
        ),
        (json!("function_call"), json!("wait"), json!("functions"))
    );
}

#[test]
fn muse_dotted_history_resolves() {
    let unified = ResponsesTransformer::new().parse_request(&json!({"model": "m", "tools": [{"type": "namespace", "name": "muse", "tools": [{"type": "function", "name": "bash", "parameters": {"type": "object", "properties": {}}}]}], "input": [user_message(), {"type": "function_call", "call_id": "call_1", "name": "muse.bash", "arguments": "{}"}]})).unwrap();
    assert_eq!(
        unified["messages"][1]["tool_calls"][0]["function"]["name"],
        "muse__bash"
    );
    let unified = ResponsesTransformer::new().parse_request(&json!({"model": "m", "tools": [{"type": "function", "name": "x.y", "parameters": {}}], "input": [{"type": "function_call", "call_id": "call_1", "name": "x.y", "arguments": "{}"}]})).unwrap();
    assert_eq!(
        unified["messages"][0]["tool_calls"][0]["function"]["name"],
        "x.y"
    );
}

// ---- request parsing / building ----------------------------------------------

#[test]
fn parse_request_hygiene_and_full_shape() {
    let minimal = json!({"model": "gpt-4o", "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Hello"}]}]});
    let unified = ResponsesTransformer::new().parse_request(&minimal).unwrap();
    for f in [
        "requestId",
        "max_tokens",
        "temperature",
        "stream",
        "reasoning",
        "include",
        "prompt_cache_key",
        "text",
        "parallel_tool_calls",
        "metadata",
        "tools",
        "response_format",
    ] {
        assert!(unified.get(f).is_none(), "{f}");
    }
    assert_eq!(
        stringify(&unified),
        format!(
            r#"{{"model":"gpt-4o","messages":[{{"role":"user","content":"Hello"}}],"tool_choice":"auto","incomingApiType":"responses","originalBody":{}}}"#,
            stringify(&minimal)
        )
    );
    assert!(ResponsesTransformer::new()
        .parse_request(&json!({"input": "x"}))
        .unwrap_err()
        .message
        .contains("model"));
    assert!(ResponsesTransformer::new()
        .parse_request(&json!({"model": "m"}))
        .unwrap_err()
        .message
        .contains("input"));

    let unified = ResponsesTransformer::new().parse_request(&json!({"model": "m", "input": "hi", "instructions": "Be concise.", "text": {"format": {"type": "json_schema", "name": "weather_report", "description": "d", "strict": false, "schema": {"type": "object"}}}})).unwrap();
    assert_eq!(
        unified["messages"],
        json!([{"role": "system", "content": "Be concise."}, {"role": "user", "content": "hi"}])
    );
    assert_eq!(
        stringify(&unified["response_format"]),
        r#"{"type":"json_schema","json_schema":{"type":"object"},"name":"weather_report","description":"d","strict":false}"#
    );
    let chat = OpenAITransformer.transform_request(&unified).unwrap();
    assert_eq!(
        chat["response_format"],
        json!({"type": "json_schema", "json_schema": {"name": "weather_report", "description": "d", "schema": {"type": "object"}, "strict": false}})
    );
}

#[test]
fn parse_request_tolerates_null_entries() {
    let unified = ResponsesTransformer::new().parse_request(&json!({"model": "gpt-4o", "input": [
        null,
        {"type": "message", "role": "user", "content": [null, {"type": "input_text", "text": "Hi"}]},
        {"type": "reasoning", "summary": [null, {"type": "summary_text", "text": "thinking"}]},
        {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "a"}, null, {"type": "output_text", "text": "b"}]},
    ]})).unwrap();
    assert_eq!(
        unified["messages"],
        json!([
            {"role": "user", "content": "Hi"},
            {"role": "assistant", "content": "thinking"},
            {"role": "assistant", "content": [{"type": "text", "text": "a"}, {"type": "text", "text": "b"}]},
        ])
    );
}

#[test]
fn build_request_round_trip_and_passthrough() {
    let request = json!({
        "model": "gpt-4o", "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Hello"}]}],
        "stream": true, "max_output_tokens": 1024, "temperature": 0.7, "top_p": 0.9, "top_logprobs": 3, "max_tool_calls": 5,
        "instructions": "Be concise.", "reasoning": {"effort": "medium"}, "include": ["reasoning.encrypted_content"],
        "prompt_cache_key": "cache-1", "parallel_tool_calls": true, "text": {"format": {"type": "text"}}, "user": "user-abc",
        "store": true, "background": false, "service_tier": "auto", "truncation": "auto", "metadata": {"session": "s1"},
        "previous_response_id": "resp_prev_1", "conversation": "conv_1", "prompt_cache_retention": "24h", "safety_identifier": "si-1",
        "stream_options": {"include_obfuscation": true},
        "tools": [{"type": "function", "name": "get_weather", "description": "Get the weather", "parameters": {"type": "object"}, "strict": false}, {"type": "web_search"}],
    });
    let t = ResponsesTransformer::new();
    let unified = t.parse_request(&request).unwrap();
    let built = t.transform_request(&unified).unwrap();
    assert_eq!(
        stringify(&built),
        concat!(
            r#"{"model":"gpt-4o","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"Hello"}]}],"stream":true,"#,
            r#""instructions":"Be concise.","max_output_tokens":1024,"temperature":0.7,"#,
            r#""tools":[{"type":"function","name":"get_weather","description":"Get the weather","parameters":{"type":"object"},"strict":false},{"type":"web_search"}],"#,
            r#""tool_choice":"auto","reasoning":{"effort":"medium"},"include":["reasoning.encrypted_content"],"prompt_cache_key":"cache-1","parallel_tool_calls":true,"#,
            r#""text":{"format":{"type":"text"}},"user":"user-abc","store":true,"background":false,"service_tier":"auto","truncation":"auto","metadata":{"session":"s1"},"#,
            r#""top_p":0.9,"top_logprobs":3,"max_tool_calls":5,"previous_response_id":"resp_prev_1","conversation":"conv_1","prompt_cache_retention":"24h","#,
            r#""safety_identifier":"si-1","stream_options":{"include_obfuscation":true}}"#,
        )
    );

    let mut cross = unified.clone();
    cross
        .as_object_mut()
        .unwrap()
        .shift_remove("incomingApiType");
    cross.as_object_mut().unwrap().shift_remove("originalBody");
    let built = t.transform_request(&cross).unwrap();
    assert!(built.get("user").is_none() && built.get("stream_options").is_none());

    // composite call ids normalized before parse are used when rebuilding
    let long =
        "call_enS4L7YycCRyOiWOg31Xpvwm|fc_0281edd961557cf2016a4b062d87948195968b8fa6c46b8c7a";
    let mut body = json!({"model": "gpt-4o", "input": [{"type": "function_call", "name": "exec_command", "call_id": long, "arguments": "{}"}, {"type": "function_call_output", "call_id": long, "output": "ok"}]});
    normalize_composite_responses_call_ids(&mut body);
    let built = t
        .transform_request(&t.parse_request(&body).unwrap())
        .unwrap();
    assert_eq!(
        built["input"],
        json!([
            {"type": "message", "role": "assistant", "content": []},
            {"type": "function_call", "call_id": "call_enS4L7YycCRyOiWOg31Xpvwm", "name": "exec_command", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "call_enS4L7YycCRyOiWOg31Xpvwm", "output": "ok"},
        ])
    );
}

#[test]
fn reasoning_projection() {
    assert_eq!(
        project_reasoning_for_responses(Some(
            &json!({"enabled": true, "effort": "high", "max_tokens": 5})
        )),
        Some(json!({"effort": "high"}))
    );
    assert_eq!(
        project_reasoning_for_responses(Some(&json!({"enabled": false}))),
        Some(json!({"effort": "none"}))
    );
    assert_eq!(
        project_reasoning_for_responses(Some(&json!({"effort": "off"}))),
        Some(json!({"effort": "none"}))
    );
    assert_eq!(
        project_reasoning_for_responses(Some(&json!({"enabled": true}))),
        None
    );
    assert_eq!(project_reasoning_for_responses(None), None);
}

// ---- unary response transform / format ---------------------------------------

#[test]
fn unary_transform_and_format() {
    let response = json!({"id": "resp_1", "object": "response", "model": "gpt-5", "created_at": 7, "output": [
        {"type": "reasoning", "summary": [{"type": "summary_text", "text": "think"}]},
        {"type": "image_generation_call", "id": "ig_1", "status": "completed", "result": TINY_IMAGE_B64},
        {"type": "message", "content": [{"type": "output_text", "text": "Here is your image:", "annotations": [{"type": "url_citation", "url": "https://x", "title": "X", "text": "t", "start_index": 0, "end_index": 3}]}]},
    ], "usage": {"input_tokens": 10, "output_tokens": 5, "total_tokens": 15, "input_tokens_details": {"cached_tokens": 4}}});
    let unified = ResponsesTransformer::new()
        .transform_response(&response)
        .unwrap();
    assert_eq!(unified["content"], "Here is your image:");
    assert_eq!(unified["reasoning_content"], "think");
    assert_eq!(unified["created"], json!(7));
    assert_eq!(
        unified["annotations"],
        json!([{"type": "url_citation", "url_citation": {"url": "https://x", "title": "X", "content": "t", "start_index": 0, "end_index": 3}}])
    );
    assert_eq!(
        unified["image_generation_calls"],
        json!([{"id": "ig_1", "status": "completed", "result": TINY_IMAGE_B64}])
    );
    assert_eq!(unified["usage"]["input_tokens"], json!(6));
    assert!(unified.get("tool_calls").is_none());

    let chat = OpenAITransformer.format_response(&unified).unwrap();
    assert_eq!(
        chat["choices"][0]["message"]["content"],
        json!(format!("Here is your image:\n{}", tiny_markdown()))
    );

    let formatted = ResponsesTransformer::new()
        .format_response(&unified)
        .unwrap();
    let types: Vec<&Value> = formatted["output"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| &i["type"])
        .collect();
    assert_eq!(
        types,
        vec![
            &json!("reasoning"),
            &json!("image_generation_call"),
            &json!("message")
        ]
    );
    assert_eq!(
        formatted["output"][0]["content"],
        json!([{"type": "reasoning_text", "text": "think"}])
    );
    assert_eq!(
        formatted["output"][2]["content"][0]["text"],
        "Here is your image:"
    );
    assert_eq!(formatted["usage"]["input_tokens"], json!(10));
    assert_eq!(
        formatted.as_object().unwrap().keys().collect::<Vec<_>>(),
        [
            "id",
            "object",
            "created_at",
            "completed_at",
            "status",
            "model",
            "output",
            "usage"
        ]
    );

    let chat_case = ResponsesTransformer::new().transform_response(&json!({"id": "c", "model": "m", "created": 3, "choices": [{"message": {"content": "", "tool_calls": [{"id": "t"}]}}]})).unwrap();
    assert_eq!(
        stringify(&chat_case),
        r#"{"id":"c","model":"m","created":3,"content":null,"reasoning_content":null,"tool_calls":[{"id":"t"}]}"#
    );
}

#[test]
fn extract_usage_from_completed_events() {
    let t = ResponsesTransformer::new();
    let u = t.extract_usage(r#"{"type":"response.completed","response":{"usage":{"input_tokens":10,"output_tokens":5,"total_tokens":15,"input_tokens_details":{"cached_tokens":4},"output_tokens_details":{"reasoning_tokens":2}}}}"#).unwrap();
    assert_eq!(
        u,
        json!({"input_tokens": 6, "output_tokens": 5, "cached_tokens": 4, "cache_creation_tokens": 0, "reasoning_tokens": 2})
    );
    assert_eq!(
        t.extract_usage(r#"{"type":"response.output_text.delta"}"#),
        None
    );
    assert_eq!(t.extract_usage("not json"), None);
    assert_eq!(
        (t.name(), t.default_endpoint()),
        ("responses", "/responses")
    );
}

/// Differential harness (manual): `RESPONSES_DIFF_IN=<scenarios.json>
/// RESPONSES_DIFF_OUT=<out.json> cargo test -- --ignored responses_diff_harness`.
/// Produces the same shape as a TS script driving the reference transformer.
#[test]
#[ignore]
fn responses_diff_harness() {
    let (Ok(input), Ok(output)) = (
        std::env::var("RESPONSES_DIFF_IN"),
        std::env::var("RESPONSES_DIFF_OUT"),
    ) else {
        return;
    };
    let sc: Value = parse(&std::fs::read_to_string(input).unwrap()).unwrap();
    let mut out = json!({"requests": [], "responses": [], "streams": [], "chunk_streams": []});
    for req in sc["requests"].as_array().unwrap() {
        let t = ResponsesTransformer::new();
        let r = t
            .parse_request(req)
            .and_then(|u| Ok(json!({"unified": u, "built": t.transform_request(&u)?})));
        out["requests"]
            .as_array_mut()
            .unwrap()
            .push(r.unwrap_or_else(|e| json!({"error": e.message})));
    }
    for r in sc["responses"].as_array().unwrap() {
        let t = ResponsesTransformer::new();
        let res = (|| {
            if let Some(req) = r.get("request") {
                t.parse_request(req)?;
            }
            let u = ResponsesTransformer::new().transform_response(&r["provider"])?;
            Ok::<_, EngineError>(json!({"unified": u, "formatted": t.format_response(&u)?}))
        })();
        out["responses"]
            .as_array_mut()
            .unwrap()
            .push(res.unwrap_or_else(|e| json!({"error": e.message})));
    }
    for s in sc["streams"].as_array().unwrap() {
        let t = ResponsesTransformer::new();
        if let Some(req) = s.get("request") {
            t.parse_request(req).unwrap();
        }
        let chunks = transform_events(s["events"].as_array().unwrap());
        let (raw, _) = format_chunks(&t, &chunks);
        out["streams"]
            .as_array_mut()
            .unwrap()
            .push(json!({"chunks": chunks, "raw": raw}));
    }
    for s in sc["chunk_streams"].as_array().unwrap() {
        let t = ResponsesTransformer::new();
        if let Some(req) = s.get("request") {
            t.parse_request(req).unwrap();
        }
        let mut f = t.stream_formatter().unwrap();
        let mut raw = String::new();
        for c in s["chunks"].as_array().unwrap() {
            raw.extend(f.on_chunk(c));
        }
        raw.extend(f.finish());
        out["chunk_streams"]
            .as_array_mut()
            .unwrap()
            .push(json!({"raw": raw}));
    }
    std::fs::write(output, serde_json::to_string_pretty(&out).unwrap()).unwrap();
}

/// `responses-codex-lite-golden-trace.test.ts`: real Codex lite request +
/// OpenRouter chat chunks (staging d083e55a), replayed through
/// parse_request → OpenAI stream transformer → Responses stream formatter.
/// Reads the TS fixtures from `.reference`; skipped when they are absent.
#[test]
fn codex_lite_golden_trace() {
    let dir = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../.reference/plexus/packages/backend/src/transformers/__tests__/fixtures/"
    );
    let (Ok(req), Ok(chunks)) = (
        std::fs::read_to_string(format!("{dir}codex-lite-openrouter-chat-request.json")),
        std::fs::read_to_string(format!(
            "{dir}codex-lite-openrouter-chat-response-chunks.json"
        )),
    ) else {
        return;
    };
    let (req, chunks) = (parse(&req).unwrap(), parse(&chunks).unwrap());
    let t = ResponsesTransformer::new();
    let unified = t.parse_request(&req).unwrap();
    let names: Vec<&str> = unified["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["function"]["name"].as_str())
        .collect();
    for n in ["exec", "wait", "request_user_input"] {
        assert!(names.contains(&n), "{n}");
    }
    let msgs = unified["messages"].as_array().unwrap();
    assert!(msgs.iter().any(|m| m["tool_calls"]
        .as_array()
        .is_some_and(|tc| tc.iter().any(|c| c["function"]["name"] == "exec"))));
    assert_eq!(msgs.iter().filter(|m| m["role"] == "tool").count(), 7);
    assert!(!msgs
        .iter()
        .any(|m| stringify(m).contains("\"type\":\"additional_tools\"")));

    let mut parser = SseParser::new();
    let mut st = OpenAITransformer.stream_transformer().unwrap();
    let mut unified_chunks = Vec::new();
    for c in chunks.as_array().unwrap() {
        for ev in parser.feed(&format!("data: {}\n\n", stringify(c))) {
            unified_chunks.extend(st.on_event(&ev));
        }
    }
    for ev in parser.feed("data: [DONE]\n\n") {
        unified_chunks.extend(st.on_event(&ev));
    }
    unified_chunks.extend(st.finish());
    let (_, events) = format_chunks(&t, &unified_chunks);
    let done = find(&events, |e| {
        e["type"] == "response.output_item.done" && e["item"]["type"] == "custom_tool_call"
    });
    assert_eq!(done["item"]["name"], "exec");
    assert_eq!(done["item"]["call_id"], "call_x1iAotVivK6LxSOhrkT9PeQ0");
    let input = done["item"]["input"].as_str().unwrap();
    assert!(input.contains(
        "const r = await tools.exec_command({cmd:\"nl -ba packages/plexus-models/src/convert.ts"
    ));
    assert!(input.ends_with("text(r.output);"));
    assert!(!events
        .iter()
        .any(|e| e["type"] == "response.function_call_arguments.delta"));
    assert!(events.iter().any(|e| e["type"] == "response.completed"));
}
