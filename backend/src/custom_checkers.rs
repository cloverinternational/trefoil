//! CRUD management for administrator-authored quota checker definitions.

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json, Router,
};
use serde::Deserialize;
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::{timeout, Duration};

use crate::{
    auth::{resolve_principal, Principal},
    AppState,
};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CheckerInput {
    display_name: Option<String>,
    code: Option<String>,
    enabled: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CheckerPatch {
    display_name: Option<String>,
    code: Option<String>,
    enabled: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct TestInput {
    provider: Option<String>,
    options: Option<Value>,
    code: Option<String>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/v0/management/custom-checkers", axum::routing::get(list))
        .route(
            "/v0/management/custom-checkers/:id",
            axum::routing::get(get)
                .put(put)
                .patch(patch)
                .delete(delete)
                .post(test),
        )
        .route(
            "/v0/management/custom-checkers/:id/check",
            axum::routing::post(check),
        )
}

fn auth(state: &AppState, headers: &HeaderMap) -> Result<(), Response> {
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

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(serde_json::json!({"error": message.into()}))).into_response()
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

fn valid_text(value: &str, field: &str) -> Result<(), Response> {
    if value.trim().is_empty() {
        return Err(error(
            StatusCode::BAD_REQUEST,
            format!("{field} must not be empty"),
        ));
    }
    if value.len() > 64 * 1024 {
        return Err(error(
            StatusCode::BAD_REQUEST,
            format!("{field} is too long"),
        ));
    }
    Ok(())
}

fn builtin(id: &str) -> bool {
    crate::config_routes::checker_types()
        .as_array()
        .is_some_and(|types| types.iter().any(|t| t["type"] == id))
}

fn row_json(row: &sqlx::postgres::PgRow) -> Value {
    use sqlx::Row;
    let created: i64 = row.try_get("created_at").unwrap_or_default();
    let updated: i64 = row.try_get("updated_at").unwrap_or_default();
    serde_json::json!({
        "id": row.try_get::<String, _>("id").unwrap_or_default(),
        "type": row.try_get::<String, _>("id").unwrap_or_default(),
        "displayName": row.try_get::<String, _>("display_name").unwrap_or_default(),
        "code": row.try_get::<String, _>("code").unwrap_or_default(),
        "enabled": row.try_get::<bool, _>("enabled").unwrap_or(true),
        "createdAt": chrono::DateTime::<chrono::Utc>::from_timestamp_millis(created).map(|d| d.to_rfc3339()),
        "updatedAt": chrono::DateTime::<chrono::Utc>::from_timestamp_millis(updated).map(|d| d.to_rfc3339())
    })
}

async fn list(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(r) = auth(&state, &headers) {
        return r;
    }
    let Some(pool) = &state.inner.database else {
        return Json(Vec::<Value>::new()).into_response();
    };
    match sqlx::query("SELECT id,display_name,code,enabled,created_at,updated_at FROM custom_checkers ORDER BY id")
        .fetch_all(pool).await {
        Ok(rows) => Json(rows.iter().map(row_json).collect::<Vec<_>>()).into_response(),
        Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to retrieve custom checkers"),
    }
}

async fn get(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(r) = auth(&state, &headers) {
        return r;
    }
    let Some(pool) = &state.inner.database else {
        return error(StatusCode::NOT_FOUND, "Custom checker not found");
    };
    match sqlx::query("SELECT id,display_name,code,enabled,created_at,updated_at FROM custom_checkers WHERE id=$1").bind(&id).fetch_optional(pool).await {
        Ok(Some(row)) => Json(row_json(&row)).into_response(),
        Ok(None) => error(StatusCode::NOT_FOUND, "Custom checker not found"),
        Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to retrieve custom checker"),
    }
}

async fn put(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<CheckerInput>,
) -> Response {
    if let Err(r) = auth(&state, &headers) {
        return r;
    }
    if !valid_id(&id) {
        return error(StatusCode::BAD_REQUEST, "Invalid checker id");
    }
    if builtin(&id) {
        return error(
            StatusCode::CONFLICT,
            "Checker id collides with a built-in checker",
        );
    }
    let Some(name) = body.display_name.as_deref() else {
        return error(StatusCode::BAD_REQUEST, "displayName is required");
    };
    let Some(code) = body.code.as_deref() else {
        return error(StatusCode::BAD_REQUEST, "code is required");
    };
    if let Err(r) = valid_text(name, "displayName").and(valid_text(code, "code")) {
        return r;
    }
    let Some(pool) = &state.inner.database else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "PostgreSQL is required");
    };
    let now = chrono::Utc::now().timestamp_millis();
    match sqlx::query("INSERT INTO custom_checkers (id,display_name,code,enabled,created_at,updated_at) VALUES ($1,$2,$3,$4,$5,$5) ON CONFLICT(id) DO UPDATE SET display_name=EXCLUDED.display_name,code=EXCLUDED.code,enabled=EXCLUDED.enabled,updated_at=EXCLUDED.updated_at RETURNING id,display_name,code,enabled,created_at,updated_at")
        .bind(&id).bind(name).bind(code).bind(body.enabled.unwrap_or(true)).bind(now).fetch_one(pool).await {
        Ok(row) => Json(row_json(&row)).into_response(), Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to save custom checker")
    }
}

