//! `utils/calculate-costs.ts`, `utils/provider-cost.ts` and
//! `services/observability/pricing-manager.ts`, plus the SSE `: cost` /
//! `: energy` comment readers from `inspectors/debug-logging.ts`.
use super::record::{js_stringify, parse_float, to_fixed_num, UsageRecord};
use crate::engine::js::{self, get, nullish, num, truthy, Obj};
use regex::Regex;
use serde_json::Value;
use std::sync::OnceLock;

/// `CostAttribution` (values are the raw `plexus.*` JSON members; `None` is
/// `undefined`, so a JSON `null` `pricingFallback` still counts as defined).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CostAttribution {
    pub upstream_model: Option<Value>,
    pub pricing_model: Option<Value>,
    pub pricing_fallback: Option<Value>,
}

impl CostAttribution {
    /// `{ upstreamModel: plexus?.upstreamModel, pricingModel: plexus?.pricingModel, pricingFallback: plexus?.pricingFallback }`.
    pub fn from_plexus(plexus: Option<&Value>) -> Self {
        let g = |k: &str| plexus.and_then(|p| p.get(k)).cloned();
        Self {
            upstream_model: g("upstreamModel"),
            pricing_model: g("pricingModel"),
            pricing_fallback: g("pricingFallback"),
        }
    }

    fn keys(&self) -> Obj {
        let mut out = Obj::new();
        let any = truthy(self.upstream_model.as_ref())
            || truthy(self.pricing_model.as_ref())
            || self.pricing_fallback.is_some();
        if !any {
            return out;
        }
        if truthy(self.upstream_model.as_ref()) {
            out.insert(
                "upstream_model".into(),
                self.upstream_model.clone().unwrap_or(Value::Null),
            );
        }
        if truthy(self.pricing_model.as_ref()) {
            out.insert(
                "pricing_model".into(),
                self.pricing_model.clone().unwrap_or(Value::Null),
            );
        }
        if let Some(f) = &self.pricing_fallback {
            out.insert("pricing_fallback".into(), f.clone());
        }
        out
    }
}

/// OpenRouter pricing lookup (`ModelMetadataManager.getMetadata('openrouter', slug)?.pricing`).
/// Returns the raw pricing object (`{ prompt, completion, input_cache_read?, … }`,
/// string rates per token) or `None`.
pub trait OpenRouterCatalog: Send + Sync {
    fn openrouter_pricing(&self, slug: &str) -> Option<Value>;
    /// `isInitialized('openrouter')`.
    fn is_initialized(&self) -> bool {
        true
    }
    /// `getAllIds('openrouter')`.
    fn all_slugs(&self) -> Vec<String> {
        Vec::new()
    }
}

/// `PricingManager`: a stateless facade over the OpenRouter catalog.
pub struct PricingManager<'a> {
    pub catalog: &'a dyn OpenRouterCatalog,
}

impl<'a> PricingManager<'a> {
    pub fn new(catalog: &'a dyn OpenRouterCatalog) -> Self {
        Self { catalog }
    }

    /// `getPricing(slug)`: `-1` prompt/completion (router / variable priced)
    /// means "unknown" → `None`.
    pub fn get_pricing(&self, slug: &str) -> Option<Value> {
        let pricing = self.catalog.openrouter_pricing(slug)?;
        if parse_float(pricing.get("prompt")) < 0.0 || parse_float(pricing.get("completion")) < 0.0
        {
            return None;
        }
        Some(pricing)
    }
    pub fn is_initialized(&self) -> bool {
        self.catalog.is_initialized()
    }
    pub fn get_all_model_slugs(&self) -> Vec<String> {
        self.catalog.all_slugs()
    }

