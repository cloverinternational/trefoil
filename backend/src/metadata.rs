//! In-process model metadata catalogs.  A failed source refresh never replaces
//! the last known-good catalog.

use std::sync::{OnceLock, RwLock};
use std::time::Instant;

use serde::{Deserialize, Serialize};

const DEFAULT_OPENROUTER: &str = "https://openrouter.ai/api/v1/models";
const DEFAULT_MODELS_DEV: &str = "https://models.dev/api.json";
const DEFAULT_CATWALK: &str = "https://catwalk.charm.sh/v2/providers";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ModelMetadata {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_length: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub architecture: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pricing: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supported_parameters: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_provider: Option<serde_json::Value>,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Openrouter,
    ModelsDev,
    Catwalk,
}

impl Source {
    fn name(self) -> &'static str {
        match self {
            Self::Openrouter => "openrouter",
            Self::ModelsDev => "models.dev",
            Self::Catwalk => "catwalk",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct SourceReport {
    pub source: String,
    pub initialized: bool,
    pub count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RefreshReport {
    pub success: bool,
    pub message: String,
    pub trigger: String,
    pub refreshed_at: String,
    pub duration_ms: u128,
    pub interval_minutes: u64,
    pub had_errors: bool,
    pub sources: indexmap::IndexMap<String, SourceReport>,
}

#[derive(Clone, Debug)]
pub struct SourceUrls {
    pub openrouter: String,
    pub models_dev: String,
    pub catwalk: String,
}
impl Default for SourceUrls {
    fn default() -> Self {
        Self {
            openrouter: DEFAULT_OPENROUTER.into(),
            models_dev: DEFAULT_MODELS_DEV.into(),
            catwalk: DEFAULT_CATWALK.into(),
        }
    }
}

#[derive(Default)]
struct Catalogs {
    openrouter: Catalog,
    models_dev: Catalog,
    catwalk: Catalog,
}
static CATALOGS: OnceLock<RwLock<Catalogs>> = OnceLock::new();
fn catalogs() -> &'static RwLock<Catalogs> {
    CATALOGS.get_or_init(|| RwLock::new(Catalogs::default()))
}

/// Insertion-ordered like the Plexus `Map`s (fetch order, first key position kept).
pub type Catalog = indexmap::IndexMap<String, ModelMetadata>;

fn positive(v: Option<f64>) -> Option<u64> {
    v.filter(|v| *v > 0.0).map(|v| v as u64)
}
fn num(v: &serde_json::Value) -> Option<f64> { v.as_f64() }
/// `String(perMillion / 1_000_000)` with JS number text.
fn per_token(v: &serde_json::Value) -> Option<serde_json::Value> {
    if v.is_null() { return None; }
    v.as_f64().map(|n| serde_json::Value::String(crate::engine::js::js_number_text(n / 1_000_000.0)))
}
fn obj(pairs: Vec<(&str, Option<serde_json::Value>)>) -> serde_json::Value {
    serde_json::Value::Object(pairs.into_iter().filter_map(|(k, v)| v.map(|v| (k.to_owned(), v))).collect())
}
fn top_provider(context: Option<f64>, output: Option<f64>) -> Option<serde_json::Value> {
    let (context, output) = (positive(context), positive(output));
    (context.is_some() || output.is_some()).then(|| obj(vec![("context_length", context.map(Into::into)), ("max_completion_tokens", output.map(Into::into))]))
}
fn opt(v: Option<&serde_json::Value>) -> Option<serde_json::Value> { v.filter(|v| !v.is_null()).cloned() }

/// `normalizeOpenRouterModel(raw, kind)`.
fn openrouter_model(row: &serde_json::Value, kind: &str) -> ModelMetadata {
    let id = row["id"].as_str().unwrap_or_default();
    let architecture = opt(row.get("architecture")).or_else(|| (kind == "videos").then(|| {
        let mut input = vec!["text"];
        if row["supported_frame_images"].as_array().is_some_and(|a| !a.is_empty()) { input.push("image"); }
        let mut output = vec!["video"];
        if crate::engine::js::truthy(row.get("generate_audio")) { output.push("audio"); }
        serde_json::json!({"modality": format!("{}->{}", input.join("+"), output.join("+")), "input_modalities": input, "output_modalities": output})
    }));
    let pricing = opt(row.get("pricing")).map(|p| {
        let tiers = opt(p.get("tiers")).or_else(|| p.get("overrides").and_then(|o| o.as_array()).map(|o| {
            o.iter().filter(|t| t.get("min_prompt_tokens").is_some()).map(|t| obj(vec![
                ("input_tokens_above", t.get("min_prompt_tokens").cloned()),
                ("prompt", t.get("prompt").cloned()),
                ("completion", t.get("completion").cloned()),
                ("input_cache_read", t.get("input_cache_read").cloned()),
                ("input_cache_write", t.get("input_cache_write").cloned()),
            ])).collect::<Vec<_>>().into()
        }));
        obj(vec![
            ("prompt", opt(p.get("prompt"))),
            ("completion", opt(p.get("completion"))),
            ("input_cache_read", opt(p.get("input_cache_read"))),
            ("input_cache_write", opt(p.get("input_cache_write"))),
            ("tiers", tiers),
        ])
    });
    ModelMetadata {
        id: id.into(),
        name: row["name"].as_str().unwrap_or(id).into(),
        description: row["description"].as_str().map(Into::into),
        context_length: positive(num(&row["context_length"])),
        architecture,
        pricing,
        supported_parameters: row["supported_parameters"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(Into::into)).collect()),
        top_provider: top_provider(num(&row["top_provider"]["context_length"]), num(&row["top_provider"]["max_completion_tokens"])),
    }
}
fn openrouter(raw: serde_json::Value, kind: &str, out: &mut Catalog) -> Result<(), String> {
    let rows = raw.get("data").and_then(|v| v.as_array()).ok_or_else(|| format!("Invalid OpenRouter {kind} response format"))?;
    for row in rows {
        if row["id"].as_str().is_some_and(|id| !id.is_empty()) {
            out.insert(row["id"].as_str().unwrap().into(), openrouter_model(row, kind));
        }
    }
    Ok(())
}
fn models_dev(raw: serde_json::Value) -> Result<Catalog, String> {
    let object = raw.as_object().ok_or("Invalid models.dev response format")?;
    let mut out = Catalog::new();
    for (provider, value) in object {
        match value.get("models") {
            Some(serde_json::Value::Array(rows)) => for row in rows {
                if let Some(id) = row["id"].as_str().filter(|s| !s.is_empty()) { out.insert(format!("{provider}.{id}"), dev_model(provider, id, row)); }
            },
            Some(serde_json::Value::Object(map)) => for (id, row) in map {
                if !row.is_null() { out.insert(format!("{provider}.{id}"), dev_model(provider, id, row)); }
            },
            _ => {}
        }
    }
    if out.is_empty() { Err("Invalid models.dev response format".into()) } else { Ok(out) }
}
/// `normalizeModelsDevModel(providerId, modelId, raw)`.
fn dev_model(provider: &str, id: &str, row: &serde_json::Value) -> ModelMetadata {
    let input = row.pointer("/modalities/input").and_then(|v| v.as_array());
    let output = row.pointer("/modalities/output").and_then(|v| v.as_array());
    let join = |a: Option<&Vec<serde_json::Value>>| a.map(|a| a.iter().map(|v| crate::engine::js::to_string(Some(v))).collect::<Vec<_>>().join("+"));
    let modality = (input.is_some_and(|a| !a.is_empty()) || output.is_some_and(|a| !a.is_empty()))
        .then(|| format!("{}->{}", join(input).unwrap_or_default(), join(output).unwrap_or_default()).into());
    let architecture = obj(vec![("modality", modality), ("input_modalities", input.cloned().map(Into::into)), ("output_modalities", output.cloned().map(Into::into))]);
    let mut params: Vec<String> = Vec::new();
    let t = |k: &str| crate::engine::js::truthy(row.get(k));
    if t("tool_call") { params.extend(["tools".into(), "tool_choice".into()]); }
    if t("temperature") { params.push("temperature".into()); }
    if t("reasoning") { params.push("reasoning".into()); }
    if t("attachment") { params.push("image".into()); }
    let cost = &row["cost"];
    let pricing = (!cost["input"].is_null() || !cost["output"].is_null()).then(|| {
        let tiers = cost.get("tiers").filter(|v| crate::engine::js::truthy(Some(v))).map(|tiers| {
            tiers.as_array().into_iter().flatten().filter_map(|tier| {
                let above = opt(tier.pointer("/tier/size")).or_else(|| opt(tier.get("size")))?;
                Some(obj(vec![
                    ("input_tokens_above", Some(above)),
                    ("prompt", per_token(&tier["input"])),
                    ("completion", per_token(&tier["output"])),
                    ("input_cache_read", per_token(&tier["cache_read"])),
                    ("input_cache_write", per_token(&tier["cache_write"])),
                ]))
            }).collect::<Vec<_>>().into()
        });
        obj(vec![
            ("prompt", per_token(&cost["input"])),
            ("completion", per_token(&cost["output"])),
            ("input_cache_read", per_token(&cost["cache_read"])),
            ("input_cache_write", per_token(&cost["cache_write"])),
            ("tiers", tiers),
        ])
    });
    ModelMetadata {
        id: format!("{provider}.{id}"),
        name: row["name"].as_str().or_else(|| row["id"].as_str()).unwrap_or(id).into(),
        description: None,
        context_length: positive(num(&row["limit"]["context"])),
        architecture: Some(architecture),
        pricing,
        supported_parameters: (!params.is_empty()).then_some(params),
        top_provider: top_provider(num(&row["limit"]["context"]), num(&row["limit"]["output"])),
    }
}
fn catwalk(raw: serde_json::Value) -> Result<Catalog, String> {
    let providers = raw.as_array().ok_or("Invalid Catwalk response format")?;
    let mut out = Catalog::new();
    for provider in providers {
        let (Some(pid), Some(rows)) = (provider["id"].as_str().filter(|s| !s.is_empty()), provider["models"].as_array()) else { continue };
        for row in rows {
            let Some(id) = row["id"].as_str().filter(|s| !s.is_empty()) else { continue };
            let attach = crate::engine::js::truthy(row.get("supports_attachments"));
            let input: Vec<&str> = if attach { vec!["text", "image"] } else { vec!["text"] };
            let mut params: Vec<String> = vec!["temperature".into(), "max_tokens".into()];
            if crate::engine::js::truthy(row.get("can_reason")) { params.push("reasoning".into()); }
            if attach { params.push("image".into()); }
            let pricing = (!row["cost_per_1m_in"].is_null() || !row["cost_per_1m_out"].is_null()).then(|| obj(vec![
                ("prompt", per_token(&row["cost_per_1m_in"])),
                ("completion", per_token(&row["cost_per_1m_out"])),
                ("input_cache_read", per_token(&row["cost_per_1m_in_cached"])),
            ]));
            out.insert(format!("{pid}.{id}"), ModelMetadata {
                id: format!("{pid}.{id}"),
                name: row["name"].as_str().unwrap_or(id).into(),
                description: None,
                context_length: positive(num(&row["context_window"])),
                architecture: Some(serde_json::json!({"modality": format!("{}->text", input.join("+")), "input_modalities": input, "output_modalities": ["text"]})),
                pricing,
                supported_parameters: Some(params),
                top_provider: top_provider(num(&row["context_window"]), num(&row["default_max_tokens"])),
            });
        }
    }
    if out.is_empty() { Err("Invalid Catwalk response format".into()) } else { Ok(out) }
}
async fn fetch(url: &str) -> Result<serde_json::Value, String> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?
        .get(url)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())
}

