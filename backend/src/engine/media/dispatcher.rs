//! `services/dispatch/media-dispatcher.ts` (embeddings, transcriptions,
//! speech, image generations) plus the `Dispatcher` host helpers it calls
//! (`handleProviderError`, `parseJsonResponseBody`, `applyQuotaFilter`, …).
//!
//! Errors are TS-shaped values (`{message, code?, routingContext?, …}`).
//! Plexus quirks are kept: an HTTP failure that is not retried is recorded
//! twice in the retry history (inner and outer `catch`), and the HTTP-retry
//! `continue` of the embeddings/transcription/speech loops never releases its
//! concurrency slot.
use super::form::{form_set, to_multipart, FormPart, ImagePayload};
use super::image::ImageTarget;
use super::transformers::{self as tf, EmbeddingsTarget, TranscriptionRequest};
use crate::engine::config::{get_provider_types, Config};
use crate::engine::dispatch::attempt_history::{
    append_failure_attempt, append_skipped_attempt, append_success_attempt, attach_attempt_metadata, build_all_targets_failed_error,
    RetryAttemptRecord,
};
use crate::engine::dispatch::errors::{engine_error_value, format_failure_reason, shape_provider_error, ProviderErrorInput};
use crate::engine::dispatch::failover_policy::{is_retryable_network_error, is_retryable_status};
use crate::engine::dispatch::oauth_routes::is_oauth_route;
use crate::engine::execute::{probe_streaming_start, AttemptError, Probe, DEFAULT_USER_AGENT};
use crate::engine::js::*;
use crate::engine::routing::key_access_policy::apply_key_access_policy;
use crate::engine::routing::provider_api_selection::{resolve_image_provider_base_url, resolve_provider_base_url, select_target_api_type};
use crate::engine::routing::route_candidates::filter_quota_candidates;
use crate::engine::routing::{resolve, resolve_candidates, RouteResult, RoutingRuntime};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

/// Error value (TS `Error` with ad-hoc fields).
pub type MediaError = Value;
pub type MediaResult<T> = Result<T, MediaError>;

/// A reserved concurrency slot; dropping it releases the slot.
pub type Permit = Box<dyn Send + Sync>;

/// The process services `MediaDispatcher` reaches for (`MediaDispatchHost`
/// plus the singletons it calls directly).
#[async_trait::async_trait]
pub trait MediaHost: Send + Sync {
    fn routing(&self) -> &dyn RoutingRuntime;
    fn config(&self) -> Arc<Config> { self.routing().config() }
    /// `ConcurrencyTracker.acquire(provider, model)`.
    fn acquire(&self, provider: &str, model: &str) -> Option<Permit>;
    /// `CooldownManager.markProviderFailure(provider, model, duration, reason)`.
    async fn mark_provider_failure(&self, provider: &str, model: &str, duration_ms: Option<f64>, reason: String);
    /// `CooldownManager.markProviderSuccess`.
    async fn mark_provider_success(&self, provider: &str, model: &str);
    /// `recordAttemptMetric(route, requestId, success)`.
    async fn record_attempt_metric(&self, route: &RouteResult, request_id: &str, success: bool);
    /// `usageStorage.saveError(requestId, error, details)`.
    async fn save_error(&self, request_id: &str, error: &MediaError, details: &Value);
    /// `emitRoutingUpdate(requestId, route)`.
    fn emit_routing_update(&self, request_id: &str, route: &RouteResult);
    /// `parseCooldownDurationForProvider(resolveCooldownProviderType(route), errorText, 'HTTP')`.
    fn provider_cooldown_ms(&self, _route: &RouteResult, _error_text: &str) -> Option<f64> { None }
    /// `prepareCodexImagesDispatch({modelId, oauthAccountId})` → `(baseUrl, headers)`.
    async fn prepare_codex_images(&self, _model: &str, _oauth_account: Option<&str>) -> MediaResult<(String, Vec<(String, String)>)> {
        Err(json!({"message": "No OpenAI Codex OAuth credentials available"}))
    }
    /// `DebugManager.addTransformedRequest(requestId, payload)`.
    fn debug_transformed_request(&self, _request_id: &str, _payload: &Value) {}
    /// `DebugManager.addResponseMeta(requestId, status, extractResponseHeaders(response))`.
    fn debug_response_meta(&self, _request_id: &str, _status: u16, _headers: &Value) {}
    /// `DebugManager.addRawResponse(requestId, payload)`.
    fn debug_raw_response(&self, _request_id: &str, _payload: Value) {}
    /// `DebugManager.addReconstructedRawResponse(requestId, payload)`.
    fn debug_reconstructed_raw_response(&self, _request_id: &str, _payload: Value) {}
}

const BASE64_DATA_URL: &str = "data:";

/// `redactImageDataUrls(value)` for JSON payloads.
fn redact_image_data_urls(value: &Value) -> Value {
    match value {
        Value::String(s) => Value::String(redact_data_url(s)),
        Value::Array(items) => Value::Array(items.iter().map(redact_image_data_urls).collect()),
        Value::Object(map) => Value::Object(map.iter().map(|(k, v)| (k.clone(), redact_image_data_urls(v))).collect()),
        other => other.clone(),
    }
}

/// `/^data:([^;,]*);base64,([\s\S]*)$/` → `data:<mime>;base64,[N bytes]`.
fn redact_data_url(s: &str) -> String {
    let Some(rest) = s.strip_prefix(BASE64_DATA_URL) else { return s.to_owned() };
    let Some(end) = rest.find([';', ',']) else { return s.to_owned() };
    let (mime, tail) = rest.split_at(end);
    let Some(data) = tail.strip_prefix(";base64,") else { return s.to_owned() };
    let padding = if data.ends_with("==") { 2 } else if data.ends_with('=') { 1 } else { 0 };
    let bytes = ((data.encode_utf16().count() * 3) / 4).saturating_sub(padding);
    format!("data:{mime};base64,[{bytes} bytes]")
}

/// `summarizeFormData(form)`.
fn summarize_form_data(parts: &[crate::engine::media::form::FormPart]) -> Value {
    use crate::engine::media::form::FormValue;
    let mut summary = Obj::new();
    for part in parts {
        let value = match &part.value {
            FormValue::Text(t) => Value::String(redact_data_url(t)),
            FormValue::Blob { data, mime, .. } => Value::String(format!("{};[{} bytes]", if mime.is_empty() { "application/octet-stream" } else { mime }, data.len())),
        };
        match summary.get_mut(&part.name) {
            None => { summary.insert(part.name.clone(), value); }
            Some(Value::Array(items)) => items.push(value),
            Some(existing) => { let prev = existing.take(); *existing = json!([prev, value]); }
        }
    }
    Value::Object(summary)
}

/// `AttemptError` as the TS error value.
pub fn attempt_error_value(e: &AttemptError) -> MediaError {
    let mut out = Obj::new();
    out.insert("message".into(), json!(e.message));
    if let Some(c) = &e.code { out.insert("code".into(), json!(c)); }
    if let Some(rc) = &e.routing_context { out.insert("routingContext".into(), rc.clone()); }
    if e.is_stream_error { out.insert("isStreamError".into(), json!(true)); }
    if let Some(d) = e.cooldown_duration { out.insert("cooldownDuration".into(), json!(d)); }
    Value::Object(out)
}

