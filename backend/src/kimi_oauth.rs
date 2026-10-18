//! Kimi Code (subscription) login — port of pinned pi-ai
//! `auth/oauth/kimi-coding.js` (RFC 8628 device grant, JSON responses).
use crate::oauth_login::{js_stringify, now_ms, poll_device_code_flow, search_params, AuthEvent, DevicePoll, Interaction, OAuthCredentials};
use serde_json::Value;
use std::time::Duration;

const CLIENT_ID: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";
const DEFAULT_OAUTH_HOST: &str = "https://auth.kimi.com";
const DEVICE_CODE_TIMEOUT_SECONDS: f64 = 15.0 * 60.0;
const DEFAULT_POLL_INTERVAL_SECONDS: f64 = 5.0;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const REFRESH_MAX_RETRIES: u32 = 3;

#[derive(Debug, Clone, Default)]
pub struct Endpoints {
    /// Overrides the OAuth host (else `KIMI_CODE_OAUTH_HOST` / `KIMI_OAUTH_HOST` / default).
    pub oauth_host: Option<String>,
}

/// pi-ai `getOauthHost`.
fn oauth_host(endpoints: &Endpoints) -> String {
    let host = endpoints
        .oauth_host
        .clone()
        .or_else(|| std::env::var("KIMI_CODE_OAUTH_HOST").ok().filter(|v| !v.is_empty()))
        .or_else(|| std::env::var("KIMI_OAUTH_HOST").ok().filter(|v| !v.is_empty()))
        .unwrap_or_else(|| DEFAULT_OAUTH_HOST.into());
    host.trim_end_matches('/').to_string()
}

fn trusted_http_url(value: &str) -> bool {
    !value.is_empty() && url::Url::parse(value).is_ok_and(|u| matches!(u.scheme(), "http" | "https"))
}

async fn read_json(response: reqwest::Response) -> Option<Value> {
    response.json::<Value>().await.ok().filter(|v| v.is_object() || v.is_array())
}

fn post(client: &reqwest::Client, url: String, fields: &[(&str, &str)]) -> reqwest::RequestBuilder {
    client
        .post(url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .body(search_params(fields))
        .timeout(REQUEST_TIMEOUT)
}

struct Device {
    device_code: String,
    user_code: String,
    verification_uri_complete: String,
    interval: f64,
    expires_in: f64,
}

fn positive(value: &Value) -> Option<f64> {
    value.as_f64().filter(|v| v.is_finite() && *v > 0.0)
}

async fn start_device_authorization(client: &reqwest::Client, host: &str) -> Result<Device, String> {
    let response = post(client, format!("{host}/api/oauth/device_authorization"), &[("client_id", CLIENT_ID)]).send().await.map_err(|e| e.to_string())?;
    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        let suffix = if text.is_empty() { String::new() } else { format!(": {text}") };
        return Err(format!("Kimi Code device authorization failed with status {}{suffix}", status.as_u16()));
    }
    let json = read_json(response).await;
    let field = |key: &str| json.as_ref().and_then(|j| j[key].as_str()).map(str::to_owned);
    let (Some(device_code), Some(user_code), Some(verification_uri), Some(complete)) = (field("device_code"), field("user_code"), field("verification_uri"), field("verification_uri_complete")) else {
        return Err(format!("Invalid Kimi Code device authorization response: {}", js_stringify(json.as_ref())));
    };
    if !trusted_http_url(&complete) || !trusted_http_url(&verification_uri) {
        return Err(format!("Invalid Kimi Code device authorization response: {}", js_stringify(json.as_ref())));
    }
    let json = json.unwrap_or(Value::Null);
    Ok(Device {
        device_code,
        user_code,
        verification_uri_complete: complete,
        interval: positive(&json["interval"]).unwrap_or(DEFAULT_POLL_INTERVAL_SECONDS),
        expires_in: positive(&json["expires_in"]).unwrap_or(DEVICE_CODE_TIMEOUT_SECONDS),
    })
}

/// pi-ai `parseTokenResponse`: (access, refresh, expires).
fn parse_token_response(json: Option<&Value>, operation: &str) -> Result<(String, String, i64), String> {
    let access = json.and_then(|j| j["access_token"].as_str()).filter(|v| !v.is_empty());
    let refresh = json.and_then(|j| j["refresh_token"].as_str()).filter(|v| !v.is_empty());
    let expires_in = json.and_then(|j| positive(&j["expires_in"]));
    match (access, refresh, expires_in) {
        (Some(access), Some(refresh), Some(expires_in)) => Ok((access.into(), refresh.into(), now_ms() + (expires_in * 1000.0) as i64)),
        _ => Err(format!("Kimi Code token {operation} response missing fields: {}", js_stringify(json))),
    }
}

