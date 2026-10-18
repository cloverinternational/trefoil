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

/// JavaScript `encodeURIComponent`, used (like Plexus) for ids interpolated
/// into internal management paths so arbitrary client-supplied ids always form
/// a valid request URI.
fn encode_uri_component(value: &str) -> String {
    crate::pi_catalog::encode_uri_component(value)
}

/// Plexus `encodePathPreservingSlashes`: `encodeURIComponent` per `/` segment.
fn encode_path_preserving_slashes(value: &str) -> String {
    value.split('/').map(encode_uri_component).collect::<Vec<_>>().join("/")
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

fn tool_error(id: Value, operation: &str, message: &str, kind: &str, code: i32) -> Response {
    let result = json!({"ok":false,"operation":operation,"error":{"message":message,"type":kind,"code":code}});
    rpc_result(id, json!({"structuredContent":result,"content":[{"type":"text","text":result.to_string()}],"isError":true}))
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

fn is_destructive(operation: &str) -> bool {
    matches!(operation, "delete" | "delete_all" | "clear" | "clear_for_key" | "quota_clear" | "delete_log" | "delete_all_logs" | "disable" | "restore" | "restart" | "rotate" | "truncate" | "import" | "overwrite")
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
    if is_destructive(operation)
        && args.and_then(|v| v.get("destructive")).and_then(Value::as_str) != Some("acknowledged")
    {
        return tool_error(id, operation, "This destructive operation requires destructive: \"acknowledged\".", "confirmation_required", 400);
    }
    let allowed: &[&str] = match name {
        "plexus_config" => &["get", "export", "status"],
        "plexus_provider" => &["list", "get", "put", "create", "update", "delete", "fetch_models"],
        "plexus_model_alias" => &["list", "get", "put", "create", "update", "delete", "delete_all"],
        "plexus_key" => &["list", "get", "put", "create", "update", "delete", "disable"],
        "plexus_quota" => &["list", "get", "put", "create", "update", "delete", "status", "clear", "recompute"],
        "plexus_quota_checker" => &["types", "list", "get"],
        "plexus_system_logs" => &["recent", "level", "set_level", "reset_level"],
        "plexus_mcp_gateway" => &["servers_list", "list", "get", "put", "create", "update", "delete", "status", "start", "stop", "restart", "logs"],
        _ => &[],
    };
    if !allowed.is_empty() && !allowed.contains(&operation) {
        return tool_error(id, operation, &format!("Unsupported operation '{operation}'. Allowed operations: {}.", allowed.join(", ")), "invalid_request", 400);
    }
    let resource = match (name, operation) {
        ("plexus_provider", "get" | "put" | "create" | "update" | "delete") => Some("provider"),
        ("plexus_model_alias", "get" | "put" | "create" | "update" | "delete") => Some("model_alias"),
        ("plexus_key", "get" | "put" | "create" | "update" | "delete" | "disable") => Some("key"),
        ("plexus_quota", "get" | "put" | "create" | "update" | "delete") => Some("quota"),
        ("plexus_quota", "status") => Some("key"),
        ("plexus_quota_checker", "get") => Some("quota checker"),
        ("plexus_debug", "get_log") => Some("debug get_log"),
        ("plexus_mcp_gateway", "get" | "put" | "create" | "update" | "delete" | "status" | "start" | "stop" | "restart" | "logs") => Some("mcp_gateway"),
        _ => None,
    };
    if let Some(resource) = resource {
        if args.and_then(|args| args.get("id")).and_then(Value::as_str).is_none_or(str::is_empty) {
            return tool_error(id, operation, &format!("Missing id for {resource} operation."), "invalid_request", 400);
        }
    }
    let result = match (name, operation) {
        ("plexus_config", "status") => {
            return forwarded_operation(
                state,
                id,
                operation,
                "GET",
                "/v0/management/config/status",
                None,
            )
            .await;
        }
        ("plexus_config", "get") => {
            return forwarded_operation(state, id, operation, "GET", "/v0/management/config", None)
                .await;
        }
        ("plexus_provider", "list") => {
            let providers = state.inner.providers.read().unwrap();
            let mut entries = providers.iter().collect::<Vec<_>>();
            entries.sort_by_key(|(id, _)| *id);
            let data = entries.into_iter().map(|(id, provider)| {
                let mut value = redact_provider(provider);
                if let Value::Object(ref mut object) = value { object.insert("id".into(), json!(id)); }
                value
            }).collect::<Vec<_>>();
            json!({"ok":true,"operation":operation,"data":data})
        }
        ("plexus_provider", "get") => {
            let Some(provider_id) = args.and_then(|v| v.get("id")).and_then(Value::as_str) else {
                return rpc_error(id, -32602, "id is required");
            };
            let providers = state.inner.providers.read().unwrap();
            match providers.get(provider_id) {
                Some(provider) => {
                    {
                        let mut data = redact_provider(provider);
                        if let Value::Object(ref mut object) = data {
                            object.insert("id".into(), json!(provider_id));
                        }
                        json!({"ok":true,"operation":operation,"data":data})
                    }
                }
                None => {
                    json!({"ok":false,"operation":operation,"error":{"message":format!("Provider '{provider_id}' not found"),"type":"not_found","code":404}})
                }
            }
        }
        ("plexus_provider", "fetch_models") => {
            let body = args.and_then(|v| v.get("body")).cloned().unwrap_or_else(|| json!({}));
            return forwarded_operation(state, id, operation, "POST", "/v0/management/providers/fetch-models", Some(body)).await;
        }
        ("plexus_provider", "put")
        | ("plexus_provider", "create")
        | ("plexus_provider", "update")
        | ("plexus_provider", "delete")
        => {
            return provider_operation(state, id, operation, args).await;
        }
        ("plexus_model_alias", "list")
        | ("plexus_model_alias", "get")
        | ("plexus_model_alias", "put")
        | ("plexus_model_alias", "create")
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
        ("plexus_usage", "list")
        | ("plexus_usage", "summary")
        | ("plexus_usage", "delete")
        | ("plexus_usage", "delete_all") => {
            return usage_operation(state, id, operation, args).await;
        }
        ("plexus_mcp_gateway", "servers_list")
        | ("plexus_mcp_gateway", "list")
        | ("plexus_mcp_gateway", "get")
        | ("plexus_mcp_gateway", "status")
        | ("plexus_mcp_gateway", "process_logs")
        | ("plexus_mcp_gateway", "start")
        | ("plexus_mcp_gateway", "stop")
        | ("plexus_mcp_gateway", "restart")
        | ("plexus_mcp_gateway", "put")
        | ("plexus_mcp_gateway", "create")
        | ("plexus_mcp_gateway", "update")
        | ("plexus_mcp_gateway", "delete") => {
            return mcp_gateway_operation(state, id, operation, args).await;
        }
        ("plexus_settings", "get") => return settings_operation(state, id, args).await,
        ("plexus_system_logs", "recent")
        | ("plexus_system_logs", "level")
        | ("plexus_system_logs", "set_level")
        | ("plexus_system_logs", "reset_level")
        | ("plexus_system_logs", "modules")
        | ("plexus_system_logs", "set_modules")
        | ("plexus_system_logs", "reset_modules") => {
            return system_logs_operation(state, id, operation, args).await;
        }
        ("plexus_debug", "state") => return debug_operation(state, id).await,
        ("plexus_debug", "update") => return debug_update_operation(state, id, args).await,
        ("plexus_debug", "logs") => return debug_logs_operation(state, id, operation, args).await,
        ("plexus_debug", "get_log") => {
            return debug_logs_operation(state, id, operation, args).await
        }
        ("plexus_debug", "delete_log") => {
            return debug_logs_operation(state, id, operation, args).await
        }
        ("plexus_debug", "delete_all_logs") => {
            return debug_logs_operation(state, id, operation, args).await
        }
        ("plexus_quota", "list")
        | ("plexus_quota", "get")
        | ("plexus_quota", "put")
        | ("plexus_quota", "update")
        | ("plexus_quota", "delete") => {
            return user_quota_operation(state, id, operation, args).await;
        }
        ("plexus_quota", "status") | ("plexus_quota", "clear") | ("plexus_quota", "recompute") => {
            return quota_operation(state, id, operation, args).await;
        }
        ("plexus_quota_checker", "types")
        | ("plexus_quota_checker", "list")
        | ("plexus_quota_checker", "get")
        | ("plexus_quota_checker", "put")
        | ("plexus_quota_checker", "update")
        | ("plexus_quota_checker", "delete") => {
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
            json!({"ok":false,"operation":operation,"error":{"message":format!("Unsupported operation '{}' for {}.", operation, name),"type":"invalid_request","code":400}})
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
    let payload = if status.is_success() && (path == "/v0/management/config" || path == "/v0/management/config/export") {
        redact_key_secrets(payload)
    } else { payload };
    let payload = if status.is_success() && path == "/v0/management/user-quotas" && operation == "list" { record_list(payload) } else { payload };
    let payload = if status.is_success() && operation == "backup" {
        if content_type.contains("json") { json!({"full":false,"backup":payload}) }
        else { json!({"full":true,"bytes":bytes.len(),"contentType":content_type,"encoding":"base64","archive":base64::engine::general_purpose::STANDARD.encode(&bytes)}) }
    } else { payload };
    let result = if status.is_success() {
        json!({"ok":true,"operation":operation,"data":payload})
    } else {
        let raw = payload.get("error").unwrap_or(&payload);
        let message = raw.get("message").cloned().unwrap_or_else(|| if raw.is_string() { raw.clone() } else { json!("Management request failed") });
        let kind = raw.get("type").cloned().unwrap_or_else(|| json!(if status == StatusCode::BAD_REQUEST {"invalid_request"} else if status == StatusCode::NOT_FOUND {"not_found"} else {"server_error"}));
        json!({"ok":false,"operation":operation,"error":{"message":message,"type":kind,"code":raw.get("code").cloned().unwrap_or(json!(status.as_u16()))}})
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
    let (method, path, body) = match operation {
        "types" => (
            "GET",
            "/v0/management/quota-checker-types".to_string(),
            None,
        ),
        "list" => ("GET", "/v0/management/quota-checkers".to_string(), None),
        "get" => match args.get("id").and_then(Value::as_str) {
            Some(id) => ("GET", format!("/v0/management/quota-checkers/{}", encode_uri_component(id)), None),
            None => return rpc_error(id, -32602, "id is required"),
        },
        "put" | "update" | "delete" => {
            let Some(checker_id) = args.get("id").and_then(Value::as_str) else {
                return rpc_error(id, -32602, "id is required");
            };
            if operation == "delete" {
                if args.get("destructive").and_then(Value::as_str) != Some("acknowledged") {
                    return rpc_error(id, -32602, "destructive: \"acknowledged\" is required");
                }
                (
                    "DELETE",
                    format!("/v0/management/custom-checkers/{}", encode_uri_component(checker_id)),
                    None,
                )
            } else {
                let Some(body) = args.get("body") else {
                    return rpc_error(id, -32602, "body is required");
                };
                (
                    if matches!(operation, "put" | "create") {
                        "PUT"
                    } else {
                        "PATCH"
                    },
                    format!("/v0/management/custom-checkers/{}", encode_uri_component(checker_id)),
                    Some(body.clone()),
                )
            }
        }
        _ => unreachable!(),
    };
    forwarded_operation(state, id, operation, method, &path, body).await
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
        "restore" | "restart"
    ) && args.get("destructive").and_then(Value::as_str) != Some("acknowledged")
    {
        return rpc_error(id, -32602, "destructive: \"acknowledged\" is required");
    }
    match operation {
        "backup" => {
            let path = if args.get("query").and_then(|q|q.get("full")).is_some_and(|v|v == true || v == "true") {
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
                "/v0/management/logs/reset",
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

async fn settings_operation(state: &AppState, id: Value, args: Option<&Value>) -> Response {
    use http_body_util::BodyExt;
    // Keep settings category semantics aligned with the reference shim.  A
    // category is a named config endpoint, while `all` returns the same
    // stable aggregate shape clients use for inspection.
    let category = args
        .and_then(|value| value.get("category"))
        .and_then(Value::as_str)
        .unwrap_or("all");
    let categories = [
        ("failover", "/v0/management/config/failover"),
        ("cooldown", "/v0/management/config/cooldown"),
        ("timeout", "/v0/management/config/timeout"),
        ("stall", "/v0/management/config/stall"),
        ("trusted_proxies", "/v0/management/config/trusted-proxies"),
        (
            "vision_fallthrough",
            "/v0/management/config/vision-fallthrough",
        ),
        (
            "background_exploration",
            "/v0/management/config/background-exploration",
        ),
        ("exploration", "/v0/management/config/exploration-rate"),
    ];
    if category != "all" {
        let Some((_, path)) = categories.iter().find(|(name, _)| *name == category) else {
            return rpc_result(
                id,
                json!({"structuredContent":{"ok":false,"operation":"get","error":{"message":format!("settings category '{category}' was not found."),"type":"not_found","code":404}},"content":[{"type":"text","text":format!("settings category '{category}' was not found.")}],"isError":true}),
            );
        };
        return forwarded_operation(state, id, "get", "GET", path, None).await;
    }

    let mut data = serde_json::Map::new();
    for (name, path) in categories {
        let response = forwarded_request(state, id.clone(), "get", "GET", path, None).await;
        let payload = response.into_body().collect().await;
        let Ok(payload) = payload else {
            return rpc_error(id, -32603, "Failed to read settings response");
        };
        let Ok(value) = serde_json::from_slice::<Value>(&payload.to_bytes()) else {
            return rpc_error(id, -32603, "Invalid settings response");
        };
        let Some(value) = value
            .get("result")
            .and_then(|result| result.get("structuredContent"))
            .and_then(|content| content.get("data"))
        else {
            return rpc_result(id, value);
        };
        data.insert(name.to_string(), value.clone());
    }
    let result = json!({"ok":true,"operation":"get","data":Value::Object(data)});
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
            let Some(level) = args
                .get("level")
                .or_else(|| args.get("body").and_then(|body| body.get("level")))
                .and_then(Value::as_str)
            else {
                return tool_error(id, operation, "Missing level for set_level operation. Provide body.level or query.level.", "invalid_request", 400);
            };
            (
                "PUT",
                "/v0/management/logging/level".into(),
                Some(json!({"level":level})),
                false,
            )
        }
        "reset_level" => ("DELETE", "/v0/management/logging/level".into(), None, false),
        "modules" => ("GET", "/v0/management/logging/modules".into(), None, false),
        "set_modules" => {
            let Some(modules) = args
                .get("modules")
                .or_else(|| args.get("body").and_then(|body| body.get("modules")))
                .and_then(Value::as_array)
            else {
                return rpc_error(id, -32602, "modules is required");
            };
            (
                "PUT",
                "/v0/management/logging/modules".into(),
                Some(json!({"modules":modules})),
                false,
            )
        }
        "reset_modules" => (
            "DELETE",
            "/v0/management/logging/modules".into(),
            None,
            false,
        ),
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
        json!({"structuredContent":result,"content":[{"type":"text","text":serde_json::to_string_pretty(&result).unwrap_or_default()}],"isError":!ok}),
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

async fn debug_update_operation(state: &AppState, id: Value, args: Option<&Value>) -> Response {
    let body = args
        .and_then(|value| value.get("body"))
        .cloned()
        .unwrap_or_else(|| json!({}));
    if !body.is_object() {
        return rpc_error(id, -32602, "body must be an object");
    }
    forwarded_operation(
        state,
        id,
        "update",
        "PATCH",
        "/v0/management/debug",
        Some(body),
    )
    .await
}

async fn debug_logs_operation(
    state: &AppState,
    id: Value,
    operation: &str,
    args: Option<&Value>,
) -> Response {
    let args = args.cloned().unwrap_or_else(|| json!({}));
    let (method, path) = match operation {
        "logs" => (
            "GET",
            format!(
                "/v0/management/debug/logs?limit={}&offset={}",
                args.get("query")
                    .and_then(|q| q.get("limit"))
                    .and_then(Value::as_u64)
                    .unwrap_or(50),
                args.get("query")
                    .and_then(|q| q.get("offset"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
            ),
        ),
        "get_log" | "delete_log" => {
            let Some(log_id) = args.get("id").and_then(Value::as_str) else {
                return rpc_error(id, -32602, "id is required");
            };
            if operation == "delete_log"
                && args.get("destructive").and_then(Value::as_str) != Some("acknowledged")
            {
                return rpc_error(id, -32602, "destructive: \"acknowledged\" is required");
            }
            (
                if operation == "get_log" {
                    "GET"
                } else {
                    "DELETE"
                },
                format!("/v0/management/debug/logs/{}", encode_path_preserving_slashes(log_id)),
            )
        }
        "delete_all_logs" => {
            if args.get("destructive").and_then(Value::as_str) != Some("acknowledged") {
                return rpc_error(id, -32602, "destructive: \"acknowledged\" is required");
            }
            ("DELETE", "/v0/management/debug/logs".to_owned())
        }
        _ => unreachable!(),
    };
    forwarded_request(state, id, operation, method, &path, None).await
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
            ("GET", format!("/v0/management/quota/status/{}", encode_uri_component(key)), None)
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
            let body = args.get("body").cloned().unwrap_or_else(|| json!({}));
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
    let status = response.status();
    let ok = status.is_success();
    let payload: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
            .unwrap_or_else(|_| json!({"error":{"message":"Invalid quota response"}}));
    let result = if ok {
        json!({"ok":true,"operation":operation,"data":payload})
    } else {
        let mut error = payload.get("error").cloned().unwrap_or(payload);
        if let Some(object) = error.as_object_mut() { object.entry("code").or_insert(json!(status.as_u16())); }
        json!({"ok":false,"operation":operation,"error":error})
    };
    rpc_result(
        id,
        json!({"structuredContent":result,"content":[{"type":"text","text":result.to_string()}],"isError":!ok}),
    )
}

async fn user_quota_operation(
    state: &AppState,
    id: Value,
    operation: &str,
    args: Option<&Value>,
) -> Response {
    let args = args.cloned().unwrap_or_else(|| json!({}));
    let (method, path, body) = match operation {
        "list" => ("GET", "/v0/management/user-quotas".to_owned(), None),
        "get" | "put" | "update" | "delete" => {
            let Some(name) = args.get("id").and_then(Value::as_str) else {
                return rpc_error(id, -32602, "id is required");
            };
            if operation == "delete"
                && args.get("destructive").and_then(Value::as_str) != Some("acknowledged")
            {
                return rpc_error(id, -32602, "destructive: \"acknowledged\" is required");
            }
            let body = if matches!(operation, "put" | "update") {
                let Some(body) = args.get("body") else {
                    return rpc_error(id, -32602, "body is required");
                };
                Some(body.clone())
            } else {
                None
            };
            let method = match operation {
                "get" => "GET",
                "put" => "PUT",
                "update" => "PATCH",
                "delete" => "DELETE",
                _ => unreachable!(),
            };
            (method, format!("/v0/management/user-quotas/{}", encode_uri_component(name)), body)
        }
        _ => unreachable!(),
    };
    forwarded_operation(state, id, operation, method, &path, body).await
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
            ("GET", format!("/v0/management/aliases/{}", encode_path_preserving_slashes(slug)), None)
        }
        "put" | "create" | "update" => {
            let Some(slug) = args.get("id").and_then(Value::as_str) else {
                return rpc_error(id, -32602, "id is required");
            };
            let Some(body) = args.get("body") else {
                return rpc_error(id, -32602, "body is required");
            };
            (
                if matches!(operation, "put" | "create") {
                    "PUT"
                } else {
                    "PATCH"
                },
                format!("/v0/management/aliases/{}", encode_path_preserving_slashes(slug)),
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
            ("DELETE", format!("/v0/management/models/{}", encode_path_preserving_slashes(slug)), None)
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
    let payload = if ok && operation == "list" { record_list(payload) } else { payload };
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

fn record_list(payload: Value) -> Value {
    match payload {
        Value::Object(records) => Value::Array(records.into_iter().map(|(id, value)| {
            let mut object = value.as_object().cloned().unwrap_or_else(|| serde_json::Map::from_iter([("value".into(), value)]));
            object.insert("id".into(), Value::String(id));
            Value::Object(object)
        }).collect()),
        other => other,
    }
}

fn redact_provider(provider: &crate::state::ProviderConfig) -> Value {
    redact_key_secrets(serde_json::to_value(provider).unwrap_or_else(|_| json!({})))
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
            ("GET", format!("/v0/management/keys/{}", encode_uri_component(name)), None)
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
                format!("/v0/management/keys/{}", encode_uri_component(name)),
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
                format!("/v0/management/keys/{}", encode_uri_component(name))
            } else {
                format!("/v0/management/keys/{}/disable", encode_uri_component(name))
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
    let payload = if ok && operation == "list" { record_list(payload) } else { payload };
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
                let sensitive = matches!(
                    k.to_ascii_lowercase().as_str(),
                    "secret" | "api_key" | "apikey" | "authorization" | "cookie" | "password"
                ) || k.to_ascii_lowercase().contains("token")
                    || k.to_ascii_lowercase().contains("session")
                    || k.to_ascii_lowercase().contains("password")
                    || k.to_ascii_lowercase().contains("authcookie")
                    || k.to_ascii_lowercase().contains("managementapikey");
                if sensitive {
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
        "servers_list" | "list" => ("GET", "/v0/management/mcp-servers".to_string(), None),
        "get" => {
            let Some(name) = args.get("id").and_then(Value::as_str) else {
                return rpc_error(id, -32602, "id is required");
            };
            ("GET", format!("/v0/management/mcp-servers/{}", encode_uri_component(name)), None)
        }
        "put" | "create" | "update" => {
            let Some(name) = args.get("id").and_then(Value::as_str) else {
                return rpc_error(id, -32602, "id is required");
            };
            let Some(body) = args.get("body") else {
                return rpc_error(id, -32602, "body is required");
            };
            (
                if matches!(operation, "put" | "create") {
                    "PUT"
                } else {
                    "PATCH"
                },
                format!("/v0/management/mcp-servers/{}", encode_uri_component(name)),
                Some(body.clone()),
            )
        }
        "status" | "process_logs" => {
            let Some(name) = args.get("id").and_then(Value::as_str) else {
                return rpc_error(id, -32602, "id is required");
            };
            (
                "GET",
                format!(
                    "/v0/management/mcp-servers/{}/{}",
                    encode_uri_component(name),
                    if operation == "status" {
                        "status"
                    } else {
                        "process-logs"
                    }
                ),
                None,
            )
        }
        "start" | "stop" | "restart" => {
            let Some(name) = args.get("id").and_then(Value::as_str) else {
                return rpc_error(id, -32602, "id is required");
            };
            (
                "POST",
                format!("/v0/management/mcp-servers/{}/{operation}", encode_uri_component(name)),
                None,
            )
        }
        "delete" => {
            if args.get("destructive").and_then(Value::as_str) != Some("acknowledged") {
                return rpc_error(id, -32602, "destructive: \"acknowledged\" is required");
            }
            let Some(name) = args.get("id").and_then(Value::as_str) else {
                return rpc_error(id, -32602, "id is required");
            };
            ("DELETE", format!("/v0/management/mcp-servers/{}", encode_uri_component(name)), None)
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
    let payload = if ok && matches!(operation, "list" | "servers_list") { record_list(payload) } else { payload };
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
    if matches!(operation, "delete" | "delete_all")
        && args.get("destructive").and_then(Value::as_str) != Some("acknowledged")
    {
        return rpc_error(id, -32602, "destructive: \"acknowledged\" is required");
    }
    let mut path = match operation {
        "list" => "/v0/management/usage".to_string(),
        "summary" => "/v0/management/usage/summary".to_string(),
        "delete_all" => "/v0/management/usage".to_string(),
        "delete" => {
            let Some(request_id) = args.get("id").and_then(Value::as_str) else {
                return rpc_error(id, -32602, "id is required");
            };
            format!("/v0/management/usage/{}", encode_uri_component(request_id))
        }
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
        .method(if matches!(operation, "delete" | "delete_all") {
            "DELETE"
        } else {
            "GET"
        })
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
    let mut result = if ok {
        json!({"ok":true,"operation":operation,"data":payload})
    } else {
        json!({"ok":false,"operation":operation,"error":payload})
    };
    // The management API retains sample counts for analytics clients, while
    // Plexus's MCP usage summary omits them from its narrower tool contract.
    if operation == "summary" && ok {
        if let Some(stats) = result.pointer_mut("/data/stats").and_then(Value::as_object_mut) {
            stats.remove("ttftSamples");
            stats.remove("tpsSamples");
        }
    }
    rpc_result(
        id,
        json!({"structuredContent":result,"content":[{"type":"text","text":serde_json::to_string_pretty(&result).unwrap_or_default()}],"isError":!ok}),
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
        "put" | "create" | "update" => {
            let Some(body) = args.get("body") else {
                return rpc_error(id, -32602, "body is required");
            };
            (
                if matches!(operation, "put" | "create") {
                    "PUT"
                } else {
                    "PATCH"
                },
                format!("/v0/management/providers/{}", encode_path_preserving_slashes(provider_id)),
                Some(body.clone()),
            )
        }
        "delete" => {
            if args.get("destructive").and_then(Value::as_str) != Some("acknowledged") {
                return rpc_error(id, -32602, "destructive: \"acknowledged\" is required");
            }
            (
                "DELETE",
                format!("/v0/management/providers/{}", encode_path_preserving_slashes(provider_id)),
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

async fn post(State(state): State<AppState>, peer: Option<axum::extract::ConnectInfo<std::net::SocketAddr>>, headers: HeaderMap, body: Bytes) -> Response {
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
    let started = chrono::Utc::now().timestamp_millis();
    let method = request.get("method").and_then(Value::as_str).map(str::to_owned);
    let tool = request.pointer("/params/name").and_then(Value::as_str).map(str::to_owned);
    let response = dispatch(&state, request).await;
    if let Some(pool) = &state.inner.database {
        let bytes = rand::random::<[u8; 16]>();
        let mut uuid = bytes;
        uuid[6] = (uuid[6] & 0x0f) | 0x40;
        uuid[8] = (uuid[8] & 0x3f) | 0x80;
        let hex = hex::encode(uuid);
        let id = format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..]);
        let duration = chrono::Utc::now().timestamp_millis().saturating_sub(started);
        if let Err(error) = sqlx::query("INSERT INTO mcp_request_usage (request_id,start_time,duration_ms,server_name,upstream_url,method,jsonrpc_method,tool_name,api_key,response_status,source_ip) VALUES ($1,$2,$3,'plexus','/mcp/plexus','POST',$4,$5,'admin',$6,$7)")
            .bind(id).bind(started).bind(duration).bind(method).bind(tool).bind(response.status().as_u16() as i32).bind(peer.map(|peer| peer.0.ip().to_string())).execute(pool).await {
            crate::system_logs::event(&state, "error", "Unable to persist management MCP audit", [("error".into(), json!(error.to_string()))]);
        }
    }
    response
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

#[cfg(test)]
mod tests {
    use super::{dispatch, enabled, router, TOOLS};
    use crate::{state::KeyConfig, AppState};
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use http_body_util::BodyExt;
    use serde_json::{json, Value};
    use std::collections::HashMap;
    use tower::ServiceExt;

    async fn body(response: axum::response::Response) -> Value {
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
    }

    async fn request(
        state: AppState,
        method: &str,
        body: Body,
        key: Option<&str>,
    ) -> axum::response::Response {
        let mut builder = Request::builder().method(method).uri("/mcp/plexus");
        if let Some(key) = key {
            builder = builder.header("x-admin-key", key);
        }
        router()
            .with_state(state)
            .oneshot(builder.body(body).unwrap())
            .await
            .unwrap()
    }

    fn state() -> AppState {
        let state = AppState::new(Some("admin".into()), HashMap::<String, KeyConfig>::new());
        let mut providers = state.inner.providers.write().unwrap();
        providers.insert("zeta".into(), Default::default());
        providers.insert("alpha".into(), Default::default());
        drop(providers);
        state
    }

    fn logging_test_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::logging::test_lock()
    }

    #[tokio::test]
    async fn initialize_and_tools_list_preserve_mcp_wire_contract() {
        let state = state();
        assert!(enabled(&state));
        let initialized = body(
            dispatch(
                &state,
                json!({
                    "jsonrpc":"2.0", "id":"init", "method":"initialize"
                }),
            )
            .await,
        )
        .await;
        assert_eq!(initialized["jsonrpc"], "2.0");
        assert_eq!(initialized["id"], "init");
        assert_eq!(initialized["result"]["protocolVersion"], "2025-06-18");

        let listed = body(
            dispatch(
                &state,
                json!({
                    "jsonrpc":"2.0", "id":1, "method":"tools/list"
                }),
            )
            .await,
        )
        .await;
        assert_eq!(
            listed["result"]["tools"].as_array().unwrap().len(),
            TOOLS.len()
        );
        assert_eq!(listed["result"]["tools"][0]["name"], "plexus_config");
        assert_eq!(
            listed["result"]["tools"][0]["inputSchema"]["required"],
            json!(["operation"])
        );
    }

    #[tokio::test]
    async fn malformed_and_unknown_requests_return_jsonrpc_errors() {
        let state = state();
        let malformed = body(
            dispatch(
                &state,
                json!({"jsonrpc":"1.0","id":7,"method":"tools/list"}),
            )
            .await,
        )
        .await;
        assert_eq!(malformed["error"]["code"], -32600);
        let unknown = body(
            dispatch(
                &state,
                json!({"jsonrpc":"2.0","id":8,"method":"does/not-exist"}),
            )
            .await,
        )
        .await;
        assert_eq!(unknown["error"]["code"], -32601);
        assert_eq!(unknown["id"], 8);
    }



    #[tokio::test]
    async fn config_operations_forward_to_management_routes() {
        for (operation, expected_field) in [("status", "providerCount"), ("get", "providers")] {
            let response = body(
                dispatch(
                    &state(),
                    json!({
                        "jsonrpc":"2.0", "id": 20, "method":"tools/call",
                        "params":{"name":"plexus_config","arguments":{"operation":operation}}
                    }),
                )
                .await,
            )
            .await;
            let structured = &response["result"]["structuredContent"];
            assert_eq!(structured["ok"], true);
            assert!(
                structured["data"][expected_field].is_number()
                    || structured["data"][expected_field].is_object()
            );
            assert_ne!(structured["data"]["mcpEnabled"], true);
        }
    }

    #[tokio::test]
    async fn system_logs_set_level_accepts_body_and_validates_level() {
        let _lock = logging_test_lock();
        let response = body(
            dispatch(
                &state(),
                json!({
                    "jsonrpc":"2.0", "id": 45, "method":"tools/call",
                    "params":{"name":"plexus_system_logs","arguments":{"operation":"set_level","body":{"level":" DEBUG "}}}
                }),
            )
            .await,
        )
        .await;
        assert_eq!(response["result"]["structuredContent"]["ok"], true);
        assert_eq!(
            response["result"]["structuredContent"]["data"]["level"],
            "debug"
        );

        let missing = body(
            dispatch(
                &state(),
                json!({
                    "jsonrpc":"2.0", "id": 46, "method":"tools/call",
                    "params":{"name":"plexus_system_logs","arguments":{"operation":"set_level"}}
                }),
            )
            .await,
        )
        .await;
        assert_eq!(missing["result"]["structuredContent"]["error"]["code"], 400);
        assert_eq!(missing["result"]["structuredContent"]["error"]["message"], "Missing level for set_level operation. Provide body.level or query.level.");
    }

    #[tokio::test]
    async fn system_logs_level_operations_round_trip_through_management_router() {
        let _lock = logging_test_lock();
        let changed = body(
            dispatch(
                &state(),
                json!({
                    "jsonrpc":"2.0", "id": 47, "method":"tools/call",
                    "params":{"name":"plexus_system_logs","arguments":{"operation":"set_level","level":"warn"}}
                }),
            )
            .await,
        )
        .await;
        assert_eq!(changed["result"]["structuredContent"]["ok"], true);
        assert_eq!(
            changed["result"]["structuredContent"]["data"]["level"],
            "warn"
        );

        let read = body(
            dispatch(
                &state(),
                json!({
                    "jsonrpc":"2.0", "id": 48, "method":"tools/call",
                    "params":{"name":"plexus_system_logs","arguments":{"operation":"level"}}
                }),
            )
            .await,
        )
        .await;
        assert_eq!(read["result"]["structuredContent"]["data"]["level"], "warn");

        let reset = body(
            dispatch(
                &state(),
                json!({
                    "jsonrpc":"2.0", "id": 49, "method":"tools/call",
                    "params":{"name":"plexus_system_logs","arguments":{"operation":"reset_level"}}
                }),
            )
            .await,
        )
        .await;
        let reset_data = &reset["result"]["structuredContent"]["data"];
        assert_eq!(reset["result"]["structuredContent"]["ok"], true);
        assert_eq!(reset_data["level"], reset_data["startupLevel"]);
    }

    #[tokio::test]
    async fn unsupported_system_log_modules_are_tool_errors() {
        for operation in ["modules", "set_modules", "reset_modules"] {
            let response = body(dispatch(&state(), json!({"jsonrpc":"2.0","id":50,"method":"tools/call","params":{"name":"plexus_system_logs","arguments":{"operation":operation}}})).await).await;
            assert_eq!(response["result"]["isError"], true);
            assert_eq!(response["result"]["structuredContent"]["error"]["message"], format!("Unsupported operation '{operation}'. Allowed operations: recent, level, set_level, reset_level."));
        }
    }

    #[tokio::test]
    async fn post_enforces_auth_and_reports_parse_errors() {
        let state = state();
        let unauthorized = request(state.clone(), "POST", Body::from("{}"), None).await;
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(body(unauthorized).await["error"]["code"], 401);

        let malformed = request(state, "POST", Body::from("not-json"), Some("admin")).await;
        assert_eq!(malformed.status(), StatusCode::OK);
        let value = body(malformed).await;
        assert_eq!(value["jsonrpc"], "2.0");
        assert_eq!(value["id"], Value::Null);
        assert_eq!(value["error"]["code"], -32700);
    }

    #[tokio::test]
    async fn tool_validation_and_unknown_tool_are_jsonrpc_success_envelopes() {
        let state = state();
        let missing_name = body(
            dispatch(
                &state,
                json!({
                    "jsonrpc":"2.0", "id": 3, "method":"tools/call", "params":{}
                }),
            )
            .await,
        )
        .await;
        assert_eq!(missing_name["error"]["code"], -32602);

        let missing_id = body(
            dispatch(
                &state,
                json!({
                    "jsonrpc":"2.0", "id": 4, "method":"tools/call",
                    "params":{"name":"plexus_mcp_gateway","arguments":{"operation":"get"}}
                }),
            )
            .await,
        )
        .await;
        assert_eq!(missing_id["result"]["structuredContent"]["error"]["code"], 400);
        assert_eq!(missing_id["result"]["structuredContent"]["error"]["message"], "Missing id for mcp_gateway operation.");

        let unknown = body(
            dispatch(
                &state,
                json!({
                    "jsonrpc":"2.0", "id": 5, "method":"tools/call",
                    "params":{"name":"not-a-tool","arguments":{"operation":"list"}}
                }),
            )
            .await,
        )
        .await;
        assert_eq!(unknown["result"]["isError"], true);
        assert_eq!(unknown["result"]["content"][0]["text"], "Unknown tool");
    }

    #[tokio::test]
    async fn mcp_gateway_status_is_forwarded_to_management_route() {
        let response = body(
            dispatch(
                &state(),
                json!({
                    "jsonrpc":"2.0", "id": 6, "method":"tools/call",
                    "params":{"name":"plexus_mcp_gateway","arguments":{"operation":"status","id":"missing"}}
                }),
            )
            .await,
        )
        .await;
        let structured = &response["result"]["structuredContent"];
        assert_eq!(structured["operation"], "status");
        assert!(structured["error"].is_object());
        assert_ne!(
            structured["error"]["message"],
            "Operation is not implemented yet"
        );
    }

    #[tokio::test]
    async fn mcp_gateway_process_logs_requires_id() {
        let response = body(
            dispatch(
                &state(),
                json!({
                    "jsonrpc":"2.0", "id": 12, "method":"tools/call",
                    "params":{"name":"plexus_mcp_gateway","arguments":{"operation":"logs"}}
                }),
            )
            .await,
        )
        .await;
        assert_eq!(response["result"]["structuredContent"]["error"]["code"], 400);
        assert_eq!(response["result"]["structuredContent"]["error"]["message"], "Missing id for mcp_gateway operation.");
    }



    #[tokio::test]
    async fn operations_reset_logs_uses_usage_reset_endpoint() {
        let response = body(
            dispatch(
                &state(),
                json!({
                    "jsonrpc":"2.0", "id": 45, "method":"tools/call",
                    "params":{"name":"plexus_operations","arguments":{
                        "operation":"reset_logs", "destructive":"acknowledged"
                    }}
                }),
            )
            .await,
        )
        .await;
        let structured = &response["result"]["structuredContent"];
        assert_eq!(structured["operation"], "reset_logs");
        assert_ne!(
            structured["error"]["message"],
            "Operation is not implemented yet"
        );
    }

    #[tokio::test]
    async fn settings_supports_all_config_categories_and_rejects_unknown() {
        let state = state();
        let all = body(
            dispatch(
                &state,
                json!({
                    "jsonrpc":"2.0", "id": 14, "method":"tools/call",
                    "params":{"name":"plexus_settings","arguments":{"operation":"get"}}
                }),
            )
            .await,
        )
        .await;
        let data = &all["result"]["structuredContent"]["data"];
        for category in [
            "failover",
            "cooldown",
            "timeout",
            "stall",
            "trusted_proxies",
            "vision_fallthrough",
            "background_exploration",
            "exploration",
        ] {
            assert!(data.get(category).is_some(), "missing category {category}");
        }

        let unknown = body(
            dispatch(
                &state,
                json!({
                    "jsonrpc":"2.0", "id": 15, "method":"tools/call",
                    "params":{"name":"plexus_settings","arguments":{"operation":"get","category":"nope"}}
                }),
            )
            .await,
        )
        .await;
        assert_eq!(unknown["result"]["structuredContent"]["error"]["code"], 404);
        assert_eq!(unknown["result"]["isError"], true);
    }



    #[tokio::test]
    async fn disabled_mcp_rejects_all_http_methods_before_dispatch() {
        let state = state();
        state
            .inner
            .settings
            .write()
            .unwrap()
            .insert("mcpEnabled".into(), json!(false));
        for method in ["GET", "POST", "DELETE"] {
            let response = request(state.clone(), method, Body::from("{}"), Some("admin")).await;
            assert_eq!(response.status(), StatusCode::IM_A_TEAPOT);
            assert_eq!(body(response).await["error"]["type"], "mcp_disabled");
        }
    }
    #[tokio::test]
    async fn reference_rejects_unsupported_config_operation_as_tool_result() {
        let response = body(dispatch(&state(), json!({
            "jsonrpc":"2.0", "id": 99, "method":"tools/call",
            "params":{"name":"plexus_config","arguments":{"operation":"timeout"}}
        })).await).await;
        assert_eq!(response["result"]["structuredContent"]["ok"], false);
        assert_eq!(response["result"]["structuredContent"]["error"]["type"], "invalid_request");
    }

}

#[cfg(test)]
mod redaction_regression_tests {
    use super::*;
    #[test]
    fn provider_redaction_preserves_shape_and_covers_nested_sensitive_fields() {
        let provider = crate::state::ProviderConfig {
            api_key: Some("private".into()),
            ..Default::default()
        };
        let original = serde_json::to_value(&provider).unwrap();
        let redacted = redact_provider(&provider);
        assert_eq!(
            redacted.as_object().unwrap().keys().collect::<Vec<_>>(),
            original.as_object().unwrap().keys().collect::<Vec<_>>()
        );
        assert_eq!(redacted["api_key"], "[REDACTED]");
        let nested = redact_key_secrets(
            json!({"safe":"keep", "nested":[{"dbPassword":"private","managementApiKey":"private","authCookie":"private"}]}),
        );
        assert_eq!(nested["safe"], "keep");
        assert!(!nested.to_string().contains("private"));
    }
}

#[cfg(test)]
mod collection_contract_tests {
    use super::*;
    #[test]
    fn record_collections_become_id_bearing_arrays() {
        assert_eq!(record_list(json!({})), json!([]));
        assert_eq!(record_list(json!({"alpha":{"enabled":true}})), json!([{"id":"alpha","enabled":true}]));
    }
}
