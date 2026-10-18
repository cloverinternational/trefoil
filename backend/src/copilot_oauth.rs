//! GitHub Copilot login — port of pinned pi-ai `auth/oauth/github-copilot.js`.
//!
//! Flow: text prompt for an optional GitHub Enterprise domain, RFC 8628
//! device grant on that domain, Copilot token exchange, model policy probe
//! (enabling `unconfigured` catalog models), credentials. The GitHub token is
//! the refresh credential; an enterprise domain is persisted with it as
//! `<domain>\n<github_token>` (see `OAuthCredentials::stored_refresh`).
use crate::oauth_login::{poll_device_code_flow, status_text, AuthEvent, AuthPrompt, DevicePoll, Interaction, OAuthCredentials};
use serde_json::Value;
use std::time::{Duration, Instant};

const CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";
const USER_AGENT: &str = "GitHubCopilotChat/0.35.0";
const COPILOT_API_VERSION: &str = "2026-06-01";
const COPILOT_HEADERS: [(&str, &str); 4] = [
    ("User-Agent", "GitHubCopilotChat/0.35.0"),
    ("Editor-Version", "vscode/1.107.0"),
    ("Editor-Plugin-Version", "copilot-chat/0.35.0"),
    ("Copilot-Integration-Id", "vscode-chat"),
];

/// Copilot API base for inference. Unlike pi-ai, only `*.githubcopilot.com`
/// proxy hosts are honoured (the token is sent there).
pub fn api_base(token: &str) -> String {
    let host = token.split(';').find_map(|part| part.strip_prefix("proxy-ep="));
    if let Some(host) = host.filter(|host| host.starts_with("proxy.") && host.ends_with(".githubcopilot.com") && host.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')) {
        return format!("https://api.{}", &host[6..]);
    }
    "https://api.individual.githubcopilot.com".into()
}

/// pi-ai `getGitHubCopilotBaseUrl` (login-time model discovery).
fn base_url_from_token(token: &str, enterprise: Option<&str>) -> String {
    if let Some(start) = token.find("proxy-ep=") {
        let host: String = token[start + 9..].chars().take_while(|c| *c != ';').collect();
        if !host.is_empty() {
            let api = host.strip_prefix("proxy.").map(|rest| format!("api.{rest}")).unwrap_or(host);
            return format!("https://{api}");
        }
    }
    if let Some(domain) = enterprise {
        return format!("https://copilot-api.{domain}");
    }
    "https://api.individual.githubcopilot.com".into()
}

/// Test overrides for the domain-derived URLs.
#[derive(Debug, Clone, Default)]
pub struct Endpoints {
    /// Replaces `https://{domain}` (device + access-token endpoints).
    pub github_base: Option<String>,
    /// Replaces `https://api.{domain}` (Copilot token endpoint).
    pub api_base: Option<String>,
    /// Replaces the Copilot API base used for `/models` and policies.
    pub copilot_base: Option<String>,
}

impl Endpoints {
    fn github(&self, domain: &str) -> String {
        self.github_base.clone().unwrap_or_else(|| format!("https://{domain}"))
    }
    fn api(&self, domain: &str) -> String {
        self.api_base.clone().unwrap_or_else(|| format!("https://api.{domain}"))
    }
}

/// pi-ai `normalizeDomain`.
fn normalize_domain(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    let candidate = if trimmed.contains("://") { trimmed.to_string() } else { format!("https://{trimmed}") };
    url::Url::parse(&candidate).ok().and_then(|u| u.host_str().map(str::to_owned)).filter(|h| !h.is_empty())
}

async fn fetch_json(request: reqwest::RequestBuilder) -> Result<Value, String> {
    let response = request.send().await.map_err(|e| e.to_string())?;
    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        return Err(format!("{} {}: {text}", status.as_u16(), status_text(status)));
    }
    response.json::<Value>().await.map_err(|e| e.to_string())
}

struct Device {
    device_code: String,
    user_code: String,
    verification_uri: String,
    interval: Option<f64>,
    expires_in: f64,
}

async fn start_device_flow(client: &reqwest::Client, base: &str) -> Result<Device, String> {
    let data = fetch_json(
        client
            .post(format!("{base}/login/device/code"))
            .header("Accept", "application/json")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("User-Agent", USER_AGENT)
            .body(crate::oauth_login::search_params(&[("client_id", CLIENT_ID), ("scope", "read:user")])),
    )
    .await?;
    if !data.is_object() {
        return Err("Invalid device code response".into());
    }
    let (Some(device_code), Some(user_code), Some(verification_uri)) = (data["device_code"].as_str(), data["user_code"].as_str(), data["verification_uri"].as_str()) else {
        return Err("Invalid device code response fields".into());
    };
    let interval = &data["interval"];
    if !(interval.is_null() && data.get("interval").is_none() || interval.is_number()) || !data["expires_in"].is_number() {
        return Err("Invalid device code response fields".into());
    }
    let parsed = url::Url::parse(verification_uri).map_err(|_| "Untrusted verification_uri in device code response".to_string())?;
    if !matches!(parsed.scheme(), "https" | "http") {
        return Err("Untrusted verification_uri in device code response".into());
    }
    Ok(Device {
        device_code: device_code.into(),
        user_code: user_code.into(),
        verification_uri: parsed.to_string(),
        interval: interval.as_f64(),
        expires_in: data["expires_in"].as_f64().unwrap_or(0.0),
    })
}

