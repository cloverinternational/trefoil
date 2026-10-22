//! `services/dispatch/request-manager.ts` (`RequestManager.dispatch`),
//! `standard-attempt-request.ts` and the response-side halves of
//! `dispatcher.ts` (`handleProviderError`, `parseJsonResponseBody`,
//! `handleNonStreamingResponse`, `handleStreamingResponse`).
//!
//! Errors are TS-shaped JSON values (`{message, routingContext?, code?,
//! isStreamError?, cooldownDuration?}`). Client disconnects drop the
//! dispatch future, so the TS abort-signal branches have no equivalent.
use super::cooldown_parsers::{
    parse_cooldown_duration_for_provider, resolve_cooldown_provider_type,
};
use super::engine::Engine;
use super::stall::{derive_probe_stall_config, global_stall_config, resolve_stall_config};
use super::transformer_factory::get_transformer;
use crate::engine::dispatch::adapters::ResolvedAdapter;
use crate::engine::dispatch::attempt_history::{
    self as history, AttemptStatus, RetryAttemptRecord,
};
use crate::engine::dispatch::auto_compat::*;
use crate::engine::dispatch::cache_retention::resolve_sticky_cache_ttl_ms;
use crate::engine::dispatch::empty_completion::{
    is_empty_unified_response, EMPTY_COMPLETION_REASON,
};
use crate::engine::dispatch::errors::{
    engine_error_value, format_failure_reason, shape_provider_error, ProviderErrorInput,
};
use crate::engine::dispatch::failover_policy::{is_retryable_network_error, is_retryable_status};
use crate::engine::dispatch::oauth_routes::{
    is_claude_masking_api_key_route, is_native_oauth_route, is_pi_ai_route,
};
use crate::engine::dispatch::request_payload::{
    apply_auto_anthropic_betas, build_request_payload, build_request_url, setup_headers,
};
use crate::engine::dispatch::resolve_adapters;
use crate::engine::dispatch::service_tier_metadata::{
    extract_actual_service_tier, extract_actual_service_tier_from_headers,
    extract_requested_service_tier, ServiceTierCapture,
};
use crate::engine::dispatch::util::Headers;
use crate::engine::execute::{self, AttemptError, ByteStream, Probe, StallConfig};
use crate::engine::js::*;
use crate::engine::observability::storage::VisionMetadata;
use crate::engine::routing::provider_api_selection::select_target_api_type;
use crate::engine::routing::route_candidates::resolve_route_candidates;
use crate::engine::routing::sticky_session::StickySessionManager;
use crate::engine::routing::RouteResult;
use crate::engine::sse::SseParser;
use crate::engine::transformer::{EngineError, Transformer};
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::time::Instant;

/// Unified response plus the raw provider stream for streaming replies.
pub struct DispatchResponse {
    pub unified: Value,
    pub stream: Option<ByteStream>,
    /// The `StallInspector` config left by the last `addStallConfig` (the
    /// winning target's merged overrides); `None` keeps the global base.
    pub stall: Option<Option<StallConfig>>,
}

type ErrValue = Value;

fn error(message: impl Into<String>) -> Value {
    json!({"message": message.into()})
}

fn message(e: &Value) -> String {
    e.get("message")
        .map(|m| to_string(Some(m)))
        .unwrap_or_default()
}

/// `AttemptError` → TS error value.
pub fn attempt_error_value(e: AttemptError) -> Value {
    let mut m = Obj::new();
    m.insert("message".into(), json!(e.message));
    if let Some(rc) = e.routing_context {
        m.insert("routingContext".into(), rc);
    }
    if e.is_stream_error {
        m.insert("isStreamError".into(), json!(true));
    }
    if let Some(c) = e.cooldown_duration {
        m.insert("cooldownDuration".into(), json!(c));
    }
    if let Some(c) = e.code {
        m.insert("code".into(), json!(c));
    }
    Value::Object(m)
}

fn rc_merge(e: &mut Value, pairs: Vec<(&str, Value)>) {
    let Some(map) = e.as_object_mut() else { return };
    let rc = map.entry("routingContext").or_insert_with(|| json!({}));
    if !rc.is_object() {
        *rc = json!({});
    }
    for (k, v) in pairs {
        rc[k] = v;
    }
}

/// `attachRequestedServiceTierToError`.
fn attach_tier_to_error(e: &mut Value, capture: &ServiceTierCapture) {
    let opt = |s: &Option<String>| s.clone().map(Value::String).unwrap_or(Value::Null);
    rc_merge(
        e,
        vec![
            ("requestedServiceTier", opt(&capture.tier)),
            ("requestedServiceTierRaw", opt(&capture.raw)),
        ],
    );
}

/// `attachRequestedServiceTier(response, payload, headers, apiBaseType)`.
fn attach_requested_tier(response: &mut Value, payload: &Value, headers: &Headers, api_type: &str) {
    let capture = extract_requested_service_tier(
        payload,
        Some(&Value::Object(headers.clone())),
        Some(api_type),
    );
    if capture.raw.is_none() {
        return;
    }
    let plexus = response
        .as_object_mut()
        .map(|m| m.entry("plexus").or_insert_with(|| json!({})));
    if let Some(p) = plexus.and_then(Value::as_object_mut) {
        p.insert(
            "requestedServiceTier".into(),
            capture.tier.map(Value::String).unwrap_or(Value::Null),
        );
        p.insert(
            "requestedServiceTierRaw".into(),
            capture.raw.map(Value::String).unwrap_or(Value::Null),
        );
    }
}

fn timeout_error() -> Value {
    json!({"message": "Upstream timeout", "routingContext": {"statusCode": 504, "code": "upstream_timeout"}})
}

fn header_pairs(headers: &Headers) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(k, v)| {
            (
                k.clone(),
                match v {
                    Value::String(s) => s.clone(),
                    other => to_string(Some(other)),
                },
            )
        })
        .collect()
}

fn vision_metadata(request: &Value) -> VisionMetadata {
    VisionMetadata {
        is_vision_fallthrough: truthy(request.get("_hasVisionFallthrough")),
        is_descriptor_request: truthy(request.get("_isVisionDescriptorRequest")),
        vision_fallthrough_model: request
            .get("_visionFallthroughModel")
            .and_then(Value::as_str)
            .map(str::to_owned),
    }
}

