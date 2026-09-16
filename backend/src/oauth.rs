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
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json, Router,
};

pub fn router() -> Router<AppState> {
    Router::new().route(
        "/v0/management/oauth/providers",
        axum::routing::get(providers),
    )
}
async fn providers(State(state): State<AppState>, headers: HeaderMap) -> Response {
    match resolve_principal(&state,headers.get("x-admin-key").and_then(|v|v.to_str().ok())){Some(Principal::Admin)=>{},Some(Principal::Limited{..})=>return (StatusCode::FORBIDDEN,Json(serde_json::json!({"error":{"message":"Admin privileges required","type":"forbidden","code":403}}))).into_response(),None=>return (StatusCode::UNAUTHORIZED,Json(serde_json::json!({"error":{"message":"Unauthorized","type":"auth_error","code":401}}))).into_response()}
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
}
