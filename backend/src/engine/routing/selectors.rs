//! `routing/selectors/*`: target selection strategies and `SelectorFactory`.
//!
//! Targets are JSON `EnrichedModelTarget`s (`{provider, model, ..., route:
//! {modelConfig?}}`). `select` returns the index of the chosen target (TS
//! returns the object itself), `None` for TS `null`.
use super::runtime::RoutingRuntime;
use super::{date_parse_ms, iso_from_ms, js_num};
use crate::engine::js::{coalesce, get, get_str, nullish, to_string, truthy};
use crate::engine::transformer::{EngineError, EngineResult};
use serde_json::Value;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::Mutex;

#[async_trait::async_trait]
pub trait Selector: Send + Sync {
    /// `select(targets)`.
    async fn select(&self, targets: &[Value], rt: &dyn RoutingRuntime) -> Option<usize>;
}

fn provider_of(t: &Value) -> &str {
    get_str(t, &["provider"]).unwrap_or("")
}
fn model_of(t: &Value) -> &str {
    get_str(t, &["model"]).unwrap_or("")
}

/// `CandidateStats`.
struct CandidateStats {
    index: usize,
    sample_count: Option<Value>,
    last_updated: f64,
}

fn stats_for(index: usize, row: Option<&Value>) -> CandidateStats {
    // `targetStats?.sample_count ?? 0`, `targetStats?.last_updated ?? 0`
    let sample = row
        .and_then(|r| r.get("sample_count"))
        .filter(|v| !v.is_null())
        .cloned();
    let last = row
        .and_then(|r| r.get("last_updated"))
        .filter(|v| !v.is_null())
        .map(|v| js_num(Some(v)))
        .unwrap_or(0.0);
    CandidateStats {
        index,
        sample_count: sample,
        last_updated: last,
    }
}

/// `Selector.pickExplorationTarget`: random among never-sampled targets,
/// else the stalest (`reduce((a, b) => a.lastUpdated <= b.lastUpdated ? a : b)`).
fn pick_exploration_target(
    candidates: &[CandidateStats],
    rt: &dyn RoutingRuntime,
) -> Option<usize> {
    // `c.sampleCount === 0`
    let unseen: Vec<&CandidateStats> = candidates
        .iter()
        .filter(|c| match &c.sample_count {
            None => true,
            Some(Value::Number(n)) => n.as_f64() == Some(0.0),
            _ => false,
        })
        .collect();
    if !unseen.is_empty() {
        let i = (rt.random() * unseen.len() as f64).floor() as usize;
        return unseen.get(i.min(unseen.len() - 1)).map(|c| c.index);
    }
    let mut it = candidates.iter();
    let first = it.next()?;
    Some(
        it.fold(first, |a, b| {
            if a.last_updated <= b.last_updated {
                a
            } else {
                b
            }
        })
        .index,
    )
}

/// Exploration rate from config keys (`a ?? b ?? default`), disabled when
/// background exploration is on.
fn exploration_rate(rt: &dyn RoutingRuntime, keys: &[&str], default: f64) -> f64 {
    let cfg = rt.config();
    if get(&cfg.root, &["backgroundExploration", "enabled"]) == Some(&Value::Bool(true)) {
        return 0.0;
    }
    let mut v: Option<Value> = None;
    for k in keys {
        v = coalesce(v.as_ref(), cfg.root.get(*k));
    }
    if nullish(v.as_ref()) {
        default
    } else {
        js_num(v.as_ref())
    }
}

pub struct RandomSelector;
#[async_trait::async_trait]
impl Selector for RandomSelector {
    async fn select(&self, targets: &[Value], rt: &dyn RoutingRuntime) -> Option<usize> {
        if targets.is_empty() {
            return None;
        }
        let i = (rt.random() * targets.len() as f64).floor() as usize;
        Some(i.min(targets.len() - 1))
    }
}

pub struct InOrderSelector;
#[async_trait::async_trait]
impl Selector for InOrderSelector {
    async fn select(&self, targets: &[Value], _rt: &dyn RoutingRuntime) -> Option<usize> {
        if targets.is_empty() {
            None
        } else {
            Some(0)
        }
    }
}

