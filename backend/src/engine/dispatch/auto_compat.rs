//! `services/dispatch/dispatcher-auto-compat.ts` (+ `transformers/utils.ts`
//! `projectReasoningForResponses`): registry-aware request projection and
//! the reactive strip-and-retry planners.
use super::pi_ai::*;
use super::responses_extensions::LITE_ALLOWED_TOOL_TYPES;
use super::service_tier_selection::{
    get_default_service_tier, has_explicit_tier, strip_key_default_tier, SERVICE_TIER_API_TYPES,
};
use super::util::{base_type, del, is_object_like, obj_of, strict_eq};
use crate::engine::anthropic::thinking_clamp::clamp_anthropic_effort_and_thinking;
use crate::engine::js::*;
use crate::engine::routing::RouteResult;
use regex::Regex;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::OnceLock;

/// `projectReasoningForResponses(reasoning)` (`transformers/utils.ts`).
pub fn project_reasoning_for_responses(reasoning: Option<&Value>) -> Option<Value> {
    let r = reasoning.filter(|r| truthy(Some(r)))?;
    let effort = r.get("effort");
    if r.get("enabled") == Some(&Value::Bool(false))
        || effort
            .and_then(Value::as_str)
            .is_some_and(|e| e == "off" || e == "none")
    {
        return Some(json!({"effort": "none"}));
    }
    let projected = obj(vec![
        ("effort", effort.cloned()),
        ("summary", r.get("summary").cloned()),
    ]);
    projected
        .as_object()
        .is_some_and(|o| !o.is_empty())
        .then_some(projected)
}

/// `getThinkLevel(thinking_budget)` (`transformers/utils.ts`).
pub fn get_think_level(thinking_budget: f64) -> &'static str {
    if thinking_budget <= 0.0 {
        "none"
    } else if thinking_budget <= 1024.0 {
        "low"
    } else if thinking_budget <= 8192.0 {
        "medium"
    } else {
        "high"
    }
}

// ─── intent extraction ───────────────────────────────────────────────────

fn normalize_reasoning_from_unified(reasoning: Option<&Value>) -> ReasoningIntent {
    let r = |k: &str| reasoning.and_then(|v| v.get(k));
    let effort = normalize_effort(r("effort"));
    let enabled = if effort == Some("off") {
        Some(false)
    } else {
        r("enabled").and_then(Value::as_bool)
    };
    let summary = r("summary");
    ReasoningIntent {
        effort: effort.filter(|e| *e != "off"),
        budget_tokens: r("max_tokens")
            .filter(|v| !v.is_null())
            .and_then(Value::as_f64),
        enabled,
        adaptive: truthy(r("adaptive")),
        visibility: normalize_visibility(summary),
        summary_detail: summary.filter(|s| truthy(Some(s))).cloned(),
    }
}

/// `extractReasoningIntent(payload, request)`.
pub fn extract_reasoning_intent(source: &Value, request: &Value) -> ReasoningIntent {
    let incoming = get_str(request, &["incomingApiType"]).map(str::to_lowercase);
    let thinking = source.get("thinking").filter(|t| is_object_like(Some(t)));
    if incoming.as_deref() == Some("messages") {
        if let Some(thinking) = thinking {
            let ty = get_str(thinking, &["type"]).map(str::to_lowercase);
            return ReasoningIntent {
                enabled: Some(ty.as_deref() != Some("disabled")),
                adaptive: ty.as_deref() == Some("adaptive"),
                budget_tokens: get_f64(thinking, &["budget_tokens"]),
                visibility: normalize_visibility(thinking.get("display")),
                ..Default::default()
            };
        }
    }
    let raw = coalesce(source.get("reasoning"), request.get("reasoning"));
    if let Some(raw) = raw.filter(|r| is_object_like(Some(r))) {
        let effort = normalize_effort(raw.get("effort"));
        let summary_detail = raw.get("summary").filter(|s| s.is_string()).cloned();
        let mut enabled = (effort == Some("off")).then_some(false);
        if raw.get("enabled").is_some() {
            enabled = Some(raw.get("enabled") == Some(&Value::Bool(true)));
        }
        return ReasoningIntent {
            effort: effort.filter(|e| *e != "off"),
            budget_tokens: get_f64(&raw, &["max_tokens"]),
            enabled,
            adaptive: false,
            visibility: normalize_visibility(summary_detail.as_ref()),
            summary_detail: summary_detail.filter(|s| truthy(Some(s))),
        };
    }
    if let Some(chat) = normalize_effort(source.get("reasoning_effort")) {
        return if chat == "off" {
            ReasoningIntent {
                enabled: Some(false),
                ..Default::default()
            }
        } else {
            ReasoningIntent {
                effort: Some(chat),
                enabled: Some(true),
                ..Default::default()
            }
        };
    }
    if let Some(tc) =
        get(source, &["generationConfig", "thinkingConfig"]).filter(|t| is_object_like(Some(t)))
    {
        let budget = coalesce(tc.get("thinkingBudget"), tc.get("thinking_budget"));
        let level = coalesce(tc.get("thinkingLevel"), tc.get("thinking_level"));
        let effort = normalize_effort(level.as_ref());
        let b = budget.as_ref().and_then(Value::as_f64);
        return ReasoningIntent {
            effort: effort.filter(|e| *e != "off"),
            budget_tokens: b,
            enabled: Some(b != Some(0.0)),
            adaptive: b == Some(-1.0),
            visibility: (tc.get("includeThoughts") == Some(&Value::Bool(true)))
                .then_some("summary"),
            summary_detail: None,
        };
    }
    normalize_reasoning_from_unified(request.get("reasoning"))
}

