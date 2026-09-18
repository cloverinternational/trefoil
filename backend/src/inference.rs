//! Initial OpenAI-compatible inference surface.
//!
//! Model discovery plus chat, legacy completions, and embeddings pass-through
//! for HTTP(S) providers. Shared authentication, request quotas, key access
//! policy, cooldown admission, request IDs, and PostgreSQL usage accounting are
//! enforced. Provider-specific transforms, full failover, pricing, and stream
//! terminal inspection remain intentionally deferred parity slices.

use axum::{
    body::{Body, Bytes},
    extract::{Multipart, Path, Query, State},
    http::HeaderMap,
    response::{IntoResponse, Response},
    Json, Router,
};
use futures_util::{Stream, StreamExt};
use serde_json::Value;
use sqlx::Row;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::{
    auth::{resolve_principal, Principal},
    AppState,
};

/// State carried by a provider SSE stream until its terminal event (or an
/// upstream/client disconnect).  Keeping this state in the stream, rather
/// than in the request handler, makes accounting request-local and ensures a
/// row is finalized exactly once when the body is dropped or exhausted.
struct StreamAccounting {
    pool: Option<sqlx::PgPool>,
    event_sender: Option<tokio::sync::broadcast::Sender<crate::usage::UsageEvent>>,
    request_id: String,
    key_name: Option<String>,
    provider: String,
    model: String,
    api_type: String,
    start_time: i64,
    input_tokens: Option<i32>,
    output_tokens: Option<i32>,
    cost: Option<f32>,
    /// Gemini's final chunk carries usageMetadata and has no [DONE] marker.
    terminal_on_usage: bool,
    stream_success: bool,
    finalized: bool,
}

impl StreamAccounting {
    async fn finalize(&mut self, outcome: &str) {
        if self.finalized {
            return;
        }
        self.finalized = true;
        if let Some(sender) = &self.event_sender {
            let id = now_ms();
            let _ = sender.send(crate::usage::UsageEvent {
                kind: "completed".into(), id, api_key: self.key_name.clone(),
                data: serde_json::json!({"requestId": self.request_id, "apiKey": self.key_name, "provider": self.provider, "responseStatus": outcome, "durationMs": id.saturating_sub(self.start_time)}),
            });
        }
        let Some(pool) = &self.pool else { return };
        let _ = crate::database::record_request_usage(
            pool,
            &self.request_id,
            self.key_name.as_deref(),
            Some(&self.provider),
            Some(&self.model),
            &self.api_type,
            outcome,
            true,
            self.start_time,
        )
        .await;
        let _ = crate::database::update_request_usage_metrics(
            pool,
            &self.request_id,
            self.input_tokens,
            self.output_tokens,
            self.cost,
            now_ms().saturating_sub(self.start_time),
        )
        .await;
    }

    fn observe(&mut self, data: &str) -> Option<bool> {
        if data.trim() == "[DONE]" {
            return Some(true);
        }
        let Ok(value) = serde_json::from_str::<Value>(data) else {
            return None;
        };
        let gemini_usage = value.get("usageMetadata");
        let usage = value
            .get("usage")
            .or(gemini_usage)
            .or_else(|| value.get("response").and_then(|v| v.get("usage")));
        if let Some(usage) = usage {
            self.input_tokens = usage
                .get("prompt_tokens")
                .or_else(|| usage.get("input_tokens"))
                .or_else(|| usage.get("promptTokenCount"))
                .and_then(Value::as_i64)
                .map(|v| v as i32)
                .or(self.input_tokens);
            self.output_tokens = usage
                .get("completion_tokens")
                .or_else(|| usage.get("output_tokens"))
                .or_else(|| usage.get("candidatesTokenCount"))
                .and_then(Value::as_i64)
                .map(|v| v as i32)
                .or(self.output_tokens);
            self.cost = usage
                .get("total_cost")
                .or_else(|| usage.get("cost"))
                .and_then(Value::as_f64)
                .map(|v| v as f32)
                .or(self.cost);
        }
        if self.terminal_on_usage && gemini_usage.is_some() {
            return Some(self.stream_success);
        }
        let event = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if event.ends_with(".completed") || event == "message_stop" || event == "completion" {
            Some(true)
        } else if event.ends_with(".failed") || event.ends_with(".incomplete") {
            Some(false)
        } else {
            None
        }
    }
}

struct AccountingGuard {
    state: Arc<tokio::sync::Mutex<StreamAccounting>>,
    finished: Arc<AtomicBool>,
}

impl Drop for AccountingGuard {
    fn drop(&mut self) {
        if self.finished.swap(true, Ordering::AcqRel) {
            return;
        }
        let state = self.state.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                state.lock().await.finalize("cancelled").await;
            });
        }
    }
}

fn accounted_sse_body<S>(stream: S, accounting: StreamAccounting) -> Body
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
{
    let state = Arc::new(tokio::sync::Mutex::new(accounting));
    let guard = Arc::new(AccountingGuard {
        state: state.clone(),
        finished: Arc::new(AtomicBool::new(false)),
    });
    let stream_guard = guard.clone();
    let output = stream.scan(String::new(), move |buffer, item| {
        let state = state.clone();
        let guard = stream_guard.clone();
        let (bytes, mut terminal, error) = match item {
            Ok(bytes) => {
                buffer.push_str(&String::from_utf8_lossy(&bytes));
                let mut events = Vec::new();
                while let Some(index) = buffer.find('\n') {
                    let line = buffer[..index].trim_end_matches('\r').to_owned();
                    buffer.drain(..=index);
                    if let Some(data) = line.strip_prefix("data:") {
                        events.push(data.trim().to_owned());
                    }
                }
                (Some((bytes, events)), None, None)
            }
            Err(error) => (None, None, Some(error)),
        };
        Box::pin(async move {
            if let Some((bytes, events)) = bytes {
                let mut accounting = state.lock().await;
                for event in events {
                    if let Some(success) = accounting.observe(&event) {
                        terminal = Some(success);
                    }
                }
                if let Some(success) = terminal {
                    guard.finished.store(true, Ordering::Release);
                    accounting
                        .finalize(if success { "success" } else { "error" })
                        .await;
                }
                Some(Ok(bytes))
            } else {
                guard.finished.store(true, Ordering::Release);
                state.lock().await.finalize("error").await;
                Some(Err(error.expect("stream error")))
            }
        })
    });
    Body::from_stream(output.map(move |item| {
        let _keep_guard = &guard;
        item
    }))
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/v1/models", axum::routing::get(list_models))
        .route(
            "/v1/chat/completions",
            axum::routing::post(chat_completions),
        )
        .route("/v1/embeddings", axum::routing::post(embeddings))
        .route("/v1/completions", axum::routing::post(completions))
        .route("/completions", axum::routing::post(completions))
        .route("/v1beta/models/:model_action", axum::routing::post(gemini))
        .route("/v1/metadata/search", axum::routing::get(metadata_search))
        .route("/v1/metadata/lookup", axum::routing::get(metadata_lookup))
        .route(
            "/v1/openrouter/models",
            axum::routing::get(openrouter_models),
        )
        .route("/v1/muse-code/models", axum::routing::get(muse_models))
        .route(
            "/v1/responses/:response_id",
            axum::routing::get(get_response).delete(delete_response),
        )
        .route(
            "/v1/conversations/:conversation_id",
            axum::routing::get(get_conversation),
        )
        .route("/v1/responses", axum::routing::post(responses))
        .route("/v1/codex/responses", axum::routing::post(responses))
        .route("/v1/messages", axum::routing::post(messages))
        .route("/v1/images/generations", axum::routing::post(images))
        .route("/v1/images", axum::routing::post(images))
        .route("/v1/images/edits", axum::routing::post(image_edits))
        .route("/v1/audio/speech", axum::routing::post(audio_speech))
        .route(
            "/v1/audio/transcriptions",
            axum::routing::post(audio_transcriptions),
        )
}

async fn audio_transcriptions(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Response {
    let principal = match authenticate(&state, &headers) {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    let key_config = match &principal {
        Principal::Limited { config, .. } => Some(config),
        Principal::Admin => None,
    };
    if let Err(response) = enforce_request_quotas(&state, &principal).await {
        return response;
    }
    let mut fields = Vec::new();
    while let Ok(Some(field)) = multipart.next_field().await {
        let Some(name) = field.name().map(str::to_owned) else {
            continue;
        };
        let file_name = field.file_name().map(str::to_owned);
        let content_type = field.content_type().map(str::to_owned);
        let Ok(bytes) = field.bytes().await else {
            return error_response(
                axum::http::StatusCode::BAD_REQUEST,
                "Invalid multipart field",
                "invalid_request_error",
            );
        };
        fields.push((name, file_name, content_type, bytes.to_vec()));
    }
    let Some((_, _, _, model_bytes)) = fields.iter().find(|(name, _, _, _)| name == "model") else {
        return error_response(
            axum::http::StatusCode::BAD_REQUEST,
            "model is required",
            "invalid_request_error",
        );
    };
    let model = String::from_utf8_lossy(model_bytes).into_owned();
    if key_config.is_some_and(|config| !model_allowed(config, &model)) {
        return error_response(
            axum::http::StatusCode::FORBIDDEN,
            &format!("Key is not allowed to access model '{model}'"),
            "access_denied",
        );
    }
    let client = reqwest::Client::new();
    let request_id = new_request_id();
    let start_time = now_ms();
    let mut attempted = None;
    for (attempt, (provider_slug, upstream_model)) in
        inference_targets(&state, &model, key_config).enumerate()
    {
        let attempt_index = (attempt + 1) as i32;
        if key_config.is_some_and(|config| !provider_allowed(config, &provider_slug)) {
            return error_response(
                axum::http::StatusCode::FORBIDDEN,
                "Key is not allowed to access any provider configured for model",
                "access_denied",
            );
        }
        let Some(provider) = state
            .inner
            .providers
            .read()
            .unwrap()
            .get(&provider_slug)
            .cloned()
        else {
            return error_response(
                axum::http::StatusCode::NOT_FOUND,
                "No provider configured for model",
                "model_not_found",
            );
        };
        let Ok(base) = dispatch_provider_url(&provider.api_base_url) else {
            record_attempt(
                &state,
                &request_id,
                attempt_index,
                &provider_slug,
                &upstream_model,
                "audio_transcriptions",
                "failed",
                Some("Provider API base URL is invalid"),
                None,
                true,
                start_time,
            )
            .await;
            return error_response(
                axum::http::StatusCode::BAD_GATEWAY,
                "Provider API base URL is invalid",
                "upstream_error",
            );
        };
        let mut form = reqwest::multipart::Form::new().text("model", upstream_model.clone());
        for (name, file_name, content_type, bytes) in &fields {
            if name == "model" {
                continue;
            }
            let mut part = reqwest::multipart::Part::bytes(bytes.clone());
            if let Some(file_name) = file_name {
                part = part.file_name(file_name.clone());
            }
            if let Some(content_type) = content_type {
                part = part
                    .mime_str(content_type)
                    .unwrap_or_else(|_| reqwest::multipart::Part::bytes(Vec::new()));
            }
            form = form.part(name.clone(), part);
        }
        let mut request = client
            .post(format!(
                "{}/audio/transcriptions",
                base.as_str().trim_end_matches('/')
            ))
            .multipart(form);
        if let Some(key) = provider.api_key {
            request = request.bearer_auth(key);
        }
        match request.send().await {
            Ok(response) => {
                let status = response.status();
                let payload = response.json::<Value>().await.unwrap_or_else(|_| serde_json::json!({"error":{"message":"Invalid upstream response","type":"upstream_error"}}));
                if is_retryable_upstream_status(&state, status, None) {
                    record_attempt(
                        &state,
                        &request_id,
                        attempt_index,
                        &provider_slug,
                        &upstream_model,
                        "audio_transcriptions",
                        "failed",
                        payload["error"]["message"].as_str(),
                        Some(status.as_u16() as i32),
                        true,
                        start_time,
                    )
                    .await;
                    attempted = Some(
                        payload["error"]["message"]
                            .as_str()
                            .unwrap_or("retryable upstream error")
                            .to_owned(),
                    );
                    register_provider_failure(
                        &state,
                        &provider_slug,
                        &upstream_model,
                        attempted.clone().unwrap_or_default(),
                    )
                    .await;
                    continue;
                }
                record_attempt(
                    &state,
                    &request_id,
                    attempt_index,
                    &provider_slug,
                    &upstream_model,
                    "audio_transcriptions",
                    if status.is_success() {
                        "success"
                    } else {
                        "failed"
                    },
                    payload["error"]["message"].as_str(),
                    Some(status.as_u16() as i32),
                    false,
                    start_time,
                )
                .await;
                return (status, Json(payload)).into_response();
            }
            Err(error) => {
                record_attempt(
                    &state,
                    &request_id,
                    attempt_index,
                    &provider_slug,
                    &upstream_model,
                    "audio_transcriptions",
                    "failed",
                    Some(&error.to_string()),
                    None,
                    true,
                    start_time,
                )
                .await;
                register_provider_failure(
                    &state,
                    &provider_slug,
                    &upstream_model,
                    error.to_string(),
                )
                .await;
                attempted = Some(error.to_string());
            }
        }
    }
    error_response(
        axum::http::StatusCode::BAD_GATEWAY,
        attempted.as_deref().unwrap_or("upstream error"),
        "upstream_error",
    )
}

async fn audio_speech(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    forward_json_api(
        &state,
        &headers,
        body,
        "/audio/speech",
        "authorization",
        true,
    )
    .await
}

async fn messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    forward_json_api(&state, &headers, body, "/messages", "x-api-key", true).await
}

/// Gemini uses the same routing/retry dispatcher as the other JSON surfaces;
/// only its native URL and edge transforms differ.
async fn gemini(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(model_action): Path<String>,
    Json(mut body): Json<Value>,
) -> Response {
    let Some((model, action)) = model_action.split_once(':') else {
        return error_response(
            axum::http::StatusCode::BAD_REQUEST,
            "Model action is required",
            "invalid_request_error",
        );
    };
    if action != "generateContent" && action != "streamGenerateContent" {
        return error_response(
            axum::http::StatusCode::BAD_REQUEST,
            "Unsupported Gemini action",
            "invalid_request_error",
        );
    }
    body["model"] = Value::String(model.to_owned());
    forward_json_api(
        &state,
        &headers,
        body,
        &format!("/v1beta/models/{{model}}:{action}"),
        "x-goog-api-key",
        true,
    )
    .await
}

/// Anthropic's wire protocol is deliberately kept at the edge.  Providers
/// which expose an OpenAI-compatible endpoint receive the equivalent chat
/// request, while native Anthropic endpoints continue to receive the request
/// unchanged.
fn gemini_native_provider(slug: &str, provider: &crate::state::ProviderConfig) -> bool {
    let host = url::Url::parse(&provider.api_base_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase));
    slug.eq_ignore_ascii_case("gemini")
        || slug.to_ascii_lowercase().contains("gemini")
        || host.as_deref().is_some_and(|host| {
            host == "googleapis.com"
                || host.ends_with(".googleapis.com")
                || host == "generativelanguage.googleapis.com"
        })
}