async fn patch(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<CheckerPatch>,
) -> Response {
    if let Err(r) = auth(&state, &headers) {
        return r;
    }
    if !valid_id(&id) {
        return error(StatusCode::BAD_REQUEST, "Invalid checker id");
    }
    let Some(pool) = &state.inner.database else {
        return error(StatusCode::NOT_FOUND, "Custom checker not found");
    };
    let Some(existing) =
        sqlx::query("SELECT display_name,code,enabled FROM custom_checkers WHERE id=$1")
            .bind(&id)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten()
    else {
        return error(StatusCode::NOT_FOUND, "Custom checker not found");
    };
    use sqlx::Row;
    let name = body
        .display_name
        .as_deref()
        .unwrap_or_else(|| existing.try_get("display_name").unwrap_or(""));
    let code = body
        .code
        .as_deref()
        .unwrap_or_else(|| existing.try_get("code").unwrap_or(""));
    if let Err(r) = valid_text(name, "displayName").and(valid_text(code, "code")) {
        return r;
    }
    let enabled = body
        .enabled
        .unwrap_or_else(|| existing.try_get("enabled").unwrap_or(true));
    let now = chrono::Utc::now().timestamp_millis();
    match sqlx::query("UPDATE custom_checkers SET display_name=$2,code=$3,enabled=$4,updated_at=$5 WHERE id=$1 RETURNING id,display_name,code,enabled,created_at,updated_at").bind(&id).bind(name).bind(code).bind(enabled).bind(now).fetch_one(pool).await {
        Ok(row) => Json(row_json(&row)).into_response(), Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to update custom checker")
    }
}

async fn delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(r) = auth(&state, &headers) {
        return r;
    }
    let Some(pool) = &state.inner.database else {
        return error(StatusCode::NOT_FOUND, "Custom checker not found");
    };
    match sqlx::query("DELETE FROM custom_checkers WHERE id=$1")
        .bind(&id)
        .execute(pool)
        .await
    {
        Ok(result) if result.rows_affected() > 0 => {
            Json(serde_json::json!({"success":true,"id":id})).into_response()
        }
        Ok(_) => error(StatusCode::NOT_FOUND, "Custom checker not found"),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to delete custom checker",
        ),
    }
}

