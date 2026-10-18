//! Plexus media inference routes on the engine port
//! (`routes/inference/{embeddings,images,speech,transcriptions}.ts`):
//! `POST /v1/embeddings`, `/v1/images`, `/v1/images/generations`,
//! `/v1/images/edits`, `/v1/audio/speech`, `/v1/audio/transcriptions`.
//!
//! Authentication is the shared `createAuthHook` port; usage rows and
//! inference errors are written through the engine usage storage.
use crate::engine::dispatcher::cooldown_parsers::{parse_cooldown_duration_for_provider, resolve_cooldown_provider_type};
use crate::engine::dispatcher::engine::{Engine, KEY_NAME};
use crate::engine::dispatcher::routes::{authenticate_peer, Caller};
use crate::engine::js::*;
use crate::engine::media::dispatcher::{self as md, error_details, MediaError, MediaHost, Permit};
use crate::engine::media::image::{format_openrouter_image_response, image_reference_from_buffer, parse_openrouter_generation_request};
use crate::engine::media::transformers::{format_embeddings_response, format_transcription_response, speech_mime_type, TranscriptionRequest};
use crate::engine::observability::costs::calculate_costs;
use crate::engine::observability::{DebugHooks, ErrorInfo, UsageRecord};
use crate::engine::routing::{RouteResult, RoutingRuntime};
use crate::state::{AppState, KeyConfig};
use axum::{
    body::{Body, Bytes},
    extract::{FromRequest, Multipart, Request, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::Response,
    routing::post,
    Router,
};
use serde_json::{json, Value};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/v1/embeddings", post(embeddings))
        .route("/v1/images", post(images_openrouter))
        .route("/v1/images/generations", post(images_generations))
        .route("/v1/images/edits", post(image_edits))
        .route("/v1/audio/speech", post(speech))
        .route("/v1/audio/transcriptions", post(transcriptions))
}

/// The engine services the media dispatcher runs on.
pub fn media_host(state: &AppState) -> Engine { Engine::new(state) }

#[async_trait::async_trait]
impl MediaHost for Engine {
    fn routing(&self) -> &dyn RoutingRuntime { self }
    fn acquire(&self, provider: &str, model: &str) -> Option<Permit> {
        let cfg = self.config.provider(provider).cloned();
        let Some(cfg) = cfg else { return Some(Box::new(())) };
        let limit = |v: Option<&Value>| v.filter(|v| !v.is_null()).and_then(Value::as_f64).map(|n| n.max(0.0) as u64);
        let provider_limit = limit(cfg.get("maxConcurrency"));
        let model_limit = limit(crate::engine::config::model_config(&cfg, model).and_then(|m| m.get("maxConcurrency")));
        crate::concurrency::try_acquire(&self.state, provider, model, provider_limit, model_limit).map(|p| Box::new(p) as Permit)
    }
    async fn mark_provider_failure(&self, provider: &str, model: &str, duration_ms: Option<f64>, reason: String) {
        crate::cooldown_manager::mark_failure(&self.state, provider, model, duration_ms.map(|d| d as i64), Some(reason)).await;
    }
    async fn mark_provider_success(&self, provider: &str, model: &str) { crate::cooldown_manager::mark_success(&self.state, provider, model).await; }
    async fn record_attempt_metric(&self, route: &RouteResult, request_id: &str, success: bool) {
        let Some(usage) = &self.usage else { return };
        let id = if request_id.is_empty() { format!("failover-attempt-{}-{}", now_ms(), random_base36(6)) } else { request_id.to_owned() };
        if success {
            usage.record_successful_attempt(&route.provider, &route.model, route.canonical_model.as_deref(), &id, None).await;
        } else {
            usage.record_failed_attempt(&route.provider, &route.model, route.canonical_model.as_deref(), &id, None).await;
        }
    }
    async fn save_error(&self, request_id: &str, error: &MediaError, details: &Value) {
        self.sink().save_error(request_id, &error_info(error), Some(details), None).await;
    }
    fn emit_routing_update(&self, request_id: &str, route: &RouteResult) {
        self.emit_updated_async(UsageRecord::from_value(obj(vec![
            ("requestId", Some(json!(request_id))),
            ("provider", Some(json!(route.provider))),
            ("selectedModelName", Some(json!(route.model))),
            ("canonicalModelName", route.canonical_model.clone().map(Value::String)),
        ])));
    }
    fn provider_cooldown_ms(&self, route: &RouteResult, error_text: &str) -> Option<f64> {
        parse_cooldown_duration_for_provider(resolve_cooldown_provider_type(route).as_deref(), error_text)
    }
    async fn prepare_codex_images(&self, model: &str, oauth_account: Option<&str>) -> crate::engine::media::dispatcher::MediaResult<(String, Vec<(String, String)>)> {
        crate::engine::dispatcher::oauth::native::prepare_codex_images_dispatch(&self.state, model, oauth_account).await.map_err(|message| json!({"message": message}))
    }
    fn debug_transformed_request(&self, request_id: &str, payload: &Value) { self.debug().add_transformed_request(request_id, payload); }
    fn debug_response_meta(&self, request_id: &str, status: u16, headers: &Value) { self.debug().add_response_meta(request_id, status, headers); }
    fn debug_raw_response(&self, request_id: &str, payload: Value) { self.debug().add_raw_response(request_id, payload); }
    fn debug_reconstructed_raw_response(&self, request_id: &str, payload: Value) { self.debug().add_reconstructed_raw_response(request_id, payload); }
}