async fn fetch_openrouter(source: &str) -> Result<Catalog, String> {
    let mut catalog = Catalog::new();
    openrouter(fetch(source).await?, "models", &mut catalog)?;
    if let Ok(mut url) = url::Url::parse(source) {
        if let Some(prefix) = url.path().strip_suffix("/models").map(str::to_owned) {
            for kind in ["embeddings", "videos"] {
                url.set_path(&format!("{prefix}/{kind}/models"));
                if let Ok(raw) = fetch(url.as_str()).await { let _ = openrouter(raw, kind, &mut catalog); }
            }
        }
    }
    if catalog.is_empty() { return Err("Invalid OpenRouter response format".into()); }
    Ok(catalog)
}

type RefreshFuture = futures_util::future::Shared<std::pin::Pin<Box<dyn std::future::Future<Output=RefreshReport> + Send>>>;
static IN_FLIGHT: OnceLock<tokio::sync::Mutex<Option<(u64, RefreshFuture)>>> = OnceLock::new();
static REFRESH_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub async fn refresh(urls: SourceUrls, trigger: &str) -> RefreshReport {
    use futures_util::FutureExt;
    let gate=IN_FLIGHT.get_or_init(||tokio::sync::Mutex::new(None));
    let (id, future)={
        let mut slot=gate.lock().await;
        if let Some((id,future))=slot.as_ref() { (*id,future.clone()) }
        else {
            let id=REFRESH_ID.fetch_add(1,std::sync::atomic::Ordering::Relaxed);
            let trigger=trigger.to_owned();
            let future=Box::pin(async move { refresh_catalogs(urls,&trigger).await }) as std::pin::Pin<Box<dyn std::future::Future<Output=RefreshReport> + Send>>;
            let future=future.shared();
            *slot=Some((id,future.clone()));
            (id,future)
        }
    };
    let result=future.await;
    let mut slot=gate.lock().await;
    if slot.as_ref().is_some_and(|(current,_)|*current==id) { *slot=None; }
    result
}

