//! Plexus provider persistence shape.
//!
//! Plexus stores a provider with `ProviderRepository.saveProvider` and serves
//! it from `rowToProviderConfig`. `repository_view` reproduces that round trip
//! (JavaScript truthiness included) for parsed PUT/PATCH bodies and raw backup
//! configs alike; the result is what GET/list/backup return. `from_view`
//! projects it onto the typed `ProviderConfig` used for routing.
use crate::state::{
    AdapterEntry, ModelAutosync, ProviderConfig, ProviderModel, QuotaCheckerConfig,
    RawPassthroughConfig,
};
use serde_json::{json, Map, Value};

/// JavaScript truthiness.
fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}
fn present(value: Option<&Value>) -> Option<&Value> {
    value.filter(|v| !v.is_null())
}
fn is_true(value: Option<&Value>) -> bool {
    value == Some(&Value::Bool(true))
}

/// `normalizeAdapterEntries`; `None` when empty or absent.
pub fn normalize_adapters(raw: Option<&Value>) -> Option<Value> {
    let raw = present(raw)?;
    let items = match raw {
        Value::Array(items) => items.clone(),
        other => vec![other.clone()],
    };
    let entries: Vec<Value> = items
        .into_iter()
        .filter_map(|entry| match entry {
            Value::String(name) => Some(json!({"name": name, "options": {}, "enabled": true})),
            Value::Object(map) if map.contains_key("name") => {
                let name = map.get("name")?.as_str()?.to_owned();
                let options = map
                    .get("options")
                    .filter(|o| o.is_object() || o.is_array())
                    .cloned()
                    .unwrap_or(json!({}));
                let enabled = map.get("enabled").and_then(Value::as_bool).unwrap_or(true);
                Some(json!({"name": name, "options": options, "enabled": enabled}))
            }
            _ => None,
        })
        .collect();
    (!entries.is_empty()).then_some(Value::Array(entries))
}

fn models_view(models: Option<&Value>) -> Option<Value> {
    match models {
        Some(Value::Array(names)) if !names.is_empty() => Some(Value::Array(names.clone())),
        Some(Value::Object(map)) if !map.is_empty() => {
            let has_config = map
                .values()
                .any(|cfg| present(cfg.get("pricing")).is_some());
            if !has_config {
                return Some(Value::Array(map.keys().map(|k| json!(k)).collect()));
            }
            let mut out = Map::new();
            for (name, cfg) in map {
                let mut model = Map::new();
                model.insert(
                    "pricing".into(),
                    present(cfg.get("pricing"))
                        .cloned()
                        .unwrap_or(json!({"source": "simple", "input": 0, "output": 0})),
                );
                if truthy(cfg.get("type")) {
                    model.insert("type".into(), cfg["type"].clone());
                }
                if truthy(cfg.get("access_via")) {
                    model.insert("access_via".into(), cfg["access_via"].clone());
                }
                if truthy(cfg.get("extraBody")) {
                    model.insert("extraBody".into(), cfg["extraBody"].clone());
                }
                if cfg
                    .get("adapter")
                    .is_some_and(|a| a.as_array().is_some_and(|a| !a.is_empty()))
                {
                    if let Some(adapter) = normalize_adapters(cfg.get("adapter")) {
                        model.insert("adapter".into(), adapter);
                    }
                }
                if present(cfg.get("auto_compat")).is_some() {
                    model.insert("auto_compat".into(), json!(is_true(cfg.get("auto_compat"))));
                }
                if let Some(v) = present(cfg.get("maxConcurrency")) {
                    model.insert("maxConcurrency".into(), v.clone());
                }
                if let Some(v) = present(cfg.get("pi_ai_model_id")) {
                    model.insert("pi_ai_model_id".into(), v.clone());
                }
                out.insert(name.clone(), Value::Object(model));
            }
            Some(Value::Object(out))
        }
        _ => None,
    }
}

