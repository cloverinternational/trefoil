//! PostgreSQL-backed MCP server configuration and key management.

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

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/v0/management/mcp-servers",
            axum::routing::get(list_servers),
        )
        .route(
            "/v0/management/mcp-servers/:name",
            axum::routing::get(get_server)
                .put(put_server)
                .patch(patch_server)
                .delete(delete_server),
        )
        .route(
            "/v0/management/mcp-servers/:name/keys",
            axum::routing::get(list_keys).post(add_key),
        )
        .route(
            "/v0/management/mcp-servers/:name/keys/:key_id",
            axum::routing::delete(delete_key),
        )
        .route(
            "/v0/management/mcp-servers/:name/keys/:key_id/clear-cooldown",
            axum::routing::post(clear_key_cooldown),
        )
        .route(
            "/v0/management/mcp-servers/:name/start",
            axum::routing::post(start_server),
        )
        .route(
            "/v0/management/mcp-servers/:name/stop",
            axum::routing::post(stop_server),
        )
        .route(
            "/v0/management/mcp-servers/:name/restart",
            axum::routing::post(restart_server),
        )
        .route(
            "/v0/management/mcp-servers/:name/process-logs",
            axum::routing::get(process_logs),
        )
}

fn admin(state: &AppState, headers: &HeaderMap) -> Result<(), Response> {
    match resolve_principal(state, headers.get("x-admin-key").and_then(|v| v.to_str().ok())) {
        Some(Principal::Admin) => Ok(()),
        Some(Principal::Limited { .. }) => Err((StatusCode::FORBIDDEN, Json(serde_json::json!({"error":{"message":"Admin privileges required","type":"forbidden","code":403}}))).into_response()),
        None => Err((StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error":{"message":"Unauthorized","type":"auth_error","code":401}}))).into_response()),
    }
}

#[derive(Deserialize, Default)]
struct ServerBody {
    upstream_url: Option<String>,
    enabled: Option<bool>,
    mode: Option<String>,
    auth_scheme: Option<String>,
}
#[derive(Deserialize)]
struct KeyBody {
    key: String,
    is_active: Option<bool>,
}

fn serialize_server(row: &sqlx::postgres::PgRow) -> serde_json::Value {
    serde_json::json!({"upstreamUrl":row.try_get::<String,_>("upstream_url").unwrap_or_default(),"enabled":row.try_get::<bool,_>("enabled").unwrap_or(true),"mode":row.try_get::<String,_>("mode").unwrap_or_else(|_|"remote_http".into()),"authScheme":row.try_get::<Option<String>,_>("auth_scheme").unwrap_or(None)})
}

async fn list_servers(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(r) = admin(&state, &headers) {
        return r;
    }
    let Some(pool) = &state.inner.database else {
        return Json(serde_json::json!({})).into_response();
    };
    let Ok(rows) = sqlx::query(
        "SELECT name,upstream_url,enabled,mode,auth_scheme FROM mcp_servers ORDER BY name",
    )
    .fetch_all(pool)
    .await
    else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error":"Internal server error"})),
        )
            .into_response();
    };
    let mut out = serde_json::Map::new();
    for row in rows {
        let name: String = row.try_get("name").unwrap_or_default();
        out.insert(name, serialize_server(&row));
    }
    Json(serde_json::Value::Object(out)).into_response()
}
async fn get_server(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    if let Err(r) = admin(&state, &headers) {
        return r;
    }
    let Some(pool) = &state.inner.database else {
        return not_found(&name);
    };
    match sqlx::query(
        "SELECT name,upstream_url,enabled,mode,auth_scheme FROM mcp_servers WHERE name=$1",
    )
    .bind(&name)
    .fetch_optional(pool)
    .await
    {
        Ok(Some(row)) => {
            let mut value = serialize_server(&row);
            value["name"] = name.into();
            Json(value).into_response()
        }
        _ => not_found(&name),
    }
}
async fn put_server(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Json(body): Json<ServerBody>,
) -> Response {
    if let Err(r) = admin(&state, &headers) {
        return r;
    }
    let Some(url) = body.upstream_url else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":"upstream_url is required"})),
        )
            .into_response();
    };
    let Some(pool) = &state.inner.database else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error":"PostgreSQL is required"})),
        )
            .into_response();
    };
    let now = chrono::Utc::now().naive_utc();
    match sqlx::query("INSERT INTO mcp_servers (name,upstream_url,enabled,mode,auth_scheme,created_at,updated_at) VALUES ($1,$2,$3,$4,$5,$6,$6) ON CONFLICT(name) DO UPDATE SET upstream_url=EXCLUDED.upstream_url,enabled=EXCLUDED.enabled,mode=EXCLUDED.mode,auth_scheme=EXCLUDED.auth_scheme,updated_at=EXCLUDED.updated_at").bind(&name).bind(url).bind(body.enabled.unwrap_or(true)).bind(body.mode.unwrap_or_else(||"remote_http".into())).bind(body.auth_scheme).bind(now).execute(pool).await {Ok(_)=>Json(serde_json::json!({"success":true,"name":name})).into_response(),Err(_)=>(StatusCode::INTERNAL_SERVER_ERROR,Json(serde_json::json!({"error":"Internal server error"}))).into_response()}
}
async fn patch_server(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Json(body): Json<ServerBody>,
) -> Response {
    if let Err(r) = admin(&state, &headers) {
        return r;
    }
    let Some(pool) = &state.inner.database else {
        return not_found(&name);
    };
    let Ok(row) =
        sqlx::query("SELECT upstream_url,enabled,mode,auth_scheme FROM mcp_servers WHERE name=$1")
            .bind(&name)
            .fetch_optional(pool)
            .await
    else {
        return not_found(&name);
    };
    let Some(row) = row else {
        return not_found(&name);
    };
    put_server(
        State(state),
        headers,
        Path(name),
        Json(ServerBody {
            upstream_url: body
                .upstream_url
                .or_else(|| row.try_get("upstream_url").ok()),
            enabled: body.enabled.or_else(|| row.try_get("enabled").ok()),
            mode: body.mode.or_else(|| row.try_get("mode").ok()),
            auth_scheme: body
                .auth_scheme
                .or_else(|| row.try_get("auth_scheme").ok().flatten()),
        }),
    )
    .await
}
async fn delete_server(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    if let Err(r) = admin(&state, &headers) {
        return r;
    }
    let Some(pool) = &state.inner.database else {
        return Json(serde_json::json!({"success":true})).into_response();
    };
    match sqlx::query("DELETE FROM mcp_servers WHERE name=$1")
        .bind(name)
        .execute(pool)
        .await
    {
        Ok(_) => Json(serde_json::json!({"success":true})).into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error":"Internal server error"})),
        )
            .into_response(),
    }
}
async fn list_keys(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    if let Err(r) = admin(&state, &headers) {
        return r;
    }
    let Some(pool) = &state.inner.database else {
        return Json(serde_json::json!({"keys":[]})).into_response();
    };
    let Ok(rows)=sqlx::query("SELECT k.id,k.key,k.is_active FROM mcp_keys k JOIN mcp_servers s ON s.id=k.mcp_server_id WHERE s.name=$1 ORDER BY k.id").bind(name).fetch_all(pool).await else{return (StatusCode::INTERNAL_SERVER_ERROR,Json(serde_json::json!({"error":"Internal server error"}))).into_response()};
    Json(serde_json::json!({"keys":rows.into_iter().map(|r|serde_json::json!({"id":r.try_get::<i32,_>("id").unwrap_or_default(),"key":r.try_get::<String,_>("key").unwrap_or_default(),"isActive":r.try_get::<bool,_>("is_active").unwrap_or(true)})).collect::<Vec<_>>() })).into_response()
}
async fn add_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Json(body): Json<KeyBody>,
) -> Response {
    if let Err(r) = admin(&state, &headers) {
        return r;
    }
    let Some(pool) = &state.inner.database else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error":"PostgreSQL is required"})),
        )
            .into_response();
    };
    match sqlx::query("INSERT INTO mcp_keys (mcp_server_id,key,is_active,created_at,updated_at) SELECT id,$2,$3,NOW(),NOW() FROM mcp_servers WHERE name=$1 RETURNING id,key,is_active").bind(name).bind(body.key).bind(body.is_active.unwrap_or(true)).fetch_optional(pool).await{Ok(Some(r))=>(StatusCode::CREATED,Json(serde_json::json!({"id":r.try_get::<i32,_>("id").unwrap_or_default(),"key":r.try_get::<String,_>("key").unwrap_or_default(),"isActive":r.try_get::<bool,_>("is_active").unwrap_or(true)}))).into_response(),Ok(None)=>not_found("MCP server"),Err(_)=>(StatusCode::INTERNAL_SERVER_ERROR,Json(serde_json::json!({"error":"Internal server error"}))).into_response()}
}
async fn delete_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((name, id)): Path<(String, String)>,
) -> Response {
    if let Err(r) = admin(&state, &headers) {
        return r;
    }
    let Ok(id) = id.parse::<i32>() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":"keyId must be a positive integer"})),
        )
            .into_response();
    };
    let Some(pool) = &state.inner.database else {
        return not_found("MCP key");
    };
    match sqlx::query("DELETE FROM mcp_keys k USING mcp_servers s WHERE k.mcp_server_id=s.id AND s.name=$1 AND k.id=$2").bind(name).bind(id).execute(pool).await{Ok(r) if r.rows_affected()>0=>Json(serde_json::json!({"success":true})).into_response(),_=>not_found("MCP key")}
}
async fn clear_key_cooldown(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((_name, id)): Path<(String, String)>,
) -> Response {
    if let Err(r) = admin(&state, &headers) {
        return r;
    }
    let Ok(id) = id.parse::<i32>() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":"keyId must be a positive integer"})),
        )
            .into_response();
    };
    let Some(pool) = &state.inner.database else {
        return Json(serde_json::json!({"success":true})).into_response();
    };
    match sqlx::query("UPDATE mcp_keys SET cooldown_until=NULL, updated_at=NOW() WHERE id=$1")
        .bind(id)
        .execute(pool)
        .await
    {
        Ok(result) if result.rows_affected() > 0 => {
            Json(serde_json::json!({"success":true})).into_response()
        }
        _ => not_found("MCP key"),
    }
}

fn not_found(name: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({"error":format!("MCP server '{name}' not found")})),
    )
        .into_response()
}

async fn start_server(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(_name): Path<String>,
) -> Response {
    if let Err(response) = admin(&state, &headers) {
        return response;
    }
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(serde_json::json!({"error":"MCP subprocess lifecycle is not implemented"})),
    )
        .into_response()
}

async fn stop_server(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    start_server(State(state), headers, Path(name)).await
}

async fn restart_server(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    start_server(State(state), headers, Path(name)).await
}

async fn process_logs(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    if let Err(response) = admin(&state, &headers) {
        return response;
    }
    Json(serde_json::json!({"serverName":name,"data":[]})).into_response()
}
