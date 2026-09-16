//! Ported contract: `GET`/`PATCH /v0/management/system-settings` from Plexus
//! (`packages/backend/src/routes/management/config.ts`), mounted under the
//! admin-only scope in `packages/backend/src/routes/management.ts`.
//!
//! Settings are stored as an untyped `Record<string, unknown>` bag; the only
//! validated field is `default_quotas`, which must reference names that
//! exist in `user_quotas` (referential integrity check against the sibling
//! store, mirroring `user_quotas.rs`'s reverse check on DELETE).

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json, Router,
};
use serde_json::Value;

use crate::auth::{resolve_principal, Principal};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route(
        "/v0/management/system-settings",
        axum::routing::get(get_settings).patch(patch_settings),
    )
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

async fn get_settings(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(resp) = require_admin(&state, &headers) {
        return resp;
    }
    let settings = state.inner.settings.read().unwrap();
    let map: serde_json::Map<String, Value> = settings
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    Json(Value::Object(map)).into_response()
}

async fn patch_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(raw): Json<Value>,
) -> Response {
    if let Err(resp) = require_admin(&state, &headers) {
        return resp;
    }
    let Some(body) = raw.as_object() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "Object body is required" })),
        )
            .into_response();
    };

    if let Some(dq) = body.get("default_quotas") {
        if !dq.is_null() {
            let is_string_array = dq
                .as_array()
                .is_some_and(|arr| arr.iter().all(|v| v.is_string()));
            if !is_string_array {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": {
                            "message": "'default_quotas' must be an array of quota names",
                            "type": "invalid_request_error",
                        }
                    })),
                )
                    .into_response();
            }

            let names: Vec<&str> = dq
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect();
            let defined = state.inner.user_quotas.read().unwrap();
            let unknown: Vec<&str> = names
                .into_iter()
                .filter(|n| !defined.contains_key(*n))
                .collect();
            if !unknown.is_empty() {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": {
                            "message": format!(
                                "Unknown quota name(s) in 'default_quotas': {}. Quotas must be defined in user_quotas first.",
                                unknown.join(", ")
                            ),
                            "type": "invalid_request_error",
                        }
                    })),
                )
                    .into_response();
            }
        }
    }

    if let Some(pool) = &state.inner.database {
        if let Err(error) = crate::database::upsert_settings_bulk(pool, body).await {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": error.to_string() })),
            )
                .into_response();
        }
    }

    let mut settings = state.inner.settings.write().unwrap();
    for (k, v) in body {
        settings.insert(k.clone(), v.clone());
    }

    Json(serde_json::json!({ "success": true })).into_response()
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
    async fn patch_400s_when_default_quotas_references_unknown_name() {
        let state = admin_state();
        state.inner.user_quotas.write().unwrap().insert(
            "my-quota".to_string(),
            crate::state::QuotaDefinition {
                quota_type: "daily".to_string(),
                limit_type: "requests".to_string(),
                limit: 100.0,
                duration: None,
                allowed_providers: None,
                excluded_providers: None,
                allowed_models: None,
                excluded_models: None,
                shared: None,
                warn_at: None,
            },
        );

        let (status, body) = call(
            state,
            "PATCH",
            "/v0/management/system-settings",
            Some("correct-admin-key"),
            Some(serde_json::json!({ "default_quotas": ["my-quota", "ghost-quota"] })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["type"], "invalid_request_error");
        assert!(body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("ghost-quota"));
    }

    #[tokio::test]
    async fn patch_400s_when_default_quotas_is_not_an_array() {
        let (status, body) = call(
            admin_state(),
            "PATCH",
            "/v0/management/system-settings",
            Some("correct-admin-key"),
            Some(serde_json::json!({ "default_quotas": "my-quota" })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["type"], "invalid_request_error");
    }

    #[tokio::test]
    async fn patch_400s_when_default_quotas_has_non_string_entries() {
        let (status, _) = call(
            admin_state(),
            "PATCH",
            "/v0/management/system-settings",
            Some("correct-admin-key"),
            Some(serde_json::json!({ "default_quotas": [1] })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn patch_200s_for_empty_array_and_null() {
        let state = admin_state();
        let (status, _) = call(
            state.clone(),
            "PATCH",
            "/v0/management/system-settings",
            Some("correct-admin-key"),
            Some(serde_json::json!({ "default_quotas": [] })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (status, _) = call(
            state,
            "PATCH",
            "/v0/management/system-settings",
            Some("correct-admin-key"),
            Some(serde_json::json!({ "default_quotas": null })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn patch_leaves_other_settings_free_form() {
        let state = admin_state();
        let (status, _) = call(
            state.clone(),
            "PATCH",
            "/v0/management/system-settings",
            Some("correct-admin-key"),
            Some(serde_json::json!({ "some_other_setting": "whatever" })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (_, body) = call(
            state,
            "GET",
            "/v0/management/system-settings",
            Some("correct-admin-key"),
            None,
        )
        .await;
        assert_eq!(body["some_other_setting"], "whatever");
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
            "/v0/management/system-settings",
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
            "/v0/management/system-settings",
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
}
