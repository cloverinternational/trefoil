//! `POST /v1/decisions` (alias `/v1/systemone`), mirroring pinned Plexus
//! `routes/inference/decisions.ts`, `types/decisions.ts` and
//! `MediaDispatcher.dispatchDecisions`.
//!
//! Buffered Jev-style Decisions on the System One protocol: `{model, state,
//! questions}` in, one typed answer per question out. Only targets whose
//! provider advertises the `systemone` API type (or a deprecated alias) can
//! serve the request; the body sent upstream is exactly `{model, state,
//! questions}` and the upstream core response is validated (502 otherwise).
use crate::engine::dispatcher::engine::KEY_NAME;
use crate::engine::dispatcher::quota::{attach_quota_context, load_quota_context, quota_headers, record_usage};
use crate::engine::dispatcher::routes::authenticate_peer;
use crate::engine::js::*;
use crate::engine::media::dispatcher::{self as md, error_details};
use crate::engine::observability::QuotaUsage;
use crate::engine::routing::route_candidates::build_quota_exceeded_body;
use crate::media_routes::{attach_key_access_policy, error_info, peer_ip, Ctx};
use crate::AppState;
use axum::{extract::{Request, State}, response::Response, routing::post, Router};
use serde_json::{json, Map, Value};

const MAX_CHOICE_OPTIONS: usize = 255;
const MIN_SCORE_LEVELS: usize = 2;
const MAX_SCORE_LEVELS: usize = 10;

fn type_name(value: Option<&Value>) -> &'static str {
    match value {
        None => "undefined",
        Some(Value::Null) => "null",
        Some(Value::Bool(_)) => "boolean",
        Some(Value::Number(_)) => "number",
        Some(Value::String(_)) => "string",
        Some(Value::Array(_)) => "array",
        Some(Value::Object(_)) => "object",
    }
}
fn expected(kind: &str, value: Option<&Value>) -> String { format!("Invalid input: expected {kind}, received {}", type_name(value)) }
/// Structured guidance: string, JSON object or array (zod union → "Invalid input").
fn structured(value: Option<&Value>) -> bool { matches!(value, Some(Value::String(_) | Value::Object(_) | Value::Array(_))) }

fn validate_question(question: Option<&Value>) -> Result<(), String> {
    let Some(Value::Object(q)) = question else { return Err(expected("object", question)) };
    let kind = q.get("type").and_then(Value::as_str);
    if !matches!(kind, Some("noul" | "choice" | "score")) {
        return Err("Invalid discriminator value. Expected 'noul' | 'choice' | 'score'".into());
    }
    if !structured(q.get("instructions")) { return Err("Invalid input".into()); }
    let criteria = q.get("criteria");
    match kind.unwrap() {
        "noul" => match criteria {
            None => Ok(()),
            Some(Value::Object(c)) => if structured(c.get("true")) && structured(c.get("false")) { Ok(()) } else { Err("Invalid input".into()) },
            other => Err(expected("object", other)),
        },
        "choice" => match criteria {
            Some(Value::Object(c)) => {
                if c.values().any(|v| !(v.is_null() || structured(Some(v)))) { return Err("Invalid input".into()); }
                if c.is_empty() { return Err("choice criteria must define at least one option".into()); }
                if c.len() > MAX_CHOICE_OPTIONS { return Err(format!("choice criteria must define at most {MAX_CHOICE_OPTIONS} options")); }
                Ok(())
            }
            other => Err(expected("record", other)),
        },
        _ => match criteria {
            Some(Value::Array(levels)) => {
                if levels.iter().any(|v| !structured(Some(v))) { return Err("Invalid input".into()); }
                if levels.len() < MIN_SCORE_LEVELS { return Err(format!("score criteria must define at least {MIN_SCORE_LEVELS} levels")); }
                if levels.len() > MAX_SCORE_LEVELS { return Err(format!("score criteria must define at most {MAX_SCORE_LEVELS} levels")); }
                Ok(())
            }
            other => Err(expected("array", other)),
        },
    }
}

/// Ingress validation with zod's first-issue message (schema key order).
fn validate(body: &Value) -> Result<(String, Value, Value), String> {
    let Value::Object(object) = body else { return Err(expected("object", Some(body))) };
    let model = match object.get("model") {
        Some(Value::String(m)) if !m.trim().is_empty() => Some(m.trim().to_owned()),
        Some(Value::String(_)) => return Err("model must be a non-empty string".into()),
        other => return Err(expected("string", other)),
    };
    if !structured(object.get("state")) { return Err("Invalid input".into()); }
    let questions = match object.get("questions") {
        Some(Value::Object(q)) => q,
        other => return Err(expected("record", other)),
    };
    for question in questions.values() { validate_question(Some(question))?; }
    if questions.is_empty() { return Err("questions must define at least one question".into()); }
    if object.contains_key("stream") { return Err("streaming is not supported for decisions requests".into()); }
    Ok((model.unwrap(), object["state"].clone(), Value::Object(questions.clone())))
}

