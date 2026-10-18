//! Anthropic (Claude Pro/Max) login — port of the pinned Plexus override
//! `services/oauth/anthropic.ts` (copy-code flow), which replaces pi-ai's
//! loopback-callback login. The browser is sent to Anthropic's code page
//! (`https://platform.claude.com/oauth/code/callback`), which shows
//! `code#state` for the user to paste. Refresh stays with the generic
//! transport (`oauth_transport`).
use crate::oauth_login::{generate_pkce, search_param, search_params, url_param, AuthEvent, AuthPrompt, Interaction, OAuthCredentials};
use serde_json::Value;
use std::time::Duration;

pub const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
pub const COPY_CODE_REDIRECT_URI: &str = "https://platform.claude.com/oauth/code/callback";
const SCOPES: &str = "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";
const TOKEN_TIMEOUT: Duration = Duration::from_secs(30);
const EXPIRY_SKEW_MS: i64 = 5 * 60 * 1000;

#[derive(Debug, Clone)]
pub struct Endpoints {
    pub authorize_url: String,
    pub token_url: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self { authorize_url: "https://claude.ai/oauth/authorize".into(), token_url: "https://platform.claude.com/v1/oauth/token".into() }
    }
}

/// `parseAuthorizationInput`: `code#state`, a redirect URL, a query string, or
/// a bare code. `Some("")` is kept distinct from `None` (JS `??` semantics).
pub fn parse_authorization_input(input: &str) -> (Option<String>, Option<String>) {
    let value = input.trim();
    if value.is_empty() {
        return (None, None);
    }
    if let Ok(url) = url::Url::parse(value) {
        return (url_param(&url, "code"), url_param(&url, "state"));
    }
    if value.contains('#') {
        let mut parts = value.split('#');
        return (parts.next().map(str::to_owned), parts.next().map(str::to_owned));
    }
    if value.contains("code=") {
        return (search_param(value, "code"), search_param(value, "state"));
    }
    (Some(value.to_owned()), None)
}

pub async fn login(interaction: Interaction, endpoints: Endpoints) -> Result<OAuthCredentials, String> {
    let (verifier, challenge) = generate_pkce();
    let params = search_params(&[
        ("code", "true"),
        ("client_id", CLIENT_ID),
        ("response_type", "code"),
        ("redirect_uri", COPY_CODE_REDIRECT_URI),
        ("scope", SCOPES),
        ("code_challenge", &challenge),
        ("code_challenge_method", "S256"),
        // Parity with pi-ai: the PKCE verifier doubles as the OAuth state.
        ("state", &verifier),
    ]);
    interaction.notify(AuthEvent::AuthUrl {
        url: format!("{}?{params}", endpoints.authorize_url),
        instructions: Some("Sign in in your browser. Anthropic shows an authorization code — copy it and paste it below.".into()),
    });
    let input = interaction
        .prompt(AuthPrompt::ManualCode { message: "Paste the code Anthropic shows after you sign in:".into(), placeholder: Some("code#state".into()) })
        .await?;
    let (code, state) = parse_authorization_input(&input);
    if let Some(state) = state.as_deref().filter(|s| !s.is_empty()) {
        if state != verifier {
            return Err("OAuth state mismatch".into());
        }
    }
    let Some(code) = code.filter(|c| !c.is_empty()) else {
        return Err("Missing authorization code".into());
    };
    interaction.notify(AuthEvent::Progress("Exchanging authorization code for tokens...".into()));
    exchange_authorization_code(&endpoints.token_url, &code, &state.unwrap_or_else(|| verifier.clone()), &verifier).await
}

async fn exchange_authorization_code(token_url: &str, code: &str, state: &str, verifier: &str) -> Result<OAuthCredentials, String> {
    let body = serde_json::json!({
        "grant_type": "authorization_code",
        "client_id": CLIENT_ID,
        "code": code,
        "state": state,
        "redirect_uri": COPY_CODE_REDIRECT_URI,
        "code_verifier": verifier,
    });
    let response = reqwest::Client::new()
        .post(token_url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .body(body.to_string())
        .timeout(TOKEN_TIMEOUT)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = response.status();
    let text = response.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("Anthropic token exchange failed. status={}; body={text}", status.as_u16()));
    }
    // Never echo a 2xx body: it can carry token material.
    let json: Value = serde_json::from_str(&text).map_err(|_| "Anthropic token exchange returned invalid JSON".to_string())?;
    let mut missing = Vec::new();
    let object = json.as_object();
    match object {
        None => missing.push(String::new()),
        Some(map) => {
            for field in ["access_token", "refresh_token"] {
                if map.get(field).and_then(Value::as_str).is_none_or(|v| v.is_empty()) {
                    missing.push(field.to_string());
                }
            }
            if !map.get("expires_in").is_some_and(Value::is_number) {
                missing.push("expires_in".into());
            }
        }
    }
    if !missing.is_empty() {
        return Err(format!("Anthropic token exchange response missing fields: {}", missing.join(", ")));
    }
    let expires_in = json["expires_in"].as_f64().unwrap_or(0.0);
    Ok(OAuthCredentials::new(
        json["access_token"].as_str().unwrap_or_default(),
        json["refresh_token"].as_str().unwrap_or_default(),
        crate::oauth_login::now_ms() + (expires_in * 1000.0) as i64 - EXPIRY_SKEW_MS,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_copy_code_inputs_like_reference() {
        assert_eq!(parse_authorization_input("abc#ver"), (Some("abc".into()), Some("ver".into())));
        assert_eq!(parse_authorization_input("a#b#c"), (Some("a".into()), Some("b".into())));
        assert_eq!(parse_authorization_input("abc#"), (Some("abc".into()), Some("".into())));
        assert_eq!(parse_authorization_input("https://platform.claude.com/oauth/code/callback?code=c&state=s"), (Some("c".into()), Some("s".into())));
        assert_eq!(parse_authorization_input("code=c&state=s"), (Some("c".into()), Some("s".into())));
        assert_eq!(parse_authorization_input("?code=c"), (Some("c".into()), None));
        assert_eq!(parse_authorization_input(" bare "), (Some("bare".into()), None));
        assert_eq!(parse_authorization_input("   "), (None, None));
    }
}
