//! The pi-ai helpers the dispatch path imports: `services/pi-ai/reasoning.ts`
//! (effort/visibility vocabulary, intent → effort), `generation.ts`
//! (`GenerationIntent`, `normalizeVerbosity`), `registry.ts`
//! (`buildReasoningOptionsForModel`, `buildGenerationOptions`,
//! `resolvePiAiModel`) and pi-ai's own `getSupportedThinkingLevels` /
//! `clampThinkingLevel` (`@earendil-works/pi-ai@1.2.0` `models.js`).
//!
//! The catalog is pi-ai 1.2.0's built-in chat models (`pi_models.json`,
//! generated from `builtinModels().getModels()` with the capability fields
//! the registry reads). Plexus's runtime pi.dev overlay is not reproduced.
use super::service_tier_selection::{infer_service_tier_format, resolve_service_tier};
use super::util::js_trim;
use crate::engine::js::*;
use serde_json::{json, Value};
use std::sync::OnceLock;

/// `EFFORT_LADDER`.
pub const EFFORT_LADDER: [&str; 6] = ["minimal", "low", "medium", "high", "xhigh", "max"];
/// pi-ai `EXTENDED_THINKING_LEVELS`.
pub const EXTENDED_THINKING_LEVELS: [&str; 7] =
    ["off", "minimal", "low", "medium", "high", "xhigh", "max"];
pub const ADAPTIVE_DEFAULT_EFFORT: &str = "high";

/// `ReasoningIntent` (`source` is always `"client"` on this path).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReasoningIntent {
    pub effort: Option<&'static str>,
    pub budget_tokens: Option<f64>,
    pub enabled: Option<bool>,
    pub adaptive: bool,
    pub visibility: Option<&'static str>,
    /// Raw `summary` value (verbatim, may be non-string).
    pub summary_detail: Option<Value>,
}

/// `GenerationIntent`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GenerationIntent {
    pub reasoning: ReasoningIntent,
    pub max_tokens: Option<f64>,
    pub temperature: Option<f64>,
    pub verbosity: Option<&'static str>,
    pub service_tier: Option<String>,
    /// `suffix` | `speed` | `service_tier` | `key_default`.
    pub service_tier_source: Option<&'static str>,
}

/// `normalizeEffort(raw)` (`"off"` for none/off/disabled).
pub fn normalize_effort(raw: Option<&Value>) -> Option<&'static str> {
    match js_trim(raw?.as_str()?).to_lowercase().as_str() {
        "none" | "off" | "disabled" => Some("off"),
        "minimal" => Some("minimal"),
        "low" => Some("low"),
        "medium" => Some("medium"),
        "high" => Some("high"),
        "xhigh" => Some("xhigh"),
        "max" | "maximum" => Some("max"),
        _ => None,
    }
}

/// `normalizeVisibility(raw)`.
pub fn normalize_visibility(raw: Option<&Value>) -> Option<&'static str> {
    match raw? {
        Value::Bool(true) => Some("summary"),
        Value::Bool(false) => Some("hidden"),
        Value::String(s) => match js_trim(s).to_lowercase().as_str() {
            "hidden" | "none" | "off" => Some("hidden"),
            "summary" | "summarized" | "concise" | "detailed" | "auto" => Some("summary"),
            "full" | "raw" | "all" => Some("full"),
            _ => None,
        },
        _ => None,
    }
}

/// `normalizeVerbosity(raw)`.
pub fn normalize_verbosity(raw: Option<&Value>) -> Option<&'static str> {
    match js_trim(raw?.as_str()?).to_lowercase().as_str() {
        "low" => Some("low"),
        "medium" => Some("medium"),
        "high" => Some("high"),
        _ => None,
    }
}

/// `effortToBudget(effort)`.
pub fn effort_to_budget(effort: &str) -> f64 {
    match effort {
        "minimal" => 1024.0,
        "low" => 2048.0,
        "medium" => 8192.0,
        "xhigh" => 32768.0,
        "max" => 65536.0,
        _ => 16384.0,
    }
}

/// `budgetToEffort(budget)`.
pub fn budget_to_effort(budget: f64) -> &'static str {
    if budget <= 0.0 {
        "off"
    } else if budget <= 1024.0 {
        "minimal"
    } else if budget <= 2048.0 {
        "low"
    } else if budget <= 8192.0 {
        "medium"
    } else if budget <= 16384.0 {
        "high"
    } else if budget <= 32768.0 {
        "xhigh"
    } else {
        "max"
    }
}

