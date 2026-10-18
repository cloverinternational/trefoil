//! Plexus core inference routes on the engine
//! (`routes/inference/{chat,messages,gemini,responses,completions,_quota-error}.ts`):
//! `POST /v1/chat/completions`, `/v1/messages`, `/v1beta/models/:modelWithAction`,
//! `/v1/responses` (+ `/v1/codex/responses`), `/v1/completions` (+ `/completions`)
//! and the stored-response reads.
//!
//! OAuth / Claude-masking targets run through the same path
//! (`engine::dispatcher::oauth`).
use super::completions::OpenAICompletionTransformer;
use super::engine::{Engine, KEY_NAME};
use super::quota::{attach_quota_context, load_quota_context};
use super::reasoning_log::get_reasoning_log_value;
use super::request_manager::{dispatch, DispatchResponse};
use super::responses_storage as storage;
use crate::engine::anthropic::AnthropicTransformer;
use crate::engine::dispatch::service_tier_metadata::requested_service_tier_from_error;
use crate::engine::gemini::GeminiTransformer;
use crate::engine::js::*;
use crate::engine::observability::{handle_response, AbortKind, ErrorInfo, HandledBody, ResponseContext, StreamSession, UsageRecord};
use crate::engine::openai::OpenAITransformer;
use crate::engine::responses::{normalize_composite_responses_call_ids, normalize_responses_function_call_item_ids, normalize_responses_null_entries, normalize_responses_reasoning_content, ResponsesTransformer};
use crate::engine::routing::route_candidates::build_quota_exceeded_body;
use crate::engine::transformer::Transformer;
use crate::state::{AppState, KeyConfig};
use axum::{
    body::{Body, Bytes},
    extract::{Path, Request, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::Response,
    routing::{get as get_route, post},
    Router,
};
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::sync::Arc;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/v1/chat/completions", post(chat))
        .route("/v1/messages", post(messages))
        .route("/v1beta/models/:model_with_action", post(gemini))
        .route("/v1/responses", post(responses))
        .route("/v1/codex/responses", post(responses))
        .route("/v1/completions", post(completions))
        .route("/completions", post(completions))
        .route("/v1/responses/:response_id", get_route(get_response).delete(delete_response))
        .route("/v1/conversations/:conversation_id", get_route(get_conversation))
}

// ── auth (`createAuthHook` + `@fastify/bearer-auth` + `validatePlexusApiKey`) ──

/// Authenticated caller (`request.keyName`, `keyConfig`, `attribution`).
pub struct Caller {
    pub key_name: String,
    pub config: KeyConfig,
    pub attribution: Option<String>,
}

fn json_response(status: u16, body: &Value) -> Response {
    let mut r = Response::new(Body::from(stringify(body)));
    *r.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json; charset=utf-8"));
    r
}

fn auth_error(message: &str) -> Response { json_response(401, &json!({"error": {"message": message, "type": "auth_error", "code": 401}})) }

pub(crate) fn query_key(query: Option<&str>) -> Option<String> {
    url::form_urlencoded::parse(query?.as_bytes()).find(|(k, _)| k == "key").map(|(_, v)| v.into_owned())
}

pub fn authenticate(state: &AppState, headers: &HeaderMap, query: Option<&str>) -> Result<Caller, Response> {
    authenticate_peer(state, headers, query, None)
}

/// `authenticate` with the socket peer, which the key IP allowlist falls back to.
pub fn authenticate_peer(state: &AppState, headers: &HeaderMap, query: Option<&str>, peer: Option<std::net::IpAddr>) -> Result<Caller, Response> {
    let header_str = |k: &str| headers.get(k).and_then(|v| v.to_str().ok()).map(str::to_owned);
    let authorization = match header_str("authorization").filter(|a| !a.is_empty()) {
        Some(a) if a.to_lowercase().starts_with("bearer ") => Some(a),
        Some(a) => Some(format!("Bearer {a}")),
        None => header_str("x-api-key").or_else(|| header_str("x-goog-api-key")).or_else(|| query_key(query)).map(|k| format!("Bearer {k}")),
    };
    let Some(authorization) = authorization.filter(|a| !a.is_empty()) else { return Err(auth_error("missing authorization header")) };
    if !authorization.starts_with("Bearer ") { return Err(auth_error("invalid authorization header")); }
    let key = authorization[7..].trim().to_owned();
    let (secret, attribution) = match key.find(':') {
        Some(i) => (key[..i].to_owned(), Some(key[i + 1..].to_lowercase()).filter(|a| !a.is_empty())),
        None => (key.clone(), None),
    };
    let keys = state.inner.keys.read().unwrap();
    let Some((name, config)) = keys.iter().find(|(_, k)| k.secret == secret) else { return Err(auth_error("invalid authorization header")) };
    if config.is_disabled(now_ms()) { return Err(auth_error("invalid authorization header")); }
    let trusted: Option<Vec<String>> = state.inner.settings.read().unwrap().get("trustedProxies").and_then(Value::as_array).map(|a| a.iter().map(|v| to_string(Some(v))).collect());
    if !super::ip::is_ip_allowed(super::ip::trusted_client_ip(headers, peer, trusted.as_deref()).as_deref(), &config.allowed_ips) { return Err(auth_error("invalid authorization header")); }
    Ok(Caller { key_name: name.clone(), config: config.clone(), attribution })
}

