//! OpenAI (ChatGPT subscription) login — port of pinned pi-ai
//! `auth/oauth/openai-chatgpt.js`.
//!
//! Every login dynamically registers a client; OpenAI returns the issued
//! client id in the loopback callback and refresh must reuse it. The flow
//! binds the fixed loopback port 1455 inside the gateway process (as Plexus
//! does) and races it against a pasted final redirect URL.
//!
//! pi-ai requires a per-installation device id (`getDeviceId`). Plexus calls
//! `login(interaction)` without options, so its login fails immediately with
//! "Sign in with ChatGPT requires a device ID (UUID) for this installation".
//! Trefoil reproduces that unless `TREFOIL_OAUTH_DEVICE_ID` holds a UUID.
//!
//! The issued client id is persisted with the refresh token as
//! `<client_id>\n<refresh_token>` (`OAuthCredentials::stored_refresh`).
use crate::oauth_login::{
    callback_host, generate_pkce, now_ms, oauth_error_html, oauth_success_html, random_bytes,
    search_params, serve_until, status_text, url_param, AuthEvent, AuthPrompt, CallbackDecision,
    Interaction, OAuthCredentials, RawCallbackServer,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde_json::Value;

const DYNAMIC_CLIENT_ID: &str = "dynamic_agent_client";
const AGENT_NAME_HINT: &str = "Pi";
const RESOURCE: &str = "https://api.openai.com/v1";
const CALLBACK_PORT: u16 = 1455;
pub const REDIRECT_URI: &str = "http://127.0.0.1:1455/auth/callback";
const DIRECT_TOKEN_SCOPE: &str = "chatgpt.tokens.use.direct";
const SCOPE: &str = "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
const EXPIRY_MARGIN_MS: i64 = 3 * 60 * 1000;

#[derive(Debug, Clone)]
pub struct Endpoints {
    pub authorize_url: String,
    pub token_url: String,
    pub callback_port: u16,
    /// pi-ai `options.getDeviceId()`; `None` uses the session's installation
    /// device ID, then `TREFOIL_OAUTH_DEVICE_ID`.
    pub device_id: Option<String>,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            authorize_url: "https://auth.openai.com/api/accounts/authorize".into(),
            token_url: "https://auth.openai.com/api/accounts/oauth/token".into(),
            callback_port: CALLBACK_PORT,
            device_id: None,
        }
    }
}

fn random_value() -> String {
    URL_SAFE_NO_PAD.encode(random_bytes(32))
}

fn is_uuid(value: &str) -> bool {
    let b = value.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                *c == b'-'
            } else {
                c.is_ascii_hexdigit()
            }
        })
}

/// pi-ai `agentHostId`.
fn agent_host_id(device_id: Option<String>) -> Result<String, String> {
    match device_id.filter(|id| is_uuid(id)) {
        Some(id) => Ok(format!("urn:uuid:{}", id.to_ascii_lowercase())),
        None => {
            Err("Sign in with ChatGPT requires a device ID (UUID) for this installation".into())
        }
    }
}

/// pi-ai `authorizationResultFromCallback`: (code, issued client id).
fn authorization_result_from_callback(
    url: &url::Url,
    expected_state: &str,
) -> Result<(String, String), String> {
    let code = url_param(url, "code")
        .filter(|v| !v.is_empty())
        .ok_or("Missing authorization code")?;
    let state = url_param(url, "state")
        .filter(|v| !v.is_empty())
        .ok_or("Missing OAuth state")?;
    if state != expected_state {
        return Err("OAuth state mismatch".into());
    }
    let client = url_param(url, "client_id")
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
        .ok_or("OpenAI OAuth registration callback did not contain an issued client ID")?;
    Ok((code, client))
}

/// pi-ai `authorizationResultFromManualInput`.
pub fn parse_callback(input: &str, expected_state: &str) -> Result<(String, String), String> {
    let url = url::Url::parse(input.trim())
        .map_err(|_| "Paste the full callback URL from the browser".to_string())?;
    let expected = url::Url::parse(REDIRECT_URI).expect("static URL");
    if url.origin() != expected.origin() || url.path() != expected.path() {
        return Err(format!(
            "The pasted callback URL must start with {REDIRECT_URI}"
        ));
    }
    if let Some(error) = url_param(&url, "error").filter(|e| !e.is_empty()) {
        return Err(format!("ChatGPT authorization failed: {error}"));
    }
    authorization_result_from_callback(&url, expected_state)
}

async fn request_token(token_url: &str, body: String) -> Result<Value, String> {
    let response = reqwest::Client::new()
        .post(token_url)
        .header("accept", "application/json")
        .header("content-type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        return Err(format!(
            "OpenAI OAuth token request failed ({}): {}",
            status.as_u16(),
            if text.is_empty() {
                status_text(status).to_string()
            } else {
                text
            }
        ));
    }
    let data: Value = response.json().await.map_err(|e| e.to_string())?;
    if !data.is_object() {
        return Err("OpenAI OAuth token response must be an object".into());
    }
    Ok(data)
}

