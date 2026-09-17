//! Minimal stateless MCP JSON-RPC management endpoint.

use crate::{
    auth::{resolve_principal, Principal},
    AppState,
};
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json, Router,
};
use serde_json::Value;

pub fn router() -> Router<AppState> {
    Router::new().route("/mcp/plexus", axum::routing::post(handle))
}

async fn handle(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if !state
        .inner
        .settings
        .read()
        .unwrap()
        .get("mcpEnabled")
        .and_then(Value::as_bool)
        .unwrap_or(true)
    {
        return (StatusCode::IM_A_TEAPOT,Json(serde_json::json!({"error":{"message":"Plexus Management MCP is disabled","type":"mcp_disabled","code":418}}))).into_response();
    }
    match resolve_principal(&state,headers.get("x-admin-key").and_then(|v|v.to_str().ok())){Some(Principal::Admin)=>{},Some(Principal::Limited{..})=>return (StatusCode::FORBIDDEN,Json(serde_json::json!({"error":{"message":"Admin privileges required","type":"forbidden","code":403}}))).into_response(),None=>return (StatusCode::UNAUTHORIZED,Json(serde_json::json!({"error":{"message":"Unauthorized","type":"auth_error","code":401}}))).into_response()}
    let id = body.get("id").cloned().unwrap_or(Value::Null);
    match body.get("method").and_then(Value::as_str){
        Some("tools/list")=>{let names=["plexus_config","plexus_provider","plexus_model_alias","plexus_key","plexus_quota","plexus_quota_checker","plexus_usage","plexus_debug","plexus_mcp_gateway","plexus_settings","plexus_system_logs","plexus_operations"];let tools:Vec<Value>=names.iter().map(|name|serde_json::json!({"name":name,"title":name,"description":"Plexus management operation","inputSchema":{"type":"object"}})).collect();Json(serde_json::json!({"jsonrpc":"2.0","id":id,"result":{"tools":tools}})).into_response()},
        Some("tools/call")=>{let name=body.pointer("/params/name").and_then(Value::as_str);if name==Some("config_status"){Json(serde_json::json!({"jsonrpc":"2.0","id":id,"result":{"content":[{"type":"text","text":serde_json::to_string(&serde_json::json!({"providerCount":state.inner.providers.read().unwrap().len(),"modelAliasCount":state.inner.aliases.read().unwrap().len(),"keyCount":state.inner.keys.read().unwrap().len(),"quotaCount":state.inner.user_quotas.read().unwrap().len()})).unwrap()}]}})).into_response()}else{Json(serde_json::json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Unknown tool"}})).into_response()}},
        Some(method)=>Json(serde_json::json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":format!("Method not found: {method}")}})).into_response(),
        None=>(StatusCode::BAD_REQUEST,Json(serde_json::json!({"error":"Invalid JSON-RPC request"}))).into_response()
    }
}
