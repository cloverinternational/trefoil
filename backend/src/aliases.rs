//! Ported contract: `/v0/management/aliases/*` and `/v0/management/models*`
//! CRUD from Plexus (`packages/backend/src/routes/management/config.ts`, the
//! "Model Aliases" section, mounted under the admin-only scope in
//! `packages/backend/src/routes/management.ts`).
//!
//! Scoped to the load-bearing subset of `ModelConfigSchema`
//! (`packages/backend/src/config.ts`) — see `state::ModelConfig` for the
//! fields covered and which round-trip as opaque JSON.
//!
//! All routes require the `Admin` principal (403 for a limited key, 401 for
//! no/bad credential), same as `backend/src/keys.rs` and `providers.rs`.

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json, Router,
};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

use crate::auth::{resolve_principal, Principal};
use crate::state::{AppState, ModelConfig};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/v0/management/aliases", axum::routing::get(list_aliases))
        .route(
            "/v0/management/aliases/*slug",
            axum::routing::get(get_alias)
                .put(put_alias)
                .patch(patch_alias),
        )
        .route(
            "/v0/management/models",
            axum::routing::delete(delete_all_aliases),
        )
        .route(
            "/v0/management/models/*slug",
            axum::routing::delete(delete_alias),
        )
}

enum ApiError {
    Auth(StatusCode, &'static str, &'static str),
    Simple(StatusCode, String),
}

impl ApiError {
    fn unauthorized() -> Self {
        Self::Auth(StatusCode::UNAUTHORIZED, "Unauthorized", "auth_error")
    }
    fn forbidden() -> Self {
        Self::Auth(
            StatusCode::FORBIDDEN,
            "Admin privileges required",
            "forbidden",
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self {
            ApiError::Auth(status, message, error_type) => {
                let body = serde_json::json!({
                    "error": { "message": message, "type": error_type, "code": status.as_u16() }
                });
                (status, Json(body)).into_response()
            }
            ApiError::Simple(status, message) => {
                let body = serde_json::json!({ "error": message });
                (status, Json(body)).into_response()
            }
        }
    }
}

fn require_admin(state: &AppState, headers: &HeaderMap) -> Result<(), ApiError> {
    let provided_key = headers.get("x-admin-key").and_then(|v| v.to_str().ok());
    match resolve_principal(state, provided_key) {
        Some(Principal::Admin) => Ok(()),
        Some(Principal::Limited { .. }) => Err(ApiError::forbidden()),
        None => Err(ApiError::unauthorized()),
    }
}

/// Mirrors `assertNoAliasRefCycles`: detects `alias` reference cycles
/// (through canonical slugs and `additional_aliases` nicknames) in the
/// full alias set that would result from applying this write.
fn assert_no_alias_ref_cycles(models: &HashMap<String, ModelConfig>) -> Result<(), String> {
    let mut canonical_by_slug: HashMap<String, String> = HashMap::new();
    for (slug, config) in models {
        canonical_by_slug
            .entry(slug.clone())
            .or_insert_with(|| slug.clone());
        for nickname in &config.additional_aliases {
            canonical_by_slug
                .entry(nickname.clone())
                .or_insert_with(|| slug.clone());
        }
    }

    let referenced_aliases = |slug: &str| -> Vec<String> {
        let Some(model) = models.get(slug) else {
            return Vec::new();
        };
        let mut refs = Vec::new();
        for group in &model.target_groups {
            for target in &group.targets {
                if target.enabled {
                    if let Some(alias) = &target.alias {
                        refs.push(
                            canonical_by_slug
                                .get(alias)
                                .cloned()
                                .unwrap_or_else(|| alias.clone()),
                        );
                    }
                }
            }
        }
        refs
    };

    // Plexus walks its config record in key order; a HashMap has none, so
    // walk sorted keys to report the same, reproducible cycle path.
    let mut start_slugs: Vec<&String> = models.keys().collect();
    start_slugs.sort();
    for start_slug in start_slugs {
        let mut path: Vec<String> = Vec::new();
        let mut visiting: HashSet<String> = HashSet::new();

        fn walk(
            slug: &str,
            path: &mut Vec<String>,
            visiting: &mut HashSet<String>,
            referenced_aliases: &dyn Fn(&str) -> Vec<String>,
        ) -> Result<(), String> {
            if visiting.contains(slug) {
                let cycle_start = path.iter().position(|s| s == slug).unwrap_or(0);
                let mut cycle: Vec<String> = path[cycle_start..].to_vec();
                cycle.push(slug.to_string());
                return Err(format!(
                    "Alias reference cycle detected: {}",
                    cycle.join(" -> ")
                ));
            }
            visiting.insert(slug.to_string());
            path.push(slug.to_string());
            for r in referenced_aliases(slug) {
                walk(&r, path, visiting, referenced_aliases)?;
            }
            path.pop();
            visiting.remove(slug);
            Ok(())
        }

        walk(start_slug, &mut path, &mut visiting, &referenced_aliases)?;
    }
    Ok(())
}

async fn list_aliases(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(e) = require_admin(&state, &headers) {
        return e.into_response();
    }
    let aliases = state.inner.aliases.read().unwrap();
    Json(aliases.clone()).into_response()
}

async fn get_alias(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(slug): Path<String>,
) -> Response {
    if let Err(e) = require_admin(&state, &headers) {
        return e.into_response();
    }
    let aliases = state.inner.aliases.read().unwrap();
    match aliases.get(&slug) {
        Some(config) => {
            let mut value = serde_json::to_value(config).unwrap();
            value["slug"] = Value::String(slug);
            Json(value).into_response()
        }
        None => ApiError::Simple(StatusCode::NOT_FOUND, format!("Alias '{slug}' not found"))
            .into_response(),
    }
}

/// `ModelConfigSchema.safeParse` → repository round trip; `Err` is the 400.
fn validate_alias(body: &Value) -> Result<ModelConfig, Response> {
    let parsed = crate::alias_view::model_config().safe_parse(body).map_err(|details| {
        (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "Validation failed", "details": details}))).into_response()
    })?;
    Ok(crate::alias_view::from_view(crate::alias_view::repository_view(&parsed)))
}

