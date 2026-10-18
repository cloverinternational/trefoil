//! Ported contract: `/v0/management/user-quotas/*` CRUD from Plexus
//! (`packages/backend/src/routes/management/user-quotas.ts`), mounted under
//! the admin-only scope in `packages/backend/src/routes/management.ts`.
//!
//! Uses the error envelope this route file uses on its own
//! (`{"error":{"message","type"}}`, no `code`/status-number field) —
//! distinct from both other error shapes already ported in `keys.rs`
//! (`{"error":"<string>"}`) and `auth.rs`
//! (`{"error":{"message","type","code"}}`).

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json, Router,
};
use serde_json::Value;

use crate::auth::{resolve_principal, Principal};
use crate::state::{AppState, QuotaDefinition};

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/v0/management/user-quotas",
            axum::routing::get(list_quotas),
        )
        .route(
            "/v0/management/user-quotas/:name",
            axum::routing::get(get_quota)
                .put(put_quota)
                .patch(patch_quota)
                .delete(delete_quota),
        )
}

fn error_body(message: impl Into<String>, error_type: &str) -> Value {
    serde_json::json!({ "error": { "message": message.into(), "type": error_type } })
}

fn require_admin(state: &AppState, headers: &HeaderMap) -> Result<(), Response> {
    let provided_key = headers.get("x-admin-key").and_then(|v| v.to_str().ok());
    match resolve_principal(state, provided_key) {
        Some(Principal::Admin) => Ok(()),
        Some(Principal::Limited { .. }) => Err((
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": { "message": "Admin privileges required", "type": "forbidden", "code": 403 }
            })),
        )
            .into_response()),
        None => Err((
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "error": { "message": "Unauthorized", "type": "auth_error", "code": 401 }
            })),
        )
            .into_response()),
    }
}

/// Mirrors the PUT route's slug check: `/^[a-z0-9][a-z0-9-_]{1,62}$/`.
fn is_valid_quota_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    let rest: Vec<char> = chars.collect();
    if rest.len() < 1 || rest.len() > 62 {
        return false;
    }
    rest.iter()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-' || *c == '_')
}

/// Validates a merged JSON value against the load-bearing subset of
/// `QuotaDefinitionSchema` and builds a `QuotaDefinition`.
/// Pinned Plexus `QuotaDefinitionSchema` (`config.ts`).
fn quota_schema() -> &'static crate::zod::Schema {
    use crate::zod::*;
    static SCHEMA: std::sync::OnceLock<Schema> = std::sync::OnceLock::new();
    SCHEMA.get_or_init(|| {
        let variant = |kind: &'static str, rolling: bool| {
            let mut shape = vec![
                ("type", literal(kind)),
                ("limitType", enumeration(&["requests", "tokens", "cost"])),
                ("limit", number().min(1.0)),
            ];
            if rolling { shape.push(("duration", string().min(1.0))); }
            shape.extend([
                ("allowedProviders", array(string()).optional()),
                ("excludedProviders", array(string()).optional()),
                ("allowedModels", array(string()).optional()),
                ("excludedModels", array(string()).optional()),
                ("shared", boolean().optional()),
                ("warnAt", number().gt(0.0).lt(1.0).optional()),
            ]);
            object(shape)
        };
        discriminated("type", vec![variant("rolling", true), variant("daily", false), variant("weekly", false), variant("monthly", false)])
    })
}