fn gemini_parts_to_chat(parts: &Value) -> Value {
    let parts = parts
        .as_array()
        .cloned()
        .unwrap_or_else(|| vec![parts.clone()]);
    let mut output = Vec::new();
    for part in parts {
        if let Some(text) = part.get("text") {
            output.push(serde_json::json!({"type":"text", "text":text}));
        }
        if let Some(data) = part.get("inlineData") {
            if let (Some(mime), Some(encoded)) = (
                data.get("mimeType").and_then(Value::as_str),
                data.get("data").and_then(Value::as_str),
            ) {
                output.push(serde_json::json!({"type":"image_url", "image_url":{"url":format!("data:{mime};base64,{encoded}")}}));
            }
        }
    }
    if output.len() == 1 {
        output.remove(0)
    } else {
        Value::Array(output)
    }
}

fn gemini_to_chat(body: Value, model: &str) -> Value {
    let mut messages = Vec::new();
    if let Some(instruction) = body
        .get("systemInstruction")
        .or_else(|| body.get("system_instruction"))
    {
        let parts = instruction.get("parts").unwrap_or(instruction);
        messages.push(serde_json::json!({"role":"system", "content":gemini_parts_to_chat(parts)}));
    }
    if let Some(contents) = body.get("contents").and_then(Value::as_array) {
        for content in contents {
            let source_role = content
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or("user");
            let role = if source_role == "model" {
                "assistant"
            } else {
                source_role
            };
            let parts = content
                .get("parts")
                .cloned()
                .unwrap_or_else(|| Value::Array(vec![]));
            let mut message =
                serde_json::json!({"role":role, "content":gemini_parts_to_chat(&parts)});
            let calls: Vec<Value> = parts.as_array().into_iter().flatten().filter_map(|part| part.get("functionCall")).enumerate().map(|(index, call)| serde_json::json!({"id":call.get("id").cloned().unwrap_or_else(|| Value::String(format!("call_{index}"))),"type":"function","function":{"name":call.get("name"),"arguments":serde_json::to_string(call.get("args").unwrap_or(&Value::Null)).unwrap_or_else(|_| "{}".into())}})).collect();
            if !calls.is_empty() {
                message["tool_calls"] = Value::Array(calls);
            }
            if let Some(response) = parts
                .as_array()
                .into_iter()
                .flatten()
                .find_map(|part| part.get("functionResponse"))
            {
                message = serde_json::json!({"role":"tool", "content":response.get("response").cloned().unwrap_or(Value::Null)});
            }
            messages.push(message);
        }
    }
    let config = body.get("generationConfig").cloned().unwrap_or(Value::Null);
    let mut output = serde_json::json!({"model":model, "messages":messages});
    for (source, target) in [
        ("maxOutputTokens", "max_tokens"),
        ("temperature", "temperature"),
        ("topP", "top_p"),
        ("stopSequences", "stop"),
    ] {
        if let Some(value) = config.get(source) {
            output[target] = value.clone();
        }
    }
    if config.get("responseMimeType").and_then(Value::as_str) == Some("application/json") {
        output["response_format"] = serde_json::json!({"type":"json_object"});
    }
    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        let mut converted = Vec::new();
        for tool in tools {
            if let Some(declarations) = tool.get("functionDeclarations").and_then(Value::as_array) {
                for function in declarations {
                    converted.push(serde_json::json!({"type":"function", "function":{"name":function["name"],"description":function["description"],"parameters":function.get("parameters").cloned().unwrap_or_else(|| serde_json::json!({"type":"object"}))}}));
                }
            }
        }
        output["tools"] = Value::Array(converted);
    }
    output
}

fn chat_to_gemini(payload: Value) -> Value {
    let mut candidates = Vec::new();
    if let Some(choices) = payload.get("choices").and_then(Value::as_array) {
        for choice in choices {
            let message = choice.get("message").unwrap_or(&Value::Null);
            let mut parts = Vec::new();
            if let Some(content) = message.get("content") {
                let values = content
                    .as_array()
                    .cloned()
                    .unwrap_or_else(|| vec![content.clone()]);
                for value in values {
                    if value
                        .get("type")
                        .and_then(Value::as_str)
                        .is_none_or(|kind| kind == "text")
                    {
                        parts.push(
                            serde_json::json!({"text":value.get("text").cloned().unwrap_or(value)}),
                        );
                    }
                }
            }
            if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
                for call in calls {
                    let function = call.get("function").unwrap_or(&Value::Null);
                    let args = function
                        .get("arguments")
                        .and_then(Value::as_str)
                        .and_then(|value| serde_json::from_str(value).ok())
                        .unwrap_or(Value::Object(Default::default()));
                    parts.push(serde_json::json!({"functionCall":{"name":function.get("name"),"args":args,"id":call.get("id")}}));
                }
            }
            let finish = match choice.get("finish_reason").and_then(Value::as_str) {
                Some("length") => "MAX_TOKENS",
                Some("tool_calls") => "STOP",
                Some(reason) => reason,
                None => "STOP",
            };
            candidates.push(
                serde_json::json!({"content":{"role":"model","parts":parts},"finishReason":finish}),
            );
        }
    }
    let usage = payload.get("usage").unwrap_or(&Value::Null);
    serde_json::json!({"candidates":candidates,"usageMetadata":{"promptTokenCount":usage.get("prompt_tokens").cloned().unwrap_or(Value::from(0)),"candidatesTokenCount":usage.get("completion_tokens").cloned().unwrap_or(Value::from(0)),"totalTokenCount":usage.get("total_tokens").cloned().unwrap_or(Value::from(0))}})
}

fn normalize_anthropic_error(payload: Value, status: axum::http::StatusCode) -> Value {
    normalize_provider_error(payload, status, |kind| kind)
}

fn normalize_gemini_error(payload: Value, status: axum::http::StatusCode) -> Value {
    normalize_provider_error(payload, status, |kind| match kind {
        "INVALID_ARGUMENT" => "invalid_request_error",
        "RESOURCE_EXHAUSTED" => "rate_limit_error",
        "PERMISSION_DENIED" | "UNAUTHENTICATED" => "auth_error",
        "NOT_FOUND" => "model_not_found",
        "ABORTED" | "DEADLINE_EXCEEDED" => "timeout_error",
        _ => "upstream_error",
    })
}

fn normalize_provider_error<F>(payload: Value, status: axum::http::StatusCode, map_type: F) -> Value
where
    F: Fn(&str) -> &str,
{
    let Some(error) = payload.get("error").and_then(Value::as_object) else {
        return serde_json::json!({"error":{"message":"Invalid upstream error response","type":"upstream_error","code":status.as_u16()}});
    };
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .filter(|message| !message.is_empty())
        .unwrap_or("Upstream provider request failed");
    let provider_type = error
        .get("status")
        .and_then(Value::as_str)
        .or_else(|| error.get("type").and_then(Value::as_str))
        .unwrap_or("upstream_error");
    serde_json::json!({"error":{"message":message,"type":map_type(provider_type),"code":status.as_u16()}})
}

fn anthropic_native_provider(provider: &crate::state::ProviderConfig) -> bool {
    let anthropic_host = url::Url::parse(&provider.api_base_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
        .is_some_and(|host| host == "anthropic.com" || host.ends_with(".anthropic.com"));
    anthropic_host
        || provider
            .oauth_provider
            .as_deref()
            .is_some_and(|name| name.eq_ignore_ascii_case("anthropic"))
}

fn claude_masking_enabled(provider: &crate::state::ProviderConfig) -> bool {
    anthropic_native_provider(provider)
        && (provider.use_claude_masking
            || provider
                .oauth_provider
                .as_deref()
                .is_some_and(|name| name.eq_ignore_ascii_case("anthropic")))
}

fn anthropic_to_chat(mut body: Value) -> Value {
    let mut messages = Vec::new();
    if let Some(system) = body.get("system").cloned() {
        let content = match system {
            Value::String(text) => Value::String(text),
            other => other,
        };
        messages.push(serde_json::json!({"role":"system", "content": content}));
    }
    if let Some(input) = body.get("messages").and_then(Value::as_array) {
        for message in input {
            let role = message
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or("user");
            let content = message.get("content").cloned().unwrap_or(Value::Null);
            let blocks = content.as_array().cloned().unwrap_or_else(|| vec![content]);
            let mut text = Vec::new();
            let mut tool_calls = Vec::new();
            let mut tool_results = Vec::new();
            for block in blocks {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => if let Some(value) = block.get("text") { text.push(value.clone()); },
                    Some("tool_use") => tool_calls.push(serde_json::json!({
                        "id": block.get("id").cloned().unwrap_or(Value::String("tool_call".into())),
                        "type": "function",
                        "function": {"name": block.get("name"), "arguments": serde_json::to_string(block.get("input").unwrap_or(&Value::Null)).unwrap_or_else(|_| "{}".into())}
                    })),
                    Some("tool_result") => tool_results.push(serde_json::json!({
                        "role": "tool", "tool_call_id": block.get("tool_use_id"),
                        "content": block.get("content").cloned().unwrap_or(Value::Null)
                    })),
                    _ => text.push(block),
                }
            }
            // A tool result belongs after the assistant tool call it answers.
            // Do not prepend it to the current user turn: that produces an
            // invalid OpenAI sequence (tool -> user) for Anthropic's normal
            // assistant-tool_use / user-tool_result exchange.
            if !text.is_empty() || !tool_calls.is_empty() {
                let mut converted = serde_json::json!({"role": role, "content": if text.len() == 1 { text.remove(0) } else { Value::Array(text) }});
                if !tool_calls.is_empty() {
                    converted["tool_calls"] = Value::Array(tool_calls);
                }
                messages.push(converted);
            }
            messages.extend(tool_results);
        }
    }
    body["messages"] = Value::Array(messages);
    if let Some(max) = body.get("max_tokens").cloned() {
        body["max_tokens"] = max;
    }
    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        body["tools"] = Value::Array(tools.iter().map(|tool| serde_json::json!({
            "type":"function", "function": {
                "name": tool.get("name"), "description": tool.get("description"),
                "parameters": tool.get("input_schema").cloned().unwrap_or_else(|| serde_json::json!({"type":"object"}))
            }
        })).collect());
    }
    body
}