pub(crate) fn error_info(error: &MediaError) -> ErrorInfo {
    let message = get_str(error, &["message"]).unwrap_or_default().to_owned();
    let name = get_str(error, &["name"]).unwrap_or("Error");
    ErrorInfo { stack: Some(format!("{name}: {message}")), message }
}

/// `attachKeyAccessPolicy(request, unifiedRequest)`.
pub(crate) fn attach_key_access_policy(config: &KeyConfig, request: &mut Value) {
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

/// `getClientIp(request)`: proxy header chain, then the socket address.
fn client_ip(headers: &HeaderMap, peer: Option<std::net::IpAddr>) -> Option<String> {
    let h = |k: &str| headers.get(k).and_then(|v| v.to_str().ok()).filter(|v| !v.is_empty()).map(str::to_owned);
    h("cf-connecting-ip").or_else(|| h("true-client-ip")).or_else(|| h("x-real-ip"))
        .or_else(|| h("x-forwarded-for").and_then(|v| v.split(',').next().map(|s| s.trim().to_owned())).filter(|s| !s.is_empty()))
        .or_else(|| h("x-client-ip")).or_else(|| h("fastly-client-ip")).or_else(|| h("x-cluster-client-ip"))
        .or_else(|| peer.map(|ip| match ip { std::net::IpAddr::V6(v6) => v6.to_ipv4_mapped().map(|v4| v4.to_string()).unwrap_or_else(|| v6.to_string()), v4 => v4.to_string() }))
}

pub(crate) fn peer_ip(extensions: &axum::http::Extensions) -> Option<std::net::IpAddr> {
    extensions.get::<axum::extract::ConnectInfo<std::net::SocketAddr>>().map(|c| c.0.ip())
}

/// `getClientRequestId(headers)`.
fn client_request_id(headers: &HeaderMap) -> Option<String> {
    let v = headers.get("x-client-request-id").and_then(|v| v.to_str().ok())?.trim().to_owned();
    (!v.is_empty() && v.chars().count() <= 255).then_some(v)
}

/// Per-request bookkeeping shared by the handlers.
pub(crate) struct Ctx {
    pub(crate) engine: Engine,
    pub(crate) request_id: String,
    client_request_id: Option<String>,
    pub(crate) start: i64,
    pub(crate) record: UsageRecord,
}

impl Ctx {
    pub(crate) fn new(state: &AppState, headers: &HeaderMap, peer: Option<std::net::IpAddr>, api_type: &str) -> Self {
        let engine = media_host(state);
        let request_id = uuid();
        let start = now_ms();
        let client_request_id = client_request_id(headers);
        let record = UsageRecord::from_value(json!({
            "requestId": request_id,
            "clientRequestId": client_request_id,
            "date": now_iso(),
            "sourceIp": client_ip(headers, peer),
            "incomingApiType": api_type,
            "startTime": start,
            "isStreamed": false,
            "responseStatus": "pending",
        }));
        engine.emit_started_async(record.clone());
        Self { engine, request_id, client_request_id, start, record }
    }

    pub(crate) fn caller_fields(&mut self, caller: &Caller, model: Option<&Value>) {
        self.record.set_opt("incomingModelAlias", model.cloned());
        self.record.set("apiKey", json!(caller.key_name));
        self.record.set("attribution", caller.attribution.clone().map(Value::String).unwrap_or(Value::Null));
        self.engine.emit_updated_async(UsageRecord::from_value(obj(vec![
            ("requestId", Some(json!(self.request_id))),
            ("incomingModelAlias", model.cloned()),
            ("apiKey", Some(json!(caller.key_name))),
            ("attribution", Some(caller.attribution.clone().map(Value::String).unwrap_or(Value::Null))),
        ])));
    }

    pub(crate) fn routed(&mut self, plexus: Option<&Value>) {
        let p = |k: &str| plexus.and_then(|p| p.get(k)).cloned();
        self.engine.emit_updated_async(UsageRecord::from_value(obj(vec![
            ("requestId", Some(json!(self.request_id))),
            ("provider", p("provider")),
            ("selectedModelName", p("model")),
            ("canonicalModelName", p("canonicalModel")),
        ])));
        self.record.set_opt("provider", p("provider"));
        self.record.set_opt("selectedModelName", p("model"));
        self.record.set_opt("canonicalModelName", p("canonicalModel"));
        self.record.set_opt("outgoingApiType", p("apiType"));
    }

    pub(crate) fn finish_success(&mut self, plexus: Option<&Value>) {
        self.record.set_num("durationMs", (now_ms() - self.start) as f64);
        self.record.set("responseStatus", json!("success"));
        let p = |k: &str| plexus.and_then(|p| p.get(k)).cloned();
        calculate_costs(&mut self.record, p("pricing").as_ref(), p("providerDiscount").as_ref(), None, None);
    }

    pub(crate) fn attempt_metadata(&mut self, plexus: Option<&Value>) {
        let p = |k: &str| plexus.and_then(|p| p.get(k)).cloned();
        self.record.set("attemptCount", if truthy(p("attemptCount").as_ref()) { p("attemptCount").unwrap() } else { json!(1) });
        self.record.set("retryHistory", if truthy(p("retryHistory").as_ref()) { p("retryHistory").unwrap() } else { Value::Null });
    }

    pub(crate) async fn save(&self) { self.engine.sink().save_request(&self.record).await; }

    /// `DebugManager.startLog(requestId, body, sanitizeHeaders(request.headers))`.
    pub(crate) fn start_log(&self, body: &Value, headers: &HeaderMap, query: Option<&str>) {
        self.engine.debug().start_log(&self.request_id, Some(body), &auth_normalized_headers(headers, query));
    }

    /// `DebugManager.addTransformedResponse(requestId, body)` + `flush(requestId)`.
    pub(crate) fn finish_log(&self, body: &Value) {
        let debug = self.engine.debug();
        DebugHooks::add_transformed_response(&debug, &self.request_id, body);
        DebugHooks::flush(&debug, &self.request_id);
    }

    /// The shared `catch` block: error row + `{error: {message, type}}`.
    pub(crate) async fn fail(&mut self, error: MediaError, api_type: &str, image_types: bool) -> Response {
        let (api_type, label, module) = match api_type {
            "embeddings" => ("embeddings", "Error processing embeddings request", "embeddings"),
            "speech" => ("speech", "Error processing speech request", "speech"),
            "transcriptions" => ("transcriptions", "Error processing transcription request", "transcriptions"),
            "image-edit" => ("images", "Error processing image edit request", "images"),
            "decisions" => ("decisions", "Error processing decisions request", "decisions"),
            other => (other, "Error processing image generation request", "images"),
        };
        let rc = |k: &str| get(&error, &["routingContext", k]).cloned();
        self.record.set("responseStatus", json!("error"));
        self.record.set_num("durationMs", (now_ms() - self.start) as f64);
        let attempt = if truthy(rc("attemptCount").as_ref()) { rc("attemptCount") } else if truthy(self.record.get("attemptCount")) { self.record.get("attemptCount").cloned() } else { Some(json!(1)) };
        self.record.set_opt("attemptCount", attempt);
        let history = if truthy(rc("retryHistory").as_ref()) { rc("retryHistory").unwrap() } else if truthy(self.record.get("retryHistory")) { self.record.get("retryHistory").cloned().unwrap() } else { Value::Null };
        self.record.set("retryHistory", history);
        self.save().await;
        self.engine.sink().save_error(&self.request_id, &error_info(&error), Some(&error_details(api_type, &error)), None).await;
        DebugHooks::flush(&self.engine.debug(), &self.request_id);
        crate::system_logs::route_error(&self.engine.state, label, module, &error);
        let status = rc("statusCode").filter(|s| truthy(Some(s))).and_then(|s| s.as_f64()).map(|s| s as u16).unwrap_or(500);
        let kind = if image_types {
            match rc("code").filter(|c| truthy(Some(c))) { Some(c) => to_string(Some(&c)), None => if status == 400 { "invalid_request_error".into() } else { "api_error".into() } }
        } else {
            "api_error".into()
        };
        let message = error.get("message").cloned().unwrap_or(Value::Null);
        self.json(status, &json!({"error": {"message": message, "type": kind}}))
    }

    pub(crate) fn headers(&self, mut response: Response) -> Response {
        let h = response.headers_mut();
        if let Ok(v) = HeaderValue::from_str(&self.request_id) { h.insert("x-request-id", v); }
        if let Some(c) = self.client_request_id.as_deref().and_then(|c| HeaderValue::from_str(c).ok()) { h.insert("x-client-request-id", c); }
        response
    }

    pub(crate) fn json(&self, status: u16, body: &Value) -> Response {
        self.raw(status, "application/json; charset=utf-8", stringify(body).into_bytes())
    }

    fn raw(&self, status: u16, content_type: &str, body: Vec<u8>) -> Response {
        let mut r = Response::new(Body::from(body));
        *r.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        if let Ok(v) = HeaderValue::from_str(content_type) { r.headers_mut().insert(header::CONTENT_TYPE, v); }
        self.headers(r)
    }
}

/// Fastify's parsed JSON body (`undefined` when empty).
fn json_body(raw: &Bytes) -> Option<Value> {
    if raw.is_empty() { None } else { serde_json::from_slice(raw).ok() }
}

/// Bun's TypeError for `undefined.<prop>`.
fn undefined_property_error(prop: &str) -> MediaError {
    json!({"message": format!("undefined is not an object (evaluating 'body.{prop}')"), "name": "TypeError"})
}

async fn embeddings(State(state): State<AppState>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let caller = match authenticate_peer(&state, &parts.headers, parts.uri.query(), peer_ip(&parts.extensions)) { Ok(c) => c, Err(r) => return r };
    let raw = axum::body::to_bytes(body, usize::MAX).await.unwrap_or_default();
    let mut ctx = Ctx::new(&state, &parts.headers, peer_ip(&parts.extensions), "embeddings");
    KEY_NAME.scope(Some(caller.key_name.clone()), async move {
        let Some(body) = json_body(&raw) else { return ctx.fail(undefined_property_error("model"), "embeddings", false).await };
        ctx.caller_fields(&caller, body.get("model"));
        let mut unified = obj(vec![
            ("model", body.get("model").cloned()),
            ("input", body.get("input").cloned()),
            ("encoding_format", body.get("encoding_format").cloned()),
            ("dimensions", body.get("dimensions").cloned()),
            ("user", body.get("user").cloned()),
            ("incomingApiType", Some(json!("embeddings"))),
            ("originalBody", Some(body.clone())),
            ("requestId", Some(json!(ctx.request_id))),
        ]);
        attach_key_access_policy(&caller.config, &mut unified);
        ctx.start_log(&body, &parts.headers, parts.uri.query());
        match md::dispatch_embeddings(&ctx.engine, &unified).await {
            Ok(response) => {
                let plexus = response.get("plexus").cloned();
                ctx.routed(plexus.as_ref());
                let p = |k: &str| plexus.as_ref().and_then(|p| p.get(k)).cloned();
                ctx.record.set("attemptCount", coalesce(p("attemptCount").as_ref(), None).unwrap_or(json!(1)));
                ctx.record.set("retryHistory", coalesce(p("retryHistory").as_ref(), None).unwrap_or(Value::Null));
                ctx.record.set("isPassthrough", json!(true));
                ctx.record.set("tokensInput", coalesce(get(&response, &["usage", "prompt_tokens"]), None).unwrap_or(json!(0)));
                ctx.record.set("tokensOutput", json!(0));
                ctx.finish_success(plexus.as_ref());
                ctx.save().await;
                let formatted = format_embeddings_response(&response);
                ctx.finish_log(&formatted);
                ctx.json(200, &formatted)
            }
            Err(e) => ctx.fail(e, "embeddings", false).await,
        }
    })
    .await
}

const VALID_RESPONSE_FORMATS: [&str; 3] = ["json", "text", "verbose_json"];

async fn speech(State(state): State<AppState>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let caller = match authenticate_peer(&state, &parts.headers, parts.uri.query(), peer_ip(&parts.extensions)) { Ok(c) => c, Err(r) => return r };
    let raw = axum::body::to_bytes(body, usize::MAX).await.unwrap_or_default();
    let mut ctx = Ctx::new(&state, &parts.headers, peer_ip(&parts.extensions), "speech");
    KEY_NAME.scope(Some(caller.key_name.clone()), async move {
        let Some(body) = json_body(&raw) else { return ctx.fail(undefined_property_error("model"), "speech", false).await };
        ctx.caller_fields(&caller, body.get("model"));
        let mut unified = obj(vec![
            ("model", body.get("model").cloned()),
            ("input", body.get("input").cloned()),
            ("voice", body.get("voice").cloned()),
            ("instructions", body.get("instructions").cloned()),
            ("response_format", body.get("response_format").cloned()),
            ("speed", body.get("speed").cloned()),
            ("stream_format", body.get("stream_format").cloned()),
            ("requestId", Some(json!(ctx.request_id))),
            ("incomingApiType", Some(json!("speech"))),
            ("originalBody", Some(body.clone())),
        ]);
        attach_key_access_policy(&caller.config, &mut unified);
        let mut debug_body = body.as_object().cloned().unwrap_or_default();
        debug_body.insert("inputLength".into(), json!(js_length(body.get("input"))));
        ctx.start_log(&Value::Object(debug_body), &parts.headers, parts.uri.query());
        match md::dispatch_speech(&ctx.engine, &unified).await {
            Ok(response) => {
                ctx.routed(Some(&response.plexus));
                ctx.record.set("isPassthrough", json!(true));
                ctx.record.set("isStreamed", json!(response.is_streamed));
                ctx.finish_success(Some(&response.plexus));
                ctx.save().await;
                ctx.finish_log(&json!({"size": if response.is_streamed { 0 } else { response.audio.len() }, "isStreamed": response.is_streamed}));
                ctx.raw(200, speech_mime_type(unified.get("response_format")), response.audio)
            }
            Err(e) => ctx.fail(e, "speech", false).await,
        }
    })
    .await
}

/// One `@fastify/multipart` part (`attachFieldsToBody`).
struct Part { name: String, filename: Option<String>, mime: Option<String>, data: Vec<u8> }

impl Part {
    fn is_file(&self) -> bool { self.filename.is_some() }
    fn value(&self) -> String { String::from_utf8_lossy(&self.data).into_owned() }
}

fn is_multipart(headers: &HeaderMap) -> bool {
    headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|c| c.to_ascii_lowercase().starts_with("multipart/"))
}