    /// `searchModelSlugs(query)`: substring match, prefix matches first, then
    /// `localeCompare` (approximated by a case-insensitive, then exact, compare).
    pub fn search_model_slugs(&self, query: &str) -> Vec<String> {
        if query.is_empty() {
            return self.get_all_model_slugs();
        }
        let q = query.to_lowercase();
        let mut out: Vec<String> = self
            .get_all_model_slugs()
            .into_iter()
            .filter(|s| s.to_lowercase().contains(&q))
            .collect();
        out.sort_by(|a, b| {
            let (al, bl) = (a.to_lowercase(), b.to_lowercase());
            match (al.starts_with(&q), bl.starts_with(&q)) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => al.cmp(&bl).then_with(|| a.cmp(b)),
            }
        });
        out
    }
}

fn number_of(v: Option<&Value>) -> f64 {
    js::to_number(v).unwrap_or(f64::NAN)
}
/// `x || 0` as a number.
fn or_zero(v: Option<&Value>) -> f64 {
    if truthy(v) {
        number_of(v)
    } else {
        0.0
    }
}

/// `calculateCosts(usageRecord, pricing, providerDiscount, attribution)`.
/// `pricing` / `provider_discount` are the raw `plexus.pricing` /
/// `plexus.providerDiscount` values; `catalog` backs `source: 'openrouter'`.
pub fn calculate_costs(
    record: &mut UsageRecord,
    pricing: Option<&Value>,
    provider_discount: Option<&Value>,
    attribution: Option<&CostAttribution>,
    catalog: Option<&dyn OpenRouterCatalog>,
) {
    let input_tokens = record.num_or_zero("tokensInput");
    let output_tokens = record.num_or_zero("tokensOutput");
    let cached_tokens = record.num_or_zero("tokensCached");
    let cache_write_tokens = record.num_or_zero("tokensCacheWrite");

    let (mut input_cost, mut output_cost, mut cached_cost, mut cache_write_cost) =
        (0.0, 0.0, 0.0, 0.0);
    let mut calculated = false;
    let attribution_keys = attribution.map(CostAttribution::keys).unwrap_or_default();
    let with_attr = |mut base: Obj| {
        for (k, v) in &attribution_keys {
            base.insert(k.clone(), v.clone());
        }
        Value::Object(base)
    };

    record.set("costSource", "default");
    let mut default_meta = Obj::new();
    for k in ["input", "output", "cached", "cache_write"] {
        default_meta.insert(k.into(), Value::from(0));
    }
    record.set("costMetadata", js_stringify(&with_attr(default_meta)));

    let Some(pricing) = pricing.filter(|p| truthy(Some(p))) else {
        return;
    };
    let source = get(pricing, &["source"]).and_then(Value::as_str);
    // `pricing.discount ?? providerDiscount`
    let effective_discount = js::coalesce(pricing.get("discount"), provider_discount);
    let apply_discount = |costs: [&mut f64; 4]| {
        if truthy(effective_discount.as_ref()) {
            let multiplier = 1.0 - number_of(effective_discount.as_ref());
            for c in costs {
                *c *= multiplier;
            }
        }
    };
    // `discount: effectiveDiscount` (undefined is dropped by JSON.stringify).
    let set_discount = |m: &mut Obj| match &effective_discount {
        Some(d) => {
            m.insert("discount".into(), d.clone());
        }
        None => {
            m.remove("discount");
        }
    };

    if source == Some("simple") {
        input_cost = (input_tokens / 1_000_000.0) * number_of(pricing.get("input"));
        output_cost = (output_tokens / 1_000_000.0) * number_of(pricing.get("output"));
        cached_cost = (cached_tokens / 1_000_000.0) * or_zero(pricing.get("cached"));
        cache_write_cost = (cache_write_tokens / 1_000_000.0) * or_zero(pricing.get("cache_write"));
        calculated = true;
        apply_discount([
            &mut input_cost,
            &mut output_cost,
            &mut cached_cost,
            &mut cache_write_cost,
        ]);
        record.set("costSource", "simple");
        let mut meta = pricing.as_object().cloned().unwrap_or_default();
        set_discount(&mut meta);
        record.set("costMetadata", js_stringify(&with_attr(meta)));
    } else if source == Some("defined") && pricing.get("range").is_some_and(Value::is_array) {
        let ranges = pricing["range"].as_array().cloned().unwrap_or_default();
        let found = ranges.iter().find(|r| {
            let lower = js::coalesce(r.get("lower_bound"), None)
                .map(|v| number_of(Some(&v)))
                .unwrap_or(0.0);
            let upper = js::coalesce(r.get("upper_bound"), None)
                .map(|v| number_of(Some(&v)))
                .unwrap_or(f64::INFINITY);
            input_tokens >= lower && input_tokens <= upper
        });
        if let Some(m) = found {
            input_cost = (input_tokens / 1_000_000.0) * number_of(m.get("input_per_m"));
            output_cost = (output_tokens / 1_000_000.0) * number_of(m.get("output_per_m"));
            cached_cost = (cached_tokens / 1_000_000.0) * or_zero(m.get("cached_per_m"));
            cache_write_cost =
                (cache_write_tokens / 1_000_000.0) * or_zero(m.get("cache_write_per_m"));
            calculated = true;
            apply_discount([
                &mut input_cost,
                &mut output_cost,
                &mut cached_cost,
                &mut cache_write_cost,
            ]);
            record.set("costSource", "defined");
            let or0 = |k: &str| {
                if truthy(m.get(k)) {
                    m[k].clone()
                } else {
                    Value::from(0)
                }
            };
            let mut meta = Obj::new();
            meta.insert("source".into(), "defined".into());
            js::set_opt(&mut meta, "input", m.get("input_per_m").cloned());
            js::set_opt(&mut meta, "output", m.get("output_per_m").cloned());
            meta.insert("cached".into(), or0("cached_per_m"));
            meta.insert("cache_write".into(), or0("cache_write_per_m"));
            meta.insert("range".into(), m.clone());
            set_discount(&mut meta);
            record.set("costMetadata", js_stringify(&with_attr(meta)));
        }
    } else if source == Some("openrouter") && truthy(pricing.get("slug")) {
        let slug = js::to_string(pricing.get("slug"));
        let or_pricing = catalog.and_then(|c| PricingManager::new(c).get_pricing(&slug));
        if let Some(orp) = or_pricing {
            let rate = |v: Option<&Value>| {
                let n = parse_float(v);
                if n.is_nan() || n == 0.0 {
                    0.0
                } else {
                    n
                }
            };
            let fallback0 = |k: &str| {
                if truthy(orp.get(k)) {
                    orp[k].clone()
                } else {
                    Value::from("0")
                }
            };
            let prompt_rate = rate(orp.get("prompt"));
            let completion_rate = rate(orp.get("completion"));
            let cache_read_rate = rate(Some(&fallback0("input_cache_read")));
            let cache_write_rate = rate(Some(&fallback0("input_cache_write")));
            input_cost = input_tokens * prompt_rate;
            output_cost = output_tokens * completion_rate;
            cached_cost = cached_tokens * cache_read_rate;
            cache_write_cost = cache_write_tokens * cache_write_rate;
            apply_discount([
                &mut input_cost,
                &mut output_cost,
                &mut cached_cost,
                &mut cache_write_cost,
            ]);
            calculated = true;
            record.set("costSource", "openrouter");
            let mut meta = Obj::new();
            meta.insert("slug".into(), pricing["slug"].clone());
            meta.insert("prompt".into(), num(prompt_rate));
            meta.insert("completion".into(), num(completion_rate));
            meta.insert("input_cache_read".into(), num(cache_read_rate));
            meta.insert("input_cache_write".into(), num(cache_write_rate));
            set_discount(&mut meta);
            record.set("costMetadata", js_stringify(&with_attr(meta)));
        }
    } else if source == Some("per_request") {
        input_cost = number_of(pricing.get("amount"));
        output_cost = 0.0;
        cached_cost = 0.0;
        cache_write_cost = 0.0;
        calculated = true;
        record.set("costSource", "per_request");
        let mut meta = Obj::new();
        js::set_opt(&mut meta, "amount", pricing.get("amount").cloned());
        record.set("costMetadata", js_stringify(&with_attr(meta)));
    }

    if calculated {
        record.set_num("costInput", to_fixed_num(input_cost, 8));
        record.set_num("costOutput", to_fixed_num(output_cost, 8));
        record.set_num("costCached", to_fixed_num(cached_cost, 8));
        record.set_num("costCacheWrite", to_fixed_num(cache_write_cost, 8));
        record.set_num(
            "costTotal",
            to_fixed_num(input_cost + output_cost + cached_cost + cache_write_cost, 8),
        );
    }
}

