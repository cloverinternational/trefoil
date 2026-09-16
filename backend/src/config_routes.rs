//! Initial management config surface: resource-count status.
//!
//! Ports `GET /v0/management/config/status` from Plexus. MCP server
//! management is not yet ported, so `mcpServerCount` is currently zero.

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
    Router::new().route(
        "/v0/management/config/status",
        axum::routing::get(config_status),
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
}