/// JS `parseFloat(String(v))`: longest numeric prefix, `NaN` if none.
pub(crate) fn parse_float(v: Option<&Value>) -> f64 {
    let s = to_string(v);
    let s = s.trim_start();
    let (sign, rest) = match s.as_bytes().first() {
        Some(b'-') => (-1.0, &s[1..]),
        Some(b'+') => (1.0, &s[1..]),
        _ => (1.0, s),
    };
    if rest.starts_with("Infinity") {
        return sign * f64::INFINITY;
    }
    let b = rest.as_bytes();
    let mut i = 0;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    let int_digits = i;
    if i < b.len() && b[i] == b'.' {
        let mut j = i + 1;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
        if j > i + 1 || int_digits > 0 {
            i = j;
        }
    }
    if i == 0 || (i == 1 && b[0] == b'.') {
        return f64::NAN;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        let mut j = i + 1;
        if j < b.len() && (b[j] == b'+' || b[j] == b'-') {
            j += 1;
        }
        let start = j;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
        if j > start {
            i = j;
        }
    }
    rest[..i]
        .parse::<f64>()
        .map(|n| sign * n)
        .unwrap_or(f64::NAN)
}

/// `x || 0` for numeric use (`NaN` when the truthy value is not numeric).
fn or_zero(v: Option<&Value>) -> f64 {
    if truthy(v) {
        js_num(v)
    } else {
        0.0
    }
}

/// `calculatePriceForTarget(pricing, input, output, cached = 0, cacheWrite = 0)` (cost.ts).
pub fn calculate_price_for_target(
    pricing: Option<&Value>,
    input: f64,
    output: f64,
    cached: f64,
    cache_write: f64,
    rt: &dyn RoutingRuntime,
) -> f64 {
    let Some(pricing) = pricing.filter(|p| truthy(Some(p))) else {
        return 0.0;
    };
    let source = get_str(pricing, &["source"]);
    match source {
        Some("simple") => {
            let input_cost = (input / 1_000_000.0) * or_zero(pricing.get("input"));
            let output_cost = (output / 1_000_000.0) * or_zero(pricing.get("output"));
            let cached_cost = if truthy(pricing.get("cached")) {
                (cached / 1_000_000.0) * js_num(pricing.get("cached"))
            } else {
                0.0
            };
            let cw_cost = if truthy(pricing.get("cache_write")) {
                (cache_write / 1_000_000.0) * js_num(pricing.get("cache_write"))
            } else {
                0.0
            };
            input_cost + output_cost + cached_cost + cw_cost
        }
        Some("defined") if pricing.get("range").is_some_and(Value::is_array) => {
            let ranges = pricing["range"].as_array().cloned().unwrap_or_default();
            let found = ranges.iter().find(|r| {
                let lower = if nullish(r.get("lower_bound")) {
                    0.0
                } else {
                    js_num(r.get("lower_bound"))
                };
                let upper = if nullish(r.get("upper_bound")) {
                    f64::INFINITY
                } else {
                    js_num(r.get("upper_bound"))
                };
                input >= lower && input <= upper
            });
            match found {
                Some(m) => {
                    (input / 1_000_000.0) * or_zero(m.get("input_per_m"))
                        + (output / 1_000_000.0) * or_zero(m.get("output_per_m"))
                        + (cached / 1_000_000.0) * or_zero(m.get("cached_per_m"))
                        + (cache_write / 1_000_000.0) * or_zero(m.get("cache_write_per_m"))
                }
                None => 0.0,
            }
        }
        Some("openrouter") if truthy(pricing.get("slug")) => {
            match rt
                .openrouter_pricing(&to_string(pricing.get("slug")))
                .filter(|p| truthy(Some(p)))
            {
                Some(p) => {
                    let rate = |v: Option<&Value>| {
                        let n = parse_float(v);
                        if n.is_nan() || n == 0.0 {
                            0.0
                        } else {
                            n
                        }
                    };
                    let zero = Value::String("0".into());
                    let cr = if truthy(p.get("input_cache_read")) {
                        p.get("input_cache_read")
                    } else {
                        Some(&zero)
                    };
                    let cw = if truthy(p.get("input_cache_write")) {
                        p.get("input_cache_write")
                    } else {
                        Some(&zero)
                    };
                    input * rate(p.get("prompt"))
                        + output * rate(p.get("completion"))
                        + cached * rate(cr)
                        + cache_write * rate(cw)
                }
                None => 0.0,
            }
        }
        Some("default") => {
            (input / 1_000_000.0) * or_zero(pricing.get("input"))
                + (output / 1_000_000.0) * or_zero(pricing.get("output"))
        }
        _ => 0.0,
    }
}

