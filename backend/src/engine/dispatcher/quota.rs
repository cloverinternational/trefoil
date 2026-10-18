//! `services/quota/quota-enforcer.ts` (`loadQuotaContext`, `recordUsage`,
//! `selectHeaderQuota`) and `quota-middleware.ts` (`buildQuotaHeaders`,
//! `attachQuotaContext`) over Trefoil's `quota_state` table.
use crate::engine::config::Config;
use crate::engine::js::*;
use crate::engine::observability::{QuotaHooks, QuotaUsage};
use crate::engine::routing::route_candidates::most_constrained;
use crate::engine::routing::scope_match::{is_global_scope, scope_matches};
use crate::state::AppState;
use chrono::{Datelike, TimeZone, Utc};
use serde_json::{json, Value};
use sqlx::Row;

/// Pooled bucket owner for `shared: true` quotas.
pub const SHARED_OWNER: &str = "*";

/// `parse-duration` (ms) for the units Plexus quota durations use.
pub fn parse_duration(s: &str) -> Option<f64> {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"(?i)(-?(?:\d+\.?\d*|\.\d+)(?:e[-+]?\d+)?)\s*([a-zµ]*)").unwrap()
    });
    let mut total = 0.0;
    let mut any = false;
    for c in re.captures_iter(&s.replace([',', '_'], "")) {
        let n: f64 = c[1].parse().ok()?;
        let unit = c[2].to_lowercase();
        let mult = match unit.as_str() {
            "" | "ms" | "millisecond" | "milliseconds" | "msec" => 1.0,
            "ns" | "nanosecond" | "nanoseconds" => 1e-6,
            "us" | "µs" | "microsecond" | "microseconds" => 1e-3,
            "s" | "sec" | "secs" | "second" | "seconds" => 1000.0,
            "m" | "min" | "mins" | "minute" | "minutes" => 60_000.0,
            "h" | "hr" | "hrs" | "hour" | "hours" => 3_600_000.0,
            "d" | "day" | "days" => 86_400_000.0,
            "w" | "wk" | "wks" | "week" | "weeks" => 604_800_000.0,
            "b" | "month" | "months" => 2_629_746_000.0,
            "y" | "yr" | "yrs" | "year" | "years" => 31_556_952_000.0,
            _ => return None,
        };
        total += n * mult;
        any = true;
    }
    (any && total != 0.0).then_some(total)
}

fn window_start(kind: &str, now_ms: i64) -> i64 {
    let now = Utc
        .timestamp_millis_opt(now_ms)
        .single()
        .unwrap_or_else(Utc::now);
    let day = now.date_naive().and_hms_opt(0, 0, 0).unwrap().and_utc();
    match kind {
        "daily" => day.timestamp_millis(),
        "weekly" => (day - chrono::Duration::days(now.weekday().num_days_from_monday() as i64))
            .timestamp_millis(),
        _ => Utc
            .with_ymd_and_hms(now.year(), now.month(), 1, 0, 0, 0)
            .single()
            .unwrap()
            .timestamp_millis(),
    }
}

fn window_end(kind: &str, start: i64) -> i64 {
    match kind {
        "daily" => start + 86_400_000,
        "weekly" => start + 7 * 86_400_000,
        _ => {
            let d = Utc.timestamp_millis_opt(start).single().unwrap();
            let (y, m) = if d.month() == 12 {
                (d.year() + 1, 1)
            } else {
                (d.year(), d.month() + 1)
            };
            Utc.with_ymd_and_hms(y, m, 1, 0, 0, 0)
                .single()
                .unwrap()
                .timestamp_millis()
        }
    }
}

fn align(now: f64, duration: f64) -> f64 {
    (now / duration).floor() * duration
}

/// `resolveQuotaNames(keyConfig, config)`.
fn resolve_quota_names(key: &Value, config: &Config) -> Option<(Vec<String>, &'static str)> {
    let names = |v: Option<&Value>| {
        v.and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .filter(|a| !a.is_empty())
    };
    if let Some(n) = names(key.get("quotas")) {
        return Some((n, "assigned"));
    }
    names(config.root.get("default_quotas")).map(|n| (n, "default"))
}

fn scope_of(def: &Value) -> Value {
    obj(vec![
        ("allowedModels", def.get("allowedModels").cloned()),
        ("allowedProviders", def.get("allowedProviders").cloned()),
        ("excludedModels", def.get("excludedModels").cloned()),
        ("excludedProviders", def.get("excludedProviders").cloned()),
    ])
}

struct Row_ {
    limit_type: String,
    current_usage: f64,
    last_updated: i64,
    window_start: Option<i64>,
}

