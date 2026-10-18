//! `services/oauth/oauth-native-request.ts`: wire preparation for native
//! OAuth providers (Anthropic incl. the Claude-masking API-key route, Codex,
//! GitHub Copilot, Meta) and the generic pi-ai OAuth providers.
use super::auth;
use super::masking;
use crate::engine::anthropic::thinking_clamp::clamp_anthropic_effort_and_thinking;
use crate::engine::dispatch::adapters::strip_unsupported_gpt5_options;
use crate::engine::dispatch::responses_extensions::detect_responses_extensions;
use crate::engine::js::*;
use crate::state::AppState;
use base64::Engine as _;
use serde_json::{json, Value};

/// `PreparedOAuthRequest`; `reverse_pairs` drive `reverseResponseFrame`
/// (identity when empty).
#[derive(Debug, Clone, Default)]
pub struct PreparedOAuthRequest {
    pub url: String,
    pub headers: Vec<(String, Value)>,
    pub body: Value,
    pub reverse_pairs: Vec<(String, String)>,
}

impl PreparedOAuthRequest {
    /// `reverseResponseFrame(frame)`.
    pub fn reverse_frame(&self, frame: &str) -> String {
        if self.reverse_pairs.is_empty() { frame.to_owned() } else { masking::reverse_tool_renames(frame, &self.reverse_pairs) }
    }
    pub fn set_header(&mut self, name: &str, value: Option<String>) {
        let pos = self.headers.iter().position(|(k, _)| k == name);
        match (pos, value) {
            (Some(i), Some(v)) => self.headers[i].1 = json!(v),
            (None, Some(v)) => self.headers.push((name.to_owned(), json!(v))),
            (Some(i), None) => { self.headers.remove(i); }
            (None, None) => {}
        }
    }
}

fn h(pairs: &[(&str, String)]) -> Vec<(String, Value)> {
    pairs.iter().map(|(k, v)| ((*k).to_owned(), json!(v))).collect()
}

fn accept(streaming: bool) -> String { (if streaming { "text/event-stream" } else { "application/json" }).into() }

fn base_url_of(model: &Value) -> Option<String> {
    model.get("baseUrl").filter(|v| truthy(Some(v))).map(|v| to_string(Some(v)))
}

/// `OAUTH_PROVIDER_BASE_URLS`.
fn provider_default_base_url(provider: &str) -> Option<&'static str> {
    match provider {
        "anthropic" => Some("https://api.anthropic.com"),
        "openai-codex" => Some("https://chatgpt.com/backend-api"),
        "meta" => Some("https://api.meta.ai/v1"),
        _ => None,
    }
}

fn catalog_model(provider: &str, model: &str) -> Option<Value> { crate::pi_catalog::catalog().get_model(provider, model) }

/// `resolveOAuthBaseUrl(provider, modelId)`.
fn resolve_oauth_base_url(provider: &str, model_id: &str) -> Result<String, String> {
    let base = catalog_model(provider, model_id).as_ref().and_then(base_url_of).or_else(|| provider_default_base_url(provider).map(str::to_owned))
        .ok_or_else(|| format!("OAuth: no baseUrl for provider '{provider}' model '{model_id}'. Cannot resolve upstream endpoint."))?;
    Ok(base.strip_suffix('/').map(str::to_owned).unwrap_or(base))
}

/// `PIAI_API_TO_PLEXUS`.
fn piai_api_to_plexus(api: &str) -> Option<&'static str> {
    match api { "anthropic-messages" => Some("messages"), "openai-completions" => Some("chat"), "openai-responses" => Some("responses"), _ => None }
}

/// `nativeOAuthApiType(provider, modelId)`.
pub fn native_oauth_api_type(provider: &str, model_id: &str) -> Option<String> {
    match provider {
        "github-copilot" => Some(copilot_wire_api_type(model_id)),
        "anthropic" => Some("messages".into()),
        "openai-codex" | "meta" => Some("responses".into()),
        _ => None,
    }
}

/// `copilotWireApiType(modelId)`.
pub fn copilot_wire_api_type(model_id: &str) -> String {
    if !model_id.is_empty() {
        if let Some(t) = catalog_model("github-copilot", model_id).as_ref().and_then(|m| get_str(m, &["api"])).and_then(piai_api_to_plexus) { return t.into(); }
    }
    "chat".into()
}