fn status_code(error: &MediaError) -> Option<u16> {
    get(error, &["routingContext", "statusCode"]).filter(|v| !v.is_null()).and_then(Value::as_f64).map(|n| n as u16)
}

fn has_status_code(error: &MediaError) -> bool { get(error, &["routingContext", "statusCode"]).is_some() }

struct Failover { enabled: bool, codes: Vec<u16>, errors: Vec<String> }

fn failover(config: &Config) -> Failover {
    let f = get(&config.root, &["failover"]);
    let codes = f.and_then(|f| f.get("retryableStatusCodes")).and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_f64).map(|n| n as u16).collect())
        .unwrap_or_default();
    let errors = f.and_then(|f| f.get("retryableErrors")).and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect())
        .unwrap_or_default();
    Failover { enabled: f.and_then(|f| f.get("enabled")) != Some(&Value::Bool(false)), codes, errors }
}

/// Upstream response as read by the dispatcher.
struct Upstream {
    status: u16,
    headers: Value,
    retry_after: Option<String>,
    content_type: Option<String>,
    response: Option<reqwest::Response>,
    /// The body, once read for an error.
    body_text: Option<String>,
}

impl Upstream {
    fn new(response: reqwest::Response) -> Self {
        let headers = crate::engine::execute::response_headers(&response);
        let header = |k: &str| response.headers().get(k).and_then(|v| v.to_str().ok()).map(str::to_owned);
        Self { status: response.status().as_u16(), retry_after: header("retry-after"), content_type: header("content-type"), headers, response: Some(response), body_text: None }
    }
    fn ok(&self) -> bool { (200..300).contains(&self.status) }
    async fn text(mut self) -> String {
        if let Some(t) = self.body_text.take() { return t; }
        match self.response.take() {
            Some(r) => match r.bytes().await { Ok(b) => String::from_utf8_lossy(&b).into_owned(), Err(_) => String::new() },
            None => String::new(),
        }
    }
    async fn bytes(mut self) -> Result<Vec<u8>, String> {
        match self.response.take() { Some(r) => r.bytes().await.map(|b| b.to_vec()).map_err(|e| e.to_string()), None => Ok(Vec::new()) }
    }
}

/// `response.json()`.
async fn response_json(up: Upstream) -> MediaResult<Value> {
    let text = up.text().await;
    parse(&text).ok_or_else(|| json!({"message": "Failed to parse JSON", "name": "SyntaxError"}))
}

/// `parseJsonResponseBody(response, requestId, route, targetApiType)`.
async fn parse_json_response_body(host: &dyn MediaHost, up: Upstream, request_id: &str, route: &RouteResult, target_api_type: &str) -> MediaResult<Value> {
    let status = up.status;
    let content_type = up.content_type.clone();
    let text = up.text().await;
    if let Some(v) = parse(&text) { return Ok(v); }
    let is_event_stream = content_type.as_deref().is_some_and(|c| c.contains("text/event-stream"))
        || text.lines().any(|l| { let t = l.trim_start(); t.starts_with("event:") || t.starts_with("data:") });
    if is_event_stream {
        if let Some(done) = completed_response_from_sse(&text) { return Ok(done); }
    }
    if !request_id.is_empty() {
        host.debug_raw_response(request_id, json!(text));
        host.debug_reconstructed_raw_response(request_id, json!({
            "parseError": true,
            "rawResponseText": text,
            "contentType": content_type,
            "provider": route.provider,
            "targetModel": route.model,
            "targetApiType": target_api_type,
        }));
    }
    let message = if text.is_empty() { "JSON Parse error: Unable to parse JSON string".to_owned() } else { text.clone() };
    Err(json!({
        "message": message,
        "routingContext": obj(vec![
            ("provider", Some(json!(route.provider))),
            ("targetModel", Some(json!(route.model))),
            ("targetApiType", Some(json!(target_api_type))),
            ("statusCode", Some(json!(if status == 0 { 500 } else { status }))),
            ("rawResponseText", Some(json!(text))),
            ("providerResponse", Some(json!(text))),
            ("contentType", Some(content_type.map(Value::String).unwrap_or(Value::Null))),
        ]),
    }))
}

fn completed_response_from_sse(text: &str) -> Option<Value> {
    let mut parser = crate::engine::sse::SseParser::default();
    let mut items: Vec<(f64, Value)> = Vec::new();
    let mut completed = None;
    for event in parser.feed(text) {
        let Some(payload) = parse(&event.data) else { continue };
        if payload.get("type") == Some(&json!("response.output_item.done")) && truthy(payload.get("item")) {
            let index = payload.get("output_index").and_then(Value::as_f64).unwrap_or(items.len() as f64);
            match items.iter_mut().find(|(i, _)| *i == index) { Some(slot) => slot.1 = payload["item"].clone(), None => items.push((index, payload["item"].clone())) }
        }
        if payload.get("type") == Some(&json!("response.completed")) && truthy(payload.get("response")) {
            let mut sorted = items.clone();
            sorted.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
            let mut resp = payload["response"].clone();
            let has_output = resp.get("output").and_then(Value::as_array).is_some_and(|o| !o.is_empty());
            if !has_output { resp["output"] = Value::Array(sorted.into_iter().map(|(_, v)| v).collect()); }
            completed = Some(resp);
        }
    }
    completed
}

/// `handleProviderError(...)`: cooldown side effect + the thrown error.
#[allow(clippy::too_many_arguments)]
async fn handle_provider_error(host: &dyn MediaHost, up: &Upstream, route: &RouteResult, error_text: &str, url: &str, headers: &[(String, String)], target_api_type: &str, request_id: &str) -> MediaError {
    let request_headers: Obj = headers.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
    let outcome = shape_provider_error(
        route,
        ProviderErrorInput {
            status: up.status,
            error_text,
            retry_after: up.retry_after.as_deref(),
            response_headers: up.headers.as_object().cloned().unwrap_or_default(),
            url: Some(url),
            request_headers: Some(&request_headers),
            target_api_type: Some(target_api_type),
            now_ms: now_ms() as f64,
        },
        &|text| host.provider_cooldown_ms(route, text),
    );
    if let Some(cd) = outcome.cooldown {
        host.mark_provider_failure(&route.provider, &route.model, cd.duration_ms, cd.reason).await;
    }
    if !request_id.is_empty() {
        host.debug_response_meta(request_id, up.status, &up.headers);
        host.debug_raw_response(request_id, json!(error_text));
    }
    outcome.error
}

fn header_list(pairs: Vec<(String, String)>, extra: Option<&Value>) -> Vec<(String, String)> {
    let mut out = pairs;
    if let Some(Value::Object(h)) = extra {
        for (k, v) in h {
            let v = to_string(Some(v));
            match out.iter_mut().find(|(ek, _)| ek == k) { Some(e) => e.1 = v, None => out.push((k.clone(), v)) }
        }
    }
    out
}

fn set_header(headers: &mut Vec<(String, String)>, key: &str, value: &str) {
    match headers.iter_mut().find(|(k, _)| k == key) { Some(e) => e.1 = value.into(), None => headers.push((key.into(), value.into())) }
}

fn api_key(route: &RouteResult) -> Option<String> {
    route.config.get("api_key").filter(|k| truthy(Some(k))).map(|k| to_string(Some(k)))
}

