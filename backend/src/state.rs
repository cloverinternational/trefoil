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
}

impl AppState {
    pub fn new(admin_key: Option<String>, keys: HashMap<String, KeyConfig>) -> Self {
        Self {
            inner: Arc::new(AppStateInner {
                admin_key,
                keys: RwLock::new(keys),
            }),
        }
    }
}