/// Drains the multipart body.
async fn read_parts(state: &AppState, request: Request) -> Result<Vec<Part>, MediaError> {
    let mut multipart = Multipart::from_request(request, state).await.map_err(|e| json!({"message": e.body_text()}))?;
    let mut parts = Vec::new();
    loop {
        match multipart.next_field().await {
            Ok(Some(field)) => {
                let name = field.name().unwrap_or_default().to_owned();
                let filename = field.file_name().map(str::to_owned);
                // busboy reports `text/plain` for a part without Content-Type.
                let mime = field.content_type().map(str::to_owned).or_else(|| filename.as_ref().map(|_| "text/plain".to_owned()));
                let data = field.bytes().await.map_err(|e| json!({"message": e.body_text()}))?.to_vec();
                parts.push(Part { name, filename, mime, data });
            }
            Ok(None) => break,
            Err(e) => return Err(json!({"message": e.body_text()})),
        }
    }
    Ok(parts)
}

fn to_buffer_error() -> MediaError {
    json!({"message": "fileData.toBuffer is not a function. (In 'fileData.toBuffer()', 'fileData.toBuffer' is undefined)", "name": "TypeError"})
}

fn bad_request(ctx: &Ctx, message: &str, kind: &str) -> Response {
    ctx.json(400, &json!({"error": {"message": message, "type": kind}}))
}

