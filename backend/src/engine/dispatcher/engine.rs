//! Trefoil glue for the engine: the `getConfig()` snapshot built from
//! `AppState`, the `RoutingRuntime` / observability hooks backed by
//! Trefoil's cooldown manager, concurrency tracker and PostgreSQL usage
//! storage.
use crate::engine::config::Config;
use crate::engine::js::*;
use crate::engine::observability::storage::{Pagination, StorageHooks, UsageFilters};
use crate::engine::observability::*;
use crate::engine::routing::route_candidates::build_quota_exceeded_body;
use crate::engine::routing::RoutingRuntime;
use crate::engine::transformer::EngineError;
use crate::state::AppState;
use rand::Rng;
use serde_json::{json, Map, Value};
use std::sync::{Arc, Mutex};

tokio::task_local! {
    /// `getCurrentKeyName()` request context (set by the v1 auth step).
    pub static KEY_NAME: Option<String>;
}

struct TaskKeyName;
impl KeyNameContext for TaskKeyName {
    fn current_key_name(&self) -> Option<String> {
        KEY_NAME.try_with(|k| k.clone()).ok().flatten()
    }
}

/// Usage events → the dashboard broadcast channel.
struct BroadcastEvents(tokio::sync::broadcast::Sender<crate::usage::UsageEvent>);
impl UsageEvents for BroadcastEvents {
    fn emit(&self, kind: UsageEventKind, record: &Value) {
        let _ = self.0.send(crate::usage::UsageEvent {
            kind: kind.as_str().into(),
            id: now_ms(),
            api_key: record
                .get("apiKey")
                .and_then(Value::as_str)
                .map(str::to_owned),
            data: record.clone(),
        });
    }
}

/// Sink used when Trefoil runs without a database.
struct NullSink;
#[async_trait::async_trait]
impl UsageSink for NullSink {
    async fn save_request(&self, _record: &UsageRecord) {}
    async fn save_error(
        &self,
        _request_id: &str,
        _error: &ErrorInfo,
        _details: Option<&Value>,
        _api_key: Option<&str>,
    ) {
    }
    async fn update_performance_metrics(&self, _sample: &PerformanceSample) {}
    fn register_in_flight(&self, request_id: &str, api_key: Option<&str>, is_streamed: bool) {
        super::stall_inspector::register_in_flight(request_id, api_key, is_streamed);
    }
    fn deregister_in_flight(&self, request_id: &str) {
        super::stall_inspector::deregister_in_flight(request_id);
    }
}

fn setting(
    settings: &std::collections::HashMap<String, Value>,
    key: &str,
    default: Value,
) -> Value {
    settings.get(key).cloned().unwrap_or(default)
}

/// `ConfigService.doRebuild` cache from Trefoil state.
pub fn build_config(state: &AppState) -> Config {
    let settings = state.inner.settings.read().unwrap().clone();
    let mut root: Map<String, Value> = settings
        .iter()
        .filter(|(k, _)| !k.contains('.'))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let mut providers: Vec<(String, Value)> = state
        .inner
        .providers
        .read()
        .unwrap()
        .iter()
        .map(|(k, p)| (k.clone(), p.public_view()))
        .collect();
    providers.sort_by(|a, b| a.0.cmp(&b.0));
    root.insert(
        "providers".into(),
        Value::Object(providers.into_iter().collect()),
    );
    let mut models: Vec<(String, Value)> = state
        .inner
        .aliases
        .read()
        .unwrap()
        .iter()
        .map(|(k, a)| (k.clone(), a.public_view()))
        .collect();
    models.sort_by(|a, b| a.0.cmp(&b.0));
    root.insert("models".into(), Value::Object(models.into_iter().collect()));
    root.insert(
        "keys".into(),
        serde_json::to_value(state.inner.keys.read().unwrap().clone()).unwrap_or(Value::Null),
    );
    let codes: Vec<i32> = (100..=599)
        .filter(|c| !(200..=299).contains(c) && *c != 413 && *c != 422)
        .collect();
    root.insert("failover".into(), json!({
        "enabled": setting(&settings, "failover.enabled", json!(true)),
        "retryableStatusCodes": setting(&settings, "failover.retryableStatusCodes", json!(codes)),
        "retryableErrors": setting(&settings, "failover.retryableErrors", json!(["ECONNREFUSED", "ETIMEDOUT", "ENOTFOUND"])),
    }));
    root.insert(
        "cooldown".into(),
        json!({
            "initialMinutes": setting(&settings, "cooldown.initialMinutes", json!(2)),
            "maxMinutes": setting(&settings, "cooldown.maxMinutes", json!(300)),
        }),
    );
    root.insert(
        "timeout".into(),
        json!({"defaultSeconds": setting(&settings, "timeout.defaultSeconds", json!(300))}),
    );
    root.insert(
        "stall".into(),
        json!({
            "ttfbSeconds": setting(&settings, "stall.ttfbSeconds", Value::Null),
            "ttfbBytes": setting(&settings, "stall.ttfbBytes", json!(100)),
            "minBytesPerSecond": setting(&settings, "stall.minBytesPerSecond", Value::Null),
            "windowSeconds": setting(&settings, "stall.windowSeconds", json!(10)),
            "gracePeriodSeconds": setting(&settings, "stall.gracePeriodSeconds", json!(30)),
            "stallCooldown": setting(&settings, "stall.stallCooldown", json!(false)),
        }),
    );
    root.insert("backgroundExploration".into(), json!({
        "enabled": setting(&settings, "backgroundExploration.enabled", json!(false)),
        "stalenessThresholdSeconds": setting(&settings, "backgroundExploration.stalenessThresholdSeconds", json!(600)),
        "workerConcurrency": setting(&settings, "backgroundExploration.workerConcurrency", json!(2)),
    }));
    let user_quotas = state.inner.user_quotas.read().unwrap();
    if !user_quotas.is_empty() {
        root.insert(
            "user_quotas".into(),
            serde_json::to_value(user_quotas.clone()).unwrap_or(Value::Null),
        );
    }
    Config::new(Value::Object(root))
}