fn chat_to_anthropic(payload: Value) -> Value {
    let message = payload
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(|c| c.get("message"));
    let mut content = Vec::new();
    if let Some(value) = message.and_then(|m| m.get("content")) {
        match value {
            Value::String(text) => content.push(serde_json::json!({"type":"text", "text": text})),
            Value::Array(blocks) => {
                for block in blocks {
                    match block.get("type").and_then(Value::as_str) {
                        Some("text") => content.push(block.clone()),
                        Some("output_text") => content.push(serde_json::json!({
                            "type": "text",
                            "text": block.get("text").cloned().unwrap_or(Value::String(String::new()))
                        })),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    if let Some(calls) = message
        .and_then(|m| m.get("tool_calls"))
        .and_then(Value::as_array)
    {
        for call in calls {
            let function = call.get("function").unwrap_or(&Value::Null);
            let input = match function.get("arguments") {
                Some(Value::String(s)) => serde_json::from_str(s).unwrap_or(Value::Null),
                Some(value) => value.clone(),
                None => Value::Object(Default::default()),
            };
            content.push(serde_json::json!({"type":"tool_use", "id":call.get("id").cloned().unwrap_or(Value::String("tool_call".into())), "name":function.get("name").cloned().unwrap_or(Value::String("unknown".into())), "input":input}));
        }
    }
    let finish = payload
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(|c| c.get("finish_reason"))
        .and_then(Value::as_str);
    let stop_reason = match finish {
        Some("tool_calls") => "tool_use",
        Some("length") => "max_tokens",
        _ => "end_turn",
    };
    serde_json::json!({"id": payload.get("id"), "type":"message", "role":"assistant", "content":content,
        "model":payload.get("model"), "stop_reason":stop_reason, "stop_sequence":null,
        "usage":{"input_tokens":payload.get("usage").and_then(|u| u.get("prompt_tokens")).cloned().unwrap_or(Value::from(0)), "output_tokens":payload.get("usage").and_then(|u| u.get("completion_tokens")).cloned().unwrap_or(Value::from(0))}})
}

async fn image_edits(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    forward_json_api(
        &state,
        &headers,
        body,
        "/images/edits",
        "authorization",
        true,
    )
    .await
}

async fn images(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    forward_json_api(
        &state,
        &headers,
        body,
        "/images/generations",
        "authorization",
        true,
    )
    .await
}

async fn forward_json_api(
    state: &AppState,
    headers: &HeaderMap,
    mut body: Value,
    path: &str,
    auth_header: &str,
    failover: bool,
) -> Response {
    let principal = match authenticate(state, headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let Some(model) = body.get("model").and_then(Value::as_str).map(str::to_owned) else {
        return error_response(
            axum::http::StatusCode::BAD_REQUEST,
            "model is required",
            "invalid_request_error",
        );
    };
    let key_config = match &principal {
        Principal::Limited { config, .. } => Some(config),
        Principal::Admin => None,
    };
    if let Err(response) = enforce_request_quotas(state, &principal).await {
        return response;
    }
    // Validate image parts before dispatch. This keeps malformed multimodal
    // payloads on the normal text path and avoids unsafe assumptions in
    // provider transforms; descriptor generation is opt-in via configuration.
    let _image_urls = crate::vision::image_urls(&body);
    apply_native_compaction(&mut body, state);
    apply_headroom_compaction(&mut body, state);
    if let Some(config) = key_config {
        if !model_allowed(config, &model) {
            return error_response(
                axum::http::StatusCode::FORBIDDEN,
                &format!("Key is not allowed to access model '{model}'"),
                "access_denied",
            );
        }
    }
    crate::system_logs::event(
        state,
        "info",
        "inference request started",
        [
            ("model".into(), Value::String(model.clone())),
            ("path".into(), Value::String(path.into())),
        ],
    );
    let client = reqwest::Client::new();
    let original_body = body.clone();
    let request_id = new_request_id();
    let start_time = now_ms();
    let mut attempted = None;
    let targets = inference_targets(state, &model, key_config);
    for (attempt, (provider_slug, upstream_model)) in targets.enumerate() {
        if !failover && attempt > 0 {
            break;
        }
        if key_config.is_some_and(|config| !provider_allowed(config, &provider_slug)) {
            return error_response(
                axum::http::StatusCode::FORBIDDEN,
                "Key is not allowed to access any provider configured for model",
                "access_denied",
            );
        }
        let Some(provider) = state
            .inner
            .providers
            .read()
            .unwrap()
            .get(&provider_slug)
            .cloned()
        else {
            return error_response(
                axum::http::StatusCode::NOT_FOUND,
                "No provider configured for model",
                "model_not_found",
            );
        };
        let Ok(base) = dispatch_provider_url(&provider.api_base_url) else {
            return error_response(
                axum::http::StatusCode::BAD_GATEWAY,
                "Provider API base URL is invalid",
                "upstream_error",
            );
        };
        let mut request_body = original_body.clone();
        request_body["model"] = Value::String(upstream_model.clone());
        let native_anthropic = path == "/messages" && anthropic_native_provider(&provider);
        let claude_masking = claude_masking_enabled(&provider);
        let codex_oauth = provider
            .oauth_provider
            .as_deref()
            .is_some_and(|name| name.eq_ignore_ascii_case("openai-codex"));
        let native_gemini = path.starts_with("/v1beta/models/")
            && gemini_native_provider(&provider_slug, &provider);
        if path == "/messages" && !native_anthropic {
            request_body = anthropic_to_chat(request_body);
            request_body["model"] = Value::String(upstream_model.clone());
        } else if path.starts_with("/v1beta/models/") && !native_gemini {
            request_body = gemini_to_chat(request_body, &upstream_model);
        }
        let request_path = path.replace("{model}", &upstream_model);
        let mut request = client
            .post(format!(
                "{}{}",
                base.as_str().trim_end_matches('/'),
                request_path
            ))
            .json(&request_body);
        if claude_masking {
            request_body = crate::anthropic_masking::mask_body(&request_body);
            request = request
                .header("user-agent", crate::anthropic_masking::user_agent())
                .header("x-app", "cli")
                .header("anthropic-dangerous-direct-browser-access", "true")
                .json(&request_body);
        }
        if let Some(key) = provider.api_key {
            request = if codex_oauth {
                let mut request = request
                    .bearer_auth(&key)
                    .header("originator", "codex_cli_rs")
                    .header("Version", crate::codex_masking::CODEX_VERSION)
                    .header("User-Agent", crate::codex_masking::user_agent());
                if let Some(account) = crate::codex_masking::account_id(&key) {
                    request = request.header("chatgpt-account-id", account);
                }
                request
            } else if auth_header == "x-api-key" {
                request.header("x-api-key", key)
            } else {
                request.bearer_auth(key)
            };
        }
        let mut retry_count = 0;
        let response_result = loop {
            let result = request
                .try_clone()
                .expect("inference request is cloneable")
                .send()
                .await;
            match result {
                Ok(response)
                    if is_retryable_upstream_status(state, response.status(), None)
                        && retry_count < provider.retry_policy.max_retries =>
                {
                    let _ = response.bytes().await;
                    retry_count += 1;
                    let delay = (provider.retry_policy.delay_ms as f64
                        * provider
                            .retry_policy
                            .backoff_multiplier
                            .powi((retry_count - 1) as i32)) as u64;
                    if delay > 0 {
                        tokio::time::sleep(Duration::from_millis(delay)).await;
                    }
                }
                result => break result,
            }
        };
        match response_result {
            Ok(response) => {
                let status = response.status();
                if original_body
                    .get("stream")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                    && (native_gemini || native_anthropic)
                {
                    let content_type = response
                        .headers()
                        .get("content-type")
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or("text/event-stream");
                    return axum::http::Response::builder()
                        .status(status)
                        .header("content-type", content_type)
                        .body(accounted_sse_body(
                            response.bytes_stream(),
                            StreamAccounting {
                                pool: state.inner.database.clone(),
                                event_sender: state.inner.usage_events.clone(),
                                request_id: new_request_id(),
                                key_name: None,
                                provider: provider_slug.clone(),
                                model: model.clone(),
                                api_type: if native_gemini { "gemini" } else { "messages" }.into(),
                                start_time: now_ms(),
                                input_tokens: None,
                                output_tokens: None,
                                cost: None,
                                terminal_on_usage: native_gemini,
                                stream_success: status.is_success(),
                                finalized: false,
                            },
                        ))
                        .expect("valid streaming response")
                        .into_response();
                }
                let mut payload = response.json::<Value>().await.unwrap_or_else(|_| serde_json::json!({"error":{"message":"Invalid upstream response","type":"upstream_error"}}));
                if path.starts_with("/v1beta/models/") && native_gemini && !status.is_success() {
                    payload = normalize_gemini_error(payload, status);
                } else if path.starts_with("/v1beta/models/")
                    && !native_gemini
                    && status.is_success()
                {
                    payload = chat_to_gemini(payload);
                } else if path == "/messages" && !native_anthropic && status.is_success() {
                    payload = chat_to_anthropic(payload);
                } else if path == "/messages" && native_anthropic && !status.is_success() {
                    payload = normalize_anthropic_error(payload, status);
                }
                // Retry provider failures, but preserve deterministic client errors.
                if failover && is_retryable_upstream_status(state, status, None) {
                    record_attempt(
                        state,
                        &request_id,
                        (attempt + 1) as i32,
                        &provider_slug,
                        &upstream_model,
                        path,
                        "failed",
                        payload["error"]["message"].as_str(),
                        Some(status.as_u16() as i32),
                        true,
                        start_time,
                    )
                    .await;
                    register_provider_failure(
                        state,
                        &provider_slug,
                        &upstream_model,
                        format!("upstream returned HTTP {}", status.as_u16()),
                    )
                    .await;
                    attempted = Some(
                        payload["error"]["message"]
                            .as_str()
                            .unwrap_or("retryable upstream error")
                            .to_owned(),
                    );
                    continue;
                }
                record_attempt(
                    state,
                    &request_id,
                    (attempt + 1) as i32,
                    &provider_slug,
                    &upstream_model,
                    path,
                    if status.is_success() {
                        "success"
                    } else {
                        "failed"
                    },
                    payload["error"]["message"].as_str(),
                    Some(status.as_u16() as i32),
                    false,
                    start_time,
                )
                .await;
                crate::system_logs::event(
                    state,
                    if status.is_success() { "info" } else { "error" },
                    "inference request completed",
                    [
                        ("provider".into(), Value::String(provider_slug.clone())),
                        ("model".into(), Value::String(upstream_model.clone())),
                        ("status".into(), Value::from(status.as_u16())),
                    ],
                );
                return (status, Json(payload)).into_response();
            }
            Err(error) => {
                if !is_retryable_upstream_error(state, &error.to_string()) {
                    record_attempt(
                        state,
                        &request_id,
                        (attempt + 1) as i32,
                        &provider_slug,
                        &upstream_model,
                        path,
                        "failed",
                        Some(&error.to_string()),
                        None,
                        false,
                        start_time,
                    )
                    .await;
                    return error_response(
                        axum::http::StatusCode::BAD_GATEWAY,
                        &error.to_string(),
                        "upstream_error",
                    );
                }
                record_attempt(
                    state,
                    &request_id,
                    (attempt + 1) as i32,
                    &provider_slug,
                    &upstream_model,
                    path,
                    "failed",
                    Some(&error.to_string()),
                    None,
                    true,
                    start_time,
                )
                .await;
                register_provider_failure(
                    state,
                    &provider_slug,
                    &upstream_model,
                    error.to_string(),
                )
                .await;
                attempted = Some(error.to_string());
            }
        }
    }
    error_response(
        axum::http::StatusCode::BAD_GATEWAY,
        attempted.as_deref().unwrap_or("upstream error"),
        "upstream_error",
    )
}

async fn responses(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut body): Json<Value>,
) -> Response {
    let principal = match authenticate(&state, &headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let Some(model) = body.get("model").and_then(Value::as_str).map(str::to_owned) else {
        return error_response(
            axum::http::StatusCode::BAD_REQUEST,
            "model is required",
            "invalid_request_error",
        );
    };
    let conversation_id = body.get("conversation").and_then(|value| {
        value
            .as_str()
            .map(str::to_owned)
            .or_else(|| value.get("id").and_then(Value::as_str).map(str::to_owned))
    });
    let new_input = body.get("input").cloned();
    if let Some(previous_id) = body.get("previous_response_id").and_then(Value::as_str) {
        let Some(pool) = &state.inner.database else {
            return error_response(
                axum::http::StatusCode::NOT_FOUND,
                "Previous response not found",
                "invalid_request_error",
            );
        };
        let previous = sqlx::query_scalar::<_, Option<String>>(
            "SELECT output_items FROM responses WHERE id=$1",
        )
        .bind(previous_id)
        .fetch_optional(pool)
        .await;
        let Some(Some(items)) = previous.ok().flatten() else {
            return error_response(
                axum::http::StatusCode::NOT_FOUND,
                &format!("Previous response not found: {previous_id}"),
                "invalid_request_error",
            );
        };
        let mut merged = serde_json::from_str::<Value>(&items)
            .ok()
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default();
        let current = new_input.clone().unwrap_or_else(|| serde_json::json!([]));
        if let Some(values) = current.as_array() {
            merged.extend(values.iter().cloned());
        } else {
            merged.push(current);
        }
        body["input"] = Value::Array(merged);
    }
    let key_config = match &principal {
        Principal::Limited { config, .. } => Some(config),
        Principal::Admin => None,
    };
    let usage_key_name = match &principal {
        Principal::Limited { key_name, .. } => Some(key_name.as_str()),
        Principal::Admin => None,
    };
    if key_config.is_some_and(|config| !model_allowed(config, &model)) {
        return error_response(
            axum::http::StatusCode::FORBIDDEN,
            &format!("Key is not allowed to access model '{model}'"),
            "access_denied",
        );
    }
    let targets: Vec<_> = inference_targets(&state, &model, key_config).collect();
    if targets.is_empty() {
        return error_response(
            axum::http::StatusCode::NOT_FOUND,
            "No provider configured for model",
            "model_not_found",
        );
    }
    let request_id = new_request_id();
    let start_time = now_ms();
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            return error_response(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                &error.to_string(),
                "server_error",
            )
        }
    };
    let streaming = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let mut last_error = None;

    for (index, (provider_slug, upstream_model)) in targets.into_iter().enumerate() {
        let attempt = (index + 1) as i32;
        let Some(provider) = state
            .inner
            .providers
            .read()
            .unwrap()
            .get(&provider_slug)
            .cloned()
        else {
            continue;
        };
        let base = match dispatch_provider_url(&provider.api_base_url) {
            Ok(base) => base,
            Err(_) => {
                record_attempt(
                    &state,
                    &request_id,
                    attempt,
                    &provider_slug,
                    &upstream_model,
                    "responses",
                    "failed",
                    Some("Provider API base URL is invalid"),
                    None,
                    true,
                    start_time,
                )
                .await;
                last_error = Some("Provider API base URL is invalid".to_owned());
                continue;
            }
        };
        let mut request_body = body.clone();
        request_body["model"] = Value::String(upstream_model.clone());
        let mut request = client
            .post(format!("{}/responses", base.as_str().trim_end_matches('/')))
            .json(&request_body);
        if let Some(key) = provider.api_key.clone() {
            request = request.bearer_auth(key);
        }
        match request.send().await {
            Ok(response) => {
                let status = response.status();
                let retryable = is_retryable_upstream_status(&state, status, None);
                if retryable {
                    let payload = response.json::<Value>().await.unwrap_or_else(|_| serde_json::json!({"error":{"message":"Invalid upstream response","type":"upstream_error"}}));
                    let reason = payload["error"]["message"]
                        .as_str()
                        .unwrap_or("retryable upstream error")
                        .to_owned();
                    record_attempt(
                        &state,
                        &request_id,
                        attempt,
                        &provider_slug,
                        &upstream_model,
                        "responses",
                        "failed",
                        Some(&reason),
                        Some(status.as_u16() as i32),
                        true,
                        start_time,
                    )
                    .await;
                    register_provider_failure(
                        &state,
                        &provider_slug,
                        &upstream_model,
                        reason.clone(),
                    )
                    .await;
                    last_error = Some(reason);
                    continue;
                }
                record_attempt(
                    &state,
                    &request_id,
                    attempt,
                    &provider_slug,
                    &upstream_model,
                    "responses",
                    if status.is_success() {
                        "success"
                    } else {
                        "failed"
                    },
                    None,
                    Some(status.as_u16() as i32),
                    false,
                    start_time,
                )
                .await;
                if streaming {
                    let content_type = response
                        .headers()
                        .get("content-type")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("text/event-stream");
                    record_basic_usage(
                        &state,
                        &request_id,
                        usage_key_name,
                        &provider_slug,
                        &model,
                        "responses",
                        if status.is_success() {
                            "success"
                        } else {
                            "error"
                        },
                        true,
                        start_time,
                    )
                    .await;
                    return axum::http::Response::builder()
                        .status(status)
                        .header("content-type", content_type)
                        .body(accounted_sse_body(
                            response.bytes_stream(),
                            StreamAccounting {
                                pool: state.inner.database.clone(),
                                event_sender: state.inner.usage_events.clone(),
                                request_id: request_id.clone(),
                                key_name: usage_key_name.map(str::to_owned),
                                provider: provider_slug,
                                model: model.clone(),
                                api_type: "responses".to_owned(),
                                start_time,
                                input_tokens: None,
                                output_tokens: None,
                                cost: None,
                                terminal_on_usage: false,
                                stream_success: status.is_success(),
                                finalized: false,
                            },
                        ))
                        .unwrap()
                        .into_response();
                }
                let payload = response.json::<Value>().await.unwrap_or_else(|_| serde_json::json!({"error":{"message":"Invalid upstream response","type":"upstream_error"}}));
                if status.is_success() {
                    if let (Some(pool), Some(response_id)) = (
                        &state.inner.database,
                        payload.get("id").and_then(Value::as_str),
                    ) {
                        let output = payload
                            .get("output")
                            .cloned()
                            .unwrap_or_else(|| serde_json::json!([]));
                        let _ = sqlx::query("INSERT INTO responses (id, object, created_at, completed_at, status, model, output_items, previous_response_id, conversation_id, store, background) VALUES ($1,'response',$2,$2,$3,$4,$5,$6,$7,$8,$9) ON CONFLICT (id) DO UPDATE SET output_items=EXCLUDED.output_items, status=EXCLUDED.status, completed_at=EXCLUDED.completed_at").bind(response_id).bind(now_ms() / 1000).bind(payload.get("status").and_then(Value::as_str).unwrap_or("completed")).bind(payload.get("model").and_then(Value::as_str).unwrap_or(&model)).bind(output.to_string()).bind(body.get("previous_response_id").and_then(Value::as_str)).bind(&conversation_id).bind(if body.get("store").and_then(Value::as_bool).unwrap_or(true) { 1i32 } else { 0i32 }).bind(if body.get("background").and_then(Value::as_bool).unwrap_or(false) { 1i32 } else { 0i32 }).execute(pool).await;
                        if let Some(conversation_id) = &conversation_id {
                            let input = new_input.clone().unwrap_or_else(|| serde_json::json!([]));
                            let mut appended =
                                input.as_array().cloned().unwrap_or_else(|| vec![input]);
                            appended.extend(output.as_array().cloned().unwrap_or_default());
                            let _ = sqlx::query(
                                "UPDATE conversations SET items=$1, updated_at=$2 WHERE id=$3",
                            )
                            .bind(Value::Array(appended).to_string())
                            .bind(now_ms() / 1000)
                            .bind(conversation_id)
                            .execute(pool)
                            .await;
                        }
                    }
                }
                return (status, Json(payload)).into_response();
            }
            Err(error) => {
                let reason = error.to_string();
                let retryable = is_retryable_upstream_error(&state, &reason);
                record_attempt(
                    &state,
                    &request_id,
                    attempt,
                    &provider_slug,
                    &upstream_model,
                    "responses",
                    "failed",
                    Some(&reason),
                    None,
                    retryable,
                    start_time,
                )
                .await;
                if retryable {
                    register_provider_failure(
                        &state,
                        &provider_slug,
                        &upstream_model,
                        reason.clone(),
                    )
                    .await;
                    last_error = Some(reason);
                }
            }
        }
    }
    error_response(
        axum::http::StatusCode::BAD_GATEWAY,
        last_error.as_deref().unwrap_or("upstream error"),
        "upstream_error",
    )
}

async fn get_response(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if authenticate(&state, &headers).is_err() {
        return auth_error(axum::http::StatusCode::UNAUTHORIZED, "Unauthorized");
    }
    let Some(pool) = &state.inner.database else {
        return error_response(
            axum::http::StatusCode::NOT_FOUND,
            "Response not found",
            "not_found_error",
        );
    };
    match sqlx::query_scalar::<_, Option<String>>("SELECT output_items FROM responses WHERE id=$1")
        .bind(&id)
        .fetch_optional(pool)
        .await
    {
        Ok(Some(Some(value))) => Json(
            serde_json::from_str::<Value>(&value).unwrap_or_else(|_| serde_json::json!({"id": id})),
        )
        .into_response(),
        _ => error_response(
            axum::http::StatusCode::NOT_FOUND,
            "Response not found",
            "not_found_error",
        ),
    }
}

/// Retrieve a stored Responses API conversation.
///
/// Conversation items are persisted as JSON in the `items` column.  Keep the
/// stored representation opaque here: the Responses API permits several item
/// variants and clients expect those objects to be returned unchanged.
async fn get_conversation(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if authenticate(&state, &headers).is_err() {
        return auth_error(axum::http::StatusCode::UNAUTHORIZED, "Unauthorized");
    }
    let Some(pool) = &state.inner.database else {
        return error_response(
            axum::http::StatusCode::NOT_FOUND,
            "Conversation not found",
            "not_found_error",
        );
    };

    let row = sqlx::query(
        "SELECT id, created_at, updated_at, items, metadata FROM conversations WHERE id=$1",
    )
    .bind(&id)
    .fetch_optional(pool)
    .await;

    match row {
        Ok(Some(row)) => {
            let items = row
                .try_get::<String, _>("items")
                .ok()
                .and_then(|value| serde_json::from_str::<Value>(&value).ok())
                .unwrap_or_else(|| serde_json::json!([]));
            let metadata = row
                .try_get::<Option<String>, _>("metadata")
                .ok()
                .flatten()
                .and_then(|value| serde_json::from_str::<Value>(&value).ok());

            let mut conversation = serde_json::json!({
                "id": row.try_get::<String, _>("id").unwrap_or(id),
                "object": "conversation",
                "created_at": row.try_get::<i64, _>("created_at").unwrap_or_default(),
                "updated_at": row.try_get::<i64, _>("updated_at").unwrap_or_default(),
                "items": items,
            });
            if let Some(metadata) = metadata {
                conversation["metadata"] = metadata;
            }
            Json(conversation).into_response()
        }
        Ok(None) => error_response(
            axum::http::StatusCode::NOT_FOUND,
            "Conversation not found",
            "not_found_error",
        ),
        Err(_) => error_response(
            axum::http::StatusCode::NOT_FOUND,
            "Conversation not found",
            "not_found_error",
        ),
    }
}

async fn delete_response(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if authenticate(&state, &headers).is_err() {
        return auth_error(axum::http::StatusCode::UNAUTHORIZED, "Unauthorized");
    }
    let Some(pool) = &state.inner.database else {
        return error_response(
            axum::http::StatusCode::NOT_FOUND,
            "Response not found",
            "not_found_error",
        );
    };
    match sqlx::query("DELETE FROM responses WHERE id=$1")
        .bind(&id)
        .execute(pool)
        .await
    {
        Ok(result) if result.rows_affected() > 0 => {
            Json(serde_json::json!({"deleted":true,"id":id})).into_response()
        }
        _ => error_response(
            axum::http::StatusCode::NOT_FOUND,
            "Response not found",
            "not_found_error",
        ),
    }
}

async fn metadata_search(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<MetadataQuery>,
) -> Response {
    if authenticate(&state, &headers).is_err() {
        return auth_error(axum::http::StatusCode::UNAUTHORIZED, "Unauthorized");
    }
    let Some(source) = query
        .source
        .as_deref()
        .filter(|source| ["openrouter", "models.dev", "catwalk"].contains(source))
    else {
        return error_response(
            axum::http::StatusCode::BAD_REQUEST,
            "Missing or invalid 'source' parameter",
            "invalid_request_error",
        );
    };
    let q = query.q.unwrap_or_default().to_lowercase();
    let results = state
        .inner
        .aliases
        .read()
        .unwrap()
        .keys()
        .filter(|id| q.is_empty() || id.to_lowercase().contains(&q))
        .take(query.limit.unwrap_or(50).min(200))
        .map(|id| serde_json::json!({"id":id,"source":source}))
        .collect::<Vec<_>>();
    Json(serde_json::json!({"data":results,"count":results.len()})).into_response()
}
#[derive(serde::Deserialize)]
struct MetadataQuery {
    source: Option<String>,
    q: Option<String>,
    source_path: Option<String>,
    limit: Option<usize>,
}
async fn metadata_lookup(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<MetadataQuery>,
) -> Response {
    if authenticate(&state, &headers).is_err() {
        return auth_error(axum::http::StatusCode::UNAUTHORIZED, "Unauthorized");
    }
    if query
        .source
        .as_deref()
        .is_none_or(|source| !["openrouter", "models.dev", "catwalk"].contains(&source))
        || query.source_path.is_none()
    {
        return error_response(
            axum::http::StatusCode::BAD_REQUEST,
            "Missing or invalid metadata parameters",
            "invalid_request_error",
        );
    }
    let path = query.source_path.unwrap();
    if state.inner.aliases.read().unwrap().contains_key(&path) {
        Json(serde_json::json!({"data":{"id":path}})).into_response()
    } else {
        error_response(
            axum::http::StatusCode::NOT_FOUND,
            "No metadata found",
            "not_found_error",
        )
    }
}
async fn openrouter_models(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<MetadataQuery>,
) -> Response {
    if authenticate(&state, &headers).is_err() {
        return auth_error(axum::http::StatusCode::UNAUTHORIZED, "Unauthorized");
    }
    let q = query.q.unwrap_or_default().to_lowercase();
    let data = state
        .inner
        .aliases
        .read()
        .unwrap()
        .keys()
        .filter(|id| q.is_empty() || id.to_lowercase().contains(&q))
        .map(|id| Value::String(id.clone()))
        .collect::<Vec<_>>();
    Json(serde_json::json!({"data":data,"count":data.len()})).into_response()
}
async fn muse_models(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if authenticate(&state, &headers).is_err() {
        return auth_error(axum::http::StatusCode::UNAUTHORIZED, "Unauthorized");
    }
    let data = state
        .inner
        .aliases
        .read()
        .unwrap()
        .keys()
        .map(|id| serde_json::json!({"id":id,"object":"model","created":0,"owned_by":"meta","metadata":{"muse-code":{"name":id,"reasoning":false,"temperature":true,"tool_call":true,"attachment":false,"modalities":{"input":["text"],"output":["text"]}}}}))
        .collect::<Vec<_>>();
    Json(serde_json::json!({"object":"list","data":data})).into_response()
}

async fn completions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if body.get("prompt").is_none() {
        return error_response(
            axum::http::StatusCode::BAD_REQUEST,
            "prompt is required",
            "invalid_request_error",
        );
    }
    forward_json_api(
        &state,
        &headers,
        body,
        "/completions",
        "authorization",
        true,
    )
    .await
}

async fn embeddings(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if let Err(response) = authenticate(&state, &headers) {
        return response;
    }
    if body.get("input").is_none() {
        return error_response(
            axum::http::StatusCode::BAD_REQUEST,
            "input is required",
            "invalid_request_error",
        );
    }
    forward_json_api(&state, &headers, body, "/embeddings", "authorization", true).await
}

fn auth_error(status: axum::http::StatusCode, message: &'static str) -> Response {
    (
        status,
        Json(serde_json::json!({
            "error": { "message": message, "type": "auth_error", "code": status.as_u16() }
        })),
    )
        .into_response()
}

#[allow(clippy::result_large_err)] // Route handlers return Axum responses directly on auth failure.
fn authenticate(state: &AppState, headers: &HeaderMap) -> Result<Principal, Response> {
    if !headers.contains_key("authorization") && !headers.contains_key("x-api-key") {
        return Err(auth_error(
            axum::http::StatusCode::UNAUTHORIZED,
            "missing authorization header",
        ));
    }
    let key = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .or_else(|| headers.get("x-api-key").and_then(|v| v.to_str().ok()));
    match resolve_principal(state, key) {
        Some(principal) if headers.contains_key("x-api-key") => Ok(principal),
        Some(Principal::Limited { key_name, config }) => {
            Ok(Principal::Limited { key_name, config })
        }
        Some(Principal::Admin) | None => Err(auth_error(
            axum::http::StatusCode::UNAUTHORIZED,
            "invalid authorization header",
        )),
    }
}

async fn list_models(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if authenticate(&state, &headers).is_err() {
        return auth_error(axum::http::StatusCode::UNAUTHORIZED, "Unauthorized");
    }
    // Only alias slugs are listed here, matching Plexus's /v1/models: direct
    // provider/model syntax (e.g. "openai/gpt-4o") is intentionally excluded
    // from discovery since it's meant for debugging only, not client-facing
    // model selection.
    let mut data = Vec::new();
    for slug in state.inner.aliases.read().unwrap().keys() {
        data.push(serde_json::json!({ "id": slug, "object": "model", "owned_by": "trefoil" }));
    }
    data.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    Json(serde_json::json!({ "object": "list", "data": data })).into_response()
}

async fn chat_completions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut body): Json<Value>,
) -> Response {
    let principal = match authenticate(&state, &headers) {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    let usage_key_name = match &principal {
        Principal::Limited { key_name, .. } => Some(key_name.as_str()),
        Principal::Admin => None,
    };
    let request_id = format!(
        "trefoil-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default()
    );
    let start_time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default();
    let Some(model) = body.get("model").and_then(Value::as_str).map(str::to_owned) else {
        return error_response(
            axum::http::StatusCode::BAD_REQUEST,
            "model is required",
            "invalid_request_error",
        );
    };
    if body.get("messages").and_then(Value::as_array).is_none() {
        return error_response(
            axum::http::StatusCode::BAD_REQUEST,
            "messages must be an array",
            "invalid_request_error",
        );
    }
    if let Err(response) = enforce_request_quotas(&state, &principal).await {
        return response;
    }
    apply_native_compaction(&mut body, &state);
    apply_headroom_compaction(&mut body, &state);
    let key_config = if let Principal::Limited { config, .. } = &principal {
        if !model_allowed(config, &model) {
            return error_response(
                axum::http::StatusCode::FORBIDDEN,
                &format!("Key is not allowed to access model '{model}' for chat"),
                "access_denied",
            );
        }
        Some(config)
    } else {
        None
    };
    let (provider_slug, upstream_model) = resolve_target(&state, &model, key_config);
    if let Some(config) = key_config {
        if !provider_allowed(config, &provider_slug) {
            return error_response(
                axum::http::StatusCode::FORBIDDEN,
                &format!(
                    "Key is not allowed to access any provider configured for model '{model}'"
                ),
                "access_denied",
            );
        }
    }
    if target_is_on_cooldown(&state, &provider_slug, &upstream_model) {
        return error_response(
            axum::http::StatusCode::BAD_GATEWAY,
            &format!("Provider {provider_slug}/{upstream_model} is on cooldown"),
            "upstream_error",
        );
    }
    let Some(provider) = state
        .inner
        .providers
        .read()
        .unwrap()
        .get(&provider_slug)
        .cloned()
    else {
        return error_response(
            axum::http::StatusCode::NOT_FOUND,
            "No provider configured for model",
            "model_not_found",
        );
    };
    if provider.api_base_url.is_empty() {
        return error_response(
            axum::http::StatusCode::BAD_GATEWAY,
            "Provider has no API base URL",
            "upstream_error",
        );
    }
    let Ok(provider_url) = dispatch_provider_url(&provider.api_base_url) else {
        return error_response(
            axum::http::StatusCode::BAD_GATEWAY,
            "Provider API base URL is invalid",
            "upstream_error",
        );
    };
    let original_body = body.clone();
    body["model"] = Value::String(upstream_model);
    let endpoint = format!(
        "{}/chat/completions",
        provider_url.as_str().trim_end_matches('/')
    );
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            return error_response(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                &error.to_string(),
                "server_error",
            )
        }
    };
    let mut request = client.post(&endpoint).json(&body);
    if let Some(api_key) = provider.api_key {
        request = request.bearer_auth(api_key);
    }
    match request.send().await {
        Ok(upstream) => {
            let status = upstream.status();
            if body.get("stream").and_then(Value::as_bool).unwrap_or(false) {
                let content_type = upstream
                    .headers()
                    .get("content-type")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or("text/event-stream");
                return axum::http::Response::builder()
                    .status(status)
                    .header("content-type", content_type)
                    .header("cache-control", "no-cache")
                    .header("x-request-id", &request_id)
                    .body(accounted_sse_body(
                        upstream.bytes_stream(),
                        StreamAccounting {
                            pool: state.inner.database.clone(),
                            event_sender: state.inner.usage_events.clone(),
                            request_id: request_id.clone(),
                            key_name: usage_key_name.map(str::to_owned),
                            provider: provider_slug.clone(),
                            model: model.clone(),
                            api_type: "chat".to_owned(),
                            start_time,
                            input_tokens: None,
                            output_tokens: None,
                            cost: None,
                            terminal_on_usage: false,
                            stream_success: status.is_success(),
                            finalized: false,
                        },
                    ))
                    .expect("valid streaming response")
                    .into_response();
            }
            let payload = upstream.json::<Value>().await.unwrap_or_else(|_| serde_json::json!({
                "error": { "message": "Invalid response from upstream provider", "type": "upstream_error" }
            }));
            if let Some(pool) = &state.inner.database {
                let input_tokens = payload["usage"]["prompt_tokens"]
                    .as_i64()
                    .map(|value| value as i32);
                let output_tokens = payload["usage"]["completion_tokens"]
                    .as_i64()
                    .map(|value| value as i32);
                let total_cost = payload["usage"]["total_cost"]
                    .as_f64()
                    .map(|value| value as f32);
                let duration_ms = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|value| value.as_millis() as i64)
                    .unwrap_or(start_time)
                    - start_time;
                let _ = crate::database::record_request_usage(
                    pool,
                    &request_id,
                    usage_key_name,
                    Some(&provider_slug),
                    Some(&model),
                    "chat",
                    if status.is_success() {
                        "success"
                    } else {
                        "error"
                    },
                    false,
                    start_time,
                )
                .await;
                let _ = crate::database::update_request_usage_metrics(
                    pool,
                    &request_id,
                    input_tokens,
                    output_tokens,
                    total_cost,
                    duration_ms,
                )
                .await;
            }
            let mut response = (status, Json(payload)).into_response();
            if let Ok(value) = request_id.parse() {
                response.headers_mut().insert("x-request-id", value);
            }
            response
        }
        Err(error) => {
            record_attempt(
                &state,
                &request_id,
                1,
                &provider_slug,
                &model,
                "inference",
                "failed",
                Some(&error.to_string()),
                None,
                true,
                start_time,
            )
            .await;
            register_provider_failure(&state, &provider_slug, &model, error.to_string()).await;
            // `resolve_target` intentionally returns the first eligible target.
            // Calling it again here therefore retries the same provider rather
            // than traversing the alias's ordered failover chain. Select the
            // next target explicitly so chat follows the same ordering as the
            // other inference routes.
            if let Some((fallback_provider_slug, fallback_model)) =
                inference_targets(&state, &model, key_config).nth(1)
            {
                let fallback_provider = {
                    state
                        .inner
                        .providers
                        .read()
                        .unwrap()
                        .get(&fallback_provider_slug)
                        .cloned()
                };
                if let Some(fallback_provider) = fallback_provider {
                    if let Ok(fallback_base) =
                        dispatch_provider_url(&fallback_provider.api_base_url)
                    {
                        let mut fallback_body = original_body;
                        fallback_body["model"] = Value::String(fallback_model.clone());
                        let mut fallback_request = client
                            .post(format!(
                                "{}/chat/completions",
                                fallback_base.as_str().trim_end_matches('/')
                            ))
                            .json(&fallback_body);
                        if let Some(api_key) = fallback_provider.api_key {
                            fallback_request = fallback_request.bearer_auth(api_key);
                        }
                        match fallback_request.send().await {
                            Ok(fallback) => {
                                let fallback_status = fallback.status();
                                let fallback_payload = fallback.json::<Value>().await.unwrap_or_else(|_| {
                                    serde_json::json!({"error": {"message": "Invalid response from upstream provider", "type": "upstream_error"}})
                                });
                                record_attempt(
                                    &state,
                                    &request_id,
                                    2,
                                    &fallback_provider_slug,
                                    &fallback_model,
                                    "chat",
                                    if fallback_status.is_success() {
                                        "success"
                                    } else {
                                        "failed"
                                    },
                                    fallback_payload["error"]["message"].as_str(),
                                    Some(fallback_status.as_u16() as i32),
                                    false,
                                    start_time,
                                )
                                .await;
                                record_response_usage(
                                    &state,
                                    &request_id,
                                    usage_key_name,
                                    &fallback_provider_slug,
                                    &model,
                                    "chat",
                                    fallback_status.is_success(),
                                    start_time,
                                    &fallback_payload,
                                    true,
                                )
                                .await;
                                return response_with_request_id(
                                    fallback_status,
                                    fallback_payload,
                                    &request_id,
                                );
                            }
                            Err(fallback_error) => {
                                record_attempt(
                                    &state,
                                    &request_id,
                                    2,
                                    &fallback_provider_slug,
                                    &fallback_model,
                                    "chat",
                                    "failed",
                                    Some(&fallback_error.to_string()),
                                    None,
                                    true,
                                    start_time,
                                )
                                .await;
                                register_provider_failure(
                                    &state,
                                    &fallback_provider_slug,
                                    &fallback_model,
                                    fallback_error.to_string(),
                                )
                                .await;
                            }
                        }
                    }
                }
            }
            record_basic_usage(
                &state,
                &request_id,
                usage_key_name,
                &provider_slug,
                &model,
                "chat",
                "error",
                false,
                start_time,
            )
            .await;
            error_with_request_id(
                axum::http::StatusCode::BAD_GATEWAY,
                &error.to_string(),
                "upstream_error",
                &request_id,
            )
        }
    }
}

