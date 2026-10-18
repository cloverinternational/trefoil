//! OAuth management routes — port of pinned Plexus
//! `routes/management/oauth.ts` on top of [`crate::oauth_login`].
//!
//! Plexus discovers its descriptors from the pinned pi-ai registry; Trefoil
//! mirrors that list (`oauth_login::PROVIDERS`) and must be refreshed when the
//! pinned dependency's OAuth provider set changes.

use crate::{
    auth::{resolve_principal, Principal},
    oauth_login::{database_store, OAuthSession},
    AppState,
};
use axum::{
    body::Bytes,
    extract::{Path, RawQuery, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json, Router,
};
use serde_json::{json, Map, Value};
use std::time::Duration;

/// Plexus `isKnownOAuthProviderId`.
pub(crate) fn is_supported_provider(provider: &str) -> bool {
    crate::oauth_login::PROVIDERS
        .iter()
        .any(|(id, _, _)| *id == provider)
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/v0/management/oauth/providers",
            axum::routing::get(providers),
        )
        .route(
            "/v0/management/oauth/sessions",
            axum::routing::post(create_session),
        )
        .route(
            "/v0/management/oauth/credentials",
            axum::routing::delete(delete_credentials),
        )
        .route(
            "/v0/management/oauth/credentials/status",
            axum::routing::get(credential_status),
        )
        .route(
            "/v0/management/oauth/sessions/:id",
            axum::routing::get(get_session),
        )
        .route(
            "/v0/management/oauth/sessions/:id/prompt",
            axum::routing::post(prompt_session),
        )
        .route(
            "/v0/management/oauth/sessions/:id/manual-code",
            axum::routing::post(manual_code),
        )
        .route(
            "/v0/management/oauth/sessions/:id/select",
            axum::routing::post(select_session),
        )
        .route(
            "/v0/management/oauth/sessions/:id/cancel",
            axum::routing::post(cancel_session),
        )
        .route("/v0/management/oauth/models", axum::routing::get(models))
}

#[allow(clippy::result_large_err)]
fn require_admin(state: &AppState, headers: &HeaderMap) -> Result<(), Response> {
    match resolve_principal(state, headers.get("x-admin-key").and_then(|value| value.to_str().ok())) {
        Some(Principal::Admin) => Ok(()),
        Some(Principal::Limited { .. }) => Err((StatusCode::FORBIDDEN, Json(json!({"error": {"message": "Admin privileges required", "type": "forbidden", "code": 403}}))).into_response()),
        None => Err((StatusCode::UNAUTHORIZED, Json(json!({"error": {"message": "Unauthorized", "type": "auth_error", "code": 401}}))).into_response()),
    }
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({"error": message.into()}))).into_response()
}

fn session_response(session: OAuthSession) -> Response {
    Json(json!({"data": session})).into_response()
}

/// The request body as zod sees it: `None` when absent (`undefined`).
fn body_value(body: &Bytes) -> Option<Value> {
    if body.is_empty() {
        return None;
    }
    serde_json::from_slice(body).ok()
}

/// `schema.safeParse(body)` for `z.object({...})`, including the
/// `undefined` body case that `zod::Schema::safe_parse` cannot express.
fn parse_body(schema: &crate::zod::Schema, body: &Bytes, label: &str) -> Result<Value, Response> {
    let Some(value) = body_value(body) else {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": label, "details": [{"expected": "object", "code": "invalid_type", "path": [], "message": "Invalid input: expected object, received undefined"}]})),
        )
            .into_response());
    };
    schema.safe_parse(&value).map_err(|details| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": label, "details": details})),
        )
            .into_response()
    })
}

/// Fastify query object (`fast-querystring`: repeated keys become arrays).
fn query_value(raw: Option<String>) -> Value {
    let mut out = Map::new();
    for (key, value) in url::form_urlencoded::parse(raw.unwrap_or_default().as_bytes()) {
        let value = Value::String(value.into_owned());
        match out.get_mut(key.as_ref()) {
            Some(Value::Array(items)) => items.push(value),
            Some(existing) => {
                let first = existing.take();
                *existing = Value::Array(vec![first, value]);
            }
            None => {
                out.insert(key.into_owned(), value);
            }
        }
    }
    Value::Object(out)
}

