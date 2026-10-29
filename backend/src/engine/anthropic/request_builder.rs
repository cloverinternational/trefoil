//! `transformers/anthropic/request-builder.ts`: unified request → provider
//! Anthropic request.
//!
//! - System message extraction
//! - Message role normalization (tool → user)
//! - Tool call reconstruction from unified format
//! - Message merging (consecutive messages with the same role)
use super::helpers::{js_trim_start, json_parse, not_iterable, stringify_opt, type_error};
use super::thinking_clamp::clamp_anthropic_effort_and_thinking;
use super::tool_mapper::convert_unified_tools_to_anthropic;
use crate::engine::js::{coalesce, get, get_str, obj, or, spread, truthy, Obj};
use crate::engine::transformer::{EngineError, EngineResult};
use serde_json::{json, Value};

pub const UNSUPPORTED_IMAGE_URL_MESSAGE: &str =
    "Unsupported image_url: expected a base64 data URI or an http(s) URL for Anthropic targets";

/// Top-level Anthropic fields carried through on same-format requests.
const PASSTHROUGH_FIELDS: [&str; 8] = [
    "thinking",
    "output_config",
    "metadata",
    "tool_choice",
    "top_p",
    "top_k",
    "stop_sequences",
    "prompt_cache_key",
];

/// `BASE64_DATA_URL_PATTERN = /^data:([^;,]*)(?:;[^;,]*)*;base64,([\s\S]+)$/i`
/// → `(media_type_group, data_group)`.
///
/// Neither the media type nor the parameters may contain `,`, so the comma
/// of `;base64,` is the first comma after `data:`; everything before it must
/// end in `;base64`, and the media type runs up to the first `;`.
fn match_base64_data_url(url: &str) -> Option<(&str, &str)> {
    let bytes = url.as_bytes();
    if bytes.len() < 5 || !bytes[..5].eq_ignore_ascii_case(b"data:") {
        return None;
    }
    let rest = &url[5..];
    let comma = rest.find(',')?;
    let head = &rest[..comma];
    let hb = head.as_bytes();
    if hb.len() < 7 || !hb[hb.len() - 7..].eq_ignore_ascii_case(b";base64") {
        return None;
    }
    let pre = &head[..head.len() - 7];
    let media = pre.split(';').next().unwrap_or("");
    let data = &rest[comma + 1..];
    if data.is_empty() {
        return None;
    }
    Some((media, data))
}

/// `toAnthropicImageSource`: `data:` URIs become `base64` sources and
/// http(s) URLs become `url` sources. Anything else cannot be expressed to
/// Anthropic, so it is rejected locally with a 400 instead of being
/// forwarded as an empty image (which Anthropic rejects with an opaque
/// `image cannot be empty` error).
pub fn to_anthropic_image_source(
    url: Option<&Value>,
    fallback_media_type: Option<&Value>,
) -> EngineResult<Value> {
    if let Some(url) = url.and_then(Value::as_str) {
        if let Some((media, data)) = match_base64_data_url(url) {
            let media_type = if !media.is_empty() {
                json!(media)
            } else {
                or(fallback_media_type, Some(&json!("image/jpeg"))).unwrap_or(Value::Null)
            };
            return Ok(json!({"type": "base64", "media_type": media_type, "data": data}));
        }
        let b = url.as_bytes();
        let scheme_ok = (b.len() >= 7 && b[..7].eq_ignore_ascii_case(b"http://"))
            || (b.len() >= 8 && b[..8].eq_ignore_ascii_case(b"https://"));
        if scheme_ok {
            return Ok(json!({"type": "url", "url": url}));
        }
    }
    Err(EngineError::status(
        UNSUPPORTED_IMAGE_URL_MESSAGE,
        400,
        "invalid_request_error",
    ))
}

fn with_cache_control(mut block: Obj, part: &Value) -> Value {
    if let Some(cc) = part.get("cache_control") {
        block.insert("cache_control".into(), cc.clone());
    }
    Value::Object(block)
}

