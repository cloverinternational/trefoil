//! Seams to Plexus singletons the response/usage pipeline reaches for.
//! Every method has a no-op default so a partial implementation compiles;
//! each documents the TypeScript call it replaces.
use super::record::UsageRecord;
use serde_json::Value;
use std::sync::Arc;

/// Live usage event kinds (`UsageStorageService` `EventEmitter` names).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageEventKind {
    /// `'started'`: a pending row was inserted (payload has `responseStatus: 'pending'`).
    Started,
    /// `'updated'`: partial record as routing resolves provider/model.
    Updated,
    /// `'created'`: emitted by `saveRequest` (back-compat), before `Completed`.
    Created,
    /// `'completed'`: emitted by `saveRequest` with the full record.
    Completed,
}

impl UsageEventKind {
    pub fn as_str(self) -> &'static str {
        match self { Self::Started => "started", Self::Updated => "updated", Self::Created => "created", Self::Completed => "completed" }
    }
}

/// Live/SSE fan-out of usage records (the dashboard's `/events` stream).
/// Called synchronously after the DB write, with the camelCase record JSON
/// exactly as Plexus emits it (booleans not converted to ints).
pub trait UsageEvents: Send + Sync {
    fn emit(&self, kind: UsageEventKind, record: &Value);
}

/// `getCurrentKeyName()` (AsyncLocalStorage request context seeded by the
/// v1 auth middleware); in Rust typically a tokio task-local.
pub trait KeyNameContext: Send + Sync {
    fn current_key_name(&self) -> Option<String>;
}

/// `new Error(message)` as persisted by `saveError`.
#[derive(Debug, Clone, PartialEq)]
pub struct ErrorInfo {
    pub message: String,
    /// `error.stack` (`None` → SQL NULL). Handler-created errors use the
    /// reproducible header `Error: <message>` (same convention as
    /// `execute::AttemptError::stack_header`).
    pub stack: Option<String>,
}

impl ErrorInfo {
    pub fn new(message: impl Into<String>) -> Self {
        let message = message.into();
        Self { stack: Some(format!("Error: {message}")), message }
    }
}

/// Arguments of `updatePerformanceMetrics(...)`.
#[derive(Debug, Clone, PartialEq)]
pub struct PerformanceSample {
    pub provider: String,
    pub model: String,
    pub canonical_model_name: Option<String>,
    pub time_to_first_token_ms: Option<f64>,
    pub output_tokens: Option<f64>,
    pub duration_ms: f64,
    pub request_id: String,
    pub success: bool,
}

/// The `UsageStorageService` surface `handleResponse` / `UsageInspector`
/// use. [`super::storage::UsageStorage`] is the Postgres implementation.
#[async_trait::async_trait]
pub trait UsageSink: Send + Sync {
    /// `saveRequest(record)`: full upsert of the `request_usage` row.
    async fn save_request(&self, record: &UsageRecord);
    /// `saveError(requestId, error, details, apiKey)`.
    async fn save_error(&self, request_id: &str, error: &ErrorInfo, details: Option<&Value>, api_key: Option<&str>);
    /// `updatePerformanceMetrics(...)`.
    async fn update_performance_metrics(&self, sample: &PerformanceSample);
    /// `registerInFlight(requestId, stallInspector, apiKey, isStreamed)`;
    /// the stall inspector itself is owned by the caller.
    fn register_in_flight(&self, _request_id: &str, _api_key: Option<&str>, _is_streamed: bool) {}
    /// `deregisterInFlight(requestId)`.
    fn deregister_in_flight(&self, _request_id: &str) {}
}

