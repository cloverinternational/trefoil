//! Ported contract: `/v0/management/keys/:name` CRUD from Plexus
//! (`packages/backend/src/routes/management/config.ts`, the "API Keys"
//! section, mounted under the admin-only scope in
//! `packages/backend/src/routes/management.ts`).
//!
//! All routes here require the `Admin` principal (same
//! `authenticate` + `requireAdmin` preHandler chain as the reference);
//! a limited key gets 403, no/invalid credential gets 401.

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::auth::{resolve_principal, Principal};
use crate::state::{AppState, KeyConfig};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/v0/management/keys", get(list_keys))
        .route(
            "/v0/management/keys/:name",
            get(get_key).put(put_key).patch(patch_key).delete(delete_key),
        )
        .route("/v0/management/keys/:name/disable", post(disable_key))
}

/// Error responses used by these routes. Two distinct shapes are ported
/// from the reference:
/// - Auth failures (401/403) use the shared management error envelope
///   `{"error": {"message","type","code"}}` (`ManagementAuthError`).
/// - Endpoint-specific failures (400/404/500) use a bare
///   `{"error": "<string>"}`, and validation failures additionally carry
///   a `details` array (`KeyConfigSchema.safeParse` issues).
enum ApiError {
    Auth(StatusCode, &'static str, &'static str),
    Simple(StatusCode, String),
    ValidationFailed,
}

