//! OpenRouter login — port of pinned pi-ai `auth/oauth/openrouter.js` (PKCE).
//!
//! OpenRouter exchanges the authorization code for a permanent API key. The
//! callback is handled by a one-shot loopback server on an ephemeral port in
//! the gateway process (as in Plexus), raced against a pasted code / redirect
//! URL for browsers that cannot reach it.
use crate::oauth_login::{
    callback_host, generate_pkce, random_uuid, search_param, search_params, url_param, wait_for_callback_or_manual_input, AuthEvent, CallbackOrManual, Interaction, OAuthCallbackServer, OAuthCredentials,
};
use serde_json::Value;
use std::time::Duration;

const LOGIN_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const TOKEN_EXCHANGE_TIMEOUT: Duration = Duration::from_secs(30);
/// `Number.MAX_SAFE_INTEGER`: OpenRouter keys do not expire.
pub const NEVER_EXPIRES: i64 = 9_007_199_254_740_991;

#[derive(Debug, Clone)]
pub struct Endpoints {
    pub authorize_url: String,
    pub token_url: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self { authorize_url: "https://openrouter.ai/auth".into(), token_url: "https://openrouter.ai/api/v1/auth/keys".into() }
    }
}

/// pi-ai openrouter `parseAuthorizationInput` (code only).
fn parse_authorization_input(input: &str) -> Option<String> {
    let value = input.trim();
    if value.is_empty() {
        return None;
    }
    if let Ok(url) = url::Url::parse(value) {
        return url_param(&url, "code");
    }
    if value.contains("code=") {
        return search_param(value, "code");
    }
    Some(value.to_string())
}

fn error_detail(body: &serde_json::Map<String, Value>) -> Option<String> {
    for key in ["error_description", "message", "error"] {
        if let Some(text) = body.get(key).and_then(Value::as_str) {
            return Some(text.to_string());
        }
    }
    body.get("error").and_then(Value::as_object).and_then(|e| e.get("message")).and_then(Value::as_str).map(str::to_owned)
}

async fn exchange_authorization_code(interaction: &Interaction, token_url: &str, code: &str, verifier: &str) -> Result<OAuthCredentials, String> {
    if interaction.is_aborted() {
        return Err("Login cancelled".into());
    }
    let request = reqwest::Client::new()
        .post(token_url)
        .header("accept", "application/json")
        .header("content-type", "application/json")
        .body(serde_json::json!({"code": code, "code_verifier": verifier, "code_challenge_method": "S256"}).to_string())
        .send();
    let response = match tokio::time::timeout(TOKEN_EXCHANGE_TIMEOUT, request).await {
        Err(_) => return Err("OpenRouter OAuth token exchange timed out".into()),
        Ok(Err(error)) => return Err(error.to_string()),
        Ok(Ok(response)) => response,
    };
    let status = response.status();
    let body = match tokio::time::timeout(TOKEN_EXCHANGE_TIMEOUT, response.json::<Value>()).await {
        Err(_) => return Err("OpenRouter OAuth token exchange timed out".into()),
        Ok(Ok(Value::Object(map))) => map,
        Ok(Ok(_)) => serde_json::Map::new(),
        Ok(Err(_)) if status.is_success() => return Err("OpenRouter OAuth returned invalid JSON".into()),
        Ok(Err(_)) => serde_json::Map::new(),
    };
    if !status.is_success() {
        let detail = error_detail(&body).map(|d| format!(": {d}")).unwrap_or_default();
        return Err(format!("OpenRouter OAuth key exchange failed (HTTP {}){detail}", status.as_u16()));
    }
    let Some(key) = body.get("key").and_then(Value::as_str).filter(|k| !k.is_empty()) else {
        return Err("OpenRouter OAuth response carries no \"key\"".into());
    };
    Ok(OAuthCredentials::new(key, "", NEVER_EXPIRES))
}

pub async fn login(interaction: Interaction, endpoints: Endpoints) -> Result<OAuthCredentials, String> {
    let (verifier, challenge) = generate_pkce();
    // OpenRouter sends no `state`; the random path keeps stray requests out.
    let path = format!("/oauth/callback/{}", random_uuid());
    let callback = OAuthCallbackServer::start("OpenRouter", &callback_host(), 0, &path, None, None, Some(LOGIN_TIMEOUT)).await?;
    let redirect_uri = callback.redirect_uri.clone();
    let mut authorize = url::Url::parse(&endpoints.authorize_url).map_err(|e| e.to_string())?;
    authorize.set_query(Some(&search_params(&[("callback_url", &redirect_uri), ("code_challenge", &challenge), ("code_challenge_method", "S256")])));
    interaction.notify(AuthEvent::Progress(format!("Listening for OpenRouter OAuth callback on {redirect_uri}")));
    interaction.notify(AuthEvent::AuthUrl {
        url: authorize.to_string(),
        instructions: Some("Complete sign-in in your browser. If the browser is on another machine, paste the final redirect URL here.".into()),
    });
    let token_url = endpoints.token_url.clone();
    let result = wait_for_callback_or_manual_input(
        &interaction,
        Some(&callback),
        |code| {
            let interaction = interaction.clone();
            let token_url = token_url.clone();
            let verifier = verifier.clone();
            async move { exchange_authorization_code(&interaction, &token_url, &code, &verifier).await }
        },
        "Complete sign-in in your browser, or paste the authorization code / redirect URL here:",
        &redirect_uri,
    )
    .await?;
    match result {
        CallbackOrManual::Callback(credentials) => Ok(credentials),
        CallbackOrManual::Manual(input) => {
            let code = parse_authorization_input(&input).filter(|c| !c.is_empty()).ok_or("Missing authorization code")?;
            interaction.notify(AuthEvent::Progress("Exchanging authorization code for an API key...".into()));
            exchange_authorization_code(&interaction, &endpoints.token_url, &code, &verifier).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_code_from_url_query_or_bare() {
        assert_eq!(parse_authorization_input("http://127.0.0.1:1/oauth/callback/x?code=abc").as_deref(), Some("abc"));
        assert_eq!(parse_authorization_input("http://127.0.0.1:1/oauth/callback/x").as_deref(), None);
        assert_eq!(parse_authorization_input("code=abc&x=1").as_deref(), Some("abc"));
        assert_eq!(parse_authorization_input(" abc ").as_deref(), Some("abc"));
        assert_eq!(parse_authorization_input("  "), None);
    }
}
