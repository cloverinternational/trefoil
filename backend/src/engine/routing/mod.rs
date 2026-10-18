//! Port of Plexus `services/routing/*` and
//! `services/providers/provider-api-selection.ts`.
//!
//! Module ↔ TypeScript file:
//! - `router` — `routing/router.ts` (`Router.resolveCandidates`, `Router.resolve`,
//!   `Router.splitServiceTier`, direct routing, target filtering, alias refs).
//! - `selectors` — `routing/selectors/*` (all selectors + `SelectorFactory`).
//! - `sticky_session` — `routing/sticky-session-manager.ts` (incl. `Bun.hash`).
//! - `key_access_policy`, `scope_match`, `service_tier` — the same-named files.
//! - `route_candidates` — `routing/route-candidates.ts` (+ `appendSkippedAttempt`,
//!   `QuotaEnforcer.filterCandidates`, `buildQuotaExceededError`).
//! - `background_explorer` — `routing/background-explorer.ts`.
//! - `provider_api_selection` — `providers/provider-api-selection.ts`.
//! - `runtime` — `RoutingRuntime`, the process services the TS reaches for.
//!
//! Config, providers and targets are Plexus-shaped JSON (see `engine::config`);
//! Plexus `logger` calls are not ported (no observable behavior).
pub mod background_explorer;
pub mod key_access_policy;
pub mod provider_api_selection;
pub mod route_candidates;
pub mod router;
pub mod runtime;
pub mod scope_match;
pub mod selectors;
pub mod service_tier;
pub mod sticky_session;

pub use router::{resolve, resolve_candidates, split_service_tier};
pub use runtime::RoutingRuntime;

use serde_json::{Map, Value};

/// `RouteResult` (router.ts). `config` is the provider config JSON,
/// `model_config` is `providers[provider].models[model]` (record form only).
#[derive(Debug, Clone, PartialEq)]
pub struct RouteResult {
    pub provider: String,
    pub model: String,
    pub config: Value,
    pub model_config: Option<Value>,
    pub incoming_model_alias: Option<String>,
    pub canonical_model: Option<String>,
}

/// Keys in JS enumeration order (`Object.keys` / `Object.entries`): integer-like
/// keys (canonical array indices) ascending first, then string keys in
/// insertion order.
pub(crate) fn js_keys(map: &Map<String, Value>) -> Vec<&String> {
    fn index(k: &str) -> Option<u32> {
        let n: u32 = k.parse().ok()?;
        (n != u32::MAX && n.to_string() == k).then_some(n)
    }
    let mut ints: Vec<(u32, &String)> = map
        .keys()
        .filter_map(|k| index(k).map(|n| (n, k)))
        .collect();
    ints.sort_by_key(|(n, _)| *n);
    let mut out: Vec<&String> = ints.into_iter().map(|(_, k)| k).collect();
    out.extend(map.keys().filter(|k| index(k).is_none()));
    out
}

/// `Number(v)` for numeric comparisons, `NaN` when not coercible
/// (comparisons with `NaN` are false, like JS).
pub(crate) fn js_num(v: Option<&Value>) -> f64 {
    super::js::to_number(v).unwrap_or(f64::NAN)
}

/// `Date.parse` for the formats Plexus stores (ISO-8601 / RFC 3339, RFC 2822,
/// date-only `YYYY-MM-DD`); `NaN` otherwise. Offset-less date-times are read as
/// UTC (Plexus servers run in UTC).
pub(crate) fn date_parse_ms(s: &str) -> f64 {
    use chrono::{DateTime, NaiveDate, NaiveDateTime};
    let s = s.trim();
    if let Ok(d) = DateTime::parse_from_rfc3339(s) {
        return d.timestamp_millis() as f64;
    }
    if let Ok(d) = DateTime::parse_from_rfc2822(s) {
        return d.timestamp_millis() as f64;
    }
    for fmt in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M"] {
        if let Ok(d) = NaiveDateTime::parse_from_str(s, fmt) {
            return d.and_utc().timestamp_millis() as f64;
        }
    }
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        if let Some(dt) = d.and_hms_opt(0, 0, 0) {
            return dt.and_utc().timestamp_millis() as f64;
        }
    }
    f64::NAN
}

/// `new Date(ms).toISOString()`.
pub(crate) fn iso_from_ms(ms: f64) -> String {
    chrono::DateTime::from_timestamp_millis(ms as i64)
        .map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_else(|| "Invalid Date".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn js_key_order_puts_integer_keys_first() {
        let v = json!({"b": 1, "2": 1, "a": 1, "1": 1, "01": 1});
        let keys: Vec<&str> = js_keys(v.as_object().unwrap())
            .into_iter()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, vec!["1", "2", "b", "a", "01"]);
    }

    #[test]
    fn date_parse() {
        assert_eq!(date_parse_ms("1970-01-01T00:00:01.000Z"), 1000.0);
        assert_eq!(date_parse_ms("1970-01-02"), 86_400_000.0);
        assert!(date_parse_ms("nope").is_nan());
        assert_eq!(iso_from_ms(1000.0), "1970-01-01T00:00:01.000Z");
    }
}