/// Returns the zod output (echoed by PUT/PATCH) and the definition as the
/// quota repository stores and re-reads it (`saveUserQuota`/`getAllUserQuotas`).
fn validate_quota(value: &Value) -> Result<(Value, QuotaDefinition), Vec<Value>> {
    let parsed = quota_schema().safe_parse(value)?;
    let list = |key: &str| -> Option<Vec<String>> {
        let items: Vec<String> = parsed.get(key)?.as_array()?.iter().filter_map(Value::as_str).map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()).collect();
        (!items.is_empty()).then_some(items)
    };
    let stored = QuotaDefinition {
        quota_type: parsed["type"].as_str().unwrap_or_default().to_owned(),
        limit_type: parsed["limitType"].as_str().unwrap_or_default().to_owned(),
        limit: parsed["limit"].as_f64().unwrap_or_default(),
        duration: parsed.get("duration").and_then(Value::as_str).filter(|d| !d.is_empty()).map(str::to_owned),
        allowed_providers: list("allowedProviders"),
        excluded_providers: list("excludedProviders"),
        allowed_models: list("allowedModels"),
        excluded_models: list("excludedModels"),
        shared: (parsed.get("shared") == Some(&Value::Bool(true))).then_some(true),
        warn_at: parsed.get("warnAt").and_then(Value::as_f64),
    };
    Ok((parsed, stored))
}

async fn list_quotas(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(resp) = require_admin(&state, &headers) {
        return resp;
    }
    let quotas = state.inner.user_quotas.read().unwrap();
    Json(quotas.clone()).into_response()
}

async fn get_quota(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    if let Err(resp) = require_admin(&state, &headers) {
        return resp;
    }
    let quotas = state.inner.user_quotas.read().unwrap();
    match quotas.get(&name) {
        Some(quota) => {
            let mut value = serde_json::to_value(quota).unwrap();
            value["name"] = Value::String(name);
            Json(value).into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(error_body(
                format!("Quota not found: {name}"),
                "not_found_error",
            )),
        )
            .into_response(),
    }
}

async fn put_quota(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Json(raw): Json<Value>,
) -> Response {
    if let Err(resp) = require_admin(&state, &headers) {
        return resp;
    }
    if !is_valid_quota_name(&name) {
        return (
            StatusCode::BAD_REQUEST,
            Json(error_body(
                "Invalid quota name. Must be a slug (lowercase letters, numbers, hyphens, underscores, 2-63 characters)",
                "invalid_request_error",
            )),
        )
            .into_response();
    }

    let (response_value, quota) = match validate_quota(&raw) {
        Ok(q) => q,
        Err(details) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": { "message": "Validation failed", "type": "invalid_request_error" },
                    "details": details,
                })),
            )
                .into_response();
        }
    };

    if let Some(pool) = &state.inner.database {
        if let Err(error) = crate::database::upsert_user_quota(pool, &name, &quota).await {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(error_body(error.to_string(), "internal_error")),
            )
                .into_response();
        }
    }
    state
        .inner
        .user_quotas
        .write()
        .unwrap()
        .insert(name.clone(), quota);
    Json(serde_json::json!({ "success": true, "name": name, "quota": response_value }))
        .into_response()
}

async fn patch_quota(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Json(raw): Json<Value>,
) -> Response {
    if let Err(resp) = require_admin(&state, &headers) {
        return resp;
    }

    let Some(existing) = state.inner.user_quotas.read().unwrap().get(&name).cloned() else {
        return (
            StatusCode::NOT_FOUND,
            Json(error_body(
                format!("Quota not found: {name}"),
                "not_found_error",
            )),
        )
            .into_response();
    };

    let mut merged = serde_json::to_value(&existing).unwrap();
    if let (Value::Object(merged_obj), Value::Object(updates)) = (&mut merged, &raw) {
        for (k, v) in updates {
            merged_obj.insert(k.clone(), v.clone());
        }
    }

    let (response_value, quota) = match validate_quota(&merged) {
        Ok(q) => q,
        Err(details) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": { "message": "Validation failed", "type": "invalid_request_error" },
                    "details": details,
                })),
            )
                .into_response();
        }
    };

    if let Some(pool) = &state.inner.database {
        if let Err(error) = crate::database::upsert_user_quota(pool, &name, &quota).await {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(error_body(error.to_string(), "internal_error")),
            )
                .into_response();
        }
    }
    state
        .inner
        .user_quotas
        .write()
        .unwrap()
        .insert(name.clone(), quota);
    Json(serde_json::json!({ "success": true, "name": name, "quota": response_value }))
        .into_response()
}