/// `genericOAuthProviderProfile(provider)` → `(apiType, baseUrl)`.
fn generic_profile(provider: &str) -> (Option<String>, Option<String>) {
    let models = crate::pi_catalog::catalog().get_models(Some(provider));
    if models.is_empty() { return (None, None); }
    let mut api_types: Vec<&'static str> = Vec::new();
    let mut base_urls: Vec<String> = Vec::new();
    let mut unsupported = false;
    for m in &models {
        match get_str(m, &["api"]).and_then(piai_api_to_plexus) {
            Some(t) => if !api_types.contains(&t) { api_types.push(t) },
            None => unsupported = true,
        }
        if let Some(b) = base_url_of(m) {
            let b = b.trim_end_matches('/').to_owned();
            if !base_urls.contains(&b) { base_urls.push(b); }
        }
    }
    (
        (!unsupported && api_types.len() == 1).then(|| api_types[0].to_owned()),
        (base_urls.len() == 1).then(|| base_urls[0].clone()),
    )
}

/// `genericOAuthApiType(provider, modelId)`.
pub fn generic_oauth_api_type(provider: &str, model_id: &str) -> Option<String> {
    match catalog_model(provider, model_id) {
        Some(m) => get_str(&m, &["api"]).and_then(piai_api_to_plexus).map(str::to_owned),
        None => generic_profile(provider).0,
    }
}

fn resolve_generic_oauth_base_url(provider: &str, model_id: &str) -> Result<String, String> {
    let base = catalog_model(provider, model_id).as_ref().and_then(base_url_of)
        .or_else(|| provider_default_base_url(provider).map(str::to_owned))
        .or_else(|| generic_profile(provider).1)
        .ok_or_else(|| format!("OAuth: no baseUrl for provider '{provider}' model '{model_id}'. Cannot resolve upstream endpoint."))?;
    Ok(base.trim_end_matches('/').to_owned())
}

// ─── Claude genuine-client detection ───────────────────────────────────────

fn claude_cli_user_agent(ua: &str) -> bool {
    let Some(rest) = ua.strip_prefix("claude-cli/") else { return false };
    let Some(version) = rest.strip_suffix(" (external, cli)") else { return false };
    let parts: Vec<&str> = version.split('.').collect();
    parts.len() == 3 && parts.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
}

fn parse_claude_code_user_id(raw: Option<&Value>) -> Option<(String, String)> {
    let raw = raw.and_then(Value::as_str).filter(|s| !s.is_empty())?;
    let parsed = parse(raw)?;
    let obj = parsed.as_object()?;
    let device = obj.get("device_id").and_then(Value::as_str).filter(|s| !s.is_empty())?;
    let session = obj.get("session_id").and_then(Value::as_str).filter(|s| !s.is_empty())?;
    Some((device.to_owned(), session.to_owned()))
}

/// `extractBodySessionId(body)`.
pub fn extract_body_session_id(body: &Value) -> String {
    if !body.is_object() { return String::new(); }
    parse_claude_code_user_id(get(body, &["metadata", "user_id"])).map(|(_, s)| s).unwrap_or_default()
}

/// `isGenuineClaudeCodeRequest(request)`.
pub fn is_genuine_claude_code_request(request: &Value) -> bool {
    let trim = crate::engine::dispatch::util::js_trim;
    if !request.is_object() { return false; }
    let ua = get_str(request, &["userAgent"]).map(trim).unwrap_or("");
    if !claude_cli_user_agent(ua) { return false; }
    let Some(x_app) = get_str(request, &["metadata", "plexus_metadata", "clientHeaders", "x-app"]) else { return false };
    if trim(x_app).to_lowercase() != "cli" { return false; }
    let betas = get_str(request, &["anthropicBeta"]).unwrap_or("");
    if !betas.split(',').any(|b| trim(b) == "claude-code-20250219") { return false; }
    let Some(body) = request.get("originalBody").filter(|b| b.is_object()) else { return false };
    let Some(system) = body.get("system").and_then(Value::as_array).filter(|s| s.len() >= 2) else { return false };
    let Some(billing) = get_str(&system[0], &["text"]) else { return false };
    if !billing.starts_with(masking::BILLING_HEADER_PREFIX) || !billing.contains("cc_entrypoint=cli") || billing.contains("cch=") { return false; }
    if get_str(&system[1], &["text"]) != Some(masking::CLAUDE_CODE_IDENTITY_TEXT) { return false; }
    let Some((_, session)) = parse_claude_code_user_id(get(body, &["metadata", "user_id"])) else { return false };
    let header_session = get_str(request, &["claudeCodeSessionId"]).map(trim).unwrap_or("");
    header_session.is_empty() || session == header_session
}