/// `fetchWithSession(url, {method: 'POST', headers, body})` with Bun's
/// default request headers.
async fn send(url: &str, headers: &[(String, String)], body: Body, timeout: Option<Duration>) -> Result<reqwest::Response, AttemptError> {
    let mut req = crate::engine::execute::client().post(url);
    let mut has_ua = false;
    let mut has_ct = false;
    let mut has_accept = false;
    for (k, v) in headers {
        if k.eq_ignore_ascii_case("user-agent") { has_ua = true; }
        if k.eq_ignore_ascii_case("content-type") { has_ct = true; }
        if k.eq_ignore_ascii_case("accept") { has_accept = true; }
        req = req.header(k.as_str(), v.as_str());
    }
    if !has_ua { req = req.header("user-agent", DEFAULT_USER_AGENT); }
    if !has_accept { req = req.header("accept", "*/*"); }
    req = match body {
        Body::Json(text) => {
            if !has_ct { req = req.header("content-type", "text/plain;charset=UTF-8"); }
            req.body(text)
        }
        // FormData always carries its own multipart content type.
        Body::Form(parts) => req.multipart(to_multipart(&parts)),
    };
    if let Some(t) = timeout { req = req.timeout(t); }
    req.send().await.map_err(crate::engine::execute::transport_error)
}

enum Body { Json(String), Form(Vec<FormPart>) }

/// `Object.assign(payload, extraBody)` for each configured layer.
fn merge_extra_bodies(payload: &mut Value, route: &RouteResult, config: &Config) {
    let mut layers = vec![route.config.get("extraBody").cloned(), route.model_config.as_ref().and_then(|m| m.get("extraBody")).cloned()];
    if let Some(c) = route.canonical_model.as_deref() { layers.push(config.alias(c).and_then(|a| a.get("extraBody")).cloned()); }
    for layer in layers.into_iter().flatten() {
        if let (Some(p), Some(extra)) = (payload.as_object_mut(), layer.as_object()) {
            for (k, v) in extra { p.insert(k.clone(), v.clone()); }
        }
    }
}

/// `Router.resolveCandidates` falling back to `Router.resolve`, then key
/// policy and quota filter.
async fn resolve_targets(host: &dyn MediaHost, request: &Value, api_type: &str, history: &mut Vec<RetryAttemptRecord>) -> MediaResult<Vec<RouteResult>> {
    let rt = host.routing();
    let model = to_string(request.get("model"));
    let mut candidates = resolve_candidates(rt, &model, Some(api_type), None).await.map_err(|e| engine_error_value(&e))?;
    if candidates.is_empty() {
        candidates = vec![resolve(rt, &model, Some(api_type)).await.map_err(|e| engine_error_value(&e))?];
    }
    let candidates = apply_key_access_policy(request, candidates, api_type).map_err(|e| engine_error_value(&e))?;
    Ok(candidates).and_then(|c| apply_quota_filter(host, request, c, history, api_type))
}

/// `applyQuotaFilter(request, candidates, retryHistory, apiType)`.
fn apply_quota_filter(host: &dyn MediaHost, request: &Value, candidates: Vec<RouteResult>, history: &mut Vec<RetryAttemptRecord>, api_type: &str) -> MediaResult<Vec<RouteResult>> {
    let ctx = get(request, &["metadata", "plexus_metadata", "plexus_quota_context"]).filter(|c| !c.is_null());
    let Some(ctx) = ctx else { return Ok(candidates) };
    let original = candidates.clone();
    let (allowed, blocked) = filter_quota_candidates(Some(ctx), candidates);
    if blocked.is_empty() { return Ok(original); }
    for (candidate, quota) in &blocked {
        append_skipped_attempt(history, candidate, &format!("quota_exceeded:{}", to_string(quota.get("quotaName"))), Some(api_type));
    }
    if allowed.is_empty() {
        let quotas: Vec<Value> = blocked.into_iter().map(|(_, q)| q).collect();
        let history_json: Vec<Value> = history.iter().map(RetryAttemptRecord::to_json).collect();
        return Err(engine_error_value(&host.routing().build_quota_exceeded_error(&quotas, &history_json)));
    }
    Ok(allowed)
}

fn targets_for(candidates: Vec<RouteResult>, enabled: bool) -> Vec<RouteResult> {
    if enabled { candidates } else { candidates.into_iter().take(1).collect() }
}

fn with_model(request: &Value, model: &str) -> Value {
    let mut r = request.clone();
    if let Some(m) = r.as_object_mut() { m.insert("model".into(), json!(model)); }
    r
}

fn request_id_of(request: &Value) -> String { to_string(request.get("requestId").filter(|v| truthy(Some(v)))) }

/// Outcome of the shared "non-ok HTTP" branch of the embeddings,
/// transcription and speech loops.
enum HttpFailure { Retry, Throw(MediaError) }

#[allow(clippy::too_many_arguments)]
async fn handle_http_failure(
    host: &dyn MediaHost, up: Upstream, route: &RouteResult, url: &str, headers: &[(String, String)], api_type: &str, request_id: &str,
    can_retry_base: bool, fo: &Failover, history: &mut Vec<RetryAttemptRecord>, last_error: &mut MediaError,
) -> HttpFailure {
    let status = up.status;
    let can_retry = can_retry_base && is_retryable_status(status, &fo.codes);
    let error_text = up_text_keep(&up);
    let e = handle_provider_error(host, &up, route, &error_text, url, headers, api_type, request_id).await;
    drop(up);
    *last_error = e.clone();
    append_failure_attempt(history, route, &e, Some(api_type), Some(can_retry), None);
    if can_retry {
        host.record_attempt_metric(route, request_id, false).await;
        if get(&e, &["routingContext", "cooldownTriggered"]) == Some(&json!(true)) {
            host.mark_provider_failure(&route.provider, &route.model, None, format_failure_reason(&e, true)).await;
        }
        save_intermediate_error(host, request_id, api_type, &e).await;
        return HttpFailure::Retry;
    }
    HttpFailure::Throw(e)
}

/// The error body was already read into `Upstream::body_text`.
fn up_text_keep(up: &Upstream) -> String { up.body_text.clone().unwrap_or_default() }

async fn save_intermediate_error(host: &dyn MediaHost, request_id: &str, api_type: &str, error: &MediaError) {
    if request_id.is_empty() { return; }
    host.save_error(request_id, error, &error_details(api_type, error)).await;
}

/// `{ apiType, ...(error.routingContext || {}) }`.
pub fn error_details(api_type: &str, error: &MediaError) -> Value {
    let mut d = Obj::new();
    d.insert("apiType".into(), json!(api_type));
    if let Some(rc) = error.get("routingContext").and_then(Value::as_object) {
        for (k, v) in rc { d.insert(k.clone(), v.clone()); }
    }
    Value::Object(d)
}

/// The outer `catch` shared by the embeddings / transcription / speech loops.
/// Returns `true` to continue with the next target.
#[allow(clippy::too_many_arguments)]
async fn outer_catch(
    host: &dyn MediaHost, error: MediaError, route: &RouteResult, api_type: &str, request_id: &str, can_retry_base: bool, fo: &Failover,
    history: &mut Vec<RetryAttemptRecord>, last_error: &mut MediaError,
) -> bool {
    *last_error = error.clone();
    if !has_status_code(&error) {
        host.mark_provider_failure(&route.provider, &route.model, None, format_failure_reason(&error, false)).await;
    }
    host.record_attempt_metric(route, request_id, false).await;
    let can_retry = can_retry_base && is_retryable_network_error(Some(&error), &fo.errors);
    append_failure_attempt(history, route, &error, Some(api_type), Some(can_retry), None);
    if can_retry { save_intermediate_error(host, request_id, api_type, &error).await; }
    can_retry
}