fn ladder_idx(e: &str) -> Option<usize> {
    EFFORT_LADDER.iter().position(|x| *x == e)
}

/// `clampEffortToWindow(effort, floor, ceiling)`.
pub fn clamp_effort_to_window<'a>(
    effort: &'a str,
    floor: Option<&str>,
    ceiling: Option<&str>,
) -> &'a str {
    let Some(mut idx) = ladder_idx(effort) else {
        return effort;
    };
    if let Some(f) = floor.and_then(ladder_idx) {
        if f > idx {
            idx = f;
        }
    }
    if let Some(c) = ceiling.and_then(ladder_idx) {
        if c < idx {
            idx = c;
        }
    }
    EFFORT_LADDER[idx]
}

/// `intentToEffort(intent)`.
pub fn intent_to_effort(intent: &ReasoningIntent) -> Option<&'static str> {
    if intent.enabled == Some(false) {
        return Some("off");
    }
    if let Some(e) = intent.effort {
        return Some(e);
    }
    if intent.budget_tokens == Some(-1.0) || intent.adaptive {
        return Some(ADAPTIVE_DEFAULT_EFFORT);
    }
    if let Some(b) = intent.budget_tokens {
        return Some(budget_to_effort(b));
    }
    if intent.enabled == Some(true) {
        return Some("medium");
    }
    None
}

/// `splitReasoningSuffix(modelAlias)` → `(alias, intent)`.
pub fn split_reasoning_suffix(model_alias: &str) -> (String, Option<ReasoningIntent>) {
    let Some(idx) = model_alias.rfind(':') else {
        return (model_alias.to_owned(), None);
    };
    if idx == 0 || idx == model_alias.len() - 1 {
        return (model_alias.to_owned(), None);
    }
    let Some(effort) = normalize_effort(Some(&json!(&model_alias[idx + 1..]))) else {
        return (model_alias.to_owned(), None);
    };
    let alias = model_alias[..idx].to_owned();
    if effort == "off" {
        return (
            alias,
            Some(ReasoningIntent {
                enabled: Some(false),
                ..Default::default()
            }),
        );
    }
    (
        alias,
        Some(ReasoningIntent {
            effort: Some(effort),
            enabled: Some(true),
            ..Default::default()
        }),
    )
}

// ─── pi-ai models.js ─────────────────────────────────────────────────────

/// `getSupportedThinkingLevels(model)`.
pub fn get_supported_thinking_levels(model: &Value) -> Vec<&'static str> {
    if !truthy(model.get("reasoning")) {
        return vec!["off"];
    }
    EXTENDED_THINKING_LEVELS
        .iter()
        .copied()
        .filter(|level| {
            let mapped = get(model, &["thinkingLevelMap", level]);
            if mapped == Some(&Value::Null) {
                return false;
            }
            if *level == "xhigh" || *level == "max" {
                return mapped.is_some();
            }
            true
        })
        .collect()
}

/// `clampThinkingLevel(model, level)`.
pub fn clamp_thinking_level(model: &Value, level: &str) -> &'static str {
    let available = get_supported_thinking_levels(model);
    if let Some(l) = available.iter().find(|l| **l == level) {
        return l;
    }
    let Some(requested) = EXTENDED_THINKING_LEVELS.iter().position(|l| *l == level) else {
        return available.first().copied().unwrap_or("off");
    };
    for candidate in &EXTENDED_THINKING_LEVELS[requested..] {
        if available.contains(candidate) {
            return candidate;
        }
    }
    for candidate in EXTENDED_THINKING_LEVELS[..requested].iter().rev() {
        if available.contains(candidate) {
            return candidate;
        }
    }
    available.first().copied().unwrap_or("off")
}

// ─── catalog ─────────────────────────────────────────────────────────────

fn catalog() -> &'static Value {
    static CATALOG: OnceLock<Value> = OnceLock::new();
    CATALOG.get_or_init(|| {
        serde_json::from_str(include_str!("pi_models.json"))
            .expect("embedded pi-ai model catalog must be valid")
    })
}

