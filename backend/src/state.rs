use std::collections::HashMap;
use std::sync::Arc;
use std::sync::RwLock;

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
}

impl AppState {
    pub fn new(admin_key: Option<String>, keys: HashMap<String, KeyConfig>) -> Self {
        Self {
            inner: Arc::new(AppStateInner {
                admin_key,
                keys: RwLock::new(keys),
                providers: RwLock::new(HashMap::new()),
            }),
        }
    }
}
