//! Pure parts of `services/inspectors/usage-logging.ts` (`UsageInspector`),
//! the actual-tier reader from `services/dispatch/service-tier-metadata.ts`,
//! and the stream visibility tracker from `services/dispatch/empty-completion.ts`.
use crate::engine::config::api_format;
use crate::engine::js::{self, get, get_str, truthy};
use crate::engine::usage::{normalize_anthropic, normalize_gemini, normalize_openai_chat, normalize_openai_responses};
use serde_json::Value;

/// `ExtractedObservedUsage`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ObservedUsage {
    pub input_tokens: f64,
    pub output_tokens: f64,
    pub cached_tokens: f64,
    pub cache_write_tokens: f64,
    pub reasoning_tokens: f64,
}

impl ObservedUsage {
    /// From a unified usage object (`input_tokens`, `output_tokens`, …).
    pub fn from_unified(u: &Value) -> Self {
        let n = |k: &str| u.get(k).and_then(Value::as_f64).unwrap_or(0.0);
        Self { input_tokens: n("input_tokens"), output_tokens: n("output_tokens"), cached_tokens: n("cached_tokens"), cache_write_tokens: n("cache_creation_tokens"), reasoning_tokens: n("reasoning_tokens") }
    }
}

/// `extractUsageFromReconstructed(reconstructed, apiType)`.
pub fn extract_usage_from_reconstructed(reconstructed: Option<&Value>, api_type: &str) -> Option<ObservedUsage> {
    let r = reconstructed.filter(|r| truthy(Some(r)))?;
    let usage = |key: &str| r.get(key).filter(|u| truthy(Some(u)));
    match api_type {
        "chat" => usage("usage").map(|u| ObservedUsage::from_unified(&normalize_openai_chat(u))),
        "responses" => usage("usage").map(|u| ObservedUsage::from_unified(&normalize_openai_responses(u))),
        "messages" => usage("usage").map(|u| ObservedUsage::from_unified(&normalize_anthropic(u))),
        "gemini" => usage("usageMetadata").map(|u| ObservedUsage::from_unified(&normalize_gemini(u))),
        "oauth" => usage("usage").map(|u| {
            let n = |k: &str| if truthy(u.get(k)) { js::to_number(u.get(k)).unwrap_or(f64::NAN) } else { 0.0 };
            ObservedUsage { input_tokens: n("input_tokens"), output_tokens: n("output_tokens"), cached_tokens: n("cached_tokens"), cache_write_tokens: n("cache_creation_tokens"), reasoning_tokens: n("reasoning_tokens") }
        }),
        _ => None,
    }
}

/// Tool-call count and finish reason read from a reconstructed response.
/// `finish_reason: None` is `undefined` (only the Responses status
/// passthrough can produce it); `Some(Value::Null)` is `null`.
#[derive(Debug, Clone, PartialEq)]
pub struct ResponseMetadata {
    pub tool_calls_count: Option<f64>,
    pub finish_reason: Option<Value>,
}

fn filter_boolean_len(v: Option<&Value>) -> Option<usize> {
    v.and_then(Value::as_array).map(|a| a.iter().filter(|x| truthy(Some(x))).count())
}

/// `deepSearchToolCalls`.
pub fn deep_search_tool_calls(obj: &Value) -> usize {
    if !matches!(obj, Value::Object(_) | Value::Array(_)) { return 0; }
    let mut count = 0;
    if let Some(n) = filter_boolean_len(obj.get("tool_calls")) { count = count.max(n); }
    if truthy(obj.get("functionCall")) { count = count.max(1); }
    if let Some(Value::Array(parts)) = obj.get("parts") {
        count = count.max(parts.iter().filter(|p| truthy(p.get("functionCall"))).count());
    }
    if let Some(Value::Array(choices)) = obj.get("choices") {
        for c in choices {
            let inner = js::or(c.get("message"), js::or(c.get("delta"), Some(c)).as_ref()).unwrap_or(Value::Null);
            count = count.max(deep_search_tool_calls(&inner));
        }
    }
    if let Some(Value::Array(cands)) = obj.get("candidates") {
        for c in cands {
            let inner = js::or(c.get("content"), Some(c)).unwrap_or(Value::Null);
            count = count.max(deep_search_tool_calls(&inner));
        }
    }
    count
}

fn tool_finish(incoming_api_type: &str) -> Value {
    Value::from(if incoming_api_type == "messages" { "tool_use" } else { "tool_calls" })
}