/// Per-request view of the process services.
pub struct Engine {
    pub state: AppState,
    pub config: Arc<Config>,
    pub usage: Option<Arc<UsageStorage>>,
    /// Body of the last `buildQuotaExceededError` (EngineError cannot carry it).
    quota_body: Mutex<Option<(Value, String)>>,
}

impl Engine {
    pub fn new(state: &AppState) -> Self {
        let usage = state.inner.database.clone().map(|pool| {
            state
                .inner
                .engine_usage
                .0
                .get_or_init(|| {
                    let events = state
                        .inner
                        .usage_events
                        .clone()
                        .map(|s| Arc::new(BroadcastEvents(s)) as Arc<dyn UsageEvents>);
                    Arc::new(UsageStorage::new(
                        pool,
                        StorageHooks {
                            events,
                            key_context: Some(Arc::new(TaskKeyName)),
                            debug: Some(Arc::new(super::debug::DebugManager::new(state))),
                        },
                    ))
                })
                .clone()
        });
        Self {
            state: state.clone(),
            config: Arc::new(build_config(state)),
            usage,
            quota_body: Mutex::new(None),
        }
    }

    /// `DebugManager.getInstance()`.
    pub fn debug(&self) -> super::debug::DebugManager {
        super::debug::DebugManager::new(&self.state)
    }

    pub fn sink(&self) -> Arc<dyn UsageSink> {
        match &self.usage {
            Some(u) => u.clone(),
            None => Arc::new(NullSink),
        }
    }

    pub fn response_services(&self) -> ResponseServices {
        ResponseServices {
            sink: self.sink(),
            config: self.config.clone(),
            debug: Arc::new(self.debug()),
            transformers: Arc::new(super::transformer_factory::Registry),
            quota: Some(Arc::new(super::quota::QuotaRecorder {
                state: self.state.clone(),
            })),
            cooldowns: Some(Arc::new(StallCooldowns {
                state: self.state.clone(),
            })),
            openrouter: Some(Arc::new(MetadataCatalog)),
            gemini_rewriter: Some(Arc::new(|| {
                Box::new(GeminiRewriter(
                    crate::engine::gemini::malformed::MalformedFunctionCallRewriter::new(),
                )) as Box<dyn RawStreamRewriter>
            })),
            admin_key: self.state.inner.admin_key.clone(),
        }
    }

    pub fn emit_started_async(&self, record: UsageRecord) {
        if let Some(u) = &self.usage {
            u.emit_started_async(record);
        }
    }
    pub fn emit_updated_async(&self, record: UsageRecord) {
        if let Some(u) = &self.usage {
            u.emit_updated_async(record);
        }
    }

    /// Takes the quota-exceeded body/retryHistory stashed by routing.
    pub fn take_quota_body(&self) -> Option<(Value, String)> {
        self.quota_body.lock().unwrap().take()
    }
}

