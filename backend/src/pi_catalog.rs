//! Dynamic pi-ai model catalog, ported from pinned Plexus
//! `packages/backend/src/services/pi-ai/catalog.ts` together with the pi-ai
//! 1.2.0 overlay semantics it relies on (`createProvider()` baseline+overlay
//! merge in `dist/models.js`, `ModelsStoreEntry` in `dist/models-store.d.ts`).
//!
//! * Baseline: `assets/pi-catalog.json`, a snapshot of pi-ai's
//!   `builtinModels()` (every model type: chat, image, classifier) in provider
//!   registration order, plus `getBuiltinProviders()`. Regenerate it with a Bun
//!   script run from the pinned reference (`packages/backend`) that walks
//!   `builtinModels().getProviders()` and records `{id, nativeRefresh:
//!   typeof refreshModels === 'function', models: getAllModels()}`.
//! * Overlay: `GET https://pi.dev/api/models/providers/<id>` (`accept:
//!   application/json`, `User-Agent: plexus-gateway`); 404/501 keeps the
//!   stored overlay. Overlay models replace baseline entries of the same
//!   type+id and append otherwise.
//! * Scheduling: refresh always restores the persisted overlay first and only
//!   hits the network when the stored check is older than 4h (or forced).
//! * Store: Plexus writes `models-store.json` next to its SQLite database and
//!   falls back to memory for PostgreSQL. Trefoil is PostgreSQL-only, so the
//!   file location is explicit: `TREFOIL_MODELS_STORE_PATH` (same JSON format
//!   as Plexus's file, so a Plexus `models-store.json` can be reused). Unset or
//!   empty keeps the overlay in memory (Plexus's non-SQLite behaviour).
//!   `system_settings` was not used because its whole bag is returned by
//!   `GET /v0/management/system-settings`.
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::Duration;

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::auth::{resolve_principal, Principal};
use crate::AppState;

const DEFAULT_CATALOG_BASE_URL: &str = "https://pi.dev";
const CATALOG_USER_AGENT: &str = "plexus-gateway";
/// Matches pi's own throttle for configured-provider catalog checks.
pub const REMOTE_CATALOG_REFRESH_INTERVAL_MS: u64 = 4 * 60 * 60 * 1000;
/// Providers that manage their own catalog lifecycle (e.g. Radius gateways).
const SELF_REFRESHING_PROVIDERS: &[&str] = &["radius"];
pub const MODELS_STORE_PATH_ENV: &str = "TREFOIL_MODELS_STORE_PATH";
const MODULE: &str = "catalog";

/// pi-ai `ModelsStoreEntry`; unknown fields (`lastModified`, `etag`, ...) are kept.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ModelsStoreEntry {
    pub models: Vec<Value>,
    #[serde(rename = "checkedAt", default, skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<i64>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// `{ refreshed, errors }` exactly as Plexus serialises `CatalogRefreshResult`.
#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub struct CatalogRefreshResult {
    pub refreshed: usize,
    pub errors: Map<String, Value>,
}

// ─── Stores ─────────────────────────────────────────────────────────────────

pub enum ModelsStore {
    Memory(Mutex<HashMap<String, ModelsStoreEntry>>),
    File(FileModelsStore),
}

impl ModelsStore {
    pub fn memory() -> Self {
        Self::Memory(Mutex::new(HashMap::new()))
    }
    pub fn file(path: impl Into<PathBuf>) -> Self {
        Self::File(FileModelsStore { path: path.into(), entries: tokio::sync::Mutex::new(None) })
    }
    /// `TREFOIL_MODELS_STORE_PATH` file store, else in-memory.
    pub fn from_env() -> Self {
        match std::env::var(MODELS_STORE_PATH_ENV) {
            Ok(path) if !path.trim().is_empty() => Self::file(path),
            _ => Self::memory(),
        }
    }
    pub async fn read(&self, provider_id: &str) -> Result<Option<ModelsStoreEntry>, String> {
        match self {
            Self::Memory(entries) => Ok(entries.lock().unwrap().get(provider_id).cloned()),
            Self::File(store) => store.read(provider_id).await,
        }
    }
    pub async fn write(&self, provider_id: &str, entry: ModelsStoreEntry) -> Result<(), String> {
        match self {
            Self::Memory(entries) => {
                entries.lock().unwrap().insert(provider_id.to_owned(), entry);
                Ok(())
            }
            Self::File(store) => store.write(provider_id, entry).await,
        }
    }
    pub async fn delete(&self, provider_id: &str) -> Result<(), String> {
        match self {
            Self::Memory(entries) => {
                entries.lock().unwrap().remove(provider_id);
                Ok(())
            }
            Self::File(store) => store.delete(provider_id).await,
        }
    }
}