/// `attachKeyAccessPolicy(request, unifiedRequest)`.
fn attach_key_access_policy(config: &KeyConfig, request: &mut Value) {
    let clean = |l: &[String]| l.iter().map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()).collect::<Vec<_>>();
    let (am, ap, em, ep) = (clean(&config.allowed_models), clean(&config.allowed_providers), clean(&config.excluded_models), clean(&config.excluded_providers));
    let tier = config.default_service_tier.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned);
    let has_lists = !(am.is_empty() && ap.is_empty() && em.is_empty() && ep.is_empty());
    if !has_lists && tier.is_none() { return; }
    let Some(map) = request.as_object_mut() else { return };
    let mut metadata = map.get("metadata").and_then(Value::as_object).cloned().unwrap_or_default();
    let mut plexus = metadata.get("plexus_metadata").and_then(Value::as_object).cloned().unwrap_or_default();
    if let Some(t) = tier { plexus.insert("defaultServiceTier".into(), json!(t)); }
    if has_lists {
        let mut policy = Obj::new();
        for (k, l) in [("allowedModels", am), ("allowedProviders", ap), ("excludedModels", em), ("excludedProviders", ep)] {
            if !l.is_empty() { policy.insert(k.into(), json!(l)); }
        }
        plexus.insert("plexus_key_policy".into(), Value::Object(policy));
    }
    metadata.insert("plexus_metadata".into(), Value::Object(plexus));
    map.insert("metadata".into(), Value::Object(metadata));
}

fn header_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> { headers.get(name).and_then(|v| v.to_str().ok()) }

/// `getClientRequestId(headers)`.
fn client_request_id(headers: &HeaderMap) -> Option<String> {
    let v = header_value(headers, "x-client-request-id")?.trim().to_owned();
    (!v.is_empty() && v.chars().count() <= 255).then_some(v)
}

/// `getCacheRoutingHeaders(headers, promptCacheKey)`.
fn cache_routing_headers(headers: &HeaderMap, prompt_cache_key: Option<&Value>) -> Option<Value> {
    let h = |k: &str| header_value(headers, k).filter(|v| !v.is_empty()).map(|v| json!(v));
    let pck = prompt_cache_key.filter(|v| truthy(Some(v))).cloned();
    let out = obj(vec![
        ("session_id", h("session_id").or_else(|| h("session-id")).or_else(|| pck.clone())),
        ("x-client-request-id", h("x-client-request-id").or(pck)),
        ("x-session-affinity", h("x-session-affinity")),
        ("x-session-id", h("x-session-id")),
        ("x-prompt-cache-isolation-key", h("x-prompt-cache-isolation-key")),
        ("x-multi-turn-session-id", h("x-multi-turn-session-id")),
        ("x-opencode-session", h("x-opencode-session")),
    ]);
    out.as_object().is_some_and(|m| m.values().any(|v| truthy(Some(v)))).then_some(out)
}

fn request_headers(headers: &HeaderMap) -> Vec<(String, String)> {
    headers.iter().filter_map(|(k, v)| v.to_str().ok().map(|v| (k.as_str().to_owned(), v.to_owned()))).collect()
}

// ── shared route flow ─────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
enum Api { Chat, Messages, Gemini, Responses, Completions }

impl Api {
    fn name(self) -> &'static str {
        match self { Api::Chat => "chat", Api::Messages => "messages", Api::Gemini => "gemini", Api::Responses => "responses", Api::Completions => "completions" }
    }
}

/// Plexus route `catch` reply body for `e`.
fn error_body(api: Api, status: u16, e: &Value) -> Value {
    let msg = e.get("message").cloned().unwrap_or(Value::Null);
    let code = get(e, &["routingContext", "code"]).filter(|c| truthy(Some(c))).cloned();
    match api {
        Api::Messages => {
            let kind = if status == 401 { "authentication_error" } else if status == 400 { "invalid_request_error" } else { "api_error" };
            json!({"type": "error", "error": obj(vec![("type", Some(json!(kind))), ("code", code), ("message", Some(msg))])})
        }
        Api::Gemini => {
            let st = if status == 401 { "UNAUTHENTICATED" } else if status == 400 { "INVALID_ARGUMENT" } else { "INTERNAL" };
            json!({"error": obj(vec![("message", Some(msg)), ("code", Some(json!(status))), ("status", Some(json!(st))), ("reason", code)])})
        }
        Api::Responses => {
            let message = if truthy(Some(&msg)) { msg } else { json!("Internal server error") };
            let rc = e.get("routingContext").filter(|r| truthy(Some(r)));
            json!({"error": obj(vec![
                ("message", Some(message)),
                ("type", Some(json!(if status >= 500 { "server_error" } else { "invalid_request_error" }))),
                ("code", code),
                ("routing_context", rc.map(|r| obj(vec![("provider", r.get("provider").cloned()), ("target_model", r.get("targetModel").cloned()), ("target_api_type", r.get("targetApiType").cloned())]))),
            ])})
        }
        Api::Chat | Api::Completions => {
            let kind = if status == 401 { "authentication_error" } else if status == 400 { "invalid_request_error" } else { "api_error" };
            json!({"error": obj(vec![("message", Some(msg)), ("type", Some(json!(kind))), ("code", code)])})
        }
    }
}

