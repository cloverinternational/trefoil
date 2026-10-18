//! `transformers/responses/response-formatter.ts`: unified response →
//! client Responses API body.
use super::js_ext::{iterate, prop};
use super::tool_mapper::{build_tool_output_item, ResponsesToolState};
use super::utils::{build_responses_usage_payload, generate_item_id, generate_response_id};
use crate::engine::js::*;
use crate::engine::transformer::EngineResult;
use serde_json::{json, Value};

/// `for (const x of v ?? [])`.
fn items_or_empty(v: Option<&Value>, expr: &str) -> EngineResult<Vec<Value>> {
    if nullish(v) { return Ok(Vec::new()); }
    iterate(v, expr)
}

/// `convertChatResponseToOutputItems`: reasoning, tool calls, typed image
/// items, client-executed tool items, then the assistant message.
pub fn convert_chat_response_to_output_items(response: &Value, state: Option<&ResponsesToolState>) -> EngineResult<Vec<Value>> {
    let mut items = Vec::new();
    let thinking_content = get(response, &["thinking", "content"]);

    if truthy(response.get("reasoning_content")) || truthy(thinking_content) {
        let empty = json!("");
        let reasoning_text = or(response.get("reasoning_content"), Some(&empty)).unwrap_or_default();
        let reasoning_summary = or(thinking_content, Some(&empty)).unwrap_or_default();
        let content_parts = if truthy(Some(&reasoning_text)) { json!([{"type": "reasoning_text", "text": reasoning_text}]) } else { json!([]) };
        let summary_parts = if truthy(Some(&reasoning_summary)) { json!([{"type": "summary_text", "text": reasoning_summary}]) } else { json!([]) };
        items.push(json!({
            "type": "reasoning",
            "id": generate_item_id("reason"),
            "status": "completed",
            "content": content_parts,
            "summary": summary_parts,
        }));
    }

    if let Some(tool_calls) = response.get("tool_calls").filter(|t| truthy(Some(t))) {
        // `response.tool_calls.length > 0`: only arrays and strings have one.
        let has_length = match tool_calls {
            Value::Array(a) => !a.is_empty(),
            Value::String(s) => !s.is_empty(),
            _ => false,
        };
        if has_length {
            for tc in &iterate(Some(tool_calls), "response.tool_calls")? { items.push(build_tool_output_item(tc, state)?); }
        }
    }

    let message_text = or(response.get("content"), Some(&json!(""))).unwrap_or_default();

    for image_call in &items_or_empty(response.get("image_generation_calls"), "response.image_generation_calls ?? []")? {
        if prop(Some(image_call), "imageCall.result")?.and_then(Value::as_str).is_none_or(str::is_empty) { continue; }
        items.push(json!({
            "type": "image_generation_call",
            "id": coalesce(image_call.get("id"), None).unwrap_or_else(|| Value::String(generate_item_id("ig"))),
            "status": coalesce(image_call.get("status"), None).unwrap_or_else(|| json!("completed")),
            "result": image_call["result"],
        }));
    }

    for client_tool_call in items_or_empty(response.get("client_tool_calls"), "response.client_tool_calls ?? []")? {
        items.push(client_tool_call);
    }

    let annotations = or(response.get("annotations"), Some(&json!([]))).unwrap_or_default();
    items.push(json!({
        "type": "message",
        "id": generate_item_id("msg"),
        "status": "completed",
        "role": "assistant",
        "content": [{"type": "output_text", "text": message_text, "annotations": annotations}],
    }));
    Ok(items)
}

/// `formatResponsesResponse`.
pub fn format_responses_response(response: &Value, state: Option<&ResponsesToolState>) -> EngineResult<Value> {
    let output = convert_chat_response_to_output_items(response, state)?;
    let created = if truthy(response.get("created")) { response["created"].clone() } else { json!(now_secs()) };
    Ok(obj(vec![
        ("id", Some(Value::String(generate_response_id()))),
        ("object", Some(json!("response"))),
        ("created_at", Some(created)),
        ("completed_at", Some(json!(now_secs()))),
        ("status", Some(json!("completed"))),
        ("model", response.get("model").cloned()),
        ("output", Some(Value::Array(output))),
        ("usage", build_responses_usage_payload(response.get("usage"))),
        ("plexus", response.get("plexus").cloned()),
    ]))
}
