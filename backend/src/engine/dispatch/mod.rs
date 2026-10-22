//! Port of the Plexus per-attempt request pipeline for API-key routes
//! (`services/dispatch/*`, `services/models/model-behaviors.ts`,
//! `transformers/adapters/*`, the pi-ai registry helpers it imports).
//!
//! Module ↔ TypeScript file:
//! - `request_payload` — `request-payload-builder.ts` (+ `setupProviderHeaders`,
//!   `Dispatcher.setupHeaders` / `buildRequestUrl`).
//! - `adapters` / `adapter_resolver` — `transformers/adapters/*`,
//!   `types/provider-adapter.ts`, `adapter-resolver.ts`.
//! - `auto_compat` — `dispatcher-auto-compat.ts` (+ `transformers/utils.ts`
//!   `projectReasoningForResponses` / `getThinkLevel`).
//! - `pi_ai` — `services/pi-ai/{reasoning,generation,registry}.ts` and
//!   pi-ai's thinking-level clamp over the bundled 1.2.0 catalog.
//! - `attempt_history`, `errors`, `failover_policy`, `cache_key_injection`,
//!   `cache_retention`, `service_tier_selection`, `service_tier_metadata`,
//!   `eager_tool_streaming`, `empty_completion`, `model_behaviors`,
//!   `responses_extensions`, `oauth_routes` — the same-named TS files
//!   (`errors` = dispatcher.ts pure helpers + `sanitize-headers.ts`).
//!
//! Requests, payloads and errors are Plexus-shaped JSON (`engine::js`
//! conventions); `logger` calls are not ported.
pub mod adapter_resolver;
pub mod adapters;
pub mod attempt_history;
pub mod auto_compat;
pub mod cache_key_injection;
pub mod cache_retention;
pub mod eager_tool_streaming;
pub mod empty_completion;
pub mod errors;
pub mod failover_policy;
pub mod model_behaviors;
pub mod oauth_routes;
pub mod pi_ai;
pub mod request_payload;
pub mod responses_extensions;
pub mod service_tier_metadata;
pub mod service_tier_selection;
pub mod util;

pub use adapter_resolver::resolve_adapters;
pub use adapters::{ProviderAdapter, ResolvedAdapter};
pub use attempt_history::RetryAttemptRecord;
pub use request_payload::{
    apply_auto_anthropic_betas, build_request_payload, build_request_url, setup_headers,
    RequestPayload,
};

#[cfg(test)]
mod tests;
