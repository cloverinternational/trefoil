use crate::{
    auth::{resolve_principal, Principal},
    AppState,
};
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json, Router,
};
use serde::Deserialize;
use sqlx::Row;
use std::collections::BTreeMap;

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/v0/management/catalog-organization",
            axum::routing::get(get_catalog),
        )
        .route(
            "/v0/management/catalog-organization/groups/:name",
            axum::routing::put(create_group).delete(delete_group),
        )
        .route(
            "/v0/management/catalog-organization/:kind/:id",
            axum::routing::put(put_item).delete(delete_item),
        )
}

fn admin(state: &AppState, headers: &HeaderMap) -> Result<(), Response> {
    match resolve_principal(
        state,
        headers.get("x-admin-key").and_then(|v| v.to_str().ok()),
    ) {
        Some(Principal::Admin) => Ok(()),
        Some(Principal::Limited { .. }) => {
            Err(error(StatusCode::FORBIDDEN, "Admin privileges required"))
        }
        None => Err(error(StatusCode::UNAUTHORIZED, "Unauthorized")),
    }
}
fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(serde_json::json!({"error":message}))).into_response()
}
fn db(state: &AppState) -> Result<&sqlx::PgPool, Response> {
    state
        .inner
        .database
        .as_ref()
        .ok_or_else(|| error(StatusCode::SERVICE_UNAVAILABLE, "Database unavailable"))
}
fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && s.as_bytes()[0].is_ascii_alphanumeric()
        && s.as_bytes()[s.len() - 1].is_ascii_alphanumeric()
        && !s.contains("--")
}
fn valid_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 512
}

async fn get_catalog(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(r) = admin(&state, &headers) {
        return r;
    }
    let pool = match db(&state) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let groups = match sqlx::query_scalar::<_, String>(
        "SELECT name FROM catalog_organization_groups ORDER BY name",
    )
    .fetch_all(pool)
    .await
    {
        Ok(v) => v,
        Err(_) => return error(StatusCode::SERVICE_UNAVAILABLE, "Database unavailable"),
    };
    let rows = match sqlx::query("SELECT kind, resource_id, group_name, tags FROM catalog_organization_items ORDER BY kind, resource_id").fetch_all(pool).await { Ok(v)=>v, Err(_)=>return error(StatusCode::SERVICE_UNAVAILABLE,"Database unavailable") };
    let mut items: BTreeMap<String, BTreeMap<String, serde_json::Value>> =
        ["provider", "model", "alias", "key"]
            .into_iter()
            .map(|k| (k.to_string(), BTreeMap::new()))
            .collect();
    for row in rows {
        let kind: String = row.get("kind");
        let id: String = row.get("resource_id");
        let group: Option<String> = row.get("group_name");
        let tags: Vec<String> = row.get("tags");
        items
            .entry(kind)
            .or_default()
            .insert(id, serde_json::json!({"group":group,"tags":tags}));
    }
    Json(serde_json::json!({"groups":groups,"items":items})).into_response()
}

async fn create_group(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    if let Err(r) = admin(&state, &headers) {
        return r;
    }
    if !valid_name(&name) {
        return error(StatusCode::BAD_REQUEST, "Invalid group name");
    }
    let pool = match db(&state) {
        Ok(p) => p,
        Err(r) => return r,
    };
    match sqlx::query(
        "INSERT INTO catalog_organization_groups(name) VALUES($1) ON CONFLICT DO NOTHING",
    )
    .bind(name)
    .execute(pool)
    .await
    {
        Ok(r) if r.rows_affected() == 0 => error(StatusCode::CONFLICT, "Group already exists"),
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(_) => error(StatusCode::SERVICE_UNAVAILABLE, "Database unavailable"),
    }
}
async fn delete_group(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    if let Err(r) = admin(&state, &headers) {
        return r;
    }
    let pool = match db(&state) {
        Ok(p) => p,
        Err(r) => return r,
    };
    match sqlx::query("DELETE FROM catalog_organization_groups WHERE name=$1")
        .bind(name)
        .execute(pool)
        .await
    {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(_) => error(StatusCode::SERVICE_UNAVAILABLE, "Database unavailable"),
    }
}
#[derive(Deserialize)]
struct ItemInput {
    #[serde(default)]
    group: Option<Option<String>>,
    tags: Vec<String>,
}
async fn put_item(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((kind, id)): Path<(String, String)>,
    Json(input): Json<ItemInput>,
) -> Response {
    if let Err(r) = admin(&state, &headers) {
        return r;
    }
    if !["provider", "model", "alias", "key"].contains(&kind.as_str())
        || !valid_id(&id)
        || input.tags.len() > 64
        || input.tags.iter().any(|t| !valid_name(t))
    {
        return error(StatusCode::BAD_REQUEST, "Invalid item metadata");
    }
    if input
        .group
        .as_ref()
        .and_then(|g| g.as_ref())
        .is_some_and(|g| !valid_name(g))
    {
        return error(StatusCode::BAD_REQUEST, "Invalid group name");
    }
    let pool = match db(&state) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let group = input.group.flatten();
    if let Some(ref g) = group {
        match sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM catalog_organization_groups WHERE name=$1)",
        )
        .bind(g)
        .fetch_one(pool)
        .await
        {
            Ok(true) => (),
            Ok(false) => return error(StatusCode::BAD_REQUEST, "Unknown group"),
            Err(_) => return error(StatusCode::SERVICE_UNAVAILABLE, "Database unavailable"),
        }
    }
    match sqlx::query("INSERT INTO catalog_organization_items(kind,resource_id,group_name,tags) VALUES($1,$2,$3,$4) ON CONFLICT(kind,resource_id) DO UPDATE SET group_name=EXCLUDED.group_name,tags=EXCLUDED.tags").bind(kind).bind(id).bind(group).bind(input.tags).execute(pool).await {Ok(_)=>StatusCode::NO_CONTENT.into_response(),Err(_)=>error(StatusCode::SERVICE_UNAVAILABLE,"Database unavailable")}
}
async fn delete_item(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((kind, id)): Path<(String, String)>,
) -> Response {
    if let Err(r) = admin(&state, &headers) {
        return r;
    }
    if !["provider", "model", "alias", "key"].contains(&kind.as_str()) {
        return error(StatusCode::BAD_REQUEST, "Invalid resource kind");
    }
    let pool = match db(&state) {
        Ok(p) => p,
        Err(r) => return r,
    };
    match sqlx::query("DELETE FROM catalog_organization_items WHERE kind=$1 AND resource_id=$2")
        .bind(kind)
        .bind(id)
        .execute(pool)
        .await
    {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(_) => error(StatusCode::SERVICE_UNAVAILABLE, "Database unavailable"),
    }
}
