//! `services/dispatch/service-tier-metadata.ts`: requested/actual tier
//! capture for usage logging.
use super::util::{base_type, js_trim};
use crate::engine::js::*;
use serde_json::Value;

const HEADER_VERTEX_REQUEST_TYPE: &str = "x-vertex-ai-llm-shared-request-type";
const HEADER_GEMINI_SERVICE_TIER: &str = "x-gemini-service-tier";

/// `ServiceTierCapture`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServiceTierCapture {
    pub tier: Option<String>,
    pub raw: Option<String>,
}

/// `normalizeServiceTier` (reporting vocabulary; unknown values verbatim).
pub fn normalize_reported_service_tier(raw: Option<&Value>) -> Option<String> {
    let value = match raw? {
        Value::Null => return None,
        Value::String(s) => s.clone(),
        other => to_string(Some(other)),
    };
    let trimmed = js_trim(&value);
    if trimmed.is_empty() {
        return None;
    }
    let lower = trimmed.to_lowercase();
    Some(match lower.as_str() {
        "on_demand" | "ondemand" | "standard" | "standard_only" | "default" => "default".into(),
        "fast" | "priority" | "on_demand_priority" => "priority".into(),
        "flex" | "on_demand_flex" => "flex".into(),
        "auto" | "performance" | "ultrafast" | "scale" | "reserved" | "deferred" => lower,
        _ => trimmed.to_owned(),
    })
}

fn read_string(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str)
        .filter(|s| !js_trim(s).is_empty())
        .map(str::to_owned)
}

fn read_header(headers: Option<&Value>, name: &str) -> Option<String> {
    let target = name.to_lowercase();
    for (key, value) in headers?.as_object()? {
        if key.to_lowercase() != target {
            continue;
        }
        return match value {
            Value::String(s) => (!js_trim(s).is_empty()).then(|| s.clone()),
            Value::Array(items) => items
                .iter()
                .filter_map(Value::as_str)
                .find(|s| !js_trim(s).is_empty())
                .map(str::to_owned),
            _ => None,
        };
    }
    None
}

fn capture(raw: Option<String>) -> ServiceTierCapture {
    ServiceTierCapture {
        tier: normalize_reported_service_tier(
            raw.as_ref().map(|s| Value::String(s.clone())).as_ref(),
        ),
        raw,
    }
}

fn tier_from_nested(v: Option<&Value>) -> Option<String> {
    match v {
        Some(Value::String(_)) => read_string(v),
        Some(Value::Object(o)) => read_string(o.get("type")),
        _ => None,
    }
}

fn find_requested_raw(
    payload: &Value,
    headers: Option<&Value>,
    api_base: Option<&str>,
) -> Option<String> {
    let empty = Value::Object(Obj::new());
    let body = if payload.is_object() || payload.is_array() {
        payload
    } else {
        &empty
    };
    if api_base == Some("messages") {
        if let Some(speed) = read_string(body.get("speed")) {
            if js_trim(&speed).to_lowercase() == "fast" {
                return Some(speed);
            }
        }
    }
    if let Some(raw) = tier_from_nested(body.get("serviceTier")) {
        return Some(raw);
    }
    if let Some(h) = read_header(headers, HEADER_VERTEX_REQUEST_TYPE) {
        return Some(h);
    }
    read_string(body.get("service_tier"))
}

/// `extractRequestedServiceTier(payload, headers, apiBaseType)`.
pub fn extract_requested_service_tier(
    payload: &Value,
    headers: Option<&Value>,
    api_type: Option<&str>,
) -> ServiceTierCapture {
    let base = api_type.map(base_type);
    capture(find_requested_raw(payload, headers, base.as_deref()))
}

fn unwrap_event_envelope(body: &Value) -> &Value {
    let Some(t) = body.get("type") else {
        return body;
    };
    if (t == "message_start" || t == "message_delta")
        && body.get("message").is_some_and(Value::is_object)
    {
        return unwrap_event_envelope(&body["message"]);
    }
    if t.as_str().is_some_and(|s| s.starts_with("response."))
        && body.get("response").is_some_and(Value::is_object)
    {
        return unwrap_event_envelope(&body["response"]);
    }
    body
}

fn find_actual_raw(raw_body: &Value, api_base: Option<&str>) -> Option<String> {
    let body = unwrap_event_envelope(raw_body);
    if !(body.is_object() || body.is_array()) {
        return None;
    }
    if api_base == Some("responses") {
        if let Some(status) = read_string(body.get("status")) {
            if !["completed", "failed", "incomplete"].contains(&status.to_lowercase().as_str()) {
                return None;
            }
        }
    }
    let usage = body.get("usage").filter(|u| u.is_object() || u.is_array());
    let usage_speed = read_string(usage.and_then(|u| u.get("speed")));
    if usage_speed
        .as_deref()
        .is_some_and(|s| js_trim(s).to_lowercase() == "fast")
    {
        return usage_speed;
    }
    if let Some(t) = read_string(usage.and_then(|u| u.get("service_tier"))) {
        return Some(t);
    }
    if usage_speed.is_some() {
        return usage_speed;
    }
    if let Some(t) = read_string(get(body, &["usageMetadata", "trafficType"])) {
        return Some(t);
    }
    for candidate in [
        body.get("serviceTier"),
        get(body, &["metadata", "serviceTier"]),
        usage.and_then(|u| u.get("serviceTier")),
    ] {
        if let Some(t) = tier_from_nested(candidate) {
            return Some(t);
        }
    }
    read_string(body.get("service_tier"))
}

