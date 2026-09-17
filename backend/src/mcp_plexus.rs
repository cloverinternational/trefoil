//! Stateless JSON-RPC management MCP endpoint.
//!
//! This is deliberately separate from `mcp_proxy`: `plexus` is an in-process
//! management server and must never be treated as an upstream server name.

use crate::{
    auth::{resolve_principal, Principal},
    AppState,
};
use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json, Router,
};
use base64::Engine;
use serde_json::{json, Value};

const TOOLS: &[(&str, &str)] = &[
    ("plexus_config", "Inspect Plexus configuration and status."),
    ("plexus_provider", "Inspect and manage providers."),
    ("plexus_model_alias", "Inspect and manage model aliases."),
    ("plexus_key", "Inspect and manage inference keys."),
    ("plexus_quota", "Inspect and manage user quotas."),
    ("plexus_quota_checker", "Inspect provider quota checkers."),
    ("plexus_usage", "Review request usage."),
    ("plexus_debug", "Review and manage debug tracing."),
    (
        "plexus_mcp_gateway",
        "Inspect and manage MCP gateway configuration.",
    ),
    ("plexus_settings", "Inspect Plexus settings."),
    (
        "plexus_system_logs",
        "Inspect system logs and logging level.",
    ),
    ("plexus_operations", "Run operational actions."),
];

pub fn router() -> Router<AppState> {
    Router::new().route(
        "/mcp/plexus",
        axum::routing::post(post).get(get).delete(delete),
    )
}

fn auth(state: &AppState, headers: &HeaderMap) -> Result<(), Response> {
    match resolve_principal(state, headers.get("x-admin-key").and_then(|v| v.to_str().ok())) {
        Some(Principal::Admin) => Ok(()),
        Some(Principal::Limited { .. }) => Err((StatusCode::FORBIDDEN, Json(json!({"error":{"message":"Admin privileges required","type":"forbidden","code":403}}))).into_response()),
        None => Err((StatusCode::UNAUTHORIZED, Json(json!({"error":{"message":"Unauthorized","type":"auth_error","code":401}}))).into_response()),
    }
}

fn enabled(state: &AppState) -> bool {
    state
        .inner
        .settings
        .read()
        .unwrap()
        .get("mcpEnabled")
        .and_then(Value::as_bool)
        .unwrap_or(true)
}

fn disabled() -> Response {
    (StatusCode::IM_A_TEAPOT, Json(json!({"error":{"message":"Plexus Management MCP is disabled. Enable it on the MCP Servers page.","type":"mcp_disabled"}}))).into_response()
}

fn rpc_error(id: Value, code: i32, message: &str) -> Response {
    (
        StatusCode::OK,
        Json(json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})),
    )
        .into_response()
}

fn rpc_result(id: Value, result: Value) -> Response {
    (
        StatusCode::OK,
        Json(json!({"jsonrpc":"2.0","id":id,"result":result})),
    )
        .into_response()
}

