//! Initial management config surface: status, timeout, and MCP enablement.
//!
//! Ports `GET /v0/management/config/status` and `GET`/`PATCH
//! `/v0/management/config/timeout` and `/v0/management/config/mcp-enabled`
//! from Plexus. MCP server management is not yet ported, so
//! `mcpServerCount` is currently zero.

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
        .route(
            "/v0/management/config/mcp-enabled",
            axum::routing::get(get_mcp_enabled).patch(patch_mcp_enabled),
        )
        .route(
            "/v0/management/config/exploration-rate",
            axum::routing::get(get_exploration_rate).patch(patch_exploration_rate),
        )
        .route(
            "/v0/management/config/failover",
            axum::routing::get(get_failover).patch(patch_failover),
        )
        .route(
            "/v0/management/config/cooldown",
            axum::routing::get(get_cooldown).patch(patch_cooldown),
        )
}

fn config_value(state: &AppState, key: &str, default: serde_json::Value) -> serde_json::Value {
    state
        .inner
        .settings
        .read()
        .unwrap()
        .get(key)
        .cloned()
        .unwrap_or(default)
}

async fn get_failover(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    let retryable_status_codes: Vec<i32> = (100..=599)
        .filter(|code| !(200..=299).contains(code) && *code != 413 && *code != 422)
        .collect();
    Json(serde_json::json!({
        "enabled": config_value(&state, "failover.enabled", serde_json::json!(true)),
        "retryableStatusCodes": config_value(
            &state,
            "failover.retryableStatusCodes",
            serde_json::json!(retryable_status_codes),
        ),
        "retryableErrors": config_value(
            &state,
            "failover.retryableErrors",
            serde_json::json!(["ECONNREFUSED", "ETIMEDOUT", "ENOTFOUND"]),
        ),
    }))
    .into_response()
}

async fn patch_failover(
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
            Json(serde_json::json!({"error":"Object body is required"})),
        )
            .into_response();
    };
    let mut entries = serde_json::Map::new();
    for key in ["enabled", "retryableStatusCodes", "retryableErrors"] {
        if let Some(value) = object.get(key) {
            entries.insert(format!("failover.{key}"), value.clone());
        }
    }
    if let Some(pool) = &state.inner.database {
        if crate::database::upsert_settings_bulk(pool, &entries)
            .await
            .is_err()
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error":"Internal server error"})),
            )
                .into_response();
        }
    }
    state.inner.settings.write().unwrap().extend(entries);
    get_failover(State(state), headers).await
}

async fn get_cooldown(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    Json(serde_json::json!({
        "initialMinutes": config_value(
            &state,
            "cooldown.initialMinutes",
            serde_json::json!(2),
        ),
        "maxMinutes": config_value(
            &state,
            "cooldown.maxMinutes",
            serde_json::json!(300),
        ),
    }))
    .into_response()
}

async fn patch_cooldown(
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
            Json(serde_json::json!({"error":"Object body is required"})),
        )
            .into_response();
    };
    let mut entries = serde_json::Map::new();
    for key in ["initialMinutes", "maxMinutes"] {
        if let Some(value) = object.get(key) {
            let Some(number) =
                js_number(value).filter(|number| number.is_finite() && *number >= 0.1)
            else {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": format!("{key} must be at least 0.1")
                    })),
                )
                    .into_response();
            };
            entries.insert(format!("cooldown.{key}"), serde_json::json!(number));
        }
    }
    if let Some(pool) = &state.inner.database {
        if crate::database::upsert_settings_bulk(pool, &entries)
            .await
            .is_err()
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error":"Internal server error"})),
            )
                .into_response();
        }
    }
    state.inner.settings.write().unwrap().extend(entries);
    get_cooldown(State(state), headers).await
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

fn mcp_enabled(state: &AppState) -> bool {
    state
        .inner
        .settings
        .read()
        .unwrap()
        .get("mcpEnabled")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true)
}

async fn get_mcp_enabled(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    Json(serde_json::json!({ "enabled": mcp_enabled(&state) })).into_response()
}