fn some_count(n: usize) -> Option<f64> { if n > 0 { Some(n as f64) } else { None } }

/// `extractResponseMetadataFromReconstructed(reconstructed, providerApiType)`;
/// `incoming_api_type` picks `tool_use` vs `tool_calls` normalization.
pub fn extract_response_metadata(reconstructed: Option<&Value>, api_type: &str, incoming_api_type: &str) -> ResponseMetadata {
    let Some(r) = reconstructed.filter(|r| truthy(Some(r))) else {
        return ResponseMetadata { tool_calls_count: None, finish_reason: Some(Value::Null) };
    };
    let lower = |v: &Value| Value::from(js::to_string(Some(v)).to_lowercase());
    match api_type {
        "chat" => {
            let choice = get(r, &["choices", "0"]);
            let cg = |path: &[&str]| choice.and_then(|c| get(c, path));
            let candidates = [cg(&["delta", "tool_calls"]), cg(&["message", "tool_calls"]), cg(&["tool_calls"]), r.get("tool_calls"), cg(&["message", "function_call"]), r.get("function_call")];
            let tool_calls = candidates.into_iter().find(|c| truthy(*c)).flatten();
            let mut finish = js::or(cg(&["finish_reason"]), cg(&["finishReason"])).filter(|v| truthy(Some(v)));
            let mut count = filter_boolean_len(tool_calls).unwrap_or(0);
            if count == 0 && (truthy(cg(&["message", "function_call"])) || truthy(r.get("function_call"))) { count = 1; }
            if count == 0 { count = deep_search_tool_calls(r); }
            if count == 0 && truthy(get(r, &["candidates", "0"])) {
                let cand = &r["candidates"][0];
                if let Some(Value::Array(parts)) = get(cand, &["content", "parts"]) {
                    count = parts.iter().filter(|p| truthy(p.get("functionCall"))).count();
                }
                if finish.is_none() { finish = cand.get("finishReason").filter(|v| truthy(Some(v))).cloned(); }
            }
            let finish = match finish {
                Some(f) => {
                    let l = lower(&f);
                    if (l == "stop" || l == "end_turn") && count > 0 { tool_finish(incoming_api_type) } else { l }
                }
                None if count > 0 => tool_finish(incoming_api_type),
                None => Value::Null,
            };
            ResponseMetadata { tool_calls_count: some_count(count), finish_reason: Some(finish) }
        }
        "responses" => {
            let count = match r.get("output") {
                Some(Value::Array(items)) => items.iter().filter(|i| matches!(get_str(i, &["type"]), Some("function_call" | "custom_tool_call"))).count(),
                _ => 0,
            };
            let status = r.get("status");
            let finish = match status.and_then(Value::as_str) {
                Some("completed") => Some(Value::from(if count > 0 { "tool_calls" } else { "stop" })),
                Some("failed") => Some(Value::from("error")),
                Some("incomplete") => Some(Value::from(if get_str(r, &["incomplete_details", "reason"]) == Some("content_filter") { "content_filter" } else { "length" })),
                _ => status.cloned(),
            };
            ResponseMetadata { tool_calls_count: some_count(count), finish_reason: finish }
        }
        "messages" => {
            let count = match r.get("content") {
                Some(Value::Array(blocks)) => blocks.iter().filter(|b| get_str(b, &["type"]) == Some("tool_use")).count(),
                _ => 0,
            };
            let finish = js::or(r.get("stop_reason"), r.get("finish_reason")).filter(|v| truthy(Some(v))).unwrap_or(Value::Null);
            ResponseMetadata { tool_calls_count: some_count(count), finish_reason: Some(finish) }
        }
        "gemini" => {
            let cand = get(r, &["candidates", "0"]);
            let mut count = match cand.and_then(|c| get(c, &["content", "parts"])) {
                Some(Value::Array(parts)) => parts.iter().filter(|p| truthy(p.get("functionCall"))).count(),
                _ => 0,
            };
            if count == 0 { count = deep_search_tool_calls(r); }
            let mut finish = cand.and_then(|c| c.get("finishReason")).filter(|v| truthy(Some(v))).cloned();
            if count == 0 && truthy(get(r, &["choices", "0"])) {
                let choice = &r["choices"][0];
                let calls = js::or(get(choice, &["delta", "tool_calls"]), get(choice, &["message", "tool_calls"]));
                if let Some(n) = filter_boolean_len(calls.as_ref()) { count = n; }
                if finish.is_none() { finish = choice.get("finish_reason").filter(|v| truthy(Some(v))).cloned(); }
            }
            let finish = match finish {
                Some(f) => {
                    let l = lower(&f);
                    if l == "stop" && count > 0 { tool_finish(incoming_api_type) } else { l }
                }
                None if count > 0 => tool_finish(incoming_api_type),
                None => Value::Null,
            };
            ResponseMetadata { tool_calls_count: some_count(count), finish_reason: Some(finish) }
        }
        "oauth" => {
            let count = match r.get("tool_calls") {
                Some(Value::Array(a)) => a.len(),
                Some(Value::String(s)) => s.encode_utf16().count(),
                _ => 0,
            };
            ResponseMetadata { tool_calls_count: some_count(count), finish_reason: Some(js::coalesce(r.get("finishReason"), None).unwrap_or(Value::Null)) }
        }
        _ => {
            let calls = js::or(r.get("tool_calls"), get(r, &["choices", "0", "tool_calls"]));
            let count = calls.as_ref().and_then(Value::as_array).map(Vec::len).unwrap_or(0);
            let finish = js::or(r.get("finish_reason"), get(r, &["choices", "0", "finish_reason"])).filter(|v| truthy(Some(v))).unwrap_or(Value::Null);
            ResponseMetadata { tool_calls_count: some_count(count), finish_reason: Some(finish) }
        }
    }
}

