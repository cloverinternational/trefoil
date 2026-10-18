//! `services/vision/vision-request-preprocessor.ts` and
//! `vision-descriptor-service.ts`: image-to-text fallthrough for aliases with
//! `use_image_fallthrough`, described by `vision_fallthrough.descriptor_model`
//! through a nested dispatch (tagged `_isVisionDescriptorRequest`).
use super::engine::{Engine, MetadataCatalog};
use super::request_manager::dispatch;
use crate::engine::js::*;
use crate::engine::observability::{calculate_costs, UsageRecord};
use crate::engine::routing::RouteResult;
use serde_json::{json, Value};

pub const DEFAULT_VISION_DESCRIPTION_PROMPT: &str = "Please provide a comprehensive and detailed description of this image for a highly intelligent text-only language model.

Your description should include:
1. **Overall Context**: What is the primary subject or scene?
2. **Visual Layout**: Where are key elements positioned?
3. **Important Details**: Text (OCR), colors, textures, and specific objects.
4. **Atmosphere/Style**: Mood, lighting, and artistic style if applicable.
5. **Action/Dynamics**: What is happening in the scene?

Focus on information that is critical for understanding the image's meaning or answering questions about it. Avoid generic descriptions; be precise and descriptive.";

fn is_image_block(block: &Value) -> bool { matches!(get_str(block, &["type"]), Some("image_url") | Some("image")) }

/// `VisionDescriptorService.hasImages(messages)`.
pub fn has_images(messages: Option<&Value>) -> bool {
    messages.and_then(Value::as_array).is_some_and(|ms| ms.iter().any(|m| m.get("content").and_then(Value::as_array).is_some_and(|c| c.iter().any(is_image_block))))
}

fn extract_images(messages: &[Value]) -> Vec<String> {
    let mut urls = Vec::new();
    for m in messages {
        let Some(content) = m.get("content").and_then(Value::as_array) else { continue };
        for block in content {
            match get_str(block, &["type"]) {
                Some("image_url") => urls.push(to_string(get(block, &["image_url", "url"]))),
                Some("image") => {
                    if let Some(data) = get(block, &["source", "data"]).filter(|d| truthy(Some(d))) {
                        let media = get(block, &["source", "media_type"]).filter(|m| truthy(Some(m))).map(|m| to_string(Some(m))).unwrap_or_else(|| "image/png".into());
                        urls.push(format!("data:{media};base64,{}", to_string(Some(data))));
                    }
                }
                _ => {}
            }
        }
    }
    urls
}

fn inject_descriptions(messages: &[Value], descriptions: &[String]) -> Vec<Value> {
    let mut idx = 0;
    messages.iter().map(|m| {
        let Some(content) = m.get("content").and_then(Value::as_array) else { return m.clone() };
        let new_content: Vec<Value> = content.iter().map(|block| {
            if !is_image_block(block) { return block.clone(); }
            let desc = descriptions.get(idx).filter(|d| !d.is_empty()).cloned().unwrap_or_else(|| "No description available.".into());
            idx += 1;
            json!({"type": "text", "text": format!("[Image Description: {desc}]")})
        }).collect();
        let mut out = m.as_object().cloned().unwrap_or_default();
        out.insert("content".into(), Value::Array(new_content));
        Value::Object(out)
    }).collect()
}

fn parent_field(parent: &Value, key: &str) -> Value { parent.get(key).cloned().unwrap_or(Value::Null) }

