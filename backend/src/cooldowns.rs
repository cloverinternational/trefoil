//! Ported contract: `/v0/management/cooldowns/*` from Plexus
//! (`packages/backend/src/routes/management/cooldowns.ts`), mounted under
//! the *scoped* (not admin-only) block for GET, and admin-only for DELETE —
//! see `packages/backend/src/routes/management.ts` line ~87 vs the
//! `requireAdmin` preHandler on the delete routes.
//!
//! Cooldowns are dual-written to PostgreSQL (mirroring Plexus's
//! `cooldown-manager.ts`, which persists them via Drizzle) when a database
//! pool is attached, in addition to the in-memory `RwLock` used for the
//! hot read path.

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json, Router,
};
use serde::Deserialize;
use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::auth::{resolve_principal, Principal};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/v0/management/cooldowns", axum::routing::get(list_cooldowns).delete(delete_all_cooldowns))
        .route(
            "/v0/management/cooldowns/:provider",
            axum::routing::delete(delete_provider_cooldowns),
        )
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64
}

/// Read access is available to both admin and limited principals; only a
/// valid credential (of either role) is required.
fn require_any_principal(state: &AppState, headers: &HeaderMap) -> Result<(), Response> {
    let provided_key = headers.get("x-admin-key").and_then(|v| v.to_str().ok());
    match resolve_principal(state, provided_key) {
        Some(_) => Ok(()),
        None => Err((
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "error": { "message": "Unauthorized", "type": "auth_error", "code": 401 }
            })),
        )
            .into_response()),
    }
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

async fn list_cooldowns(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(resp) = require_any_principal(&state, &headers) {
        return resp;
    }

    let now = now_ms();
    let providers = state.inner.providers.read().unwrap();
    let cooldowns = state.inner.cooldowns.read().unwrap();

    let mut results: Vec<Value> = Vec::new();
    for ((provider, model), entry) in cooldowns.iter() {
        if entry.expiry_ms <= now {
            continue;
        }
        if providers.get(provider).is_some_and(|p| p.disable_cooldown) {
            continue;
        }
        let mut item = serde_json::json!({
            "provider": provider,
            "model": model,
            "expiry": entry.expiry_ms,
            "timeRemainingMs": entry.expiry_ms - now,
            "consecutiveFailures": entry.consecutive_failures,
        });
        if let Some(err) = &entry.last_error {
            item["lastError"] = Value::String(err.clone());
        }
        results.push(item);
    }

    Json(Value::Array(results)).into_response()
}

async fn delete_all_cooldowns(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(resp) = require_admin(&state, &headers) {
        return resp;
    }
    if let Some(pool) = &state.inner.database {
        if let Err(error) = crate::database::delete_all_cooldowns(pool).await {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": error.to_string() })),
            )
                .into_response();
        }
    }
    state.inner.cooldowns.write().unwrap().clear();
    Json(serde_json::json!({ "success": true })).into_response()
}

#[derive(Deserialize)]
struct ProviderCooldownQuery {
    model: Option<String>,
}