/// `extractActualServiceTierFromBody(body, apiBaseType)`.
pub fn extract_actual_service_tier_from_body(
    body: &Value,
    api_type: Option<&str>,
) -> ServiceTierCapture {
    let base = api_type.map(base_type);
    capture(find_actual_raw(body, base.as_deref()))
}

/// `extractActualServiceTierFromHeaders(headers)`.
pub fn extract_actual_service_tier_from_headers(headers: Option<&Value>) -> ServiceTierCapture {
    capture(read_header(headers, HEADER_GEMINI_SERVICE_TIER))
}

/// `extractActualServiceTier(body, headers, apiBaseType)`.
pub fn extract_actual_service_tier(
    body: &Value,
    headers: Option<&Value>,
    api_type: Option<&str>,
) -> ServiceTierCapture {
    let base = api_type.map(base_type);
    let from_headers = extract_actual_service_tier_from_headers(headers);
    if base.as_deref() == Some("gemini") && from_headers.raw.is_some() {
        return from_headers;
    }
    match find_actual_raw(body, base.as_deref()) {
        Some(raw) => capture(Some(raw)),
        None => from_headers,
    }
}

/// `applyRequestedServiceTierFromError(usageRecord, error)`: returns
/// `(requestedServiceTier, requestedServiceTierRaw)` to copy, if any.
pub fn requested_service_tier_from_error(error: &Value) -> Option<(Value, Value)> {
    let rc = error.get("routingContext")?;
    let raw = rc
        .get("requestedServiceTierRaw")
        .filter(|v| truthy(Some(v)))?;
    Some((
        coalesce(rc.get("requestedServiceTier"), Some(&Value::Null)).unwrap_or(Value::Null),
        raw.clone(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn requested() {
        let c = extract_requested_service_tier(
            &json!({"speed": "fast", "service_tier": "auto"}),
            None,
            Some("messages"),
        );
        assert_eq!(
            c,
            ServiceTierCapture {
                tier: Some("priority".into()),
                raw: Some("fast".into())
            }
        );
        let c = extract_requested_service_tier(
            &json!({"speed": "fast", "service_tier": "auto"}),
            None,
            Some("chat"),
        );
        assert_eq!(c.raw.as_deref(), Some("auto"));
        let c =
            extract_requested_service_tier(&json!({"serviceTier": {"type": "flex"}}), None, None);
        assert_eq!(c.tier.as_deref(), Some("flex"));
        let h = json!({"X-Vertex-AI-LLM-Shared-Request-Type": "priority"});
        let c = extract_requested_service_tier(
            &json!({"service_tier": "flex"}),
            Some(&h),
            Some("gemini"),
        );
        assert_eq!(c.raw.as_deref(), Some("priority"));
        assert_eq!(
            extract_requested_service_tier(&json!({}), None, None),
            ServiceTierCapture::default()
        );
        assert_eq!(
            normalize_reported_service_tier(Some(&json!("Custom-X "))).as_deref(),
            Some("Custom-X")
        );
    }

    #[test]
    fn actual() {
        let ev = json!({"type": "response.in_progress", "response": {"status": "in_progress", "service_tier": "priority"}});
        assert_eq!(
            extract_actual_service_tier(&ev, None, Some("responses")).raw,
            None
        );
        let ev = json!({"type": "response.completed", "response": {"status": "completed", "service_tier": "default"}});
        assert_eq!(
            extract_actual_service_tier(&ev, None, Some("responses"))
                .tier
                .as_deref(),
            Some("default")
        );
        let ms = json!({"type": "message_start", "message": {"usage": {"speed": "fast", "service_tier": "standard"}}});
        assert_eq!(
            extract_actual_service_tier(&ms, None, Some("messages"))
                .tier
                .as_deref(),
            Some("priority")
        );
        let h = json!({"x-gemini-service-tier": "flex"});
        assert_eq!(
            extract_actual_service_tier(
                &json!({"service_tier": "standard"}),
                Some(&h),
                Some("gemini")
            )
            .raw
            .as_deref(),
            Some("flex")
        );
        assert_eq!(
            extract_actual_service_tier(
                &json!({"service_tier": "standard"}),
                Some(&h),
                Some("chat")
            )
            .raw
            .as_deref(),
            Some("standard")
        );
        assert_eq!(
            extract_actual_service_tier(
                &json!({"usageMetadata": {"trafficType": "ON_DEMAND"}}),
                None,
                None
            )
            .tier
            .as_deref(),
            Some("default")
        );
    }
}
