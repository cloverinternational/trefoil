//! PostgreSQL-backed MCP server configuration and key management.

use crate::mcp_local::LocalConfig;
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
use serde_json::{json, Value};
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
            "/v0/management/mcp-servers/:name/status",
            axum::routing::get(server_status),
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

#[derive(Deserialize)]
struct KeyBody {
    key: String,
    is_active: Option<bool>,
}

/// Pinned Plexus `McpServerConfigSchema` (`@plexus/shared` mcp.ts).
fn server_schema() -> &'static crate::zod::Schema {
    use crate::zod::*;
    static SCHEMA: std::sync::OnceLock<Schema> = std::sync::OnceLock::new();
    SCHEMA.get_or_init(|| {
        let settings = || {
            vec![
                ("auth_scheme", trimmed().min(1.0).nullable().optional()),
                (
                    "rate_limit_cooldown_ms",
                    number().int().min(0.0).default(60_000).optional(),
                ),
                (
                    "quota_cooldown_ms",
                    number().int().min(0.0).default(86_400_000).optional(),
                ),
            ]
        };
        let local = object(vec![
            ("mode", literal("local_http")),
            ("enabled", boolean().default(true)),
            ("launcher", enumeration(&["bunx", "uvx"])),
            ("package", trimmed().min(1.0)),
            ("args", array(string()).default(json!([])).optional()),
            (
                "env",
                record(string(), string()).default(json!({})).optional(),
            ),
            ("port", number().int().min(1.0).max(65_535.0)),
            ("path", trimmed().min(1.0).default("/mcp").optional()),
            (
                "startup_timeout_ms",
                number()
                    .int()
                    .min(1_000.0)
                    .max(300_000.0)
                    .default(30_000)
                    .optional(),
            ),
            ("headers", record(string(), string()).optional()),
        ])
        .extend(settings());
        let remote = object(vec![
            (
                "mode",
                literal("remote_http").default("remote_http").optional(),
            ),
            ("upstream_url", string().url()),
            ("enabled", boolean().default(true)),
            ("headers", record(string(), string()).optional()),
        ])
        .extend(settings());
        union(vec![local, remote])
    })
}

fn encrypt_json(value: &Value) -> String {
    crate::secret_crypto::encrypt(&value.to_string())
}
fn decrypt_json(text: Option<String>) -> Option<Value> {
    let text = text?;
    let plain = crate::secret_crypto::decrypt(&text).ok()?;
    serde_json::from_str(&plain)
        .ok()
        .or(Some(Value::String(plain)))
}

/// `getAllMcpServers` row mapping.
fn server_view(row: &sqlx::postgres::PgRow) -> Value {
    let mode: String = row
        .try_get::<Option<String>, _>("mode")
        .ok()
        .flatten()
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| "remote_http".into());
    let headers = decrypt_json(row.try_get("headers").ok().flatten());
    let auth_scheme: Option<String> = row.try_get("auth_scheme").ok().flatten();
    let rate: i64 = row
        .try_get::<i32, _>("rate_limit_cooldown_ms")
        .map(i64::from)
        .unwrap_or(60_000);
    let quota: i64 = row
        .try_get::<i32, _>("quota_cooldown_ms")
        .map(i64::from)
        .unwrap_or(86_400_000);
    if mode == "local_http" {
        let mut out = serde_json::Map::new();
        out.insert("mode".into(), json!("local_http"));
        out.insert(
            "enabled".into(),
            json!(row.try_get::<bool, _>("enabled").unwrap_or(true)),
        );
        out.insert(
            "launcher".into(),
            json!(row.try_get::<Option<String>, _>("launcher").ok().flatten()),
        );
        out.insert(
            "package".into(),
            json!(row
                .try_get::<Option<String>, _>("package_name")
                .ok()
                .flatten()
                .unwrap_or_default()),
        );
        out.insert(
            "args".into(),
            decrypt_json(row.try_get("args").ok().flatten()).unwrap_or(json!([])),
        );
        if let Some(env) = decrypt_json(row.try_get("env").ok().flatten()) {
            out.insert("env".into(), env);
        }
        out.insert(
            "port".into(),
            json!(row
                .try_get::<Option<i64>, _>("port")
                .ok()
                .flatten()
                .unwrap_or(0)),
        );
        out.insert(
            "path".into(),
            json!(row
                .try_get::<Option<String>, _>("path")
                .ok()
                .flatten()
                .filter(|p| !p.is_empty())
                .unwrap_or_else(|| "/mcp".into())),
        );
        out.insert(
            "startup_timeout_ms".into(),
            json!(row
                .try_get::<Option<i64>, _>("startup_timeout_ms")
                .ok()
                .flatten()
                .filter(|t| *t != 0)
                .unwrap_or(30_000)),
        );
        if let Some(h) = headers {
            out.insert("headers".into(), h);
        }
        out.insert("auth_scheme".into(), json!(auth_scheme));
        out.insert("rate_limit_cooldown_ms".into(), json!(rate));
        out.insert("quota_cooldown_ms".into(), json!(quota));
        return Value::Object(out);
    }
    let mut out = serde_json::Map::new();
    out.insert(
        "upstream_url".into(),
        json!(row.try_get::<String, _>("upstream_url").unwrap_or_default()),
    );
    out.insert(
        "enabled".into(),
        json!(row.try_get::<bool, _>("enabled").unwrap_or(true)),
    );
    if let Some(h) = headers {
        out.insert("headers".into(), h);
    }
    out.insert("auth_scheme".into(), json!(auth_scheme));
    out.insert("rate_limit_cooldown_ms".into(), json!(rate));
    out.insert("quota_cooldown_ms".into(), json!(quota));
    Value::Object(out)
}