fn history_from_values(values: Vec<Value>) -> Vec<RetryAttemptRecord> {
    values
        .into_iter()
        .map(|v| RetryAttemptRecord {
            index: v.get("index").and_then(Value::as_u64).unwrap_or(0) as usize,
            provider: get_str(&v, &["provider"]).unwrap_or("").into(),
            model: get_str(&v, &["model"]).unwrap_or("").into(),
            upstream_model: None,
            api_type: get_str(&v, &["apiType"]).map(str::to_owned),
            status: AttemptStatus::Skipped,
            reason: get_str(&v, &["reason"]).unwrap_or("").into(),
            status_code: None,
            retryable: Some(false),
            provider_response_headers: None,
        })
        .collect()
}

/// Shared mutable dispatch bookkeeping.
struct Ctx<'a> {
    eng: &'a Engine,
    retry_history: Vec<RetryAttemptRecord>,
    attempted: Vec<String>,
    failover_enabled: bool,
    retryable_status_codes: Vec<u16>,
    retryable_errors: Vec<String>,
}

impl Ctx<'_> {
    async fn record_attempt_metric(&self, route: &RouteResult, request: &Value, success: bool) {
        let Some(u) = &self.eng.usage else { return };
        let rid = match get_str(request, &["requestId"]).filter(|s| !s.is_empty()) {
            Some(r) => r.to_owned(),
            None => format!("failover-attempt-{}-{}", now_ms(), random_base36(6)),
        };
        let meta = vision_metadata(request);
        if success {
            u.record_successful_attempt(
                &route.provider,
                &route.model,
                route.canonical_model.as_deref(),
                &rid,
                Some(&meta),
            )
            .await;
        } else {
            u.record_failed_attempt(
                &route.provider,
                &route.model,
                route.canonical_model.as_deref(),
                &rid,
                Some(&meta),
            )
            .await;
        }
    }

    async fn save_intermediate_error(&self, request: &Value, api_type: &str, e: &Value) {
        let Some(rid) = get_str(request, &["requestId"]).filter(|s| !s.is_empty()) else {
            return;
        };
        if self.eng.usage.is_none() {
            return;
        }
        let mut details = Obj::new();
        details.insert("apiType".into(), json!(api_type));
        if let Some(rc) = e.get("routingContext").and_then(Value::as_object) {
            for (k, v) in rc {
                details.insert(k.clone(), v.clone());
            }
        }
        let info = crate::engine::observability::ErrorInfo::new(message(e));
        // Plexus fires this without awaiting.
        let sink = self.eng.sink();
        let (rid, details) = (rid.to_owned(), Value::Object(details));
        tokio::spawn(async move { sink.save_error(&rid, &info, Some(&details), None).await });
    }

    fn emit_routing_update(&self, request: &Value, route: &RouteResult) {
        let Some(rid) = get_str(request, &["requestId"]).filter(|s| !s.is_empty()) else {
            return;
        };
        let mut r = crate::engine::observability::UsageRecord::with_request_id(rid);
        r.set("provider", route.provider.clone());
        r.set("selectedModelName", route.model.clone());
        r.set_opt(
            "canonicalModelName",
            route.canonical_model.clone().map(Value::String),
        );
        self.eng.emit_updated_async(r);
    }

    fn record_sticky(
        &self,
        session_key: Option<&str>,
        route: &RouteResult,
        request: &Value,
        expires_at: i64,
    ) {
        let (Some(key), Some(canonical)) = (session_key, route.canonical_model.as_deref()) else {
            return;
        };
        if truthy(request.get("_isVisionDescriptorRequest")) {
            return;
        }
        if !truthy(
            self.eng
                .config
                .alias(canonical)
                .and_then(|a| a.get("sticky_session")),
        ) {
            return;
        }
        let api = get_str(request, &["incomingApiType"])
            .filter(|s| !s.is_empty())
            .unwrap_or("chat");
        StickySessionManager::global().set(
            canonical,
            api,
            key,
            &route.provider,
            &route.model,
            Some(expires_at),
        );
    }

    async fn mark_failure(&self, route: &RouteResult, duration: Option<f64>, reason: String) {
        crate::cooldown_manager::mark_failure(
            &self.eng.state,
            &route.provider,
            &route.model,
            duration.map(|d| d as i64),
            Some(reason),
        )
        .await;
    }

    async fn mark_stall_failure(&self, route: &RouteResult, reason: String) {
        crate::cooldown_manager::mark_stall_failure(
            &self.eng.state,
            &route.provider,
            &route.model,
            Some(reason),
        )
        .await;
    }

    fn append_failure(
        &mut self,
        route: &RouteResult,
        e: &Value,
        api_type: Option<&str>,
        retryable: bool,
        upstream: Option<&str>,
    ) {
        history::append_failure_attempt(
            &mut self.retry_history,
            route,
            e,
            api_type,
            Some(retryable),
            upstream,
        );
    }

    fn all_failed(&self, last_error: &Value) -> Value {
        history::build_all_targets_failed_error(last_error, &self.attempted, &self.retry_history)
    }
}

/// Concurrency slot (`admitProvider` release).
struct Slot(Option<crate::concurrency::Permit>);
impl Slot {
    fn release(&mut self) {
        self.0.take();
    }
}

fn concurrency_limits(
    config: &crate::engine::config::Config,
    provider: &str,
    model: &str,
) -> (Option<u64>, Option<u64>, bool) {
    let Some(pc) = config.provider(provider).filter(|p| truthy(Some(p))) else {
        return (None, None, false);
    };
    let limit = |v: Option<&Value>| {
        v.filter(|v| !v.is_null())
            .and_then(Value::as_f64)
            .map(|f| f.max(0.0).ceil() as u64)
    };
    let model_limit = pc
        .get("models")
        .and_then(Value::as_object)
        .and_then(|m| m.get(model))
        .and_then(|m| limit(m.get("maxConcurrency")));
    (limit(pc.get("maxConcurrency")), model_limit, true)
}

