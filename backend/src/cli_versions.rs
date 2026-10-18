//! Claude Code / Codex CLI version services, mirroring pinned Plexus
//! `claude-code-version-service.ts` and `codex-version-service.ts`.
//!
//! Masked OAuth requests fingerprint as the current CLI release; Anthropic
//! gates new models on this version. Each value starts at a compiled fallback,
//! is fetched once at startup and refreshed hourly, and keeps the last good
//! value when a fetch fails.
use std::sync::RwLock;
use std::time::Duration;

const NPM_DIST_TAGS_URL: &str = "https://registry.npmjs.org/-/package/@anthropic-ai%2Fclaude-code/dist-tags";
const GITHUB_RELEASES_URL: &str = "https://api.github.com/repos/openai/codex/releases/latest";
pub const AUTO_REFRESH_MINUTES: u64 = 60;

static CLAUDE_CODE: RwLock<Option<String>> = RwLock::new(None);
static CODEX: RwLock<Option<String>> = RwLock::new(None);

pub fn claude_code_version() -> String {
    CLAUDE_CODE.read().unwrap().clone().unwrap_or_else(|| crate::anthropic_masking::CLAUDE_CODE_VERSION.into())
}
pub fn codex_version() -> String {
    CODEX.read().unwrap().clone().unwrap_or_else(|| crate::codex_masking::CODEX_VERSION.into())
}

fn semver(value: &str) -> Option<String> {
    let found = value.split(|c: char| !(c.is_ascii_digit() || c == '.')).find(|part| {
        let pieces: Vec<_> = part.split('.').collect();
        pieces.len() == 3 && pieces.iter().all(|p| !p.is_empty())
    })?;
    Some(found.to_owned())
}

async fn fetch_json(url: &str, accept: &str) -> Result<serde_json::Value, String> {
    let response = reqwest::Client::new().get(url).header("accept", accept).header("user-agent", "plexus-gateway")
        .timeout(Duration::from_secs(10)).send().await.map_err(|error| error.to_string())?;
    if !response.status().is_success() { return Err(format!("returned status {}", response.status().as_u16())); }
    response.json().await.map_err(|error| error.to_string())
}

async fn fetch_claude_code_at(url: &str) -> Result<String, String> {
    match fetch_json(url, "application/json").await {
        Ok(body) => body["latest"].as_str().filter(|v| semver(v).as_deref() == Some(*v)).map(str::to_owned)
            .ok_or_else(|| "npm dist-tags response missing a valid latest tag".into()),
        Err(error) => Err(format!("Failed to fetch claude-code version from npm: {error}. Using fallback: {}", claude_code_version())),
    }
}
async fn fetch_codex_at(url: &str) -> Result<String, String> {
    match fetch_json(url, "application/vnd.github+json").await {
        Ok(body) => match body["tag_name"].as_str() {
            Some(tag) => semver(tag).ok_or_else(|| format!("Unexpected tag format: {tag}, ignoring")),
            None => Err("GitHub release response missing tag_name".into()),
        },
        Err(error) => Err(format!("Failed to fetch codex version from GitHub: {error}. Using fallback: {}", codex_version())),
    }
}
/// Returns an error message when the fetch failed; the old version is kept.
pub async fn refresh_claude_code() -> Option<String> {
    fetch_claude_code_at(NPM_DIST_TAGS_URL).await.map(|v| *CLAUDE_CODE.write().unwrap() = Some(v)).err()
}
pub async fn refresh_codex() -> Option<String> {
    fetch_codex_at(GITHUB_RELEASES_URL).await.map(|v| *CODEX.write().unwrap() = Some(v)).err()
}

/// Startup wiring (Plexus index.ts order): schedule codex, fetch codex,
/// schedule claude-code, fetch claude-code.
pub fn start(state: crate::AppState) {
    for (name, codex) in [("codex", true), ("claude-code", false)] {
        let module = if codex { "codex-version-service" } else { "claude-code-version-service" };
        crate::system_logs::event(&state, "info", format!("Scheduled {name} version auto-refresh every {AUTO_REFRESH_MINUTES} minutes"), [("module".into(), serde_json::json!(module)), ("functionName".into(), serde_json::json!("startAutoRefresh"))]);
        let state = state.clone();
        tokio::spawn(async move {
            let fetch = || async move { if codex { refresh_codex().await } else { refresh_claude_code().await } };
            // Plexus logs bad registry responses at debug and transport failures at warn.
            if let Some(error) = fetch().await { crate::system_logs::event(&state, if error.starts_with("Failed to fetch") { "warn" } else { "debug" }, error, [("module".into(), serde_json::json!(module))]); }
            loop {
                tokio::time::sleep(Duration::from_secs(AUTO_REFRESH_MINUTES * 60)).await;
                if let Some(error) = fetch().await { crate::system_logs::event(&state, "error", format!("Scheduled {name} version refresh failed"), [("module".into(), serde_json::json!(module)), ("error".into(), serde_json::json!(error))]); }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::get, Json, Router};
    #[tokio::test]
    async fn versions_refresh_from_registry_and_keep_last_good_on_failure() {
        assert_eq!(semver("rust-v0.128.0").as_deref(), Some("0.128.0"));
        let app = Router::new()
            .route("/npm", get(|| async { Json(serde_json::json!({"latest":"9.9.9"})) }))
            .route("/bad", get(|| async { Json(serde_json::json!({"latest":"next"})) }))
            .route("/gh", get(|| async { Json(serde_json::json!({"tag_name":"rust-v0.200.1"})) }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        assert!(fetch_claude_code_at(&format!("http://{addr}/bad")).await.is_err());
        assert_eq!(fetch_claude_code_at(&format!("http://{addr}/npm")).await.unwrap(), "9.9.9");
        assert!(fetch_claude_code_at(&format!("http://{addr}/missing")).await.unwrap_err().contains("Using fallback"));
        assert_eq!(fetch_codex_at(&format!("http://{addr}/gh")).await.unwrap(), "0.200.1");
        server.abort();
    }
}