async fn refresh_catalogs(urls: SourceUrls, trigger: &str) -> RefreshReport {
    let start = Instant::now();
    let refreshed_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis,true);
    let (or, md, cw) = tokio::join!(
        fetch_openrouter(&urls.openrouter),
        fetch(&urls.models_dev),
        fetch(&urls.catwalk)
    );
    let mut reports = indexmap::IndexMap::new();
    let mut errors = false;
    let mut guard = catalogs().write().unwrap();
    for (source, result) in [
        (Source::Openrouter, Ok(or)),
        (Source::ModelsDev, md.map(models_dev)),
        (Source::Catwalk, cw.map(catwalk)),
    ] {
        let parsed = result.and_then(|v| v);
        let (catalog, initialized, count, error) = match parsed {
            Ok(map) => {
                let count = map.len();
                match source {
                    Source::Openrouter => guard.openrouter = map,
                    Source::ModelsDev => guard.models_dev = map,
                    Source::Catwalk => guard.catwalk = map,
                };
                (count, true, count, None)
            }
            Err(e) => {
                errors = true;
                let count = match source {
                    Source::Openrouter => guard.openrouter.len(),
                    Source::ModelsDev => guard.models_dev.len(),
                    Source::Catwalk => guard.catwalk.len(),
                };
                (count, count > 0, count, Some(e))
            }
        };
        let _ = catalog;
        reports.insert(
            (if source == Source::ModelsDev { "modelsDev" } else { source.name() }).into(),
            SourceReport {
                source: source.name().into(),
                initialized,
                count,
                error,
            },
        );
    }
    RefreshReport {
        success: !errors,
        message: if errors {
            "Model metadata refresh completed with errors"
        } else {
            "Model metadata refresh completed successfully"
        }
        .into(),
        trigger: trigger.into(),
        refreshed_at,
        duration_ms: start.elapsed().as_millis(),
        interval_minutes: 60,
        had_errors: errors,
        sources: reports,
    }
}

