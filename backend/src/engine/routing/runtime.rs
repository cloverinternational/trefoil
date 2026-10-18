//! `RoutingRuntime`: the Plexus process services routing reads. One method
//! per capability; each documents the TS call it replaces and the exact data
//! it must return. Trefoil implements it over its state/DB.
use super::background_explorer::BackgroundExplorer;
use super::sticky_session::StickySessionManager;
use crate::engine::config::Config;
use crate::engine::transformer::EngineError;
use serde_json::Value;
use std::sync::Arc;

#[async_trait::async_trait]
pub trait RoutingRuntime: Send + Sync {
    /// `getConfig()`: the current config snapshot (`providers`, `models`,
    /// `backgroundExploration`, flat settings such as
    /// `performanceExplorationRate` / `latencyExplorationRate` /
    /// `e2ePerformanceExplorationRate`).
    fn config(&self) -> Arc<Config>;

    /// `Math.random()`: uniform in `[0, 1)`. Used by the random selector and
    /// selector exploration (`pickExplorationTarget`, exploration-rate rolls).
    fn random(&self) -> f64;

    /// `Date.now()` in epoch milliseconds (usage-selector window, quota
    /// selector reset arithmetic).
    fn now_ms(&self) -> i64 {
        crate::engine::js::now_ms()
    }

    /// `CooldownManager.getInstance().isProviderHealthy(provider, model)`:
    /// `false` while a provider-wide cooldown (`model == ""` key) or the
    /// `(provider, model)` cooldown is active, else `true` (including the
    /// side effect of clearing expired entries). The router builds
    /// `filterHealthyTargets(targets)` from this by calling it sequentially for
    /// each target in order and keeping the healthy ones.
    async fn is_provider_healthy(&self, provider: &str, model: &str) -> bool;

    /// `ConcurrencyTracker.getInstance().getProviderCount(provider)`: in-flight
    /// requests for the provider (0 when none).
    fn provider_concurrency(&self, provider: &str) -> f64;

    /// `ConcurrencyTracker.getInstance().getTargetCount(provider, model)`:
    /// in-flight requests for `${provider}/${model}` (0 when none).
    fn target_concurrency(&self, provider: &str, model: &str) -> f64;

    /// Whether `SelectorFactory.setUsageStorage` was called. When `false`, the
    /// `performance`, `e2e_performance`, `latency` and `usage` selectors throw
    /// `UsageStorageService not initialized in SelectorFactory. Call setUsageStorage first.`
    fn usage_storage_ready(&self) -> bool {
        true
    }

    /// `UsageStorageService.getProviderPerformance(provider, model)`: rows as
    /// JSON objects with the snake_case fields Plexus maps:
    /// `{provider, model, target_model, avg_ttft_ms, min_ttft_ms, max_ttft_ms,
    /// avg_tokens_per_sec, min_tokens_per_sec, max_tokens_per_sec,
    /// avg_e2e_tokens_per_sec, min_e2e_tokens_per_sec, max_e2e_tokens_per_sec,
    /// sample_count, success_count, failure_count, last_updated}` (numbers;
    /// aggregates default to 0, `last_updated` = MAX(created_at) epoch ms),
    /// grouped by (provider, model) and ordered by AVG(tokens_per_sec) DESC,
    /// followed by zero rows for providers seen in request usage with
    /// `canonical_model_name = model` but no performance rows. Selectors read
    /// only `rows[0]` (`avg_ttft_ms`, `avg_tokens_per_sec`,
    /// `avg_e2e_tokens_per_sec`, `sample_count`, `last_updated`). Return `[]`
    /// on errors (Plexus logs and returns `[]`).
    async fn provider_performance(&self, provider: &str, model: &str) -> Vec<Value>;

    /// `UsageStorageService.getUsage({ provider, selectedModelName, startDate,
    /// endDate }, { limit: 1000, offset: 0 }).total`: the number of request
    /// usage rows for that provider and selected (provider-side) model with
    /// `date` between the two ISO-8601 timestamps (inclusive).
    async fn usage_total(
        &self,
        provider: &str,
        selected_model_name: &str,
        start_date: &str,
        end_date: &str,
    ) -> f64;

    /// Whether `SelectorFactory.setQuotaScheduler` was called with a scheduler.
    /// When `false`, the `quota` selector throws
    /// `QuotaScheduler not initialized in SelectorFactory. Call setQuotaScheduler first.`
    fn quota_scheduler_ready(&self) -> bool {
        true
    }

    /// `QuotaScheduler.getLatestQuotaForProvider(provider)`: the latest
    /// non-stale, fresh `MeterCheckResult` JSON for the first quota checker
    /// configured for that provider — `{checkerId, checkerType, provider,
    /// checkedAt, success, stale?, error?, meters: [{key, label, kind, unit,
    /// limit?, used?, remaining?, utilizationPercent (number | 'unknown' …),
    /// resetsAt? (ISO-8601), status ('ok' | 'exhausted' | …), …}]}` — or
    /// `None` (no checker, no snapshot, stale, too old, or on error: the
    /// selector maps rejections to `null`).
    async fn latest_quota_for_provider(&self, provider: &str) -> Option<Value>;

