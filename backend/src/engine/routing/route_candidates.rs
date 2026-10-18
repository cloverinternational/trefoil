//! `routing/route-candidates.ts` (`resolveRouteCandidates`) plus the pure
//! helpers it uses: `appendSkippedAttempt` (dispatch/attempt-history.ts),
//! `QuotaEnforcer.filterCandidates`, `buildQuotaExceededBody` /
//! `buildQuotaExceededError` (quota-middleware.ts) and `mostConstrained`
//! (shared/quota-ranking.ts).
use super::key_access_policy::apply_key_access_policy;
use super::router::{resolve, resolve_candidates, split_service_tier};
use super::runtime::RoutingRuntime;
use super::scope_match::scope_matches;
use super::{iso_from_ms, js_num, RouteResult};
use crate::engine::js::{get, obj, to_string, truthy};
use crate::engine::transformer::{EngineError, EngineResult};
use serde_json::{json, Value};

/// `appendSkippedAttempt(retryHistory, route, reason, apiType?)`.
pub fn append_skipped_attempt(retry_history: &mut Vec<Value>, route: &RouteResult, reason: &str, api_type: Option<&str>) {
    let index = retry_history.len() + 1;
    retry_history.push(obj(vec![
        ("index", Some(json!(index))),
        ("provider", Some(json!(route.provider))),
        ("model", Some(json!(route.model))),
        ("apiType", api_type.map(|a| json!(a))),
        ("status", Some(json!("skipped"))),
        ("reason", Some(json!(reason))),
        ("retryable", Some(json!(false))),
    ]));
}

/// `QuotaEnforcer.filterCandidates(ctx, candidates)`: a candidate is blocked
/// by the first check with `allowed === false` whose `scope` matches it.
/// `ctx` is the JSON `QuotaContext` (`{keyName, checks: QuotaCheckSnapshot[],
/// blockedGlobal}`). Returns (allowed, [(candidate, blocking quota)]).
pub fn filter_quota_candidates(ctx: Option<&Value>, candidates: Vec<RouteResult>) -> (Vec<RouteResult>, Vec<(RouteResult, Value)>) {
    let Some(ctx) = ctx.filter(|c| truthy(Some(c))) else { return (candidates, vec![]) };
    let checks = ctx.get("checks").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut allowed = Vec::new();
    let mut blocked = Vec::new();
    for c in candidates {
        let exhausted = checks.iter().find(|q| !truthy(q.get("allowed")) && scope_matches(q.get("scope").unwrap_or(&Value::Null), &c.provider, &c.model));
        match exhausted {
            Some(q) => blocked.push((c, q.clone())),
            None => allowed.push(c),
        }
    }
    (allowed, blocked)
}

/// `constrainedRatio`: `limit > 0 ? remaining / limit : 0`.
fn constrained_ratio(q: &Value) -> f64 {
    let limit = js_num(q.get("limit"));
    if limit > 0.0 { js_num(q.get("remaining")) / limit } else { 0.0 }
}

/// `mostConstrained(entries)`: smallest remaining/limit ratio, first wins ties.
pub fn most_constrained(entries: &[Value]) -> Option<&Value> {
    let mut it = entries.iter();
    let first = it.next()?;
    Some(it.fold(first, |min, e| if constrained_ratio(e) < constrained_ratio(min) { e } else { min }))
}

/// `buildQuotaExceededBody(blocking)` (assumes non-empty `blocking`).
pub fn build_quota_exceeded_body(blocking: &[Value]) -> Value {
    let primary = most_constrained(blocking).cloned().unwrap_or(Value::Null);
    let cloned = |v: Option<&Value>| v.cloned();
    json!({"error": obj(vec![
        ("message", Some(json!(quota_message(&primary)))),
        ("type", Some(json!("quota_exceeded"))),
        ("quota_name", cloned(primary.get("quotaName"))),
        ("current_usage", cloned(primary.get("currentUsage"))),
        ("limit", cloned(primary.get("limit"))),
        ("resets_at", Some(json!(iso_from_ms(js_num(primary.get("resetsAtMs")))))),
        ("blocking_quotas", Some(Value::Array(blocking.iter().map(|q| obj(vec![
            ("quotaName", cloned(q.get("quotaName"))),
            ("limitType", cloned(q.get("limitType"))),
            ("limit", cloned(q.get("limit"))),
            ("currentUsage", cloned(q.get("currentUsage"))),
            ("remaining", cloned(q.get("remaining"))),
            ("resetsAt", Some(json!(iso_from_ms(js_num(q.get("resetsAtMs")))))),
        ])).collect()))),
    ])})
}