/// One startup refresh and an hourly refresh thereafter. The task owns no
/// credentials and keeps the last good catalog when a public source fails.
pub fn start_scheduler(state: crate::AppState) -> tokio::task::JoinHandle<()> {
    let log = |state: &crate::AppState, report: &RefreshReport| {
        let module = [("module".into(), serde_json::json!("model-metadata-manager"))];
        if report.had_errors {
            crate::system_logs::event(state, "warn", format!("Model metadata refresh ({}) completed with errors", report.trigger), module);
        } else {
            crate::system_logs::event(state, "info", format!("Model metadata refresh ({}) completed in {}ms", report.trigger, report.duration_ms), module);
        }
    };
    crate::system_logs::event(&state, "info", "Scheduled model metadata auto-refresh every 60 minutes", [("module".into(), serde_json::json!("model-metadata-manager")), ("functionName".into(), serde_json::json!("startAutoRefresh"))]);
    tokio::spawn(async move {
        log(&state, &refresh(SourceUrls::default(), "startup").await);
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
            log(&state, &refresh(SourceUrls::default(), "scheduled").await);
        }
    })
}

/// Auto-refresh cadence wired at startup (Plexus `startAutoRefresh(60)`).
pub const AUTO_REFRESH_MINUTES: u64 = 60;