/// pi-ai `credentialFromTokenResponse`.
fn credential_from_token_response(
    token: &Value,
    client_id: &str,
) -> Result<OAuthCredentials, String> {
    let s = |f: &str| {
        token[f]
            .as_str()
            .filter(|v| !v.trim().is_empty())
            .map(str::to_owned)
            .ok_or_else(|| format!("OpenAI OAuth token response has invalid {f}"))
    };
    let (access, refresh, scope) = (s("access_token")?, s("refresh_token")?, s("scope")?);
    let expires_in = token["expires_in"]
        .as_f64()
        .filter(|v| v.is_finite() && *v > 0.0)
        .ok_or("OpenAI OAuth token response has invalid expires_in")?;
    let scopes: Vec<String> = scope.split_whitespace().map(str::to_owned).collect();
    if !scopes.iter().any(|v| v == DIRECT_TOKEN_SCOPE) {
        return Err(format!(
            "OpenAI OAuth grant did not include {DIRECT_TOKEN_SCOPE}"
        ));
    }
    let mut credentials = OAuthCredentials::new(
        access,
        refresh,
        now_ms() + (expires_in * 1000.0) as i64 - EXPIRY_MARGIN_MS,
    );
    credentials.client_id = Some(client_id.to_string());
    credentials.scopes = scopes;
    Ok(credentials)
}

async fn exchange_authorization_code(
    token_url: &str,
    code: &str,
    verifier: &str,
    client_id: &str,
) -> Result<OAuthCredentials, String> {
    let token = request_token(
        token_url,
        search_params(&[
            ("grant_type", "authorization_code"),
            ("client_id", client_id),
            ("code", code),
            ("code_verifier", verifier),
            ("redirect_uri", REDIRECT_URI),
            ("resource", RESOURCE),
        ]),
    )
    .await?;
    if token["id_token"]
        .as_str()
        .is_none_or(|v| v.trim().is_empty())
    {
        return Err("OpenAI OAuth token response did not contain an ID token".into());
    }
    credential_from_token_response(&token, client_id)
}

pub async fn login(
    interaction: Interaction,
    endpoints: Endpoints,
) -> Result<OAuthCredentials, String> {
    let device_id = endpoints
        .device_id
        .clone()
        .or_else(|| interaction.device_id().map(str::to_owned))
        .or_else(|| {
            std::env::var("TREFOIL_OAUTH_DEVICE_ID")
                .ok()
                .map(|v| v.trim().to_string())
        });
    let host_id = agent_host_id(device_id)?;
    let (verifier, challenge) = generate_pkce();
    let state = random_value();
    let nonce = random_value();
    let server = RawCallbackServer::bind(&callback_host(), endpoints.callback_port).await.map_err(|error| {
        if error.kind() == std::io::ErrorKind::AddrInUse {
            format!("Port {CALLBACK_PORT} is in use, probably by an unfinished login in another pi session or by the Codex CLI. Cancel that login and try again.")
        } else {
            error.to_string()
        }
    })?;
    let mut url = url::Url::parse(&endpoints.authorize_url).map_err(|e| e.to_string())?;
    url.set_query(Some(&search_params(&[
        ("client_id", DYNAMIC_CLIENT_ID),
        ("agent_name_hint", AGENT_NAME_HINT),
        ("ext_agent_host_id", &host_id),
        ("response_type", "code"),
        ("redirect_uri", REDIRECT_URI),
        ("resource", RESOURCE),
        ("scope", SCOPE),
        ("state", &state),
        ("code_challenge", &challenge),
        ("code_challenge_method", "S256"),
        ("nonce", &nonce),
    ])));
    interaction.notify(AuthEvent::AuthUrl { url: url.to_string(), instructions: Some("Complete sign-in in your browser. If the callback does not complete, paste the final redirect URL here.".into()) });
    let manual = interaction.prompt(AuthPrompt::ManualCode {
        message: "Complete login in your browser, or paste the final redirect URL here:".into(),
        placeholder: Some(REDIRECT_URI.into()),
    });
    let expected = state.clone();
    let callback = serve_until(&server, REDIRECT_URI, false, |_method, url| {
        let expected = expected.clone();
        async move {
            if url.path() != "/auth/callback" {
                return CallbackDecision::Respond(
                    404,
                    oauth_error_html("Callback route not found.", None),
                );
            }
            if let Some(error) = url_param(&url, "error").filter(|e| !e.is_empty()) {
                return CallbackDecision::Finish(
                    400,
                    oauth_error_html(
                        "ChatGPT was not connected.",
                        Some(&format!("Error: {error}")),
                    ),
                    Err(format!("ChatGPT authorization failed: {error}")),
                );
            }
            match authorization_result_from_callback(&url, &expected) {
                Err(message) => CallbackDecision::Respond(400, oauth_error_html(&message, None)),
                Ok(result) => CallbackDecision::Finish(
                    200,
                    oauth_success_html(
                        "ChatGPT authentication completed. You can close this window.",
                    ),
                    Ok(result),
                ),
            }
        }
    });
    let result = tokio::select! {
        result = callback => result,
        input = manual => input.and_then(|input| parse_callback(&input, &state)),
    };
    let outcome = async {
        let (code, client_id) = result?;
        interaction.notify(AuthEvent::Progress(
            "Exchanging authorization code for tokens...".into(),
        ));
        exchange_authorization_code(&endpoints.token_url, &code, &verifier, &client_id).await
    }
    .await;
    drop(server);
    outcome.map_err(|error| {
        if interaction.is_aborted() {
            "Login cancelled".into()
        } else {
            error
        }
    })
}