/// Reads the error body of a failed upstream (kept on `Upstream`).
async fn with_error_text(mut up: Upstream) -> Upstream {
    let text = match up.response.take() {
        Some(r) => match r.bytes().await { Ok(b) => String::from_utf8_lossy(&b).into_owned(), Err(_) => String::new() },
        None => String::new(),
    };
    up.body_text = Some(text);
    up
}

/// `MediaDispatcher.dispatchEmbeddings(request)`.
pub async fn dispatch_embeddings(host: &dyn MediaHost, request: &Value) -> MediaResult<Value> {
    let config = host.config();
    let fo = failover(&config);
    let mut history = Vec::new();
    let candidates = resolve_targets(host, request, "embeddings", &mut history).await?;
    let targets = targets_for(candidates, fo.enabled);
    let request_id = request_id_of(request);
    let mut attempted: Vec<String> = Vec::new();
    let mut last_error = Value::Null;
    let total = targets.len();
    for (i, route) in targets.iter().enumerate() {
        if !host.routing().is_provider_healthy(&route.provider, &route.model).await {
            let reason = format!("Provider {}/{} is on cooldown", route.provider, route.model);
            last_error = json!({"message": reason});
            append_skipped_attempt(&mut history, route, &reason, Some("embeddings"));
            continue;
        }
        let Some(permit) = host.acquire(&route.provider, &route.model) else {
            let reason = format!("Provider {}/{} concurrency limit exceeded", route.provider, route.model);
            last_error = json!({"message": reason});
            append_skipped_attempt(&mut history, route, &reason, Some("embeddings"));
            continue;
        };
        attempted.push(format!("{}/{}", route.provider, route.model));
        if !request_id.is_empty() { host.emit_routing_update(&request_id, route); }
        let can_retry_base = fo.enabled && i < total - 1;

        let attempt: MediaResult<Option<Value>> = async {
            let transformer = EmbeddingsTarget::resolve(&get_provider_types(&route.config));
            let request_with_model = with_model(request, &route.model);
            let base_url = resolve_provider_base_url(route, "embeddings").map_err(|e| engine_error_value(&e))?;
            let url = format!("{base_url}{}", transformer.endpoint(&request_with_model));
            let mut headers = vec![("Content-Type".to_owned(), "application/json".to_owned()), ("Accept".to_owned(), "application/json".to_owned())];
            if let Some(key) = api_key(route) {
                match transformer.auth_header(&key) { Some((k, v)) => set_header(&mut headers, &k, &v), None => set_header(&mut headers, "Authorization", &format!("Bearer {key}")) }
            }
            let headers = header_list(headers, route.config.get("headers"));
            let mut payload = transformer.transform_request(&request_with_model)?;
            merge_extra_bodies(&mut payload, route, &config);
            if !request_id.is_empty() { host.debug_transformed_request(&request_id, &payload); }
            let response = send(&url, &headers, Body::Json(stringify(&payload)), None).await.map_err(|e| attempt_error_value(&e))?;
            let up = Upstream::new(response);
            if !request_id.is_empty() { host.debug_response_meta(&request_id, up.status, &up.headers); }
            if !up.ok() {
                let up = with_error_text(up).await;
                return match handle_http_failure(host, up, route, &url, &headers, "embeddings", &request_id, can_retry_base, &fo, &mut history, &mut last_error).await {
                    HttpFailure::Retry => Ok(None),
                    HttpFailure::Throw(e) => Err(e),
                };
            }
            let raw = parse_json_response_body(host, up, &request_id, route, "embeddings").await?;
            if !request_id.is_empty() { host.debug_raw_response(&request_id, raw.clone()); }
            let transformed = transformer.transform_response(&raw, &request_with_model);
            let mut enriched = transformed;
            enriched["plexus"] = obj(vec![
                ("provider", Some(json!(route.provider))),
                ("model", Some(json!(route.model))),
                ("apiType", Some(json!("embeddings"))),
                ("isPassthrough", Some(json!(true))),
                ("pricing", route.model_config.as_ref().and_then(|m| m.get("pricing")).cloned()),
                ("providerDiscount", route.config.get("discount").cloned()),
                ("canonicalModel", route.canonical_model.clone().map(Value::String)),
                ("config", Some(route.config.clone())),
            ]);
            host.record_attempt_metric(route, &request_id, true).await;
            host.mark_provider_success(&route.provider, &route.model).await;
            append_success_attempt(&mut history, route, Some("embeddings"), None);
            attach_attempt_metadata(&mut enriched, &attempted, &mut history, route, "embeddings", None);
            Ok(Some(enriched))
        }
        .await;
        match attempt {
            Ok(Some(response)) => { drop(permit); return Ok(response); }
            // Plexus `continue`s here without releasing the slot.
            Ok(None) => { std::mem::forget(permit); continue; }
            Err(error) => {
                let retry = outer_catch(host, error, route, "embeddings", &request_id, can_retry_base, &fo, &mut history, &mut last_error).await;
                drop(permit);
                if retry { continue; }
                return Err(build_all_targets_failed_error(&last_error, &attempted, &history));
            }
        }
    }
    Err(build_all_targets_failed_error(&last_error, &attempted, &history))
}