fn number(value: Option<&Value>) -> Result<f64, String> { value.and_then(Value::as_f64).ok_or_else(|| expected("number", value)) }
fn number_record(value: Option<&Value>) -> Result<Option<Value>, String> {
    match value {
        None => Ok(None),
        Some(Value::Object(m)) if m.values().all(Value::is_number) => Ok(Some(Value::Object(m.clone()))),
        Some(Value::Object(_)) => Err("Invalid input: expected number".into()),
        other => Err(expected("record", other)),
    }
}

/// Upstream core validation; unknown answer keys are stripped like zod.
pub(crate) fn parse_upstream(body: &Value) -> Result<Value, String> {
    let model = match body.get("model") { Some(Value::String(m)) if !m.is_empty() => m.clone(), Some(Value::String(_)) => return Err("Too small: expected string to have >=1 characters".into()), other => return Err(expected("string", other)) };
    let Some(Value::Object(answers)) = body.get("answers") else { return Err(expected("record", body.get("answers"))) };
    let mut parsed = Map::new();
    for (id, answer) in answers {
        let Some(Value::Object(a)) = Some(answer) else { return Err(expected("object", Some(answer))) };
        let mut out = Map::new();
        match a.get("type").and_then(Value::as_str) {
            Some("noul") => { out.insert("type".into(), "noul".into()); out.insert("noul".into(), Value::from(number(a.get("noul"))?)); }
            Some(kind @ ("choice" | "score")) => {
                out.insert("type".into(), kind.into());
                if kind == "choice" {
                    let Some(Value::String(choice)) = a.get("choice") else { return Err(expected("string", a.get("choice"))) };
                    out.insert("choice".into(), choice.clone().into());
                } else {
                    out.insert("score".into(), Value::from(number(a.get("score"))?));
                    if let Some(legend) = a.get("legend") {
                        let Value::Object(entries) = legend else { return Err(expected("record", Some(legend))) };
                        if entries.values().any(|v| !structured(Some(v))) { return Err("Invalid input".into()); }
                        out.insert("legend".into(), legend.clone());
                    }
                }
                if let Some(p) = number_record(a.get("probabilities"))? { out.insert("probabilities".into(), p); }
                if a.contains_key("confidence") { out.insert("confidence".into(), Value::from(number(a.get("confidence"))?)); }
            }
            _ => return Err("Invalid discriminator value. Expected 'noul' | 'choice' | 'score'".into()),
        }
        // Keep integer formatting identical to JSON.parse round-tripping.
        for key in ["noul", "score", "confidence"] {
            if let Some(original) = a.get(key) { if out.contains_key(key) { out.insert(key.into(), original.clone()); } }
        }
        parsed.insert(id.clone(), Value::Object(out));
    }
    let usage = body.get("usage");
    let Some(Value::Object(u)) = usage else { return Err(expected("object", usage)) };
    number(u.get("input_tokens"))?;
    number(u.get("output_tokens"))?;
    if u.contains_key("cost") { number(u.get("cost"))?; }
    if parsed.is_empty() { return Err("empty answers map".into()); }
    let mut usage_out = Map::new();
    usage_out.insert("input_tokens".into(), u["input_tokens"].clone());
    usage_out.insert("output_tokens".into(), u["output_tokens"].clone());
    if let Some(cost) = u.get("cost") { usage_out.insert("cost".into(), cost.clone()); }
    let mut response = Map::new();
    response.insert("model".into(), model.into());
    response.insert("answers".into(), Value::Object(parsed));
    response.insert("usage".into(), Value::Object(usage_out));
    if let Some(Value::String(id)) = body.get("id") { response.insert("id".into(), id.clone().into()); }
    if let Some(Value::String(provider)) = body.get("provider") { response.insert("provider".into(), provider.clone().into()); }
    Ok(Value::Object(response))
}

pub fn router() -> Router<AppState> {
    Router::new().route("/v1/decisions", post(handler)).route("/v1/systemone", post(handler))
}