/// `applyProviderReportedCost(record, costData)` for SSE `: cost {...}` payloads.
pub fn apply_provider_reported_cost(record: &mut UsageRecord, cost_data: &Value) {
    let Some(request_cost) = cost_data.get("request_cost_usd").and_then(Value::as_f64) else {
        return;
    };
    if request_cost < 0.0 {
        return;
    }
    let previous_source = record.get("costSource").cloned();
    let previous_total = record.get("costTotal").cloned();
    record.set_num("costTotal", to_fixed_num(request_cost, 8));
    record.set("costSource", "provider_reported");
    record.set(
        "providerReportedCost",
        cost_data["request_cost_usd"].clone(),
    );

    let input = record.num_or_zero("costInput");
    let output = record.num_or_zero("costOutput");
    let cached = record.num_or_zero("costCached");
    let cache_write = record.num_or_zero("costCacheWrite");
    let total = input + output + cached + cache_write;
    if total > 0.0 {
        record.set_num("costInput", to_fixed_num((input / total) * request_cost, 8));
        record.set_num(
            "costOutput",
            to_fixed_num((output / total) * request_cost, 8),
        );
        record.set_num(
            "costCached",
            to_fixed_num((cached / total) * request_cost, 8),
        );
        record.set_num(
            "costCacheWrite",
            to_fixed_num((cache_write / total) * request_cost, 8),
        );
    } else {
        record.set_num("costInput", to_fixed_num(request_cost, 8));
        for k in ["costOutput", "costCached", "costCacheWrite"] {
            record.set(k, 0);
        }
    }
    let mut meta = Obj::new();
    meta.insert("source".into(), "provider_reported".into());
    meta.insert(
        "request_cost_usd".into(),
        cost_data["request_cost_usd"].clone(),
    );
    for k in [
        "cache_savings_usd",
        "allowance_remaining_usd",
        "budget_remaining_usd",
    ] {
        js::set_opt(&mut meta, k, cost_data.get(k).cloned());
    }
    js::set_opt(&mut meta, "previous_cost_source", previous_source);
    js::set_opt(&mut meta, "previous_cost_total", previous_total);
    record.set("costMetadata", js_stringify(&Value::Object(meta)));
}

