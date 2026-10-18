//! Port of Plexus `routes/inference/models.ts` (+ `models-ui.ts`) and the
//! alias-facing helpers of `model-metadata-manager.ts` /
//! `alias-service-tiers.ts`: `GET /v1/models` (public, ETag'd, `?ui` viewer),
//! `/v1/metadata/{search,lookup}`, `/v1/openrouter/models` (public) and the
//! authenticated Muse CLI catalog (`/v1/muse-code/models`, `/muse-code/models`).
//!
//! Works on the Plexus-shaped config views (`ProviderConfig::public_view`,
//! `ModelConfig::public_view`) with aliases in repository row order, exactly
//! what Plexus' `getConfig()` exposes.
use std::sync::{Mutex, OnceLock};

use axum::{
    extract::{RawQuery, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Router,
};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::engine::config::{api_format, get_provider_types};
use crate::engine::dispatch::auto_compat::{apply_quirk_overlay, resolve_inline_quirks};
use crate::engine::dispatch::pi_ai::get_supported_thinking_levels;
use crate::engine::dispatch::service_tier_selection::infer_service_tier_format;
use crate::engine::js::{self, get, get_str, truthy};
use crate::AppState;

type Obj = Map<String, Value>;

pub fn router() -> Router<AppState> {
    model_created_at();
    Router::new()
        .route("/v1/models", axum::routing::get(list_models))
        .route("/v1/metadata/search", axum::routing::get(metadata_search))
        .route("/v1/metadata/lookup", axum::routing::get(metadata_lookup))
        .route("/v1/openrouter/models", axum::routing::get(openrouter_models))
        .route("/v1/muse-code/models", axum::routing::get(muse_models))
        .route("/muse-code/models", axum::routing::get(muse_models))
}

/// `MODEL_CREATED_AT`: seconds at module load.
fn model_created_at() -> i64 {
    static AT: OnceLock<i64> = OnceLock::new();
    *AT.get_or_init(js::now_secs)
}

// ─── config snapshot ─────────────────────────────────────────────────────

struct Snapshot {
    providers: Obj,
    /// `config.models` in JS key order (repository row order, integer-like keys first).
    models: Obj,
    vision_fallthrough: bool,
}

async fn snapshot(state: &AppState) -> Snapshot {
    let order: Vec<String> = match &state.inner.database {
        Some(pool) => sqlx::query_scalar::<_, String>("SELECT slug FROM model_aliases ORDER BY id").fetch_all(pool).await.unwrap_or_default(),
        None => vec![],
    };
    let providers: Obj = state.inner.providers.read().unwrap().iter().map(|(k, p)| (k.clone(), p.public_view())).collect();
    let mut rows = Obj::new();
    {
        let aliases = state.inner.aliases.read().unwrap();
        for slug in &order {
            if let Some(alias) = aliases.get(slug) { rows.insert(slug.clone(), alias.public_view()); }
        }
        let mut rest: Vec<_> = aliases.keys().filter(|k| !rows.contains_key(*k)).cloned().collect();
        rest.sort();
        for slug in rest { let view = aliases[&slug].public_view(); rows.insert(slug, view); }
    }
    let models: Obj = crate::engine::routing::js_keys(&rows).into_iter().map(|k| (k.clone(), rows[k].clone())).collect();
    let vision_fallthrough = truthy(state.inner.settings.read().unwrap().get("vision_fallthrough"));
    Snapshot { providers, models, vision_fallthrough }
}

fn targets(model: &Value) -> Vec<&Value> {
    model.get("target_groups").and_then(Value::as_array).into_iter().flatten()
        .flat_map(|g| g.get("targets").and_then(Value::as_array).into_iter().flatten()).collect()
}
fn enabled(target: &Value) -> bool { target.get("enabled") != Some(&Value::Bool(false)) }
fn s(v: Option<&Value>) -> Option<&str> { v.and_then(Value::as_str) }

// ─── model-metadata-manager.ts helpers ───────────────────────────────────

fn infer_provider_from_model(model: &str) -> Option<String> {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let normalized = model.rsplit('/').next().unwrap_or(model).to_lowercase();
    if normalized.contains("claude") { return Some("anthropic".into()); }
    if RE.get_or_init(|| regex::Regex::new(r"^(gpt|o[134](?:-|$)|text-embedding|dall-e)").unwrap()).is_match(&normalized) { return Some("openai".into()); }
    if normalized.contains("gemini") || normalized.contains("imagen") { return Some("google".into()); }
    if normalized.contains("mistral") || normalized.contains("codestral") { return Some("mistralai".into()); }
    None
}

fn normalize_catalog_provider(provider: &str, model: &str) -> String {
    match provider {
        "openai-codex" => "openai".into(),
        "github-copilot" => infer_provider_from_model(model).unwrap_or_else(|| provider.into()),
        _ => provider.into(),
    }
}

struct Identity { provider: Option<String>, model: String }

/// `resolveAutomaticModelIdentity`.
fn automatic_identity(alias_id: &str, model: &Value, providers: &Obj) -> Identity {
    if let Some(pi) = model.get("pi_model").filter(|v| v.is_object()) {
        let provider = s(pi.get("provider")).unwrap_or_default();
        let model_id = s(pi.get("model_id")).unwrap_or_default();
        return Identity { provider: Some(normalize_catalog_provider(provider, model_id)), model: model_id.into() };
    }
    let mut unique: Vec<(String, (String, String))> = Vec::new();
    for target in targets(model).into_iter().filter(|t| enabled(t) && !truthy(t.get("alias"))) {
        let provider_id = s(target.get("provider")).unwrap_or_default();
        let target_model = s(target.get("model")).unwrap_or_default();
        let provider = providers.get(provider_id);
        let provider_model = provider.and_then(|p| crate::engine::config::model_config(p, target_model));
        let m = provider_model.and_then(|pm| js::coalesce(pm.get("pi_ai_model_id"), None)).map(|v| js::to_string(Some(&v))).unwrap_or_else(|| target_model.into());
        let pid = provider.and_then(|p| js::coalesce(p.get("pi_ai_provider"), None)).map(|v| js::to_string(Some(&v)))
            .or_else(|| infer_provider_from_model(&m)).unwrap_or_else(|| provider_id.into());
        let identity = (normalize_catalog_provider(&pid, &m), m);
        let key = format!("{}\0{}", identity.0.to_lowercase(), identity.1.to_lowercase());
        match unique.iter_mut().find(|(k, _)| *k == key) { Some(slot) => slot.1 = identity, None => unique.push((key, identity)) }
    }
    if unique.len() == 1 {
        let (_, (provider, model)) = unique.remove(0);
        return Identity { provider: Some(provider), model };
    }
    Identity { provider: infer_provider_from_model(alias_id), model: alias_id.into() }
}

/// `resolvePreferredApi`.
fn resolve_preferred_api(alias_id: &str, model: &Value, providers: &Obj) -> Option<Value> {
    if let Some(v) = model.get("preferred_api") { return Some(v.clone()); }
    if model.get("type").is_some_and(|t| t != "text") { return None; }
    let identity = automatic_identity(alias_id, model, providers);
    let provider = identity.provider.as_deref().unwrap_or_default().to_lowercase();
    let m = identity.model.rsplit('/').next().unwrap_or(&identity.model).to_lowercase();
    static GPT: OnceLock<regex::Regex> = OnceLock::new();
    static GEMINI: OnceLock<regex::Regex> = OnceLock::new();
    let api = if provider.contains("anthropic") || m.contains("claude") { "messages" }
        else if GPT.get_or_init(|| regex::Regex::new(r"^(gpt|o[134])(?:-|$)").unwrap()).is_match(&m) { "responses" }
        else if GEMINI.get_or_init(|| regex::Regex::new(r"^gemini(?:-|$)").unwrap()).is_match(&m) { "gemini" }
        else { "chat_completions" };
    Some(json!([api]))
}

/// `mergeOverrides(base, overrides)`.
fn merge_overrides(base: Option<Value>, overrides: Option<&Value>) -> Option<Value> {
    let Some(overrides) = overrides.filter(|o| !o.is_null()) else { return base };
    let b = base.as_ref();
    let mut merged = Obj::new();
    merged.insert("id".into(), b.and_then(|b| js::coalesce(b.get("id"), None)).unwrap_or(json!("")));
    merged.insert("name".into(), b.and_then(|b| js::coalesce(b.get("name"), None)).unwrap_or(json!("")));
    for key in ["description", "context_length", "architecture", "pricing", "supported_parameters", "top_provider"] {
        if let Some(v) = b.and_then(|b| b.get(key)) { merged.insert(key.into(), v.clone()); }
    }
    for key in ["name", "description", "context_length"] {
        if let Some(v) = overrides.get(key) { merged.insert(key.into(), v.clone()); }
    }
    for key in ["pricing", "architecture", "top_provider"] {
        if truthy(overrides.get(key)) {
            let spread = js::spread(merged.get(key).unwrap_or(&Value::Null), &overrides[key]);
            merged.insert(key.into(), spread);
        }
    }
    if let Some(v) = overrides.get("supported_parameters") { merged.insert("supported_parameters".into(), v.clone()); }
    let empty = !truthy(merged.get("name"))
        && ["description", "context_length", "pricing", "architecture", "supported_parameters", "top_provider"].iter().all(|k| !merged.contains_key(*k));
    (!empty).then_some(Value::Object(merged))
}

fn humanize_model_name(model: &str) -> String {
    let id = model.rsplit('/').next().unwrap_or(model);
    id.split(['-', '_', '.']).filter(|t| !t.is_empty()).map(|token| match token.to_lowercase().as_str() {
        "ai" => "AI".into(), "api" => "API".into(), "gpt" => "GPT".into(), "llm" => "LLM".into(), "tts" => "TTS".into(),
        _ => { let mut c = token.chars(); c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default() }
    }).collect::<Vec<_>>().join(" ")
}

fn infer_model_type(model: &str, configured: Option<&str>) -> String {
    if let Some(t) = configured.filter(|t| !t.is_empty()) { return t.into(); }
    static RES: OnceLock<[regex::Regex; 4]> = OnceLock::new();
    let [embed, stt, tts, image] = RES.get_or_init(|| [
        regex::Regex::new(r"embed|embedding").unwrap(),
        regex::Regex::new(r"whisper|transcri|speech[-_.]?to[-_.]?text|(?:^|[-_.])stt(?:$|[-_.])").unwrap(),
        regex::Regex::new(r"(?:^|[-_.])tts(?:$|[-_.])|text[-_.]?to[-_.]?speech").unwrap(),
        regex::Regex::new(r"dall[-_.]?e|(?:^|[/_.-])(flux|imagen)(?:$|[/_.-])").unwrap(),
    ]);
    let n = model.to_lowercase();
    if embed.is_match(&n) { "embeddings" } else if stt.is_match(&n) { "transcriptions" } else if tts.is_match(&n) { "speech" } else if image.is_match(&n) { "image" } else { "text" }.into()
}

fn infer_architecture(model: &str, configured: Option<&str>) -> Value {
    let (modality, input, output) = match infer_model_type(model, configured).as_str() {
        "embeddings" => ("text->embeddings", "text", "embeddings"),
        "transcriptions" => ("audio->text", "audio", "text"),
        "speech" => ("text->audio", "text", "audio"),
        "image" => ("text->image", "text", "image"),
        _ => ("text->text", "text", "text"),
    };
    json!({"modality": modality, "input_modalities": [input], "output_modalities": [output]})
}

fn apply_configured_model_type(mut metadata: Value, configured: Option<&str>) -> Value {
    if configured != Some("embeddings") { return metadata; }
    let input = get(&metadata, &["architecture", "input_modalities"]).filter(|v| !v.is_null()).cloned().unwrap_or(json!(["text"]));
    let joined = input.as_array().map(|a| a.iter().map(|v| js::to_string(Some(v))).collect::<Vec<_>>().join("+")).unwrap_or_else(|| js::to_string(Some(&input)));
    let mut arch = metadata.get("architecture").and_then(Value::as_object).cloned().unwrap_or_default();
    arch.insert("modality".into(), json!(format!("{joined}->embeddings")));
    arch.insert("input_modalities".into(), input);
    arch.insert("output_modalities".into(), json!(["embeddings"]));
    metadata["architecture"] = Value::Object(arch);
    metadata
}

fn catalog_metadata(source: &str, path: &str) -> Option<Value> {
    crate::metadata::lookup(source, path).and_then(|m| serde_json::to_value(m).ok())
}

/// `findExactMetadata(provider, model)`.
fn find_exact_metadata(provider: &str, model: &str) -> Option<Value> {
    [("models.dev", format!("{provider}.{model}")), ("openrouter", format!("{provider}/{model}")), ("catwalk", format!("{provider}.{model}"))]
        .into_iter().find_map(|(source, path)| catalog_metadata(source, &path))
}

/// `resolveModelMetadata(...)?.metadata`.
pub(crate) fn resolve_model_metadata(alias_id: &str, model: &Value, providers: &Obj) -> Option<Value> {
    let configured = model.get("metadata").filter(|v| v.is_object());
    let source = configured.and_then(|c| s(c.get("source")));
    let overrides = configured.and_then(|c| c.get("overrides"));
    let ty = s(model.get("type"));
    match source {
        Some("disabled") => None,
        Some("custom") => merge_overrides(None, overrides).map(|m| apply_configured_model_type(m, ty)),
        Some(src @ ("openrouter" | "models.dev" | "catwalk")) => {
            let path = configured.map(|c| js::to_string(c.get("source_path"))).unwrap_or_default();
            catalog_metadata(src, &path).and_then(|c| merge_overrides(Some(c), overrides)).map(|m| apply_configured_model_type(m, ty))
        }
        _ => {
            let identity = automatic_identity(alias_id, model, providers);
            let exact = identity.provider.as_deref().filter(|p| !p.is_empty()).and_then(|p| find_exact_metadata(p, &identity.model));
            if let Some(exact) = exact {
                return merge_overrides(Some(exact), overrides).map(|m| apply_configured_model_type(m, ty));
            }
            let heuristic = json!({"id": identity.model, "name": humanize_model_name(&identity.model), "architecture": infer_architecture(&identity.model, ty)});
            merge_overrides(Some(heuristic), overrides).map(|m| apply_configured_model_type(m, ty))
        }
    }
}

fn catalog_model(provider: &str, model: &str) -> Option<Value> { crate::pi_catalog::catalog().get_model(provider, model) }

// ─── alias-service-tiers.ts ──────────────────────────────────────────────

const CANONICAL_TIERS: [&str; 5] = ["auto", "standard", "flex", "priority", "ultrafast"];

fn resolve_alias_service_tiers(alias_id: &str, models: &Obj, providers: &Obj) -> Option<Value> {
    let mut supported: Vec<&str> = Vec::new();
    let mut visited: Vec<String> = Vec::new();
    let mut known = false;
    // Depth-first in target order, like the recursive TS `visit`.
    fn visit(name: &str, models: &Obj, providers: &Obj, visited: &mut Vec<String>, supported: &mut Vec<&'static str>, known: &mut bool) {
        let found = models.get(name).map(|m| (name.to_owned(), m)).or_else(|| models.iter()
            .find(|(_, m)| m.get("additional_aliases").and_then(Value::as_array).is_some_and(|a| a.iter().any(|x| x == name)))
            .map(|(k, m)| (k.clone(), m)));
        let Some((key, alias)) = found else { return };
        if visited.contains(&key) { return; }
        visited.push(key);
        for target in targets(alias) {
            if !enabled(target) { continue; }
            if truthy(target.get("alias")) { visit(&js::to_string(target.get("alias")), models, providers, visited, supported, known); continue; }
            let (Some(pslug), Some(tmodel)) = (s(target.get("provider")).filter(|v| !v.is_empty()), s(target.get("model")).filter(|v| !v.is_empty())) else { continue };
            let Some(provider) = providers.get(pslug) else { continue };
            if provider.get("enabled") == Some(&Value::Bool(false)) { continue; }
            let model = crate::engine::config::model_config(provider, tmodel);
            if provider.get("auto_compat") != Some(&Value::Bool(true)) && model.and_then(|m| m.get("auto_compat")) != Some(&Value::Bool(true)) { continue; }
            let base = s(provider.get("pi_ai_provider")).filter(|p| !p.is_empty())
                .and_then(|p| catalog_model(p, s(model.and_then(|m| m.get("pi_ai_model_id"))).unwrap_or(tmodel)));
            let access = model.and_then(|m| m.get("access_via")).filter(|a| a.as_array().is_some_and(|a| !a.is_empty()));
            let api_types = match access { Some(a) => api_format::normalize_access_list(Some(a)), None => get_provider_types(provider) };
            let quirks = provider.get("pi_ai_quirks").filter(|q| !q.is_null());
            for api_type in api_types {
                let capability = match &base {
                    Some(b) => Some(apply_quirk_overlay(b, quirks, &api_type, tmodel)),
                    None => resolve_inline_quirks(quirks, &api_type, tmodel),
                };
                let common = quirks.and_then(|q| q.get(api_format::base_type(&api_type)));
                let map = common.and_then(|c| get(c, &["models", tmodel, "serviceTierMap"])).filter(|v| !v.is_null()).cloned()
                    .or_else(|| base.as_ref().and_then(|b| b.get("serviceTierMap")).filter(|v| !v.is_null()).cloned());
                let (Some(map), Some(capability)) = (map.filter(|m| truthy(Some(m))), capability) else { continue };
                let api = get_str(&capability, &["api"]);
                let format = get_str(&capability, &["compat", "serviceTierFormat"]).map(str::to_owned).or_else(|| infer_service_tier_format(api).map(str::to_owned));
                let Some(format) = format.filter(|f| !f.is_empty()) else { continue };
                if format == "anthropic-speed" && api != Some("anthropic-messages") { continue; }
                *known = true;
                for tier in CANONICAL_TIERS {
                    let Some(native) = map.get(tier).and_then(Value::as_str).filter(|v| !v.is_empty()) else { continue };
                    if format == "anthropic-speed" && native != "fast" && native != "standard" { continue; }
                    if !supported.contains(&tier) { supported.push(tier); }
                }
            }
        }
    }
    visit(alias_id, models, providers, &mut visited, &mut supported, &mut known);
    known.then(|| json!(CANONICAL_TIERS.iter().filter(|t| supported.contains(t)).collect::<Vec<_>>()))
}

// ─── GET /v1/models ──────────────────────────────────────────────────────

fn quirks_target_for_builtin_api(api: Option<&str>) -> Option<&'static str> {
    match api? {
        "openai-completions" => Some("chat"),
        "openai-responses" | "openai-codex-responses" | "azure-openai-responses" => Some("responses"),
        "anthropic-messages" => Some("messages"),
        "google-generative-ai" | "google-generative-ai-vertex" => Some("gemini"),
        _ => None,
    }
}

