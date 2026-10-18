//! `utils/stall.ts` (`getGlobalStallConfig`, `resolveStallConfig`) and the
//! TTFB budget helper from `standard-attempt-request.ts`. The mid-stream
//! `StallInspector` (throughput monitoring) is not ported.
use crate::engine::config::Config;
use crate::engine::execute::StallConfig;
use crate::engine::js::*;
use serde_json::Value;

fn default_config() -> StallConfig {
    StallConfig {
        ttfb_ms: None,
        ttfb_bytes: 100,
        min_bps: None,
        window_ms: 10_000,
        grace_period_ms: 30_000,
    }
}

fn int(v: f64) -> i64 {
    v as i64
}

/// `getGlobalStallConfig()`.
pub fn global_stall_config(config: &Config) -> Option<StallConfig> {
    let stall = config.root.get("stall").filter(|s| truthy(Some(s)))?;
    let f = |k: &str| {
        stall
            .get(k)
            .filter(|v| !v.is_null())
            .and_then(Value::as_f64)
    };
    let ttfb_ms = f("ttfbSeconds").map(|s| int(s * 1000.0));
    let min_bps = f("minBytesPerSecond").map(int);
    if ttfb_ms.is_none() && min_bps.is_none() {
        return None;
    }
    Some(StallConfig {
        ttfb_ms,
        ttfb_bytes: f("ttfbBytes").map(int).unwrap_or(100),
        min_bps,
        window_ms: int(f("windowSeconds").unwrap_or(10.0) * 1000.0),
        grace_period_ms: int(f("gracePeriodSeconds").unwrap_or(30.0) * 1000.0),
    })
}

/// `resolveStallConfig(global, { stallTtfbMs: route.config.stallTtfbMs, … })`
/// as the request manager calls it (always with the five override keys).
pub fn resolve_stall_config(global: Option<&StallConfig>, provider: &Value) -> Option<StallConfig> {
    let base = global.cloned().unwrap_or_else(default_config);
    // `!== undefined` (null disables) vs `!= null` (null keeps base).
    let defined = |k: &str| provider.get(k);
    let nullable = |k: &str, b: Option<i64>| match defined(k) {
        Some(Value::Null) => None,
        Some(v) => v.as_f64().map(int),
        None => b,
    };
    let present = |k: &str, b: i64| {
        defined(k)
            .filter(|v| !v.is_null())
            .and_then(Value::as_f64)
            .map(int)
            .unwrap_or(b)
    };
    let merged = StallConfig {
        ttfb_ms: nullable("stallTtfbMs", base.ttfb_ms),
        ttfb_bytes: present("stallTtfbBytes", base.ttfb_bytes),
        min_bps: nullable("stallMinBps", base.min_bps),
        window_ms: present("stallWindowMs", base.window_ms),
        grace_period_ms: present("stallGracePeriodMs", base.grace_period_ms),
    };
    if merged.ttfb_ms.is_none() && merged.min_bps.is_none() {
        return None;
    }
    Some(merged)
}

/// `deriveProbeStallConfig(pristine, fetchElapsedMs)`.
pub fn derive_probe_stall_config(
    pristine: Option<&StallConfig>,
    fetch_elapsed_ms: i64,
) -> Option<StallConfig> {
    let p = pristine?;
    let Some(ttfb) = p.ttfb_ms else {
        return Some(p.clone());
    };
    let remaining = (ttfb - fetch_elapsed_ms).max(0);
    Some(StallConfig {
        ttfb_ms: (remaining > 0).then_some(remaining),
        ..p.clone()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn resolve() {
        assert_eq!(resolve_stall_config(None, &json!({})), None);
        let c = resolve_stall_config(None, &json!({"stallTtfbMs": 500})).unwrap();
        assert_eq!((c.ttfb_ms, c.ttfb_bytes), (Some(500), 100));
        let g = global_stall_config(&Config::new(json!({"stall": {"ttfbSeconds": 2, "ttfbBytes": 100, "minBytesPerSecond": null, "windowSeconds": 10, "gracePeriodSeconds": 30}}))).unwrap();
        assert_eq!(g.ttfb_ms, Some(2000));
        assert_eq!(
            resolve_stall_config(Some(&g), &json!({"stallTtfbMs": null})),
            None
        );
        assert_eq!(
            derive_probe_stall_config(Some(&g), 2500).unwrap().ttfb_ms,
            None
        );
    }
}