/// `buildAnthropicRequest`.
pub fn build_anthropic_request(request: &Value) -> EngineResult<Value> {
    let mut system: Option<Value> = None;
    // Built messages; the third element carries the caller's raw
    // mid-conversation `role: 'system'` entry.
    let mut messages: Vec<(String, Vec<Value>, Option<Value>)> = Vec::new();

    // Anthropic mid-conversation `role: 'system'` entries live inside
    // `messages` and must be re-emitted there verbatim: they carry per-turn
    // directives (`output_config` under the mid-conversation-output-config
    // beta, the system-level content of mid-conversation-system) plus fields
    // the unified shape does not model. The parser marks each one, so pairing
    // the markers with the caller's own entries in order recovers the exact
    // entry — content order and block-level `cache_control` included. Anything
    // rebuilt from a unified request without `originalBody` falls back to the
    // flattened content.
    let native_mid_conversation_entries: Vec<&Value> = request
        .get("originalBody")
        .and_then(|b| b.get("messages"))
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter(|m| m.is_object() && get_str(m, &["role"]) == Some("system"))
                .collect()
        })
        .unwrap_or_default();
    let mut native_mid_conversation_cursor = 0;

    let Some(msgs) = request.get("messages").and_then(Value::as_array) else {
        return Err(not_iterable("request.messages"));
    };
    for msg in msgs {
        if msg.is_null() {
            return Err(type_error(Some(msg), "msg.role"));
        }
        let role = get_str(msg, &["role"]);
        match role {
            Some("system") if truthy(msg.get("mid_conversation")) => {
                let native = native_mid_conversation_entries
                    .get(native_mid_conversation_cursor)
                    .copied();
                native_mid_conversation_cursor += 1;
                let entry = match native {
                    Some(entry) => entry.clone(),
                    None => json!({
                        "role": "system",
                        "content": msg.get("content").and_then(Value::as_str).unwrap_or(""),
                    }),
                };
                messages.push(("system".to_owned(), Vec::new(), Some(entry)));
            }
            Some("system") => match msg.get("content") {
                Some(Value::String(s)) => system = Some(json!(s)),
                Some(Value::Array(blocks)) => {
                    // Filter out Claude Code-specific billing header blocks. These are only
                    // valid for the pi-ai OAuth Claude Code path and must not be forwarded via
                    // the translation path to upstream messages endpoints.
                    if let Some(b) = blocks.iter().find(|b| b.is_null()) {
                        return Err(type_error(Some(b), "block.type"));
                    }
                    let filtered: Vec<&Value> = blocks
                        .iter()
                        .filter(|b| {
                            !(get_str(b, &["type"]) == Some("text")
                                && get_str(b, &["text"]).is_some_and(|t| {
                                    js_trim_start(t).starts_with("x-anthropic-billing-header:")
                                }))
                        })
                        .collect();
                    if !filtered.is_empty() {
                        system = Some(Value::Array(
                            filtered
                                .iter()
                                .map(|b| {
                                    let mut block = Obj::new();
                                    if let Some(t) = b.get("type") {
                                        block.insert("type".into(), t.clone());
                                    }
                                    if let Some(t) = b.get("text") {
                                        block.insert("text".into(), t.clone());
                                    }
                                    with_cache_control(block, b)
                                })
                                .collect(),
                        ));
                    }
                }
                _ => {}
            },
            Some(r @ ("user" | "assistant")) => {
                let mut content: Vec<Value> = Vec::new();
                if truthy(msg.get("thinking")) {
                    content.push(obj(vec![
                        ("type", Some(json!("thinking"))),
                        ("thinking", get(msg, &["thinking", "content"]).cloned()),
                        ("signature", get(msg, &["thinking", "signature"]).cloned()),
                    ]));
                }
                if truthy(msg.get("content")) {
                    match &msg["content"] {
                        Value::String(s) => content.push(json!({"type": "text", "text": s})),
                        Value::Array(parts) => {
                            for part in parts {
                                if part.is_null() {
                                    return Err(type_error(Some(part), "part.type"));
                                }
                                match get_str(part, &["type"]) {
                                    Some("text") => {
                                        let mut block = Obj::new();
                                        block.insert("type".into(), json!("text"));
                                        if let Some(t) = part.get("text") {
                                            block.insert("text".into(), t.clone());
                                        }
                                        content.push(with_cache_control(block, part));
                                    }
                                    Some("image_url") => {
                                        let mut block = Obj::new();
                                        block.insert("type".into(), json!("image"));
                                        block.insert(
                                            "source".into(),
                                            to_anthropic_image_source(
                                                get(part, &["image_url", "url"]),
                                                part.get("media_type"),
                                            )?,
                                        );
                                        content.push(with_cache_control(block, part));
                                    }
                                    _ => {}
                                }
                            }
                        }
                        _ => {}
                    }
                }
                if r == "assistant" && truthy(msg.get("tool_calls")) {
                    let Some(calls) = msg["tool_calls"].as_array() else {
                        return Err(not_iterable("msg.tool_calls"));
                    };
                    for tc in calls {
                        if tc.is_null() {
                            return Err(type_error(Some(tc), "tc.id"));
                        }
                        let func = tc.get("function").filter(|f| !f.is_null());
                        let Some(func) = func else {
                            return Err(type_error(tc.get("function"), "tc.function.name"));
                        };
                        content.push(obj(vec![
                            ("type", Some(json!("tool_use"))),
                            ("id", tc.get("id").cloned()),
                            ("name", func.get("name").cloned()),
                            ("input", Some(json_parse(func.get("arguments"))?)),
                        ]));
                    }
                }
                messages.push((r.to_owned(), content, None));
            }
            Some("tool") => {
                let result_content = match msg.get("content") {
                    Some(Value::String(s)) => Some(json!(s)),
                    other => stringify_opt(other),
                };
                messages.push((
                    "user".into(),
                    vec![obj(vec![
                        ("type", Some(json!("tool_result"))),
                        ("tool_use_id", msg.get("tool_call_id").cloned()),
                        ("content", result_content),
                    ])],
                    None,
                ));
            }
            _ => {}
        }
    }

    // Merge consecutive messages of the same role. This is required by the
    // Anthropic API: can't have consecutive user or assistant messages.
    // Mid-conversation entries are forwarded byte-for-byte: merging would
    // concatenate content arrays and rewrite the caller's entries.
    enum Built {
        Entry(Value),
        Message(String, Vec<Value>),
    }
    let mut merged: Vec<Built> = Vec::new();
    for (role, content, mid_conversation_entry) in messages {
        if let Some(entry) = mid_conversation_entry {
            merged.push(Built::Entry(entry));
            continue;
        }
        if let Some(Built::Message(last_role, last_content)) = merged.last_mut() {
            if *last_role == role {
                last_content.extend(content);
                continue;
            }
        }
        merged.push(Built::Message(role, content));
    }
    let merged: Vec<Value> = merged
        .into_iter()
        .map(|b| match b {
            Built::Entry(entry) => entry,
            Built::Message(role, content) => json!({"role": role, "content": content}),
        })
        .collect();

    let tools = if truthy(request.get("tools")) {
        let Some(tools) = request["tools"].as_array() else {
            return Err(EngineError::new("tools.map is not a function."));
        };
        Some(Value::Array(convert_unified_tools_to_anthropic(tools)?))
    } else {
        None
    };

    let mut payload = obj(vec![
        ("model", request.get("model").cloned()),
        ("messages", Some(Value::Array(merged))),
        ("system", system),
        (
            "max_tokens",
            or(request.get("max_tokens"), Some(&json!(64000))),
        ),
        ("temperature", request.get("temperature").cloned()),
        ("stream", request.get("stream").cloned()),
        ("tools", tools),
    ]);

    // For same-format (messages -> messages) requests, carry through
    // Anthropic-native top-level fields the unified schema does not model
    // (thinking config, output config, metadata, ...) so they are not dropped
    // when the client talks the same API type as the upstream provider.
    let same_api =
        get_str(request, &["incomingApiType"]).is_some_and(|t| t.to_lowercase() == "messages");
    if same_api && truthy(request.get("originalBody")) {
        let original = &request["originalBody"];
        let map = payload.as_object_mut().expect("payload is an object");
        for field in PASSTHROUGH_FIELDS {
            if let Some(v) = original.get(field) {
                if !map.contains_key(field) {
                    map.insert(field.into(), v.clone());
                }
            }
        }
    }

    // Cross-format structured outputs (chat `response_format`, responses
    // `text.format` — both normalized onto `request.response_format`) are
    // expressed to Anthropic as `output_config.format`. A client-supplied
    // `format` (same-format pass-through above) always wins; a pre-existing
    // `output_config` (e.g. `effort`) is spread-preserved. Only `json_schema`
    // with a present schema is expressible; an explicit `strict: false` opts
    // out entirely, while an unset `strict` still forwards the schema.
    if let Some(rf) = request.get("response_format") {
        if get_str(rf, &["type"]) == Some("json_schema")
            && rf.get("json_schema").is_some()
            && rf.get("strict") != Some(&Value::Bool(false))
            && get(&payload, &["output_config", "format"]).is_none()
        {
            let existing =
                coalesce(payload.get("output_config"), Some(&json!({}))).unwrap_or(Value::Null);
            let mut output_config = spread(&existing, &json!({}));
            output_config
                .as_object_mut()
                .expect("spread yields an object")
                .insert(
                    "format".into(),
                    json!({"type": "json_schema", "schema": rf["json_schema"]}),
                );
            payload
                .as_object_mut()
                .expect("payload is an object")
                .insert("output_config".into(), output_config);
        }
    }

    Ok(clamp_anthropic_effort_and_thinking(
        payload,
        get_str(request, &["model"]),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::js::stringify;

    const PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

    fn request_with_image(url: &str, extra: Value) -> Value {
        let mut image = json!({"type": "image_url", "image_url": {"url": url}});
        for (k, v) in extra.as_object().unwrap() {
            image[k] = v.clone();
        }
        json!({"model": "claude-sonnet-5-5", "messages": [{"role": "user", "content": [{"type": "text", "text": "what is this?"}, image]}], "max_tokens": 1024})
    }

    fn image_block(req: Value) -> Value {
        build_anthropic_request(&req).unwrap()["messages"][0]["content"][1].clone()
    }

    #[test]
    fn image_url_conversion() {
        assert_eq!(
            image_block(request_with_image(
                &format!("data:image/png;base64,{PNG_B64}"),
                json!({})
            )),
            json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": PNG_B64}})
        );
        let big = "A".repeat(300_000);
        assert_eq!(
            image_block(request_with_image(
                &format!("data:image/png;base64,{big}"),
                json!({})
            ))["source"]["data"]
                .as_str()
                .unwrap()
                .len(),
            big.len()
        );
        assert_eq!(
            image_block(request_with_image(
                &format!("data:image/webp;base64,{PNG_B64}"),
                json!({"media_type": "image/jpeg"})
            ))["source"]["media_type"],
            json!("image/webp")
        );
        assert_eq!(
            image_block(request_with_image(
                &format!("data:;base64,{PNG_B64}"),
                json!({"media_type": "image/gif"})
            ))["source"],
            json!({"type": "base64", "media_type": "image/gif", "data": PNG_B64})
        );
        assert_eq!(
            image_block(request_with_image(
                &format!("data:image/png;name=a.png;base64,{PNG_B64}"),
                json!({})
            ))["source"],
            json!({"type": "base64", "media_type": "image/png", "data": PNG_B64})
        );
        assert_eq!(
            image_block(request_with_image("https://example.com/cat.png", json!({})))["source"],
            json!({"type": "url", "url": "https://example.com/cat.png"})
        );
        assert_eq!(
            image_block(request_with_image(
                &format!("data:image/png;base64,{PNG_B64}"),
                json!({"cache_control": {"type": "ephemeral"}})
            ))["cache_control"],
            json!({"type": "ephemeral"})
        );
        assert_eq!(
            image_block(request_with_image(
                &format!("DATA:;BASE64,{PNG_B64}"),
                json!({})
            ))["source"]["media_type"],
            json!("image/jpeg")
        );
        for url in [
            "data:image/png;base64,",
            "data:image/svg+xml,%3Csvg%2F%3E",
            "file:///tmp/a.png",
            "",
        ] {
            let err = build_anthropic_request(&request_with_image(url, json!({}))).unwrap_err();
            assert!(err.message.contains("Unsupported image_url"));
            assert_eq!(
                (err.status, err.code.as_deref()),
                (Some(400), Some("invalid_request_error"))
            );
        }
    }

    #[test]
    fn builds_and_merges_messages() {
        let req = json!({
            "model": "claude-sonnet-4-6",
            "messages": [
                {"role": "system", "content": [{"type": "text", "text": "  x-anthropic-billing-header: cc"}, {"type": "text", "text": "sys", "cache_control": {"type": "ephemeral"}}]},
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": "calling", "thinking": {"content": "hmm", "signature": "s"}, "tool_calls": [{"id": "t1", "type": "function", "function": {"name": "get", "arguments": "{\"a\":1}"}}]},
                {"role": "tool", "tool_call_id": "t1", "content": "ok"},
                {"role": "tool", "tool_call_id": "t2", "content": {"x": 1}},
                {"role": "user", "content": "more"},
            ],
            "tools": [{"type": "function", "function": {"name": "get", "parameters": {"type": "object"}}}],
            "stream": true,
            "incomingApiType": "Messages",
            "originalBody": {"thinking": {"type": "disabled"}, "output_config": {"effort": "high"}, "top_k": 5, "metadata": null},
        });
        let out = build_anthropic_request(&req).unwrap();
        assert_eq!(
            stringify(&out),
            stringify(&json!({
                "model": "claude-sonnet-4-6",
                "messages": [
                    {"role": "user", "content": [{"type": "text", "text": "hi"}]},
                    {"role": "assistant", "content": [{"type": "thinking", "thinking": "hmm", "signature": "s"}, {"type": "text", "text": "calling"}, {"type": "tool_use", "id": "t1", "name": "get", "input": {"a": 1}}]},
                    {"role": "user", "content": [
                        {"type": "tool_result", "tool_use_id": "t1", "content": "ok"},
                        {"type": "tool_result", "tool_use_id": "t2", "content": "{\"x\":1}"},
                        {"type": "text", "text": "more"},
                    ]},
                ],
                "system": [{"type": "text", "text": "sys", "cache_control": {"type": "ephemeral"}}],
                "max_tokens": 64000,
                "stream": true,
                "tools": [{"name": "get", "description": "", "input_schema": {"type": "object"}}],
                "thinking": {"type": "disabled"},
                "metadata": null,
                "top_k": 5,
            }))
        );
    }

    /// Parse an inbound Messages body and rebuild it the way the dispatcher
    /// does (same-format egress with `originalBody` attached).
    fn roundtrip(body: &Value, with_original: bool) -> Value {
        use crate::engine::anthropic::request_parser::parse_anthropic_request;
        let mut unified = parse_anthropic_request(body).unwrap();
        if with_original {
            unified["incomingApiType"] = json!("messages");
            unified["originalBody"] = body.clone();
        }
        build_anthropic_request(&unified).unwrap()
    }

    fn request_with_entry() -> Value {
        json!({
            "model": "claude-sonnet-4-6",
            "max_tokens": 1024,
            "system": "You are a helpful assistant.",
            "thinking": {"type": "adaptive"},
            "output_config": {"effort": "low"},
            "messages": [
                {"role": "user", "content": "first turn"},
                {"role": "assistant", "content": [{"type": "text", "text": "hi"}]},
                {"role": "system", "content": [{"type": "text", "text": "Answer in one sentence."}], "output_config": {"effort": "high"}},
                {"role": "user", "content": "second turn"},
            ],
        })
    }

    #[test]
    fn mid_conversation_entries_stay_in_place() {
        let request = request_with_entry();
        let built = roundtrip(&request, true);
        assert_eq!(built["system"], json!("You are a helpful assistant."));
        assert_eq!(built["messages"].as_array().unwrap().len(), 4);
        assert_eq!(built["messages"][2], request["messages"][2]);
    }

    #[test]
    fn mid_conversation_entries_are_not_hoisted_into_system() {
        let mut request = request_with_entry();
        request.as_object_mut().unwrap().remove("system");
        let built = roundtrip(&request, true);
        assert!(built.get("system").is_none());
        assert_eq!(built["messages"][2], request["messages"][2]);
    }

    #[test]
    fn consecutive_mid_conversation_entries_stay_separate() {
        let request = json!({
            "model": "claude-opus-5.5",
            "max_tokens": 1024,
            "messages": [
                {"role": "user", "content": "hello"},
                {"role": "system", "content": [], "output_config": {"effort": "low"}},
                {"role": "system", "content": [{"type": "text", "text": "Be terse."}]},
                {"role": "assistant", "content": [{"type": "text", "text": "hi"}]},
            ],
        });
        let built = roundtrip(&request, true);
        assert_eq!(built["messages"][1], request["messages"][1]);
        assert_eq!(built["messages"][2], request["messages"][2]);
    }

    #[test]
    fn output_config_only_entry_is_reemitted_verbatim() {
        let entry = json!({"role": "system", "content": [], "output_config": {"effort": "high"}});
        let request = json!({
            "model": "claude-opus-5.5",
            "max_tokens": 1024,
            "messages": [
                {"role": "user", "content": "hello"},
                entry,
                {"role": "assistant", "content": [{"type": "text", "text": "hi"}]},
            ],
        });
        assert_eq!(roundtrip(&request, true)["messages"][1], entry);
    }

    #[test]
    fn per_entry_effort_is_clamped_without_mutating_the_inbound_body() {
        let request = json!({
            "model": "claude-sonnet-4-6",
            "max_tokens": 1024,
            "messages": [
                {"role": "user", "content": "hello"},
                {"role": "system", "content": [
                    {"type": "text", "text": "Think hard."},
                    {"type": "text", "text": "And long."},
                ], "output_config": {"effort": "minimal"}},
                {"role": "assistant", "content": [{"type": "text", "text": "ok"}]},
            ],
        });
        let built = roundtrip(&request, true);
        assert_eq!(
            built["messages"][1]["output_config"],
            json!({"effort": "low"})
        );
        assert_eq!(
            request["messages"][1]["output_config"],
            json!({"effort": "minimal"})
        );
    }

    #[test]
    fn mid_conversation_entry_falls_back_to_flattened_content() {
        let built = roundtrip(&request_with_entry(), false);
        assert_eq!(
            built["messages"][2],
            json!({"role": "system", "content": "Answer in one sentence."})
        );
    }

    #[test]
    fn invalid_tool_arguments_throw() {
        let req = json!({"messages": [{"role": "assistant", "content": null, "tool_calls": [{"id": "t", "function": {"name": "n", "arguments": "{bad"}}]}]});
        assert!(build_anthropic_request(&req)
            .unwrap_err()
            .message
            .starts_with("JSON Parse error"));
    }
}
