//! Resolve pi-ai built-in providers from OAuth IDs or API endpoint URLs.

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use serde::Deserialize;

use crate::{auth::{resolve_principal, Principal}, state::AppState};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResolveInput {
    urls: Option<Vec<String>>,
    oauth_provider: Option<String>,
}

#[derive(Clone, Copy)]
struct Endpoint { id: &'static str, base_url: &'static str }

// Snapshot of pi-ai's builtin provider IDs and base URLs. Providers without a
// fixed base URL are intentionally absent from URL matching but retained as IDs.
const BUILTIN_IDS: &[&str] = &[
    "amazon-bedrock", "anthropic", "azure", "cerebras", "cloudflare", "cohere",
    "deepinfra", "deepseek", "github-copilot", "google", "google-gemini-cli",
    "groq", "huggingface", "kimi-coding", "meta", "mistral", "moonshotai",
    "openai", "openai-codex", "opencode", "openrouter", "zai", "xai",
];

const ENDPOINTS: &[Endpoint] = &[
    Endpoint { id: "anthropic", base_url: "https://api.anthropic.com/v1" },
    Endpoint { id: "cerebras", base_url: "https://api.cerebras.ai/v1" },
    Endpoint { id: "deepinfra", base_url: "https://api.deepinfra.com/v1/openai" },
    Endpoint { id: "deepseek", base_url: "https://api.deepseek.com" },
    Endpoint { id: "google", base_url: "https://generativelanguage.googleapis.com/v1beta" },
    Endpoint { id: "groq", base_url: "https://api.groq.com/openai/v1" },
    Endpoint { id: "mistral", base_url: "https://api.mistral.ai/v1" },
    Endpoint { id: "moonshotai", base_url: "https://api.moonshot.ai/v1" },
    Endpoint { id: "openai", base_url: "https://api.openai.com/v1" },
    Endpoint { id: "openai-codex", base_url: "https://chatgpt.com/backend-api/codex" },
    Endpoint { id: "openrouter", base_url: "https://openrouter.ai/api/v1" },
    Endpoint { id: "xai", base_url: "https://api.x.ai/v1" },
];

pub fn router() -> Router<AppState> {
    Router::new().route("/v0/management/pi/resolve-provider", post(resolve))
}

async fn resolve(State(state): State<AppState>, headers: HeaderMap, body: Option<Json<ResolveInput>>) -> Response {
    match resolve_principal(&state, headers.get("x-admin-key").and_then(|v| v.to_str().ok())) {
        Some(Principal::Admin) => {},
        Some(Principal::Limited { .. }) => return auth_error(StatusCode::FORBIDDEN, "Admin privileges required"),
        None => return auth_error(StatusCode::UNAUTHORIZED, "Unauthorized"),
    }
    let input = body.map(|Json(v)| v).unwrap_or(ResolveInput { urls: None, oauth_provider: None });
    let provider = resolve_provider(&input);
    Json(serde_json::json!({"data":{"provider":provider}})).into_response()
}

fn auth_error(status: StatusCode, message: &str) -> Response {
    let (kind, code) = if status == StatusCode::FORBIDDEN { ("forbidden", 403) } else { ("auth_error", 401) };
    (status, Json(serde_json::json!({"error":{"message":message,"type":kind,"code":code}}))).into_response()
}

fn resolve_provider(input: &ResolveInput) -> Option<&'static str> {
    if let Some(id) = input.oauth_provider.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        if let Some(known) = BUILTIN_IDS.iter().find(|&&known| known == id) { return Some(known); }
    }
    match_urls(input.urls.as_deref().unwrap_or(&[]))
}

fn parse_url(raw: &str) -> Option<(String, String)> {
    let s = raw.trim();
    let (scheme, rest) = s.split_once("://")?;
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") { return None; }
    let authority_end = rest.find('/').unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    if authority.is_empty() || authority.contains('@') { return None; }
    let path = if authority_end == rest.len() { "/" } else { &rest[authority_end..] };
    let path = path.split(['?', '#']).next().unwrap_or("/").trim_end_matches('/');
    Some((authority.to_ascii_lowercase(), if path.is_empty() { "/".into() } else { path.to_ascii_lowercase() }))
}

fn path_prefix(prefix: &str, full: &str) -> bool {
    prefix == "/" || full == prefix || full.strip_prefix(prefix).is_some_and(|tail| tail.starts_with('/'))
}

fn match_urls(urls: &[String]) -> Option<&'static str> {
    let candidates: Vec<_> = urls.iter().filter_map(|u| parse_url(u)).collect();
    for (origin, path) in &candidates {
        for ep in ENDPOINTS {
            if let Some((known_origin, known_path)) = parse_url(ep.base_url) {
                if origin == &known_origin && path == &known_path { return Some(ep.id); }
            }
        }
    }
    let mut best: Option<(&str, usize)> = None;
    for (origin, path) in &candidates {
        for ep in ENDPOINTS {
            if let Some((known_origin, known_path)) = parse_url(ep.base_url) {
                if origin == &known_origin && (path_prefix(&known_path, path) || path_prefix(path, &known_path))
                    && best.is_none_or(|(_, n)| known_path.len() > n) { best = Some((ep.id, known_path.len())); }
            }
        }
    }
    best.map(|(id, _)| id)
}
