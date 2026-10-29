//! Plexus model aliases: `ModelConfigSchema` (`config.ts`) on `crate::zod`,
//! and the `AliasRepository.saveAlias` → `rowToModelConfig` round trip that
//! defines the served shape. `from_view` projects onto the typed
//! `ModelConfig` used by routing.
use crate::state::{ModelConfig, ModelTarget, ModelTargetGroup};
use crate::zod::*;
use serde_json::{json, Map, Value};
use std::sync::OnceLock;

const SELECTORS: &[&str] = &[
    "random",
    "in_order",
    "cost",
    "latency",
    "usage",
    "quota",
    "performance",
    "e2e_performance",
];
const MODEL_TYPES: &[&str] = &[
    "text",
    "embeddings",
    "transcriptions",
    "speech",
    "image",
    "decisions",
];

fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

fn target() -> Schema {
    object(vec![
        ("provider", string().optional()),
        ("model", string().optional()),
        ("alias", string().min(1.0).optional()),
        ("enabled", boolean().default(true).optional()),
    ])
    .refine(
        |t| {
            if truthy(t.get("alias")) {
                !truthy(t.get("provider")) && !truthy(t.get("model"))
            } else {
                truthy(t.get("provider")) && truthy(t.get("model"))
            }
        },
        "A target must specify either 'alias' or both 'provider' and 'model', but not both",
    )
}

fn overrides(name_required: bool) -> Schema {
    let pricing = object(vec![
        ("prompt", string().optional()),
        ("completion", string().optional()),
        ("input_cache_read", string().optional()),
        ("input_cache_write", string().optional()),
        (
            "tiers",
            array(object(vec![
                ("input_tokens_above", number().int().min(0.0)),
                ("prompt", string().optional()),
                ("completion", string().optional()),
                ("input_cache_read", string().optional()),
                ("input_cache_write", string().optional()),
            ]))
            .optional(),
        ),
    ]);
    object(vec![
        (
            "name",
            if name_required {
                string().min(1.0)
            } else {
                string().optional()
            },
        ),
        ("short_name", trimmed().min(1.0).max(64.0).optional()),
        ("description", string().optional()),
        ("context_length", number().int().positive().optional()),
        ("pricing", pricing.optional()),
        (
            "architecture",
            object(vec![
                ("input_modalities", array(string()).optional()),
                ("output_modalities", array(string()).optional()),
                ("tokenizer", string().optional()),
            ])
            .optional(),
        ),
        ("supported_parameters", array(string()).optional()),
        (
            "top_provider",
            object(vec![
                ("context_length", number().int().positive().optional()),
                (
                    "max_completion_tokens",
                    number().int().positive().optional(),
                ),
            ])
            .optional(),
        ),
    ])
}

fn metadata() -> Schema {
    discriminated(
        "source",
        vec![
            object(vec![
                ("source", literal("auto")),
                ("overrides", overrides(false).optional()),
            ]),
            object(vec![("source", literal("disabled"))]),
            object(vec![
                ("source", literal("openrouter")),
                ("source_path", string().min(1.0)),
                ("overrides", overrides(false).optional()),
            ]),
            object(vec![
                ("source", literal("models.dev")),
                ("source_path", string().min(1.0)),
                ("overrides", overrides(false).optional()),
            ]),
            object(vec![
                ("source", literal("catwalk")),
                ("source_path", string().min(1.0)),
                ("overrides", overrides(false).optional()),
            ]),
            object(vec![
                ("source", literal("custom")),
                ("source_path", string().optional()),
                ("overrides", overrides(true)),
            ]),
        ],
    )
}

fn target_key(t: &Value) -> String {
    if truthy(t.get("alias")) {
        json!(["alias", t["alias"]]).to_string()
    } else {
        json!([t.get("provider"), t.get("model")]).to_string()
    }
}