struct InlineTarget { model: String, api_type: String, quirks: Value }

fn resolve_alias_inline_target(model: &Value, providers: &Obj, preferred: Option<&Value>, builtin_api: Option<&str>) -> Option<InlineTarget> {
    let mut unique: Vec<(String, String)> = Vec::new();
    for t in targets(model).into_iter().filter(|t| enabled(t) && truthy(t.get("provider")) && truthy(t.get("model"))) {
        let pair = (js::to_string(t.get("provider")), js::to_string(t.get("model")));
        if !unique.contains(&pair) { unique.push(pair); }
    }
    if unique.len() != 1 { return None; }
    let (provider, target_model) = unique.remove(0);
    let quirks = providers.get(&provider).and_then(|p| p.get("pi_ai_quirks")).filter(|q| truthy(Some(q)))?.clone();
    let api_type = match preferred.and_then(Value::as_array).filter(|a| a.len() == 1) {
        Some(a) => { let first = js::to_string(a.first()); if first == "chat_completions" { "chat".into() } else { first } }
        None => quirks_target_for_builtin_api(builtin_api)?.into(),
    };
    if api_type.is_empty() { return None; }
    Some(InlineTarget { model: target_model, api_type, quirks })
}

fn model_entry(alias_id: &str, model: &Value, snap: &Snapshot, created: i64) -> Value {
    let providers = &snap.providers;
    let identity = automatic_identity(alias_id, model, providers);
    let mut pi_model_config: Option<(String, String)> = model.get("pi_model").filter(|v| truthy(Some(v)))
        .map(|pi| (js::to_string(pi.get("provider")), js::to_string(pi.get("model_id"))));
    let preferred_api = resolve_preferred_api(alias_id, model, providers);
    let has_target_quirks = targets(model).iter().any(|t| enabled(t) && truthy(t.get("provider"))
        && truthy(providers.get(&js::to_string(t.get("provider"))).and_then(|p| p.get("pi_ai_quirks"))));
    let has_target_pi_ai_provider = targets(model).iter().any(|t| enabled(t) && truthy(t.get("provider"))
        && truthy(providers.get(&js::to_string(t.get("provider"))).and_then(|p| p.get("pi_ai_provider"))));
    let can_resolve_builtin = !has_target_quirks || has_target_pi_ai_provider;
    if can_resolve_builtin && pi_model_config.is_none() {
        if let Some(provider) = identity.provider.as_deref().filter(|p| !p.is_empty()) {
            if catalog_model(provider, &identity.model).is_some() { pi_model_config = Some((provider.into(), identity.model.clone())); }
        }
    }
    let pi_model = pi_model_config.as_ref().and_then(|(p, m)| catalog_model(p, m));
    let inline_target = if has_target_quirks {
        resolve_alias_inline_target(model, providers, preferred_api.as_ref(), pi_model.as_ref().and_then(|m| get_str(m, &["api"])))
    } else { None };
    let inline_traits = inline_target.as_ref().and_then(|t| resolve_inline_quirks(Some(&t.quirks), &t.api_type, &t.model));
    let advertised = match (&pi_model, &inline_target) {
        (Some(pm), Some(t)) => Some(apply_quirk_overlay(pm, Some(&t.quirks), &t.api_type, &t.model)),
        _ => pi_model.clone().or_else(|| inline_traits.clone()),
    };
    let pi_options = advertised.as_ref().and_then(|a| a.get("compat")).and_then(Value::as_object).filter(|c| !c.is_empty()).and_then(|c| {
        let mut c = c.clone();
        c.shift_remove("serviceTierFormat");
        (!c.is_empty()).then_some(Value::Object(c))
    });
    let inline_levels: Vec<String> = match &inline_traits {
        Some(t) if t.get("reasoning") == Some(&Value::Bool(true)) && truthy(t.get("thinkingLevelMap")) =>
            t["thinkingLevelMap"].as_object().map(|m| crate::engine::routing::js_keys(m).into_iter().filter(|k| !m[*k].is_null()).cloned().collect()).unwrap_or_default(),
        _ => vec![],
    };
    let reasoning_options = if pi_model.is_some() && advertised.as_ref().and_then(|a| a.get("reasoning")) == Some(&Value::Bool(true)) {
        Some(json!([{"type": "effort", "values": get_supported_thinking_levels(advertised.as_ref().unwrap())}]))
    } else if !inline_levels.is_empty() {
        Some(json!([{"type": "effort", "values": inline_levels}]))
    } else { None };
    let service_tiers = resolve_alias_service_tiers(alias_id, &snap.models, providers);
    let mut result = js::obj(vec![
        ("id", Some(json!(alias_id))),
        ("object", Some(json!("model"))),
        ("created", Some(json!(created))),
        ("owned_by", Some(json!("plexus"))),
        ("type", Some(js::coalesce(model.get("type"), None).unwrap_or(json!("text")))),
        ("preferred_api", preferred_api),
        ("pi_provider", pi_model_config.as_ref().map(|(p, _)| json!(p))),
        ("pi_model", pi_model_config.as_ref().map(|(_, m)| json!(m))),
        ("pi_options", pi_options),
        ("reasoning_options", reasoning_options),
        ("service_tiers", service_tiers),
    ]);
    let fallthrough = snap.vision_fallthrough && truthy(model.get("use_image_fallthrough"));
    let Some(enriched) = resolve_model_metadata(alias_id, model, providers) else {
        if fallthrough { result["architecture"] = json!({"input_modalities": ["text", "image"], "output_modalities": ["text"]}); }
        return result;
    };
    let out = result.as_object_mut().unwrap();
    out.insert("name".into(), enriched.get("name").cloned().unwrap_or(Value::Null));
    for key in ["description", "context_length", "architecture", "pricing", "supported_parameters", "top_provider"] {
        if let Some(v) = enriched.get(key) { out.insert(key.into(), v.clone()); }
    }
    if fallthrough {
        let arch = out.get("architecture").filter(|a| !a.is_null()).cloned().unwrap_or(json!({}));
        let input: Vec<Value> = arch.get("input_modalities").and_then(Value::as_array).cloned().unwrap_or_default();
        if !input.iter().any(|v| v == "image") {
            let mut next = js::spread(&arch, &json!({}));
            let mut inputs = input.clone();
            inputs.push(json!("image"));
            next["input_modalities"] = Value::Array(inputs);
            out.insert("architecture".into(), next);
        }
        if !truthy(arch.get("output_modalities")) {
            let mut next = js::spread(out.get("architecture").unwrap_or(&Value::Null), &json!({}));
            next["output_modalities"] = json!(["text"]);
            out.insert("architecture".into(), next);
        }
    }
    result
}