/// `applyUsageCostDetails(record, costDetails)` (output of
/// `usage::extract_cost_details`): superset / normal / minimal tiers.
pub fn apply_usage_cost_details(record: &mut UsageRecord, details: &Value) {
    let f = |k: &str| details.get(k).and_then(Value::as_f64);
    if !details.is_object() || nullish(details.get("total_cost")) {
        return;
    }
    let total = f("total_cost").unwrap_or(f64::NAN);
    let previous_source = record.get("costSource").cloned();
    let previous_total = record.get("costTotal").cloned();
    record.set_num("costTotal", to_fixed_num(total, 8));
    record.set("costSource", "provider_reported");
    record.set("providerReportedCost", details["total_cost"].clone());

    let (input, output, cached, cache_write) = (
        f("input_cost"),
        f("output_cost"),
        f("cached_input_cost"),
        f("cache_write_input_cost"),
    );
    let prompt_up = f("upstream_inference_prompt_cost");
    let completions_up = f("upstream_inference_completions_cost");
    if input.is_some() || cached.is_some() || cache_write.is_some() {
        record.set_num("costInput", to_fixed_num(input.unwrap_or(0.0), 8));
        record.set_num("costOutput", to_fixed_num(output.unwrap_or(0.0), 8));
        record.set_num("costCached", to_fixed_num(cached.unwrap_or(0.0), 8));
        record.set_num(
            "costCacheWrite",
            to_fixed_num(cache_write.unwrap_or(0.0), 8),
        );
    } else if prompt_up.is_some_and(|v| v > 0.0) || completions_up.is_some_and(|v| v > 0.0) {
        let prompt_total = prompt_up.unwrap_or(0.0);
        record.set_num("costOutput", to_fixed_num(completions_up.unwrap_or(0.0), 8));
        let prev_input = record.num_or_zero("costInput");
        let prev_cached = record.num_or_zero("costCached");
        let prev_cw = record.num_or_zero("costCacheWrite");
        let prev_prompt = prev_input + prev_cached + prev_cw;
        if prev_prompt > 0.0 {
            record.set_num(
                "costInput",
                to_fixed_num((prev_input / prev_prompt) * prompt_total, 8),
            );
            record.set_num(
                "costCached",
                to_fixed_num((prev_cached / prev_prompt) * prompt_total, 8),
            );
            record.set_num(
                "costCacheWrite",
                to_fixed_num((prev_cw / prev_prompt) * prompt_total, 8),
            );
        } else {
            record.set_num("costInput", to_fixed_num(prompt_total, 8));
            record.set("costCached", 0);
            record.set("costCacheWrite", 0);
        }
    } else {
        let pi = record.num_or_zero("costInput");
        let po = record.num_or_zero("costOutput");
        let pc = record.num_or_zero("costCached");
        let pw = record.num_or_zero("costCacheWrite");
        let calc = pi + po + pc + pw;
        if calc > 0.0 {
            record.set_num("costInput", to_fixed_num((pi / calc) * total, 8));
            record.set_num("costOutput", to_fixed_num((po / calc) * total, 8));
            record.set_num("costCached", to_fixed_num((pc / calc) * total, 8));
            record.set_num("costCacheWrite", to_fixed_num((pw / calc) * total, 8));
        } else {
            record.set_num("costInput", to_fixed_num(total, 8));
            for k in ["costOutput", "costCached", "costCacheWrite"] {
                record.set(k, 0);
            }
        }
    }
    let mut meta = Obj::new();
    meta.insert("source".into(), "provider_reported".into());
    meta.insert("cost_details".into(), details.clone());
    js::set_opt(&mut meta, "previous_cost_source", previous_source);
    js::set_opt(&mut meta, "previous_cost_total", previous_total);
    record.set("costMetadata", js_stringify(&Value::Object(meta)));
}