async fn register_provider_failure(state: &AppState, provider: &str, model: &str, message: String) {
    let entry = {
        let mut cooldowns = state.inner.cooldowns.write().unwrap();
        let key = (provider.to_owned(), model.to_owned());
        let failures = cooldowns
            .get(&key)
            .map(|e| e.consecutive_failures)
            .unwrap_or(0)
            + 1;
        let entry = crate::state::CooldownEntry {
            expiry_ms: now_ms() + 60_000,
            consecutive_failures: failures,
            last_error: Some(message),
        };
        cooldowns.insert(key, entry.clone());
        entry
    };
    if let Some(pool) = &state.inner.database {
        let _ = crate::database::upsert_cooldown(pool, provider, model, &entry).await;
    }
}

fn error_response(status: axum::http::StatusCode, message: &str, error_type: &str) -> Response {
    (
        status,
        Json(serde_json::json!({ "error": { "message": message, "type": error_type } })),
    )
        .into_response()
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

fn new_request_id() -> String {
    format!(
        "trefoil-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default()
    )
}

fn model_allowed(config: &crate::state::KeyConfig, model: &str) -> bool {
    !config.excluded_models.iter().any(|value| value == model)
        && (config.allowed_models.is_empty()
            || config.allowed_models.iter().any(|value| value == model))
}

fn response_with_request_id(
    status: axum::http::StatusCode,
    payload: Value,
    request_id: &str,
) -> Response {
    let mut response = (status, Json(payload)).into_response();
    if let Ok(value) = request_id.parse() {
        response.headers_mut().insert("x-request-id", value);
    }
    response
}

fn error_with_request_id(
    status: axum::http::StatusCode,
    message: &str,
    error_type: &str,
    request_id: &str,
) -> Response {
    let mut response = error_response(status, message, error_type);
    if let Ok(value) = request_id.parse() {
        response.headers_mut().insert("x-request-id", value);
    }
    response
}

#[allow(clippy::too_many_arguments)] // Keeps route call sites explicit about every accounting field.
async fn record_basic_usage(
    state: &AppState,
    request_id: &str,
    key_name: Option<&str>,
    provider: &str,
    model: &str,
    api_type: &str,
    status: &str,
    streamed: bool,
    start_time: i64,
) {
    if let Some(pool) = &state.inner.database {
        let _ = crate::database::record_request_usage(
            pool,
            request_id,
            key_name,
            Some(provider),
            Some(model),
            api_type,
            status,
            streamed,
            start_time,
        )
        .await;
        let history = serde_json::json!([{"index":1,"provider":provider,"model":model,"apiType":api_type,"status":if status == "success" { "success" } else { "failed" },"reason":if status == "success" { "Request completed successfully" } else { "Request failed" },"retryable":false}]);
        let _ = crate::database::update_request_attempts(
            pool, request_id, 1, provider, model, &history,
        )
        .await;
    }
}

#[allow(clippy::too_many_arguments)] // Extends record_basic_usage with response-derived metrics.
async fn record_response_usage(
    state: &AppState,
    request_id: &str,
    key_name: Option<&str>,
    provider: &str,
    model: &str,
    api_type: &str,
    success: bool,
    start_time: i64,
    payload: &Value,
    include_output_tokens: bool,
) {
    record_basic_usage(
        state,
        request_id,
        key_name,
        provider,
        model,
        api_type,
        if success { "success" } else { "error" },
        false,
        start_time,
    )
    .await;
    if let Some(pool) = &state.inner.database {
        let input_tokens = payload["usage"]["prompt_tokens"]
            .as_i64()
            .map(|value| value as i32);
        let output_tokens = include_output_tokens
            .then(|| payload["usage"]["completion_tokens"].as_i64())
            .flatten()
            .map(|value| value as i32)
            .or_else(|| (!include_output_tokens).then_some(0));
        let total_cost = payload["usage"]["total_cost"]
            .as_f64()
            .map(|value| value as f32);
        let _ = crate::database::update_request_usage_metrics(
            pool,
            request_id,
            input_tokens,
            output_tokens,
            total_cost,
            now_ms() - start_time,
        )
        .await;
    }
}

/// Persist one provider attempt as soon as its outcome is known. The unique
/// request/index constraint makes repeated recovery updates duplicate-safe.
async fn record_attempt(
    state: &AppState,
    request_id: &str,
    attempt_index: i32,
    provider: &str,
    model: &str,
    api_type: &str,
    status: &str,
    reason: Option<&str>,
    status_code: Option<i32>,
    retryable: bool,
    started_at: i64,
) {
    if let Some(pool) = &state.inner.database {
        let _ = crate::database::insert_inference_attempt(
            pool,
            request_id,
            attempt_index,
            provider,
            model,
            Some(api_type),
            status,
            reason,
            status_code,
            retryable,
            None,
            started_at,
            Some(now_ms()),
        )
        .await;
    }
}

fn dispatch_provider_url(value: &str) -> Result<url::Url, ()> {
    let parsed = url::Url::parse(value).map_err(|_| ())?;
    if matches!(parsed.scheme(), "http" | "https") && parsed.host_str().is_some() {
        Ok(parsed)
    } else {
        Err(())
    }
}

fn apply_native_compaction(body: &mut Value, state: &AppState) {
    let settings = state.inner.settings.read().unwrap();
    let Some(config) = settings.get("compaction").and_then(Value::as_object) else {
        return;
    };
    if config.get("enabled").and_then(Value::as_bool) != Some(true)
        || config.get("strategy").and_then(Value::as_str) != Some("native")
    {
        return;
    }
    let estimated = serde_json::to_string(body)
        .map(|s| s.len() / 4)
        .unwrap_or(0);
    let context_limit = config
        .get("absoluteTriggerTokens")
        .and_then(Value::as_u64)
        .map(|v| v as usize);
    let ratio = config
        .get("triggerRatio")
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite() && (0.0..=1.0).contains(value));
    let threshold = match (context_limit, ratio) {
        (Some(limit), Some(ratio)) => (limit as f64 * ratio).ceil() as usize,
        (Some(limit), None) => limit,
        // A ratio has no useful meaning without a context limit. Preserve the
        // historical no-op for ratio-only configurations.
        (None, _) => return,
    };
    let minimum = config.get("minTokens").and_then(Value::as_u64).unwrap_or(0) as usize;
    if estimated < threshold || estimated < minimum {
        return;
    }
    let protect_recent = config
        .get("protectRecent")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    let native = config.get("native").and_then(Value::as_object);
    let arrays = native
        .and_then(|v| v.get("maxArrayItems"))
        .and_then(Value::as_u64)
        .map(|v| v as usize);
    let strings = native
        .and_then(|v| v.get("maxStringChars"))
        .and_then(Value::as_u64)
        .map(|v| v as usize);
    if arrays.is_none() && strings.is_none() {
        return;
    }
    compact_json_value(body, arrays, strings, protect_recent);
}

