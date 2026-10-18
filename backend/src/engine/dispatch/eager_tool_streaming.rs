//! `services/dispatch/eager-tool-streaming.ts`.
use super::adapter_resolver::select_dispatch_urls;
use super::oauth_routes::{is_claude_masking_api_key_route, is_oauth_route};
use super::util::{base_type, hostname, js_trim};
use crate::engine::config::oauth_provider_of;
use crate::engine::js::*;
use crate::engine::routing::RouteResult;
use serde_json::{json, Value};

/// `sendsToAnthropicApi(route, targetApiType)`.
pub fn sends_to_anthropic_api(route: &RouteResult, target_api_type: &str) -> bool {
    if base_type(target_api_type) != "messages" {
        return false;
    }
    if is_claude_masking_api_key_route(route, target_api_type) {
        return true;
    }
    if is_oauth_route(route, target_api_type) {
        return oauth_provider_of(&route.provider, &route.config) == "anthropic";
    }
    select_dispatch_urls(route.config.get("api_base_url"), Some(target_api_type))
        .iter()
        .any(|u| {
            hostname(js_trim(u))
                .is_some_and(|h| h == "anthropic.com" || h.ends_with(".anthropic.com"))
        })
}

/// `applyEagerToolInputStreaming(payload, request, route, targetApiType, bypassTransformation)`.
pub fn apply_eager_tool_input_streaming(
    payload: Value,
    request: &Value,
    route: &RouteResult,
    target_api_type: &str,
    bypass_transformation: bool,
) -> Value {
    if bypass_transformation
        || payload.get("stream") != Some(&Value::Bool(true))
        || get_arr(&payload, &["tools"]).is_none()
        || get_str(request, &["incomingApiType"]).is_some_and(|t| t.to_lowercase() == "messages")
        || !sends_to_anthropic_api(route, target_api_type)
    {
        return payload;
    }
    let tools: Vec<Value> = payload["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|tool| {
            let ty = tool.get("type");
            let server_tool = truthy(ty) && ty.and_then(Value::as_str) != Some("custom");
            if !tool.is_object() || server_tool || tool.get("eager_input_streaming").is_some() {
                return tool.clone();
            }
            let mut t = tool.as_object().cloned().unwrap_or_default();
            t.insert("eager_input_streaming".into(), json!(true));
            Value::Object(t)
        })
        .collect();
    let mut map = payload.as_object().cloned().unwrap_or_default();
    map.insert("tools".into(), Value::Array(tools));
    Value::Object(map)
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
    fn eager_streaming() {
        let anthropic = route(json!({"api_base_url": "https://api.anthropic.com/v1"}));
        let req = json!({"incomingApiType": "chat"});
        let p = json!({"stream": true, "tools": [{"name": "a"}, {"type": "custom", "name": "b"}, {"type": "web_search_20250305"}, {"name": "c", "eager_input_streaming": false}]});
        let out = apply_eager_tool_input_streaming(p.clone(), &req, &anthropic, "messages", false);
        assert_eq!(
            out["tools"],
            json!([{"name": "a", "eager_input_streaming": true}, {"type": "custom", "name": "b", "eager_input_streaming": true}, {"type": "web_search_20250305"}, {"name": "c", "eager_input_streaming": false}])
        );
        assert_eq!(
            apply_eager_tool_input_streaming(p.clone(), &req, &anthropic, "messages", true),
            p
        );
        assert_eq!(
            apply_eager_tool_input_streaming(
                p.clone(),
                &json!({"incomingApiType": "Messages"}),
                &anthropic,
                "messages",
                false
            ),
            p
        );
        let proxy = route(json!({"api_base_url": "https://anthropic.com.evil.example/v1"}));
        assert_eq!(
            apply_eager_tool_input_streaming(p.clone(), &req, &proxy, "messages", false),
            p
        );
        let mut ns = p.clone();
        ns["stream"] = json!(false);
        assert_eq!(
            apply_eager_tool_input_streaming(ns.clone(), &req, &anthropic, "messages", false),
            ns
        );
        assert!(sends_to_anthropic_api(
            &route(json!({"api_base_url": "oauth://a", "oauth_provider": "anthropic"})),
            "messages"
        ));
    }
}