// ---------------------------------------------------------------------------
// Service tier (actual, provider-reported)
// ---------------------------------------------------------------------------

/// `normalizeServiceTier(raw)`.
pub fn normalize_service_tier(raw: Option<&Value>) -> Option<String> {
    if js::nullish(raw) { return None; }
    let value = js::to_string(raw);
    let trimmed = value.trim();
    if trimmed.is_empty() { return None; }
    let lower = trimmed.to_lowercase();
    Some(match lower.as_str() {
        "on_demand" | "ondemand" | "standard" | "standard_only" | "default" => "default".into(),
        "fast" | "priority" | "on_demand_priority" => "priority".into(),
        "flex" | "on_demand_flex" => "flex".into(),
        "auto" | "performance" | "ultrafast" | "scale" | "reserved" | "deferred" => lower,
        _ => trimmed.to_owned(),
    })
}

/// `ServiceTierCapture`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ServiceTierCapture { pub tier: Option<String>, pub raw: Option<String> }

fn capture(raw: Option<String>) -> ServiceTierCapture {
    ServiceTierCapture { tier: normalize_service_tier(raw.clone().map(Value::String).as_ref()), raw }
}

fn read_string(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str).filter(|s| !s.trim().is_empty()).map(str::to_owned)
}

fn unwrap_event_envelope(body: &Value) -> &Value {
    if !body.is_object() { return body; }
    let t = get_str(body, &["type"]);
    if matches!(t, Some("message_start" | "message_delta")) && body.get("message").is_some_and(|m| m.is_object() || m.is_array()) {
        return unwrap_event_envelope(&body["message"]);
    }
    if t.is_some_and(|t| t.starts_with("response.")) && body.get("response").is_some_and(|m| m.is_object() || m.is_array()) {
        return unwrap_event_envelope(&body["response"]);
    }
    body
}

/// `findActualRaw(rawBody, apiBaseType)`.
pub fn find_actual_service_tier_raw(raw_body: Option<&Value>, api_base_type: Option<&str>) -> Option<String> {
    let body = unwrap_event_envelope(raw_body?);
    if !matches!(body, Value::Object(_) | Value::Array(_)) { return None; }
    if api_base_type == Some("responses") {
        if let Some(status) = read_string(body.get("status")) {
            if !matches!(status.to_lowercase().as_str(), "completed" | "failed" | "incomplete") { return None; }
        }
    }
    let usage = body.get("usage").filter(|u| u.is_object() || u.is_array());
    let usage_speed = read_string(usage.and_then(|u| u.get("speed")));
    if usage_speed.as_deref().is_some_and(|s| s.trim().to_lowercase() == "fast") { return usage_speed; }
    if let Some(t) = read_string(usage.and_then(|u| u.get("service_tier"))) { return Some(t); }
    if usage_speed.is_some() { return usage_speed; }
    if let Some(t) = read_string(get(body, &["usageMetadata", "trafficType"])) { return Some(t); }
    for candidate in [body.get("serviceTier"), get(body, &["metadata", "serviceTier"]), usage.and_then(|u| u.get("serviceTier"))] {
        match candidate {
            Some(Value::String(_)) => if let Some(raw) = read_string(candidate) { return Some(raw); },
            Some(Value::Object(o)) => if let Some(t) = read_string(o.get("type")) { return Some(t); },
            _ => {}
        }
    }
    read_string(body.get("service_tier"))
}

