//! In-process model metadata catalogs.  A failed source refresh never replaces
//! the last known-good catalog.

use std::collections::BTreeMap;
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
    pub sources: BTreeMap<String, SourceReport>,
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
    openrouter: BTreeMap<String, ModelMetadata>,
    models_dev: BTreeMap<String, ModelMetadata>,
    catwalk: BTreeMap<String, ModelMetadata>,
}
static CATALOGS: OnceLock<RwLock<Catalogs>> = OnceLock::new();
fn catalogs() -> &'static RwLock<Catalogs> {
    CATALOGS.get_or_init(|| RwLock::new(Catalogs::default()))
}

fn positive(v: Option<u64>) -> Option<u64> {
    v.filter(|v| *v > 0)
}
fn openrouter(
    raw: serde_json::Value,
    kind: &str,
) -> Result<BTreeMap<String, ModelMetadata>, String> {
    let rows = raw
        .get("data")
        .and_then(|v| v.as_array())
        .ok_or("invalid OpenRouter response format")?;
    let mut out = BTreeMap::new();
    for row in rows {
        let id = row
            .get("id")
            .and_then(|v| v.as_str())
            .filter(|v| !v.is_empty())
            .ok_or("OpenRouter model is missing id")?;
        let architecture = row.get("architecture").cloned().or_else(|| (kind == "videos").then(|| serde_json::json!({"modality":"text->video","input_modalities":["text"],"output_modalities":["video"]})));
        out.insert(
            id.into(),
            ModelMetadata {
                id: id.into(),
                name: row
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or(id)
                    .into(),
                description: row
                    .get("description")
                    .and_then(|v| v.as_str())
                    .map(Into::into),
                context_length: positive(row.get("context_length").and_then(|v| v.as_u64())),
                architecture,
                pricing: row.get("pricing").cloned(),
                top_provider: row.get("top_provider").cloned(),
                supported_parameters: row
                    .get("supported_parameters")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(Into::into))
                            .collect()
                    }),
            },
        );
    }
    if out.is_empty() {
        return Err("invalid OpenRouter response format".into());
    }
    Ok(out)
}
fn models_dev(raw: serde_json::Value) -> Result<BTreeMap<String, ModelMetadata>, String> {
    let object = raw
        .as_object()
        .ok_or("invalid models.dev response format")?;
    let mut out = BTreeMap::new();
    for (provider, value) in object {
        let models = value
            .get("models")
            .ok_or_else(|| format!("models.dev provider {provider} is missing models"))?;
        if let Some(map) = models.as_object() {
            for (id, row) in map {
                out.insert(format!("{provider}.{id}"), dev_model(provider, id, row));
            }
        } else if let Some(rows) = models.as_array() {
            for row in rows {
                let id = row
                    .get("id")
                    .and_then(|v| v.as_str())
                    .ok_or("models.dev model is missing id")?;
                out.insert(format!("{provider}.{id}"), dev_model(provider, id, row));
            }
        } else {
            return Err("invalid models.dev models format".into());
        }
    }
    if out.is_empty() {
        Err("invalid models.dev response format".into())
    } else {
        Ok(out)
    }
}
fn per_token_pricing(row: &serde_json::Value, fields: &[(&str,&str)]) -> Option<serde_json::Value> {
    let mut pricing=serde_json::Map::new();
    for (input,output) in fields {
        if let Some(value)=row.get(*input).and_then(serde_json::Value::as_f64) {
            pricing.insert((*output).into(),serde_json::Value::String((value/1_000_000.0).to_string()));
        }
    }
    if pricing.is_empty() {None} else {Some(serde_json::Value::Object(pricing))}
}
fn top_provider(context: Option<u64>, output: Option<u64>) -> Option<serde_json::Value> {
    let mut value=serde_json::Map::new();
    if let Some(context)=positive(context) {value.insert("context_length".into(),context.into());}
    if let Some(output)=positive(output) {value.insert("max_completion_tokens".into(),output.into());}
    if value.is_empty() {None} else {Some(value.into())}
}
fn dev_model(provider: &str, id: &str, row: &serde_json::Value) -> ModelMetadata {
    let input=row.pointer("/modalities/input").and_then(|v|v.as_array());
    let output=row.pointer("/modalities/output").and_then(|v|v.as_array());
    let mut architecture=serde_json::Map::new();
    if let Some(input)=input {architecture.insert("input_modalities".into(),input.clone().into());}
    if let Some(output)=output {architecture.insert("output_modalities".into(),output.clone().into());}
    if input.is_some_and(|a|!a.is_empty()) || output.is_some_and(|a|!a.is_empty()) {
        let join=|array:Option<&Vec<serde_json::Value>>|array.into_iter().flatten().filter_map(|v|v.as_str()).collect::<Vec<_>>().join("+");
        architecture.insert("modality".into(),format!("{}->{}",join(input),join(output)).into());
    }
    let mut params=Vec::new();
    if row["tool_call"]==true {params.extend(["tools".into(),"tool_choice".into()]);}
    for (flag,param) in [("temperature","temperature"),("reasoning","reasoning"),("attachment","image")] {if row[flag]==true {params.push(param.into());}}
    ModelMetadata {
        id:format!("{provider}.{id}"),name:row["name"].as_str().or_else(||row["id"].as_str()).unwrap_or(id).into(),description:None,
        context_length:positive(row.pointer("/limit/context").and_then(|v|v.as_u64())),architecture:Some(architecture.into()),
        pricing:per_token_pricing(&row["cost"],&[("input","prompt"),("output","completion"),("cache_read","input_cache_read"),("cache_write","input_cache_write")]),
        supported_parameters:(!params.is_empty()).then_some(params),
        top_provider:top_provider(row.pointer("/limit/context").and_then(|v|v.as_u64()),row.pointer("/limit/output").and_then(|v|v.as_u64())),
    }
}
fn catwalk(raw: serde_json::Value) -> Result<BTreeMap<String, ModelMetadata>, String> {
    let providers = raw.as_array().ok_or("invalid Catwalk response format")?;
    let mut out = BTreeMap::new();
    for provider in providers {
        let pid = provider
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or("Catwalk provider is missing id")?;
        let rows = provider
            .get("models")
            .and_then(|v| v.as_array())
            .ok_or("Catwalk provider is missing models")?;
        for row in rows {
            let id = row
                .get("id")
                .and_then(|v| v.as_str())
                .ok_or("Catwalk model is missing id")?;
            out.insert(format!("{pid}.{id}"), ModelMetadata { id: format!("{pid}.{id}"), name: row.get("name").and_then(|v| v.as_str()).unwrap_or(id).into(), description: None, context_length: positive(row.get("context_window").and_then(|v| v.as_u64())), architecture: Some(if row["supports_attachments"]==true {serde_json::json!({"modality":"text+image->text","input_modalities":["text","image"],"output_modalities":["text"]})} else {serde_json::json!({"modality":"text->text","input_modalities":["text"],"output_modalities":["text"]})}), pricing: per_token_pricing(row,&[("cost_per_1m_in","prompt"),("cost_per_1m_out","completion"),("cost_per_1m_in_cached","input_cache_read")]), top_provider:top_provider(row["context_window"].as_u64(),row["default_max_tokens"].as_u64()), supported_parameters: Some({let mut p=vec!["temperature".into(),"max_tokens".into()];if row["can_reason"]==true {p.push("reasoning".into());} if row["supports_attachments"]==true {p.push("image".into());}p}) });
        }
    }
    if out.is_empty() {
        Err("invalid Catwalk response format".into())
    } else {
        Ok(out)
    }
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

async fn fetch_openrouter(source: &str) -> Result<BTreeMap<String, ModelMetadata>, String> {
    let mut catalog = openrouter(fetch(source).await?, "models")?;
    if let Ok(mut url) = url::Url::parse(source) {
        if let Some(prefix) = url.path().strip_suffix("/models").map(str::to_owned) {
            for kind in ["embeddings", "videos"] {
                url.set_path(&format!("{prefix}/{kind}/models"));
                if let Ok(raw) = fetch(url.as_str()).await {
                    if let Ok(auxiliary) = openrouter(raw, kind) { catalog.extend(auxiliary); }
                }
            }
        }
    }
    Ok(catalog)
}

pub async fn refresh(urls: SourceUrls, trigger: &str) -> RefreshReport {
    let start = Instant::now();
    let (or, md, cw) = tokio::join!(
        fetch_openrouter(&urls.openrouter),
        fetch(&urls.models_dev),
        fetch(&urls.catwalk)
    );
    let mut reports = BTreeMap::new();
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
        refreshed_at: chrono::Utc::now().to_rfc3339(),
        duration_ms: start.elapsed().as_millis(),
        interval_minutes: 60,
        had_errors: errors,
        sources: reports,
    }
}