fn provider_account_schema() -> crate::zod::Schema {
    use crate::zod::*;
    object(vec![
        ("providerId", string().min(1.0)),
        ("accountId", string().min(1.0)),
    ])
}

fn input_schema() -> crate::zod::Schema {
    use crate::zod::*;
    object(vec![("value", string())])
}

async fn providers(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    let data: Vec<Value> = state
        .inner
        .oauth_sessions
        .list_providers()
        .into_iter()
        .map(|(id, name, cb)| json!({"id": id, "name": name, "usesCallbackServer": cb}))
        .collect();
    let total = data.len();
    Json(json!({"data": data, "total": total})).into_response()
}

async fn create_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    let parsed = match parse_body(&provider_account_schema(), &body, "Invalid request body") {
        Ok(parsed) => parsed,
        Err(response) => return response,
    };
    let provider = parsed["providerId"].as_str().unwrap_or_default();
    let account = parsed["accountId"].as_str().unwrap_or_default();
    match state
        .inner
        .oauth_sessions
        .create_session(provider, account, database_store(state.clone()))
        .await
    {
        Ok(session) => session_response(session),
        Err(message) => error(StatusCode::BAD_REQUEST, message),
    }
}

async fn get_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    match state.inner.oauth_sessions.get_session(&id) {
        Some(session) => session_response(session),
        None => error(StatusCode::NOT_FOUND, "OAuth session not found"),
    }
}

#[derive(Clone, Copy)]
enum InputKind {
    Prompt,
    ManualCode,
    Select,
}

async fn submit_input(
    state: AppState,
    headers: HeaderMap,
    id: String,
    body: Bytes,
    kind: InputKind,
) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    let parsed = match parse_body(&input_schema(), &body, "Invalid request body") {
        Ok(parsed) => parsed,
        Err(response) => return response,
    };
    let value = parsed["value"].as_str().unwrap_or_default().to_string();
    let manager = &state.inner.oauth_sessions;
    let result = match kind {
        InputKind::Prompt => manager.submit_prompt(&id, value).await,
        InputKind::ManualCode => manager.submit_manual_code(&id, value).await,
        InputKind::Select => manager.submit_select(&id, value).await,
    };
    match result {
        Ok(session) => session_response(session),
        Err(message) => error(StatusCode::BAD_REQUEST, message),
    }
}

async fn prompt_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    submit_input(state, headers, id, body, InputKind::Prompt).await
}

async fn manual_code(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    submit_input(state, headers, id, body, InputKind::ManualCode).await
}

async fn select_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    submit_input(state, headers, id, body, InputKind::Select).await
}

async fn cancel_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    match state.inner.oauth_sessions.cancel(&id).await {
        Ok(session) => session_response(session),
        Err(message) => error(StatusCode::BAD_REQUEST, message),
    }
}

async fn delete_credentials(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    let parsed = match parse_body(&provider_account_schema(), &body, "Invalid request body") {
        Ok(parsed) => parsed,
        Err(response) => return response,
    };
    let provider = parsed["providerId"].as_str().unwrap_or_default();
    let account = parsed["accountId"].as_str().unwrap_or_default();
    let not_found = || error(StatusCode::NOT_FOUND, "OAuth credentials not found");
    if account.trim().is_empty() {
        return not_found();
    }
    let Some(pool) = state.inner.database.as_ref() else {
        return not_found();
    };
    match crate::database::delete_oauth_credential(pool, provider, account).await {
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        Ok(false) => not_found(),
        Ok(true) => {
            state
                .inner
                .oauth_accounts
                .write()
                .unwrap()
                .retain(|(p, a)| !(p == provider && a == account));
            Json(json!({"data": {"deleted": true}})).into_response()
        }
    }
}