fn comment_re(kind: &str) -> &'static Regex {
    static COST: OnceLock<Regex> = OnceLock::new();
    static ENERGY: OnceLock<Regex> = OnceLock::new();
    // `/^:\s*cost\s+(\{.+\})\s*$/` (no `s` flag: `.` excludes line terminators).
    let cell = if kind == "cost" { &COST } else { &ENERGY };
    cell.get_or_init(|| {
        Regex::new(&format!(
            r"^:\s*{kind}\s+(\{{[^\n\r\u{{2028}}\u{{2029}}]+\}})\s*$"
        ))
        .expect("valid regex")
    })
}

fn last_comment_payload(full_body: &str, kind: &str) -> Option<Value> {
    let mut last = None;
    for line in split_lines(full_body) {
        if let Some(c) = comment_re(kind).captures(line) {
            if let Some(v) = js::parse(&c[1]) {
                last = Some(v);
            }
        }
    }
    last
}

/// `fullBody.split(/\r?\n/)`.
pub fn split_lines(s: &str) -> impl Iterator<Item = &str> {
    s.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l))
}

/// `extractProviderCostFromSSEComments`: last parseable `: cost {json}` line.
pub fn provider_cost_from_sse_comments(full_body: &str) -> Option<Value> {
    last_comment_payload(full_body, "cost")
}
/// `extractProviderEnergyFromSSEComments`: last parseable `: energy {json}` line.
pub fn provider_energy_from_sse_comments(full_body: &str) -> Option<Value> {
    last_comment_payload(full_body, "energy")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rec(v: Value) -> UsageRecord {
        UsageRecord::from_value(v)
    }
    fn meta(r: &UsageRecord) -> Value {
        js::parse(r.str("costMetadata").unwrap()).unwrap()
    }

    struct Catalog(Value);
    impl OpenRouterCatalog for Catalog {
        fn openrouter_pricing(&self, slug: &str) -> Option<Value> {
            self.0.get(slug).cloned()
        }
        fn all_slugs(&self) -> Vec<String> {
            self.0.as_object().unwrap().keys().cloned().collect()
        }
    }

    #[test]
    fn default_metadata_without_pricing() {
        let mut r = rec(json!({"tokensInput": 10}));
        calculate_costs(&mut r, None, None, None, None);
        assert_eq!(r.str("costSource"), Some("default"));
        assert_eq!(
            r.str("costMetadata"),
            Some(r#"{"input":0,"output":0,"cached":0,"cache_write":0}"#)
        );
        assert!(r.get("costTotal").is_none());
    }

    #[test]
    fn simple_pricing_with_attribution() {
        let mut r = rec(
            json!({"tokensInput": 1_000_000, "tokensOutput": 500_000, "tokensCached": 0, "tokensCacheWrite": 0}),
        );
        let attr = CostAttribution {
            upstream_model: Some(json!("muse-spark-1.3")),
            pricing_model: Some(json!("muse-spark-1.3")),
            pricing_fallback: Some(json!(false)),
        };
        calculate_costs(
            &mut r,
            Some(&json!({"source": "simple", "input": 10, "output": 20})),
            None,
            Some(&attr),
            None,
        );
        assert_eq!(r.f64("costTotal"), Some(20.0));
        assert_eq!(
            r.str("costMetadata"),
            Some(
                r#"{"source":"simple","input":10,"output":20,"upstream_model":"muse-spark-1.3","pricing_model":"muse-spark-1.3","pricing_fallback":false}"#
            )
        );
        // Provider-reported cost then wins and drops the fallback label.
        r.set("requestId", "r1");
        let mut r2 = rec(json!({"tokensInput": 1000, "tokensOutput": 500}));
        calculate_costs(
            &mut r2,
            Some(&json!({"source": "simple", "input": 10, "output": 20})),
            None,
            Some(&attr),
            None,
        );
        apply_provider_reported_cost(&mut r2, &json!({"request_cost_usd": 0.000721}));
        assert_eq!(r2.f64("costTotal"), Some(0.000721));
        assert_eq!(meta(&r2)["source"], "provider_reported");
        assert!(meta(&r2).get("pricing_fallback").is_none());
    }

    #[test]
    fn simple_pricing_discounts() {
        let mut r = rec(
            json!({"tokensInput": 1_000_000, "tokensOutput": 1_000_000, "tokensCached": 1_000_000}),
        );
        calculate_costs(
            &mut r,
            Some(&json!({"source": "simple", "input": 1, "output": 2, "cached": 0.5})),
            Some(&json!(0.1)),
            None,
            None,
        );
        assert_eq!(r.f64("costInput"), Some(0.9));
        assert_eq!(r.f64("costOutput"), Some(1.8));
        assert_eq!(r.f64("costCached"), Some(0.45));
        assert_eq!(r.f64("costTotal"), Some(3.15));
        assert_eq!(meta(&r)["discount"], json!(0.1));
        // pricing.discount takes precedence over the provider discount.
        let mut r = rec(json!({"tokensInput": 1_000_000}));
        calculate_costs(
            &mut r,
            Some(&json!({"source": "simple", "input": 1, "output": 2, "discount": 0.5})),
            Some(&json!(0.1)),
            None,
            None,
        );
        assert_eq!(r.f64("costInput"), Some(0.5));
        // A null discount with no provider discount is dropped from metadata.
        let mut r = rec(json!({"tokensInput": 1}));
        calculate_costs(
            &mut r,
            Some(&json!({"source": "simple", "input": 1, "output": 2, "discount": null})),
            None,
            None,
            None,
        );
        assert!(meta(&r).get("discount").is_none());
    }

    #[test]
    fn defined_ranges() {
        let pricing = json!({"source": "defined", "range": [
            {"lower_bound": 0, "upper_bound": 1000, "input_per_m": 0.01, "output_per_m": 0.02},
            {"lower_bound": 1001, "input_per_m": 0.005, "output_per_m": 0.01}
        ]});
        let mut r = rec(json!({"tokensInput": 500, "tokensOutput": 1000}));
        calculate_costs(&mut r, Some(&pricing), None, None, None);
        assert_eq!(r.f64("costInput"), Some(0.000005));
        assert_eq!(r.f64("costOutput"), Some(0.00002));
        assert_eq!(r.f64("costTotal"), Some(0.000025));
        assert_eq!(r.str("costSource"), Some("defined"));
        let mut r = rec(json!({"tokensInput": 2000, "tokensOutput": 1000}));
        calculate_costs(&mut r, Some(&pricing), None, None, None);
        assert_eq!(r.f64("costInput"), Some(0.00001));
        assert_eq!(r.f64("costOutput"), Some(0.00001));
        let m = meta(&r);
        assert_eq!(m["input"], json!(0.005));
        assert_eq!(m["cached"], json!(0));
        assert_eq!(m["range"]["lower_bound"], json!(1001));
        // No matching range → stays on the default source.
        let mut r = rec(json!({"tokensInput": 1000.5}));
        calculate_costs(&mut r, Some(&pricing), None, None, None);
        assert_eq!(r.str("costSource"), Some("default"));
    }

    #[test]
    fn openrouter_pricing() {
        let catalog = Catalog(json!({
            "anthropic/claude": {"prompt": "0.000003", "completion": "0.000015", "input_cache_read": "0.0000003"},
            "router/variable": {"prompt": "-1", "completion": "-1"}
        }));
        let mut r = rec(json!({"tokensInput": 1000, "tokensOutput": 500, "tokensCached": 2000}));
        calculate_costs(
            &mut r,
            Some(&json!({"source": "openrouter", "slug": "anthropic/claude"})),
            None,
            None,
            Some(&catalog),
        );
        assert_eq!(r.f64("costInput"), Some(0.003));
        assert_eq!(r.f64("costOutput"), Some(0.0075));
        assert_eq!(r.f64("costCached"), Some(0.0006));
        assert_eq!(r.f64("costTotal"), Some(0.0111));
        assert_eq!(
            r.str("costMetadata"),
            Some(
                r#"{"slug":"anthropic/claude","prompt":0.000003,"completion":0.000015,"input_cache_read":3e-7,"input_cache_write":0}"#
            )
        );
        let mut r = rec(json!({"tokensInput": 1000}));
        calculate_costs(
            &mut r,
            Some(&json!({"source": "openrouter", "slug": "router/variable"})),
            None,
            None,
            Some(&catalog),
        );
        assert_eq!(r.str("costSource"), Some("default"));
        let pm = PricingManager::new(&catalog);
        assert_eq!(
            pm.search_model_slugs("ROUTER"),
            vec!["router/variable".to_string()]
        );
    }

    #[test]
    fn per_request_pricing() {
        let mut r = rec(json!({"tokensInput": 123456, "tokensOutput": 9}));
        calculate_costs(
            &mut r,
            Some(&json!({"source": "per_request", "amount": 0.04})),
            Some(&json!(0.5)),
            None,
            None,
        );
        assert_eq!(r.f64("costInput"), Some(0.04));
        assert_eq!(r.f64("costOutput"), Some(0.0));
        assert_eq!(r.f64("costTotal"), Some(0.04));
        assert_eq!(r.str("costMetadata"), Some(r#"{"amount":0.04}"#));
    }

    #[test]
    fn provider_reported_cost_distribution() {
        let base = json!({"requestId": "r", "costInput": 0.001, "costOutput": 0.002, "costCached": 0.0005, "costCacheWrite": 0, "costTotal": 0.0035, "costSource": "simple"});
        let mut r = rec(base.clone());
        apply_provider_reported_cost(
            &mut r,
            &json!({"request_cost_usd": 0.0007217243274280318, "cache_savings_usd": 0.0, "allowance_remaining_usd": 14.991, "budget_remaining_usd": 14.991}),
        );
        assert_eq!(r.f64("costTotal"), Some(0.00072172));
        assert_eq!(r.f64("providerReportedCost"), Some(0.0007217243274280318));
        let m = meta(&r);
        assert_eq!(m["previous_cost_source"], "simple");
        assert_eq!(m["previous_cost_total"], json!(0.0035));
        let mut r = rec(base.clone());
        apply_provider_reported_cost(&mut r, &json!({"request_cost_usd": 0.007}));
        assert!((r.f64("costInput").unwrap() - (0.001 / 0.0035) * 0.007).abs() < 1e-8);
        let mut r = rec(json!({"costInput": 0, "costOutput": 0, "costTotal": 0}));
        apply_provider_reported_cost(&mut r, &json!({"request_cost_usd": 0.005}));
        assert_eq!(r.f64("costInput"), Some(0.005));
        assert_eq!(r.f64("costOutput"), Some(0.0));
        for bad in [
            json!({"request_cost_usd": "0.1"}),
            json!({"request_cost_usd": -1}),
            json!({}),
        ] {
            let mut r = rec(base.clone());
            apply_provider_reported_cost(&mut r, &bad);
            assert_eq!(r, rec(base.clone()));
        }
    }

    #[test]
    fn usage_cost_detail_tiers() {
        let details = |v: Value| crate::engine::usage::extract_cost_details(&v).unwrap();
        // Superset breakdown.
        let mut r = rec(json!({"costInput": 1, "costOutput": 1}));
        apply_usage_cost_details(
            &mut r,
            &details(
                json!({"cost": 0.005, "cost_details": {"input_cost": 0.003, "output_cost": 0.002}}),
            ),
        );
        assert_eq!(
            (r.f64("costInput"), r.f64("costOutput"), r.f64("costCached")),
            (Some(0.003), Some(0.002), Some(0.0))
        );
        assert_eq!(meta(&r)["source"], "provider_reported");
        // Normal tier: prompt split by Plexus's own input/cached ratio.
        let mut r = rec(json!({"costInput": 0.003, "costCached": 0.001, "costOutput": 0.01}));
        apply_usage_cost_details(
            &mut r,
            &details(
                json!({"cost": 0.02, "cost_details": {"upstream_inference_prompt_cost": 0.008, "upstream_inference_completions_cost": 0.012}}),
            ),
        );
        assert_eq!(
            (
                r.f64("costInput"),
                r.f64("costCached"),
                r.f64("costOutput"),
                r.f64("costTotal")
            ),
            (Some(0.006), Some(0.002), Some(0.012), Some(0.02))
        );
        // Minimal tier: proportional to the calculated costs.
        let mut r = rec(json!({"costInput": 0.001, "costOutput": 0.003}));
        apply_usage_cost_details(&mut r, &details(json!({"cost": 0.008})));
        assert_eq!(
            (r.f64("costInput"), r.f64("costOutput")),
            (Some(0.002), Some(0.006))
        );
        // Minimal with no prior costs → all on input.
        let mut r = rec(json!({}));
        apply_usage_cost_details(&mut r, &details(json!({"cost": 0.008})));
        assert_eq!(
            (r.f64("costInput"), r.f64("costOutput")),
            (Some(0.008), Some(0.0))
        );
    }

    #[test]
    fn sse_cost_and_energy_comments() {
        let body = ": cost {\"request_cost_usd\": 1}\ndata: {}\n: cost {bad}\n: cost {\"request_cost_usd\": 2}\r\n: energy {\"energy_kwh\": 1.5e-7}\n";
        assert_eq!(
            provider_cost_from_sse_comments(body),
            Some(json!({"request_cost_usd": 2}))
        );
        assert_eq!(
            provider_energy_from_sse_comments(body),
            Some(json!({"energy_kwh": 1.5e-7}))
        );
        assert_eq!(provider_cost_from_sse_comments("data: {}\n"), None);
    }
}