async fn dispatch(state: &AppState, request: Value) -> Response {
    let Some(object) = request.as_object() else {
        return rpc_error(Value::Null, -32600, "Invalid Request");
    };
    let id = object.get("id").cloned().unwrap_or(Value::Null);
    if object.get("jsonrpc") != Some(&Value::String("2.0".into())) {
        return rpc_error(id, -32600, "Invalid Request");
    }
    let Some(method) = object.get("method").and_then(Value::as_str) else {
        return rpc_error(id, -32600, "Invalid Request");
    };
    match method {
        "initialize" => rpc_result(
            id,
            json!({"protocolVersion":"2025-06-18","capabilities":{"tools":{},"resources":{},"prompts":{}},"serverInfo":{"name":"plexus-management","version":"0.1.0"},"instructions":"Use /mcp/plexus for admin-only Plexus management."}),
        ),
        "notifications/initialized" => (StatusCode::NO_CONTENT, "").into_response(),
        "tools/list" => rpc_result(
            id,
            json!({"tools": TOOLS.iter().map(|(name, description)| json!({"name":name,"title":name,"description":description,"inputSchema":{"type":"object","required":["operation"],"properties":{"operation":{"type":"string"},"id":{"type":"string"},"category":{"type":"string"},"query":{"type":"object"},"body":{"type":"object"},"destructive":{"type":"string"},"redact":{"type":"boolean"}}}})).collect::<Vec<_>>() }),
        ),
        "prompts/list" => rpc_result(
            id,
            json!({"prompts":[{"name":"plexus_management_guide","title":"Plexus Management Guide","description":"Best practices for privileged management."}]}),
        ),
        "prompts/get" => rpc_result(
            id,
            json!({"description":"Plexus management guide","messages":[{"role":"user","content":{"type":"text","text":"Plexus is a unified API gateway. Inspect before mutating; destructive operations require destructive: \"acknowledged\"."}}]}),
        ),
        "resources/list" => rpc_result(
            id,
            json!({"resources":[{"name":"plexus_management_guide","uri":"plexus://management/guide","mimeType":"text/markdown"}]}),
        ),
        "resources/read" => rpc_result(
            id,
            json!({"contents":[{"uri":"plexus://management/guide","mimeType":"text/markdown","text":"Plexus management guide. Inspect before mutating."}]}),
        ),
        "tools/call" => {
            call_tool(
                state,
                id,
                object
                    .get("params")
                    .and_then(|p| p.get("name"))
                    .and_then(Value::as_str),
                object.get("params").and_then(|p| p.get("arguments")),
            )
            .await
        }
        _ => rpc_error(id, -32601, "Method not found"),
    }
}