/// `computeSnapshot(name, def, row, nowMs, source)`.
fn compute_snapshot(
    name: &str,
    def: &Value,
    row: Option<&Row_>,
    now: i64,
    source: &str,
) -> Option<Value> {
    let scope = scope_of(def);
    let global = is_global_scope(&scope);
    let shared = def.get("shared") == Some(&Value::Bool(true));
    let kind = get_str(def, &["type"]).unwrap_or("");
    let limit_type = get_str(def, &["limitType"]).unwrap_or("");
    let limit = get_f64(def, &["limit"]).unwrap_or(0.0);
    let (current, resets_at) = if matches!(kind, "daily" | "weekly" | "monthly") {
        let expected = window_start(kind, now);
        let stale =
            row.is_none_or(|r| r.window_start != Some(expected) || r.limit_type != limit_type);
        (
            if stale {
                0.0
            } else {
                row.unwrap().current_usage
            },
            window_end(kind, expected) as f64,
        )
    } else if kind == "rolling" && limit_type == "cost" {
        let d = parse_duration(get_str(def, &["duration"]).unwrap_or(""))?;
        let expected = align(now as f64, d);
        let stale = row.is_none_or(|r| {
            r.window_start.map(|w| w as f64) != Some(expected) || r.limit_type != limit_type
        });
        (
            if stale {
                0.0
            } else {
                row.unwrap().current_usage
            },
            expected + d,
        )
    } else {
        let d = parse_duration(get_str(def, &["duration"]).unwrap_or(""))?;
        let current = match row.filter(|r| r.limit_type == limit_type) {
            None => 0.0,
            Some(r) => {
                (r.current_usage - ((now - r.last_updated).max(0) as f64) * (limit / d)).max(0.0)
            }
        };
        (current, now as f64 + (current / limit) * d)
    };
    let remaining = (limit - current).max(0.0);
    let (display_usage, display_remaining) = if limit_type == "cost" {
        (current, remaining)
    } else {
        (js_round(current), js_round(remaining))
    };
    let mut snap = json!({
        "quotaName": name, "limitType": limit_type, "limit": num(limit), "currentUsage": num(display_usage), "remaining": num(display_remaining),
        "allowed": current < limit, "resetsAtMs": num(resets_at), "scope": scope, "global": global, "shared": shared,
    });
    if let Some(w) = def.get("warnAt") {
        snap["warnAt"] = w.clone();
    }
    snap["source"] = json!(source);
    Some(snap)
}

/// `Math.round`.
fn js_round(x: f64) -> f64 {
    (x + 0.5).floor()
}

/// `QuotaEnforcer.loadQuotaContext(keyName)`.
pub async fn load_quota_context(
    state: &AppState,
    config: &Config,
    key_name: &str,
) -> Option<Value> {
    let key = get(&config.root, &["keys", key_name])?;
    let (names, source) = resolve_quota_names(key, config)?;
    let mut defs = Vec::new();
    for name in names {
        let Some(def) = get(&config.root, &["user_quotas", &name]).filter(|d| truthy(Some(d)))
        else {
            continue;
        };
        let owner = if def.get("shared") == Some(&Value::Bool(true)) {
            SHARED_OWNER.to_owned()
        } else {
            key_name.to_owned()
        };
        defs.push((name, def.clone(), owner));
    }
    if defs.is_empty() {
        return Some(json!({"keyName": key_name, "checks": [], "blockedGlobal": null}));
    }
    let mut rows = std::collections::HashMap::new();
    if let Some(pool) = &state.inner.database {
        let names: Vec<String> = defs.iter().map(|d| d.0.clone()).collect();
        let owners = [key_name.to_owned(), SHARED_OWNER.to_owned()];
        if let Ok(found) = sqlx::query("SELECT key_name, quota_name, limit_type, current_usage::double precision AS current_usage, last_updated, window_start FROM quota_state WHERE key_name = ANY($1) AND quota_name = ANY($2)")
            .bind(&owners[..]).bind(&names[..]).fetch_all(pool).await {
            for r in found {
                let k: String = r.try_get("key_name").unwrap_or_default();
                let q: String = r.try_get("quota_name").unwrap_or_default();
                rows.insert(format!("{k}::{q}"), Row_ {
                    limit_type: r.try_get("limit_type").unwrap_or_default(),
                    current_usage: r.try_get::<f64, _>("current_usage").unwrap_or(0.0),
                    last_updated: r.try_get("last_updated").unwrap_or(0),
                    window_start: r.try_get("window_start").ok().flatten(),
                });
            }
        }
    }
    let now = now_ms();
    let checks: Vec<Value> = defs
        .iter()
        .filter_map(|(name, def, owner)| {
            compute_snapshot(
                name,
                def,
                rows.get(&format!("{owner}::{name}")),
                now,
                source,
            )
        })
        .collect();
    let blocked = checks
        .iter()
        .find(|c| truthy(c.get("global")) && !truthy(c.get("allowed")))
        .cloned()
        .unwrap_or(Value::Null);
    Some(json!({"keyName": key_name, "checks": checks, "blockedGlobal": blocked}))
}

