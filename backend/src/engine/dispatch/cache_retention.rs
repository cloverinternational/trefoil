//! `services/dispatch/cache-retention.ts`.
use super::util::base_type;
use crate::engine::js::*;
use serde_json::Value;

const HOUR_MS: i64 = 60 * 60 * 1000;
/// `routing/sticky-session-manager.ts` `DEFAULT_STICKY_TTL_MS`.
pub const DEFAULT_STICKY_TTL_MS: i64 = 5 * 60 * 1000;

/// `resolveStickyCacheTtlMs(payload, apiType)`.
pub fn resolve_sticky_cache_ttl_ms(payload: &Value, api_type: &str) -> i64 {
    let base = base_type(api_type);
    if base == "chat" || base == "responses" {
        if get_str(payload, &["prompt_cache_options", "ttl"]) == Some("30m") {
            return 30 * 60 * 1000;
        }
        if get_str(payload, &["prompt_cache_retention"]) == Some("24h") {
            return 24 * HOUR_MS;
        }
        return DEFAULT_STICKY_TTL_MS;
    }
    if base != "messages" {
        return DEFAULT_STICKY_TTL_MS;
    }
    let mut markers: Vec<Option<&Value>> = vec![payload.get("cache_control")];
    for field in ["system", "tools", "messages"] {
        let Some(blocks) = get_arr(payload, &[field]) else {
            continue;
        };
        for block in blocks {
            markers.push(block.get("cache_control"));
            if let Some(parts) = get_arr(block, &["content"]) {
                for part in parts {
                    markers.push(part.get("cache_control"));
                }
            }
        }
    }
    let long = markers.iter().any(|m| {
        m.is_some_and(|m| {
            get_str(m, &["type"]) == Some("ephemeral") && get_str(m, &["ttl"]) == Some("1h")
        })
    });
    if long {
        HOUR_MS
    } else {
        DEFAULT_STICKY_TTL_MS
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn ttl() {
        assert_eq!(
            resolve_sticky_cache_ttl_ms(&json!({"prompt_cache_retention": "24h"}), "responses"),
            24 * HOUR_MS
        );
        assert_eq!(
            resolve_sticky_cache_ttl_ms(&json!({"prompt_cache_options": {"ttl": "30m"}}), "chat"),
            30 * 60 * 1000
        );
        let msgs = json!({"messages": [{"content": [{"type": "text", "cache_control": {"type": "ephemeral", "ttl": "1h"}}]}]});
        assert_eq!(resolve_sticky_cache_ttl_ms(&msgs, "messages"), HOUR_MS);
        assert_eq!(
            resolve_sticky_cache_ttl_ms(&msgs, "gemini"),
            DEFAULT_STICKY_TTL_MS
        );
        assert_eq!(
            resolve_sticky_cache_ttl_ms(
                &json!({"system": [{"cache_control": {"type": "ephemeral"}}]}),
                "messages"
            ),
            DEFAULT_STICKY_TTL_MS
        );
    }
}