/// `extractActualServiceTier(body, headers, apiBaseType)`; `header_tier` is
/// the `x-gemini-service-tier` response header value, if any.
pub fn extract_actual_service_tier(body: Option<&Value>, header_tier: Option<&str>, api_base_type: Option<&str>) -> ServiceTierCapture {
    let base = api_base_type.map(api_format::base_type);
    let from_headers = capture(header_tier.filter(|h| !h.trim().is_empty()).map(str::to_owned));
    if base.as_deref() == Some("gemini") && from_headers.raw.is_some() { return from_headers; }
    match find_actual_service_tier_raw(body, base.as_deref()) {
        Some(raw) => capture(Some(raw)),
        None => from_headers,
    }
}

// ---------------------------------------------------------------------------
// Stream visibility (empty-completion detection)
// ---------------------------------------------------------------------------

/// `StreamVisibilityTracker`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StreamVisibilityTracker {
    pub has_text: bool,
    pub has_reasoning: bool,
    pub tool_call_count: usize,
    pub annotation_count: usize,
    pub image_count: usize,
    pub client_tool_call_count: usize,
}

fn has_visible_text(v: Option<&Value>) -> bool { v.and_then(Value::as_str).is_some_and(|s| !s.trim().is_empty()) }
fn non_empty_len(v: Option<&Value>) -> usize { v.and_then(Value::as_array).map(Vec::len).unwrap_or(0) }