fn pick_service_tier_intent(request: &Value, source: &Value) -> Option<(String, &'static str)> {
    if let Some(s) = get_str(request, &["serviceTier"]) {
        return Some((s.to_owned(), "suffix"));
    }
    let original = request.get("originalBody");
    let key_default = get_default_service_tier(request);
    if let Some(kd) = key_default {
        if truthy(original) && !has_explicit_tier(original) {
            return Some((kd.to_owned(), "key_default"));
        }
    }
    let candidates = [
        (source.get("speed"), "speed"),
        (source.get("service_tier"), "service_tier"),
        (original.and_then(|o| o.get("speed")), "speed"),
        (original.and_then(|o| o.get("service_tier")), "service_tier"),
    ];
    for (value, src) in candidates {
        if let Some(s) = value.and_then(Value::as_str).filter(|s| !s.is_empty()) {
            return Some((s.to_owned(), src));
        }
    }
    key_default.map(|kd| (kd.to_owned(), "key_default"))
}

/// `extractGenerationIntent(payload, request)`.
pub fn extract_generation_intent(payload: &Value, request: &Value) -> GenerationIntent {
    let empty = Value::Object(Obj::new());
    let source = if is_object_like(Some(payload)) {
        payload
    } else {
        &empty
    };
    let chain = |vals: &[Option<&Value>]| {
        vals.iter()
            .copied()
            .flatten()
            .find(|v| !v.is_null())
            .cloned()
    };
    let max_tokens = chain(&[
        source.get("max_output_tokens"),
        source.get("max_tokens"),
        source.get("max_completion_tokens"),
        get(source, &["generationConfig", "maxOutputTokens"]),
        request.get("max_tokens"),
    ]);
    let temperature = chain(&[
        source.get("temperature"),
        get(source, &["generationConfig", "temperature"]),
        request.get("temperature"),
    ]);
    let verbosity = normalize_verbosity(
        chain(&[
            get(source, &["text", "verbosity"]),
            get(request, &["text", "verbosity"]),
        ])
        .as_ref(),
    );
    let tier = pick_service_tier_intent(request, source);
    // Cross-format requests express reasoning in the client's own dialect, so
    // read it from the original body rather than the translated payload.
    let reasoning_source = match request.get("originalBody").filter(|b| truthy(Some(b))) {
        Some(original)
            if get_str(request, &["incomingApiType"])
                .map(str::to_lowercase)
                .as_deref()
                != Some("messages") =>
        {
            original
        }
        _ => source,
    };
    GenerationIntent {
        reasoning: extract_reasoning_intent(reasoning_source, request),
        max_tokens: max_tokens.as_ref().and_then(Value::as_f64),
        temperature: temperature.as_ref().and_then(Value::as_f64),
        verbosity,
        service_tier_source: tier.as_ref().map(|t| t.1),
        service_tier: tier.map(|t| t.0),
    }
}

// ─── projections ─────────────────────────────────────────────────────────

fn mapped_thinking_value(model: &Value, effort: Option<&str>) -> Option<String> {
    let effort = effort?;
    Some(
        get_str(model, &["thinkingLevelMap", effort])
            .unwrap_or(effort)
            .to_owned(),
    )
}

fn mapped_off_value(model: &Value) -> Option<String> {
    get_str(model, &["thinkingLevelMap", "off"]).map(str::to_owned)
}

fn should_drop_temperature(intent: &GenerationIntent, options: &Obj) -> bool {
    intent.temperature.is_some() && !options.contains_key("temperature")
}

fn apply_temperature(next: &mut Obj, intent: &GenerationIntent, options: &Obj) {
    if let Some(t) = options.get("temperature") {
        next.insert("temperature".into(), t.clone());
    } else if should_drop_temperature(intent, options) {
        del(next, "temperature");
    }
}

fn project_openai_service_tier(next: &mut Obj, options: &Obj, source: Option<&str>) {
    let Some(tier) = options.get("serviceTier") else {
        return;
    };
    if options.get("serviceTierFormat").and_then(Value::as_str) == Some("anthropic-speed") {
        return;
    }
    if matches!(source, Some("speed" | "suffix")) {
        del(next, "speed");
    }
    if tier.is_null() {
        del(next, "service_tier");
    } else {
        next.insert("service_tier".into(), tier.clone());
    }
}

fn project_anthropic_service_tier(next: &mut Obj, options: &Obj, source: Option<&str>) {
    let Some(tier) = options.get("serviceTier") else {
        return;
    };
    if options.get("serviceTierFormat").and_then(Value::as_str) == Some("service-tier") {
        if matches!(source, Some("speed" | "suffix")) {
            del(next, "speed");
        }
        if tier.is_null() {
            del(next, "service_tier");
        } else {
            next.insert("service_tier".into(), tier.clone());
        }
        return;
    }
    if let Some(st) = next.get("service_tier").and_then(Value::as_str) {
        if !matches!(
            super::util::js_trim(st).to_lowercase().as_str(),
            "auto" | "standard_only"
        ) {
            del(next, "service_tier");
        }
    }
    if tier.is_null() {
        del(next, "speed");
    } else {
        next.insert("speed".into(), tier.clone());
    }
}

fn resolve_chat_template_kwargs(model: &Value, options: &Obj) -> Obj {
    let mut kwargs = Obj::new();
    let Some(template) = get_obj(model, &["compat", "chatTemplateKwargs"]) else {
        return kwargs;
    };
    let effort = options.get("reasoningEffort").and_then(Value::as_str);
    for (key, value) in template {
        let resolved = if is_object_like(Some(value)) {
            let falsy_effort = effort.is_none_or(str::is_empty);
            if falsy_effort && truthy(value.get("omitWhenOff")) {
                None
            } else if get_str(value, &["$var"]) == Some("thinking.enabled") {
                Some(json!(!falsy_effort))
            } else {
                let mapped = if falsy_effort {
                    get(model, &["thinkingLevelMap", "off"])
                } else {
                    get(model, &["thinkingLevelMap", effort.unwrap_or_default()])
                };
                match mapped {
                    None => effort.map(|e| json!(e)),
                    Some(Value::String(s)) => Some(json!(s)),
                    Some(_) => None,
                }
            }
        } else {
            Some(value.clone())
        };
        if let Some(v) = resolved {
            kwargs.insert(key.clone(), v);
        }
    }
    kwargs
}