async fn poll_for_github_access_token(client: &reqwest::Client, base: &str, device: &Device) -> Result<String, String> {
    poll_device_code_flow(device.interval, Some(device.expires_in), true, || async {
        let raw = fetch_json(
            client
                .post(format!("{base}/login/oauth/access_token"))
                .header("Accept", "application/json")
                .header("Content-Type", "application/x-www-form-urlencoded")
                .header("User-Agent", USER_AGENT)
                .body(crate::oauth_login::search_params(&[
                    ("client_id", CLIENT_ID),
                    ("device_code", &device.device_code),
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ])),
        )
        .await?;
        if let Some(token) = raw["access_token"].as_str() {
            return Ok(DevicePoll::Complete(token.to_string()));
        }
        if let Some(error) = raw["error"].as_str() {
            return Ok(match error {
                "authorization_pending" => DevicePoll::Pending,
                "slow_down" => DevicePoll::SlowDown(raw["interval"].as_f64()),
                _ => {
                    let description = raw["error_description"].as_str().filter(|d| !d.is_empty()).map(|d| format!(": {d}")).unwrap_or_default();
                    DevicePoll::Failed(format!("Device flow failed: {error}{description}"))
                }
            });
        }
        Ok(DevicePoll::Failed("Invalid device token response".into()))
    })
    .await
}

/// pi-ai `refreshGitHubCopilotAccessToken`.
async fn copilot_token(client: &reqwest::Client, api: &str, github_token: &str, enterprise: Option<String>) -> Result<OAuthCredentials, String> {
    let mut request = client.get(format!("{api}/copilot_internal/v2/token")).header("Accept", "application/json").header("Authorization", format!("Bearer {github_token}"));
    for (k, v) in COPILOT_HEADERS {
        request = request.header(k, v);
    }
    let raw = fetch_json(request).await?;
    if !raw.is_object() {
        return Err("Invalid Copilot token response".into());
    }
    let (Some(token), Some(expires_at)) = (raw["token"].as_str(), raw["expires_at"].as_f64()) else {
        return Err("Invalid Copilot token response fields".into());
    };
    let mut credentials = OAuthCredentials::new(token, github_token, (expires_at * 1000.0) as i64 - 5 * 60 * 1000);
    credentials.enterprise_url = enterprise;
    Ok(credentials)
}

struct RetryPolicy {
    max_retries: u32,
    max_elapsed: Duration,
}

/// pi-ai `fetchWithRateLimitRetry` (5 s per attempt, 429 backoff budget).
async fn fetch_with_rate_limit_retry(build: impl Fn() -> reqwest::RequestBuilder, policy: &RetryPolicy) -> Result<reqwest::Response, String> {
    let budget = (policy.max_retries > 0 && !policy.max_elapsed.is_zero()).then(|| Instant::now() + policy.max_elapsed);
    let mut retry = 0;
    loop {
        let per_attempt = match budget {
            Some(deadline) => deadline.saturating_duration_since(Instant::now()).min(Duration::from_secs(5)),
            None => Duration::from_secs(5),
        };
        let response = build().timeout(per_attempt).send().await.map_err(|e| e.to_string())?;
        if response.status().as_u16() != 429 || retry == policy.max_retries {
            return Ok(response);
        }
        let mut delay_ms = 500.0 * 2f64.powi(retry as i32);
        if let Some(retry_after) = response.headers().get("retry-after").and_then(|v| v.to_str().ok()) {
            delay_ms = match retry_after.trim().parse::<f64>() {
                Ok(seconds) => seconds * 1000.0,
                Err(_) => match chrono::DateTime::parse_from_rfc2822(retry_after.trim()) {
                    Ok(date) => (date.timestamp_millis() - crate::oauth_login::now_ms()) as f64,
                    Err(_) => return Ok(response),
                },
            };
            if !delay_ms.is_finite() {
                return Ok(response);
            }
        }
        let delay_ms = delay_ms.max(0.0);
        if let Some(deadline) = budget {
            if delay_ms >= deadline.saturating_duration_since(Instant::now()).as_millis() as f64 {
                return Ok(response);
            }
        }
        tokio::time::sleep(Duration::from_millis(delay_ms as u64)).await;
        retry += 1;
    }
}

