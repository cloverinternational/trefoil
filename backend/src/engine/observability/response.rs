//! `services/responses/response-handler.ts` (`handleResponse`,
//! `finalizeUsage`) and the stream-side `UsageInspector` lifecycle from
//! `services/inspectors/usage-logging.ts`.
//!
//! HTTP output is framework-neutral: a status, headers and either a JSON
//! body or a push-based [`StreamSession`] that the caller drives with the
//! upstream bytes (`push`), then `finish` (upstream ended) or `abort`
//! (client disconnect / timeout / stall). Each call returns the client-bound
//! text chunks to write, in order.
use super::costs::{apply_provider_reported_cost, apply_usage_cost_details, calculate_costs, CostAttribution, OpenRouterCatalog};
use super::estimate::{estimate_input_tokens, estimate_tokens, estimate_tokens_from_reconstructed};
use super::hooks::*;
use super::inspect::{extract_actual_service_tier, extract_response_metadata, extract_usage_from_reconstructed, find_actual_service_tier_raw, ObservedUsage, ResponseMetadata, StreamVisibilityTracker};
use super::record::{to_fixed_num, UsageRecord};
use crate::engine::anthropic::synthetic_safeguards as safeguards;
use crate::engine::config::{api_format, Config};
use crate::engine::js::{self, get, get_str, now_ms, truthy, Obj};
use crate::engine::sse::{self, SseParser};
use crate::engine::transformer::{EngineResult, StreamFormatter, StreamTransformer, Transformer};
use crate::engine::usage::extract_cost_details;
use serde_json::{json, Value};
use std::sync::Arc;

/// Process-wide collaborators of `handleResponse`.
#[derive(Clone)]
pub struct ResponseServices {
    /// `UsageStorageService` (saveRequest / saveError / updatePerformanceMetrics).
    pub sink: Arc<dyn UsageSink>,
    /// `getConfig()` (synthetic-safeguard alias toggle).
    pub config: Arc<Config>,
    pub debug: Arc<dyn DebugHooks>,
    /// `TransformerFactory.getTransformer` for the provider side of streams.
    pub transformers: Arc<dyn TransformerRegistry>,
    /// Quota headers (whenever a quota context exists) and usage recording.
    pub quota: Option<Arc<dyn QuotaHooks>>,
    pub cooldowns: Option<Arc<dyn CooldownHooks>>,
    /// OpenRouter pricing catalog for `pricing.source === 'openrouter'`.
    pub openrouter: Option<Arc<dyn OpenRouterCatalog>>,
    /// Factory for the Gemini `MALFORMED_FUNCTION_CALL` raw-stream rewriter.
    pub gemini_rewriter: Option<Arc<dyn Fn() -> Box<dyn RawStreamRewriter> + Send + Sync>>,
    /// `process.env.ADMIN_KEY` (playground routing metadata gate).
    pub admin_key: Option<String>,
}

/// Per-request inputs of `handleResponse`.
pub struct ResponseContext {
    /// `usageRecord` (mutated in place and persisted).
    pub record: UsageRecord,
    pub start_time: i64,
    /// Client api type: `chat` | `messages` | `gemini` | `responses` | `completions`.
    pub api_type: String,
    pub should_estimate_tokens: bool,
    pub original_request: Option<Value>,
    /// `true` when the route passed a `quotaEnforcer`.
    pub quota_enforcer: bool,
    pub key_name: Option<String>,
    /// Incoming request headers (lower-case names preferred).
    pub request_headers: Vec<(String, String)>,
    /// `request.quotaContext` stashed by `checkQuotaMiddleware`.
    pub quota_context: Option<Value>,
}

/// Response body.
pub enum HandledBody {
    Json(Value),
    Stream(Box<StreamSession>),
}

/// Framework-neutral reply.
pub struct HandledResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: HandledBody,
    /// Final usage record for unary replies (the stream session owns it otherwise).
    pub record: Option<UsageRecord>,
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
}

/// `hasValidAdminKey`: SHA-256 both sides, constant-time compare.
pub fn has_valid_admin_key(admin_key: Option<&str>, headers: &[(String, String)]) -> bool {
    use sha2::{Digest, Sha256};
    use subtle::ConstantTimeEq;
    let (Some(admin), Some(provided)) = (admin_key.filter(|k| !k.is_empty()), header(headers, "x-admin-key").filter(|k| !k.is_empty())) else { return false };
    let a = Sha256::digest(admin.as_bytes());
    let b = Sha256::digest(provided.as_bytes());
    a.ct_eq(&b).into()
}

/// `shouldIncludePlaygroundRouting`.
pub fn should_include_playground_routing(admin_key: Option<&str>, headers: &[(String, String)]) -> bool {
    header(headers, "x-plexus-playground") == Some("true") && has_valid_admin_key(admin_key, headers)
}

/// `formatClientError(apiType, error)`.
pub fn format_client_error(api_type: &str, error: &Value) -> Value {
    match api_type {
        "gemini" => json!({"error": {"code": error.get("statusCode").cloned().unwrap_or(Value::Null), "status": "UNAVAILABLE", "message": error.get("message").cloned().unwrap_or(Value::Null)}}),
        "messages" => json!({"type": "error", "error": {"type": "api_error", "message": error.get("message").cloned().unwrap_or(Value::Null)}}),
        _ => json!({"error": {"message": error.get("message").cloned().unwrap_or(Value::Null), "type": "server_error", "code": "upstream_malformed_function_call"}}),
    }
}

/// `{ apiType, provider, targetModel, statusCode, code, clientSignaled: true, ...extra }`.
fn client_signal_details(api_type: &str, record: &UsageRecord, status: Option<Value>, code: Option<Value>, extra: Option<Obj>) -> Value {
    let mut m = Obj::new();
    m.insert("apiType".into(), api_type.into());
    js::set_opt(&mut m, "provider", record.get("provider").cloned());
    js::set_opt(&mut m, "targetModel", record.get("selectedModelName").cloned());
    js::set_opt(&mut m, "statusCode", status);
    js::set_opt(&mut m, "code", code);
    m.insert("clientSignaled".into(), Value::Bool(true));
    if let Some(e) = extra { for (k, v) in e { m.insert(k, v); } }
    Value::Object(m)
}

/// Shared, per-request pricing inputs.
#[derive(Clone)]
struct Pricing {
    pricing: Option<Value>,
    provider_discount: Option<Value>,
    attribution: CostAttribution,
}

/// Provider cost / energy overrides shared by `finalizeUsage` and `UsageInspector.finalize`.
fn apply_provider_overrides(record: &mut UsageRecord, reported_cost: Option<&Value>, usage_block: Option<&Value>, energy: Option<&Value>) {
    if let Some(cost) = reported_cost.filter(|c| truthy(Some(c))) {
        apply_provider_reported_cost(record, cost);
    }
    if !record.truthy("providerReportedCost") {
        if let Some(details) = usage_block.filter(|u| truthy(Some(u))).and_then(extract_cost_details) {
            apply_usage_cost_details(record, &details);
        }
    }
    if let Some(e) = energy.and_then(|e| e.get("energy_kwh")).filter(|v| !v.is_null()) {
        if let Some(kwh) = js::to_number(Some(e)).filter(|k| !k.is_nan() && *k >= 0.0) {
            record.set_num("kwhUsed", to_fixed_num(kwh, 10));
        }
    }
}

async fn record_quota(svc_quota: Option<&Arc<dyn QuotaHooks>>, enabled: bool, key_name: Option<&str>, record: &UsageRecord) {
    let (Some(q), true, Some(key)) = (svc_quota, enabled, key_name.filter(|k| !k.is_empty())) else { return };
    let provider = record.str("finalAttemptProvider").unwrap_or("").to_owned();
    let model = record.str("finalAttemptModel").unwrap_or("").to_owned();
    q.record_usage(key, &provider, &model, &QuotaUsage::from_record(record)).await;
}