// ─── Anthropic ─────────────────────────────────────────────────────────────

/// `NativeAnthropicAuth`.
pub enum NativeAuth { OAuth(String), ApiKey(String) }

/// Anthropic-only options (`AnthropicNativeOptions`).
#[derive(Default, Clone)]
pub struct AnthropicOptions {
    pub caller_betas: Option<String>,
    pub claude_passthrough: bool,
    pub caller_user_agent: Option<String>,
    pub caller_session_id: Option<String>,
}

/// `mergeBetas(callerBetas)`.
fn merge_betas(caller: Option<&str>) -> String {
    let mut merged: Vec<String> = masking::REQUIRED_BETAS.iter().map(|b| (*b).to_owned()).collect();
    for raw in caller.unwrap_or("").split(',') {
        let beta = crate::engine::dispatch::util::js_trim(raw);
        if !beta.is_empty() && !merged.iter().any(|b| b == beta) { merged.push(beta.to_owned()); }
    }
    merged.join(",")
}

fn anthropic_headers(auth: &NativeAuth, streaming: bool, caller_betas: Option<&str>, ua: Option<&str>, session: Option<&str>) -> Vec<(String, Value)> {
    let mut stainless = masking::stainless_headers();
    for (k, v) in stainless.iter_mut() {
        if k == "user-agent" { if let Some(u) = ua { *v = u.to_owned(); } }
        if k == "x-claude-code-session-id" { if let Some(s) = session { *v = s.to_owned(); } }
    }
    let mut out = h(&[
        ("Content-Type", "application/json".into()),
        ("Accept", accept(streaming)),
        ("anthropic-version", "2023-06-01".into()),
        ("anthropic-beta", merge_betas(caller_betas)),
    ]);
    out.extend(stainless.into_iter().map(|(k, v)| (k, json!(v))));
    match auth {
        NativeAuth::OAuth(token) => out.push(("Authorization".into(), json!(format!("Bearer {token}")))),
        NativeAuth::ApiKey(key) => out.push(("x-api-key".into(), json!(key))),
    }
    out
}

fn prepare_anthropic(model_id: &str, auth: &NativeAuth, native_body: Value, streaming: bool, options: &AnthropicOptions) -> Result<PreparedOAuthRequest, String> {
    let url = format!("{}/v1/messages", resolve_oauth_base_url("anthropic", model_id)?);
    if options.claude_passthrough {
        let caller_ua = options.caller_user_agent.as_deref().map(crate::engine::dispatch::util::js_trim).filter(|s| !s.is_empty());
        let caller_session = options.caller_session_id.as_deref().map(crate::engine::dispatch::util::js_trim).filter(|s| !s.is_empty()).map(str::to_owned)
            .or_else(|| Some(extract_body_session_id(&native_body)).filter(|s| !s.is_empty()));
        let mut headers = anthropic_headers(auth, streaming, options.caller_betas.as_deref(), caller_ua, caller_session.as_deref());
        if caller_session.is_none() { headers.retain(|(k, _)| k != "x-claude-code-session-id"); }
        return Ok(PreparedOAuthRequest { url, headers, body: native_body, reverse_pairs: Vec::new() });
    }
    let prepared = clamp_anthropic_effort_and_thinking(native_body, Some(model_id));
    let masking_token = match auth { NativeAuth::OAuth(t) => t.clone(), NativeAuth::ApiKey(k) => format!("sk-ant-oat-mask-{k}") };
    let transformed = masking::apply_claude_oauth_transform(prepared.clone(), &masking_token);
    let (masked, pairs) = masking::apply_claude_code_masking(&transformed);
    let caller_tools: Vec<String> = prepared.get("tools").and_then(Value::as_array).map(|t| t.iter().filter_map(|t| get_str(t, &["name"]).map(str::to_owned)).collect()).unwrap_or_default();
    let headers = anthropic_headers(auth, streaming, options.caller_betas.as_deref(), None, None);
    Ok(PreparedOAuthRequest { url, headers, body: masked, reverse_pairs: masking::frame_reverse_pairs(&caller_tools, &pairs) })
}