/// File-backed store: one JSON object keyed by provider ID. Writes are atomic
/// (tmp + rename) and serialized by the entries lock, so concurrent provider
/// refreshes cannot interleave and a failed write never poisons later ones.
pub struct FileModelsStore {
    path: PathBuf,
    entries: tokio::sync::Mutex<Option<Map<String, Value>>>,
}

impl FileModelsStore {
    async fn loaded<'a>(&self, slot: &'a mut Option<Map<String, Value>>) -> &'a mut Map<String, Value> {
        if slot.is_none() {
            let entries = match tokio::fs::read(&self.path).await {
                Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
                    Ok(Value::Object(map)) => map,
                    Ok(_) => {
                        log("warn", "Model catalog store unreadable, starting empty: not a JSON object".into());
                        Map::new()
                    }
                    Err(error) => {
                        log("warn", format!("Model catalog store unreadable, starting empty: {error}"));
                        Map::new()
                    }
                },
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Map::new(),
                Err(error) => {
                    log("warn", format!("Model catalog store unreadable, starting empty: {error}"));
                    Map::new()
                }
            };
            *slot = Some(entries);
        }
        slot.as_mut().unwrap()
    }

    async fn read(&self, provider_id: &str) -> Result<Option<ModelsStoreEntry>, String> {
        let mut guard = self.entries.lock().await;
        let entries = self.loaded(&mut guard).await;
        match entries.get(provider_id) {
            None => Ok(None),
            Some(value) => serde_json::from_value(value.clone()).map(Some).map_err(|error| error.to_string()),
        }
    }

    async fn write(&self, provider_id: &str, entry: ModelsStoreEntry) -> Result<(), String> {
        let mut guard = self.entries.lock().await;
        let entries = self.loaded(&mut guard).await;
        entries.insert(provider_id.to_owned(), serde_json::to_value(entry).map_err(|error| error.to_string())?);
        let body = serde_json::to_vec(entries).map_err(|error| error.to_string())?;
        self.persist(body).await
    }

    async fn delete(&self, provider_id: &str) -> Result<(), String> {
        let mut guard = self.entries.lock().await;
        let entries = self.loaded(&mut guard).await;
        entries.shift_remove(provider_id);
        let body = serde_json::to_vec(entries).map_err(|error| error.to_string())?;
        self.persist(body).await
    }

    async fn persist(&self, body: Vec<u8>) -> Result<(), String> {
        if let Some(parent) = self.path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
            tokio::fs::create_dir_all(parent).await.map_err(|error| error.to_string())?;
        }
        let mut tmp = self.path.clone().into_os_string();
        tmp.push(".tmp");
        tokio::fs::write(&tmp, body).await.map_err(|error| error.to_string())?;
        tokio::fs::rename(&tmp, &self.path).await.map_err(|error| error.to_string())
    }
}

// ─── Model helpers (pi-ai utils/model-operations.js) ────────────────────────

/// `model.type ?? "chat"`.
fn model_type(model: &Value) -> Value {
    match model.get("type") {
        None | Some(Value::Null) => Value::String("chat".into()),
        Some(value) => value.clone(),
    }
}
fn is_model_type(model: &Value, ty: &str) -> bool {
    model_type(model).as_str() == Some(ty)
}
/// Models from stores and remote sources may have types only newer versions know.
fn has_known_model_type(model: &Value) -> bool {
    matches!(model_type(model).as_str(), Some("chat" | "image" | "classifier"))
}

/// pi.dev answers model-ID keyed objects; tolerate array/{models:[]} shapes too.
pub fn parse_catalog(provider_id: &str, value: Value) -> Result<Vec<Value>, String> {
    let entries: Vec<Value> = match value {
        Value::Array(items) => items,
        Value::Object(mut map) => match map.get("models") {
            Some(Value::Array(_)) => match map.remove("models") {
                Some(Value::Array(items)) => items,
                _ => unreachable!(),
            },
            _ => map.into_iter().map(|(_, value)| value).collect(),
        },
        _ => return Err(format!("Invalid model catalog for provider \"{provider_id}\"")),
    };
    Ok(entries
        .into_iter()
        .filter_map(|entry| match entry {
            Value::Object(mut model) if model.contains_key("id") => {
                model.insert("provider".into(), Value::String(provider_id.to_owned()));
                Some(Value::Object(model))
            }
            _ => None,
        })
        .collect())
}

