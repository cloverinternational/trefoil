//! PostgreSQL-backed MCP server configuration and key management.

use crate::mcp_stdio::LocalMcpTransport;
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
use std::collections::HashMap;
use tokio::process::Command;
use tokio::time::{sleep, Duration, Instant};
use url::Url;

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

#[derive(Deserialize, Default)]
struct ServerBody {
    upstream_url: Option<String>,
    enabled: Option<bool>,
    mode: Option<String>,
    auth_scheme: Option<String>,
    launcher: Option<String>,
    package_name: Option<String>,
    args: Option<String>,
    env: Option<String>,
    startup_timeout_ms: Option<i64>,
}
#[derive(Deserialize)]
struct KeyBody {
    key: String,
    is_active: Option<bool>,
}

fn serialize_server(row: &sqlx::postgres::PgRow) -> serde_json::Value {
    serde_json::json!({"upstream_url":row.try_get::<String,_>("upstream_url").unwrap_or_default(),"enabled":row.try_get::<bool,_>("enabled").unwrap_or(true),"auth_scheme":row.try_get::<Option<String>,_>("auth_scheme").unwrap_or(None),"rate_limit_cooldown_ms":row.try_get::<i64,_>("rate_limit_cooldown_ms").unwrap_or(60000),"quota_cooldown_ms":row.try_get::<i64,_>("quota_cooldown_ms").unwrap_or(86400000)})
}

async fn list_servers(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(r) = admin(&state, &headers) {
        return r;
    }
    let Some(pool) = &state.inner.database else {
        return Json(serde_json::json!({})).into_response();
    };
    let Ok(rows) = sqlx::query(
        "SELECT name,upstream_url,enabled,mode,auth_scheme,launcher,package_name,args,env,startup_timeout_ms FROM mcp_servers ORDER BY name",
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
    lifecycle_log(&state, format!("MCP local start requested for '{name}'"));
    let Some(pool) = &state.inner.database else {
        return not_found(&name);
    };
    match sqlx::query(
        "SELECT name,upstream_url,enabled,mode,auth_scheme,launcher,package_name,args,env,startup_timeout_ms FROM mcp_servers WHERE name=$1",
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
    if !valid_name(&name) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":"invalid server name"})),
        )
            .into_response();
    }
    let Some(url) = body.upstream_url else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":"upstream_url is required"})),
        )
            .into_response();
    };
    if Url::parse(&url).is_err() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":"upstream_url must be a valid URL"})),
        )
            .into_response();
    }
    let mode = body.mode.clone().unwrap_or_else(|| "remote_http".into());
    if mode != "remote_http" && mode != "local_process" {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "Validation failed",
                "details": [{
                    "code": "invalid_union",
                    "errors": [[
                        {"code":"invalid_value","values":["local_http"],"path":["mode"],"message":"Invalid input: expected \"local_http\""},
                        {"code":"invalid_value","values":["bunx","uvx"],"path":["launcher"],"message":"Invalid option: expected one of \"bunx\"|\"uvx\""},
                        {"expected":"string","code":"invalid_type","path":["package"],"message":"Invalid input: expected string, received undefined"},
                        {"expected":"number","code":"invalid_type","path":["port"],"message":"Invalid input: expected number, received undefined"}
                    ],[
                        {"code":"invalid_value","values":["remote_http"],"path":["mode"],"message":"Invalid input: expected \"remote_http\""}
                    ]],
                    "path": [],
                    "message": "Invalid input"
                }]
            })),
        )
            .into_response();
    }
    let Some(pool) = &state.inner.database else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error":"PostgreSQL is required"})),
        )
            .into_response();
    };
    // The migration stores these timestamps as epoch milliseconds (the
    // management tables predate the timestamp columns used by mcp_keys).
    let now = chrono::Utc::now().timestamp_millis();
    match sqlx::query("INSERT INTO mcp_servers (name,upstream_url,enabled,mode,auth_scheme,launcher,package_name,args,env,startup_timeout_ms,created_at,updated_at) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$11) ON CONFLICT(name) DO UPDATE SET upstream_url=EXCLUDED.upstream_url,enabled=EXCLUDED.enabled,mode=EXCLUDED.mode,auth_scheme=EXCLUDED.auth_scheme,launcher=EXCLUDED.launcher,package_name=EXCLUDED.package_name,args=EXCLUDED.args,env=EXCLUDED.env,startup_timeout_ms=EXCLUDED.startup_timeout_ms,updated_at=EXCLUDED.updated_at").bind(&name).bind(url).bind(body.enabled.unwrap_or(true)).bind(mode).bind(body.auth_scheme).bind(body.launcher).bind(body.package_name).bind(body.args).bind(body.env).bind(body.startup_timeout_ms).bind(now).execute(pool).await {Ok(_)=>Json(serde_json::json!({"success":true,"name":name})).into_response(),Err(_)=>internal_error()}
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
        sqlx::query("SELECT upstream_url,enabled,mode,auth_scheme,launcher,package_name,args,env,startup_timeout_ms FROM mcp_servers WHERE name=$1")
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
            launcher: body
                .launcher
                .or_else(|| row.try_get("launcher").ok().flatten()),
            package_name: body
                .package_name
                .or_else(|| row.try_get("package_name").ok().flatten()),
            args: body.args.or_else(|| row.try_get("args").ok().flatten()),
            env: body.env.or_else(|| row.try_get("env").ok().flatten()),
            startup_timeout_ms: body
                .startup_timeout_ms
                .or_else(|| row.try_get("startup_timeout_ms").ok().flatten()),
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