/// `buildQuotaHeaders(ctx, provider, model)`.
pub fn quota_headers(ctx: &Value, provider: &str, model: &str) -> Vec<(String, String)> {
    let applicable: Vec<Value> = ctx
        .get("checks")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|c| scope_matches(c.get("scope").unwrap_or(&Value::Null), provider, model))
        .cloned()
        .collect();
    let Some(q) = most_constrained(&applicable) else {
        return vec![];
    };
    let mut out = vec![
        ("x-plexus-quota".into(), to_string(q.get("quotaName"))),
        ("x-plexus-quota-limit".into(), to_string(q.get("limit"))),
        (
            "x-plexus-quota-remaining".into(),
            to_string(q.get("remaining")),
        ),
        (
            "x-plexus-quota-reset".into(),
            crate::engine::routing::iso_from_ms(get_f64(q, &["resetsAtMs"]).unwrap_or(f64::NAN)),
        ),
    ];
    let limit = get_f64(q, &["limit"]).unwrap_or(0.0);
    if let Some(w) = q.get("warnAt").and_then(Value::as_f64) {
        if limit > 0.0 && get_f64(q, &["currentUsage"]).unwrap_or(0.0) / limit >= w {
            out.push((
                "x-plexus-quota-warning".into(),
                to_string(q.get("quotaName")),
            ));
        }
    }
    out
}

/// `attachQuotaContext(unifiedRequest, ctx)`.
pub fn attach_quota_context(request: &mut Value, ctx: &Value) {
    let Some(map) = request.as_object_mut() else {
        return;
    };
    let meta = map.entry("metadata").or_insert_with(|| json!({}));
    if !meta.is_object() {
        *meta = json!({});
    }
    let pm = meta
        .as_object_mut()
        .unwrap()
        .entry("plexus_metadata")
        .or_insert_with(|| json!({}));
    if !pm.is_object() {
        *pm = json!({});
    }
    pm["plexus_quota_context"] = ctx.clone();
}

/// `QuotaEnforcer.recordUsage(keyName, provider, model, usage)`.
pub async fn record_usage(
    state: &AppState,
    config: &Config,
    key_name: &str,
    provider: &str,
    model: &str,
    usage: &QuotaUsage,
) {
    let Some(key) = get(&config.root, &["keys", key_name]) else {
        return;
    };
    let Some((names, _)) = resolve_quota_names(key, config) else {
        return;
    };
    let Some(pool) = &state.inner.database else {
        return;
    };
    let now = now_ms();
    for name in names {
        let Some(def) = get(&config.root, &["user_quotas", &name]).filter(|d| truthy(Some(d)))
        else {
            continue;
        };
        if !scope_matches(&scope_of(def), provider, model) {
            continue;
        }
        let owner = if def.get("shared") == Some(&Value::Bool(true)) {
            SHARED_OWNER
        } else {
            key_name
        };
        let limit_type = get_str(def, &["limitType"]).unwrap_or("").to_owned();
        let kind = get_str(def, &["type"]).unwrap_or("");
        let z = |v: Option<f64>| v.filter(|x| !x.is_nan()).unwrap_or(0.0);
        let value = match limit_type.as_str() {
            "requests" => 1.0,
            "cost" => z(usage.cost_total),
            _ => {
                z(usage.tokens_input)
                    + z(usage.tokens_output)
                    + z(usage.tokens_reasoning)
                    + z(usage.tokens_cached)
                    + z(usage.tokens_cache_write)
            }
        };
        let duration = parse_duration(get_str(def, &["duration"]).unwrap_or(""));
        if kind == "rolling" && limit_type != "cost" {
            let leak = duration
                .map(|d| get_f64(def, &["limit"]).unwrap_or(0.0) / d)
                .unwrap_or(0.0);
            let _ = sqlx::query("INSERT INTO quota_state (key_name, quota_name, limit_type, current_usage, last_updated, window_start) VALUES ($1, $2, $3, $4, $5, NULL) ON CONFLICT (key_name, quota_name) DO UPDATE SET current_usage = CASE WHEN quota_state.limit_type != $3 THEN $4 ELSE GREATEST(0, quota_state.current_usage - ($5 - quota_state.last_updated) * $6::double precision) + $4 END, last_updated = $5, limit_type = $3")
                .bind(owner).bind(&name).bind(&limit_type).bind(value).bind(now).bind(leak).execute(pool).await;
            continue;
        }
        let expected = if kind == "rolling" {
            duration.map(|d| align(now as f64, d) as i64).unwrap_or(now)
        } else {
            window_start(kind, now)
        };
        let _ = sqlx::query("INSERT INTO quota_state (key_name, quota_name, limit_type, current_usage, last_updated, window_start) VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (key_name, quota_name) DO UPDATE SET current_usage = CASE WHEN quota_state.window_start IS NULL OR quota_state.window_start != $6 OR quota_state.limit_type != $3 THEN $4 ELSE quota_state.current_usage + $4 END, last_updated = $5, window_start = $6, limit_type = $3")
            .bind(owner).bind(&name).bind(&limit_type).bind(value).bind(now).bind(expected).execute(pool).await;
    }
}