/// Number of models loaded for one source (Plexus `getAllIds(source).length`).
pub fn count(source: &str) -> usize {
    let guard = catalogs().read().unwrap();
    match source {
        "openrouter" => guard.openrouter.len(),
        "models.dev" => guard.models_dev.len(),
        "catwalk" => guard.catwalk.len(),
        _ => 0,
    }
}

pub fn initialized(source: &str) -> bool {
    count(source) > 0
}

pub fn lookup(source: &str, id: &str) -> Option<ModelMetadata> {
    let guard=catalogs().read().unwrap();
    let map=match source {"openrouter"=>&guard.openrouter,"models.dev"=>&guard.models_dev,"catwalk"=>&guard.catwalk,_=>return None};
    map.get(id).cloned()
}

/// Ids of one source in catalog order (Plexus `getAllIds`).
pub fn all_ids(source: &str) -> Vec<String> {
    let guard = catalogs().read().unwrap();
    let map = match source { "openrouter" => &guard.openrouter, "models.dev" => &guard.models_dev, "catwalk" => &guard.catwalk, _ => return vec![] };
    map.keys().cloned().collect()
}

/// `ModelMetadataManager.search(source, query, limit)`: `(id, name)` pairs.
pub fn search(source: &str, query: &str, limit: usize) -> Vec<(String, String)> {
    let guard = catalogs().read().unwrap();
    let map = match source {
        "openrouter" => &guard.openrouter,
        "models.dev" => &guard.models_dev,
        "catwalk" => &guard.catwalk,
        _ => return vec![],
    };
    let q = query.to_lowercase();
    let mut results: Vec<(String, String)> = map.iter().filter(|(key, m)| {
        if query.is_empty() { return true; }
        let arch = m.architecture.as_ref();
        let mut parts: Vec<&str> = vec![key.as_str(), m.name.as_str()];
        if let Some(d) = &m.description { parts.push(d); }
        if let Some(a) = arch {
            if let Some(x) = a["modality"].as_str() { parts.push(x); }
            for k in ["input_modalities", "output_modalities"] {
                parts.extend(a[k].as_array().into_iter().flatten().filter_map(|v| v.as_str()));
            }
        }
        parts.join(" ").to_lowercase().contains(&q)
    }).map(|(k, m)| (k.clone(), m.name.clone())).collect();
    if !query.is_empty() {
        results.sort_by(|a, b| {
            let starts = |x: &(String, String)| x.0.to_lowercase().starts_with(&q) || x.1.to_lowercase().starts_with(&q);
            match (starts(a), starts(b)) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => locale_compare(&a.0, &b.0),
            }
        });
    }
    results.truncate(limit);
    results
}