fn quota_message(primary: &Value) -> String {
    format!("Quota exceeded: {} limit of {} reached", to_string(primary.get("quotaName")), to_string(primary.get("limit")))
}

/// `buildQuotaExceededError` without `body` / `retryHistory` (which
/// `EngineError` cannot carry): message + `{statusCode: 429, code: 'quota_exceeded'}`.
pub fn default_quota_exceeded_error(blocking: &[Value]) -> EngineError {
    let primary = most_constrained(blocking).cloned().unwrap_or(Value::Null);
    EngineError::status(quota_message(&primary), 429, "quota_exceeded")
}

/// `resolveRouteCandidates(request, retryHistory, sessionKey, appendSkippedAttempt)`.
///
/// `request` is the unified request JSON (`model`, `incomingApiType`,
/// `metadata.plexus_metadata.{plexus_key_policy, plexus_quota_context}`);
/// `request.serviceTier` is set when the model carries an `@<tier>` suffix.
/// Quota-blocked candidates are recorded in `retry_history` via
/// `append_skipped_attempt` (`quota_exceeded:<quotaName>`).
pub async fn resolve_route_candidates(rt: &dyn RoutingRuntime, request: &mut Value, retry_history: &mut Vec<Value>, session_key: Option<&str>) -> EngineResult<Vec<RouteResult>> {
    let model = to_string(request.get("model"));
    let split = split_service_tier(rt, &model);
    if let (Some(tier), Some(map)) = (&split.service_tier, request.as_object_mut()) {
        map.insert("serviceTier".into(), json!(tier));
    }
    let routed = split.model.clone();
    let incoming = request.get("incomingApiType").and_then(Value::as_str).map(str::to_owned);

    let mut candidates = resolve_candidates(rt, &routed, incoming.as_deref(), session_key).await?;
    if candidates.is_empty() {
        candidates = vec![resolve(rt, &routed, incoming.as_deref()).await?];
    }
    if candidates.is_empty() {
        return Err(EngineError::new(format!("No route candidates found for model '{model}'")));
    }

    let api_type = incoming.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| "chat".into());
    let mut policy_request = request.clone();
    if let Some(map) = policy_request.as_object_mut() {
        if split.service_tier.is_some() { map.insert("baseModel".into(), json!(routed)); } else { map.remove("baseModel"); }
    }
    let candidates = apply_key_access_policy(&policy_request, candidates, &api_type)?;

    let quota_ctx = get(request, &["metadata", "plexus_metadata", "plexus_quota_context"]).filter(|c| !c.is_null()).cloned();
    let Some(quota_ctx) = quota_ctx.filter(|c| truthy(Some(c))) else { return Ok(candidates) };

    let (allowed, blocked) = filter_quota_candidates(Some(&quota_ctx), candidates);
    for (candidate, quota) in &blocked {
        append_skipped_attempt(retry_history, candidate, &format!("quota_exceeded:{}", to_string(quota.get("quotaName"))), Some(&api_type));
    }
    if allowed.is_empty() {
        let quotas: Vec<Value> = blocked.into_iter().map(|(_, q)| q).collect();
        return Err(rt.build_quota_exceeded_error(&quotas, retry_history));
    }
    Ok(allowed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::routing::runtime::mock::MockRuntime;
    use serde_json::json;

    fn rt() -> MockRuntime {
        MockRuntime::new(json!({
            "providers": {
                "p1": {"api_base_url": "https://p1/v1", "models": {"m1": {}}},
                "p2": {"api_base_url": "https://p2/v1", "models": {"m2": {}}}
            },
            "models": {"luna": {"target_groups": [{"name": "g", "selector": "in_order", "targets": [{"provider": "p1", "model": "m1"}, {"provider": "p2", "model": "m2"}]}]}}
        }))
    }

    #[tokio::test]
    async fn tier_suffix_direct_fallback_and_policy() {
        let rt = rt();
        let mut req = json!({"model": "luna@fast", "incomingApiType": "chat"});
        let mut hist = vec![];
        let c = resolve_route_candidates(&rt, &mut req, &mut hist, None).await.unwrap();
        assert_eq!(c.len(), 2);
        assert_eq!(req["serviceTier"], json!("fast"));
        assert_eq!(c[0].incoming_model_alias.as_deref(), Some("luna"));

        let mut req = json!({"model": "direct/p2/m2"});
        assert_eq!(resolve_route_candidates(&rt, &mut req, &mut hist, None).await.unwrap()[0].provider, "p2");
        let mut req = json!({"model": "ghost"});
        assert_eq!(resolve_route_candidates(&rt, &mut req, &mut hist, None).await.unwrap_err().message, "Model 'ghost' not found in configuration");

        let mut req = json!({"model": "luna@priority", "metadata": {"plexus_metadata": {"plexus_key_policy": {"allowedModels": ["luna@fast"], "allowedProviders": ["p2"]}}}});
        let c = resolve_route_candidates(&rt, &mut req, &mut hist, None).await.unwrap();
        assert_eq!(c.iter().map(|c| c.provider.as_str()).collect::<Vec<_>>(), vec!["p2"]);
        let mut req = json!({"model": "luna", "metadata": {"plexus_metadata": {"plexus_key_policy": {"allowedModels": ["luna@fast"]}}}});
        let e = resolve_route_candidates(&rt, &mut req, &mut hist, None).await.unwrap_err();
        assert_eq!((e.message.as_str(), e.status), ("Key is not allowed to access model 'luna' for chat", Some(403)));
    }

    #[tokio::test]
    async fn quota_filtering() {
        let rt = rt();
        let check = |name: &str, scope: Value, allowed: bool, remaining: f64| json!({"quotaName": name, "limitType": "requests", "limit": 10, "currentUsage": 10.0 - remaining,
            "remaining": remaining, "allowed": allowed, "resetsAtMs": 0, "scope": scope, "global": false, "shared": false, "source": "assigned"});
        let mut req = json!({"model": "luna", "incomingApiType": "messages", "metadata": {"plexus_metadata": {"plexus_quota_context": {"keyName": "k", "blockedGlobal": null,
            "checks": [check("p1-only", json!({"allowedProviders": ["p1"]}), false, 0.0), check("ok", json!({}), true, 5.0)]}}}});
        let mut hist = vec![];
        let c = resolve_route_candidates(&rt, &mut req, &mut hist, None).await.unwrap();
        assert_eq!(c.iter().map(|c| c.provider.as_str()).collect::<Vec<_>>(), vec!["p2"]);
        assert_eq!(hist, vec![json!({"index": 1, "provider": "p1", "model": "m1", "apiType": "messages", "status": "skipped", "reason": "quota_exceeded:p1-only", "retryable": false})]);

        let mut req = json!({"model": "luna", "metadata": {"plexus_metadata": {"plexus_quota_context": {"keyName": "k", "blockedGlobal": null,
            "checks": [check("all", json!({}), false, 1.0), check("tighter", json!({}), false, 0.0)]}}}});
        let mut hist = vec![];
        let e = resolve_route_candidates(&rt, &mut req, &mut hist, None).await.unwrap_err();
        assert_eq!((e.message.as_str(), e.status, e.code.as_deref()), ("Quota exceeded: all limit of 10 reached", Some(429), Some("quota_exceeded")));
        assert_eq!(hist.len(), 2);
        let body = build_quota_exceeded_body(&[check("a", json!({}), false, 3.0), check("b", json!({}), false, 1.0)]);
        assert_eq!(body["error"]["quota_name"], json!("b"));
        assert_eq!(body["error"]["resets_at"], json!("1970-01-01T00:00:00.000Z"));
        assert_eq!(body["error"]["blocking_quotas"].as_array().unwrap().len(), 2);
    }
}