#[derive(Default)]
struct EtagState { hash: Option<String>, last_modified: Option<String> }

fn http_date_now() -> String { chrono::Utc::now().format("%a, %d %b %Y %H:%M:%S GMT").to_string() }

fn http_date_ms(value: &str) -> f64 {
    chrono::DateTime::parse_from_rfc2822(value.trim()).map(|d| d.timestamp_millis() as f64)
        .unwrap_or_else(|_| crate::engine::routing::date_parse_ms(value))
}

/// ETag / Last-Modified negotiation shared by `/v1/models` and `/v1/openrouter/models`.
fn conditional_json(cell: &Mutex<EtagState>, headers: &HeaderMap, payload: String) -> Response {
    let hash = hex::encode(Sha256::digest(payload.as_bytes()));
    let last_modified = {
        let mut st = cell.lock().unwrap();
        if st.hash.as_deref() != Some(hash.as_str()) || st.last_modified.is_none() {
            st.hash = Some(hash.clone());
            st.last_modified = Some(http_date_now());
        }
        st.last_modified.clone().unwrap_or_default()
    };
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).map(str::to_owned);
    let if_none_match = header("if-none-match").filter(|v| !v.is_empty());
    let if_modified_since = header("if-modified-since").filter(|v| !v.is_empty());
    let etag_matches = if_none_match.as_deref().is_some_and(|inm| {
        let clean = inm.strip_prefix("W/").unwrap_or(inm);
        let clean = clean.strip_prefix('"').unwrap_or(clean);
        let clean = clean.strip_suffix('"').unwrap_or(clean);
        clean == hash || inm == format!("\"{hash}\"") || inm == hash
    });
    let last_modified_matches = if_modified_since.as_deref().is_some_and(|ims| ims == last_modified || http_date_ms(ims) >= http_date_ms(&last_modified));
    let not_modified = if if_none_match.is_some() { etag_matches } else { last_modified_matches };
    let mut response = if not_modified {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        ([(axum::http::header::CONTENT_TYPE, "application/json")], payload).into_response()
    };
    let h = response.headers_mut();
    h.insert(axum::http::header::ETAG, format!("\"{hash}\"").parse().unwrap());
    if let Ok(v) = last_modified.parse() { h.insert(axum::http::header::LAST_MODIFIED, v); }
    response
}

