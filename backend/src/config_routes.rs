//! Initial management config surface: resource-count status and timeout.
//!
//! Ports `GET /v0/management/config/status` and `GET`/`PATCH
//! `/v0/management/config/timeout` from Plexus. MCP server management is not
//! yet ported, so `mcpServerCount` is currently zero.

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json, Router,
};

use crate::{
    auth::{resolve_principal, Principal},
    AppState,
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/v0/management/config/status",
            axum::routing::get(config_status),
        )
        .route(
            "/v0/management/config/timeout",
            axum::routing::get(get_timeout).patch(patch_timeout),
        )
}

#[allow(clippy::result_large_err)]
fn require_admin(state: &AppState, headers: &HeaderMap) -> Result<(), Response> {
    let provided_key = headers
        .get("x-admin-key")
        .and_then(|value| value.to_str().ok());
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

async fn config_status(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    Json(serde_json::json!({
        "providerCount": state.inner.providers.read().unwrap().len(),
        "modelAliasCount": state.inner.aliases.read().unwrap().len(),
        "keyCount": state.inner.keys.read().unwrap().len(),
        "quotaCount": state.inner.user_quotas.read().unwrap().len(),
        "mcpServerCount": 0,
    }))
    .into_response()
}

fn timeout_seconds(state: &AppState) -> i64 {
    state
        .inner
        .settings
        .read()
        .unwrap()
        .get("timeout.defaultSeconds")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(300)
}

async fn get_timeout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    Json(serde_json::json!({ "defaultSeconds": timeout_seconds(&state) })).into_response()
}

async fn patch_timeout(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    let Some(object) = body.as_object() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "Object body is required" })),
        )
            .into_response();
    };

    if let Some(raw) = object.get("defaultSeconds") {
        let value = match raw {
            serde_json::Value::Number(number) => number.as_f64(),
            serde_json::Value::String(value) => value.trim().parse::<f64>().ok(),
            serde_json::Value::Bool(value) => Some(if *value { 1.0 } else { 0.0 }),
            serde_json::Value::Null => Some(0.0),
            _ => None,
        };
        let Some(value) = value.filter(|value| {
            value.is_finite() && value.fract() == 0.0 && (1.0..=3600.0).contains(value)
        }) else {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "defaultSeconds must be an integer between 1 and 3600"
                })),
            )
                .into_response();
        };
        let value = value as i64;
        if let Some(pool) = &state.inner.database {
            let entries = serde_json::Map::from_iter([(
                "timeout.defaultSeconds".to_string(),
                serde_json::json!(value),
            )]);
            if crate::database::upsert_settings_bulk(pool, &entries)
                .await
                .is_err()
            {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({ "error": "Internal server error" })),
                )
                    .into_response();
            }
        }
        state
            .inner
            .settings
            .write()
            .unwrap()
            .insert("timeout.defaultSeconds".into(), serde_json::json!(value));
    }

    Json(serde_json::json!({ "defaultSeconds": timeout_seconds(&state) })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{KeyConfig, ModelConfig, ProviderConfig, QuotaDefinition};
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use std::collections::HashMap;
    use tower::ServiceExt;

    fn fixture() -> AppState {
        let mut keys = HashMap::new();
        keys.insert(
            "limited".into(),
            KeyConfig {
                secret: "secret".into(),
                ..Default::default()
            },
        );
        let state = AppState::new(Some("admin".into()), keys);
        state
            .inner
            .providers
            .write()
            .unwrap()
            .insert("provider".into(), ProviderConfig::default());
        state
            .inner
            .aliases
            .write()
            .unwrap()
            .insert("alias".into(), ModelConfig::default());
        state.inner.user_quotas.write().unwrap().insert(
            "quota".into(),
            QuotaDefinition {
                quota_type: "daily".into(),
                limit_type: "requests".into(),
                limit: 10.0,
                duration: None,
                allowed_providers: None,
                excluded_providers: None,
                allowed_models: None,
                excluded_models: None,
                shared: None,
                warn_at: None,
            },
        );
        state
    }

    #[tokio::test]
    async fn returns_resource_counts_for_admin() {
        let response = router()
            .with_state(fixture())
            .oneshot(
                Request::get("/v0/management/config/status")
                    .header("x-admin-key", "admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(
            body,
            serde_json::json!({
                "providerCount": 1,
                "modelAliasCount": 1,
                "keyCount": 1,
                "quotaCount": 1,
                "mcpServerCount": 0,
            })
        );
    }

    #[tokio::test]
    async fn rejects_limited_and_missing_credentials() {
        for (credential, expected) in [
            (Some("secret"), StatusCode::FORBIDDEN),
            (None, StatusCode::UNAUTHORIZED),
        ] {
            let mut request = Request::get("/v0/management/config/status");
            if let Some(credential) = credential {
                request = request.header("x-admin-key", credential);
            }
            let response = router()
                .with_state(fixture())
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), expected);
        }
    }

    #[tokio::test]
    async fn timeout_defaults_to_300_and_patch_round_trips() {
        let state = fixture();
        let get = router()
            .with_state(state.clone())
            .oneshot(
                Request::get("/v0/management/config/timeout")
                    .header("x-admin-key", "admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let get_body: serde_json::Value =
            serde_json::from_slice(&get.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(get_body, serde_json::json!({ "defaultSeconds": 300 }));

        let patch = router()
            .with_state(state)
            .oneshot(
                Request::patch("/v0/management/config/timeout")
                    .header("x-admin-key", "admin")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"defaultSeconds":"45"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        let patch_body: serde_json::Value =
            serde_json::from_slice(&patch.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(patch_body, serde_json::json!({ "defaultSeconds": 45 }));
    }

    #[tokio::test]
    async fn timeout_patch_rejects_invalid_values_and_non_object_body() {
        for (body, expected_error) in [
            (
                r#"{"defaultSeconds":0}"#,
                "defaultSeconds must be an integer between 1 and 3600",
            ),
            (
                r#"{"defaultSeconds":3601}"#,
                "defaultSeconds must be an integer between 1 and 3600",
            ),
            (
                r#"{"defaultSeconds":1.5}"#,
                "defaultSeconds must be an integer between 1 and 3600",
            ),
            (r#"[]"#, "Object body is required"),
        ] {
            let response = router()
                .with_state(fixture())
                .oneshot(
                    Request::patch("/v0/management/config/timeout")
                        .header("x-admin-key", "admin")
                        .header("content-type", "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            let response_body: serde_json::Value =
                serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                    .unwrap();
            assert_eq!(response_body["error"], expected_error);
        }
    }
}