async fn test(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<TestInput>,
) -> Response {
    if let Err(r) = auth(&state, &headers) {
        return r;
    }
    let Some(provider) = body.provider.as_deref().filter(|p| !p.trim().is_empty()) else {
        return error(StatusCode::BAD_REQUEST, "provider is required");
    };
    let code = if let Some(code) = body.code.as_deref() {
        code
    } else {
        let Some(pool) = &state.inner.database else {
            return error(StatusCode::NOT_FOUND, "Custom checker not found");
        };
        match sqlx::query_scalar::<_, String>("SELECT code FROM custom_checkers WHERE id=$1").bind(&id).fetch_optional(pool).await { Ok(Some(code)) => return Json(serde_json::json!({"success":true,"provider":provider,"meters":[],"codeValid":validate_code(&code)})).into_response(), Ok(None) => return error(StatusCode::NOT_FOUND, "Custom checker not found"), Err(_) => return error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to retrieve custom checker") }
    };
    if let Err(r) = valid_text(code, "code") {
        return r;
    }
    Json(serde_json::json!({"success":true,"provider":provider,"meters":[],"codeValid":validate_code(code)})).into_response()
}

async fn check(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<TestInput>,
) -> Response {
    if let Err(r) = auth(&state, &headers) {
        return r;
    }
    let Some(provider) = body.provider.as_deref().filter(|p| !p.trim().is_empty()) else {
        return error(StatusCode::BAD_REQUEST, "provider is required");
    };
    let code = if let Some(code) = body.code {
        code
    } else {
        let Some(pool) = &state.inner.database else {
            return error(StatusCode::NOT_FOUND, "Custom checker not found");
        };
        match sqlx::query_scalar::<_, String>(
            "SELECT code FROM custom_checkers WHERE id=$1 AND enabled=true",
        )
        .bind(&id)
        .fetch_optional(pool)
        .await
        {
            Ok(Some(code)) => code,
            Ok(None) => {
                return error(
                    StatusCode::NOT_FOUND,
                    "Custom checker not found or disabled",
                )
            }
            Err(_) => {
                return error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to retrieve custom checker",
                )
            }
        }
    };
    if let Err(r) = valid_text(&code, "code") {
        return r;
    }
    if !validate_code(&code) {
        return error(
            StatusCode::BAD_REQUEST,
            "Checker code contains forbidden operations",
        );
    }
    let input =
        serde_json::json!({"provider": provider, "options": body.options.unwrap_or(Value::Null)});
    match run_checker(&code, &input).await {
        Ok(meters) => Json(serde_json::json!({"success":true,"provider":provider,"meters":meters}))
            .into_response(),
        Err(message) => Json(
            serde_json::json!({"success":false,"provider":provider,"meters":[],"error":message}),
        )
        .into_response(),
    }
}

async fn run_checker(code: &str, input: &Value) -> Result<Value, String> {
    const SCRIPT: &str = r#"
const vm = require('vm'); let raw=''; process.stdin.setEncoding('utf8');
process.stdin.on('data', c => raw += c); process.stdin.on('end', async () => {
 try { const i=JSON.parse(raw); const s=Object.freeze({provider:i.provider,options:i.options});
 const f=vm.runInNewContext(`(async function(){ ${i.code}\n})`,s,{timeout:900});
 const r=await f(); if(!Array.isArray(r)) throw new Error('checker must return an array');
 process.stdout.write(JSON.stringify(r)); } catch(e) { process.stderr.write(String(e&&e.message||e)); process.exitCode=1; }
});"#;
    let payload =
        serde_json::json!({"provider":input["provider"],"options":input["options"],"code":code});
    let mut child = Command::new("node")
        .args(["--no-addons", "--disable-proto=throw", "-e", SCRIPT])
        .env_clear()
        .kill_on_drop(true)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to start checker runtime: {e}"))?;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .await
        .map_err(|e| format!("failed to send checker input: {e}"))?;
    let output = timeout(Duration::from_secs(2), child.wait_with_output())
        .await
        .map_err(|_| "checker timed out".to_string())?
        .map_err(|e| format!("checker runtime failed: {e}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr)
            .chars()
            .take(512)
            .collect());
    }
    serde_json::from_slice(&output.stdout).map_err(|_| "checker returned invalid JSON".to_string())
}

fn validate_code(code: &str) -> bool {
    let trimmed = code.trim();
    !trimmed.is_empty()
        && trimmed.len() <= 64 * 1024
        && !trimmed.contains("require(")
        && !trimmed.contains("process.exit")
        && !trimmed.contains("process.")
        && !trimmed.contains("require")
        && !trimmed.contains("import(")
        && !trimmed.contains("eval(")
        && !trimmed.contains("Function(")
        && !trimmed.contains("constructor")
        && !trimmed.contains("globalThis")
        && !trimmed.contains("__proto__")
        && !trimmed.contains("prototype")
}
