//! Ephemeral management logging controls.
//!
//! Runtime levels apply to both the recent-log buffer and live stream.

use axum::{
    extract::State,
    http::HeaderMap,
    response::{IntoResponse, Response},
    Json, Router,
};
use std::sync::{OnceLock, RwLock};

use crate::{
    auth::{resolve_principal, Principal},
    AppState,
};

static LEVEL: OnceLock<RwLock<String>> = OnceLock::new();
static MODULES: OnceLock<RwLock<Vec<String>>> = OnceLock::new();
static STARTUP_LEVEL: OnceLock<String> = OnceLock::new();
const SUPPORTED: &[&str] = &["error", "warn", "info", "debug", "verbose", "silly"];

fn startup_level() -> &'static str {
    STARTUP_LEVEL
        .get_or_init(|| {
            std::env::var("LOG_LEVEL")
                .ok()
                .map(|value| value.trim().to_lowercase())
                .filter(|value| SUPPORTED.contains(&value.as_str()))
                .or_else(|| {
                    (std::env::var("DEBUG").as_deref() == Ok("true")).then(|| "debug".into())
                })
                .unwrap_or_else(|| "info".into())
        })
        .as_str()
}

fn level() -> &'static RwLock<String> {
    LEVEL.get_or_init(|| RwLock::new(startup_level().into()))
}
fn modules() -> &'static RwLock<Vec<String>> {
    MODULES.get_or_init(|| RwLock::new(Vec::new()))
}

/// Mirror Winston severity ordering. Module filters are console-only in the
/// reference and must not suppress the management stream.
pub fn enabled(event_level: &str) -> bool {
    level_enabled(&level().read().unwrap(), event_level)
}

fn level_enabled(configured: &str, event_level: &str) -> bool {
    match (
        SUPPORTED.iter().position(|v| *v == configured),
        SUPPORTED.iter().position(|v| *v == event_level),
    ) {
        (Some(limit), Some(event)) => event <= limit,
        _ => false,
    }
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/v0/management/logging/level",
            axum::routing::get(get_level)
                .put(put_level)
                .delete(delete_level),
        )
        .route(
            "/v0/management/logging/modules",
            axum::routing::get(get_modules)
                .put(put_modules)
                .delete(delete_modules),
        )
}

#[allow(clippy::result_large_err)]
fn admin(state: &AppState, headers: &HeaderMap) -> Result<(), Response> {
    match resolve_principal(
        state,
        headers.get("x-admin-key").and_then(|v| v.to_str().ok()),
    ) {
        Some(Principal::Admin) => Ok(()),
        Some(Principal::Limited { .. }) => Err((
            axum::http::StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": { "message": "Admin privileges required", "type": "forbidden", "code": 403 }
            })),
        )
            .into_response()),
        None => Err((
            axum::http::StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "error": { "message": "Unauthorized", "type": "auth_error", "code": 401 }
            })),
        )
            .into_response()),
    }
}

