//! `providers/provider-api-selection.ts`: per-target API type selection and
//! base URL resolution.
use super::{js_keys, RouteResult};
use crate::engine::config::api_format::{
    base_type, is_decisions_target, is_image_target, is_subtype, normalize_access_list,
};
use crate::engine::config::{get_provider_types, is_oauth_placeholder_url};
use crate::engine::js::{get_str, to_string, truthy};
use crate::engine::transformer::{EngineError, EngineResult};
use serde_json::{Map, Value};
use std::sync::OnceLock;

/// `API_TYPE_ALIASES`: provider-level URL keys to try for request-level types.
pub fn api_type_aliases(type_key: &str) -> Option<&'static [&'static str]> {
    match type_key {
        "completions" => Some(&["completions", "chat", "gemini"]),
        "embeddings" | "transcriptions" | "speech" | "openai-images" => Some(&["chat", "gemini"]),
        _ => None,
    }
}

/// `stripTrailingApiVersion`: `url.replace(/\/(v\d+beta\d*)$/i, '')`.
fn strip_trailing_api_version(url: &str) -> String {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re =
        RE.get_or_init(|| regex::Regex::new(r"(?i)/(v[0-9]+beta[0-9]*)$").expect("valid regex"));
    re.replace(url, "").into_owned()
}

/// `getApiMetadata(metadata)`: metadata without `plexus_metadata`.
pub fn get_api_metadata(metadata: &Value) -> Value {
    let mut out: Map<String, Value> = metadata.as_object().cloned().unwrap_or_default();
    out.shift_remove("plexus_metadata");
    Value::Object(out)
}

/// Result of `selectTargetApiType`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiTypeSelection {
    pub target_api_type: String,
    pub selection_reason: String,
}

/// `selectTargetApiType(route, incomingApiType?)`.
pub fn select_target_api_type(
    route: &RouteResult,
    incoming_api_type: Option<&str>,
) -> EngineResult<ApiTypeSelection> {
    let provider_types = get_provider_types(&route.config);
    let model_access = route
        .model_config
        .as_ref()
        .and_then(|m| m.get("access_via"))
        .filter(|v| truthy(Some(v)));
    let is_decisions_typed = route
        .model_config
        .as_ref()
        .and_then(|m| get_str(m, &["type"]))
        == Some("decisions");
    let normalized_model_types = if model_access.is_some() {
        normalize_access_list(model_access)
    } else {
        vec![]
    };
    let available: Vec<String> = if is_decisions_typed {
        if normalized_model_types
            .iter()
            .any(|t| is_decisions_target(t))
        {
            normalized_model_types
        } else {
            provider_types
        }
    } else if model_access
        .and_then(Value::as_array)
        .is_some_and(|a| !a.is_empty())
    {
        normalized_model_types
    } else {
        provider_types
    };

    if incoming_api_type.is_some_and(|i| i.to_lowercase() == "decisions") {
        if let Some(m) = available.iter().find(|t| is_decisions_target(t)) {
            return Ok(ApiTypeSelection {
                target_api_type: m.clone(),
                selection_reason: "matched incoming request type 'decisions'".into(),
            });
        }
    }

    let Some(mut target) = available.first().cloned() else {
        return Err(EngineError::new(format!(
            "No available API type found for provider '{}' and model '{}'. Check configuration.",
            route.provider, route.model
        )));
    };
    let mut reason = "default (first available)".to_owned();

    if let Some(incoming) = incoming_api_type.filter(|i| !i.is_empty()) {
        let incoming = incoming.to_lowercase();
        if let Some(m) = available
            .iter()
            .find(|t| t.to_lowercase() == incoming || (incoming == "images" && is_image_target(t)))
        {
            target = m.clone();
            reason = format!("matched incoming request type '{incoming}'");
        } else if is_subtype(Some(&incoming)) {
            let base = base_type(&incoming);
            if let Some(m) = available.iter().find(|t| t.to_lowercase() == base) {
                target = m.clone();
                reason = format!("incoming API subtype '{incoming}' not directly supported, fell back to base type '{base}'");
            } else {
                reason = format!(
                    "incoming API subtype '{incoming}' not supported, defaulted to '{target}'"
                );
            }
        } else {
            reason = format!("incoming type '{incoming}' not supported, defaulted to '{target}'");
        }
    }
    Ok(ApiTypeSelection {
        target_api_type: target,
        selection_reason: reason,
    })
}

