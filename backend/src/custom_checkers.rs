//! CRUD management for administrator-authored quota checker definitions.

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json, Router,
};
use reqwest::Client;
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
            axum::routing::get(get).put(put).patch(patch).delete(delete),
        )
        .route(
            "/v0/management/custom-checkers/:id/test",
            axum::routing::post(test),
        )
        .route(
            "/v0/management/custom-checkers/:id/check",
            axum::routing::post(check),
        )
}

/// Poll enabled provider custom checkers in the background.
pub fn start_poller(state: AppState) {
    tokio::spawn(async move {
        let mut last_checked =
            std::collections::HashMap::<(String, String), tokio::time::Instant>::new();
        loop {
            let providers = state.inner.providers.read().unwrap().clone();
            let delay = Duration::from_secs(1);
            for (provider, config) in providers {
                if !config.enabled {
                    continue;
                }
                let Some(quota) = config.quota_checker.filter(|q| q.enabled) else {
                    continue;
                };
                let Some(checker_id) = quota.id.or(quota.checker_type) else {
                    continue;
                };
                let schedule_key = (provider.clone(), checker_id.clone());
                let now = tokio::time::Instant::now();
                if !checker_due(
                    last_checked.get(&schedule_key).copied(),
                    now,
                    quota.interval,
                ) {
                    continue;
                }
                last_checked.insert(schedule_key, now);
                let result = if builtin(&checker_id) {
                    run_builtin_checker(&state, &checker_id, &provider, &quota.options).await
                } else {
                    let Some(pool) = &state.inner.database else {
                        continue;
                    };
                    let Ok(Some(code)) = sqlx::query_scalar::<_, String>(
                        "SELECT code FROM custom_checkers WHERE id=$1 AND enabled=true",
                    )
                    .bind(&checker_id)
                    .fetch_optional(pool)
                    .await
                    else {
                        continue;
                    };
                    let input = serde_json::json!({"provider": provider, "options": quota.options});
                    run_checker(&code, &input).await
                };
                match result {
                    Ok(meters) => {
                        persist_result(&state, &checker_id, &provider, &meters, None).await
                    }
                    Err(message) => {
                        persist_result(
                            &state,
                            &checker_id,
                            &provider,
                            &Value::Array(vec![]),
                            Some(&message),
                        )
                        .await
                    }
                }
            }
            tokio::time::sleep(delay).await;
        }
    });
}