fn apply_headroom_compaction(body: &mut Value, state: &AppState) {
    let settings = state.inner.settings.read().unwrap();
    let Some(config) = settings.get("compaction").and_then(Value::as_object) else {
        return;
    };
    if config.get("enabled").and_then(Value::as_bool) != Some(true)
        || config.get("strategy").and_then(Value::as_str) != Some("headroom")
    {
        return;
    }
    let Some(limit) = config.get("absoluteTriggerTokens").and_then(Value::as_u64) else {
        return;
    };
    let trigger = config
        .get("triggerRatio")
        .and_then(Value::as_f64)
        .filter(|ratio| ratio.is_finite() && (0.0..=1.0).contains(ratio))
        .unwrap_or(1.0);
    if estimate_tokens(body) <= (limit as f64 * trigger).ceil() as usize {
        return;
    }
    let target_ratio = config
        .get("headroom")
        .and_then(Value::as_object)
        .and_then(|headroom| headroom.get("targetRatio"))
        .and_then(Value::as_f64)
        .filter(|ratio| ratio.is_finite() && (0.0..=1.0).contains(ratio))
        .unwrap_or(0.8);
    let target = (limit as f64 * target_ratio).floor() as usize;
    let protect_recent = config
        .get("protectRecent")
        .and_then(Value::as_u64)
        .unwrap_or(1) as usize;
    let Some(messages) = body.get("messages").and_then(Value::as_array) else {
        return;
    };
    let protected_start = messages.len().saturating_sub(protect_recent);
    let removable: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(index, message)| {
            *index < protected_start
                && !matches!(
                    message.get("role").and_then(Value::as_str),
                    Some("system" | "developer")
                )
        })
        .map(|(index, _)| index)
        .collect();
    if removable.is_empty() {
        return;
    }
    let mut removed = vec![false; messages.len()];
    for index in removable {
        let mut candidate = body.clone();
        candidate["messages"] = Value::Array(
            messages
                .iter()
                .enumerate()
                .filter(|(position, _)| !removed[*position] && *position != index)
                .map(|(_, message)| message.clone())
                .collect(),
        );
        removed[index] = true;
        if estimate_tokens(&candidate) <= target {
            break;
        }
    }
    body["messages"] = Value::Array(
        messages
            .iter()
            .enumerate()
            .filter(|(index, _)| !removed[*index])
            .map(|(_, message)| message.clone())
            .collect(),
    );
}

fn estimate_tokens(value: &Value) -> usize {
    serde_json::to_string(value)
        .map(|json| json.len().div_ceil(4))
        .unwrap_or(usize::MAX)
}