/// `rowToProviderConfig(saveProvider(config))`. `oauth_account` is the
/// account the persisted credential link resolves to, if any.
pub fn repository_view(config: &Value, oauth_account: Option<&str>) -> Value {
    let get = |key: &str| config.get(key);
    let mut out = Map::new();
    out.insert(
        "api_base_url".into(),
        present(get("api_base_url")).cloned().unwrap_or(json!("")),
    );
    if truthy(get("display_name")) {
        out.insert("display_name".into(), get("display_name").unwrap().clone());
    }
    if truthy(get("api_key")) {
        out.insert("api_key".into(), get("api_key").unwrap().clone());
    }
    if truthy(get("oauth_provider")) {
        out.insert(
            "oauth_provider".into(),
            get("oauth_provider").unwrap().clone(),
        );
    }
    if let Some(account) = oauth_account.filter(|a| !a.is_empty()) {
        out.insert("oauth_account".into(), json!(account));
    }
    out.insert(
        "enabled".into(),
        json!(get("enabled") != Some(&Value::Bool(false))),
    );
    out.insert(
        "disable_cooldown".into(),
        json!(is_true(get("disable_cooldown"))),
    );
    out.insert(
        "stall_cooldown".into(),
        json!(is_true(get("stall_cooldown"))),
    );
    out.insert(
        "allow_100_percent_utilization".into(),
        json!(is_true(get("allow_100_percent_utilization"))),
    );
    if let Some(discount) = present(get("discount")) {
        out.insert("discount".into(), discount.clone());
    }
    out.insert(
        "estimateTokens".into(),
        json!(is_true(get("estimateTokens"))),
    );
    out.insert(
        "useClaudeMasking".into(),
        json!(is_true(get("useClaudeMasking"))),
    );
    out.insert(
        "gemini_thinking_enabled".into(),
        json!(is_true(get("geminiThinkingEnabled"))),
    );
    if truthy(get("cache_key_injection")) {
        out.insert(
            "cache_key_injection".into(),
            get("cache_key_injection").unwrap().clone(),
        );
    }
    if let Some(Value::Array(ext)) = get("responses_extensions") {
        out.insert("responses_extensions".into(), Value::Array(ext.clone()));
    }
    out.insert("auto_compat".into(), json!(is_true(get("auto_compat"))));
    if let Some(models) = models_view(get("models")) {
        out.insert("models".into(), models);
    }
    if truthy(get("headers")) {
        out.insert("headers".into(), get("headers").unwrap().clone());
    }
    if let Some(Value::Object(extra)) = get("extraBody") {
        out.insert("extraBody".into(), Value::Object(extra.clone()));
    }
    if truthy(get("compaction")) {
        out.insert("compaction".into(), get("compaction").unwrap().clone());
    }
    if let Some(checker) = get("quota_checker") {
        if truthy(checker.get("type")) {
            let mut q = Map::new();
            q.insert("type".into(), checker["type"].clone());
            q.insert(
                "enabled".into(),
                json!(checker.get("enabled") != Some(&Value::Bool(false))),
            );
            q.insert(
                "intervalMinutes".into(),
                present(checker.get("intervalMinutes"))
                    .cloned()
                    .unwrap_or(json!(30)),
            );
            if truthy(checker.get("id")) {
                q.insert("id".into(), checker["id"].clone());
            }
            if truthy(checker.get("options")) {
                q.insert("options".into(), checker["options"].clone());
            }
            out.insert("quota_checker".into(), Value::Object(q));
        }
    }
    let autosync = get("model_autosync");
    let interval = autosync
        .and_then(|a| present(a.get("intervalMinutes")))
        .and_then(Value::as_f64)
        .unwrap_or(60.0)
        .max(1.0);
    out.insert("model_autosync".into(), json!({"enabled": is_true(autosync.and_then(|a| a.get("enabled"))), "intervalMinutes": num(interval)}));
    if get("adapter").is_some_and(|a| a.as_array().is_some_and(|a| !a.is_empty())) {
        if let Some(adapter) = normalize_adapters(get("adapter")) {
            out.insert("adapter".into(), adapter);
        }
    }
    for key in [
        "timeoutMs",
        "stallTtfbMs",
        "stallTtfbBytes",
        "stallMinBps",
        "stallWindowMs",
        "stallGracePeriodMs",
        "maxConcurrency",
        "pi_ai_provider",
    ] {
        if let Some(v) = present(get(key)) {
            out.insert(key.into(), v.clone());
        }
    }
    if truthy(get("pi_ai_quirks")) {
        out.insert("pi_ai_quirks".into(), get("pi_ai_quirks").unwrap().clone());
    }
    if truthy(get("raw_passthrough")) {
        out.insert(
            "raw_passthrough".into(),
            get("raw_passthrough").unwrap().clone(),
        );
    }
    Value::Object(out)
}

