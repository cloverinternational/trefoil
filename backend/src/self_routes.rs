//! Ported contract: `/v0/management/self/*` from Plexus
//! (`packages/backend/src/routes/management/self.ts`), mounted in the
//! *scoped* (authenticated, not admin-only) block — both admin and limited
//! principals may call these, with admin required to pass `?keyName=` to
//! pick a target since admins have no key of their own.
//!
//! Module named `self_routes` (not `self`) since `self` is a reserved
//! keyword.
//!
//! Not ported: the quota-status numbers in `GET /self/quota` beyond the
//! "no quota assigned" shape — Trefoil has no `QuotaEnforcer` (no live
//! usage tracking yet), so `currentUsage`/`limit`/`remaining`/`resetsAt` are
//! always the "no data" zero-value shape Plexus itself falls back to when
//! its `quotaEnforcer` is unset or returns no result. This mirrors real
//! Plexus behavior for a deployment with usage tracking not (yet) wired in
//! rather than diverging from the contract.

use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json, Router,
};
use rand::RngCore;
use serde::Deserialize;
use serde_json::Value;

use crate::auth::{resolve_principal, Principal};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/v0/management/self/me", axum::routing::get(get_me))
        .route("/v0/management/self/rotate", axum::routing::post(rotate_secret))
        .route("/v0/management/self/comment", axum::routing::patch(update_comment))
        .route(
            "/v0/management/self/debug/toggle",
            axum::routing::post(toggle_debug),
        )
        .route("/v0/management/self/quota", axum::routing::get(get_quota))
}

fn auth_error_401() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({
            "error": { "message": "Unauthorized", "type": "auth_error", "code": 401 }
        })),
    )
        .into_response()
}

fn generate_secret() -> String {
    let mut bytes = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut bytes);
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("sk-{hex}")
}

#[derive(Deserialize)]
struct KeyNameQuery {
    #[serde(rename = "keyName")]
    key_name: Option<String>,
}

/// Resolves the key name a self-service action should operate on. Limited
/// principals always operate on their own key; admins must supply
/// `?keyName=`.
fn resolve_target(principal: &Principal, key_name_query: Option<String>) -> Result<String, Response> {
    match principal {
        Principal::Limited { key_name, .. } => Ok(key_name.clone()),
        Principal::Admin => key_name_query.ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "Admin must supply ?keyName= for self-service endpoints"
                })),
            )
                .into_response()
        }),
    }
}

async fn get_me(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let provided_key = headers.get("x-admin-key").and_then(|v| v.to_str().ok());
    let Some(principal) = resolve_principal(&state, provided_key) else {
        return auth_error_401();
    };

    match principal {
        Principal::Admin => Json(serde_json::json!({ "role": "admin" })).into_response(),
        Principal::Limited { key_name, config } => {
            let global = *state.inner.debug_enabled_global.read().unwrap();
            let debug_enabled =
                global || state.inner.debug_enabled_keys.read().unwrap().contains(&key_name);
            Json(serde_json::json!({
                "role": "limited",
                "keyName": key_name,
                "allowedProviders": config.allowed_providers,
                "allowedModels": config.allowed_models,
                "excludedProviders": config.excluded_providers,
                "excludedModels": config.excluded_models,
                "allowRawPassthrough": config.allow_raw_passthrough,
                "quotaNames": config.quota_names,
                "quotaName": config.quota_names.first(),
                "comment": config.comment,
                "traceEnabled": debug_enabled,
                "traceEnabledGlobal": global,
            }))
            .into_response()
        }
    }
}

async fn rotate_secret(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<KeyNameQuery>,
) -> Response {
    let provided_key = headers.get("x-admin-key").and_then(|v| v.to_str().ok());
    let Some(principal) = resolve_principal(&state, provided_key) else {
        return auth_error_401();
    };
    let key_name = match resolve_target(&principal, query.key_name) {
        Ok(name) => name,
        Err(resp) => return resp,
    };

    let mut keys = state.inner.keys.write().unwrap();
    let Some(existing) = keys.get_mut(&key_name) else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("Key '{key_name}' not found") })),
        )
            .into_response();
    };

    let new_secret = generate_secret();
    existing.secret = new_secret.clone();

    Json(serde_json::json!({
        "keyName": key_name,
        "secret": new_secret,
        "message": "Secret rotated. Store it now — it will not be shown again.",
    }))
    .into_response()
}