/// Plexus `resolveAccountId` over persisted accounts: an explicit account,
/// else `legacy`, else the provider's single account.
fn resolve_account(
    accounts: &[(String, String, i64)],
    provider: &str,
    account: Option<&str>,
) -> Option<String> {
    if let Some(account) = account.map(str::trim).filter(|a| !a.is_empty()) {
        return Some(account.to_string());
    }
    let mine: Vec<&String> = accounts
        .iter()
        .filter(|(p, _, _)| p == provider)
        .map(|(_, a, _)| a)
        .collect();
    if mine.is_empty() {
        return None;
    }
    if mine.iter().any(|a| a.as_str() == "legacy") {
        return Some("legacy".into());
    }
    (mine.len() == 1).then(|| mine[0].clone())
}

async fn credential_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    let parsed = match provider_account_schema().safe_parse(&query_value(raw)) {
        Ok(parsed) => parsed,
        Err(details) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "Invalid query parameters", "details": details})),
            )
                .into_response()
        }
    };
    let provider = parsed["providerId"].as_str().unwrap_or_default();
    let account = parsed["accountId"]
        .as_str()
        .unwrap_or_default()
        .trim()
        .to_string();
    let Some(pool) = state.inner.database.as_ref() else {
        return Json(json!({"data": {"ready": false}})).into_response();
    };
    let accounts = crate::database::list_oauth_credential_accounts(pool)
        .await
        .unwrap_or_default();
    let ready = if account.is_empty() {
        accounts.iter().any(|(p, _, _)| p == provider)
    } else {
        accounts
            .iter()
            .any(|(p, a, _)| p == provider && *a == account)
    };
    if !ready {
        return Json(json!({"data": {"ready": ready}})).into_response();
    }
    let timestamps = crate::database::oauth_credential_timestamps(pool, provider, &account)
        .await
        .ok()
        .flatten();
    let expires = resolve_account(&accounts, provider, Some(&account)).and_then(|resolved| {
        accounts
            .iter()
            .find(|(p, a, _)| p == provider && *a == resolved)
            .map(|(_, _, e)| *e)
    });
    let mut data = Map::new();
    data.insert("ready".into(), json!(true));
    if let Some((created, updated, _)) = timestamps {
        if created != 0 {
            data.insert("connectedAt".into(), json!(created));
        }
        if updated != 0 {
            data.insert("refreshedAt".into(), json!(updated));
        }
    }
    if let Some(expires) = expires.filter(|e| *e > 0) {
        data.insert("expiresAt".into(), json!(expires));
    }
    Json(json!({"data": data})).into_response()
}

// ─── /v0/management/oauth/models ─────────────────────────────────────────

const MUSE_STATIC_MODELS: [&str; 6] = [
    "muse-spark-1.1",
    "muse-spark-1.2",
    "muse-spark-1.2-contributor",
    "muse-spark-1.3",
    "muse-spark-1.3-contributor",
    "muse-wonder-1.0",
];
const CODEX_IMAGE_MODELS: [&str; 3] = [
    "gpt-image-2",
    "gpt-image-2.5-flare",
    "gpt-image-2.5-sunburst",
];

/// JavaScript `Number.prototype.toString` for catalog prices.
fn js_number(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e21 {
        return format!("{}", value as i64);
    }
    if value != 0.0 && value.abs() < 1e-6 {
        return format!("{value:e}");
    }
    format!("{value}")
}

/// Plexus `getOAuthProviderModels` over the embedded pi-ai catalog.
fn catalog_models(provider: &str) -> Vec<Value> {
    let catalog: Value =
        serde_json::from_str(include_str!("assets/pi-catalog.json")).unwrap_or(Value::Null);
    let Some(models) = catalog["providers"]
        .as_array()
        .and_then(|providers| providers.iter().find(|p| p["id"] == provider))
        .and_then(|p| p["models"].as_array())
    else {
        return vec![];
    };
    models
        .iter()
        .filter(|m| m["type"].as_str().unwrap_or("chat") == "chat")
        .map(|m| {
            let mut out = Map::new();
            out.insert("id".into(), m["id"].clone());
            if let Some(name) = m.get("name").filter(|v| !v.is_null()) {
                out.insert("name".into(), name.clone());
            }
            if let Some(context) = m.get("contextWindow").filter(|v| !v.is_null()) {
                out.insert("context_length".into(), context.clone());
            }
            if let (Some(input), Some(output)) =
                (m["cost"]["input"].as_f64(), m["cost"]["output"].as_f64())
            {
                out.insert(
                    "pricing".into(),
                    json!({"prompt": js_number(input), "completion": js_number(output)}),
                );
            }
            Value::Object(out)
        })
        .collect()
}