/// `MediaDispatcher.dispatchTranscription(request)`.
pub async fn dispatch_transcription(host: &dyn MediaHost, request: &TranscriptionRequest) -> MediaResult<Value> {
    let config = host.config();
    let fo = failover(&config);
    let mut history = Vec::new();
    let view = request.routing_view();
    let candidates = resolve_targets(host, &view, "transcriptions", &mut history).await?;
    let targets = targets_for(candidates, fo.enabled);
    let request_id = request.request_id.clone();
    let mut attempted: Vec<String> = Vec::new();
    let mut last_error = Value::Null;
    let total = targets.len();
    for (i, route) in targets.iter().enumerate() {
        if !host.routing().is_provider_healthy(&route.provider, &route.model).await {
            let reason = format!("Provider {}/{} is on cooldown", route.provider, route.model);
            last_error = json!({"message": reason});
            append_skipped_attempt(&mut history, route, &reason, Some("transcriptions"));
            continue;
        }
        let Some(permit) = host.acquire(&route.provider, &route.model) else {
            let reason = format!("Provider {}/{} concurrency limit exceeded", route.provider, route.model);
            last_error = json!({"message": reason});
            append_skipped_attempt(&mut history, route, &reason, Some("transcriptions"));
            continue;
        };
        attempted.push(format!("{}/{}", route.provider, route.model));
        if !request_id.is_empty() { host.emit_routing_update(&request_id, route); }
        let can_retry_base = fo.enabled && i < total - 1;

        let attempt: MediaResult<Option<Value>> = async {
            let base_url = resolve_provider_base_url(route, "transcriptions").map_err(|e| engine_error_value(&e))?;
            let url = format!("{base_url}/audio/transcriptions");
            let mut headers = Vec::new();
            if let Some(key) = api_key(route) { headers.push(("Authorization".to_owned(), format!("Bearer {key}"))); }
            let headers = header_list(headers, route.config.get("headers"));
            let form = tf::transcription_form(request, &route.model);
            if !request_id.is_empty() {
                host.debug_transformed_request(&request_id, &obj(vec![
                    ("model", Some(json!(request.model))),
                    ("filename", request.filename.clone().map(Value::String)),
                    ("mimeType", request.mime_type.clone().map(Value::String)),
                    ("language", request.language.clone().map(Value::String)),
                    ("prompt", request.prompt.clone().map(Value::String)),
                    ("response_format", Some(json!(request.response_format))),
                    ("temperature", request.temperature.map(num)),
                ]));
            }
            let response = send(&url, &headers, Body::Form(form), None).await.map_err(|e| attempt_error_value(&e))?;
            let up = Upstream::new(response);
            if !request_id.is_empty() { host.debug_response_meta(&request_id, up.status, &up.headers); }
            if !up.ok() {
                let up = with_error_text(up).await;
                return match handle_http_failure(host, up, route, &url, &headers, "transcriptions", &request_id, can_retry_base, &fo, &mut history, &mut last_error).await {
                    HttpFailure::Retry => Ok(None),
                    HttpFailure::Throw(e) => Err(e),
                };
            }
            let format = if request.response_format.is_empty() { "json" } else { request.response_format.as_str() };
            let body = if format == "text" { Value::String(up.text().await) } else { response_json(up).await? };
            if !request_id.is_empty() { host.debug_raw_response(&request_id, body.clone()); }
            let mut unified = tf::transcription_response(&body, format);
            unified["plexus"] = plexus_meta(route, "transcriptions", None);
            host.record_attempt_metric(route, &request_id, true).await;
            append_success_attempt(&mut history, route, Some("transcriptions"), None);
            attach_attempt_metadata(&mut unified, &attempted, &mut history, route, "transcriptions", None);
            Ok(Some(unified))
        }
        .await;
        match attempt {
            Ok(Some(response)) => { drop(permit); return Ok(response); }
            Ok(None) => { std::mem::forget(permit); continue; }
            Err(error) => {
                drop(permit);
                if outer_catch(host, error, route, "transcriptions", &request_id, can_retry_base, &fo, &mut history, &mut last_error).await { continue; }
                return Err(build_all_targets_failed_error(&last_error, &attempted, &history));
            }
        }
    }
    Err(build_all_targets_failed_error(&last_error, &attempted, &history))
}

fn plexus_meta(route: &RouteResult, api_type: &str, target_api_type: Option<&str>) -> Value {
    obj(vec![
        ("provider", Some(json!(route.provider))),
        ("model", Some(json!(route.model))),
        ("apiType", Some(json!(api_type))),
        ("targetApiType", target_api_type.map(|t| json!(t))),
        ("pricing", route.model_config.as_ref().and_then(|m| m.get("pricing")).cloned()),
        ("providerDiscount", route.config.get("discount").cloned()),
        ("canonicalModel", route.canonical_model.clone().map(Value::String)),
        ("config", Some(route.config.clone())),
    ])
}

/// `UnifiedSpeechResponse`: audio bytes (or the SSE byte stream, buffered
/// like Plexus) plus routing metadata.
pub struct SpeechResponse {
    pub audio: Vec<u8>,
    pub is_streamed: bool,
    pub plexus: Value,
}

/// `MediaDispatcher.dispatchSpeech(request)`.
pub async fn dispatch_speech(host: &dyn MediaHost, request: &Value) -> MediaResult<SpeechResponse> {
    let config = host.config();
    let fo = failover(&config);
    let mut history = Vec::new();
    let candidates = resolve_targets(host, request, "speech", &mut history).await?;
    let targets = targets_for(candidates, fo.enabled);
    let request_id = request_id_of(request);
    let mut attempted: Vec<String> = Vec::new();
    let mut last_error = Value::Null;
    let total = targets.len();
    for (i, route) in targets.iter().enumerate() {
        if !host.routing().is_provider_healthy(&route.provider, &route.model).await {
            let reason = format!("Provider {}/{} is on cooldown", route.provider, route.model);
            last_error = json!({"message": reason});
            append_skipped_attempt(&mut history, route, &reason, Some("speech"));
            continue;
        }
        let Some(permit) = host.acquire(&route.provider, &route.model) else {
            let reason = format!("Provider {}/{} concurrency limit exceeded", route.provider, route.model);
            last_error = json!({"message": reason});
            append_skipped_attempt(&mut history, route, &reason, Some("speech"));
            continue;
        };
        attempted.push(format!("{}/{}", route.provider, route.model));
        if !request_id.is_empty() { host.emit_routing_update(&request_id, route); }
        let can_retry_base = fo.enabled && i < total - 1;

        enum Step { Done(SpeechResponse), Retry, RetryReleased }
        let attempt: MediaResult<Step> = async {
            let base_url = resolve_provider_base_url(route, "speech").map_err(|e| engine_error_value(&e))?;
            let url = format!("{base_url}/audio/speech");
            let mut headers = vec![("Content-Type".to_owned(), "application/json".to_owned())];
            if let Some(key) = api_key(route) { headers.push(("Authorization".to_owned(), format!("Bearer {key}"))); }
            let mut headers = header_list(headers, route.config.get("headers"));
            let mut payload = tf::speech_request(&with_model(request, &route.model));
            merge_extra_bodies(&mut payload, route, &config);
            let is_streamed = request.get("stream_format") == Some(&json!("sse"));
            if !request_id.is_empty() { host.debug_transformed_request(&request_id, &payload); }
            set_header(&mut headers, "Accept", if is_streamed { "text/event-stream" } else { "audio/*" });
            let response = send(&url, &headers, Body::Json(stringify(&payload)), None).await.map_err(|e| attempt_error_value(&e))?;
            let up = Upstream::new(response);
            if !request_id.is_empty() { host.debug_response_meta(&request_id, up.status, &up.headers); }
            if !up.ok() {
                let up = with_error_text(up).await;
                return match handle_http_failure(host, up, route, &url, &headers, "speech", &request_id, can_retry_base, &fo, &mut history, &mut last_error).await {
                    HttpFailure::Retry => Ok(Step::Retry),
                    HttpFailure::Throw(e) => Err(e),
                };
            }
            let audio = if is_streamed {
                match probe_streaming_start(up.response.expect("unread response"), None).await {
                    Probe::Failed { error, stream_started } => {
                        let err = attempt_error_value(&error);
                        last_error = err.clone();
                        let can_retry = can_retry_base && !stream_started
                            && (is_retryable_network_error(Some(&err), &fo.errors) || error.is_stream_error);
                        if can_retry {
                            host.record_attempt_metric(route, &request_id, false).await;
                            append_failure_attempt(&mut history, route, &err, Some("speech"), Some(true), None);
                            host.mark_provider_failure(&route.provider, &route.model, error.cooldown_duration.map(|d| d as f64), error.message.clone()).await;
                            save_intermediate_error(host, &request_id, "speech", &err).await;
                            return Ok(Step::RetryReleased);
                        }
                        if error.is_stream_error {
                            host.mark_provider_failure(&route.provider, &route.model, error.cooldown_duration.map(|d| d as f64), error.message.clone()).await;
                        }
                        return Err(err);
                    }
                    Probe::Ok { mut stream, .. } => {
                        use futures_util::StreamExt;
                        let mut buf = Vec::new();
                        while let Some(chunk) = stream.next().await {
                            match chunk { Ok(c) => buf.extend_from_slice(&c), Err(e) => return Err(json!({"message": e})) }
                        }
                        buf
                    }
                }
            } else {
                up.bytes().await.map_err(|e| json!({"message": e}))?
            };
            if !request_id.is_empty() { host.debug_raw_response(&request_id, json!({"size": audio.len(), "isStreamed": is_streamed})); }
            let mut unified = json!({"plexus": plexus_meta(route, "speech", None)});
            host.record_attempt_metric(route, &request_id, true).await;
            append_success_attempt(&mut history, route, Some("speech"), None);
            attach_attempt_metadata(&mut unified, &attempted, &mut history, route, "speech", None);
            Ok(Step::Done(SpeechResponse { audio, is_streamed, plexus: unified["plexus"].clone() }))
        }
        .await;
        match attempt {
            Ok(Step::Done(response)) => { drop(permit); return Ok(response); }
            Ok(Step::Retry) => { std::mem::forget(permit); continue; }
            // The stream-probe retry path also skips the release.
            Ok(Step::RetryReleased) => { std::mem::forget(permit); continue; }
            Err(error) => {
                drop(permit);
                if outer_catch(host, error, route, "speech", &request_id, can_retry_base, &fo, &mut history, &mut last_error).await { continue; }
                return Err(build_all_targets_failed_error(&last_error, &attempted, &history));
            }
        }
    }
    Err(build_all_targets_failed_error(&last_error, &attempted, &history))
}