async fn update_comment(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<KeyNameQuery>,
    body: Option<Json<Value>>,
) -> Response {
    let provided_key = headers.get("x-admin-key").and_then(|v| v.to_str().ok());
    let Some(principal) = resolve_principal(&state, provided_key) else {
        return auth_error_401();
    };

    let raw = body.map(|Json(v)| v).unwrap_or(Value::Null);
    let comment_value = match &raw {
        Value::Object(obj) => obj.get("comment").cloned(),
        _ => None,
    };
    if !raw.is_object() && !raw.is_null() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "Invalid body" })),
        )
            .into_response();
    }
    if let Some(v) = &comment_value {
        if !v.is_null() && !v.is_string() {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "Invalid body",
                    "details": [{
                        "code": "invalid_type",
                        "expected": "string",
                        "path": ["comment"],
                        "message": format!("Invalid input: expected string, received {}", json_type_name(v)),
                    }],
                })),
            )
                .into_response();
        }
    }

    let key_name = match resolve_target(&principal, query.key_name) {
        Ok(name) => name,
        Err(resp) => return resp,
    };

    let mut keys = state.inner.keys.write().unwrap();
    let Some(existing) = keys.get_mut(&key_name) else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("Key '{key_name}' not found") })),
        )
            .into_response();
    };

    let comment_str = comment_value.as_ref().and_then(|v| v.as_str().map(String::from));
    existing.comment = comment_str.clone();

    // Mirror Plexus exactly: when the request body omits `comment`
    // entirely, the response also omits it (its JSON.stringify drops the
    // `undefined` field) even though the stored comment is still cleared.
    // Only an explicit `comment` (string or null) in the request causes
    // the field to appear in the response.
    let mut response = serde_json::json!({
        "success": true,
        "keyName": key_name,
    });
    if comment_value.is_some() {
        response["comment"] = match comment_str {
            Some(s) => Value::String(s),
            None => Value::Null,
        };
    }
    Json(response)
    .into_response()
}

fn json_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[derive(Deserialize)]
struct ToggleBody {
    enabled: Value,
}

async fn toggle_debug(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<KeyNameQuery>,
    body: Option<Json<Value>>,
) -> Response {
    let provided_key = headers.get("x-admin-key").and_then(|v| v.to_str().ok());
    let Some(principal) = resolve_principal(&state, provided_key) else {
        return auth_error_401();
    };

    let raw = body.map(|Json(v)| v).unwrap_or(Value::Null);
    let parsed: Result<ToggleBody, _> = serde_json::from_value(raw.clone());
    let enabled = match &parsed {
        Ok(b) => match b.enabled.as_bool() {
            Some(v) => v,
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": "Invalid body",
                        "details": [{
                            "code": "invalid_type",
                            "expected": "boolean",
                            "path": ["enabled"],
                            "message": format!("Invalid input: expected boolean, received {}", json_type_name(&b.enabled)),
                        }],
                    })),
                )
                    .into_response();
            }
        },
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "Invalid body" })),
            )
                .into_response();
        }
    };

    let key_name = match resolve_target(&principal, query.key_name) {
        Ok(name) => name,
        Err(resp) => return resp,
    };

    // Note: mirrors the reference exactly — no existence check on the
    // target key for this endpoint (unlike rotate/comment), so toggling
    // trace for a nonexistent ?keyName= still returns 200.
    {
        let mut debug_keys = state.inner.debug_enabled_keys.write().unwrap();
        if enabled {
            debug_keys.insert(key_name.clone());
        } else {
            debug_keys.remove(&key_name);
        }
    }

    let global = *state.inner.debug_enabled_global.read().unwrap();
    let now_enabled = global || state.inner.debug_enabled_keys.read().unwrap().contains(&key_name);
    Json(serde_json::json!({
        "keyName": key_name,
        "enabled": now_enabled,
        "enabledGlobal": global,
    }))
    .into_response()
}

