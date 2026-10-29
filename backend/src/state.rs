use std::collections::HashMap;
use std::sync::Arc;
use std::sync::RwLock;

use serde_json::Value;
use sqlx::PgPool;

/// Configuration for a single limited (non-admin) API key.
///
/// Mirrors the fields Plexus's `Principal`/`api_keys` config exposes on
/// `GET /v0/management/auth/verify` for a `limited` principal.
///
/// Field set also covers what `KeyConfigSchema`
/// (`packages/backend/src/config.ts`) accepts/persists for
/// `/v0/management/keys/:name` CRUD (see `backend/src/keys.rs`).
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default)]
pub struct KeyConfig {
    pub secret: String,
    #[serde(rename = "permissionGroup", skip_serializing_if = "Option::is_none")]
    pub permission_group: Option<String>,
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
    // Plexus exposes this flag on every key, including its false default.
    // Keeping the default in the wire shape is important for management key
    // reads and config backups, which are structural parity contracts.
    #[serde(rename = "allowRawPassthrough")]
    pub allow_raw_passthrough: bool,
    #[serde(rename = "allowedIps", skip_serializing_if = "Vec::is_empty")]
    pub allowed_ips: Vec<String>,
    #[serde(rename = "expiresAt", skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    #[serde(rename = "disabledAt", skip_serializing_if = "Option::is_none")]
    pub disabled_at: Option<i64>,
    /// Plexus distinguishes an explicit `quotas: []` from no quotas at all.
    #[serde(skip)]
    pub quotas_defined: bool,
    #[serde(rename = "defaultServiceTier", default)]
    pub default_service_tier: Option<String>,
}

/// Plexus `KeyRepository.getAllKeys` shape.
impl serde::Serialize for KeyConfig {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut out = serde_json::Map::new();
        out.insert("secret".into(), Value::String(self.secret.clone()));
        if let Some(comment) = self.comment.as_ref().filter(|c| !c.is_empty()) {
            out.insert("comment".into(), Value::String(comment.clone()));
        }
        if let Some(v) = self.expires_at {
            out.insert("expiresAt".into(), v.into());
        }
        if let Some(v) = self.disabled_at {
            out.insert("disabledAt".into(), v.into());
        }
        if self.quotas_defined || !self.quota_names.is_empty() {
            out.insert("quotas".into(), serde_json::json!(self.quota_names));
        }
        for (key, list) in [
            ("allowedModels", &self.allowed_models),
            ("allowedProviders", &self.allowed_providers),
            ("excludedModels", &self.excluded_models),
            ("excludedProviders", &self.excluded_providers),
        ] {
            if !list.is_empty() {
                out.insert(key.into(), serde_json::json!(list));
            }
        }
        out.insert(
            "allowRawPassthrough".into(),
            Value::Bool(self.allow_raw_passthrough),
        );
        if !self.allowed_ips.is_empty() {
            out.insert("allowedIps".into(), serde_json::json!(self.allowed_ips));
        }
        if let Some(tier) = &self.default_service_tier {
            out.insert("defaultServiceTier".into(), Value::String(tier.clone()));
        }
        Value::Object(out).serialize(serializer)
    }
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

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct RetryPolicy {
    #[serde(default)]
    pub max_retries: u32,
    #[serde(default, rename = "delayMs")]
    pub delay_ms: u64,
    #[serde(default = "default_retry_backoff", rename = "backoffMultiplier")]
    pub backoff_multiplier: f64,
}

