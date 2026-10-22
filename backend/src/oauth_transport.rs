//! Provider-neutral OAuth transport and response validation.
//!
//! The scheduler and HTTP handlers can use the real reqwest adapter, while
//! callers (and tests) can inject a transport without opening a socket.

use std::{future::Future, pin::Pin, time::Duration};

use serde_json::Value;
use thiserror::Error;

pub const PROVIDERS: [&str; 6] = [
    "anthropic",
    "github-copilot",
    "kimi-coding",
    "openai-codex",
    "openrouter",
    "xai",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenField {
    AccessToken,
    Key,
}

#[derive(Debug, Clone, Copy)]
pub struct ProviderSpec {
    pub id: &'static str,
    pub token_endpoint: &'static str,
    pub token_field: TokenField,
    pub client_id: &'static str,
}

pub fn provider_spec(provider: &str) -> Option<ProviderSpec> {
    Some(match provider {
        "anthropic" => ProviderSpec {
            id: "anthropic",
            token_endpoint: "https://platform.claude.com/v1/oauth/token",
            token_field: TokenField::AccessToken,
            client_id: "9d1c250a-e61b-44d9-88ed-5944d1962f5e",
        },
        "github-copilot" => ProviderSpec {
            id: "github-copilot",
            token_endpoint: "https://github.com/login/oauth/access_token",
            token_field: TokenField::AccessToken,
            client_id: "Iv1.b507a08c87ecfe98",
        },
        "kimi-coding" => ProviderSpec {
            id: "kimi-coding",
            token_endpoint: "https://auth.kimi.com/api/oauth/token",
            token_field: TokenField::AccessToken,
            client_id: "17e5f671-d194-4dfb-9706-5516cb48c098",
        },
        "openai-codex" => ProviderSpec {
            id: "openai-codex",
            token_endpoint: "https://auth.openai.com/oauth/token",
            token_field: TokenField::AccessToken,
            client_id: "app_EMoamEEZ73f0CkXaXp7hrann",
        },
        "openrouter" => ProviderSpec {
            id: "openrouter",
            token_endpoint: "https://openrouter.ai/api/v1/auth/keys",
            token_field: TokenField::Key,
            client_id: "",
        },
        "xai" => ProviderSpec {
            id: "xai",
            token_endpoint: "https://accounts.x.ai/oauth/token",
            token_field: TokenField::AccessToken,
            client_id: "b1a00492-073a-47ea-816f-4c329264a828",
        },
        _ => return None,
    })
}

#[derive(Debug, Clone)]
pub struct TokenResponse {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: i64,
}

#[derive(Debug, Error)]
pub enum OAuthTransportError {
    #[error("unsupported OAuth provider")]
    UnsupportedProvider,
    #[error("OAuth transport failed")]
    Transport,
    #[error("OAuth request timed out")]
    Timeout,
    #[error("OAuth endpoint returned HTTP {status}")]
    Http { status: u16 },
    #[error("OAuth endpoint returned malformed JSON")]
    Json,
    #[error("OAuth response omitted required token fields")]
    MissingFields,
}

#[derive(Debug, Clone)]
pub struct OAuthRequest {
    pub provider: String,
    pub url: String,
    pub fields: Vec<(String, String)>,
    pub json: bool,
    pub timeout: Duration,
}

#[derive(Debug, Clone)]
pub struct OAuthResponse {
    pub status: u16,
    pub body: Value,
}

pub type TransportFuture =
    Pin<Box<dyn Future<Output = Result<OAuthResponse, OAuthTransportError>> + Send>>;
pub trait OAuthTransport: Send + Sync {
    fn send(&self, request: OAuthRequest) -> TransportFuture;
}

pub async fn refresh<T: OAuthTransport + ?Sized>(
    transport: &T,
    provider: &str,
    refresh_token: &str,
) -> Result<TokenResponse, OAuthTransportError> {
    let spec = provider_spec(provider).ok_or(OAuthTransportError::UnsupportedProvider)?;
    if refresh_token.trim().is_empty() {
        return Err(OAuthTransportError::MissingFields);
    }
    let response = transport
        .send(OAuthRequest {
            provider: provider.to_owned(),
            url: spec.token_endpoint.to_owned(),
            fields: vec![
                ("grant_type".into(), "refresh_token".into()),
                ("refresh_token".into(), refresh_token.into()),
                ("client_id".into(), spec.client_id.into()),
            ],
            json: provider == "anthropic",
            timeout: Duration::from_secs(15),
        })
        .await?;
    if !(200..300).contains(&response.status) {
        return Err(OAuthTransportError::Http {
            status: response.status,
        });
    }
    parse_token_response(&response.body, spec.token_field, refresh_token)
}

pub async fn exchange<T: OAuthTransport + ?Sized>(
    transport: &T,
    provider: &str,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<TokenResponse, OAuthTransportError> {
    exchange_with_state(transport, provider, code, verifier, redirect_uri, None).await
}

pub async fn exchange_with_state<T: OAuthTransport + ?Sized>(
    transport: &T,
    provider: &str,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
    state: Option<&str>,
) -> Result<TokenResponse, OAuthTransportError> {
    let spec = provider_spec(provider).ok_or(OAuthTransportError::UnsupportedProvider)?;
    let (mut fields, json) = if provider == "openrouter" {
        (
            vec![
                ("code".into(), code.into()),
                ("code_verifier".into(), verifier.into()),
                ("code_challenge_method".into(), "S256".into()),
            ],
            true,
        )
    } else {
        (
            vec![
                ("grant_type".into(), "authorization_code".into()),
                ("code".into(), code.into()),
                ("client_id".into(), spec.client_id.into()),
                ("redirect_uri".into(), redirect_uri.into()),
                ("code_verifier".into(), verifier.into()),
            ],
            provider == "anthropic",
        )
    };
    if provider == "anthropic" {
        let state = state
            .filter(|state| !state.is_empty())
            .ok_or(OAuthTransportError::MissingFields)?;
        fields.push(("state".into(), state.into()));
    }
    let response = transport
        .send(OAuthRequest {
            provider: provider.to_owned(),
            url: spec.token_endpoint.to_owned(),
            fields,
            json,
            timeout: Duration::from_secs(15),
        })
        .await?;
    if !(200..300).contains(&response.status) {
        return Err(OAuthTransportError::Http {
            status: response.status,
        });
    }
    if provider == "openrouter" {
        parse_openrouter_response(&response.body)
    } else {
        parse_token_response(&response.body, spec.token_field, "")
    }
}

fn parse_openrouter_response(body: &Value) -> Result<TokenResponse, OAuthTransportError> {
    let access_token = body
        .get("key")
        .and_then(Value::as_str)
        .filter(|v| !v.trim().is_empty())
        .ok_or(OAuthTransportError::MissingFields)?;
    Ok(TokenResponse {
        access_token: access_token.to_owned(),
        refresh_token: String::new(),
        expires_in: 0,
    })
}

pub fn parse_token_response(
    body: &Value,
    field: TokenField,
    old_refresh: &str,
) -> Result<TokenResponse, OAuthTransportError> {
    let access_token = match field {
        TokenField::AccessToken => body.get("access_token").and_then(Value::as_str),
        TokenField::Key => body.get("key").and_then(Value::as_str),
    }
    .filter(|v| !v.trim().is_empty())
    .ok_or(OAuthTransportError::MissingFields)?;
    let expires_in = body
        .get("expires_in")
        .and_then(Value::as_i64)
        .ok_or(OAuthTransportError::MissingFields)?;
    if expires_in <= 0 {
        return Err(OAuthTransportError::MissingFields);
    }
    Ok(TokenResponse {
        access_token: access_token.to_owned(),
        refresh_token: body
            .get("refresh_token")
            .and_then(Value::as_str)
            .filter(|v| !v.trim().is_empty())
            .unwrap_or(old_refresh)
            .to_owned(),
        expires_in,
    })
}

pub struct ReqwestTransport;
impl OAuthTransport for ReqwestTransport {
    fn send(&self, request: OAuthRequest) -> TransportFuture {
        Box::pin(async move {
            let client = reqwest::Client::new();
            let mut builder = client.post(&request.url).timeout(request.timeout);
            if request.json {
                let body: serde_json::Map<String, Value> = request
                    .fields
                    .into_iter()
                    .map(|(key, value)| (key, Value::String(value)))
                    .collect();
                builder = builder.json(&body);
            } else {
                builder = builder.form(&request.fields);
            }
            let response = builder.send().await.map_err(|e| {
                if e.is_timeout() {
                    OAuthTransportError::Timeout
                } else {
                    OAuthTransportError::Transport
                }
            })?;
            let status = response.status().as_u16();
            let body = response
                .json::<Value>()
                .await
                .map_err(|_| OAuthTransportError::Json)?;
            Ok(OAuthResponse { status, body })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    struct Mock {
        requests: Arc<Mutex<Vec<OAuthRequest>>>,
        response: OAuthResponse,
    }
    impl OAuthTransport for Mock {
        fn send(&self, request: OAuthRequest) -> TransportFuture {
            self.requests.lock().unwrap().push(request);
            let response = self.response.clone();
            Box::pin(async move { Ok(response) })
        }
    }

    #[tokio::test]
    async fn all_six_providers_use_injected_transport() {
        for provider in PROVIDERS {
            let requests = Arc::new(Mutex::new(Vec::new()));
            let field = if provider == "openrouter" {
                "key"
            } else {
                "access_token"
            };
            let mock = Mock {
                requests: requests.clone(),
                response: OAuthResponse {
                    status: 200,
                    body: serde_json::json!({field:"access", "refresh_token":"rotated", "expires_in":3600}),
                },
            };
            let token = refresh(&mock, provider, "old-secret").await.unwrap();
            assert_eq!(token.access_token, "access");
            assert_eq!(requests.lock().unwrap()[0].provider, provider);
        }
    }

    #[tokio::test]
    async fn anthropic_exchange_and_refresh_use_current_json_contract() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let mock = Mock {
            requests: requests.clone(),
            response: OAuthResponse {
                status: 200,
                body: serde_json::json!({"access_token":"access","refresh_token":"refresh","expires_in":3600}),
            },
        };
        exchange_with_state(
            &mock,
            "anthropic",
            "code",
            "verifier",
            "http://localhost:53692/callback",
            Some("state"),
        )
        .await
        .unwrap();
        refresh(&mock, "anthropic", "refresh").await.unwrap();
        let requests = requests.lock().unwrap();
        for request in requests.iter() {
            assert!(request.json);
            assert_eq!(request.url, "https://platform.claude.com/v1/oauth/token");
            assert!(request.fields.contains(&(
                "client_id".into(),
                "9d1c250a-e61b-44d9-88ed-5944d1962f5e".into()
            )));
        }
        assert!(requests[0]
            .fields
            .contains(&("state".into(), "state".into())));
        assert!(requests[0]
            .fields
            .contains(&("code_verifier".into(), "verifier".into())));
        assert!(requests[1]
            .fields
            .contains(&("grant_type".into(), "refresh_token".into())));
    }

    #[tokio::test]
    async fn failures_are_classified_without_secret_details() {
        let mock = Mock {
            requests: Arc::new(Mutex::new(Vec::new())),
            response: OAuthResponse {
                status: 401,
                body: Value::Null,
            },
        };
        let error = refresh(&mock, "xai", "super-secret")
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(error, "OAuth endpoint returned HTTP 401");
        assert!(!error.contains("super-secret"));
    }

    #[tokio::test]
    async fn exchange_uses_injected_transport_and_provider_token_field() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let mock = Mock {
            requests: requests.clone(),
            response: OAuthResponse {
                status: 200,
                body: serde_json::json!({"key":"access-key", "expires_in":3600}),
            },
        };
        let token = exchange(
            &mock,
            "openrouter",
            "authorization-code",
            "pkce-verifier",
            "/callback",
        )
        .await
        .unwrap();
        assert_eq!(token.access_token, "access-key");
        let request = &requests.lock().unwrap()[0];
        assert_eq!(request.url, "https://openrouter.ai/api/v1/auth/keys");
        assert!(request
            .fields
            .iter()
            .any(|(name, value)| name == "code" && value == "authorization-code"));
    }

    #[tokio::test]
    async fn exchange_failure_does_not_expose_code_or_verifier() {
        let mock = Mock {
            requests: Arc::new(Mutex::new(Vec::new())),
            response: OAuthResponse {
                status: 502,
                body: Value::Null,
            },
        };
        let error = exchange_with_state(
            &mock,
            "anthropic",
            "secret-code",
            "secret-verifier",
            "/callback",
            Some("state"),
        )
        .await
        .unwrap_err()
        .to_string();
        assert_eq!(error, "OAuth endpoint returned HTTP 502");
        assert!(!error.contains("secret-code"));
        assert!(!error.contains("secret-verifier"));
    }
}

#[cfg(test)]
mod wire_tests {
    use super::*;
    use axum::{routing::post, Json, Router};
    #[tokio::test]
    async fn openrouter_wire_is_json_and_key_response_needs_no_expiry() {
        let app = Router::new().route("/keys", post(|headers: axum::http::HeaderMap, Json(body): Json<Value>| async move {
            assert_eq!(headers["content-type"], "application/json");
            assert_eq!(body, serde_json::json!({"code":"code","code_verifier":"verifier","code_challenge_method":"S256"}));
            Json(serde_json::json!({"key":"permanent-key"}))
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let response = ReqwestTransport
            .send(OAuthRequest {
                provider: "openrouter".into(),
                url: format!("http://{addr}/keys"),
                json: true,
                fields: vec![
                    ("code".into(), "code".into()),
                    ("code_verifier".into(), "verifier".into()),
                    ("code_challenge_method".into(), "S256".into()),
                ],
                timeout: Duration::from_secs(1),
            })
            .await
            .unwrap();
        server.abort();
        let token = parse_openrouter_response(&response.body).unwrap();
        assert_eq!(token.access_token, "permanent-key");
        assert!(token.refresh_token.is_empty());
        assert_eq!(token.expires_in, 0);
    }
}