async fn call_tool(
    state: &AppState,
    id: Value,
    name: Option<&str>,
    args: Option<&Value>,
) -> Response {
    let Some(name) = name else {
        return rpc_error(id, -32602, "Invalid params");
    };
    let operation = args
        .and_then(|v| v.get("operation"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let result = match (name, operation) {
        ("plexus_config", "status") | ("plexus_config", "get") => {
            json!({"ok":true,"operation":operation,"data":{"mcpEnabled":enabled(state)}})
        }
        ("plexus_provider", "list") => {
            let providers = state.inner.providers.read().unwrap();
            json!({"ok":true,"operation":operation,"data":providers.keys().collect::<Vec<_>>()})
        }
        ("plexus_provider", "get") => {
            let Some(provider_id) = args.and_then(|v| v.get("id")).and_then(Value::as_str) else {
                return rpc_error(id, -32602, "id is required");
            };
            let providers = state.inner.providers.read().unwrap();
            match providers.get(provider_id) {
                Some(provider) => {
                    json!({"ok":true,"operation":operation,"data":{"id":provider_id,"provider":redact_provider(provider)}})
                }
                None => {
                    json!({"ok":false,"operation":operation,"error":{"message":format!("Provider '{provider_id}' not found"),"type":"not_found","code":404}})
                }
            }
        }
        ("plexus_provider", "put")
        | ("plexus_provider", "update")
        | ("plexus_provider", "delete")
        | ("plexus_provider", "fetch_models") => {
            return provider_operation(state, id, operation, args).await;
        }
        ("plexus_model_alias", "list")
        | ("plexus_model_alias", "get")
        | ("plexus_model_alias", "put")
        | ("plexus_model_alias", "update")
        | ("plexus_model_alias", "delete")
        | ("plexus_model_alias", "delete_all") => {
            return model_alias_operation(state, id, operation, args).await;
        }
        ("plexus_key", "list")
        | ("plexus_key", "get")
        | ("plexus_key", "put")
        | ("plexus_key", "create")
        | ("plexus_key", "update")
        | ("plexus_key", "delete")
        | ("plexus_key", "disable") => {
            return key_operation(state, id, operation, args).await;
        }
        ("plexus_usage", "list") | ("plexus_usage", "summary") => {
            return usage_operation(state, id, operation, args).await;
        }
        ("plexus_mcp_gateway", "servers_list")
        | ("plexus_mcp_gateway", "get")
        | ("plexus_mcp_gateway", "put")
        | ("plexus_mcp_gateway", "update")
        | ("plexus_mcp_gateway", "delete") => {
            return mcp_gateway_operation(state, id, operation, args).await;
        }
        ("plexus_settings", "get") => return settings_operation(state, id).await,
        ("plexus_system_logs", "recent")
        | ("plexus_system_logs", "level")
        | ("plexus_system_logs", "set_level")
        | ("plexus_system_logs", "reset_level") => {
            return system_logs_operation(state, id, operation, args).await;
        }
        ("plexus_debug", "state") => return debug_operation(state, id).await,
        ("plexus_quota", "status") | ("plexus_quota", "clear") | ("plexus_quota", "recompute") => {
            return quota_operation(state, id, operation, args).await;
        }
        ("plexus_quota_checker", "types")
        | ("plexus_quota_checker", "list")
        | ("plexus_quota_checker", "get") => {
            return quota_checker_operation(state, id, operation, args).await;
        }
        ("plexus_config", "export") => {
            return forwarded_operation(
                state,
                id,
                operation,
                "GET",
                "/v0/management/config/export",
                None,
            )
            .await
        }
        ("plexus_operations", "backup")
        | ("plexus_operations", "restore")
        | ("plexus_operations", "restart")
        | ("plexus_operations", "list_cooldowns")
        | ("plexus_operations", "clear_cooldowns")
        | ("plexus_operations", "reset_logs")
        | ("plexus_operations", "refresh_metadata") => {
            return operations_operation(state, id, operation, args).await
        }
        _ if TOOLS.iter().any(|(tool, _)| *tool == name) => {
            json!({"ok":false,"operation":operation,"error":{"message":"Operation is not implemented yet","type":"invalid_request","code":400}})
        }
        _ => {
            return rpc_result(
                id,
                json!({"isError":true,"content":[{"type":"text","text":"Unknown tool"}]}),
            )
        }
    };
    rpc_result(
        id,
        json!({"structuredContent":result,"content":[{"type":"text","text":result.to_string()}],"isError":result.get("ok").and_then(Value::as_bool)==Some(false)}),
    )
}

async fn forwarded_operation(
    state: &AppState,
    id: Value,
    operation: &str,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> Response {
    let input = body.map(|value| (value.to_string().into_bytes(), "application/json"));
    forwarded_request(state, id, operation, method, path, input).await
}

async fn forwarded_request(
    state: &AppState,
    id: Value,
    operation: &str,
    method: &str,
    path: &str,
    input: Option<(Vec<u8>, &str)>,
) -> Response {
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let mut request = Request::builder().method(method).uri(path).header(
        "x-admin-key",
        state.inner.admin_key.clone().unwrap_or_default(),
    );
    let input = match input {
        Some((bytes, content_type)) => {
            request = request.header("content-type", content_type);
            Body::from(bytes)
        }
        None => Body::empty(),
    };
    let response = crate::app(state.clone())
        .oneshot(request.body(input).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let payload = if content_type.contains("json") {
        serde_json::from_slice(&bytes).unwrap_or_else(|_| json!({"success": status.is_success()}))
    } else {
        json!({"encoding":"base64","contentType":content_type,"data":base64::engine::general_purpose::STANDARD.encode(&bytes)})
    };
    let result = if status.is_success() {
        json!({"ok":true,"operation":operation,"data":payload})
    } else {
        json!({"ok":false,"operation":operation,"error":payload})
    };
    rpc_result(
        id,
        json!({"structuredContent":result,"content":[{"type":"text","text":result.to_string()}],"isError":!status.is_success()}),
    )
}

async fn quota_checker_operation(
    state: &AppState,
    id: Value,
    operation: &str,
    args: Option<&Value>,
) -> Response {
    let args = args.cloned().unwrap_or_else(|| json!({}));
    let path = match operation {
        "types" => "/v0/management/quota-checker-types".to_string(),
        "list" => "/v0/management/quota-checkers".to_string(),
        "get" => match args.get("id").and_then(Value::as_str) {
            Some(id) => format!("/v0/management/quota-checkers/{id}"),
            None => return rpc_error(id, -32602, "id is required"),
        },
        _ => unreachable!(),
    };
    forwarded_operation(state, id, operation, "GET", &path, None).await
}

async fn operations_operation(
    state: &AppState,
    id: Value,
    operation: &str,
    args: Option<&Value>,
) -> Response {
    let args = args.cloned().unwrap_or_else(|| json!({}));
    if matches!(
        operation,
        "restore" | "restart" | "clear_cooldowns" | "reset_logs"
    ) && args.get("destructive").and_then(Value::as_str) != Some("acknowledged")
    {
        return rpc_error(id, -32602, "destructive: \"acknowledged\" is required");
    }
    match operation {
        "backup" => {
            let path = if args.get("full").and_then(Value::as_bool).unwrap_or(false) {
                "/v0/management/backup?full=true"
            } else {
                "/v0/management/backup"
            };
            forwarded_request(state, id, operation, "GET", path, None).await
        }
        "restore" => match args.get("body") {
            Some(body) if body.is_object() || body.is_array() => {
                forwarded_operation(
                    state,
                    id,
                    operation,
                    "POST",
                    "/v0/management/restore",
                    Some(body.clone()),
                )
                .await
            }
            Some(body) => {
                let Some(encoded) = body
                    .as_str()
                    .or_else(|| body.get("data").and_then(Value::as_str))
                else {
                    return rpc_error(id, -32602, "body must be a JSON object or base64 string");
                };
                let bytes = match base64::engine::general_purpose::STANDARD.decode(encoded) {
                    Ok(bytes) => bytes,
                    Err(_) => return rpc_error(id, -32602, "body contains invalid base64"),
                };
                forwarded_request(
                    state,
                    id,
                    operation,
                    "POST",
                    "/v0/management/restore",
                    Some((bytes, "application/gzip")),
                )
                .await
            }
            None => rpc_error(id, -32602, "body is required"),
        },
        "restart" => {
            forwarded_operation(state, id, operation, "POST", "/v0/management/restart", None).await
        }
        "list_cooldowns" => {
            forwarded_operation(
                state,
                id,
                operation,
                "GET",
                "/v0/management/cooldowns",
                None,
            )
            .await
        }
        "clear_cooldowns" => {
            forwarded_operation(
                state,
                id,
                operation,
                "DELETE",
                "/v0/management/cooldowns",
                None,
            )
            .await
        }
        "reset_logs" => {
            forwarded_operation(
                state,
                id,
                operation,
                "DELETE",
                "/v0/management/mcp-logs",
                None,
            )
            .await
        }
        "refresh_metadata" => {
            forwarded_operation(
                state,
                id,
                operation,
                "POST",
                "/v0/management/models/metadata/refresh",
                None,
            )
            .await
        }
        _ => unreachable!(),
    }
}

async fn settings_operation(state: &AppState, id: Value) -> Response {
    let settings = state.inner.settings.read().unwrap();
    let data = Value::Object(
        settings
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    );
    let result = json!({"ok":true,"operation":"get","data":data});
    rpc_result(
        id,
        json!({"structuredContent":result,"content":[{"type":"text","text":result.to_string()}],"isError":false}),
    )
}

async fn system_logs_operation(
    state: &AppState,
    id: Value,
    operation: &str,
    args: Option<&Value>,
) -> Response {
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let args = args.cloned().unwrap_or_else(|| json!({}));
    let (method, path, body, use_system) = match operation {
        "recent" => (
            "GET",
            format!(
                "/v0/system/logs/recent?limit={}&offset={}",
                args.get("limit").and_then(Value::as_u64).unwrap_or(100),
                args.get("offset").and_then(Value::as_u64).unwrap_or(0)
            ),
            None,
            true,
        ),
        "level" => ("GET", "/v0/management/logging/level".into(), None, false),
        "set_level" => {
            let Some(level) = args.get("level").and_then(Value::as_str) else {
                return rpc_error(id, -32602, "level is required");
            };
            (
                "PUT",
                "/v0/management/logging/level".into(),
                Some(json!({"level":level})),
                false,
            )
        }
        "reset_level" => ("DELETE", "/v0/management/logging/level".into(), None, false),
        _ => unreachable!(),
    };
    let mut req = Request::builder().method(method).uri(path).header(
        "x-admin-key",
        state.inner.admin_key.clone().unwrap_or_default(),
    );
    let b = match body {
        Some(v) => {
            req = req.header("content-type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let response = if use_system {
        crate::system_logs::router()
            .with_state(state.clone())
            .oneshot(req.body(b).unwrap())
            .await
            .unwrap()
    } else {
        crate::logging::router()
            .with_state(state.clone())
            .oneshot(req.body(b).unwrap())
            .await
            .unwrap()
    };
    let ok = response.status().is_success();
    let payload: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
            .unwrap_or_else(|_| json!({"error":"Invalid response"}));
    let result = if ok {
        json!({"ok":true,"operation":operation,"data":payload})
    } else {
        json!({"ok":false,"operation":operation,"error":payload})
    };
    rpc_result(
        id,
        json!({"structuredContent":result,"content":[{"type":"text","text":result.to_string()}],"isError":!ok}),
    )
}

async fn debug_operation(state: &AppState, id: Value) -> Response {
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let req = Request::get("/v0/management/debug")
        .header(
            "x-admin-key",
            state.inner.admin_key.clone().unwrap_or_default(),
        )
        .body(Body::empty())
        .unwrap();
    let response = crate::debug::router()
        .with_state(state.clone())
        .oneshot(req)
        .await
        .unwrap();
    let ok = response.status().is_success();
    let payload: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
            .unwrap_or_else(|_| json!({"error":"Invalid response"}));
    let result = if ok {
        json!({"ok":true,"operation":"state","data":payload})
    } else {
        json!({"ok":false,"operation":"state","error":payload})
    };
    rpc_result(
        id,
        json!({"structuredContent":result,"content":[{"type":"text","text":result.to_string()}],"isError":!ok}),
    )
}

async fn quota_operation(
    state: &AppState,
    id: Value,
    operation: &str,
    args: Option<&Value>,
) -> Response {
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let args = args.cloned().unwrap_or_else(|| json!({}));
    let (method, path, body) = match operation {
        "status" => {
            let Some(key) = args.get("id").and_then(Value::as_str) else {
                return rpc_error(id, -32602, "id is required");
            };
            ("GET", format!("/v0/management/quota/status/{key}"), None)
        }
        "clear" | "recompute" => {
            if operation == "clear"
                && args.get("destructive").and_then(Value::as_str) != Some("acknowledged")
            {
                return rpc_result(
                    id,
                    json!({"structuredContent":{"ok":false,"operation":operation,"error":{"message":"Confirmation required","type":"confirmation_required","code":400}},"content":[{"type":"text","text":"Confirmation required"}],"isError":true}),
                );
            }
            let Some(body) = args.get("body") else {
                return rpc_error(id, -32602, "body is required");
            };
            (
                "POST",
                format!("/v0/management/quota/{operation}"),
                Some(body.clone()),
            )
        }
        _ => unreachable!(),
    };
    let mut request = Request::builder().method(method).uri(path).header(
        "x-admin-key",
        state.inner.admin_key.clone().unwrap_or_default(),
    );
    let body = match body {
        Some(value) => {
            request = request.header("content-type", "application/json");
            Body::from(value.to_string())
        }
        None => Body::empty(),
    };
    let response = crate::quota_management::router()
        .with_state(state.clone())
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let ok = response.status().is_success();
    let payload: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
            .unwrap_or_else(|_| json!({"error":{"message":"Invalid quota response"}}));
    let result = if ok {
        json!({"ok":true,"operation":operation,"data":payload})
    } else {
        json!({"ok":false,"operation":operation,"error":payload.get("error").cloned().unwrap_or(payload)})
    };
    rpc_result(
        id,
        json!({"structuredContent":result,"content":[{"type":"text","text":result.to_string()}],"isError":!ok}),
    )
}

async fn model_alias_operation(
    state: &AppState,
    id: Value,
    operation: &str,
    args: Option<&Value>,
) -> Response {
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let args = args.cloned().unwrap_or_else(|| json!({}));
    let (method, path, body) = match operation {
        "list" => ("GET", "/v0/management/aliases".to_string(), None),
        "get" => {
            let Some(slug) = args.get("id").and_then(Value::as_str) else {
                return rpc_error(id, -32602, "id is required");
            };
            ("GET", format!("/v0/management/aliases/{slug}"), None)
        }
        "put" | "update" => {
            let Some(slug) = args.get("id").and_then(Value::as_str) else {
                return rpc_error(id, -32602, "id is required");
            };
            let Some(body) = args.get("body") else {
                return rpc_error(id, -32602, "body is required");
            };
            (
                if operation == "put" { "PUT" } else { "PATCH" },
                format!("/v0/management/aliases/{slug}"),
                Some(body.clone()),
            )
        }
        "delete" => {
            if args.get("destructive").and_then(Value::as_str) != Some("acknowledged") {
                return rpc_error(id, -32602, "destructive: \"acknowledged\" is required");
            }
            let Some(slug) = args.get("id").and_then(Value::as_str) else {
                return rpc_error(id, -32602, "id is required");
            };
            ("DELETE", format!("/v0/management/models/{slug}"), None)
        }
        "delete_all" => {
            if args.get("destructive").and_then(Value::as_str) != Some("acknowledged") {
                return rpc_error(id, -32602, "destructive: \"acknowledged\" is required");
            }
            ("DELETE", "/v0/management/models".to_string(), None)
        }
        _ => unreachable!(),
    };
    let mut request = Request::builder().method(method).uri(path).header(
        "x-admin-key",
        state.inner.admin_key.clone().unwrap_or_default(),
    );
    let body = match body {
        Some(value) => {
            request = request.header("content-type", "application/json");
            Body::from(value.to_string())
        }
        None => Body::empty(),
    };
    let response = crate::aliases::router()
        .with_state(state.clone())
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let ok = response.status().is_success();
    let payload: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
            .unwrap_or_else(|_| json!({"error":"Invalid alias response"}));
    let result = if ok {
        json!({"ok":true,"operation":operation,"data":payload})
    } else {
        json!({"ok":false,"operation":operation,"error":payload})
    };
    rpc_result(
        id,
        json!({"structuredContent":result,"content":[{"type":"text","text":result.to_string()}],"isError":!ok}),
    )
}

fn redact_provider(provider: &crate::state::ProviderConfig) -> Value {
    let mut value = serde_json::to_value(provider).unwrap_or_else(|_| json!({}));
    if let Some(object) = value.as_object_mut() {
        object.remove("api_key");
    }
    value
}

async fn key_operation(
    state: &AppState,
    id: Value,
    operation: &str,
    args: Option<&Value>,
) -> Response {
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let args = args.cloned().unwrap_or_else(|| json!({}));
    let name = args.get("id").and_then(Value::as_str);
    let (method, path, body) = match operation {
        "list" => ("GET", "/v0/management/keys".into(), None),
        "get" => {
            let Some(name) = name else {
                return rpc_error(id, -32602, "id is required");
            };
            ("GET", format!("/v0/management/keys/{name}"), None)
        }
        "put" | "create" | "update" => {
            let Some(name) = name else {
                return rpc_error(id, -32602, "id is required");
            };
            let method = if operation == "update" {
                "PATCH"
            } else {
                "PUT"
            };
            (
                method,
                format!("/v0/management/keys/{name}"),
                Some(args.get("body").cloned().unwrap_or_else(|| json!({}))),
            )
        }
        "delete" | "disable" => {
            if args.get("destructive").and_then(Value::as_str) != Some("acknowledged") {
                return rpc_error(id, -32602, "destructive: \"acknowledged\" is required");
            }
            let Some(name) = name else {
                return rpc_error(id, -32602, "id is required");
            };
            let path = if operation == "delete" {
                format!("/v0/management/keys/{name}")
            } else {
                format!("/v0/management/keys/{name}/disable")
            };
            (
                if operation == "delete" {
                    "DELETE"
                } else {
                    "POST"
                },
                path,
                None,
            )
        }
        _ => unreachable!(),
    };
    let mut request = Request::builder().method(method).uri(path).header(
        "x-admin-key",
        state.inner.admin_key.clone().unwrap_or_default(),
    );
    let body = match body {
        Some(v) => {
            request = request.header("content-type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let response = crate::keys::router()
        .with_state(state.clone())
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let ok = response.status().is_success();
    let payload: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
            .unwrap_or_else(|_| json!({"error":"Invalid key response"}));
    let payload = if ok && matches!(operation, "list" | "get") {
        redact_key_secrets(payload)
    } else {
        payload
    };
    let result = if ok {
        json!({"ok":true,"operation":operation,"data":payload})
    } else {
        json!({"ok":false,"operation":operation,"error":payload})
    };
    rpc_result(
        id,
        json!({"structuredContent":result,"content":[{"type":"text","text":result.to_string()}],"isError":!ok}),
    )
}

fn redact_key_secrets(value: Value) -> Value {
    match value {
        Value::Array(v) => Value::Array(v.into_iter().map(redact_key_secrets).collect()),
        Value::Object(mut o) => {
            for (k, v) in o.iter_mut() {
                if k.eq_ignore_ascii_case("secret") {
                    *v = json!("[REDACTED]");
                } else {
                    *v = redact_key_secrets(v.take());
                }
            }
            Value::Object(o)
        }
        v => v,
    }
}

async fn mcp_gateway_operation(
    state: &AppState,
    id: Value,
    operation: &str,
    args: Option<&Value>,
) -> Response {
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    let args = args.cloned().unwrap_or_else(|| json!({}));
    let (method, path, body) = match operation {
        "servers_list" => ("GET", "/v0/management/mcp-servers".to_string(), None),
        "get" => {
            let Some(name) = args.get("id").and_then(Value::as_str) else {
                return rpc_error(id, -32602, "id is required");
            };
            ("GET", format!("/v0/management/mcp-servers/{name}"), None)
        }
        "put" | "update" => {
            let Some(name) = args.get("id").and_then(Value::as_str) else {
                return rpc_error(id, -32602, "id is required");
            };
            let Some(body) = args.get("body") else {
                return rpc_error(id, -32602, "body is required");
            };
            (
                if operation == "put" { "PUT" } else { "PATCH" },
                format!("/v0/management/mcp-servers/{name}"),
                Some(body.clone()),
            )
        }
        "delete" => {
            if args.get("destructive").and_then(Value::as_str) != Some("acknowledged") {
                return rpc_error(id, -32602, "destructive: \"acknowledged\" is required");
            }
            let Some(name) = args.get("id").and_then(Value::as_str) else {
                return rpc_error(id, -32602, "id is required");
            };
            ("DELETE", format!("/v0/management/mcp-servers/{name}"), None)
        }
        _ => unreachable!(),
    };
    let mut request = Request::builder().method(method).uri(path).header(
        "x-admin-key",
        state.inner.admin_key.clone().unwrap_or_default(),
    );
    let body = match body {
        Some(value) => {
            request = request.header("content-type", "application/json");
            Body::from(value.to_string())
        }
        None => Body::empty(),
    };
    let response = crate::mcp::router()
        .with_state(state.clone())
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let ok = response.status().is_success();
    let payload: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
            .unwrap_or_else(|_| json!({"error": "Invalid MCP gateway response"}));
    let result = if ok {
        json!({"ok":true,"operation":operation,"data":payload})
    } else {
        json!({"ok":false,"operation":operation,"error":payload})
    };
    rpc_result(
        id,
        json!({"structuredContent":result,"content":[{"type":"text","text":result.to_string()}],"isError":!ok}),
    )
}

/// Forward the read-only usage operations through the same management router
/// used by the HTTP API.  Keeping this as a router call avoids subtly
/// different authentication, pagination, or response shapes on the MCP
/// surface.
async fn usage_operation(
    state: &AppState,
    id: Value,
    operation: &str,
    args: Option<&Value>,
) -> Response {
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    let args = args.cloned().unwrap_or_else(|| json!({}));
    let mut path = match operation {
        "list" => "/v0/management/usage".to_string(),
        "summary" => "/v0/management/usage/summary".to_string(),
        _ => unreachable!(),
    };
    if let Some(query) = args.get("query") {
        let Some(query) = query.as_object() else {
            return rpc_error(id, -32602, "query must be an object");
        };
        let mut serializer = url::form_urlencoded::Serializer::new(String::new());
        for (key, value) in query {
            if let Some(value) = value.as_str() {
                serializer.append_pair(key, value);
            } else if let Some(value) = value.as_i64() {
                serializer.append_pair(key, &value.to_string());
            } else if let Some(value) = value.as_u64() {
                serializer.append_pair(key, &value.to_string());
            } else if let Some(value) = value.as_f64() {
                serializer.append_pair(key, &value.to_string());
            } else if let Some(value) = value.as_bool() {
                serializer.append_pair(key, if value { "true" } else { "false" });
            } else {
                return rpc_error(id, -32602, "query values must be scalar");
            }
        }
        let query = serializer.finish();
        if !query.is_empty() {
            path.push('?');
            path.push_str(&query);
        }
    }
    let request = Request::builder()
        .method("GET")
        .uri(path)
        .header(
            "x-admin-key",
            state.inner.admin_key.clone().unwrap_or_default(),
        )
        .body(Body::empty())
        .unwrap();
    let response = crate::usage::router()
        .with_state(state.clone())
        .oneshot(request)
        .await
        .unwrap();
    let ok = response.status().is_success();
    let payload: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
            .unwrap_or_else(|_| json!({"error":"Invalid usage response"}));
    let result = if ok {
        json!({"ok":true,"operation":operation,"data":payload})
    } else {
        json!({"ok":false,"operation":operation,"error":payload})
    };
    rpc_result(
        id,
        json!({"structuredContent":result,"content":[{"type":"text","text":result.to_string()}],"isError":!ok}),
    )
}

async fn provider_operation(
    state: &AppState,
    id: Value,
    operation: &str,
    args: Option<&Value>,
) -> Response {
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let args = args.cloned().unwrap_or_else(|| json!({}));
    let Some(provider_id) = args.get("id").and_then(Value::as_str) else {
        return rpc_error(id, -32602, "id is required");
    };
    let (method, path, body) = match operation {
        "put" | "update" => {
            let Some(body) = args.get("body") else {
                return rpc_error(id, -32602, "body is required");
            };
            (
                if operation == "put" { "PUT" } else { "PATCH" },
                format!("/v0/management/providers/{provider_id}"),
                Some(body.clone()),
            )
        }
        "delete" => {
            if args.get("destructive").and_then(Value::as_str) != Some("acknowledged") {
                return rpc_error(id, -32602, "destructive: \"acknowledged\" is required");
            }
            (
                "DELETE",
                format!("/v0/management/providers/{provider_id}"),
                None,
            )
        }
        "fetch_models" => {
            let Some(body) = args.get("body") else {
                return rpc_error(id, -32602, "body is required");
            };
            (
                "POST",
                "/v0/management/providers/fetch-models".into(),
                Some(body.clone()),
            )
        }
        _ => unreachable!(),
    };
    let mut request = Request::builder().method(method).uri(path).header(
        "x-admin-key",
        state.inner.admin_key.clone().unwrap_or_default(),
    );
    let body = match body {
        Some(v) => {
            request = request.header("content-type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let response = crate::providers::router()
        .with_state(state.clone())
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let ok = response.status().is_success();
    let payload: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
            .unwrap_or_else(|_| json!({"error":"Invalid provider response"}));
    let result = if ok {
        json!({"ok":true,"operation":operation,"data":payload})
    } else {
        json!({"ok":false,"operation":operation,"error":payload})
    };
    rpc_result(
        id,
        json!({"structuredContent":result,"content":[{"type":"text","text":result.to_string()}],"isError":!ok}),
    )
}

async fn post(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    if !enabled(&state) {
        return disabled();
    }
    if let Err(response) = auth(&state, &headers) {
        return response;
    }
    let request: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return rpc_error(Value::Null, -32700, "Parse error"),
    };
    if request.is_array() {
        return rpc_error(Value::Null, -32600, "Invalid Request");
    }
    dispatch(&state, request).await
}

async fn get(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !enabled(&state) {
        return disabled();
    }
    if let Err(response) = auth(&state, &headers) {
        return response;
    }
    dispatch(
        &state,
        json!({"jsonrpc":"2.0","id":null,"method":"tools/list"}),
    )
    .await
}

async fn delete(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !enabled(&state) {
        return disabled();
    }
    if let Err(response) = auth(&state, &headers) {
        return response;
    }
    (StatusCode::OK, Json(json!({"closed":true}))).into_response()
}