fn default_retry_backoff() -> f64 {
    2.0
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 0,
            delay_ms: 0,
            backoff_multiplier: 2.0,
        }
    }
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
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct ProviderConfig {
    /// Monotonic per-provider revision for conditional management edits.
    #[serde(default)]
    pub version: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub api_base_url: String,
    /// Optional per-API endpoint overrides; missing entries retain legacy routing.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub api_base_urls: HashMap<String, String>,
    /// Explicit inventory; empty preserves legacy unrestricted alias/direct routing.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub models: HashMap<String, ProviderModel>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quota_checker: Option<QuotaCheckerConfig>,
    #[serde(default, rename = "retryPolicy")]
    pub retry_policy: RetryPolicy,
    /// Plexus repository view (`provider_view`); the served JSON shape.
    #[serde(skip)]
    pub view: Option<Value>,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ProviderModel {
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub model_type: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub access_via: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_id: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct QuotaCheckerConfig {
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub checker_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(
        default = "default_quota_interval",
        rename = "intervalMinutes",
        alias = "interval"
    )]
    pub interval: u64,
    #[serde(default)]
    pub options: Value,
}

fn default_quota_interval() -> u64 {
    30
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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
    /// Resolve an endpoint override, retaining the legacy scalar fallback.
    pub fn api_url_for(&self, api_type: &str) -> &str {
        self.api_base_urls
            .get(api_type)
            .map(String::as_str)
            .unwrap_or(&self.api_base_url)
    }

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
    pub concurrency: Arc<crate::concurrency::ConcurrencyTracker>,
    pub vision_descriptor_cache: std::sync::Mutex<crate::vision::VisionDescriptorCache>,
    /// In-flight provider OAuth login sessions (Plexus
    /// `OAuthLoginSessionManager`): in-memory only, 20-minute TTL; completed
    /// credentials are persisted separately.
    pub oauth_sessions: crate::oauth_login::OAuthLoginSessionManager,
    /// Snapshot of persisted `(provider_type, account_id)` OAuth credentials,
    /// refreshed before provider reads/writes resolve account links.
    pub oauth_accounts: RwLock<Vec<(String, String)>>,
    /// Per-account refresh gates. The network request is made outside the
    /// database transaction, while this gate prevents duplicate refreshes.
    pub oauth_refresh_locks: std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    pub usage_events: Option<tokio::sync::broadcast::Sender<crate::usage::UsageEvent>>,
    /// Bounded recent logs, newest entries at the back.
    pub recent_system_logs:
        std::sync::Mutex<std::collections::VecDeque<crate::system_logs::SystemLog>>,
    /// Subscribers for the admin-only live system log stream.
    pub system_logs: Option<tokio::sync::broadcast::Sender<crate::system_logs::SystemLog>>,
    /// Production state is being migrated to PostgreSQL repository by
    /// repository. `None` is reserved for unit-test-only in-memory fixtures.
    pub database: Option<PgPool>,
    pub admin_key: Option<String>,
    pub keys: RwLock<HashMap<String, KeyConfig>>,
    pub providers: RwLock<HashMap<String, ProviderConfig>>,
    pub aliases: RwLock<HashMap<String, ModelConfig>>,
    pub user_quotas: RwLock<HashMap<String, QuotaDefinition>>,
    pub settings: RwLock<HashMap<String, Value>>,
    pub cooldowns: RwLock<HashMap<(String, String), CooldownEntry>>,
    pub mcp_key_round_robin: RwLock<HashMap<i32, i32>>,
    /// Local `local_http` MCP processes (Plexus `mcpProcessManager`).
    pub mcp_manager: Arc<crate::mcp_local::McpProcessManager>,
    /// Per-key trace/debug capture toggle, mirroring `DebugManager`'s
    /// `enabledKeys` set. Trefoil does not implement actual trace capture —
    /// only the on/off bookkeeping the `self/debug/toggle` and
    /// `/v0/management/debug` endpoints read and write.
    pub debug_enabled_keys: RwLock<std::collections::HashSet<String>>,
    /// Mirrors `DebugManager.enabledGlobal`.
    pub debug_enabled_global: RwLock<bool>,
    /// Mirrors `DebugManager.enabledAliases`.
    pub debug_enabled_aliases: RwLock<std::collections::HashSet<String>>,
    /// Mirrors `DebugManager.enabledProviders` (the "provider filter").
    pub debug_enabled_providers: RwLock<std::collections::HashSet<String>>,
    /// Engine `UsageStorageService` (created on first inference request).
    pub engine_usage: EngineUsage,
}

/// Process-wide engine usage storage (created on first inference request).
#[derive(Default)]
pub struct EngineUsage(pub std::sync::OnceLock<Arc<crate::engine::observability::UsageStorage>>);

impl std::fmt::Debug for EngineUsage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("EngineUsage")
    }
}