fn level_body() -> serde_json::Value {
    serde_json::json!({"level":level().read().unwrap().clone(),"startupLevel":startup_level(),"supportedLevels":SUPPORTED,"ephemeral":true})
}
async fn get_level(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(r) = admin(&state, &headers) {
        return r;
    }
    Json(level_body()).into_response()
}
async fn put_level(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    if let Err(r) = admin(&state, &headers) {
        return r;
    }
    let Some(value) = body.get("level").and_then(|v| v.as_str()) else {
        return (axum::http::StatusCode::BAD_REQUEST,Json(serde_json::json!({"error":"Invalid request body. Expected: { level: string }","supportedLevels":SUPPORTED}))).into_response();
    };
    if value.trim().is_empty() {
        return (axum::http::StatusCode::BAD_REQUEST,Json(serde_json::json!({"error":"Invalid request body. Expected: { level: string }","supportedLevels":SUPPORTED}))).into_response();
    }
    let normalized = value.trim().to_lowercase();
    if !SUPPORTED.contains(&normalized.as_str()) {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!(
                    "Invalid log level '{value}'. Supported levels: {}",
                    SUPPORTED.join(", ")
                ),
                "supportedLevels": SUPPORTED
            })),
        )
            .into_response();
    }
    *level().write().unwrap() = normalized;
    Json(level_body()).into_response()
}
async fn delete_level(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(r) = admin(&state, &headers) {
        return r;
    }
    *level().write().unwrap() = startup_level().into();
    Json(level_body()).into_response()
}
async fn get_modules(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(r) = admin(&state, &headers) {
        return r;
    }
    Json(serde_json::json!({"modules":modules().read().unwrap().clone()})).into_response()
}
async fn put_modules(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    if let Err(r) = admin(&state, &headers) {
        return r;
    }
    let Some(values) = body.get("modules").and_then(|v| v.as_array()) else {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":"Invalid request body. Expected: { modules: string[] }"})),
        )
            .into_response();
    };
    if !values
        .iter()
        .all(|value| value.as_str().is_some_and(|value| !value.is_empty()))
    {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":"Invalid request body. Expected: { modules: string[] }"})),
        )
            .into_response();
    }
    let mut values_out = Vec::new();
    for value in values.iter().filter_map(serde_json::Value::as_str) {
        if !values_out.iter().any(|existing| existing == value) {
            values_out.push(value.to_owned());
        }
    }
    *modules().write().unwrap() = values_out.clone();
    Json(serde_json::json!({"modules":values_out})).into_response()
}
async fn delete_modules(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(r) = admin(&state, &headers) {
        return r;
    }
    modules().write().unwrap().clear();
    Json(serde_json::json!({"modules":[]})).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::KeyConfig;
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
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

    async fn request(
        method: &str,
        path: &str,
        body: Option<&str>,
    ) -> (StatusCode, serde_json::Value) {
        let mut builder = Request::builder()
            .method(method)
            .uri(path)
            .header("x-admin-key", "admin");
        if body.is_some() {
            builder = builder.header("content-type", "application/json");
        }
        let response = router()
            .with_state(fixture())
            .oneshot(
                builder
                    .body(Body::from(body.unwrap_or_default().to_owned()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let json =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        (status, json)
    }

    #[tokio::test]
    async fn level_and_module_controls_round_trip_and_reset() {
        let (_, reset) = request("DELETE", "/v0/management/logging/level", None).await;
        assert_eq!(reset["level"], reset["startupLevel"]);
        assert_eq!(reset["supportedLevels"], serde_json::json!(SUPPORTED));

        let (_, changed) = request(
            "PUT",
            "/v0/management/logging/level",
            Some(r#"{"level":" DEBUG "}"#),
        )
        .await;
        assert_eq!(changed["level"], "debug");

        let (status, invalid) = request(
            "PUT",
            "/v0/management/logging/level",
            Some(r#"{"level":"trace"}"#),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(invalid["supportedLevels"], serde_json::json!(SUPPORTED));

        let (_, modules_body) = request(
            "PUT",
            "/v0/management/logging/modules",
            Some(r#"{"modules":["routing","routing","quota"]}"#),
        )
        .await;
        assert_eq!(
            modules_body,
            serde_json::json!({ "modules": ["routing", "quota"] })
        );

        let _ = request("DELETE", "/v0/management/logging/level", None).await;
        let (_, cleared) = request("DELETE", "/v0/management/logging/modules", None).await;
        assert_eq!(cleared, serde_json::json!({ "modules": [] }));
    }

    #[tokio::test]
    async fn logging_controls_require_admin() {
        for (credential, expected) in [
            (Some("secret"), StatusCode::FORBIDDEN),
            (None, StatusCode::UNAUTHORIZED),
        ] {
            let mut request = Request::get("/v0/management/logging/level");
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

#[cfg(test)]
mod severity_tests {
    #[test]
    fn filtering_matches_winston_order() {
        for (limit, configured) in super::SUPPORTED.iter().enumerate() {
            for (severity, event) in super::SUPPORTED.iter().enumerate() {
                assert_eq!(super::level_enabled(configured, event), severity <= limit);
            }
        }
        assert!(!super::level_enabled("info", "unknown"));
    }
}