/// `finalizeUsage(...)` (unary + client-error paths).
#[allow(clippy::too_many_arguments)]
async fn finalize_usage(svc: &ResponseServices, record: &mut UsageRecord, unified: &Value, start_time: i64, pricing: &Pricing, quota_enforcer: bool, key_name: Option<&str>, response_status: &str, update_performance_metrics: bool) {
    if let Some(usage) = unified.get("usage").filter(|u| truthy(Some(u))) {
        record.set_opt("tokensInput", usage.get("input_tokens").cloned());
        record.set_opt("tokensOutput", usage.get("output_tokens").cloned());
        record.set_opt("tokensCached", usage.get("cached_tokens").cloned());
        record.set_opt("tokensCacheWrite", usage.get("cache_creation_tokens").cloned());
        record.set_opt("tokensReasoning", usage.get("reasoning_tokens").cloned());
    }
    let tool_calls_count = match unified.get("tool_calls") {
        Some(Value::Array(a)) => Value::from(a.len()),
        Some(Value::String(s)) => Value::from(s.encode_utf16().count()),
        _ => Value::Null,
    };
    record.set("toolCallsCount", tool_calls_count);
    let finish = js::coalesce(get(unified, &["clientError", "code"]), js::coalesce(unified.get("finishReason"), None).as_ref()).unwrap_or(Value::Null);
    record.set("finishReason", finish);

    calculate_costs(record, pricing.pricing.as_ref(), pricing.provider_discount.as_ref(), Some(&pricing.attribution), svc.openrouter.as_deref());
    let reconstructed = svc.debug.reconstructed_raw_response(&record.request_id()).filter(|r| truthy(Some(r)));
    apply_provider_overrides(record, reconstructed.as_ref().and_then(|r| r.get("providerReportedCost")), reconstructed.as_ref().and_then(|r| r.get("usage")), reconstructed.as_ref().and_then(|r| r.get("providerReportedEnergy")));

    record.set("responseStatus", response_status);
    let duration = (now_ms() - start_time) as f64;
    record.set_num("durationMs", duration);
    let total_output = record.num_or_zero("tokensOutput") + record.num_or_zero("tokensReasoning");
    record.set_num("ttftMs", duration);
    if total_output > 0.0 && duration > 0.0 { record.set_num("tokensPerSec", (total_output / duration) * 1000.0); }

    svc.sink.save_request(record).await;

    if update_performance_metrics && record.truthy("provider") && record.truthy("selectedModelName") {
        svc.sink.update_performance_metrics(&PerformanceSample {
            provider: js::to_string(record.get("provider")),
            model: js::to_string(record.get("selectedModelName")),
            canonical_model_name: record.str("canonicalModelName").map(str::to_owned),
            time_to_first_token_ms: Some(duration),
            output_tokens: if total_output > 0.0 { Some(total_output) } else { None },
            duration_ms: duration,
            request_id: record.request_id(),
            success: true,
        }).await;
    }
    record_quota(svc.quota.as_ref(), quota_enforcer, key_name, record).await;
}

/// `handleResponse(...)`. `is_stream` is `!!unifiedResponse.stream` (the
/// upstream body is then fed to the returned session); `stall` is the
/// optional `StallInspector` seam.
pub async fn handle_response(svc: &ResponseServices, ctx: ResponseContext, mut unified: Value, client: Arc<dyn Transformer>, is_stream: bool, stall: Option<Box<dyn StallObserver>>) -> EngineResult<HandledResponse> {
    let ResponseContext { mut record, start_time, api_type, should_estimate_tokens, original_request, quota_enforcer, key_name, request_headers, quota_context } = ctx;
    let plexus = unified.get("plexus").cloned();
    let p = |k: &str| plexus.as_ref().and_then(|p| p.get(k));
    let rid = record.request_id();

    record.set_opt("selectedModelName", js::or(p("model"), unified.get("model")));
    record.set("provider", js::or(p("provider"), Some(&json!("unknown"))).unwrap_or(Value::Null));
    record.set("canonicalModelName", js::or(p("canonicalModel"), None).unwrap_or(Value::Null));
    if record.truthy("provider") { svc.debug.set_provider_for_request(&rid, &js::to_string(record.get("provider"))); }
    let alias = js::or(record.get("canonicalModelName"), record.get("incomingModelAlias"));
    svc.debug.set_model_alias_for_request(&rid, alias.as_ref().and_then(Value::as_str), original_request.as_ref(), &request_headers);
    record.set("attemptCount", js::or(p("attemptCount"), Some(&json!(1))).unwrap_or(Value::Null));
    record.set("retryHistory", js::or(p("retryHistory"), None).unwrap_or(Value::Null));
    let fap = js::or(p("finalAttemptProvider"), record.get("provider")).unwrap_or(Value::Null);
    record.set("finalAttemptProvider", fap);
    let fam = js::or(p("finalAttemptModel"), record.get("selectedModelName")).unwrap_or(Value::Null);
    record.set("finalAttemptModel", fam);
    let all = match js::or(p("allAttemptedProviders"), None) {
        Some(v) => v,
        None => {
            let provider = js::or(record.get("provider"), Some(&json!("unknown")));
            let model = js::or(record.get("selectedModelName"), unified.get("model"));
            Value::String(js::stringify(&json!([format!("{}/{}", js::to_string(provider.as_ref()), js::to_string(model.as_ref()))])))
        }
    };
    record.set("allAttemptedProviders", all);
    let upstream = js::or(p("upstreamModel"), record.get("finalAttemptModel")).unwrap_or(Value::Null);
    record.set("upstreamModel", upstream);
    for (field, key) in [("requestedServiceTier", "requestedServiceTier"), ("requestedServiceTierRaw", "requestedServiceTierRaw"), ("serviceTier", "serviceTier"), ("serviceTierRaw", "serviceTierRaw")] {
        record.set(field, js::coalesce(p(key), None).unwrap_or(Value::Null));
    }
    let outgoing = p("apiType").and_then(Value::as_str).map(str::to_lowercase);
    record.set_opt("outgoingApiType", outgoing.map(Value::String));
    record.set("isStreamed", is_stream);
    record.set_opt("isPassthrough", unified.get("bypassTransformation").cloned());
    let bypass = truthy(unified.get("bypassTransformation"));

    let mut headers: Vec<(String, String)> = vec![("x-request-id".into(), rid.clone())];
    if let (Some(qc), Some(q)) = (quota_context.as_ref().filter(|q| !q.is_null()), svc.quota.as_ref()) {
        headers.extend(q.quota_headers(qc, record.str("finalAttemptProvider").unwrap_or(""), record.str("finalAttemptModel").unwrap_or("")));
    }

    let pricing = Pricing { pricing: p("pricing").cloned(), provider_discount: p("providerDiscount").cloned(), attribution: CostAttribution::from_plexus(plexus.as_ref()) };
    let provider_api_type = api_format::base_type(&js::or(p("apiType"), Some(&json!("chat"))).map(|v| js::to_string(Some(&v))).unwrap_or_default());

    let was_debug_enabled = svc.debug.is_enabled();
    if should_estimate_tokens {
        svc.debug.mark_ephemeral(&rid);
        if !was_debug_enabled { svc.debug.set_enabled(true); }
    }
    let restore_debug = should_estimate_tokens && !was_debug_enabled;

    // Client-signalled transient error (unary only).
    if !is_stream && truthy(unified.get("clientError")) {
        let client_error = unified["clientError"].clone();
        let body = format_client_error(&api_type, &client_error);
        svc.debug.add_transformed_response(&rid, &body);
        svc.debug.flush(&rid);
        finalize_usage(svc, &mut record, &unified, start_time, &pricing, quota_enforcer, key_name.as_deref(), "error", false).await;
        let details = client_signal_details(&api_type, &record, client_error.get("statusCode").cloned(), client_error.get("code").cloned(), None);
        svc.sink.save_error(&rid, &ErrorInfo::new(js::to_string(client_error.get("message"))), Some(&details), key_name.as_deref()).await;
        if restore_debug { svc.debug.set_enabled(false); }
        let status = client_error.get("statusCode").and_then(Value::as_u64).map(|s| s as u16).unwrap_or(500);
        headers.push(("content-type".into(), "application/json; charset=utf-8".into()));
        return Ok(HandledResponse { status, headers, body: HandledBody::Json(body), record: Some(record) });
    }

    if is_stream {
        let provider_transformer = svc.transformers.transformer(&provider_api_type);
        let rewriter = if provider_api_type == "gemini" { svc.gemini_rewriter.as_ref().map(|f| f()) } else { None };
        let mut safeguard = None;
        if api_type == "messages" && !bypass {
            let alias_toggle = safeguards::resolve_synthetic_safeguard_toggle(p("canonicalModel").and_then(Value::as_str), svc.config.root.get("models"));
            let gate = safeguards::SafeguardSynthesisGate {
                incoming_api_type: Some(&api_type), original_body: original_request.as_ref(), alias_toggle,
                outgoing_api_type: p("apiType").and_then(Value::as_str), bypass_transformation: bypass,
                has_client_error: truthy(unified.get("clientError")), has_existing_results: false,
            };
            if safeguards::should_synthesize_safeguards(&gate) {
                safeguard = Some(safeguards::SyntheticSafeguardStream::new(safeguards::get_requested_safeguard_types(original_request.as_ref())));
            }
        }
        let mut stall = stall;
        if let Some(s) = stall.as_mut() {
            s.set_request_id(&rid);
            s.set_progress_api_type(&api_type);
            svc.sink.register_in_flight(&rid, record.str("apiKey"), is_stream);
        }
        headers.push(("Content-Type".into(), "text/event-stream".into()));
        headers.push(("Cache-Control".into(), "no-cache".into()));
        headers.push(("Connection".into(), "keep-alive".into()));
        record.set("responseStatus", "success");
        let session = StreamSession::new(StreamInit {
            svc: svc.clone(), record, start_time, api_type, provider_api_type, should_estimate_tokens, restore_debug,
            original_request, quota_enforcer, key_name, pricing, bypass, provider_transformer, client, rewriter, safeguard, stall,
        });
        return Ok(HandledResponse { status: 200, headers, body: HandledBody::Stream(Box::new(session)), record: None });
    }

    // --- Unary ---
    let playground = if should_include_playground_routing(svc.admin_key.as_deref(), &request_headers) {
        let mut m = Obj::new();
        js::set_opt(&mut m, "requestId", record.get("requestId").cloned());
        if let Some(Value::Object(pm)) = &plexus { for (k, v) in pm { m.insert(k.clone(), v.clone()); } }
        Some(Value::Object(m))
    } else { None };
    let canonical_snapshot = p("canonicalModel").and_then(Value::as_str).map(str::to_owned);
    let api_type_snapshot = p("apiType").and_then(Value::as_str).map(str::to_owned);
    let client_error_snapshot = truthy(unified.get("clientError"));
    let tool_calls_snapshot = unified.get("tool_calls").cloned();
    if let Some(m) = unified.as_object_mut() { m.remove("plexus"); }

    let mut body = match unified.get("rawResponse").filter(|r| bypass && truthy(Some(r))) {
        Some(raw) => raw.clone(),
        None => client.format_response(&unified)?,
    };
    if api_type == "messages" && body.is_object() && body.get("safeguard_results").is_none() {
        let requested = safeguards::get_requested_safeguard_types(original_request.as_ref());
        let alias_toggle = safeguards::resolve_synthetic_safeguard_toggle(canonical_snapshot.as_deref(), svc.config.root.get("models"));
        let gate = safeguards::SafeguardSynthesisGate {
            incoming_api_type: Some(&api_type), original_body: original_request.as_ref(), alias_toggle,
            outgoing_api_type: api_type_snapshot.as_deref(), bypass_transformation: bypass, has_client_error: client_error_snapshot, has_existing_results: false,
        };
        if safeguards::should_synthesize_safeguards(&gate) {
            let ids = safeguards::collect_unified_tool_ids(tool_calls_snapshot.as_ref());
            body["safeguard_results"] = Value::Array(safeguards::build_synthetic_safeguard_results(&ids, &requested));
        }
    }
    if let (Some(pg), Some(m)) = (playground, body.as_object_mut()) { m.insert("plexus".into(), pg); }

    svc.debug.add_transformed_response(&rid, &body);
    svc.debug.flush(&rid);
    if restore_debug { svc.debug.set_enabled(false); }
    finalize_usage(svc, &mut record, &unified, start_time, &pricing, quota_enforcer, key_name.as_deref(), "success", true).await;
    headers.push(("content-type".into(), "application/json; charset=utf-8".into()));
    Ok(HandledResponse { status: 200, headers, body: HandledBody::Json(body), record: Some(record) })
}

