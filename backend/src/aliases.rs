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
use serde::Deserialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};

use crate::auth::{resolve_principal, Principal};
use crate::state::{AppState, ModelConfig, ModelTarget, ModelTargetGroup};

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
    Validation(Vec<ValidationIssue>),
}

struct ValidationIssue {
    message: String,
    path: Vec<Value>,
    code: &'static str,
    values: Option<Vec<&'static str>>,
}

impl ValidationIssue {
    fn at(path: Vec<Value>, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            path,
            code: "custom",
            values: None,
        }
    }

    fn invalid_value(
        path: Vec<Value>,
        message: impl Into<String>,
        values: Vec<&'static str>,
    ) -> Self {
        Self {
            message: message.into(),
            path,
            code: "invalid_value",
            values: Some(values),
        }
    }
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
    fn validation(issues: Vec<ValidationIssue>) -> Self {
        Self::Validation(issues)
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
            ApiError::Validation(issues) => {
                let details: Vec<Value> = issues
                    .iter()
                    .map(|i| {
                        let mut obj = serde_json::json!({
                            "code": i.code,
                            "path": i.path,
                            "message": i.message,
                        });
                        if let Some(values) = &i.values {
                            obj["values"] = serde_json::json!(values);
                        }
                        obj
                    })
                    .collect();
                let body = serde_json::json!({ "error": "Validation failed", "details": details });
                (StatusCode::BAD_REQUEST, Json(body)).into_response()
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

/// Wire shape accepted by PUT/PATCH bodies. Mirrors `ModelConfigSchema`,
/// including the legacy flat `selector`/`targets` fields that get folded
/// into `target_groups` (see `normalize_legacy_targets`).
#[derive(Debug, Deserialize, Default)]
struct ModelBody {
    selector: Option<String>,
    #[serde(default)]
    priority: Option<String>,
    targets: Option<Vec<ModelTarget>>,
    target_groups: Option<Vec<ModelTargetGroupBody>>,
    additional_aliases: Option<Vec<String>>,
    use_image_fallthrough: Option<bool>,
    enforce_limits: Option<bool>,
    sticky_session: Option<bool>,
    preferred_api: Option<Vec<String>>,
    #[serde(rename = "type")]
    model_type: Option<String>,
    advanced: Option<Value>,
    metadata: Option<Value>,
    pi_model: Option<Value>,
    #[serde(rename = "extraBody")]
    extra_body: Option<Value>,
    compaction: Option<Value>,
}

#[derive(Debug, Deserialize, Clone)]
struct ModelTargetGroupBody {
    name: String,
    selector: Option<String>,
    targets: Vec<ModelTarget>,
}

const VALID_SELECTORS: &[&str] = &[
    "random",
    "in_order",
    "cost",
    "latency",
    "usage",
    "quota",
    "performance",
    "e2e_performance",
];

/// Mirrors `ModelConfigSchema`'s `.transform`: normalizes the legacy flat
/// `selector`/`targets` shape into a single `target_groups` entry when
/// `target_groups` wasn't given directly.
fn normalize_legacy_targets(body: &ModelBody) -> Option<Vec<ModelTargetGroupBody>> {
    if body.target_groups.is_some() {
        return body.target_groups.clone();
    }
    body.targets.as_ref().map(|targets| {
        vec![ModelTargetGroupBody {
            name: "default".to_string(),
            selector: Some(
                body.selector
                    .clone()
                    .unwrap_or_else(|| "random".to_string()),
            ),
            targets: targets.clone(),
        }]
    })
}

/// Mirrors `findDuplicateAliasTargets`.
fn find_duplicate_targets(target_groups: &[ModelTargetGroup]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut duplicates = Vec::new();
    for group in target_groups {
        for target in &group.targets {
            let key = target
                .alias
                .as_ref()
                .map(|a| format!("alias:{a}"))
                .unwrap_or_else(|| {
                    format!(
                        "{}/{}",
                        target.provider.as_deref().unwrap_or(""),
                        target.model.as_deref().unwrap_or("")
                    )
                });
            if !seen.insert(key.clone()) && !duplicates.contains(&key) {
                duplicates.push(key);
            }
        }
    }
    duplicates
}

/// Mirrors `findDuplicateAdditionalAliases`.
fn find_duplicate_additional_aliases(aliases: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut duplicates = Vec::new();
    for alias in aliases {
        if !seen.insert(alias.clone()) && !duplicates.contains(alias) {
            duplicates.push(alias.clone());
        }
    }
    duplicates
}

/// Validates a `ModelBody` against the load-bearing subset of
/// `ModelConfigSchema`'s refinements, and builds a `ModelConfig` from it
/// merged over `existing` (None for a from-scratch PUT).
fn validate_and_build(
    body: ModelBody,
    existing: Option<&ModelConfig>,
) -> Result<ModelConfig, Vec<ValidationIssue>> {
    let mut issues = Vec::new();

    let target_group_bodies = normalize_legacy_targets(&body);
    let target_groups: Vec<ModelTargetGroup> = if let Some(bodies) = target_group_bodies {
        let mut groups = Vec::new();
        for (index, group) in bodies.into_iter().enumerate() {
            let selector = match group.selector {
                Some(s) if VALID_SELECTORS.contains(&s.as_str()) => s,
                Some(_) | None => {
                    issues.push(ValidationIssue::invalid_value(
                        vec!["target_groups".into(), index.into(), "selector".into()],
                        format!(
                            "Invalid option: expected one of {}",
                            VALID_SELECTORS
                                .iter()
                                .map(|s| format!("\"{s}\""))
                                .collect::<Vec<_>>()
                                .join("|")
                        ),
                        VALID_SELECTORS.to_vec(),
                    ));
                    continue;
                }
            };
            groups.push(ModelTargetGroup {
                name: group.name,
                selector,
                targets: group.targets,
            });
        }
        groups
    } else {
        existing
            .map(|e| e.target_groups.clone())
            .unwrap_or_default()
    };

    for group in &target_groups {
        for target in &group.targets {
            let has_alias = target.alias.is_some();
            let has_provider_model = target.provider.is_some() && target.model.is_some();
            if has_alias == has_provider_model {
                issues.push(ValidationIssue::at(
                    vec!["target_groups".into(), 0.into(), "targets".into(), 0.into()],
                    "A target must specify either 'alias' or both 'provider' and 'model', but not both",
                ));
            }
        }
    }

    for dup in find_duplicate_targets(&target_groups) {
        let message = if let Some(alias) = dup.strip_prefix("alias:") {
            format!("Duplicate target 'alias:{alias}' is not allowed")
        } else {
            format!("Duplicate target '{dup}' is not allowed")
        };
        issues.push(ValidationIssue::at(vec!["target_groups".into()], message));
    }

    let additional_aliases = body
        .additional_aliases
        .clone()
        .or_else(|| existing.map(|e| e.additional_aliases.clone()))
        .unwrap_or_default();
    for dup in find_duplicate_additional_aliases(&additional_aliases) {
        issues.push(ValidationIssue::at(
            vec!["additional_aliases".into()],
            format!("Duplicate additional alias '{dup}' is not allowed"),
        ));
    }

    if !issues.is_empty() {
        return Err(issues);
    }

    Ok(ModelConfig {
        priority: body
            .priority
            .or_else(|| existing.map(|e| e.priority.clone()))
            .unwrap_or_else(|| "selector".to_string()),
        target_groups,
        additional_aliases,
        use_image_fallthrough: body
            .use_image_fallthrough
            .unwrap_or_else(|| existing.map(|e| e.use_image_fallthrough).unwrap_or(false)),
        enforce_limits: body
            .enforce_limits
            .unwrap_or_else(|| existing.map(|e| e.enforce_limits).unwrap_or(false)),
        sticky_session: body
            .sticky_session
            .unwrap_or_else(|| existing.map(|e| e.sticky_session).unwrap_or(true)),
        preferred_api: body
            .preferred_api
            .or_else(|| existing.and_then(|e| e.preferred_api.clone())),
        model_type: body
            .model_type
            .or_else(|| existing.and_then(|e| e.model_type.clone())),
        advanced: body
            .advanced
            .or_else(|| existing.and_then(|e| e.advanced.clone())),
        metadata: body
            .metadata
            .or_else(|| existing.and_then(|e| e.metadata.clone())),
        pi_model: body
            .pi_model
            .or_else(|| existing.and_then(|e| e.pi_model.clone())),
        extra_body: body
            .extra_body
            .or_else(|| existing.and_then(|e| e.extra_body.clone())),
        compaction: body
            .compaction
            .or_else(|| existing.and_then(|e| e.compaction.clone())),
    })
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

    for start_slug in models.keys() {
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

async fn put_alias(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(slug): Path<String>,
    Json(raw): Json<Value>,
) -> Response {
    if let Err(e) = require_admin(&state, &headers) {
        return e.into_response();
    }
    let body: ModelBody = match serde_json::from_value(raw) {
        Ok(b) => b,
        Err(_) => return ApiError::validation(vec![]).into_response(),
    };
    let config = match validate_and_build(body, None) {
        Ok(c) => c,
        Err(issues) => return ApiError::validation(issues).into_response(),
    };

    {
        let aliases = state.inner.aliases.read().unwrap();
        let mut candidate = aliases.clone();
        candidate.insert(slug.clone(), config.clone());
        if let Err(_message) = assert_no_alias_ref_cycles(&candidate) {
            return ApiError::Simple(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal server error".to_string(),
            )
            .into_response();
        }
    }

    if let Some(pool) = &state.inner.database {
        if let Err(error) = crate::database::upsert_alias(pool, &slug, &config).await {
            return ApiError::Simple(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
                .into_response();
        }
    }

    state
        .inner
        .aliases
        .write()
        .unwrap()
        .insert(slug.clone(), config);
    Json(serde_json::json!({ "success": true, "slug": slug })).into_response()
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
    if !raw.is_object() {
        return ApiError::Simple(
            StatusCode::BAD_REQUEST,
            "Object body is required".to_string(),
        )
        .into_response();
    }

    let Some(existing) = state.inner.aliases.read().unwrap().get(&slug).cloned() else {
        return ApiError::Simple(StatusCode::NOT_FOUND, format!("Alias '{slug}' not found"))
            .into_response();
    };

    let body: ModelBody = match serde_json::from_value(raw) {
        Ok(b) => b,
        Err(_) => return ApiError::validation(vec![]).into_response(),
    };
    let merged = match validate_and_build(body, Some(&existing)) {
        Ok(c) => c,
        Err(issues) => return ApiError::validation(issues).into_response(),
    };

    {
        let aliases = state.inner.aliases.read().unwrap();
        let mut candidate = aliases.clone();
        candidate.insert(slug.clone(), merged.clone());
        if let Err(_message) = assert_no_alias_ref_cycles(&candidate) {
            return ApiError::Simple(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal server error".to_string(),
            )
            .into_response();
        }
    }

    if let Some(pool) = &state.inner.database {
        if let Err(error) = crate::database::upsert_alias(pool, &slug, &merged).await {
            return ApiError::Simple(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
                .into_response();
        }
    }

    state
        .inner
        .aliases
        .write()
        .unwrap()
        .insert(slug.clone(), merged);
    Json(serde_json::json!({ "success": true, "slug": slug })).into_response()
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
