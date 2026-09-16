//! Initial management config surface: status, timeout, and MCP enablement.
//!
//! Ports `GET /v0/management/config/status` and `GET`/`PATCH
//! `/v0/management/config/timeout` and `/v0/management/config/mcp-enabled`
//! from Plexus. MCP server management is not yet ported, so
//! `mcpServerCount` is currently zero.

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json, Router,
};

use crate::{
    auth::{resolve_principal, Principal},
    AppState,
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/v0/management/config", axum::routing::get(config_snapshot))
        .route(
            "/v0/management/config/export",
            axum::routing::get(config_export),
        )
        .route(
            "/v0/management/config/status",
            axum::routing::get(config_status),
        )
        .route(
            "/v0/management/config/timeout",
            axum::routing::get(get_timeout).patch(patch_timeout),
        )
        .route(
            "/v0/management/config/mcp-enabled",
            axum::routing::get(get_mcp_enabled).patch(patch_mcp_enabled),
        )
        .route(
            "/v0/management/config/exploration-rate",
            axum::routing::get(get_exploration_rate).patch(patch_exploration_rate),
        )
        .route(
            "/v0/management/config/failover",
            axum::routing::get(get_failover).patch(patch_failover),
        )
        .route(
            "/v0/management/config/cooldown",
            axum::routing::get(get_cooldown).patch(patch_cooldown),
        )
        .route(
            "/v0/management/config/capture-trace-on-error",
            axum::routing::get(get_capture_trace_on_error).patch(patch_capture_trace_on_error),
        )
        .route(
            "/v0/management/config/stall",
            axum::routing::get(get_stall).patch(patch_stall),
        )
        .route(
            "/v0/management/config/vision-fallthrough",
            axum::routing::get(get_vision_fallthrough).patch(patch_vision_fallthrough),
        )
        .route(
            "/v0/management/config/background-exploration",
            axum::routing::get(get_background_exploration).patch(patch_background_exploration),
        )
        .route(
            "/v0/management/config/trusted-proxies",
            axum::routing::get(get_trusted_proxies).patch(patch_trusted_proxies),
        )
        .route(
            "/v0/management/cache/vision-descriptor/clear",
            axum::routing::post(clear_vision_descriptor_cache),
        )
        .route(
            "/v0/management/quota-checker-types",
            axum::routing::get(quota_checker_types),
        )
        .route(
            "/v0/management/pi/providers",
            axum::routing::get(pi_providers),
        )
        .route(
            "/v0/management/config/compaction",
            axum::routing::get(get_compaction).patch(patch_compaction),
        )
}

async fn get_compaction(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    Json(config_value(&state, "compaction", serde_json::json!({}))).into_response()
}

fn merge_compaction(
    current: &serde_json::Map<String, serde_json::Value>,
    patch: &serde_json::Map<String, serde_json::Value>,
) -> serde_json::Map<String, serde_json::Value> {
    let mut merged = current.clone();
    for (key, value) in patch {
        if value.is_null() {
            merged.remove(key);
        } else if matches!(key.as_str(), "native" | "headroom") && value.is_object() {
            let current_nested = merged
                .get(key)
                .and_then(serde_json::Value::as_object)
                .cloned()
                .unwrap_or_default();
            let nested = merge_compaction(&current_nested, value.as_object().unwrap());
            if nested.is_empty() {
                merged.remove(key);
            } else {
                merged.insert(key.clone(), serde_json::Value::Object(nested));
            }
        } else {
            merged.insert(key.clone(), value.clone());
        }
    }
    merged
}

fn compaction_issue(
    object: &serde_json::Map<String, serde_json::Value>,
) -> Option<serde_json::Value> {
    if let Some(value) = object.get("enabled") {
        if !value.is_boolean() {
            return Some(type_issue("boolean", ["enabled"]));
        }
    }
    if let Some(value) = object.get("strategy") {
        if !matches!(value.as_str(), Some("native" | "headroom")) {
            return Some(serde_json::json!({
                "code": "invalid_value", "values": ["native", "headroom"],
                "path": ["strategy"], "message": "Invalid option: expected one of \"native\"|\"headroom\""
            }));
        }
    }
    for (key, min, inclusive, nullable) in [
        ("triggerRatio", 0.0, true, false),
        ("absoluteTriggerTokens", 0.0, false, true),
        ("minTokens", 0.0, true, false),
        ("protectRecent", 0.0, true, false),
    ] {
        if let Some(value) = object.get(key) {
            if nullable && value.is_null() {
                continue;
            }
            let Some(number) = value.as_f64() else {
                return Some(type_issue("number", [key]));
            };
            if key != "triggerRatio" && number.fract() != 0.0 {
                return Some(serde_json::json!({
                    "expected":"int","format":"safeint","code":"invalid_type","path":[key],
                    "message":"Invalid input: expected int, received number"
                }));
            }
            if number < min || (!inclusive && number <= min) {
                return Some(too_small_issue([key], min, inclusive));
            }
            if key == "triggerRatio" && number > 1.0 {
                return Some(serde_json::json!({
                    "origin":"number","code":"too_big","maximum":1,"inclusive":true,
                    "path":[key],"message":"Too big: expected number to be <=1"
                }));
            }
        }
    }
    if let Some(native) = object.get("native") {
        let Some(native) = native.as_object() else {
            return Some(type_issue("object", ["native"]));
        };
        for key in ["maxArrayItems", "maxStringChars"] {
            if let Some(value) = native.get(key) {
                let Some(number) = value.as_f64() else {
                    return Some(type_issue("number", ["native", key]));
                };
                if number.fract() != 0.0 || number <= 0.0 {
                    return Some(too_small_issue(["native", key], 0.0, false));
                }
            }
        }
    }
    if let Some(headroom) = object.get("headroom") {
        let Some(headroom) = headroom.as_object() else {
            return Some(type_issue("object", ["headroom"]));
        };
        if let Some(value) = headroom.get("baseUrl") {
            if value
                .as_str()
                .and_then(|v| url::Url::parse(v).ok())
                .is_none()
            {
                return Some(serde_json::json!({
                    "code":"invalid_format","format":"url","path":["headroom","baseUrl"],
                    "message":"Invalid URL"
                }));
            }
        }
        if headroom.get("apiKey").is_some_and(|v| !v.is_string()) {
            return Some(type_issue("string", ["headroom", "apiKey"]));
        }
        if let Some(value) = headroom.get("targetRatio") {
            if !value.is_null() {
                let Some(number) = value.as_f64() else {
                    return Some(type_issue("number", ["headroom", "targetRatio"]));
                };
                if !(0.0..=1.0).contains(&number) {
                    return Some(if number < 0.0 {
                        too_small_issue(["headroom", "targetRatio"], 0.0, true)
                    } else {
                        serde_json::json!({"origin":"number","code":"too_big","maximum":1,"inclusive":true,"path":["headroom","targetRatio"],"message":"Too big: expected number to be <=1"})
                    });
                }
            }
        }
        if let Some(value) = headroom.get("timeoutMs") {
            let Some(number) = value.as_f64() else {
                return Some(type_issue("number", ["headroom", "timeoutMs"]));
            };
            if number.fract() != 0.0 || !(100.0..=60000.0).contains(&number) {
                return Some(if number < 100.0 {
                    too_small_issue(["headroom", "timeoutMs"], 100.0, true)
                } else {
                    serde_json::json!({"origin":"number","code":"too_big","maximum":60000,"inclusive":true,"path":["headroom","timeoutMs"],"message":"Too big: expected number to be <=60000"})
                });
            }
        }
    }
    None
}

