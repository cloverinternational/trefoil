//! OpenAI Codex (ChatGPT Plus/Pro) login — port of pinned pi-ai
//! `auth/oauth/openai-codex.js`.
//!
//! The login first asks for a method (`select`): browser login runs the
//! loopback callback server on port 1455 inside the gateway process (as
//! Plexus does) raced against a pasted code / redirect URL; device-code login
//! polls OpenAI's device-auth endpoints. Refresh stays with the generic
//! transport (`oauth_transport`).
use crate::oauth_login::{
    callback_host, generate_pkce, js_stringify, now_ms, poll_device_code_flow, random_bytes,
    status_text, wait_for_callback_or_manual_input, AuthEvent, AuthPrompt, CallbackOrManual,
    DevicePoll, Interaction, OAuthCallbackServer, OAuthCredentials, OAuthSelectOption,
};
use serde_json::Value;

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const DEVICE_CODE_TIMEOUT_SECONDS: f64 = 15.0 * 60.0;
const SCOPE: &str = "openid profile email offline_access";
const JWT_CLAIM_PATH: &str = "https://api.openai.com/auth";

#[derive(Debug, Clone)]
pub struct Endpoints {
    /// `https://auth.openai.com`.
    pub auth_base: String,
    pub callback_port: u16,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            auth_base: "https://auth.openai.com".into(),
            callback_port: 1455,
        }
    }
}

impl Endpoints {
    fn authorize_url(&self) -> String {
        format!("{}/oauth/authorize", self.auth_base)
    }
    fn token_url(&self) -> String {
        format!("{}/oauth/token", self.auth_base)
    }
}

/// `https://auth.openai.com/codex/device` (shown to the user, never fetched).
const DEVICE_VERIFICATION_URI: &str = "https://auth.openai.com/codex/device";
const DEVICE_REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";

/// pi-ai `parseAuthorizationInput` (code/state from URL, `code#state`, query, bare).
fn parse_authorization_input(input: &str) -> (Option<String>, Option<String>) {
    crate::anthropic_oauth::parse_authorization_input(input)
}

/// `atob` (forgiving base64; base64url characters are rejected).
fn atob(input: &str) -> Option<Vec<u8>> {
    let mut s: String = input.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    if s.len().is_multiple_of(4) {
        if s.ends_with("==") {
            s.truncate(s.len() - 2);
        } else if s.ends_with('=') {
            s.truncate(s.len() - 1);
        }
    }
    if s.len() % 4 == 1
        || !s
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/')
    {
        return None;
    }
    base64::Engine::decode(&base64::engine::general_purpose::STANDARD_NO_PAD, s).ok()
}

/// pi-ai `getAccountId` (via `decodeJwt`).
pub fn jwt_account_id(token: &str) -> Option<String> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let payload: Value = serde_json::from_slice(&atob(parts[1])?).ok()?;
    payload[JWT_CLAIM_PATH]["chatgpt_account_id"]
        .as_str()
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
}

async fn read_token_response(
    response: reqwest::Response,
    operation: &str,
) -> Result<OAuthCredentials, String> {
    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        return Err(format!(
            "OpenAI Codex token {operation} failed ({}): {}",
            status.as_u16(),
            if text.is_empty() {
                status_text(status).to_string()
            } else {
                text
            }
        ));
    }
    let json: Value = response.json().await.map_err(|e| e.to_string())?;
    let access = json["access_token"].as_str().filter(|v| !v.is_empty());
    let refresh = json["refresh_token"].as_str().filter(|v| !v.is_empty());
    let (Some(access), Some(refresh), Some(expires_in)) =
        (access, refresh, json["expires_in"].as_f64())
    else {
        return Err(format!(
            "OpenAI Codex token {operation} response missing fields: {}",
            js_stringify(Some(&json))
        ));
    };
    Ok(OAuthCredentials::new(
        access,
        refresh,
        now_ms() + (expires_in * 1000.0) as i64,
    ))
}

