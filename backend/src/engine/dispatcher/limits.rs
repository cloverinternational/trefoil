//! `services/models/enforce-limits.ts` (`enforceContextLimit`): the
//! per-candidate context window check for aliases with `enforce_limits`.
use crate::engine::config::Config;
use crate::engine::js::*;
use crate::engine::observability::estimate::estimate_input_tokens;
use serde_json::{json, Value};

const ESTIMATE_SAFETY_MULTIPLIER: f64 = 1.1;
const DEFAULT_OUTPUT_RESERVATION: f64 = 4096.0;

fn resolve_metadata(config: &Config, alias: &Value, slug: &str) -> Option<Value> {
    let auto = match alias.get("metadata") {
        None | Some(Value::Null) => true,
        Some(m) => get_str(m, &["source"]) == Some("auto"),
    };
    let providers = if auto {
        config
            .root
            .get("providers")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default()
    } else {
        Obj::new()
    };
    crate::models_routes::resolve_model_metadata(slug, alias, &providers)
}

fn positive(v: Option<&Value>) -> Option<f64> {
    v.filter(|v| !v.is_null()).and_then(Value::as_f64)
}

/// `enforceContextLimit(request, aliasConfig, aliasSlug)`; `Err` is the
/// `ContextLengthExceededError` value (`{message, routingContext}`).
pub fn enforce_context_limit(
    config: &Config,
    request: &Value,
    alias: &Value,
    slug: &str,
) -> Result<(), Value> {
    let merged = resolve_metadata(config, alias, slug);
    let m = merged.as_ref();
    let context_length = positive(m.and_then(|m| get(m, &["top_provider", "context_length"])))
        .or_else(|| positive(m.and_then(|m| m.get("context_length"))));
    let Some(context_length) = context_length.filter(|c| *c > 0.0) else {
        return Ok(());
    };
    let metadata_max = positive(m.and_then(|m| get(m, &["top_provider", "max_completion_tokens"])));
    let requested = request
        .get("max_tokens")
        .and_then(Value::as_f64)
        .filter(|n| *n > 0.0);
    let reserved = match (requested, metadata_max) {
        (Some(r), Some(mm)) => r.min(mm),
        (r, mm) => r.or(mm).unwrap_or(DEFAULT_OUTPUT_RESERVATION),
    };
    let api_type = get_str(request, &["incomingApiType"])
        .filter(|s| !s.is_empty())
        .unwrap_or("chat");
    let body = match request.get("originalBody") {
        Some(b) if !b.is_null() => b.clone(),
        _ => json!({"messages": request.get("messages").cloned().unwrap_or(Value::Null)}),
    };
    let estimated = (estimate_input_tokens(&body, api_type) * ESTIMATE_SAFETY_MULTIPLIER).ceil();
    if estimated + reserved > context_length {
        let (cl, est, res) = (
            js_num_string(context_length),
            js_num_string(estimated),
            js_num_string(reserved),
        );
        let message = format!("This model's context window is {cl} tokens. Your request is estimated at ~{est} input tokens with {res} reserved for the response, which exceeds the limit. Please shorten the prompt or lower max_tokens.");
        return Err(json!({"message": message, "routingContext": {
            "statusCode": 400, "code": "context_length_exceeded",
            "estimatedInputTokens": num(estimated), "reservedOutputTokens": num(reserved), "contextLength": num(context_length), "aliasSlug": slug,
        }}));
    }
    Ok(())
}