struct Flow {
    engine: Engine,
    api: Api,
    /// Usage `apiType` (`responses` / `responses:lite` for Responses).
    usage_api: String,
    request_id: String,
    client_request_id: Option<String>,
    start: i64,
    record: UsageRecord,
    /// Whether `emitStartedAsync` has run (see `ensure_started`).
    started: bool,
}

impl Flow {
    fn new(state: &AppState, headers: &HeaderMap, peer: Option<std::net::IpAddr>, api: Api, usage_api: &str, body: Option<&Value>) -> Self {
        let engine = Engine::new(state);
        let request_id = uuid();
        let start = now_ms();
        let client_request_id = client_request_id(headers);
        let effort = get_reasoning_log_value(None, body.unwrap_or(&Value::Null));
        let record = UsageRecord::from_value(json!({
            "requestId": request_id, "clientRequestId": client_request_id, "date": now_iso(), "sourceIp": super::ip::client_ip(headers, peer),
            "incomingApiType": usage_api, "startTime": start, "isStreamed": false, "responseStatus": "pending",
            "reasoningEffort": effort,
        }));
        Self { engine, api, usage_api: usage_api.to_owned(), request_id, client_request_id, start, record, started: false }
    }

    /// `emitStartedAsync(usageRecord)`. Plexus enqueues the shared record
    /// object, so the started event/pending row carries the caller fields
    /// assigned synchronously after it (alias, apiKey, attribution).
    fn ensure_started(&mut self) {
        if self.started { return; }
        self.started = true;
        self.engine.emit_started_async(self.record.clone());
    }

    fn headers(&self, mut r: Response) -> Response {
        let h = r.headers_mut();
        if let Ok(v) = HeaderValue::from_str(&self.request_id) { h.insert("x-request-id", v); }
        if let Some(c) = self.client_request_id.as_deref().and_then(|c| HeaderValue::from_str(c).ok()) { h.insert("x-client-request-id", c); }
        r
    }

    fn json(&self, status: u16, body: &Value) -> Response { self.headers(json_response(status, body)) }

    fn caller_fields(&mut self, caller: &Caller, model: Option<Value>) {
        self.record.set_opt("incomingModelAlias", model.clone());
        self.record.set("apiKey", json!(caller.key_name));
        let attribution = caller.attribution.clone().map(Value::String).unwrap_or(Value::Null);
        self.record.set("attribution", attribution.clone());
        self.ensure_started();
        self.engine.emit_updated_async(UsageRecord::from_value(obj(vec![
            ("requestId", Some(json!(self.request_id))), ("incomingModelAlias", model), ("apiKey", Some(json!(caller.key_name))), ("attribution", Some(attribution)),
        ])));
    }

    /// `DebugManager.startLog(requestId, body, sanitizeHeaders(headers))`.
    fn start_log(&self, body: Option<&Value>, headers: &HeaderMap, query: Option<&str>) {
        self.engine.debug().start_log(&self.request_id, body, &crate::media_routes::auth_normalized_headers(headers, query));
    }

    /// The route `catch` block.
    async fn fail(&mut self, e: Value) -> Response {
        self.ensure_started();
        let rc = |k: &str| get(&e, &["routingContext", k]).cloned();
        let code = rc("code").and_then(|c| c.as_str().map(str::to_owned));
        let sink = self.engine.sink();
        let fill = |record: &mut UsageRecord| {
            let attempt = rc("attemptCount").filter(|v| truthy(Some(v))).or_else(|| record.get("attemptCount").filter(|v| truthy(Some(v))).cloned()).unwrap_or(json!(1));
            record.set("attemptCount", attempt);
            let history = rc("retryHistory").filter(|v| truthy(Some(v))).or_else(|| record.get("retryHistory").filter(|v| truthy(Some(v))).cloned()).unwrap_or(Value::Null);
            record.set("retryHistory", history);
        };
        let details = {
            let mut d = Obj::new();
            d.insert("apiType".into(), json!(self.usage_api));
            if let Some(m) = e.get("routingContext").and_then(Value::as_object) { for (k, v) in m { d.insert(k.clone(), v.clone()); } }
            Value::Object(d)
        };
        let info = ErrorInfo::new(to_string(e.get("message")));
        if code.as_deref() == Some("client_disconnected") {
            self.record.set("responseStatus", "cancelled");
            self.record.set_num("durationMs", (now_ms() - self.start) as f64);
            fill(&mut self.record);
            if let Some((t, raw)) = requested_service_tier_from_error(&e) { self.record.set("requestedServiceTier", t); self.record.set("requestedServiceTierRaw", raw); }
            sink.save_request(&self.record).await;
            crate::system_logs::event(&self.engine.state, "info", format!("Request {}: {}, usage recorded as cancelled", self.request_id, info.message), [("module".to_owned(), json!(match self.api { Api::Chat => "chat", Api::Messages => "messages", Api::Gemini => "gemini", Api::Responses => "responses", Api::Completions => "completions" }))]);
            return self.headers(Response::new(Body::empty()));
        }
        if code.as_deref() == Some("quota_exceeded") {
            self.record.set("responseStatus", "quota_exceeded");
            self.record.set_num("durationMs", (now_ms() - self.start) as f64);
            fill(&mut self.record);
            sink.save_request(&self.record).await;
            sink.save_error(&self.request_id, &info, Some(&details), None).await;
            crate::engine::observability::DebugHooks::flush(&self.engine.debug(), &self.request_id);
            return self.json(429, &rc("body").unwrap_or(Value::Null));
        }
        self.record.set("responseStatus", if code.as_deref() == Some("upstream_timeout") { "timeout" } else { "error" });
        self.record.set_num("durationMs", (now_ms() - self.start) as f64);
        fill(&mut self.record);
        if let Some((t, raw)) = requested_service_tier_from_error(&e) { self.record.set("requestedServiceTier", t); self.record.set("requestedServiceTierRaw", raw); }
        sink.save_request(&self.record).await;
        sink.save_error(&self.request_id, &info, Some(&details), None).await;
        crate::engine::observability::DebugHooks::flush(&self.engine.debug(), &self.request_id);
        let (label, module) = match self.api {
            Api::Chat => ("Error processing OpenAI request", "chat"),
            Api::Messages => ("Error processing Anthropic request", "messages"),
            Api::Gemini => ("Error processing Gemini request", "gemini"),
            Api::Responses => ("Error processing Responses API request", "responses"),
            Api::Completions => ("Error processing Completions request", "completions"),
        };
        crate::system_logs::route_error(&self.engine.state, label, module, &e);
        let status = rc("statusCode").filter(|s| truthy(Some(s))).and_then(|s| s.as_f64()).map(|s| s as u16).filter(|s| (100..=599).contains(s)).unwrap_or(500);
        let body = error_body(self.api, status, &e);
        self.json(status, &body)
    }