fn project_openai_completions(
    payload: &Value,
    request: &Value,
    model: &Value,
    intent: &GenerationIntent,
    options: &Obj,
) -> Value {
    let mut next = obj_of(Some(payload));
    let compat = model
        .get("compat")
        .filter(|c| !c.is_null())
        .cloned()
        .unwrap_or(json!({}));
    if let Some(max) = options.get("maxTokens").filter(|v| !v.is_null()) {
        if get_str(&compat, &["maxTokensField"]) == Some("max_completion_tokens") {
            del(&mut next, "max_tokens");
            next.insert("max_completion_tokens".into(), max.clone());
        } else {
            next.insert("max_tokens".into(), max.clone());
        }
    }
    apply_temperature(&mut next, intent, options);
    project_openai_service_tier(&mut next, options, intent.service_tier_source);
    if !truthy(model.get("reasoning")) {
        return Value::Object(next);
    }

    let effort = options.get("reasoningEffort").and_then(Value::as_str);
    let enabled = effort.is_some();
    let explicit_off = options.get("reasoning").and_then(Value::as_str) == Some("off")
        || intent.reasoning.enabled == Some(false);
    if !enabled && !explicit_off {
        return Value::Object(next);
    }
    let mapped = mapped_thinking_value(model, effort);
    let off = mapped_off_value(model);
    let resolved_reasoning = coalesce(next.get("reasoning"), request.get("reasoning"));
    let had_reasoning_object = matches!(resolved_reasoning, Some(Value::Object(_)));
    let supports_effort = compat.get("supportsReasoningEffort");
    let mapped_effort = |next: &mut Obj| {
        if enabled && truthy(supports_effort) {
            if let Some(m) = mapped.as_ref().filter(|m| !m.is_empty()) {
                next.insert("reasoning_effort".into(), json!(m));
            }
        }
    };

    match get_str(&compat, &["thinkingFormat"]) {
        Some("zai") => {
            next.insert(
                "thinking".into(),
                if enabled {
                    json!({"type": "enabled", "clear_thinking": false})
                } else {
                    json!({"type": "disabled"})
                },
            );
            del(&mut next, "reasoning");
            del(&mut next, "reasoning_effort");
            mapped_effort(&mut next);
        }
        Some("qwen") => {
            next.insert("enable_thinking".into(), json!(enabled));
            del(&mut next, "reasoning");
            del(&mut next, "reasoning_effort");
        }
        Some("qwen-chat-template") => {
            let mut kw = obj_of(next.get("chat_template_kwargs"));
            kw.insert("enable_thinking".into(), json!(enabled));
            kw.insert("preserve_thinking".into(), json!(true));
            next.insert("chat_template_kwargs".into(), Value::Object(kw));
            del(&mut next, "reasoning");
            del(&mut next, "reasoning_effort");
        }
        Some("chat-template") => {
            let mut kw = obj_of(next.get("chat_template_kwargs"));
            for (k, v) in resolve_chat_template_kwargs(model, options) {
                kw.insert(k, v);
            }
            next.insert("chat_template_kwargs".into(), Value::Object(kw));
            del(&mut next, "reasoning");
            del(&mut next, "reasoning_effort");
        }
        Some("deepseek") => {
            next.insert(
                "thinking".into(),
                if enabled {
                    json!({"type": "enabled"})
                } else {
                    json!({"type": "disabled"})
                },
            );
            del(&mut next, "reasoning");
            del(&mut next, "reasoning_effort");
            mapped_effort(&mut next);
        }
        Some("openrouter") => {
            let reasoning = if enabled {
                obj(vec![("effort", mapped.clone().map(Value::String))])
            } else {
                json!({"effort": off.clone().unwrap_or_else(|| "none".into())})
            };
            next.insert("reasoning".into(), reasoning);
            del(&mut next, "reasoning_effort");
        }
        Some("ant-ling") => {
            del(&mut next, "reasoning");
            del(&mut next, "reasoning_effort");
            if enabled {
                if let Some(m) = mapped.as_ref().filter(|m| !m.is_empty()) {
                    next.insert("reasoning".into(), json!({"effort": m}));
                }
            }
        }
        Some("together") => {
            next.insert("reasoning".into(), json!({"enabled": enabled}));
            mapped_effort(&mut next);
        }
        Some("string-thinking") => {
            let v = if enabled {
                mapped.clone().map(Value::String)
            } else {
                Some(json!(off.clone().unwrap_or_else(|| "none".into())))
            };
            match v {
                Some(v) => {
                    next.insert("thinking".into(), v);
                }
                None => {
                    del(&mut next, "thinking");
                }
            }
            del(&mut next, "reasoning");
            del(&mut next, "reasoning_effort");
        }
        _ => {
            del(&mut next, "reasoning");
            if enabled && truthy(supports_effort) && mapped.as_ref().is_some_and(|m| !m.is_empty())
            {
                next.insert("reasoning_effort".into(), json!(mapped));
            } else if !enabled
                && truthy(supports_effort)
                && off.as_ref().is_some_and(|o| !o.is_empty())
            {
                next.insert("reasoning_effort".into(), json!(off));
            } else if supports_effort == Some(&Value::Bool(false)) || had_reasoning_object {
                del(&mut next, "reasoning_effort");
            }
        }
    }
    Value::Object(next)
}