/// `CostSelector`: cheapest by simulated 1000 input / 500 output tokens
/// using `route.modelConfig.pricing`; first wins ties.
pub struct CostSelector;
#[async_trait::async_trait]
impl Selector for CostSelector {
    async fn select(&self, targets: &[Value], rt: &dyn RoutingRuntime) -> Option<usize> {
        let mut lowest = f64::INFINITY;
        let mut cheapest = None;
        for (i, t) in targets.iter().enumerate() {
            let cost = calculate_price_for_target(
                get(t, &["route", "modelConfig", "pricing"]),
                1000.0,
                500.0,
                0.0,
                0.0,
                rt,
            );
            if cost < lowest {
                lowest = cost;
                cheapest = Some(i);
            }
        }
        cheapest
    }
}

/// Shared shape of performance/e2e selectors: score = metric `|| 0`, sorted
/// descending (stable), optional exploration.
async fn throughput_select(
    targets: &[Value],
    rt: &dyn RoutingRuntime,
    field: &str,
    rate_keys: &[&str],
) -> Option<usize> {
    if targets.is_empty() {
        return None;
    }
    if targets.len() == 1 {
        return Some(0);
    }
    let rate = exploration_rate(rt, rate_keys, 0.05);
    let mut candidates: Vec<(usize, f64)> = Vec::new();
    let mut stats = Vec::new();
    for (i, t) in targets.iter().enumerate() {
        let rows = rt.provider_performance(provider_of(t), model_of(t)).await;
        let row = rows.first();
        let score = row.map(|r| or_zero(r.get(field))).unwrap_or(0.0);
        candidates.push((i, score));
        stats.push(stats_for(i, row));
    }
    candidates.sort_by(|a, b| js_cmp(b.1 - a.1));
    let best = candidates.first()?;
    if rate > 0.0 && rt.random() < rate && candidates.len() > 1 {
        if let Some(choice) = pick_exploration_target(&stats, rt) {
            return Some(choice);
        }
    }
    Some(best.0)
}

/// JS comparator result → `Ordering` (`NaN` is treated as equal).
fn js_cmp(d: f64) -> Ordering {
    if d < 0.0 {
        Ordering::Less
    } else if d > 0.0 {
        Ordering::Greater
    } else {
        Ordering::Equal
    }
}

/// `PerformanceSelector`: highest `avg_tokens_per_sec`; exploration rate
/// `performanceExplorationRate ?? 0.05`.
pub struct PerformanceSelector;
#[async_trait::async_trait]
impl Selector for PerformanceSelector {
    async fn select(&self, targets: &[Value], rt: &dyn RoutingRuntime) -> Option<usize> {
        throughput_select(
            targets,
            rt,
            "avg_tokens_per_sec",
            &["performanceExplorationRate"],
        )
        .await
    }
}

/// `E2EPerformanceSelector`: highest `avg_e2e_tokens_per_sec`; exploration
/// rate `e2ePerformanceExplorationRate ?? performanceExplorationRate ?? 0.05`.
pub struct E2EPerformanceSelector;
#[async_trait::async_trait]
impl Selector for E2EPerformanceSelector {
    async fn select(&self, targets: &[Value], rt: &dyn RoutingRuntime) -> Option<usize> {
        throughput_select(
            targets,
            rt,
            "avg_e2e_tokens_per_sec",
            &[
                "e2ePerformanceExplorationRate",
                "performanceExplorationRate",
            ],
        )
        .await
    }
}

/// `LatencySelector`: lowest `avg_ttft_ms` among targets with data;
/// exploration rate `latencyExplorationRate ?? performanceExplorationRate ?? 0`;
/// first target when none has data.
pub struct LatencySelector;
#[async_trait::async_trait]
impl Selector for LatencySelector {
    async fn select(&self, targets: &[Value], rt: &dyn RoutingRuntime) -> Option<usize> {
        if targets.is_empty() {
            return None;
        }
        if targets.len() == 1 {
            return Some(0);
        }
        let mut candidates: Vec<(usize, f64)> = Vec::new();
        let mut stats = Vec::new();
        for (i, t) in targets.iter().enumerate() {
            let rows = rt.provider_performance(provider_of(t), model_of(t)).await;
            let row = rows.first();
            let ttft = row
                .and_then(|r| r.get("avg_ttft_ms"))
                .filter(|v| !v.is_null())
                .map(|v| js_num(Some(v)))
                .unwrap_or(f64::INFINITY);
            candidates.push((i, ttft));
            stats.push(stats_for(i, row));
        }
        let mut valid: Vec<(usize, f64)> = candidates
            .into_iter()
            .filter(|c| c.1 != f64::INFINITY)
            .collect();
        if !valid.is_empty() {
            valid.sort_by(|a, b| js_cmp(a.1 - b.1));
            let rate = exploration_rate(
                rt,
                &["latencyExplorationRate", "performanceExplorationRate"],
                0.0,
            );
            if rate > 0.0 && valid.len() > 1 && rt.random() < rate {
                if let Some(choice) = pick_exploration_target(&stats, rt) {
                    return Some(choice);
                }
            }
            return Some(valid[0].0);
        }
        Some(0)
    }
}