    /// Quota pre-check (`checkQuotaMiddleware`); `Err` is the 429 reply.
    async fn check_quota(&mut self, caller: &Caller, unified: &mut Value) -> Result<Option<Value>, Response> {
        let Some(ctx) = load_quota_context(&self.engine.state, &self.engine.config, &caller.key_name).await else { return Ok(None) };
        if let Some(blocked) = ctx.get("blockedGlobal").filter(|b| !b.is_null()) {
            let body = build_quota_exceeded_body(std::slice::from_ref(blocked));
            self.record.set("responseStatus", "quota_exceeded");
            self.record.set_num("durationMs", (now_ms() - self.start) as f64);
            self.engine.sink().save_request(&self.record).await;
            crate::engine::observability::DebugHooks::flush(&self.engine.debug(), &self.request_id);
            return Err(self.json(429, &body));
        }
        attach_quota_context(unified, &ctx);
        Ok(Some(ctx))
    }

    /// `usageStorage.emitUpdatedAsync({... routing decision ...})`.
    fn routed(&self, plexus: Option<&Value>) {
        let p = |k: &str| plexus.and_then(|p| p.get(k)).cloned();
        self.engine.emit_updated_async(UsageRecord::from_value(obj(vec![
            ("requestId", Some(json!(self.request_id))),
            ("provider", p("provider")),
            ("selectedModelName", p("model")),
            ("canonicalModelName", p("canonicalModel")),
            ("reasoningEffort", self.record.get("reasoningEffort").cloned()),
            ("requestedServiceTier", p("requestedServiceTier")),
            ("requestedServiceTierRaw", p("requestedServiceTierRaw")),
            ("serviceTier", p("serviceTier")),
            ("serviceTierRaw", p("serviceTierRaw")),
        ])));
    }

    /// Dispatch + `handleResponse`, mapping errors through `fail`.
    async fn run(&mut self, caller: &Caller, mut unified: Value, client: Arc<dyn Transformer>, body: &Value, headers: &HeaderMap, quota_context: Option<Value>, metadata: impl FnOnce(&mut UsageRecord, &Value)) -> Result<(Response, Value), Response> {
        // `wireEarlyDisconnectDetection`: a client that goes away while the
        // dispatch is in flight drops this future; record it as cancelled.
        let mut guard = CancelGuard::new(self, &self.usage_api != "responses:lite");
        let dispatched = dispatch(&self.engine, &mut unified).await;
        guard.disarm();
        let result = match dispatched {
            Ok(r) => r,
            Err(e) => return Err(self.fail(e).await),
        };
        let DispatchResponse { unified: response, stream, stall } = result;
        self.routed(response.get("plexus"));
        let estimate = truthy(get(&response, &["plexus", "config", "estimateTokens"]));
        metadata(&mut self.record, &unified);
        let ctx = ResponseContext {
            record: self.record.clone(), start_time: self.start, api_type: self.api.name().into(), should_estimate_tokens: estimate,
            original_request: Some(body.clone()), quota_enforcer: true, key_name: Some(caller.key_name.clone()),
            request_headers: request_headers(headers), quota_context,
        };
        let is_stream = stream.is_some();
        let svc = self.engine.response_services();
        // `wireStallDetection(abortController, getGlobalStallConfig())`.
        let global = super::stall::global_stall_config(&self.engine.config);
        let base = match stall { Some(merged) => merged, None => global };
        let (inspector, stall_handle) = super::stall_inspector::StallInspector::new(base.unwrap_or_else(super::stall_inspector::default_stall_config));
        let observer: Option<Box<dyn crate::engine::observability::StallObserver>> = if is_stream { Some(Box::new(inspector)) } else { None };
        let handled = match handle_response(&svc, ctx, response.clone(), client, is_stream, observer).await {
            Ok(h) => h,
            Err(e) => return Err(self.fail(crate::engine::dispatch::errors::engine_error_value(&e)).await),
        };
        let mut r = match handled.body {
            HandledBody::Json(v) => json_response(handled.status, &v),
            HandledBody::Stream(session) => stream_response(session, stream.unwrap(), Some(stall_handle)),
        };
        *r.status_mut() = StatusCode::from_u16(handled.status).unwrap_or(StatusCode::OK);
        for (k, v) in handled.headers {
            if let (Ok(name), Ok(value)) = (header::HeaderName::from_bytes(k.to_lowercase().as_bytes()), HeaderValue::from_str(&v)) { r.headers_mut().insert(name, value); }
        }
        Ok((self.headers(r), response))
    }
}