impl ApiError {
    fn unauthorized() -> Self {
        Self::Auth(StatusCode::UNAUTHORIZED, "Unauthorized", "auth_error")
    }
    fn forbidden() -> Self {
        Self::Auth(
            StatusCode::FORBIDDEN,
            "Admin privileges required",
            "forbidden",
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self {
            ApiError::Auth(status, message, error_type) => {
                let body = serde_json::json!({
                    "error": { "message": message, "type": error_type, "code": status.as_u16() }
                });
                (status, Json(body)).into_response()
            }
            ApiError::Simple(status, message) => {
                let body = serde_json::json!({ "error": message });
                (status, Json(body)).into_response()
            }
            ApiError::ValidationFailed => {
                let body = serde_json::json!({ "error": "Validation failed", "details": [] });
                (StatusCode::BAD_REQUEST, Json(body)).into_response()
            }
        }
    }
}

fn require_admin(state: &AppState, headers: &HeaderMap) -> Result<(), ApiError> {
    let provided_key = headers.get("x-admin-key").and_then(|v| v.to_str().ok());
    match resolve_principal(state, provided_key) {
        Some(Principal::Admin) => Ok(()),
        Some(Principal::Limited { .. }) => Err(ApiError::forbidden()),
        None => Err(ApiError::unauthorized()),
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// Wire shape accepted by PUT/PATCH bodies. Mirrors `KeyConfigSchema` in
/// `packages/backend/src/config.ts`. All fields optional at the wire level;
/// `secret` is required by PUT (full create-or-replace) but not required by
/// PATCH (partial update against an existing key).
#[derive(Debug, Deserialize, Default)]
struct KeyBody {
    secret: Option<String>,
    comment: Option<String>,
    #[serde(rename = "expiresInMinutes")]
    expires_in_minutes: Option<i64>,
    /// Deprecated single-quota field, folded into `quotas` by
    /// `normalize_quota_field` below (mirrors `normalizeKeyConfig`).
    quota: Option<String>,
    quotas: Option<Vec<String>>,
    #[serde(rename = "allowedModels")]
    allowed_models: Option<Vec<String>>,
    #[serde(rename = "allowedProviders")]
    allowed_providers: Option<Vec<String>>,
    #[serde(rename = "excludedModels")]
    excluded_models: Option<Vec<String>>,
    #[serde(rename = "excludedProviders")]
    excluded_providers: Option<Vec<String>>,
    #[serde(rename = "allowRawPassthrough")]
    allow_raw_passthrough: Option<bool>,
    #[serde(rename = "allowedIps")]
    allowed_ips: Option<Vec<String>>,
}

/// Mirrors `normalizeKeyConfig` in `packages/backend/src/config.ts`:
/// an explicit `quotas` (even `[]`) always wins; `quota` is only used as a
/// fallback when `quotas` is entirely absent.
fn normalize_quota_field(body: &KeyBody) -> Option<Vec<String>> {
    if let Some(quotas) = &body.quotas {
        return Some(quotas.clone());
    }
    match &body.quota {
        Some(q) if !q.is_empty() => Some(vec![q.clone()]),
        _ => None,
    }
}

fn has_expiry_fields(value: &Value) -> bool {
    value.get("expiresInMinutes").is_some()
        || value.get("expiresAt").is_some()
        || value.get("disabledAt").is_some()
}

#[derive(Serialize)]
struct NamedKey<'a> {
    name: &'a str,
    #[serde(flatten)]
    config: &'a KeyConfig,
}

async fn list_keys(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(e) = require_admin(&state, &headers) {
        return e.into_response();
    }
    let keys = state.inner.keys.read().unwrap();
    Json(keys.clone()).into_response()
}

async fn get_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    if let Err(e) = require_admin(&state, &headers) {
        return e.into_response();
    }
    let keys = state.inner.keys.read().unwrap();
    match keys.get(&name) {
        Some(config) => Json(NamedKey {
            name: &name,
            config,
        })
        .into_response(),
        None => ApiError::Simple(
            StatusCode::NOT_FOUND,
            format!("API key '{name}' not found"),
        )
        .into_response(),
    }
}

async fn put_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Json(raw): Json<Value>,
) -> Response {
    if let Err(e) = require_admin(&state, &headers) {
        return e.into_response();
    }
    if name.contains('*') {
        return ApiError::Simple(
            StatusCode::BAD_REQUEST,
            "Key name cannot contain '*' (reserved for the shared-quota bucket)".to_string(),
        )
        .into_response();
    }
    if !raw.is_object() {
        return ApiError::Simple(StatusCode::BAD_REQUEST, "Object body is required".to_string())
            .into_response();
    }

    let mut keys = state.inner.keys.write().unwrap();
    let existing = keys.get(&name).cloned();

    if existing.is_some() && has_expiry_fields(&raw) {
        return ApiError::Simple(
            StatusCode::BAD_REQUEST,
            "Key expiry cannot be changed after creation".to_string(),
        )
        .into_response();
    }
    if raw.get("expiresAt").is_some() || raw.get("disabledAt").is_some() {
        return ApiError::Simple(
            StatusCode::BAD_REQUEST,
            "Expiry timestamps are managed by Plexus".to_string(),
        )
        .into_response();
    }

    let body: KeyBody = match serde_json::from_value(raw) {
        Ok(b) => b,
        Err(_) => return ApiError::ValidationFailed.into_response(),
    };
    let Some(secret) = body.secret.clone() else {
        return ApiError::ValidationFailed.into_response();
    };

    let quotas = normalize_quota_field(&body).unwrap_or_default();
    let expires_at = match &existing {
        Some(existing) => existing.expires_at,
        None => body
            .expires_in_minutes
            .map(|minutes| now_ms() + minutes * 60_000),
    };
    let disabled_at = existing.as_ref().and_then(|e| e.disabled_at);

    let config = KeyConfig {
        secret,
        comment: body.comment,
        quota_names: quotas,
        allowed_providers: body.allowed_providers.unwrap_or_default(),
        allowed_models: body.allowed_models.unwrap_or_default(),
        excluded_providers: body.excluded_providers.unwrap_or_default(),
        excluded_models: body.excluded_models.unwrap_or_default(),
        allow_raw_passthrough: body.allow_raw_passthrough.unwrap_or(false),
        allowed_ips: body.allowed_ips.unwrap_or_default(),
        expires_at,
        disabled_at,
    };
    keys.insert(name.clone(), config);

    Json(serde_json::json!({ "success": true, "name": name })).into_response()
}