// ---------------------------------------------------------------------------
// Streaming
// ---------------------------------------------------------------------------

/// Why a stream was torn down before completing.
#[derive(Debug, Clone, PartialEq)]
pub enum AbortKind {
    /// Client went away (`responseStatus = 'cancelled'`). For
    /// `incomingApiType === 'responses:lite'` Plexus only logs a socket-close
    /// observation (see [`StreamSession::defers_socket_close`]).
    ClientDisconnect,
    /// Absolute upstream timeout (`'timeout'`).
    Timeout,
    /// Stall detection fired (`'stall'`, provider/model stall cooldown).
    Stall { message: Option<String> },
}

/// Incremental UTF-8 decoding (`TextDecoder` with `stream: true`).
#[derive(Default)]
struct Utf8Decoder { carry: Vec<u8> }

impl Utf8Decoder {
    fn decode(&mut self, bytes: &[u8], last: bool) -> String {
        self.carry.extend_from_slice(bytes);
        let mut out = String::new();
        let mut buf = std::mem::take(&mut self.carry);
        loop {
            match std::str::from_utf8(&buf) {
                Ok(s) => { out.push_str(s); break; }
                Err(e) => {
                    let valid = e.valid_up_to();
                    out.push_str(std::str::from_utf8(&buf[..valid]).unwrap_or_default());
                    match e.error_len() {
                        Some(n) => { out.push('\u{FFFD}'); buf.drain(..valid + n); }
                        None => {
                            if last { out.push('\u{FFFD}'); } else { self.carry = buf[valid..].to_vec(); }
                            break;
                        }
                    }
                }
            }
        }
        out
    }
}

/// Built-in stand-in for the debug reconstruction, used when
/// `DebugHooks::reconstructed_raw_response` yields nothing: provider usage
/// via `Transformer::extract_usage` per SSE event (later non-zero values win
/// per field), SSE `: cost` / `: energy` comments, the last provider-reported
/// service tier, and finish reason / tool calls / text from unified chunks.
#[derive(Default)]
struct BuiltinObservation {
    seen: bool,
    usage: Option<Obj>,
    line_buf: String,
    cost: Option<Value>,
    energy: Option<Value>,
    tier_raw: Option<String>,
    finish_reason: Option<Value>,
    tool_call_keys: Vec<String>,
    output_text: String,
    reasoning_text: String,
}

impl BuiltinObservation {
    fn merge_usage(&mut self, u: &Value) {
        let Some(obj) = u.as_object() else { return };
        let merged = self.usage.get_or_insert_with(Obj::new);
        for (k, v) in obj {
            let nonzero = v.as_f64().is_some_and(|n| n != 0.0);
            if nonzero || !merged.contains_key(k) { merged.insert(k.clone(), v.clone()); }
        }
    }

    fn comment_lines(&mut self, text: &str) {
        self.line_buf.push_str(text);
        let buf = std::mem::take(&mut self.line_buf);
        let mut lines: Vec<&str> = buf.split('\n').collect();
        let rest = lines.pop().unwrap_or("").to_owned();
        for line in lines { self.comment_line(line); }
        self.line_buf = rest;
    }

    fn comment_line(&mut self, line: &str) {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if !line.starts_with(':') { return; }
        if let Some(v) = super::costs::provider_cost_from_sse_comments(line) { self.cost = Some(v); }
        if let Some(v) = super::costs::provider_energy_from_sse_comments(line) { self.energy = Some(v); }
    }

