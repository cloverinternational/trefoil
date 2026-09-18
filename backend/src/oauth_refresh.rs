//! Safe, provider-agnostic OAuth access-token refresh scheduling.
//!
//! Refreshes happen before expiry, are serialized per provider/account, and
//! only replace credentials after a complete, validated response is received.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::state::AppState;
use serde_json::Value;

/// Refresh this far ahead of expiry so requests do not race token expiry.
pub const REFRESH_LEAD_TIME: Duration = Duration::from_secs(5 * 60);
pub const POLL_INTERVAL: Duration = Duration::from_secs(60);

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn endpoint(provider: &str) -> Option<&'static str> {
    match provider {
        "anthropic" => Some("https://console.anthropic.com/v1/oauth/token"),
        "github-copilot" => Some("https://github.com/login/oauth/access_token"),
        "kimi-coding" => Some("https://auth.kimi.com/oauth/token"),
        "openai-codex" => Some("https://auth.openai.com/oauth/token"),
        "xai" => Some("https://accounts.x.ai/oauth/token"),
        _ => None,
    }
}

async fn refresh_one(state: &AppState, provider: String, account: String, refresh_token: String) {
    let Some(_url) = endpoint(&provider) else {
        return;
    };
    if refresh_token.is_empty() {
        return;
    }
    let key = format!("{provider}\0{account}");
    let gate = {
        let mut locks = state.inner.oauth_refresh_locks.lock().unwrap();
        locks
            .entry(key)
            .or_insert_with(|| std::sync::Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    };
    let _guard = gate.lock().await;
    let transport = crate::oauth_transport::ReqwestTransport;
    let Ok(response) = crate::oauth_transport::refresh(&transport, &provider, &refresh_token).await
    else {
        return;
    };
    let Ok(token) = parse_refresh_response(
        &serde_json::json!({
            "access_token": response.access_token,
            "refresh_token": response.refresh_token,
            "expires_in": response.expires_in,
        }),
        &refresh_token,
        now_ms(),
    ) else {
        return;
    };
    if let Some(pool) = &state.inner.database {
        let _ = crate::database::upsert_oauth_credential(
            pool,
            &provider,
            &account,
            &token.access_token,
            &token.refresh_token,
            token.expires_at_ms,
        )
        .await;
    }
}

pub fn start_scheduler(state: AppState) {
    tokio::spawn(async move {
        loop {
            if let Some(pool) = &state.inner.database {
                if let Ok(credentials) = crate::database::load_oauth_credentials(pool).await {
                    let now = now_ms();
                    for (provider, account, _access, refresh, expires) in credentials {
                        if refresh_due(now, expires) {
                            refresh_one(&state, provider, account, refresh).await;
                        }
                    }
                }
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    });
}

pub fn refresh_due(now_ms: i64, expires_at_ms: i64) -> bool {
    expires_at_ms <= now_ms.saturating_add(REFRESH_LEAD_TIME.as_millis() as i64)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshedToken {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at_ms: i64,
}

/// Parse without accepting malformed or non-expiring responses. The old
/// refresh token is retained when a provider rotates only its access token.
pub fn parse_refresh_response(
    payload: &Value,
    old_refresh_token: &str,
    now_ms: i64,
) -> Result<RefreshedToken, &'static str> {
    let access_token = payload
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or("refresh response omitted access token")?;
    let expires_in = payload
        .get("expires_in")
        .and_then(Value::as_i64)
        .filter(|value| *value > 0)
        .ok_or("refresh response omitted valid expiry")?;
    if old_refresh_token.is_empty() {
        return Err("refresh token is unavailable");
    }
    let expires_at_ms = now_ms
        .checked_add(
            expires_in
                .checked_mul(1000)
                .ok_or("refresh expiry is too large")?,
        )
        .ok_or("refresh expiry is too large")?;
    Ok(RefreshedToken {
        access_token: access_token.to_owned(),
        refresh_token: payload
            .get("refresh_token")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(old_refresh_token)
            .to_owned(),
        expires_at_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedules_only_inside_lead_time() {
        assert!(!refresh_due(0, 301_000));
        assert!(refresh_due(0, 300_000));
        assert!(refresh_due(0, -1));
    }

    #[test]
    fn validates_response_and_retains_rotating_token() {
        let token = parse_refresh_response(
            &serde_json::json!({"access_token":"new","expires_in":3600}),
            "old",
            100,
        )
        .unwrap();
        assert_eq!(token.refresh_token, "old");
        assert_eq!(token.expires_at_ms, 3_600_100);
        assert!(
            parse_refresh_response(&serde_json::json!({"access_token":"new"}), "old", 0).is_err()
        );
        assert!(parse_refresh_response(
            &serde_json::json!({"access_token":"secret","expires_in":1}),
            "",
            0
        )
        .is_err());
        assert!(parse_refresh_response(
            &serde_json::json!({"access_token":"new","expires_in":i64::MAX}),
            "old",
            0
        )
        .is_err());
        assert!(parse_refresh_response(
            &serde_json::json!({"access_token":"new","expires_in":1}),
            "old",
            i64::MAX
        )
        .is_err());
    }

    #[test]
    fn rotation_is_atomic_from_the_parser_callers_point_of_view() {
        let old = "old-refresh";
        let malformed = [
            serde_json::json!({"access_token":"new","expires_in":3600}),
            serde_json::json!({"access_token":"new","expires_in":0}),
            serde_json::json!({"access_token":"new","expires_in":-1}),
            serde_json::json!({"access_token":"new","expires_in":"3600"}),
            serde_json::json!({"access_token":"   ","expires_in":3600}),
            serde_json::json!({"access_token":null,"expires_in":3600}),
        ];

        // A rejected response must not manufacture replacement state. The
        // scheduler consequently leaves the persisted credential untouched.
        for payload in malformed {
            let result = parse_refresh_response(&payload, old, 1_000);
            if payload.get("access_token").and_then(Value::as_str) == Some("new")
                && payload.get("expires_in").and_then(Value::as_i64) == Some(3600)
            {
                assert!(result.is_ok());
                assert_eq!(result.unwrap().refresh_token, old);
            } else {
                assert!(result.is_err(), "accepted malformed payload: {payload}");
            }
        }
    }

    #[test]
    fn provider_rotation_replaces_only_a_nonempty_refresh_token() {
        let rotated = parse_refresh_response(
            &serde_json::json!({
                "access_token": "new-access",
                "refresh_token": "new-refresh",
                "expires_in": 3600
            }),
            "old-refresh",
            1_000,
        )
        .unwrap();
        assert_eq!(rotated.refresh_token, "new-refresh");

        for refresh_token in ["", "   "] {
            let retained = parse_refresh_response(
                &serde_json::json!({
                    "access_token": "new-access",
                    "refresh_token": refresh_token,
                    "expires_in": 3600
                }),
                "old-refresh",
                1_000,
            )
            .unwrap();
            assert_eq!(retained.refresh_token, "old-refresh");
        }
    }
}