/// JavaScript `encodeURIComponent`.
pub(crate) fn encode_uri_component(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn system_now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

// ─── Logging ────────────────────────────────────────────────────────────────

static LOG_STATE: OnceLock<AppState> = OnceLock::new();

fn log(level: &str, message: String) {
    if let Some(state) = LOG_STATE.get() {
        crate::system_logs::event(state, level, message, [("module".into(), serde_json::json!(MODULE))]);
    }
}

fn log_refresh_outcome(result: &CatalogRefreshResult) {
    if result.errors.is_empty() {
        log("debug", format!("Model catalog refresh completed ({} providers)", result.refreshed));
    } else {
        let details = result
            .errors
            .iter()
            .map(|(id, message)| format!("{id}: {}", message.as_str().unwrap_or_default()))
            .collect::<Vec<_>>()
            .join("; ");
        log(
            "warn",
            format!("Model catalog refresh completed with {} provider error(s): {details}", result.errors.len()),
        );
    }
}

// ─── Catalog ────────────────────────────────────────────────────────────────

struct ProviderSlot {
    id: String,
    baseline: Vec<Value>,
    dynamic: Vec<Value>,
    /// Built without its own fetchModels, so the pi.dev overlay wraps it.
    wrappable: bool,
}

impl ProviderSlot {
    /// pi-ai `createProvider().getAllModels()`: overlay replaces same type+id.
    fn current_models(&self) -> Vec<Value> {
        let mut merged = self.baseline.clone();
        for model in &self.dynamic {
            let ty = model_type(model);
            match merged.iter().position(|entry| model_type(entry) == ty && entry.get("id") == model.get("id")) {
                Some(index) => merged[index] = model.clone(),
                None => merged.push(model.clone()),
            }
        }
        merged
    }
}

pub struct ModelCatalogDeps {
    pub snapshot: Value,
    pub store: ModelsStore,
    pub catalog_base_url: String,
    pub now: Arc<dyn Fn() -> i64 + Send + Sync>,
    /// Periodic re-refresh cadence. Defaults to the 4h throttle interval.
    pub refresh_interval_ms: u64,
}

impl ModelCatalogDeps {
    pub fn new(snapshot: Value, store: ModelsStore) -> Self {
        Self {
            snapshot,
            store,
            catalog_base_url: DEFAULT_CATALOG_BASE_URL.into(),
            now: Arc::new(system_now_ms),
            refresh_interval_ms: REMOTE_CATALOG_REFRESH_INTERVAL_MS,
        }
    }
}

pub struct ModelCatalog {
    providers: RwLock<Vec<ProviderSlot>>,
    builtin_provider_ids: Vec<String>,
    store: ModelsStore,
    client: reqwest::Client,
    catalog_base_url: String,
    now: Arc<dyn Fn() -> i64 + Send + Sync>,
    refresh_interval_ms: u64,
    generations: Mutex<HashMap<String, u64>>,
    wrapped: AtomicBool,
    timer: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl ModelCatalog {
    pub fn new(deps: ModelCatalogDeps) -> Self {
        let providers = deps.snapshot["providers"]
            .as_array()
            .map(|providers| {
                providers
                    .iter()
                    .filter_map(|provider| {
                        let id = provider["id"].as_str()?.to_owned();
                        let native_refresh = provider["nativeRefresh"].as_bool().unwrap_or(false);
                        Some(ProviderSlot {
                            wrappable: !native_refresh && !SELF_REFRESHING_PROVIDERS.contains(&id.as_str()),
                            id,
                            baseline: provider["models"].as_array().cloned().unwrap_or_default(),
                            dynamic: Vec::new(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let builtin_provider_ids = deps.snapshot["builtinProviderIds"]
            .as_array()
            .map(|ids| ids.iter().filter_map(|id| id.as_str().map(str::to_owned)).collect())
            .unwrap_or_default();
        Self {
            providers: RwLock::new(providers),
            builtin_provider_ids,
            store: deps.store,
            client: reqwest::Client::new(),
            catalog_base_url: deps.catalog_base_url,
            now: deps.now,
            refresh_interval_ms: deps.refresh_interval_ms,
            generations: Mutex::new(HashMap::new()),
            wrapped: AtomicBool::new(false),
            timer: Mutex::new(None),
        }
    }

    /// `getBuiltinProviders()`: provider IDs of the static chat catalog.
    pub fn builtin_provider_ids(&self) -> &[String] {
        &self.builtin_provider_ids
    }

    /// Every model type (chat, image, classifier), one provider or all.
    pub fn get_all_models(&self, provider: Option<&str>) -> Vec<Value> {
        let providers = self.providers.read().unwrap();
        match provider {
            Some(id) => providers.iter().find(|slot| slot.id == id).map(ProviderSlot::current_models).unwrap_or_default(),
            None => providers.iter().flat_map(ProviderSlot::current_models).collect(),
        }
    }

    /// Chat models (pi-ai `getModels`), one provider or all.
    pub fn get_models(&self, provider: Option<&str>) -> Vec<Value> {
        self.get_models_of_type("chat", provider)
    }

    pub fn get_models_of_type(&self, ty: &str, provider: Option<&str>) -> Vec<Value> {
        self.get_all_models(provider).into_iter().filter(|model| is_model_type(model, ty)).collect()
    }

    /// Chat model lookup; `None` for unknown pairs.
    pub fn get_model(&self, provider: &str, id: &str) -> Option<Value> {
        self.get_models(Some(provider)).into_iter().find(|model| model["id"].as_str() == Some(id))
    }

    fn refreshable_ids(&self) -> Vec<String> {
        // wrapProviders(): every built-in provider without a native catalog
        // source gains the pi.dev overlay; self-refreshing ones are skipped.
        self.wrapped.store(true, Ordering::Relaxed);
        self.providers.read().unwrap().iter().filter(|slot| slot.wrappable).map(|slot| slot.id.clone()).collect()
    }

    fn generation_is_current(&self, provider_id: &str, generation: u64) -> bool {
        self.generations.lock().unwrap().get(provider_id) == Some(&generation)
    }

    fn set_dynamic(&self, provider_id: &str, models: Vec<Value>) {
        if let Some(slot) = self.providers.write().unwrap().iter_mut().find(|slot| slot.id == provider_id) {
            slot.dynamic = models;
        }
    }

    /// Catalog `publish`: drop superseded publications, persist, then update.
    async fn publish(
        &self,
        provider_id: &str,
        generation: u64,
        persist: Option<ModelsStoreEntry>,
        update: Vec<Value>,
    ) -> Result<bool, String> {
        if !self.generation_is_current(provider_id, generation) {
            return Ok(false);
        }
        if let Some(entry) = persist {
            self.store.write(provider_id, entry).await?;
        }
        if !self.generation_is_current(provider_id, generation) {
            return Ok(false);
        }
        self.set_dynamic(provider_id, update);
        Ok(true)
    }

    /// pi-ai `createProvider().refreshModels` with Plexus's pi.dev `fetchModels`.
    async fn refresh_provider(&self, provider_id: &str, allow_network: bool, generation: u64) -> Result<(), String> {
        let stored = self.store.read(provider_id).await?;
        if let Some(stored) = &stored {
            let restored = stored
                .models
                .iter()
                .filter(|model| model.get("provider").and_then(Value::as_str) == Some(provider_id))
                .cloned()
                .collect();
            if !self.publish(provider_id, generation, None, restored).await? {
                return Ok(());
            }
        }
        if !allow_network {
            return Ok(());
        }
        let fetched = self.fetch_pi_dev_catalog(provider_id, stored.as_ref()).await?;
        let refreshed: Vec<Value> = fetched.into_iter().filter(has_known_model_type).collect();
        let entry = ModelsStoreEntry { models: refreshed.clone(), checked_at: Some(system_now_ms()), extra: Map::new() };
        self.publish(provider_id, generation, Some(entry), refreshed).await?;
        Ok(())
    }

    /// 404/501 means no overlay is published — keep whatever the store holds.
    async fn fetch_pi_dev_catalog(&self, provider_id: &str, stored: Option<&ModelsStoreEntry>) -> Result<Vec<Value>, String> {
        let url = reqwest::Url::parse(&self.catalog_base_url)
            .and_then(|base| base.join(&format!("/api/models/providers/{}", encode_uri_component(provider_id))))
            .map_err(|error| error.to_string())?;
        let response = self
            .client
            .get(url)
            .header("accept", "application/json")
            .header("User-Agent", CATALOG_USER_AGENT)
            .timeout(Duration::from_secs(60))
            .send()
            .await
            .map_err(|error| error.to_string())?;
        let status = response.status().as_u16();
        if status == 404 || status == 501 {
            return Ok(stored.map(|entry| entry.models.clone()).unwrap_or_default());
        }
        if !response.status().is_success() {
            return Err(format!("Model catalog request failed for {provider_id}: {status}"));
        }
        let body: Value = response.json().await.map_err(|error| error.to_string())?;
        parse_catalog(provider_id, body)
    }

    /// Restore every provider's persisted overlay, then fetch stale providers
    /// from pi.dev (throttled to 4h unless forced).
    pub async fn refresh(&self, force: bool, allow_network: bool) -> CatalogRefreshResult {
        let ids = self.refreshable_ids();
        let outcomes = futures_util::future::join_all(ids.into_iter().map(|id| async move {
            let generation = {
                let mut generations = self.generations.lock().unwrap();
                let next = generations.get(&id).copied().unwrap_or(0) + 1;
                generations.insert(id.clone(), next);
                next
            };
            let result: Result<(), String> = async {
                // Always restore the persisted overlay first.
                self.refresh_provider(&id, false, generation).await?;
                if allow_network {
                    let stored = self.store.read(&id).await?;
                    let stale = force
                        || match stored.and_then(|entry| entry.checked_at) {
                            None => true,
                            Some(checked_at) => (self.now)() - checked_at >= REMOTE_CATALOG_REFRESH_INTERVAL_MS as i64,
                        };
                    if stale {
                        self.refresh_provider(&id, true, generation).await?;
                    }
                }
                Ok(())
            }
            .await;
            (id, result)
        }))
        .await;
        let mut result = CatalogRefreshResult::default();
        for (id, outcome) in outcomes {
            match outcome {
                Ok(()) => result.refreshed += 1,
                Err(error) => {
                    result.errors.insert(id, Value::String(error));
                }
            }
        }
        result
    }

    /// Restore the persisted overlay; with network, also refresh in the
    /// background and schedule periodic re-refreshes (errors are logged).
    pub async fn init(self: &Arc<Self>, allow_network: bool) {
        self.refresh(false, false).await;
        if allow_network {
            let catalog = Arc::clone(self);
            tokio::spawn(async move { log_refresh_outcome(&catalog.refresh(false, true).await) });
            let mut timer = self.timer.lock().unwrap();
            if timer.is_none() {
                let catalog = Arc::clone(self);
                let period = Duration::from_millis(self.refresh_interval_ms.max(1));
                *timer = Some(tokio::spawn(async move {
                    loop {
                        tokio::time::sleep(period).await;
                        let catalog = Arc::clone(&catalog);
                        tokio::spawn(async move { log_refresh_outcome(&catalog.refresh(false, true).await) });
                    }
                }));
            }
        }
    }

    /// Stop the periodic re-refresh scheduled by `init`.
    pub fn dispose(&self) {
        if let Some(timer) = self.timer.lock().unwrap().take() {
            timer.abort();
        }
    }
}

// ─── Singleton ──────────────────────────────────────────────────────────────

pub fn builtin_snapshot() -> Value {
    serde_json::from_str(include_str!("assets/pi-catalog.json")).expect("embedded pi catalog must be valid")
}

static CATALOG: OnceLock<Arc<ModelCatalog>> = OnceLock::new();

pub fn catalog() -> &'static Arc<ModelCatalog> {
    CATALOG.get_or_init(|| Arc::new(ModelCatalog::new(ModelCatalogDeps::new(builtin_snapshot(), ModelsStore::from_env()))))
}

/// `PLEXUS_MODEL_CATALOG_REFRESH=false` disables pi.dev network refresh.
pub fn network_refresh_allowed() -> bool {
    std::env::var("PLEXUS_MODEL_CATALOG_REFRESH").map_or(true, |value| value != "false")
}

/// Startup hook (Plexus `initModelCatalog`): restore the persisted overlay,
/// then refresh from pi.dev in the background unless disabled.
pub async fn init(state: &AppState) {
    let _ = LOG_STATE.set(state.clone());
    catalog().init(network_refresh_allowed()).await;
}

// ─── Routes (Plexus routes/management/models.ts) ────────────────────────────

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/v0/management/catalog/status", axum::routing::get(catalog_status))
        .route("/v0/management/catalog/refresh-all", axum::routing::post(refresh_all))
}

#[allow(clippy::result_large_err)]
fn require_admin(state: &AppState, headers: &HeaderMap) -> Result<(), Response> {
    let provided_key = headers.get("x-admin-key").and_then(|value| value.to_str().ok());
    match resolve_principal(state, provided_key) {
        Some(Principal::Admin) => Ok(()),
        Some(Principal::Limited { .. }) => Err((
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": { "message": "Admin privileges required", "type": "forbidden", "code": 403 }
            })),
        )
            .into_response()),
        None => Err((
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "error": { "message": "Unauthorized", "type": "auth_error", "code": 401 }
            })),
        )
            .into_response()),
    }
}

fn catalog_status_body() -> Value {
    let source = |id: &str| serde_json::json!({ "initialized": crate::metadata::initialized(id), "count": crate::metadata::count(id) });
    serde_json::json!({
        "intervals": {
            "metadataMinutes": crate::metadata::AUTO_REFRESH_MINUTES,
            "piCatalogMs": REMOTE_CATALOG_REFRESH_INTERVAL_MS,
            "versionMinutes": crate::cli_versions::AUTO_REFRESH_MINUTES,
        },
        "versions": {
            "codex": crate::cli_versions::codex_version(),
            "claudeCode": crate::cli_versions::claude_code_version(),
        },
        "metadata": {
            "openrouter": source("openrouter"),
            "modelsDev": source("models.dev"),
            "catwalk": source("catwalk"),
        },
        "piCatalog": { "modelCount": catalog().get_all_models(None).len() },
    })
}

async fn catalog_status(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    Json(catalog_status_body()).into_response()
}

async fn refresh_all(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    let started = std::time::Instant::now();
    let refreshed_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let codex_previous = crate::cli_versions::codex_version();
    let claude_code_previous = crate::cli_versions::claude_code_version();
    let (metadata, pi_catalog) = tokio::join!(
        crate::metadata::refresh(crate::metadata::SourceUrls::default(), "manual"),
        catalog().refresh(true, network_refresh_allowed()),
    );
    // Version fetches keep the last-known version on failure; run them after
    // the catalog work so a slow registry can't delay it.
    let (codex_error, claude_code_error) =
        tokio::join!(crate::cli_versions::refresh_codex(), crate::cli_versions::refresh_claude_code());
    let duration_ms = started.elapsed().as_millis() as u64;
    let had_errors = metadata.had_errors || !pi_catalog.errors.is_empty() || codex_error.is_some() || claude_code_error.is_some();
    if had_errors {
        let mut context = vec![
            ("module".to_owned(), serde_json::json!("models")),
            ("piCatalogErrors".to_owned(), Value::Object(pi_catalog.errors.clone())),
        ];
        if let Some(error) = &codex_error {
            context.push(("codexError".into(), serde_json::json!(error)));
        }
        if let Some(error) = &claude_code_error {
            context.push(("claudeCodeError".into(), serde_json::json!(error)));
        }
        crate::system_logs::event(&state, "warn", "Catalog refresh-all (manual) completed with errors", context);
    } else {
        crate::system_logs::event(
            &state,
            "info",
            format!("Catalog refresh-all (manual) completed in {duration_ms}ms"),
            [("module".into(), serde_json::json!("models"))],
        );
    }
    let version = |previous: String, current: String, error: Option<String>| {
        let mut body = serde_json::json!({ "previous": previous, "current": current });
        if let Some(error) = error {
            body["error"] = Value::String(error);
        }
        body
    };
    Json(serde_json::json!({
        "success": !had_errors,
        "message": if had_errors { "Catalog refresh completed with errors" } else { "All catalogs refreshed successfully" },
        "trigger": "manual",
        "refreshedAt": refreshed_at,
        "durationMs": duration_ms,
        "hadErrors": had_errors,
        "metadata": metadata,
        "piCatalog": pi_catalog,
        "versions": {
            "codex": version(codex_previous, crate::cli_versions::codex_version(), codex_error),
            "claudeCode": version(claude_code_previous, crate::cli_versions::claude_code_version(), claude_code_error),
        },
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{extract::Path, routing::get};
    use std::sync::atomic::AtomicUsize;

    fn snapshot(providers: Vec<(&str, bool, Vec<Value>)>) -> Value {
        serde_json::json!({
            "builtinProviderIds": providers.iter().map(|(id, _, _)| *id).collect::<Vec<_>>(),
            "providers": providers.into_iter().map(|(id, native, models)| serde_json::json!({"id": id, "nativeRefresh": native, "models": models})).collect::<Vec<_>>(),
        })
    }
    fn baseline() -> Value {
        serde_json::json!({"id":"claude-base","name":"Baseline","provider":"anthropic","api":"anthropic-messages","contextWindow":200000})
    }

    /// Mock pi.dev: `/api/models/providers/:id` answers `(status, body)` from `respond`.
    async fn serve(
        respond: impl Fn(String) -> (u16, Value) + Send + Sync + 'static,
    ) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let respond = Arc::new(respond);
        let app = Router::new().route(
            "/api/models/providers/:id",
            get(move |Path(id): Path<String>, headers: HeaderMap| {
                let respond = respond.clone();
                let counter = counter.clone();
                async move {
                    assert_eq!(headers["accept"], "application/json");
                    assert_eq!(headers["user-agent"], "plexus-gateway");
                    counter.fetch_add(1, Ordering::SeqCst);
                    let (status, body) = respond(id);
                    (StatusCode::from_u16(status).unwrap(), Json(body))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), calls, server)
    }

    fn make(base_url: &str, providers: Vec<(&str, bool, Vec<Value>)>, store: ModelsStore, now: Option<Arc<dyn Fn() -> i64 + Send + Sync>>) -> Arc<ModelCatalog> {
        let mut deps = ModelCatalogDeps::new(snapshot(providers), store);
        deps.catalog_base_url = base_url.into();
        if let Some(now) = now {
            deps.now = now;
        }
        Arc::new(ModelCatalog::new(deps))
    }
    fn anthropic() -> Vec<(&'static str, bool, Vec<Value>)> {
        vec![("anthropic", false, vec![baseline()])]
    }

    #[tokio::test]
    async fn pi_catalog_serves_baseline_without_network() {
        let catalog = make("http://127.0.0.1:9", anthropic(), ModelsStore::memory(), None);
        catalog.init(false).await;
        assert_eq!(catalog.get_model("anthropic", "claude-base").unwrap()["id"], "claude-base");
        assert!(catalog.get_model("anthropic", "nope").is_none());
        assert!(catalog.get_models(Some("unknown")).is_empty());
    }

    #[tokio::test]
    async fn pi_catalog_merges_overlay_and_persists() {
        let (url, calls, server) = serve(|_| (200, serde_json::json!({
            "claude-new": {"id":"claude-new","name":"New","contextWindow":500000},
            "claude-base": {"id":"claude-base","name":"Baseline v2","contextWindow":400000},
            "future": {"id":"future","type":"video"},
        }))).await;
        let dir = std::env::temp_dir().join(format!("trefoil-picat-{}", std::process::id()));
        let path = dir.join("nested/models-store.json");
        let catalog = make(&url, anthropic(), ModelsStore::file(&path), None);
        catalog.init(false).await;
        let result = catalog.refresh(false, true).await;
        assert_eq!(serde_json::to_value(&result).unwrap(), serde_json::json!({"refreshed":1,"errors":{}}));
        assert_eq!(catalog.get_model("anthropic", "claude-new").unwrap()["contextWindow"], 500000);
        assert_eq!(catalog.get_model("anthropic", "claude-new").unwrap()["provider"], "anthropic");
        assert_eq!(catalog.get_model("anthropic", "claude-base").unwrap()["name"], "Baseline v2");
        // Baseline order is kept; new models append; unknown types are dropped.
        let ids: Vec<_> = catalog.get_all_models(None).iter().map(|m| m["id"].clone()).collect();
        assert_eq!(ids, vec![serde_json::json!("claude-base"), serde_json::json!("claude-new")]);
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let stored: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(stored["anthropic"]["models"].as_array().unwrap().len(), 2);
        assert!(stored["anthropic"]["checkedAt"].is_i64());

        let second = make("http://127.0.0.1:9", anthropic(), ModelsStore::file(&path), None);
        second.init(false).await;
        assert_eq!(second.get_model("anthropic", "claude-new").unwrap()["id"], "claude-new");
        server.abort();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn pi_catalog_throttles_unless_forced() {
        let now = Arc::new(std::sync::atomic::AtomicI64::new(system_now_ms()));
        let clock = now.clone();
        let (url, calls, server) = serve(|_| (200, serde_json::json!({"claude-new":{"id":"claude-new"}}))).await;
        let catalog = make(&url, anthropic(), ModelsStore::memory(), Some(Arc::new(move || clock.load(Ordering::SeqCst))));
        catalog.refresh(false, true).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        catalog.refresh(false, true).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        now.store(system_now_ms() + REMOTE_CATALOG_REFRESH_INTERVAL_MS as i64 + 1, Ordering::SeqCst);
        catalog.refresh(false, true).await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        catalog.refresh(true, true).await;
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        catalog.refresh(true, false).await;
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        server.abort();
    }

    #[tokio::test]
    async fn pi_catalog_404_records_check_and_failures_keep_stored() {
        let (url, calls, server) = serve(|id| if id == "anthropic" { (404, serde_json::json!({})) } else { (500, serde_json::json!({})) }).await;
        let catalog = make(&url, anthropic(), ModelsStore::memory(), None);
        assert!(catalog.refresh(false, true).await.errors.is_empty());
        assert_eq!(catalog.get_model("anthropic", "claude-base").unwrap()["id"], "claude-base");
        catalog.refresh(false, true).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let store = ModelsStore::memory();
        store.write("p", ModelsStoreEntry { models: vec![serde_json::json!({"id":"stored","provider":"p"}), serde_json::json!({"id":"other","provider":"q"})], checked_at: Some(1), extra: Map::new() }).await.unwrap();
        let catalog = make(&url, vec![("p", false, vec![])], store, Some(Arc::new(|| 2_000_000_000)));
        let result = catalog.refresh(false, true).await;
        assert_eq!(result.errors["p"], "Model catalog request failed for p: 500");
        assert_eq!(result.refreshed, 0);
        assert_eq!(catalog.get_model("p", "stored").unwrap()["id"], "stored");
        assert!(catalog.get_model("p", "other").is_none());
        server.abort();
        let down = make("http://127.0.0.1:9", vec![("p", false, vec![])], ModelsStore::memory(), None);
        assert!(down.refresh(false, true).await.errors.contains_key("p"));
    }

    #[tokio::test]
    async fn pi_catalog_accepts_catalog_shapes_and_skips_self_refreshing() {
        for body in [serde_json::json!({"m1":{"id":"m1"}}), serde_json::json!([{"id":"m1"}, 3, {"name":"x"}]), serde_json::json!({"models":[{"id":"m1"}]})] {
            let (url, _, server) = serve(move |_| (200, body.clone())).await;
            let catalog = make(&url, vec![("p", false, vec![])], ModelsStore::memory(), None);
            catalog.refresh(false, true).await;
            assert_eq!(catalog.get_model("p", "m1").unwrap()["provider"], "p");
            assert_eq!(catalog.get_all_models(Some("p")).len(), 1);
            server.abort();
        }
        assert!(parse_catalog("p", serde_json::json!("x")).unwrap_err().contains("Invalid model catalog for provider \"p\""));
        let (url, calls, server) = serve(|_| (200, serde_json::json!({"m1":{"id":"m1"}}))).await;
        let catalog = make(&url, vec![("radius", true, vec![]), ("anthropic", false, vec![baseline()])], ModelsStore::memory(), None);
        assert_eq!(catalog.refresh(false, true).await.refreshed, 1);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        server.abort();
    }

    #[tokio::test]
    async fn pi_catalog_background_and_periodic_refresh() {
        let (url, calls, server) = serve(|_| (200, serde_json::json!({"claude-new":{"id":"claude-new"}}))).await;
        let mut deps = ModelCatalogDeps::new(snapshot(anthropic()), ModelsStore::memory());
        deps.catalog_base_url = url;
        deps.now = Arc::new(|| system_now_ms() + REMOTE_CATALOG_REFRESH_INTERVAL_MS as i64);
        deps.refresh_interval_ms = 25;
        let catalog = Arc::new(ModelCatalog::new(deps));
        catalog.init(true).await;
        for _ in 0..200 {
            if calls.load(Ordering::SeqCst) >= 2 && catalog.get_model("anthropic", "claude-new").is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(calls.load(Ordering::SeqCst) >= 2);
        catalog.dispose();
        let at_dispose = calls.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(calls.load(Ordering::SeqCst) <= at_dispose + 1);
        server.abort();
    }

    #[tokio::test]
    async fn pi_catalog_file_store_tolerates_corrupt_file() {
        let path = std::env::temp_dir().join(format!("trefoil-picat-corrupt-{}.json", std::process::id()));
        std::fs::write(&path, b"{not json").unwrap();
        let store = ModelsStore::file(&path);
        assert!(store.read("p").await.unwrap().is_none());
        store.write("p", ModelsStoreEntry { models: vec![], checked_at: Some(5), extra: Map::new() }).await.unwrap();
        assert_eq!(store.read("p").await.unwrap().unwrap().checked_at, Some(5));
        store.delete("p").await.unwrap();
        assert!(ModelsStore::file(&path).read("p").await.unwrap().is_none());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn pi_catalog_builtin_snapshot_matches_pinned_pi_ai() {
        let catalog = ModelCatalog::new(ModelCatalogDeps::new(builtin_snapshot(), ModelsStore::memory()));
        assert_eq!(catalog.builtin_provider_ids().len(), 42);
        assert_eq!(catalog.get_all_models(None).len(), 1649);
        assert_eq!(catalog.get_models(None).len(), 1554);
        assert_eq!(catalog.get_models_of_type("image", None).len(), 61);
        assert_eq!(catalog.get_models_of_type("classifier", None).len(), 34);
        assert_eq!(catalog.get_model("openai", "gpt-4o").unwrap()["api"], "openai-responses");
        assert_eq!(encode_uri_component("a b/c"), "a%20b%2Fc");
    }

    #[tokio::test]
    async fn pi_catalog_status_and_refresh_all_routes_require_admin() {
        let state = crate::AppState::new(Some("admin".into()), HashMap::new());
        let app = router().with_state(state);
        use tower::ServiceExt;
        let response = app
            .clone()
            .oneshot(axum::http::Request::get("/v0/management/catalog/status").body(axum::body::Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let response = app
            .oneshot(axum::http::Request::get("/v0/management/catalog/status").header("x-admin-key", "admin").body(axum::body::Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value = serde_json::from_slice(&axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
        assert_eq!(body["intervals"], serde_json::json!({"metadataMinutes":60,"piCatalogMs":14400000,"versionMinutes":60}));
        assert!(body["piCatalog"]["modelCount"].as_u64().unwrap() >= 1649);
        assert!(body["metadata"]["modelsDev"]["initialized"].is_boolean());
        assert!(body["versions"]["codex"].is_string());
    }
}