/// `resolvePiAiModel(provider, modelId)` / `getCatalogModel` over the
/// built-in pi-ai 1.2.0 chat catalog (model record JSON, `provider` added).
pub fn resolve_pi_ai_model(provider: &str, model_id: &str) -> Option<Value> {
    let model = get_arr(catalog(), &["providers", provider])?
        .iter()
        .find(|m| get_str(m, &["id"]) == Some(model_id))?;
    let mut out = model.as_object().cloned().unwrap_or_default();
    out.insert("provider".into(), json!(provider));
    Some(Value::Object(out))
}

// ─── registry.ts ─────────────────────────────────────────────────────────

fn is_openai_family(api: Option<&str>) -> bool {
    matches!(
        api,
        Some(
            "openai-responses"
                | "openai-codex-responses"
                | "openai-completions"
                | "azure-openai-responses"
        )
    )
}

/// `buildReasoningOptionsForModel(model, intent)`.
pub fn build_reasoning_options_for_model(model: &Value, intent: Option<&ReasoningIntent>) -> Obj {
    let api = get_str(model, &["api"]);
    if !truthy(model.get("reasoning")) {
        return Obj::new();
    }
    let Some(resolved) = intent.and_then(intent_to_effort) else {
        return Obj::new();
    };
    if resolved == "off" {
        let supports_disable = get(model, &["thinkingLevelMap", "off"]) != Some(&Value::Null);
        if !supports_disable {
            let adaptive_anthropic = api == Some("anthropic-messages")
                && get(model, &["compat", "forceAdaptiveThinking"]) == Some(&Value::Bool(true));
            let supported: Vec<&str> = get_supported_thinking_levels(model)
                .into_iter()
                .filter(|l| *l != "off" && (!adaptive_anthropic || *l != "minimal"))
                .collect();
            let lowest = if adaptive_anthropic {
                Some("low")
            } else {
                supported.first().copied()
            };
            let Some(lowest) = lowest else {
                return Obj::new();
            };
            return build_enabled_options(model, lowest, intent);
        }
        let mut o = Obj::new();
        match api {
            Some("anthropic-messages") => {
                o.insert("thinkingEnabled".into(), json!(false));
            }
            Some("google-generative-ai" | "google-generative-ai-vertex") => {
                o.insert("thinking".into(), json!({"enabled": false}));
            }
            _ => {}
        }
        o.insert("reasoning".into(), json!("off"));
        return o;
    }
    let clamped = clamp_thinking_level(model, resolved);
    if clamped == "off" {
        return Obj::new();
    }
    build_enabled_options(model, clamped, intent)
}

fn build_enabled_options(
    model: &Value,
    effort: &'static str,
    intent: Option<&ReasoningIntent>,
) -> Obj {
    let api = get_str(model, &["api"]);
    let mut base = Obj::new();
    base.insert("reasoning".into(), json!(effort));
    let visibility = intent.and_then(|i| i.visibility);
    if is_openai_family(api) {
        base.insert("reasoningEffort".into(), json!(effort));
        if matches!(visibility, Some("summary" | "full")) {
            base.insert(
                "reasoningSummary".into(),
                intent
                    .and_then(|i| i.summary_detail.clone())
                    .filter(|v| !v.is_null())
                    .unwrap_or(json!("auto")),
            );
        }
        return base;
    }
    if api == Some("anthropic-messages") {
        base.insert("thinkingEnabled".into(), json!(true));
        match visibility {
            Some("full") => {
                base.insert("thinkingDisplay".into(), json!("raw"));
            }
            Some("summary") => {
                base.insert("thinkingDisplay".into(), json!("summarized"));
            }
            _ => {}
        }
        if get(model, &["compat", "forceAdaptiveThinking"]) == Some(&Value::Bool(true)) {
            if intent.is_some_and(|i| i.adaptive && i.effort.is_none()) {
                super::util::del(&mut base, "reasoning");
            } else {
                base.insert(
                    "effort".into(),
                    json!(clamp_effort_to_window(effort, Some("low"), Some("max"))),
                );
            }
        } else {
            let budget = intent
                .and_then(|i| i.budget_tokens)
                .unwrap_or_else(|| effort_to_budget(effort));
            base.insert("thinkingBudgetTokens".into(), num(budget));
        }
        return base;
    }
    if matches!(
        api,
        Some("google-generative-ai" | "google-generative-ai-vertex")
    ) {
        let tlm = model.get("thinkingLevelMap").filter(|v| !v.is_null());
        let level_based = tlm.is_some_and(|m| {
            ["minimal", "low", "medium", "high", "xhigh"]
                .iter()
                .any(|l| get_str(m, &[l]).is_some())
        });
        let include_thoughts = visibility != Some("hidden");
        let thinking = if level_based {
            let level = tlm
                .and_then(|m| get_str(m, &[effort]))
                .map(str::to_owned)
                .unwrap_or_else(|| effort.to_uppercase());
            json!({"enabled": true, "includeThoughts": include_thoughts, "level": level})
        } else {
            let budget = intent
                .and_then(|i| i.budget_tokens)
                .unwrap_or_else(|| effort_to_budget(effort));
            json!({"enabled": true, "includeThoughts": include_thoughts, "budgetTokens": num(budget)})
        };
        base.insert("thinking".into(), thinking);
        return base;
    }
    base
}