/// `RequestManager.dispatch(request)`.
pub async fn dispatch(eng: &Engine, request: &mut Value) -> Result<DispatchResponse, ErrValue> {
    let config = eng.config.clone();
    let failover = config.root.get("failover").cloned().unwrap_or(Value::Null);
    let failover_enabled = failover.get("enabled") != Some(&Value::Bool(false));
    let session_key = StickySessionManager::compute_session_key(request);
    let mut raw_history: Vec<Value> = Vec::new();
    let candidates = match resolve_route_candidates(
        eng,
        request,
        &mut raw_history,
        session_key.as_deref(),
    )
    .await
    {
        Ok(c) => c,
        Err(e) => return Err(routing_error_value(eng, &e)),
    };
    let mut ctx = Ctx {
        eng,
        retry_history: history_from_values(raw_history),
        attempted: Vec::new(),
        failover_enabled,
        retryable_status_codes: failover
            .get("retryableStatusCodes")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_u64)
                    .map(|c| c as u16)
                    .collect()
            })
            .unwrap_or_default(),
        retryable_errors: failover
            .get("retryableErrors")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
    };
    if crate::engine::media::bridge::is_image_model_route(request, &candidates, &config) {
        return image_bridge(eng, request).await;
    }
    let targets: Vec<RouteResult> = if failover_enabled {
        candidates
    } else {
        candidates.into_iter().take(1).collect()
    };
    let mut last_error: Value = Value::Null;
    let default_timeout_ms = config
        .root
        .get("timeout")
        .and_then(|t| t.get("defaultSeconds"))
        .filter(|v| !v.is_null())
        .and_then(Value::as_f64)
        .unwrap_or(300.0)
        * 1000.0;

    for (i, route) in targets.iter().enumerate() {
        let current = request.clone();
        let incoming = get_str(&current, &["incomingApiType"]).map(str::to_owned);
        let selection = select_target_api_type(route, incoming.as_deref())
            .map_err(|e| engine_error_value(&e))?;
        let target_api_type = selection.target_api_type.clone();
        let timeout_ms = route
            .config
            .get("timeoutMs")
            .filter(|v| !v.is_null())
            .and_then(Value::as_f64)
            .unwrap_or(default_timeout_ms);
        let deadline =
            Instant::now() + std::time::Duration::from_millis(timeout_ms.max(0.0) as u64);

        // Vision preprocessing happens before context validation and provider admission.
        let current = super::vision::preprocess_vision_request(eng, current, route).await;
        // Context validation must happen before a concurrency slot is acquired.
        if let Some(canonical) = route.canonical_model.as_deref() {
            if let Some(alias) = config
                .alias(canonical)
                .filter(|a| truthy(a.get("enforce_limits")))
            {
                super::limits::enforce_context_limit(&config, &current, alias, canonical)?;
            }
        }

        // Provider admission (cooldown, then concurrency).
        if !crate::cooldown_manager::is_healthy(&eng.state, &route.provider, &route.model).await {
            let reason = format!("Provider {}/{} is on cooldown", route.provider, route.model);
            last_error = error(reason.clone());
            history::append_skipped_attempt(&mut ctx.retry_history, route, &reason, None);
            continue;
        }
        let (pl, ml, known) = concurrency_limits(&config, &route.provider, &route.model);
        let permit = if known {
            crate::concurrency::try_acquire(&eng.state, &route.provider, &route.model, pl, ml)
        } else {
            Some(crate::concurrency::acquire(
                &eng.state,
                route.provider.clone(),
                route.model.clone(),
            ))
        };
        let Some(permit) = permit else {
            let reason = format!(
                "Provider {}/{} concurrency limit exceeded",
                route.provider, route.model
            );
            last_error = error(reason.clone());
            history::append_skipped_attempt(&mut ctx.retry_history, route, &reason, None);
            continue;
        };
        let mut slot = Slot(Some(permit));
        ctx.attempted
            .push(format!("{}/{}", route.provider, route.model));
        ctx.emit_routing_update(&current, route);

        let mut completed_upstream: Option<String> = None;
        let result = attempt(
            &mut ctx,
            &current,
            route,
            &target_api_type,
            &mut slot,
            deadline,
            i < targets.len() - 1,
            session_key.as_deref(),
            &mut completed_upstream,
        )
        .await;
        match result {
            Ok(AttemptOutcome::Success(resp)) => return Ok(resp),
            Ok(AttemptOutcome::Retry(e)) => {
                last_error = e;
                continue;
            }
            Err(e) => {
                let timed_out = Instant::now() >= deadline;
                let mut effective = if timed_out {
                    timeout_error()
                } else {
                    e.clone()
                };
                if timed_out {
                    let rc = e.get("routingContext");
                    let (t, raw) = (
                        rc.and_then(|r| r.get("requestedServiceTier")).cloned(),
                        rc.and_then(|r| r.get("requestedServiceTierRaw")).cloned(),
                    );
                    if t.is_some() || raw.is_some() {
                        rc_merge(
                            &mut effective,
                            vec![
                                ("requestedServiceTier", t.unwrap_or(Value::Null)),
                                ("requestedServiceTierRaw", raw.unwrap_or(Value::Null)),
                            ],
                        );
                    }
                }
                last_error = effective.clone();
                slot.release();
                let is_http = get(&effective, &["routingContext", "statusCode"]).is_some();
                let is_timeout =
                    get_str(&effective, &["routingContext", "code"]) == Some("upstream_timeout");
                let stalled = message(&effective).contains("stalled");
                if !is_http || is_timeout {
                    if stalled {
                        ctx.mark_stall_failure(route, format_failure_reason(&effective, false))
                            .await;
                    } else {
                        ctx.mark_failure(route, None, format_failure_reason(&effective, false))
                            .await;
                    }
                }
                ctx.record_attempt_metric(route, &current, false).await;
                let can_retry = ctx.failover_enabled
                    && i < targets.len() - 1
                    && (is_timeout
                        || is_retryable_network_error(Some(&effective), &ctx.retryable_errors)
                        || stalled);
                ctx.append_failure(
                    route,
                    &effective,
                    None,
                    can_retry,
                    completed_upstream.as_deref(),
                );
                if can_retry {
                    let api = get_str(&effective, &["routingContext", "targetApiType"])
                        .filter(|s| !s.is_empty())
                        .unwrap_or("chat")
                        .to_owned();
                    ctx.save_intermediate_error(&current, &api, &effective)
                        .await;
                    continue;
                }
                return Err(ctx.all_failed(&last_error));
            }
        }
    }
    Err(ctx.all_failed(&last_error))
}