async fn delete_provider_cooldowns(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(provider): Path<String>,
    Query(query): Query<ProviderCooldownQuery>,
) -> Response {
    if let Err(resp) = require_admin(&state, &headers) {
        return resp;
    }

    if let Some(pool) = &state.inner.database {
        let result = match &query.model {
            Some(model) => crate::database::delete_cooldown(pool, &provider, model).await,
            None => crate::database::delete_cooldowns_by_provider(pool, &provider).await,
        };
        if let Err(error) = result {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": error.to_string() })),
            )
                .into_response();
        }
    }

    let mut cooldowns = state.inner.cooldowns.write().unwrap();
    match &query.model {
        Some(model) => {
            cooldowns.retain(|(p, m), _| !(p == &provider && m == model));
        }
        None => {
            cooldowns.retain(|(p, _), _| p != &provider);
        }
    }

    Json(serde_json::json!({ "success": true })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::CooldownEntry;
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
    ) -> (StatusCode, Value) {
        let mut req = Request::builder().uri(uri).method(method);
        if let Some(h) = admin_header {
            req = req.header("x-admin-key", h);
        }
        let response = router()
            .with_state(state)
            .oneshot(req.body(Body::empty()).unwrap())
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
    async fn list_is_empty_when_no_cooldowns() {
        let (status, body) = call(admin_state(), "GET", "/v0/management/cooldowns", Some("correct-admin-key")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, serde_json::json!([]));
    }

    #[tokio::test]
    async fn list_is_readable_by_limited_principal() {
        let mut keys = HashMap::new();
        keys.insert(
            "limited".to_string(),
            crate::state::KeyConfig { secret: "limited-secret".to_string(), ..Default::default() },
        );
        let state = AppState::new(Some("correct-admin-key".to_string()), keys);
        let (status, _) = call(state, "GET", "/v0/management/cooldowns", Some("limited-secret")).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn missing_credential_gets_401() {
        let (status, _) = call(admin_state(), "GET", "/v0/management/cooldowns", None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn list_excludes_expired_entries() {
        let state = admin_state();
        state.inner.cooldowns.write().unwrap().insert(
            ("prov-a".to_string(), "model-a".to_string()),
            CooldownEntry { expiry_ms: now_ms() - 1000, consecutive_failures: 1, last_error: None },
        );
        let (_, body) = call(state, "GET", "/v0/management/cooldowns", Some("correct-admin-key")).await;
        assert_eq!(body, serde_json::json!([]));
    }

    #[tokio::test]
    async fn list_includes_active_entries_with_last_error() {
        let state = admin_state();
        state.inner.cooldowns.write().unwrap().insert(
            ("prov-a".to_string(), "model-a".to_string()),
            CooldownEntry {
                expiry_ms: now_ms() + 60_000,
                consecutive_failures: 2,
                last_error: Some("boom".to_string()),
            },
        );
        let (_, body) = call(state, "GET", "/v0/management/cooldowns", Some("correct-admin-key")).await;
        let arr = body.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["provider"], "prov-a");
        assert_eq!(arr[0]["lastError"], "boom");
    }

    #[tokio::test]
    async fn list_excludes_disable_cooldown_providers() {
        let state = admin_state();
        state.inner.providers.write().unwrap().insert(
            "prov-a".to_string(),
            crate::state::ProviderConfig { disable_cooldown: true, ..Default::default() },
        );
        state.inner.cooldowns.write().unwrap().insert(
            ("prov-a".to_string(), "model-a".to_string()),
            CooldownEntry { expiry_ms: now_ms() + 60_000, consecutive_failures: 1, last_error: None },
        );
        let (_, body) = call(state, "GET", "/v0/management/cooldowns", Some("correct-admin-key")).await;
        assert_eq!(body, serde_json::json!([]));
    }

    #[tokio::test]
    async fn delete_all_requires_admin() {
        let mut keys = HashMap::new();
        keys.insert(
            "limited".to_string(),
            crate::state::KeyConfig { secret: "limited-secret".to_string(), ..Default::default() },
        );
        let state = AppState::new(Some("correct-admin-key".to_string()), keys);
        let (status, _) = call(state, "DELETE", "/v0/management/cooldowns", Some("limited-secret")).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn delete_all_clears_every_cooldown() {
        let state = admin_state();
        state.inner.cooldowns.write().unwrap().insert(
            ("prov-a".to_string(), "model-a".to_string()),
            CooldownEntry { expiry_ms: now_ms() + 60_000, consecutive_failures: 1, last_error: None },
        );
        let (status, body) = call(state.clone(), "DELETE", "/v0/management/cooldowns", Some("correct-admin-key")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["success"], true);
        assert!(state.inner.cooldowns.read().unwrap().is_empty());
    }

    #[tokio::test]
    async fn delete_by_provider_only_removes_that_providers_entries() {
        let state = admin_state();
        {
            let mut c = state.inner.cooldowns.write().unwrap();
            c.insert(
                ("prov-a".to_string(), "model-a".to_string()),
                CooldownEntry { expiry_ms: now_ms() + 60_000, consecutive_failures: 1, last_error: None },
            );
            c.insert(
                ("prov-b".to_string(), "model-b".to_string()),
                CooldownEntry { expiry_ms: now_ms() + 60_000, consecutive_failures: 1, last_error: None },
            );
        }
        let (status, _) = call(state.clone(), "DELETE", "/v0/management/cooldowns/prov-a", Some("correct-admin-key")).await;
        assert_eq!(status, StatusCode::OK);
        let remaining = state.inner.cooldowns.read().unwrap();
        assert!(!remaining.contains_key(&("prov-a".to_string(), "model-a".to_string())));
        assert!(remaining.contains_key(&("prov-b".to_string(), "model-b".to_string())));
    }

    #[tokio::test]
    async fn delete_by_provider_and_model_only_removes_that_pair() {
        let state = admin_state();
        {
            let mut c = state.inner.cooldowns.write().unwrap();
            c.insert(
                ("prov-a".to_string(), "model-a".to_string()),
                CooldownEntry { expiry_ms: now_ms() + 60_000, consecutive_failures: 1, last_error: None },
            );
            c.insert(
                ("prov-a".to_string(), "model-b".to_string()),
                CooldownEntry { expiry_ms: now_ms() + 60_000, consecutive_failures: 1, last_error: None },
            );
        }
        let (status, _) = call(
            state.clone(),
            "DELETE",
            "/v0/management/cooldowns/prov-a?model=model-a",
            Some("correct-admin-key"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let remaining = state.inner.cooldowns.read().unwrap();
        assert!(!remaining.contains_key(&("prov-a".to_string(), "model-a".to_string())));
        assert!(remaining.contains_key(&("prov-a".to_string(), "model-b".to_string())));
    }
}
