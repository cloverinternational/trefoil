//! xAI (Grok/X subscription) login — port of pinned pi-ai `auth/oauth/xai.js`
//! (device-code grant, HTTPS-only verification URIs).
use crate::oauth_login::{
    now_ms, poll_device_code_flow, search_params, AuthEvent, DevicePoll, Interaction,
    OAuthCredentials,
};
use serde_json::{Map, Value};

const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";
const REFRESH_SKEW_MS: i64 = 5 * 60 * 1000;
const DEFAULT_TOKEN_LIFETIME_SECONDS: f64 = 3600.0;

#[derive(Debug, Clone)]
pub struct Endpoints {
    pub device_code_url: String,
    pub token_url: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            device_code_url: "https://auth.x.ai/oauth2/device/code".into(),
            token_url: "https://auth.x.ai/oauth2/token".into(),
        }
    }
}

fn required_string(body: &Map<String, Value>, field: &str) -> Result<String, String> {
    body.get(field)
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("Invalid xAI OAuth response field: {field}"))
}

fn positive_number(body: &Map<String, Value>, field: &str) -> Result<f64, String> {
    body.get(field)
        .and_then(Value::as_f64)
        .filter(|v| v.is_finite() && *v > 0.0)
        .ok_or_else(|| format!("Invalid xAI OAuth response field: {field}"))
}

fn validate_verification_uri(raw: &str) -> Result<String, String> {
    let url = url::Url::parse(raw)
        .map_err(|_| "Untrusted verification URI in xAI OAuth response".to_string())?;
    if url.scheme() != "https" {
        return Err("Untrusted verification URI in xAI OAuth response".into());
    }
    Ok(url.to_string())
}

struct FormResponse {
    ok: bool,
    status: u16,
    body: Map<String, Value>,
}

async fn post_form(url: &str, fields: &[(&str, &str)]) -> Result<FormResponse, String> {
    let response = reqwest::Client::new()
        .post(url)
        .header("Accept", "application/json")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(search_params(fields))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = response.status();
    let body = match response.json::<Value>().await {
        Ok(Value::Object(map)) => map,
        Ok(_) => Map::new(),
        Err(_) => {
            return Err(format!(
                "xAI OAuth returned invalid JSON (HTTP {})",
                status.as_u16()
            ))
        }
    };
    Ok(FormResponse {
        ok: status.is_success(),
        status: status.as_u16(),
        body,
    })
}

fn request_failure(action: &str, response: &FormResponse) -> String {
    let detail = [
        response.body.get("error").and_then(Value::as_str),
        response
            .body
            .get("error_description")
            .and_then(Value::as_str),
    ]
    .into_iter()
    .flatten()
    .filter(|v| !v.is_empty())
    .collect::<Vec<_>>()
    .join(": ");
    format!(
        "xAI OAuth {action} failed (HTTP {}){}",
        response.status,
        if detail.is_empty() {
            String::new()
        } else {
            format!(": {detail}")
        }
    )
}

struct Device {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: Option<String>,
    interval: Option<f64>,
    expires_in: f64,
}

fn parse_device_code(body: &Map<String, Value>) -> Result<Device, String> {
    let interval = body
        .get("interval")
        .and_then(Value::as_f64)
        .filter(|v| v.is_finite() && *v > 0.0);
    let complete = match body
        .get("verification_uri_complete")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
    {
        Some(raw) => Some(validate_verification_uri(raw)?),
        None => None,
    };
    let device_code = required_string(body, "device_code")?;
    let user_code = required_string(body, "user_code")?;
    let verification_uri = validate_verification_uri(&required_string(body, "verification_uri")?)?;
    let expires_in = positive_number(body, "expires_in")?;
    Ok(Device {
        device_code,
        user_code,
        verification_uri,
        verification_uri_complete: complete,
        interval,
        expires_in,
    })
}

