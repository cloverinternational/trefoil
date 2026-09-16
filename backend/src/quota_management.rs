//! Administrative quota-state management.

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json, Router,
};
use serde_json::Value;

use crate::{
    auth::{resolve_principal, Principal},
    AppState,
};

pub fn router() -> Router<AppState> {
    Router::new().route(
        "/v0/management/quota/clear",
        axum::routing::post(clear_quota),
    )
}

#[allow(clippy::result_large_err)]
fn require_admin(state: &AppState, headers: &HeaderMap) -> Result<(), Response> {
    let key = headers.get("x-admin-key").and_then(|v| v.to_str().ok());
    match resolve_principal(state,key){
        Some(Principal::Admin)=>Ok(()),
        Some(Principal::Limited{..})=>Err((StatusCode::FORBIDDEN,Json(serde_json::json!({"error":{"message":"Admin privileges required","type":"forbidden","code":403}}))).into_response()),
        None=>Err((StatusCode::UNAUTHORIZED,Json(serde_json::json!({"error":{"message":"Unauthorized","type":"auth_error","code":401}}))).into_response()),
    }
}

async fn clear_quota(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    let key = body
        .get("key")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty());
    let Some(key) = key else {
        return error(
            StatusCode::BAD_REQUEST,
            "Missing required field: key",
            "invalid_request_error",
        );
    };
    let quota = body
        .get("quota")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty());
    if let Some(quota) = quota {
        let keys = state.inner.keys.read().unwrap();
        let Some(config) = keys.get(key) else {
            return error(
                StatusCode::NOT_FOUND,
                &format!("Key not found: {key}"),
                "not_found_error",
            );
        };
        let attached: Vec<String> = if config.quota_names.is_empty() {
            state
                .inner
                .settings
                .read()
                .unwrap()
                .get("default_quotas")
                .and_then(Value::as_array)
                .map(|v| {
                    v.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default()
        } else {
            config.quota_names.clone()
        };
        if !attached.iter().any(|attached| attached == quota) {
            return error(
                StatusCode::BAD_REQUEST,
                &format!("Quota '{quota}' is not attached to key '{key}'"),
                "invalid_request_error",
            );
        }
    }
    if let Some(pool) = &state.inner.database {
        let result = if let Some(quota) = quota {
            sqlx::query("DELETE FROM quota_state WHERE key_name=$1 AND quota_name=$2")
                .bind(key)
                .bind(quota)
                .execute(pool)
                .await
        } else {
            sqlx::query("DELETE FROM quota_state WHERE key_name=$1")
                .bind(key)
                .execute(pool)
                .await
        };
        if let Err(error) = result {
            return error_response(&error.to_string());
        }
    }
    Json(serde_json::json!({"success":true,"key":key,"quota":quota,"message":quota.map(|q|format!("Quota '{q}' reset successfully")).unwrap_or_else(||"Quota reset successfully".into())})).into_response()
}
fn error(status: StatusCode, message: &str, error_type: &str) -> Response {
    (
        status,
        Json(serde_json::json!({"error":{"message":message,"type":error_type}})),
    )
        .into_response()
}
fn error_response(message: &str) -> Response {
    error(StatusCode::INTERNAL_SERVER_ERROR, message, "server_error")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::KeyConfig;
    use axum::{body::Body, http::Request};
    use std::collections::HashMap;
    use tower::ServiceExt;
    fn fixture() -> AppState {
        let mut keys = HashMap::new();
        keys.insert(
            "key".into(),
            KeyConfig {
                secret: "secret".into(),
                quota_names: vec!["q1".into()],
                ..Default::default()
            },
        );
        AppState::new(Some("admin".into()), keys)
    }
    async fn post(body: &str) -> Response {
        router()
            .with_state(fixture())
            .oneshot(
                Request::post("/v0/management/quota/clear")
                    .header("x-admin-key", "admin")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_owned()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }
    #[tokio::test]
    async fn validates_and_clears() {
        assert_eq!(post("{}").await.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            post(r#"{"key":"missing","quota":"q1"}"#).await.status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            post(r#"{"key":"key","quota":"other"}"#).await.status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            post(r#"{"key":"key","quota":"q1"}"#).await.status(),
            StatusCode::OK
        );
        assert_eq!(post(r#"{"key":"anything"}"#).await.status(), StatusCode::OK);
    }
}
