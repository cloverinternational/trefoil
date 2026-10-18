//! `routing/key-access-policy.ts`.
//!
//! The policy request is JSON shaped like the TS `PolicyRequest`:
//! `{ model, baseModel?, serviceTier?, metadata?: { plexus_metadata?: {
//! plexus_key_policy?: ScopeLists } } }` (a unified request spread with
//! `baseModel` works as-is).
use super::scope_match::{is_global_scope, list_allows, list_allows_any};
use super::service_tier::service_tier_names;
use super::RouteResult;
use crate::engine::js::{get, get_str, to_string, truthy};
use crate::engine::transformer::{EngineError, EngineResult};
use serde_json::Value;

/// `buildAccessDeniedError`: `routingContext = { statusCode: 403,
/// errorType: 'access_denied' }`. `errorType` has no consumer in Plexus and no
/// `EngineError` field; `code` stays unset as in TS.
fn access_denied(message: String) -> EngineError { EngineError { message, status: Some(403), code: None } }

/// `getKeyAccessPolicy`: the non-global policy, or `None`.
pub fn get_key_access_policy(request: &Value) -> Option<&Value> {
    let policy = get(request, &["metadata", "plexus_metadata", "plexus_key_policy"]).filter(|p| truthy(Some(p)))?;
    if is_global_scope(policy) { return None; }
    Some(policy)
}

fn arr<'a>(policy: &'a Value, key: &str) -> Option<&'a [Value]> {
    policy.get(key).and_then(Value::as_array).map(Vec::as_slice)
}

/// `applyKeyAccessPolicy(request, candidates, apiType)`.
pub fn apply_key_access_policy(request: &Value, candidates: Vec<RouteResult>, api_type: &str) -> EngineResult<Vec<RouteResult>> {
    let Some(policy) = get_key_access_policy(request) else { return Ok(candidates) };
    let model = to_string(request.get("model"));

    let mut requested_names = vec![model.clone()];
    if let Some(base) = get_str(request, &["baseModel"]).filter(|s| !s.is_empty()) {
        requested_names.push(base.to_owned());
        if let Some(tier) = get_str(request, &["serviceTier"]).filter(|s| !s.is_empty()) {
            requested_names.extend(service_tier_names(base, tier));
        }
    }
    if !list_allows_any(arr(policy, "allowedModels"), arr(policy, "excludedModels"), &requested_names) {
        return Err(access_denied(format!("Key is not allowed to access model '{model}' for {api_type}")));
    }

    let filtered: Vec<RouteResult> = candidates.into_iter()
        .filter(|c| list_allows(arr(policy, "allowedProviders"), arr(policy, "excludedProviders"), &c.provider))
        .collect();
    if filtered.is_empty() {
        return Err(access_denied(format!("Key is not allowed to access any provider configured for model '{model}'")));
    }
    Ok(filtered)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cand(p: &str) -> RouteResult {
        RouteResult { provider: p.into(), model: "m".into(), config: json!({}), model_config: None, incoming_model_alias: None, canonical_model: None }
    }

    fn req(model: &str, policy: Value) -> Value { json!({"model": model, "metadata": {"plexus_metadata": {"plexus_key_policy": policy}}}) }

    #[test]
    fn no_or_global_policy_passes_through() {
        assert_eq!(apply_key_access_policy(&json!({"model": "a"}), vec![cand("p")], "chat").unwrap().len(), 1);
        assert_eq!(apply_key_access_policy(&req("a", json!({"allowedModels": []})), vec![cand("p")], "chat").unwrap().len(), 1);
    }

    #[test]
    fn model_denied() {
        let err = apply_key_access_policy(&req("a", json!({"allowedModels": ["b"]})), vec![cand("p")], "chat").unwrap_err();
        assert_eq!(err.message, "Key is not allowed to access model 'a' for chat");
        assert_eq!(err.status, Some(403));
        let err = apply_key_access_policy(&req("b", json!({"allowedModels": ["b"], "excludedModels": ["b"]})), vec![cand("p")], "messages").unwrap_err();
        assert_eq!(err.message, "Key is not allowed to access model 'b' for messages");
    }

    #[test]
    fn tier_names_are_checked() {
        let mut r = req("luna@fast", json!({"allowedModels": ["luna@priority"]}));
        r["baseModel"] = json!("luna");
        r["serviceTier"] = json!("fast");
        assert!(apply_key_access_policy(&r, vec![cand("p")], "chat").is_ok());
        let mut r = req("luna@flex", json!({"allowedModels": ["luna"]}));
        r["baseModel"] = json!("luna");
        r["serviceTier"] = json!("flex");
        assert!(apply_key_access_policy(&r, vec![cand("p")], "chat").is_ok());
        let mut r = req("luna@FAST", json!({"excludedModels": ["luna@priority"]}));
        r["baseModel"] = json!("luna");
        r["serviceTier"] = json!("fast");
        assert!(apply_key_access_policy(&r, vec![cand("p")], "chat").is_err());
    }

    #[test]
    fn providers_filtered() {
        let out = apply_key_access_policy(&req("a", json!({"allowedProviders": ["p2"]})), vec![cand("p1"), cand("p2")], "chat").unwrap();
        assert_eq!(out.iter().map(|c| c.provider.as_str()).collect::<Vec<_>>(), vec!["p2"]);
        let err = apply_key_access_policy(&req("a", json!({"excludedProviders": ["p1"]})), vec![cand("p1")], "chat").unwrap_err();
        assert_eq!(err.message, "Key is not allowed to access any provider configured for model 'a'");
    }
}