    /// `PricingManager.getInstance().getPricing(slug)`: the OpenRouter pricing
    /// record `{prompt, completion, input_cache_read?, input_cache_write?, …}`
    /// (per-token prices as decimal strings, as OpenRouter returns them), or
    /// `None` when unknown.
    fn openrouter_pricing(&self, slug: &str) -> Option<Value>;

    /// `StickySessionManager.getInstance()`: the process-wide sticky map,
    /// shared with the dispatcher that records successful targets.
    fn sticky_sessions(&self) -> &StickySessionManager {
        StickySessionManager::global()
    }

    /// `BackgroundExplorer.getInstance()`: `None` until initialised.
    fn background_explorer(&self) -> Option<Arc<BackgroundExplorer>> {
        None
    }

    /// `buildQuotaExceededError(blocking, retryHistory)`: the default builds
    /// the message and `routingContext.{statusCode: 429, code:
    /// 'quota_exceeded'}`; Trefoil should additionally carry
    /// `body = route_candidates::build_quota_exceeded_body(blocking)` and,
    /// when non-empty, `retryHistory = JSON.stringify(retry_history)`.
    fn build_quota_exceeded_error(
        &self,
        blocking: &[Value],
        _retry_history: &[Value],
    ) -> EngineError {
        super::route_candidates::default_quota_exceeded_error(blocking)
    }
}

#[cfg(test)]
pub mod mock {
    use super::*;
    use std::collections::{HashMap, HashSet, VecDeque};
    use std::sync::Mutex;

    /// Test double: config + scripted randomness and service data.
    #[derive(Default)]
    pub struct MockRuntime {
        pub cfg: Arc<Config>,
        /// Values returned by `random()` in order; 0.0 once exhausted.
        pub randoms: Mutex<VecDeque<f64>>,
        pub now: Option<i64>,
        /// `provider/model` (or `provider/` for provider-wide) on cooldown.
        pub unhealthy: HashSet<String>,
        pub provider_counts: HashMap<String, f64>,
        pub target_counts: HashMap<String, f64>,
        pub performance: HashMap<String, Vec<Value>>,
        pub usage: HashMap<String, f64>,
        pub quotas: HashMap<String, Value>,
        pub quota_calls: Mutex<Vec<String>>,
        pub pricing: HashMap<String, Value>,
        pub sticky: StickySessionManager,
        pub no_usage_storage: bool,
        pub explorer: Option<Arc<BackgroundExplorer>>,
    }

    impl MockRuntime {
        pub fn new(root: Value) -> Self {
            Self {
                cfg: Arc::new(Config::new(root)),
                ..Default::default()
            }
        }
        pub fn with_randoms(self, r: &[f64]) -> Self {
            *self.randoms.lock().unwrap() = r.iter().copied().collect();
            self
        }
    }

    #[async_trait::async_trait]
    impl RoutingRuntime for MockRuntime {
        fn config(&self) -> Arc<Config> {
            self.cfg.clone()
        }
        fn random(&self) -> f64 {
            self.randoms.lock().unwrap().pop_front().unwrap_or(0.0)
        }
        fn now_ms(&self) -> i64 {
            self.now.unwrap_or_else(crate::engine::js::now_ms)
        }
        async fn is_provider_healthy(&self, provider: &str, model: &str) -> bool {
            !self.unhealthy.contains(&format!("{provider}/{model}"))
                && !self.unhealthy.contains(&format!("{provider}/"))
        }
        fn provider_concurrency(&self, provider: &str) -> f64 {
            self.provider_counts.get(provider).copied().unwrap_or(0.0)
        }
        fn target_concurrency(&self, provider: &str, model: &str) -> f64 {
            self.target_counts
                .get(&format!("{provider}/{model}"))
                .copied()
                .unwrap_or(0.0)
        }
        fn usage_storage_ready(&self) -> bool {
            !self.no_usage_storage
        }
        async fn provider_performance(&self, provider: &str, model: &str) -> Vec<Value> {
            self.performance
                .get(&format!("{provider}/{model}"))
                .cloned()
                .unwrap_or_default()
        }
        async fn usage_total(&self, provider: &str, model: &str, _s: &str, _e: &str) -> f64 {
            self.usage
                .get(&format!("{provider}/{model}"))
                .copied()
                .unwrap_or(0.0)
        }
        async fn latest_quota_for_provider(&self, provider: &str) -> Option<Value> {
            self.quota_calls.lock().unwrap().push(provider.to_owned());
            self.quotas.get(provider).cloned()
        }
        fn openrouter_pricing(&self, slug: &str) -> Option<Value> {
            self.pricing.get(slug).cloned()
        }
        fn sticky_sessions(&self) -> &StickySessionManager {
            &self.sticky
        }
        fn background_explorer(&self) -> Option<Arc<BackgroundExplorer>> {
            self.explorer.clone()
        }
    }
}