impl AppState {
    pub fn new(admin_key: Option<String>, keys: HashMap<String, KeyConfig>) -> Self {
        Self {
            inner: Arc::new(AppStateInner {
                concurrency: Arc::new(crate::concurrency::ConcurrencyTracker::default()),
                vision_descriptor_cache: std::sync::Mutex::new(
                    crate::vision::VisionDescriptorCache::default(),
                ),
                oauth_sessions: Default::default(),
                oauth_accounts: RwLock::new(Vec::new()),
                oauth_refresh_locks: std::sync::Mutex::new(HashMap::new()),
                usage_events: Some(tokio::sync::broadcast::channel(256).0),
                system_logs: Some(tokio::sync::broadcast::channel(256).0),
                recent_system_logs: Default::default(),
                database: None,
                admin_key,
                keys: RwLock::new(keys),
                providers: RwLock::new(HashMap::new()),
                aliases: RwLock::new(HashMap::new()),
                user_quotas: RwLock::new(HashMap::new()),
                settings: RwLock::new(HashMap::new()),
                cooldowns: RwLock::new(HashMap::new()),
                mcp_key_round_robin: RwLock::new(HashMap::new()),
                mcp_manager: Arc::new(crate::mcp_local::McpProcessManager::default()),
                debug_enabled_keys: RwLock::new(std::collections::HashSet::new()),
                debug_enabled_global: RwLock::new(false),
                debug_enabled_aliases: RwLock::new(std::collections::HashSet::new()),
                debug_enabled_providers: RwLock::new(std::collections::HashSet::new()),
                engine_usage: EngineUsage::default(),
            }),
        }
    }

    pub fn with_database(mut self, database: PgPool) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("database must be attached before AppState is cloned")
            .database = Some(database);
        self
    }
}

/// An active provider (or provider/model) cooldown entry.
///
/// Mirrors `CooldownManager`'s in-memory `CooldownEntry`
/// (`packages/backend/src/services/runtime/cooldown-manager.ts`). Trefoil
/// keeps cooldowns in memory only (no DB persistence) — Plexus's DB-backed
/// persistence is an implementation detail invisible to the management API
/// contract this crate ports.
#[derive(Debug, Clone)]
pub struct CooldownEntry {
    pub expiry_ms: i64,
    pub consecutive_failures: u32,
    pub last_error: Option<String>,
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
    #[serde(serialize_with = "js_number")]
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
    #[serde(
        rename = "warnAt",
        skip_serializing_if = "Option::is_none",
        serialize_with = "js_number_opt"
    )]
    pub warn_at: Option<f64>,
}

/// Serialize like a JavaScript number: integral values without `.0`.
pub fn js_number<S: serde::Serializer>(value: &f64, serializer: S) -> Result<S::Ok, S::Error> {
    if value.fract() == 0.0 && value.abs() < 9_007_199_254_740_992.0 {
        serializer.serialize_i64(*value as i64)
    } else {
        serializer.serialize_f64(*value)
    }
}
pub fn js_number_opt<S: serde::Serializer>(
    value: &Option<f64>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match value {
        Some(v) => js_number(v, serializer),
        None => serializer.serialize_none(),
    }
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
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ModelConfig {
    pub priority: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub target_groups: Vec<ModelTargetGroup>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub additional_aliases: Vec<String>,
    pub use_image_fallthrough: bool,
    pub enforce_limits: bool,
    pub sticky_session: bool,
    #[serde(default)]
    pub synthetic_safeguard_approval: bool,
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
    /// Plexus repository view (`alias_view`); the served JSON shape.
    #[serde(skip)]
    pub view: Option<Value>,
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
            synthetic_safeguard_approval: false,
            preferred_api: None,
            model_type: None,
            advanced: None,
            metadata: None,
            pi_model: None,
            extra_body: None,
            compaction: None,
            view: None,
        }
    }
}

impl serde::Serialize for ModelConfig {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.public_view().serialize(serializer)
    }
}

/// Every serialized provider (management API, backups, config export) uses
/// the Plexus repository shape; see `provider_view`.
impl serde::Serialize for ProviderConfig {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.public_view().serialize(serializer)
    }
}
