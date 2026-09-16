use std::collections::HashMap;
use std::sync::Arc;

/// Configuration for a single limited (non-admin) API key.
///
/// Mirrors the fields Plexus's `Principal`/`api_keys` config exposes on
/// `GET /v0/management/auth/verify` for a `limited` principal.
#[derive(Debug, Clone, Default)]
pub struct KeyConfig {
    pub secret: String,
    pub allowed_providers: Vec<String>,
    pub allowed_models: Vec<String>,
    pub excluded_providers: Vec<String>,
    pub excluded_models: Vec<String>,
    pub quota_names: Vec<String>,
    pub comment: Option<String>,
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
    pub keys: HashMap<String, KeyConfig>,
}

impl AppState {
    pub fn new(admin_key: Option<String>, keys: HashMap<String, KeyConfig>) -> Self {
        Self {
            inner: Arc::new(AppStateInner { admin_key, keys }),
        }
    }
}