fn with_codex_image_models(mut models: Vec<Value>) -> Vec<Value> {
    for id in CODEX_IMAGE_MODELS {
        if !models.iter().any(|m| m["id"] == id) {
            models.push(json!({"id": id, "name": id, "description": "Codex Images (ChatGPT OAuth)", "type": "image", "access_via": ["codex-images"]}));
        }
    }
    models
}

/// Plexus `OAuthAuthManager.getApiKey` error semantics over persisted
/// credentials (no refresh is attempted here).
async fn oauth_api_key(
    state: &AppState,
    provider: &str,
    account: Option<&str>,
) -> Result<String, String> {
    let not_authenticated = format!("OAuth: Not authenticated for provider '{provider}'. Please run OAuth login for this provider.");
    let pool = state
        .inner
        .database
        .as_ref()
        .ok_or_else(|| not_authenticated.clone())?;
    let credentials = crate::database::load_oauth_credentials(pool)
        .await
        .map_err(|e| e.to_string())?;
    let accounts: Vec<(String, String, i64)> = credentials
        .iter()
        .filter(|(p, ..)| p == provider)
        .map(|(p, a, _, _, e)| (p.clone(), a.clone(), *e))
        .collect();
    if accounts.is_empty() {
        return Err(not_authenticated);
    }
    let resolved = resolve_account(&accounts, provider, account).ok_or_else(|| {
        format!("OAuth: accountId is required to resolve credentials for provider '{provider}'.")
    })?;
    credentials
        .into_iter()
        .find(|(p, a, ..)| p == provider && *a == resolved)
        .map(|(_, _, access, _, _)| access)
        .ok_or_else(|| format!("OAuth: Not authenticated for provider '{provider}' and account '{resolved}'. Please run OAuth login for this account."))
}

fn fetch_error(error: reqwest::Error) -> String {
    if error.is_timeout() {
        "The operation timed out.".into()
    } else {
        error.to_string()
    }
}

async fn list_codex_models(state: &AppState, account: Option<&str>) -> Result<Vec<Value>, String> {
    let token = oauth_api_key(state, "openai-codex", account).await?;
    let version = crate::cli_versions::codex_version();
    let url = format!(
        "https://chatgpt.com/backend-api/codex/models?client_version={}",
        url::form_urlencoded::byte_serialize(version.as_bytes())
            .collect::<String>()
            .replace('+', "%20")
    );
    let mut request = reqwest::Client::new()
        .get(url)
        .header("Accept", "application/json")
        .header("Authorization", format!("Bearer {token}"))
        .header("originator", "codex_cli_rs")
        .header("Version", version)
        .header("User-Agent", crate::codex_masking::user_agent())
        .timeout(Duration::from_secs(15));
    if let Some(account) = crate::codex_masking::account_id(&token) {
        request = request.header("chatgpt-account-id", account);
    }
    let response = request.send().await.map_err(fetch_error)?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!(
            "Codex model list returned {} {}",
            status.as_u16(),
            crate::oauth_login::status_text(status)
        ));
    }
    let body: Value = response.json().await.map_err(fetch_error)?;
    let records = body["models"]
        .as_array()
        .ok_or("Codex model list response had no models array")?;
    let mut records: Vec<&Value> = records
        .iter()
        .filter(|r| r["slug"].as_str().is_some_and(|s| !s.is_empty()) && r["visibility"] != "none")
        .collect();
    records.sort_by(|a, b| {
        let priority = |r: &Value| r["priority"].as_f64().unwrap_or(0.0);
        priority(b)
            .partial_cmp(&priority(a))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a["slug"].as_str().cmp(&b["slug"].as_str()))
    });
    Ok(records
        .into_iter()
        .map(|r| {
            let mut out = Map::new();
            out.insert("id".into(), r["slug"].clone());
            if r["display_name"].is_string() {
                out.insert("name".into(), r["display_name"].clone());
            }
            if r["description"].is_string() {
                out.insert("description".into(), r["description"].clone());
            }
            if r["context_window"].is_number() {
                out.insert("context_length".into(), r["context_window"].clone());
            }
            if r["visibility"] == "list" || r["visibility"] == "hide" {
                out.insert("visibility".into(), r["visibility"].clone());
            }
            Value::Object(out)
        })
        .collect())
}

