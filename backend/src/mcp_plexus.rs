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

fn dispatch(state: &AppState, request: Value) -> Response {
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
        "tools/call" => call_tool(
            state,
            id,
            object
                .get("params")
                .and_then(|p| p.get("name"))
                .and_then(Value::as_str),
            object.get("params").and_then(|p| p.get("arguments")),
        ),
        _ => rpc_error(id, -32601, "Method not found"),
    }
}

fn call_tool(state: &AppState, id: Value, name: Option<&str>, args: Option<&Value>) -> Response {
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
    dispatch(&state, request)
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