fn lifecycle_log(state:&AppState, message:String) {
    crate::system_logs::event(state,"info",message,[("module".into(),serde_json::json!("config"))]);
}

async fn start_server(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    if let Err(response) = admin(&state, &headers) {
        return response;
    }
    let Some(pool) = &state.inner.database else {
        return not_found(&name);
    };
    let row = match sqlx::query(
        "SELECT mode,launcher,package_name,args,env FROM mcp_servers WHERE name=$1",
    )
    .bind(&name)
    .fetch_optional(pool)
    .await
    {
        Ok(Some(row)) => row,
        Ok(None) => return not_found(&name),
        Err(_) => return internal_error(),
    };
    if row.try_get::<String, _>("mode").unwrap_or_default() != "local_process" {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":"MCP server is not local_http"})),
        )
            .into_response();
    }
    let launcher = row
        .try_get::<Option<String>, _>("launcher")
        .ok()
        .flatten()
        .unwrap_or_default();
    if !allowed_launcher(&launcher) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":"launcher is not allowlisted"})),
        )
            .into_response();
    }
    let mut command = Command::new(&launcher);
    command.kill_on_drop(true);
    if let Some(package) = row
        .try_get::<Option<String>, _>("package_name")
        .ok()
        .flatten()
    {
        command.arg(package);
    }
    command.args(parse_string_list(
        row.try_get::<Option<String>, _>("args")
            .ok()
            .flatten()
            .as_deref(),
    ));
    command.envs(parse_env(
        row.try_get::<Option<String>, _>("env")
            .ok()
            .flatten()
            .as_deref(),
    ));
    let transport =
        match LocalMcpTransport::spawn(command).await {
            Ok(transport) => transport,
            Err(error) => return (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({"error":format!("failed to start MCP process: {error}")})),
            )
                .into_response(),
        };
    let timeout_ms = row
        .try_get::<Option<i64>, _>("startup_timeout_ms")
        .ok()
        .flatten()
        .unwrap_or(30_000)
        .clamp(100, 300_000) as u64;
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    while Instant::now() < deadline {
        // Keep the startup window compatible with the management API while the
        // transport reader establishes the subprocess protocol.
        sleep(Duration::from_millis(100)).await;
        if Instant::now() >= deadline {
            break;
        }
    }
    let mut processes = state.inner.mcp_processes.lock().await;
    if let Some(old) = processes.insert(name.clone(), transport) {
        let _ = old.shutdown().await;
    }
    state
        .inner
        .mcp_process_status
        .lock()
        .await
        .insert(name.clone(), ("running".into(), None));
    Json(serde_json::json!({"success":true,"name":name,"status":"running"})).into_response()
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
    let mut processes = state.inner.mcp_processes.lock().await;
    if let Some(transport) = processes.remove(&name) {
        let _ = transport.shutdown().await;
    }
    state
        .inner
        .mcp_process_status
        .lock()
        .await
        .insert(name.clone(), ("stopped".into(), None));
    Json(serde_json::json!({"serverName":name,"status":"stopped","pid":null,"url":null,"lastError":null,"startedAt":null,"exitedAt":chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis,true)})).into_response()
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
    let _ = stop_server(State(state.clone()), headers.clone(), Path(name.clone())).await;
    start_server(State(state), headers, Path(name)).await
}