// ─── Codex ─────────────────────────────────────────────────────────────────

/// `isCodexCliShapedBody(body)`.
pub fn is_codex_cli_shaped_body(body: Option<&Value>) -> bool {
    let Some(body) = body.filter(|b| b.is_object() || b.is_array()) else { return false };
    if let Some(cm) = body.get("client_metadata").filter(|c| c.is_object() || c.is_array()) {
        if ["x-codex-turn-metadata", "x-codex-installation-id", "x-codex-window-id", "turn_id", "thread_id"].iter().any(|k| !nullish(cm.get(*k))) { return true; }
    }
    !detect_responses_extensions(body).is_empty()
}

/// `extractChatgptAccountId(token)`.
pub fn extract_chatgpt_account_id(token: &str) -> Option<String> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 { return None; }
    let cleaned: String = parts[1].chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(cleaned).ok()?;
    let payload: Value = serde_json::from_slice(&bytes).ok()?;
    get_str(&payload, &["https://api.openai.com/auth", "chatgpt_account_id"]).filter(|s| !s.is_empty()).map(str::to_owned)
}

/// `buildCodexOAuthHeaders(token)`.
pub fn codex_oauth_headers(token: &str) -> Vec<(String, Value)> {
    let mut out = h(&[("Authorization", format!("Bearer {token}"))]);
    if let Some(account) = extract_chatgpt_account_id(token) { out.push(("chatgpt-account-id".into(), json!(account))); }
    out.extend(h(&[
        ("originator", "codex_cli_rs".into()),
        ("Version", crate::cli_versions::codex_version()),
        ("User-Agent", crate::codex_masking::user_agent()),
    ]));
    out
}

/// `adornCodexResponsesBody(body)`.
fn adorn_codex_body(body: Value) -> Value {
    let mut next = body.as_object().cloned().unwrap_or_default();
    next.insert("store".into(), json!(false));
    next.insert("stream".into(), json!(true));
    if next.get("instructions").and_then(Value::as_str).is_none_or(str::is_empty) { next.insert("instructions".into(), json!("You are a helpful assistant.")); }
    let mut include: Vec<Value> = next.get("include").and_then(Value::as_array).cloned().unwrap_or_default();
    include.push(json!("reasoning.encrypted_content"));
    let mut unique: Vec<Value> = Vec::new();
    for v in include { if !unique.contains(&v) || v.is_object() || v.is_array() { unique.push(v); } }
    next.insert("include".into(), Value::Array(unique));
    let text = next.get("text").cloned();
    let verbosity = coalesce(text.as_ref().and_then(|t| t.get("verbosity")), None).unwrap_or(json!("low"));
    let mut text_obj = match text.filter(|t| !nullish(Some(t))) { Some(Value::Object(m)) => m, _ => Default::default() };
    text_obj.insert("verbosity".into(), verbosity);
    next.insert("text".into(), Value::Object(text_obj));
    if nullish(next.get("tool_choice")) { next.insert("tool_choice".into(), json!("auto")); }
    if nullish(next.get("parallel_tool_calls")) { next.insert("parallel_tool_calls".into(), json!(true)); }
    Value::Object(next)
}

fn prepare_codex(model_id: &str, token: &str, native_body: Value, streaming: bool, passthrough: bool) -> Result<PreparedOAuthRequest, String> {
    let mut body = strip_unsupported_gpt5_options(if passthrough { native_body } else { adorn_codex_body(native_body) });
    if get_str(&body, &["service_tier"]) == Some("flex") { if let Some(m) = body.as_object_mut() { m.shift_remove("service_tier"); } }
    let url = format!("{}/codex/responses", resolve_oauth_base_url("openai-codex", model_id)?);
    let session = get_str(&body, &["prompt_cache_key"]).filter(|s| !s.is_empty()).map(str::to_owned);
    let mut headers = h(&[("Content-Type", "application/json".into()), ("accept", accept(streaming))]);
    headers.extend(codex_oauth_headers(token));
    headers.push(("OpenAI-Beta".into(), json!("responses=experimental")));
    if let Some(s) = session { headers.extend(h(&[("session-id", s.clone()), ("x-client-request-id", s)])); }
    Ok(PreparedOAuthRequest { url, headers, body, reverse_pairs: Vec::new() })
}