fn project_responses(
    payload: &Value,
    model: &Value,
    intent: &GenerationIntent,
    options: &Obj,
) -> Value {
    let mut next = obj_of(Some(payload));
    if let Some(max) = options.get("maxTokens").filter(|v| !v.is_null()) {
        next.insert("max_output_tokens".into(), max.clone());
    }
    apply_temperature(&mut next, intent, options);
    project_openai_service_tier(&mut next, options, intent.service_tier_source);
    if let Some(v) = options.get("textVerbosity") {
        let mut text = obj_of(next.get("text"));
        text.insert("verbosity".into(), v.clone());
        next.insert("text".into(), Value::Object(text));
    }
    let existing = project_reasoning_for_responses(next.get("reasoning"));
    if truthy(options.get("reasoningEffort")) || truthy(options.get("reasoningSummary")) {
        let mut r = obj_of(existing.as_ref());
        let effort = mapped_thinking_value(
            model,
            options.get("reasoningEffort").and_then(Value::as_str),
        )
        .unwrap_or_else(|| "medium".into());
        r.insert("effort".into(), json!(effort));
        let summary = options
            .get("reasoningSummary")
            .filter(|v| !v.is_null())
            .cloned()
            .or_else(|| {
                existing
                    .as_ref()
                    .and_then(|e| e.get("summary"))
                    .filter(|v| !v.is_null())
                    .cloned()
            })
            .unwrap_or(json!("auto"));
        r.insert("summary".into(), summary);
        next.insert("reasoning".into(), Value::Object(r));
        let mut include: Vec<Value> = Vec::new();
        for item in next
            .get("include")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .chain(std::iter::once(&json!("reasoning.encrypted_content")))
        {
            let dup = !is_object_like(Some(item))
                && include.iter().any(|x| strict_eq(Some(x), Some(item)));
            if !dup {
                include.push(item.clone());
            }
        }
        next.insert("include".into(), Value::Array(include));
    } else if options.get("reasoning").and_then(Value::as_str) == Some("off") {
        let mut r = obj_of(existing.as_ref());
        r.insert(
            "effort".into(),
            json!(mapped_off_value(model).unwrap_or_else(|| "none".into())),
        );
        next.insert("reasoning".into(), Value::Object(r));
    }
    Value::Object(next)
}

fn project_anthropic(
    payload: &Value,
    model: &Value,
    intent: &GenerationIntent,
    options: &Obj,
    inline: bool,
) -> Value {
    let mut next = obj_of(Some(payload));
    if let Some(max) = options.get("maxTokens").filter(|v| !v.is_null()) {
        next.insert("max_tokens".into(), max.clone());
    }
    apply_temperature(&mut next, intent, options);
    project_anthropic_service_tier(&mut next, options, intent.service_tier_source);
    match options.get("thinkingEnabled") {
        Some(Value::Bool(true)) => {
            let display = options
                .get("thinkingDisplay")
                .filter(|v| !v.is_null())
                .cloned()
                .unwrap_or(json!("summarized"));
            if get(model, &["compat", "forceAdaptiveThinking"]) == Some(&Value::Bool(true)) {
                next.insert(
                    "thinking".into(),
                    json!({"type": "adaptive", "display": display}),
                );
                if let Some(effort) = options
                    .get("effort")
                    .and_then(Value::as_str)
                    .filter(|e| !e.is_empty())
                {
                    let mut oc = obj_of(next.get("output_config"));
                    oc.insert(
                        "effort".into(),
                        json!(clamp_effort_to_window(effort, Some("low"), Some("max"))),
                    );
                    next.insert("output_config".into(), Value::Object(oc));
                }
            } else {
                let budget = options
                    .get("thinkingBudgetTokens")
                    .filter(|v| !v.is_null())
                    .cloned()
                    .unwrap_or(json!(1024));
                next.insert(
                    "thinking".into(),
                    json!({"type": "enabled", "budget_tokens": budget, "display": display}),
                );
            }
        }
        Some(Value::Bool(false)) => {
            next.insert("thinking".into(), json!({"type": "disabled"}));
            if truthy(get(
                &Value::Object(next.clone()),
                &["output_config", "effort"],
            )) {
                if let Some(Value::Object(oc)) = next.get_mut("output_config") {
                    del(oc, "effort");
                    if oc.is_empty() {
                        del(&mut next, "output_config");
                    }
                }
            }
        }
        _ => {}
    }
    let next = Value::Object(next);
    if inline {
        next
    } else {
        clamp_anthropic_effort_and_thinking(next, get_str(model, &["id"]))
    }
}

fn project_gemini(payload: &Value, intent: &GenerationIntent, options: &Obj) -> Value {
    let mut next = obj_of(Some(payload));
    let mut gc = obj_of(next.get("generationConfig"));
    if let Some(max) = options.get("maxTokens").filter(|v| !v.is_null()) {
        gc.insert("maxOutputTokens".into(), max.clone());
    }
    if let Some(t) = options.get("temperature") {
        gc.insert("temperature".into(), t.clone());
    } else if should_drop_temperature(intent, options) {
        del(&mut gc, "temperature");
    }
    match options.get("thinking").and_then(|t| t.get("enabled")) {
        Some(Value::Bool(true)) => {
            let t = &options["thinking"];
            let tc = obj(vec![
                (
                    "includeThoughts",
                    Some(json!(t.get("includeThoughts") != Some(&Value::Bool(false)))),
                ),
                ("thinkingLevel", t.get("level").cloned()),
                ("thinkingBudget", t.get("budgetTokens").cloned()),
            ]);
            gc.insert("thinkingConfig".into(), tc);
        }
        Some(Value::Bool(false)) => {
            gc.insert("thinkingConfig".into(), json!({"thinkingBudget": 0}));
        }
        _ => {}
    }
    next.insert("generationConfig".into(), Value::Object(gc));
    Value::Object(next)
}