fn type_issue<const N: usize>(expected: &str, path: [&str; N]) -> serde_json::Value {
    let path = path.to_vec();
    serde_json::json!({
        "expected": expected, "code": "invalid_type", "path": path,
        "message": format!("Invalid input: expected {expected}, received {}", "invalid")
    })
}

fn too_small_issue<const N: usize>(
    path: [&str; N],
    minimum: f64,
    inclusive: bool,
) -> serde_json::Value {
    let path = path.to_vec();
    let comparison = if inclusive { ">=" } else { ">" };
    serde_json::json!({
        "origin":"number","code":"too_small","minimum":minimum,"inclusive":inclusive,
        "path":path,"message":format!("Too small: expected number to be {comparison}{minimum}")
    })
}

async fn patch_compaction(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    let Some(patch) = body.as_object() else {
        return bad_request("Object body is required");
    };
    let current = config_value(&state, "compaction", serde_json::json!({}))
        .as_object()
        .cloned()
        .unwrap_or_default();
    let mut merged = merge_compaction(&current, patch);
    // Zod objects strip unknown keys.
    merged.retain(|key, _| {
        matches!(
            key.as_str(),
            "enabled"
                | "strategy"
                | "triggerRatio"
                | "absoluteTriggerTokens"
                | "minTokens"
                | "protectRecent"
                | "native"
                | "headroom"
        )
    });
    if let Some(issue) = compaction_issue(&merged) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "Invalid compaction config", "details": [issue] })),
        )
            .into_response();
    }
    let value = serde_json::Value::Object(merged);
    let entries = serde_json::Map::from_iter([("compaction".into(), value.clone())]);
    if let Some(pool) = &state.inner.database {
        if crate::database::upsert_settings_bulk(pool, &entries)
            .await
            .is_err()
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error":"Internal server error"})),
            )
                .into_response();
        }
    }
    state.inner.settings.write().unwrap().extend(entries);
    Json(value).into_response()
}

async fn pi_providers(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    // Pinned to Plexus's pi-ai provider registry at the reference revision.
    let data = [
        "amazon-bedrock",
        "ant-ling",
        "anthropic",
        "azure-openai-responses",
        "baseten",
        "cerebras",
        "cloudflare-ai-gateway",
        "cloudflare-workers-ai",
        "deepseek",
        "fireworks",
        "github-copilot",
        "google",
        "google-vertex",
        "groq",
        "huggingface",
        "kimi-coding",
        "minimax",
        "minimax-cn",
        "mistral",
        "moonshotai",
        "moonshotai-cn",
        "nvidia",
        "openai",
        "openai-codex",
        "opencode",
        "opencode-go",
        "openrouter",
        "qwen-token-plan",
        "qwen-token-plan-cn",
        "qwen-token-plan-individual",
        "together",
        "vercel-ai-gateway",
        "xai",
        "xiaomi",
        "xiaomi-token-plan-ams",
        "xiaomi-token-plan-cn",
        "xiaomi-token-plan-sgp",
        "zai",
        "zai-coding-cn",
    ];
    Json(serde_json::json!({ "data": data.as_slice() })).into_response()
}

async fn quota_checker_types(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    // Plexus builds this from its checker registry. Trefoil does not yet run
    // provider quota checkers, so discovery mirrors the pinned reference
    // revision's registry for management-client compatibility.
    let types = serde_json::json!([
        {"type":"naga","displayName":"Naga"},
        {"type":"synthetic","displayName":"Synthetic"},
        {"type":"nanogpt","displayName":"NanoGPT"},
        {"type":"zai","displayName":"ZAI Coding Plan (Global)"},
        {"type":"moonshot","displayName":"Moonshot"},
        {"type":"novita","displayName":"Novita"},
        {"type":"minimax","displayName":"MiniMax"},
        {"type":"minimax-coding","displayName":"MiniMax Coding"},
        {"type":"openrouter","displayName":"OpenRouter"},
        {"type":"kilo","displayName":"Kilo"},
        {"type":"openai-codex","displayName":"OpenAI Codex"},
        {"type":"kimi-code","displayName":"Kimi Code"},
        {"type":"claude-code","displayName":"Claude Code"},
        {"type":"copilot","displayName":"GitHub Copilot"},
        {"type":"wisdomgate","displayName":"Wisdom Gate"},
        {"type":"apertis","displayName":"Apertis"},
        {"type":"poe","displayName":"POE"},
        {"type":"routing-run","displayName":"Routing.run"},
        {"type":"ollama","displayName":"Ollama"},
        {"type":"neuralwatt","displayName":"Neuralwatt"},
        {"type":"zenmux","displayName":"Zenmux"},
        {"type":"devpass","displayName":"DevPass"},
        {"type":"wafer","displayName":"Wafer"},
        {"type":"opencode-go","displayName":"OpenCode Go"},
        {"type":"crof","displayName":"Crof"},
        {"type":"exedev","displayName":"exe.dev"},
        {"type":"hyper","displayName":"Hyper"},
        {"type":"deepseek","displayName":"DeepSeek"},
        {"type":"sakana","displayName":"Sakana"},
        {"type":"cline","displayName":"Cline"}
    ]);
    Json(serde_json::json!({ "types": types, "count": 30 })).into_response()
}

async fn clear_vision_descriptor_cache(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    // Vision descriptor generation/cache storage is not yet implemented in
    // Trefoil, so clearing the currently-empty cache is an idempotent no-op.
    Json(serde_json::json!({
        "success": true,
        "message": "Vision descriptor cache cleared."
    }))
    .into_response()
}

async fn config_snapshot(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    let settings = state.inner.settings.read().unwrap();
    let mut config: serde_json::Map<String, serde_json::Value> = settings
        .iter()
        .filter(|(key, _)| !key.contains('.'))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    drop(settings);

    config.insert(
        "providers".into(),
        serde_json::to_value(state.inner.providers.read().unwrap().clone()).unwrap(),
    );
    config.insert(
        "models".into(),
        serde_json::to_value(state.inner.aliases.read().unwrap().clone()).unwrap(),
    );
    config.insert(
        "keys".into(),
        serde_json::to_value(state.inner.keys.read().unwrap().clone()).unwrap(),
    );
    config.insert("failover".into(), failover_body(&state));
    config.insert("cooldown".into(), cooldown_body(&state));
    config.insert(
        "timeout".into(),
        serde_json::json!({ "defaultSeconds": timeout_seconds(&state) }),
    );
    config.insert("stall".into(), stall_body(&state));
    config.insert(
        "backgroundExploration".into(),
        background_exploration_body(&state),
    );
    config.insert(
        "mcpOAuth".into(),
        config_value(
            &state,
            "mcpOAuth",
            serde_json::json!({ "enabled": false, "provider": "plexus-idp" }),
        ),
    );
    // Provider quota-checker configuration is outside Trefoil's current
    // ProviderConfig subset; expose the truthful derived empty list.
    config.insert("quotas".into(), serde_json::json!([]));
    let user_quotas = state.inner.user_quotas.read().unwrap();
    if !user_quotas.is_empty() {
        config.insert(
            "user_quotas".into(),
            serde_json::to_value(user_quotas.clone()).unwrap(),
        );
    }
    Json(serde_json::Value::Object(config)).into_response()
}

async fn config_export(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    Json(serde_json::json!({
        "providers": state.inner.providers.read().unwrap().clone(),
        "models": state.inner.aliases.read().unwrap().clone(),
        "keys": state.inner.keys.read().unwrap().clone(),
        "user_quotas": state.inner.user_quotas.read().unwrap().clone(),
        "mcp_servers": {},
        "mcp_keys": [],
        "settings": state.inner.settings.read().unwrap().clone(),
        "oauth_providers": [],
    }))
    .into_response()
}