pub async fn handler(State(state): State<AppState>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let peer = peer_ip(&parts.extensions);
    let caller = match authenticate_peer(&state, &parts.headers, parts.uri.query(), peer) { Ok(c) => c, Err(r) => return r };
    let raw = axum::body::to_bytes(body, usize::MAX).await.unwrap_or_default();
    let mut ctx = Ctx::new(&state, &parts.headers, peer, "decisions");
    KEY_NAME.scope(Some(caller.key_name.clone()), async move {
        // `request.body ?? {}`.
        let body: Value = if raw.is_empty() { Value::Object(Map::new()) } else {
            match serde_json::from_slice(&raw) { Ok(Value::Null) | Err(_) => Value::Object(Map::new()), Ok(v) => v }
        };
        let (model, decision_state, questions) = match validate(&body) {
            Ok(parsed) => parsed,
            Err(message) => {
                let e = json!({"message": message, "routingContext": {"statusCode": 400, "code": "invalid_request_error"}});
                return ctx.fail(e, "decisions", true).await;
            }
        };
        ctx.caller_fields(&caller, Some(&json!(model)));
        let mut unified = json!({
            "model": model,
            "state": decision_state,
            "questions": questions,
            "requestId": ctx.request_id,
            "incomingApiType": "decisions",
            "originalBody": body,
        });
        attach_key_access_policy(&caller.config, &mut unified);
        ctx.start_log(&body, &parts.headers, parts.uri.query());
        let quota_context = load_quota_context(&ctx.engine.state, &ctx.engine.config, &caller.key_name).await;
        if let Some(qc) = &quota_context {
            if let Some(blocked) = qc.get("blockedGlobal").filter(|b| !b.is_null()) {
                let body = build_quota_exceeded_body(std::slice::from_ref(blocked));
                ctx.record.set("responseStatus", "quota_exceeded");
                ctx.record.set_num("durationMs", (now_ms() - ctx.start) as f64);
                ctx.save().await;
                return ctx.json(429, &body);
            }
            attach_quota_context(&mut unified, qc);
        }
        let response = match md::dispatch_decisions(&ctx.engine, &unified, None, &parse_upstream).await {
            Ok(r) => r,
            Err(e) => {
                if get(&e, &["routingContext", "code"]) == Some(&json!("quota_exceeded")) {
                    let rc = |k: &str| get(&e, &["routingContext", k]).filter(|v| truthy(Some(v))).cloned();
                    ctx.record.set("responseStatus", "quota_exceeded");
                    ctx.record.set_num("durationMs", (now_ms() - ctx.start) as f64);
                    ctx.record.set("attemptCount", rc("attemptCount").or_else(|| ctx.record.get("attemptCount").filter(|v| truthy(Some(v))).cloned()).unwrap_or(json!(1)));
                    ctx.record.set("retryHistory", rc("retryHistory").or_else(|| ctx.record.get("retryHistory").filter(|v| truthy(Some(v))).cloned()).unwrap_or(Value::Null));
                    ctx.save().await;
                    ctx.engine.sink().save_error(&ctx.request_id, &error_info(&e), Some(&error_details("decisions", &e)), None).await;
                    return ctx.json(429, &get(&e, &["routingContext", "body"]).cloned().unwrap_or(Value::Null));
                }
                return ctx.fail(e, "decisions", true).await;
            }
        };
        let plexus = response.get("plexus").cloned();
        let p = |k: &str| plexus.as_ref().and_then(|p| p.get(k)).cloned();
        ctx.routed(plexus.as_ref());
        ctx.record.set("outgoingApiType", coalesce(p("targetApiType").as_ref(), p("apiType").as_ref()).unwrap_or(Value::Null));
        for key in ["requestedServiceTier", "requestedServiceTierRaw", "serviceTier", "serviceTierRaw"] {
            ctx.record.set(key, coalesce(p(key).as_ref(), None).unwrap_or(Value::Null));
        }
        ctx.record.set("isPassthrough", json!(false));
        let u = |k: &str| coalesce(get(&response, &["usage", k]), None).unwrap_or(Value::Null);
        ctx.record.set("tokensInput", u("input_tokens"));
        ctx.record.set("tokensOutput", u("output_tokens"));
        ctx.record.set("providerReportedCost", u("cost"));
        ctx.finish_success(plexus.as_ref());
        ctx.attempt_metadata(plexus.as_ref());
        ctx.save().await;
        let provider = to_string(p("provider").filter(|v| truthy(Some(v))).as_ref().or(Some(&json!(""))));
        let routed_model = to_string(p("model").filter(|v| truthy(Some(v))).as_ref().or(Some(&json!(""))));
        let extra_headers = quota_context.as_ref().map(|qc| quota_headers(qc, &provider, &routed_model)).unwrap_or_default();
        let usage = QuotaUsage {
            tokens_input: ctx.record.get("tokensInput").and_then(Value::as_f64),
            tokens_output: ctx.record.get("tokensOutput").and_then(Value::as_f64),
            cost_total: ctx.record.get("costTotal").and_then(Value::as_f64),
            ..Default::default()
        };
        record_usage(&ctx.engine.state, &ctx.engine.config, &caller.key_name, &provider, &routed_model, &usage).await;
        let answer_count = get_obj(&response, &["answers"]).map(|a| a.len()).unwrap_or(0);
        ctx.finish_log(&obj(vec![("model", response.get("model").cloned()), ("answerCount", Some(json!(answer_count)))]));
        let mut client = Map::new();
        client.insert("model".into(), response.get("model").cloned().unwrap_or(Value::Null));
        client.insert("answers".into(), response.get("answers").cloned().unwrap_or(Value::Null));
        client.insert("usage".into(), response.get("usage").cloned().unwrap_or(Value::Null));
        if let Some(id) = response.get("id") { client.insert("id".into(), id.clone()); }
        if let Some(provider) = response.get("provider") { client.insert("provider".into(), provider.clone()); }
        let mut reply = ctx.json(200, &Value::Object(client));
        for (k, v) in extra_headers {
            if let (Ok(name), Ok(value)) = (axum::http::HeaderName::from_bytes(k.as_bytes()), axum::http::HeaderValue::from_str(&v)) { reply.headers_mut().insert(name, value); }
        }
        reply
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ingress_messages_match_pinned_zod_schema() {
        let q = serde_json::json!({"a":{"type":"noul","instructions":"x"}});
        let cases = [
            (serde_json::json!({"state":"s","questions":q}), "Invalid input: expected string, received undefined"),
            (serde_json::json!({"model":"  ","state":"s","questions":q}), "model must be a non-empty string"),
            (serde_json::json!({"model":"m","state":5,"questions":q}), "Invalid input"),
            (serde_json::json!({"model":"m","state":"s"}), "Invalid input: expected record, received undefined"),
            (serde_json::json!({"model":"m","state":"s","questions":{}}), "questions must define at least one question"),
            (serde_json::json!({"model":"m","state":"s","questions":[]}), "Invalid input: expected record, received array"),
            (serde_json::json!({"model":"m","state":"s","questions":{"a":{"type":"x","instructions":"i"}}}), "Invalid discriminator value. Expected 'noul' | 'choice' | 'score'"),
            (serde_json::json!({"model":"m","state":"s","questions":{"a":{"type":"noul","instructions":"i","criteria":{"true":"t"}}}}), "Invalid input"),
            (serde_json::json!({"model":"m","state":"s","questions":{"a":{"type":"choice","instructions":"i","criteria":{}}}}), "choice criteria must define at least one option"),
            (serde_json::json!({"model":"m","state":"s","questions":{"a":{"type":"choice","instructions":"i"}}}), "Invalid input: expected record, received undefined"),
            (serde_json::json!({"model":"m","state":"s","questions":{"a":{"type":"score","instructions":"i","criteria":["a"]}}}), "score criteria must define at least 2 levels"),
            (serde_json::json!({"model":"m","state":"s","questions":q,"stream":false}), "streaming is not supported for decisions requests"),
            (serde_json::json!("str"), "Invalid input: expected object, received string"),
            (serde_json::json!({"model":"m","state":"s","questions":{"a":5}}), "Invalid input: expected object, received number"),
        ];
        for (body, message) in cases { assert_eq!(validate(&body).unwrap_err(), message, "{body}"); }
        assert!(validate(&serde_json::json!({"model":"m","state":"s","questions":{"a":{"type":"choice","instructions":"i","criteria":{"x":null}}}})).is_ok());
    }
    #[test]
    fn upstream_core_is_validated_and_stripped() {
        let parsed = parse_upstream(&serde_json::json!({"model":"m","id":"r1","extra":1,"answers":{"a":{"type":"choice","choice":"x","junk":true}},"usage":{"input_tokens":1,"output_tokens":2,"other":3}})).unwrap();
        assert_eq!(parsed, serde_json::json!({"model":"m","id":"r1","answers":{"a":{"type":"choice","choice":"x"}},"usage":{"input_tokens":1,"output_tokens":2}}));
        assert_eq!(parse_upstream(&serde_json::json!({"model":"m","answers":{},"usage":{"input_tokens":1,"output_tokens":2}})).unwrap_err(), "empty answers map");
    }
}