fn num(n: f64) -> Value {
    if n.fract() == 0.0 {
        json!(n as i64)
    } else {
        json!(n)
    }
}

fn access_via_strings(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| match item {
                    Value::String(s) => Some(s.trim().to_lowercase()),
                    Value::Object(o) => {
                        let kind = o.get("type")?.as_str()?.trim().to_lowercase();
                        Some(
                            match o
                                .get("subtype")
                                .and_then(Value::as_str)
                                .map(str::trim)
                                .filter(|s| !s.is_empty())
                            {
                                Some(sub) => format!("{kind}:{}", sub.to_lowercase()),
                                None => kind,
                            },
                        )
                    }
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Typed routing projection of a repository view.
pub fn from_view(view: Value, version: i64) -> ProviderConfig {
    let str_of = |key: &str| view.get(key).and_then(Value::as_str).map(str::to_owned);
    let bool_of = |key: &str| view.get(key).and_then(Value::as_bool).unwrap_or(false);
    let (api_base_url, api_base_urls) = match view.get("api_base_url") {
        Some(Value::Object(map)) => (
            String::new(),
            map.iter()
                .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_owned())))
                .collect(),
        ),
        other => (
            other.and_then(Value::as_str).unwrap_or_default().to_owned(),
            Default::default(),
        ),
    };
    let models = match view.get("models") {
        Some(Value::Array(names)) => names
            .iter()
            .filter_map(Value::as_str)
            .map(|n| (n.to_owned(), ProviderModel::default()))
            .collect(),
        Some(Value::Object(map)) => map
            .iter()
            .map(|(name, cfg)| {
                (
                    name.clone(),
                    ProviderModel {
                        model_type: cfg.get("type").and_then(Value::as_str).map(str::to_owned),
                        access_via: access_via_strings(cfg.get("access_via")),
                        upstream_id: None,
                    },
                )
            })
            .collect(),
        _ => Default::default(),
    };
    let quota_checker = view.get("quota_checker").map(|q| QuotaCheckerConfig {
        checker_type: q.get("type").and_then(Value::as_str).map(str::to_owned),
        id: q.get("id").and_then(Value::as_str).map(str::to_owned),
        enabled: q.get("enabled").and_then(Value::as_bool).unwrap_or(true),
        interval: q
            .get("intervalMinutes")
            .and_then(Value::as_f64)
            .unwrap_or(30.0) as u64,
        options: q.get("options").cloned().unwrap_or(Value::Null),
    });
    ProviderConfig {
        version,
        display_name: str_of("display_name"),
        api_base_url,
        api_base_urls,
        models,
        api_key: str_of("api_key"),
        oauth_provider: str_of("oauth_provider"),
        oauth_account: str_of("oauth_account"),
        enabled: view.get("enabled").and_then(Value::as_bool).unwrap_or(true),
        disable_cooldown: bool_of("disable_cooldown"),
        stall_cooldown: bool_of("stall_cooldown"),
        allow_100_percent_utilization: bool_of("allow_100_percent_utilization"),
        discount: view.get("discount").and_then(Value::as_f64),
        estimate_tokens: bool_of("estimateTokens"),
        use_claude_masking: bool_of("useClaudeMasking"),
        gemini_thinking_enabled: bool_of("gemini_thinking_enabled"),
        auto_compat: bool_of("auto_compat"),
        adapter: view
            .get("adapter")
            .and_then(|a| serde_json::from_value::<Vec<AdapterEntry>>(a.clone()).ok())
            .unwrap_or_default(),
        model_autosync: ModelAutosync {
            enabled: view
                .pointer("/model_autosync/enabled")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            interval_minutes: view
                .pointer("/model_autosync/intervalMinutes")
                .and_then(Value::as_f64)
                .unwrap_or(60.0) as u32,
        },
        raw_passthrough: view
            .get("raw_passthrough")
            .and_then(|r| serde_json::from_value::<RawPassthroughConfig>(r.clone()).ok()),
        quota_checker,
        retry_policy: Default::default(),
        view: Some(view),
    }
}

