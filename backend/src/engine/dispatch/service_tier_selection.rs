//! `services/dispatch/service-tier-selection.ts`.
use super::util::{base_type, del, js_trim};
use crate::engine::js::*;
use serde_json::{json, Value};

/// `normalizeServiceTier` (canonical vocabulary).
pub fn normalize_service_tier(raw: Option<&str>) -> Option<&'static str> {
    match js_trim(raw?).to_lowercase().as_str() {
        "auto" => Some("auto"),
        "default" | "on_demand" | "standard_only" | "standard" => Some("standard"),
        "flex" => Some("flex"),
        "fast" | "priority" => Some("priority"),
        "ultrafast" => Some("ultrafast"),
        _ => None,
    }
}

/// `inferServiceTierFormat(api)`.
pub fn infer_service_tier_format(api: Option<&str>) -> Option<&'static str> {
    match api? {
        "anthropic-messages" => Some("anthropic-speed"),
        "openai-completions"
        | "openai-responses"
        | "openai-codex-responses"
        | "azure-openai-responses" => Some("service-tier"),
        _ => None,
    }
}

fn fallbacks(canonical: &str) -> &'static [&'static str] {
    match canonical {
        "ultrafast" => &["ultrafast", "priority", "standard"],
        "priority" => &["priority", "standard"],
        "flex" => &["flex", "standard"],
        "standard" => &["standard"],
        _ => &["auto"],
    }
}

/// `resolveServiceTier(intent, map, format)`: outer `None` = no intent;
/// inner `None` = omit the field (native default).
pub fn resolve_service_tier(
    intent: Option<&str>,
    map: Option<&Value>,
    format: &str,
) -> Option<Option<String>> {
    let intent = intent?;
    let Some(canonical) = normalize_service_tier(Some(intent)) else {
        return Some(if format == "service-tier" {
            Some(intent.to_owned())
        } else {
            None
        });
    };
    for tier in fallbacks(canonical) {
        if let Some(native) = map
            .and_then(|m| get_str(m, &[tier]))
            .filter(|s| !s.is_empty())
        {
            if format == "anthropic-speed" {
                return Some(match native {
                    "fast" => Some("fast".into()),
                    "standard" => Some("standard".into()),
                    _ => None,
                });
            }
            return Some(Some(native.to_owned()));
        }
    }
    Some(None)
}

/// Target base types whose bodies carry an OpenAI-style `service_tier`.
pub const SERVICE_TIER_API_TYPES: [&str; 2] = ["chat", "responses"];

/// `hasExplicitTier(value)`.
pub fn has_explicit_tier(value: Option<&Value>) -> bool {
    let Some(v) = value.filter(|v| v.is_object()) else {
        return false;
    };
    get_str(v, &["speed"]).is_some_and(|s| !s.is_empty())
        || get_str(v, &["service_tier"]).is_some_and(|s| !s.is_empty())
}

/// `getDefaultServiceTier(request)`.
pub fn get_default_service_tier(request: &Value) -> Option<&str> {
    get_str(
        request,
        &["metadata", "plexus_metadata", "defaultServiceTier"],
    )
    .filter(|s| !s.is_empty())
}

/// `legacyServiceTierWireValue`.
pub fn legacy_service_tier_wire_value(tier: &str) -> String {
    match js_trim(tier).to_lowercase().as_str() {
        "standard" => "default".into(),
        "fast" | "ultrafast" => "priority".into(),
        _ => tier.to_owned(),
    }
}

fn with_service_tier(payload: Value, tier: String) -> Value {
    let mut map = payload.as_object().cloned().unwrap_or_default();
    map.insert("service_tier".into(), json!(tier));
    Value::Object(map)
}

