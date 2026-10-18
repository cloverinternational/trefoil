//! `utils/usage-normalizer.ts`.
use super::js::{get, num, to_number};
use serde_json::{json, Value};

/// `safeToken`: finite, floored, non-negative (NaN/undefined → 0).
pub fn safe_token(v: Option<&Value>) -> f64 {
    match v.map(|v| if v.is_null() { Some(0.0) } else { to_number(Some(v)) }) {
        Some(Some(n)) if n.is_finite() => n.floor().max(0.0),
        _ => 0.0,
    }
}
fn optional_token(v: Option<&Value>) -> Option<f64> { if super::js::nullish(v) { None } else { Some(safe_token(v)) } }
fn safe_cost(v: Option<&Value>) -> Option<f64> {
    if super::js::nullish(v) { return None; }
    to_number(v).filter(|n| n.is_finite() && *n >= 0.0)
}
fn first<'a>(candidates: &[Option<&'a Value>]) -> Option<&'a Value> {
    candidates.iter().copied().find(|c| !super::js::nullish(*c)).flatten()
}

fn usage(input: f64, output: f64, total: f64, reasoning: f64, cached: f64, cache_creation: f64) -> Value {
    json!({"input_tokens": num(input), "output_tokens": num(output), "total_tokens": num(total), "reasoning_tokens": num(reasoning), "cached_tokens": num(cached), "cache_creation_tokens": num(cache_creation)})
}

pub fn normalize_openai_chat(u: &Value) -> Value {
    let prompt = safe_token(get(u, &["prompt_tokens"]));
    let cached = safe_token(first(&[get(u, &["prompt_tokens_details", "cached_tokens"]), get(u, &["cached_tokens"]), get(u, &["prompt_cache_hit_tokens"])]));
    let cache_write = safe_token(get(u, &["prompt_tokens_details", "cache_write_tokens"]));
    let output = safe_token(get(u, &["completion_tokens"]));
    let reasoning = safe_token(get(u, &["completion_tokens_details", "reasoning_tokens"]));
    let input = if cached > prompt { prompt } else { (prompt - cached).max(0.0) };
    let total = safe_token(get(u, &["total_tokens"]));
    usage(input, output, if total != 0.0 { total } else { input + cached + output }, reasoning, cached, cache_write)
}

pub fn normalize_openai_responses(u: &Value) -> Value {
    let reported = safe_token(get(u, &["input_tokens"]));
    let cached = safe_token(get(u, &["input_tokens_details", "cached_tokens"]));
    let cache_write = safe_token(get(u, &["input_tokens_details", "cache_write_tokens"]));
    let output = safe_token(get(u, &["output_tokens"]));
    let reasoning = safe_token(get(u, &["output_tokens_details", "reasoning_tokens"]));
    let input_image = optional_token(get(u, &["input_tokens_details", "image_tokens"]));
    let output_image = optional_token(get(u, &["output_tokens_details", "image_tokens"]));
    let combined = cached + cache_write;
    let input = if combined > reported { reported } else { (reported - combined).max(0.0) };
    let total = safe_token(get(u, &["total_tokens"]));
    let mut out = usage(input, output, if total != 0.0 { total } else { input + cached + cache_write + output }, reasoning, cached, cache_write);
    if let Some(v) = input_image { out["input_image_tokens"] = num(v); }
    if let Some(v) = output_image { out["output_image_tokens"] = num(v); }
    out
}

pub fn normalize_gemini(u: &Value) -> Value {
    let prompt = safe_token(get(u, &["promptTokenCount"]));
    let cached = safe_token(get(u, &["cachedContentTokenCount"]));
    let output = safe_token(get(u, &["candidatesTokenCount"]));
    let reasoning = safe_token(get(u, &["thoughtsTokenCount"]));
    let tool_use = safe_token(get(u, &["toolUsePromptTokenCount"]));
    let input = if cached > prompt { prompt } else { (prompt - cached).max(0.0) };
    let total = safe_token(get(u, &["totalTokenCount"]));
    usage(input, output, if total != 0.0 { total } else { prompt + output + tool_use + reasoning }, reasoning, cached, 0.0)
}

