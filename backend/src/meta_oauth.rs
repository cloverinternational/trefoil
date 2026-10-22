//! Meta (Muse subscription) login — port of pinned pi-ai `auth/oauth/meta.js`.
//!
//! RFC 8628 device grant against auth.meta.com yields an identity token that
//! is not accepted for inference; it is exchanged for a day-long Model API
//! key. The identity token is stored as `refresh` and the minted key as
//! `access`, so the refresh scheduler re-mints the key. A 401/403 from mint
//! means the identity session is dead and the user must sign in again.
use crate::oauth_login::{
    js_stringify, now_ms, poll_device_code_flow, search_params, AuthEvent, DevicePoll, Interaction,
    OAuthCredentials,
};
use serde_json::Value;
use std::time::Duration;

const CLIENT_ID: &str = "1031625952748946";
const API_KEY_LIFETIME_MS: i64 = 24 * 60 * 60 * 1000;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct Endpoints {
    pub device_authorization_url: String,
    pub device_token_url: String,
    pub api_key_mint_url: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            device_authorization_url: "https://auth.meta.com/oidc/device/authorization/".into(),
            device_token_url: "https://auth.meta.com/oidc/device/token/".into(),
            api_key_mint_url: "https://api.meta.ai/muse-code/key".into(),
        }
    }
}

async fn read_json(response: reqwest::Response) -> Option<Value> {
    response
        .json::<Value>()
        .await
        .ok()
        .filter(|v| v.is_object() || v.is_array())
}

fn error_detail(json: Option<&Value>) -> String {
    for key in ["error_description", "detail", "message", "error"] {
        if let Some(value) = json
            .and_then(|j| j[key].as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty())
        {
            return format!(": {value}");
        }
    }
    String::new()
}

fn trusted_http_url(value: &Value) -> Option<String> {
    let url = url::Url::parse(value.as_str().filter(|v| !v.is_empty())?).ok()?;
    matches!(url.scheme(), "http" | "https").then(|| url.to_string())
}

fn positive(value: &Value) -> Option<f64> {
    value.as_f64().filter(|v| v.is_finite() && *v > 0.0)
}

struct Device {
    device_code: String,
    user_code: String,
    verification_uri: String,
    interval: Option<f64>,
    expires_in: Option<f64>,
}

async fn start_device_authorization(
    client: &reqwest::Client,
    endpoints: &Endpoints,
) -> Result<Device, String> {
    let response = client
        .post(&endpoints.device_authorization_url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .body(search_params(&[("client_id", CLIENT_ID)]))
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = response.status();
    let json = read_json(response).await;
    if !status.is_success() {
        return Err(format!(
            "Meta device authorization failed with status {}{}",
            status.as_u16(),
            error_detail(json.as_ref())
        ));
    }
    let value = json.clone().unwrap_or(Value::Null);
    let field = |key: &str| {
        value[key]
            .as_str()
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
    };
    let uri = trusted_http_url(&value["verification_uri_complete"])
        .or_else(|| trusted_http_url(&value["verification_uri"]));
    let (Some(device_code), Some(user_code), Some(verification_uri)) =
        (field("device_code"), field("user_code"), uri)
    else {
        return Err(format!(
            "Invalid Meta device authorization response: {}",
            js_stringify(json.as_ref())
        ));
    };
    Ok(Device {
        device_code,
        user_code,
        verification_uri,
        interval: positive(&value["interval"]),
        expires_in: positive(&value["expires_in"]),
    })
}

async fn poll_for_identity_token(
    client: &reqwest::Client,
    endpoints: &Endpoints,
    device: &Device,
) -> Result<String, String> {
    poll_device_code_flow(device.interval, device.expires_in, true, || async {
        let response = client
            .post(&endpoints.device_token_url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Accept", "application/json")
            .body(search_params(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("device_code", &device.device_code),
                ("client_id", CLIENT_ID),
            ]))
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let status = response.status();
        let json = read_json(response).await;
        if status.is_success() {
            if let Some(token) = json
                .as_ref()
                .and_then(|j| j["access_token"].as_str())
                .filter(|v| !v.is_empty())
            {
                return Ok(DevicePoll::Complete(token.to_string()));
            }
        }
        Ok(match json.as_ref().and_then(|j| j["error"].as_str()) {
            Some("authorization_pending") => DevicePoll::Pending,
            Some("slow_down") => {
                DevicePoll::SlowDown(json.as_ref().and_then(|j| positive(&j["interval"])))
            }
            Some("access_denied") => DevicePoll::Failed("Meta login was denied.".into()),
            Some("expired_token") => DevicePoll::Failed(
                "Meta device authorization expired. Please restart login.".into(),
            ),
            _ => DevicePoll::Failed(format!(
                "Meta device token request failed with status {}{}",
                status.as_u16(),
                error_detail(json.as_ref())
            )),
        })
    })
    .await
}

async fn mint_api_key(url: &str, identity: &str) -> Result<OAuthCredentials, String> {
    let response = reqwest::Client::new()
        .post(url)
        .header("Accept", "application/json")
        .header("Authorization", format!("Bearer {identity}"))
        .header("Content-Type", "application/json")
        .header("x-api-version", "1.0.0")
        .body("{}")
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = response.status().as_u16();
    let json = read_json(response).await;
    if status == 401 || status == 403 {
        return Err(format!(
            "Meta session expired (status {status}). Run `/login meta` to sign in again.{}",
            error_detail(json.as_ref())
        ));
    }
    if !(200..300).contains(&status) {
        return Err(format!(
            "Meta API key mint failed with status {status}{}",
            error_detail(json.as_ref())
        ));
    }
    let value = json.unwrap_or(Value::Null);
    let Some(key) = value["api_key"].as_str().filter(|v| !v.is_empty()) else {
        return Err(match trusted_http_url(&value["action_url"]) {
            Some(url) => format!("Meta did not issue an API key. Complete setup at {url}"),
            None => "Meta did not issue an API key.".into(),
        });
    };
    Ok(OAuthCredentials::new(
        key,
        identity,
        now_ms() + API_KEY_LIFETIME_MS,
    ))
}

pub async fn login(
    interaction: Interaction,
    endpoints: Endpoints,
) -> Result<OAuthCredentials, String> {
    let client = reqwest::Client::new();
    let result = async {
        let device = start_device_authorization(&client, &endpoints).await?;
        interaction.notify(AuthEvent::DeviceCode {
            user_code: device.user_code.clone(),
            verification_uri: device.verification_uri.clone(),
        });
        let identity = poll_for_identity_token(&client, &endpoints, &device).await?;
        interaction.notify(AuthEvent::Progress(
            "Enabling Meta Model API access...".into(),
        ));
        mint_api_key(&endpoints.api_key_mint_url, &identity).await
    }
    .await;
    result.map_err(|error| {
        if interaction.is_aborted() {
            "Login cancelled".into()
        } else {
            error
        }
    })
}

/// Exchange an identity token for a Model API key: (access, refresh, expires).
pub async fn mint(identity: &str) -> Result<(String, String, i64), String> {
    let credentials = mint_api_key(&Endpoints::default().api_key_mint_url, identity).await?;
    Ok((credentials.access, credentials.refresh, credentials.expires))
}
