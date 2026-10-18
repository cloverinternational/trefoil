//! `services/dispatch/failover-policy.ts`. Errors are TS-shaped values.
use crate::engine::js::*;
use serde_json::Value;

/// `isRetryableStatus`: 402 always, else membership in the configured list.
pub fn is_retryable_status(status_code: u16, retryable_status_codes: &[u16]) -> bool {
    status_code == 402 || retryable_status_codes.contains(&status_code)
}

/// `isRetryableOAuthError`.
pub fn is_retryable_oauth_error(error: Option<&Value>) -> bool {
    let Some(error) = error.filter(|e| truthy(Some(e))) else { return false };
    let message = get_str(error, &["message"]).map(str::to_lowercase).unwrap_or_default();
    let status = or(error.get("status"), error.get("statusCode"));
    if !truthy(status.as_ref()) { return true; }
    let n = to_number(status.as_ref()).unwrap_or(f64::NAN);
    if (500.0..600.0).contains(&n) || n == 429.0 || n == 402.0 { return true; }
    ["timeout", "econnrefused", "ECONNREFUSED", "etimedout", "ETIMEDOUT", "network", "socket", "temporary", "unavailable", "service unavailable"]
        .iter()
        .any(|p| message.contains(p))
}

/// `isRetryableNetworkError`: `code`/`message` contains any configured token
/// (case-insensitive).
pub fn is_retryable_network_error(error: Option<&Value>, retryable_errors: &[String]) -> bool {
    let Some(error) = error.filter(|e| truthy(Some(e))) else { return false };
    let field = |k: &str| {
        let v = error.get(k).filter(|v| truthy(Some(v)));
        v.map(|v| to_string(Some(v))).unwrap_or_default().to_uppercase()
    };
    let (code, message) = (field("code"), field("message"));
    retryable_errors.iter().any(|t| {
        let n = t.to_uppercase();
        code.contains(&n) || message.contains(&n)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn policy() {
        assert!(is_retryable_status(402, &[]));
        assert!(is_retryable_status(503, &[503]) && !is_retryable_status(400, &[503]));
        let tokens = vec!["ECONNRESET".to_string(), "socket hang up".to_string()];
        assert!(is_retryable_network_error(Some(&json!({"code": "econnreset"})), &tokens));
        assert!(is_retryable_network_error(Some(&json!({"message": "Socket hang up!"})), &tokens));
        assert!(!is_retryable_network_error(Some(&json!({"message": "nope"})), &tokens));
        assert!(is_retryable_oauth_error(Some(&json!({"message": "x"}))));
        assert!(!is_retryable_oauth_error(Some(&json!({"message": "bad", "status": 401}))));
        assert!(is_retryable_oauth_error(Some(&json!({"message": "Network down", "status": 401}))));
        assert!(!is_retryable_oauth_error(None));
    }
}
