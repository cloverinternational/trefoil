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
        ("plexus_quota", "status") | ("plexus_quota", "clear") | ("plexus_quota", "recompute") => {
            return quota_operation(state, id, operation, args).await;
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
