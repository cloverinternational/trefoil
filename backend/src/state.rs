use std::collections::HashMap;
use std::sync::Arc;
use std::sync::RwLock;

use serde_json::Value;

/// Configuration for a single limited (non-admin) API key.
///
/// Mirrors the fields Plexus's `Principal`/`api_keys` config exposes on
/// `GET /v0/management/auth/verify` for a `limited` principal.
///
/// Field set also covers what `KeyConfigSchema`
/// (`packages/backend/src/config.ts`) accepts/persists for
/// `/v0/management/keys/:name` CRUD (see `backend/src/keys.rs`).
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct KeyConfig {
    pub secret: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    #[serde(rename = "quotas", skip_serializing_if = "Vec::is_empty")]
    pub quota_names: Vec<String>,
    #[serde(rename = "allowedProviders", skip_serializing_if = "Vec::is_empty")]
    pub allowed_providers: Vec<String>,
    #[serde(rename = "allowedModels", skip_serializing_if = "Vec::is_empty")]
    pub allowed_models: Vec<String>,
    #[serde(rename = "excludedProviders", skip_serializing_if = "Vec::is_empty")]
    pub excluded_providers: Vec<String>,
    #[serde(rename = "excludedModels", skip_serializing_if = "Vec::is_empty")]
    pub excluded_models: Vec<String>,
    #[serde(rename = "allowRawPassthrough")]
    pub allow_raw_passthrough: bool,
    #[serde(rename = "allowedIps", skip_serializing_if = "Vec::is_empty")]
    pub allowed_ips: Vec<String>,
    #[serde(rename = "expiresAt", skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    #[serde(rename = "disabledAt", skip_serializing_if = "Option::is_none")]
    pub disabled_at: Option<i64>,
}

impl KeyConfig {
    /// Mirrors `isKeyDisabled` in `packages/backend/src/config.ts`.
    pub fn is_disabled(&self, at_ms: i64) -> bool {
        self.disabled_at.is_some() || self.expires_at.is_some_and(|e| e <= at_ms)
    }
}

/// A single adapter entry, matching the normalized `AdapterEntry` shape
/// Plexus's `AdapterConfigSchema` produces (accepts string/array-of-string
/// on input, always serializes as `{name, options, enabled}[]`).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct AdapterEntry {
    pub name: String,
    #[serde(default)]
    pub options: serde_json::Map<String, serde_json::Value>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

/// Raw-passthrough transport config for a provider. Mirrors
/// `RawPassthroughConfigSchema` in `packages/backend/src/config.ts`.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct RawPassthroughConfig {
    #[serde(default)]
    pub enabled: bool,
    pub base_url: String,
    #[serde(default = "default_auth")]
    pub auth: String,
}

fn default_auth() -> String {
    "bearer".to_string()
}

/// Configuration for a single LLM provider.
///
/// Scoped to the fields exercised by the ported `/v0/management/providers/*`
/// CRUD in `backend/src/providers.rs` — mirrors the load-bearing subset of
/// Plexus's `ProviderConfigSchema` (`packages/backend/src/config.ts`).
/// OAuth-provider-config, quota_checker, model_autosync, and compaction
/// fields are intentionally deferred to a later parity slice.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ProviderConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub api_base_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oauth_provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oauth_account: Option<String>,
    pub enabled: bool,
    pub disable_cooldown: bool,
    pub stall_cooldown: bool,
    pub allow_100_percent_utilization: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub discount: Option<f64>,
    #[serde(rename = "estimateTokens")]
    pub estimate_tokens: bool,
    #[serde(rename = "useClaudeMasking")]
    pub use_claude_masking: bool,
    pub gemini_thinking_enabled: bool,
    pub auto_compat: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub adapter: Vec<AdapterEntry>,
    #[serde(rename = "model_autosync")]
    pub model_autosync: ModelAutosync,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_passthrough: Option<RawPassthroughConfig>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ModelAutosync {
    pub enabled: bool,
    #[serde(rename = "intervalMinutes")]
    pub interval_minutes: u32,
}

impl Default for ModelAutosync {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_minutes: 60,
        }
    }
}

impl ProviderConfig {
    /// Mirrors `isOAuthProviderConfig`: `api_base_url` is an `oauth://`
    /// placeholder (case-insensitive, whitespace-trimmed).
    pub fn is_oauth_provider_config(&self) -> bool {
        Self::is_oauth_placeholder_url(&self.api_base_url)
    }

    /// Mirrors `isOAuthPlaceholderUrl` in `packages/shared/src/provider.ts`.
    pub fn is_oauth_placeholder_url(value: &str) -> bool {
        value.trim().to_lowercase().starts_with("oauth://")
    }
}

