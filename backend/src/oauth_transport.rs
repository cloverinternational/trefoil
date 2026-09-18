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
}

pub fn provider_spec(provider: &str) -> Option<ProviderSpec> {
    Some(match provider {
        "anthropic" => ProviderSpec {
            id: "anthropic",
            token_endpoint: "https://console.anthropic.com/v1/oauth/token",
            token_field: TokenField::AccessToken,
        },
        "github-copilot" => ProviderSpec {
            id: "github-copilot",
            token_endpoint: "https://github.com/login/oauth/access_token",
            token_field: TokenField::AccessToken,
        },
        "kimi-coding" => ProviderSpec {
            id: "kimi-coding",
            token_endpoint: "https://auth.kimi.com/oauth/token",
            token_field: TokenField::AccessToken,
        },
        "openai-codex" => ProviderSpec {
            id: "openai-codex",
            token_endpoint: "https://auth.openai.com/oauth/token",
            token_field: TokenField::AccessToken,
        },
        "openrouter" => ProviderSpec {
            id: "openrouter",
            token_endpoint: "https://openrouter.ai/api/v1/auth/keys",
            token_field: TokenField::Key,
        },
        "xai" => ProviderSpec {
            id: "xai",
            token_endpoint: "https://accounts.x.ai/oauth/token",
            token_field: TokenField::AccessToken,
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
                ("client_id".into(), "trefoil".into()),
            ],
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
    let spec = provider_spec(provider).ok_or(OAuthTransportError::UnsupportedProvider)?;
    let response = transport
        .send(OAuthRequest {
            provider: provider.to_owned(),
            url: spec.token_endpoint.to_owned(),
            fields: vec![
                ("grant_type".into(), "authorization_code".into()),
                ("code".into(), code.into()),
                ("client_id".into(), "trefoil".into()),
                ("redirect_uri".into(), redirect_uri.into()),
                ("code_verifier".into(), verifier.into()),
            ],
            timeout: Duration::from_secs(15),
        })
        .await?;
    if !(200..300).contains(&response.status) {
        return Err(OAuthTransportError::Http {
            status: response.status,
        });
    }
    parse_token_response(&response.body, spec.token_field, "")
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
        .unwrap_or(i64::MAX / 1000);
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
            let response = client
                .post(&request.url)
                .form(&request.fields)
                .timeout(request.timeout)
                .send()
                .await
                .map_err(|e| {
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
        let error = exchange(
            &mock,
            "anthropic",
            "secret-code",
            "secret-verifier",
            "/callback",
        )
        .await
        .unwrap_err()
        .to_string();
        assert_eq!(error, "OAuth endpoint returned HTTP 502");
        assert!(!error.contains("secret-code"));
        assert!(!error.contains("secret-verifier"));
    }
}