async fn list_muse_models(state: &AppState, account: Option<&str>) -> Result<Vec<Value>, String> {
    let token = oauth_api_key(state, "meta", account).await?;
    let response = reqwest::Client::new()
        .get("https://api.meta.ai/v1/models")
        .header("Accept", "application/json")
        .header("Authorization", format!("Bearer {token}"))
        .header("x-api-version", "1.0.0")
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(fetch_error)?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!(
            "Muse model list returned {} {}",
            status.as_u16(),
            crate::oauth_login::status_text(status)
        ));
    }
    let body: Value = response.json().await.map_err(fetch_error)?;
    let entries: Vec<Value> = if let Some(data) = body["data"].as_array() {
        data.clone()
    } else if let Some(models) = body["models"].as_array() {
        models.iter().map(|m| if let Some(s) = m.as_str() { json!({"id": s}) } else { json!({"id": m.get("name").filter(|v| v.as_str().is_some_and(|s| !s.is_empty())).or_else(|| m.get("id")).or_else(|| m.get("model")).cloned().unwrap_or(Value::Null)}) }).collect()
    } else if body.is_object() {
        vec![body.clone()]
    } else {
        vec![]
    };
    let mut ids: Vec<String> = entries
        .iter()
        .filter_map(|m| {
            m["id"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        })
        .collect();
    if ids.is_empty() {
        return Err("Muse model list response had no model ids".into());
    }
    ids.sort();
    Ok(ids
        .into_iter()
        .map(|id| json!({"id": id, "name": id}))
        .collect())
}

async fn models(
    State(state): State<AppState>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    let schema = {
        use crate::zod::*;
        object(vec![
            ("providerId", string().min(1.0)),
            ("accountId", string().optional()),
        ])
    };
    let parsed = match schema.safe_parse(&query_value(raw)) {
        Ok(parsed) => parsed,
        Err(details) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "Invalid query parameters", "details": details})),
            )
                .into_response()
        }
    };
    let provider = parsed["providerId"].as_str().unwrap_or_default();
    let account = parsed["accountId"].as_str().filter(|a| !a.is_empty());
    let fallback_warning = |label: &str, reason: &str| {
        format!("Could not reach the {label} model list ({reason}); showing the static catalog. Log in with OAuth to see your account's models.")
    };
    match provider {
        "openai-codex" => match list_codex_models(&state, account).await {
            Ok(models) => Json(json!({"data": with_codex_image_models(models), "source": "codex-backend"})).into_response(),
            Err(reason) => Json(json!({"data": with_codex_image_models(catalog_models("openai-codex")), "source": "catalog", "warning": fallback_warning("Codex", &reason)})).into_response(),
        },
        "meta" => match list_muse_models(&state, account).await {
            Ok(models) => Json(json!({"data": models, "source": "muse-backend"})).into_response(),
            Err(reason) => {
                let data: Vec<Value> = MUSE_STATIC_MODELS.iter().map(|id| json!({"id": id, "name": id})).collect();
                Json(json!({"data": data, "source": "catalog", "warning": fallback_warning("Muse", &reason)})).into_response()
            }
        },
        _ => Json(json!({"data": catalog_models(provider), "source": "catalog"})).into_response(),
    }
}

#[cfg(test)]
mod tests;