async fn transcriptions(State(state): State<AppState>, request: Request) -> Response {
    let caller = match authenticate_peer(&state, request.headers(), request.uri().query(), peer_ip(request.extensions())) { Ok(c) => c, Err(r) => return r };
    let headers = request.headers().clone();
    let query = request.uri().query().map(str::to_owned);
    let mut ctx = Ctx::new(&state, &headers, peer_ip(request.extensions()), "transcriptions");
    let multipart = is_multipart(&headers);
    KEY_NAME.scope(Some(caller.key_name.clone()), async move {
        let parts = if multipart {
            match read_parts(&state, request).await { Ok(p) => p, Err(e) => return ctx.fail(e, "transcriptions", false).await }
        } else {
            let raw = axum::body::to_bytes(request.into_body(), usize::MAX).await.unwrap_or_default();
            match json_body(&raw) {
                None => return ctx.fail(undefined_property_error("file"), "transcriptions", false).await,
                Some(b) if truthy(b.get("file")) => return ctx.fail(to_buffer_error(), "transcriptions", false).await,
                Some(_) => Vec::new(),
            }
        };
        let named: Vec<&Part> = parts.iter().filter(|p| p.name == "file").collect();
        let Some(file) = named.first() else { return bad_request(&ctx, "No file uploaded", "invalid_request_error") };
        if named.len() > 1 || !file.is_file() {
            return ctx.fail(to_buffer_error(), "transcriptions", false).await;
        }
        if file.data.len() > 25 * 1024 * 1024 { return bad_request(&ctx, "File size exceeds 25MB limit", "invalid_request_error"); }
        // `body.<field>?.value`: only a single non-file part has a value.
        let field = |name: &str| {
            let matching: Vec<&Part> = parts.iter().filter(|p| p.name == name).collect();
            match matching.as_slice() { [p] if !p.is_file() => Some(p.value()), _ => None }
        };
        let Some(model) = field("model").filter(|m| !m.is_empty()) else {
            return bad_request(&ctx, "Missing required parameter: model", "invalid_request_error");
        };
        let response_format = field("response_format").filter(|f| !f.is_empty()).unwrap_or_else(|| "json".into());
        let temperature = field("temperature").filter(|t| !t.is_empty()).map(|t| crate::engine::observability::record::parse_float_str(&t));
        if !VALID_RESPONSE_FORMATS.contains(&response_format.as_str()) {
            return bad_request(&ctx, &format!("Unsupported response_format: {response_format}. Supported formats: json, text, verbose_json"), "invalid_request_error");
        }
        let mut policy_view = json!({});
        attach_key_access_policy(&caller.config, &mut policy_view);
        let unified = TranscriptionRequest {
            file: file.data.clone(),
            filename: file.filename.clone(),
            mime_type: file.mime.clone(),
            model: model.clone(),
            language: field("language"),
            prompt: field("prompt"),
            response_format: response_format.clone(),
            temperature,
            request_id: ctx.request_id.clone(),
            metadata: policy_view.get("metadata").cloned(),
        };
        ctx.caller_fields(&caller, Some(&json!(model)));
        ctx.start_log(&obj(vec![
            ("model", Some(json!(model))),
            ("filename", file.filename.clone().map(Value::String)),
            ("fileSize", Some(json!(file.data.len()))),
            ("mimeType", file.mime.clone().map(Value::String)),
            ("language", unified.language.clone().map(Value::String)),
            ("prompt", unified.prompt.clone().map(Value::String)),
            ("response_format", Some(json!(response_format))),
            ("temperature", temperature.map(num)),
        ]), &headers, query.as_deref());
        match md::dispatch_transcription(&ctx.engine, &unified).await {
            Ok(response) => {
                let plexus = response.get("plexus").cloned();
                ctx.routed(plexus.as_ref());
                ctx.record.set("isPassthrough", json!(true));
                let tok = |k: &str| { let v = get(&response, &["usage", k]); if truthy(v) { v.cloned().unwrap() } else { json!(0) } };
                ctx.record.set("tokensInput", tok("input_tokens"));
                ctx.record.set("tokensOutput", tok("output_tokens"));
                ctx.finish_success(plexus.as_ref());
                ctx.attempt_metadata(plexus.as_ref());
                ctx.save().await;
                let formatted = format_transcription_response(&response, &response_format);
                ctx.finish_log(&formatted);
                if response_format == "text" {
                    ctx.raw(200, "text/plain", to_string(Some(&formatted)).into_bytes())
                } else {
                    ctx.json(200, &formatted)
                }
            }
            Err(e) => ctx.fail(e, "transcriptions", false).await,
        }
    })
    .await
}

