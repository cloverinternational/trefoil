//! `transformers/responses/response-transformer.ts`: provider response
//! (Responses API, or Chat Completions when routing responses → chat) →
//! unified response.
use super::js_ext::{join, prop};
use super::tool_mapper::custom_tool_arguments_for_model;
use super::utils::{is_client_executed_tool_item, to_unified_image_generation_call};
use crate::engine::js::*;
use crate::engine::transformer::{EngineError, EngineResult};
use crate::engine::usage::{normalize_openai_chat, normalize_openai_responses};
use serde_json::{json, Value};

/// `parts.map(part => part.text).join('\n') || null` (`parts?.` absent → null).
fn joined_texts(parts: Option<&Value>, what: &str) -> EngineResult<Value> {
    let parts = match parts {
        None | Some(Value::Null) => return Ok(Value::Null),
        Some(Value::Array(p)) => p,
        Some(_) => return Err(EngineError::new(format!("{what}?.map is not a function. (In '{what}?.map((part) => part.text)', '{what}?.map' is undefined)"))),
    };
    let mut texts = Vec::with_capacity(parts.len());
    for part in parts {
        texts.push(prop(Some(part), "part.text")?);
    }
    let joined = join(&texts, "\n");
    Ok(if joined.is_empty() {
        Value::Null
    } else {
        Value::String(joined)
    })
}

/// `array.find(item => item.type === type)` (throws on null items like JS).
fn find_by_type<'a>(items: &'a [Value], ty: &str) -> EngineResult<Option<&'a Value>> {
    for item in items {
        if prop(Some(item), "item.type")?.and_then(Value::as_str) == Some(ty) {
            return Ok(Some(item));
        }
    }
    Ok(None)
}

/// `transformResponsesResponse`.
pub fn transform_responses_response(response: &Value) -> EngineResult<Value> {
    if truthy(response.get("output")) && get_str(response, &["object"]) == Some("response") {
        let output: &[Value] = match &response["output"] {
            Value::Array(o) => o,
            _ => {
                return Err(EngineError::new(
                    "response.output?.find is not a function. (In 'response.output?.find((item) => item.type === \"message\")', 'response.output?.find' is undefined)",
                ))
            }
        };
        let usage = response
            .get("usage")
            .filter(|u| truthy(Some(u)))
            .map(normalize_openai_responses);

        let message_item = find_by_type(output, "message")?;
        let message_content = message_item.and_then(|m| m.get("content"));
        let content = joined_texts(message_content, "messageItem?.content")?;

        let image_generation_calls: Vec<Value> = output
            .iter()
            .filter(|item| {
                get_str(item, &["type"]) == Some("image_generation_call")
                    && get_str(item, &["result"]).is_some_and(|r| !r.is_empty())
            })
            .map(to_unified_image_generation_call)
            .collect();

        let client_tool_calls: Vec<Value> = output
            .iter()
            .filter(|i| is_client_executed_tool_item(Some(i)))
            .cloned()
            .collect();

        let mut annotations = Vec::new();
        let annotation_parts: &[Value] = match message_content {
            None | Some(Value::Null) => &[],
            Some(Value::Array(p)) => p,
            Some(_) => return Err(EngineError::new("messageItem.content is not iterable")),
        };
        for part in annotation_parts {
            if let Some(Value::Array(anns)) = prop(Some(part), "part.annotations")? {
                for ann in anns {
                    if prop(Some(ann), "ann.type")?.and_then(Value::as_str) == Some("url_citation")
                    {
                        annotations.push(json!({"type": "url_citation", "url_citation": obj(vec![
                            ("url", ann.get("url").cloned()),
                            ("title", ann.get("title").cloned()),
                            ("content", coalesce(ann.get("text"), ann.get("content"))),
                            ("start_index", ann.get("start_index").cloned()),
                            ("end_index", ann.get("end_index").cloned()),
                        ])}));
                    }
                }
            }
        }

        let reasoning_item = find_by_type(output, "reasoning")?;
        let reasoning_content_parts = reasoning_item.and_then(|r| r.get("content"));
        let content_has_length = match reasoning_content_parts {
            Some(Value::Array(a)) => !a.is_empty(),
            Some(Value::String(s)) => !s.is_empty(),
            _ => false,
        };
        let reasoning_parts = if content_has_length {
            reasoning_content_parts
        } else {
            reasoning_item.and_then(|r| r.get("summary"))
        };
        let reasoning_content = joined_texts(
            reasoning_parts,
            "(reasoningItem?.content?.length ? reasoningItem.content : reasoningItem?.summary)",
        )?;

        let mut tool_calls = Vec::new();
        for item in output {
            let ty = prop(Some(item), "item.type")?.and_then(Value::as_str);
            if ty != Some("function_call") && ty != Some("custom_tool_call") {
                continue;
            }
            let flat_name = if truthy(item.get("namespace")) {
                Some(Value::String(format!(
                    "{}__{}",
                    to_string(item.get("namespace")),
                    to_string(item.get("name"))
                )))
            } else {
                item.get("name").cloned()
            };
            let arguments = if ty == Some("custom_tool_call") {
                Some(Value::String(custom_tool_arguments_for_model(
                    item.get("input"),
                )))
            } else {
                item.get("arguments").cloned()
            };
            tool_calls.push(obj(vec![
                ("id", item.get("call_id").cloned()),
                ("type", Some(json!("function"))),
                (
                    "function",
                    Some(obj(vec![("name", flat_name), ("arguments", arguments)])),
                ),
            ]));
        }

        let created = if truthy(response.get("created_at")) {
            response["created_at"].clone()
        } else {
            json!(now_secs())
        };
        return Ok(obj(vec![
            ("id", response.get("id").cloned()),
            ("model", response.get("model").cloned()),
            ("created", Some(created)),
            ("content", Some(content)),
            ("reasoning_content", Some(reasoning_content)),
            (
                "annotations",
                if annotations.is_empty() {
                    None
                } else {
                    Some(Value::Array(annotations))
                },
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
                "image_generation_calls",
                if image_generation_calls.is_empty() {
                    None
                } else {
                    Some(Value::Array(image_generation_calls))
                },
            ),
            (
                "client_tool_calls",
                if client_tool_calls.is_empty() {
                    None
                } else {
                    Some(Value::Array(client_tool_calls))
                },
            ),
            ("usage", usage),
        ]));
    }

    // Chat Completions format.
    let choice = get(response, &["choices", "0"]);
    let message = choice.and_then(|c| c.get("message"));
    let usage = response
        .get("usage")
        .filter(|u| truthy(Some(u)))
        .map(normalize_openai_chat);
    Ok(obj(vec![
        ("id", response.get("id").cloned()),
        ("model", response.get("model").cloned()),
        ("created", response.get("created").cloned()),
        (
            "content",
            Some(or(message.and_then(|m| m.get("content")), None).unwrap_or(Value::Null)),
        ),
        (
            "reasoning_content",
            Some(or(message.and_then(|m| m.get("reasoning_content")), None).unwrap_or(Value::Null)),
        ),
        (
            "tool_calls",
            message.and_then(|m| m.get("tool_calls")).cloned(),
        ),
        ("usage", usage),
    ]))
}
