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
use serde::Serialize;
use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::auth::{resolve_principal, Principal};
use crate::state::{AppState, KeyConfig};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/v0/management/keys", get(list_keys))
        .route(
            "/v0/management/keys/:name",
            get(get_key)
                .put(put_key)
                .patch(patch_key)
                .delete(delete_key),
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
        None => ApiError::Simple(StatusCode::NOT_FOUND, format!("API key '{name}' not found"))
            .into_response(),
    }
}

/// Pinned Plexus `KeyConfigSchema` (`config.ts`).
fn key_schema() -> &'static crate::zod::Schema {
    use crate::zod::*;
    static SCHEMA: std::sync::OnceLock<Schema> = std::sync::OnceLock::new();
    SCHEMA.get_or_init(|| {
        let names = || array(string().min(1.0)).optional();
        object(vec![
            ("secret", string()),
            ("comment", string().optional()),
            ("expiresInMinutes", number().int().positive().optional()),
            ("expiresAt", number().int().positive().optional()),
            ("disabledAt", number().int().positive().optional()),
            ("quota", string().optional()),
            ("quotas", names()),
            ("allowedModels", names()),
            ("allowedProviders", names()),
            ("excludedModels", names()),
            ("excludedProviders", names()),
            ("allowRawPassthrough", boolean().optional()),
            ("defaultServiceTier", enumeration(&["auto", "default", "standard", "flex", "priority", "fast", "ultrafast"]).nullable().optional()),
            ("allowedIps", array(string().min(1.0).refine(|v| v.as_str().is_some_and(crate::config_routes::valid_ip_rule),
                "Invalid IP rule. Use IPv4/IPv6, CIDR (a.b.c.d/n, ::/n), or a range (a.b.c.d-N or addr-addr).")).optional()),
        ])
    })
}

/// `normalizeKeyConfig`: explicit `quotas` (even `[]`) wins over legacy `quota`.
fn normalize_key_config(mut value: Value) -> Value {
    if let Some(map) = value.as_object_mut() {
        if !map.contains_key("quotas") {
            if let Some(quota) = map.get("quota").and_then(Value::as_str).filter(|q| !q.is_empty()).map(str::to_owned) {
                map.insert("quotas".into(), serde_json::json!([quota]));
            }
        }
    }
    value
}

fn validation_failed(details: Vec<Value>) -> Response {
    (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "Validation failed", "details": details}))).into_response()
}

/// `KeyRepository.saveKey` from a parsed config. Expiry is fixed at creation.
fn key_from_parsed(parsed: &Value, existing: Option<&KeyConfig>) -> KeyConfig {
    let list = |key: &str| parsed.get(key).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()).collect::<Vec<_>>());
    let quotas = list("quotas");
    KeyConfig {
        secret: parsed["secret"].as_str().unwrap_or_default().to_owned(),
        permission_group: None,
        comment: parsed.get("comment").and_then(Value::as_str).map(str::to_owned),
        quotas_defined: quotas.is_some(),
        quota_names: quotas.unwrap_or_default(),
        allowed_providers: list("allowedProviders").unwrap_or_default(),
        allowed_models: list("allowedModels").unwrap_or_default(),
        excluded_providers: list("excludedProviders").unwrap_or_default(),
        excluded_models: list("excludedModels").unwrap_or_default(),
        allow_raw_passthrough: parsed.get("allowRawPassthrough") == Some(&Value::Bool(true)),
        allowed_ips: list("allowedIps").unwrap_or_default(),
        expires_at: match existing {
            Some(existing) => existing.expires_at,
            None => parsed.get("expiresInMinutes").and_then(Value::as_i64).map(|minutes| now_ms() + minutes * 60_000),
        },
        disabled_at: existing.and_then(|e| e.disabled_at),
        default_service_tier: parsed.get("defaultServiceTier").and_then(Value::as_str).map(str::to_owned),
    }
}

async fn save_key(state: &AppState, name: &str, config: KeyConfig) -> Response {
    if let Some(pool) = &state.inner.database {
        if crate::database::upsert_key(pool, name, &config).await.is_err() {
            return ApiError::Simple(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error".to_string()).into_response();
        }
    }
    state.inner.keys.write().unwrap().insert(name.to_owned(), config);
    Json(serde_json::json!({ "success": true, "name": name })).into_response()
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
        return ApiError::Simple(StatusCode::BAD_REQUEST, "Key name cannot contain '*' (reserved for the shared-quota bucket)".to_string()).into_response();
    }
    if !raw.is_object() {
        return ApiError::Simple(StatusCode::BAD_REQUEST, "Object body is required".to_string()).into_response();
    }
    let existing = state.inner.keys.read().unwrap().get(&name).cloned();
    if existing.is_some() && has_expiry_fields(&raw) {
        return ApiError::Simple(StatusCode::BAD_REQUEST, "Key expiry cannot be changed after creation".to_string()).into_response();
    }
    if raw.get("expiresAt").is_some() || raw.get("disabledAt").is_some() {
        return ApiError::Simple(StatusCode::BAD_REQUEST, "Expiry timestamps are managed by Plexus".to_string()).into_response();
    }
    let parsed = match key_schema().safe_parse(&raw) {
        Ok(parsed) => normalize_key_config(parsed),
        Err(details) => return validation_failed(details),
    };
    save_key(&state, &name, key_from_parsed(&parsed, existing.as_ref())).await
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
        return ApiError::Simple(StatusCode::BAD_REQUEST, "Object body is required".to_string()).into_response();
    }
    if has_expiry_fields(&raw) {
        return ApiError::Simple(StatusCode::BAD_REQUEST, "Key expiry cannot be changed after creation".to_string()).into_response();
    }
    let Some(existing) = state.inner.keys.read().unwrap().get(&name).cloned() else {
        return ApiError::Simple(StatusCode::NOT_FOUND, format!("API key '{name}' not found")).into_response();
    };
    let mut merged = serde_json::to_value(&existing).unwrap_or(Value::Null);
    if let (Some(map), Value::Object(body)) = (merged.as_object_mut(), normalize_key_config(raw)) {
        for (key, value) in body { map.insert(key, value); }
    }
    let parsed = match key_schema().safe_parse(&merged) {
        Ok(parsed) => parsed,
        Err(details) => return validation_failed(details),
    };
    save_key(&state, &name, key_from_parsed(&parsed, Some(&existing))).await
}

async fn delete_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    if let Err(e) = require_admin(&state, &headers) {
        return e.into_response();
    }
    if let Some(pool) = &state.inner.database {
        if let Err(error) = crate::database::delete_key(pool, &name).await {
            return ApiError::Simple(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
                .into_response();
        }
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
    let Some(mut existing) = state.inner.keys.read().unwrap().get(&name).cloned() else {
        return ApiError::Simple(StatusCode::NOT_FOUND, format!("API key '{name}' not found"))
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
    if let Some(pool) = &state.inner.database {
        if let Err(error) = crate::database::upsert_key(pool, &name, &existing).await {
            return ApiError::Simple(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
                .into_response();
        }
    }

    state
        .inner
        .keys
        .write()
        .unwrap()
        .insert(name.clone(), existing);

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