const SERVER_COLUMNS: &str = "name,upstream_url,enabled,mode,headers,auth_scheme,rate_limit_cooldown_ms,quota_cooldown_ms,launcher,package_name,args,env,port,path,startup_timeout_ms";

pub(crate) async fn load_server(state: &AppState, name: &str) -> Result<Option<Value>, ()> {
    let Some(pool) = &state.inner.database else {
        return Ok(None);
    };
    let row = sqlx::query(&format!(
        "SELECT {SERVER_COLUMNS} FROM mcp_servers WHERE name=$1"
    ))
    .bind(name)
    .fetch_optional(pool)
    .await
    .map_err(|_| ())?;
    Ok(row.map(|row| server_view(&row)))
}

async fn list_servers(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(r) = admin(&state, &headers) {
        return r;
    }
    let Some(pool) = &state.inner.database else {
        return Json(json!({})).into_response();
    };
    let Ok(rows) = sqlx::query(&format!(
        "SELECT {SERVER_COLUMNS} FROM mcp_servers ORDER BY id"
    ))
    .fetch_all(pool)
    .await
    else {
        return internal_error();
    };
    let mut out = serde_json::Map::new();
    for row in rows {
        out.insert(
            row.try_get::<String, _>("name").unwrap_or_default(),
            server_view(&row),
        );
    }
    Json(Value::Object(out)).into_response()
}

async fn get_server(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    if let Err(r) = admin(&state, &headers) {
        return r;
    }
    match load_server(&state, &name).await {
        Ok(Some(mut view)) => {
            view["name"] = json!(name);
            Json(view).into_response()
        }
        Ok(None) => not_found(&name),
        Err(()) => internal_error(),
    }
}

/// `validateServerName`: slug, 2–63 chars, not reserved.
fn valid_name(value: &str) -> bool {
    let b = value.as_bytes();
    (2..=63).contains(&b.len())
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-' || *c == b'_')
        && value != "plexus"
}

fn invalid_name() -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({"error":"Invalid server name. Must be a non-reserved slug (lowercase letters, numbers, hyphens, underscores, 2-63 characters)"}))).into_response()
}