fn checker_due(
    last: Option<tokio::time::Instant>,
    now: tokio::time::Instant,
    minutes: u64,
) -> bool {
    last.is_none_or(|last| {
        now.saturating_duration_since(last)
            >= Duration::from_secs(minutes.max(1).saturating_mul(60))
    })
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
    let message = message.into();
    if status == StatusCode::FORBIDDEN || status == StatusCode::UNAUTHORIZED {
        let (kind, code) = if status == StatusCode::FORBIDDEN {
            ("forbidden", 403)
        } else {
            ("auth_error", 401)
        };
        return (
            status,
            Json(serde_json::json!({"error":{"message":message,"type":kind,"code":code}})),
        )
            .into_response();
    }
    (status, Json(serde_json::json!({"error": message}))).into_response()
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
        Ok(None) => error(StatusCode::NOT_FOUND, format!("Custom checker '{id}' not found")),
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
    if !validate_code(code) {
        return error(
            StatusCode::BAD_REQUEST,
            "Checker code contains forbidden operations",
        );
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
    let options = body.options.unwrap_or(Value::Null);
    if builtin(&id) {
        let result = run_builtin_checker(&state, &id, provider, &options).await;
        match result {
            Ok(meters) => {
                return Json(serde_json::json!({"success":true,"checkerId":id,"provider":provider,"meters":meters})).into_response();
            }
            Err(message) => {
                return Json(serde_json::json!({"success":false,"checkerId":id,"provider":provider,"meters":[],"error":message})).into_response();
            }
        }
    }
    let code = if let Some(code) = body.code {
        code
    } else {
        let Some(pool) = &state.inner.database else {
            return error(StatusCode::NOT_FOUND, "Custom checker not found");
        };
        match sqlx::query_scalar::<_, String>("SELECT code FROM custom_checkers WHERE id=$1")
            .bind(&id)
            .fetch_optional(pool)
            .await
        {
            Ok(Some(code)) => code,
            Ok(None) => return error(StatusCode::NOT_FOUND, "Custom checker not found"),
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
    let input = serde_json::json!({
        "provider": provider,
        "options": options,
    });
    if !state.inner.providers.read().unwrap().contains_key(provider) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "success": false,
                "error": "provider is not defined",
                "errorType": "runtime"
            })),
        )
            .into_response();
    }
    match run_checker(&code, &input).await {
        Ok(meters) => Json(serde_json::json!({
            "success": true,
            "checkerId": id,
            "provider": provider,
            "meters": meters,
        }))
        .into_response(),
        Err(message) => Json(serde_json::json!({
            "success": false,
            "provider": provider,
            "meters": [],
            "codeValid": true,
            "error": message,
        }))
        .into_response(),
    }
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
    let options = body.options.unwrap_or(Value::Null);
    if builtin(&id) {
        return match run_builtin_checker(&state, &id, provider, &options).await {
            Ok(meters) => {
                persist_result(&state, &id, provider, &meters, None).await;
                Json(serde_json::json!({"success":true,"checkerId":id,"provider":provider,"meters":meters})).into_response()
            }
            Err(message) => {
                persist_result(&state, &id, provider, &Value::Array(vec![]), Some(&message)).await;
                Json(serde_json::json!({"success":false,"checkerId":id,"provider":provider,"meters":[],"error":message})).into_response()
            }
        };
    }
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
                    format!("Quota checker '{id}' not found"),
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
    let input = serde_json::json!({"provider": provider, "options": options});
    match run_checker(&code, &input).await {
        Ok(meters) => {
            persist_result(&state, &id, provider, &meters, None).await;
            Json(serde_json::json!({"success":true,"checkerId":id,"provider":provider,"meters":meters})).into_response()
        }
        Err(message) => {
            persist_result(&state, &id, provider, &Value::Array(vec![]), Some(&message)).await;
            Json(serde_json::json!({"success":false,"checkerId":id,"provider":provider,"meters":[],"error":message})).into_response()
        }
    }
}

async fn run_builtin_checker(
    state: &AppState,
    id: &str,
    provider: &str,
    _options: &Value,
) -> Result<Value, String> {
    let (endpoint, oauth_provider) = match id {
        "claude-code" => ("https://api.anthropic.com/api/oauth/usage", "anthropic"),
        "openai-codex" => ("https://chatgpt.com/backend-api/wham/usage", "openai-codex"),
        _ => {
            return Err(format!(
                "built-in checker '{id}' has no upstream implementation"
            ))
        }
    };
    let config = state
        .inner
        .providers
        .read()
        .unwrap()
        .get(provider)
        .cloned()
        .ok_or_else(|| format!("provider '{provider}' is not defined"))?;
    if config.oauth_provider.as_deref().unwrap_or(oauth_provider) != oauth_provider {
        return Err(format!(
            "provider '{provider}' is not configured for {oauth_provider}"
        ));
    }
    let account = config.oauth_account.as_deref().unwrap_or("default");
    let pool = state
        .inner
        .database
        .as_ref()
        .ok_or("PostgreSQL is required for OAuth quota checks")?;
    let token = crate::database::load_oauth_credentials(pool)
        .await
        .map_err(|_| "failed to load OAuth credentials".to_string())?
        .into_iter()
        .find(|(provider, candidate, _, _, _)| provider == oauth_provider && candidate == account)
        .filter(|(_, _, _, _, expires)| *expires > chrono::Utc::now().timestamp_millis())
        .map(|(_, _, access, _, _)| access)
        .ok_or_else(|| format!("OAuth credentials not found for {oauth_provider}/{account}"))?;
    fetch_builtin_meters(id, endpoint, &token).await
}