/// `prepareCodexImagesDispatch({modelId, oauthAccountId})` → `(baseUrl, headers)`.
pub async fn prepare_codex_images_dispatch(state: &AppState, model_id: &str, account: Option<&str>) -> Result<(String, Vec<(String, String)>), String> {
    let token = auth::get_api_key(state, "openai-codex", account, false).await?;
    let base = format!("{}/codex", resolve_oauth_base_url("openai-codex", model_id)?);
    let mut headers = vec![("Accept".to_owned(), "application/json".to_owned())];
    headers.extend(codex_oauth_headers(&token).into_iter().map(|(k, v)| (k, to_string(Some(&v)))));
    Ok((base, headers))
}

// ─── GitHub Copilot ────────────────────────────────────────────────────────

/// `copilotEndpoint(apiType)`.
pub fn copilot_endpoint(api_type: &str) -> &'static str {
    match api_type { "messages" => "/v1/messages", "responses" => "/responses", _ => "/chat/completions" }
}

fn copilot_items(body: &Value) -> Vec<Value> {
    body.get("messages").and_then(Value::as_array).or_else(|| body.get("input").and_then(Value::as_array)).cloned().unwrap_or_default()
}

fn copilot_dynamic_headers(body: &Value) -> Vec<(String, Value)> {
    let items = copilot_items(body);
    let initiator = match items.last() {
        Some(last) if last.is_object() || last.is_array() => match last.get("role").and_then(Value::as_str) { Some("user") => "user", Some(_) => "agent", None => "agent" },
        _ => "user",
    };
    let mut out = h(&[("X-Initiator", initiator.into()), ("Openai-Intent", "conversation-edits".into())]);
    let has_image = |content: Option<&Value>| content.and_then(Value::as_array).is_some_and(|parts| parts.iter().any(|p| matches!(get_str(p, &["type"]), Some("image") | Some("image_url") | Some("input_image"))));
    if items.iter().any(|it| (it.is_object() || it.is_array()) && has_image(it.get("content"))) { out.push(("Copilot-Vision-Request".into(), json!("true"))); }
    out
}

fn adorn_copilot_body(body: Value, api_type: &str, streaming: bool) -> Value {
    if api_type != "chat" || !streaming { return body; }
    let mut next = body.as_object().cloned().unwrap_or_default();
    let mut existing = match next.get("stream_options") { Some(Value::Object(m)) => m.clone(), _ => Default::default() };
    let include = coalesce(existing.get("include_usage"), None).unwrap_or(json!(true));
    existing.insert("include_usage".into(), include);
    next.insert("stream_options".into(), Value::Object(existing));
    Value::Object(next)
}

fn prepare_copilot(token: &str, native_body: Value, streaming: bool, api_type: &str, resolved_base: Option<String>) -> PreparedOAuthRequest {
    let body = adorn_copilot_body(native_body, api_type, streaming);
    let base = resolved_base.filter(|b| !b.is_empty()).unwrap_or_else(|| auth::copilot_base_url(token, None));
    let base = base.strip_suffix('/').map(str::to_owned).unwrap_or(base);
    let mut headers = h(&[
        ("Content-Type", "application/json".into()),
        ("Accept", accept(streaming)),
        ("Authorization", format!("Bearer {token}")),
        ("User-Agent", "GitHubCopilotChat/0.35.0".into()),
        ("Editor-Version", "vscode/1.107.0".into()),
        ("Editor-Plugin-Version", "copilot-chat/0.35.0".into()),
        ("Copilot-Integration-Id", "vscode-chat".into()),
    ]);
    headers.extend(copilot_dynamic_headers(&body));
    if api_type == "messages" { headers.push(("anthropic-version".into(), json!("2023-06-01"))); }
    PreparedOAuthRequest { url: format!("{base}{}", copilot_endpoint(api_type)), headers, body, reverse_pairs: Vec::new() }
}

