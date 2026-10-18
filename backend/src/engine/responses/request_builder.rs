//! `transformers/responses/request-builder.ts`: unified request → provider
//! Responses request (plus `projectReasoningForResponses` from
//! `transformers/utils.ts`).
use super::js_ext::{iterate, prop, stringify_opt};
use crate::engine::js::*;
use crate::engine::transformer::{EngineError, EngineResult};
use serde_json::{json, Value};

/// Responses-native top-level fields carried through on same-format
/// (responses → responses) non-pass-through requests.
pub const PASSTHROUGH_FIELDS: [&str; 14] = [
    "user",
    "store",
    "background",
    "service_tier",
    "truncation",
    "metadata",
    "top_p",
    "top_logprobs",
    "max_tool_calls",
    "previous_response_id",
    "conversation",
    "prompt_cache_retention",
    "safety_identifier",
    "stream_options",
];

/// `projectReasoningForResponses`: unified reasoning → Responses
/// `{effort?, summary?}`; disabled / 'off' / 'none' become `{effort:'none'}`.
pub fn project_reasoning_for_responses(reasoning: Option<&Value>) -> Option<Value> {
    let reasoning = reasoning.filter(|r| truthy(Some(r)))?;
    let effort = reasoning.get("effort");
    if reasoning.get("enabled") == Some(&Value::Bool(false))
        || matches!(effort.and_then(Value::as_str), Some("off") | Some("none"))
    {
        return Some(json!({"effort": "none"}));
    }
    let projected = obj(vec![
        ("effort", effort.cloned()),
        ("summary", reasoning.get("summary").cloned()),
    ]);
    if projected.as_object().is_some_and(|m| !m.is_empty()) {
        Some(projected)
    } else {
        None
    }
}

/// JS `x.length > 0` for arrays and strings (other values have no length).
fn has_length(v: &Value) -> bool {
    match v {
        Value::Array(a) => !a.is_empty(),
        Value::String(s) => !s.is_empty(),
        _ => false,
    }
}

