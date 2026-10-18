//! `services/dispatch/request-payload-builder.ts` (API-key routes) plus the
//! per-attempt header / URL builders the dispatcher pairs with it
//! (`Dispatcher.setupHeaders` → `providers/provider-request-headers.ts`
//! `setupProviderHeaders` + header cache-key injection, and
//! `Dispatcher.buildRequestUrl`).
//!
//! OAuth-style routes (`oauth://` providers and the Claude-masking API-key
//! route) return an [`OAuthPlan`]; the async native/generic preparation
//! (token resolution, masking, stash) runs in `engine::dispatcher::oauth`.
use super::adapter_resolver::is_anthropic_target_provider;
use super::adapters::ResolvedAdapter;
use super::auto_compat::{apply_registry_auto_compat, strip_lite_unsupported_tools};
use super::cache_key_injection::{apply_body_cache_key_injection, apply_header_cache_key_injection};
use super::eager_tool_streaming::apply_eager_tool_input_streaming;
use super::model_behaviors::apply_model_behaviors;
use super::oauth_routes::{is_claude_masking_api_key_route, is_native_oauth_route, is_pi_ai_route};
use super::responses_extensions::{detect_responses_extensions, has_unsupported_responses_extensions};
use super::service_tier_selection::apply_service_tier_selection;
use super::util::{base_type, find_key_ci, obj_of, Headers};
use crate::engine::anthropic::thinking_clamp::clamp_anthropic_effort_and_thinking;
use crate::engine::config::{oauth_provider_of, Config};
use crate::engine::gemini::utils::append_user_after_text_only_model_tail;
use crate::engine::js::*;
use crate::engine::routing::provider_api_selection::{apply_gemini_thinking_config, get_api_metadata, resolve_provider_base_url};
use crate::engine::routing::RouteResult;
use crate::engine::transformer::{EngineError, EngineResult, Transformer};
use serde_json::{json, Value};

pub const FAST_MODE_BETA: &str = "fast-mode-2026-02-01";

/// `RequestPayload`.
#[derive(Debug, Clone, PartialEq)]
pub struct RequestPayload {
    pub payload: Value,
    pub bypass_transformation: bool,
    /// Native / generic OAuth preparation still to run (`prepareNativeOAuthDispatch`
    /// / `prepareGenericOAuthDispatch`); its body replaces `payload`.
    pub oauth: Option<OAuthPlan>,
}

/// What the OAuth tail of `buildRequestPayload` must prepare.
#[derive(Debug, Clone, PartialEq)]
pub struct OAuthPlan {
    /// Native provider (Anthropic / Codex / Copilot / Meta) vs generic pi-ai OAuth.
    pub native: bool,
    pub provider: String,
    pub masking_route: bool,
    pub codex_passthrough: bool,
    pub claude_passthrough: bool,
}

/// `applyAutoAnthropicBetas(headers, payload, targetApiType)`: appends the
/// fast-mode beta for Messages bodies with `speed: "fast"`, merging into an
/// existing `anthropic-beta` header of any casing.
pub fn apply_auto_anthropic_betas(headers: &mut Headers, payload: &Value, target_api_type: &str) {
    if base_type(target_api_type) != "messages" { return; }
    if get_str(payload, &["speed"]) != Some("fast") { return; }
    let existing = find_key_ci(headers, "anthropic-beta").cloned();
    let current = existing.as_ref().and_then(|k| headers.get(k)).map(|v| if v.is_null() { String::new() } else { to_string(Some(v)) }).unwrap_or_default();
    let mut betas: Vec<String> = current.split(',').map(|b| b.trim().to_owned()).filter(|b| !b.is_empty()).collect();
    if !betas.iter().any(|b| b == FAST_MODE_BETA) { betas.push(FAST_MODE_BETA.into()); }
    headers.insert(existing.unwrap_or_else(|| "anthropic-beta".into()), json!(betas.join(",")));
}

/// `shouldUsePassThrough(request, targetApiType, route)`.
pub fn should_use_pass_through(request: &Value, target_api_type: &str, route: &RouteResult) -> bool {
    let native = is_native_oauth_route(route, target_api_type);
    if truthy(request.get("_hasVisionFallthrough")) || (is_pi_ai_route(route, target_api_type) && !native) { return false; }
    if has_unsupported_responses_extensions(request.get("originalBody"), route, target_api_type) { return false; }
    let incoming = get_str(request, &["incomingApiType"]).map(str::to_lowercase).unwrap_or_default();
    !incoming.is_empty() && incoming == target_api_type.to_lowercase() && truthy(request.get("originalBody"))
}

fn gemini_tail(payload: &mut Value, target_api_type: &str) {
    if base_type(target_api_type) != "gemini" { return; }
    let Some(contents) = payload.get("contents").and_then(Value::as_array) else { return };
    if let Some(updated) = append_user_after_text_only_model_tail(contents) { payload["contents"] = Value::Array(updated); }
}