async fn patch_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Json(raw): Json<Value>,
) -> Response {
    if let Err(e) = require_admin(&state, &headers) {
        return e.into_response();
    }
    if !raw.is_object() {
        return ApiError::Simple(StatusCode::BAD_REQUEST, "Object body is required".to_string())
            .into_response();
    }
    if has_expiry_fields(&raw) {
        return ApiError::Simple(
            StatusCode::BAD_REQUEST,
            "Key expiry cannot be changed after creation".to_string(),
        )
        .into_response();
    }

    let mut keys = state.inner.keys.write().unwrap();
    let Some(existing) = keys.get(&name).cloned() else {
        return ApiError::Simple(
            StatusCode::NOT_FOUND,
            format!("API key '{name}' not found"),
        )
        .into_response();
    };

    let body: KeyBody = match serde_json::from_value(raw) {
        Ok(b) => b,
        Err(_) => return ApiError::ValidationFailed.into_response(),
    };

    let quotas = normalize_quota_field(&body).unwrap_or(existing.quota_names);
    let merged = KeyConfig {
        secret: body.secret.unwrap_or(existing.secret),
        comment: body.comment.or(existing.comment),
        quota_names: quotas,
        allowed_providers: body.allowed_providers.unwrap_or(existing.allowed_providers),
        allowed_models: body.allowed_models.unwrap_or(existing.allowed_models),
        excluded_providers: body
            .excluded_providers
            .unwrap_or(existing.excluded_providers),
        excluded_models: body.excluded_models.unwrap_or(existing.excluded_models),
        allow_raw_passthrough: body
            .allow_raw_passthrough
            .unwrap_or(existing.allow_raw_passthrough),
        allowed_ips: body.allowed_ips.unwrap_or(existing.allowed_ips),
        expires_at: existing.expires_at,
        disabled_at: existing.disabled_at,
    };
    keys.insert(name.clone(), merged);

    Json(serde_json::json!({ "success": true, "name": name })).into_response()
}

async fn delete_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    if let Err(e) = require_admin(&state, &headers) {
        return e.into_response();
    }
    state.inner.keys.write().unwrap().remove(&name);
    Json(serde_json::json!({ "success": true })).into_response()
}