    fn observe_chunk(&mut self, chunk: &Value) {
        if let Some(f) = chunk.get("finish_reason").filter(|f| truthy(Some(f))) { self.finish_reason = Some(f.clone()); }
        let Some(delta) = chunk.get("delta") else { return };
        if let Some(c) = get_str(delta, &["content"]) { self.output_text.push_str(c); }
        if let Some(r) = get_str(delta, &["reasoning_content"]).or_else(|| get_str(delta, &["thinking", "content"])) { self.reasoning_text.push_str(r); }
        if let Some(Value::Array(calls)) = delta.get("tool_calls") {
            for (i, call) in calls.iter().enumerate() {
                let key = call.get("index").map(|v| format!("i:{}", js::to_string(Some(v)))).or_else(|| get_str(call, &["id"]).map(|id| format!("id:{id}"))).unwrap_or_else(|| format!("pos:{}:{i}", self.tool_call_keys.len()));
                if !self.tool_call_keys.contains(&key) { self.tool_call_keys.push(key); }
                if let Some(a) = get_str(call, &["function", "arguments"]) { self.output_text.push_str(a); }
            }
        }
    }

    fn usage(&self) -> Option<ObservedUsage> { self.usage.as_ref().map(|u| ObservedUsage::from_unified(&Value::Object(u.clone()))) }

    fn metadata(&self, incoming_api_type: &str) -> ResponseMetadata {
        let count = self.tool_call_keys.len();
        let tool = || Value::from(if incoming_api_type == "messages" { "tool_use" } else { "tool_calls" });
        let finish = match &self.finish_reason {
            Some(f) => {
                let l = js::to_string(Some(f)).to_lowercase();
                if (l == "stop" || l == "end_turn") && count > 0 { tool() } else { Value::from(l) }
            }
            None if count > 0 => tool(),
            None => Value::Null,
        };
        ResponseMetadata { tool_calls_count: if count > 0 { Some(count as f64) } else { None }, finish_reason: Some(finish) }
    }
}

/// The finalization view: Plexus's reconstructed body when the debug hook
/// has one, else the built-in observation.
enum Snapshot<'a> {
    Reconstructed(Value),
    Builtin(&'a BuiltinObservation),
}

impl Snapshot<'_> {
    fn usage(&self, provider_api_type: &str) -> Option<ObservedUsage> {
        match self { Self::Reconstructed(v) => extract_usage_from_reconstructed(Some(v), provider_api_type), Self::Builtin(b) => b.usage() }
    }
    fn metadata(&self, provider_api_type: &str, incoming: &str) -> ResponseMetadata {
        match self { Self::Reconstructed(v) => extract_response_metadata(Some(v), provider_api_type, incoming), Self::Builtin(b) => b.metadata(incoming) }
    }
    fn estimate(&self, provider_api_type: &str) -> (f64, f64) {
        match self {
            Self::Reconstructed(v) => estimate_tokens_from_reconstructed(Some(v), provider_api_type),
            Self::Builtin(b) => (estimate_tokens(&b.output_text), estimate_tokens(&b.reasoning_text)),
        }
    }
    fn reported_cost(&self) -> Option<Value> { match self { Self::Reconstructed(v) => v.get("providerReportedCost").cloned(), Self::Builtin(b) => b.cost.clone() } }
    fn energy(&self) -> Option<Value> { match self { Self::Reconstructed(v) => v.get("providerReportedEnergy").cloned(), Self::Builtin(b) => b.energy.clone() } }
    fn usage_block(&self) -> Option<Value> { match self { Self::Reconstructed(v) => v.get("usage").cloned(), Self::Builtin(b) => b.usage.clone().map(Value::Object) } }
    fn tier_raw(&self, provider_api_type: &str) -> Option<String> {
        match self {
            Self::Reconstructed(v) => extract_actual_service_tier(Some(v), None, Some(provider_api_type)).raw,
            Self::Builtin(b) => b.tier_raw.clone(),
        }
    }
}

struct StreamInit {
    svc: ResponseServices,
    record: UsageRecord,
    start_time: i64,
    api_type: String,
    provider_api_type: String,
    should_estimate_tokens: bool,
    restore_debug: bool,
    original_request: Option<Value>,
    quota_enforcer: bool,
    key_name: Option<String>,
    pricing: Pricing,
    bypass: bool,
    provider_transformer: Option<Arc<dyn Transformer>>,
    client: Arc<dyn Transformer>,
    rewriter: Option<Box<dyn RawStreamRewriter>>,
    safeguard: Option<safeguards::SyntheticSafeguardStream>,
    stall: Option<Box<dyn StallObserver>>,
}

/// The streaming pipeline: raw tap → (Gemini rewrite) → provider stream
/// transformer → empty/error observer → synthetic safeguards → client
/// formatter → transformed tap → stall observer → usage inspector.
pub struct StreamSession {
    init: StreamInit,
    decoder: Utf8Decoder,
    sse: SseParser,
    client_sse: SseParser,
    provider_stream: Option<Box<dyn StreamTransformer>>,
    formatter: Option<Box<dyn StreamFormatter>>,
    tracker: StreamVisibilityTracker,
    terminal_incomplete: Option<(Value, Value)>,
    gemini_signaled: bool,
    builtin: BuiltinObservation,
    client_usage: Option<Obj>,
    first_chunk: bool,
    flushed: bool,
    finished: bool,
    disconnected: bool,
}

impl StreamSession {
    fn new(init: StreamInit) -> Self {
        let provider_stream = init.provider_transformer.as_ref().and_then(|t| t.stream_transformer());
        let formatter = init.client.stream_formatter();
        Self {
            init, decoder: Utf8Decoder::default(), sse: SseParser::new(), client_sse: SseParser::new(), provider_stream, formatter,
            tracker: StreamVisibilityTracker::default(), terminal_incomplete: None, gemini_signaled: false, builtin: BuiltinObservation::default(),
            client_usage: None, first_chunk: true, flushed: false, finished: false, disconnected: false,
        }
    }

    /// The usage record as it currently stands.
    pub fn record(&self) -> &UsageRecord { &self.init.record }
    pub fn request_id(&self) -> String { self.init.record.request_id() }
    /// Socket-close alone must not abort `responses:lite` streams (Plexus
    /// logs the observation and waits for a confirmed error or timeout).
    pub fn defers_socket_close(&self) -> bool { self.init.record.str("incomingApiType") == Some("responses:lite") }