/// Shared application state.
///
/// `admin_key` and `keys` correspond to Plexus's `process.env.ADMIN_KEY` and
/// `config.keys`, which `resolvePrincipal` in
/// `packages/backend/src/routes/management/_principal.ts` reads from.
#[derive(Debug, Clone, Default)]
pub struct AppState {
    pub inner: Arc<AppStateInner>,
}

#[derive(Debug, Default)]
pub struct AppStateInner {
    pub admin_key: Option<String>,
    pub keys: RwLock<HashMap<String, KeyConfig>>,
    pub providers: RwLock<HashMap<String, ProviderConfig>>,
    pub aliases: RwLock<HashMap<String, ModelConfig>>,
    pub user_quotas: RwLock<HashMap<String, QuotaDefinition>>,
    pub settings: RwLock<HashMap<String, Value>>,
}

impl AppState {
    pub fn new(admin_key: Option<String>, keys: HashMap<String, KeyConfig>) -> Self {
        Self {
            inner: Arc::new(AppStateInner {
                admin_key,
                keys: RwLock::new(keys),
                providers: RwLock::new(HashMap::new()),
                aliases: RwLock::new(HashMap::new()),
                user_quotas: RwLock::new(HashMap::new()),
                settings: RwLock::new(HashMap::new()),
            }),
        }
    }
}

/// A user quota definition.
///
/// Mirrors `QuotaDefinitionSchema` (`packages/backend/src/config.ts`): a
/// discriminated union on `type`, with shared `QuotaScopeFields` applied to
/// every variant. `duration` is required for `rolling` and absent/ignored
/// for the other three (`daily`/`weekly`/`monthly`).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct QuotaDefinition {
    #[serde(rename = "type")]
    pub quota_type: String,
    #[serde(rename = "limitType")]
    pub limit_type: String,
    pub limit: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration: Option<String>,
    #[serde(rename = "allowedProviders", skip_serializing_if = "Option::is_none")]
    pub allowed_providers: Option<Vec<String>>,
    #[serde(rename = "excludedProviders", skip_serializing_if = "Option::is_none")]
    pub excluded_providers: Option<Vec<String>>,
    #[serde(rename = "allowedModels", skip_serializing_if = "Option::is_none")]
    pub allowed_models: Option<Vec<String>>,
    #[serde(rename = "excludedModels", skip_serializing_if = "Option::is_none")]
    pub excluded_models: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shared: Option<bool>,
    #[serde(rename = "warnAt", skip_serializing_if = "Option::is_none")]
    pub warn_at: Option<f64>,
}

/// A single dispatch target within a `ModelTargetGroup`.
///
/// Mirrors `ModelTargetSchema` (`packages/backend/src/config.ts`): a target
/// is either an `alias` reference (to another model alias/nickname) or a
/// concrete `(provider, model)` pair — never both, never neither.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ModelTarget {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    #[serde(default = "target_enabled_default")]
    pub enabled: bool,
}

fn target_enabled_default() -> bool {
    true
}

/// Mirrors `ModelTargetGroupSchema`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ModelTargetGroup {
    pub name: String,
    pub selector: String,
    pub targets: Vec<ModelTarget>,
}

/// A model alias ("model") configuration.
///
/// Mirrors the load-bearing subset of `ModelConfigSchema`
/// (`packages/backend/src/config.ts`): the routing-relevant fields
/// (`target_groups`, `priority`, `additional_aliases`,
/// `use_image_fallthrough`, `enforce_limits`, `sticky_session`,
/// `preferred_api`, `type`) are fully typed and validated; the remaining
/// schema fields (`advanced`, `metadata`, `pi_model`, `extraBody`,
/// `compaction`) are large nested schemas not required for CRUD contract
/// parity — they round-trip as opaque JSON (`serde_json::Value`) so a
/// client that sets them still gets them back verbatim on GET, without
/// this port re-implementing their full validation/normalization.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ModelConfig {
    pub priority: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub target_groups: Vec<ModelTargetGroup>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub additional_aliases: Vec<String>,
    pub use_image_fallthrough: bool,
    pub enforce_limits: bool,
    pub sticky_session: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preferred_api: Option<Vec<String>>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub model_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub advanced: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pi_model: Option<Value>,
    #[serde(rename = "extraBody", skip_serializing_if = "Option::is_none")]
    pub extra_body: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compaction: Option<Value>,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            priority: "selector".to_string(),
            target_groups: Vec::new(),
            additional_aliases: Vec::new(),
            use_image_fallthrough: false,
            enforce_limits: false,
            sticky_session: true,
            preferred_api: None,
            model_type: None,
            advanced: None,
            metadata: None,
            pi_model: None,
            extra_body: None,
            compaction: None,
        }
    }
}