/// `ModelMetadataManager` OpenRouter map (backs `PricingManager`).
pub struct MetadataCatalog;
impl OpenRouterCatalog for MetadataCatalog {
    fn openrouter_pricing(&self, slug: &str) -> Option<Value> {
        crate::metadata::lookup("openrouter", slug).and_then(|m| m.pricing)
    }
    fn is_initialized(&self) -> bool {
        crate::metadata::initialized("openrouter")
    }
    fn all_slugs(&self) -> Vec<String> {
        crate::metadata::all_ids("openrouter")
    }
}

struct StallCooldowns {
    state: AppState,
}
impl CooldownHooks for StallCooldowns {
    fn mark_provider_stall_failure(&self, provider: &str, model: &str, message: &str) {
        let (state, p, m, msg) = (
            self.state.clone(),
            provider.to_owned(),
            model.to_owned(),
            message.to_owned(),
        );
        tokio::spawn(async move {
            crate::cooldown_manager::mark_stall_failure(&state, &p, &m, Some(msg)).await
        });
    }
}

struct GeminiRewriter(crate::engine::gemini::malformed::MalformedFunctionCallRewriter);
impl GeminiRewriter {
    fn drain(&mut self, text: String) -> (String, Vec<StreamDefect>) {
        let defects = std::mem::take(&mut self.0.detected)
            .into_iter()
            .map(|d| StreamDefect {
                code: d.code.into(),
                message: d.message.into(),
                status_code: d.status_code,
                text_leak_detected: d.text_leak_detected,
            })
            .collect();
        (text, defects)
    }
}
impl RawStreamRewriter for GeminiRewriter {
    fn feed(&mut self, text: &str) -> (String, Vec<StreamDefect>) {
        let out = self.0.feed(text);
        self.drain(out)
    }
    fn flush(&mut self) -> (String, Vec<StreamDefect>) {
        let out = self.0.flush();
        self.drain(out)
    }
}

#[async_trait::async_trait]
impl RoutingRuntime for Engine {
    fn config(&self) -> Arc<Config> {
        self.config.clone()
    }
    fn random(&self) -> f64 {
        rand::thread_rng().gen::<f64>()
    }
    async fn is_provider_healthy(&self, provider: &str, model: &str) -> bool {
        crate::cooldown_manager::is_healthy(&self.state, provider, model).await
    }
    fn provider_concurrency(&self, provider: &str) -> f64 {
        crate::concurrency::provider_count(&self.state, provider) as f64
    }
    fn target_concurrency(&self, provider: &str, model: &str) -> f64 {
        crate::concurrency::target_count(&self.state, provider, model) as f64
    }
    async fn provider_performance(&self, provider: &str, model: &str) -> Vec<Value> {
        match &self.usage {
            Some(u) => u
                .get_provider_performance(Some(provider), Some(model))
                .await
                .iter()
                .map(|r| r.to_json())
                .collect(),
            None => vec![],
        }
    }
    async fn usage_total(
        &self,
        provider: &str,
        selected_model_name: &str,
        start: &str,
        end: &str,
    ) -> f64 {
        let Some(u) = &self.usage else { return 0.0 };
        let filters = UsageFilters {
            provider: Some(provider.into()),
            selected_model_name: Some(selected_model_name.into()),
            start_date: Some(start.into()),
            end_date: Some(end.into()),
            ..Default::default()
        };
        u.get_usage(
            &filters,
            &Pagination {
                limit: 1000,
                offset: 0,
                sort_by: None,
                sort_asc: false,
            },
        )
        .await
        .map(|(_, t)| t as f64)
        .unwrap_or(0.0)
    }
    async fn latest_quota_for_provider(&self, provider: &str) -> Option<Value> {
        super::quota::latest_quota_for_provider(&self.state, &self.config, provider).await
    }
    fn openrouter_pricing(&self, slug: &str) -> Option<Value> {
        PricingManager::new(&MetadataCatalog).get_pricing(slug)
    }
    fn build_quota_exceeded_error(
        &self,
        blocking: &[Value],
        retry_history: &[Value],
    ) -> EngineError {
        let history = if retry_history.is_empty() {
            String::new()
        } else {
            stringify(&Value::Array(retry_history.to_vec()))
        };
        *self.quota_body.lock().unwrap() = Some((build_quota_exceeded_body(blocking), history));
        crate::engine::routing::route_candidates::default_quota_exceeded_error(blocking)
    }
}