fn build_model_config() -> Schema {
    object(vec![
        ("selector", enumeration(SELECTORS).optional()),
        (
            "priority",
            enumeration(&["selector", "api_match"]).default("selector"),
        ),
        ("targets", array(target()).optional()),
        (
            "target_groups",
            array(object(vec![
                ("name", string().min(1.0)),
                ("selector", enumeration(SELECTORS)),
                ("targets", array(target())),
            ]))
            .optional(),
        ),
        ("additional_aliases", array(string()).optional()),
        ("use_image_fallthrough", boolean().default(false).optional()),
        ("enforce_limits", boolean().default(false).optional()),
        ("sticky_session", boolean().default(true)),
        (
            "preferred_api",
            array(enumeration(&[
                "chat_completions",
                "messages",
                "gemini",
                "responses",
            ]))
            .optional(),
        ),
        ("type", enumeration(MODEL_TYPES).optional()),
        (
            "advanced",
            array(discriminated(
                "type",
                vec![
                    object(vec![
                        ("type", literal("strip_adaptive_thinking")),
                        ("enabled", boolean().default(true)),
                    ]),
                    // Fills `cache_control` breakpoints into requests that were
                    // translated to the Messages API from a format that cannot
                    // express them (chat, responses). Auto-enabled for aliases
                    // that dispatch to Anthropic's own API; adding the behavior
                    // explicitly (or with `enabled: false`) overrides that.
                    object(vec![
                        ("type", literal("inject_cache_control")),
                        ("enabled", boolean().default(true)),
                        // Mark the last tool definition, caching the tools block.
                        ("tools", boolean().default(true)),
                        // Mark the last system text block.
                        ("system", boolean().default(true)),
                        // Trailing "walking" breakpoints on the conversation tail
                        // (0 disables).
                        (
                            "message_breakpoints",
                            number().int().min(0.0).max(4.0).default(2),
                        ),
                        // Omitted leaves Anthropic's 5m default (1.25x write);
                        // '1h' costs 2x.
                        ("ttl", enumeration(&["5m", "1h"]).optional()),
                    ]),
                ],
            ))
            .optional(),
        ),
        ("metadata", metadata().optional()),
        (
            "pi_model",
            object(vec![
                ("provider", string().min(1.0)),
                ("model_id", string().min(1.0)),
            ])
            .optional(),
        ),
        ("extraBody", record(string(), any()).optional()),
        (
            "synthetic_safeguard_approval",
            boolean().default(false).optional(),
        ),
        (
            "compaction",
            crate::provider_schema::compaction_override().optional(),
        ),
    ])
    .super_refine(|data, path, issues| {
        let groups: Vec<Value> = match (data.get("target_groups"), data.get("targets")) {
            (Some(Value::Array(groups)), _) => groups.clone(),
            (_, Some(targets)) => vec![json!({"targets": targets})],
            _ => vec![],
        };
        let mut seen = std::collections::HashSet::new();
        let mut duplicates: Vec<(String, Value)> = Vec::new();
        for group in &groups {
            for t in group
                .get("targets")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let key = target_key(t);
                if !seen.insert(key.clone()) {
                    if let Some(slot) = duplicates.iter_mut().find(|(k, _)| *k == key) {
                        slot.1 = t.clone();
                    } else {
                        duplicates.push((key, t.clone()));
                    }
                }
            }
        }
        let mut p = path.clone();
        p.push(json!("target_groups"));
        for (_, t) in duplicates {
            let message = if truthy(t.get("alias")) {
                format!(
                    "Duplicate target 'alias:{}' is not allowed",
                    t["alias"].as_str().unwrap_or_default()
                )
            } else {
                format!(
                    "Duplicate target '{}/{}' is not allowed",
                    js_str(t.get("provider")),
                    js_str(t.get("model"))
                )
            };
            issues.push(Issue::custom(&p, message));
        }
        let mut seen = std::collections::HashSet::new();
        let mut dup_aliases: Vec<String> = Vec::new();
        for alias in data
            .get("additional_aliases")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            if !seen.insert(alias) && !dup_aliases.iter().any(|a| a == alias) {
                dup_aliases.push(alias.to_owned());
            }
        }
        let mut p = path.clone();
        p.push(json!("additional_aliases"));
        for alias in dup_aliases {
            issues.push(Issue::custom(
                &p,
                format!("Duplicate additional alias '{alias}' is not allowed"),
            ));
        }
    })
    .transform(|mut data| {
        if data.get("target_groups").is_none() {
            if let Some(targets) = data.get("targets").cloned() {
                let selector = data.get("selector").cloned().unwrap_or(json!("random"));
                data["target_groups"] =
                    json!([{"name": "default", "selector": selector, "targets": targets}]);
            }
        }
        data
    })
}

fn js_str(value: Option<&Value>) -> String {
    match value {
        None => "undefined".into(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) => "null".into(),
        Some(other) => other.to_string(),
    }
}

pub fn model_config() -> &'static Schema {
    static SCHEMA: OnceLock<Schema> = OnceLock::new();
    SCHEMA.get_or_init(build_model_config)
}