fn spread_into(payload: Value, extra: Option<&Value>) -> Value {
    match extra.filter(|e| truthy(Some(e))) { Some(e) => spread(&payload, e), None => payload }
}

/// `buildRequestPayload(request, route, transformer, targetApiType, adapters)`
/// for API-key routes. `config` is the `getConfig()` snapshot (alias
/// `extraBody` / `advanced`).
pub fn build_request_payload(
    request: &Value,
    route: &RouteResult,
    transformer: &dyn Transformer,
    target_api_type: &str,
    adapters: &[ResolvedAdapter],
    config: &Config,
) -> EngineResult<RequestPayload> {
    let native_oauth = is_native_oauth_route(route, target_api_type);
    let masking_route = is_claude_masking_api_key_route(route, target_api_type);
    let generic_oauth = !native_oauth && !masking_route && is_pi_ai_route(route, target_api_type);
    let oauth_provider_native = if masking_route { "anthropic".to_owned() } else { oauth_provider_of(&route.provider, &route.config).to_owned() };
    let codex_native = native_oauth && oauth_provider_native == "openai-codex";
    let copilot_native = native_oauth && oauth_provider_native == "github-copilot";
    let muse_native = native_oauth && oauth_provider_native == "meta";
    let codex_cli_passthrough = codex_native
        && crate::engine::dispatcher::oauth::native::is_codex_cli_shaped_body(request.get("originalBody"))
        && !has_unsupported_responses_extensions(request.get("originalBody"), route, target_api_type);
    let anthropic_native = native_oauth && oauth_provider_native == "anthropic";
    let incoming_base = base_type(&get_str(request, &["incomingApiType"]).map(str::to_lowercase).unwrap_or_default());

    let bypass = if codex_native { codex_cli_passthrough } else { should_use_pass_through(request, target_api_type, route) };
    let mut payload = if bypass {
        let mut p = request.get("originalBody").cloned().unwrap_or(Value::Null);
        if let Some(m) = p.as_object_mut() { m.insert("model".into(), json!(route.model)); }
        gemini_tail(&mut p, target_api_type);
        if let Some(meta) = request.get("metadata").filter(|m| truthy(Some(m))) {
            let api_meta = get_api_metadata(meta);
            if api_meta.as_object().is_some_and(|o| !o.is_empty()) {
                if let Some(m) = p.as_object_mut() { m.insert("metadata".into(), api_meta); }
            }
        }
        p
    } else {
        let oauth_provider = if masking_route { "anthropic".to_owned() } else { oauth_provider_of(&route.provider, &route.config).to_owned() };
        let with_provider = if oauth_provider.is_empty() {
            request.clone()
        } else {
            let mut metadata = obj_of(request.get("metadata").filter(|m| truthy(Some(m))));
            let mut plexus = obj_of(metadata.get("plexus_metadata").filter(|m| truthy(Some(m))));
            plexus.insert("oauthProvider".into(), json!(oauth_provider));
            metadata.insert("plexus_metadata".into(), Value::Object(plexus));
            let mut r = obj_of(Some(request));
            r.insert("metadata".into(), Value::Object(metadata));
            Value::Object(r)
        };
        transformer.transform_request(&with_provider)?
    };

    let claude_passthrough = anthropic_native && bypass && is_anthropic_target_provider(route, Some(target_api_type))
        && crate::engine::dispatcher::oauth::native::is_genuine_claude_code_request(request);

    gemini_tail(&mut payload, target_api_type);
    payload = apply_service_tier_selection(payload, request, target_api_type);
    payload = apply_gemini_thinking_config(route, target_api_type, payload)?;
    payload = apply_registry_auto_compat(payload, request, route, target_api_type);
    payload = apply_eager_tool_input_streaming(payload, request, route, target_api_type, bypass);
    payload = apply_body_cache_key_injection(payload, route, request, target_api_type);

    payload = spread_into(payload, route.config.get("extraBody"));
    payload = spread_into(payload, route.model_config.as_ref().and_then(|m| m.get("extraBody")));
    if let Some(canonical) = route.canonical_model.as_deref().filter(|c| !c.is_empty()) {
        if let Some(alias) = config.alias(canonical) {
            payload = spread_into(payload, alias.get("extraBody"));
            if let Some(advanced) = alias.get("advanced").filter(|a| truthy(Some(a))) {
                let incoming = get_str(request, &["incomingApiType"]).unwrap_or("");
                payload = apply_model_behaviors(payload, Some(advanced), incoming, canonical);
            }
        }
    }

    for entry in adapters { payload = entry.adapter.pre_dispatch(payload, entry.options.as_ref()); }

    if is_anthropic_target_provider(route, Some(target_api_type)) || base_type(target_api_type) == "messages" {
        let outbound = get_str(&payload, &["model"]).map(str::to_owned).unwrap_or_else(|| route.model.clone());
        payload = clamp_anthropic_effort_and_thinking(payload, Some(&outbound));
    }

    if !native_oauth && target_api_type.to_lowercase() == "responses:lite" {
        let mut m = obj_of(Some(&payload));
        if let Some(reasoning) = m.get("reasoning").filter(|r| truthy(Some(r))).cloned() {
            let mut r = obj_of(Some(&reasoning));
            let context = coalesce(reasoning.get("context"), None).unwrap_or(json!("all_turns"));
            r.insert("context".into(), context);
            m.insert("reasoning".into(), Value::Object(r));
        }
        m.insert("parallel_tool_calls".into(), json!(false));
        payload = strip_lite_unsupported_tools(Value::Object(m)).0;
    }

    if native_oauth {
        let incoming_is_responses = incoming_base == "responses";
        let native_bypass = if codex_native {
            codex_cli_passthrough || (incoming_is_responses && request.get("originalBody").is_none_or(|b| detect_responses_extensions(b).is_empty()))
        } else if copilot_native || muse_native { bypass } else { incoming_base == "messages" };
        let plan = OAuthPlan { native: true, provider: oauth_provider_native, masking_route, codex_passthrough: codex_cli_passthrough, claude_passthrough };
        return Ok(RequestPayload { payload, bypass_transformation: native_bypass, oauth: Some(plan) });
    }
    if generic_oauth {
        let plan = OAuthPlan { native: false, provider: oauth_provider_of(&route.provider, &route.config).to_owned(), masking_route: false, codex_passthrough: false, claude_passthrough: false };
        return Ok(RequestPayload { payload, bypass_transformation: bypass, oauth: Some(plan) });
    }
    Ok(RequestPayload { payload, bypass_transformation: bypass, oauth: None })
}