/// `QuotaHooks` for the response handler.
pub struct QuotaRecorder {
    pub state: AppState,
}

#[async_trait::async_trait]
impl QuotaHooks for QuotaRecorder {
    fn quota_headers(
        &self,
        quota_context: &Value,
        provider: &str,
        model: &str,
    ) -> Vec<(String, String)> {
        quota_headers(quota_context, provider, model)
    }
    async fn record_usage(&self, key_name: &str, provider: &str, model: &str, usage: &QuotaUsage) {
        let config = super::engine::build_config(&self.state);
        record_usage(&self.state, &config, key_name, provider, model, usage).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn durations_and_windows() {
        assert_eq!(parse_duration("1h"), Some(3_600_000.0));
        assert_eq!(parse_duration("1h30m"), Some(5_400_000.0));
        assert_eq!(parse_duration("nope"), None);
        let monday = window_start("weekly", 1_760_620_800_000); // 2025-10-16 (Thu)
        assert_eq!(monday, 1_760_313_600_000);
        assert_eq!(
            window_end("monthly", window_start("monthly", 1_760_620_800_000)),
            1_761_955_200_000
        );
    }
}

/// `QuotaScheduler.getLatestQuotaForProvider(provider)`: the latest
/// non-stale `MeterCheckResult` of the provider's checker (`quota_checker.id
/// ?? provider`), dropped once older than two check intervals.
pub async fn latest_quota_for_provider(
    state: &AppState,
    config: &Config,
    provider: &str,
) -> Option<Value> {
    let pc = config.provider(provider)?;
    if pc.get("enabled") == Some(&Value::Bool(false)) {
        return None;
    }
    let checker = pc.get("quota_checker").filter(|q| q.is_object())?;
    if checker.get("enabled") == Some(&Value::Bool(false)) {
        return None;
    }
    let id = get_str(checker, &["id"])
        .map(str::to_owned)
        .unwrap_or_else(|| provider.to_owned())
        .trim()
        .to_owned();
    let interval = checker
        .get("intervalMinutes")
        .and_then(Value::as_f64)
        .unwrap_or(30.0);
    let pool = state.inner.database.as_ref()?;
    let rows = sqlx::query(
        "SELECT * FROM meter_snapshots WHERE checker_id=$1 ORDER BY checked_at DESC LIMIT 200",
    )
    .bind(&id)
    .fetch_all(pool)
    .await
    .ok()?;
    let first = rows.first()?;
    let checked = |r: &sqlx::postgres::PgRow| r.try_get::<i64, _>("checked_at").unwrap_or_default();
    let latest_ms = checked(first);
    let latest: Vec<&sqlx::postgres::PgRow> =
        rows.iter().filter(|r| checked(r) == latest_ms).collect();
    // A failed latest check makes the snapshot stale (or absent): never routable.
    if latest
        .iter()
        .any(|r| !r.try_get::<bool, _>("success").unwrap_or(false))
    {
        return None;
    }
    let meters: Vec<Value> = latest
        .iter()
        .filter(|r| {
            !matches!(
                r.try_get::<String, _>("meter_key")
                    .unwrap_or_default()
                    .as_str(),
                "_empty" | "_error"
            )
        })
        .map(|r| crate::config_routes::meter_json(r))
        .collect();
    if now_ms() - latest_ms > (interval * 2.0 * 60_000.0) as i64 {
        return None;
    }
    let checker_type = get_str(checker, &["type"])
        .map(str::to_owned)
        .unwrap_or_else(|| {
            latest[0]
                .try_get::<String, _>("checker_type")
                .unwrap_or_default()
        });
    let checked_at = chrono::DateTime::from_timestamp_millis(latest_ms)
        .map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
    Some(
        json!({"checkerId": id, "checkerType": checker_type, "provider": provider, "checkedAt": checked_at, "success": true, "meters": meters}),
    )
}