/// `bridgeChatToImageGeneration`: chat-shaped requests naming an image model
/// run through the image pipeline; streams carry the unified chunks as SSE.
async fn image_bridge(eng: &Engine, request: &Value) -> Result<DispatchResponse, ErrValue> {
    use crate::engine::media::bridge::{bridge_chat_to_image_generation, BridgedResponse};
    let default_ms = eng
        .config
        .root
        .get("timeout")
        .and_then(|t| t.get("defaultSeconds"))
        .filter(|v| !v.is_null())
        .and_then(Value::as_f64)
        .unwrap_or(300.0)
        * 1000.0;
    let resolve = move |t: Option<f64>| t.unwrap_or(default_ms);
    match bridge_chat_to_image_generation(eng, request, Some(&resolve)).await? {
        BridgedResponse::Unary(unified) => Ok(DispatchResponse {
            unified,
            stream: None,
            stall: None,
        }),
        BridgedResponse::Stream { response, chunks } => {
            let frames: Vec<Result<bytes::Bytes, String>> = chunks
                .iter()
                .map(|c| Ok(bytes::Bytes::from(crate::engine::sse::data(&stringify(c)))))
                .collect();
            Ok(DispatchResponse {
                unified: response,
                stream: Some(Box::pin(futures_util::stream::iter(frames))),
                stall: None,
            })
        }
    }
}

/// Routing errors (`resolveRouteCandidates`): quota errors carry the 429 body.
fn routing_error_value(eng: &Engine, e: &EngineError) -> Value {
    let mut v = engine_error_value(e);
    if e.code.as_deref() == Some("quota_exceeded") {
        if let Some((body, history)) = eng.take_quota_body() {
            let mut pairs = vec![("body", body)];
            if !history.is_empty() {
                pairs.push(("retryHistory", json!(history)));
            }
            rc_merge(&mut v, pairs);
        }
    }
    v
}

enum AttemptOutcome {
    Success(DispatchResponse),
    Retry(Value),
}

/// Per-target body of the request-manager `try` (transformer selection,
/// payload build) followed by `executeStandardAttempt`.
#[allow(clippy::too_many_arguments)]
async fn attempt(
    ctx: &mut Ctx<'_>,
    current: &Value,
    route: &RouteResult,
    target_api_type: &str,
    slot: &mut Slot,
    deadline: Instant,
    has_next: bool,
    session_key: Option<&str>,
    completed_upstream: &mut Option<String>,
) -> Result<AttemptOutcome, ErrValue> {
    let native = is_native_oauth_route(route, target_api_type);
    let generic = !native
        && !is_claude_masking_api_key_route(route, target_api_type)
        && is_pi_ai_route(route, target_api_type);
    let oauth_provider =
        crate::engine::config::oauth_provider_of(&route.provider, &route.config).to_owned();
    let effective_api_type = if is_claude_masking_api_key_route(route, target_api_type) {
        "messages".to_owned()
    } else if native {
        super::oauth::native::native_oauth_api_type(&oauth_provider, &route.model)
            .unwrap_or_else(|| "messages".into())
    } else if generic {
        super::oauth::native::generic_oauth_api_type(&oauth_provider, &route.model).ok_or_else(|| error(format!(
                "OAuth provider '{oauth_provider}' model '{}' has no known wire API for dispatch. Check that this provider's catalog has a model with a supported API.", route.model)))?
    } else {
        target_api_type.to_owned()
    };
    let transformer = get_transformer(&effective_api_type).map_err(|e| engine_error_value(&e))?;
    let mut with_model = current.clone();
    with_model["model"] = json!(route.model);
    let adapters = resolve_adapters(route, Some(&effective_api_type));
    let mut built = build_request_payload(
        &with_model,
        route,
        transformer.as_ref(),
        &effective_api_type,
        &adapters,
        &ctx.eng.config,
    )
    .map_err(|e| engine_error_value(&e))?;
    let oauth = match built.oauth.take() {
        Some(plan) => {
            let prepared = super::oauth::prepare(
                &ctx.eng.state,
                route,
                &plan,
                std::mem::take(&mut built.payload),
                &with_model,
                &effective_api_type,
            )
            .await
            .map_err(error)?;
            built.payload = prepared.body.clone();
            Some((plan, prepared))
        }
        None => None,
    };
    if let Some(rid) = get_str(current, &["requestId"]).filter(|r| !r.is_empty()) {
        ctx.eng.debug().add_transformed_request(rid, &built.payload);
    }
    let upstream = get_str(&built.payload, &["model"])
        .filter(|m| !m.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| route.model.clone());
    *completed_upstream = Some(upstream.clone());
    let global = global_stall_config(&ctx.eng.config);
    let overrides = obj([
        "stallTtfbMs",
        "stallTtfbBytes",
        "stallMinBps",
        "stallWindowMs",
        "stallGracePeriodMs",
    ]
    .iter()
    .map(|k| (*k, route.config.get(*k).cloned()))
    .collect());
    let stall = resolve_stall_config(global.as_ref(), &overrides);

    let mut tier = ServiceTierCapture::default();
    let result = standard_attempt(
        ctx,
        StandardAttempt {
            payload: built.payload,
            upstream_model: upstream,
            request: current,
            with_model: &with_model,
            route,
            target_api_type: &effective_api_type,
            transformer,
            bypass: built.bypass_transformation,
            adapters: &adapters,
            stall,
            deadline,
            has_next,
            session_key,
            oauth,
        },
        slot,
        &mut tier,
    )
    .await;
    match result {
        Ok(AttemptOutcome::Retry(mut e)) => {
            attach_tier_to_error(&mut e, &tier);
            Ok(AttemptOutcome::Retry(e))
        }
        Err(mut e) => {
            attach_tier_to_error(&mut e, &tier);
            Err(e)
        }
        ok => ok,
    }
}