/// `buildResponsesRequest`.
pub fn build_responses_request(request: &Value) -> EngineResult<Value> {
    let messages = iterate(request.get("messages"), "request.messages")?;
    let messages = messages.as_slice();
    let mut input_items: Vec<Value> = Vec::new();

    for msg in messages {
        let role = prop(Some(msg), "msg.role")?.and_then(Value::as_str);
        match role {
            Some("system") => {}
            Some(r @ ("user" | "assistant")) => {
                let text_type = if r == "user" {
                    "input_text"
                } else {
                    "output_text"
                };
                let mut content: Vec<Value> = Vec::new();
                match msg.get("content") {
                    Some(Value::String(s)) => content.push(json!({"type": text_type, "text": s})),
                    Some(Value::Array(parts)) => {
                        for part in parts {
                            match prop(Some(part), "part.type")?.and_then(Value::as_str) {
                                Some("text") => content.push(obj(vec![
                                    ("type", Some(json!(text_type))),
                                    ("text", part.get("text").cloned()),
                                ])),
                                Some("image_url") => {
                                    let url = prop(part.get("image_url"), "part.image_url.url")?;
                                    content.push(obj(vec![
                                        ("type", Some(json!("input_image"))),
                                        ("image_url", url.cloned()),
                                        ("detail", Some(json!("auto"))),
                                    ]));
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
                input_items.push(json!({"type": "message", "role": r, "content": content}));
            }
            Some("tool") => {
                let output = match msg.get("content") {
                    Some(Value::String(s)) => Some(Value::String(s.clone())),
                    other => stringify_opt(other).map(Value::String),
                };
                input_items.push(obj(vec![
                    ("type", Some(json!("function_call_output"))),
                    ("call_id", msg.get("tool_call_id").cloned()),
                    ("output", output),
                ]));
            }
            _ => {}
        }

        if role == Some("assistant") && truthy(msg.get("tool_calls")) {
            for tc in &iterate(msg.get("tool_calls"), "msg.tool_calls")? {
                let call_id = prop(Some(tc), "tc.id")?;
                let function = tc.get("function");
                input_items.push(obj(vec![
                    ("type", Some(json!("function_call"))),
                    ("call_id", call_id.cloned()),
                    ("name", prop(function, "tc.function.name")?.cloned()),
                    (
                        "arguments",
                        function.and_then(|f| f.get("arguments")).cloned(),
                    ),
                ]));
            }
        }
    }

    if request.get("messages").is_some_and(Value::is_string) {
        return Err(EngineError::new(
            "request.messages.find is not a function. (In 'request.messages.find((m) => m.role === \"system\")', 'request.messages.find' is undefined)",
        ));
    }
    let system = messages
        .iter()
        .find(|m| get_str(m, &["role"]) == Some("system"));
    let instructions: Option<Value> = system.and_then(|s| match s.get("content") {
        Some(Value::String(c)) => Some(Value::String(c.clone())),
        other => stringify_opt(other).map(Value::String),
    });

    let tools: Option<Vec<Value>> = match request.get("tools") {
        None | Some(Value::Null) => None,
        Some(Value::Array(tools)) => Some(
            tools
                .iter()
                .map(|tool| {
                    if get_str(tool, &["type"]) != Some("function") || !truthy(tool.get("function"))
                    {
                        return tool.clone();
                    }
                    let f = &tool["function"];
                    obj(vec![
                        ("type", Some(json!("function"))),
                        ("name", coalesce(f.get("name"), Some(&json!("")))),
                        (
                            "description",
                            coalesce(f.get("description"), Some(&json!(""))),
                        ),
                        (
                            "parameters",
                            coalesce(f.get("parameters"), Some(&json!({}))),
                        ),
                        ("strict", f.get("strict").cloned()),
                    ])
                })
                .collect(),
        ),
        Some(_) => return Err(EngineError::new("request.tools?.map is not a function")),
    };

    let mut payload = Obj::new();
    set_opt(&mut payload, "model", request.get("model").cloned());
    payload.insert("input".into(), Value::Array(input_items));
    set_opt(&mut payload, "stream", request.get("stream").cloned());

    if let Some(i) = instructions.filter(|i| truthy(Some(i))) {
        payload.insert("instructions".into(), i);
    }
    if truthy(request.get("max_tokens")) {
        payload.insert("max_output_tokens".into(), request["max_tokens"].clone());
    }
    set_opt(
        &mut payload,
        "temperature",
        request.get("temperature").cloned(),
    );
    if let Some(t) = tools.filter(|t| !t.is_empty()) {
        payload.insert("tools".into(), Value::Array(t));
    }
    if truthy(request.get("tool_choice")) {
        payload.insert("tool_choice".into(), request["tool_choice"].clone());
    }
    if truthy(request.get("reasoning")) {
        if let Some(r) = project_reasoning_for_responses(request.get("reasoning")) {
            payload.insert("reasoning".into(), r);
        }
    }
    if request
        .get("include")
        .is_some_and(|i| truthy(Some(i)) && has_length(i))
    {
        payload.insert("include".into(), request["include"].clone());
    }
    if truthy(request.get("prompt_cache_key")) {
        payload.insert(
            "prompt_cache_key".into(),
            request["prompt_cache_key"].clone(),
        );
    }
    set_opt(
        &mut payload,
        "parallel_tool_calls",
        request.get("parallel_tool_calls").cloned(),
    );
    if truthy(request.get("text")) {
        payload.insert("text".into(), request["text"].clone());
    } else if let Some(rf) = request.get("response_format").filter(|r| truthy(Some(r))) {
        if get_str(rf, &["type"]) == Some("json_schema") && rf.get("json_schema").is_some() {
            // Rebuild the full structured-output descriptor: `name` gets a
            // fabricated fallback (upstream 400s without it); `strict` is
            // emitted ONLY when the client set it explicitly.
            payload.insert("text".into(), json!({"format": obj(vec![
                ("type", Some(json!("json_schema"))),
                ("name", Some(coalesce(rf.get("name"), Some(&json!("response_schema"))).unwrap_or(Value::Null))),
                ("description", rf.get("description").cloned()),
                ("strict", rf.get("strict").cloned()),
                ("schema", rf.get("json_schema").cloned()),
            ])}));
        } else {
            payload.insert(
                "text".into(),
                json!({"format": obj(vec![("type", rf.get("type").cloned())])}),
            );
        }
    }

    // Same-format (responses → responses) non-pass-through: carry native
    // top-level fields the explicit mapping above does not set.
    let same_format = request
        .get("incomingApiType")
        .and_then(Value::as_str)
        .is_some_and(|t| t.to_lowercase().split(':').next().unwrap_or("") == "responses");
    if same_format && truthy(request.get("originalBody")) {
        let original = &request["originalBody"];
        for field in PASSTHROUGH_FIELDS {
            if let Some(v) = original.get(field) {
                if !payload.contains_key(field) {
                    payload.insert(field.into(), v.clone());
                }
            }
        }
    }

    Ok(Value::Object(payload))
}