fn catalog_model_ids() -> Vec<String> {
    let catalog: Value = serde_json::from_str(include_str!("assets/pi-catalog.json")).unwrap_or(Value::Null);
    catalog["providers"]
        .as_array()
        .and_then(|providers| providers.iter().find(|p| p["id"] == "github-copilot"))
        .and_then(|p| p["models"].as_array())
        .map(|models| models.iter().filter_map(|m| m["id"].as_str().map(str::to_owned)).collect())
        .unwrap_or_default()
}

/// pi-ai `parseGitHubCopilotModelCatalog` → (availableModelIds, policyModelIds).
fn parse_model_catalog(raw: &Value, allow_policy_fallback: bool) -> Result<(Vec<String>, Vec<String>), String> {
    let data = raw["data"].as_array().ok_or("Invalid Copilot models response")?;
    let known = catalog_model_ids();
    let models: Vec<(String, bool, Option<String>)> = data
        .iter()
        .filter_map(|item| {
            let id = item.get("id")?.as_str()?;
            if item["capabilities"]["supports"]["tool_calls"] == Value::Bool(false) {
                return None;
            }
            Some((id.to_string(), item["model_picker_enabled"] == Value::Bool(true), item["policy"]["state"].as_str().map(str::to_owned)))
        })
        .collect();
    let picker: Vec<String> = models.iter().filter(|(_, picker, state)| *picker && state.as_deref() != Some("disabled")).map(|(id, _, _)| id.clone()).collect();
    let use_fallback = allow_policy_fallback && picker.is_empty();
    let available = if !picker.is_empty() || !allow_policy_fallback {
        picker
    } else {
        models.iter().filter(|(_, _, state)| state.as_deref() == Some("enabled")).map(|(id, _, _)| id.clone()).collect()
    };
    let policy = models
        .iter()
        .filter(|(id, picker, state)| state.as_deref() == Some("unconfigured") && known.contains(id) && (*picker || use_fallback))
        .map(|(id, _, _)| id.clone())
        .collect();
    Ok((available, policy))
}

async fn fetch_models(client: &reqwest::Client, base: &str, token: &str, policy: &RetryPolicy) -> Result<(Vec<String>, Vec<String>), String> {
    let allow_policy_fallback = base == "https://api.individual.githubcopilot.com";
    let response = fetch_with_rate_limit_retry(
        || {
            let mut request = client.get(format!("{base}/models")).header("Accept", "application/json").header("Authorization", format!("Bearer {token}"));
            for (k, v) in COPILOT_HEADERS {
                request = request.header(k, v);
            }
            request.header("X-GitHub-Api-Version", COPILOT_API_VERSION)
        },
        policy,
    )
    .await?;
    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        return Err(format!("{} {}: {text}", status.as_u16(), status_text(status)));
    }
    let raw: Value = response.json().await.map_err(|e| e.to_string())?;
    parse_model_catalog(&raw, allow_policy_fallback)
}