/// `resolveImageProviderBaseUrl(route, targetApiType)`: OpenAI-compatible
/// targets prefer a configured `openai-images` URL.
pub fn resolve_image_provider_base_url(
    route: &RouteResult,
    target_api_type: &str,
) -> EngineResult<String> {
    let base = route.config.get("api_base_url");
    if !truthy(base) || base.is_some_and(Value::is_string) {
        return resolve_provider_base_url(route, target_api_type);
    }
    let openai_compatible =
        ["chat", "completions", "openai"].contains(&base_type(target_api_type).as_str());
    if openai_compatible && truthy(base.and_then(|b| b.get("openai-images"))) {
        return resolve_provider_base_url(route, "openai-images");
    }
    resolve_provider_base_url(route, target_api_type)
}

/// `resolveConfiguredBaseUrl(route, targetApiType)`: the configured URL
/// (`oauth://` placeholders verbatim), trailing slash and `/v1beta`-style
/// version stripped. For predicates only; dispatch uses
/// `resolve_provider_base_url`.
pub fn resolve_configured_base_url(
    route: &RouteResult,
    target_api_type: &str,
) -> EngineResult<String> {
    let base = route.config.get("api_base_url");
    let raw: String = if !truthy(base) || base.is_some_and(Value::is_string) {
        let raw = base.and_then(Value::as_str).unwrap_or("").to_owned();
        if raw.is_empty() {
            return Err(EngineError::new(format!(
                "No base URL configured for api type '{target_api_type}'."
            )));
        }
        raw
    } else {
        let empty = Map::new();
        let url_map = base.and_then(Value::as_object).unwrap_or(&empty);
        let type_key = target_api_type.to_lowercase();
        let pick = |k: &str| url_map.get(k).filter(|v| truthy(Some(v)));
        let no_default = || {
            EngineError::new(format!(
                "No base URL configured for api type '{target_api_type}' and no default found."
            ))
        };
        if let Some(u) = pick(&type_key).or_else(|| pick(&base_type(&type_key))) {
            to_string(Some(u))
        } else if let Some(u) = pick("default") {
            to_string(Some(u))
        } else if let Some(u) =
            api_type_aliases(&type_key).and_then(|aliases| aliases.iter().find_map(|a| pick(a)))
        {
            to_string(Some(u))
        } else {
            let first = js_keys(url_map).into_iter().next().ok_or_else(no_default)?;
            to_string(Some(pick(first).ok_or_else(no_default)?))
        }
    };

    if is_oauth_placeholder_url(&raw) {
        return Ok(raw);
    }
    let trimmed = raw.strip_suffix('/').unwrap_or(&raw);
    Ok(strip_trailing_api_version(trimmed))
}

/// `resolveProviderBaseUrl(route, targetApiType)`: refuses `oauth://`.
pub fn resolve_provider_base_url(
    route: &RouteResult,
    target_api_type: &str,
) -> EngineResult<String> {
    let url = resolve_configured_base_url(route, target_api_type)?;
    if is_oauth_placeholder_url(&url) {
        return Err(EngineError::new(format!(
            "Provider '{}' resolved the placeholder base URL 'oauth://' for api type '{target_api_type}'. OAuth providers must be dispatched through the OAuth path, which resolves the real upstream URL.",
            route.provider
        )));
    }
    Ok(url)
}