struct StandardAttempt<'a> {
    payload: Value,
    upstream_model: String,
    request: &'a Value,
    with_model: &'a Value,
    route: &'a RouteResult,
    target_api_type: &'a str,
    transformer: Arc<dyn Transformer>,
    bypass: bool,
    adapters: &'a [ResolvedAdapter],
    stall: Option<StallConfig>,
    deadline: Instant,
    has_next: bool,
    session_key: Option<&'a str>,
    /// `route[NATIVE_OAUTH_STASH]`: the prepared OAuth wire request.
    oauth: Option<(
        crate::engine::dispatch::request_payload::OAuthPlan,
        super::oauth::PreparedOAuthRequest,
    )>,
}

/// `Dispatcher.setupHeaders` / `buildRequestUrl` with the OAuth stash.
fn stash_url_headers(
    route: &RouteResult,
    transformer: &dyn Transformer,
    with_model: &Value,
    target_api_type: &str,
    oauth: Option<&super::oauth::PreparedOAuthRequest>,
) -> Result<(String, Headers), ErrValue> {
    match oauth {
        Some(p) => {
            let headers: Headers = p.headers.iter().cloned().collect();
            Ok((
                p.url.clone(),
                crate::engine::dispatch::cache_key_injection::apply_header_cache_key_injection(
                    headers, route, with_model,
                ),
            ))
        }
        None => Ok((
            build_request_url(route, transformer, with_model, target_api_type)
                .map_err(|e| engine_error_value(&e))?,
            setup_headers(route, target_api_type, with_model)
                .map_err(|e| engine_error_value(&e))?,
        )),
    }
}

async fn with_deadline<T>(
    deadline: Instant,
    fut: impl std::future::Future<Output = Result<T, AttemptError>>,
) -> Result<T, ErrValue> {
    match tokio::time::timeout_at(deadline, fut).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(attempt_error_value(e)),
        Err(_) => Err(error("The operation timed out.")),
    }
}

