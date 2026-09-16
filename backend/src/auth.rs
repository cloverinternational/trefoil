//! Ported contract: `GET /v0/management/auth/verify` from Plexus
//! (`packages/backend/src/routes/management.ts` +
//! `packages/backend/src/routes/management/_principal.ts`).
//!
//! Behavior preserved from the reference:
//! - Credential is read from the `x-admin-key` header.
//! - Missing/empty header -> 401.
//! - Header matches `ADMIN_KEY` (constant-time) -> 200 admin principal.
//! - Header matches a configured key's `secret` (constant-time, hashed) ->
//!   200 limited principal with that key's metadata.
//! - Otherwise -> 401.
//! - Error body shape: `{"error": {"message", "type", "code"}}`.

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::state::{AppState, KeyConfig};

pub fn router() -> Router<AppState> {
    Router::new().route("/v0/management/auth/verify", get(verify))
}

#[derive(Debug, Clone)]
pub enum Principal {
    Admin,
    Limited { key_name: String, config: KeyConfig },
}

#[derive(Debug, Serialize)]
struct AdminVerifyResponse {
    ok: bool,
    role: &'static str,
}

#[derive(Debug, Serialize)]
struct LimitedVerifyResponse {
    ok: bool,
    role: &'static str,
    #[serde(rename = "keyName")]
    key_name: String,
    #[serde(rename = "allowedProviders")]
    allowed_providers: Vec<String>,
    #[serde(rename = "allowedModels")]
    allowed_models: Vec<String>,
    #[serde(rename = "excludedProviders")]
    excluded_providers: Vec<String>,
    #[serde(rename = "excludedModels")]
    excluded_models: Vec<String>,
    #[serde(rename = "quotaNames")]
    quota_names: Vec<String>,
    #[serde(rename = "quotaName")]
    quota_name: Option<String>,
    comment: Option<String>,
}

pub struct AuthError {
    status: StatusCode,
    message: &'static str,
    error_type: &'static str,
}

impl AuthError {
    fn unauthorized() -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            message: "Unauthorized",
            error_type: "auth_error",
        }
    }
}

impl IntoResponse for AuthError {
    fn into_response(self) -> Response {
        let code = self.status.as_u16();
        let body = serde_json::json!({
            "error": {
                "message": self.message,
                "type": self.error_type,
                "code": code,
            }
        });
        (self.status, Json(body)).into_response()
    }
}

/// Constant-time equality on raw bytes (mirrors Node's `timingSafeEqual`
/// after a length check).
fn constant_time_equals(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.as_bytes().ct_eq(b.as_bytes()).into()
}

/// SHA-256 the two inputs then constant-time compare, matching
/// `constantTimeHashEquals` in the reference so secret length never leaks.
fn constant_time_hash_equals(a: &str, b: &str) -> bool {
    let a_hash = Sha256::digest(a.as_bytes());
    let b_hash = Sha256::digest(b.as_bytes());
    a_hash.ct_eq(&b_hash).into()
}

/// Resolve the principal for a request, walking the full key set even after
/// a match (matches the reference's rationale for avoiding a timing signal
/// on key position).
pub fn resolve_principal(state: &AppState, provided_key: Option<&str>) -> Option<Principal> {
    let provided_key = provided_key?;
    if provided_key.is_empty() {
        return None;
    }

    if let Some(admin_key) = &state.inner.admin_key {
        if constant_time_equals(provided_key, admin_key) {
            return Some(Principal::Admin);
        }
    }

    let mut matched: Option<(String, KeyConfig)> = None;
    for (name, cfg) in state.inner.keys.iter() {
        if constant_time_hash_equals(&cfg.secret, provided_key) && matched.is_none() {
            matched = Some((name.clone(), cfg.clone()));
        }
    }
    matched.map(|(key_name, config)| Principal::Limited { key_name, config })
}

async fn verify(State(state): State<AppState>, headers: HeaderMap) -> Result<Response, AuthError> {
    let provided_key = headers
        .get("x-admin-key")
        .and_then(|v| v.to_str().ok());

    match resolve_principal(&state, provided_key) {
        Some(Principal::Admin) => Ok(Json(AdminVerifyResponse {
            ok: true,
            role: "admin",
        })
        .into_response()),
        Some(Principal::Limited { key_name, config }) => Ok(Json(LimitedVerifyResponse {
            ok: true,
            role: "limited",
            key_name,
            allowed_providers: config.allowed_providers,
            allowed_models: config.allowed_models,
            excluded_providers: config.excluded_providers,
            excluded_models: config.excluded_models,
            quota_name: config.quota_names.first().cloned(),
            quota_names: config.quota_names,
            comment: config.comment,
        })
        .into_response()),
        None => Err(AuthError::unauthorized()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use std::collections::HashMap;
    use tower::ServiceExt;

    fn state_with(admin_key: &str) -> AppState {
        let mut keys = HashMap::new();
        keys.insert(
            "test-key".to_string(),
            KeyConfig {
                secret: "sk-test-secret".to_string(),
                ..Default::default()
            },
        );
        AppState::new(Some(admin_key.to_string()), keys)
    }

    async fn call(state: AppState, header: Option<&str>) -> (StatusCode, serde_json::Value) {
        let mut req = Request::builder()
            .uri("/v0/management/auth/verify")
            .method("GET");
        if let Some(h) = header {
            req = req.header("x-admin-key", h);
        }
        let response = router()
            .with_state(state)
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body = if bytes.is_empty() {
            serde_json::json!(null)
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        (status, body)
    }

    // Ported from admin-auth.test.ts: "returns 200 with admin principal info
    // when the correct admin key is provided".
    #[tokio::test]
    async fn returns_200_with_admin_principal_for_correct_admin_key() {
        let (status, body) = call(state_with("correct-admin-key"), Some("correct-admin-key")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ok"], true);
        assert_eq!(body["role"], "admin");
    }

    // Ported: "returns 200 with limited principal info when an api_keys
    // secret is provided".
    #[tokio::test]
    async fn returns_200_with_limited_principal_for_key_secret() {
        let (status, body) = call(state_with("correct-admin-key"), Some("sk-test-secret")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ok"], true);
        assert_eq!(body["role"], "limited");
        assert_eq!(body["keyName"], "test-key");
    }

    // Ported: "returns 401 when an incorrect admin key is provided".
    #[tokio::test]
    async fn returns_401_for_incorrect_admin_key() {
        let (status, _) = call(state_with("correct-admin-key"), Some("wrong-key")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    // Ported: "returns 401 when no x-admin-key header is present".
    #[tokio::test]
    async fn returns_401_when_header_missing() {
        let (status, _) = call(state_with("correct-admin-key"), None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    // Ported: "returns a well-formed error body on rejection".
    #[tokio::test]
    async fn returns_well_formed_error_body_on_rejection() {
        let (_, body) = call(state_with("correct-admin-key"), Some("bad-key")).await;
        assert_eq!(body["error"]["message"], "Unauthorized");
        assert_eq!(body["error"]["type"], "auth_error");
        assert_eq!(body["error"]["code"], 401);
    }

    // Ported: "returns 401 when x-admin-key is an empty string".
    #[tokio::test]
    async fn returns_401_for_empty_header() {
        let (status, _) = call(state_with("correct-admin-key"), Some("")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
}
