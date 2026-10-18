//! `services/runtime/cooldown-parsers.ts` + `services/providers/provider-cooldown.ts`.
use crate::engine::config::get_provider_types;
use crate::engine::js::*;
use crate::engine::routing::RouteResult;
use regex::Regex;
use serde_json::Value;
use std::sync::OnceLock;

fn js_parse_float(s: &str) -> f64 {
    crate::engine::dispatch::util::parse_float(s)
}

fn openai_codex(text: &str) -> Option<f64> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"(?i)try again in ~(\d+)\s*min").unwrap());
    let minutes: f64 = re.captures(text)?.get(1)?.as_str().parse().ok()?;
    (minutes.is_finite() && minutes > 0.0).then_some(minutes * 60.0 * 1000.0)
}

fn openrouter(text: &str) -> Option<f64> {
    if let Some(data) = parse(text) {
        let meta = get(&data, &["error", "metadata"])
            .filter(|m| truthy(Some(m)))
            .or_else(|| data.get("metadata"));
        let retry = meta.and_then(|m| {
            coalesce(
                m.get("retry_after_seconds"),
                m.get("retry_after_seconds_raw"),
            )
        });
        if let Some(retry) = retry.filter(|v| !v.is_null()) {
            let sec = match &retry {
                Value::Number(n) => n.as_f64().unwrap_or(f64::NAN),
                other => js_parse_float(&to_string(Some(other))),
            };
            if sec.is_finite() && sec > 0.0 {
                return Some((sec * 1000.0).ceil());
            }
        }
        let header = meta
            .and_then(|m| m.get("headers"))
            .and_then(|h| or(h.get("Retry-After"), h.get("retry-after")));
        if let Some(h) = header.filter(|h| truthy(Some(h))) {
            let sec = js_parse_float(&to_string(Some(&h)));
            if sec.is_finite() && sec > 0.0 {
                return Some((sec * 1000.0).ceil());
            }
        }
    }
    static SEC: OnceLock<Regex> = OnceLock::new();
    static MIN: OnceLock<Regex> = OnceLock::new();
    let sec_re = SEC.get_or_init(|| {
        Regex::new(r"(?i)retry(?:-|\s+)after[:\s]+(\d+(?:\.\d+)?)\s*s(?:ec(?:onds?)?)?").unwrap()
    });
    if let Some(sec) = sec_re
        .captures(text)
        .and_then(|c| c.get(1))
        .and_then(|m| m.as_str().parse::<f64>().ok())
    {
        if sec.is_finite() && sec > 0.0 {
            return Some((sec * 1000.0).ceil());
        }
    }
    let min_re = MIN.get_or_init(|| {
        Regex::new(r"(?i)retry(?:-|\s+)after[:\s]+(\d+(?:\.\d+)?)\s*m(?:in(?:utes?)?)?").unwrap()
    });
    if let Some(min) = min_re
        .captures(text)
        .and_then(|c| c.get(1))
        .and_then(|m| m.as_str().parse::<f64>().ok())
    {
        if min.is_finite() && min > 0.0 {
            return Some((min * 60.0 * 1000.0).ceil());
        }
    }
    None
}

/// `CooldownParserRegistry.parseCooldown(providerType, errorText)`.
pub fn parse_cooldown(provider_type: &str, error_text: &str) -> Option<f64> {
    match provider_type.to_lowercase().as_str() {
        "openai-codex" => openai_codex(error_text),
        "openrouter" => openrouter(error_text),
        _ => None,
    }
}

/// `resolveCooldownProviderType(route)`.
pub fn resolve_cooldown_provider_type(route: &RouteResult) -> Option<String> {
    for key in ["oauth_provider", "pi_ai_provider"] {
        if let Some(v) = route
            .config
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return Some(v.to_owned());
        }
    }
    if route.provider.to_lowercase().contains("openrouter") {
        return Some("openrouter".into());
    }
    if let Some(url) = route.config.get("api_base_url").and_then(Value::as_str) {
        if url.to_lowercase().contains("openrouter.ai") {
            return Some("openrouter".into());
        }
    }
    get_provider_types(&route.config).into_iter().next()
}

/// `parseCooldownDurationForProvider(providerType, errorText, source)`.
pub fn parse_cooldown_duration_for_provider(
    provider_type: Option<&str>,
    error_text: &str,
) -> Option<f64> {
    if let Some(t) = provider_type {
        if let Some(d) = parse_cooldown(t, error_text) {
            return Some(d);
        }
    }
    if error_text.contains("openrouter.ai")
        || error_text.contains("retry_after_seconds")
        || error_text.contains("is_byok")
    {
        return parse_cooldown("openrouter", error_text);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parsers() {
        assert_eq!(
            parse_cooldown("openai-codex", "Try again in ~45 min."),
            Some(2_700_000.0)
        );
        assert_eq!(
            parse_cooldown(
                "openrouter",
                r#"{"error":{"metadata":{"retry_after_seconds":1.5}}}"#
            ),
            Some(1500.0)
        );
        assert_eq!(
            parse_cooldown("openrouter", "please retry after 3 seconds"),
            Some(3000.0)
        );
        assert_eq!(
            parse_cooldown_duration_for_provider(
                Some("chat"),
                r#"{"metadata":{"retry_after_seconds":2}}"#
            ),
            Some(2000.0)
        );
        assert_eq!(
            parse_cooldown_duration_for_provider(Some("chat"), "nope"),
            None
        );
    }
}