async fn disable_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    if let Err(e) = require_admin(&state, &headers) {
        return e.into_response();
    }
    let mut keys = state.inner.keys.write().unwrap();
    let Some(existing) = keys.get_mut(&name) else {
        return ApiError::Simple(
            StatusCode::NOT_FOUND,
            format!("API key '{name}' not found"),
        )
        .into_response();
    };
    if existing.expires_at.is_none() {
        return ApiError::Simple(
            StatusCode::BAD_REQUEST,
            "Only time-bound keys can be disabled".to_string(),
        )
        .into_response();
    }
    existing.disabled_at = Some(now_ms());

    Json(serde_json::json!({ "success": true, "name": name })).into_response()
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

    // Ported from config-keys-multi-quota.test.ts.

    #[tokio::test]
    async fn get_keys_returns_quotas_as_is() {
        let mut keys = HashMap::new();
        keys.insert(
            "compat-key".to_string(),
            KeyConfig {
                secret: "sk-compat".to_string(),
                quota_names: vec!["quota-a".to_string(), "quota-b".to_string()],
                ..Default::default()
            },
        );
        let state = AppState::new(Some("correct-admin-key".to_string()), keys);
        let (status, body) = call(
            state,
            "GET",
            "/v0/management/keys",
            Some("correct-admin-key"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["compat-key"]["quotas"],
            serde_json::json!(["quota-a", "quota-b"])
        );
        assert!(body["compat-key"].get("quota").is_none());
    }

    #[tokio::test]
    async fn get_key_by_name_returns_quotas_as_is() {
        let mut keys = HashMap::new();
        keys.insert(
            "compat-key-2".to_string(),
            KeyConfig {
                secret: "sk-compat-2".to_string(),
                quota_names: vec!["quota-x".to_string()],
                ..Default::default()
            },
        );
        let state = AppState::new(Some("correct-admin-key".to_string()), keys);
        let (status, body) = call(
            state,
            "GET",
            "/v0/management/keys/compat-key-2",
            Some("correct-admin-key"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["quotas"], serde_json::json!(["quota-x"]));
        assert!(body.get("quota").is_none());
    }

    #[tokio::test]
    async fn put_rejects_name_containing_star() {
        let (status, _) = call(
            admin_state(),
            "PUT",
            "/v0/management/keys/wild*card",
            Some("correct-admin-key"),
            Some(serde_json::json!({ "secret": "sk-test" })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn put_rejects_name_that_is_only_star() {
        let (status, _) = call(
            admin_state(),
            "PUT",
            "/v0/management/keys/*",
            Some("correct-admin-key"),
            Some(serde_json::json!({ "secret": "sk-test" })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn put_accepts_name_without_star() {
        let (status, body) = call(
            admin_state(),
            "PUT",
            "/v0/management/keys/normal-key",
            Some("correct-admin-key"),
            Some(serde_json::json!({ "secret": "sk-test" })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["success"], true);
    }

    #[tokio::test]
    async fn put_accepts_positive_whole_minute_expiry_when_creating() {
        let state = admin_state();
        let (status, _) = call(
            state.clone(),
            "PUT",
            "/v0/management/keys/time-bound-key",
            Some("correct-admin-key"),
            Some(serde_json::json!({ "secret": "sk-test", "expiresInMinutes": 60 })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(state
            .inner
            .keys
            .read()
            .unwrap()
            .get("time-bound-key")
            .unwrap()
            .expires_at
            .is_some());
    }

    #[tokio::test]
    async fn put_rejects_expiry_changes_for_existing_key() {
        let mut keys = HashMap::new();
        keys.insert(
            "existing-expiry-key".to_string(),
            KeyConfig {
                secret: "sk-existing".to_string(),
                expires_at: Some(now_ms() + 60_000),
                ..Default::default()
            },
        );
        let state = AppState::new(Some("correct-admin-key".to_string()), keys);
        let (status, _) = call(
            state,
            "PUT",
            "/v0/management/keys/existing-expiry-key",
            Some("correct-admin-key"),
            Some(serde_json::json!({ "secret": "sk-existing", "expiresInMinutes": 60 })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn put_normalizes_legacy_quota_field() {
        let state = admin_state();
        let (status, _) = call(
            state.clone(),
            "PUT",
            "/v0/management/keys/legacy-quota-key",
            Some("correct-admin-key"),
            Some(serde_json::json!({ "secret": "sk-test", "quota": "my-quota" })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            state
                .inner
                .keys
                .read()
                .unwrap()
                .get("legacy-quota-key")
                .unwrap()
                .quota_names,
            vec!["my-quota".to_string()]
        );
    }

    #[tokio::test]
    async fn patch_legacy_quota_replaces_existing_quotas() {
        let mut keys = HashMap::new();
        keys.insert(
            "existing-key".to_string(),
            KeyConfig {
                secret: "sk-existing".to_string(),
                quota_names: vec!["old-quota".to_string()],
                ..Default::default()
            },
        );
        let state = AppState::new(Some("correct-admin-key".to_string()), keys);
        let (status, _) = call(
            state.clone(),
            "PATCH",
            "/v0/management/keys/existing-key",
            Some("correct-admin-key"),
            Some(serde_json::json!({ "quota": "new-quota" })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            state
                .inner
                .keys
                .read()
                .unwrap()
                .get("existing-key")
                .unwrap()
                .quota_names,
            vec!["new-quota".to_string()]
        );
    }

    #[tokio::test]
    async fn patch_explicit_quotas_wins_over_legacy_quota() {
        let mut keys = HashMap::new();
        keys.insert(
            "existing-key-2".to_string(),
            KeyConfig {
                secret: "sk-existing-2".to_string(),
                quota_names: vec!["old-quota".to_string()],
                ..Default::default()
            },
        );
        let state = AppState::new(Some("correct-admin-key".to_string()), keys);
        let (status, _) = call(
            state.clone(),
            "PATCH",
            "/v0/management/keys/existing-key-2",
            Some("correct-admin-key"),
            Some(serde_json::json!({ "quota": "legacy-quota", "quotas": ["explicit-quota"] })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            state
                .inner
                .keys
                .read()
                .unwrap()
                .get("existing-key-2")
                .unwrap()
                .quota_names,
            vec!["explicit-quota".to_string()]
        );
    }

    #[tokio::test]
    async fn patch_unrelated_to_quotas_leaves_quotas_untouched() {
        let mut keys = HashMap::new();
        keys.insert(
            "existing-key-3".to_string(),
            KeyConfig {
                secret: "sk-existing-3".to_string(),
                quota_names: vec!["stays-the-same".to_string()],
                ..Default::default()
            },
        );
        let state = AppState::new(Some("correct-admin-key".to_string()), keys);
        let (status, _) = call(
            state.clone(),
            "PATCH",
            "/v0/management/keys/existing-key-3",
            Some("correct-admin-key"),
            Some(serde_json::json!({ "comment": "updated comment" })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let updated = state.inner.keys.read().unwrap();
        let updated = updated.get("existing-key-3").unwrap();
        assert_eq!(updated.quota_names, vec!["stays-the-same".to_string()]);
        assert_eq!(updated.comment, Some("updated comment".to_string()));
    }

    #[tokio::test]
    async fn patch_nonexistent_key_returns_404() {
        let (status, _) = call(
            admin_state(),
            "PATCH",
            "/v0/management/keys/does-not-exist",
            Some("correct-admin-key"),
            Some(serde_json::json!({ "comment": "x" })),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn patch_rejects_expiry_changes() {
        let mut keys = HashMap::new();
        keys.insert(
            "existing-expiry-key".to_string(),
            KeyConfig {
                secret: "sk-existing".to_string(),
                expires_at: Some(now_ms() + 60_000),
                ..Default::default()
            },
        );
        let state = AppState::new(Some("correct-admin-key".to_string()), keys);
        let (status, _) = call(
            state,
            "PATCH",
            "/v0/management/keys/existing-expiry-key",
            Some("correct-admin-key"),
            Some(serde_json::json!({ "expiresInMinutes": 60 })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn disable_a_time_bound_key() {
        let mut keys = HashMap::new();
        keys.insert(
            "time-bound-key".to_string(),
            KeyConfig {
                secret: "sk-test".to_string(),
                expires_at: Some(now_ms() + 60_000),
                ..Default::default()
            },
        );
        let state = AppState::new(Some("correct-admin-key".to_string()), keys);
        let (status, _) = call(
            state,
            "POST",
            "/v0/management/keys/time-bound-key/disable",
            Some("correct-admin-key"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn disable_rejects_a_non_expiring_key() {
        let mut keys = HashMap::new();
        keys.insert(
            "permanent-key".to_string(),
            KeyConfig {
                secret: "sk-test".to_string(),
                ..Default::default()
            },
        );
        let state = AppState::new(Some("correct-admin-key".to_string()), keys);
        let (status, _) = call(
            state,
            "POST",
            "/v0/management/keys/permanent-key/disable",
            Some("correct-admin-key"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    // Admin gating (not in the multi-quota test file, but required by the
    // reference's adminOnly scope — ported from admin-auth.test.ts's
    // pattern applied to a mutating route).

    #[tokio::test]
    async fn limited_key_gets_403() {
        let mut keys = HashMap::new();
        keys.insert(
            "limited-key".to_string(),
            KeyConfig {
                secret: "sk-limited".to_string(),
                ..Default::default()
            },
        );
        let state = AppState::new(Some("correct-admin-key".to_string()), keys);
        let (status, body) = call(
            state,
            "GET",
            "/v0/management/keys",
            Some("sk-limited"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["error"]["type"], "forbidden");
    }

    #[tokio::test]
    async fn missing_credential_gets_401() {
        let (status, _) = call(admin_state(), "GET", "/v0/management/keys", None, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
}
