//! `services/dispatch/image-model-bridge.ts`: a chat-shaped request naming an
//! image model is served through the image pipeline and answered with
//! synthesized unified chat output.
use super::dispatcher::{dispatch_image_generations, MediaHost, MediaResult};
use super::image::{encode_base64, resolve_image_reference, validation_error};
use crate::engine::config::Config;
use crate::engine::image_rendering::image_generation_call_markdown;
use crate::engine::js::*;
use crate::engine::routing::RouteResult;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::Mutex;

fn warned_aliases() -> &'static Mutex<HashSet<String>> {
    static WARNED: std::sync::OnceLock<Mutex<HashSet<String>>> = std::sync::OnceLock::new();
    WARNED.get_or_init(|| Mutex::new(HashSet::new()))
}

/// `isImageModelRoute(request, candidates, config)`.
pub fn is_image_model_route(request: &Value, candidates: &[RouteResult], config: &Config) -> bool {
    if truthy(request.get("_isVisionDescriptorRequest")) || candidates.is_empty() { return false; }
    let canonical = candidates[0].canonical_model.as_deref();
    if let Some(c) = canonical.filter(|c| !c.is_empty()) {
        if config.alias(c).and_then(|a| a.get("type")) == Some(&json!("image")) { return true; }
    }
    let types: Vec<Option<&Value>> = candidates.iter().map(|c| c.model_config.as_ref().and_then(|m| m.get("type"))).collect();
    if types.iter().all(|t| *t == Some(&json!("image"))) { return true; }
    if types.iter().any(|t| *t == Some(&json!("image"))) {
        let alias = canonical.map(str::to_owned).unwrap_or_else(|| to_string(request.get("model")));
        warned_aliases().lock().unwrap().insert(alias);
    }
    false
}

fn message_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.trim().to_owned(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter(|p| p.get("type") == Some(&json!("text")))
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .filter(|t| !t.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_owned(),
        _ => String::new(),
    }
}

fn message_image_references(content: Option<&Value>) -> Vec<Value> {
    let Some(Value::Array(parts)) = content else { return Vec::new() };
    parts
        .iter()
        .filter(|p| p.get("type") == Some(&json!("image_url")) && get_str(p, &["image_url", "url"]).is_some())
        .map(|p| {
            let mut r = json!({"type": "image_url", "image_url": {"url": p["image_url"]["url"].clone()}});
            if let Some(m) = p.get("media_type").filter(|m| truthy(Some(m))) { r["media_type"] = m.clone(); }
            r
        })
        .collect()
}

/// `buildImageRequestFromChat(request)`.
pub fn build_image_request_from_chat(request: &Value) -> MediaResult<Value> {
    let messages = get_arr(request, &["messages"]).cloned().unwrap_or_default();
    let last_user = messages.iter().rev().find(|m| m.get("role") == Some(&json!("user")));
    let completions_prompt = match request.get("prompt") {
        Some(Value::String(s)) => s.trim().to_owned(),
        Some(Value::Array(a)) => a.iter().map(|v| to_string(Some(v))).collect::<Vec<_>>().join("\n").trim().to_owned(),
        _ => String::new(),
    };
    let prompt = if completions_prompt.is_empty() { message_text(last_user.and_then(|m| m.get("content"))) } else { completions_prompt };
    if prompt.is_empty() {
        return Err(validation_error(
            format!("Model '{}' is an image model, but the request carries no prompt text to generate from.", to_string(request.get("model"))),
            400,
        ));
    }
    let refs = message_image_references(last_user.and_then(|m| m.get("content")));
    let mut out = Obj::new();
    set_opt(&mut out, "model", request.get("model").cloned());
    out.insert("prompt".into(), json!(prompt));
    out.insert("n".into(), json!(1));
    if !refs.is_empty() { out.insert("input_references".into(), Value::Array(refs.clone())); }
    out.insert("incomingApiType".into(), json!("images"));
    set_opt(&mut out, "requestId", request.get("requestId").cloned());
    set_opt(&mut out, "metadata", request.get("metadata").cloned());
    out.insert("originalBody".into(), obj(vec![
        ("bridged_from", Some(coalesce(request.get("incomingApiType"), None).unwrap_or(json!("chat")))),
        ("model", request.get("model").cloned()),
        ("messages", Some(json!(get_arr(request, &["messages"]).map(Vec::len).unwrap_or(0)))),
        ("input_references", Some(json!(refs.len()))),
        ("stream", Some(json!(request.get("stream") == Some(&json!(true))))),
    ]));
    Ok(Value::Object(out))
}

/// `toUnifiedUsage(usage)`.
fn to_unified_usage(usage: Option<&Value>) -> Value {
    let u = usage.cloned().unwrap_or(Value::Null);
    let input = coalesce(u.get("input_tokens"), u.get("prompt_tokens")).unwrap_or(json!(0));
    let output = coalesce(u.get("output_tokens"), u.get("completion_tokens")).unwrap_or(json!(0));
    let total = coalesce(u.get("total_tokens"), None).unwrap_or_else(|| num(input.as_f64().unwrap_or(0.0) + output.as_f64().unwrap_or(0.0)));
    json!({"input_tokens": input, "output_tokens": output, "total_tokens": total, "reasoning_tokens": 0, "cached_tokens": 0, "cache_creation_tokens": 0})
}