fn failover_body(state: &AppState) -> serde_json::Value {
    let retryable_status_codes: Vec<i32> = (100..=599)
        .filter(|code| !(200..=299).contains(code) && *code != 413 && *code != 422)
        .collect();
    serde_json::json!({
        "enabled": config_value(state, "failover.enabled", serde_json::json!(true)),
        "retryableStatusCodes": config_value(state, "failover.retryableStatusCodes", serde_json::json!(retryable_status_codes)),
        "retryableErrors": config_value(state, "failover.retryableErrors", serde_json::json!(["ECONNREFUSED", "ETIMEDOUT", "ENOTFOUND"])),
    })
}

fn cooldown_body(state: &AppState) -> serde_json::Value {
    serde_json::json!({
        "initialMinutes": config_value(state, "cooldown.initialMinutes", serde_json::json!(2)),
        "maxMinutes": config_value(state, "cooldown.maxMinutes", serde_json::json!(300)),
    })
}

fn stall_body(state: &AppState) -> serde_json::Value {
    serde_json::json!({
        "ttfbSeconds": config_value(state, "stall.ttfbSeconds", serde_json::Value::Null),
        "ttfbBytes": config_value(state, "stall.ttfbBytes", serde_json::json!(100)),
        "minBytesPerSecond": config_value(state, "stall.minBytesPerSecond", serde_json::Value::Null),
        "windowSeconds": config_value(state, "stall.windowSeconds", serde_json::json!(10)),
        "gracePeriodSeconds": config_value(state, "stall.gracePeriodSeconds", serde_json::json!(30)),
        "stallCooldown": config_value(state, "stall.stallCooldown", serde_json::json!(false)),
    })
}

#[derive(Clone, Copy, PartialEq)]
enum IpFamily {
    V4,
    V6,
}

fn parse_ip(value: &str) -> Option<(IpFamily, u128)> {
    match value.trim().parse::<std::net::IpAddr>().ok()? {
        std::net::IpAddr::V4(ip) => Some((IpFamily::V4, u32::from(ip) as u128)),
        std::net::IpAddr::V6(ip) => ip
            .to_ipv4_mapped()
            .map(|mapped| (IpFamily::V4, u32::from(mapped) as u128))
            .or_else(|| Some((IpFamily::V6, u128::from(ip)))),
    }
}

fn valid_ip_rule(rule: &str) -> bool {
    let rule = rule.trim();
    if rule.is_empty() || rule.contains('%') || rule.starts_with('[') {
        return false;
    }
    if let Some((address, prefix)) = rule.split_once('/') {
        let Some((family, _)) = parse_ip(address) else {
            return false;
        };
        let Ok(prefix) = prefix.parse::<u16>() else {
            return false;
        };
        return prefix <= if family == IpFamily::V4 { 32 } else { 128 };
    }
    if let Some((left, right)) = rule.split_once('-') {
        let Some((family, low)) = parse_ip(left) else {
            return false;
        };
        if family == IpFamily::V4
            && !right.contains('.')
            && right.chars().all(|c| c.is_ascii_digit())
        {
            let Ok(octet) = right.parse::<u16>() else {
                return false;
            };
            if octet > 255 {
                return false;
            }
            let high = (low & !0xff) | u128::from(octet);
            return high >= low;
        }
        let Some((high_family, high)) = parse_ip(right) else {
            return false;
        };
        return family == high_family && high >= low;
    }
    parse_ip(rule).is_some()
}

fn trusted_proxies(state: &AppState) -> serde_json::Value {
    config_value(
        state,
        "trustedProxies",
        serde_json::json!(["0.0.0.0/0", "::/0"]),
    )
}

async fn get_trusted_proxies(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    Json(serde_json::json!({ "trustedProxies": trusted_proxies(&state) })).into_response()
}

async fn patch_trusted_proxies(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    let Some(object) = body.as_object() else {
        return bad_request("Object body is required");
    };
    let Some(values) = object
        .get("trustedProxies")
        .and_then(serde_json::Value::as_array)
        .filter(|values| values.iter().all(serde_json::Value::is_string))
    else {
        return bad_request("trustedProxies must be an array of strings");
    };
    let normalized: Vec<String> = values
        .iter()
        .filter_map(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect();
    if let Some(invalid) = normalized.iter().find(|value| !valid_ip_rule(value)) {
        return bad_request(&format!(
            "Invalid IP rule: {invalid}. Use IPv4/IPv6, CIDR (a.b.c.d/n, ::/n), or a range (a.b.c.d-N or addr-addr)."
        ));
    }
    let value = serde_json::json!(normalized);
    let entries = serde_json::Map::from_iter([("trustedProxies".into(), value.clone())]);
    if let Some(pool) = &state.inner.database {
        if crate::database::upsert_settings_bulk(pool, &entries)
            .await
            .is_err()
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "Internal server error" })),
            )
                .into_response();
        }
    }
    state.inner.settings.write().unwrap().extend(entries);
    Json(serde_json::json!({ "trustedProxies": value })).into_response()
}

fn background_exploration_body(state: &AppState) -> serde_json::Value {
    serde_json::json!({
        "enabled": config_value(&state, "backgroundExploration.enabled", serde_json::json!(false)),
        "stalenessThresholdSeconds": config_value(&state, "backgroundExploration.stalenessThresholdSeconds", serde_json::json!(600)),
        "workerConcurrency": config_value(&state, "backgroundExploration.workerConcurrency", serde_json::json!(2)),
    })
}

async fn get_background_exploration(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    Json(background_exploration_body(&state)).into_response()
}

async fn patch_background_exploration(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    let Some(object) = body.as_object() else {
        return bad_request("Object body is required");
    };
    let mut entries = serde_json::Map::new();
    if let Some(raw) = object.get("enabled") {
        let Some(value) = raw.as_bool() else {
            return bad_request("enabled must be a boolean");
        };
        entries.insert(
            "backgroundExploration.enabled".into(),
            serde_json::json!(value),
        );
    }
    if let Some(raw) = object.get("stalenessThresholdSeconds") {
        let Some(value) = js_number(raw)
            .filter(|value| value.is_finite() && value.fract() == 0.0 && *value >= 1.0)
        else {
            return bad_request("stalenessThresholdSeconds must be an integer >= 1");
        };
        entries.insert(
            "backgroundExploration.stalenessThresholdSeconds".into(),
            serde_json::json!(value as i64),
        );
    }
    if let Some(raw) = object.get("workerConcurrency") {
        let Some(value) = js_number(raw).filter(|value| {
            value.is_finite() && value.fract() == 0.0 && (1.0..=16.0).contains(value)
        }) else {
            return bad_request("workerConcurrency must be an integer between 1 and 16");
        };
        entries.insert(
            "backgroundExploration.workerConcurrency".into(),
            serde_json::json!(value as i64),
        );
    }
    if !entries.is_empty() {
        if let Some(pool) = &state.inner.database {
            if crate::database::upsert_settings_bulk(pool, &entries)
                .await
                .is_err()
            {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({ "error": "Internal server error" })),
                )
                    .into_response();
            }
        }
        state.inner.settings.write().unwrap().extend(entries);
    }
    Json(background_exploration_body(&state)).into_response()
}

async fn get_vision_fallthrough(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    Json(config_value(
        &state,
        "vision_fallthrough",
        serde_json::json!({}),
    ))
    .into_response()
}