fn query_pairs(raw: &Option<String>) -> Vec<(String, String)> {
    raw.as_deref().map(|q| url::form_urlencoded::parse(q.as_bytes()).into_owned().collect()).unwrap_or_default()
}
/// Fastify's querystring parser keeps the first value as a string, repeated keys as arrays.
fn query_get(pairs: &[(String, String)], key: &str) -> Option<String> {
    pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
}

async fn list_models(State(state): State<AppState>, headers: HeaderMap, RawQuery(raw): RawQuery) -> Response {
    static ETAG: OnceLock<Mutex<EtagState>> = OnceLock::new();
    let pairs = query_pairs(&raw);
    let wants_ui = pairs.iter().any(|(k, _)| k == "ui");
    let snap = snapshot(&state).await;
    let created = model_created_at();
    let data: Vec<Value> = snap.models.iter().map(|(id, model)| model_entry(id, model, &snap, created)).collect();
    let count = data.len();
    let payload = js::stringify(&json!({"object": "list", "data": data}));
    if wants_ui {
        return ([(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")], render_models_ui_page(&payload, count)).into_response();
    }
    conditional_json(ETAG.get_or_init(Default::default), &headers, payload)
}

fn error(status: StatusCode, message: String) -> Response {
    (status, [(axum::http::header::CONTENT_TYPE, "application/json; charset=utf-8")], js::stringify(&json!({"error": message}))).into_response()
}

const SOURCES: [&str; 3] = ["openrouter", "models.dev", "catwalk"];
const INVALID_SOURCE: &str = "Missing or invalid 'source' parameter. Must be one of: openrouter, models.dev, catwalk";

/// JS `parseInt(s, 10)` (NaN → None).
fn parse_int(s: &str) -> Option<i64> {
    let t = s.trim_start();
    let (neg, digits) = match t.as_bytes().first() { Some(b'-') => (true, &t[1..]), Some(b'+') => (false, &t[1..]), _ => (false, t) };
    let end = digits.find(|c: char| !c.is_ascii_digit()).unwrap_or(digits.len());
    let n: i64 = digits[..end].parse().ok()?;
    Some(if neg { -n } else { n })
}

async fn metadata_search(RawQuery(raw): RawQuery) -> Response {
    let pairs = query_pairs(&raw);
    let Some(source) = query_get(&pairs, "source").filter(|s| SOURCES.contains(&s.as_str())) else {
        return error(StatusCode::BAD_REQUEST, INVALID_SOURCE.into());
    };
    if !crate::metadata::initialized(&source) {
        return error(StatusCode::SERVICE_UNAVAILABLE, format!("Metadata source '{source}' is not yet loaded or failed to load"));
    }
    let q = query_get(&pairs, "q").unwrap_or_default();
    let limit = match query_get(&pairs, "limit").filter(|l| !l.is_empty()) {
        Some(l) => parse_int(&l).filter(|n| *n != 0).unwrap_or(50).min(200),
        None => 50,
    };
    let mut results = crate::metadata::search(&source, &q, usize::MAX);
    // `results.slice(0, limit)` (negative limits count from the end).
    let len = results.len() as i64;
    let end = if limit < 0 { (len + limit).max(0) } else { limit.min(len) } as usize;
    results.truncate(end);
    let data: Vec<Value> = results.into_iter().map(|(id, name)| json!({"id": id, "name": name})).collect();
    let count = data.len();
    (StatusCode::OK, [(axum::http::header::CONTENT_TYPE, "application/json; charset=utf-8")], js::stringify(&json!({"data": data, "count": count}))).into_response()
}

async fn metadata_lookup(RawQuery(raw): RawQuery) -> Response {
    let pairs = query_pairs(&raw);
    let Some(source) = query_get(&pairs, "source").filter(|s| SOURCES.contains(&s.as_str())) else {
        return error(StatusCode::BAD_REQUEST, INVALID_SOURCE.into());
    };
    let Some(path) = query_get(&pairs, "source_path").filter(|p| !p.is_empty()) else {
        return error(StatusCode::BAD_REQUEST, "Missing 'source_path' parameter".into());
    };
    if !crate::metadata::initialized(&source) {
        return error(StatusCode::SERVICE_UNAVAILABLE, format!("Metadata source '{source}' is not yet loaded or failed to load"));
    }
    match catalog_metadata(&source, &path) {
        Some(metadata) => (StatusCode::OK, [(axum::http::header::CONTENT_TYPE, "application/json; charset=utf-8")], js::stringify(&json!({"data": metadata}))).into_response(),
        None => error(StatusCode::NOT_FOUND, format!("No metadata found for '{path}' in source '{source}'")),
    }
}

async fn openrouter_models(headers: HeaderMap, RawQuery(raw): RawQuery) -> Response {
    static ETAG: OnceLock<Mutex<EtagState>> = OnceLock::new();
    if !crate::metadata::initialized("openrouter") {
        return error(StatusCode::SERVICE_UNAVAILABLE, "OpenRouter pricing data not yet loaded".into());
    }
    let query = query_get(&query_pairs(&raw), "q").unwrap_or_default();
    let mut slugs = crate::metadata::all_ids("openrouter");
    if !query.is_empty() {
        let q = query.to_lowercase();
        slugs.retain(|s| s.to_lowercase().contains(&q));
        slugs.sort_by(|a, b| match (a.to_lowercase().starts_with(&q), b.to_lowercase().starts_with(&q)) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => crate::metadata::locale_compare(a, b),
        });
    }
    let count = slugs.len();
    conditional_json(ETAG.get_or_init(Default::default), &headers, js::stringify(&json!({"data": slugs, "count": count})))
}

// ─── Muse CLI catalog ────────────────────────────────────────────────────

fn muse_static_metadata() -> Value {
    json!({
        "family": "avocado",
        "release_date": "2026-09-02",
        "is_hidden": false,
        "options": {"reasoningEffort": "high", "forceReasoning": true, "include": ["reasoning.encrypted_content"], "temperature": 0.9, "top_p": 0.9},
        "variants": {
            "minimal": {"reasoningEffort": "minimal"}, "low": {"reasoningEffort": "low"}, "medium": {"reasoningEffort": "medium"},
            "high": {"reasoningEffort": "high"}, "xhigh": {"reasoningEffort": "xhigh"}, "max": {"reasoningEffort": "max"}
        }
    })
}

async fn muse_models(State(state): State<AppState>, request: axum::extract::Request) -> Response {
    if let Err(response) = crate::engine::dispatcher::routes::authenticate_peer(&state, request.headers(), request.uri().query(), crate::media_routes::peer_ip(request.extensions())) { return response; }
    let snap = snapshot(&state).await;
    let mut data = Vec::new();
    for (id, model) in &snap.models {
        let Some(metadata) = resolve_model_metadata(id, model, &snap.providers) else { continue };
        let pricing = metadata.get("pricing");
        let context = js::coalesce(metadata.get("context_length"), get(&metadata, &["top_provider", "context_length"]));
        let output = get(&metadata, &["top_provider", "max_completion_tokens"]).cloned();
        if !truthy(context.as_ref()) || !truthy(output.as_ref()) || !truthy(pricing.and_then(|p| p.get("prompt"))) || !truthy(pricing.and_then(|p| p.get("completion"))) { continue; }
        let pricing = pricing.unwrap();
        let caps: Vec<&str> = metadata.get("supported_parameters").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
        let identity = automatic_identity(id, model, &snap.providers);
        let pi_config = match model.get("pi_model").filter(|v| !v.is_null()) {
            Some(pi) => Some((js::to_string(pi.get("provider")), js::to_string(pi.get("model_id")))),
            None => identity.provider.clone().filter(|p| !p.is_empty()).map(|p| (p, identity.model.clone())),
        };
        let pi_model = pi_config.as_ref().and_then(|(p, m)| catalog_model(p, m));
        let input = get(&metadata, &["architecture", "input_modalities"]).filter(|v| !v.is_null());
        let output_modalities = get(&metadata, &["architecture", "output_modalities"]).filter(|v| !v.is_null());
        let mut muse = Obj::new();
        muse.insert("name".into(), json!(id));
        if let Value::Object(m) = muse_static_metadata() { muse.extend(m); }
        muse.insert("attachment".into(), json!(input.and_then(Value::as_array).is_some_and(|a| a.iter().any(|v| v == "image"))));
        muse.insert("reasoning".into(), pi_model.as_ref().and_then(|m| m.get("reasoning")).filter(|v| !v.is_null()).cloned().unwrap_or(json!(caps.contains(&"reasoning"))));
        muse.insert("temperature".into(), json!(caps.contains(&"temperature")));
        muse.insert("tool_call".into(), json!(caps.contains(&"tools") || caps.contains(&"tool_choice")));
        muse.insert("modalities".into(), json!({"input": input.cloned().unwrap_or(json!([])), "output": output_modalities.cloned().unwrap_or(json!([]))}));
        muse.insert("limit".into(), json!({"context": context, "output": output}));
        muse.insert("cost".into(), json!({
            "currency": "USD",
            "input": pricing["prompt"],
            "output": pricing["completion"],
            "cached": js::coalesce(pricing.get("input_cache_read"), pricing.get("prompt")),
        }));
        data.push(json!({"id": id, "object": "model", "created": model_created_at(), "owned_by": "meta", "metadata": {"muse-code": muse}}));
    }
    ([(axum::http::header::CONTENT_TYPE, "application/json")], js::stringify(&json!({"object": "list", "data": data}))).into_response()
}

// ─── models-ui.ts ────────────────────────────────────────────────────────

const VANILLA_JSONEDITOR_VERSION: &str = "3.13.0";

fn render_models_ui_page(payload_json: &str, model_count: usize) -> String {
    let safe_json = payload_json.replace("</", "<\\/");
    let cdn = format!("https://cdn.jsdelivr.net/npm/vanilla-jsoneditor@{VANILLA_JSONEDITOR_VERSION}");
    let plural = if model_count == 1 { "" } else { "s" };
    include_str!("assets/models-ui.html")
        .replace("${CDN_BASE}", &cdn)
        .replace("${modelCount}", &model_count.to_string())
        .replace("${PLURAL}", plural)
        .replace("${safeJson}", &safe_json)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn humanize_and_identity() {
        assert_eq!(humanize_model_name("openai/gpt-4o-mini"), "GPT 4o Mini");
        assert_eq!(infer_provider_from_model("o3"), Some("openai".into()));
        assert_eq!(infer_provider_from_model("o3x"), None);
        assert_eq!(parse_int(" -5abc"), Some(-5));
        let merged = merge_overrides(None, Some(&json!({"name": "X", "pricing": {"prompt": "1"}}))).unwrap();
        assert_eq!(js::stringify(&merged), r#"{"id":"","name":"X","pricing":{"prompt":"1"}}"#);
    }

    #[tokio::test]
    async fn public_list_has_aliases_only() {
        use tower::ServiceExt;
        let state = AppState::new(Some("admin".into()), std::collections::HashMap::new());
        state.inner.providers.write().unwrap().insert("openai".into(), Default::default());
        state.inner.aliases.write().unwrap().insert("gpt-4o-alias".into(), Default::default());
        let response = router().with_state(state)
            .oneshot(axum::http::Request::get("/v1/models").body(axum::body::Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers().contains_key("etag"));
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        let ids: Vec<&str> = json["data"].as_array().unwrap().iter().map(|m| m["id"].as_str().unwrap()).collect();
        assert_eq!(ids, vec!["gpt-4o-alias"]);
        assert_eq!(json["data"][0]["owned_by"], "plexus");
    }
}