async fn images_openrouter(state: State<AppState>, request: Request) -> Response { images(state, request, true).await }
async fn images_generations(state: State<AppState>, request: Request) -> Response { images(state, request, false).await }

async fn images(State(state): State<AppState>, request: Request, openrouter: bool) -> Response {
    let (parts, body) = request.into_parts();
    let caller = match authenticate_peer(&state, &parts.headers, parts.uri.query(), peer_ip(&parts.extensions)) { Ok(c) => c, Err(r) => return r };
    let raw = axum::body::to_bytes(body, usize::MAX).await.unwrap_or_default();
    let mut ctx = Ctx::new(&state, &parts.headers, peer_ip(&parts.extensions), "images");
    KEY_NAME.scope(Some(caller.key_name.clone()), async move {
        let body = json_body(&raw).filter(|b| !b.is_null()).unwrap_or_else(|| json!({}));
        ctx.caller_fields(&caller, body.get("model"));
        ctx.start_log(&body, &parts.headers, parts.uri.query());
        let unified = if openrouter {
            let mut input = body.as_object().cloned().unwrap_or_default();
            input.insert("requestId".into(), json!(ctx.request_id));
            input.insert("incomingApiType".into(), json!("images"));
            input.insert("originalBody".into(), body.clone());
            parse_openrouter_generation_request(&Value::Object(input))
        } else {
            Ok(obj(vec![
                ("model", body.get("model").cloned()),
                ("prompt", body.get("prompt").cloned()),
                ("n", body.get("n").cloned()),
                ("size", body.get("size").cloned()),
                ("response_format", body.get("response_format").cloned()),
                ("quality", body.get("quality").cloned()),
                ("style", body.get("style").cloned()),
                ("user", body.get("user").cloned()),
                ("requestId", Some(json!(ctx.request_id))),
                ("incomingApiType", Some(json!("images"))),
                ("originalBody", Some(body.clone())),
            ]))
        };
        let mut unified = match unified { Ok(u) => u, Err(e) => return ctx.fail(e, "images", true).await };
        attach_key_access_policy(&caller.config, &mut unified);
        let response = match md::dispatch_image_generations(&ctx.engine, &unified, None).await { Ok(r) => r, Err(e) => return ctx.fail(e, "images", true).await };
        let client = if openrouter {
            match format_openrouter_image_response(&response).await { Ok(c) => c, Err(e) => return ctx.fail(e, "images", true).await }
        } else {
            let mut c = response.clone();
            if let Some(m) = c.as_object_mut() { m.shift_remove("plexus"); }
            c
        };
        let plexus = response.get("plexus").cloned();
        ctx.routed(plexus.as_ref());
        ctx.record.set("isPassthrough", json!(!openrouter));
        let image_count = get_arr(&response, &["data"]).map(Vec::len).unwrap_or(0);
        let usage = |a: &str, b: &str| coalesce(get(&response, &["usage", a]), get(&response, &["usage", b])).unwrap_or(Value::Null);
        ctx.record.set("tokensInput", usage("input_tokens", "prompt_tokens"));
        ctx.record.set("tokensOutput", usage("output_tokens", "completion_tokens"));
        ctx.record.set("providerReportedCost", coalesce(get(&response, &["usage", "cost"]), None).unwrap_or(Value::Null));
        ctx.finish_success(plexus.as_ref());
        ctx.attempt_metadata(plexus.as_ref());
        ctx.save().await;
        ctx.finish_log(&obj(vec![("created", response.get("created").cloned()), ("imageCount", Some(json!(image_count)))]));
        ctx.json(200, &client)
    })
    .await
}

