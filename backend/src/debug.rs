//! Ported contract: `GET`/`PATCH /v0/management/debug` from Plexus
//! (`packages/backend/src/routes/management/debug.ts`).
//!
//! Only the global/per-key/per-alias/per-provider toggle bookkeeping is
//! ported (mirroring `DebugManager`'s in-memory state) — actual trace
//! capture and the `/v0/management/debug/logs*` sub-routes are not, since
//! they require a `UsageStorageService` fed by live inference traffic that
//! Trefoil doesn't have yet.

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
        "/v0/management/debug",
        axum::routing::get(get_debug).patch(patch_debug),
    )
}

fn auth_error_401() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({
            "error": { "message": "Unauthorized", "type": "auth_error", "code": 401 }
        })),
    )
        .into_response()
}

fn forbidden_403() -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(serde_json::json!({
            "error": { "message": "Admin privileges required", "type": "forbidden", "code": 403 }
        })),
    )
        .into_response()
}

fn sorted(set: &std::collections::HashSet<String>) -> Vec<String> {
    let mut v: Vec<String> = set.iter().cloned().collect();
    v.sort();
    v
}

async fn get_debug(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let provided_key = headers.get("x-admin-key").and_then(|v| v.to_str().ok());
    let Some(principal) = resolve_principal(&state, provided_key) else {
        return auth_error_401();
    };

    let enabled_global = *state.inner.debug_enabled_global.read().unwrap();
    let enabled_keys = sorted(&state.inner.debug_enabled_keys.read().unwrap());
    let enabled_aliases = sorted(&state.inner.debug_enabled_aliases.read().unwrap());
    let providers_set = sorted(&state.inner.debug_enabled_providers.read().unwrap());
    let providers: Value = if providers_set.is_empty() {
        Value::Null
    } else {
        serde_json::to_value(providers_set).unwrap()
    };

    match principal {
        Principal::Limited { key_name, .. } => {
            let enabled_for_key = enabled_global || enabled_keys.contains(&key_name);
            let keys: Vec<String> = if enabled_keys.contains(&key_name) {
                vec![key_name.clone()]
            } else {
                vec![]
            };
            Json(serde_json::json!({
                "enabled": enabled_for_key,
                "enabledGlobal": enabled_global,
                "enabledForKey": enabled_for_key,
                "providers": providers,
                "keys": keys,
                "aliases": enabled_aliases,
            }))
            .into_response()
        }
        Principal::Admin => Json(serde_json::json!({
            "enabled": enabled_global,
            "enabledGlobal": enabled_global,
            "enabledKeys": enabled_keys,
            "providers": providers,
            "keys": enabled_keys,
            "aliases": enabled_aliases,
        }))
        .into_response(),
    }
}

fn json_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Validates a `string[] | null | undefined` field, returning a 400 detail
/// entry on type mismatch, or `Ok(None)` when the field was absent/null
/// (meaning "no change" vs Plexus's `undefined` vs "clear" semantics are
/// distinguished by the caller via `Option<Option<..>>`).
fn parse_string_array_field(
    field: &str,
    value: &Option<Value>,
) -> Result<Option<Option<Vec<String>>>, Value> {
    match value {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(None)),
        Some(Value::Array(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for (i, item) in items.iter().enumerate() {
                match item.as_str() {
                    Some(s) => out.push(s.to_string()),
                    None => {
                        return Err(serde_json::json!({
                            "code": "invalid_type",
                            "expected": "string",
                            "path": [field, i],
                            "message": format!("Invalid input: expected string, received {}", json_type_name(item)),
                        }));
                    }
                }
            }
            Ok(Some(Some(out)))
        }
        Some(other) => Err(serde_json::json!({
            "code": "invalid_type",
            "expected": "array",
            "path": [field],
            "message": format!("Invalid input: expected array, received {}", json_type_name(other)),
        })),
    }
}