/// `executeStandardAttempt` (+ `runStandardAttempt`).
async fn standard_attempt(
    ctx: &mut Ctx<'_>,
    a: StandardAttempt<'_>,
    slot: &mut Slot,
    tier: &mut ServiceTierCapture,
) -> Result<AttemptOutcome, ErrValue> {
    let StandardAttempt {
        mut payload,
        upstream_model,
        request,
        with_model,
        route,
        target_api_type,
        transformer,
        bypass,
        adapters,
        stall,
        deadline,
        has_next,
        session_key,
        mut oauth,
    } = a;
    let rid = get_str(request, &["requestId"]).map(str::to_owned);
    let (mut url, mut base_headers) = stash_url_headers(
        route,
        transformer.as_ref(),
        with_model,
        target_api_type,
        oauth.as_ref().map(|o| &o.1),
    )?;
    let mut oauth_refresh_attempted = false;
    let mut param_state = UnsupportedParamStripState::default();
    let mut thinking_state = StripState::default();
    let mut advisor_state = StripState::default();
    let mut lite_state = StripState::default();
    let is_stream = truthy(request.get("stream"));

    let (response, headers, mut effective_stall, sticky_expires_at) = loop {
        let mut effective_stall = stall.clone();
        let mut headers = base_headers.clone();
        apply_auto_anthropic_betas(&mut headers, &payload, target_api_type);
        *tier = extract_requested_service_tier(
            &payload,
            Some(&Value::Object(headers.clone())),
            Some(target_api_type),
        );
        let dispatch_start = now_ms();
        let sticky_expires_at =
            dispatch_start + resolve_sticky_cache_ttl_ms(&payload, target_api_type);
        let pairs = header_pairs(&headers);

        let response = match effective_stall
            .as_ref()
            .and_then(|s| s.ttfb_ms)
            .filter(|_| is_stream)
        {
            Some(ttfb_ms) => {
                let ttfb_deadline =
                    Instant::now() + std::time::Duration::from_millis(ttfb_ms.max(0) as u64);
                tokio::select! {
                    r = with_deadline(deadline, execute::execute_upstream(&url, &pairs, &payload, None)) => r?,
                    _ = tokio::time::sleep_until(ttfb_deadline), if ttfb_deadline < deadline => {
                        let stall_error = error(format!("Stream stalled: TTFB timeout — no response within {ttfb_ms}ms"));
                        let can_retry = ctx.failover_enabled && has_next;
                        if can_retry {
                            ctx.record_attempt_metric(route, request, false).await;
                            ctx.append_failure(route, &stall_error, Some(target_api_type), true, Some(&upstream_model));
                            ctx.mark_stall_failure(route, format_failure_reason(&stall_error, false)).await;
                            ctx.save_intermediate_error(request, target_api_type, &stall_error).await;
                            slot.release();
                            return Ok(AttemptOutcome::Retry(stall_error));
                        }
                        slot.release();
                        return Err(stall_error);
                    }
                }
            }
            None => {
                with_deadline(
                    deadline,
                    execute::execute_upstream(&url, &pairs, &payload, None),
                )
                .await?
            }
        };
        if effective_stall
            .as_ref()
            .is_some_and(|s| s.ttfb_ms.is_some())
            && is_stream
        {
            effective_stall = derive_probe_stall_config(stall.as_ref(), now_ms() - dispatch_start);
        }

        if response.status().is_success() {
            break (response, headers, effective_stall, sticky_expires_at);
        }
        let status = response.status().as_u16();
        let response_headers = execute::response_headers(&response);
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let error_text = with_deadline(deadline, async {
            response.text().await.map_err(execute::transport_error)
        })
        .await?;

        // Reactive OAuth refresh: retry the same target once with a fresh token.
        if status == 401
            && !oauth_refresh_attempted
            && crate::engine::dispatch::oauth_routes::is_oauth_route(route, target_api_type)
        {
            oauth_refresh_attempted = true;
            if let Some((plan, stash)) = oauth.as_mut() {
                if let Ok(true) =
                    super::oauth::refresh(&ctx.eng.state, route, plan, stash, target_api_type).await
                {
                    (url, base_headers) = stash_url_headers(
                        route,
                        transformer.as_ref(),
                        with_model,
                        target_api_type,
                        Some(stash),
                    )?;
                    continue;
                }
            }
        }

        if status == 400 {
            if plan_thinking_signature_strip(&error_text, &payload, &mut thinking_state) {
                let (stripped, n) = strip_thinking_signature_blocks(payload.clone());
                if n > 0 {
                    payload = stripped;
                    continue;
                }
                refund_strip(&mut thinking_state);
            }
            if plan_advisor_result_strip(&error_text, &payload, &mut advisor_state) {
                let (stripped, n) = strip_advisor_result_blocks(payload.clone());
                if n > 0 {
                    payload = stripped;
                    continue;
                }
                refund_strip(&mut advisor_state);
            }
            if let Some(param) = plan_unsupported_param_strip(&error_text, &mut param_state) {
                let (stripped, deleted) = delete_dotted_path(payload.clone(), &param);
                if deleted {
                    payload = stripped;
                    continue;
                }
            }
            if target_api_type.to_lowercase() == "responses:lite"
                && plan_lite_tool_strip(&error_text, &mut lite_state)
            {
                let (stripped, n) = strip_lite_unsupported_tools(payload.clone());
                if n > 0 {
                    payload = stripped;
                    continue;
                }
            }
        }

        let can_retry = ctx.failover_enabled
            && has_next
            && is_retryable_status(status, &ctx.retryable_status_codes);
        let meta_headers = response_headers.clone();
        let e = handle_provider_error(
            ctx,
            route,
            status,
            &error_text,
            retry_after.as_deref(),
            response_headers,
            &url,
            &headers,
            target_api_type,
        )
        .await;
        if let Some(rid) = rid.as_deref().filter(|r| !r.is_empty()) {
            let debug = ctx.eng.debug();
            debug.add_response_meta(rid, status, &meta_headers);
            debug.add_raw_response(rid, json!(error_text));
        }
        ctx.append_failure(
            route,
            &e,
            Some(target_api_type),
            can_retry,
            Some(&upstream_model),
        );
        if can_retry {
            slot.release();
            ctx.record_attempt_metric(route, request, false).await;
            if truthy(get(&e, &["routingContext", "cooldownTriggered"])) {
                ctx.mark_failure(route, None, format_failure_reason(&e, true))
                    .await;
            }
            ctx.save_intermediate_error(request, target_api_type, &e)
                .await;
            return Ok(AttemptOutcome::Retry(e));
        }
        slot.release();
        return Err(e);
    };
    let _ = &mut effective_stall;

    if is_stream {
        let probe = execute::probe_streaming_start(response, effective_stall.as_ref()).await;
        let (status_headers, stream) = match probe {
            Probe::Ok {
                headers: h, stream, ..
            } => (h, stream),
            Probe::Failed {
                error: e,
                stream_started,
            } => {
                let stalled = e.message.contains("stalled");
                let cooldown_duration = e.cooldown_duration;
                let e = attempt_error_value(e);
                let can_retry = ctx.failover_enabled
                    && has_next
                    && !stream_started
                    && (is_retryable_network_error(Some(&e), &ctx.retryable_errors)
                        || stalled
                        || truthy(e.get("isStreamError"))
                        || get(&e, &["routingContext", "statusCode"]).is_some());
                if can_retry {
                    ctx.record_attempt_metric(route, request, false).await;
                    ctx.append_failure(
                        route,
                        &e,
                        Some(target_api_type),
                        true,
                        Some(&upstream_model),
                    );
                    if stalled {
                        ctx.mark_stall_failure(route, format_failure_reason(&e, false))
                            .await;
                    } else {
                        ctx.mark_failure(
                            route,
                            cooldown_duration.map(|d| d as f64),
                            format_failure_reason(&e, false),
                        )
                        .await;
                    }
                    ctx.save_intermediate_error(request, target_api_type, &e)
                        .await;
                    slot.release();
                    return Ok(AttemptOutcome::Retry(e));
                }
                if truthy(e.get("isStreamError"))
                    || truthy(get(&e, &["routingContext", "cooldownTriggered"]))
                {
                    ctx.mark_failure(
                        route,
                        cooldown_duration.map(|d| d as f64),
                        format_failure_reason(&e, false),
                    )
                    .await;
                }
                slot.release();
                return Err(e);
            }
        };
        // `handleStreamingResponse`.
        if let Some(rid) = rid.as_deref().filter(|r| !r.is_empty()) {
            ctx.eng.debug().add_response_meta(rid, 200, &status_headers);
        }
        let mut raw = stream;
        let stream_adapters: Vec<ResolvedAdapter> = adapters
            .iter()
            .filter(|a| {
                a.adapter
                    .pre_dispatch_stream_chunk("", a.options.as_ref())
                    .is_some()
            })
            .cloned()
            .collect();
        if let Some(pairs) = oauth
            .as_ref()
            .map(|o| o.1.reverse_pairs.clone())
            .filter(|p| !p.is_empty())
        {
            raw = rewrite_lines_with(raw, move |line| {
                super::oauth::masking::reverse_tool_renames(line, &pairs)
            });
        }
        if !stream_adapters.is_empty() {
            raw = rewrite_sse_lines(raw, stream_adapters);
        }
        let mut unified = obj(vec![
            ("id", Some(json!(format!("stream-{}", now_ms())))),
            ("model", request.get("model").cloned()),
            ("content", Some(Value::Null)),
            ("bypassTransformation", Some(json!(bypass))),
        ]);
        enrich(&mut unified, route, target_api_type);
        let header_tier = extract_actual_service_tier_from_headers(Some(&status_headers));
        if header_tier.raw.is_some() {
            set_actual_tier(&mut unified, &header_tier);
        }
        let permit = slot.0.take();
        let guarded: ByteStream = Box::pin(raw.map(move |chunk| {
            let _keep = &permit;
            chunk
        }));
        ctx.record_attempt_metric(route, request, true).await;
        crate::cooldown_manager::mark_success(&ctx.eng.state, &route.provider, &route.model).await;
        ctx.record_sticky(session_key, route, request, sticky_expires_at);
        history::append_success_attempt(
            &mut ctx.retry_history,
            route,
            Some(target_api_type),
            Some(&upstream_model),
        );
        history::attach_attempt_metadata(
            &mut unified,
            &ctx.attempted,
            &mut ctx.retry_history,
            route,
            target_api_type,
            Some(&upstream_model),
        );
        attach_requested_tier(&mut unified, &payload, &headers, target_api_type);
        return Ok(AttemptOutcome::Success(DispatchResponse {
            unified,
            stream: Some(guarded),
            stall: Some(stall.clone()),
        }));
    }

    // `handleNonStreamingResponse`.
    let status = response.status().as_u16();
    let resp_headers = execute::response_headers(&response);
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let debug = ctx.eng.debug();
    let debug_rid = rid.clone().filter(|r| !r.is_empty());
    if let Some(r) = &debug_rid {
        debug.add_response_meta(r, status, &resp_headers);
    }
    let text = with_deadline(deadline, async {
        response.text().await.map_err(execute::transport_error)
    })
    .await?;
    let mut body = match parse_json_response_body(
        &text,
        status,
        content_type.as_deref(),
        route,
        target_api_type,
    ) {
        Ok(b) => b,
        Err(e) => {
            if let Some(r) = &debug_rid {
                debug.add_raw_response(r, json!(text));
                debug.add_reconstructed_raw_response(r, json!({"parseError": true, "rawResponseText": text, "contentType": content_type, "provider": route.provider, "targetModel": route.model, "targetApiType": target_api_type}));
            }
            return Err(e);
        }
    };
    let native_tier =
        extract_actual_service_tier(&body, Some(&resp_headers), Some(target_api_type));
    if let Some((_, stash)) = oauth.as_ref().filter(|o| !o.1.reverse_pairs.is_empty()) {
        if body.is_object() || body.is_array() {
            if let Some(reversed) = parse(&stash.reverse_frame(&stringify(&body))) {
                body = reversed;
            }
        }
    }
    for entry in adapters.iter().rev() {
        body = entry.adapter.post_dispatch(body, entry.options.as_ref());
    }
    if let Some(r) = &debug_rid {
        debug.add_response_meta(r, status, &resp_headers);
        debug.add_raw_response(r, body.clone());
    }
    let synthetic = transformer
        .transform_response(&body)
        .map_err(|e| engine_error_value(&e))?;
    let mut unified = if bypass {
        let mut m = synthetic.as_object().cloned().unwrap_or_default();
        m.insert("bypassTransformation".into(), json!(true));
        m.insert("rawResponse".into(), body);
        Value::Object(m)
    } else {
        synthetic
    };
    enrich(&mut unified, route, target_api_type);
    if native_tier.raw.is_some() {
        set_actual_tier(&mut unified, &native_tier);
    }

    if ctx.failover_enabled
        && has_next
        && !truthy(unified.get("clientError"))
        && is_empty_unified_response(&unified)
    {
        slot.release();
        ctx.record_attempt_metric(route, request, false).await;
        let e = json!({"message": EMPTY_COMPLETION_REASON, "routingContext": {"provider": route.provider, "targetModel": route.model, "targetApiType": target_api_type, "statusCode": 200, "cooldownTriggered": false}});
        ctx.append_failure(
            route,
            &e,
            Some(target_api_type),
            true,
            Some(&upstream_model),
        );
        ctx.save_intermediate_error(request, target_api_type, &e)
            .await;
        return Ok(AttemptOutcome::Retry(e));
    }
    ctx.record_attempt_metric(route, request, true).await;
    crate::cooldown_manager::mark_success(&ctx.eng.state, &route.provider, &route.model).await;
    ctx.record_sticky(session_key, route, request, sticky_expires_at);
    history::append_success_attempt(
        &mut ctx.retry_history,
        route,
        Some(target_api_type),
        Some(&upstream_model),
    );
    history::attach_attempt_metadata(
        &mut unified,
        &ctx.attempted,
        &mut ctx.retry_history,
        route,
        target_api_type,
        Some(&upstream_model),
    );
    attach_requested_tier(&mut unified, &payload, &headers, target_api_type);
    slot.release();
    let _ = rid;
    Ok(AttemptOutcome::Success(DispatchResponse {
        unified,
        stream: None,
        stall: Some(stall.clone()),
    }))
}