/// `DebugManager` + `DebugLoggingInspector` taps.
///
/// Usage extraction prefers `reconstructed_raw_response` (Plexus's
/// `getReconstructedRawResponse`): if the implementation reconstructs the
/// provider stream in `raw_end` (port of `DebugLoggingInspector`), streaming
/// usage/metadata extraction is byte-for-byte Plexus. When it returns `None`
/// the response handler falls back to its built-in observation
/// (`Transformer::extract_usage` per SSE event, unified-chunk metadata).
pub trait DebugHooks: Send + Sync {
    fn set_provider_for_request(&self, _request_id: &str, _provider: &str) {}
    /// `setModelAliasForRequest(requestId, alias, originalRequest, sanitizeHeaders(headers))`;
    /// headers are passed raw — sanitize in the implementation.
    fn set_model_alias_for_request(&self, _request_id: &str, _alias: Option<&str>, _original_request: Option<&Value>, _headers: &[(String, String)]) {}
    fn is_enabled(&self) -> bool { false }
    fn set_enabled(&self, _enabled: bool) {}
    fn mark_ephemeral(&self, _request_id: &str) {}
    fn discard_ephemeral(&self, _request_id: &str) {}
    fn add_transformed_response(&self, _request_id: &str, _body: &Value) {}
    fn flush(&self, _request_id: &str) {}
    fn mark_force_persist(&self, _request_id: &str) {}
    /// True when `raw_end` runs the real `DebugLoggingInspector`
    /// reconstruction; the response handler then never substitutes its
    /// built-in observation for a missing snapshot.
    fn reconstructs(&self) -> bool { false }
    /// `getReconstructedRawResponse(requestId)` (`rawResponseSnapshot || null`).
    fn reconstructed_raw_response(&self, _request_id: &str) -> Option<Value> { None }
    /// `getPendingLog(requestId)?.transformedResponseSnapshot`.
    fn transformed_response_snapshot(&self, _request_id: &str) -> Option<Value> { None }
    /// Raw provider bytes (`DebugLoggingInspector(requestId, 'raw')` tap), keyed by provider api type.
    fn raw_chunk(&self, _request_id: &str, _provider_api_type: &str, _text: &str) {}
    /// Raw tap ended / `finalize()` (idempotent; called on normal end and on teardown).
    fn raw_end(&self, _request_id: &str, _provider_api_type: &str) {}
    /// Client-bound text (`'transformed'` tap), keyed by the client api type.
    fn transformed_chunk(&self, _request_id: &str, _api_type: &str, _text: &str) {}
    fn transformed_end(&self, _request_id: &str, _api_type: &str) {}
}

/// No-op debug manager.
pub struct NoDebug;
impl DebugHooks for NoDebug {}

/// `{ tokensInput, tokensOutput, tokensCached, tokensCacheWrite, tokensReasoning, costTotal }`
/// (`null` → `undefined`, i.e. `None`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct QuotaUsage {
    pub tokens_input: Option<f64>,
    pub tokens_output: Option<f64>,
    pub tokens_cached: Option<f64>,
    pub tokens_cache_write: Option<f64>,
    pub tokens_reasoning: Option<f64>,
    pub cost_total: Option<f64>,
}

impl QuotaUsage {
    pub fn from_record(r: &UsageRecord) -> Self {
        let n = |k: &str| r.f64(k);
        Self { tokens_input: n("tokensInput"), tokens_output: n("tokensOutput"), tokens_cached: n("tokensCached"), tokens_cache_write: n("tokensCacheWrite"), tokens_reasoning: n("tokensReasoning"), cost_total: n("costTotal") }
    }
}

/// `quota-middleware.ts`: `buildQuotaHeaders` and `recordQuotaUsage`.
#[async_trait::async_trait]
pub trait QuotaHooks: Send + Sync {
    /// `buildQuotaHeaders(request.quotaContext, finalProvider || '', finalModel || '')`.
    fn quota_headers(&self, _quota_context: &Value, _provider: &str, _model: &str) -> Vec<(String, String)> { Vec::new() }
    /// `recordQuotaUsage(keyName, finalProvider, finalModel, usage, quotaEnforcer)`
    /// → `quotaEnforcer.recordUsage(keyName, provider ?? '', model ?? '', usage)`; errors are swallowed.
    async fn record_usage(&self, _key_name: &str, _provider: &str, _model: &str, _usage: &QuotaUsage) {}
}

/// Provider/model cooldown seam (`CooldownManager.markProviderStallFailure`).
pub trait CooldownHooks: Send + Sync {
    fn mark_provider_stall_failure(&self, _provider: &str, _model: &str, _message: &str) {}
}

/// `TransformerFactory.getTransformer(apiType)` for the provider side of a stream.
pub trait TransformerRegistry: Send + Sync {
    fn transformer(&self, api_type: &str) -> Option<Arc<dyn crate::engine::transformer::Transformer>>;
}

/// Defect signalled by a raw-stream rewriter (Gemini `MALFORMED_FUNCTION_CALL`).
#[derive(Debug, Clone, PartialEq)]
pub struct StreamDefect {
    pub code: String,
    pub message: String,
    pub status_code: u16,
    pub text_leak_detected: bool,
}

/// `rewriteGeminiMalformedFunctionCallStream`: line-based rewriter applied to
/// raw provider text when the provider api type is `gemini`
/// (`engine::gemini::malformed::MalformedFunctionCallRewriter` adapts directly).
pub trait RawStreamRewriter: Send {
    fn feed(&mut self, text: &str) -> (String, Vec<StreamDefect>);
    fn flush(&mut self) -> (String, Vec<StreamDefect>);
}

/// `StallInspector` placement in the client pipeline: it observes every
/// client-bound chunk. Abort decisions are made by the caller (who owns the
/// timers) and reported through [`super::response::StreamSession::abort`].
pub trait StallObserver: Send {
    fn set_request_id(&mut self, _request_id: &str) {}
    fn set_progress_api_type(&mut self, _api_type: &str) {}
    fn on_chunk(&mut self, _chunk: &str) {}
}