fn has_any_override(o: &Value) -> bool {
    let defined = |v: Option<&Value>| v.is_some();
    defined(o.get("name"))
        || defined(o.get("short_name"))
        || defined(o.get("description"))
        || defined(o.get("context_length"))
        || o.get("pricing")
            .and_then(Value::as_object)
            .is_some_and(|p| p.values().any(|_| true))
        || o.get("architecture").is_some_and(|a| {
            defined(a.get("tokenizer"))
                || defined(a.get("input_modalities"))
                || defined(a.get("output_modalities"))
        })
        || defined(o.get("supported_parameters"))
        || o.get("top_provider")
            .and_then(Value::as_object)
            .is_some_and(|t| !t.is_empty())
}

/// `overrideRowToOverrides(save(overrides))`.
fn stored_overrides(o: &Value) -> Value {
    let mut out = Map::new();
    let nn = |v: Option<&Value>| v.filter(|v| !v.is_null()).cloned();
    if let Some(v) = nn(o.get("name")) {
        out.insert("name".into(), v);
    }
    if let Some(v) = nn(o.get("short_name")) {
        out.insert("short_name".into(), v);
    }
    if let Some(v) = nn(o.get("description")) {
        out.insert("description".into(), v);
    }
    if let Some(v) = nn(o.get("context_length")) {
        out.insert("context_length".into(), v);
    }
    let mut pricing = Map::new();
    if let Some(p) = o.get("pricing") {
        for key in [
            "prompt",
            "completion",
            "input_cache_read",
            "input_cache_write",
        ] {
            if let Some(v) = nn(p.get(key)) {
                pricing.insert(key.into(), v);
            }
        }
        if let Some(Value::Array(tiers)) = p.get("tiers") {
            pricing.insert("tiers".into(), Value::Array(tiers.clone()));
        }
    }
    if !pricing.is_empty() {
        out.insert("pricing".into(), Value::Object(pricing));
    }
    let mut arch = Map::new();
    if let Some(a) = o.get("architecture") {
        for key in ["input_modalities", "output_modalities"] {
            if let Some(Value::Array(v)) = a.get(key) {
                arch.insert(key.into(), Value::Array(v.clone()));
            }
        }
        if let Some(v) = nn(a.get("tokenizer")) {
            arch.insert("tokenizer".into(), v);
        }
    }
    if !arch.is_empty() {
        out.insert("architecture".into(), Value::Object(arch));
    }
    if let Some(Value::Array(v)) = o.get("supported_parameters") {
        out.insert("supported_parameters".into(), Value::Array(v.clone()));
    }
    let mut top = Map::new();
    if let Some(t) = o.get("top_provider") {
        for key in ["context_length", "max_completion_tokens"] {
            if let Some(v) = nn(t.get(key)) {
                top.insert(key.into(), v);
            }
        }
    }
    if !top.is_empty() {
        out.insert("top_provider".into(), Value::Object(top));
    }
    Value::Object(out)
}