const RESERVED_IMAGE_PROVIDER_OPTION_KEYS: [&str; 22] = [
    "model", "prompt", "n", "resolution", "aspect_ratio", "size", "quality", "output_format", "background", "output_compression", "seed",
    "input_references", "image", "mask", "response_format", "stream", "provider", "headers", "authorization", "api_key", "base_url", "endpoint",
];

fn image_routing_error(message: &str) -> MediaError {
    json!({"message": message, "routingContext": {"statusCode": 400, "code": "invalid_request_error"}})
}

/// `applyImageProviderPreferences(candidates, request)`.
fn apply_image_provider_preferences(candidates: Vec<RouteResult>, request: &Value) -> MediaResult<Vec<RouteResult>> {
    let Some(prefs) = request.get("provider").filter(|p| truthy(Some(p))) else { return Ok(candidates) };
    if prefs.get("sort").is_some() { return Err(image_routing_error("provider.sort is not supported for image routing")); }
    let list = |k: &str| prefs.get(k).and_then(Value::as_array).map(|a| a.iter().map(|v| to_string(Some(v))).collect::<Vec<_>>());
    let ignored = list("ignore").unwrap_or_default();
    let mut eligible: Vec<RouteResult> = candidates.into_iter().filter(|c| !ignored.contains(&c.provider)).collect();
    if let Some(only) = list("only").filter(|o| !o.is_empty()) {
        eligible.retain(|c| only.contains(&c.provider));
        if eligible.is_empty() { return Err(image_routing_error("provider.only did not match any configured image target")); }
    }
    if let Some(order) = list("order").filter(|o| !o.is_empty()) {
        // `new Map(order.map((p, i) => [p, i]))`: the last index wins.
        let rank = |p: &str| order.iter().rposition(|o| o == p).unwrap_or(usize::MAX);
        let mut ranked: Vec<(usize, usize, RouteResult)> = eligible.into_iter().enumerate().map(|(i, c)| (rank(&c.provider), i, c)).collect();
        ranked.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        eligible = ranked.into_iter().map(|(_, _, c)| c).collect();
    }
    if eligible.is_empty() { return Err(image_routing_error("Image provider preferences excluded every configured target")); }
    Ok(eligible)
}

/// `mergeImagePayload(payload, extraBody)`.
fn merge_image_payload(payload: &mut ImagePayload, extra: Option<&Value>) {
    let Some(extra) = extra.filter(|e| truthy(Some(e))).and_then(Value::as_object) else { return };
    match payload {
        ImagePayload::Json(p) => {
            let mut out = p.as_object().cloned().unwrap_or_default();
            for (k, v) in extra { out.insert(k.clone(), v.clone()); }
            *p = Value::Object(out);
        }
        ImagePayload::Form(parts) => {
            for (k, v) in extra {
                if v.is_null() { continue; }
                form_set(parts, k, match v { Value::String(s) => s.clone(), other => stringify(other) });
            }
        }
    }
}

