//! OAuth management discovery surface.
//!
//! Plexus discovers these descriptors dynamically from its pinned pi-ai
//! provider registry. Trefoil has no pi-ai dependency, so this list mirrors
//! the reference registry at the pinned Plexus revision and must be refreshed
//! when that dependency's OAuth provider set changes.

use crate::{
    auth::{resolve_principal, Principal},
    AppState,
};
use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json, Router,
};
use serde::Deserialize;

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/v0/management/oauth/providers",
            axum::routing::get(providers),
        )
        .route(
            "/v0/management/oauth/credentials/status",
            axum::routing::get(credential_status),
        )
        .route(
            "/v0/management/oauth/credentials",
            axum::routing::delete(delete_credentials),
        )
        .route(
            "/v0/management/oauth/sessions/:id",
            axum::routing::get(session_not_found),
        )
}

async fn session_not_found(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    let _ = id;
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({"error":"OAuth session not found"})),
    )
        .into_response()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CredentialStatusQuery {
    provider_id: Option<String>,
    account_id: Option<String>,
}

#[allow(clippy::result_large_err)]
fn require_admin(state: &AppState, headers: &HeaderMap) -> Result<(), Response> {
    match resolve_principal(
        state,
        headers.get("x-admin-key").and_then(|value| value.to_str().ok()),
    ) {
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

async fn providers(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    let data = serde_json::json!([
        { "id": "anthropic", "name": "Anthropic (Claude Pro/Max)", "usesCallbackServer": true },
        { "id": "github-copilot", "name": "GitHub Copilot", "usesCallbackServer": false },
        { "id": "kimi-coding", "name": "Kimi Code (subscription)", "usesCallbackServer": false },
        { "id": "openai-codex", "name": "OpenAI (ChatGPT Plus/Pro)", "usesCallbackServer": true },
        { "id": "openrouter", "name": "OpenRouter OAuth", "usesCallbackServer": false },
        { "id": "xai", "name": "xAI (Grok/X subscription)", "usesCallbackServer": false },
    ]);
    Json(serde_json::json!({ "data": data, "total": 6 })).into_response()
}

async fn credential_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<CredentialStatusQuery>,
) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    let mut details = Vec::new();
    for (field, value) in [
        ("providerId", query.provider_id.as_deref()),
        ("accountId", query.account_id.as_deref()),
    ] {
        if value.is_none_or(str::is_empty) {
            details.push(query_issue(field, value.is_some()));
        }
    }
    if !details.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "Invalid query parameters", "details": details })),
        )
            .into_response();
    }
    let provider_id = query.provider_id.unwrap();
    let account_id = query.account_id.unwrap();
    let ready = if let Some(pool) = &state.inner.database {
        sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM oauth_credentials WHERE oauth_provider_type = $1 AND account_id = $2)",
        )
        .bind(provider_id)
        .bind(account_id)
        .fetch_one(pool)
        .await
        .unwrap_or(false)
    } else {
        false
    };
    Json(serde_json::json!({ "data": { "ready": ready } })).into_response()
}

fn query_issue(field: &str, present: bool) -> serde_json::Value {
    if present {
        serde_json::json!({
            "origin": "string", "code": "too_small", "minimum": 1,
            "inclusive": true, "path": [field],
            "message": "Too small: expected string to have >=1 characters"
        })
    } else {
        serde_json::json!({
            "expected": "string", "code": "invalid_type", "path": [field],
            "message": "Invalid input: expected string, received undefined"
        })
    }
}

async fn delete_credentials(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    let object = body.as_object();
    let mut details = Vec::new();
    for field in ["providerId", "accountId"] {
        match object.and_then(|object| object.get(field)) {
            Some(serde_json::Value::String(value)) if !value.is_empty() => {}
            Some(serde_json::Value::String(_)) => details.push(serde_json::json!({
                "origin": "string", "code": "too_small", "minimum": 1,
                "inclusive": true, "path": [field],
                "message": "Too small: expected string to have >=1 characters"
            })),
            Some(value) => details.push(serde_json::json!({
                "expected": "string", "code": "invalid_type", "path": [field],
                "message": format!("Invalid input: expected string, received {}", json_type(value))
            })),
            None => details.push(query_issue(field, false)),
        }
    }
    if !details.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "Invalid request body", "details": details })),
        )
            .into_response();
    }
    let object = object.unwrap();
    let deleted = if let Some(pool) = &state.inner.database {
        sqlx::query(
            "DELETE FROM oauth_credentials WHERE oauth_provider_type = $1 AND account_id = $2",
        )
        .bind(object["providerId"].as_str().unwrap())
        .bind(object["accountId"].as_str().unwrap())
        .execute(pool)
        .await
        .map(|result| result.rows_affected() > 0)
        .unwrap_or(false)
    } else {
        false
    };
    if !deleted {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "OAuth credentials not found" })),
        )
            .into_response();
    }
    Json(serde_json::json!({ "data": { "deleted": true } })).into_response()
}

fn json_type(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::KeyConfig;
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
        AppState::new(Some("admin".into()), keys)
    }

    #[tokio::test]
    async fn lists_pinned_oauth_provider_descriptors() {
        let response = router()
            .with_state(fixture())
            .oneshot(
                Request::get("/v0/management/oauth/providers")
                    .header("x-admin-key", "admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(body["total"], 6);
        assert_eq!(body["data"][0]["id"], "anthropic");
        assert_eq!(body["data"][3]["usesCallbackServer"], true);
        assert!(!body["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["id"] == "radius"));
    }

    #[tokio::test]
    async fn oauth_provider_discovery_is_admin_only() {
        for (credential, expected) in [
            (Some("secret"), StatusCode::FORBIDDEN),
            (None, StatusCode::UNAUTHORIZED),
        ] {
            let mut request = Request::get("/v0/management/oauth/providers");
            if let Some(value) = credential {
                request = request.header("x-admin-key", value);
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
    async fn credential_status_defaults_false_and_validates_queries() {
        let response = router()
            .with_state(fixture())
            .oneshot(
                Request::get("/v0/management/oauth/credentials/status?providerId=anthropic&accountId=default")
                    .header("x-admin-key", "admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(body, serde_json::json!({ "data": { "ready": false } }));

        let invalid = router()
            .with_state(fixture())
            .oneshot(
                Request::get("/v0/management/oauth/credentials/status")
                    .header("x-admin-key", "admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
        let body: serde_json::Value =
            serde_json::from_slice(&invalid.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(body["details"].as_array().unwrap().len(), 2);
        assert_eq!(
            body["details"][0]["path"],
            serde_json::json!(["providerId"])
        );
    }

    #[tokio::test]
    async fn delete_credentials_validates_body_and_returns_not_found() {
        let missing = router()
            .with_state(fixture())
            .oneshot(
                Request::delete("/v0/management/oauth/credentials")
                    .header("x-admin-key", "admin")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"providerId":"anthropic","accountId":"missing"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);

        let invalid = router()
            .with_state(fixture())
            .oneshot(
                Request::delete("/v0/management/oauth/credentials")
                    .header("x-admin-key", "admin")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
        let body: serde_json::Value =
            serde_json::from_slice(&invalid.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(body["details"].as_array().unwrap().len(), 2);
    }
}