/// Saves the `client_disconnected` usage row (`responseStatus: 'cancelled'`)
/// if dropped while armed.
struct CancelGuard {
    armed: bool,
    sink: Arc<dyn crate::engine::observability::UsageSink>,
    record: UsageRecord,
    start: i64,
}

impl CancelGuard {
    fn new(flow: &Flow, armed: bool) -> Self {
        Self { armed, sink: flow.engine.sink(), record: flow.record.clone(), start: flow.start }
    }
    fn disarm(&mut self) { self.armed = false; }
}

impl Drop for CancelGuard {
    fn drop(&mut self) {
        if !self.armed { return; }
        let mut record = self.record.clone();
        record.set("responseStatus", "cancelled");
        record.set_num("durationMs", (now_ms() - self.start) as f64);
        let attempt = record.get("attemptCount").filter(|v| truthy(Some(v))).cloned().unwrap_or(json!(1));
        record.set("attemptCount", attempt);
        let history = record.get("retryHistory").filter(|v| truthy(Some(v))).cloned().unwrap_or(Value::Null);
        record.set("retryHistory", history);
        let sink = self.sink.clone();
        tokio::spawn(async move { sink.save_request(&record).await });
    }
}

/// Client-bound SSE body: the upstream bytes are pumped through the session
/// on a task; a dropped client aborts it (`onDisconnect`).
fn stream_response(mut session: Box<StreamSession>, mut upstream: crate::engine::execute::ByteStream, stall: Option<super::stall_inspector::StallHandle>) -> Response {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(64);
    tokio::spawn(async move {
        let mut client_gone = false;
        let mut deferred = None;
        loop {
            // StallInspector timers (grace end / periodic throughput check).
            let wake = stall.as_ref().and_then(|s| s.next_deadline()).map(|at| tokio::time::Instant::now() + std::time::Duration::from_millis((at - now_ms()).max(0) as u64));
            let next = if let Some(d) = deferred.take() { d } else { match wake {
                Some(at) => tokio::select! {
                    n = upstream.next() => n,
                    _ = tokio::time::sleep_until(at) => {
                        if let Some(message) = stall.as_ref().and_then(|s| s.poll()) {
                            session.abort(AbortKind::Stall { message: Some(message.clone()) }).await;
                            // `pipeline.destroy()`: the reply is cut off mid-body.
                            let _ = tx.send(Err(std::io::Error::other(message))).await;
                            return;
                        }
                        continue;
                    }
                },
                None => upstream.next().await,
            } };
            let chunks = match &next {
                Some(Ok(bytes)) => {
                    // Like Bun's fetch reader, take everything already
                    // buffered as one chunk (affects TTFT/terminal detection
                    // and stall samples the same way).
                    let mut buf = bytes.to_vec();
                    let mut tail = None;
                    while let Some(more) = futures_util::FutureExt::now_or_never(upstream.next()) {
                        match more { Some(Ok(b)) => buf.extend_from_slice(&b), other => { tail = Some(other); break; } }
                    }
                    if tail.is_some() { deferred = tail; }
                    session.push(&buf).await
                }
                Some(Err(_)) | None => session.finish().await,
            };
            // A chunk that trips the throughput check never reaches the client
            // (the TS pipeline is destroyed inside the inspector's transform).
            if let Some(message) = stall.as_ref().and_then(|s| s.take_stall()) {
                session.abort(AbortKind::Stall { message: Some(message.clone()) }).await;
                let _ = tx.send(Err(std::io::Error::other(message))).await;
                return;
            }
            for c in chunks {
                if client_gone { break; }
                if tx.send(Ok(Bytes::from(c))).await.is_err() { client_gone = true; }
            }
            if client_gone && !session.defers_socket_close() {
                session.abort(AbortKind::ClientDisconnect).await;
                return;
            }
            if !matches!(next, Some(Ok(_))) { return; }
        }
    });
    let body = futures_util::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|item| (item, rx)) });
    Response::new(Body::from_stream(body))
}

fn parse_body(raw: &Bytes) -> Option<Value> { if raw.is_empty() { None } else { serde_json::from_slice(raw).ok() } }

fn type_error(prop: &str) -> Value { json!({"message": format!("undefined is not an object (evaluating 'body.{prop}')")}) }

