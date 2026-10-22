//! OpenAI Codex OAuth request identity.
//!
//! ChatGPT's Codex backend expects the same CLI fingerprint as the native
//! Codex client in addition to the bearer token.  Keep this transformation
//! separate from request-shape translation so it can be reused by every
//! Codex-native endpoint.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde_json::Value;

pub const CODEX_VERSION: &str = "0.155.1";

pub fn user_agent() -> String {
    format!(
        "codex_cli_rs/{} (Debian 13.0.0; x86_64) WindowsTerminal",
        crate::cli_versions::codex_version()
    )
}

/// Extract the ChatGPT account claim from a JWT access token, if present.
/// Malformed or non-JWT tokens intentionally produce no account header.
pub fn account_id(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    value
        .get("https://api.openai.com/auth")
        .and_then(|auth| auth.get("chatgpt_account_id"))
        .or_else(|| value.get("chatgpt_account_id"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_nested_account_claim() {
        let token = "eyJhbGciOiJub25lIn0.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjdC0xMjMifX0.sig";
        assert_eq!(account_id(token).as_deref(), Some("acct-123"));
    }

    #[test]
    fn ignores_malformed_and_missing_claims() {
        assert_eq!(account_id("not-a-jwt"), None);
        assert_eq!(account_id("a.eyJmb28iOiJiYXIifQ.c"), None);
    }

    #[test]
    fn uses_codex_cli_identity() {
        assert_eq!(
            user_agent(),
            "codex_cli_rs/0.155.1 (Debian 13.0.0; x86_64) WindowsTerminal"
        );
    }
}