async fn patch_mcp_enabled(
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
    let Some(enabled) = object.get("enabled").and_then(serde_json::Value::as_bool) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "enabled must be a boolean" })),
        )
            .into_response();
    };

    if let Some(pool) = &state.inner.database {
        let entries =
            serde_json::Map::from_iter([("mcpEnabled".to_string(), serde_json::json!(enabled))]);
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
        .insert("mcpEnabled".into(), serde_json::json!(enabled));
    Json(serde_json::json!({ "enabled": enabled })).into_response()
}

const EXPLORATION_FIELDS: [&str; 3] = [
    "performanceExplorationRate",
    "latencyExplorationRate",
    "e2ePerformanceExplorationRate",
];

fn js_number(value: &serde_json::Value) -> Option<f64> {
    match value {
        serde_json::Value::Number(number) => number.as_f64(),
        serde_json::Value::String(value) => {
            let value = value.trim();
            if value.is_empty() {
                Some(0.0)
            } else {
                value.parse().ok()
            }
        }
        serde_json::Value::Bool(value) => Some(if *value { 1.0 } else { 0.0 }),
        serde_json::Value::Null => Some(0.0),
        _ => None,
    }
}

fn exploration_value(state: &AppState, key: &str) -> f64 {
    state
        .inner
        .settings
        .read()
        .unwrap()
        .get(key)
        .and_then(serde_json::Value::as_f64)
        .unwrap_or(0.05)
}

fn exploration_body(state: &AppState) -> serde_json::Value {
    serde_json::json!({
        "performanceExplorationRate": exploration_value(state, "performanceExplorationRate"),
        "latencyExplorationRate": exploration_value(state, "latencyExplorationRate"),
        "e2ePerformanceExplorationRate": exploration_value(state, "e2ePerformanceExplorationRate"),
    })
}

async fn get_exploration_rate(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    Json(exploration_body(&state)).into_response()
}