async fn delete_quota(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    if let Err(resp) = require_admin(&state, &headers) {
        return resp;
    }

    if !state.inner.user_quotas.read().unwrap().contains_key(&name) {
        return (
            StatusCode::NOT_FOUND,
            Json(error_body(
                format!("Quota not found: {name}"),
                "not_found_error",
            )),
        )
            .into_response();
    }

    let keys_using_quota: Vec<String> = {
        let keys = state.inner.keys.read().unwrap();
        keys.iter()
            .filter(|(_, config)| config.quota_names.iter().any(|q| q == &name))
            .map(|(key_name, _)| key_name.clone())
            .collect()
    };

    let used_as_default = {
        let settings = state.inner.settings.read().unwrap();
        settings
            .get("default_quotas")
            .and_then(|v| v.as_array())
            .is_some_and(|arr| arr.iter().any(|v| v.as_str() == Some(name.as_str())))
    };

    if !keys_using_quota.is_empty() || used_as_default {
        let mut reasons = Vec::new();
        if !keys_using_quota.is_empty() {
            reasons.push(format!(
                "assigned to the following keys: {}",
                keys_using_quota.join(", ")
            ));
        }
        if used_as_default {
            reasons.push("referenced by the 'default_quotas' system setting".to_string());
        }
        return (
            StatusCode::CONFLICT,
            Json(error_body(
                format!(
                    "Cannot delete quota '{name}'. It is {}.",
                    reasons.join(" and ")
                ),
                "conflict_error",
            )),
        )
            .into_response();
    }

    if let Some(pool) = &state.inner.database {
        if let Err(error) = crate::database::delete_user_quota(pool, &name).await {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(error_body(error.to_string(), "internal_error")),
            )
                .into_response();
        }
    }
    state.inner.user_quotas.write().unwrap().remove(&name);
    Json(serde_json::json!({
        "success": true,
        "name": name,
        "message": format!("Quota '{name}' deleted successfully"),
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
        AppState::new(Some("correct-admin-key".to_string()), HashMap::new())
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
    async fn put_rejects_invalid_name() {
        let (status, body) = call(
            admin_state(),
            "PUT",
            "/v0/management/user-quotas/A",
            Some("correct-admin-key"),
            Some(serde_json::json!({ "type": "daily", "limitType": "requests", "limit": 100 })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("Invalid quota name"));
    }

    #[tokio::test]
    async fn put_creates_and_get_returns_it() {
        let state = admin_state();
        let (status, body) = call(
            state.clone(),
            "PUT",
            "/v0/management/user-quotas/my-quota",
            Some("correct-admin-key"),
            Some(serde_json::json!({ "type": "daily", "limitType": "requests", "limit": 100 })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["success"], true);

        let (status, body) = call(
            state,
            "GET",
            "/v0/management/user-quotas/my-quota",
            Some("correct-admin-key"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["name"], "my-quota");
        assert_eq!(body["type"], "daily");
        assert_eq!(body["limit"], 100.0);
    }

    #[tokio::test]
    async fn put_rolling_requires_duration() {
        let (status, body) = call(
            admin_state(),
            "PUT",
            "/v0/management/user-quotas/rolling-q",
            Some("correct-admin-key"),
            Some(serde_json::json!({ "type": "rolling", "limitType": "tokens", "limit": 50 })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["details"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["path"] == serde_json::json!(["duration"])));
    }

    #[tokio::test]
    async fn patch_merges_partial_update() {
        let state = admin_state();
        call(
            state.clone(),
            "PUT",
            "/v0/management/user-quotas/my-quota",
            Some("correct-admin-key"),
            Some(serde_json::json!({ "type": "daily", "limitType": "requests", "limit": 100 })),
        )
        .await;

        let (status, body) = call(
            state,
            "PATCH",
            "/v0/management/user-quotas/my-quota",
            Some("correct-admin-key"),
            Some(serde_json::json!({ "limit": 200 })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["quota"]["limit"], 200.0);
    }

    #[tokio::test]
    async fn patch_nonexistent_returns_404() {
        let (status, _) = call(
            admin_state(),
            "PATCH",
            "/v0/management/user-quotas/nope",
            Some("correct-admin-key"),
            Some(serde_json::json!({ "limit": 5 })),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn delete_404s_when_quota_does_not_exist() {
        let (status, _) = call(
            admin_state(),
            "DELETE",
            "/v0/management/user-quotas/does-not-exist",
            Some("correct-admin-key"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn delete_409s_when_key_references_quota() {
        let mut keys = HashMap::new();
        keys.insert(
            "some-key".to_string(),
            crate::state::KeyConfig {
                secret: "s".to_string(),
                quota_names: vec!["my-quota".to_string()],
                ..Default::default()
            },
        );
        let state = AppState::new(Some("correct-admin-key".to_string()), keys);
        call(
            state.clone(),
            "PUT",
            "/v0/management/user-quotas/my-quota",
            Some("correct-admin-key"),
            Some(serde_json::json!({ "type": "daily", "limitType": "requests", "limit": 100 })),
        )
        .await;

        let (status, _) = call(
            state,
            "DELETE",
            "/v0/management/user-quotas/my-quota",
            Some("correct-admin-key"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn delete_409s_when_default_quotas_references_it() {
        let state = admin_state();
        call(
            state.clone(),
            "PUT",
            "/v0/management/user-quotas/my-quota",
            Some("correct-admin-key"),
            Some(serde_json::json!({ "type": "daily", "limitType": "requests", "limit": 100 })),
        )
        .await;
        state.inner.settings.write().unwrap().insert(
            "default_quotas".to_string(),
            serde_json::json!(["my-quota"]),
        );

        let (status, _) = call(
            state,
            "DELETE",
            "/v0/management/user-quotas/my-quota",
            Some("correct-admin-key"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn delete_succeeds_when_unreferenced() {
        let state = admin_state();
        call(
            state.clone(),
            "PUT",
            "/v0/management/user-quotas/my-quota",
            Some("correct-admin-key"),
            Some(serde_json::json!({ "type": "daily", "limitType": "requests", "limit": 100 })),
        )
        .await;

        let (status, body) = call(
            state,
            "DELETE",
            "/v0/management/user-quotas/my-quota",
            Some("correct-admin-key"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["success"], true);
    }

    #[tokio::test]
    async fn delete_does_not_409_on_key_with_no_quotas() {
        let mut keys = HashMap::new();
        keys.insert(
            "no-quotas-key".to_string(),
            crate::state::KeyConfig {
                secret: "s".to_string(),
                ..Default::default()
            },
        );
        let state = AppState::new(Some("correct-admin-key".to_string()), keys);
        call(
            state.clone(),
            "PUT",
            "/v0/management/user-quotas/my-quota",
            Some("correct-admin-key"),
            Some(serde_json::json!({ "type": "daily", "limitType": "requests", "limit": 100 })),
        )
        .await;

        let (status, _) = call(
            state,
            "DELETE",
            "/v0/management/user-quotas/my-quota",
            Some("correct-admin-key"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn limited_key_gets_403() {
        let mut keys = HashMap::new();
        keys.insert(
            "limited".to_string(),
            crate::state::KeyConfig {
                secret: "limited-secret".to_string(),
                ..Default::default()
            },
        );
        let state = AppState::new(Some("correct-admin-key".to_string()), keys);
        let (status, _) = call(
            state,
            "GET",
            "/v0/management/user-quotas",
            Some("limited-secret"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn missing_credential_gets_401() {
        let (status, _) = call(
            admin_state(),
            "GET",
            "/v0/management/user-quotas",
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
}