async fn fetch_builtin_meters(id: &str, endpoint: &str, token: &str) -> Result<Value, String> {
    let mut request = Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|_| "failed to initialize quota client".to_string())?
        .get(endpoint)
        .bearer_auth(token);
    if id == "claude-code" {
        request = request.header("anthropic-beta", "oauth-2025-04-20");
    } else if id == "openai-codex" {
        request = request.header("content-type","application/json")
            .header("user-agent",crate::codex_masking::user_agent())
            .header("version",crate::codex_masking::CODEX_VERSION);
        if let Some(account)=crate::codex_masking::account_id(token) { request=request.header("chatgpt-account-id",account); }
    }
    let response = request.send().await.map_err(|e| e.to_string())?;
    let status = response.status();
    let payload: Value = response.json().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("quota request failed with status {status}"));
    }
    if id == "openai-codex" && payload.get("rate_limit").is_none_or(Value::is_null) {
        return Err("codex usage response missing rate_limit".into());
    }
    Ok(builtin_meters(id, &payload))
}

fn reset_iso(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|date| date.to_utc().to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}

fn allowance(key: &str, label: &str, used: f64, hours: bool, reset: Option<String>, scope: Option<&str>) -> Value {
    let remaining=(100.0-used).max(0.0);
    let percent=(used/(used+remaining)*100.0).min(100.0);
    let mut meter=serde_json::json!({"key":key,"label":label,"kind":"allowance","unit":"percentage","used":used,"remaining":remaining,"utilizationPercent":percent,"status":meter_status(percent),"periodValue":if hours {5} else {7},"periodUnit":if hours {"hour"} else {"day"},"periodCycle":"rolling"});
    if let Some(reset)=reset { meter["resetsAt"]=Value::String(reset); }
    if let Some(scope)=scope { meter["scope"]=Value::String(scope.into()); }
    meter
}

fn normalized_scope(name: &str) -> String {
    let mut normalized=String::new();
    for character in name.to_ascii_lowercase().chars() {
        if character.is_ascii_alphanumeric() { normalized.push(character); }
        else if !normalized.ends_with('_') { normalized.push('_'); }
    }
    normalized.trim_matches('_').into()
}

fn builtin_meters(id: &str, payload: &Value) -> Value {
    let mut meters=Vec::new();
    if id == "claude-code" {
        let fields=[("five_hour","5-hour quota","five_hour"),("weekly","Weekly quota","seven_day")];
        // Reference schema rejects the whole response when a legacy window is malformed.
        for field in ["five_hour","seven_day","seven_day_opus","seven_day_omelette"] {
            if payload.get(field).is_some_and(|v| !v.is_null() && (v.get("utilization").and_then(Value::as_f64).is_none() || v.get("resets_at").is_some_and(|r|!r.is_null()&&!r.is_string()))) { return Value::Array(vec![]); }
        }
        for (key,label,field) in fields {
            if let Some(window)=payload.get(field) {
                if let Some(used)=window["utilization"].as_f64() { meters.push(allowance(key,label,used,key=="five_hour",reset_iso(window.get("resets_at")),None)); }
            }
        }
        // (dimension, id, display name, utilization, reset)
        let mut scoped:Vec<(String,Option<String>,String,Option<f64>,Option<String>)>=Vec::new();
        for (field,name) in [("seven_day_opus","Opus"),("seven_day_omelette","Omelette")] {
            if let Some(used)=payload.get(field).and_then(|v|v["utilization"].as_f64()) {
                scoped.push(("model".into(),None,name.into(),Some(used),reset_iso(payload[field].get("resets_at"))));
            }
        }
        if let Some(limits)=payload.get("limits").and_then(Value::as_array) {
            for limit in limits {
                if limit["kind"]!="weekly_scoped" || limit.get("percent").is_some_and(|v|!v.is_null()&&!v.is_number()) {continue;}
                for dimension in ["model","surface"] {
                    let entry=&limit["scope"][dimension];
                    let scope_id=entry["id"].as_str().map(str::trim).filter(|s|!s.is_empty()).map(str::to_owned);
                    let name=entry["display_name"].as_str().map(str::trim).filter(|s|!s.is_empty()).map(str::to_owned).or_else(||scope_id.clone());
                    let Some(name)=name else {continue};
                    let mut next=(dimension.to_owned(),scope_id,name,limit["percent"].as_f64(),reset_iso(limit.get("resets_at")));
                    let index=scoped.iter().position(|old|old.0==next.0 && match (&old.1,&next.1) {(Some(a),Some(b))=>a==b,_=>normalized_scope(&old.2)==normalized_scope(&next.2)});
                    if let Some(index)=index {next.3=next.3.or(scoped[index].3);next.4=next.4.or(scoped[index].4.clone());scoped[index]=next;}
                    else {scoped.push(next);}
                    break;
                }
            }
        }
        let mut taken=std::collections::HashSet::new();
        for (dimension,id,name,used,reset) in scoped {
            let base=format!("weekly_{dimension}_{}",id.unwrap_or_else(||normalized_scope(&name)));
            let mut key=base.clone();let mut suffix=2;
            while !taken.insert(key.clone()) {key=format!("{base}_{suffix}");suffix+=1;}
            meters.push(allowance(&key,&format!("Weekly · {name}"),used.unwrap_or(0.0),false,reset,Some(&dimension)));
        }
    } else if let Some(rate)=payload.get("rate_limit") {
        if rate["limit_reached"]==true { return Value::Array(vec![allowance("primary","Primary rate limit",100.0,true,None,None)]); }
        for (key,label) in [("primary","Primary rate limit"),("secondary","Secondary rate limit")] {
            if let Some(window)=rate.get(format!("{key}_window")).filter(|v|!v.is_null()) {
                let used=window["used_percent"].as_f64().unwrap_or(0.0).clamp(0.0,100.0);
                let reset=window["reset_at"].as_i64().filter(|v|*v>0).and_then(|v|chrono::DateTime::from_timestamp(v,0)).map(|d|d.to_rfc3339_opts(chrono::SecondsFormat::Millis,true));
                meters.push(allowance(key,label,used,window["limit_window_seconds"].as_u64()==Some(18000),reset,None));
            }
        }
    }
    Value::Array(meters)
}