/// pi-ai `credentialsFromTokenResponse`.
fn credentials_from_token_response(
    body: &Map<String, Value>,
    previous_refresh: Option<&str>,
) -> Result<OAuthCredentials, String> {
    let access = required_string(body, "access_token")?;
    let refresh = match (
        body.get("refresh_token"),
        previous_refresh.filter(|v| !v.is_empty()),
    ) {
        (None, Some(previous)) => previous.to_string(),
        _ => required_string(body, "refresh_token")?,
    };
    let expires_in = if body.contains_key("expires_in") {
        positive_number(body, "expires_in")?
    } else {
        DEFAULT_TOKEN_LIFETIME_SECONDS
    };
    Ok(OAuthCredentials::new(
        access,
        refresh,
        now_ms() + (expires_in * 1000.0) as i64 - REFRESH_SKEW_MS,
    ))
}

pub async fn login(
    interaction: Interaction,
    endpoints: Endpoints,
) -> Result<OAuthCredentials, String> {
    let response = post_form(
        &endpoints.device_code_url,
        &[
            ("client_id", CLIENT_ID),
            ("scope", SCOPE),
            ("referrer", "pi"),
        ],
    )
    .await?;
    if !response.ok {
        return Err(request_failure("device authorization", &response));
    }
    let device = parse_device_code(&response.body)?;
    interaction.notify(AuthEvent::DeviceCode {
        user_code: device.user_code.clone(),
        verification_uri: device
            .verification_uri_complete
            .clone()
            .unwrap_or_else(|| device.verification_uri.clone()),
    });
    let token_url = endpoints.token_url.clone();
    poll_device_code_flow(device.interval, Some(device.expires_in), true, || async {
        let response = post_form(
            &token_url,
            &[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("client_id", CLIENT_ID),
                ("device_code", &device.device_code),
            ],
        )
        .await?;
        if response.ok {
            return credentials_from_token_response(&response.body, None).map(DevicePoll::Complete);
        }
        Ok(match response.body.get("error").and_then(Value::as_str) {
            Some("authorization_pending") => DevicePoll::Pending,
            Some("slow_down") => {
                DevicePoll::SlowDown(response.body.get("interval").and_then(Value::as_f64))
            }
            Some("access_denied") | Some("authorization_denied") => {
                DevicePoll::Failed("xAI device authorization was denied".into())
            }
            Some("expired_token") => DevicePoll::Failed("xAI device code expired".into()),
            _ => DevicePoll::Failed(request_failure("device token polling", &response)),
        })
    })
    .await
}

/// pi-ai `refreshXaiToken`: (access, refresh, expires).
pub async fn refresh(token: &str) -> Result<(String, String, i64), String> {
    refresh_at(&Endpoints::default().token_url, token).await
}

async fn refresh_at(url: &str, token: &str) -> Result<(String, String, i64), String> {
    let response = post_form(
        url,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", CLIENT_ID),
            ("refresh_token", token),
        ],
    )
    .await?;
    if !response.ok {
        return Err(request_failure("token refresh", &response));
    }
    let credentials = credentials_from_token_response(&response.body, Some(token))?;
    Ok((credentials.access, credentials.refresh, credentials.expires))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Bytes, routing::post, Json, Router};

    #[test]
    fn rejects_non_https_and_retains_refresh_when_omitted() {
        assert!(validate_verification_uri("http://x.ai").is_err());
        let body = serde_json::json!({"access_token":"a"});
        let credentials =
            credentials_from_token_response(body.as_object().unwrap(), Some("old")).unwrap();
        assert_eq!(
            (credentials.access.as_str(), credentials.refresh.as_str()),
            ("a", "old")
        );
        assert_eq!(
            credentials_from_token_response(body.as_object().unwrap(), None).unwrap_err(),
            "Invalid xAI OAuth response field: refresh_token"
        );
    }

    #[tokio::test]
    async fn refresh_form_follows_contract() {
        let app = Router::new().route(
            "/refresh",
            post(|body: Bytes| async move {
                let f: std::collections::HashMap<_, _> =
                    url::form_urlencoded::parse(&body).into_owned().collect();
                assert_eq!(f["grant_type"], "refresh_token");
                assert_eq!(f["refresh_token"], "old");
                Json(serde_json::json!({"access_token":"new","expires_in":3600}))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let (a, r, _) = refresh_at(&format!("http://{addr}/refresh"), "old")
            .await
            .unwrap();
        assert_eq!((a.as_str(), r.as_str()), ("new", "old"));
        server.abort();
    }
}