/// Approximation of ICU root-locale `localeCompare` (Bun/JSC) for ASCII ids:
/// whitespace < punctuation/symbols (CLDR root order) < digits < letters,
/// compared case-insensitively first, lowercase before uppercase on ties.
pub fn locale_compare(a: &str, b: &str) -> std::cmp::Ordering {
    const PUNCT: &str = "_-,;:!?.'\"()[]{}@*/\\&#%`^+<=>|~$";
    fn key(c: char) -> (u8, u32) {
        let l = c.to_ascii_lowercase();
        if c.is_whitespace() { (0, c as u32) }
        else if let Some(i) = PUNCT.find(c) { (1, i as u32) }
        else if l.is_ascii_digit() { (2, l as u32) }
        else if l.is_ascii_alphabetic() { (3, l as u32) }
        else { (4, c as u32) }
    }
    let primary = a.chars().map(key).cmp(b.chars().map(key));
    primary.then_with(|| a.chars().map(|c| c.is_ascii_uppercase()).cmp(b.chars().map(|c| c.is_ascii_uppercase())))
}

/// Admin-only preview of automatic model identity, metadata and API selection.
pub async fn resolve_metadata(
    axum::extract::State(state): axum::extract::State<crate::AppState>,
    headers: axum::http::HeaderMap,
    axum::extract::Json(body): axum::extract::Json<serde_json::Value>,
) -> axum::response::Response {
    use axum::{http::StatusCode, response::IntoResponse, Json};
    use crate::auth::{resolve_principal, Principal};

    match resolve_principal(&state, headers.get("x-admin-key").and_then(|v| v.to_str().ok())) {
        Some(Principal::Admin) => (),
        Some(Principal::Limited { .. }) => return (StatusCode::FORBIDDEN, Json(serde_json::json!({"error":{"message":"Admin privileges required","type":"forbidden","code":403}}))).into_response(),
        None => return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error":{"message":"Unauthorized","type":"auth_error","code":401}}))).into_response(),
    }

    let Some(alias_id) = body.get("alias_id").and_then(serde_json::Value::as_str) else {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error":"alias_id is required"}))).into_response();
    };
    // `ModelConfigSchema.safeParse(body.model)`.
    let model_view = match crate::alias_view::model_config().safe_parse(body.get("model").unwrap_or(&serde_json::Value::Null)) {
        Ok(view) => view,
        Err(issues) => return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error":"Validation failed","details":issues}))).into_response(),
    };
    let parsed = crate::alias_view::from_view(model_view.clone());

    let providers = state.inner.providers.read().unwrap();
    let identity = automatic_identity(alias_id, &parsed, &providers);
    // `resolveModelMetadata(alias_id, parsed.data, providers)`: same resolution
    // as `/v1/models`, including the derived/overridden `short_name`.
    let provider_views: serde_json::Map<String, serde_json::Value> = providers.iter().map(|(k, p)| (k.clone(), p.public_view())).collect();
    let resolved = crate::models_routes::resolve_model_metadata_with_source(alias_id, &model_view, &provider_views);
    let preferred_api = parsed.preferred_api.clone().or_else(|| {
        if parsed.model_type.as_deref().is_some_and(|t| t != "text") { return None; }
        let provider = identity.get("provider").and_then(|v| v.as_str()).unwrap_or("").to_lowercase();
        let model = identity.get("model").and_then(|v| v.as_str()).unwrap_or("").rsplit('/').next().unwrap_or("").to_lowercase();
        Some(if provider.contains("anthropic") || model.contains("claude") { vec!["messages"] }
        else if regex::Regex::new(r"^(gpt|o[134])(?:-|$)").unwrap().is_match(&model) { vec!["responses"] }
        else if regex::Regex::new(r"^gemini(?:-|$)").unwrap().is_match(&model) { vec!["gemini"] }
        else { vec!["chat_completions"] }.into_iter().map(str::to_string).collect())
    });
    // Plexus: getCatalogModel(identity.provider, identity.model) — the pi-ai
    // built-in catalog merged with the pi.dev overlay.
    let pi_model = identity.get("provider").and_then(|p| p.as_str()).filter(|p| !p.is_empty()).and_then(|provider| {
        let model = identity.get("model")?.as_str()?;
        crate::pi_catalog::catalog().get_model(provider, model).map(|m| serde_json::json!({"provider":provider,"model_id":model,"name":m.get("name")}))
    }).unwrap_or(serde_json::Value::Null);
    let metadata = resolved.map(|(metadata, source, path)| {
        let mut out = serde_json::Map::new();
        out.insert("source".into(), serde_json::json!(source));
        if let Some(path) = path { out.insert("source_path".into(), serde_json::json!(path)); }
        out.insert("name".into(), metadata.get("name").cloned().unwrap_or(serde_json::Value::Null));
        out.insert("short_name".into(), metadata.get("short_name").cloned().unwrap_or(serde_json::Value::Null));
        serde_json::Value::Object(out)
    }).unwrap_or(serde_json::Value::Null);
    Json(serde_json::json!({"canonical_model":identity,"pi_model":pi_model,"metadata":metadata,"preferred_api":preferred_api})).into_response()
}