/// Return the in-memory runtime state for a managed MCP process.  The
/// configuration is read from PostgreSQL first so that an unknown server is
/// not indistinguishable from a known, stopped server.
async fn server_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    if let Err(response) = admin(&state, &headers) {
        return response;
    }
    lifecycle_log(&state, format!("MCP local status requested for '{name}'"));
    let Some(pool) = &state.inner.database else {
        return not_found(&name);
    };
    let row = match sqlx::query("SELECT upstream_url FROM mcp_servers WHERE name=$1")
        .bind(&name)
        .fetch_optional(pool)
        .await
    {
        Ok(Some(row)) => row,
        Ok(None) => return not_found(&name),
        Err(_) => return internal_error(),
    };

    let recorded = state
        .inner
        .mcp_process_status
        .lock()
        .await
        .get(&name)
        .cloned();
    let process = state.inner.mcp_processes.lock().await;
    let pid = if let Some(transport) = process.get(&name) {
        transport.pid().await
    } else {
        None
    };
    let status = if pid.is_some() {
        "running"
    } else {
        recorded
            .as_ref()
            .map(|value| value.0.as_str())
            .unwrap_or("stopped")
    };
    let status = status.to_owned();
    let last_error = recorded.and_then(|value| value.1);
    let url = row.try_get::<String, _>("upstream_url").ok();
    let url = (status == "running").then_some(url).flatten();

    Json(serde_json::json!({
        "serverName": name,
        "status": status,
        "pid": pid,
        "url": url,
        "lastError": last_error,
        "startedAt": null,
        "exitedAt": null
    }))
    .into_response()
}

fn allowed_launcher(value: &str) -> bool {
    matches!(
        value,
        "node" | "npx" | "uvx" | "python" | "python3" | "bun" | "deno"
    )
}
fn valid_name(value: &str) -> bool {
    let b = value.as_bytes();
    (2..=63).contains(&b.len())
        && b[0].is_ascii_lowercase()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-' || *c == b'_')
}

fn parse_string_list(value: Option<&str>) -> Vec<String> {
    value
        .and_then(|v| serde_json::from_str(v).ok())
        .unwrap_or_default()
}
fn parse_env(value: Option<&str>) -> HashMap<String, String> {
    value
        .and_then(|v| serde_json::from_str(v).ok())
        .unwrap_or_default()
}
fn internal_error() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({"error":"Internal server error"})),
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
    lifecycle_log(&state, format!("MCP local process logs requested for '{name}'"));
    let logs = state
        .inner
        .mcp_process_logs
        .lock()
        .await
        .get(&name)
        .cloned()
        .unwrap_or_default();
    Json(serde_json::json!({"data":logs})).into_response()
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

    #[tokio::test]
    async fn process_logs_returns_stored_logs_without_invented_status_fields() {
        let state = AppState::new(Some("admin".into()), Default::default());
        state
            .inner
            .mcp_process_logs
            .lock()
            .await
            .insert("example".into(), vec!["started".into()]);
        let response = router()
            .with_state(state)
            .oneshot(
                Request::get("/v0/management/mcp-servers/example/process-logs")
                    .header("x-admin-key", "admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body: Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(body, serde_json::json!({"data":["started"]}));
    }
}

#[cfg(test)]
mod stop_contract_tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    #[tokio::test]
    async fn stop_is_idempotent_and_reports_reference_runtime_shape() {
        let state = AppState::new(Some("admin".into()), Default::default());
        for _ in 0..2 {
            let response = router().with_state(state.clone()).oneshot(Request::post("/v0/management/mcp-servers/absent/stop").header("x-admin-key","admin").body(Body::empty()).unwrap()).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body: serde_json::Value = serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
            assert_eq!(body["serverName"], "absent");
            assert_eq!(body["status"], "stopped");
            assert!(chrono::DateTime::parse_from_rfc3339(body["exitedAt"].as_str().unwrap()).is_ok());
            assert_eq!(body.as_object().unwrap().len(), 7);
        }
    }
}

#[cfg(test)]
mod lifecycle_log_tests {
    use super::*;
    #[test]
    fn management_actions_emit_reference_module_and_messages() {
        let state=AppState::new(Some("admin".into()),Default::default());
        lifecycle_log(&state,"MCP local start requested for 'fixture'".into());
        let logs=state.inner.recent_system_logs.lock().unwrap();
        assert_eq!(logs.back().unwrap().message,"MCP local start requested for 'fixture'");
        assert_eq!(logs.back().unwrap().context["module"],"config");
    }
}
