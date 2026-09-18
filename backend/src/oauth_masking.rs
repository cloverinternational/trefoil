//! Redaction helpers for OAuth diagnostics and management responses.

use serde_json::Value;

const SECRET_KEYS: &[&str] = &[
    "access_token",
    "refresh_token",
    "token",
    "key",
    "api_key",
    "authorization",
    "state",
    "code",
    "code_verifier",
    "client_secret",
];

pub fn mask_token(value: &str) -> String {
    if value.is_empty() {
        return String::new();
    }
    if value.len() <= 8 {
        return "***".into();
    }
    let chars: Vec<char> = value.chars().collect();
    if chars.len() <= 8 {
        return "***".into();
    }
    format!(
        "{}…{}",
        chars[..4].iter().collect::<String>(),
        chars[chars.len() - 4..].iter().collect::<String>()
    )
}

pub fn mask_url(input: &str) -> String {
    let Ok(mut url) = url::Url::parse(input) else {
        return input.to_owned();
    };
    let pairs: Vec<(String, String)> = url
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    let mut query = url.query_pairs_mut();
    query.clear();
    for (key, value) in pairs {
        let value = if SECRET_KEYS
            .iter()
            .any(|secret| key.eq_ignore_ascii_case(secret))
        {
            "***".to_owned()
        } else {
            value
        };
        query.append_pair(&key, &value);
    }
    drop(query);
    url.to_string()
}

pub fn mask_value(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, value)| {
                    let masked = if SECRET_KEYS
                        .iter()
                        .any(|secret| key.eq_ignore_ascii_case(secret))
                    {
                        match value {
                            Value::String(s) => Value::String(mask_token(s)),
                            _ => Value::String("***".into()),
                        }
                    } else {
                        mask_value(value)
                    };
                    (key.clone(), masked)
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(mask_value).collect()),
        Value::String(s) if s.starts_with("http://") || s.starts_with("https://") => {
            Value::String(mask_url(s))
        }
        _ => value.clone(),
    }
}

pub fn mask_headers(headers: &[(String, String)]) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(key, value)| {
            if key.eq_ignore_ascii_case("authorization")
                || key.to_ascii_lowercase().contains("token")
            {
                (key.clone(), mask_token(value))
            } else {
                (key.clone(), value.clone())
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn masks_nested_secrets_and_preserves_context() {
        let value = serde_json::json!({"provider":"xai","credentials":{"access_token":"abcdefghijkl","refresh_token":"refresh-secret"},"ok":true});
        let masked = mask_value(&value);
        assert_eq!(masked["provider"], "xai");
        assert_eq!(masked["ok"], true);
        assert!(!masked.to_string().contains("refresh-secret"));
    }
    #[test]
    fn masks_url_and_headers_without_changing_non_secrets() {
        assert!(
            !mask_url("https://example.test/callback?state=secret&code=hidden").contains("secret")
        );
        assert!(
            !mask_url("https://example.test/callback?state=secret&code=hidden").contains("hidden")
        );
        let headers = mask_headers(&[
            ("Authorization".into(), "Bearer secret".into()),
            ("X-Trace".into(), "trace".into()),
        ]);
        assert!(!headers[0].1.contains("secret"));
        assert_eq!(headers[1].1, "trace");
        assert_eq!(mask_token("éclair-token"), "écla…oken");
        assert!(
            mask_url("https://example.test/callback?state=secret&keep=yes").contains("keep=yes")
        );
    }
}