/// `UsageSelector`: least-used target over the last 24 hours.
pub struct UsageSelector;
#[async_trait::async_trait]
impl Selector for UsageSelector {
    async fn select(&self, targets: &[Value], rt: &dyn RoutingRuntime) -> Option<usize> {
        if targets.is_empty() {
            return None;
        }
        if targets.len() == 1 {
            return Some(0);
        }
        let now = rt.now_ms() as f64;
        let one_day_ago = iso_from_ms(now - 24.0 * 60.0 * 60.0 * 1000.0);
        let today = iso_from_ms(now);
        let mut candidates: Vec<(usize, f64)> = Vec::new();
        for (i, t) in targets.iter().enumerate() {
            let total = rt
                .usage_total(provider_of(t), model_of(t), &one_day_ago, &today)
                .await;
            candidates.push((i, total));
        }
        candidates.sort_by(|a, b| js_cmp(a.1 - b.1));
        candidates.first().map(|c| c.0)
    }
}

// ---- quota.ts ----

const MILLISECONDS_PER_HOUR: f64 = 60.0 * 60.0 * 1000.0;

fn finite_num(v: Option<&Value>) -> Option<f64> {
    v.and_then(Value::as_f64).filter(|n| n.is_finite())
}

/// `getUtilization(meter)`.
pub fn get_utilization(meter: &Value) -> Option<f64> {
    if get_str(meter, &["status"]) == Some("exhausted") {
        return Some(100.0);
    }
    let remaining = finite_num(meter.get("remaining"));
    let used = finite_num(meter.get("used"));
    let limit = finite_num(meter.get("limit"));
    let clamp = |x: f64| x.clamp(0.0, 100.0);
    if remaining.is_some_and(|r| r <= 0.0) {
        return Some(100.0);
    }
    if let Some(u) = finite_num(meter.get("utilizationPercent")) {
        return Some(clamp(u));
    }
    if let (Some(u), Some(l)) = (used, limit) {
        if l > 0.0 {
            return Some(clamp(u / l * 100.0));
        }
    }
    if let (Some(r), Some(l)) = (remaining, limit) {
        if l > 0.0 {
            return Some(clamp(100.0 - (r / l) * 100.0));
        }
    }
    if let (Some(u), Some(r)) = (used, remaining) {
        let total = u + r;
        if total > 0.0 {
            return Some(clamp(u / total * 100.0));
        }
    }
    None
}

/// Category ranks: `expiring` 0, `balance` 1, `unknown` 2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum QuotaCategory {
    Expiring,
    Balance,
    Unknown,
}

/// `getBurnDownScore(result)` at `now` (ms).
pub fn get_burn_down_score(result: &Value, now: f64) -> Option<(QuotaCategory, f64)> {
    if !truthy(result.get("success")) {
        return None;
    }
    let meters: Vec<(&Value, f64)> = result
        .get("meters")
        .and_then(Value::as_array)
        .map(|m| {
            m.iter()
                .filter_map(|meter| get_utilization(meter).map(|u| (meter, u)))
                .collect()
        })
        .unwrap_or_default();
    if meters.is_empty() {
        return None;
    }
    let mut deadline = Vec::new();
    let mut balance = Vec::new();
    for (meter, utilization) in meters {
        if !truthy(meter.get("resetsAt")) {
            balance.push((100.0 - utilization) / 100.0);
            continue;
        }
        let resets_at = date_parse_ms(&to_string(meter.get("resetsAt")));
        if !resets_at.is_finite() || resets_at <= now {
            continue;
        }
        let headroom = (100.0 - utilization) / 100.0;
        let hours = ((resets_at - now) / MILLISECONDS_PER_HOUR).max(1.0 / 60.0);
        deadline.push(headroom / hours);
    }
    let min = |v: &[f64]| v.iter().copied().fold(f64::INFINITY, f64::min);
    if !deadline.is_empty() {
        return Some((QuotaCategory::Expiring, min(&deadline)));
    }
    if !balance.is_empty() {
        return Some((QuotaCategory::Balance, min(&balance)));
    }
    Some((QuotaCategory::Unknown, 0.0))
}