/// `rowToModelConfig(saveAlias(config))`.
pub fn repository_view(config: &Value) -> Value {
    let get = |key: &str| config.get(key);
    let groups = get("target_groups")
        .and_then(Value::as_array)
        .filter(|g| !g.is_empty());
    let mut target_groups = Vec::new();
    if let Some(groups) = groups {
        let rows: Vec<(Value, &Value)> = groups
            .iter()
            .flat_map(|g| {
                g.get("targets")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(move |t| (g.get("name").cloned().unwrap_or(Value::Null), t))
            })
            .collect();
        for group in groups {
            let name = group.get("name").cloned().unwrap_or(Value::Null);
            let targets: Vec<Value> = rows.iter().filter(|(n, _)| *n == name).map(|(_, t)| {
                let enabled = t.get("enabled") != Some(&Value::Bool(false));
                if truthy(t.get("alias")) { json!({"alias": t["alias"], "enabled": enabled}) }
                else { json!({"provider": t.get("provider").cloned().unwrap_or(Value::Null), "model": t.get("model").cloned().unwrap_or(Value::Null), "enabled": enabled}) }
            }).collect();
            target_groups.push(json!({"name": name, "selector": group.get("selector").cloned().unwrap_or(Value::Null), "targets": targets}));
        }
    }
    let mut out = Map::new();
    out.insert("target_groups".into(), Value::Array(target_groups));
    out.insert(
        "priority".into(),
        get("priority")
            .filter(|v| !v.is_null())
            .cloned()
            .unwrap_or(json!("selector")),
    );
    for key in [
        "use_image_fallthrough",
        "enforce_limits",
        "synthetic_safeguard_approval",
        "sticky_session",
    ] {
        out.insert(key.into(), json!(get(key) == Some(&Value::Bool(true))));
    }
    if truthy(get("selector")) {
        out.insert("selector".into(), get("selector").unwrap().clone());
    }
    if truthy(get("type")) {
        out.insert("type".into(), get("type").unwrap().clone());
    }
    for key in [
        "additional_aliases",
        "advanced",
        "preferred_api",
        "pi_model",
        "extraBody",
        "compaction",
    ] {
        if truthy(get(key)) {
            out.insert(key.into(), get(key).unwrap().clone());
        }
    }
    if let Some(meta) = get("metadata") {
        if truthy(meta.get("source")) {
            let source = meta["source"].clone();
            let stored = meta
                .get("overrides")
                .filter(|o| has_any_override(o))
                .map(stored_overrides);
            let non_empty = stored
                .clone()
                .filter(|o| o.as_object().is_some_and(|m| !m.is_empty()));
            let source_path = meta.get("source_path").filter(|v| !v.is_null()).cloned();
            let mut m = Map::new();
            m.insert("source".into(), source.clone());
            match source.as_str() {
                Some("disabled") => {}
                Some("auto") => {
                    if let Some(o) = non_empty {
                        m.insert("overrides".into(), o);
                    }
                }
                Some("custom") => {
                    if let Some(p) = source_path.filter(|p| truthy(Some(p))) {
                        m.insert("source_path".into(), p);
                    }
                    m.insert("overrides".into(), stored.unwrap_or(json!({})));
                }
                _ => {
                    m.insert("source_path".into(), source_path.unwrap_or(Value::Null));
                    if let Some(o) = non_empty {
                        m.insert("overrides".into(), o);
                    }
                }
            }
            out.insert("metadata".into(), Value::Object(m));
        }
    }
    Value::Object(out)
}