pub fn initialized(source: &str) -> bool {
    !search(source, "", 1).is_empty()
}

pub fn lookup(source: &str, id: &str) -> Option<ModelMetadata> {
    let guard=catalogs().read().unwrap();
    let map=match source {"openrouter"=>&guard.openrouter,"models.dev"=>&guard.models_dev,"catwalk"=>&guard.catwalk,_=>return None};
    map.get(id).cloned()
}

pub fn search(source: &str, query: &str, limit: usize) -> Vec<ModelMetadata> {
    let guard = catalogs().read().unwrap();
    let map = match source {
        "openrouter" => &guard.openrouter,
        "models.dev" => &guard.models_dev,
        "catwalk" => &guard.catwalk,
        _ => return vec![],
    };
    let q = query.to_lowercase();
    map.values()
        .filter(|m| {
            q.is_empty() || m.id.to_lowercase().contains(&q) || m.name.to_lowercase().contains(&q)
        })
        .take(limit.min(200))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn malformed_keeps_catalog() {
        assert!(openrouter(serde_json::json!({"data":[]}), "models").is_err());
    }
    #[test]
    fn normalizes_openrouter() {
        let m = openrouter(
            serde_json::json!({"data":[{"id":"x","context_length":0}]}),
            "models",
        )
        .unwrap();
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
        catalogs().write().unwrap().openrouter.remove("fixture/vision");
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