/// `enrichResponseWithMetadata`.
fn enrich(response: &mut Value, route: &RouteResult, target_api_type: &str) {
    let plexus = obj(vec![
        ("provider", Some(json!(route.provider))),
        ("model", Some(json!(route.model))),
        ("apiType", Some(json!(target_api_type))),
        (
            "pricing",
            route
                .model_config
                .as_ref()
                .and_then(|m| m.get("pricing"))
                .cloned(),
        ),
        ("providerDiscount", route.config.get("discount").cloned()),
        (
            "canonicalModel",
            route.canonical_model.clone().map(Value::String),
        ),
        ("config", Some(route.config.clone())),
    ]);
    if let Some(m) = response.as_object_mut() {
        m.insert("plexus".into(), plexus);
    }
}

fn set_actual_tier(response: &mut Value, tier: &ServiceTierCapture) {
    if let Some(p) = response.get_mut("plexus").and_then(Value::as_object_mut) {
        p.insert(
            "serviceTier".into(),
            tier.tier.clone().map(Value::String).unwrap_or(Value::Null),
        );
        p.insert(
            "serviceTierRaw".into(),
            tier.raw.clone().map(Value::String).unwrap_or(Value::Null),
        );
    }
}

/// `handleProviderError` (cooldown side effect + enriched error).
#[allow(clippy::too_many_arguments)]
async fn handle_provider_error(
    ctx: &Ctx<'_>,
    route: &RouteResult,
    status: u16,
    error_text: &str,
    retry_after: Option<&str>,
    response_headers: Value,
    url: &str,
    headers: &Headers,
    target_api_type: &str,
) -> Value {
    let provider_type = resolve_cooldown_provider_type(route);
    let outcome = shape_provider_error(
        route,
        ProviderErrorInput {
            status,
            error_text,
            retry_after,
            response_headers: response_headers.as_object().cloned().unwrap_or_default(),
            url: Some(url),
            request_headers: Some(headers),
            target_api_type: Some(target_api_type),
            now_ms: now_ms() as f64,
        },
        &|text| parse_cooldown_duration_for_provider(provider_type.as_deref(), text),
    );
    if let Some(c) = outcome.cooldown {
        ctx.mark_failure(route, c.duration_ms, c.reason).await;
    }
    outcome.error
}