async fn patch_vision_fallthrough(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    let Some(updates) = body.as_object() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":"Object body is required"})),
        )
            .into_response();
    };
    let mut merged = config_value(&state, "vision_fallthrough", serde_json::json!({}))
        .as_object()
        .cloned()
        .unwrap_or_default();
    merged.extend(updates.clone());
    let merged = serde_json::Value::Object(merged);
    let entries =
        serde_json::Map::from_iter([(String::from("vision_fallthrough"), merged.clone())]);
    if let Some(pool) = &state.inner.database {
        if crate::database::upsert_settings_bulk(pool, &entries)
            .await
            .is_err()
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error":"Internal server error"})),
            )
                .into_response();
        }
    }
    state.inner.settings.write().unwrap().extend(entries);
    Json(merged).into_response()
}

async fn get_stall(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    Json(serde_json::json!({
        "ttfbSeconds": config_value(&state, "stall.ttfbSeconds", serde_json::Value::Null),
        "ttfbBytes": config_value(&state, "stall.ttfbBytes", serde_json::json!(100)),
        "minBytesPerSecond": config_value(&state, "stall.minBytesPerSecond", serde_json::Value::Null),
        "windowSeconds": config_value(&state, "stall.windowSeconds", serde_json::json!(10)),
        "gracePeriodSeconds": config_value(&state, "stall.gracePeriodSeconds", serde_json::json!(30)),
        "stallCooldown": config_value(&state, "stall.stallCooldown", serde_json::json!(false)),
    }))
    .into_response()
}
async fn patch_stall(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    let Some(object) = body.as_object() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":"Object body is required"})),
        )
            .into_response();
    };
    let mut entries = serde_json::Map::new();
    if let Some(raw) = object.get("ttfbSeconds") {
        if raw.is_null() {
            entries.insert("stall.ttfbSeconds".into(), serde_json::Value::Null);
        } else if let Some(value) =
            js_number(raw).filter(|value| value.is_finite() && (5.0..=120.0).contains(value))
        {
            entries.insert("stall.ttfbSeconds".into(), serde_json::json!(value));
        } else {
            return bad_request("ttfbSeconds must be null or a number between 5 and 120");
        }
    }
    if let Some(raw) = object.get("ttfbBytes") {
        if let Some(value) = js_number(raw).filter(|value| {
            value.is_finite() && value.fract() == 0.0 && (50.0..=10000.0).contains(value)
        }) {
            entries.insert("stall.ttfbBytes".into(), serde_json::json!(value as i64));
        } else {
            return bad_request("ttfbBytes must be an integer between 50 and 10000");
        }
    }
    if let Some(raw) = object.get("minBytesPerSecond") {
        if raw.is_null() {
            entries.insert("stall.minBytesPerSecond".into(), serde_json::Value::Null);
        } else if let Some(value) = js_number(raw).filter(|value| {
            value.is_finite() && value.fract() == 0.0 && (50.0..=5000.0).contains(value)
        }) {
            entries.insert(
                "stall.minBytesPerSecond".into(),
                serde_json::json!(value as i64),
            );
        } else {
            return bad_request("minBytesPerSecond must be null or an integer between 50 and 5000");
        }
    }
    for (key, min, max) in [
        ("windowSeconds", 3.0, 30.0),
        ("gracePeriodSeconds", 0.0, 120.0),
    ] {
        if let Some(raw) = object.get(key) {
            if let Some(value) = js_number(raw).filter(|value| {
                value.is_finite() && value.fract() == 0.0 && (min..=max).contains(value)
            }) {
                entries.insert(format!("stall.{key}"), serde_json::json!(value as i64));
            } else {
                return bad_request(&format!(
                    "{key} must be an integer between {} and {}",
                    min as i64, max as i64
                ));
            }
        }
    }
    if let Some(raw) = object.get("stallCooldown") {
        if let Some(value) = raw.as_bool() {
            entries.insert("stall.stallCooldown".into(), serde_json::json!(value));
        } else {
            return bad_request("stallCooldown must be a boolean");
        }
    }
    if let Some(pool) = &state.inner.database {
        if crate::database::upsert_settings_bulk(pool, &entries)
            .await
            .is_err()
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error":"Internal server error"})),
            )
                .into_response();
        }
    }
    state.inner.settings.write().unwrap().extend(entries);
    get_stall(State(state), headers).await
}

fn bad_request(message: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({ "error": message })),
    )
        .into_response()
}

fn config_value(state: &AppState, key: &str, default: serde_json::Value) -> serde_json::Value {
    state
        .inner
        .settings
        .read()
        .unwrap()
        .get(key)
        .cloned()
        .unwrap_or(default)
}

async fn get_failover(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    let retryable_status_codes: Vec<i32> = (100..=599)
        .filter(|code| !(200..=299).contains(code) && *code != 413 && *code != 422)
        .collect();
    Json(serde_json::json!({
        "enabled": config_value(&state, "failover.enabled", serde_json::json!(true)),
        "retryableStatusCodes": config_value(
            &state,
            "failover.retryableStatusCodes",
            serde_json::json!(retryable_status_codes),
        ),
        "retryableErrors": config_value(
            &state,
            "failover.retryableErrors",
            serde_json::json!(["ECONNREFUSED", "ETIMEDOUT", "ENOTFOUND"]),
        ),
    }))
    .into_response()
}

async fn patch_failover(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    let Some(object) = body.as_object() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":"Object body is required"})),
        )
            .into_response();
    };
    let mut entries = serde_json::Map::new();
    for key in ["enabled", "retryableStatusCodes", "retryableErrors"] {
        if let Some(value) = object.get(key) {
            entries.insert(format!("failover.{key}"), value.clone());
        }
    }
    if let Some(pool) = &state.inner.database {
        if crate::database::upsert_settings_bulk(pool, &entries)
            .await
            .is_err()
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error":"Internal server error"})),
            )
                .into_response();
        }
    }
    state.inner.settings.write().unwrap().extend(entries);
    get_failover(State(state), headers).await
}

async fn get_cooldown(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    Json(serde_json::json!({
        "initialMinutes": config_value(
            &state,
            "cooldown.initialMinutes",
            serde_json::json!(2),
        ),
        "maxMinutes": config_value(
            &state,
            "cooldown.maxMinutes",
            serde_json::json!(300),
        ),
    }))
    .into_response()
}

async fn patch_cooldown(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    let Some(object) = body.as_object() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":"Object body is required"})),
        )
            .into_response();
    };
    let mut entries = serde_json::Map::new();
    for key in ["initialMinutes", "maxMinutes"] {
        if let Some(value) = object.get(key) {
            let Some(number) =
                js_number(value).filter(|number| number.is_finite() && *number >= 0.1)
            else {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": format!("{key} must be at least 0.1")
                    })),
                )
                    .into_response();
            };
            entries.insert(format!("cooldown.{key}"), serde_json::json!(number));
        }
    }
    if let Some(pool) = &state.inner.database {
        if crate::database::upsert_settings_bulk(pool, &entries)
            .await
            .is_err()
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error":"Internal server error"})),
            )
                .into_response();
        }
    }
    state.inner.settings.write().unwrap().extend(entries);
    get_cooldown(State(state), headers).await
}