async fn get_quota(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<KeyNameQuery>,
) -> Response {
    let provided_key = headers.get("x-admin-key").and_then(|v| v.to_str().ok());
    let Some(principal) = resolve_principal(&state, provided_key) else {
        return auth_error_401();
    };
    let key_name = match resolve_target(&principal, query.key_name) {
        Ok(name) => name,
        Err(resp) => return resp,
    };

    let keys = state.inner.keys.read().unwrap();
    let Some(key_config) = keys.get(&key_name) else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("Key '{key_name}' not found") })),
        )
            .into_response();
    };

    let assigned_names: Vec<String> = if !key_config.quota_names.is_empty() {
        key_config.quota_names.clone()
    } else {
        state
            .inner
            .settings
            .read()
            .unwrap()
            .get("default_quotas")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default()
    };

    // No live QuotaEnforcer: always report the "no data" shape, labeling
    // the card with the first assigned name when present. Matches Plexus's
    // own fallback when quotaEnforcer is unset.
    Json(serde_json::json!({
        "key": key_name,
        "quotas": Value::Array(vec![]),
        "quotaName": assigned_names.first(),
        "allowed": true,
        "currentUsage": 0,
        "limit": Value::Null,
        "remaining": Value::Null,
        "resetsAt": Value::Null,
        "limitType": Value::Null,
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use std::collections::HashMap;
    use tower::ServiceExt;

    fn admin_state() -> AppState {
        let mut keys = HashMap::new();
        keys.insert(
            "test-key".to_string(),
            crate::state::KeyConfig { secret: "sk-test-secret".to_string(), ..Default::default() },
        );
        AppState::new(Some("correct-admin-key".to_string()), keys)
    }

    async fn call(
        state: AppState,
        method: &str,
        uri: &str,
        admin_header: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut req = Request::builder().uri(uri).method(method);
        if let Some(h) = admin_header {
            req = req.header("x-admin-key", h);
        }
        let body = match body {
            Some(v) => {
                req = req.header("content-type", "application/json");
                Body::from(serde_json::to_vec(&v).unwrap())
            }
            None => Body::empty(),
        };
        let response = router()
            .with_state(state)
            .oneshot(req.body(body).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        (status, json)
    }

    #[tokio::test]
    async fn me_returns_admin_role() {
        let (status, body) = call(admin_state(), "GET", "/v0/management/self/me", Some("correct-admin-key"), None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, serde_json::json!({ "role": "admin" }));
    }

    #[tokio::test]
    async fn me_returns_limited_metadata() {
        let (status, body) = call(admin_state(), "GET", "/v0/management/self/me", Some("sk-test-secret"), None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["role"], "limited");
        assert_eq!(body["keyName"], "test-key");
        assert_eq!(body["traceEnabled"], false);
    }

    #[tokio::test]
    async fn me_missing_credential_gets_401() {
        let (status, _) = call(admin_state(), "GET", "/v0/management/self/me", None, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn rotate_as_limited_replaces_own_secret() {
        let state = admin_state();
        let (status, body) = call(state.clone(), "POST", "/v0/management/self/rotate", Some("sk-test-secret"), None).await;
        assert_eq!(status, StatusCode::OK);
        let new_secret = body["secret"].as_str().unwrap().to_string();
        assert_ne!(new_secret, "sk-test-secret");
        assert_eq!(state.inner.keys.read().unwrap()["test-key"].secret, new_secret);
    }

    #[tokio::test]
    async fn rotate_as_admin_requires_key_name() {
        let (status, body) = call(admin_state(), "POST", "/v0/management/self/rotate", Some("correct-admin-key"), None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"].as_str().unwrap().contains("keyName"));
    }

    #[tokio::test]
    async fn rotate_nonexistent_key_returns_404() {
        let (status, _) = call(
            admin_state(),
            "POST",
            "/v0/management/self/rotate?keyName=ghost",
            Some("correct-admin-key"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn comment_updates_and_clears() {
        let state = admin_state();
        let (status, body) = call(
            state.clone(),
            "PATCH",
            "/v0/management/self/comment",
            Some("sk-test-secret"),
            Some(serde_json::json!({ "comment": "hi" })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["comment"], "hi");
        assert_eq!(state.inner.keys.read().unwrap()["test-key"].comment, Some("hi".to_string()));

        let (status, body) = call(
            state.clone(),
            "PATCH",
            "/v0/management/self/comment",
            Some("sk-test-secret"),
            Some(serde_json::json!({})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["comment"], Value::Null);
        assert!(!body.as_object().unwrap().contains_key("comment"));
        assert_eq!(state.inner.keys.read().unwrap()["test-key"].comment, None);
    }

    #[tokio::test]
    async fn comment_explicit_null_clears_and_is_present_in_response() {
        let state = admin_state();
        let (status, body) = call(
            state.clone(),
            "PATCH",
            "/v0/management/self/comment",
            Some("sk-test-secret"),
            Some(serde_json::json!({ "comment": Value::Null })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.as_object().unwrap().contains_key("comment"));
        assert_eq!(body["comment"], Value::Null);
        assert_eq!(state.inner.keys.read().unwrap()["test-key"].comment, None);
    }

    #[tokio::test]
    async fn comment_rejects_non_string() {
        let (status, body) = call(
            admin_state(),
            "PATCH",
            "/v0/management/self/comment",
            Some("sk-test-secret"),
            Some(serde_json::json!({ "comment": 123 })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "Invalid body");
    }

    #[tokio::test]
    async fn debug_toggle_enables_and_disables() {
        let state = admin_state();
        let (status, body) = call(
            state.clone(),
            "POST",
            "/v0/management/self/debug/toggle",
            Some("sk-test-secret"),
            Some(serde_json::json!({ "enabled": true })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["enabled"], true);
        assert!(state.inner.debug_enabled_keys.read().unwrap().contains("test-key"));

        let (status, body) = call(
            state.clone(),
            "POST",
            "/v0/management/self/debug/toggle",
            Some("sk-test-secret"),
            Some(serde_json::json!({ "enabled": false })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["enabled"], false);
        assert!(!state.inner.debug_enabled_keys.read().unwrap().contains("test-key"));
    }

    #[tokio::test]
    async fn debug_toggle_rejects_non_boolean() {
        let (status, _) = call(
            admin_state(),
            "POST",
            "/v0/management/self/debug/toggle",
            Some("sk-test-secret"),
            Some(serde_json::json!({ "enabled": "yes" })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn debug_toggle_allows_nonexistent_key_name() {
        let (status, body) = call(
            admin_state(),
            "POST",
            "/v0/management/self/debug/toggle?keyName=ghost",
            Some("correct-admin-key"),
            Some(serde_json::json!({ "enabled": true })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["keyName"], "ghost");
    }

    #[tokio::test]
    async fn quota_returns_no_quota_assigned_shape() {
        let (status, body) = call(admin_state(), "GET", "/v0/management/self/quota", Some("sk-test-secret"), None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["key"], "test-key");
        assert_eq!(body["quotaName"], Value::Null);
        assert_eq!(body["allowed"], true);
    }

    #[tokio::test]
    async fn quota_reports_first_assigned_quota_name() {
        let mut keys = HashMap::new();
        keys.insert(
            "test-key".to_string(),
            crate::state::KeyConfig {
                secret: "sk-test-secret".to_string(),
                quota_names: vec!["my-quota".to_string()],
                ..Default::default()
            },
        );
        let state = AppState::new(Some("correct-admin-key".to_string()), keys);
        let (_, body) = call(state, "GET", "/v0/management/self/quota", Some("sk-test-secret"), None).await;
        assert_eq!(body["quotaName"], "my-quota");
    }

    #[tokio::test]
    async fn quota_falls_back_to_default_quotas_when_key_has_none() {
        let state = admin_state();
        state
            .inner
            .settings
            .write()
            .unwrap()
            .insert("default_quotas".to_string(), serde_json::json!(["fallback-q"]));
        let (_, body) = call(state, "GET", "/v0/management/self/quota", Some("sk-test-secret"), None).await;
        assert_eq!(body["quotaName"], "fallback-q");
    }

    #[tokio::test]
    async fn quota_as_admin_requires_key_name() {
        let (status, _) = call(admin_state(), "GET", "/v0/management/self/quota", Some("correct-admin-key"), None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
}