/// Typed routing projection of an alias view.
pub fn from_view(view: Value) -> ModelConfig {
    let target_groups = view
        .get("target_groups")
        .and_then(Value::as_array)
        .map(|groups| {
            groups
                .iter()
                .map(|g| ModelTargetGroup {
                    name: g
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    selector: g
                        .get("selector")
                        .and_then(Value::as_str)
                        .unwrap_or("random")
                        .to_owned(),
                    targets: g
                        .get("targets")
                        .and_then(Value::as_array)
                        .map(|ts| {
                            ts.iter()
                                .map(|t| ModelTarget {
                                    provider: t
                                        .get("provider")
                                        .and_then(Value::as_str)
                                        .map(str::to_owned),
                                    model: t
                                        .get("model")
                                        .and_then(Value::as_str)
                                        .map(str::to_owned),
                                    alias: t
                                        .get("alias")
                                        .and_then(Value::as_str)
                                        .map(str::to_owned),
                                    enabled: t
                                        .get("enabled")
                                        .and_then(Value::as_bool)
                                        .unwrap_or(true),
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                })
                .collect()
        })
        .unwrap_or_default();
    let strings = |key: &str| {
        view.get(key).and_then(Value::as_array).map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
    };
    ModelConfig {
        priority: view
            .get("priority")
            .and_then(Value::as_str)
            .unwrap_or("selector")
            .to_owned(),
        target_groups,
        additional_aliases: strings("additional_aliases").unwrap_or_default(),
        use_image_fallthrough: view
            .get("use_image_fallthrough")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        enforce_limits: view
            .get("enforce_limits")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        sticky_session: view
            .get("sticky_session")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        synthetic_safeguard_approval: view
            .get("synthetic_safeguard_approval")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        preferred_api: strings("preferred_api"),
        model_type: view.get("type").and_then(Value::as_str).map(str::to_owned),
        advanced: view.get("advanced").cloned(),
        metadata: view.get("metadata").cloned(),
        pi_model: view.get("pi_model").cloned(),
        extra_body: view.get("extraBody").cloned(),
        compaction: view.get("compaction").cloned(),
        view: Some(view),
    }
}

/// View for aliases persisted before the `config` column.
pub fn legacy_view(config: &ModelConfig) -> Value {
    let mut legacy = Map::new();
    legacy.insert("priority".into(), json!(config.priority));
    legacy.insert("target_groups".into(), Value::Array(config.target_groups.iter().map(|g| json!({
        "name": g.name, "selector": g.selector,
        "targets": g.targets.iter().map(|t| { let mut m = Map::new(); if let Some(v) = &t.provider { m.insert("provider".into(), json!(v)); }
            if let Some(v) = &t.model { m.insert("model".into(), json!(v)); }
            if let Some(v) = &t.alias { m.insert("alias".into(), json!(v)); } m.insert("enabled".into(), json!(t.enabled)); Value::Object(m) }).collect::<Vec<_>>(),
    })).collect()));
    if !config.additional_aliases.is_empty() {
        legacy.insert(
            "additional_aliases".into(),
            json!(config.additional_aliases),
        );
    }
    for (key, value) in [
        ("use_image_fallthrough", config.use_image_fallthrough),
        ("enforce_limits", config.enforce_limits),
        ("sticky_session", config.sticky_session),
        (
            "synthetic_safeguard_approval",
            config.synthetic_safeguard_approval,
        ),
    ] {
        legacy.insert(key.into(), json!(value));
    }
    if let Some(v) = &config.preferred_api {
        legacy.insert("preferred_api".into(), json!(v));
    }
    if let Some(v) = &config.model_type {
        legacy.insert("type".into(), json!(v));
    }
    for (key, value) in [
        ("advanced", &config.advanced),
        ("metadata", &config.metadata),
        ("pi_model", &config.pi_model),
        ("extraBody", &config.extra_body),
        ("compaction", &config.compaction),
    ] {
        if let Some(v) = value {
            legacy.insert(key.into(), v.clone());
        }
    }
    repository_view(&Value::Object(legacy))
}

impl ModelConfig {
    pub fn public_view(&self) -> Value {
        self.view.clone().unwrap_or_else(|| legacy_view(self))
    }
}

#[cfg(test)]
mod advanced_behaviors {
    use serde_json::json;

    fn advanced(entries: serde_json::Value) -> Result<serde_json::Value, Vec<serde_json::Value>> {
        super::model_config()
            .safe_parse(&json!({"targets": [], "advanced": entries}))
            .map(|ok| ok["advanced"].clone())
    }

    #[test]
    fn inject_cache_control_gets_schema_defaults() {
        assert_eq!(
            advanced(json!([{"type": "inject_cache_control"}])).unwrap(),
            json!([{
                "type": "inject_cache_control",
                "enabled": true,
                "tools": true,
                "system": true,
                "message_breakpoints": 2,
            }])
        );
        assert_eq!(
            advanced(json!([{"type": "inject_cache_control", "enabled": false, "tools": false, "system": false, "message_breakpoints": 0, "ttl": "1h"}])).unwrap(),
            json!([{
                "type": "inject_cache_control",
                "enabled": false,
                "tools": false,
                "system": false,
                "message_breakpoints": 0,
                "ttl": "1h",
            }])
        );
    }

    #[test]
    fn inject_cache_control_rejects_out_of_range_values() {
        for bad in [
            json!({"type": "inject_cache_control", "message_breakpoints": 5}),
            json!({"type": "inject_cache_control", "message_breakpoints": -1}),
            json!({"type": "inject_cache_control", "message_breakpoints": 1.5}),
            json!({"type": "inject_cache_control", "ttl": "2h"}),
        ] {
            assert!(advanced(json!([bad.clone()])).is_err(), "{bad}");
        }
    }

    #[test]
    fn strip_adaptive_thinking_is_unchanged() {
        assert_eq!(
            advanced(json!([{"type": "strip_adaptive_thinking"}])).unwrap(),
            json!([{"type": "strip_adaptive_thinking", "enabled": true}])
        );
    }
}

#[cfg(test)]
mod corpus {
    /// `ALIAS_SCHEMA_CORPUS=<file>` generated from the pinned Plexus schema.
    #[test]
    #[ignore = "needs a corpus generated from the pinned Plexus checkout"]
    fn matches_reference_corpus() {
        let path = std::env::var("ALIAS_SCHEMA_CORPUS").expect("ALIAS_SCHEMA_CORPUS");
        let cases: Vec<serde_json::Value> =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let mut failures = 0;
        for case in &cases {
            let actual = match super::model_config().safe_parse(&case["input"]) {
                Ok(ok) => serde_json::json!({"ok": ok}),
                Err(issues) => serde_json::json!({"issues": issues}),
            };
            if actual != case["expected"] {
                failures += 1;
                eprintln!(
                    "INPUT {}\n  plexus  {}\n  trefoil {}",
                    case["input"], case["expected"], actual
                );
            }
        }
        assert_eq!(
            failures,
            0,
            "{failures}/{} corpus cases differ",
            cases.len()
        );
    }
}