/// `applyServiceTierSelection(payload, request, targetApiType)`.
pub fn apply_service_tier_selection(
    payload: Value,
    request: &Value,
    target_api_type: &str,
) -> Value {
    let base = base_type(target_api_type);
    let supported = SERVICE_TIER_API_TYPES.contains(&base.as_str());
    if let Some(tier) = request.get("serviceTier").filter(|t| truthy(Some(t))) {
        if !supported {
            return payload;
        }
        return with_service_tier(
            payload,
            legacy_service_tier_wire_value(&to_string(Some(tier))),
        );
    }
    if has_explicit_tier(Some(&payload)) || has_explicit_tier(request.get("originalBody")) {
        return payload;
    }
    let Some(default_tier) = get_default_service_tier(request) else {
        return payload;
    };
    if !supported {
        return payload;
    }
    let wire = legacy_service_tier_wire_value(default_tier);
    with_service_tier(payload, wire)
}

/// `stripKeyDefaultTier(payload, request)`.
pub fn strip_key_default_tier(payload: Value, request: &Value) -> Value {
    if !payload.is_object() {
        return payload;
    }
    if get_str(request, &["serviceTier"]).is_some_and(|s| !s.is_empty()) {
        return payload;
    }
    if has_explicit_tier(request.get("originalBody")) {
        return payload;
    }
    let Some(key_default) = get_default_service_tier(request) else {
        return payload;
    };
    let Some(current) = get_str(&payload, &["service_tier"]).filter(|s| !s.is_empty()) else {
        return payload;
    };
    if js_trim(current).to_lowercase() != legacy_service_tier_wire_value(key_default).to_lowercase()
    {
        return payload;
    }
    let mut map = payload.as_object().cloned().unwrap_or_default();
    del(&mut map, "service_tier");
    Value::Object(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve() {
        let map = json!({"standard": "default", "priority": "priority", "flex": null});
        assert_eq!(resolve_service_tier(None, Some(&map), "service-tier"), None);
        assert_eq!(
            resolve_service_tier(Some("fast"), Some(&map), "service-tier"),
            Some(Some("priority".into()))
        );
        assert_eq!(
            resolve_service_tier(Some("flex"), Some(&map), "service-tier"),
            Some(Some("default".into()))
        );
        assert_eq!(
            resolve_service_tier(Some("ultrafast"), Some(&json!({})), "service-tier"),
            Some(None)
        );
        assert_eq!(
            resolve_service_tier(Some("scale"), None, "service-tier"),
            Some(Some("scale".into()))
        );
        assert_eq!(
            resolve_service_tier(Some("scale"), None, "anthropic-speed"),
            Some(None)
        );
        let speed = json!({"priority": "fast", "auto": "auto"});
        assert_eq!(
            resolve_service_tier(Some("priority"), Some(&speed), "anthropic-speed"),
            Some(Some("fast".into()))
        );
        assert_eq!(
            resolve_service_tier(Some("auto"), Some(&speed), "anthropic-speed"),
            Some(None)
        );
    }

    #[test]
    fn selection() {
        let req = json!({"serviceTier": "ultrafast"});
        assert_eq!(
            apply_service_tier_selection(json!({"model": "m"}), &req, "chat"),
            json!({"model": "m", "service_tier": "priority"})
        );
        assert_eq!(
            apply_service_tier_selection(json!({"model": "m"}), &req, "messages"),
            json!({"model": "m"})
        );
        let req = json!({"metadata": {"plexus_metadata": {"defaultServiceTier": "standard"}}});
        assert_eq!(
            apply_service_tier_selection(json!({}), &req, "responses:lite"),
            json!({"service_tier": "default"})
        );
        assert_eq!(
            apply_service_tier_selection(json!({"speed": "fast"}), &req, "chat"),
            json!({"speed": "fast"})
        );
        let req2 = json!({"metadata": {"plexus_metadata": {"defaultServiceTier": "standard"}}, "originalBody": {"service_tier": "flex"}});
        assert_eq!(
            apply_service_tier_selection(json!({}), &req2, "chat"),
            json!({})
        );
        assert_eq!(
            strip_key_default_tier(json!({"service_tier": "default", "a": 1}), &req),
            json!({"a": 1})
        );
        assert_eq!(
            strip_key_default_tier(json!({"service_tier": "flex"}), &req),
            json!({"service_tier": "flex"})
        );
        assert_eq!(
            strip_key_default_tier(json!({"service_tier": "default"}), &req2),
            json!({"service_tier": "default"})
        );
    }
}