// ─── quirks ──────────────────────────────────────────────────────────────

fn merge_obj(a: Option<&Value>, b: Option<&Value>) -> Value {
    let mut m = obj_of(a);
    for (k, v) in obj_of(b) {
        m.insert(k, v);
    }
    Value::Object(m)
}

/// `resolveInlineQuirks(quirks, targetApiType, modelId)`.
pub fn resolve_inline_quirks(
    quirks: Option<&Value>,
    target_api_type: &str,
    model_id: &str,
) -> Option<Value> {
    let common = quirks?
        .get(base_type(target_api_type))
        .filter(|c| truthy(Some(c)))?;
    let mut traits = obj_of(Some(common));
    let models = del(&mut traits, "models");
    let Some(model) = models
        .as_ref()
        .and_then(|m| m.get(model_id))
        .filter(|m| truthy(Some(m)))
    else {
        return Some(Value::Object(traits));
    };
    let compat = merge_obj(traits.get("compat"), model.get("compat"));
    let reasoning = coalesce(model.get("reasoning"), traits.get("reasoning"));
    let mut merged = traits.clone();
    for (k, v) in obj_of(Some(model)) {
        merged.insert(k, v);
    }
    merged.insert("compat".into(), compat);
    if reasoning == Some(Value::Bool(false)) {
        del(&mut merged, "thinkingLevelMap");
    }
    Some(Value::Object(merged))
}

/// `applyQuirkOverlay(base, quirks, targetApiType, modelId)`.
pub fn apply_quirk_overlay(
    base: &Value,
    quirks: Option<&Value>,
    target_api_type: &str,
    model_id: &str,
) -> Value {
    let Some(target) = quirks
        .and_then(|q| q.get(base_type(target_api_type)))
        .filter(|t| truthy(Some(t)))
    else {
        return base.clone();
    };
    let mut traits = obj_of(Some(target));
    let models = del(&mut traits, "models");
    let model = models
        .as_ref()
        .and_then(|m| m.get(model_id))
        .filter(|m| truthy(Some(m)));
    let base_compat = base
        .get("compat")
        .filter(|c| !c.is_null())
        .cloned()
        .unwrap_or(json!({}));
    let mut with_target = obj_of(Some(base));
    if truthy(traits.get("api")) {
        with_target.insert("api".into(), traits["api"].clone());
    }
    for k in [
        "reasoning",
        "maxTokens",
        "thinkingLevelMap",
        "serviceTierMap",
    ] {
        if let Some(v) = traits.get(k) {
            with_target.insert(k.into(), v.clone());
        }
    }
    if truthy(traits.get("compat")) {
        with_target.insert(
            "compat".into(),
            merge_obj(Some(&base_compat), traits.get("compat")),
        );
    }
    if with_target.get("reasoning") == Some(&Value::Bool(false)) {
        del(&mut with_target, "thinkingLevelMap");
    }
    let Some(model) = model else {
        return Value::Object(with_target);
    };
    let target_compat = with_target
        .get("compat")
        .filter(|c| !c.is_null())
        .cloned()
        .unwrap_or(base_compat);
    let mut merged = with_target;
    for k in [
        "reasoning",
        "maxTokens",
        "thinkingLevelMap",
        "serviceTierMap",
    ] {
        if let Some(v) = model.get(k) {
            merged.insert(k.into(), v.clone());
        }
    }
    if truthy(model.get("compat")) {
        merged.insert(
            "compat".into(),
            merge_obj(Some(&target_compat), model.get("compat")),
        );
    }
    if merged.get("reasoning") == Some(&Value::Bool(false)) {
        del(&mut merged, "thinkingLevelMap");
    }
    Value::Object(merged)
}

fn select_inline_generation_intent(traits: &Value, intent: &GenerationIntent) -> GenerationIntent {
    let compat = |k: &str| get(traits, &["compat", k]);
    let tlm = traits.get("thinkingLevelMap");
    let can_map = traits.get("reasoning") == Some(&Value::Bool(true))
        && tlm.is_some()
        && (get_str(traits, &["api"]) != Some("openai-completions")
            || compat("thinkingFormat").is_some()
            || compat("supportsReasoningEffort").is_some());
    let explicit_off = intent.reasoning.enabled == Some(false);
    let map_off = truthy(tlm)
        && tlm
            .and_then(Value::as_object)
            .is_some_and(|m| m.contains_key("off"));
    GenerationIntent {
        reasoning: if can_map && (!explicit_off || map_off) {
            intent.reasoning.clone()
        } else {
            ReasoningIntent::default()
        },
        max_tokens: intent
            .max_tokens
            .filter(|_| traits.get("maxTokens").is_some() || compat("maxTokensField").is_some()),
        temperature: intent
            .temperature
            .filter(|_| compat("supportsTemperature").is_some()),
        verbosity: None,
        service_tier: intent
            .service_tier
            .clone()
            .filter(|_| traits.get("serviceTierMap").is_some()),
        service_tier_source: intent
            .service_tier_source
            .filter(|_| traits.get("serviceTierMap").is_some() && intent.service_tier.is_some()),
    }
}

fn route_declares_tier_support(route: &RouteResult, target_api_type: &str) -> bool {
    let Some(block) = get(
        &route.config,
        &["pi_ai_quirks", &base_type(target_api_type)],
    )
    .filter(|b| truthy(Some(b))) else {
        return false;
    };
    let model_map =
        get(block, &["models", &route.model, "serviceTierMap"]).filter(|v| !v.is_null());
    let effective = model_map.or_else(|| block.get("serviceTierMap").filter(|v| !v.is_null()));
    let values: Vec<&Value> = match effective.filter(|e| truthy(Some(e))) {
        Some(Value::Object(m)) => m.values().collect(),
        Some(Value::Array(a)) => a.iter().collect(),
        _ => vec![],
    };
    values
        .iter()
        .any(|v| v.as_str().is_some_and(|s| !s.is_empty()))
}