/// `providerImageOptions(request, provider)`.
fn provider_image_options(request: &Value, provider: &str) -> Option<Value> {
    let options = get(request, &["provider", "options"]).filter(|o| o.is_object())?;
    let scoped = options.get(provider).filter(|s| s.is_object())?.as_object()?;
    let sanitized: Obj = scoped.iter().filter(|(k, _)| !RESERVED_IMAGE_PROVIDER_OPTION_KEYS.contains(&k.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect();
    (!sanitized.is_empty()).then_some(Value::Object(sanitized))
}

/// `buildTimeoutError()`.
fn timeout_error() -> MediaError { json!({"message": "Upstream timeout", "routingContext": {"statusCode": 504, "code": "upstream_timeout"}}) }

/// `resolveImageDispatchTarget(route, targetApiType, transformer)`.
async fn resolve_image_dispatch_target(host: &dyn MediaHost, route: &RouteResult, target_api_type: &str, transformer: ImageTarget) -> MediaResult<(String, Vec<(String, String)>)> {
    let mut headers = vec![("Accept".to_owned(), "application/json".to_owned())];
    let base_url;
    if is_oauth_route(route, target_api_type) {
        let oauth_provider = route.config.get("oauth_provider").filter(|v| truthy(Some(v))).map(|v| to_string(Some(v))).unwrap_or_else(|| route.provider.clone());
        if oauth_provider != "openai-codex" || crate::engine::config::api_format::base_type(target_api_type) != "codex-images" {
            return Err(image_routing_error(&format!("OAuth provider {oauth_provider} cannot serve image target {target_api_type}")));
        }
        let account = route.config.get("oauth_account").and_then(Value::as_str).map(str::trim);
        let (url, extra) = host.prepare_codex_images(&route.model, account).await?;
        base_url = url;
        for (k, v) in extra { set_header(&mut headers, &k, &v); }
    } else {
        base_url = resolve_image_provider_base_url(route, target_api_type).map_err(|e| engine_error_value(&e))?;
        if let Some(key) = api_key(route) {
            match transformer.auth_header(&key) { Some((k, v)) => set_header(&mut headers, &k, &v), None => set_header(&mut headers, "Authorization", &format!("Bearer {key}")) }
        }
    }
    Ok((base_url, header_list(headers, route.config.get("headers"))))
}

fn attempt_timeout_ms(host: &dyn MediaHost, route: &RouteResult, resolve_timeout_ms: Option<&(dyn Fn(Option<f64>) -> f64 + Sync)>) -> f64 {
    let provider = route.config.get("timeoutMs").and_then(Value::as_f64);
    match resolve_timeout_ms {
        Some(f) => f(provider),
        None => provider.unwrap_or_else(|| get_f64(&host.config().root, &["timeout", "defaultSeconds"]).unwrap_or(300.0) * 1000.0),
    }
}

/// `MediaDispatcher.dispatchImageGenerations(request, signal, resolveTimeoutMs)`.
/// Returns the unified image response with `plexus` metadata attached.
pub async fn dispatch_image_generations(host: &dyn MediaHost, request: &Value, resolve_timeout_ms: Option<&(dyn Fn(Option<f64>) -> f64 + Sync)>) -> MediaResult<Value> {
    let config = host.config();
    let fo = failover(&config);
    let rt = host.routing();
    let model = to_string(request.get("model"));
    let mut candidates = resolve_candidates(rt, &model, Some("images"), None).await.map_err(|e| engine_error_value(&e))?;
    if candidates.is_empty() { candidates = vec![resolve(rt, &model, Some("images")).await.map_err(|e| engine_error_value(&e))?]; }
    let candidates = apply_key_access_policy(request, candidates, "images").map_err(|e| engine_error_value(&e))?;
    let candidates = apply_image_provider_preferences(candidates, request)?;
    let mut history = Vec::new();
    let candidates = apply_quota_filter(host, request, candidates, &mut history, "images")?;
    let allows_fallbacks = get(request, &["provider", "allow_fallbacks"]) != Some(&Value::Bool(false));
    let targets = targets_for(candidates, fo.enabled && allows_fallbacks);
    let request_id = request_id_of(request);
    let mut attempted: Vec<String> = Vec::new();
    let mut last_error = Value::Null;
    let total = targets.len();

    for (i, route) in targets.iter().enumerate() {
        // `admitProvider(route)`.
        if !rt.is_provider_healthy(&route.provider, &route.model).await {
            let reason = format!("Provider {}/{} is on cooldown", route.provider, route.model);
            last_error = json!({"message": reason});
            append_skipped_attempt(&mut history, route, &reason, Some("images"));
            continue;
        }
        let Some(permit) = host.acquire(&route.provider, &route.model) else {
            let reason = format!("Provider {}/{} concurrency limit exceeded", route.provider, route.model);
            last_error = json!({"message": reason});
            append_skipped_attempt(&mut history, route, &reason, Some("images"));
            continue;
        };
        attempted.push(format!("{}/{}", route.provider, route.model));
        let timeout = Duration::from_millis(attempt_timeout_ms(host, route, resolve_timeout_ms).max(0.0) as u64);

        let work = async {
            if !request_id.is_empty() { host.emit_routing_update(&request_id, route); }
            let target_api_type = select_target_api_type(route, Some("images")).map_err(|e| engine_error_value(&e))?.target_api_type;
            let target_api_type = if target_api_type.is_empty() { "chat".to_owned() } else { target_api_type };
            let transformer = ImageTarget::resolve(Some(&target_api_type))?;
            let request_with_model = with_model(request, &route.model);
            let (base_url, mut headers) = resolve_image_dispatch_target(host, route, &target_api_type, transformer).await?;
            let url = format!("{base_url}{}", transformer.get_endpoint(&request_with_model)?);
            let mut payload = transformer.transform_request(&request_with_model).await?;
            merge_image_payload(&mut payload, route.config.get("extraBody"));
            merge_image_payload(&mut payload, route.model_config.as_ref().and_then(|m| m.get("extraBody")));
            if let Some(c) = route.canonical_model.as_deref() { merge_image_payload(&mut payload, config.alias(c).and_then(|a| a.get("extraBody"))); }
            merge_image_payload(&mut payload, provider_image_options(request, &route.provider).as_ref());
            if !request_id.is_empty() {
                let debug_payload = match &payload { ImagePayload::Json(p) => redact_image_data_urls(p), ImagePayload::Form(parts) => summarize_form_data(parts) };
                host.debug_transformed_request(&request_id, &debug_payload);
            }
            let body = match payload {
                ImagePayload::Json(p) => { set_header(&mut headers, "Content-Type", "application/json"); Body::Json(stringify(&p)) }
                ImagePayload::Form(parts) => Body::Form(parts),
            };
            let response = send(&url, &headers, body, None).await.map_err(|e| attempt_error_value(&e))?;
            let up = Upstream::new(response);
            if !request_id.is_empty() { host.debug_response_meta(&request_id, up.status, &up.headers); }
            if !up.ok() {
                let up = with_error_text(up).await;
                let text = up_text_keep(&up);
                return Err(handle_provider_error(host, &up, route, &text, &url, &headers, "images", &request_id).await);
            }
            let body = response_json(up).await?;
            if !request_id.is_empty() { host.debug_raw_response(&request_id, body.clone()); }
            let mut unified = transformer.transform_response(&body)?;
            unified["plexus"] = plexus_meta(route, "images", Some(&target_api_type));
            Ok::<Value, MediaError>(unified)
        };
        let outcome = match tokio::time::timeout(timeout, work).await { Ok(r) => r.map_err(|e| (e, false)), Err(_) => Err((timeout_error(), true)) };
        match outcome {
            Ok(mut unified) => {
                host.record_attempt_metric(route, &request_id, true).await;
                append_success_attempt(&mut history, route, Some("images"), None);
                attach_attempt_metadata(&mut unified, &attempted, &mut history, route, "images", None);
                drop(permit);
                return Ok(unified);
            }
            Err((error, timed_out)) => {
                last_error = error.clone();
                if timed_out || !has_status_code(&error) {
                    host.mark_provider_failure(&route.provider, &route.model, None, format_failure_reason(&error, false)).await;
                }
                host.record_attempt_metric(route, &request_id, false).await;
                let can_retry = fo.enabled && i < total - 1 && match status_code(&error) {
                    Some(code) if has_status_code(&error) => is_retryable_status(code, &fo.codes),
                    _ => is_retryable_network_error(Some(&error), &fo.errors),
                };
                append_failure_attempt(&mut history, route, &error, Some("images"), Some(can_retry), None);
                drop(permit);
                if can_retry {
                    save_intermediate_error(host, &request_id, "images", &error).await;
                    continue;
                }
                return Err(build_all_targets_failed_error(&last_error, &attempted, &history));
            }
        }
    }
    Err(build_all_targets_failed_error(&last_error, &attempted, &history))
}

/// `decisionsRoutingError(message)`.
fn decisions_routing_error(message: String) -> MediaError {
    json!({"message": message, "routingContext": {"statusCode": 400, "code": "invalid_request_error"}})
}

/// `decisionsEndpointForTarget(targetApiType)`.
fn decisions_endpoint_for_target(target_api_type: &str) -> MediaResult<&'static str> {
    match crate::engine::responses::extensions::get_api_base_type(target_api_type).as_str() {
        "systemone" | "openrouter-decisions" | "typesafe-decisions" => Ok("/systemone"),
        _ => Err(decisions_routing_error(format!("Target API type '{target_api_type}' cannot serve decisions requests"))),
    }
}

/// `buildDecisionsUpstreamPayload(request, route)`.
fn build_decisions_upstream_payload(request: &Value, route: &RouteResult, config: &Config) -> Value {
    let mut payload = Obj::new();
    payload.insert("model".into(), json!(route.model));
    payload.insert("state".into(), request.get("state").cloned().unwrap_or(Value::Null));
    payload.insert("questions".into(), request.get("questions").cloned().unwrap_or(Value::Null));
    let mut layers = vec![route.config.get("extraBody").cloned(), route.model_config.as_ref().and_then(|m| m.get("extraBody")).cloned()];
    if let Some(c) = route.canonical_model.as_deref() { layers.push(config.alias(c).and_then(|a| a.get("extraBody")).cloned()); }
    for layer in layers.into_iter().flatten() {
        if !truthy(Some(&layer)) { continue; }
        if let Some(extra) = layer.as_object() {
            for (k, v) in extra {
                if matches!(k.as_str(), "model" | "provider" | "state" | "questions" | "session_id" | "trace" | "user") { continue; }
                payload.insert(k.clone(), v.clone());
            }
        }
    }
    payload.insert("model".into(), json!(route.model));
    payload.insert("state".into(), request.get("state").cloned().unwrap_or(Value::Null));
    payload.insert("questions".into(), request.get("questions").cloned().unwrap_or(Value::Null));
    Value::Object(payload)
}

/// `MediaDispatcher.dispatchDecisions(request, signal, resolveTimeoutMs)`.
/// `parse_upstream` is `parseDecisionsUpstreamResponse` (message on failure).
pub async fn dispatch_decisions(
    host: &dyn MediaHost,
    request: &Value,
    resolve_timeout_ms: Option<&(dyn Fn(Option<f64>) -> f64 + Sync)>,
    parse_upstream: &(dyn Fn(&Value) -> Result<Value, String> + Sync),
) -> MediaResult<Value> {
    let config = host.config();
    let fo = failover(&config);
    let rt = host.routing();
    let model = to_string(request.get("model"));
    let mut candidates = resolve_candidates(rt, &model, Some("decisions"), None).await.map_err(|e| engine_error_value(&e))?;
    if candidates.is_empty() {
        match resolve(rt, &model, Some("decisions")).await {
            Ok(route) => candidates = vec![route],
            Err(_) => return Err(decisions_routing_error(format!("No decisions-capable target configured for model '{model}'"))),
        }
    }
    let candidates = apply_key_access_policy(request, candidates, "decisions").map_err(|e| engine_error_value(&e))?;
    let mut history = Vec::new();
    let candidates = apply_quota_filter(host, request, candidates, &mut history, "decisions")?;
    let targets = targets_for(candidates, fo.enabled);
    let request_id = request_id_of(request);
    let mut attempted: Vec<String> = Vec::new();
    let mut last_error = Value::Null;
    let total = targets.len();

    for (i, route) in targets.iter().enumerate() {
        if !rt.is_provider_healthy(&route.provider, &route.model).await {
            let reason = format!("Provider {}/{} is on cooldown", route.provider, route.model);
            last_error = json!({"message": reason});
            append_skipped_attempt(&mut history, route, &reason, Some("decisions"));
            continue;
        }
        let Some(permit) = host.acquire(&route.provider, &route.model) else {
            let reason = format!("Provider {}/{} concurrency limit exceeded", route.provider, route.model);
            last_error = json!({"message": reason});
            append_skipped_attempt(&mut history, route, &reason, Some("decisions"));
            continue;
        };
        attempted.push(format!("{}/{}", route.provider, route.model));
        let timeout = Duration::from_millis(attempt_timeout_ms(host, route, resolve_timeout_ms).max(0.0) as u64);

        let work = async {
            if !request_id.is_empty() { host.emit_routing_update(&request_id, route); }
            let target_api_type = select_target_api_type(route, Some("decisions")).map_err(|e| engine_error_value(&e))?.target_api_type;
            if target_api_type.is_empty() { return Err(decisions_routing_error("No API type available for decisions routing".into())); }
            let endpoint = decisions_endpoint_for_target(&target_api_type)?;
            let base_url = resolve_provider_base_url(route, &target_api_type).map_err(|e| engine_error_value(&e))?;
            let url = format!("{base_url}{endpoint}");
            let payload = build_decisions_upstream_payload(request, route, &config);
            let mut headers = vec![("Content-Type".to_owned(), "application/json".to_owned()), ("Accept".to_owned(), "application/json".to_owned())];
            if let Some(key) = api_key(route) { headers.push(("Authorization".to_owned(), format!("Bearer {key}"))); }
            let headers = header_list(headers, route.config.get("headers"));
            if !request_id.is_empty() { host.debug_transformed_request(&request_id, &payload); }
            let response = send(&url, &headers, Body::Json(stringify(&payload)), None).await.map_err(|e| attempt_error_value(&e))?;
            let up = Upstream::new(response);
            if !request_id.is_empty() { host.debug_response_meta(&request_id, up.status, &up.headers); }
            if !up.ok() {
                let up = with_error_text(up).await;
                let text = up_text_keep(&up);
                return Err(handle_provider_error(host, &up, route, &text, &url, &headers, "decisions", &request_id).await);
            }
            let body = response_json(up).await?;
            if !request_id.is_empty() { host.debug_raw_response(&request_id, body.clone()); }
            let parsed = parse_upstream(&body).map_err(|m| json!({
                "message": format!("Upstream returned an unusable Decisions response: {m}"),
                "routingContext": {"statusCode": 502, "code": "provider_response_error"},
            }))?;
            let mut unified = parsed;
            unified["plexus"] = obj(vec![
                ("provider", Some(json!(route.provider))),
                ("model", Some(json!(route.model))),
                ("apiType", Some(json!("decisions"))),
                ("targetApiType", Some(json!(target_api_type))),
                ("pricing", route.model_config.as_ref().and_then(|m| m.get("pricing")).cloned()),
                ("providerDiscount", route.config.get("discount").cloned()),
                ("canonicalModel", route.canonical_model.clone().map(Value::String)),
                ("config", Some(route.config.clone())),
            ]);
            Ok::<Value, MediaError>(unified)
        };
        let outcome = match tokio::time::timeout(timeout, work).await { Ok(r) => r.map_err(|e| (e, false)), Err(_) => Err((timeout_error(), true)) };
        match outcome {
            Ok(mut unified) => {
                host.record_attempt_metric(route, &request_id, true).await;
                append_success_attempt(&mut history, route, Some("decisions"), None);
                attach_attempt_metadata(&mut unified, &attempted, &mut history, route, "decisions", None);
                drop(permit);
                return Ok(unified);
            }
            Err((error, timed_out)) => {
                last_error = error.clone();
                if timed_out || !has_status_code(&error) {
                    host.mark_provider_failure(&route.provider, &route.model, None, format_failure_reason(&error, false)).await;
                }
                host.record_attempt_metric(route, &request_id, false).await;
                let can_retry = fo.enabled && i < total - 1 && match status_code(&error) {
                    Some(code) if has_status_code(&error) => is_retryable_status(code, &fo.codes),
                    _ => is_retryable_network_error(Some(&error), &fo.errors),
                };
                append_failure_attempt(&mut history, route, &error, Some("decisions"), Some(can_retry), None);
                drop(permit);
                if can_retry {
                    save_intermediate_error(host, &request_id, "decisions", &error).await;
                    continue;
                }
                return Err(build_all_targets_failed_error(&last_error, &attempted, &history));
            }
        }
    }
    Err(build_all_targets_failed_error(&last_error, &attempted, &history))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn json_errors_and_preferences() {
        let route = |p: &str| RouteResult { provider: p.into(), model: "m".into(), config: json!({}), model_config: None, incoming_model_alias: None, canonical_model: None };
        let out = apply_image_provider_preferences(vec![route("a"), route("b"), route("c")], &json!({"provider": {"order": ["c", "a"], "ignore": ["b"]}})).unwrap();
        assert_eq!(out.iter().map(|r| r.provider.as_str()).collect::<Vec<_>>(), vec!["c", "a"]);
        let err = apply_image_provider_preferences(vec![route("a")], &json!({"provider": {"only": ["z"]}})).unwrap_err();
        assert_eq!(err["message"], json!("provider.only did not match any configured image target"));
    }
}