/// pi-ai `enableGitHubCopilotModels`: best effort; exhausted 429 stops.
async fn enable_models(client: &reqwest::Client, base: &str, token: &str, ids: &[String]) -> Vec<String> {
    let policy = RetryPolicy { max_retries: 2, max_elapsed: Duration::from_millis(5000) };
    let mut enabled = Vec::new();
    for id in ids {
        let response = fetch_with_rate_limit_retry(
            || {
                let mut request = client
                    .post(format!("{base}/models/{id}/policy"))
                    .header("Content-Type", "application/json")
                    .header("Authorization", format!("Bearer {token}"));
                for (k, v) in COPILOT_HEADERS {
                    request = request.header(k, v);
                }
                request.header("openai-intent", "chat-policy").header("x-interaction-type", "chat-policy").body(r#"{"state":"enabled"}"#)
            },
            &policy,
        )
        .await;
        match response {
            Err(_) => continue,
            Ok(response) if response.status().as_u16() == 429 => break,
            Ok(response) if response.status().is_success() => enabled.push(id.clone()),
            Ok(_) => {}
        }
    }
    enabled
}

pub async fn login(interaction: Interaction, endpoints: Endpoints) -> Result<OAuthCredentials, String> {
    let input = interaction
        .prompt(AuthPrompt::Text { message: "GitHub Enterprise URL/domain (blank for github.com)".into(), placeholder: Some("company.ghe.com".into()) })
        .await?;
    if interaction.is_aborted() {
        return Err("Login cancelled".into());
    }
    let trimmed = input.trim();
    let enterprise = normalize_domain(&input);
    if !trimmed.is_empty() && enterprise.is_none() {
        return Err("Invalid GitHub Enterprise URL/domain".into());
    }
    let domain = enterprise.clone().unwrap_or_else(|| "github.com".into());
    let client = reqwest::Client::new();
    let github = endpoints.github(&domain);
    let device = start_device_flow(&client, &github).await?;
    interaction.notify(AuthEvent::DeviceCode { user_code: device.user_code.clone(), verification_uri: device.verification_uri.clone() });
    let github_token = poll_for_github_access_token(&client, &github, &device).await?;
    let mut credentials = copilot_token(&client, &endpoints.api(&domain), &github_token, enterprise.clone()).await?;
    let base = endpoints.copilot_base.clone().unwrap_or_else(|| base_url_from_token(&credentials.access, enterprise.as_deref()));
    let (available, policy_ids) = fetch_models(&client, &base, &credentials.access, &RetryPolicy { max_retries: 2, max_elapsed: Duration::from_millis(5000) }).await?;
    let mut enabled = Vec::new();
    if !policy_ids.is_empty() {
        interaction.notify(AuthEvent::Progress("Enabling models...".into()));
        enabled = enable_models(&client, &base, &credentials.access, &policy_ids).await;
    }
    let mut ids = Vec::new();
    for id in available.into_iter().chain(enabled) {
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    credentials.available_model_ids = ids;
    Ok(credentials)
}

/// Exchange a GitHub token for a Copilot token on github.com:
/// `(access, expires_ms)`.
pub async fn exchange(github_token: &str) -> Result<(String, i64), String> {
    let credentials = copilot_token(&reqwest::Client::new(), "https://api.github.com", github_token, None).await?;
    Ok((credentials.access, credentials.expires))
}

/// Refresh from the stored refresh value (`<domain>\n<token>` for GitHub
/// Enterprise, else the bare GitHub token): `(access, stored, expires)`.
pub async fn refresh_stored(stored: &str) -> Result<(String, String, i64), String> {
    let (domain, token) = match stored.split_once('\n') {
        Some((domain, token)) => (Some(domain.to_string()), token),
        None => (None, stored),
    };
    let api = format!("https://api.{}", domain.as_deref().unwrap_or("github.com"));
    let credentials = copilot_token(&reqwest::Client::new(), &api, token, domain).await?;
    Ok((credentials.access.clone(), credentials.stored_refresh(), credentials.expires))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_proxy_host_is_restricted_to_copilot_domain() {
        assert_eq!(api_base("tid=x;proxy-ep=proxy.business.githubcopilot.com;exp=1"), "https://api.business.githubcopilot.com");
        for token in ["proxy-ep=proxy.attacker.test", "proxy-ep=proxy.x.githubcopilot.com@attacker.test", "proxy-ep=proxy.x.githubcopilot.com/path"] {
            assert_eq!(api_base(token), "https://api.individual.githubcopilot.com");
        }
    }

    #[test]
    fn domain_normalization_and_login_base_follow_pi_ai() {
        assert_eq!(normalize_domain(" https://Company.GHE.com/x "), Some("company.ghe.com".into()));
        assert_eq!(normalize_domain("company.ghe.com"), Some("company.ghe.com".into()));
        assert_eq!(normalize_domain("  "), None);
        assert_eq!(base_url_from_token("tid=1;proxy-ep=proxy.enterprise.githubcopilot.com;", None), "https://api.enterprise.githubcopilot.com");
        assert_eq!(base_url_from_token("tid=1", Some("corp.ghe.com")), "https://copilot-api.corp.ghe.com");
    }

    #[test]
    fn model_catalog_selects_picker_models_and_unconfigured_policies() {
        let known = catalog_model_ids();
        let first = known.first().cloned().unwrap();
        let raw = serde_json::json!({"data":[
            {"id":"a","model_picker_enabled":true,"policy":{"state":"enabled"}},
            {"id":"b","model_picker_enabled":true,"policy":{"state":"disabled"}},
            {"id":"c","model_picker_enabled":true,"capabilities":{"supports":{"tool_calls":false}}},
            {"id":first,"model_picker_enabled":true,"policy":{"state":"unconfigured"}},
        ]});
        let (available, policy) = parse_model_catalog(&raw, false).unwrap();
        assert_eq!(available, vec!["a".to_string(), first.clone()]);
        assert_eq!(policy, vec![first]);
        let fallback = serde_json::json!({"data":[{"id":"x","policy":{"state":"enabled"}},{"id":"y"}]});
        assert_eq!(parse_model_catalog(&fallback, true).unwrap().0, vec!["x".to_string()]);
        assert!(parse_model_catalog(&fallback, false).unwrap().0.is_empty());
        assert_eq!(parse_model_catalog(&serde_json::json!({}), false).unwrap_err(), "Invalid Copilot models response");
    }
}