/// `applyNativeAnthropicCompat(payload, request, modelId)`: catalog-backed
/// translation on cross-format native Anthropic OAuth routes.
pub fn apply_native_anthropic_compat(
    payload: Value,
    request: &Value,
    model_id: &str,
    resolve: PiAiResolver<'_>,
) -> Value {
    let Some(model) = resolve("anthropic", model_id)
        .filter(|m| get_str(m, &["api"]) == Some("anthropic-messages"))
    else {
        return payload;
    };
    let original_body = request
        .get("originalBody")
        .cloned()
        .unwrap_or_else(|| Value::Object(Obj::new()));
    let intent = extract_generation_intent(&original_body, request);
    let mut options = build_generation_options(&model, Some(&intent));
    // Messages requires max_tokens. When the client omitted a limit, use the
    // catalog ceiling rather than the generic builder's conservative fallback.
    if request.get("max_tokens").is_none() && intent.max_tokens.is_none() {
        if let Some(ceiling) = get_f64(&model, &["maxTokens"]).filter(|c| *c > 0.0) {
            options.insert("maxTokens".into(), num(ceiling));
        }
    }
    project_anthropic(&payload, &model, &intent, &options, false)
}

/// `resolvePiAiModel(provider, modelId)` hook (the TS tests spy on it).
pub type PiAiResolver<'a> = &'a dyn Fn(&str, &str) -> Option<Value>;

/// `applyRegistryAutoCompat(providerPayload, request, route, targetApiType)`
/// against the bundled pi-ai catalog.
pub fn apply_registry_auto_compat(
    payload: Value,
    request: &Value,
    route: &RouteResult,
    target_api_type: &str,
) -> Value {
    apply_registry_auto_compat_with(
        payload,
        request,
        route,
        target_api_type,
        &resolve_pi_ai_model,
    )
}

/// `applyRegistryAutoCompat` with an explicit `resolvePiAiModel`.
pub fn apply_registry_auto_compat_with(
    payload: Value,
    request: &Value,
    route: &RouteResult,
    target_api_type: &str,
    resolve: PiAiResolver<'_>,
) -> Value {
    let auto_compat = route.config.get("auto_compat") == Some(&Value::Bool(true))
        || route
            .model_config
            .as_ref()
            .and_then(|m| m.get("auto_compat"))
            == Some(&Value::Bool(true));
    let tier_payload = if !route_declares_tier_support(route, target_api_type)
        && SERVICE_TIER_API_TYPES.contains(&base_type(target_api_type).as_str())
        && is_object_like(Some(&payload))
    {
        strip_key_default_tier(payload, request)
    } else {
        payload
    };
    if !auto_compat {
        return tier_payload;
    }

    let pi_provider = get_str(&route.config, &["pi_ai_provider"]).filter(|s| !s.is_empty());
    let pi_model_id = route
        .model_config
        .as_ref()
        .and_then(|m| m.get("pi_ai_model_id"))
        .filter(|v| !v.is_null());
    let quirks = route.config.get("pi_ai_quirks").filter(|q| truthy(Some(q)));
    let base = pi_provider.and_then(|p| {
        let id = pi_model_id
            .map(|v| to_string(Some(v)))
            .unwrap_or_else(|| route.model.clone());
        resolve(p, &id)
    });
    let inline = if base.is_none() {
        quirks.and_then(|q| resolve_inline_quirks(Some(q), target_api_type, &route.model))
    } else {
        None
    };

    let pi_model = if let Some(inline) = &inline {
        let tlm = inline.get("thinkingLevelMap");
        obj(vec![
            ("id", Some(json!(route.model))),
            ("api", inline.get("api").cloned()),
            (
                "reasoning",
                Some(json!(
                    inline.get("reasoning") == Some(&Value::Bool(true)) && tlm.is_some()
                )),
            ),
            (
                "thinkingLevelMap",
                Some(tlm.filter(|v| !v.is_null()).cloned().unwrap_or(json!({}))),
            ),
            ("serviceTierMap", inline.get("serviceTierMap").cloned()),
            ("maxTokens", inline.get("maxTokens").cloned()),
            (
                "compat",
                Some(
                    inline
                        .get("compat")
                        .filter(|v| !v.is_null())
                        .cloned()
                        .unwrap_or(json!({})),
                ),
            ),
        ])
    } else if let Some(base) = &base {
        apply_quirk_overlay(base, quirks, target_api_type, &route.model)
    } else {
        return tier_payload;
    };

    let intent = extract_generation_intent(&tier_payload, request);
    let selected = match &inline {
        Some(traits) => select_inline_generation_intent(traits, &intent),
        None => intent,
    };
    let options = build_generation_options(&pi_model, Some(&selected));
    if let Some(inline) = &inline {
        if options.is_empty()
            && get(inline, &["compat", "supportsTemperature"]) != Some(&Value::Bool(false))
        {
            return tier_payload;
        }
    }
    let api = get_str(&pi_model, &["api"])
        .map(str::to_owned)
        .unwrap_or_else(|| target_api_type.to_owned());
    match api.as_str() {
        "openai-responses" | "openai-codex-responses" | "azure-openai-responses" => {
            project_responses(&tier_payload, &pi_model, &selected, &options)
        }
        "anthropic-messages" => project_anthropic(
            &tier_payload,
            &pi_model,
            &selected,
            &options,
            inline.is_some(),
        ),
        "google-generative-ai" | "google-generative-ai-vertex" => {
            project_gemini(&tier_payload, &selected, &options)
        }
        _ => project_openai_completions(&tier_payload, request, &pi_model, &selected, &options),
    }
}

// ─── reactive strip-and-retry ────────────────────────────────────────────