fn set_x_app(unified: &mut Value, headers: &HeaderMap) {
    let Some(x) = header_value(headers, "x-app").filter(|v| !v.trim().is_empty()) else { return };
    let Some(map) = unified.as_object_mut() else { return };
    let mut metadata = map.get("metadata").and_then(Value::as_object).cloned().unwrap_or_default();
    let mut plexus = metadata.get("plexus_metadata").and_then(Value::as_object).cloned().unwrap_or_default();
    plexus.insert("clientHeaders".into(), json!({"x-app": x}));
    metadata.insert("plexus_metadata".into(), Value::Object(plexus));
    map.insert("metadata".into(), Value::Object(metadata));
}

/// Common request preamble: auth, body, usage start.
async fn begin(state: &AppState, request: Request, api: Api, usage_api: impl FnOnce(&HeaderMap, Option<&Value>) -> String) -> Result<(Caller, HeaderMap, Option<String>, Option<Value>, Flow), Response> {
    let (parts, body) = request.into_parts();
    let peer = super::ip::peer_ip(&parts.extensions);
    let caller = authenticate_peer(state, &parts.headers, parts.uri.query(), peer)?;
    let raw = axum::body::to_bytes(body, usize::MAX).await.unwrap_or_default();
    let body = parse_body(&raw);
    let usage = usage_api(&parts.headers, body.as_ref());
    let flow = Flow::new(state, &parts.headers, peer, api, &usage, body.as_ref());
    Ok((caller, parts.headers, parts.uri.query().map(str::to_owned), body, flow))
}

/// Chat / messages / completions share one shape.
async fn simple_route(state: AppState, request: Request, api: Api) -> Response {
    let (caller, headers, query, body, mut flow) = match begin(&state, request, api, |_, _| api.name().to_owned()).await { Ok(v) => v, Err(r) => return r };
    let debug_rid = flow.request_id.clone();
    KEY_NAME.scope(Some(caller.key_name.clone()), super::debug::REQUEST_ID.scope(Some(debug_rid), async move {
        let Some(body) = body else { return flow.fail(type_error("model")).await };
        flow.caller_fields(&caller, body.get("model").cloned());
        flow.start_log(Some(&body), &headers, query.as_deref());
        let client: Arc<dyn Transformer> = match api {
            Api::Messages => Arc::new(AnthropicTransformer),
            Api::Completions => Arc::new(OpenAICompletionTransformer::new()),
            _ => Arc::new(OpenAITransformer),
        };
        let mut unified = match client.parse_request(&body) { Ok(u) => u, Err(e) => return flow.fail(crate::engine::dispatch::errors::engine_error_value(&e)).await };
        unified["incomingApiType"] = json!(api.name());
        unified["originalBody"] = body.clone();
        unified["requestId"] = json!(flow.request_id);
        match api {
            Api::Messages => {
                let effort = get_reasoning_log_value(Some(&unified), &body);
                flow.record.set("reasoningEffort", effort.map(Value::String).unwrap_or(Value::Null));
                set_opt_value(&mut unified, "cacheRoutingHeaders", cache_routing_headers(&headers, None));
                set_opt_value(&mut unified, "anthropicBeta", header_value(&headers, "anthropic-beta").map(|v| json!(v)));
                set_opt_value(&mut unified, "userAgent", header_value(&headers, "user-agent").map(|v| json!(v)));
                set_opt_value(&mut unified, "claudeCodeSessionId", header_value(&headers, "x-claude-code-session-id").map(|v| json!(v)));
            }
            _ => {
                set_opt_value(&mut unified, "cacheRoutingHeaders", cache_routing_headers(&headers, body.get("prompt_cache_key")));
                let effort = get_reasoning_log_value(Some(&unified), &body);
                flow.record.set("reasoningEffort", effort.map(Value::String).unwrap_or(Value::Null));
            }
        }
        attach_key_access_policy(&caller.config, &mut unified);
        set_x_app(&mut unified, &headers);
        let quota = match flow.check_quota(&caller, &mut unified).await { Ok(q) => q, Err(r) => return r };
        let body_for_meta = body.clone();
        match flow.run(&caller, unified, client, &body, &headers, quota, move |record, unified| {
            match api {
                Api::Completions => { record.set("toolsDefined", 0); record.set("messageCount", 1); }
                _ => {
                    record.set("toolsDefined", get_arr(unified, &["tools"]).map(|t| t.len()).unwrap_or(0));
                    record.set("messageCount", get_arr(unified, &["messages"]).map(|t| t.len()).unwrap_or(0));
                    record.set("parallelToolCallsEnabled", if api == Api::Messages { Value::Null } else { coalesce(body_for_meta.get("parallel_tool_calls"), None).unwrap_or(Value::Null) });
                }
            }
        }).await {
            Ok((r, _)) | Err(r) => r,
        }
    })).await
}

fn set_opt_value(unified: &mut Value, key: &str, v: Option<Value>) {
    if let Some(m) = unified.as_object_mut() { match v { Some(v) => { m.insert(key.into(), v); } None => { m.shift_remove(key); } } }
}