/// Refresh from the stored `<client_id>\n<refresh>` value:
/// `(access, stored_refresh, expires)`.
pub async fn refresh(stored: &str) -> Result<(String, String, i64), String> {
    refresh_at(&Endpoints::default().token_url, stored).await
}

async fn refresh_at(token_url: &str, stored: &str) -> Result<(String, String, i64), String> {
    let Some((client_id, refresh)) = stored
        .split_once('\n')
        .filter(|(c, r)| !c.trim().is_empty() && !r.is_empty())
    else {
        return Err("Stored OpenAI OAuth credential does not contain an issued client ID; reconnect ChatGPT".into());
    };
    let token = request_token(
        token_url,
        search_params(&[
            ("grant_type", "refresh_token"),
            ("client_id", client_id),
            ("refresh_token", refresh),
            ("resource", RESOURCE),
        ]),
    )
    .await?;
    let credentials = credential_from_token_response(&token, client_id)?;
    Ok((
        credentials.access.clone(),
        credentials.stored_refresh(),
        credentials.expires,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Bytes, routing::post, Json, Router};

    #[test]
    fn callback_parsing_and_device_id_follow_pinned_contract() {
        assert_eq!(
            parse_callback(
                "http://127.0.0.1:1455/auth/callback?code=c&state=st&client_id=issued",
                "st"
            )
            .unwrap(),
            ("c".into(), "issued".into())
        );
        assert_eq!(
            parse_callback(
                "http://127.0.0.1:1455/auth/callback?code=c&state=other&client_id=i",
                "st"
            )
            .unwrap_err(),
            "OAuth state mismatch"
        );
        assert!(
            parse_callback("http://127.0.0.1:1455/auth/callback?code=c&state=st", "st")
                .unwrap_err()
                .contains("issued client ID")
        );
        assert!(parse_callback(
            "https://evil.example/auth/callback?code=c&state=st&client_id=i",
            "st"
        )
        .unwrap_err()
        .contains("must start with"));
        assert_eq!(
            parse_callback("code-only", "st").unwrap_err(),
            "Paste the full callback URL from the browser"
        );
        assert_eq!(
            agent_host_id(None).unwrap_err(),
            "Sign in with ChatGPT requires a device ID (UUID) for this installation"
        );
        assert_eq!(
            agent_host_id(Some("123E4567-E89B-12D3-A456-426614174000".into())).unwrap(),
            "urn:uuid:123e4567-e89b-12d3-a456-426614174000"
        );
    }

    #[tokio::test]
    async fn exchange_and_refresh_reuse_issued_client_id() {
        let app = Router::new()
            .route(
                "/token",
                post(|body: Bytes| async move {
                    let f: std::collections::HashMap<_, _> = url::form_urlencoded::parse(&body).into_owned().collect();
                    assert_eq!(f["client_id"], "issued");
                    assert_eq!(f["resource"], RESOURCE);
                    let id = if f["grant_type"] == "authorization_code" {
                        assert_eq!(f["redirect_uri"], REDIRECT_URI);
                        Some("id")
                    } else {
                        assert_eq!(f["refresh_token"], "r1");
                        None
                    };
                    Json(serde_json::json!({"access_token":"a","refresh_token":"r1","scope":"openid chatgpt.tokens.use.direct","expires_in":3600,"id_token":id}))
                }),
            )
            .route("/noscope", post(|| async { Json(serde_json::json!({"access_token":"a","refresh_token":"r","scope":"openid","expires_in":3600,"id_token":"id"})) }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let credentials =
            exchange_authorization_code(&format!("http://{addr}/token"), "code", "v", "issued")
                .await
                .unwrap();
        assert_eq!(
            (
                credentials.access.as_str(),
                credentials.stored_refresh().as_str()
            ),
            ("a", "issued\nr1")
        );
        let (_, again, _) = refresh_at(
            &format!("http://{addr}/token"),
            &credentials.stored_refresh(),
        )
        .await
        .unwrap();
        assert_eq!(again, "issued\nr1");
        assert!(
            refresh_at(&format!("http://{addr}/token"), "legacy-refresh")
                .await
                .unwrap_err()
                .contains("reconnect")
        );
        assert!(exchange_authorization_code(
            &format!("http://{addr}/noscope"),
            "code",
            "v",
            "issued"
        )
        .await
        .unwrap_err()
        .contains(DIRECT_TOKEN_SCOPE));
        server.abort();
    }
}