fn infer_provider(model: &str) -> Option<String> {
    let name = model.rsplit('/').next().unwrap_or(model).to_lowercase();
    if name.contains("claude") { Some("anthropic".into()) }
    else if name.starts_with("gpt") || ["o1", "o3", "o4", "text-embedding", "dall-e"].iter().any(|p| name.starts_with(p)) { Some("openai".into()) }
    else if name.contains("gemini") || name.contains("imagen") { Some("google".into()) }
    else if name.contains("mistral") || name.contains("codestral") { Some("mistralai".into()) } else { None }
}

fn automatic_identity(alias: &str, config: &crate::state::ModelConfig, providers: &std::collections::HashMap<String, crate::state::ProviderConfig>) -> serde_json::Value {
    if let Some(pi) = config.pi_model.as_ref().filter(|v| v.is_object()) {
        if let (Some(provider), Some(model)) = (pi["provider"].as_str(), pi["model_id"].as_str()) {
            let normalized = match provider { "openai-codex" => "openai", _ => provider };
            return serde_json::json!({"provider":normalized,"model":model,"basis":"pi_model"});
        }
    }
    let mut identities = Vec::new();
    for target in config.target_groups.iter().flat_map(|g| &g.targets).filter(|t| t.enabled && t.alias.is_none()) {
        if let (Some(provider_id), Some(model)) = (target.provider.as_deref(), target.model.as_deref()) {
            let provider = providers.get(provider_id);
            let provider_name = provider.and_then(|p| p.oauth_provider.clone()).or_else(|| infer_provider(model)).unwrap_or_else(|| provider_id.to_string());
            let normalized = if provider_name == "openai-codex" { "openai".to_string() } else { provider_name };
            identities.push((normalized, model.to_string()));
        }
    }
    identities.sort(); identities.dedup();
    if identities.len() == 1 { let (provider, model)=identities.remove(0); serde_json::json!({"provider":provider,"model":model,"basis":"target"}) }
    else { serde_json::json!({"provider":infer_provider(alias),"model":alias,"basis":"alias"}) }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn malformed_keeps_catalog() {
        assert!(openrouter(serde_json::json!({"rows":[]}), "models", &mut Catalog::new()).is_err());
    }
    #[test]
    fn normalizes_openrouter() {
        let mut m = Catalog::new();
        openrouter(serde_json::json!({"data":[{"id":"x","context_length":0}]}), "models", &mut m).unwrap();
        assert_eq!(m["x"].name, "x");
        assert_eq!(m["x"].context_length, None);
    }
}