/// `QuotaSelector`: prefers resettable quotas with the most headroom per hour
/// until reset, then balances, then unknown; declared order breaks ties.
/// Snapshots are cached per selector instance (one per `selectOrderedTargets`).
#[derive(Default)]
pub struct QuotaSelector {
    snapshot_cache: Mutex<HashMap<String, Option<Value>>>,
}

impl QuotaSelector {
    async fn snapshot(&self, provider: &str, rt: &dyn RoutingRuntime) -> Option<Value> {
        if let Some(c) = self
            .snapshot_cache
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(provider)
        {
            return c.clone();
        }
        let snap = rt.latest_quota_for_provider(provider).await;
        self.snapshot_cache
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(provider.to_owned(), snap.clone());
        snap
    }
}

#[async_trait::async_trait]
impl Selector for QuotaSelector {
    async fn select(&self, targets: &[Value], rt: &dyn RoutingRuntime) -> Option<usize> {
        if targets.is_empty() {
            return None;
        }
        if targets.len() == 1 {
            return Some(0);
        }
        let now = rt.now_ms() as f64;
        let mut scored: Vec<(usize, QuotaCategory, f64)> = Vec::new();
        for (order, t) in targets.iter().enumerate() {
            if !truthy(t.get("provider")) {
                scored.push((order, QuotaCategory::Unknown, 0.0));
                continue;
            }
            let result = self.snapshot(&to_string(t.get("provider")), rt).await;
            let score = result.as_ref().and_then(|r| get_burn_down_score(r, now));
            scored.push((
                order,
                score.map(|s| s.0).unwrap_or(QuotaCategory::Unknown),
                score.map(|s| s.1).unwrap_or(0.0),
            ));
        }
        scored.sort_by(|a, b| {
            a.1.cmp(&b.1).then_with(|| {
                if a.2 != b.2 {
                    js_cmp(b.2 - a.2)
                } else {
                    a.0.cmp(&b.0)
                }
            })
        });
        scored.first().map(|s| s.0)
    }
}