async fn patch_debug(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<Value>>,
) -> Response {
    let provided_key = headers.get("x-admin-key").and_then(|v| v.to_str().ok());
    let Some(principal) = resolve_principal(&state, provided_key) else {
        return auth_error_401();
    };
    if !matches!(principal, Principal::Admin) {
        return forbidden_403();
    }

    let raw = body
        .map(|Json(v)| v)
        .unwrap_or(Value::Object(Default::default()));
    let Some(obj) = raw.as_object() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "Invalid request body" })),
        )
            .into_response();
    };

    let mut details = Vec::new();

    // `obj.get(field)` distinguishes an absent field (None, "no change")
    // from an explicit `null` (Some(Value::Null), "clear") the same way
    // Zod's `.optional()` vs an explicit null value does.
    let enabled_field = obj.get("enabled");
    let enabled_value = match enabled_field {
        None => None,
        Some(v) => match v.as_bool() {
            Some(b) => Some(b),
            None => {
                details.push(serde_json::json!({
                    "code": "invalid_type",
                    "expected": "boolean",
                    "path": ["enabled"],
                    "message": format!("Invalid input: expected boolean, received {}", json_type_name(v)),
                }));
                None
            }
        },
    };

    let providers_result = parse_string_array_field("providers", &obj.get("providers").cloned());
    let keys_result = parse_string_array_field("keys", &obj.get("keys").cloned());
    let aliases_result = parse_string_array_field("aliases", &obj.get("aliases").cloned());
    for result in [&providers_result, &keys_result, &aliases_result] {
        if let Err(detail) = result {
            details.push(detail.clone());
        }
    }

    if !details.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "Invalid request body", "details": details })),
        )
            .into_response();
    }

    if let Some(enabled) = enabled_value {
        *state.inner.debug_enabled_global.write().unwrap() = enabled;
        crate::system_logs::event(
            &state,
            "warn",
            format!(
                "Debug mode (global) {}",
                if enabled { "enabled" } else { "disabled" }
            ),
            [
                ("module".into(), serde_json::json!("debug-manager")),
                ("functionName".into(), serde_json::json!("setEnabled")),
            ],
        );
    }
    if let Ok(Some(providers)) = providers_result {
        let mut set = state.inner.debug_enabled_providers.write().unwrap();
        set.clear();
        let mut values = Vec::new();
        for p in providers.unwrap_or_default() {
            let trimmed = p.trim();
            if !trimmed.is_empty() {
                set.insert(trimmed.to_string());
                values.push(trimmed.to_string());
            }
        }
        crate::system_logs::event(
            &state,
            "warn",
            format!(
                "Debug provider capture {}",
                if values.is_empty() {
                    "cleared".into()
                } else {
                    format!("set to: {}", values.join(", "))
                }
            ),
            [
                ("module".into(), serde_json::json!("debug-manager")),
                (
                    "functionName".into(),
                    serde_json::json!("setEnabledProviders"),
                ),
            ],
        );
    }
    if let Ok(Some(keys)) = keys_result {
        let mut set = state.inner.debug_enabled_keys.write().unwrap();
        set.clear();
        let mut values = Vec::new();
        for k in keys.unwrap_or_default() {
            let trimmed = k.trim();
            if !trimmed.is_empty() {
                set.insert(trimmed.to_string());
                values.push(trimmed.to_string());
            }
        }
        crate::system_logs::event(
            &state,
            "warn",
            format!(
                "Debug key capture {}",
                if values.is_empty() {
                    "cleared".into()
                } else {
                    format!("set to: {}", values.join(", "))
                }
            ),
            [
                ("module".into(), serde_json::json!("debug-manager")),
                ("functionName".into(), serde_json::json!("setEnabledKeys")),
            ],
        );
    }
    if let Ok(Some(aliases)) = aliases_result {
        let mut set = state.inner.debug_enabled_aliases.write().unwrap();
        set.clear();
        let mut values = Vec::new();
        for a in aliases.unwrap_or_default() {
            let trimmed = a.trim();
            if !trimmed.is_empty() {
                set.insert(trimmed.to_string());
                values.push(trimmed.to_string());
            }
        }
        crate::system_logs::event(
            &state,
            "warn",
            format!(
                "Debug alias capture {}",
                if values.is_empty() {
                    "cleared".into()
                } else {
                    format!("set to: {}", values.join(", "))
                }
            ),
            [
                ("module".into(), serde_json::json!("debug-manager")),
                (
                    "functionName".into(),
                    serde_json::json!("setEnabledAliases"),
                ),
            ],
        );
    }

    let enabled_global = *state.inner.debug_enabled_global.read().unwrap();
    let enabled_keys = sorted(&state.inner.debug_enabled_keys.read().unwrap());
    let enabled_aliases = sorted(&state.inner.debug_enabled_aliases.read().unwrap());
    let providers_set = sorted(&state.inner.debug_enabled_providers.read().unwrap());
    let providers: Value = if providers_set.is_empty() {
        Value::Null
    } else {
        serde_json::to_value(providers_set).unwrap()
    };

    Json(serde_json::json!({
        "enabled": enabled_global,
        "enabledGlobal": enabled_global,
        "enabledKeys": enabled_keys,
        "providers": providers,
        "keys": enabled_keys,
        "aliases": enabled_aliases,
    }))
    .into_response()
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
        let mut keys = HashMap::new();
        keys.insert(
            "test-key".to_string(),
            crate::state::KeyConfig {
                secret: "sk-test-secret".to_string(),
                ..Default::default()
            },
        );
        AppState::new(Some("correct-admin-key".to_string()), keys)
    }

    async fn call(
        state: AppState,
        method: &str,
        admin_header: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut req = Request::builder()
            .uri("/v0/management/debug")
            .method(method);
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
    async fn get_as_admin_default_state() {
        let (status, body) = call(admin_state(), "GET", Some("correct-admin-key"), None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            serde_json::json!({
                "enabled": false, "enabledGlobal": false, "enabledKeys": [],
                "providers": Value::Null, "keys": [], "aliases": []
            })
        );
    }

    #[tokio::test]
    async fn get_as_limited_default_state() {
        let (status, body) = call(admin_state(), "GET", Some("sk-test-secret"), None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            serde_json::json!({
                "enabled": false, "enabledGlobal": false, "enabledForKey": false,
                "providers": Value::Null, "keys": [], "aliases": []
            })
        );
    }

    #[tokio::test]
    async fn missing_credential_gets_401() {
        let (status, _) = call(admin_state(), "GET", None, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn patch_as_limited_gets_403() {
        let (status, _) = call(
            admin_state(),
            "PATCH",
            Some("sk-test-secret"),
            Some(serde_json::json!({ "enabled": true })),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn patch_sets_global_and_reflects_in_limited_get() {
        let state = admin_state();
        let (status, body) = call(
            state.clone(),
            "PATCH",
            Some("correct-admin-key"),
            Some(serde_json::json!({ "enabled": true })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["enabledGlobal"], true);

        let (_, body) = call(state, "GET", Some("sk-test-secret"), None).await;
        assert_eq!(body["enabledGlobal"], true);
        assert_eq!(body["enabledForKey"], true);
        assert_eq!(body["enabled"], true);
    }

    #[tokio::test]
    async fn patch_sets_providers_keys_aliases() {
        let state = admin_state();
        let (status, body) = call(
            state.clone(),
            "PATCH",
            Some("correct-admin-key"),
            Some(serde_json::json!({
                "providers": ["openai", "anthropic"],
                "keys": ["test-key"],
                "aliases": ["gpt-4"],
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["providers"],
            serde_json::json!(["anthropic", "openai"])
        );
        assert_eq!(body["keys"], serde_json::json!(["test-key"]));
        assert_eq!(body["aliases"], serde_json::json!(["gpt-4"]));

        let (_, body) = call(state, "GET", Some("sk-test-secret"), None).await;
        assert_eq!(body["keys"], serde_json::json!(["test-key"]));
        assert_eq!(body["enabledForKey"], true);
    }

    #[tokio::test]
    async fn patch_null_clears_providers_keys_aliases() {
        let state = admin_state();
        let _ = call(
            state.clone(),
            "PATCH",
            Some("correct-admin-key"),
            Some(serde_json::json!({ "providers": ["openai"], "keys": ["test-key"], "aliases": ["gpt-4"] })),
        )
        .await;
        let (_, body) = call(
            state,
            "PATCH",
            Some("correct-admin-key"),
            Some(serde_json::json!({ "providers": Value::Null, "keys": Value::Null, "aliases": Value::Null })),
        )
        .await;
        assert_eq!(body["providers"], Value::Null);
        assert_eq!(body["keys"], serde_json::json!([]));
        assert_eq!(body["aliases"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn patch_rejects_non_boolean_enabled() {
        let (status, body) = call(
            admin_state(),
            "PATCH",
            Some("correct-admin-key"),
            Some(serde_json::json!({ "enabled": "yes" })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "Invalid request body");
        assert_eq!(body["details"][0]["code"], "invalid_type");
        assert_eq!(body["details"][0]["path"], serde_json::json!(["enabled"]));
    }

    #[tokio::test]
    async fn patch_empty_body_is_a_noop() {
        let (status, body) = call(
            admin_state(),
            "PATCH",
            Some("correct-admin-key"),
            Some(serde_json::json!({})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["enabledGlobal"], false);
    }
}