/// View for a provider persisted before the `config` column existed.
pub fn legacy_view(config: &ProviderConfig) -> Value {
    let mut legacy = Map::new();
    legacy.insert(
        "api_base_url".into(),
        if config.api_base_urls.is_empty() {
            json!(config.api_base_url)
        } else {
            let mut record: Map<String, Value> = config
                .api_base_urls
                .iter()
                .map(|(k, v)| (k.clone(), json!(v)))
                .collect();
            if !config.api_base_url.is_empty() {
                record.entry("chat").or_insert(json!(config.api_base_url));
            }
            Value::Object(record)
        },
    );
    if let Some(v) = &config.display_name {
        legacy.insert("display_name".into(), json!(v));
    }
    if let Some(v) = &config.api_key {
        legacy.insert("api_key".into(), json!(v));
    }
    if let Some(v) = &config.oauth_provider {
        legacy.insert("oauth_provider".into(), json!(v));
    }
    for (key, value) in [
        ("enabled", config.enabled),
        ("disable_cooldown", config.disable_cooldown),
        ("stall_cooldown", config.stall_cooldown),
        (
            "allow_100_percent_utilization",
            config.allow_100_percent_utilization,
        ),
        ("estimateTokens", config.estimate_tokens),
        ("useClaudeMasking", config.use_claude_masking),
        ("geminiThinkingEnabled", config.gemini_thinking_enabled),
        ("auto_compat", config.auto_compat),
    ] {
        legacy.insert(key.into(), json!(value));
    }
    if let Some(v) = config.discount {
        legacy.insert("discount".into(), json!(v));
    }
    if !config.models.is_empty() {
        legacy.insert(
            "models".into(),
            Value::Object(
                config
                    .models
                    .iter()
                    .map(|(name, model)| {
                        let mut m = Map::new();
                        m.insert(
                            "pricing".into(),
                            json!({"source": "simple", "input": 0, "output": 0}),
                        );
                        if let Some(t) = &model.model_type {
                            m.insert("type".into(), json!(t));
                        }
                        if !model.access_via.is_empty() {
                            m.insert("access_via".into(), json!(model.access_via));
                        }
                        (name.clone(), Value::Object(m))
                    })
                    .collect(),
            ),
        );
    }
    if !config.adapter.is_empty() {
        legacy.insert(
            "adapter".into(),
            serde_json::to_value(&config.adapter).unwrap_or(Value::Null),
        );
    }
    legacy.insert("model_autosync".into(), json!({"enabled": config.model_autosync.enabled, "intervalMinutes": config.model_autosync.interval_minutes}));
    if let Some(r) = &config.raw_passthrough {
        legacy.insert(
            "raw_passthrough".into(),
            serde_json::to_value(r).unwrap_or(Value::Null),
        );
    }
    if let Some(q) = &config.quota_checker {
        legacy.insert("quota_checker".into(), json!({"type": q.checker_type, "id": q.id, "enabled": q.enabled, "intervalMinutes": q.interval, "options": if q.options.is_null() { json!({}) } else { q.options.clone() }}));
    }
    repository_view(&Value::Object(legacy), config.oauth_account.as_deref())
}

impl ProviderConfig {
    /// The Plexus-shaped JSON served by the management API.
    pub fn public_view(&self) -> Value {
        self.view.clone().unwrap_or_else(|| legacy_view(self))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn round_trip_matches_reference_repository() {
        let parsed = crate::provider_schema::provider_config()
            .safe_parse(
                &json!({"api_base_url":"https://b.example/v1","api_key":"sk-b","models":{"x":{}}}),
            )
            .unwrap();
        assert_eq!(
            repository_view(&parsed, None),
            json!({"allow_100_percent_utilization": false, "api_base_url": "https://b.example/v1", "api_key": "sk-b", "auto_compat": false, "disable_cooldown": false, "enabled": true, "estimateTokens": false, "gemini_thinking_enabled": false, "model_autosync": {"enabled": false, "intervalMinutes": 60}, "models": {"x": {"pricing": {"input": 0, "output": 0, "source": "simple"}}}, "stall_cooldown": false, "useClaudeMasking": false})
        );
        let typed = from_view(
            repository_view(
                &json!({"api_base_url":{"chat":"https://a/v1"},"api_key":"k","models":["m"]}),
                None,
            ),
            3,
        );
        assert_eq!(
            (
                typed.api_base_url.as_str(),
                typed.api_url_for("chat"),
                typed.models.contains_key("m")
            ),
            ("", "https://a/v1", true)
        );
    }
}