/// `SelectorFactory.getSelector(type)`.
pub fn get_selector(
    selector_type: Option<&Value>,
    rt: &dyn RoutingRuntime,
) -> EngineResult<Box<dyn Selector>> {
    let usage_err = || {
        EngineError::new(
            "UsageStorageService not initialized in SelectorFactory. Call setUsageStorage first.",
        )
    };
    let kind = match selector_type {
        None | Some(Value::Null) => return Ok(Box::new(RandomSelector)),
        Some(Value::String(s)) => s.as_str(),
        Some(other) => {
            return Err(EngineError::new(format!(
                "Unknown selector type: {}",
                to_string(Some(other))
            )))
        }
    };
    Ok(match kind {
        "random" => Box::new(RandomSelector),
        "in_order" => Box::new(InOrderSelector),
        "cost" => Box::new(CostSelector),
        "performance" => {
            if !rt.usage_storage_ready() {
                return Err(usage_err());
            }
            Box::new(PerformanceSelector)
        }
        "e2e_performance" => {
            if !rt.usage_storage_ready() {
                return Err(usage_err());
            }
            Box::new(E2EPerformanceSelector)
        }
        "latency" => {
            if !rt.usage_storage_ready() {
                return Err(usage_err());
            }
            Box::new(LatencySelector)
        }
        "usage" => {
            if !rt.usage_storage_ready() {
                return Err(usage_err());
            }
            Box::new(UsageSelector)
        }
        "quota" => {
            if !rt.quota_scheduler_ready() {
                return Err(EngineError::new("QuotaScheduler not initialized in SelectorFactory. Call setQuotaScheduler first."));
            }
            Box::new(QuotaSelector::default())
        }
        other => return Err(EngineError::new(format!("Unknown selector type: {other}"))),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::routing::runtime::mock::MockRuntime;
    use serde_json::json;

    fn t(p: &str, m: &str) -> Value {
        json!({"provider": p, "model": m})
    }
    fn targets3() -> Vec<Value> {
        vec![t("p1", "m1"), t("p2", "m2"), t("p3", "m3")]
    }

    #[tokio::test]
    async fn random_and_in_order() {
        let rt = MockRuntime::new(json!({})).with_randoms(&[0.5, 0.99]);
        assert_eq!(RandomSelector.select(&[], &rt).await, None);
        assert_eq!(RandomSelector.select(&targets3(), &rt).await, Some(1));
        assert_eq!(RandomSelector.select(&targets3(), &rt).await, Some(2));
        assert_eq!(InOrderSelector.select(&targets3(), &rt).await, Some(0));
        assert_eq!(InOrderSelector.select(&[], &rt).await, None);
    }

    #[tokio::test]
    async fn cost_selector() {
        let mut rt = MockRuntime::new(json!({}));
        rt.pricing.insert(
            "or/x".into(),
            json!({"prompt": "0.000001", "completion": "0.000002"}),
        );
        let mk = |p: &str, pricing: Value| json!({"provider": p, "model": "m", "route": {"modelConfig": {"pricing": pricing}}});
        let ts = vec![
            mk("a", json!({"source": "simple", "input": 3, "output": 15})),
            mk(
                "b",
                json!({"source": "simple", "input": 0.5, "output": 1.5}),
            ),
            mk(
                "c",
                json!({"source": "defined", "range": [{"lower_bound": 0, "upper_bound": 500, "input_per_m": 0.1}, {"lower_bound": 501, "input_per_m": 1, "output_per_m": 2}]}),
            ),
        ];
        // a = .003+.0075, b = .0005+.00075, c = .001+.001
        assert_eq!(CostSelector.select(&ts, &rt).await, Some(1));
        let ts2 = vec![
            mk("a", json!({"source": "simple", "input": 1, "output": 1})),
            mk("or", json!({"source": "openrouter", "slug": "or/x"})),
        ];
        // openrouter: 1000*1e-6 + 500*2e-6 = 0.002 > simple 0.0015
        assert_eq!(CostSelector.select(&ts2, &rt).await, Some(0));
        // No pricing → cost 0 → first such target wins.
        let ts3 = vec![
            mk("a", json!({"source": "simple", "input": 1})),
            json!({"provider": "z", "model": "m"}),
            json!({"provider": "y", "model": "m"}),
        ];
        assert_eq!(CostSelector.select(&ts3, &rt).await, Some(1));
        assert_eq!(CostSelector.select(&[], &rt).await, None);
        assert!(
            (calculate_price_for_target(
                Some(&json!({"source": "default", "input": 2, "output": 4})),
                1000.0,
                500.0,
                0.0,
                0.0,
                &rt
            ) - 0.004)
                .abs()
                < 1e-12
        );
    }

    #[test]
    fn parse_float_semantics() {
        assert_eq!(parse_float(Some(&json!("0.5abc"))), 0.5);
        assert_eq!(parse_float(Some(&json!(" 1e-6"))), 1e-6);
        assert_eq!(parse_float(Some(&json!(".5"))), 0.5);
        assert_eq!(parse_float(Some(&json!("5."))), 5.0);
        assert!(parse_float(Some(&json!("abc"))).is_nan());
        assert!(parse_float(Some(&json!("."))).is_nan());
        assert_eq!(parse_float(Some(&json!(2))), 2.0);
    }

    #[tokio::test]
    async fn latency_selector() {
        let mut rt = MockRuntime::new(json!({"performanceExplorationRate": 0}));
        rt.performance
            .insert("p1/m1".into(), vec![json!({"avg_ttft_ms": 100})]);
        rt.performance
            .insert("p2/m2".into(), vec![json!({"avg_ttft_ms": 50})]);
        rt.performance
            .insert("p3/m3".into(), vec![json!({"avg_ttft_ms": 200})]);
        assert_eq!(LatencySelector.select(&targets3(), &rt).await, Some(1));
        assert_eq!(LatencySelector.select(&targets3()[..1], &rt).await, Some(0));
        let empty = MockRuntime::new(json!({}));
        assert_eq!(LatencySelector.select(&targets3(), &empty).await, Some(0));
        // Data beats no data.
        let mut rt2 = MockRuntime::new(json!({}));
        rt2.performance
            .insert("p2/m2".into(), vec![json!({"avg_ttft_ms": 500})]);
        assert_eq!(LatencySelector.select(&targets3(), &rt2).await, Some(1));
    }

    #[tokio::test]
    async fn latency_exploration_prefers_unseen_then_stalest() {
        let mut rt =
            MockRuntime::new(json!({"latencyExplorationRate": 0.5})).with_randoms(&[0.1, 0.0]);
        rt.performance.insert(
            "p1/m1".into(),
            vec![json!({"avg_ttft_ms": 100, "sample_count": 5, "last_updated": 10})],
        );
        rt.performance.insert(
            "p2/m2".into(),
            vec![json!({"avg_ttft_ms": 50, "sample_count": 5, "last_updated": 20})],
        );
        // p3 has no data: unseen → explored.
        assert_eq!(LatencySelector.select(&targets3(), &rt).await, Some(2));
        let mut rt = MockRuntime::new(json!({"latencyExplorationRate": 0.5})).with_randoms(&[0.1]);
        rt.performance.insert(
            "p1/m1".into(),
            vec![json!({"avg_ttft_ms": 100, "sample_count": 5, "last_updated": 10})],
        );
        rt.performance.insert(
            "p2/m2".into(),
            vec![json!({"avg_ttft_ms": 50, "sample_count": 5, "last_updated": 20})],
        );
        assert_eq!(LatencySelector.select(&targets3()[..2], &rt).await, Some(0));
        // Roll above the rate → best.
        let mut rt = MockRuntime::new(json!({"latencyExplorationRate": 0.5})).with_randoms(&[0.9]);
        rt.performance.insert(
            "p1/m1".into(),
            vec![json!({"avg_ttft_ms": 100, "sample_count": 5, "last_updated": 10})],
        );
        rt.performance.insert(
            "p2/m2".into(),
            vec![json!({"avg_ttft_ms": 50, "sample_count": 5, "last_updated": 20})],
        );
        assert_eq!(LatencySelector.select(&targets3()[..2], &rt).await, Some(1));
        // Background exploration disables in-band exploration.
        let mut rt = MockRuntime::new(
            json!({"latencyExplorationRate": 1, "backgroundExploration": {"enabled": true}}),
        )
        .with_randoms(&[0.0]);
        rt.performance.insert(
            "p1/m1".into(),
            vec![json!({"avg_ttft_ms": 100, "sample_count": 5, "last_updated": 10})],
        );
        rt.performance.insert(
            "p2/m2".into(),
            vec![json!({"avg_ttft_ms": 50, "sample_count": 5, "last_updated": 20})],
        );
        assert_eq!(LatencySelector.select(&targets3()[..2], &rt).await, Some(1));
    }

    #[tokio::test]
    async fn performance_and_e2e() {
        let mut rt = MockRuntime::new(json!({"performanceExplorationRate": 0}));
        rt.performance.insert(
            "p1/m1".into(),
            vec![json!({"avg_tokens_per_sec": 10, "avg_e2e_tokens_per_sec": 90})],
        );
        rt.performance.insert(
            "p2/m2".into(),
            vec![json!({"avg_tokens_per_sec": 30, "avg_e2e_tokens_per_sec": 5})],
        );
        assert_eq!(PerformanceSelector.select(&targets3(), &rt).await, Some(1));
        assert_eq!(
            E2EPerformanceSelector.select(&targets3(), &rt).await,
            Some(0)
        );
        // Default 5% exploration: roll 0.01 explores the unseen p3.
        let mut rt = MockRuntime::new(json!({})).with_randoms(&[0.01, 0.0]);
        rt.performance.insert(
            "p1/m1".into(),
            vec![json!({"avg_tokens_per_sec": 10, "sample_count": 3})],
        );
        rt.performance.insert(
            "p2/m2".into(),
            vec![json!({"avg_tokens_per_sec": 30, "sample_count": 3})],
        );
        assert_eq!(PerformanceSelector.select(&targets3(), &rt).await, Some(2));
        // e2e rate key takes precedence.
        let rt = MockRuntime::new(
            json!({"e2ePerformanceExplorationRate": 0, "performanceExplorationRate": 1}),
        )
        .with_randoms(&[0.0]);
        assert_eq!(
            E2EPerformanceSelector.select(&targets3(), &rt).await,
            Some(0)
        );
        assert_eq!(rt.randoms.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn usage_selector() {
        let mut rt = MockRuntime::new(json!({}));
        rt.usage.insert("p1/m1".into(), 10.0);
        rt.usage.insert("p2/m2".into(), 2.0);
        rt.usage.insert("p3/m3".into(), 2.0);
        assert_eq!(UsageSelector.select(&targets3(), &rt).await, Some(1));
    }

    fn allowance(util: f64, resets_in_hours: f64, now: i64) -> Value {
        json!({"key": "w", "label": "W", "kind": "allowance", "unit": "requests", "limit": 100, "used": util, "remaining": 100.0 - util,
               "utilizationPercent": util, "resetsAt": iso_from_ms(now as f64 + resets_in_hours * MILLISECONDS_PER_HOUR), "status": "ok"})
    }
    fn result(meters: Vec<Value>) -> Value {
        json!({"checkerId": "c", "checkerType": "test", "provider": "x", "checkedAt": "", "success": true, "meters": meters})
    }

    #[tokio::test]
    async fn quota_selector() {
        let now = 1_700_000_000_000;
        let qt = |p: &str| json!({"provider": p, "model": "claude"});
        let mut rt = MockRuntime::new(json!({}));
        rt.now = Some(now);
        rt.quotas
            .insert("soon".into(), result(vec![allowance(0.0, 24.0, now)]));
        rt.quotas
            .insert("later".into(), result(vec![allowance(50.0, 72.0, now)]));
        rt.quotas.insert(
            "nearly-full".into(),
            result(vec![allowance(96.0, 1.0, now)]),
        );
        rt.quotas
            .insert("expired".into(), result(vec![allowance(0.0, -1.0, now)]));
        let mut unknown_util = allowance(0.0, 24.0, now);
        unknown_util["utilizationPercent"] = json!("unknown");
        unknown_util["used"] = json!(10);
        unknown_util["remaining"] = json!(90);
        rt.quotas
            .insert("available".into(), result(vec![unknown_util]));
        rt.quotas
            .insert("full".into(), result(vec![allowance(90.0, 24.0, now)]));
        rt.quotas.insert(
            "balance".into(),
            result(vec![
                json!({"limit": 100, "used": 10, "status": "ok", "utilizationPercent": 10}),
            ]),
        );
        let s = QuotaSelector::default();
        assert_eq!(s.select(&[], &rt).await, None);
        assert_eq!(s.select(&[qt("one")], &rt).await, Some(0));
        assert!(rt.quota_calls.lock().unwrap().is_empty());
        assert_eq!(s.select(&[qt("later"), qt("soon")], &rt).await, Some(1));
        assert_eq!(s.select(&[qt("full"), qt("available")], &rt).await, Some(1));
        let s = QuotaSelector::default();
        assert_eq!(
            s.select(&[qt("later"), qt("nearly-full")], &rt).await,
            Some(1)
        );
        // Expired-only → unknown, ranked after balance; balance after expiring.
        assert_eq!(
            s.select(&[qt("expired"), qt("balance")], &rt).await,
            Some(1)
        );
        assert_eq!(s.select(&[qt("balance"), qt("later")], &rt).await, Some(1));
        // Unknown providers keep declared order.
        assert_eq!(s.select(&[qt("x"), qt("y")], &rt).await, Some(0));
        // Cached per instance: each provider read once.
        let calls = rt.quota_calls.lock().unwrap().clone();
        assert_eq!(calls.iter().filter(|c| c.as_str() == "later").count(), 2);
        assert_eq!(
            get_utilization(&json!({"status": "exhausted"})),
            Some(100.0)
        );
        assert_eq!(
            get_utilization(&json!({"remaining": 0, "utilizationPercent": 5})),
            Some(100.0)
        );
        assert_eq!(
            get_utilization(&json!({"remaining": 25, "limit": 100})),
            Some(75.0)
        );
        assert_eq!(
            get_utilization(&json!({"utilizationPercent": "unknown"})),
            None
        );
        assert_eq!(
            get_burn_down_score(&json!({"success": false, "meters": []}), 0.0),
            None
        );
    }

    #[test]
    fn factory() {
        let rt = MockRuntime::new(json!({}));
        assert!(get_selector(None, &rt).is_ok());
        for k in [
            "random",
            "in_order",
            "cost",
            "performance",
            "e2e_performance",
            "latency",
            "usage",
            "quota",
        ] {
            assert!(get_selector(Some(&json!(k)), &rt).is_ok(), "{k}");
        }
        assert_eq!(
            get_selector(Some(&json!("bogus")), &rt)
                .err()
                .unwrap()
                .message,
            "Unknown selector type: bogus"
        );
        let mut rt = MockRuntime::new(json!({}));
        rt.no_usage_storage = true;
        assert_eq!(
            get_selector(Some(&json!("latency")), &rt)
                .err()
                .unwrap()
                .message,
            "UsageStorageService not initialized in SelectorFactory. Call setUsageStorage first."
        );
    }
}