/// `saveMcpServer`.
async fn save_server(state: &AppState, name: &str, config: &Value) -> Result<(), ()> {
    let Some(pool) = &state.inner.database else {
        return Err(());
    };
    let local = config.get("mode").and_then(Value::as_str) == Some("local_http");
    let port = config.get("port").and_then(Value::as_i64);
    let path = config
        .get("path")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())
        .unwrap_or("/mcp")
        .to_owned();
    let upstream = if local {
        format!("http://127.0.0.1:{}{path}", port.unwrap_or(0))
    } else {
        config
            .get("upstream_url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let opt_enc = |key: &str| config.get(key).filter(|v| !v.is_null()).map(encrypt_json);
    let now = chrono::Utc::now().timestamp_millis();
    sqlx::query("INSERT INTO mcp_servers (name,upstream_url,enabled,headers,auth_scheme,rate_limit_cooldown_ms,quota_cooldown_ms,mode,launcher,package_name,args,env,port,path,startup_timeout_ms,created_at,updated_at)
        VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$16)
        ON CONFLICT(name) DO UPDATE SET upstream_url=EXCLUDED.upstream_url,enabled=EXCLUDED.enabled,headers=EXCLUDED.headers,auth_scheme=EXCLUDED.auth_scheme,
        rate_limit_cooldown_ms=EXCLUDED.rate_limit_cooldown_ms,quota_cooldown_ms=EXCLUDED.quota_cooldown_ms,mode=EXCLUDED.mode,launcher=EXCLUDED.launcher,
        package_name=EXCLUDED.package_name,args=EXCLUDED.args,env=EXCLUDED.env,port=EXCLUDED.port,path=EXCLUDED.path,startup_timeout_ms=EXCLUDED.startup_timeout_ms,updated_at=EXCLUDED.updated_at")
        .bind(name).bind(upstream)
        .bind(config.get("enabled") != Some(&Value::Bool(false)))
        .bind(opt_enc("headers"))
        .bind(config.get("auth_scheme").and_then(Value::as_str))
        .bind(config.get("rate_limit_cooldown_ms").and_then(Value::as_i64).unwrap_or(60_000) as i32)
        .bind(config.get("quota_cooldown_ms").and_then(Value::as_i64).unwrap_or(86_400_000) as i32)
        .bind(if local { "local_http" } else { "remote_http" })
        .bind(local.then(|| config.get("launcher").and_then(Value::as_str)).flatten())
        .bind(local.then(|| config.get("package").and_then(Value::as_str)).flatten())
        .bind(if local { opt_enc("args") } else { None })
        .bind(if local { opt_enc("env") } else { None })
        .bind(if local { port } else { None })
        .bind(local.then_some(path))
        .bind(if local { Some(config.get("startup_timeout_ms").and_then(Value::as_i64).filter(|t| *t != 0).unwrap_or(30_000)) } else { None })
        .bind(now)
        .execute(pool).await.map(|_| ()).map_err(|_| ())
}

async fn put_server(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    if let Err(r) = admin(&state, &headers) {
        return r;
    }
    if !valid_name(&name) {
        return invalid_name();
    }
    let parsed = match server_schema().safe_parse(&body) {
        Ok(parsed) => parsed,
        Err(details) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error":"Validation failed","details":details})),
            )
                .into_response()
        }
    };
    match save_server(&state, &name, &parsed).await {
        Ok(()) => Json(json!({"success":true,"name":name})).into_response(),
        Err(()) => internal_error(),
    }
}

async fn patch_server(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    if let Err(r) = admin(&state, &headers) {
        return r;
    }
    let Some(patch) = body.as_object() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"Object body is required"})),
        )
            .into_response();
    };
    if !valid_name(&name) {
        return invalid_name();
    }
    let mut merged = match load_server(&state, &name).await {
        Ok(Some(view)) => view,
        Ok(None) => return not_found(&name),
        Err(()) => return internal_error(),
    };
    if let Some(map) = merged.as_object_mut() {
        for (k, v) in patch {
            map.insert(k.clone(), v.clone());
        }
    }
    let parsed = match server_schema().safe_parse(&merged) {
        Ok(parsed) => parsed,
        Err(details) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error":"Validation failed","details":details})),
            )
                .into_response()
        }
    };
    match save_server(&state, &name, &parsed).await {
        Ok(()) => Json(json!({"success":true,"name":name})).into_response(),
        Err(()) => internal_error(),
    }
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
        return Json(json!({"success":true})).into_response();
    };
    // Plexus `deleteServer`: stop the process, delete the config, then drop
    // the server's persisted MCP_DATA_DIR.
    let _ = state.inner.mcp_manager.stop(&name).await;
    match sqlx::query("DELETE FROM mcp_servers WHERE name=$1")
        .bind(&name)
        .execute(pool)
        .await
    {
        Ok(_) => match crate::mcp_local::remove_mcp_server_data_dir(&name, None) {
            Ok(()) => Json(json!({"success":true})).into_response(),
            Err(_) => internal_error(),
        },
        Err(_) => internal_error(),
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
    Json(serde_json::json!({"keys":rows.into_iter().map(|r|serde_json::json!({"id":r.try_get::<i32,_>("id").unwrap_or_default(),"key":r.try_get::<String,_>("key").unwrap_or_default(),"is_active":r.try_get::<bool,_>("is_active").unwrap_or(true),"cooldown_until":serde_json::Value::Null})).collect::<Vec<_>>() })).into_response()
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
    match sqlx::query("INSERT INTO mcp_keys (mcp_server_id,key,is_active,created_at,updated_at) SELECT id,$2,$3,NOW(),NOW() FROM mcp_servers WHERE name=$1 RETURNING id,key,is_active").bind(name).bind(body.key).bind(body.is_active.unwrap_or(true)).fetch_optional(pool).await{Ok(Some(r))=>(StatusCode::CREATED,Json(serde_json::json!({"id":r.try_get::<i32,_>("id").unwrap_or_default(),"key":r.try_get::<String,_>("key").unwrap_or_default(),"is_active":r.try_get::<bool,_>("is_active").unwrap_or(true),"cooldown_until":serde_json::Value::Null}))).into_response(),Ok(None)=>not_found("MCP server"),Err(_)=>(StatusCode::INTERNAL_SERVER_ERROR,Json(serde_json::json!({"error":"Internal server error"}))).into_response()}
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
        return key_not_found(id);
    };
    match sqlx::query("DELETE FROM mcp_keys k USING mcp_servers s WHERE k.mcp_server_id=s.id AND s.name=$1 AND k.id=$2").bind(name).bind(id).execute(pool).await{Ok(r) if r.rows_affected()>0=>Json(serde_json::json!({"success":true})).into_response(),_=>key_not_found(id)}
}
async fn clear_key_cooldown(
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
        return Json(serde_json::json!({"success":true})).into_response();
    };
    match sqlx::query("UPDATE mcp_keys k SET cooldown_until=NULL, updated_at=NOW() FROM mcp_servers s WHERE k.mcp_server_id=s.id AND s.name=$1 AND k.id=$2")
        .bind(name).bind(id)
        .execute(pool)
        .await
    {
        Ok(result) if result.rows_affected() > 0 => {
            Json(serde_json::json!({"success":true})).into_response()
        }
        _ => key_not_found(id),
    }
}

