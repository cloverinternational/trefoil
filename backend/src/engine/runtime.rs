//! Process-wide services the engine reads, as a trait so the port stays
//! decoupled from Trefoil's state/storage. Mirrors the Plexus singletons the
//! TypeScript reaches for (`getConfig()`, `CooldownManager`,
//! `ConcurrencyTracker`, `UsageStorageService`, `QuotaScheduler`, …).
//! Methods are added as engine modules need them; each documents the TS
//! call it replaces. Trefoil implements it in `crate::engine_runtime`.
use super::config::Config;
use std::sync::Arc;

#[async_trait::async_trait]
pub trait Runtime: Send + Sync {
    /// `getConfig()`.
    fn config(&self) -> Arc<Config>;
}
