//! `routing/background-explorer.ts`: fires synthetic chat probes at stale
//! targets of performance-selector groups, off the live request path.
use super::js_num;
use crate::engine::config::{model_config, Config};
use crate::engine::js::{get, get_arr, get_str, truthy};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

const PERFORMANCE_SELECTORS: [&str; 3] = ["latency", "performance", "e2e_performance"];

/// Services the explorer needs (TS: `getConfig`, `CooldownManager`,
/// `ProbeService`).
#[async_trait::async_trait]
pub trait ExplorerHooks: Send + Sync {
    /// `getConfig()` (reads `backgroundExploration.{enabled,
    /// stalenessThresholdSeconds, workerConcurrency}` and `providers`).
    fn config(&self) -> Arc<Config>;
    /// `Date.now()`.
    fn now_ms(&self) -> i64 {
        crate::engine::js::now_ms()
    }
    /// `CooldownManager.isProviderHealthy(provider, model)`; `false` on error.
    async fn is_provider_healthy(&self, provider: &str, model: &str) -> bool;
    /// `ProbeService.runProbe({ provider, model, apiType: 'chat', source:
    /// 'background' })`: run one chat-shaped probe and record its
    /// performance; must not panic (errors are swallowed by the caller).
    async fn run_probe(&self, provider: &str, model: &str);
}

#[derive(Clone, Copy)]
struct TargetState {
    last_probed_at: i64,
    in_flight: bool,
}

#[derive(Default)]
struct Inner {
    state: HashMap<String, TargetState>,
    queue: VecDeque<(String, String)>,
    active_workers: usize,
}

struct Shared {
    hooks: Arc<dyn ExplorerHooks>,
    inner: Mutex<Inner>,
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// `BackgroundExplorer` (one per process; exposed to routing through
/// `RoutingRuntime::background_explorer`).
pub struct BackgroundExplorer {
    shared: Arc<Shared>,
    process_start_time: i64,
}

fn key_for(provider: &str, model: &str) -> String {
    format!("{provider}:{model}")
}

/// `declaredModelType`: provider model config `type` first, alias type second.
fn declared_model_type<'a>(
    provider_cfg: &'a Value,
    model: &str,
    alias_type: Option<&'a str>,
) -> Option<&'a str> {
    if let Some(t) = model_config(provider_cfg, model)
        .and_then(|m| get_str(m, &["type"]))
        .filter(|t| !t.is_empty())
    {
        return Some(t);
    }
    alias_type
}

impl BackgroundExplorer {
    /// `BackgroundExplorer.initialize(probeService)`.
    pub fn new(hooks: Arc<dyn ExplorerHooks>) -> Self {
        let process_start_time = hooks.now_ms();
        Self {
            shared: Arc::new(Shared {
                hooks,
                inner: Mutex::new(Inner::default()),
            }),
            process_start_time,
        }
    }

    /// `maybeTrigger(group, aliasType?)`: non-blocking. For each enabled
    /// concrete text target of a performance-selector group whose last probe
    /// (initially process start) is older than `stalenessThresholdSeconds`
    /// and not in flight, checks cooldown in the background and enqueues a
    /// probe; workers (`workerConcurrency`) drain the queue. Background work
    /// needs a Tokio runtime and is skipped without one.
    pub fn maybe_trigger(&self, group: &Value, alias_type: Option<&str>) {
        let hooks = &self.shared.hooks;
        let config = hooks.config();
        let Some(bg) = get(&config.root, &["backgroundExploration"]).filter(|b| truthy(Some(b)))
        else {
            return;
        };
        if bg.get("enabled") != Some(&Value::Bool(true)) {
            return;
        }
        if !get_str(group, &["selector"]).is_some_and(|s| PERFORMANCE_SELECTORS.contains(&s)) {
            return;
        }

        let now = hooks.now_ms() as f64;
        let threshold_ms = js_num(bg.get("stalenessThresholdSeconds")) * 1000.0;

        for target in get_arr(group, &["targets"])
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            if target.get("enabled") == Some(&Value::Bool(false)) {
                continue;
            }
            if truthy(target.get("alias"))
                || !truthy(target.get("provider"))
                || !truthy(target.get("model"))
            {
                continue;
            }
            let (Some(provider), Some(model)) =
                (get_str(target, &["provider"]), get_str(target, &["model"]))
            else {
                continue;
            };
            let Some(provider_cfg) = config.provider(provider).filter(|p| truthy(Some(p))) else {
                continue;
            };
            if provider_cfg.get("enabled") == Some(&Value::Bool(false)) {
                continue;
            }
            if declared_model_type(provider_cfg, model, alias_type).is_some_and(|t| t != "text") {
                continue;
            }

            let key = key_for(provider, model);
            {
                let mut inner = self.shared.lock();
                let st = *inner.state.entry(key.clone()).or_insert(TargetState {
                    last_probed_at: self.process_start_time,
                    in_flight: false,
                });
                if st.in_flight {
                    continue;
                }
                if now - (st.last_probed_at as f64) < threshold_ms {
                    continue;
                }
            }

            let Ok(handle) = tokio::runtime::Handle::try_current() else {
                continue;
            };
            let shared = self.shared.clone();
            let (provider, model) = (provider.to_owned(), model.to_owned());
            handle.spawn(async move {
                if !shared.hooks.is_provider_healthy(&provider, &model).await {
                    return;
                }
                {
                    let mut inner = shared.lock();
                    let Some(st) = inner.state.get(&key).copied() else {
                        return;
                    };
                    if st.in_flight {
                        return;
                    }
                    if ((shared.hooks.now_ms() - st.last_probed_at) as f64) < threshold_ms {
                        return;
                    }
                    inner.queue.push_back((provider, model));
                }
                pump_workers(&shared);
            });
        }
    }

    /// Test helper: (queued, active workers).
    #[cfg(test)]
    fn counts(&self) -> (usize, usize) {
        let i = self.shared.lock();
        (i.queue.len(), i.active_workers)
    }
}