/// `applyGeminiThinkingConfig(route, targetApiType, payload)`: for Gemini base
/// URLs, strips top-level `reasoning` and (when `geminiThinkingEnabled`)
/// maps `reasoning.effort` to `generationConfig.thinkingConfig.thinkingLevel`.
pub fn apply_gemini_thinking_config(
    route: &RouteResult,
    target_api_type: &str,
    payload: Value,
) -> EngineResult<Value> {
    let base = resolve_configured_base_url(route, target_api_type)?.to_lowercase();
    let is_gemini = base.contains("generativelanguage.googleapis.com");
    let enabled = route.config.get("geminiThinkingEnabled") == Some(&Value::Bool(true));
    if !is_gemini || !truthy(payload.get("reasoning")) {
        return Ok(payload);
    }

    let mut result = payload.as_object().cloned().unwrap_or_default();
    let reasoning = result.shift_remove("reasoning");
    if enabled {
        let effort = reasoning.as_ref().and_then(|r| r.get("effort"));
        if truthy(effort)
            && effort.and_then(Value::as_str) != Some("none")
            && effort.and_then(Value::as_str) != Some("minimal")
        {
            let level = match effort.and_then(Value::as_str) {
                Some("low") => "low",
                Some("medium") => "medium",
                _ => "high",
            };
            let mut gen = payload
                .get("generationConfig")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            gen.insert(
                "thinkingConfig".into(),
                serde_json::json!({"thinkingLevel": level}),
            );
            result.insert("generationConfig".into(), Value::Object(gen));
        }
    }
    Ok(Value::Object(result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn route(config: Value, model_config: Option<Value>) -> RouteResult {
        RouteResult {
            provider: "codex".into(),
            model: "gpt-image-1".into(),
            config,
            model_config,
            incoming_model_alias: None,
            canonical_model: None,
        }
    }

    #[test]
    fn selects_image_targets() {
        let r = select_target_api_type(
            &route(
                json!({"api_base_url": "oauth://", "api_key": "oauth"}),
                Some(json!({"access_via": ["codex-images"], "type": "image"})),
            ),
            Some("images"),
        )
        .unwrap();
        assert_eq!(
            r,
            ApiTypeSelection {
                target_api_type: "codex-images".into(),
                selection_reason: "matched incoming request type 'images'".into()
            }
        );
        for api in ["chat", "gemini", "openai-images", "openrouter-images"] {
            let r = select_target_api_type(
                &route(
                    json!({"api_base_url": "https://provider.example/v1"}),
                    Some(json!({"access_via": [api]})),
                ),
                Some("images"),
            )
            .unwrap();
            assert_eq!(r.target_api_type, api);
        }
        let r = select_target_api_type(
            &route(
                json!({"api_base_url": "https://provider.example/v1"}),
                Some(json!({"access_via": ["messages"]})),
            ),
            Some("images"),
        )
        .unwrap();
        assert_eq!(r.target_api_type, "messages");
        assert_eq!(
            r.selection_reason,
            "incoming type 'images' not supported, defaulted to 'messages'"
        );
    }

    #[test]
    fn subtype_and_decisions_selection() {
        let cfg = json!({"api_base_url": {"chat": "https://x/v1", "responses": "https://x/v1"}});
        let r = select_target_api_type(&route(cfg.clone(), None), Some("responses:lite")).unwrap();
        assert_eq!(r.target_api_type, "responses");
        assert_eq!(r.selection_reason, "incoming API subtype 'responses:lite' not directly supported, fell back to base type 'responses'");
        let r = select_target_api_type(
            &route(json!({"api_base_url": "https://x/v1"}), None),
            Some("responses:lite"),
        )
        .unwrap();
        assert_eq!(
            r.selection_reason,
            "incoming API subtype 'responses:lite' not supported, defaulted to 'chat'"
        );
        let r = select_target_api_type(&route(cfg, None), None).unwrap();
        assert_eq!(
            r,
            ApiTypeSelection {
                target_api_type: "chat".into(),
                selection_reason: "default (first available)".into()
            }
        );
        // Decisions-typed model with stale chat access_via still finds the provider's systemone URL.
        let cfg = json!({"api_base_url": {"chat": "https://or/v1", "systemone": "https://or/v1"}});
        let r = select_target_api_type(
            &route(
                cfg,
                Some(json!({"type": "decisions", "access_via": ["chat"]})),
            ),
            Some("decisions"),
        )
        .unwrap();
        assert_eq!(
            r,
            ApiTypeSelection {
                target_api_type: "systemone".into(),
                selection_reason: "matched incoming request type 'decisions'".into()
            }
        );
        let err = select_target_api_type(&route(json!({"api_base_url": {}}), None), Some("chat"))
            .unwrap_err();
        assert_eq!(err.message, "No available API type found for provider 'codex' and model 'gpt-image-1'. Check configuration.");
    }

    #[test]
    fn base_urls() {
        assert_eq!(
            resolve_provider_base_url(
                &route(
                    json!({"api_base_url": "https://provider.example/v1/"}),
                    None
                ),
                "chat"
            )
            .unwrap(),
            "https://provider.example/v1"
        );
        assert_eq!(
            resolve_provider_base_url(
                &route(
                    json!({"api_base_url": "https://generativelanguage.googleapis.com/v1beta/"}),
                    None
                ),
                "gemini"
            )
            .unwrap(),
            "https://generativelanguage.googleapis.com"
        );
        assert_eq!(
            resolve_provider_base_url(
                &route(json!({"api_base_url": "https://g/V1BETA2"}), None),
                "gemini"
            )
            .unwrap(),
            "https://g"
        );
        let err = resolve_provider_base_url(
            &route(json!({"api_base_url": "oauth://"}), None),
            "codex-images",
        )
        .unwrap_err()
        .message;
        assert!(
            err.contains("oauth://")
                && err.contains("Provider 'codex'")
                && err.contains("'codex-images'")
        );
        assert!(resolve_provider_base_url(
            &route(json!({"api_base_url": {"codex-images": "oauth://"}}), None),
            "codex-images"
        )
        .unwrap_err()
        .message
        .contains("oauth://"));
        assert_eq!(
            resolve_provider_base_url(&route(json!({}), None), "chat")
                .unwrap_err()
                .message,
            "No base URL configured for api type 'chat'."
        );

        let map = json!({"api_base_url": {"messages": "https://a/", "gemini": "https://g/v1beta", "default": "https://d"}});
        assert_eq!(
            resolve_provider_base_url(&route(map.clone(), None), "messages").unwrap(),
            "https://a"
        );
        assert_eq!(
            resolve_provider_base_url(&route(map.clone(), None), "responses").unwrap(),
            "https://d"
        );
        let map = json!({"api_base_url": {"messages": "https://a", "gemini": "https://g/v1beta"}});
        assert_eq!(
            resolve_provider_base_url(&route(map.clone(), None), "embeddings").unwrap(),
            "https://g"
        );
        assert_eq!(
            resolve_provider_base_url(&route(map.clone(), None), "responses").unwrap(),
            "https://a"
        );
        let map = json!({"api_base_url": {"responses": "https://r"}});
        assert_eq!(
            resolve_provider_base_url(&route(map, None), "responses:lite").unwrap(),
            "https://r"
        );
        let map = json!({"api_base_url": {"messages": ""}});
        assert_eq!(
            resolve_provider_base_url(&route(map, None), "chat")
                .unwrap_err()
                .message,
            "No base URL configured for api type 'chat' and no default found."
        );

        let img = json!({"api_base_url": {"chat": "https://c", "openai-images": "https://i"}});
        assert_eq!(
            resolve_image_provider_base_url(&route(img.clone(), None), "chat").unwrap(),
            "https://i"
        );
        assert_eq!(
            resolve_image_provider_base_url(&route(img, None), "gemini").unwrap(),
            "https://c"
        );
    }

    #[test]
    fn gemini_thinking() {
        let payload = json!({"reasoning": {"effort": "high"}});
        assert_eq!(
            apply_gemini_thinking_config(
                &route(
                    json!({"api_base_url": "oauth://", "geminiThinkingEnabled": true}),
                    None
                ),
                "responses",
                payload.clone()
            )
            .unwrap(),
            payload
        );
        let gem = json!({"api_base_url": "https://generativelanguage.googleapis.com/v1beta", "geminiThinkingEnabled": true});
        let out = apply_gemini_thinking_config(&route(gem.clone(), None), "gemini", json!({"model": "x", "reasoning": {"effort": "high"}, "generationConfig": {"temperature": 1}})).unwrap();
        assert_eq!(
            out,
            json!({"model": "x", "generationConfig": {"temperature": 1, "thinkingConfig": {"thinkingLevel": "high"}}})
        );
        let out = apply_gemini_thinking_config(
            &route(gem.clone(), None),
            "gemini",
            json!({"reasoning": {"effort": "minimal"}}),
        )
        .unwrap();
        assert_eq!(out, json!({}));
        let out = apply_gemini_thinking_config(
            &route(gem, None),
            "gemini",
            json!({"reasoning": {"effort": "low"}}),
        )
        .unwrap();
        assert_eq!(
            out,
            json!({"generationConfig": {"thinkingConfig": {"thinkingLevel": "low"}}})
        );
        let off = json!({"api_base_url": "https://generativelanguage.googleapis.com/v1beta"});
        assert_eq!(
            apply_gemini_thinking_config(
                &route(off, None),
                "gemini",
                json!({"a": 1, "reasoning": {"effort": "low"}})
            )
            .unwrap(),
            json!({"a": 1})
        );
        assert_eq!(
            get_api_metadata(&json!({"plexus_metadata": {}, "user": "u"})),
            json!({"user": "u"})
        );
    }
}
