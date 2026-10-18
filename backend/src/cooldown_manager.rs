//! Pinned Plexus `services/runtime/cooldown-manager.ts` over Trefoil's
//! in-memory cooldown map and `provider_cooldowns` table.
//!
//! Keys are `(provider, model)`; `model == ""` is a provider-wide cooldown
//! that blocks every model. Durations escalate `initialMinutes · 2^(n-1)`
//! capped at `maxMinutes` unless the caller supplies one (Retry-After, quota
//! parsers). An expired entry is kept with `expiry = 0` so the failure count
//! survives until the next success.
use crate::state::{AppState, CooldownEntry};
use serde_json::Value;

const DEADLINE_EXPIRED_PATTERNS: [&str; 3] = ["deadline expired", "deadline_exceeded", "deadline exceeded"];

fn now_ms() -> i64 { chrono::Utc::now().timestamp_millis() }

fn provider_view(state: &AppState, provider: &str) -> Option<Value> {
    state.inner.providers.read().unwrap().get(provider).map(|p| p.public_view())
}

fn cooldown_disabled(state: &AppState, provider: &str) -> bool {
    provider_view(state, provider).is_some_and(|p| p.get("disable_cooldown") == Some(&Value::Bool(true)))
}

fn stall_cooldown_enabled(state: &AppState, provider: &str) -> bool {
    if provider_view(state, provider).is_some_and(|p| p.get("stall_cooldown") == Some(&Value::Bool(true))) { return true; }
    state.inner.settings.read().unwrap().get("stall.stallCooldown").and_then(Value::as_bool).unwrap_or(false)
}

fn setting_f64(state: &AppState, key: &str, default: f64) -> f64 {
    state.inner.settings.read().unwrap().get(key).and_then(Value::as_f64).unwrap_or(default)
}

/// `calculateCooldownDuration(consecutiveFailures)`.
pub fn duration_ms(state: &AppState, consecutive_failures: u32) -> i64 {
    let initial = setting_f64(state, "cooldown.initialMinutes", 2.0) * 60_000.0;
    let max = setting_f64(state, "cooldown.maxMinutes", 300.0) * 60_000.0;
    (initial * 2f64.powi(consecutive_failures as i32)).min(max) as i64
}

/// `markProviderFailure(provider, model, durationMs?, lastError?)`.
pub async fn mark_failure(state: &AppState, provider: &str, model: &str, duration: Option<i64>, last_error: Option<String>) {
    if cooldown_disabled(state, provider) { return; }
    if last_error.as_ref().is_some_and(|e| { let l = e.to_lowercase(); DEADLINE_EXPIRED_PATTERNS.iter().any(|p| l.contains(p)) }) { return; }
    let entry = {
        let mut map = state.inner.cooldowns.write().unwrap();
        let key = (provider.to_owned(), model.to_owned());
        let failures = map.get(&key).map(|e| e.consecutive_failures).unwrap_or(0) + 1;
        let duration = duration.filter(|d| *d != 0).unwrap_or_else(|| duration_ms(state, failures - 1));
        let entry = CooldownEntry { expiry_ms: now_ms() + duration, consecutive_failures: failures, last_error };
        map.insert(key, entry.clone());
        entry
    };
    // Capture-on-error: persist the triggering request's debug trace.
    crate::engine::dispatcher::debug::DebugManager::new(state).mark_force_persist_current();
    if let Some(pool) = &state.inner.database {
        let _ = crate::database::upsert_cooldown(pool, provider, model, &entry).await;
    }
}

/// `markProviderStallFailure`.
pub async fn mark_stall_failure(state: &AppState, provider: &str, model: &str, last_error: Option<String>) {
    if !stall_cooldown_enabled(state, provider) { return; }
    mark_failure(state, provider, model, None, last_error).await;
}

/// `markProviderSuccess`.
pub async fn mark_success(state: &AppState, provider: &str, model: &str) {
    let removed = state.inner.cooldowns.write().unwrap().remove(&(provider.to_owned(), model.to_owned()));
    if removed.is_none() { return; }
    if let Some(pool) = &state.inner.database {
        let _ = crate::database::delete_cooldown(pool, provider, model).await;
    }
}

/// `isProviderHealthy` (provider-wide entry checked first).
pub async fn is_healthy(state: &AppState, provider: &str, model: &str) -> bool {
    if !model.is_empty() && !Box::pin(is_healthy(state, provider, "")).await { return false; }
    let key = (provider.to_owned(), model.to_owned());
    let expired = {
        let mut map = state.inner.cooldowns.write().unwrap();
        let Some(entry) = map.get(&key).cloned() else { return true };
        if entry.expiry_ms == 0 { return true; }
        if now_ms() <= entry.expiry_ms { return false; }
        map.insert(key.clone(), CooldownEntry { expiry_ms: 0, consecutive_failures: entry.consecutive_failures, last_error: None });
        true
    };
    if expired {
        if let Some(pool) = &state.inner.database { let _ = crate::database::delete_cooldown(pool, provider, model).await; }
    }
    true
}