#[cfg(test)]
mod upstream_tests {
    use super::*;
    use axum::{routing::get, Json, Router};
    #[tokio::test]
    async fn loads_auxiliary_catalogs_and_retains_primary_on_auxiliary_failure() {
        let router = Router::new()
            .route("/models", get(|| async { Json(serde_json::json!({"data":[{"id":"chat"}]})) }))
            .route("/embeddings/models", get(|| async { Json(serde_json::json!({"data":[{"id":"embed"}]})) }))
            .route("/videos/models", get(|| async { axum::http::StatusCode::SERVICE_UNAVAILABLE }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let catalog = fetch_openrouter(&format!("http://{addr}/models")).await.unwrap();
        assert_eq!(catalog.len(), 2);
        assert!(catalog.contains_key("chat") && catalog.contains_key("embed"));
        server.abort();
    }
}

#[cfg(test)]
mod consumer_tests {
    use super::*;
    use axum::{body::Body,http::Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    #[tokio::test]
    async fn metadata_routes_read_refreshed_catalog_not_aliases() {
        let model=ModelMetadata{id:"fixture/vision".into(),name:"Fixture vision".into(),description:None,context_length:Some(64000),architecture:None,pricing:None,supported_parameters:None,top_provider:None};
        catalogs().write().unwrap().openrouter.insert(model.id.clone(),model);
        let state=crate::AppState::new(Some("admin".into()),Default::default());
        for (path,lookup) in [("/v1/metadata/search?source=openrouter&q=fixture/vision",false),("/v1/metadata/lookup?source=openrouter&source_path=fixture/vision",true)] {
            let response=crate::app(state.clone()).oneshot(Request::get(path).header("x-api-key","admin").body(Body::empty()).unwrap()).await.unwrap();
            assert_eq!(response.status(),axum::http::StatusCode::OK);
            let payload:serde_json::Value=serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
            if lookup {assert_eq!(payload["data"]["context_length"],64000);} else {assert_eq!(payload["data"][0]["name"],"Fixture vision");}
        }
        catalogs().write().unwrap().openrouter.shift_remove("fixture/vision");
    }
}

#[cfg(test)]
mod normalization_tests {
    use super::*;
    #[test]
    fn models_dev_converts_million_token_prices_and_capabilities() {
        let model=dev_model("provider","model",&serde_json::json!({"limit":{"context":128000,"output":4096},"cost":{"input":3,"output":15,"cache_read":0.3},"modalities":{"input":["text","image"],"output":["text"]},"tool_call":true,"reasoning":true}));
        assert_eq!(model.pricing.as_ref().unwrap()["prompt"],"0.000003");
        assert_eq!(model.architecture.as_ref().unwrap()["modality"],"text+image->text");
        assert_eq!(model.top_provider.as_ref().unwrap()["max_completion_tokens"],4096);
        assert_eq!(model.supported_parameters.unwrap(),vec!["tools","tool_choice","reasoning"]);
    }
    #[test]
    fn catwalk_preserves_vision_reasoning_prices_and_output_limits() {
        let map=catwalk(serde_json::json!([{"id":"provider","models":[{"id":"model","context_window":64000,"default_max_tokens":2048,"cost_per_1m_in":2,"supports_attachments":true,"can_reason":true}]}])).unwrap();
        let model=&map["provider.model"];
        assert_eq!(model.pricing.as_ref().unwrap()["prompt"],"0.000002");
        assert_eq!(model.architecture.as_ref().unwrap()["input_modalities"],serde_json::json!(["text","image"]));
        assert!(model.supported_parameters.as_ref().unwrap().contains(&"reasoning".into()));
    }
}

#[cfg(test)]
mod refresh_coalescing_tests {
    use super::*;
    use axum::{routing::get,Json,Router};
    use std::sync::{Arc,atomic::{AtomicUsize,Ordering}};
    #[tokio::test]
    async fn simultaneous_refreshes_share_source_requests_and_original_trigger() {
        let hits=Arc::new(AtomicUsize::new(0));let count=hits.clone();
        let app=Router::new().route("/catalog",get(move || {let count=count.clone();async move {count.fetch_add(1,Ordering::SeqCst);tokio::time::sleep(std::time::Duration::from_millis(25)).await;Json(serde_json::json!({"data":[{"id":"coalesce-fixture"}]}))}}));
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let addr=listener.local_addr().unwrap();
        let server=tokio::spawn(async move {axum::serve(listener,app).await.unwrap()});
        let urls=SourceUrls{openrouter:format!("http://{addr}/catalog"),models_dev:format!("http://{addr}/catalog"),catwalk:format!("http://{addr}/catalog")};
        let (first,second)=tokio::join!(refresh(urls.clone(),"startup"),refresh(urls,"manual"));
        assert_eq!(hits.load(Ordering::SeqCst),3);
        assert_eq!(first.trigger,second.trigger);
        assert_eq!(first.refreshed_at,second.refreshed_at);
        assert_eq!(first.duration_ms,second.duration_ms);
        server.abort();
    }
}