// ─── Meta / generic ────────────────────────────────────────────────────────

fn prepare_meta(token: &str, native_body: Value, streaming: bool) -> Result<PreparedOAuthRequest, String> {
    let base = resolve_oauth_base_url("meta", "muse-spark")?;
    let base = base.strip_suffix('/').map(str::to_owned).unwrap_or(base);
    Ok(PreparedOAuthRequest {
        url: format!("{base}/responses"),
        headers: h(&[("Content-Type", "application/json".into()), ("Accept", accept(streaming)), ("Authorization", format!("Bearer {token}")), ("x-api-version", "1.0.0".into())]),
        body: native_body,
        reverse_pairs: Vec::new(),
    })
}

/// Inputs of `prepareNativeOAuthDispatch`.
pub struct NativeParams<'a> {
    pub provider: &'a str,
    pub model_id: &'a str,
    pub native_body: Value,
    pub streaming: bool,
    pub oauth_account: Option<&'a str>,
    pub masking_api_key: Option<String>,
    pub codex_passthrough: bool,
    pub api_type: &'a str,
    pub anthropic: AnthropicOptions,
}

/// `prepareNativeOAuthDispatch(params)`.
pub async fn prepare_native_oauth_dispatch(state: &AppState, p: NativeParams<'_>) -> Result<PreparedOAuthRequest, String> {
    let auth = match p.masking_api_key {
        Some(key) => {
            let key = crate::engine::dispatch::util::js_trim(&key).to_owned();
            if key.is_empty() { return Err("OAuth: API key is not configured for Claude masking provider. Set the provider's api_key.".into()); }
            NativeAuth::ApiKey(key)
        }
        None => NativeAuth::OAuth(auth::get_api_key(state, p.provider, p.oauth_account, false).await?),
    };
    let base_url = if p.provider == "github-copilot" { auth::get_base_url(state, p.provider, p.oauth_account).await } else { None };
    match (p.provider, &auth) {
        ("anthropic", _) => prepare_anthropic(p.model_id, &auth, p.native_body, p.streaming, &p.anthropic),
        ("openai-codex", NativeAuth::OAuth(t)) => prepare_codex(p.model_id, t, p.native_body, p.streaming, p.codex_passthrough),
        ("openai-codex", _) => Err("Codex native OAuth requires an OAuth token (apiKey mode unsupported).".into()),
        ("github-copilot", NativeAuth::OAuth(t)) => Ok(prepare_copilot(t, p.native_body, p.streaming, if p.api_type.is_empty() { "chat" } else { p.api_type }, base_url)),
        ("github-copilot", _) => Err("Copilot native OAuth requires an OAuth token (apiKey mode unsupported).".into()),
        ("meta", NativeAuth::OAuth(t)) => prepare_meta(t, p.native_body, p.streaming),
        ("meta", _) => Err("Meta OAuth requires an OAuth token (apiKey mode unsupported).".into()),
        (other, _) => Err(format!("OAuth native request preparation not implemented for provider '{other}'")),
    }
}