/// `parseJsonResponseBody`.
fn parse_json_response_body(
    text: &str,
    status: u16,
    content_type: Option<&str>,
    route: &RouteResult,
    target_api_type: &str,
) -> Result<Value, ErrValue> {
    if let Ok(v) = serde_json::from_str::<Value>(text) {
        return Ok(v);
    }
    static EVENT_LINE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = EVENT_LINE.get_or_init(|| regex::Regex::new(r"(?m)^\s*(?:event|data):").unwrap());
    if content_type.is_some_and(|c| c.contains("text/event-stream")) || re.is_match(text) {
        let mut completed: Option<Value> = None;
        let mut items: Vec<(f64, Value)> = Vec::new();
        for event in SseParser::new().feed(&format!("{text}\n\n")) {
            let Some(p) = parse(&event.data) else {
                continue;
            };
            if get_str(&p, &["type"]) == Some("response.output_item.done") && truthy(p.get("item"))
            {
                let index = p
                    .get("output_index")
                    .and_then(Value::as_f64)
                    .unwrap_or(items.len() as f64);
                match items.iter_mut().find(|(i, _)| *i == index) {
                    Some(e) => e.1 = p["item"].clone(),
                    None => items.push((index, p["item"].clone())),
                }
            }
            if get_str(&p, &["type"]) == Some("response.completed") && truthy(p.get("response")) {
                let mut sorted = items.clone();
                sorted.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
                let mut resp = p["response"].as_object().cloned().unwrap_or_default();
                let has_output = resp
                    .get("output")
                    .and_then(Value::as_array)
                    .is_some_and(|o| !o.is_empty());
                if !has_output {
                    resp.insert(
                        "output".into(),
                        Value::Array(sorted.into_iter().map(|(_, v)| v).collect()),
                    );
                }
                completed = Some(Value::Object(resp));
            }
        }
        if let Some(c) = completed {
            return Ok(c);
        }
    }
    let msg = if text.is_empty() {
        "JSON Parse error: Unable to parse JSON string".to_owned()
    } else {
        text.to_owned()
    };
    Err(json!({"message": msg, "routingContext": {
        "provider": route.provider, "targetModel": route.model, "targetApiType": target_api_type,
        "statusCode": if status == 0 { 500 } else { status }, "rawResponseText": text, "providerResponse": text,
        "contentType": content_type.map(Value::from).unwrap_or(Value::Null),
    }}))
}

/// `buildSseFrameRewriteTransform(rewrite)`: line-buffered frame rewrite.
fn rewrite_lines_with(
    stream: ByteStream,
    rewrite: impl Fn(&str) -> String + Send + Sync + 'static,
) -> ByteStream {
    struct St<F> {
        inner: ByteStream,
        buf: String,
        carry: Vec<u8>,
        rewrite: F,
        done: bool,
    }
    let st = St {
        inner: stream,
        buf: String::new(),
        carry: Vec::new(),
        rewrite,
        done: false,
    };
    Box::pin(futures_util::stream::unfold(st, |mut st| async move {
        loop {
            if st.done {
                return None;
            }
            match st.inner.next().await {
                Some(Ok(chunk)) => {
                    st.carry.extend_from_slice(&chunk);
                    let valid = match std::str::from_utf8(&st.carry) {
                        Ok(_) => st.carry.len(),
                        Err(e) => e.valid_up_to(),
                    };
                    let text = String::from_utf8_lossy(&st.carry[..valid]).into_owned();
                    st.carry.drain(..valid);
                    st.buf.push_str(&text);
                    let Some(last_nl) = st.buf.rfind('\n') else {
                        continue;
                    };
                    let complete: String = st.buf[..last_nl].to_owned();
                    st.buf = st.buf[last_nl + 1..].to_owned();
                    let out: String = complete
                        .split('\n')
                        .map(|l| (st.rewrite)(l) + "\n")
                        .collect();
                    return Some((Ok(bytes::Bytes::from(out)), st));
                }
                Some(Err(e)) => return Some((Err(e), st)),
                None => {
                    st.done = true;
                    if st.buf.is_empty() {
                        return None;
                    }
                    let out = (st.rewrite)(&std::mem::take(&mut st.buf));
                    return Some((Ok(bytes::Bytes::from(out)), st));
                }
            }
        }
    }))
}

/// `buildSseRewriteTransform(adapters)`: line-buffered `preDispatchStreamChunk`.
fn rewrite_sse_lines(stream: ByteStream, adapters: Vec<ResolvedAdapter>) -> ByteStream {
    struct St {
        inner: ByteStream,
        buf: String,
        carry: Vec<u8>,
        adapters: Vec<ResolvedAdapter>,
        done: bool,
    }
    fn rewrite(adapters: &[ResolvedAdapter], line: &str) -> String {
        adapters.iter().fold(line.to_owned(), |acc, a| {
            a.adapter
                .pre_dispatch_stream_chunk(&acc, a.options.as_ref())
                .unwrap_or(acc)
        })
    }
    let st = St {
        inner: stream,
        buf: String::new(),
        carry: Vec::new(),
        adapters,
        done: false,
    };
    Box::pin(futures_util::stream::unfold(st, |mut st| async move {
        loop {
            if st.done {
                return None;
            }
            match st.inner.next().await {
                Some(Ok(chunk)) => {
                    st.carry.extend_from_slice(&chunk);
                    let valid = match std::str::from_utf8(&st.carry) {
                        Ok(_) => st.carry.len(),
                        Err(e) => e.valid_up_to(),
                    };
                    let text = String::from_utf8_lossy(&st.carry[..valid]).into_owned();
                    st.carry.drain(..valid);
                    st.buf.push_str(&text);
                    let Some(last_nl) = st.buf.rfind('\n') else {
                        continue;
                    };
                    let complete: String = st.buf[..last_nl].to_owned();
                    st.buf = st.buf[last_nl + 1..].to_owned();
                    let out: String = complete
                        .split('\n')
                        .map(|l| rewrite(&st.adapters, l) + "\n")
                        .collect();
                    return Some((Ok(bytes::Bytes::from(out)), st));
                }
                Some(Err(e)) => return Some((Err(e), st)),
                None => {
                    st.done = true;
                    if st.buf.is_empty() {
                        return None;
                    }
                    let out = rewrite(&st.adapters, &std::mem::take(&mut st.buf));
                    return Some((Ok(bytes::Bytes::from(out)), st));
                }
            }
        }
    }))
}