/// Legacy (unmapped) OpenAI-family pass-through spelling.
fn legacy_openai_service_tier(tier: &str) -> String {
    match js_trim(tier).to_lowercase().as_str() {
        "standard" | "standard_only" => "default".into(),
        "fast" | "ultrafast" => "priority".into(),
        _ => tier.to_owned(),
    }
}

/// `buildGenerationOptions(model, intent)`.
pub fn build_generation_options(model: &Value, intent: Option<&GenerationIntent>) -> Obj {
    let Some(intent) = intent else {
        return Obj::new();
    };
    let api = get_str(model, &["api"]);
    let mut opts = build_reasoning_options_for_model(model, Some(&intent.reasoning));
    if let Some(max) = intent.max_tokens.filter(|m| *m > 0.0) {
        let ceiling = get_f64(model, &["maxTokens"]).filter(|c| *c > 0.0);
        opts.insert("maxTokens".into(), num(ceiling.map_or(max, |c| max.min(c))));
    }
    if let Some(t) = intent.temperature {
        let thinking_on = opts.get("thinkingEnabled") == Some(&Value::Bool(true))
            || !nullish(opts.get("reasoningEffort"));
        let supported = get(model, &["compat", "supportsTemperature"]) != Some(&Value::Bool(false));
        let incompatible = api == Some("anthropic-messages") && thinking_on;
        if supported && !incompatible {
            opts.insert("temperature".into(), num(t));
        }
    }
    if let Some(v) = intent.verbosity {
        if is_openai_family(api) {
            opts.insert("textVerbosity".into(), json!(v));
        }
    }
    if let Some(tier) = intent.service_tier.as_deref() {
        let map = model.get("serviceTierMap").filter(|m| truthy(Some(m)));
        if let Some(map) = map {
            let explicit = get_str(model, &["compat", "serviceTierFormat"]);
            let format = explicit.or_else(|| infer_service_tier_format(api));
            if format == Some("anthropic-speed") && api != Some("anthropic-messages") {
                // Configuration error: ignored (TS logs a warning).
            } else if let Some(format) = format {
                if let Some(resolved) = resolve_service_tier(Some(tier), Some(map), format) {
                    opts.insert(
                        "serviceTier".into(),
                        resolved.map_or(Value::Null, Value::String),
                    );
                    opts.insert("serviceTierFormat".into(), json!(format));
                }
            }
        } else if is_openai_family(api)
            && !matches!(intent.service_tier_source, Some("speed" | "key_default"))
        {
            opts.insert(
                "serviceTier".into(),
                json!(legacy_openai_service_tier(tier)),
            );
        }
    }
    opts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vocab() {
        assert_eq!(normalize_effort(Some(&json!(" Maximum "))), Some("max"));
        assert_eq!(normalize_effort(Some(&json!("none"))), Some("off"));
        assert_eq!(budget_to_effort(1025.0), "low");
        assert_eq!(
            clamp_effort_to_window("minimal", Some("low"), Some("max")),
            "low"
        );
        assert_eq!(split_reasoning_suffix("m:high").0, "m");
        assert_eq!(split_reasoning_suffix("m:foo").0, "m:foo");
        assert_eq!(
            intent_to_effort(&ReasoningIntent {
                adaptive: true,
                ..Default::default()
            }),
            Some("high")
        );
        assert_eq!(
            intent_to_effort(&ReasoningIntent {
                budget_tokens: Some(4000.0),
                ..Default::default()
            }),
            Some("medium")
        );
    }

    #[test]
    fn catalog_and_clamp() {
        let fable = resolve_pi_ai_model("anthropic", "claude-fable-5").expect("catalog model");
        assert_eq!(fable["reasoning"], true);
        assert_eq!(
            get_supported_thinking_levels(&fable),
            vec!["minimal", "low", "medium", "high", "xhigh", "max"]
        );
        let glm = resolve_pi_ai_model("zai", "glm-4.7").unwrap();
        assert_eq!(glm["compat"]["thinkingFormat"], "zai");
        assert!(resolve_pi_ai_model("zai", "nope").is_none());
        let m = json!({"reasoning": true, "thinkingLevelMap": {"minimal": null}});
        assert_eq!(clamp_thinking_level(&m, "minimal"), "low");
        assert_eq!(clamp_thinking_level(&m, "max"), "high");
    }

    #[test]
    fn reasoning_options() {
        let adaptive = json!({"api": "anthropic-messages", "reasoning": true, "thinkingLevelMap": {"off": null, "xhigh": "xhigh", "max": "max"}, "compat": {"forceAdaptiveThinking": true}});
        let off = ReasoningIntent {
            enabled: Some(false),
            ..Default::default()
        };
        assert_eq!(
            Value::Object(build_reasoning_options_for_model(&adaptive, Some(&off))),
            json!({"reasoning": "low", "thinkingEnabled": true, "effort": "low"})
        );
        let ad = ReasoningIntent {
            enabled: Some(true),
            adaptive: true,
            ..Default::default()
        };
        assert_eq!(
            Value::Object(build_reasoning_options_for_model(&adaptive, Some(&ad))),
            json!({"thinkingEnabled": true})
        );
        let budget = json!({"api": "anthropic-messages", "reasoning": true});
        let b = ReasoningIntent {
            budget_tokens: Some(5000.0),
            enabled: Some(true),
            visibility: Some("full"),
            ..Default::default()
        };
        assert_eq!(
            Value::Object(build_reasoning_options_for_model(&budget, Some(&b))),
            json!({"reasoning": "medium", "thinkingEnabled": true, "thinkingDisplay": "raw", "thinkingBudgetTokens": 5000})
        );
        let gem = json!({"api": "google-generative-ai", "reasoning": true, "thinkingLevelMap": {"low": "LOW", "high": "HIGH"}});
        let e = ReasoningIntent {
            effort: Some("high"),
            ..Default::default()
        };
        assert_eq!(
            Value::Object(build_reasoning_options_for_model(&gem, Some(&e))),
            json!({"reasoning": "high", "thinking": {"enabled": true, "includeThoughts": true, "level": "HIGH"}})
        );
        assert!(
            build_reasoning_options_for_model(&json!({"reasoning": false}), Some(&e)).is_empty()
        );
        assert!(
            build_reasoning_options_for_model(&gem, Some(&ReasoningIntent::default())).is_empty()
        );
    }

    #[test]
    fn generation_options() {
        let model = json!({"api": "openai-completions", "reasoning": false, "maxTokens": 1000});
        let intent = GenerationIntent {
            max_tokens: Some(5000.0),
            temperature: Some(0.5),
            verbosity: Some("low"),
            service_tier: Some("fast".into()),
            service_tier_source: Some("service_tier"),
            ..Default::default()
        };
        assert_eq!(
            Value::Object(build_generation_options(&model, Some(&intent))),
            json!({"maxTokens": 1000, "temperature": 0.5, "textVerbosity": "low", "serviceTier": "priority"})
        );
        let mapped = json!({"api": "anthropic-messages", "reasoning": false, "serviceTierMap": {"priority": "fast"}});
        let i = GenerationIntent {
            service_tier: Some("priority".into()),
            service_tier_source: Some("suffix"),
            ..Default::default()
        };
        assert_eq!(
            Value::Object(build_generation_options(&mapped, Some(&i))),
            json!({"serviceTier": "fast", "serviceTierFormat": "anthropic-speed"})
        );
        let kd = GenerationIntent {
            service_tier: Some("flex".into()),
            service_tier_source: Some("key_default"),
            ..Default::default()
        };
        assert!(build_generation_options(&model, Some(&kd)).is_empty());
    }
}