fn capture_trace_on_error(state: &AppState) -> bool {
    state
        .inner
        .settings
        .read()
        .unwrap()
        .get("debug.captureOnError")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

async fn get_capture_trace_on_error(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    Json(serde_json::json!({ "enabled": capture_trace_on_error(&state) })).into_response()
}

async fn patch_capture_trace_on_error(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    let Some(object) = body.as_object() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "Object body is required" })),
        )
            .into_response();
    };
    let Some(enabled) = object.get("enabled").and_then(serde_json::Value::as_bool) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "enabled must be a boolean" })),
        )
            .into_response();
    };
    if let Some(pool) = &state.inner.database {
        let entries = serde_json::Map::from_iter([(
            "debug.captureOnError".to_string(),
            serde_json::json!(enabled),
        )]);
        if crate::database::upsert_settings_bulk(pool, &entries)
            .await
            .is_err()
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "Internal server error" })),
            )
                .into_response();
        }
    }
    state
        .inner
        .settings
        .write()
        .unwrap()
        .insert("debug.captureOnError".into(), serde_json::json!(enabled));
    Json(serde_json::json!({ "enabled": enabled })).into_response()
}

#[allow(clippy::result_large_err)]
fn require_admin(state: &AppState, headers: &HeaderMap) -> Result<(), Response> {
    let provided_key = headers
        .get("x-admin-key")
        .and_then(|value| value.to_str().ok());
    match resolve_principal(state, provided_key) {
        Some(Principal::Admin) => Ok(()),
        Some(Principal::Limited { .. }) => Err((
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": { "message": "Admin privileges required", "type": "forbidden", "code": 403 }
            })),
        )
            .into_response()),
        None => Err((
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "error": { "message": "Unauthorized", "type": "auth_error", "code": 401 }
            })),
        )
            .into_response()),
    }
}

async fn config_status(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    Json(serde_json::json!({
        "providerCount": state.inner.providers.read().unwrap().len(),
        "modelAliasCount": state.inner.aliases.read().unwrap().len(),
        "keyCount": state.inner.keys.read().unwrap().len(),
        "quotaCount": state.inner.user_quotas.read().unwrap().len(),
        "mcpServerCount": 0,
    }))
    .into_response()
}

fn timeout_seconds(state: &AppState) -> i64 {
    state
        .inner
        .settings
        .read()
        .unwrap()
        .get("timeout.defaultSeconds")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(300)
}

async fn get_timeout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    Json(serde_json::json!({ "defaultSeconds": timeout_seconds(&state) })).into_response()
}

async fn patch_timeout(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    let Some(object) = body.as_object() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "Object body is required" })),
        )
            .into_response();
    };

    if let Some(raw) = object.get("defaultSeconds") {
        let value = match raw {
            serde_json::Value::Number(number) => number.as_f64(),
            serde_json::Value::String(value) => value.trim().parse::<f64>().ok(),
            serde_json::Value::Bool(value) => Some(if *value { 1.0 } else { 0.0 }),
            serde_json::Value::Null => Some(0.0),
            _ => None,
        };
        let Some(value) = value.filter(|value| {
            value.is_finite() && value.fract() == 0.0 && (1.0..=3600.0).contains(value)
        }) else {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "defaultSeconds must be an integer between 1 and 3600"
                })),
            )
                .into_response();
        };
        let value = value as i64;
        if let Some(pool) = &state.inner.database {
            let entries = serde_json::Map::from_iter([(
                "timeout.defaultSeconds".to_string(),
                serde_json::json!(value),
            )]);
            if crate::database::upsert_settings_bulk(pool, &entries)
                .await
                .is_err()
            {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({ "error": "Internal server error" })),
                )
                    .into_response();
            }
        }
        state
            .inner
            .settings
            .write()
            .unwrap()
            .insert("timeout.defaultSeconds".into(), serde_json::json!(value));
    }

    Json(serde_json::json!({ "defaultSeconds": timeout_seconds(&state) })).into_response()
}

fn mcp_enabled(state: &AppState) -> bool {
    state
        .inner
        .settings
        .read()
        .unwrap()
        .get("mcpEnabled")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true)
}

async fn get_mcp_enabled(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    Json(serde_json::json!({ "enabled": mcp_enabled(&state) })).into_response()
}

async fn patch_mcp_enabled(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    let Some(object) = body.as_object() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "Object body is required" })),
        )
            .into_response();
    };
    let Some(enabled) = object.get("enabled").and_then(serde_json::Value::as_bool) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "enabled must be a boolean" })),
        )
            .into_response();
    };

    if let Some(pool) = &state.inner.database {
        let entries =
            serde_json::Map::from_iter([("mcpEnabled".to_string(), serde_json::json!(enabled))]);
        if crate::database::upsert_settings_bulk(pool, &entries)
            .await
            .is_err()
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "Internal server error" })),
            )
                .into_response();
        }
    }
    state
        .inner
        .settings
        .write()
        .unwrap()
        .insert("mcpEnabled".into(), serde_json::json!(enabled));
    Json(serde_json::json!({ "enabled": enabled })).into_response()
}

const EXPLORATION_FIELDS: [&str; 3] = [
    "performanceExplorationRate",
    "latencyExplorationRate",
    "e2ePerformanceExplorationRate",
];

fn js_number(value: &serde_json::Value) -> Option<f64> {
    match value {
        serde_json::Value::Number(number) => number.as_f64(),
        serde_json::Value::String(value) => {
            let value = value.trim();
            if value.is_empty() {
                Some(0.0)
            } else {
                value.parse().ok()
            }
        }
        serde_json::Value::Bool(value) => Some(if *value { 1.0 } else { 0.0 }),
        serde_json::Value::Null => Some(0.0),
        _ => None,
    }
}

fn exploration_value(state: &AppState, key: &str) -> f64 {
    state
        .inner
        .settings
        .read()
        .unwrap()
        .get(key)
        .and_then(serde_json::Value::as_f64)
        .unwrap_or(0.05)
}

fn exploration_body(state: &AppState) -> serde_json::Value {
    serde_json::json!({
        "performanceExplorationRate": exploration_value(state, "performanceExplorationRate"),
        "latencyExplorationRate": exploration_value(state, "latencyExplorationRate"),
        "e2ePerformanceExplorationRate": exploration_value(state, "e2ePerformanceExplorationRate"),
    })
}

async fn get_exploration_rate(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    Json(exploration_body(&state)).into_response()
}

