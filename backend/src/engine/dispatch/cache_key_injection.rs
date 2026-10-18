//! `services/dispatch/cache-key-injection.ts` (+ `@plexus/shared`
//! `isBodyCacheKeyInjectionField` / `getDefaultCacheKeyInjection`).
use super::util::{base_type, del, sha256_hex, Headers};
use crate::engine::js::*;
use crate::engine::routing::sticky_session::StickySessionManager;
use crate::engine::routing::RouteResult;
use serde_json::{json, Value};

/// `isBodyCacheKeyInjectionField(value)`.
pub fn is_body_cache_key_injection_field(value: &str) -> bool {
    value == "prompt_cache_key"
}

/// `getDefaultCacheKeyInjection(oauthProvider)`.
pub fn default_cache_key_injection(oauth_provider: Option<&str>) -> Option<&'static str> {
    (oauth_provider == Some("meta")).then_some("prompt_cache_key")
}

/// `resolveCacheKeyInjection(config)`.
pub fn resolve_cache_key_injection(config: Option<&Value>) -> Option<String> {
    let explicit = config.and_then(|c| c.get("cache_key_injection"));
    if explicit.and_then(Value::as_str) == Some("off") {
        return None;
    }
    if truthy(explicit) {
        return Some(to_string(explicit));
    }
    default_cache_key_injection(config.and_then(|c| get_str(c, &["oauth_provider"])))
        .map(str::to_owned)
}

/// `deriveCacheInjectionKey(request)`.
pub fn derive_cache_injection_key(request: &Value) -> Option<String> {
    let routing = request.get("cacheRoutingHeaders");
    let pick = |k: &str| routing.and_then(|r| r.get(k)).filter(|v| !v.is_null());
    let session = pick("session_id")
        .or_else(|| pick("x-session-affinity"))
        .or_else(|| pick("x-session-id"))
        .or_else(|| pick("x-client-request-id"))
        .or_else(|| pick("x-multi-turn-session-id"))
        .or_else(|| request.get("prompt_cache_key").filter(|v| !v.is_null()));
    if truthy(session) {
        return Some(to_string(session));
    }
    let mut without_chain = request.as_object().cloned().unwrap_or_default();
    del(&mut without_chain, "previousResponseId");
    StickySessionManager::compute_session_key(&Value::Object(without_chain))
}

/// `applyBodyCacheKeyInjection(payload, route, request, targetApiType)`.
pub fn apply_body_cache_key_injection(
    payload: Value,
    route: &RouteResult,
    request: &Value,
    target_api_type: &str,
) -> Value {
    let Some(field) = resolve_cache_key_injection(Some(&route.config)) else {
        return payload;
    };
    if !is_body_cache_key_injection_field(&field) {
        return payload;
    }
    if !matches!(base_type(target_api_type).as_str(), "chat" | "responses") {
        return payload;
    }
    let Some(value) = derive_cache_injection_key(request) else {
        return payload;
    };
    let mut map = payload.as_object().cloned().unwrap_or_default();
    map.insert(field, json!(value));
    Value::Object(map)
}

fn header_safe(value: &str) -> String {
    let ok = !value.is_empty()
        && value.len() <= 256
        && value.bytes().all(|b| (0x21..=0x7e).contains(&b));
    if ok {
        value.to_owned()
    } else {
        sha256_hex(value.as_bytes())
    }
}

/// `applyHeaderCacheKeyInjection(headers, route, request)`: only when the
/// header is not already present (case-insensitive).
pub fn apply_header_cache_key_injection(
    headers: Headers,
    route: &RouteResult,
    request: &Value,
) -> Headers {
    let Some(field) = resolve_cache_key_injection(Some(&route.config)) else {
        return headers;
    };
    if is_body_cache_key_injection_field(&field) {
        return headers;
    }
    let wire = if field == "session_id" {
        "session-id".to_owned()
    } else {
        field
    };
    let lower = wire.to_lowercase();
    if headers.keys().any(|k| k.to_lowercase() == lower) {
        return headers;
    }
    let Some(value) = derive_cache_injection_key(request) else {
        return headers;
    };
    let mut out = headers;
    out.insert(wire, json!(header_safe(&value)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(config: Value) -> RouteResult {
        RouteResult {
            provider: "p".into(),
            model: "m".into(),
            config,
            model_config: None,
            incoming_model_alias: None,
            canonical_model: None,
        }
    }

    #[test]
    fn resolution() {
        assert_eq!(
            resolve_cache_key_injection(Some(
                &json!({"cache_key_injection": "off", "oauth_provider": "meta"})
            )),
            None
        );
        assert_eq!(
            resolve_cache_key_injection(Some(&json!({"oauth_provider": "meta"}))).as_deref(),
            Some("prompt_cache_key")
        );
        assert_eq!(
            resolve_cache_key_injection(Some(&json!({"cache_key_injection": "x-session-id"})))
                .as_deref(),
            Some("x-session-id")
        );
        assert_eq!(resolve_cache_key_injection(Some(&json!({}))), None);
    }

    #[test]
    fn derive_key_precedence() {
        let req = json!({"cacheRoutingHeaders": {"x-session-id": "s2", "x-client-request-id": "c"}, "prompt_cache_key": "pk"});
        assert_eq!(derive_cache_injection_key(&req).as_deref(), Some("s2"));
        assert_eq!(
            derive_cache_injection_key(&json!({"prompt_cache_key": "pk"})).as_deref(),
            Some("pk")
        );
        let conv = json!({"previousResponseId": "resp_1", "messages": [{"role": "user", "content": "a"}, {"role": "assistant", "content": "b"}]});
        let key = derive_cache_injection_key(&conv).unwrap();
        assert!(key.starts_with("m:"));
        assert_eq!(
            derive_cache_injection_key(&json!({"messages": [{"role": "user", "content": "a"}]})),
            None
        );
    }

    #[test]
    fn body_and_header_injection() {
        let req = json!({"cacheRoutingHeaders": {"session_id": "abc"}});
        let r = route(json!({"cache_key_injection": "prompt_cache_key"}));
        assert_eq!(
            apply_body_cache_key_injection(json!({"model": "m"}), &r, &req, "responses"),
            json!({"model": "m", "prompt_cache_key": "abc"})
        );
        assert_eq!(
            apply_body_cache_key_injection(json!({"model": "m"}), &r, &req, "messages"),
            json!({"model": "m"})
        );
        let r = route(json!({"cache_key_injection": "session_id"}));
        let h = apply_header_cache_key_injection(Headers::new(), &r, &req);
        assert_eq!(Value::Object(h), json!({"session-id": "abc"}));
        let mut existing = Headers::new();
        existing.insert("Session-Id".into(), json!("mine"));
        assert_eq!(
            Value::Object(apply_header_cache_key_injection(existing, &r, &req)),
            json!({"Session-Id": "mine"})
        );
        let req = json!({"cacheRoutingHeaders": {"session_id": "has space"}});
        let h = apply_header_cache_key_injection(Headers::new(), &r, &req);
        assert_eq!(h["session-id"], json!(sha256_hex(b"has space")));
    }
}