async fn chat(State(state): State<AppState>, request: Request) -> Response { simple_route(state, request, Api::Chat).await }
async fn messages(State(state): State<AppState>, request: Request) -> Response { simple_route(state, request, Api::Messages).await }
async fn completions(State(state): State<AppState>, request: Request) -> Response { simple_route(state, request, Api::Completions).await }

async fn gemini(State(state): State<AppState>, Path(model_with_action): Path<String>, request: Request) -> Response {
    let (caller, headers, query, body, mut flow) = match begin(&state, request, Api::Gemini, |_, _| "gemini".into()).await { Ok(v) => v, Err(r) => return r };
    let debug_rid = flow.request_id.clone();
    KEY_NAME.scope(Some(caller.key_name.clone()), super::debug::REQUEST_ID.scope(Some(debug_rid), async move {
        let model_name = model_with_action.split(':').next().unwrap_or("").to_owned();
        flow.caller_fields(&caller, Some(json!(model_name)));
        let body = body.unwrap_or(Value::Null);
        flow.start_log(Some(&body), &headers, query.as_deref());
        let client: Arc<dyn Transformer> = Arc::new(GeminiTransformer);
        let input = spread(&body, &json!({"model": model_name}));
        let mut unified = match client.parse_request(&input) { Ok(u) => u, Err(e) => return flow.fail(crate::engine::dispatch::errors::engine_error_value(&e)).await };
        unified["incomingApiType"] = json!("gemini");
        unified["originalBody"] = body.clone();
        unified["requestId"] = json!(flow.request_id);
        let effort = get_reasoning_log_value(Some(&unified), &body);
        flow.record.set("reasoningEffort", effort.map(Value::String).unwrap_or(Value::Null));
        attach_key_access_policy(&caller.config, &mut unified);
        set_x_app(&mut unified, &headers);
        if model_with_action.contains("streamGenerateContent") { unified["stream"] = json!(true); }
        let quota = match flow.check_quota(&caller, &mut unified).await { Ok(q) => q, Err(r) => return r };
        match flow.run(&caller, unified, client, &body, &headers, quota, |record, unified| {
            record.set("toolsDefined", get_arr(unified, &["tools"]).map(|t| t.len()).unwrap_or(0));
            record.set("messageCount", get_arr(unified, &["messages"]).map(|t| t.len()).unwrap_or(0));
            record.set("parallelToolCallsEnabled", Value::Null);
        }).await {
            Ok((r, _)) | Err(r) => r,
        }
    })).await
}

/// `detectResponsesApiType(headers, body)`.
pub fn detect_responses_api_type(headers: &HeaderMap, body: Option<&Value>) -> &'static str {
    if header_value(headers, "x-openai-internal-codex-responses-lite").is_some_and(|h| h.to_lowercase() == "true") { return "responses:lite"; }
    let additional = body.and_then(|b| get_arr(b, &["input"])).is_some_and(|i| i.iter().any(|x| get_str(x, &["type"]) == Some("additional_tools")));
    let tool_search = body.and_then(|b| get_arr(b, &["tools"])).is_some_and(|t| t.iter().any(|x| get_str(x, &["type"]) == Some("tool_search")));
    if additional || tool_search { "responses:lite" } else { "responses" }
}