async fn patch_exploration_rate(
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

    let mut updates = serde_json::Map::new();
    for key in EXPLORATION_FIELDS {
        let Some(raw) = object.get(key) else {
            continue;
        };
        let Some(value) =
            js_number(raw).filter(|value| value.is_finite() && (0.0..=1.0).contains(value))
        else {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": format!("{key} must be a number between 0 and 1")
                })),
            )
                .into_response();
        };
        updates.insert(key.to_string(), serde_json::json!(value));
    }

    if !updates.is_empty() {
        if let Some(pool) = &state.inner.database {
            if crate::database::upsert_settings_bulk(pool, &updates)
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
        let mut settings = state.inner.settings.write().unwrap();
        for (key, value) in updates {
            settings.insert(key, value);
        }
    }
    Json(exploration_body(&state)).into_response()
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

    #[tokio::test]
    async fn mcp_enabled_defaults_true_and_patch_round_trips() {
        let state = fixture();
        let get = router()
            .with_state(state.clone())
            .oneshot(
                Request::get("/v0/management/config/mcp-enabled")
                    .header("x-admin-key", "admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let get_body: serde_json::Value =
            serde_json::from_slice(&get.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(get_body, serde_json::json!({ "enabled": true }));

        let patch = router()
            .with_state(state)
            .oneshot(
                Request::patch("/v0/management/config/mcp-enabled")
                    .header("x-admin-key", "admin")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"enabled":false}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        let patch_body: serde_json::Value =
            serde_json::from_slice(&patch.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(patch_body, serde_json::json!({ "enabled": false }));
    }

    #[tokio::test]
    async fn mcp_enabled_patch_rejects_missing_non_boolean_and_array() {
        for (body, expected_error) in [
            (r#"{}"#, "enabled must be a boolean"),
            (r#"{"enabled":"false"}"#, "enabled must be a boolean"),
            (r#"{"enabled":null}"#, "enabled must be a boolean"),
            (r#"[]"#, "Object body is required"),
        ] {
            let response = router()
                .with_state(fixture())
                .oneshot(
                    Request::patch("/v0/management/config/mcp-enabled")
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

    #[tokio::test]
    async fn exploration_rate_defaults_and_coercions_match_javascript_number() {
        let state = fixture();
        let get = router()
            .with_state(state.clone())
            .oneshot(
                Request::get("/v0/management/config/exploration-rate")
                    .header("x-admin-key", "admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let get_body: serde_json::Value =
            serde_json::from_slice(&get.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(
            get_body,
            serde_json::json!({
                "performanceExplorationRate": 0.05,
                "latencyExplorationRate": 0.05,
                "e2ePerformanceExplorationRate": 0.05,
            })
        );

        let patch = router()
            .with_state(state)
            .oneshot(
                Request::patch("/v0/management/config/exploration-rate")
                    .header("x-admin-key", "admin")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"performanceExplorationRate":"0.2","latencyExplorationRate":true,"e2ePerformanceExplorationRate":null}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let patch_body: serde_json::Value =
            serde_json::from_slice(&patch.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(
            patch_body,
            serde_json::json!({
                "performanceExplorationRate": 0.2,
                "latencyExplorationRate": 1.0,
                "e2ePerformanceExplorationRate": 0.0,
            })
        );
    }

    #[tokio::test]
    async fn exploration_rate_rejects_out_of_range_and_non_object() {
        for (body, expected_error) in [
            (
                r#"{"performanceExplorationRate":1.1}"#,
                "performanceExplorationRate must be a number between 0 and 1",
            ),
            (
                r#"{"latencyExplorationRate":-0.1}"#,
                "latencyExplorationRate must be a number between 0 and 1",
            ),
            (r#"[]"#, "Object body is required"),
        ] {
            let response = router()
                .with_state(fixture())
                .oneshot(
                    Request::patch("/v0/management/config/exploration-rate")
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

    #[tokio::test]
    async fn failover_defaults_and_patch_round_trip_recognized_fields() {
        let state = fixture();
        let get = router()
            .with_state(state.clone())
            .oneshot(
                Request::get("/v0/management/config/failover")
                    .header("x-admin-key", "admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let get_body: serde_json::Value =
            serde_json::from_slice(&get.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(get_body["enabled"], true);
        assert_eq!(
            get_body["retryableErrors"],
            serde_json::json!(["ECONNREFUSED", "ETIMEDOUT", "ENOTFOUND"])
        );
        assert_eq!(
            get_body["retryableStatusCodes"].as_array().unwrap().len(),
            398
        );
        assert!(!get_body["retryableStatusCodes"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!(413)));

        let patch = router()
            .with_state(state)
            .oneshot(
                Request::patch("/v0/management/config/failover")
                    .header("x-admin-key", "admin")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"enabled":false,"retryableStatusCodes":[503],"ignored":"value"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&patch.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(body["enabled"], false);
        assert_eq!(body["retryableStatusCodes"], serde_json::json!([503]));
        assert!(body.get("ignored").is_none());
    }

    #[tokio::test]
    async fn cooldown_defaults_coercion_and_validation_match_plexus() {
        let state = fixture();
        let get = router()
            .with_state(state.clone())
            .oneshot(
                Request::get("/v0/management/config/cooldown")
                    .header("x-admin-key", "admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&get.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(
            body,
            serde_json::json!({ "initialMinutes": 2, "maxMinutes": 300 })
        );

        let patch = router()
            .with_state(state.clone())
            .oneshot(
                Request::patch("/v0/management/config/cooldown")
                    .header("x-admin-key", "admin")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"initialMinutes":"0.5","maxMinutes":true}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&patch.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(
            body,
            serde_json::json!({ "initialMinutes": 0.5, "maxMinutes": 1.0 })
        );

        let invalid = router()
            .with_state(state)
            .oneshot(
                Request::patch("/v0/management/config/cooldown")
                    .header("x-admin-key", "admin")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"initialMinutes":null}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
        let body: serde_json::Value =
            serde_json::from_slice(&invalid.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(body["error"], "initialMinutes must be at least 0.1");
    }
}