async fn image_edits(State(state): State<AppState>, request: Request) -> Response {
    let caller = match authenticate_peer(&state, request.headers(), request.uri().query(), peer_ip(request.extensions())) { Ok(c) => c, Err(r) => return r };
    let headers = request.headers().clone();
    let query = request.uri().query().map(str::to_owned);
    let mut ctx = Ctx::new(&state, &headers, peer_ip(request.extensions()), "images");
    let multipart = is_multipart(&headers);
    KEY_NAME.scope(Some(caller.key_name.clone()), async move {
        if !multipart { return ctx.fail(json!({"message": "the request is not multipart"}), "image-edit", true).await; }
        let parts = match read_parts(&state, request).await { Ok(p) => p, Err(e) => return ctx.fail(e, "image-edit", true).await };
        // `extractImageEditUpload`: first entry per name; files feed image/mask.
        let mut image: Option<&Part> = None;
        let mut mask: Option<&Part> = None;
        let mut fields = Obj::new();
        let mut seen = std::collections::HashSet::new();
        for part in &parts {
            if !seen.insert(part.name.clone()) { continue; }
            if part.is_file() {
                match part.name.as_str() { "image" => image = Some(part), "mask" => mask = Some(part), _ => {} }
            } else {
                fields.insert(part.name.clone(), json!(part.value()));
            }
        }
        let Some(image) = image else { return bad_request(&ctx, "Missing required field: image", "validation_error") };
        if !truthy(fields.get("prompt")) { return bad_request(&ctx, "Missing required field: prompt", "validation_error") }
        let form = Value::Object(fields.clone());
        ctx.caller_fields(&caller, fields.get("model"));
        let n = fields.get("n").filter(|n| truthy(Some(n))).and_then(|n| crate::engine::observability::record::parse_int(&to_string(Some(n)))).map(|n| json!(n));
        let mut unified = Obj::new();
        unified.insert("model".into(), fields.get("model").cloned().unwrap_or(Value::Null));
        unified.insert("prompt".into(), fields["prompt"].clone());
        unified.insert("input_references".into(), json!([image_reference_from_buffer(&image.data, image.mime.as_deref())]));
        if let Some(m) = mask { unified.insert("mask".into(), image_reference_from_buffer(&m.data, m.mime.as_deref())); }
        set_opt(&mut unified, "n", n);
        for k in ["size", "response_format", "quality", "user"] { set_opt(&mut unified, k, fields.get(k).cloned()); }
        unified.insert("requestId".into(), json!(ctx.request_id));
        unified.insert("incomingApiType".into(), json!("images"));
        unified.insert("originalBody".into(), form);
        if !fields.contains_key("model") { unified.shift_remove("model"); }
        let mut unified = Value::Object(unified);
        attach_key_access_policy(&caller.config, &mut unified);
        let prompt = to_string(fields.get("prompt"));
        ctx.start_log(&obj(vec![
            ("model", fields.get("model").cloned()),
            ("prompt", Some(json!(js_substring(&prompt, 100)))),
            ("filename", image.filename.clone().map(Value::String)),
            ("hasMask", Some(json!(mask.is_some()))),
            ("n", fields.get("n").cloned()),
            ("size", fields.get("size").cloned()),
        ]), &headers, query.as_deref());
        let mut response = match md::dispatch_image_generations(&ctx.engine, &unified, None).await { Ok(r) => r, Err(e) => return ctx.fail(e, "image-edit", true).await };
        let plexus = response.get("plexus").cloned();
        ctx.routed(plexus.as_ref());
        ctx.record.set("isPassthrough", json!(false));
        ctx.finish_success(plexus.as_ref());
        ctx.attempt_metadata(plexus.as_ref());
        ctx.save().await;
        let image_count = get_arr(&response, &["data"]).map(Vec::len).unwrap_or(0);
        ctx.finish_log(&obj(vec![("created", response.get("created").cloned()), ("imageCount", Some(json!(image_count)))]));
        if let Some(m) = response.as_object_mut() { m.shift_remove("plexus"); }
        ctx.json(200, &response)
    })
    .await
}