/// Exchange an authorization code and require the ChatGPT account claim.
async fn exchange_for_credentials(
    endpoints: &Endpoints,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<OAuthCredentials, String> {
    let response = reqwest::Client::new()
        .post(endpoints.token_url())
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(crate::oauth_login::search_params(&[
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("code", code),
            ("code_verifier", verifier),
            ("redirect_uri", redirect_uri),
        ]))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let mut credentials = read_token_response(response, "exchange").await?;
    let account =
        jwt_account_id(&credentials.access).ok_or("Failed to extract accountId from token")?;
    credentials.account_id = Some(account);
    Ok(credentials)
}

async fn login_device_code(
    interaction: &Interaction,
    endpoints: &Endpoints,
) -> Result<OAuthCredentials, String> {
    let client = reqwest::Client::new();
    let response = client
        .post(format!(
            "{}/api/accounts/deviceauth/usercode",
            endpoints.auth_base
        ))
        .header("Content-Type", "application/json")
        .body(serde_json::json!({"client_id": CLIENT_ID}).to_string())
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = response.status();
    if !status.is_success() {
        if status.as_u16() == 404 {
            return Err("OpenAI Codex device code login is not enabled for this server. Use browser login or verify the server URL.".into());
        }
        let body = response.text().await.unwrap_or_default();
        return Err(format!(
            "OpenAI Codex device code request failed with status {}{}",
            status.as_u16(),
            if body.is_empty() {
                String::new()
            } else {
                format!(": {body}")
            }
        ));
    }
    let json: Value = response.json().await.map_err(|e| e.to_string())?;
    let interval = match &json["interval"] {
        Value::String(s) => {
            let t = s.trim();
            if t.is_empty() {
                Some(0.0)
            } else {
                t.parse::<f64>().ok()
            }
        }
        other => other.as_f64(),
    };
    let device_id = json["device_auth_id"].as_str().filter(|v| !v.is_empty());
    let user_code = json["user_code"].as_str().filter(|v| !v.is_empty());
    let (Some(device_id), Some(user_code), Some(interval)) = (
        device_id,
        user_code,
        interval.filter(|v| v.is_finite() && *v >= 0.0),
    ) else {
        return Err(format!(
            "Invalid OpenAI Codex device code response: {}",
            js_stringify(Some(&json))
        ));
    };
    interaction.notify(AuthEvent::DeviceCode {
        user_code: user_code.into(),
        verification_uri: DEVICE_VERIFICATION_URI.into(),
    });
    let token_url = format!("{}/api/accounts/deviceauth/token", endpoints.auth_base);
    let (code, verifier) = poll_device_code_flow(
        Some(interval),
        Some(DEVICE_CODE_TIMEOUT_SECONDS),
        false,
        || async {
            let response = client
                .post(&token_url)
                .header("Content-Type", "application/json")
                .body(
                    serde_json::json!({"device_auth_id": device_id, "user_code": user_code})
                        .to_string(),
                )
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let status = response.status();
            if status.is_success() {
                let json: Value = response.json().await.map_err(|e| e.to_string())?;
                let code = json["authorization_code"]
                    .as_str()
                    .filter(|v| !v.is_empty());
                let verifier = json["code_verifier"].as_str().filter(|v| !v.is_empty());
                return Ok(match (code, verifier) {
                    (Some(code), Some(verifier)) => {
                        DevicePoll::Complete((code.to_string(), verifier.to_string()))
                    }
                    _ => DevicePoll::Failed(format!(
                        "Invalid OpenAI Codex device auth token response: {}",
                        js_stringify(Some(&json))
                    )),
                });
            }
            if matches!(status.as_u16(), 403 | 404) {
                return Ok(DevicePoll::Pending);
            }
            let body = response.text().await.unwrap_or_default();
            let error_code = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|json| match &json["error"] {
                    Value::Object(map) => {
                        map.get("code").and_then(Value::as_str).map(str::to_owned)
                    }
                    Value::String(s) => Some(s.clone()),
                    _ => None,
                });
            Ok(match error_code.as_deref() {
                Some("deviceauth_authorization_pending") => DevicePoll::Pending,
                Some("slow_down") => DevicePoll::SlowDown(None),
                _ => DevicePoll::Failed(format!(
                    "OpenAI Codex device auth failed with status {}{}",
                    status.as_u16(),
                    if body.is_empty() {
                        String::new()
                    } else {
                        format!(": {body}")
                    }
                )),
            })
        },
    )
    .await?;
    exchange_for_credentials(endpoints, &code, &verifier, DEVICE_REDIRECT_URI).await
}