/// `normalizeInput(input)`.
fn normalize_input(input: Option<&Value>) -> Vec<Value> {
    match input {
        Some(Value::Array(a)) => a.clone(),
        other => vec![json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": to_string(other)}]})],
    }
}

async fn responses(State(state): State<AppState>, request: Request) -> Response {
    let (caller, headers, query, body, mut flow) = match begin(&state, request, Api::Responses, |h, b| detect_responses_api_type(h, b).to_owned()).await { Ok(v) => v, Err(r) => return r };
    let incoming = flow.usage_api.clone();
    let debug_rid = flow.request_id.clone();
    KEY_NAME.scope(Some(caller.key_name.clone()), super::debug::REQUEST_ID.scope(Some(debug_rid), async move {
        let Some(mut body) = body else { return flow.fail(type_error("model")).await };
        flow.caller_fields(&caller, body.get("model").cloned());
        let pool = state.inner.database.clone();
        if let Some(prev) = body.get("previous_response_id").filter(|p| truthy(Some(p))).cloned() {
            let row = match &pool { Some(p) => storage::get_response(p, &to_string(Some(&prev))).await.ok().flatten(), None => None };
            let Some(row) = row else {
                return flow.json(404, &json!({"error": {"message": format!("Previous response not found: {}", to_string(Some(&prev))), "type": "invalid_request_error", "code": "response_not_found", "param": "previous_response_id"}}));
            };
            let mut items = storage::stored_items(&row, "output_items");
            items.extend(normalize_input(body.get("input")));
            body["input"] = Value::Array(items);
        }
        if let Some(conv) = body.get("conversation").filter(|c| truthy(Some(c))).cloned() {
            let id = if let Value::String(s) = &conv { s.clone() } else { to_string(conv.get("id")) };
            let row = match &pool { Some(p) => storage::get_conversation(p, &id).await.ok().flatten(), None => None };
            let Some(row) = row else {
                return flow.json(404, &json!({"error": {"message": format!("Conversation not found: {id}"), "type": "invalid_request_error", "code": "conversation_not_found", "param": "conversation"}}));
            };
            let mut items = storage::stored_items(&row, "items");
            items.extend(normalize_input(body.get("input")));
            body["input"] = Value::Array(items);
        }
        flow.start_log(Some(&body), &headers, query.as_deref());
        normalize_responses_null_entries(&mut body);
        normalize_composite_responses_call_ids(&mut body);
        normalize_responses_function_call_item_ids(&mut body);
        normalize_responses_reasoning_content(&mut body);
        let transformer = ResponsesTransformer::new();
        let client: Arc<dyn Transformer> = Arc::new(transformer.clone());
        let mut unified = match client.parse_request(&body) { Ok(u) => u, Err(e) => return flow.fail(crate::engine::dispatch::errors::engine_error_value(&e)).await };
        unified["incomingApiType"] = json!(incoming);
        unified["originalBody"] = body.clone();
        unified["requestId"] = json!(flow.request_id);
        let effort = get_reasoning_log_value(Some(&unified), &body);
        flow.record.set("reasoningEffort", effort.map(Value::String).unwrap_or(Value::Null));
        if let Some(prev) = body.get("previous_response_id").filter(|p| truthy(Some(p))) { unified["previousResponseId"] = prev.clone(); }
        set_opt_value(&mut unified, "cacheRoutingHeaders", cache_routing_headers(&headers, body.get("prompt_cache_key")));
        attach_key_access_policy(&caller.config, &mut unified);
        set_x_app(&mut unified, &headers);
        let quota = match flow.check_quota(&caller, &mut unified).await { Ok(q) => q, Err(r) => return r };
        let input_items: Vec<Value> = get_arr(&body, &["input"]).cloned().unwrap_or_default();
        let meta_body = body.clone();
        let (r, response) = match flow.run(&caller, unified, client.clone(), &body, &headers, quota, move |record, unified| {
            record.set("toolsDefined", get_arr(&meta_body, &["tools"]).map(|t| t.len()).unwrap_or(0));
            record.set("messageCount", get_arr(unified, &["messages"]).map(|t| t.len()).unwrap_or(0));
            record.set("parallelToolCallsEnabled", coalesce(meta_body.get("parallel_tool_calls"), None).unwrap_or(Value::Null));
        }).await {
            Ok(v) => v,
            Err(r) => return r,
        };
        if body.get("store") != Some(&Value::Bool(false)) && !truthy(body.get("stream")) {
            let mut stripped = response.clone();
            if let Some(m) = stripped.as_object_mut() { m.shift_remove("plexus"); }
            match client.format_response(&stripped) {
                Ok(formatted) => {
                    if let Some(p) = &pool {
                        if storage::store_response(p, &formatted, &body).await.is_err() { return flow.fail(json!({"message": "Error storing response"})).await; }
                        if let Some(conv) = body.get("conversation").filter(|c| truthy(Some(c))) {
                            let id = if let Value::String(s) = conv { s.clone() } else { to_string(conv.get("id")) };
                            let _ = storage::update_conversation(p, &id, formatted.get("output").unwrap_or(&Value::Null), &input_items).await;
                        }
                    }
                }
                Err(e) => return flow.fail(crate::engine::dispatch::errors::engine_error_value(&e)).await,
            }
        }
        r
    })).await
}

fn stored_error(status: u16, message: String, code: &str) -> Response {
    json_response(status, &json!({"error": {"message": message, "type": "invalid_request_error", "code": code}}))
}

fn server_error() -> Response { json_response(500, &json!({"error": {"message": "Internal server error", "type": "server_error"}})) }

async fn get_response(State(state): State<AppState>, Path(id): Path<String>, request: Request) -> Response {
    if let Err(r) = authenticate_peer(&state, request.headers(), request.uri().query(), super::ip::peer_ip(request.extensions())) { return r; }
    let Some(pool) = &state.inner.database else { return server_error() };
    match storage::get_response(pool, &id).await {
        Ok(Some(row)) => json_response(200, &storage::format_stored_response(&row)),
        Ok(None) => stored_error(404, format!("Response not found: {id}"), "response_not_found"),
        Err(_) => server_error(),
    }
}

async fn delete_response(State(state): State<AppState>, Path(id): Path<String>, request: Request) -> Response {
    if let Err(r) = authenticate_peer(&state, request.headers(), request.uri().query(), super::ip::peer_ip(request.extensions())) { return r; }
    let Some(pool) = &state.inner.database else { return server_error() };
    match storage::delete_response(pool, &id).await {
        Ok(true) => json_response(200, &json!({"deleted": true, "id": id})),
        Ok(false) => stored_error(404, format!("Response not found: {id}"), "response_not_found"),
        Err(_) => server_error(),
    }
}

async fn get_conversation(State(state): State<AppState>, Path(id): Path<String>, request: Request) -> Response {
    if let Err(r) = authenticate_peer(&state, request.headers(), request.uri().query(), super::ip::peer_ip(request.extensions())) { return r; }
    let Some(pool) = &state.inner.database else { return server_error() };
    match storage::get_conversation(pool, &id).await {
        Ok(Some(row)) => json_response(200, &storage::format_stored_conversation(&row)),
        Ok(None) => stored_error(404, format!("Conversation not found: {id}"), "conversation_not_found"),
        Err(_) => server_error(),
    }
}