/// `value?.length || 0` (strings count UTF-16 units, like JS).
fn js_length(value: Option<&Value>) -> usize {
    match value {
        Some(Value::String(s)) => s.encode_utf16().count(),
        Some(Value::Array(a)) => a.len(),
        _ => 0,
    }
}

/// `s.substring(0, n)` in UTF-16 units.
fn js_substring(s: &str, n: usize) -> String {
    let units: Vec<u16> = s.encode_utf16().take(n).collect();
    String::from_utf16_lossy(&units)
}

/// Request headers as Fastify holds them after `createAuthHook().onRequest`
/// normalized `authorization` (Bearer prefix, or copied from
/// `x-api-key` / `x-goog-api-key` / `?key=`).
pub(crate) fn auth_normalized_headers(headers: &HeaderMap, query: Option<&str>) -> Vec<(String, String)> {
    let mut list: Vec<(String, String)> = headers.iter().filter_map(|(k, v)| v.to_str().ok().map(|v| (k.as_str().to_owned(), v.to_owned()))).collect();
    let header = |k: &str| list.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    let value = match header("authorization").filter(|a| !a.is_empty()) {
        Some(a) if a.to_lowercase().starts_with("bearer ") => None,
        Some(a) => Some(format!("Bearer {a}")),
        None => header("x-api-key").or_else(|| header("x-goog-api-key")).or_else(|| crate::engine::dispatcher::routes::query_key(query)).map(|k| format!("Bearer {k}")),
    };
    if let Some(v) = value {
        match list.iter_mut().find(|(n, _)| n == "authorization") { Some(entry) => entry.1 = v, None => list.push(("authorization".into(), v)) }
    }
    list
}