/// `setupProviderHeaders(route, apiType, request)`.
pub fn setup_provider_headers(route: &RouteResult, api_type: &str, request: &Value) -> EngineResult<Headers> {
    let mut h = Headers::new();
    h.insert("Content-Type".into(), json!("application/json"));
    h.insert("Accept".into(), json!(if truthy(request.get("stream")) { "text/event-stream" } else { "application/json" }));
    let key = route.config.get("api_key").filter(|k| truthy(Some(k)));
    let Some(key) = key else {
        return Err(EngineError::new(format!("No API key configured for provider '{}'", route.provider)));
    };
    let key = to_string(Some(key));
    match base_type(api_type).as_str() {
        "messages" => { h.insert("x-api-key".into(), json!(key)); h.insert("anthropic-version".into(), json!("2023-06-01")); }
        "gemini" => { h.insert("x-goog-api-key".into(), json!(key)); }
        _ => { h.insert("Authorization".into(), json!(format!("Bearer {key}"))); }
    }
    if let Some(Value::Object(extra)) = route.config.get("headers") {
        for (k, v) in extra { h.insert(k.clone(), v.clone()); }
    }
    if let Some(crh) = request.get("cacheRoutingHeaders").filter(|c| truthy(Some(c))) {
        let field = |k: &str| crh.get(k).filter(|v| truthy(Some(v)));
        if let Some(s) = field("session_id") { h.insert("session-id".into(), s.clone()); }
        let opencode = field("x-opencode-session").or_else(|| field("session_id"));
        let auto_compat = route.config.get("auto_compat") == Some(&Value::Bool(true))
            || route.model_config.as_ref().and_then(|m| m.get("auto_compat")) == Some(&Value::Bool(true));
        if let Some(s) = opencode {
            if get_str(&route.config, &["pi_ai_provider"]) == Some("opencode-go") && auto_compat { h.insert("x-opencode-session".into(), s.clone()); }
        }
        for k in ["x-client-request-id", "x-session-affinity", "x-session-id", "x-prompt-cache-isolation-key", "x-multi-turn-session-id"] {
            if let Some(v) = field(k) { h.insert(k.into(), v.clone()); }
        }
    }
    if base_type(api_type) == "messages" {
        if let Some(beta) = request.get("anthropicBeta").filter(|b| truthy(Some(b))) { h.insert("anthropic-beta".into(), beta.clone()); }
    }
    if api_type.to_lowercase() == "responses:lite" { h.insert("x-openai-internal-codex-responses-lite".into(), json!("true")); }
    Ok(h)
}

/// `Dispatcher.setupHeaders(route, apiType, request)` for API-key routes.
pub fn setup_headers(route: &RouteResult, api_type: &str, request: &Value) -> EngineResult<Headers> {
    Ok(apply_header_cache_key_injection(setup_provider_headers(route, api_type, request)?, route, request))
}

/// `Dispatcher.buildRequestUrl(route, transformer, request, targetApiType)`:
/// resolved base URL + the transformer's endpoint for this request (e.g.
/// Gemini's `/models/{model}:streamGenerateContent?alt=sse`).
pub fn build_request_url(route: &RouteResult, transformer: &dyn Transformer, request: &Value, target_api_type: &str) -> EngineResult<String> {
    Ok(format!("{}{}", resolve_provider_base_url(route, target_api_type)?, transformer.endpoint(request)))
}