fn unsupported_param_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?i)(?:unsupported|unknown) parameter[:\s]+['"`]?([A-Za-z0-9_.\[\]]+)['"`]?"#)
            .expect("valid regex")
    })
}

/// `matchUnsupportedParameter(responseBody)` (bracket segments normalized).
pub fn match_unsupported_parameter(body: &str) -> Option<String> {
    if body.is_empty() {
        return None;
    }
    let name = unsupported_param_re()
        .captures(body)?
        .get(1)?
        .as_str()
        .to_owned();
    static BR: OnceLock<Regex> = OnceLock::new();
    Some(
        BR.get_or_init(|| Regex::new(r"\[([A-Za-z0-9_]+)\]").expect("valid regex"))
            .replace_all(&name, ".$1")
            .into_owned(),
    )
}

fn is_canonical_index(seg: &str, len: usize) -> Option<usize> {
    let canonical = seg == "0"
        || (!seg.is_empty() && !seg.starts_with('0') && seg.bytes().all(|b| b.is_ascii_digit()));
    if !canonical {
        return None;
    }
    seg.parse::<usize>().ok().filter(|i| *i < len)
}

/// `deleteDottedPath(payload, path)` → `(payload, deleted)`. Dangerous
/// segments are refused; arrays only via canonical in-bounds indices; an
/// array leaf is spliced out.
pub fn delete_dotted_path(payload: Value, path: &str) -> (Value, bool) {
    let segments: Vec<&str> = path.split('.').collect();
    if segments
        .iter()
        .any(|s| matches!(*s, "__proto__" | "constructor" | "prototype"))
    {
        return (payload, false);
    }
    fn walk(target: &mut Value, segs: &[&str]) -> bool {
        let (seg, rest) = (segs[0], &segs[1..]);
        match target {
            Value::Object(m) => {
                if rest.is_empty() {
                    return del(m, seg).is_some();
                }
                m.get_mut(seg).is_some_and(|next| walk(next, rest))
            }
            Value::Array(a) => {
                let Some(i) = is_canonical_index(seg, a.len()) else {
                    return false;
                };
                if rest.is_empty() {
                    a.remove(i);
                    return true;
                }
                walk(&mut a[i], rest)
            }
            _ => false,
        }
    }
    let mut out = payload.clone();
    if walk(&mut out, &segments) {
        (out, true)
    } else {
        (payload, false)
    }
}

pub const MAX_UNSUPPORTED_PARAM_STRIP_RETRIES: u32 = 2;

/// `UnsupportedParamStripState`.
#[derive(Debug, Clone, Default)]
pub struct UnsupportedParamStripState {
    pub attempts: u32,
    pub stripped_params: HashSet<String>,
}

fn is_structural_array_element_path(path: &str) -> bool {
    let segs: Vec<&str> = path.split('.').collect();
    segs.len() == 2
        && matches!(segs[0], "messages" | "input")
        && !segs[1].is_empty()
        && segs[1].bytes().all(|b| b.is_ascii_digit())
}

/// `planUnsupportedParamStrip(responseBody, state)`.
pub fn plan_unsupported_param_strip(
    body: &str,
    state: &mut UnsupportedParamStripState,
) -> Option<String> {
    if state.attempts >= MAX_UNSUPPORTED_PARAM_STRIP_RETRIES {
        return None;
    }
    let name = match_unsupported_parameter(body)?;
    if matches!(name.as_str(), "messages" | "input" | "model")
        || is_structural_array_element_path(&name)
    {
        return None;
    }
    if state.stripped_params.contains(&name) {
        return None;
    }
    state.attempts += 1;
    state.stripped_params.insert(name.clone());
    Some(name)
}

/// `matchThinkingSignatureError(responseBody)`.
pub fn match_thinking_signature_error(body: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    !body.is_empty()
        && RE
            .get_or_init(|| {
                Regex::new(r"(?i)invalid\s+`?signature`?\s+in\s+`?thinking`?\s+block")
                    .expect("valid regex")
            })
            .is_match(body)
}

/// `isAnthropicMessagesPayload(payload)`.
pub fn is_anthropic_messages_payload(payload: &Value) -> bool {
    payload.get("messages").is_some_and(Value::is_array)
}

fn content_has_block_type(content: Option<&Value>, ty: &str) -> bool {
    content
        .and_then(Value::as_array)
        .is_some_and(|c| c.iter().any(|b| get_str(b, &["type"]) == Some(ty)))
}

/// Shared drop-vs-placeholder rebuild of `stripThinkingSignatureBlocks` /
/// `stripAdvisorResultBlocks`.
fn strip_blocks(
    payload: Value,
    drop: impl Fn(&Value) -> (bool, bool),
    placeholder: &str,
) -> (Value, usize) {
    if !is_anthropic_messages_payload(&payload) {
        return (payload, 0);
    }
    let mut stripped = 0usize;
    let messages = payload["messages"].as_array().cloned().unwrap_or_default();
    let per: Vec<(Value, Option<Vec<Value>>, bool)> = messages
        .into_iter()
        .map(|message| {
            let Some(content) = message.get("content").and_then(Value::as_array) else {
                return (message, None, false);
            };
            let mut kept = Vec::new();
            for block in content {
                let (remove, counts) = drop(block);
                if counts {
                    stripped += 1;
                }
                if !remove {
                    kept.push(block.clone());
                }
            }
            let changed = kept.len() != content.len();
            (message, Some(kept), changed)
        })
        .collect();
    if stripped == 0 {
        return (payload, 0);
    }
    let mut result: Vec<Value> = Vec::new();
    for i in 0..per.len() {
        let (message, content, changed) = &per[i];
        match content {
            None => {
                result.push(message.clone());
                continue;
            }
            Some(c) if !c.is_empty() => {
                if *changed {
                    let mut m = obj_of(Some(message));
                    m.insert("content".into(), Value::Array(c.clone()));
                    result.push(Value::Object(m));
                } else {
                    result.push(message.clone());
                }
                continue;
            }
            Some(_) => {}
        }
        let prev = result.last();
        let next = per.get(i + 1).map(|p| &p.0);
        let break_alt = truthy(prev)
            && truthy(next)
            && strict_eq(
                prev.and_then(|p| p.get("role")),
                next.and_then(|n| n.get("role")),
            );
        let orphan = content_has_block_type(next.and_then(|n| n.get("content")), "tool_result")
            && !content_has_block_type(prev.and_then(|p| p.get("content")), "tool_use");
        if break_alt || orphan {
            let mut m = obj_of(Some(message));
            m.insert(
                "content".into(),
                json!([{"type": "text", "text": placeholder}]),
            );
            result.push(Value::Object(m));
        }
    }
    let mut out = obj_of(Some(&payload));
    out.insert("messages".into(), Value::Array(result));
    (Value::Object(out), stripped)
}