/// `toImageGenerationCalls(response)`.
async fn to_image_generation_calls(response: &Value) -> MediaResult<Vec<Value>> {
    let mut results = Vec::new();
    for item in get_arr(response, &["data"]).cloned().unwrap_or_default() {
        if let Some(b) = item.get("b64_json").and_then(Value::as_str).filter(|s| !s.is_empty()) { results.push(b.to_owned()); continue; }
        let Some(url) = item.get("url").and_then(Value::as_str).filter(|s| !s.is_empty()) else { results.push(String::new()); continue };
        let mut reference = json!({"type": "image_url", "image_url": {"url": url}});
        if let Some(m) = item.get("media_type").filter(|m| truthy(Some(m))) { reference["media_type"] = m.clone(); }
        let (_, data) = resolve_image_reference(&reference).await?;
        results.push(encode_base64(&data));
    }
    Ok(results.into_iter().filter(|r| !r.is_empty()).map(|r| json!({"id": format!("ig_{}", uuid()), "status": "completed", "result": r})).collect())
}

/// Unary or streaming synthesis result. The streaming form carries the
/// already-unified chunk list (`bypassTransformation: false`).
pub enum BridgedResponse {
    Unary(Value),
    Stream { response: Value, chunks: Vec<Value> },
}

/// `synthesizeChatResponse` / `synthesizeChatStream`.
pub async fn synthesize(image_response: &Value, request: &Value) -> MediaResult<BridgedResponse> {
    let id = format!("imgbridge_{}", uuid());
    let created = coalesce(image_response.get("created"), None).unwrap_or_else(|| json!(now_secs()));
    let items = to_image_generation_calls(image_response).await?;
    let plexus = image_response.get("plexus").cloned();
    if request.get("stream").is_none_or(|s| !truthy(Some(s))) {
        return Ok(BridgedResponse::Unary(obj(vec![
            ("id", Some(json!(id))),
            ("model", request.get("model").cloned()),
            ("created", Some(created)),
            ("content", Some(Value::Null)),
            ("image_generation_calls", Some(Value::Array(items))),
            ("finishReason", Some(json!("stop"))),
            ("usage", Some(to_unified_usage(image_response.get("usage")))),
            ("plexus", plexus),
        ])));
    }
    let mut chunks = Vec::new();
    for item in items {
        let mut typed = item.clone();
        typed["type"] = json!("image_generation_call");
        let Some(markdown) = image_generation_call_markdown(&typed) else { continue };
        chunks.push(obj(vec![
            ("id", Some(json!(id))),
            ("model", request.get("model").cloned()),
            ("created", Some(created.clone())),
            ("delta", Some(json!({"role": "assistant", "content": markdown}))),
            ("image_generation_calls", Some(json!([item]))),
            ("finish_reason", Some(Value::Null)),
        ]));
    }
    chunks.push(obj(vec![
        ("id", Some(json!(id))),
        ("model", request.get("model").cloned()),
        ("created", Some(created)),
        ("delta", Some(json!({}))),
        ("finish_reason", Some(json!("stop"))),
        ("usage", Some(to_unified_usage(image_response.get("usage")))),
    ]));
    let response = obj(vec![
        ("id", Some(json!(id))),
        ("model", request.get("model").cloned()),
        ("content", Some(Value::Null)),
        ("bypassTransformation", Some(json!(false))),
        ("plexus", plexus),
    ]);
    Ok(BridgedResponse::Stream { response, chunks })
}

/// `bridgeChatToImageGeneration(request, candidates, host)`.
pub async fn bridge_chat_to_image_generation(
    host: &dyn MediaHost,
    request: &Value,
    resolve_timeout_ms: Option<&(dyn Fn(Option<f64>) -> f64 + Sync)>,
) -> MediaResult<BridgedResponse> {
    let image_request = build_image_request_from_chat(request)?;
    let image_response = dispatch_image_generations(host, &image_request, resolve_timeout_ms).await?;
    synthesize(&image_response, request).await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn builds_image_request_from_last_user_message() {
        let req = json!({"model": "img", "incomingApiType": "chat", "messages": [
            {"role": "user", "content": "old"},
            {"role": "user", "content": [{"type": "text", "text": " draw a cat "}, {"type": "image_url", "image_url": {"url": "data:image/png;base64,aGk="}}]}
        ]});
        let out = build_image_request_from_chat(&req).unwrap();
        assert_eq!(out["prompt"], json!("draw a cat"));
        assert_eq!(out["input_references"][0]["image_url"]["url"], json!("data:image/png;base64,aGk="));
        assert_eq!(out["originalBody"], json!({"bridged_from": "chat", "model": "img", "messages": 2, "input_references": 1, "stream": false}));
        assert!(build_image_request_from_chat(&json!({"model": "img", "messages": []})).is_err());
    }
}
