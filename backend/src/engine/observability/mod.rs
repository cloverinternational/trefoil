//! Usage accounting, cost calculation and response finalization.
//!
//! Ports of the pinned Plexus sources:
//! - `record`: `types/usage.ts` (`UsageRecord`, column map) + JS number helpers.
//! - `costs`: `utils/calculate-costs.ts`, `utils/provider-cost.ts`,
//!   `services/observability/pricing-manager.ts`, SSE `: cost` / `: energy` readers.
//! - `estimate`: `utils/estimate-tokens.ts` (tokenizer pluggable, heuristic fallback).
//! - `inspect`: pure parts of `services/inspectors/usage-logging.ts`, actual
//!   service tier capture, stream visibility tracking.
//! - `storage`: `services/observability/usage-storage.ts` on `sqlx::PgPool`.
//! - `response`: `services/responses/response-handler.ts` + the streaming
//!   `UsageInspector` lifecycle, as a framework-neutral reply.
//! - `hooks`: traits for the Plexus singletons (debug manager, quota,
//!   cooldowns, transformer factory, live usage events, request context).
pub mod costs;
pub mod estimate;
pub mod hooks;
pub mod inspect;
pub mod record;
pub mod response;
pub mod storage;

pub use costs::{calculate_costs, CostAttribution, OpenRouterCatalog, PricingManager};
pub use hooks::*;
pub use record::UsageRecord;
pub use response::{
    handle_response, AbortKind, HandledBody, HandledResponse, ResponseContext, ResponseServices,
    StreamSession,
};
pub use storage::UsageStorage;