/// `stripThinkingSignatureBlocks(payload)` → `(payload, strippedCount)`.
pub fn strip_thinking_signature_blocks(payload: Value) -> (Value, usize) {
    strip_blocks(
        payload,
        |b| {
            let t = matches!(
                get_str(b, &["type"]),
                Some("thinking" | "redacted_thinking")
            ) && b.is_object();
            (t, t)
        },
        "[reasoning elided]",
    )
}

pub const MAX_THINKING_SIGNATURE_STRIP_RETRIES: u32 = 1;
pub const MAX_ADVISOR_RESULT_STRIP_RETRIES: u32 = 1;
pub const MAX_LITE_TOOL_STRIP_RETRIES: u32 = 1;

/// Attempt counter shared by the single-retry strip states
/// (`ThinkingSignatureStripState`, `AdvisorResultStripState`, `LiteToolStripState`).
#[derive(Debug, Clone, Copy, Default)]
pub struct StripState {
    pub attempts: u32,
}

/// `planThinkingSignatureStrip(responseBody, payload, state)`.
pub fn plan_thinking_signature_strip(body: &str, payload: &Value, state: &mut StripState) -> bool {
    if state.attempts >= MAX_THINKING_SIGNATURE_STRIP_RETRIES
        || !match_thinking_signature_error(body)
        || !is_anthropic_messages_payload(payload)
    {
        return false;
    }
    state.attempts += 1;
    true
}

/// `refundThinkingSignatureStrip` / `refundAdvisorResultStrip`.
pub fn refund_strip(state: &mut StripState) {
    state.attempts = state.attempts.saturating_sub(1);
}

/// `matchAdvisorResultError(responseBody)`.
pub fn match_advisor_result_error(body: &str) -> bool {
    !body.is_empty()
        && body
            .to_lowercase()
            .contains("advisor tool result content could not be processed")
}

/// `stripAdvisorResultBlocks(payload)` → `(payload, strippedCount)`.
pub fn strip_advisor_result_blocks(payload: Value) -> (Value, usize) {
    if !is_anthropic_messages_payload(&payload) {
        return (payload, 0);
    }
    let mut sealed: HashSet<String> = HashSet::new();
    for m in payload["messages"].as_array().into_iter().flatten() {
        for b in m
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if get_str(b, &["type"]) == Some("advisor_tool_result") {
                if let Some(id) = get_str(b, &["tool_use_id"]) {
                    sealed.insert(id.to_owned());
                }
            }
        }
    }
    strip_blocks(
        payload,
        move |b| {
            let ty = get_str(b, &["type"]);
            if ty == Some("advisor_tool_result") {
                return (true, true);
            }
            let invocation = matches!(ty, Some("server_tool_use" | "advisor_tool_use"))
                && get_str(b, &["id"]).is_some_and(|id| sealed.contains(id));
            (invocation, false)
        },
        "[advisor result elided]",
    )
}

/// `planAdvisorResultStrip(responseBody, payload, state)`.
pub fn plan_advisor_result_strip(body: &str, payload: &Value, state: &mut StripState) -> bool {
    if state.attempts >= MAX_ADVISOR_RESULT_STRIP_RETRIES
        || !match_advisor_result_error(body)
        || !is_anthropic_messages_payload(payload)
    {
        return false;
    }
    state.attempts += 1;
    true
}

/// `matchLiteUnsupportedToolsError(responseBody)`.
pub fn match_lite_unsupported_tools_error(body: &str) -> bool {
    !body.is_empty() && body.to_lowercase().contains("responses-lite only supports function tools, custom tools, and client-executed tool search")
}

/// `stripLiteUnsupportedTools(payload)` → `(payload, strippedCount)`.
pub fn strip_lite_unsupported_tools(payload: Value) -> (Value, usize) {
    let Some(tools) = payload.get("tools").and_then(Value::as_array) else {
        return (payload, 0);
    };
    let kept: Vec<Value> = tools
        .iter()
        .filter(|t| {
            t.is_object()
                && get_str(t, &["type"]).is_some_and(|ty| LITE_ALLOWED_TOOL_TYPES.contains(&ty))
        })
        .cloned()
        .collect();
    let stripped = tools.len() - kept.len();
    if stripped == 0 {
        return (payload, 0);
    }
    let mut out = obj_of(Some(&payload));
    out.insert("tools".into(), Value::Array(kept));
    (Value::Object(out), stripped)
}

/// `planLiteToolStrip(responseBody, state)`.
pub fn plan_lite_tool_strip(body: &str, state: &mut StripState) -> bool {
    if state.attempts >= MAX_LITE_TOOL_STRIP_RETRIES || !match_lite_unsupported_tools_error(body) {
        return false;
    }
    state.attempts += 1;
    true
}