/// `pumpWorkers`.
fn pump_workers(shared: &Arc<Shared>) {
    let config = shared.hooks.config();
    let Some(bg) = get(&config.root, &["backgroundExploration"]).filter(|b| truthy(Some(b))) else {
        return;
    };
    if bg.get("enabled") != Some(&Value::Bool(true)) {
        return;
    }
    let concurrency = js_num(bg.get("workerConcurrency"));
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let mut inner = shared.lock();
    while (inner.active_workers as f64) < concurrency && !inner.queue.is_empty() {
        inner.active_workers += 1;
        let shared = shared.clone();
        handle.spawn(async move { worker(shared).await });
    }
}

/// `worker`: drain the queue, re-checking cooldown before each probe.
async fn worker(shared: Arc<Shared>) {
    loop {
        let next = {
            let mut inner = shared.lock();
            let Some((provider, model)) = inner.queue.pop_front() else {
                break;
            };
            let key = key_for(&provider, &model);
            match inner.state.get(&key) {
                Some(st) if !st.in_flight => Some((provider, model, key)),
                _ => None,
            }
        };
        let Some((provider, model, key)) = next else {
            continue;
        };
        if !shared.hooks.is_provider_healthy(&provider, &model).await {
            continue;
        }
        {
            let mut inner = shared.lock();
            match inner.state.get_mut(&key) {
                Some(st) => st.in_flight = true,
                None => continue,
            }
        }
        shared.hooks.run_probe(&provider, &model).await;
        let now = shared.hooks.now_ms();
        if let Some(st) = shared.lock().state.get_mut(&key) {
            st.last_probed_at = now;
            st.in_flight = false;
        }
    }
    let mut inner = shared.lock();
    inner.active_workers = inner.active_workers.saturating_sub(1);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Hooks {
        cfg: Arc<Config>,
        probes: Mutex<Vec<String>>,
        now: i64,
    }
    #[async_trait::async_trait]
    impl ExplorerHooks for Hooks {
        fn config(&self) -> Arc<Config> {
            self.cfg.clone()
        }
        fn now_ms(&self) -> i64 {
            self.now
        }
        async fn is_provider_healthy(&self, provider: &str, _m: &str) -> bool {
            provider != "cool"
        }
        async fn run_probe(&self, provider: &str, model: &str) {
            self.probes
                .lock()
                .unwrap()
                .push(format!("{provider}/{model}"));
        }
    }

    #[tokio::test]
    async fn probes_stale_text_targets_only() {
        let cfg = json!({
            "backgroundExploration": {"enabled": true, "stalenessThresholdSeconds": 0, "workerConcurrency": 2},
            "providers": {
                "p": {"models": {"m": {}, "img": {"type": "image"}}},
                "cool": {"models": {"m": {}}},
                "off": {"enabled": false, "models": {"m": {}}}
            }
        });
        let hooks = Arc::new(Hooks {
            cfg: Arc::new(Config::new(cfg)),
            probes: Mutex::new(vec![]),
            now: 1000,
        });
        let ex = BackgroundExplorer::new(hooks.clone());
        let group = json!({"selector": "latency", "targets": [
            {"provider": "p", "model": "m"}, {"provider": "p", "model": "img"}, {"provider": "cool", "model": "m"},
            {"provider": "off", "model": "m"}, {"provider": "p", "model": "x", "enabled": false}, {"alias": "other"}
        ]});
        ex.maybe_trigger(&group, None);
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
        assert_eq!(*hooks.probes.lock().unwrap(), vec!["p/m".to_string()]);
        assert_eq!(ex.counts(), (0, 0));

        // Alias type `image` makes untyped targets non-probeable.
        hooks.probes.lock().unwrap().clear();
        let g2 = json!({"selector": "performance", "targets": [{"provider": "p", "model": "m2"}]});
        ex.maybe_trigger(&g2, Some("image"));
        // Non-performance selectors are ignored.
        ex.maybe_trigger(
            &json!({"selector": "in_order", "targets": [{"provider": "p", "model": "m3"}]}),
            None,
        );
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
        assert!(hooks.probes.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn respects_staleness_and_disabled_flag() {
        let cfg = json!({"backgroundExploration": {"enabled": true, "stalenessThresholdSeconds": 60, "workerConcurrency": 1}, "providers": {"p": {}}});
        let hooks = Arc::new(Hooks {
            cfg: Arc::new(Config::new(cfg)),
            probes: Mutex::new(vec![]),
            now: 1000,
        });
        let ex = BackgroundExplorer::new(hooks.clone());
        ex.maybe_trigger(
            &json!({"selector": "latency", "targets": [{"provider": "p", "model": "m"}]}),
            None,
        );
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        assert!(
            hooks.probes.lock().unwrap().is_empty(),
            "not stale yet relative to process start"
        );

        let cfg = json!({"backgroundExploration": {"enabled": false, "stalenessThresholdSeconds": 0, "workerConcurrency": 1}, "providers": {"p": {}}});
        let hooks = Arc::new(Hooks {
            cfg: Arc::new(Config::new(cfg)),
            probes: Mutex::new(vec![]),
            now: 1000,
        });
        let ex = BackgroundExplorer::new(hooks.clone());
        ex.maybe_trigger(
            &json!({"selector": "latency", "targets": [{"provider": "p", "model": "m"}]}),
            None,
        );
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        assert!(hooks.probes.lock().unwrap().is_empty());
    }
}
