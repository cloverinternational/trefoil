//! Plexus `getConfig()` snapshot as JSON, plus `config.ts` helpers.
//!
//! `root` mirrors the TypeScript config cache built by
//! `ConfigService.doRebuild`: `providers` / `models` are the repository views
//! (`provider_view`, `alias_view`), `keys`, `failover`, `cooldown`, `timeout`,
//! `stall`, `backgroundExploration`, plus every flat (non-dotted) system
//! setting spread at the top level (e.g. `latencyExplorationRate`).
//! Engine code reads it exactly like TS: `get(&cfg.root, &["providers", p])`.
use super::js::{get, get_obj, get_str};
use serde_json::Value;

#[derive(Debug, Clone, Default)]
pub struct Config {
    pub root: Value,
}

impl Config {
    pub fn new(root: Value) -> Self { Self { root } }
    pub fn provider(&self, slug: &str) -> Option<&Value> { get(&self.root, &["providers", slug]) }
    pub fn alias(&self, slug: &str) -> Option<&Value> { get(&self.root, &["models", slug]) }
    pub fn providers(&self) -> Option<&serde_json::Map<String, Value>> { get_obj(&self.root, &["providers"]) }
    pub fn models(&self) -> Option<&serde_json::Map<String, Value>> { get_obj(&self.root, &["models"]) }
}

/// `isOAuthPlaceholderUrl`.
pub fn is_oauth_placeholder_url(value: &str) -> bool { value.trim().to_lowercase().starts_with("oauth://") }

/// `getProviderTypes(provider)`: inferred from a string URL, else the record's
/// non-empty keys in insertion order.
pub fn get_provider_types(provider: &Value) -> Vec<String> {
    match provider.get("api_base_url") {
        Some(Value::String(url)) => {
            if is_oauth_placeholder_url(url) { return vec!["oauth".into()]; }
            let lower = url.to_lowercase();
            if lower.contains("anthropic.com") { vec!["messages".into()] }
            else if lower.contains("generativelanguage.googleapis.com") { vec!["gemini".into()] }
            else { vec!["chat".into()] }
        }
        Some(Value::Object(map)) => map.iter().filter(|(_, v)| v.as_str().is_some_and(|s| !s.is_empty())).map(|(k, _)| k.clone()).collect(),
        // `typeof undefined !== 'string'` → treated as a (empty) record.
        _ => vec![],
    }
}

/// Model config for `providers[p].models[m]` when `models` is a record.
pub fn model_config<'a>(provider: &'a Value, model: &str) -> Option<&'a Value> {
    provider.get("models").and_then(Value::as_object).and_then(|m| m.get(model))
}

/// `utils/api-format.ts`.
pub mod api_format {
    use serde_json::Value;
    pub const IMAGE_TARGET_API_TYPES: [&str; 5] = ["chat", "gemini", "openai-images", "openrouter-images", "codex-images"];
    pub const DECISIONS_TARGET_API_TYPES: [&str; 3] = ["systemone", "openrouter-decisions", "typesafe-decisions"];

    /// `apiAccessToKey`.
    pub fn access_to_key(access: &Value) -> String {
        match access {
            Value::String(s) => s.trim().to_lowercase(),
            Value::Object(o) => {
                let kind = o.get("type").and_then(Value::as_str).unwrap_or_default().trim().to_lowercase();
                match o.get("subtype").and_then(Value::as_str).map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty()) {
                    Some(sub) => format!("{kind}:{sub}"),
                    None => kind,
                }
            }
            _ => String::new(),
        }
    }
    /// `normalizeApiAccessList`.
    pub fn normalize_access_list(access: Option<&Value>) -> Vec<String> {
        access.and_then(Value::as_array).map(|a| a.iter().map(access_to_key).filter(|k| !k.is_empty()).collect()).unwrap_or_default()
    }
    pub fn base_type(api: &str) -> String { api.trim().to_lowercase().split(':').next().unwrap_or_default().to_owned() }
    pub fn subtype(api: &str) -> Option<String> {
        let n = api.trim().to_lowercase();
        n.find(':').map(|i| n[i + 1..].to_owned()).filter(|s| !s.is_empty())
    }
    pub fn is_subtype(api: Option<&str>) -> bool { api.is_some_and(|a| !a.is_empty() && subtype(a).is_some()) }
    pub fn is_image_target(api: &str) -> bool { IMAGE_TARGET_API_TYPES.contains(&base_type(api).as_str()) }
    pub fn is_decisions_target(api: &str) -> bool { DECISIONS_TARGET_API_TYPES.contains(&base_type(api).as_str()) }
    /// `hasDecisionsProtocol`.
    pub fn has_decisions_protocol(model_access: Option<&Value>, provider_types: &[String]) -> bool {
        normalize_access_list(model_access).iter().chain(provider_types.iter()).any(|t| is_decisions_target(t))
    }
}

/// Convenience: `providers[p].oauth_provider || p`.
pub fn oauth_provider_of<'a>(route_provider: &'a str, provider: &'a Value) -> &'a str {
    get_str(provider, &["oauth_provider"]).filter(|s| !s.is_empty()).unwrap_or(route_provider)
}