    /// Feed upstream bytes; returns client-bound text chunks.
    pub async fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        if self.finished || self.disconnected { return Vec::new(); }
        let text = self.decoder.decode(bytes, false);
        self.process_raw(&text).await
    }

    /// Upstream ended normally: flush every stage, finalize usage, and return
    /// the remaining client-bound chunks.
    pub async fn finish(&mut self) -> Vec<String> {
        if self.finished || self.disconnected { return Vec::new(); }
        let tail = self.decoder.decode(&[], true);
        let mut out = self.process_raw(&tail).await;
        let rid = self.request_id();
        let pat = self.init.provider_api_type.clone();
        self.init.svc.debug.raw_end(&rid, &pat);
        let leftover = std::mem::take(&mut self.builtin.line_buf);
        self.builtin.comment_line(&leftover);
        if let Some(rw) = self.init.rewriter.as_mut() {
            let (flushed, defects) = rw.flush();
            self.handle_defects(defects).await;
            if !flushed.is_empty() { out.extend(self.after_rewrite(&flushed).await); }
        }
        if !self.init.bypass {
            let tail_chunks = self.provider_stream.as_mut().map(|t| t.finish()).unwrap_or_default();
            out.extend(self.unified_chunks(tail_chunks).await);
            if self.provider_stream.is_some() { self.on_unified_end().await; }
            let fin = self.formatter.as_mut().map(|f| f.finish()).unwrap_or_default();
            out.extend(self.client_out(fin));
        } else if let Some(t) = self.provider_stream.as_mut() {
            let tail_chunks = t.finish();
            for c in &tail_chunks { self.builtin.observe_chunk(c); }
        }
        let api = self.init.api_type.clone();
        self.init.svc.debug.transformed_end(&rid, &api);
        self.finished = true;
        self.finalize().await;
        self.cleanup();
        out
    }

    /// Tear the stream down early (`onDisconnect` + `UsageInspector._destroy`).
    pub async fn abort(&mut self, kind: AbortKind) {
        if self.disconnected { return; }
        self.disconnected = true;
        let rid = self.request_id();
        match &kind {
            AbortKind::Stall { message } => {
                self.init.record.set("responseStatus", "stall");
                let r = &self.init.record;
                if let (true, true, Some(c)) = (r.truthy("provider"), r.truthy("selectedModelName"), self.init.svc.cooldowns.as_ref()) {
                    c.mark_provider_stall_failure(&js::to_string(r.get("provider")), &js::to_string(r.get("selectedModelName")), message.as_deref().filter(|m| !m.is_empty()).unwrap_or("Stream stalled"));
                }
            }
            AbortKind::Timeout => self.init.record.set("responseStatus", "timeout"),
            AbortKind::ClientDisconnect => {}
        }
        let (pat, api) = (self.init.provider_api_type.clone(), self.init.api_type.clone());
        self.init.svc.debug.raw_end(&rid, &pat);
        self.init.svc.debug.transformed_end(&rid, &api);
        self.destroy().await;
        self.cleanup();
    }

    fn cleanup(&mut self) {
        if self.init.stall.is_some() { self.init.svc.sink.deregister_in_flight(&self.request_id()); }
        if self.init.restore_debug { self.init.svc.debug.set_enabled(false); self.init.restore_debug = false; }
    }

    async fn process_raw(&mut self, text: &str) -> Vec<String> {
        if text.is_empty() { return Vec::new(); }
        let rid = self.request_id();
        self.init.svc.debug.raw_chunk(&rid, &self.init.provider_api_type, text);
        self.builtin.comment_lines(text);
        let rewritten = match self.init.rewriter.as_mut() {
            Some(rw) => { let (t, defects) = rw.feed(text); self.handle_defects(defects).await; t }
            None => text.to_owned(),
        };
        self.after_rewrite(&rewritten).await
    }

    async fn handle_defects(&mut self, defects: Vec<StreamDefect>) {
        for d in defects {
            if self.gemini_signaled { continue; }
            self.gemini_signaled = true;
            self.init.record.set("responseStatus", "error");
            self.init.record.set("finishReason", d.code.clone());
            let details = client_signal_details(&self.init.api_type, &self.init.record, Some(Value::from(d.status_code)), Some(Value::from(d.code.clone())), None);
            self.init.svc.sink.save_error(&self.request_id(), &ErrorInfo::new(d.message.clone()), Some(&details), self.init.key_name.as_deref()).await;
        }
    }

    async fn after_rewrite(&mut self, text: &str) -> Vec<String> {
        if text.is_empty() { return Vec::new(); }
        // Provider-side parse (also feeds the built-in observation).
        let mut chunks = Vec::new();
        let wants_raw = self.provider_stream.as_ref().is_some_and(|t| t.wants_raw());
        if wants_raw {
            if let Some(t) = self.provider_stream.as_mut() { chunks = t.on_raw(text); }
        } else {
            for ev in self.sse.feed(text) {
                self.observe_raw_event(&ev.data);
                if let Some(t) = self.provider_stream.as_mut() { chunks.extend(t.on_event(&ev)); }
            }
        }
        if self.init.bypass {
            for c in &chunks { self.builtin.observe_chunk(c); }
            return self.client_out(vec![text.to_owned()]);
        }
        if self.provider_stream.is_none() { return self.client_out(vec![text.to_owned()]); }
        self.unified_chunks(chunks).await
    }

    fn observe_raw_event(&mut self, data: &str) {
        self.builtin.seen = true;
        if let Some(u) = self.init.provider_transformer.as_ref().and_then(|t| t.extract_usage(data)) { self.builtin.merge_usage(&u); }
        if let Some(v) = js::parse(data) {
            if let Some(raw) = find_actual_service_tier_raw(Some(&v), Some(&self.init.provider_api_type)) { self.builtin.tier_raw = Some(raw); }
        }
    }

    async fn unified_chunks(&mut self, chunks: Vec<Value>) -> Vec<String> {
        let mut out = Vec::new();
        for mut chunk in chunks {
            self.tracker.observe(&chunk);
            self.builtin.observe_chunk(&chunk);
            if get_str(&chunk, &["event"]) == Some("error") {
                if !truthy(chunk.get("finish_reason")) {
                    if self.init.record.str("responseStatus") != Some("error") {
                        self.init.record.set("responseStatus", "error");
                        let code = js::coalesce(get(&chunk, &["error", "code"]), Some(&json!("response_failed"))).unwrap_or(Value::Null);
                        let message = js::coalesce(get(&chunk, &["error", "message"]), Some(&json!("The model response failed to complete."))).unwrap_or(Value::Null);
                        self.save_terminal_error(code, message, None).await;
                    }
                } else if self.terminal_incomplete.is_none() {
                    let reason = js::coalesce(get(&chunk, &["incomplete_details", "reason"]), js::coalesce(get(&chunk, &["error", "code"]), Some(&json!("unknown"))).as_ref()).unwrap_or(Value::Null);
                    let message = js::coalesce(get(&chunk, &["error", "message"]), None).unwrap_or_else(|| Value::from(format!("Response ended incomplete: {}", js::to_string(chunk.get("finish_reason")))));
                    self.terminal_incomplete = Some((reason, message));
                }
            }
            if let Some(sg) = self.init.safeguard.as_mut() { sg.process(&mut chunk); }
            let texts = match self.formatter.as_mut() {
                Some(f) => f.on_chunk(&chunk),
                None => vec![sse::data(&js::stringify(&chunk))],
            };
            out.extend(self.client_out(texts));
        }
        out
    }

    async fn on_unified_end(&mut self) {
        if self.tracker.is_empty() && self.init.record.str("responseStatus") == Some("success") {
            if let Some((reason, message)) = self.terminal_incomplete.clone() {
                self.init.record.set("responseStatus", "error");
                let mut extra = Obj::new();
                extra.insert("incompleteDetails".into(), json!({"reason": reason}));
                self.save_terminal_error(reason, message, Some(extra)).await;
            } else {
                self.init.record.set("responseStatus", "empty");
            }
        }
    }

    async fn save_terminal_error(&mut self, code: Value, message: Value, extra: Option<Obj>) {
        let details = client_signal_details(&self.init.api_type, &self.init.record, Some(Value::from(500)), Some(code), extra);
        self.init.svc.sink.save_error(&self.request_id(), &ErrorInfo::new(js::to_string(Some(&message))), Some(&details), self.init.key_name.as_deref()).await;
    }

    /// Client-bound tail: transformed tap, stall observer, TTFT, client-side usage fallback.
    fn client_out(&mut self, texts: Vec<String>) -> Vec<String> {
        let rid = self.request_id();
        let mut out = Vec::new();
        for t in texts.into_iter().filter(|t| !t.is_empty()) {
            self.init.svc.debug.transformed_chunk(&rid, &self.init.api_type, &t);
            let mut terminal = false;
            for ev in self.client_sse.feed(&t) {
                // `DebugLoggingInspector('transformed')` terminal detection: it
                // finalizes usage before this chunk reaches the UsageInspector.
                terminal |= match self.init.api_type.as_str() {
                    "chat" => ev.data.trim() == "[DONE]",
                    "responses" => js::parse(&ev.data).and_then(|v| v.get("type").and_then(Value::as_str).map(|t| matches!(t, "response.completed" | "response.failed" | "response.incomplete"))).unwrap_or(false),
                    _ => false,
                };
                if let Some(u) = self.init.client.extract_usage(&ev.data) {
                    if let Some(obj) = u.as_object() {
                        let merged = self.client_usage.get_or_insert_with(Obj::new);
                        for (k, v) in obj { if v.as_f64().is_some_and(|n| n != 0.0) || !merged.contains_key(k) { merged.insert(k.clone(), v.clone()); } }
                    }
                }
            }
            if let Some(s) = self.init.stall.as_mut() { s.on_chunk(&t); }
            if self.first_chunk {
                self.first_chunk = false;
                if !terminal { self.init.record.set_num("ttftMs", (now_ms() - self.init.start_time) as f64); }
            }
            out.push(t);
        }
        out
    }

    fn snapshot(&self) -> Option<Snapshot<'_>> {
        match self.init.svc.debug.reconstructed_raw_response(&self.request_id()).filter(|r| truthy(Some(r))) {
            Some(r) => Some(Snapshot::Reconstructed(r)),
            None if self.builtin.seen && !self.init.svc.debug.reconstructs() => Some(Snapshot::Builtin(&self.builtin)),
            None => None,
        }
    }

    /// `readObservedUsage`: raw first, then the client-facing snapshot.
    fn read_observed_usage(&self, snapshot: Option<&Snapshot<'_>>) -> Option<ObservedUsage> {
        if let Some(u) = snapshot.and_then(|s| s.usage(&self.init.provider_api_type)) { return Some(u); }
        let incoming = self.incoming_api_type();
        match self.init.svc.debug.transformed_response_snapshot(&self.request_id()) {
            Some(t) => extract_usage_from_reconstructed(Some(&t), &incoming),
            None => self.client_usage.as_ref().map(|u| ObservedUsage::from_unified(&Value::Object(u.clone()))),
        }
    }

    fn incoming_api_type(&self) -> String {
        if self.init.api_type.is_empty() { self.init.provider_api_type.clone() } else { self.init.api_type.clone() }
    }

    fn apply_actual_service_tier(record: &mut UsageRecord, snapshot: Option<&Snapshot<'_>>, provider_api_type: &str) {
        if api_format::base_type(provider_api_type) == "gemini" && record.truthy("serviceTierRaw") { return; }
        let Some(raw) = snapshot.and_then(|s| s.tier_raw(provider_api_type)) else { return };
        record.set_opt("serviceTier", super::inspect::normalize_service_tier(Some(&Value::String(raw.clone()))).map(Value::String));
        if record.get("serviceTier").is_none() { record.set("serviceTier", Value::Null); }
        record.set("serviceTierRaw", raw);
    }

    /// `UsageInspector.finalize()` (normal end).
    async fn finalize(&mut self) {
        if self.flushed { return; }
        self.flushed = true;
        let mut stats = ObservedUsage::default();
        let snapshot = self.snapshot();
        let pat = self.init.provider_api_type.clone();
        let incoming = self.incoming_api_type();
        let mut record = self.init.record.clone();
        Self::apply_actual_service_tier(&mut record, snapshot.as_ref(), &pat);
        let usage = self.read_observed_usage(snapshot.as_ref());
        let rid = record.request_id();
        if snapshot.is_some() || usage.is_some() {
            if let Some(u) = usage {
                let z = |n: f64| if n.is_nan() { 0.0 } else { n };
                stats = ObservedUsage { input_tokens: z(u.input_tokens), output_tokens: z(u.output_tokens), cached_tokens: z(u.cached_tokens), cache_write_tokens: z(u.cache_write_tokens), reasoning_tokens: z(u.reasoning_tokens) };
            }
            if let Some(s) = snapshot.as_ref() {
                let meta = s.metadata(&pat, &incoming);
                record.set_num_or_null("toolCallsCount", meta.tool_calls_count);
                record.set_opt("finishReason", meta.finish_reason);
                if self.init.should_estimate_tokens {
                    if usage.is_none() {
                        let (o, r) = s.estimate(&pat);
                        stats.output_tokens = o;
                        stats.reasoning_tokens = r;
                        record.set("tokensEstimated", 1);
                    }
                    self.init.svc.debug.discard_ephemeral(&rid);
                }
            }
            if let Some(orig) = self.init.original_request.as_ref().filter(|o| truthy(Some(o))) {
                if stats.input_tokens == 0.0 { stats.input_tokens = estimate_input_tokens(orig, &incoming); }
            }
            record.set_num("tokensInput", stats.input_tokens);
            record.set_num("tokensOutput", stats.output_tokens);
            record.set_num("tokensCached", stats.cached_tokens);
            record.set_num("tokensCacheWrite", stats.cache_write_tokens);
            record.set_num("tokensReasoning", stats.reasoning_tokens);
        }
        let duration = (now_ms() - self.init.start_time) as f64;
        record.set_num("durationMs", duration);
        let total_output = stats.output_tokens + stats.reasoning_tokens;
        if total_output > 0.0 && duration > 0.0 {
            let ttt = duration - record.num_or_zero("ttftMs");
            record.set_num("tokensPerSec", if ttt > 0.0 { (total_output / ttt) * 1000.0 } else { 0.0 });
        }
        let pricing = self.init.pricing.clone();
        calculate_costs(&mut record, pricing.pricing.as_ref(), pricing.provider_discount.as_ref(), Some(&pricing.attribution), self.init.svc.openrouter.as_deref());
        let (cost, usage_block, energy) = match snapshot.as_ref() { Some(s) => (s.reported_cost(), s.usage_block(), s.energy()), None => (None, None, None) };
        apply_provider_overrides(&mut record, cost.as_ref(), usage_block.as_ref(), energy.as_ref());
        drop(snapshot);

        let svc = self.init.svc.clone();
        svc.sink.save_request(&record).await;
        record_quota(svc.quota.as_ref(), self.init.quota_enforcer, self.init.key_name.as_deref(), &record).await;
        if record.str("responseStatus") == Some("success") && record.truthy("provider") && record.truthy("selectedModelName") {
            let ttft = record.f64("ttftMs").filter(|t| *t != 0.0);
            svc.sink.update_performance_metrics(&PerformanceSample {
                provider: js::to_string(record.get("provider")),
                model: js::to_string(record.get("selectedModelName")),
                canonical_model_name: record.str("canonicalModelName").map(str::to_owned),
                time_to_first_token_ms: ttft,
                output_tokens: if total_output > 0.0 { Some(total_output) } else { None },
                duration_ms: duration,
                request_id: rid.clone(),
                success: true,
            }).await;
        }
        svc.debug.flush(&rid);
        self.init.record = record;
    }

    /// `UsageInspector._destroy()` (torn down before the stream ended).
    async fn destroy(&mut self) {
        if self.flushed { return; }
        self.flushed = true;
        let mut record = self.init.record.clone();
        let status = match record.str("responseStatus") { Some(s @ ("stall" | "timeout")) => s.to_owned(), _ => "cancelled".to_owned() };
        record.set("responseStatus", status);
        record.set_num("durationMs", (now_ms() - self.init.start_time) as f64);
        let snapshot = self.snapshot();
        let pat = self.init.provider_api_type.clone();
        Self::apply_actual_service_tier(&mut record, snapshot.as_ref(), &pat);
        let usage = self.read_observed_usage(snapshot.as_ref());
        if let Some(u) = usage {
            let or_null = |n: f64| if n != 0.0 && !n.is_nan() { Some(n) } else { None };
            record.set_num_or_null("tokensInput", or_null(u.input_tokens));
            record.set_num_or_null("tokensOutput", or_null(u.output_tokens));
            record.set_num_or_null("tokensCached", or_null(u.cached_tokens));
            record.set_num_or_null("tokensCacheWrite", or_null(u.cache_write_tokens));
            record.set_num_or_null("tokensReasoning", or_null(u.reasoning_tokens));
        }
        if snapshot.is_some() || usage.is_some() {
            let pricing = self.init.pricing.clone();
            calculate_costs(&mut record, pricing.pricing.as_ref(), pricing.provider_discount.as_ref(), Some(&pricing.attribution), self.init.svc.openrouter.as_deref());
        }
        drop(snapshot);
        let svc = self.init.svc.clone();
        svc.sink.save_request(&record).await;
        svc.debug.flush(&record.request_id());
        self.init.record = record;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::transformer::{EngineError, StreamTransformer};
    use crate::engine::sse::SseEvent;
    use std::sync::Mutex;

    #[derive(Default)]
    struct MemSink { saved: Mutex<Vec<UsageRecord>>, errors: Mutex<Vec<(String, Value)>>, perf: Mutex<Vec<PerformanceSample>> }
    #[async_trait::async_trait]
    impl UsageSink for MemSink {
        async fn save_request(&self, r: &UsageRecord) { self.saved.lock().unwrap().push(r.clone()); }
        async fn save_error(&self, _rid: &str, e: &ErrorInfo, d: Option<&Value>, _k: Option<&str>) { self.errors.lock().unwrap().push((e.message.clone(), d.cloned().unwrap_or(Value::Null))); }
        async fn update_performance_metrics(&self, s: &PerformanceSample) { self.perf.lock().unwrap().push(s.clone()); }
    }

    /// Chat-like provider: each SSE data line is a unified chunk; usage via `extract_usage`.
    struct FakeChat;
    struct PassStream;
    impl StreamTransformer for PassStream {
        fn on_event(&mut self, e: &SseEvent) -> Vec<Value> { js::parse(&e.data).filter(|v| v.get("delta").is_some() || v.get("event").is_some()).into_iter().collect() }
    }
    struct JsonFormatter;
    impl StreamFormatter for JsonFormatter {
        fn on_chunk(&mut self, c: &Value) -> Vec<String> { vec![sse::data(&js::stringify(c))] }
        fn finish(&mut self) -> Vec<String> { vec![sse::data("[DONE]")] }
    }
    impl Transformer for FakeChat {
        fn name(&self) -> &'static str { "chat" }
        fn default_endpoint(&self) -> &'static str { "/chat/completions" }
        fn parse_request(&self, i: &Value) -> EngineResult<Value> { Ok(i.clone()) }
        fn transform_request(&self, r: &Value) -> EngineResult<Value> { Ok(r.clone()) }
        fn transform_response(&self, r: &Value) -> EngineResult<Value> { Ok(r.clone()) }
        fn format_response(&self, r: &Value) -> EngineResult<Value> {
            if r.get("fail").is_some() { return Err(EngineError::new("boom")); }
            let mut m = r.as_object().cloned().unwrap_or_default();
            m.insert("formatted".into(), Value::Bool(true));
            Ok(Value::Object(m))
        }
        fn stream_transformer(&self) -> Option<Box<dyn StreamTransformer>> { Some(Box::new(PassStream)) }
        fn stream_formatter(&self) -> Option<Box<dyn StreamFormatter>> { Some(Box::new(JsonFormatter)) }
        fn extract_usage(&self, data: &str) -> Option<Value> {
            js::parse(data).and_then(|v| v.get("usage").cloned()).map(|u| crate::engine::usage::normalize_openai_chat(&u))
        }
    }
    struct Registry;
    impl TransformerRegistry for Registry { fn transformer(&self, _: &str) -> Option<Arc<dyn Transformer>> { Some(Arc::new(FakeChat)) } }

    struct FlagDebug(Mutex<bool>);
    impl DebugHooks for FlagDebug {
        fn is_enabled(&self) -> bool { *self.0.lock().unwrap() }
        fn set_enabled(&self, e: bool) { *self.0.lock().unwrap() = e; }
    }

    fn services(sink: Arc<MemSink>, debug: Arc<dyn DebugHooks>, models: Value) -> ResponseServices {
        ResponseServices { sink, config: Arc::new(Config::new(json!({"models": models}))), debug, transformers: Arc::new(Registry), quota: None, cooldowns: None, openrouter: None, gemini_rewriter: None, admin_key: Some("secret".into()) }
    }
    fn ctx(rid: &str, api_type: &str) -> ResponseContext {
        ResponseContext { record: UsageRecord::with_request_id(rid), start_time: now_ms() - 50, api_type: api_type.into(), should_estimate_tokens: false, original_request: None, quota_enforcer: false, key_name: None, request_headers: vec![], quota_context: None }
    }

    #[tokio::test]
    async fn unary_response_maps_usage_and_costs() {
        let sink = Arc::new(MemSink::default());
        let svc = services(sink.clone(), Arc::new(NoDebug), json!({}));
        let unified = json!({"id": "resp", "model": "m-unified", "content": "Hello", "finishReason": "stop", "tool_calls": [{"id": "t"}],
            "usage": {"input_tokens": 500, "output_tokens": 1000, "cached_tokens": 5, "cache_creation_tokens": 2, "reasoning_tokens": 3, "total_tokens": 1500},
            "plexus": {"provider": "provider-1", "model": "model-orig", "apiType": "OpenAI", "canonicalModel": "alias", "pricing": {"source": "simple", "input": 1, "output": 2}}});
        let res = handle_response(&svc, ctx("req-1", "chat"), unified, Arc::new(FakeChat), false, None).await.unwrap();
        assert_eq!(res.status, 200);
        assert_eq!(res.headers[0], ("x-request-id".to_string(), "req-1".to_string()));
        let HandledBody::Json(body) = res.body else { panic!("json") };
        assert_eq!(body["formatted"], true);
        assert!(body.get("plexus").is_none());
        let r = res.record.unwrap();
        assert_eq!(r.str("selectedModelName"), Some("model-orig"));
        assert_eq!(r.str("outgoingApiType"), Some("openai"));
        assert_eq!(r.str("allAttemptedProviders"), Some(r#"["provider-1/model-orig"]"#));
        assert_eq!(r.str("upstreamModel"), Some("model-orig"));
        assert_eq!((r.f64("tokensInput"), r.f64("tokensOutput"), r.f64("tokensCached"), r.f64("tokensCacheWrite"), r.f64("tokensReasoning")), (Some(500.0), Some(1000.0), Some(5.0), Some(2.0), Some(3.0)));
        assert_eq!(r.f64("toolCallsCount"), Some(1.0));
        assert_eq!(r.str("finishReason"), Some("stop"));
        assert_eq!(r.f64("costInput"), Some(0.0005));
        assert_eq!(r.f64("costOutput"), Some(0.002));
        assert_eq!(r.str("responseStatus"), Some("success"));
        assert_eq!(r.f64("ttftMs"), r.f64("durationMs"));
        assert_eq!(r.get("isStreamed"), Some(&json!(false)));
        assert_eq!(sink.saved.lock().unwrap().len(), 1);
        let perf = sink.perf.lock().unwrap();
        assert_eq!(perf[0].output_tokens, Some(1003.0));
        assert_eq!(perf[0].canonical_model_name.as_deref(), Some("alias"));
    }

    #[tokio::test]
    async fn unary_fallbacks_and_playground_gate() {
        let sink = Arc::new(MemSink::default());
        let svc = services(sink.clone(), Arc::new(NoDebug), json!({}));
        let unified = json!({"model": "fallback-model", "plexus": {"provider": "p"}});
        let mut c = ctx("req-2", "chat");
        c.request_headers = vec![("x-plexus-playground".into(), "true".into()), ("x-admin-key".into(), "secret".into())];
        let res = handle_response(&svc, c, unified.clone(), Arc::new(FakeChat), false, None).await.unwrap();
        let HandledBody::Json(body) = res.body else { panic!() };
        assert_eq!(body["plexus"], json!({"requestId": "req-2", "provider": "p"}));
        assert_eq!(res.record.unwrap().str("selectedModelName"), Some("fallback-model"));
        let mut c = ctx("req-3", "chat");
        c.request_headers = vec![("x-plexus-playground".into(), "true".into()), ("x-admin-key".into(), "wrong".into())];
        let HandledBody::Json(body) = handle_response(&svc, c, unified, Arc::new(FakeChat), false, None).await.unwrap().body else { panic!() };
        assert!(body.get("plexus").is_none());
        assert!(!has_valid_admin_key(None, &[("x-admin-key".into(), "secret".into())]));
    }

    #[tokio::test]
    async fn unary_client_error_is_signalled() {
        let sink = Arc::new(MemSink::default());
        let debug = Arc::new(FlagDebug(Mutex::new(false)));
        let svc = services(sink.clone(), debug.clone(), json!({}));
        let unified = json!({"model": "g", "clientError": {"statusCode": 503, "code": "MALFORMED_FUNCTION_CALL", "message": "retry"}, "plexus": {"provider": "gem", "model": "g", "apiType": "gemini"}});
        let mut c = ctx("req-4", "gemini");
        c.should_estimate_tokens = true;
        let res = handle_response(&svc, c, unified, Arc::new(FakeChat), false, None).await.unwrap();
        assert_eq!(res.status, 503);
        let HandledBody::Json(body) = res.body else { panic!() };
        assert_eq!(body, json!({"error": {"code": 503, "status": "UNAVAILABLE", "message": "retry"}}));
        let r = res.record.unwrap();
        assert_eq!(r.str("responseStatus"), Some("error"));
        assert_eq!(r.str("finishReason"), Some("MALFORMED_FUNCTION_CALL"));
        assert!(sink.perf.lock().unwrap().is_empty());
        let errors = sink.errors.lock().unwrap();
        assert_eq!(errors[0].0, "retry");
        assert_eq!(errors[0].1, json!({"apiType": "gemini", "provider": "gem", "targetModel": "g", "statusCode": 503, "code": "MALFORMED_FUNCTION_CALL", "clientSignaled": true}));
        assert!(!debug.is_enabled(), "temporary debug enable is restored");
        assert_eq!(format_client_error("messages", &json!({"message": "m"})), json!({"type": "error", "error": {"type": "api_error", "message": "m"}}));
        assert_eq!(format_client_error("chat", &json!({"message": "m"}))["error"]["code"], "upstream_malformed_function_call");
    }

    #[tokio::test]
    async fn unary_synthetic_safeguards() {
        let sink = Arc::new(MemSink::default());
        let svc = services(sink, Arc::new(NoDebug), json!({"luna": {"synthetic_safeguard_approval": true}}));
        let unified = json!({"model": "m", "tool_calls": [{"id": "call_1"}], "plexus": {"provider": "p", "model": "m", "apiType": "responses", "canonicalModel": "luna"}});
        let mut c = ctx("req-5", "messages");
        c.original_request = Some(json!({"safeguards": [{"type": "dangerous_tool_use"}]}));
        let HandledBody::Json(body) = handle_response(&svc, c, unified, Arc::new(FakeChat), false, None).await.unwrap().body else { panic!() };
        assert_eq!(body["safeguard_results"][0]["status"]["tool_uses"]["call_1"]["outcome"], "not_flagged");
    }

    async fn run_stream(lines: &[&str], api_type: &str) -> (Arc<MemSink>, UsageRecord, String) {
        let sink = Arc::new(MemSink::default());
        let svc = services(sink.clone(), Arc::new(NoDebug), json!({}));
        let unified = json!({"model": "m", "plexus": {"provider": "p", "model": "m", "apiType": "chat", "pricing": {"source": "simple", "input": 1, "output": 1}}});
        let mut c = ctx("s-1", api_type);
        c.original_request = Some(json!({"messages": [{"role": "user", "content": "hello world"}]}));
        let res = handle_response(&svc, c, unified, Arc::new(FakeChat), true, None).await.unwrap();
        assert!(res.headers.contains(&("Content-Type".into(), "text/event-stream".into())));
        let HandledBody::Stream(mut s) = res.body else { panic!("stream") };
        assert_eq!(s.record().str("responseStatus"), Some("success"));
        let mut out = String::new();
        let body: String = lines.iter().map(|l| format!("data: {l}\n\n")).collect();
        let bytes = body.as_bytes();
        // Split mid-way (including inside a multi-byte char if any) to exercise buffering.
        let mid = bytes.len() / 2;
        for part in [&bytes[..mid], &bytes[mid..]] { out.extend(s.push(part).await); }
        out.extend(s.finish().await);
        (sink, s.record().clone(), out)
    }

    #[tokio::test]
    async fn stream_success_extracts_usage() {
        let (sink, r, out) = run_stream(&[r#"{"delta":{"content":"Hi"}}"#, r#"{"delta":{},"finish_reason":"stop","usage":{"prompt_tokens":10,"completion_tokens":5}}"#], "chat").await;
        assert!(out.ends_with("data: [DONE]\n\n"));
        assert_eq!(r.str("responseStatus"), Some("success"));
        assert_eq!((r.f64("tokensInput"), r.f64("tokensOutput")), (Some(10.0), Some(5.0)));
        assert_eq!(r.str("finishReason"), Some("stop"));
        assert_eq!(r.get("toolCallsCount"), Some(&Value::Null));
        assert_eq!(r.f64("costTotal"), Some(0.000015));
        assert!(r.f64("ttftMs").is_some());
        assert_eq!(sink.saved.lock().unwrap().len(), 1);
        assert_eq!(sink.perf.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn stream_empty_and_error_classification() {
        let (_, r, _) = run_stream(&[r#"{"delta":{"content":"  "},"finish_reason":"stop"}"#], "chat").await;
        assert_eq!(r.str("responseStatus"), Some("empty"));
        // Input tokens fall back to estimation over the original request.
        assert!(r.f64("tokensInput").unwrap() > 0.0);

        let (sink, r, _) = run_stream(&[r#"{"event":"error","error":{"code":"server_error","message":"failed"}}"#], "responses").await;
        assert_eq!(r.str("responseStatus"), Some("error"));
        assert!(sink.perf.lock().unwrap().is_empty(), "errored streams skip performance metrics");
        {
            let errors = sink.errors.lock().unwrap();
            assert_eq!(errors.len(), 1);
            assert_eq!(errors[0].0, "failed");
            assert_eq!(errors[0].1["code"], "server_error");
            assert_eq!(errors[0].1["statusCode"], 500);
        }

        let (_, r, _) = run_stream(&[r#"{"delta":{"content":"text"}}"#, r#"{"event":"error","finish_reason":"length","incomplete_details":{"reason":"max_output_tokens"}}"#], "responses").await;
        assert_eq!(r.str("responseStatus"), Some("success"));

        let (sink, r, _) = run_stream(&[r#"{"event":"error","finish_reason":"content_filter","incomplete_details":{"reason":"content_filter"}}"#], "responses").await;
        assert_eq!(r.str("responseStatus"), Some("error"));
        let errors = sink.errors.lock().unwrap();
        assert_eq!(errors[0].0, "Response ended incomplete: content_filter");
        assert_eq!(errors[0].1["code"], "content_filter");
        assert_eq!(errors[0].1["incompleteDetails"], json!({"reason": "content_filter"}));
    }

    #[tokio::test]
    async fn stream_abort_records_cancelled_and_timeout() {
        for (kind, expected) in [(AbortKind::ClientDisconnect, "cancelled"), (AbortKind::Timeout, "timeout"), (AbortKind::Stall { message: None }, "stall")] {
            let sink = Arc::new(MemSink::default());
            let svc = services(sink.clone(), Arc::new(NoDebug), json!({}));
            let unified = json!({"model": "m", "plexus": {"provider": "p", "model": "m", "apiType": "chat"}});
            let HandledBody::Stream(mut s) = handle_response(&svc, ctx("s-2", "chat"), unified, Arc::new(FakeChat), true, None).await.unwrap().body else { panic!() };
            s.push(b"data: {\"delta\":{\"content\":\"Hi\"},\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":0}}\n\n").await;
            s.abort(kind.clone()).await;
            s.abort(kind).await;
            assert!(s.finish().await.is_empty());
            let saved = sink.saved.lock().unwrap();
            assert_eq!(saved.len(), 1, "no double save");
            assert_eq!(saved[0].str("responseStatus"), Some(expected));
            assert_eq!(saved[0].f64("tokensInput"), Some(7.0));
            assert_eq!(saved[0].get("tokensOutput"), Some(&Value::Null));
        }
    }

    #[test]
    fn utf8_decoder_buffers_split_chars() {
        let mut d = Utf8Decoder::default();
        let s = "é€".as_bytes();
        assert_eq!(d.decode(&s[..1], false), "");
        assert_eq!(d.decode(&s[1..3], false), "é");
        assert_eq!(d.decode(&s[3..], false), "€");
        assert_eq!(d.decode(&[0xff, b'a'], false), "\u{FFFD}a");
    }
}