/// pi-ai `createAuthorizationFlow("pi")`: (verifier, state, url).
fn create_authorization_flow(endpoints: &Endpoints) -> (String, String, String) {
    let (verifier, challenge) = generate_pkce();
    let state = hex::encode(random_bytes(16));
    let mut url = url::Url::parse(&endpoints.authorize_url()).expect("authorize URL");
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("redirect_uri", REDIRECT_URI)
        .append_pair("scope", SCOPE)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state)
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true")
        .append_pair("originator", "pi");
    (verifier, state, url.to_string())
}

async fn login_browser(
    interaction: &Interaction,
    endpoints: &Endpoints,
) -> Result<OAuthCredentials, String> {
    let (verifier, state, url) = create_authorization_flow(endpoints);
    // Port 1455 is shared with the Codex CLI; when taken, only the paste works.
    let callback = OAuthCallbackServer::start(
        "OpenAI",
        &callback_host(),
        endpoints.callback_port,
        "/auth/callback",
        None,
        Some(state.clone()),
        None,
    )
    .await
    .ok();
    interaction.notify(AuthEvent::AuthUrl {
        url,
        instructions: Some("A browser window should open. Complete login to finish.".into()),
    });
    let result = wait_for_callback_or_manual_input(
        interaction,
        callback.as_ref(),
        |code| async move { Ok::<String, String>(code) },
        "Complete login in your browser, or paste the authorization code / redirect URL here:",
        REDIRECT_URI,
    )
    .await?;
    let code = match result {
        CallbackOrManual::Callback(code) => Some(code),
        CallbackOrManual::Manual(input) => {
            let (code, pasted_state) = parse_authorization_input(&input);
            if let Some(pasted) = pasted_state.filter(|s| !s.is_empty()) {
                if pasted != state {
                    return Err("State mismatch".into());
                }
            }
            code
        }
    };
    let code = code
        .filter(|c| !c.is_empty())
        .ok_or("Missing authorization code")?;
    exchange_for_credentials(endpoints, &code, &verifier, REDIRECT_URI).await
}

pub async fn login(
    interaction: Interaction,
    endpoints: Endpoints,
) -> Result<OAuthCredentials, String> {
    let method = interaction
        .prompt(AuthPrompt::Select {
            message: "Select OpenAI Codex login method:".into(),
            options: vec![
                OAuthSelectOption {
                    id: "browser".into(),
                    label: "Browser login (default)".into(),
                    description: None,
                },
                OAuthSelectOption {
                    id: "device_code".into(),
                    label: "Device code login (headless)".into(),
                    description: None,
                },
            ],
        })
        .await?;
    match method.as_str() {
        "device_code" => login_device_code(&interaction, &endpoints).await,
        "browser" => login_browser(&interaction, &endpoints).await,
        other => Err(format!("Unknown OpenAI Codex login method: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oauth_login::url_param;

    #[test]
    fn account_claim_uses_atob_semantics() {
        let payload = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            br#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acct"}}"#,
        );
        assert_eq!(
            jwt_account_id(&format!("h.{payload}.s")).as_deref(),
            Some("acct")
        );
        assert_eq!(jwt_account_id("not-a-jwt"), None);
        assert_eq!(jwt_account_id("a.b-_.c"), None);
    }

    #[test]
    fn authorization_url_matches_reference_parameter_order() {
        let (_, state, url) = create_authorization_flow(&Endpoints::default());
        let parsed = url::Url::parse(&url).unwrap();
        let keys: Vec<String> = parsed.query_pairs().map(|(k, _)| k.into_owned()).collect();
        assert_eq!(
            keys,
            [
                "response_type",
                "client_id",
                "redirect_uri",
                "scope",
                "code_challenge",
                "code_challenge_method",
                "state",
                "id_token_add_organizations",
                "codex_cli_simplified_flow",
                "originator"
            ]
        );
        assert_eq!(url_param(&parsed, "state").unwrap(), state);
        assert_eq!(state.len(), 32);
    }
}