async fn describe_image(eng: &Engine, url: String, model: &str, prompt: &str, parent: &Value) -> String {
    if let Some(cached) = eng.state.inner.vision_descriptor_cache.lock().unwrap().get(&url, model) { return cached; }
    let mut request = json!({
        "model": model,
        "messages": [{"role": "user", "content": [{"type": "image_url", "image_url": {"url": url}}, {"type": "text", "text": prompt}]}],
        "incomingApiType": "chat",
    });
    for k in ["sourceIp", "apiKey", "keyName", "attribution"] { if let Some(v) = parent.get(k) { request[k] = v.clone(); } }
    let start = now_ms();
    let request_id = format!("desc-{}", uuid());
    request["_isVisionDescriptorRequest"] = json!(true);
    request["_descriptorStartTime"] = json!(start);
    request["requestId"] = json!(request_id);
    let sink = eng.usage.clone();
    match Box::pin(dispatch(eng, &mut request)).await {
        Ok(response) => {
            let response = response.unified;
            let description = response.get("content").filter(|c| truthy(Some(c))).map(|c| to_string(Some(c)));
            if let Some(u) = &sink {
                let p = |k: &str| get(&response, &["plexus", k]).cloned();
                let usage = |k: &str| get(&response, &["usage", k]).filter(|v| !v.is_null()).cloned().unwrap_or(Value::Null);
                let mut record = UsageRecord::from_value(json!({
                    "requestId": request_id, "date": now_iso(), "startTime": start, "durationMs": now_ms() - start, "createdAt": now_ms(),
                    "incomingApiType": "chat", "outgoingApiType": p("apiType").filter(|v| !v.is_null()).unwrap_or(Value::Null),
                    "provider": p("provider").filter(|v| !v.is_null()).unwrap_or(Value::Null),
                    "selectedModelName": p("model").filter(|v| !v.is_null()).unwrap_or(json!(model)),
                    "canonicalModelName": p("canonicalModel").filter(|v| !v.is_null()).unwrap_or(Value::Null),
                    "incomingModelAlias": model, "sourceIp": parent_field(parent, "sourceIp"), "apiKey": parent_field(parent, "apiKey"),
                    "attribution": parent_field(parent, "attribution"), "attemptCount": 1, "isStreamed": false, "isPassthrough": false,
                    "responseStatus": if description.is_some() { "success" } else { "error" },
                    "tokensInput": usage("input_tokens"), "tokensOutput": usage("output_tokens"), "tokensCached": usage("cached_tokens"),
                    "tokensCacheWrite": usage("cache_creation_tokens"), "tokensReasoning": usage("reasoning_tokens"),
                    "isDescriptorRequest": true, "isVisionFallthrough": false,
                }));
                if let Some(pricing) = p("pricing").filter(|v| truthy(Some(v))) {
                    calculate_costs(&mut record, Some(&pricing), p("providerDiscount").as_ref(), None, Some(&MetadataCatalog));
                }
                let u = u.clone();
                tokio::spawn(async move { crate::engine::observability::UsageSink::save_request(u.as_ref(), &record).await });
            }
            let Some(description) = description else { return "No description available.".into() };
            eng.state.inner.vision_descriptor_cache.lock().unwrap().insert(&url, model, description.clone());
            description
        }
        Err(_) => {
            if let Some(u) = &sink {
                let record = UsageRecord::from_value(json!({
                    "requestId": request_id, "date": now_iso(), "startTime": start, "durationMs": now_ms() - start, "createdAt": now_ms(),
                    "incomingApiType": "chat", "outgoingApiType": null, "provider": null, "selectedModelName": model, "canonicalModelName": null,
                    "incomingModelAlias": model, "sourceIp": parent_field(parent, "sourceIp"), "apiKey": parent_field(parent, "apiKey"),
                    "attribution": parent_field(parent, "attribution"), "attemptCount": 1, "isStreamed": false, "isPassthrough": false,
                    "responseStatus": "error", "tokensInput": null, "tokensOutput": null, "tokensCached": null, "costInput": null,
                    "costOutput": null, "costCached": null, "costTotal": null, "costSource": null, "costMetadata": null,
                    "isDescriptorRequest": true, "isVisionFallthrough": false,
                }));
                let u = u.clone();
                tokio::spawn(async move { crate::engine::observability::UsageSink::save_request(u.as_ref(), &record).await });
            }
            "Error generating image description.".into()
        }
    }
}

/// `preprocessVisionRequest(request, route, config, usageStorage)`.
pub async fn preprocess_vision_request(eng: &Engine, request: Value, route: &RouteResult) -> Value {
    let alias = route.canonical_model.as_deref().and_then(|c| eng.config.alias(c)).cloned();
    let is_descriptor = request.get("_isVisionDescriptorRequest") == Some(&Value::Bool(true));
    if is_descriptor || !truthy(alias.as_ref().and_then(|a| a.get("use_image_fallthrough"))) || !has_images(request.get("messages")) { return request; }
    let vision = eng.config.root.get("vision_fallthrough").cloned().unwrap_or(Value::Null);
    let Some(descriptor_model) = vision.get("descriptor_model").filter(|m| truthy(Some(m))).map(|m| to_string(Some(m))) else { return request };
    let prompt = vision.get("default_prompt").filter(|p| truthy(Some(p))).map(|p| to_string(Some(p))).unwrap_or_else(|| DEFAULT_VISION_DESCRIPTION_PROMPT.into());
    let messages = request.get("messages").and_then(Value::as_array).cloned().unwrap_or_default();
    let images = extract_images(&messages);
    let mut processed = request.clone();
    if !images.is_empty() {
        let descriptions = futures_util::future::join_all(images.into_iter().map(|url| describe_image(eng, url, &descriptor_model, &prompt, &request))).await;
        processed["messages"] = Value::Array(inject_descriptions(&messages, &descriptions));
    }
    processed["_hasVisionFallthrough"] = json!(true);
    processed["_visionFallthroughModel"] = json!(descriptor_model);
    processed
}