fn meter_status(percent: f64) -> &'static str {
    if percent >= 100.0 {
        "exhausted"
    } else if percent >= 90.0 {
        "critical"
    } else if percent >= 75.0 {
        "warning"
    } else {
        "ok"
    }
}

async fn persist_result(
    state: &AppState,
    checker_id: &str,
    provider: &str,
    meters: &Value,
    error: Option<&str>,
) {
    let Some(pool) = &state.inner.database else {
        return;
    };
    let now = chrono::Utc::now().timestamp_millis();
    if let Some(message) = error {
        let _ = sqlx::query("INSERT INTO meter_snapshots (checker_id,checker_type,provider,meter_key,kind,unit,label,utilization_state,status,success,error_message,checked_at,created_at) VALUES ($1,$1,$2,'_error','unknown','unknown','error','unknown','error',false,$3,$4,$4)")
            .bind(checker_id).bind(provider).bind(message).bind(now).execute(pool).await;
        return;
    }
    let values = meters.as_array().cloned().unwrap_or_default();
    for meter in values {
        let key = meter.get("key").and_then(Value::as_str).unwrap_or("meter");
        let kind = meter.get("kind").and_then(Value::as_str).unwrap_or("quota");
        let unit = meter
            .get("unit")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let label = meter.get("label").and_then(Value::as_str).unwrap_or(key);
        let _ = sqlx::query("INSERT INTO meter_snapshots (checker_id,checker_type,provider,meter_key,kind,unit,label,\"group\",scope,\"limit\",used,remaining,utilization_state,utilization_percent,status,period_value,period_unit,period_cycle,resets_at,success,error_message,checked_at,created_at) VALUES ($1,$1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,true,NULL,$19,$19)")
            .bind(checker_id).bind(provider).bind(key).bind(kind).bind(unit).bind(label)
            .bind(meter.get("group").and_then(Value::as_str))
            .bind(meter.get("scope").and_then(Value::as_str))
            .bind(meter.get("limit").and_then(Value::as_f64).map(|v| v as f32))
            .bind(meter.get("used").and_then(Value::as_f64).map(|v| v as f32))
            .bind(meter.get("remaining").and_then(Value::as_f64).map(|v| v as f32))
            .bind(if meter.get("utilizationPercent").is_some_and(Value::is_number) { "reported" } else { meter.get("utilizationPercent").and_then(Value::as_str).unwrap_or("unknown") })
            .bind(meter.get("utilizationPercent").and_then(Value::as_f64).map(|v| v as f32))
            .bind(meter.get("status").and_then(Value::as_str).unwrap_or("ok"))
            .bind(meter.get("periodValue").and_then(Value::as_i64).map(|v| v as i32))
            .bind(meter.get("periodUnit").and_then(Value::as_str))
            .bind(meter.get("periodCycle").and_then(Value::as_str))
            .bind(meter.get("resetsAt").and_then(|value| value.as_i64().or_else(|| value.as_str().and_then(|s|chrono::DateTime::parse_from_rfc3339(s).ok()).map(|dt|dt.timestamp_millis())))).bind(now).execute(pool).await;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn checker_runtime_receives_provider_and_options_and_returns_meters() {
        let code = r#"
            return [{ key: provider, used: options.used, status: 'ok' }];
        "#;
        let input = serde_json::json!({
            "provider": "acme",
            "options": { "used": 42 }
        });

        let result = run_checker(code, &input).await.unwrap();

        assert_eq!(
            result,
            serde_json::json!([{ "key": "acme", "used": 42, "status": "ok" }])
        );
    }

    #[tokio::test]
    async fn checker_runtime_rejects_non_array_results() {
        let error = run_checker("return { key: 'balance' };", &serde_json::json!({})).await;

        assert_eq!(error.unwrap_err(), "checker must return an array");
    }

    #[test]
    fn checker_code_validation_rejects_runtime_escape_hatches() {
        for code in [
            "return require('fs');",
            "return process.env.SECRET;",
            "return eval('[]');",
            "return globalThis;",
            "return ({ }).constructor;",
        ] {
            assert!(!validate_code(code), "accepted unsafe checker code: {code}");
        }
        assert!(validate_code("return [{ key: 'balance', status: 'ok' }];"));
    }
}

#[cfg(test)]
mod builtin_tests {
    use super::*;
    use axum::{routing::get, Router};

    #[tokio::test]
    async fn claude_checker_sends_oauth_headers_and_maps_upstream_usage() {
        let app = Router::new().route("/usage", get(|headers: HeaderMap| async move {
            assert_eq!(headers["authorization"], "Bearer fixture-token");
            assert_eq!(headers["anthropic-beta"], "oauth-2025-04-20");
            Json(serde_json::json!({"five_hour":{"utilization":80,"resets_at":"2026-09-19T00:00:00Z"},"seven_day":{"utilization":100}}))
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let result = fetch_builtin_meters(
            "claude-code",
            &format!("http://{address}/usage"),
            "fixture-token",
        )
        .await
        .unwrap();
        server.abort();
        assert_eq!(result[0]["kind"], "allowance");
        assert_eq!(result[0]["periodValue"], 5);
        assert_eq!(result[0]["status"], "warning");
        assert_eq!(result[0]["remaining"], 20.0);
        assert_eq!(result[1]["status"], "exhausted");
    }

    #[tokio::test]
    async fn checker_upstream_failure_is_not_successful_empty_usage() {
        let app = Router::new().route(
            "/usage",
            get(|| async {
                (
                    StatusCode::UNAUTHORIZED,
                    Json(serde_json::json!({"error":"invalid token"})),
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let result = fetch_builtin_meters(
            "openai-codex",
            &format!("http://{address}/usage"),
            "fixture-token",
        )
        .await;
        server.abort();
        assert!(result.unwrap_err().contains("401"));
    }

    #[test]
    fn codex_windows_clamp_percentages_and_preserve_period_units() {
        let result = builtin_meters(
            "openai-codex",
            &serde_json::json!({"rate_limit":{
                "primary_window":{"used_percent":-2,"limit_window_seconds":18000},
                "secondary_window":{"used_percent":120,"limit_window_seconds":604800}
            }}),
        );
        assert_eq!(result[0]["used"], 0.0);
        assert_eq!(result[0]["periodUnit"], "hour");
        assert_eq!(result[1]["remaining"], 0.0);
        assert_eq!(result[1]["periodUnit"], "day");
    }
}

#[cfg(test)]
mod scheduling_tests {
    use super::*;
    #[test]
    fn each_checker_obeys_its_own_minute_interval() {
        let start = tokio::time::Instant::now();
        assert!(checker_due(None, start, 30));
        assert!(!checker_due(
            Some(start),
            start + Duration::from_secs(30),
            30
        ));
        assert!(checker_due(Some(start), start + Duration::from_secs(60), 1));
        assert!(!checker_due(
            Some(start),
            start + Duration::from_secs(60),
            30
        ));
        assert!(checker_due(
            Some(start),
            start + Duration::from_secs(1800),
            30
        ));
    }
}

#[cfg(test)]
mod provider_window_tests {
    use super::*;
    #[test]
    fn claude_reconciles_legacy_scopes_with_new_limits_without_losing_usage() {
        let meters=builtin_meters("claude-code",&serde_json::json!({
            "five_hour":{"utilization":120,"resets_at":"bad date"},
            "seven_day_opus":{"utilization":82,"resets_at":"2026-09-18T12:00:00Z"},
            "limits":[{"kind":"weekly_scoped","percent":null,"scope":{"model":{"id":"opus-v2","display_name":"Opus"}}},
              {"kind":"weekly_scoped","percent":12,"scope":{"surface":{"id":"chat","display_name":"Chat"}}}]
        }));
        assert_eq!(meters.as_array().unwrap().len(),3);
        assert_eq!(meters[0]["used"],120.0);
        assert_eq!(meters[0]["utilizationPercent"],100.0);
        assert!(meters[0].get("resetsAt").is_none());
        assert_eq!(meters[1]["key"],"weekly_model_opus-v2");
        assert_eq!(meters[1]["used"],82.0);
        assert_eq!(meters[1]["resetsAt"],"2026-09-18T12:00:00.000Z");
        assert_eq!(meters[2]["scope"],"surface");
    }
    #[test]
    fn codex_exhaustion_overrides_windows_and_missing_percentage_is_zero() {
        let exhausted=builtin_meters("openai-codex",&serde_json::json!({"rate_limit":{"limit_reached":true,"primary_window":{"used_percent":1}}}));
        assert_eq!(exhausted[0]["used"],100.0);
        assert_eq!(exhausted[0]["status"],"exhausted");
        let windows=builtin_meters("openai-codex",&serde_json::json!({"rate_limit":{"primary_window":{"reset_at":1,"limit_window_seconds":18000}}}));
        assert_eq!(windows[0]["used"],0.0);
        assert_eq!(windows[0]["resetsAt"],"1970-01-01T00:00:01.000Z");
    }
    #[test]
    fn malformed_legacy_window_rejects_all_claude_meters() {
        assert_eq!(builtin_meters("claude-code",&serde_json::json!({"five_hour":{"utilization":1},"seven_day":{"utilization":"bad"}})),serde_json::json!([]));
    }
}

#[cfg(test)]
mod codex_header_tests {
    use super::*;
    use axum::routing::get;
    use base64::Engine;
    #[tokio::test]
    async fn codex_usage_request_carries_account_and_client_headers() {
        let token=format!("x.{}.x",base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(r#"{"https://api.openai.com/auth":{"chatgpt_account_id":"account-test"}}"#));
        let app=Router::new().route("/usage",get(|headers:HeaderMap|async move {
            assert_eq!(headers["chatgpt-account-id"],"account-test");
            assert_eq!(headers["version"],crate::codex_masking::CODEX_VERSION);
            assert!(headers["user-agent"].to_str().unwrap().contains("codex"));
            Json(serde_json::json!({"rate_limit":{"primary_window":{"used_percent":25,"limit_window_seconds":18000}}}))
        }));
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let addr=listener.local_addr().unwrap();
        let server=tokio::spawn(async move{axum::serve(listener,app).await.unwrap()});
        let result=fetch_builtin_meters("openai-codex",&format!("http://{addr}/usage"),&token).await.unwrap();
        assert_eq!(result[0]["remaining"],75.0);server.abort();
    }
}