async fn save_alias(state: &AppState, slug: &str, config: ModelConfig) -> Response {
    {
        let aliases = state.inner.aliases.read().unwrap();
        let mut candidate = aliases.clone();
        candidate.insert(slug.to_owned(), config.clone());
        if let Err(message) = assert_no_alias_ref_cycles(&candidate) {
            crate::system_logs::event(state,"error",format!("Failed to save model alias '{slug}' {message}"),[("module".into(),serde_json::json!("config")),("stack".into(),serde_json::json!(format!("Error: {message}")))]);
            return ApiError::Simple(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error".to_string()).into_response();
        }
    }
    if let Some(pool) = &state.inner.database {
        if crate::database::upsert_alias(pool, slug, &config).await.is_err() {
            return ApiError::Simple(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error".to_string()).into_response();
        }
    }
    state.inner.aliases.write().unwrap().insert(slug.to_owned(), config);
    Json(serde_json::json!({ "success": true, "slug": slug })).into_response()
}

async fn put_alias(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(slug): Path<String>,
    Json(raw): Json<Value>,
) -> Response {
    if let Err(e) = require_admin(&state, &headers) {
        return e.into_response();
    }
    match validate_alias(&raw) {
        Ok(config) => save_alias(&state, &slug, config).await,
        Err(response) => response,
    }
}

async fn patch_alias(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(slug): Path<String>,
    Json(raw): Json<Value>,
) -> Response {
    if let Err(e) = require_admin(&state, &headers) {
        return e.into_response();
    }
    let Some(body) = raw.as_object() else {
        return ApiError::Simple(StatusCode::BAD_REQUEST, "Object body is required".to_string()).into_response();
    };
    let Some(existing) = state.inner.aliases.read().unwrap().get(&slug).cloned() else {
        return ApiError::Simple(StatusCode::NOT_FOUND, format!("Alias '{slug}' not found")).into_response();
    };
    let mut merged = existing.public_view();
    let map = merged.as_object_mut().expect("alias view is an object");
    for (key, value) in body { map.insert(key.clone(), value.clone()); }
    match validate_alias(&merged) {
        Ok(config) => save_alias(&state, &slug, config).await,
        Err(response) => response,
    }
}

async fn delete_alias(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(slug): Path<String>,
) -> Response {
    if let Err(e) = require_admin(&state, &headers) {
        return e.into_response();
    }
    if let Some(pool) = &state.inner.database {
        if let Err(error) = crate::database::delete_alias(pool, &slug).await {
            return ApiError::Simple(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
                .into_response();
        }
    }
    state.inner.aliases.write().unwrap().remove(&slug);
    Json(serde_json::json!({ "success": true })).into_response()
}

async fn delete_all_aliases(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(e) = require_admin(&state, &headers) {
        return e.into_response();
    }
    if let Some(pool) = &state.inner.database {
        if let Err(error) = crate::database::delete_all_aliases(pool).await {
            return ApiError::Simple(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
                .into_response();
        }
    }
    let mut aliases = state.inner.aliases.write().unwrap();
    let deleted_count = aliases.len();
    aliases.clear();
    Json(serde_json::json!({ "success": true, "deletedCount": deleted_count })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn admin_state() -> AppState {
        AppState::new(Some("correct-admin-key".to_string()), HashMap::new())
    }

    async fn call(
        state: AppState,
        method: &str,
        uri: &str,
        admin_header: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut req = Request::builder().uri(uri).method(method);
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
    async fn put_creates_alias_from_legacy_flat_targets() {
        let state = admin_state();
        let (status, body) = call(
            state.clone(),
            "PUT",
            "/v0/management/aliases/my-alias",
            Some("correct-admin-key"),
            Some(serde_json::json!({
                "targets": [{ "provider": "test-provider", "model": "gpt-4" }]
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["success"], true);
        assert_eq!(body["slug"], "my-alias");

        let (status, body) = call(
            state,
            "GET",
            "/v0/management/aliases/my-alias",
            Some("correct-admin-key"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["target_groups"],
            serde_json::json!([{
                "name": "default",
                "selector": "random",
                "targets": [{ "provider": "test-provider", "model": "gpt-4", "enabled": true }]
            }])
        );
        assert_eq!(body["priority"], "selector");
        assert_eq!(body["sticky_session"], true);
    }

    #[tokio::test]
    async fn patch_merges_additional_aliases_without_touching_targets() {
        let state = admin_state();
        call(
            state.clone(),
            "PUT",
            "/v0/management/aliases/my-alias",
            Some("correct-admin-key"),
            Some(serde_json::json!({
                "targets": [{ "provider": "test-provider", "model": "gpt-4" }]
            })),
        )
        .await;

        let (status, body) = call(
            state.clone(),
            "PATCH",
            "/v0/management/aliases/my-alias",
            Some("correct-admin-key"),
            Some(serde_json::json!({ "additional_aliases": ["nick"] })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["success"], true);

        let (_, body) = call(
            state,
            "GET",
            "/v0/management/aliases/my-alias",
            Some("correct-admin-key"),
            None,
        )
        .await;
        assert_eq!(body["additional_aliases"], serde_json::json!(["nick"]));
        assert_eq!(body["target_groups"][0]["targets"][0]["model"], "gpt-4");
    }

    #[tokio::test]
    async fn put_rejects_duplicate_target() {
        let (status, body) = call(
            admin_state(),
            "PUT",
            "/v0/management/aliases/dup-alias",
            Some("correct-admin-key"),
            Some(serde_json::json!({
                "target_groups": [{
                    "name": "g",
                    "selector": "random",
                    "targets": [
                        { "provider": "p", "model": "m" },
                        { "provider": "p", "model": "m" }
                    ]
                }]
            })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["details"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["message"] == "Duplicate target 'p/m' is not allowed"));
    }

    #[tokio::test]
    async fn put_rejects_target_with_neither_alias_nor_provider_model() {
        let (status, body) = call(
            admin_state(),
            "PUT",
            "/v0/management/aliases/bad-target",
            Some("correct-admin-key"),
            Some(serde_json::json!({
                "target_groups": [{ "name": "g", "selector": "random", "targets": [{}] }]
            })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["details"].as_array().unwrap().iter().any(|d| {
            d["message"] == "A target must specify either 'alias' or both 'provider' and 'model', but not both"
        }));
    }

    #[tokio::test]
    async fn put_rejects_invalid_selector() {
        let (status, body) = call(
            admin_state(),
            "PUT",
            "/v0/management/aliases/no-selector",
            Some("correct-admin-key"),
            Some(serde_json::json!({
                "target_groups": [{
                    "name": "g",
                    "selector": "not-a-selector",
                    "targets": [{ "provider": "p", "model": "m" }]
                }]
            })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["details"][0]["code"], "invalid_value");
    }

    #[tokio::test]
    async fn put_rejects_duplicate_additional_aliases() {
        let (status, body) = call(
            admin_state(),
            "PUT",
            "/v0/management/aliases/dup-add",
            Some("correct-admin-key"),
            Some(serde_json::json!({
                "additional_aliases": ["x", "x"],
                "target_groups": [{
                    "name": "g",
                    "selector": "random",
                    "targets": [{ "provider": "p", "model": "m" }]
                }]
            })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["details"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["message"] == "Duplicate additional alias 'x' is not allowed"));
    }

    #[tokio::test]
    async fn put_detects_alias_reference_cycle() {
        let state = admin_state();
        let (status, _) = call(
            state.clone(),
            "PUT",
            "/v0/management/aliases/alias-a",
            Some("correct-admin-key"),
            Some(serde_json::json!({
                "target_groups": [{ "name": "g", "selector": "random", "targets": [{ "alias": "alias-b" }] }]
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (status, _) = call(
            state.clone(),
            "PUT",
            "/v0/management/aliases/alias-b",
            Some("correct-admin-key"),
            Some(serde_json::json!({
                "target_groups": [{ "name": "g", "selector": "random", "targets": [{ "alias": "alias-a" }] }]
            })),
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);

        // alias-b must not have been persisted (cycle detected before save).
        let (status, _) = call(
            state,
            "GET",
            "/v0/management/aliases/alias-b",
            Some("correct-admin-key"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn delete_via_models_path_and_delete_all() {
        let state = admin_state();
        call(
            state.clone(),
            "PUT",
            "/v0/management/aliases/my-alias",
            Some("correct-admin-key"),
            Some(serde_json::json!({
                "targets": [{ "provider": "p", "model": "m" }]
            })),
        )
        .await;

        let (status, body) = call(
            state.clone(),
            "DELETE",
            "/v0/management/models/my-alias",
            Some("correct-admin-key"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["success"], true);

        let (status, _) = call(
            state.clone(),
            "GET",
            "/v0/management/aliases/my-alias",
            Some("correct-admin-key"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        call(
            state.clone(),
            "PUT",
            "/v0/management/aliases/another",
            Some("correct-admin-key"),
            Some(serde_json::json!({ "targets": [{ "provider": "p", "model": "m" }] })),
        )
        .await;
        let (status, body) = call(
            state,
            "DELETE",
            "/v0/management/models",
            Some("correct-admin-key"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["deletedCount"], 1);
    }

    #[tokio::test]
    async fn patch_nonexistent_alias_returns_404() {
        let (status, body) = call(
            admin_state(),
            "PATCH",
            "/v0/management/aliases/nope",
            Some("correct-admin-key"),
            Some(serde_json::json!({ "sticky_session": false })),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"], "Alias 'nope' not found");
    }

    #[tokio::test]
    async fn patch_non_object_body_returns_400() {
        let (status, body) = call(
            admin_state(),
            "PATCH",
            "/v0/management/aliases/whatever",
            Some("correct-admin-key"),
            Some(serde_json::json!([1, 2])),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "Object body is required");
    }

    #[tokio::test]
    async fn limited_key_gets_403() {
        let mut keys = HashMap::new();
        keys.insert(
            "limited".to_string(),
            crate::state::KeyConfig {
                secret: "limited-secret".to_string(),
                ..Default::default()
            },
        );
        let state = AppState::new(Some("correct-admin-key".to_string()), keys);
        let (status, _) = call(
            state,
            "GET",
            "/v0/management/aliases",
            Some("limited-secret"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn missing_credential_gets_401() {
        let (status, _) = call(admin_state(), "GET", "/v0/management/aliases", None, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
}
