//! Route-classification predicates from `services/oauth/oauth-dispatcher.ts`
//! and `oauth-native-request.ts` (`isNativeOAuthProvider`) plus
//! `isNativeOAuthRoute` (request-payload-builder.ts). The OAuth dispatch
//! itself is not ported; these only decide whether a route is API-key.
use crate::engine::config::{is_oauth_placeholder_url, oauth_provider_of};
use crate::engine::routing::RouteResult;
use serde_json::Value;

/// `isOAuthRoute(route, targetApiType)`. (TS throws on a missing
/// `api_base_url`; treated as "not OAuth" here.)
pub fn is_oauth_route(route: &RouteResult, target_api_type: &str) -> bool {
    if target_api_type.to_lowercase() == "oauth" {
        return true;
    }
    match route.config.get("api_base_url") {
        Some(Value::String(url)) => is_oauth_placeholder_url(url),
        Some(Value::Object(map)) => map
            .values()
            .any(|v| v.as_str().is_some_and(is_oauth_placeholder_url)),
        _ => false,
    }
}

/// `isClaudeMaskingApiKeyRoute(route, targetApiType)`.
pub fn is_claude_masking_api_key_route(route: &RouteResult, target_api_type: &str) -> bool {
    !is_oauth_route(route, target_api_type)
        && target_api_type.to_lowercase() == "messages"
        && route.config.get("useClaudeMasking") == Some(&Value::Bool(true))
}

/// `isPiAiRoute(route, targetApiType)`.
pub fn is_pi_ai_route(route: &RouteResult, target_api_type: &str) -> bool {
    is_oauth_route(route, target_api_type)
        || is_claude_masking_api_key_route(route, target_api_type)
}

/// `isNativeOAuthProvider(provider)`.
pub fn is_native_oauth_provider(provider: &str) -> bool {
    matches!(
        provider,
        "anthropic" | "openai-codex" | "github-copilot" | "meta"
    )
}

/// `isNativeOAuthRoute(route, targetApiType)`.
pub fn is_native_oauth_route(route: &RouteResult, target_api_type: &str) -> bool {
    if is_claude_masking_api_key_route(route, target_api_type) {
        return true;
    }
    if !is_oauth_route(route, target_api_type) {
        return false;
    }
    is_native_oauth_provider(oauth_provider_of(&route.provider, &route.config))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn route(config: Value) -> RouteResult {
        RouteResult {
            provider: "anthropic".into(),
            model: "m".into(),
            config,
            model_config: None,
            incoming_model_alias: None,
            canonical_model: None,
        }
    }
    #[test]
    fn predicates() {
        let oauth = route(json!({"api_base_url": "oauth://anthropic"}));
        assert!(is_oauth_route(&oauth, "messages") && is_native_oauth_route(&oauth, "messages"));
        let mask = route(json!({"api_base_url": "https://x", "useClaudeMasking": true}));
        assert!(
            is_claude_masking_api_key_route(&mask, "messages")
                && !is_claude_masking_api_key_route(&mask, "chat")
        );
        let plain = route(json!({"api_base_url": {"chat": "https://x"}}));
        assert!(!is_pi_ai_route(&plain, "chat") && is_oauth_route(&plain, "OAuth"));
    }
}