async fn poll_for_token(client: &reqwest::Client, host: &str, device: &Device) -> Result<(String, String, i64), String> {
    poll_device_code_flow(Some(device.interval), Some(device.expires_in), true, || async {
        let response = post(client, format!("{host}/api/oauth/token"), &[("client_id", CLIENT_ID), ("device_code", &device.device_code), ("grant_type", "urn:ietf:params:oauth:grant-type:device_code")])
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let status = response.status();
        if status.as_u16() >= 500 {
            let text = response.text().await.unwrap_or_default();
            let suffix = if text.is_empty() { String::new() } else { format!(": {text}") };
            return Ok(DevicePoll::Failed(format!("Kimi Code device token request failed with status {}{suffix}", status.as_u16())));
        }
        let json = read_json(response).await;
        if status.is_success() && json.as_ref().is_some_and(|j| j["access_token"].is_string()) {
            return Ok(match parse_token_response(json.as_ref(), "poll") {
                Ok(token) => DevicePoll::Complete(token),
                Err(message) => DevicePoll::Failed(message),
            });
        }
        let error = json.as_ref().and_then(|j| j["error"].as_str()).map(str::to_owned);
        let description = json.as_ref().and_then(|j| j["error_description"].as_str()).map(|d| format!(": {d}")).unwrap_or_default();
        Ok(match error.as_deref() {
            Some("authorization_pending") => DevicePoll::Pending,
            Some("slow_down") => DevicePoll::SlowDown(json.as_ref().and_then(|j| j["interval"].as_f64()).filter(|v| *v > 0.0)),
            Some("expired_token") => DevicePoll::Failed("Kimi Code device authorization expired. Please restart login.".into()),
            Some("access_denied") => DevicePoll::Failed("Kimi Code login was denied.".into()),
            other => DevicePoll::Failed(format!(
                "Kimi Code device token request failed (status {}){}",
                status.as_u16(),
                other.map(|e| format!(": {e}{description}")).unwrap_or_default()
            )),
        })
    })
    .await
}

pub async fn login(interaction: Interaction, endpoints: Endpoints) -> Result<OAuthCredentials, String> {
    let host = oauth_host(&endpoints);
    let client = reqwest::Client::new();
    let device = start_device_authorization(&client, &host).await?;
    interaction.notify(AuthEvent::DeviceCode { user_code: device.user_code.clone(), verification_uri: device.verification_uri_complete.clone() });
    let (access, refresh, expires) = poll_for_token(&client, &host, &device).await?;
    Ok(OAuthCredentials::new(access, refresh, expires))
}

/// pi-ai `refreshToken` (retries 429/5xx and network errors).
pub async fn refresh(refresh_token: &str) -> Result<(String, String, i64), String> {
    refresh_at(&oauth_host(&Endpoints::default()), refresh_token).await
}

async fn refresh_at(host: &str, refresh_token: &str) -> Result<(String, String, i64), String> {
    let client = reqwest::Client::new();
    let mut last: Option<String> = None;
    for attempt in 0..=REFRESH_MAX_RETRIES {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(1000 * 2u64.pow(attempt - 1))).await;
        }
        let response = match post(&client, format!("{host}/api/oauth/token"), &[("client_id", CLIENT_ID), ("grant_type", "refresh_token"), ("refresh_token", refresh_token)]).send().await {
            Ok(response) => response,
            Err(error) => {
                last = Some(error.to_string());
                continue;
            }
        };
        let status = response.status();
        let json = read_json(response).await;
        if status.is_success() {
            return parse_token_response(json.as_ref(), "refresh");
        }
        if status.as_u16() == 401 || status.as_u16() == 403 || json.as_ref().is_some_and(|j| j["error"] == "invalid_grant") {
            let description = json.as_ref().and_then(|j| j["error_description"].as_str()).map(|d| format!(": {d}")).unwrap_or_default();
            return Err(format!("Kimi Code token refresh unauthorized (status {}){description}", status.as_u16()));
        }
        if (status.as_u16() == 429 || status.as_u16() >= 500) && attempt < REFRESH_MAX_RETRIES {
            last = Some(format!("Kimi Code token refresh failed with status {}", status.as_u16()));
            continue;
        }
        return Err(format!("Kimi Code token refresh failed with status {}: {}", status.as_u16(), js_stringify(json.as_ref())));
    }
    Err(last.unwrap_or_else(|| "Kimi Code token refresh failed".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Bytes, http::StatusCode, routing::post, Json, Router};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    #[test]
    fn host_override_strips_trailing_slashes() {
        assert_eq!(oauth_host(&Endpoints { oauth_host: Some("http://h//".into()) }), "http://h");
    }

    #[tokio::test]
    async fn refresh_rotates_token_retries_transient_and_rejects_invalid_grant() {
        let hits = Arc::new(AtomicUsize::new(0));
        let count = hits.clone();
        let app = Router::new()
            .route(
                "/api/oauth/token",
                post(move |body: Bytes| {
                    let count = count.clone();
                    async move {
                        let fields: std::collections::HashMap<_, _> = url::form_urlencoded::parse(&body).into_owned().collect();
                        assert_eq!(fields["grant_type"], "refresh_token");
                        if fields["refresh_token"] == "dead" {
                            return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error":"invalid_grant","error_description":"gone"})));
                        }
                        if count.fetch_add(1, Ordering::SeqCst) == 0 {
                            (StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({"error":"busy"})))
                        } else {
                            (StatusCode::OK, Json(serde_json::json!({"access_token":"new","refresh_token":"rotated","expires_in":3600})))
                        }
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let (access, refresh, _) = refresh_at(&format!("http://{addr}"), "old").await.unwrap();
        assert_eq!((access.as_str(), refresh.as_str()), ("new", "rotated"));
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        assert_eq!(refresh_at(&format!("http://{addr}"), "dead").await.unwrap_err(), "Kimi Code token refresh unauthorized (status 400): gone");
        server.abort();
    }
}