fn key_not_found(id: i32) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({"error":format!("MCP key '{id}' not found")})),
    )
        .into_response()
}

fn not_found(name: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({"error":format!("MCP server '{name}' not found")})),
    )
        .into_response()
}

fn lifecycle_log(state: &AppState, message: String) {
    crate::system_logs::event(
        state,
        "info",
        message,
        [("module".into(), serde_json::json!("config"))],
    );
}

fn lifecycle_error(message: String) -> Response {
    // Plexus's global error handler for a thrown handler error.
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error":{"message":message,"type":"api_error"}})),
    )
        .into_response()
}

async fn start_server(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    if let Err(response) = admin(&state, &headers) {
        return response;
    }
    lifecycle_log(&state, format!("MCP local start requested for '{name}'"));
    let view = match load_server(&state, &name).await {
        Ok(Some(view)) => view,
        Ok(None) => return not_found(&name),
        Err(()) => return internal_error(),
    };
    let Some(config) = LocalConfig::from_view(&view) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"MCP server is not local_http"})),
        )
            .into_response();
    };
    match state.inner.mcp_manager.start(&name, &config).await {
        Ok(status) => Json(status).into_response(),
        Err(error) => lifecycle_error(error),
    }
}

async fn stop_server(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    if let Err(response) = admin(&state, &headers) {
        return response;
    }
    lifecycle_log(&state, format!("MCP local stop requested for '{name}'"));
    match state.inner.mcp_manager.stop(&name).await {
        Ok(status) => Json(status).into_response(),
        Err(error) => lifecycle_error(error),
    }
}

async fn restart_server(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    if let Err(response) = admin(&state, &headers) {
        return response;
    }
    lifecycle_log(&state, format!("MCP local restart requested for '{name}'"));
    let view = match load_server(&state, &name).await {
        Ok(Some(view)) => view,
        Ok(None) => return not_found(&name),
        Err(()) => return internal_error(),
    };
    let Some(config) = LocalConfig::from_view(&view) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"MCP server is not local_http"})),
        )
            .into_response();
    };
    match state.inner.mcp_manager.restart(&name, &config).await {
        Ok(status) => Json(status).into_response(),
        Err(error) => lifecycle_error(error),
    }
}

async fn server_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    if let Err(response) = admin(&state, &headers) {
        return response;
    }
    lifecycle_log(&state, format!("MCP local status requested for '{name}'"));
    let view = match load_server(&state, &name).await {
        Ok(Some(view)) => view,
        Ok(None) => return not_found(&name),
        Err(()) => return internal_error(),
    };
    Json(
        state
            .inner
            .mcp_manager
            .status(&name, LocalConfig::from_view(&view).as_ref()),
    )
    .into_response()
}

fn internal_error() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error":"Internal server error"})),
    )
        .into_response()
}

async fn process_logs(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    if let Err(response) = admin(&state, &headers) {
        return response;
    }
    lifecycle_log(
        &state,
        format!("MCP local process logs requested for '{name}'"),
    );
    Json(json!({"data": state.inner.mcp_manager.logs(&name)})).into_response()
}

#[cfg(test)]
mod contract_tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use serde_json::Value;
    use tower::ServiceExt;

    #[tokio::test]
    async fn missing_key_error_identifies_key_not_server() {
        let state = AppState::new(Some("admin".into()), Default::default());
        let response = router()
            .with_state(state)
            .oneshot(
                Request::delete("/v0/management/mcp-servers/example/keys/999")
                    .header("x-admin-key", "admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body: Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(body, serde_json::json!({"error":"MCP key '999' not found"}));
    }
}