fn compact_json_value(
    value: &mut Value,
    arrays: Option<usize>,
    strings: Option<usize>,
    protect_recent: usize,
) {
    match value {
        Value::String(text) => {
            if let Some(limit) = strings {
                *text = text.chars().take(limit).collect()
            }
        }
        Value::Array(items) => {
            if let Some(limit) = arrays {
                if limit < items.len() {
                    let keep_recent = protect_recent.min(limit);
                    let keep_old = limit - keep_recent;
                    let mut compacted = items[..keep_old].to_vec();
                    if keep_recent > 0 {
                        compacted.extend_from_slice(&items[items.len() - keep_recent..]);
                    }
                    *items = compacted;
                }
            };
            for item in items {
                compact_json_value(item, arrays, strings, protect_recent)
            }
        }
        Value::Object(object) => {
            for item in object.values_mut() {
                compact_json_value(item, arrays, strings, protect_recent)
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn is_retryable_upstream_status(
    state: &AppState,
    status: axum::http::StatusCode,
    error: Option<&str>,
) -> bool {
    let defaults = |status: axum::http::StatusCode| {
        // The default management policy retries all non-2xx statuses except
        // deterministic request failures (413 and 422).
        !status.is_success()
            && status != axum::http::StatusCode::PAYLOAD_TOO_LARGE
            && status != axum::http::StatusCode::UNPROCESSABLE_ENTITY
    };
    let settings = state.inner.settings.read().unwrap();
    let enabled = settings
        .get("failover.enabled")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    if !enabled {
        return false;
    }
    if let Some(error) = error {
        let patterns = settings
            .get("failover.retryableErrors")
            .and_then(Value::as_array)
            .map(|values| values.iter().filter_map(Value::as_str).collect::<Vec<_>>())
            .unwrap_or_else(|| vec!["ECONNREFUSED", "ETIMEDOUT", "ENOTFOUND"]);
        return patterns.iter().any(|pattern| {
            regex::RegexBuilder::new(pattern)
                .case_insensitive(true)
                .build()
                .map(|regex| regex.is_match(error))
                .unwrap_or(false)
        });
    }
    match settings
        .get("failover.retryableStatusCodes")
        .and_then(Value::as_array)
    {
        Some(values) => {
            let codes: Vec<_> = values.iter().filter_map(Value::as_i64).collect();
            if codes.is_empty() {
                defaults(status)
            } else {
                codes.contains(&i64::from(status.as_u16()))
            }
        }
        None => defaults(status),
    }
}

fn is_retryable_upstream_error(state: &AppState, error: &str) -> bool {
    is_retryable_upstream_status(state, axum::http::StatusCode::BAD_GATEWAY, Some(error))
}

#[cfg(test)]
mod retry_policy_tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn retry_policy_uses_configured_statuses_and_case_insensitive_regex_errors() {
        let state = AppState::new(None, HashMap::new());
        state.inner.settings.write().unwrap().extend([
            (
                "failover.retryableStatusCodes".into(),
                serde_json::json!([418]),
            ),
            (
                "failover.retryableErrors".into(),
                serde_json::json!(["rate\\.limit", "timeout"]),
            ),
        ]);
        assert!(is_retryable_upstream_status(
            &state,
            axum::http::StatusCode::IM_A_TEAPOT,
            None
        ));
        assert!(!is_retryable_upstream_status(
            &state,
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            None
        ));
        assert!(is_retryable_upstream_status(
            &state,
            axum::http::StatusCode::BAD_GATEWAY,
            Some("RATE.LIMIT exceeded")
        ));
        assert!(!is_retryable_upstream_status(
            &state,
            axum::http::StatusCode::BAD_GATEWAY,
            Some("invalid request")
        ));
    }

    #[test]
    fn retry_policy_defaults_and_disable_are_backward_compatible() {
        let state = AppState::new(None, HashMap::new());
        assert!(is_retryable_upstream_status(
            &state,
            axum::http::StatusCode::NOT_FOUND,
            None
        ));
        assert!(!is_retryable_upstream_status(
            &state,
            axum::http::StatusCode::PAYLOAD_TOO_LARGE,
            None
        ));
        state
            .inner
            .settings
            .write()
            .unwrap()
            .insert("failover.enabled".into(), serde_json::json!(false));
        assert!(!is_retryable_upstream_status(
            &state,
            axum::http::StatusCode::NOT_FOUND,
            Some("ECONNREFUSED")
        ));
    }
}

async fn enforce_request_quotas(state: &AppState, principal: &Principal) -> Result<(), Response> {
    let Principal::Limited { key_name, config } = principal else {
        return Ok(());
    };
    let quota_names = if config.quota_names.is_empty() {
        state
            .inner
            .settings
            .read()
            .unwrap()
            .get("default_quotas")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    } else {
        config.quota_names.clone()
    };
    let Some(pool) = &state.inner.database else {
        return Ok(());
    };
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default();
    let windows = crate::database::request_quota_windows(pool, &quota_names, now_ms)
        .await
        .map_err(|error| {
            error_response(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                &error.to_string(),
                "server_error",
            )
        })?;
    let blocked = crate::database::reserve_request_quotas(pool, key_name, &windows, now_ms)
        .await
        .map_err(|error| {
            error_response(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                &error.to_string(),
                "server_error",
            )
        })?;
    if !blocked.is_empty() {
        return Err(quota_exceeded_response(&blocked));
    }
    Ok(())
}

/// Matches Plexus's `buildQuotaExceededBody` shape exactly (only the single
/// most-constrained quota is reported here; Trefoil doesn't yet aggregate
/// multiple simultaneously-blocking quotas into `blocking_quotas`, so that
/// array always has exactly one entry -- a documented partial-parity gap).
fn quota_exceeded_response(blocked: &[(String, i32, i64, i64)]) -> Response {
    let (quota_name, limit, current_usage, resets_at_ms) = &blocked[0];
    let resets_at = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(*resets_at_ms)
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_default();
    (
        axum::http::StatusCode::TOO_MANY_REQUESTS,
        Json(serde_json::json!({
            "error": {
                "message": format!("Quota exceeded: {quota_name} limit of {limit} reached"),
                "type": "quota_exceeded",
                "quota_name": quota_name,
                "current_usage": current_usage,
                "limit": limit,
                "resets_at": resets_at,
                "blocking_quotas": blocked.iter().map(|(name, limit, usage, end)| serde_json::json!({
                    "quotaName": name, "limitType": "requests", "limit": limit,
                    "currentUsage": usage, "remaining": (*limit as i64 - *usage).max(0),
                    "resetsAt": chrono::DateTime::<chrono::Utc>::from_timestamp_millis(*end)
                        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)).unwrap_or_default(),
                })).collect::<Vec<_>>(),
            }
        })),
    )
        .into_response()
}

/// Iterate through every eligible target in configured priority order.
///
/// Alias targets are expanded depth-first at their position in a group,
/// while disabled, unknown, disallowed, and cooling-down concrete targets are
/// omitted.  Keeping this ordering in one place prevents each inference route
/// from implementing subtly different failover behavior.
struct InferenceTargetIter {
    targets: std::vec::IntoIter<(String, String)>,
}

impl Iterator for InferenceTargetIter {
    type Item = (String, String);

    fn next(&mut self) -> Option<Self::Item> {
        self.targets.next()
    }
}

fn inference_targets(
    state: &AppState,
    model: &str,
    key_config: Option<&crate::state::KeyConfig>,
) -> InferenceTargetIter {
    let providers = state.inner.providers.read().unwrap();
    let cooldowns = state.inner.cooldowns.read().unwrap();
    let aliases = state.inner.aliases.read().unwrap();
    let now = now_ms();
    let mut targets = Vec::new();
    let mut visited = std::collections::HashSet::new();

    fn expand(
        name: &str,
        aliases: &std::collections::HashMap<String, crate::state::ModelConfig>,
        providers: &std::collections::HashMap<String, crate::state::ProviderConfig>,
        cooldowns: &std::collections::HashMap<(String, String), crate::state::CooldownEntry>,
        key_config: Option<&crate::state::KeyConfig>,
        now: i64,
        visited: &mut std::collections::HashSet<String>,
        output: &mut Vec<(String, String)>,
    ) {
        if !visited.insert(name.to_owned()) {
            return;
        }
        let Some(alias) = aliases.get(name) else {
            return;
        };
        for group in &alias.target_groups {
            let mut indexes: Vec<usize> = (0..group.targets.len()).collect();
            if group.selector == "random" && indexes.len() > 1 {
                let offset = (now.unsigned_abs() as usize) % indexes.len();
                indexes.rotate_left(offset);
            }
            for index in indexes {
                let target = &group.targets[index];
                if !target.enabled {
                    continue;
                }
                if let (Some(provider), Some(target_model)) = (&target.provider, &target.model) {
                    if providers.contains_key(provider)
                        && key_config.is_none_or(|config| provider_allowed(config, provider))
                        && !cooldown_is_active(cooldowns, provider, target_model, now)
                    {
                        output.push((provider.clone(), target_model.clone()));
                    }
                } else if let Some(next) = &target.alias {
                    expand(
                        next, aliases, providers, cooldowns, key_config, now, visited, output,
                    );
                }
            }
        }
        visited.remove(name);
    }

    if let Some((provider, target_model)) = model.split_once('/') {
        if providers.contains_key(provider) {
            targets.push((provider.to_owned(), target_model.to_owned()));
        }
    } else if providers.contains_key(model) {
        targets.push((model.to_owned(), model.to_owned()));
    } else {
        expand(
            model,
            &aliases,
            &providers,
            &cooldowns,
            key_config,
            now,
            &mut visited,
            &mut targets,
        );
    }
    InferenceTargetIter {
        targets: targets.into_iter(),
    }
}

fn resolve_target(
    state: &AppState,
    model: &str,
    key_config: Option<&crate::state::KeyConfig>,
) -> (String, String) {
    inference_targets(state, model, key_config)
        .next()
        .unwrap_or_else(|| (model.to_owned(), model.to_owned()))
}

fn provider_allowed(config: &crate::state::KeyConfig, provider: &str) -> bool {
    !config
        .excluded_providers
        .iter()
        .any(|value| value == provider)
        && (config.allowed_providers.is_empty()
            || config
                .allowed_providers
                .iter()
                .any(|value| value == provider))
}

fn target_is_on_cooldown(state: &AppState, provider: &str, model: &str) -> bool {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default();
    cooldown_is_active(
        &state.inner.cooldowns.read().unwrap(),
        provider,
        model,
        now_ms,
    )
}

fn cooldown_is_active(
    cooldowns: &std::collections::HashMap<(String, String), crate::state::CooldownEntry>,
    provider: &str,
    model: &str,
    now_ms: i64,
) -> bool {
    ["", model].iter().any(|candidate_model| {
        cooldowns
            .get(&(provider.to_owned(), (*candidate_model).to_owned()))
            .is_some_and(|entry| entry.expiry_ms > now_ms)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{CooldownEntry, KeyConfig};
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use std::collections::HashMap;
    use tower::ServiceExt;

    #[test]
    fn native_compaction_applies_configured_limits_only_when_triggered() {
        let state = AppState::new(Some("admin".into()), HashMap::new());
        state.inner.settings.write().unwrap().insert(
            "compaction".into(),
            serde_json::json!({
                "enabled": true,
                "strategy": "native",
                "absoluteTriggerTokens": 1,
                "native": {"maxArrayItems": 2, "maxStringChars": 3}
            }),
        );
        let mut body = serde_json::json!({"messages":[{"content":"abcdef"}, {"content":"more"}, {"content":"last"}]});
        apply_native_compaction(&mut body, &state);
        assert_eq!(body["messages"].as_array().unwrap().len(), 2);
        assert_eq!(body["messages"][0]["content"], "abc");

        state.inner.settings.write().unwrap().insert(
            "compaction".into(),
            serde_json::json!({"enabled": false, "strategy": "native", "absoluteTriggerTokens": 1, "native": {"maxStringChars": 1}}),
        );
        let mut untouched = serde_json::json!({"messages":[{"content":"abcdef"}]});
        apply_native_compaction(&mut untouched, &state);
        assert_eq!(untouched["messages"][0]["content"], "abcdef");
    }

    #[test]
    fn native_compaction_uses_trigger_ratio_and_preserves_recent_items() {
        let state = AppState::new(Some("admin".into()), HashMap::new());
        state.inner.settings.write().unwrap().insert(
            "compaction".into(),
            serde_json::json!({
                "enabled": true,
                "strategy": "native",
                "absoluteTriggerTokens": 100,
                "triggerRatio": 0.5,
                "native": {"maxArrayItems": 3},
                "protectRecent": 1
            }),
        );

        let mut below = serde_json::json!({"messages": ["a"]});
        apply_native_compaction(&mut below, &state);
        assert_eq!(below["messages"].as_array().unwrap().len(), 1);

        let mut body = serde_json::json!({"messages": ["first", "middle", "second", "last"]});
        // Make the estimate exceed the 50-token threshold without changing
        // the array shape being asserted.
        body["padding"] = Value::String("x".repeat(220));
        apply_native_compaction(&mut body, &state);
        assert_eq!(
            body["messages"],
            serde_json::json!(["first", "middle", "last"])
        );
    }

    #[test]
    fn headroom_compaction_preserves_instructions_and_recent_history() {
        let state = AppState::new(Some("admin".into()), HashMap::new());
        state.inner.settings.write().unwrap().insert(
            "compaction".into(),
            serde_json::json!({
                "enabled": true,
                "strategy": "headroom",
                "absoluteTriggerTokens": 20,
                "headroom": {"targetRatio": 0.5},
                "protectRecent": 1
            }),
        );
        let original = serde_json::json!({"messages":[
            {"role":"system","content":"instruction"},
            {"role":"user","content":"old"},
            {"role":"assistant","content":"old answer"},
            {"role":"user","content":"latest"}
        ],"padding":"x".repeat(200)});
        let mut body = original.clone();
        apply_headroom_compaction(&mut body, &state);
        assert_eq!(body["messages"][0], original["messages"][0]);
        assert_eq!(
            body["messages"].as_array().unwrap().last().unwrap(),
            &original["messages"][3]
        );
        assert!(body["messages"].as_array().unwrap().len() < 4);
        assert_eq!(original["messages"].as_array().unwrap().len(), 4);
    }

    #[test]
    fn headroom_compaction_is_noop_without_messages_or_removable_history() {
        let state = AppState::new(Some("admin".into()), HashMap::new());
        state.inner.settings.write().unwrap().insert(
            "compaction".into(),
            serde_json::json!({"enabled":true,"strategy":"headroom","absoluteTriggerTokens":1}),
        );
        let mut malformed = serde_json::json!({"messages":"not-an-array"});
        apply_headroom_compaction(&mut malformed, &state);
        assert_eq!(malformed["messages"], "not-an-array");
        let mut instructions = serde_json::json!({"messages":[{"role":"system","content":"x"}]});
        apply_headroom_compaction(&mut instructions, &state);
        assert_eq!(instructions["messages"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn anthropic_transform_preserves_turn_order_and_tool_results() {
        let body = serde_json::json!({
            "model": "claude",
            "system": "be concise",
            "messages": [
                {"role":"user","content":"call a tool"},
                {"role":"assistant","content":[{"type":"tool_use","id":"call_1","name":"lookup","input":{"q":"rust"}}]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"call_1","content":"found"}]}
            ],
            "tools": [{"name":"lookup","description":"lookup","input_schema":{"type":"object"}}]
        });
        let chat = anthropic_to_chat(body);
        let messages = chat["messages"].as_array().unwrap();
        assert_eq!(
            messages
                .iter()
                .map(|m| m["role"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["system", "user", "assistant", "tool"]
        );
        assert_eq!(messages[3]["tool_call_id"], "call_1");
        assert_eq!(chat["tools"][0]["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn chat_transform_handles_array_text_raw_arguments_and_missing_usage() {
        let payload = serde_json::json!({"id":"x","model":"m","choices":[{"finish_reason":"tool_calls","message":{
            "content":[{"type":"text","text":"hello"}],
            "tool_calls":[{"id":"call_1","function":{"name":"lookup","arguments":{"q":"rust"}}}]
        }}]});
        let anthropic = chat_to_anthropic(payload);
        assert_eq!(
            anthropic["content"][0],
            serde_json::json!({"type":"text","text":"hello"})
        );
        assert_eq!(anthropic["content"][1]["input"]["q"], "rust");
        assert_eq!(anthropic["stop_reason"], "tool_use");
        assert_eq!(
            anthropic["usage"],
            serde_json::json!({"input_tokens":0,"output_tokens":0})
        );
    }

    #[test]
    fn anthropic_detection_does_not_match_anthropic_lookalike_hosts() {
        let provider = crate::state::ProviderConfig {
            api_base_url: "https://anthropic.com.attacker.test/v1".into(),
            ..Default::default()
        };
        assert!(!anthropic_native_provider(&provider));
    }

    #[test]
    fn normalizes_anthropic_errors_without_changing_message() {
        let value = normalize_anthropic_error(
            serde_json::json!({"type":"error","error":{"type":"invalid_request_error","message":"bad max_tokens"}}),
            axum::http::StatusCode::BAD_REQUEST,
        );
        assert_eq!(value["error"]["type"], "invalid_request_error");
        assert_eq!(value["error"]["message"], "bad max_tokens");
        assert_eq!(value["error"]["code"], 400);
    }

    #[test]
    fn normalizes_gemini_status_and_malformed_errors() {
        let rate_limit = normalize_gemini_error(
            serde_json::json!({"error":{"status":"RESOURCE_EXHAUSTED","message":"quota exceeded"}}),
            axum::http::StatusCode::TOO_MANY_REQUESTS,
        );
        assert_eq!(rate_limit["error"]["type"], "rate_limit_error");
        assert_eq!(rate_limit["error"]["code"], 429);

        let malformed = normalize_gemini_error(
            serde_json::json!({"unexpected":true}),
            axum::http::StatusCode::BAD_GATEWAY,
        );
        assert_eq!(malformed["error"]["type"], "upstream_error");
        assert_eq!(
            malformed["error"]["message"],
            "Invalid upstream error response"
        );
    }

    fn test_stream_accounting() -> StreamAccounting {
        StreamAccounting {
            pool: None,
            event_sender: None,
            request_id: "test".into(),
            key_name: None,
            provider: "gemini".into(),
            model: "test-model".into(),
            api_type: "gemini".into(),
            start_time: 0,
            input_tokens: None,
            output_tokens: None,
            cost: None,
            terminal_on_usage: true,
            stream_success: true,
            finalized: false,
        }
    }

    #[test]
    fn gemini_usage_metadata_is_terminal_and_accounted() {
        let mut accounting = test_stream_accounting();
        assert_eq!(
            accounting.observe(r#"{"candidates":[{"content":{}}]}"#),
            None
        );
        assert_eq!(
            accounting.observe(
                r#"{"usageMetadata":{"promptTokenCount":17,"candidatesTokenCount":5,"totalTokenCount":22}}"#
            ),
            Some(true)
        );
        assert_eq!(accounting.input_tokens, Some(17));
        assert_eq!(accounting.output_tokens, Some(5));
    }

    #[test]
    fn stream_accounting_finalization_is_idempotent() {
        let mut accounting = test_stream_accounting();
        accounting.finalized = true;
        assert_eq!(accounting.observe("[DONE]"), Some(true));
    }

    #[test]
    fn gemini_request_transform_maps_system_content_tools_and_generation() {
        let request = gemini_to_chat(
            serde_json::json!({
                "systemInstruction":{"parts":[{"text":"be brief"}]},
                "contents":[{"role":"user","parts":[{"text":"hi"},{"inlineData":{"mimeType":"image/png","data":"abc"}}]},
                    {"role":"model","parts":[{"functionCall":{"name":"lookup","args":{"q":"rust"}}}]}],
                "generationConfig":{"maxOutputTokens":42,"responseMimeType":"application/json"},
                "tools":[{"functionDeclarations":[{"name":"lookup","description":"find","parameters":{"type":"object"}}]}]
            }),
            "model",
        );
        assert_eq!(request["messages"][0]["role"], "system");
        assert_eq!(
            request["messages"][1]["content"][1]["image_url"]["url"],
            "data:image/png;base64,abc"
        );
        assert_eq!(
            request["messages"][2]["tool_calls"][0]["function"]["name"],
            "lookup"
        );
        assert_eq!(request["max_tokens"], 42);
        assert_eq!(request["tools"][0]["function"]["name"], "lookup");
    }

    #[test]
    fn gemini_response_transform_maps_choices_tools_finish_and_usage() {
        let response = chat_to_gemini(
            serde_json::json!({"choices":[{"finish_reason":"length","message":{"content":"done","tool_calls":[{"id":"c1","function":{"name":"lookup","arguments":"{\"q\":\"rust\"}"}}]}}],"usage":{"prompt_tokens":3,"completion_tokens":4,"total_tokens":7}}),
        );
        assert_eq!(
            response["candidates"][0]["content"]["parts"][0]["text"],
            "done"
        );
        assert_eq!(
            response["candidates"][0]["content"]["parts"][1]["functionCall"]["args"]["q"],
            "rust"
        );
        assert_eq!(response["candidates"][0]["finishReason"], "MAX_TOKENS");
        assert_eq!(response["usageMetadata"]["totalTokenCount"], 7);
    }

    #[tokio::test]
    async fn models_requires_authentication() {
        let state = AppState::new(Some("admin".into()), HashMap::new());
        let response = router()
            .with_state(state)
            .oneshot(Request::get("/v1/models").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn models_lists_aliases_only_not_raw_providers() {
        let state = AppState::new(Some("admin".into()), HashMap::new());
        state
            .inner
            .providers
            .write()
            .unwrap()
            .insert("openai".into(), Default::default());
        state
            .inner
            .aliases
            .write()
            .unwrap()
            .insert("gpt-4o-alias".into(), Default::default());
        let request = Request::get("/v1/models")
            .header("x-api-key", "admin")
            .body(Body::empty())
            .unwrap();
        let response = router().with_state(state).oneshot(request).await.unwrap();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: Value = serde_json::from_slice(&body).unwrap();
        let ids: Vec<&str> = json["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["gpt-4o-alias"]);
    }

    #[tokio::test]
    async fn embeddings_and_completions_require_authentication() {
        let state = AppState::new(Some("admin".into()), HashMap::new());
        for path in ["/v1/embeddings", "/v1/completions", "/completions"] {
            let response = router()
                .with_state(state.clone())
                .oneshot(
                    Request::post(path)
                        .header("content-type", "application/json")
                        .body(Body::from(r#"{"model":"m","input":"x","prompt":"x"}"#))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), axum::http::StatusCode::UNAUTHORIZED);
        }
    }

    #[tokio::test]
    async fn embeddings_accepts_x_api_key_and_enforces_model_policy() {
        let mut keys = HashMap::new();
        keys.insert(
            "limited".into(),
            KeyConfig {
                secret: "secret".into(),
                allowed_models: vec!["allowed".into()],
                ..Default::default()
            },
        );
        let state = AppState::new(Some("admin".into()), keys);
        let response = router()
            .with_state(state)
            .oneshot(
                Request::post("/v1/embeddings")
                    .header("x-api-key", "secret")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"model":"blocked","input":"x"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::FORBIDDEN);
        let body: Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(body["error"]["type"], "access_denied");
    }

    #[test]
    fn cooldown_matching_checks_model_specific_and_provider_wide_entries() {
        let mut cooldowns = HashMap::new();
        cooldowns.insert(
            ("openai".into(), "gpt-4o".into()),
            CooldownEntry {
                expiry_ms: 2_000,
                consecutive_failures: 1,
                last_error: None,
            },
        );
        assert!(cooldown_is_active(&cooldowns, "openai", "gpt-4o", 1_000));
        assert!(!cooldown_is_active(&cooldowns, "openai", "gpt-4o", 2_000));
        assert!(!cooldown_is_active(&cooldowns, "openai", "gpt-4.1", 1_000));

        cooldowns.insert(
            ("openai".into(), "".into()),
            CooldownEntry {
                expiry_ms: 3_000,
                consecutive_failures: 2,
                last_error: None,
            },
        );
        assert!(cooldown_is_active(&cooldowns, "openai", "gpt-4.1", 1_000));
        assert!(!cooldown_is_active(
            &cooldowns,
            "anthropic",
            "claude",
            1_000
        ));
    }

    #[test]
    fn dispatch_provider_url_requires_http_or_https_with_a_host() {
        assert!(dispatch_provider_url("https://api.example.test/v1").is_ok());
        assert!(dispatch_provider_url("http://localhost:1234/v1").is_ok());
        assert!(dispatch_provider_url("not a URL").is_err());
        assert!(dispatch_provider_url("oauth://").is_err());
        assert!(dispatch_provider_url("file:///tmp/provider").is_err());
    }

    #[test]
    fn claude_masking_requires_native_anthropic_boundary() {
        let mut provider = crate::state::ProviderConfig {
            api_base_url: "https://api.example.test".into(),
            use_claude_masking: true,
            ..Default::default()
        };
        assert!(!claude_masking_enabled(&provider));

        provider.api_base_url = "https://api.anthropic.com".into();
        assert!(claude_masking_enabled(&provider));

        provider.api_base_url = "https://api.example.test".into();
        provider.use_claude_masking = false;
        provider.oauth_provider = Some("anthropic".into());
        assert!(claude_masking_enabled(&provider));

        provider.oauth_provider = Some("openai-codex".into());
        assert!(!claude_masking_enabled(&provider));
    }

    #[test]
    fn failed_alias_target_is_replaced_by_next_eligible_target() {
        let mut aliases = HashMap::new();
        aliases.insert(
            "model".to_string(),
            crate::state::ModelConfig {
                target_groups: vec![crate::state::ModelTargetGroup {
                    name: "fallback".into(),
                    selector: "sequential".into(),
                    targets: vec![
                        crate::state::ModelTarget {
                            provider: Some("first".into()),
                            model: Some("model-a".into()),
                            alias: None,
                            enabled: true,
                        },
                        crate::state::ModelTarget {
                            provider: Some("second".into()),
                            model: Some("model-b".into()),
                            alias: None,
                            enabled: true,
                        },
                    ],
                }],
                ..Default::default()
            },
        );
        let state = AppState::new(Some("admin".into()), HashMap::new());
        *state.inner.aliases.write().unwrap() = aliases;
        state
            .inner
            .providers
            .write()
            .unwrap()
            .insert("first".into(), Default::default());
        state
            .inner
            .providers
            .write()
            .unwrap()
            .insert("second".into(), Default::default());
        state.inner.cooldowns.write().unwrap().insert(
            ("first".into(), "model-a".into()),
            crate::state::CooldownEntry {
                expiry_ms: i64::MAX,
                consecutive_failures: 1,
                last_error: None,
            },
        );
        assert_eq!(
            resolve_target(&state, "model", None),
            ("second".into(), "model-b".into())
        );
    }

    #[test]
    fn shared_dispatch_target_order_matches_direct_dispatch_for_boundary_cases() {
        let state = AppState::new(Some("admin".into()), HashMap::new());
        state.inner.providers.write().unwrap().extend([
            ("first".into(), crate::state::ProviderConfig::default()),
            ("second".into(), crate::state::ProviderConfig::default()),
        ]);
        state.inner.aliases.write().unwrap().insert(
            "model".into(),
            crate::state::ModelConfig {
                target_groups: vec![crate::state::ModelTargetGroup {
                    name: "ordered".into(),
                    selector: "sequential".into(),
                    targets: vec![
                        crate::state::ModelTarget {
                            provider: Some("first".into()),
                            model: Some("model-a".into()),
                            alias: None,
                            enabled: true,
                        },
                        crate::state::ModelTarget {
                            provider: Some("second".into()),
                            model: Some("model-b".into()),
                            alias: None,
                            enabled: true,
                        },
                    ],
                }],
                ..Default::default()
            },
        );

        let cases = [
            (
                "model",
                None,
                vec![("first", "model-a"), ("second", "model-b")],
            ),
            ("first/model-a", None, vec![("first", "model-a")]),
            ("missing", None, vec![]),
        ];
        for (model, key_config, expected) in cases {
            let shared: Vec<_> = inference_targets(&state, model, key_config)
                .map(|(provider, target_model)| (provider, target_model))
                .collect();
            assert_eq!(
                shared,
                expected
                    .into_iter()
                    .map(|(p, m)| (p.into(), m.into()))
                    .collect::<Vec<_>>()
            );
            let direct = resolve_target(&state, model, key_config);
            assert_eq!(
                direct,
                shared
                    .first()
                    .cloned()
                    .unwrap_or_else(|| (model.into(), model.into()))
            );
        }

        let limited = crate::state::KeyConfig {
            allowed_providers: vec!["second".into()],
            ..Default::default()
        };
        let shared: Vec<_> = inference_targets(&state, "model", Some(&limited)).collect();
        assert_eq!(shared, vec![("second".into(), "model-b".into())]);
        assert_eq!(resolve_target(&state, "model", Some(&limited)), shared[0]);
    }

    #[tokio::test]
    async fn messages_failover_retries_provider_not_found() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        fn mock(status: &str, body: &str) -> String {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            std::thread::spawn({
                let body = body.to_owned();
                let status = status.to_owned();
                move || loop {
                    let Ok((mut stream, _)) = listener.accept() else {
                        break;
                    };
                    let mut request = [0u8; 4096];
                    let _ = stream.read(&mut request);
                    let response = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes());
                }
            });
            format!("http://{address}/v1")
        }

        let first = mock("404 Not Found", r#"{"error":{"message":"route missing"}}"#);
        let second = mock(
            "200 OK",
            r#"{"id":"msg_1","model":"model-b","choices":[{"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1}}"#,
        );
        let mut aliases = HashMap::new();
        aliases.insert(
            "model".to_owned(),
            crate::state::ModelConfig {
                target_groups: vec![crate::state::ModelTargetGroup {
                    name: "fallback".into(),
                    selector: "sequential".into(),
                    targets: vec![
                        crate::state::ModelTarget {
                            provider: Some("first".into()),
                            model: Some("model-a".into()),
                            alias: None,
                            enabled: true,
                        },
                        crate::state::ModelTarget {
                            provider: Some("second".into()),
                            model: Some("model-b".into()),
                            alias: None,
                            enabled: true,
                        },
                    ],
                }],
                ..Default::default()
            },
        );
        let state = AppState::new(Some("admin".into()), HashMap::new());
        *state.inner.aliases.write().unwrap() = aliases;
        state.inner.providers.write().unwrap().insert(
            "first".into(),
            crate::state::ProviderConfig {
                api_base_url: first,
                ..Default::default()
            },
        );
        state.inner.providers.write().unwrap().insert(
            "second".into(),
            crate::state::ProviderConfig {
                api_base_url: second,
                ..Default::default()
            },
        );

        let response = router().with_state(state).oneshot(
            Request::post("/v1/messages")
                .header("x-api-key", "admin")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"model":"model","max_tokens":8,"messages":[{"role":"user","content":"hi"}]}"#))
                .unwrap(),
        ).await.unwrap();
        let status = response.status();
        let body: Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(
            status,
            axum::http::StatusCode::OK,
            "status={status:?} body={body}"
        );
        assert_eq!(body["content"][0]["text"], "ok");
    }

    fn inference_mock(status: &str, content_type: &str, body: &str) -> String {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let status = status.to_owned();
        let content_type = content_type.to_owned();
        let body = body.to_owned();
        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut request = [0u8; 16 * 1024];
            let _ = stream.read(&mut request);
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
        });
        format!("http://{address}/v1")
    }

    fn inference_mock_with_capture(
        status: &str,
        content_type: &str,
        response_body: &str,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Option<Value>>>) {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let captured = std::sync::Arc::new(std::sync::Mutex::new(None));
        let captured_thread = captured.clone();
        let status = status.to_owned();
        let content_type = content_type.to_owned();
        let response_body = response_body.to_owned();
        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut request = Vec::new();
            let mut chunk = [0u8; 4096];
            let body_start;
            loop {
                let Ok(size) = stream.read(&mut chunk) else {
                    return;
                };
                if size == 0 {
                    return;
                }
                request.extend_from_slice(&chunk[..size]);
                if let Some(index) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                    body_start = index + 4;
                    break;
                }
            }
            let headers = String::from_utf8_lossy(&request[..body_start]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let lower = line.to_ascii_lowercase();
                    lower
                        .strip_prefix("content-length:")
                        .and_then(|value| value.trim().parse::<usize>().ok())
                })
                .unwrap_or(0);
            while request.len() < body_start + content_length {
                let Ok(size) = stream.read(&mut chunk) else {
                    return;
                };
                if size == 0 {
                    return;
                }
                request.extend_from_slice(&chunk[..size]);
            }
            if let Ok(value) =
                serde_json::from_slice(&request[body_start..body_start + content_length])
            {
                *captured_thread.lock().unwrap() = Some(value);
            }
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
                response_body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
        });
        (format!("http://{address}/v1"), captured)
    }

    fn inference_mock_with_headers(
        status: &str,
        content_type: &str,
        response_body: &str,
    ) -> (
        String,
        std::sync::Arc<std::sync::Mutex<Option<(String, Value)>>>,
    ) {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let captured = std::sync::Arc::new(std::sync::Mutex::new(None));
        let captured_thread = captured.clone();
        let status = status.to_owned();
        let content_type = content_type.to_owned();
        let response_body = response_body.to_owned();
        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut request = Vec::new();
            let mut chunk = [0u8; 4096];
            let body_start;
            loop {
                let Ok(size) = stream.read(&mut chunk) else {
                    return;
                };
                if size == 0 {
                    return;
                }
                request.extend_from_slice(&chunk[..size]);
                if let Some(index) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                    body_start = index + 4;
                    break;
                }
            }
            let headers = String::from_utf8_lossy(&request[..body_start]).into_owned();
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let lower = line.to_ascii_lowercase();
                    lower
                        .strip_prefix("content-length:")?
                        .trim()
                        .parse::<usize>()
                        .ok()
                })
                .unwrap_or(0);
            while request.len() < body_start + content_length {
                let Ok(size) = stream.read(&mut chunk) else {
                    return;
                };
                if size == 0 {
                    return;
                }
                request.extend_from_slice(&chunk[..size]);
            }
            let body: Value =
                serde_json::from_slice(&request[body_start..body_start + content_length]).unwrap();
            *captured_thread.lock().unwrap() = Some((headers, body));
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
                response_body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
        });
        (format!("http://{address}/v1"), captured)
    }

    fn provider_state(api_base_url: String) -> AppState {
        let state = AppState::new(Some("admin".into()), HashMap::new());
        state.inner.providers.write().unwrap().insert(
            "mock".into(),
            crate::state::ProviderConfig {
                api_base_url,
                ..Default::default()
            },
        );
        state
    }

    #[tokio::test]
    async fn chat_completions_forwards_to_provider_and_returns_json_contract() {
        let state = provider_state(inference_mock(
            "200 OK",
            "application/json",
            r#"{"id":"chat-1","model":"mock-model","choices":[{"index":0,"message":{"role":"assistant","content":"hello"},"finish_reason":"stop"}],"usage":{"prompt_tokens":2,"completion_tokens":1}}"#,
        ));
        let response = router().with_state(state).oneshot(
            Request::post("/v1/chat/completions")
                .header("x-api-key", "admin")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"model":"mock/mock-model","messages":[{"role":"user","content":"hi"}]}"#))
                .unwrap(),
        ).await.unwrap();

        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let body: Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(body["choices"][0]["message"]["content"], "hello");
        assert_eq!(body["usage"]["completion_tokens"], 1);
    }

    #[tokio::test]
    async fn chat_request_boundary_applies_native_compaction_before_forwarding() {
        let (base_url, captured) = inference_mock_with_capture(
            "200 OK",
            "application/json",
            r#"{"id":"chat-1","choices":[{"message":{"role":"assistant","content":"ok"}}]}"#,
        );
        let state = provider_state(base_url);
        state.inner.settings.write().unwrap().insert(
            "compaction".into(),
            serde_json::json!({"enabled":true,"strategy":"native","absoluteTriggerTokens":1,"native":{"maxStringChars":3}}),
        );
        let response = router().with_state(state).oneshot(
            Request::post("/v1/chat/completions")
                .header("x-api-key", "admin")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"model":"mock/mock-model","messages":[{"role":"user","content":"abcdef"}]}"#))
                .unwrap(),
        ).await.unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let forwarded = captured
            .lock()
            .unwrap()
            .clone()
            .expect("provider request captured");
        assert_eq!(forwarded["messages"][0]["content"], "abc");
    }

    #[tokio::test]
    async fn oauth_anthropic_masks_request_at_http_boundary() {
        let (base_url, captured) = inference_mock_with_headers(
            "200 OK",
            "application/json",
            r#"{"id":"chat-1","choices":[{"message":{"role":"assistant","content":"ok"}}]}"#,
        );
        let state = provider_state(base_url);
        state
            .inner
            .providers
            .write()
            .unwrap()
            .get_mut("mock")
            .unwrap()
            .oauth_provider = Some("anthropic".into());
        state
            .inner
            .providers
            .write()
            .unwrap()
            .get_mut("mock")
            .unwrap()
            .api_key = Some("oauth-secret".into());
        let response = router().with_state(state).oneshot(
            Request::post("/v1/messages")
                .header("x-api-key", "admin")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"model":"mock/model","max_tokens":8,"system":[{"type":"text","text":"private instruction"}],"messages":[{"role":"user","content":"hello"}]}"#))
                .unwrap(),
        ).await.unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let (headers, body) = captured
            .lock()
            .unwrap()
            .clone()
            .expect("provider request captured");
        assert!(headers.contains("user-agent: claude-cli/2.1.258 (external, cli)"));
        assert!(headers.contains("x-app: cli"));
        assert!(
            headers
                .to_ascii_lowercase()
                .contains("x-api-key: oauth-secret"),
            "{headers}"
        );
        assert_eq!(
            body["system"][1]["text"],
            "You are Claude Code, Anthropic's official CLI for Claude."
        );
        assert!(!body.to_string().contains("private instruction"));
    }

    #[tokio::test]
    async fn oauth_codex_forwards_provider_request_identity_and_payload() {
        let (base_url, captured) = inference_mock_with_headers(
            "200 OK",
            "application/json",
            r#"{"id":"chat-1","choices":[{"message":{"role":"assistant","content":"ok"}}]}"#,
        );
        let state = provider_state(base_url);
        let mut providers = state.inner.providers.write().unwrap();
        let provider = providers.get_mut("mock").unwrap();
        provider.oauth_provider = Some("openai-codex".into());
        provider.api_key = Some("oauth-token".into());
        drop(providers);
        let response = router().with_state(state).oneshot(
            Request::post("/v1/messages")
                .header("x-api-key", "admin")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"model":"mock/model","max_tokens":8,"messages":[{"role":"user","content":"hello"}]}"#))
                .unwrap(),
        ).await.unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let (headers, body) = captured
            .lock()
            .unwrap()
            .clone()
            .expect("provider request captured");
        assert!(headers
            .to_ascii_lowercase()
            .contains("authorization: bearer oauth-token"));
        assert!(
            headers
                .to_ascii_lowercase()
                .contains("originator: codex_cli_rs"),
            "{headers}"
        );
        assert!(
            headers
                .to_ascii_lowercase()
                .contains("user-agent: codex_cli_rs/0.125.0"),
            "{headers}"
        );
        assert_eq!(body["model"], "model");
        assert_eq!(body["messages"][0]["content"], "hello");
    }

    #[tokio::test]
    async fn chat_request_boundary_preserves_multimodal_vision_parts() {
        let (base_url, captured) = inference_mock_with_capture(
            "200 OK",
            "application/json",
            r#"{"id":"chat-1","choices":[{"message":{"role":"assistant","content":"ok"}}]}"#,
        );
        let state = provider_state(base_url);
        let response = router().with_state(state).oneshot(
            Request::post("/v1/chat/completions")
                .header("x-api-key", "admin")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"model":"mock/mock-model","messages":[{"role":"user","content":[{"type":"text","text":"describe"},{"type":"image_url","image_url":{"url":"https://example.test/cat.png"}}]}]}"#))
                .unwrap(),
        ).await.unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let forwarded = captured
            .lock()
            .unwrap()
            .clone()
            .expect("provider request captured");
        assert_eq!(forwarded["messages"][0]["content"][1]["type"], "image_url");
        assert_eq!(
            forwarded["messages"][0]["content"][1]["image_url"]["url"],
            "https://example.test/cat.png"
        );
    }

    #[tokio::test]
    async fn chat_completions_stream_preserves_sse_chunks_and_done_marker() {
        let state = provider_state(inference_mock(
            "200 OK",
            "text/event-stream",
            r#"data: {"id":"chat-1","choices":[{"delta":{"content":"hel"}}]}

data: {"choices":[{"delta":{"content":"lo"}}]}

data: [DONE]

"#,
        ));
        let response = router().with_state(state).oneshot(
            Request::post("/v1/chat/completions")
                .header("x-api-key", "admin")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"model":"mock/mock-model","stream":true,"messages":[{"role":"user","content":"hi"}]}"#))
                .unwrap(),
        ).await.unwrap();

        assert_eq!(response.status(), axum::http::StatusCode::OK);
        assert_eq!(response.headers()["content-type"], "text/event-stream");
        let body = String::from_utf8(
            response
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .to_vec(),
        )
        .unwrap();
        assert!(body.contains("\"content\":\"hel\""));
        assert!(body.contains("data: [DONE]"));
    }

    #[tokio::test]
    async fn embeddings_and_completions_forward_successful_provider_payloads() {
        for (path, request_body, expected) in [
            (
                "/v1/embeddings",
                r#"{"model":"mock/mock-model","input":"hi"}"#,
                "embedding",
            ),
            (
                "/v1/completions",
                r#"{"model":"mock/mock-model","prompt":"hi"}"#,
                "completion",
            ),
        ] {
            let payload = format!(r#"{{"object":"{expected}","model":"mock-model","choices":[]}}"#);
            let state = provider_state(inference_mock("200 OK", "application/json", &payload));
            let response = router()
                .with_state(state)
                .oneshot(
                    Request::post(path)
                        .header("x-api-key", "admin")
                        .header("content-type", "application/json")
                        .body(Body::from(request_body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), axum::http::StatusCode::OK);
            let body: Value =
                serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                    .unwrap();
            assert_eq!(body["object"], expected);
        }
    }

    #[tokio::test]
    async fn remaining_json_inference_families_forward_to_mock_provider() {
        let cases = [
            (
                "/v1/responses",
                r#"{"model":"mock/mock-model","input":"hi"}"#,
                r#"{"id":"resp-1","object":"response","status":"completed","output":[]}"#,
                "response",
            ),
            (
                "/v1/messages",
                r#"{"model":"mock/mock-model","max_tokens":8,"messages":[{"role":"user","content":"hi"}]}"#,
                r#"{"id":"msg-1","model":"mock-model","choices":[{"message":{"role":"assistant","content":"hello"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1}}"#,
                "message",
            ),
            (
                "/v1/images/generations",
                r#"{"model":"mock/mock-model","prompt":"a tree"}"#,
                r#"{"created":1,"data":[{"url":"https://example.test/tree.png"}]}"#,
                "image",
            ),
            (
                "/v1/images/edits",
                r#"{"model":"mock/mock-model","prompt":"edit"}"#,
                r#"{"created":1,"data":[{"url":"https://example.test/edited.png"}]}"#,
                "image",
            ),
            (
                "/v1/audio/speech",
                r#"{"model":"mock/mock-model","input":"hello"}"#,
                r#"{"audio":"base64-audio"}"#,
                "audio",
            ),
        ];

        for (path, request_body, upstream_body, expected_kind) in cases {
            let state = provider_state(inference_mock("200 OK", "application/json", upstream_body));
            let response = router()
                .with_state(state)
                .oneshot(
                    Request::post(path)
                        .header("x-api-key", "admin")
                        .header("content-type", "application/json")
                        .body(Body::from(request_body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                axum::http::StatusCode::OK,
                "{expected_kind}"
            );
            let body: Value =
                serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                    .unwrap();
            if path == "/v1/messages" {
                assert_eq!(body["content"][0]["text"], "hello");
                assert_eq!(body["usage"]["output_tokens"], 1);
            } else {
                assert_eq!(body, serde_json::from_str::<Value>(upstream_body).unwrap());
            }
        }
    }
}