async fn patch_exploration_rate(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    if let Err(response) = require_admin(&state, &headers) {
        return response;
    }
    let Some(object) = body.as_object() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "Object body is required" })),
        )
            .into_response();
    };

    let mut updates = serde_json::Map::new();
    for key in EXPLORATION_FIELDS {
        let Some(raw) = object.get(key) else {
            continue;
        };
        let Some(value) =
            js_number(raw).filter(|value| value.is_finite() && (0.0..=1.0).contains(value))
        else {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": format!("{key} must be a number between 0 and 1")
                })),
            )
                .into_response();
        };
        updates.insert(key.to_string(), serde_json::json!(value));
    }

    if !updates.is_empty() {
        if let Some(pool) = &state.inner.database {
            if crate::database::upsert_settings_bulk(pool, &updates)
                .await
                .is_err()
            {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({ "error": "Internal server error" })),
                )
                    .into_response();
            }
        }
        let mut settings = state.inner.settings.write().unwrap();
        for (key, value) in updates {
            settings.insert(key, value);
        }
    }
    Json(exploration_body(&state)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{KeyConfig, ModelConfig, ProviderConfig, QuotaDefinition};
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use std::collections::HashMap;
    use tower::ServiceExt;

    fn fixture() -> AppState {
        let mut keys = HashMap::new();
        keys.insert(
            "limited".into(),
            KeyConfig {
                secret: "secret".into(),
                ..Default::default()
            },
        );
        let state = AppState::new(Some("admin".into()), keys);
        state
            .inner
            .providers
            .write()
            .unwrap()
            .insert("provider".into(), ProviderConfig::default());
        state
            .inner
            .aliases
            .write()
            .unwrap()
            .insert("alias".into(), ModelConfig::default());
        state.inner.user_quotas.write().unwrap().insert(
            "quota".into(),
            QuotaDefinition {
                quota_type: "daily".into(),
                limit_type: "requests".into(),
                limit: 10.0,
                duration: None,
                allowed_providers: None,
                excluded_providers: None,
                allowed_models: None,
                excluded_models: None,
                shared: None,
                warn_at: None,
            },
        );
        state
    }

    #[tokio::test]
    async fn returns_resource_counts_for_admin() {
        let response = router()
            .with_state(fixture())
            .oneshot(
                Request::get("/v0/management/config/status")
                    .header("x-admin-key", "admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(
            body,
            serde_json::json!({
                "providerCount": 1,
                "modelAliasCount": 1,
                "keyCount": 1,
                "quotaCount": 1,
                "mcpServerCount": 0,
            })
        );
    }

    #[tokio::test]
    async fn config_snapshot_flattens_settings_and_builds_structured_sections() {
        let state = fixture();
        {
            let mut settings = state.inner.settings.write().unwrap();
            settings.insert("custom".into(), serde_json::json!("value"));
            settings.insert("timeout.defaultSeconds".into(), serde_json::json!(45));
            settings.insert("cooldown.initialMinutes".into(), serde_json::json!(3));
        }
        let response = router()
            .with_state(state)
            .oneshot(
                Request::get("/v0/management/config")
                    .header("x-admin-key", "admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(body["custom"], "value");
        assert_eq!(body["timeout"]["defaultSeconds"], 45);
        assert_eq!(body["cooldown"]["initialMinutes"], 3);
        assert!(body.get("system_settings").is_none());
        assert_eq!(body["quotas"], serde_json::json!([]));
        assert_eq!(body["providers"].as_object().unwrap().len(), 1);
        assert_eq!(body["user_quotas"].as_object().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn config_export_returns_raw_repository_collections() {
        let state = fixture();
        state
            .inner
            .settings
            .write()
            .unwrap()
            .insert("custom".into(), serde_json::json!(42));
        let response = router()
            .with_state(state)
            .oneshot(
                Request::get("/v0/management/config/export")
                    .header("x-admin-key", "admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(body["providers"].as_object().unwrap().len(), 1);
        assert_eq!(body["models"].as_object().unwrap().len(), 1);
        assert_eq!(body["keys"].as_object().unwrap().len(), 1);
        assert_eq!(body["user_quotas"].as_object().unwrap().len(), 1);
        assert_eq!(body["settings"]["custom"], 42);
        assert_eq!(body["mcp_servers"], serde_json::json!({}));
        assert_eq!(body["mcp_keys"], serde_json::json!([]));
        assert_eq!(body["oauth_providers"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn rejects_limited_and_missing_credentials() {
        for (credential, expected) in [
            (Some("secret"), StatusCode::FORBIDDEN),
            (None, StatusCode::UNAUTHORIZED),
        ] {
            let mut request = Request::get("/v0/management/config/status");
            if let Some(credential) = credential {
                request = request.header("x-admin-key", credential);
            }
            let response = router()
                .with_state(fixture())
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), expected);
        }
    }

    #[tokio::test]
    async fn timeout_defaults_to_300_and_patch_round_trips() {
        let state = fixture();
        let get = router()
            .with_state(state.clone())
            .oneshot(
                Request::get("/v0/management/config/timeout")
                    .header("x-admin-key", "admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let get_body: serde_json::Value =
            serde_json::from_slice(&get.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(get_body, serde_json::json!({ "defaultSeconds": 300 }));

        let patch = router()
            .with_state(state)
            .oneshot(
                Request::patch("/v0/management/config/timeout")
                    .header("x-admin-key", "admin")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"defaultSeconds":"45"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        let patch_body: serde_json::Value =
            serde_json::from_slice(&patch.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(patch_body, serde_json::json!({ "defaultSeconds": 45 }));
    }

    #[tokio::test]
    async fn timeout_patch_rejects_invalid_values_and_non_object_body() {
        for (body, expected_error) in [
            (
                r#"{"defaultSeconds":0}"#,
                "defaultSeconds must be an integer between 1 and 3600",
            ),
            (
                r#"{"defaultSeconds":3601}"#,
                "defaultSeconds must be an integer between 1 and 3600",
            ),
            (
                r#"{"defaultSeconds":1.5}"#,
                "defaultSeconds must be an integer between 1 and 3600",
            ),
            (r#"[]"#, "Object body is required"),
        ] {
            let response = router()
                .with_state(fixture())
                .oneshot(
                    Request::patch("/v0/management/config/timeout")
                        .header("x-admin-key", "admin")
                        .header("content-type", "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            let response_body: serde_json::Value =
                serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                    .unwrap();
            assert_eq!(response_body["error"], expected_error);
        }
    }

    #[tokio::test]
    async fn mcp_enabled_defaults_true_and_patch_round_trips() {
        let state = fixture();
        let get = router()
            .with_state(state.clone())
            .oneshot(
                Request::get("/v0/management/config/mcp-enabled")
                    .header("x-admin-key", "admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let get_body: serde_json::Value =
            serde_json::from_slice(&get.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(get_body, serde_json::json!({ "enabled": true }));

        let patch = router()
            .with_state(state)
            .oneshot(
                Request::patch("/v0/management/config/mcp-enabled")
                    .header("x-admin-key", "admin")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"enabled":false}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        let patch_body: serde_json::Value =
            serde_json::from_slice(&patch.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(patch_body, serde_json::json!({ "enabled": false }));
    }

    #[tokio::test]
    async fn mcp_enabled_patch_rejects_missing_non_boolean_and_array() {
        for (body, expected_error) in [
            (r#"{}"#, "enabled must be a boolean"),
            (r#"{"enabled":"false"}"#, "enabled must be a boolean"),
            (r#"{"enabled":null}"#, "enabled must be a boolean"),
            (r#"[]"#, "Object body is required"),
        ] {
            let response = router()
                .with_state(fixture())
                .oneshot(
                    Request::patch("/v0/management/config/mcp-enabled")
                        .header("x-admin-key", "admin")
                        .header("content-type", "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            let response_body: serde_json::Value =
                serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                    .unwrap();
            assert_eq!(response_body["error"], expected_error);
        }
    }

    #[tokio::test]
    async fn exploration_rate_defaults_and_coercions_match_javascript_number() {
        let state = fixture();
        let get = router()
            .with_state(state.clone())
            .oneshot(
                Request::get("/v0/management/config/exploration-rate")
                    .header("x-admin-key", "admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let get_body: serde_json::Value =
            serde_json::from_slice(&get.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(
            get_body,
            serde_json::json!({
                "performanceExplorationRate": 0.05,
                "latencyExplorationRate": 0.05,
                "e2ePerformanceExplorationRate": 0.05,
            })
        );

        let patch = router()
            .with_state(state)
            .oneshot(
                Request::patch("/v0/management/config/exploration-rate")
                    .header("x-admin-key", "admin")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"performanceExplorationRate":"0.2","latencyExplorationRate":true,"e2ePerformanceExplorationRate":null}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let patch_body: serde_json::Value =
            serde_json::from_slice(&patch.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(
            patch_body,
            serde_json::json!({
                "performanceExplorationRate": 0.2,
                "latencyExplorationRate": 1.0,
                "e2ePerformanceExplorationRate": 0.0,
            })
        );
    }

    #[tokio::test]
    async fn exploration_rate_rejects_out_of_range_and_non_object() {
        for (body, expected_error) in [
            (
                r#"{"performanceExplorationRate":1.1}"#,
                "performanceExplorationRate must be a number between 0 and 1",
            ),
            (
                r#"{"latencyExplorationRate":-0.1}"#,
                "latencyExplorationRate must be a number between 0 and 1",
            ),
            (r#"[]"#, "Object body is required"),
        ] {
            let response = router()
                .with_state(fixture())
                .oneshot(
                    Request::patch("/v0/management/config/exploration-rate")
                        .header("x-admin-key", "admin")
                        .header("content-type", "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            let response_body: serde_json::Value =
                serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                    .unwrap();
            assert_eq!(response_body["error"], expected_error);
        }
    }

    #[tokio::test]
    async fn failover_defaults_and_patch_round_trip_recognized_fields() {
        let state = fixture();
        let get = router()
            .with_state(state.clone())
            .oneshot(
                Request::get("/v0/management/config/failover")
                    .header("x-admin-key", "admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let get_body: serde_json::Value =
            serde_json::from_slice(&get.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(get_body["enabled"], true);
        assert_eq!(
            get_body["retryableErrors"],
            serde_json::json!(["ECONNREFUSED", "ETIMEDOUT", "ENOTFOUND"])
        );
        assert_eq!(
            get_body["retryableStatusCodes"].as_array().unwrap().len(),
            398
        );
        assert!(!get_body["retryableStatusCodes"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!(413)));

        let patch = router()
            .with_state(state)
            .oneshot(
                Request::patch("/v0/management/config/failover")
                    .header("x-admin-key", "admin")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"enabled":false,"retryableStatusCodes":[503],"ignored":"value"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&patch.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(body["enabled"], false);
        assert_eq!(body["retryableStatusCodes"], serde_json::json!([503]));
        assert!(body.get("ignored").is_none());
    }

    #[tokio::test]
    async fn cooldown_defaults_coercion_and_validation_match_plexus() {
        let state = fixture();
        let get = router()
            .with_state(state.clone())
            .oneshot(
                Request::get("/v0/management/config/cooldown")
                    .header("x-admin-key", "admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&get.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(
            body,
            serde_json::json!({ "initialMinutes": 2, "maxMinutes": 300 })
        );

        let patch = router()
            .with_state(state.clone())
            .oneshot(
                Request::patch("/v0/management/config/cooldown")
                    .header("x-admin-key", "admin")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"initialMinutes":"0.5","maxMinutes":true}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&patch.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(
            body,
            serde_json::json!({ "initialMinutes": 0.5, "maxMinutes": 1.0 })
        );

        let invalid = router()
            .with_state(state)
            .oneshot(
                Request::patch("/v0/management/config/cooldown")
                    .header("x-admin-key", "admin")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"initialMinutes":null}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
        let body: serde_json::Value =
            serde_json::from_slice(&invalid.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(body["error"], "initialMinutes must be at least 0.1");
    }

    #[tokio::test]
    async fn capture_trace_defaults_false_and_patch_round_trips() {
        let state = fixture();
        let get = router()
            .with_state(state.clone())
            .oneshot(
                Request::get("/v0/management/config/capture-trace-on-error")
                    .header("x-admin-key", "admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&get.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(body, serde_json::json!({ "enabled": false }));

        let patch = router()
            .with_state(state)
            .oneshot(
                Request::patch("/v0/management/config/capture-trace-on-error")
                    .header("x-admin-key", "admin")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"enabled":true}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&patch.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(body, serde_json::json!({ "enabled": true }));
    }

    #[tokio::test]
    async fn capture_trace_rejects_missing_non_boolean_and_array() {
        for (body, expected_error) in [
            (r#"{}"#, "enabled must be a boolean"),
            (r#"{"enabled":"true"}"#, "enabled must be a boolean"),
            (r#"{"enabled":null}"#, "enabled must be a boolean"),
            (r#"[]"#, "Object body is required"),
        ] {
            let response = router()
                .with_state(fixture())
                .oneshot(
                    Request::patch("/v0/management/config/capture-trace-on-error")
                        .header("x-admin-key", "admin")
                        .header("content-type", "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            let response_body: serde_json::Value =
                serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                    .unwrap();
            assert_eq!(response_body["error"], expected_error);
        }
    }

    #[tokio::test]
    async fn stall_defaults_and_patch_all_fields() {
        let state = fixture();
        let get = router()
            .with_state(state.clone())
            .oneshot(
                Request::get("/v0/management/config/stall")
                    .header("x-admin-key", "admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&get.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(
            body,
            serde_json::json!({
                "ttfbSeconds": null,
                "ttfbBytes": 100,
                "minBytesPerSecond": null,
                "windowSeconds": 10,
                "gracePeriodSeconds": 30,
                "stallCooldown": false,
            })
        );

        let patch = router()
            .with_state(state)
            .oneshot(
                Request::patch("/v0/management/config/stall")
                    .header("x-admin-key", "admin")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"ttfbSeconds":"12.5","ttfbBytes":"250","minBytesPerSecond":500,"windowSeconds":5,"gracePeriodSeconds":0,"stallCooldown":true}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&patch.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(
            body,
            serde_json::json!({
                "ttfbSeconds": 12.5,
                "ttfbBytes": 250,
                "minBytesPerSecond": 500,
                "windowSeconds": 5,
                "gracePeriodSeconds": 0,
                "stallCooldown": true,
            })
        );
    }

    #[tokio::test]
    async fn stall_patch_validates_each_field() {
        for (body, expected_error) in [
            (
                r#"{"ttfbSeconds":4}"#,
                "ttfbSeconds must be null or a number between 5 and 120",
            ),
            (
                r#"{"ttfbBytes":50.5}"#,
                "ttfbBytes must be an integer between 50 and 10000",
            ),
            (
                r#"{"minBytesPerSecond":49}"#,
                "minBytesPerSecond must be null or an integer between 50 and 5000",
            ),
            (
                r#"{"windowSeconds":31}"#,
                "windowSeconds must be an integer between 3 and 30",
            ),
            (
                r#"{"gracePeriodSeconds":121}"#,
                "gracePeriodSeconds must be an integer between 0 and 120",
            ),
            (
                r#"{"stallCooldown":"true"}"#,
                "stallCooldown must be a boolean",
            ),
            (r#"[]"#, "Object body is required"),
        ] {
            let response = router()
                .with_state(fixture())
                .oneshot(
                    Request::patch("/v0/management/config/stall")
                        .header("x-admin-key", "admin")
                        .header("content-type", "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            let response_body: serde_json::Value =
                serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                    .unwrap();
            assert_eq!(response_body["error"], expected_error);
        }
    }

    #[tokio::test]
    async fn vision_fallthrough_patch_shallow_merges() {
        let state = fixture();
        state.inner.settings.write().unwrap().insert(
            "vision_fallthrough".into(),
            serde_json::json!({ "enabled": true, "model": "old" }),
        );
        let response = router()
            .with_state(state)
            .oneshot(
                Request::patch("/v0/management/config/vision-fallthrough")
                    .header("x-admin-key", "admin")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"model":"new","extra":{"x":1}}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(
            body,
            serde_json::json!({
                "enabled": true, "model": "new", "extra": { "x": 1 }
            })
        );
    }

    #[tokio::test]
    async fn vision_fallthrough_rejects_non_object_body() {
        let response = router()
            .with_state(fixture())
            .oneshot(
                Request::patch("/v0/management/config/vision-fallthrough")
                    .header("x-admin-key", "admin")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"[]"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body: serde_json::Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(
            body,
            serde_json::json!({ "error": "Object body is required" })
        );
    }

    #[tokio::test]
    async fn background_exploration_defaults_and_patch_round_trip() {
        let state = fixture();
        let get = router()
            .with_state(state.clone())
            .oneshot(
                Request::get("/v0/management/config/background-exploration")
                    .header("x-admin-key", "admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&get.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(
            body,
            serde_json::json!({
                "enabled": false, "stalenessThresholdSeconds": 600, "workerConcurrency": 2
            })
        );

        let patch = router()
            .with_state(state)
            .oneshot(
                Request::patch("/v0/management/config/background-exploration")
                    .header("x-admin-key", "admin")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"enabled":true,"stalenessThresholdSeconds":"120","workerConcurrency":4}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&patch.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(
            body,
            serde_json::json!({
                "enabled": true, "stalenessThresholdSeconds": 120, "workerConcurrency": 4
            })
        );
    }

    #[tokio::test]
    async fn background_exploration_validates_each_field() {
        for (body, expected_error) in [
            (r#"{"enabled":"false"}"#, "enabled must be a boolean"),
            (
                r#"{"stalenessThresholdSeconds":0}"#,
                "stalenessThresholdSeconds must be an integer >= 1",
            ),
            (
                r#"{"workerConcurrency":17}"#,
                "workerConcurrency must be an integer between 1 and 16",
            ),
            (r#"[]"#, "Object body is required"),
        ] {
            let response = router()
                .with_state(fixture())
                .oneshot(
                    Request::patch("/v0/management/config/background-exploration")
                        .header("x-admin-key", "admin")
                        .header("content-type", "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            let response_body: serde_json::Value =
                serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                    .unwrap();
            assert_eq!(response_body["error"], expected_error);
        }
    }

    #[test]
    fn trusted_proxy_rule_validation_matches_reference_cases() {
        for valid in [
            "192.168.1.10",
            "10.0.0.0/8",
            "10.1.0.10-20",
            "10.1.0.10-10.1.0.20",
            "::1",
            "2001:db8::/32",
            "2001:db8::1-2001:db8::5",
        ] {
            assert!(valid_ip_rule(valid), "expected valid: {valid}");
        }
        for invalid in [
            "999.1.1.1",
            "1.2.3",
            "10.0.0.0/33",
            "::/129",
            "1.2.3.4-300",
            "10.1.0.20-10",
            "10.1.0.20-10.1.0.10",
            "10.0.0.1-2001:db8::1",
            "203.0.113.10:443",
            "[2001:db8::1]:8443",
            "fe80::1%eth0",
            "not-an-ip",
        ] {
            assert!(!valid_ip_rule(invalid), "expected invalid: {invalid}");
        }
    }

    #[tokio::test]
    async fn trusted_proxies_defaults_normalizes_and_persists() {
        let state = fixture();
        let get = router()
            .with_state(state.clone())
            .oneshot(
                Request::get("/v0/management/config/trusted-proxies")
                    .header("x-admin-key", "admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&get.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(
            body,
            serde_json::json!({ "trustedProxies": ["0.0.0.0/0", "::/0"] })
        );

        let patch = router()
            .with_state(state)
            .oneshot(
                Request::patch("/v0/management/config/trusted-proxies")
                    .header("x-admin-key", "admin")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"trustedProxies":[" 127.0.0.1 ","","2001:db8::/32"]}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&patch.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(
            body,
            serde_json::json!({ "trustedProxies": ["127.0.0.1", "2001:db8::/32"] })
        );
    }

    #[tokio::test]
    async fn trusted_proxies_rejects_invalid_rules_and_shapes() {
        for (body, expected_error) in [
            (
                r#"{"trustedProxies":["bad"]}"#,
                "Invalid IP rule: bad. Use IPv4/IPv6, CIDR (a.b.c.d/n, ::/n), or a range (a.b.c.d-N or addr-addr).",
            ),
            (
                r#"{"trustedProxies":"x"}"#,
                "trustedProxies must be an array of strings",
            ),
            (r#"[]"#, "Object body is required"),
        ] {
            let response = router()
                .with_state(fixture())
                .oneshot(
                    Request::patch("/v0/management/config/trusted-proxies")
                        .header("x-admin-key", "admin")
                        .header("content-type", "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            let response_body: serde_json::Value = serde_json::from_slice(
                &response.into_body().collect().await.unwrap().to_bytes(),
            )
            .unwrap();
            assert_eq!(response_body["error"], expected_error);
        }
    }

    #[tokio::test]
    async fn vision_descriptor_cache_clear_is_admin_only_and_idempotent() {
        let response = router()
            .with_state(fixture())
            .oneshot(
                Request::post("/v0/management/cache/vision-descriptor/clear")
                    .header("x-admin-key", "admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(
            body,
            serde_json::json!({
                "success": true, "message": "Vision descriptor cache cleared."
            })
        );

        let limited = router()
            .with_state(fixture())
            .oneshot(
                Request::post("/v0/management/cache/vision-descriptor/clear")
                    .header("x-admin-key", "secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(limited.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn quota_checker_types_lists_pinned_registry() {
        let response = router()
            .with_state(fixture())
            .oneshot(
                Request::get("/v0/management/quota-checker-types")
                    .header("x-admin-key", "admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(body["count"], 30);
        assert_eq!(
            body["types"][0],
            serde_json::json!({ "type": "naga", "displayName": "Naga" })
        );
        assert_eq!(
            body["types"][29],
            serde_json::json!({ "type": "cline", "displayName": "Cline" })
        );
    }

    #[tokio::test]
    async fn pi_providers_lists_pinned_registry() {
        let response = router()
            .with_state(fixture())
            .oneshot(
                Request::get("/v0/management/pi/providers")
                    .header("x-admin-key", "admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        let providers = body["data"].as_array().unwrap();
        assert_eq!(providers.len(), 39);
        assert_eq!(providers.first().unwrap(), "amazon-bedrock");
        assert_eq!(providers.last().unwrap(), "zai-coding-cn");
        assert!(providers.contains(&serde_json::json!("openai")));
    }

    #[tokio::test]
    async fn compaction_nested_merge_and_null_clear() {
        let state = fixture();
        state.inner.settings.write().unwrap().insert(
            "compaction".into(),
            serde_json::json!({
                "triggerRatio": 0.7, "minTokens": 2000,
                "headroom": { "baseUrl": "http://localhost:8787", "timeoutMs": 5000 }
            }),
        );
        let response = router()
            .with_state(state)
            .oneshot(
                Request::patch("/v0/management/config/compaction")
                    .header("x-admin-key", "admin")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"triggerRatio":null,"headroom":{"timeoutMs":null}}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(
            body,
            serde_json::json!({
                "minTokens": 2000, "headroom": { "baseUrl": "http://localhost:8787" }
            })
        );
    }

    #[tokio::test]
    async fn compaction_rejects_invalid_values() {
        for body in [
            r#"{"triggerRatio":2}"#,
            r#"{"native":{"maxArrayItems":0}}"#,
            r#"{"headroom":{"baseUrl":"bad"}}"#,
        ] {
            let response = router()
                .with_state(fixture())
                .oneshot(
                    Request::patch("/v0/management/config/compaction")
                        .header("x-admin-key", "admin")
                        .header("content-type", "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        }
    }
}