impl StreamVisibilityTracker {
    /// `observeStreamChunk(tracker, chunk)`.
    pub fn observe(&mut self, chunk: &Value) {
        let Some(delta) = chunk.get("delta").filter(|d| truthy(Some(d))) else { return };
        if has_visible_text(delta.get("content")) { self.has_text = true; }
        if has_visible_text(delta.get("reasoning_content")) || has_visible_text(get(delta, &["thinking", "content"])) { self.has_reasoning = true; }
        self.tool_call_count += non_empty_len(delta.get("tool_calls"));
        self.annotation_count += non_empty_len(chunk.get("annotations"));
        self.image_count += non_empty_len(chunk.get("images"));
        self.client_tool_call_count += non_empty_len(chunk.get("client_tool_calls"));
    }
    /// `isStreamEmpty(tracker)`.
    pub fn is_empty(&self) -> bool {
        !(self.has_text || self.has_reasoning || self.tool_call_count > 0 || self.annotation_count > 0 || self.image_count > 0 || self.client_tool_call_count > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn usage_extraction_per_dialect() {
        let chat = json!({"usage": {"prompt_tokens": 100, "completion_tokens": 50, "prompt_tokens_details": {"cached_tokens": 30}, "cached_tokens": 10, "completion_tokens_details": {"reasoning_tokens": 7}}});
        let u = extract_usage_from_reconstructed(Some(&chat), "chat").unwrap();
        assert_eq!((u.input_tokens, u.cached_tokens, u.output_tokens, u.reasoning_tokens), (70.0, 30.0, 50.0, 7.0));
        let anth = json!({"usage": {"input_tokens": 10, "cache_read_input_tokens": 5, "output_tokens": 3}});
        assert_eq!(extract_usage_from_reconstructed(Some(&anth), "messages").unwrap().cached_tokens, 5.0);
        let gem = json!({"usageMetadata": {"promptTokenCount": 20, "cachedContentTokenCount": 4, "candidatesTokenCount": 2}});
        assert_eq!(extract_usage_from_reconstructed(Some(&gem), "gemini").unwrap().input_tokens, 16.0);
        assert!(extract_usage_from_reconstructed(Some(&json!({"id": "x"})), "chat").is_none());
        assert!(extract_usage_from_reconstructed(None, "chat").is_none());
        assert!(extract_usage_from_reconstructed(Some(&chat), "images").is_none());
    }

    #[test]
    fn metadata_extraction() {
        let m = extract_response_metadata(Some(&json!({"choices": [{"message": {"tool_calls": [{"id": "a"}, {"id": "b"}]}, "finish_reason": "stop"}]})), "chat", "chat");
        assert_eq!(m, ResponseMetadata { tool_calls_count: Some(2.0), finish_reason: Some(json!("tool_calls")) });
        let mixed = json!({"choices": [{"finish_reason": "STOP"}], "candidates": [{"content": {"parts": [{"functionCall": {"name": "f"}}]}}]});
        assert_eq!(extract_response_metadata(Some(&mixed), "chat", "messages").finish_reason, Some(json!("tool_use")));
        let gem = json!({"candidates": [{"content": {"parts": [{"functionCall": {}}]}, "finishReason": "STOP"}]});
        assert_eq!(extract_response_metadata(Some(&gem), "gemini", "chat"), ResponseMetadata { tool_calls_count: Some(1.0), finish_reason: Some(json!("tool_calls")) });
        let msgs = json!({"content": [{"type": "tool_use"}, {"type": "text"}], "stop_reason": "tool_use"});
        assert_eq!(extract_response_metadata(Some(&msgs), "messages", "messages"), ResponseMetadata { tool_calls_count: Some(1.0), finish_reason: Some(json!("tool_use")) });
        let generic = json!({"tool_calls": [1, 2, 3], "finish_reason": "done"});
        assert_eq!(extract_response_metadata(Some(&generic), "weird", "chat"), ResponseMetadata { tool_calls_count: Some(3.0), finish_reason: Some(json!("done")) });
        for (status, reason, expected) in [("failed", None, "error"), ("incomplete", Some("max_output_tokens"), "length"), ("incomplete", Some("content_filter"), "content_filter"), ("incomplete", None, "length"), ("completed", None, "stop")] {
            let mut r = json!({"status": status, "output": []});
            if let Some(reason) = reason { r["incomplete_details"] = json!({"reason": reason}); }
            assert_eq!(extract_response_metadata(Some(&r), "responses", "responses").finish_reason, Some(json!(expected)));
        }
        let tools = json!({"status": "completed", "output": [{"type": "function_call"}, {"type": "custom_tool_call"}, {"type": "message"}]});
        assert_eq!(extract_response_metadata(Some(&tools), "responses", "responses"), ResponseMetadata { tool_calls_count: Some(2.0), finish_reason: Some(json!("tool_calls")) });
        assert_eq!(extract_response_metadata(Some(&json!({"output": []})), "responses", "chat").finish_reason, None);
        assert_eq!(extract_response_metadata(None, "chat", "chat"), ResponseMetadata { tool_calls_count: None, finish_reason: Some(Value::Null) });
    }

    #[test]
    fn service_tier_capture() {
        let t = extract_actual_service_tier(Some(&json!({"service_tier": "default"})), None, Some("chat"));
        assert_eq!(t, ServiceTierCapture { tier: Some("default".into()), raw: Some("default".into()) });
        let fast = json!({"usage": {"speed": "fast", "service_tier": "standard"}});
        assert_eq!(extract_actual_service_tier(Some(&fast), None, Some("messages")).tier.as_deref(), Some("priority"));
        let gem = extract_actual_service_tier(Some(&json!({"service_tier": "flex"})), Some("priority"), Some("gemini"));
        assert_eq!(gem.raw.as_deref(), Some("priority"));
        let created = json!({"type": "response.created", "response": {"status": "in_progress", "service_tier": "priority"}});
        assert_eq!(extract_actual_service_tier(Some(&created), None, Some("responses")).raw, None);
        let done = json!({"status": "completed", "service_tier": "flex"});
        assert_eq!(extract_actual_service_tier(Some(&done), None, Some("responses")).tier.as_deref(), Some("flex"));
        let bedrock = json!({"metadata": {"serviceTier": {"type": "reserved"}}});
        assert_eq!(extract_actual_service_tier(Some(&bedrock), None, Some("chat")).tier.as_deref(), Some("reserved"));
        assert_eq!(normalize_service_tier(Some(&json!(" Custom "))), Some("Custom".into()));
    }

    #[test]
    fn visibility_tracker() {
        let mut t = StreamVisibilityTracker::default();
        t.observe(&json!({"delta": {"content": "   "}}));
        t.observe(&json!({"delta": {"role": "assistant"}, "finish_reason": "stop"}));
        assert!(t.is_empty());
        t.observe(&json!({"delta": {"tool_calls": [{"id": "x"}]}}));
        assert!(!t.is_empty());
    }
}