/// `prepareGenericOAuthDispatch(params)`.
#[allow(clippy::too_many_arguments)]
pub async fn prepare_generic_oauth_dispatch(state: &AppState, provider: &str, model_id: &str, body: Value, streaming: bool, api_type: &str, account: Option<&str>, extra_headers: Option<&Value>, force_refresh: bool) -> Result<PreparedOAuthRequest, String> {
    let endpoint = match api_type { "chat" => "/chat/completions", "responses" => "/responses", "messages" => "/messages", _ => {
        return Err(format!("OAuth: provider '{provider}' model '{model_id}' resolved to unsupported wire API '{api_type}' for generic OAuth dispatch."));
    } };
    let token = auth::get_api_key(state, provider, account, force_refresh).await?;
    let base = resolve_generic_oauth_base_url(provider, model_id)?;
    let mut headers = h(&[("Content-Type", "application/json".into()), ("Accept", accept(streaming)), ("Authorization", format!("Bearer {token}"))]);
    if let Some(Value::Object(extra)) = extra_headers {
        for (k, v) in extra {
            match headers.iter().position(|(hk, _)| hk == k) { Some(i) => headers[i].1 = v.clone(), None => headers.push((k.clone(), v.clone())) }
        }
    }
    Ok(PreparedOAuthRequest { url: format!("{base}{endpoint}"), headers, body, reverse_pairs: Vec::new() })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn detection_helpers() {
        assert!(claude_cli_user_agent("claude-cli/2.1.280 (external, cli)"));
        assert!(!claude_cli_user_agent("claude-cli/2.1 (external, cli)"));
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acct"}}"#);
        assert_eq!(extract_chatgpt_account_id(&format!("a.{payload}.c")).as_deref(), Some("acct"));
        assert_eq!(extract_chatgpt_account_id(&format!("a.{payload}")), None);
        assert_eq!(merge_betas(Some("x, oauth-2025-04-20,y")).split(',').filter(|b| *b == "x" || *b == "y").count(), 2);
        let body = adorn_codex_body(json!({"input": [], "include": ["a", "reasoning.encrypted_content"], "text": {"format": 1}}));
        assert_eq!(stringify(&body), r#"{"input":[],"include":["a","reasoning.encrypted_content"],"text":{"format":1,"verbosity":"low"},"store":false,"stream":true,"instructions":"You are a helpful assistant.","tool_choice":"auto","parallel_tool_calls":true}"#);
        let c = prepare_copilot("t;proxy-ep=proxy.x.githubcopilot.com", json!({"messages": [{"role": "tool", "content": [{"type": "image_url"}]}], "stream": true}), true, "chat", None);
        assert_eq!(c.url, "https://api.x.githubcopilot.com/chat/completions");
        assert_eq!(c.body["stream_options"], json!({"include_usage": true}));
        assert!(c.headers.iter().any(|(k, v)| k == "X-Initiator" && v == "agent"));
        assert!(c.headers.iter().any(|(k, _)| k == "Copilot-Vision-Request"));
    }

    /// TS-vs-Rust harness: `OAUTH_DIFF_IN=scenarios.json OAUTH_DIFF_OUT=out.json
    /// cargo test -- --ignored oauth_native_diff_harness`.
    #[test]
    #[ignore]
    fn oauth_native_diff_harness() {
        let scen: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(std::env::var("OAUTH_DIFF_IN").unwrap()).unwrap()).unwrap();
        let out: Vec<Value> = scen.iter().map(|s| {
            let opt = |k: &str| get_str(s, &["options", k]).map(str::to_owned);
            let auth = if get_str(s, &["auth", "mode"]) == Some("apiKey") { NativeAuth::ApiKey(opt_auth(s, "apiKey")) } else { NativeAuth::OAuth(opt_auth(s, "token")) };
            let body = s["body"].clone();
            let streaming = s["streaming"] == json!(true);
            let model = get_str(s, &["model"]).unwrap();
            let r = match get_str(s, &["provider"]).unwrap() {
                "anthropic" => prepare_anthropic(model, &auth, body, streaming, &AnthropicOptions {
                    caller_betas: opt("callerBetas"), claude_passthrough: s["options"]["claudePassthrough"] == json!(true),
                    caller_user_agent: opt("callerUserAgent"), caller_session_id: opt("callerSessionId"),
                }),
                "openai-codex" => prepare_codex(model, &opt_auth(s, "token"), body, streaming, s["options"]["codexPassthrough"] == json!(true)),
                "github-copilot" => Ok(prepare_copilot(&opt_auth(s, "token"), body, streaming, &opt("apiType").unwrap_or_else(|| "chat".into()), opt("baseUrl"))),
                "meta" => prepare_meta(&opt_auth(s, "token"), body, streaming),
                _ => Err("unknown".into()),
            };
            match r {
                Ok(p) => json!({"name": s["name"], "url": p.url, "headers": Value::Object(p.headers.iter().cloned().collect()), "body": p.body,
                    "frames": s["frames"].as_array().unwrap().iter().map(|f| json!(p.reverse_frame(f.as_str().unwrap()))).collect::<Vec<_>>()}),
                Err(e) => json!({"name": s["name"], "error": e}),
            }
        }).collect();
        std::fs::write(std::env::var("OAUTH_DIFF_OUT").unwrap(), serde_json::to_string_pretty(&out).unwrap()).unwrap();
    }
    fn opt_auth(s: &Value, k: &str) -> String { get_str(s, &["auth", k]).unwrap_or("").to_owned() }
}