pub fn normalize_anthropic(u: &Value) -> Value {
    let input = safe_token(get(u, &["input_tokens"]));
    let cached = safe_token(get(u, &["cache_read_input_tokens"]));
    let creation = safe_token(get(u, &["cache_creation_input_tokens"]));
    let output = safe_token(get(u, &["output_tokens"]));
    usage(input, output, input + cached + creation + output, 0.0, cached, creation)
}

pub fn normalize_oauth(u: &Value) -> Value {
    let input = safe_token(first(&[get(u, &["input"]), get(u, &["input_tokens"])]));
    let output = safe_token(first(&[get(u, &["output"]), get(u, &["output_tokens"])]));
    let cached = safe_token(first(&[get(u, &["cacheRead"]), get(u, &["cached_tokens"])]));
    let creation = safe_token(first(&[get(u, &["cacheWrite"]), get(u, &["cache_creation_tokens"])]));
    let total = safe_token(first(&[get(u, &["totalTokens"]), get(u, &["total_tokens"])]));
    usage(input, output, if total != 0.0 { total } else { input + cached + creation + output }, safe_token(get(u, &["reasoning_tokens"])), cached, creation)
}

/// `extractUsageCostDetails`.
pub fn extract_cost_details(u: &Value) -> Option<Value> {
    let keys = ["total_cost", "input_cost", "output_cost", "cached_input_cost", "cache_write_input_cost", "upstream_inference_cost", "upstream_inference_prompt_cost", "upstream_inference_completions_cost", "request_cost", "web_search_cost", "image_input_cost", "image_output_cost", "audio_input_cost", "data_storage_cost"];
    let cost_from_usage = safe_cost(first(&[get(u, &["cost"]), get(u, &["estimated_cost"])]));
    let details = get(u, &["cost_details"]).filter(|d| d.is_object());
    let Some(details) = details else {
        let ticks = get(u, &["cost_in_usd_ticks"]).and_then(Value::as_f64).and_then(|t| safe_cost(Some(&num(t / 10_000_000_000.0))));
        let total = if cost_from_usage.is_some_and(|c| c != 0.0) { cost_from_usage } else { ticks };
        let total = total?;
        let mut out = serde_json::Map::new();
        for k in keys { out.insert(k.into(), Value::Null); }
        out.insert("total_cost".into(), num(total));
        return Some(Value::Object(out));
    };
    let mut total = safe_cost(details.get("total_cost"));
    if total.is_none() {
        let upstream = safe_cost(details.get("upstream_inference_cost"));
        total = if cost_from_usage.is_some_and(|c| c != 0.0) { cost_from_usage } else { upstream };
    }
    let total = total?;
    let c = |k: &str| safe_cost(details.get(k)).map(num).unwrap_or(Value::Null);
    let c2 = |a: &str, b: &str| safe_cost(first(&[details.get(a), details.get(b)])).map(num).unwrap_or(Value::Null);
    Some(json!({
        "total_cost": num(total), "input_cost": c("input_cost"), "output_cost": c("output_cost"),
        "cached_input_cost": c("cached_input_cost"), "cache_write_input_cost": c("cache_write_input_cost"),
        "upstream_inference_cost": c("upstream_inference_cost"),
        "upstream_inference_prompt_cost": c2("upstream_inference_prompt_cost", "upstream_inference_input_cost"),
        "upstream_inference_completions_cost": c2("upstream_inference_completions_cost", "upstream_inference_output_cost"),
        "request_cost": c("request_cost"), "web_search_cost": c("web_search_cost"), "image_input_cost": c("image_input_cost"),
        "image_output_cost": c("image_output_cost"), "audio_input_cost": c("audio_input_cost"), "data_storage_cost": c("data_storage_cost"),
    }))
}
