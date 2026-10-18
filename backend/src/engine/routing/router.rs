//! `routing/router.ts`: alias / target-group / direct routing.
use super::runtime::RoutingRuntime;
use super::selectors::get_selector;
use super::service_tier::{split_service_tier_suffix, ServiceTierSplit};
use super::{js_keys, js_num, RouteResult};
use crate::engine::config::api_format::{
    base_type, has_decisions_protocol, is_decisions_target, is_image_target, is_subtype,
    normalize_access_list,
};
use crate::engine::config::{get_provider_types, model_config, Config};
use crate::engine::js::{get, get_arr, get_str, nullish, truthy};
use crate::engine::transformer::{EngineError, EngineResult};
use futures_util::future::BoxFuture;
use serde_json::Value;
use std::collections::HashSet;

/// `tryParseDirectGroup`: `direct/<alias>/<group>`.
fn try_parse_direct_group(model_name: &str) -> Option<(&str, &str)> {
    let rest = model_name.strip_prefix("direct/")?;
    let i = rest.find('/')?;
    Some((&rest[..i], &rest[i + 1..]))
}

/// `findAlias`: exact alias, else the first alias (JS key order) whose
/// `additional_aliases` contains the name. Returns (alias, canonicalModel).
fn find_alias<'a>(config: &'a Config, model_name: &str) -> (Option<&'a Value>, String) {
    let models = config.models();
    if let Some(alias) = models
        .and_then(|m| m.get(model_name))
        .filter(|a| truthy(Some(a)))
    {
        return (Some(alias), model_name.to_owned());
    }
    if let Some(models) = models {
        for key in js_keys(models) {
            let value = &models[key.as_str()];
            if get_arr(value, &["additional_aliases"])
                .is_some_and(|a| a.iter().any(|x| x.as_str() == Some(model_name)))
            {
                return (Some(value), key.clone());
            }
        }
    }
    (None, model_name.to_owned())
}

fn provider<'a>(config: &'a Config, slug: &str) -> Option<&'a Value> {
    config.provider(slug).filter(|p| truthy(Some(p)))
}
fn tp(t: &Value) -> &str {
    get_str(t, &["provider"]).unwrap_or("")
}
fn tm(t: &Value) -> &str {
    get_str(t, &["model"]).unwrap_or("")
}

/// `providers[p].models[m]` when `models` is a (non-array) record.
fn target_model_config<'a>(config: &'a Config, t: &Value) -> Option<&'a Value> {
    provider(config, tp(t)).and_then(|pc| model_config(pc, tm(t)))
}

/// `getTargetModelInfo`: `(access_via, type)` of the record-form model config.
fn target_model_info<'a>(config: &'a Config, t: &Value) -> (Option<&'a Value>, Option<&'a str>) {
    match target_model_config(config, t) {
        Some(mc) => (mc.get("access_via"), get_str(mc, &["type"])),
        None => (None, None),
    }
}

/// `modelSpecificTypes && modelSpecificTypes.length > 0`.
fn non_empty_access(access: Option<&Value>) -> bool {
    match access {
        Some(Value::Array(a)) => !a.is_empty(),
        _ => false,
    }
}

/// `availableTypes`: model `access_via` when non-empty, else provider types.
fn available_types(access: Option<&Value>, provider_cfg: &Value) -> Vec<String> {
    if non_empty_access(access) {
        normalize_access_list(access)
    } else {
        get_provider_types(provider_cfg)
    }
}

/// `filterGroupTargets`: enabled → cooldown → concurrency → embeddings →
/// images → decisions → api_match/subtype narrowing → enrich with
/// `route.modelConfig`.
async fn filter_group_targets(
    rt: &dyn RoutingRuntime,
    group_targets: Vec<Value>,
    config: &Config,
    alias: &Value,
    incoming_api_type: Option<&str>,
) -> Vec<Value> {
    if group_targets.is_empty() {
        return vec![];
    }

    // 1. Enabled targets of enabled providers.
    let enabled: Vec<Value> = group_targets
        .into_iter()
        .filter(|t| {
            if t.get("enabled") == Some(&Value::Bool(false)) {
                return false;
            }
            if !truthy(t.get("provider")) || !truthy(t.get("model")) {
                return false;
            }
            provider(config, tp(t)).is_some_and(|pc| pc.get("enabled") != Some(&Value::Bool(false)))
        })
        .collect();
    if enabled.is_empty() {
        return vec![];
    }

    // 2. Cooldown filter (`disable_cooldown === true` providers are exempt and
    //    appended after the healthy eligible targets).
    let exempt_cd = |t: &Value| {
        get(
            config.provider(tp(t)).unwrap_or(&Value::Null),
            &["disable_cooldown"],
        ) == Some(&Value::Bool(true))
    };
    let mut healthy_eligible = Vec::new();
    let mut cooldown_exempt = Vec::new();
    for t in &enabled {
        if exempt_cd(t) {
            cooldown_exempt.push(t.clone());
        }
    }
    for t in &enabled {
        if !exempt_cd(t) && rt.is_provider_healthy(tp(t), tm(t)).await {
            healthy_eligible.push(t.clone());
        }
    }
    let mut healthy: Vec<Value> = healthy_eligible
        .into_iter()
        .chain(cooldown_exempt)
        .collect();
    if healthy.is_empty() {
        return vec![];
    }

    // 2.5 Concurrency filter (order preserved).
    healthy.retain(|t| {
        let pc = config.provider(tp(t));
        let mc = target_model_config(config, t);
        let provider_max = pc
            .and_then(|p| p.get("maxConcurrency"))
            .filter(|v| !v.is_null());
        let model_max = mc
            .and_then(|m| m.get("maxConcurrency"))
            .filter(|v| !v.is_null());
        if provider_max.is_none() && model_max.is_none() {
            return true;
        }
        if let Some(max) = provider_max {
            if rt.provider_concurrency(tp(t)) >= js_num(Some(max)) {
                return false;
            }
        }
        if let Some(max) = model_max {
            if rt.target_concurrency(tp(t), tm(t)) >= js_num(Some(max)) {
                return false;
            }
        }
        true
    });
    if healthy.is_empty() {
        return vec![];
    }

    let alias_type = get_str(alias, &["type"]);

    // 3. Embeddings filter (falls back to all healthy targets).
    if incoming_api_type == Some("embeddings") {
        let emb: Vec<Value> = healthy
            .iter()
            .filter(|t| {
                let Some(pc) = provider(config, tp(t)) else {
                    return false;
                };
                if pc.get("models").is_some_and(|m| !m.is_array()) && truthy(pc.get("models")) {
                    match target_model_config(config, t).and_then(|m| get_str(m, &["type"])) {
                        Some("embeddings") => return true,
                        Some("text") => return false,
                        _ => {}
                    }
                }
                if alias_type == Some("embeddings") {
                    return true;
                }
                let types = get_provider_types(pc);
                types.iter().any(|x| x == "embeddings" || x == "gemini")
            })
            .cloned()
            .collect();
        if !emb.is_empty() {
            healthy = emb;
        }
    }

    // 3.5 Image capability filter (strict).
    if incoming_api_type == Some("images") {
        healthy.retain(|t| {
            let Some(pc) = provider(config, tp(t)) else {
                return false;
            };
            let (access, ty) = target_model_info(config, t);
            if matches!(ty, Some("text") | Some("embeddings")) {
                return false;
            }
            available_types(access, pc)
                .iter()
                .any(|x| is_image_target(x))
        });
    }

    // Decisions-typed targets serve decisions requests only, and only with a
    // decisions protocol.
    let decisions_typed = |t: &Value, incoming: &str| -> Option<bool> {
        let (access, ty) = target_model_info(config, t);
        if ty != Some("decisions") {
            return None;
        }
        if incoming != "decisions" {
            return Some(false);
        }
        Some(
            provider(config, tp(t))
                .is_some_and(|pc| has_decisions_protocol(access, &get_provider_types(pc))),
        )
    };

    // 3.6 Decisions capability filter.
    if incoming_api_type == Some("decisions") {
        healthy.retain(|t| {
            let Some(pc) = provider(config, tp(t)) else {
                return false;
            };
            if let Some(e) = decisions_typed(t, "decisions") {
                return e;
            }
            let (access, _) = target_model_info(config, t);
            if available_types(access, pc)
                .iter()
                .any(|x| is_decisions_target(x))
            {
                return true;
            }
            !non_empty_access(access) && alias_type == Some("decisions")
        });
    } else {
        healthy.retain(|t| {
            if alias_type == Some("decisions") {
                return false;
            }
            if provider(config, tp(t)).is_none() {
                return false;
            }
            let (access, ty) = target_model_info(config, t);
            if ty == Some("decisions") {
                return false;
            }
            let advertised = if non_empty_access(access) {
                normalize_access_list(access)
            } else {
                vec![]
            };
            advertised.is_empty() || !advertised.iter().all(|x| is_decisions_target(x))
        });
    }

    let find_api_compatible = |targets: &[Value], requested: &str| -> Vec<Value> {
        let incoming = requested.to_lowercase();
        targets
            .iter()
            .filter(|t| {
                let Some(pc) = provider(config, tp(t)) else {
                    return false;
                };
                let (access, ty) = target_model_info(config, t);
                if incoming == "images" && matches!(ty, Some("text") | Some("embeddings")) {
                    return false;
                }
                if let Some(e) = decisions_typed(t, &incoming) {
                    return e;
                }
                let avail = available_types(access, pc);
                if incoming == "decisions" {
                    return avail.iter().any(|x| is_decisions_target(x));
                }
                if !avail.is_empty() && avail.iter().all(|x| is_decisions_target(x)) {
                    return false;
                }
                avail.iter().any(|x| {
                    x.to_lowercase() == incoming || (incoming == "images" && is_image_target(x))
                })
            })
            .cloned()
            .collect()
    };

    // 4. API match filter.
    if let Some(incoming) = incoming_api_type.filter(|i| is_subtype(Some(i))) {
        let exact = find_api_compatible(&healthy, incoming);
        if !exact.is_empty() {
            healthy = exact;
        } else {
            let base = find_api_compatible(&healthy, &base_type(incoming));
            if !base.is_empty() {
                healthy = base;
            }
        }
    } else if get_str(alias, &["priority"]) == Some("api_match") {
        if let Some(incoming) = incoming_api_type.filter(|i| !i.is_empty()) {
            let compatible = find_api_compatible(&healthy, incoming);
            if !compatible.is_empty() {
                healthy = compatible;
            }
        }
    }

    // 5. Enrich with modelConfig.
    healthy
        .into_iter()
        .map(|t| {
            let mc = target_model_config(config, &t).cloned();
            let mut obj = t.as_object().cloned().unwrap_or_default();
            let route = match mc {
                Some(m) => serde_json::json!({"modelConfig": m}),
                None => serde_json::json!({}),
            };
            obj.insert("route".into(), route);
            Value::Object(obj)
        })
        .collect()
}

/// `selectOrderedTargets`: repeatedly select from the remaining targets.
async fn select_ordered_targets(
    rt: &dyn RoutingRuntime,
    selector_type: Option<&Value>,
    enriched: Vec<Value>,
) -> EngineResult<Vec<Value>> {
    let selector = get_selector(selector_type, rt)?;
    let mut ordered = Vec::new();
    let mut remaining = enriched;
    while !remaining.is_empty() {
        let Some(sel) = selector.select(&remaining, rt).await else {
            break;
        };
        let Some(selected) = remaining.get(sel).cloned() else {
            break;
        };
        match remaining
            .iter()
            .position(|t| tp(t) == tp(&selected) && tm(t) == tm(&selected))
        {
            Some(idx) => {
                remaining.remove(idx);
            }
            None => {
                remaining.remove(0);
            }
        }
        ordered.push(selected);
    }
    ordered.extend(remaining);
    Ok(ordered)
}

fn with_visited(visited: &HashSet<String>, slug: &str) -> HashSet<String> {
    let mut next = visited.clone();
    next.insert(slug.to_owned());
    next
}

/// `dedupeCandidates`: first (provider, model) wins.
fn dedupe(candidates: Vec<RouteResult>) -> Vec<RouteResult> {
    let mut seen = HashSet::new();
    candidates
        .into_iter()
        .filter(|c| seen.insert(format!("{}\u{0}{}", c.provider, c.model)))
        .collect()
}

/// `buildGroupCandidates`.
#[allow(clippy::too_many_arguments)]
async fn build_group_candidates(
    rt: &dyn RoutingRuntime,
    group: &Value,
    config: &Config,
    alias: &Value,
    incoming_api_type: Option<&str>,
    log_model_name: &str,
    canonical_model: &str,
    session_key: Option<&str>,
    visited: &HashSet<String>,
) -> EngineResult<Vec<RouteResult>> {
    let targets: Vec<Value> = get_arr(group, &["targets"]).cloned().unwrap_or_default();
    let concrete: Vec<Value> = targets
        .iter()
        .filter(|t| !truthy(t.get("alias")))
        .cloned()
        .collect();
    let enriched = filter_group_targets(rt, concrete, config, alias, incoming_api_type).await;
    let ordered = select_ordered_targets(rt, group.get("selector"), enriched).await?;

    let mut merged: Vec<RouteResult> = ordered
        .iter()
        .map(|t| RouteResult {
            provider: tp(t).to_owned(),
            model: tm(t).to_owned(),
            config: config.provider(tp(t)).cloned().unwrap_or(Value::Null),
            model_config: target_model_config(config, t).cloned(),
            incoming_model_alias: Some(log_model_name.to_owned()),
            canonical_model: Some(canonical_model.to_owned()),
        })
        .collect();

    for target in &targets {
        if !truthy(target.get("alias")) {
            continue;
        }
        if target.get("enabled") == Some(&Value::Bool(false)) {
            continue;
        }
        let alias_ref = crate::engine::js::to_string(target.get("alias"));
        if visited.contains(&alias_ref) {
            continue;
        }
        let nested = resolve_candidates_inner(
            rt,
            alias_ref,
            incoming_api_type.map(str::to_owned),
            session_key.map(str::to_owned),
            visited.clone(),
        )
        .await?;
        for c in nested {
            merged.push(RouteResult {
                canonical_model: Some(canonical_model.to_owned()),
                incoming_model_alias: Some(log_model_name.to_owned()),
                ..c
            });
        }
    }

    if let Some(explorer) = rt.background_explorer() {
        explorer.maybe_trigger(group, get_str(alias, &["type"]));
    }
    Ok(merged)
}

/// `Router.splitServiceTier(modelName)`: splits `<alias>@<tier>` only when the
/// full name is not itself an alias and the bare name is one.
pub fn split_service_tier(rt: &dyn RoutingRuntime, model_name: &str) -> ServiceTierSplit {
    let config = rt.config();
    if find_alias(&config, model_name).0.is_some() {
        return ServiceTierSplit::unchanged(model_name);
    }
    let split = split_service_tier_suffix(model_name);
    if split.service_tier.is_none() || find_alias(&config, &split.model).0.is_none() {
        return ServiceTierSplit::unchanged(model_name);
    }
    split
}

/// `Router.resolveCandidates(modelName, incomingApiType?, sessionKey?)`:
/// the full ordered, deduped candidate list (empty when the name is not an
/// alias with target groups; callers then fall back to `resolve`).
pub async fn resolve_candidates(
    rt: &dyn RoutingRuntime,
    model_name: &str,
    incoming_api_type: Option<&str>,
    session_key: Option<&str>,
) -> EngineResult<Vec<RouteResult>> {
    resolve_candidates_inner(
        rt,
        model_name.to_owned(),
        incoming_api_type.map(str::to_owned),
        session_key.map(str::to_owned),
        HashSet::new(),
    )
    .await
}

fn resolve_candidates_inner<'a>(
    rt: &'a dyn RoutingRuntime,
    model_name: String,
    incoming_api_type: Option<String>,
    session_key: Option<String>,
    visited: HashSet<String>,
) -> BoxFuture<'a, EngineResult<Vec<RouteResult>>> {
    Box::pin(async move {
        let config = rt.config();
        let incoming = incoming_api_type.as_deref();
        let session = session_key.as_deref();

        if let Some((alias_name, group_name)) = try_parse_direct_group(&model_name) {
            let (alias, canonical) = find_alias(&config, alias_name);
            if let Some(alias) = alias.filter(|a| truthy(a.get("target_groups"))) {
                let group = get_arr(alias, &["target_groups"]).and_then(|gs| {
                    gs.iter()
                        .find(|g| get_str(g, &["name"]) == Some(group_name))
                });
                if let Some(group) = group {
                    let results = build_group_candidates(
                        rt,
                        group,
                        &config,
                        alias,
                        incoming,
                        &model_name,
                        &canonical,
                        session,
                        &with_visited(&visited, &canonical),
                    )
                    .await?;
                    return Ok(dedupe(results));
                }
                return Ok(vec![]);
            }
        }

        let (alias, canonical) = find_alias(&config, &model_name);
        let Some(alias) = alias else {
            return Ok(vec![]);
        };
        let groups = match get_arr(alias, &["target_groups"]) {
            Some(g) if !g.is_empty() => g,
            _ => return Ok(vec![]),
        };
        if visited.contains(&canonical) {
            return Ok(vec![]);
        }
        let next_visited = with_visited(&visited, &canonical);

        let sticky = match session.filter(|s| !s.is_empty()) {
            Some(key) if truthy(alias.get("sticky_session")) => rt.sticky_sessions().get_at(
                &canonical,
                incoming.filter(|i| !i.is_empty()).unwrap_or("chat"),
                key,
                rt.now_ms(),
            ),
            _ => None,
        };

        let mut ordered = Vec::new();
        for group in groups {
            ordered.extend(
                build_group_candidates(
                    rt,
                    group,
                    &config,
                    alias,
                    incoming,
                    &model_name,
                    &canonical,
                    session,
                    &next_visited,
                )
                .await?,
            );
        }
        let mut ordered = dedupe(ordered);

        if let Some(pick) = sticky {
            if let Some(idx) = ordered
                .iter()
                .position(|c| c.provider == pick.provider && c.model == pick.model)
            {
                if idx > 0 {
                    let picked = ordered.remove(idx);
                    ordered.insert(0, picked);
                }
            }
        }
        Ok(ordered)
    })
}

/// `Router.resolve(modelName, incomingApiType?)`: the single first target.
pub async fn resolve(
    rt: &dyn RoutingRuntime,
    model_name: &str,
    incoming_api_type: Option<&str>,
) -> EngineResult<RouteResult> {
    let config = rt.config();

    if model_name.starts_with("direct/") {
        if let Some((alias_name, group_name)) = try_parse_direct_group(model_name) {
            let (alias, canonical) = find_alias(&config, alias_name);
            if let Some(alias) = alias.filter(|a| truthy(a.get("target_groups"))) {
                let group = get_arr(alias, &["target_groups"]).and_then(|gs| {
                    gs.iter()
                        .find(|g| get_str(g, &["name"]) == Some(group_name))
                });
                if let Some(group) = group {
                    if get_arr(group, &["targets"]).is_none_or(|t| t.is_empty()) {
                        return Err(EngineError::new(format!("No targets configured in group '{group_name}' for alias '{alias_name}'")));
                    }
                    let candidates = build_group_candidates(
                        rt,
                        group,
                        &config,
                        alias,
                        incoming_api_type,
                        model_name,
                        &canonical,
                        None,
                        &with_visited(&HashSet::new(), &canonical),
                    )
                    .await?;
                    return dedupe(candidates).into_iter().next().ok_or_else(|| {
                        EngineError::new(format!(
                            "No healthy targets in group '{group_name}' for alias '{alias_name}'"
                        ))
                    });
                }
                return Err(status_error(format!("Direct routing failed: Target group '{group_name}' not found for alias '{alias_name}'"), 404));
            }
        }
        return resolve_direct(rt, model_name, &config);
    }

    let (alias, canonical) = find_alias(&config, model_name);
    if let Some(alias) = alias {
        let groups =
            get_arr(alias, &["target_groups"]).filter(|_| truthy(alias.get("target_groups")));
        let has_targets = groups.is_some_and(|gs| {
            gs.iter()
                .any(|g| get_arr(g, &["targets"]).is_some_and(|t| !t.is_empty()))
        });
        if !has_targets {
            return Err(EngineError::new(format!(
                "No targets configured for alias '{model_name}'"
            )));
        }
        let visited = with_visited(&HashSet::new(), &canonical);
        for group in groups.into_iter().flatten() {
            let candidates = build_group_candidates(
                rt,
                group,
                &config,
                alias,
                incoming_api_type,
                model_name,
                &canonical,
                None,
                &visited,
            )
            .await?;
            if candidates.is_empty() {
                continue;
            }
            if let Some(explorer) = rt.background_explorer() {
                explorer.maybe_trigger(group, get_str(alias, &["type"]));
            }
            if let Some(target) = dedupe(candidates).into_iter().next() {
                return Ok(target);
            }
        }
        return Err(EngineError::new(format!(
            "No healthy target selected for alias '{model_name}'"
        )));
    }

    Err(EngineError::new(format!(
        "Model '{model_name}' not found in configuration"
    )))
}

/// Error with `routingContext = { statusCode }` (no `code`).
fn status_error(message: String, status: u16) -> EngineError {
    EngineError {
        message,
        status: Some(status),
        code: None,
    }
}

/// `Router.resolveDirect`: `direct/<provider>/<model>` bypassing selectors.
fn resolve_direct(
    rt: &dyn RoutingRuntime,
    model_name: &str,
    config: &Config,
) -> EngineResult<RouteResult> {
    let without_prefix = model_name.get(7..).unwrap_or("");
    let Some(slash) = without_prefix.find('/') else {
        return Err(status_error(format!("Direct routing failed: Invalid format '{model_name}'. Expected 'direct/provider/model'"), 400));
    };
    let provider_id = &without_prefix[..slash];
    let provider_model = &without_prefix[slash + 1..];

    let Some(pc) = provider(config, provider_id) else {
        return Err(status_error(
            format!("Direct routing failed: Provider '{provider_id}' not found in configuration"),
            404,
        ));
    };
    if pc.get("enabled") == Some(&Value::Bool(false)) {
        return Err(status_error(
            format!("Direct routing failed: Provider '{provider_id}' is disabled"),
            404,
        ));
    }
    if !nullish(pc.get("maxConcurrency"))
        && rt.provider_concurrency(provider_id) >= js_num(pc.get("maxConcurrency"))
    {
        return Err(status_error(
            format!(
                "Direct routing failed: Concurrency limit exceeded for provider '{provider_id}'"
            ),
            429,
        ));
    }
    let mc = model_config(pc, provider_model);
    if let Some(max) = mc
        .and_then(|m| m.get("maxConcurrency"))
        .filter(|v| !v.is_null())
    {
        if rt.target_concurrency(provider_id, provider_model) >= js_num(Some(max)) {
            return Err(status_error(format!("Direct routing failed: Concurrency limit exceeded for model '{provider_id}/{provider_model}'"), 429));
        }
    }
    Ok(RouteResult {
        provider: provider_id.to_owned(),
        model: provider_model.to_owned(),
        config: pc.clone(),
        model_config: mc.cloned(),
        incoming_model_alias: Some(model_name.to_owned()),
        canonical_model: Some(model_name.to_owned()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::routing::runtime::mock::MockRuntime;
    use serde_json::json;

    fn providers() -> Value {
        json!({
            "p1": {"type": "openai", "api_base_url": "https://p1.example.com/v1", "models": {"model-1": {}}},
            "p2": {"type": "openai", "api_base_url": "https://p2.example.com/v1", "models": {"model-2": {}}},
            "p3": {"type": "openai", "api_base_url": "https://p3.example.com/v1", "models": {"model-3": {}}}
        })
    }
    fn rt_with(models: Value) -> MockRuntime {
        MockRuntime::new(json!({"providers": providers(), "models": models, "keys": {}}))
    }
    fn grouped(groups: Value) -> MockRuntime {
        rt_with(json!({"test-alias": {"target_groups": groups}}))
    }
    fn g(name: &str, selector: &str, targets: Value) -> Value {
        json!({"name": name, "selector": selector, "targets": targets})
    }
    fn t(p: &str, m: &str) -> Value {
        json!({"provider": p, "model": m})
    }
    fn ids(c: &[RouteResult]) -> Vec<String> {
        c.iter()
            .map(|r| format!("{}/{}", r.provider, r.model))
            .collect()
    }

    #[tokio::test]
    async fn target_groups_resolve_and_fallback() {
        let groups = json!([
            g("primary", "in_order", json!([t("p1", "model-1")])),
            g("fallback", "in_order", json!([t("p2", "model-2")]))
        ]);
        let rt = grouped(groups.clone());
        let r = resolve(&rt, "test-alias", None).await.unwrap();
        assert_eq!((r.provider.as_str(), r.model.as_str()), ("p1", "model-1"));

        let mut rt = grouped(groups.clone());
        rt.unhealthy.insert("p1/model-1".into());
        assert_eq!(
            resolve(&rt, "test-alias", None).await.unwrap().provider,
            "p2"
        );

        let mut rt = grouped(groups);
        rt.unhealthy
            .extend(["p1/".to_string(), "p2/model-2".to_string()]);
        assert_eq!(
            resolve(&rt, "test-alias", None).await.unwrap_err().message,
            "No healthy target selected for alias 'test-alias'"
        );

        let rt = grouped(json!([g("default", "random", json!([]))]));
        assert_eq!(
            resolve(&rt, "test-alias", None).await.unwrap_err().message,
            "No targets configured for alias 'test-alias'"
        );

        let rt = grouped(json!([
            g("empty", "in_order", json!([])),
            g("real", "in_order", json!([t("p2", "model-2")]))
        ]));
        assert_eq!(
            resolve(&rt, "test-alias", None).await.unwrap().provider,
            "p2"
        );
        assert_eq!(
            resolve(&rt, "nope", None).await.unwrap_err().message,
            "Model 'nope' not found in configuration"
        );
    }

    #[tokio::test]
    async fn resolve_candidates_ordering_and_filters() {
        let groups = json!([
            g(
                "g1",
                "in_order",
                json!([t("p1", "model-1"), t("p2", "model-2")])
            ),
            g("g2", "in_order", json!([])),
            g(
                "g3",
                "in_order",
                json!([t("p3", "model-3"), t("p1", "model-1")])
            )
        ]);
        let rt = grouped(groups);
        let c = resolve_candidates(&rt, "test-alias", None, None)
            .await
            .unwrap();
        assert_eq!(ids(&c), vec!["p1/model-1", "p2/model-2", "p3/model-3"]);
        assert_eq!(c[0].canonical_model.as_deref(), Some("test-alias"));
        assert_eq!(c[0].incoming_model_alias.as_deref(), Some("test-alias"));
        assert_eq!(c[0].config, providers()["p1"]);
        assert_eq!(c[0].model_config, Some(json!({})));

        // Disabled targets and disabled providers are excluded.
        let mut p = providers();
        p["p2"]["enabled"] = json!(false);
        let rt = MockRuntime::new(
            json!({"providers": p, "models": {"test-alias": {"target_groups": [g("g", "in_order", json!([
                {"provider": "p1", "model": "model-1", "enabled": false}, t("p2", "model-2"), t("p3", "model-3")
            ]))]}}}),
        );
        assert_eq!(
            ids(&resolve_candidates(&rt, "test-alias", None, None)
                .await
                .unwrap()),
            vec!["p3/model-3"]
        );

        // Alias without target_groups.
        let rt = rt_with(json!({"bare": {"type": "text"}}));
        assert!(resolve_candidates(&rt, "bare", None, None)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            resolve(&rt, "bare", None).await.unwrap_err().message,
            "No targets configured for alias 'bare'"
        );
        assert!(resolve_candidates(&rt, "unknown", None, None)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn selectors_per_group_and_additional_aliases() {
        let mut p = providers();
        p["p1"]["models"]["model-1"]["pricing"] =
            json!({"source": "simple", "input": 10, "output": 10});
        p["p2"]["models"]["model-2"]["pricing"] =
            json!({"source": "simple", "input": 1, "output": 1});
        let rt = MockRuntime::new(
            json!({"providers": p, "models": {"canon": {"additional_aliases": ["nick"], "target_groups": [
                g("a", "cost", json!([t("p1", "model-1"), t("p2", "model-2")])), g("b", "in_order", json!([t("p3", "model-3")]))
            ]}}}),
        );
        let c = resolve_candidates(&rt, "nick", None, None).await.unwrap();
        assert_eq!(ids(&c), vec!["p2/model-2", "p1/model-1", "p3/model-3"]);
        assert_eq!(c[0].canonical_model.as_deref(), Some("canon"));
        assert_eq!(c[0].incoming_model_alias.as_deref(), Some("nick"));
        let rt = grouped(json!([g("x", "bogus", json!([t("p1", "model-1")]))]));
        assert_eq!(
            resolve_candidates(&rt, "test-alias", None, None)
                .await
                .unwrap_err()
                .message,
            "Unknown selector type: bogus"
        );
        // Absent selector = random.
        let rt =
            grouped(json!([{"name": "x", "targets": [t("p1", "model-1"), t("p2", "model-2")]}]))
                .with_randoms(&[0.9]);
        assert_eq!(
            ids(&resolve_candidates(&rt, "test-alias", None, None)
                .await
                .unwrap()),
            vec!["p2/model-2", "p1/model-1"]
        );
    }

    #[tokio::test]
    async fn alias_refs() {
        let b = json!({"target_groups": [g("b-primary", "in_order", json!([t("p1", "model-1")])), g("b-fallback", "in_order", json!([t("p2", "model-2")]))]});
        let rt = rt_with(
            json!({"A": {"target_groups": [g("primary", "in_order", json!([{"alias": "B"}]))]}, "B": b}),
        );
        let c = resolve_candidates(&rt, "A", None, None).await.unwrap();
        assert_eq!(ids(&c), vec!["p1/model-1", "p2/model-2"]);
        assert_eq!(c[0].canonical_model.as_deref(), Some("A"));
        assert_eq!(c[0].incoming_model_alias.as_deref(), Some("A"));

        // Nested, dedupe, disabled refs, appended after concrete targets.
        let rt = rt_with(json!({
            "A": {"target_groups": [g("primary", "in_order", json!([{"alias": "B"}, {"alias": "C"}, {"alias": "D", "enabled": false}, t("p2", "model-2")]))]},
            "B": {"target_groups": [g("p", "in_order", json!([{"alias": "C"}]))]},
            "C": {"target_groups": [g("p", "in_order", json!([t("p3", "model-3")]))]},
            "D": {"target_groups": [g("p", "in_order", json!([t("p1", "model-1")]))]}
        }));
        assert_eq!(
            ids(&resolve_candidates(&rt, "A", None, None).await.unwrap()),
            vec!["p2/model-2", "p3/model-3"]
        );

        // Cycles terminate.
        let rt = rt_with(json!({
            "A": {"target_groups": [g("p", "in_order", json!([t("p1", "model-1"), {"alias": "B"}]))]},
            "B": {"target_groups": [g("p", "in_order", json!([t("p2", "model-2"), {"alias": "A"}]))]}
        }));
        assert_eq!(
            ids(&resolve_candidates(&rt, "A", None, None).await.unwrap()),
            vec!["p1/model-1", "p2/model-2"]
        );

        // direct/<alias>/<group> with only an alias-ref.
        let rt = rt_with(json!({
            "A": {"target_groups": [g("primary", "in_order", json!([{"alias": "B"}]))]},
            "B": {"target_groups": [g("primary", "in_order", json!([t("p2", "model-2")]))]}
        }));
        let r = resolve(&rt, "direct/A/primary", None).await.unwrap();
        assert_eq!(
            (
                r.provider.as_str(),
                r.canonical_model.as_deref(),
                r.incoming_model_alias.as_deref()
            ),
            ("p2", Some("A"), Some("direct/A/primary"))
        );
        assert_eq!(
            ids(&resolve_candidates(&rt, "direct/A/primary", None, None)
                .await
                .unwrap()),
            vec!["p2/model-2"]
        );
        assert!(resolve_candidates(&rt, "direct/A/missing", None, None)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn direct_routing() {
        let mut p = providers();
        p["p2"]["enabled"] = json!(false);
        p["p3"]["maxConcurrency"] = json!(1);
        p["p1"]["models"]["model-1"]["maxConcurrency"] = json!(2);
        let mut rt = MockRuntime::new(json!({"providers": p, "models": {
            "A": {"target_groups": [g("primary", "in_order", json!([t("p1", "model-1")])), g("empty", "in_order", json!([]))]}
        }}));
        let r = resolve(&rt, "direct/p1/model-1", None).await.unwrap();
        assert_eq!(
            (
                r.provider.as_str(),
                r.model.as_str(),
                r.model_config.clone()
            ),
            ("p1", "model-1", Some(json!({"maxConcurrency": 2})))
        );
        assert_eq!(r.canonical_model.as_deref(), Some("direct/p1/model-1"));
        let r = resolve(&rt, "direct/p1/vendor/model-x", None)
            .await
            .unwrap();
        assert_eq!(
            (r.model.as_str(), r.model_config.is_none()),
            ("vendor/model-x", true)
        );
        let cases = [
            ("direct/p1", "Direct routing failed: Invalid format 'direct/p1'. Expected 'direct/provider/model'", 400),
            ("direct/nope/m", "Direct routing failed: Provider 'nope' not found in configuration", 404),
            ("direct/p2/model-2", "Direct routing failed: Provider 'p2' is disabled", 404),
            ("direct/A/missing", "Direct routing failed: Target group 'missing' not found for alias 'A'", 404),
        ];
        for (name, msg, status) in cases {
            let e = resolve(&rt, name, None).await.unwrap_err();
            assert_eq!(
                (e.message.as_str(), e.status),
                (msg, Some(status)),
                "{name}"
            );
        }
        assert_eq!(
            resolve(&rt, "direct/A/empty", None)
                .await
                .unwrap_err()
                .message,
            "No targets configured in group 'empty' for alias 'A'"
        );
        rt.provider_counts.insert("p3".into(), 1.0);
        rt.target_counts.insert("p1/model-1".into(), 2.0);
        let e = resolve(&rt, "direct/p3/model-3", None).await.unwrap_err();
        assert_eq!(
            (e.message.as_str(), e.status),
            (
                "Direct routing failed: Concurrency limit exceeded for provider 'p3'",
                Some(429)
            )
        );
        let e = resolve(&rt, "direct/p1/model-1", None).await.unwrap_err();
        assert_eq!(
            (e.message.as_str(), e.status),
            (
                "Direct routing failed: Concurrency limit exceeded for model 'p1/model-1'",
                Some(429)
            )
        );
        rt.unhealthy.insert("p1/model-1".into());
        assert_eq!(
            resolve(&rt, "direct/A/primary", None)
                .await
                .unwrap_err()
                .message,
            "No healthy targets in group 'primary' for alias 'A'"
        );
    }

    #[tokio::test]
    async fn concurrency_and_cooldown_order() {
        let mut p = providers();
        p["p1"]["maxConcurrency"] = json!(2);
        p["p3"]["models"]["model-3"]["maxConcurrency"] = json!(1);
        let models = json!({"test-alias": {"target_groups": [g("g", "in_order", json!([t("p1", "model-1"), t("p2", "model-2"), t("p3", "model-3")]))]}});
        let mut rt = MockRuntime::new(json!({"providers": p.clone(), "models": models.clone()}));
        rt.provider_counts.insert("p1".into(), 1.0);
        assert_eq!(
            ids(&resolve_candidates(&rt, "test-alias", None, None)
                .await
                .unwrap()),
            vec!["p1/model-1", "p2/model-2", "p3/model-3"]
        );
        rt.provider_counts.insert("p1".into(), 2.0);
        rt.target_counts.insert("p3/model-3".into(), 1.0);
        assert_eq!(
            ids(&resolve_candidates(&rt, "test-alias", None, None)
                .await
                .unwrap()),
            vec!["p2/model-2"]
        );

        // disable_cooldown providers bypass cooldown and are appended after healthy ones.
        p["p1"]["disable_cooldown"] = json!(true);
        let mut rt = MockRuntime::new(json!({"providers": p, "models": models}));
        rt.unhealthy.insert("p1/".into());
        assert_eq!(
            ids(&resolve_candidates(&rt, "test-alias", None, None)
                .await
                .unwrap()),
            vec!["p2/model-2", "p3/model-3", "p1/model-1"]
        );
    }

    #[tokio::test]
    async fn sticky_sessions() {
        let models = json!({
            "sticky-alias": {"sticky_session": true, "additional_aliases": ["sa"], "target_groups": [g("g", "in_order", json!([t("p1", "model-1"), t("p2", "model-2"), t("p3", "model-3")]))]},
            "plain": {"target_groups": [g("g", "in_order", json!([t("p1", "model-1"), t("p3", "model-3")]))]}
        });
        let rt = rt_with(models.clone());
        rt.sticky
            .set("sticky-alias", "chat", "m:abc", "p3", "model-3", None);
        assert_eq!(
            ids(
                &resolve_candidates(&rt, "sticky-alias", Some("chat"), Some("m:abc"))
                    .await
                    .unwrap()
            ),
            vec!["p3/model-3", "p1/model-1", "p2/model-2"]
        );
        // Via additional alias: canonical name keys the lookup; default api type is chat.
        assert_eq!(
            resolve_candidates(&rt, "sa", None, Some("m:abc"))
                .await
                .unwrap()[0]
                .provider,
            "p3"
        );
        assert_eq!(
            resolve_candidates(&rt, "sticky-alias", Some("chat"), None)
                .await
                .unwrap()[0]
                .provider,
            "p1"
        );
        assert_eq!(
            resolve_candidates(&rt, "sticky-alias", Some("chat"), Some("m:other"))
                .await
                .unwrap()[0]
                .provider,
            "p1"
        );
        assert_eq!(
            resolve_candidates(&rt, "sticky-alias", Some("messages"), Some("m:abc"))
                .await
                .unwrap()[0]
                .provider,
            "p1"
        );
        rt.sticky
            .set("plain", "chat", "m:abc", "p3", "model-3", None);
        assert_eq!(
            resolve_candidates(&rt, "plain", Some("chat"), Some("m:abc"))
                .await
                .unwrap()[0]
                .provider,
            "p1"
        );

        let mut rt = rt_with(models.clone());
        rt.unhealthy.insert("p3/model-3".into());
        rt.sticky
            .set("sticky-alias", "chat", "m:abc", "p3", "model-3", None);
        assert_eq!(
            ids(
                &resolve_candidates(&rt, "sticky-alias", Some("chat"), Some("m:abc"))
                    .await
                    .unwrap()
            ),
            vec!["p1/model-1", "p2/model-2"]
        );

        // Subtype filtering happens before hoisting.
        let mut p = providers();
        p["p1"]["models"]["model-1"]["access_via"] = json!(["responses"]);
        p["p2"]["models"]["model-2"]["access_via"] =
            json!([{"type": "responses", "subtype": "lite"}]);
        p["p3"]["models"]["model-3"]["access_via"] = json!(["chat"]);
        let rt = MockRuntime::new(json!({"providers": p, "models": models}));
        rt.sticky.set(
            "sticky-alias",
            "responses:lite",
            "m:abc",
            "p1",
            "model-1",
            None,
        );
        assert_eq!(
            ids(
                &resolve_candidates(&rt, "sticky-alias", Some("responses:lite"), Some("m:abc"))
                    .await
                    .unwrap()
            ),
            vec!["p2/model-2"]
        );
        // Base-type fallback when no exact subtype target exists.
        assert_eq!(
            ids(
                &resolve_candidates(&rt, "sticky-alias", Some("messages:x"), None)
                    .await
                    .unwrap()
            ),
            vec!["p1/model-1", "p2/model-2", "p3/model-3"]
        );
    }

    fn decisions_rt() -> MockRuntime {
        MockRuntime::new(json!({
            "providers": {
                "openrouter": {"api_base_url": {"chat": "https://openrouter.ai/api/v1", "systemone": "https://openrouter.ai/api/v1"}, "api_key": "k",
                    "models": {"typesafe/jev-1.13": {"access_via": ["systemone"]}, "some-chat-model": {"access_via": ["chat"]}}},
                "typesafe": {"api_base_url": "https://api.typesafe.ai/v1", "api_key": "k", "models": {"jev-latest": {"access_via": ["systemone"]}}},
                "generic": {"api_base_url": "https://chat.example.com/v1", "models": {"generic-model": {}, "dec": {"type": "decisions"}, "dec2": {"type": "decisions", "access_via": ["systemone"]}}}
            },
            "models": {
                "decisions_alias": {"type": "decisions", "target_groups": [g("g", "in_order", json!([t("openrouter", "typesafe/jev-1.13"), t("openrouter", "some-chat-model"), t("typesafe", "jev-latest")]))]},
                "chat_alias": {"type": "text", "target_groups": [g("g", "in_order", json!([t("openrouter", "typesafe/jev-1.13"), t("openrouter", "some-chat-model")]))]},
                "chat_only_alias": {"type": "text", "target_groups": [g("g", "in_order", json!([t("openrouter", "some-chat-model")]))]},
                "generic_alias": {"type": "text", "target_groups": [g("g", "in_order", json!([t("generic", "generic-model")]))]},
                "typed": {"target_groups": [g("g", "in_order", json!([t("generic", "dec"), t("generic", "dec2"), t("openrouter", "typesafe/jev-1.13")]))]}
            }
        }))
    }

    #[tokio::test]
    async fn decisions_eligibility() {
        let rt = decisions_rt();
        assert_eq!(
            ids(
                &resolve_candidates(&rt, "decisions_alias", Some("decisions"), None)
                    .await
                    .unwrap()
            ),
            vec!["openrouter/typesafe/jev-1.13", "typesafe/jev-latest"]
        );
        assert_eq!(
            ids(
                &resolve_candidates(&rt, "chat_alias", Some("decisions"), None)
                    .await
                    .unwrap()
            ),
            vec!["openrouter/typesafe/jev-1.13"]
        );
        assert!(
            resolve_candidates(&rt, "chat_only_alias", Some("decisions"), None)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            ids(&resolve_candidates(&rt, "chat_alias", Some("chat"), None)
                .await
                .unwrap()),
            vec!["openrouter/some-chat-model"]
        );
        for api in ["chat", "images", "embeddings"] {
            assert!(
                resolve_candidates(&rt, "decisions_alias", Some(api), None)
                    .await
                    .unwrap()
                    .is_empty(),
                "{api}"
            );
        }
        assert_eq!(
            resolve_candidates(&rt, "generic_alias", Some("chat"), None)
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(
            resolve_candidates(&rt, "generic_alias", Some("decisions"), None)
                .await
                .unwrap()
                .is_empty()
        );
        // Decisions-typed: rejected without a decisions protocol, admitted with an explicit access_via.
        assert_eq!(
            ids(&resolve_candidates(&rt, "typed", Some("decisions"), None)
                .await
                .unwrap()),
            vec!["generic/dec2", "openrouter/typesafe/jev-1.13"]
        );
        assert!(resolve_candidates(&rt, "typed", Some("chat"), None)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn image_and_embeddings_eligibility() {
        let rt = MockRuntime::new(json!({
            "providers": {
                "codex": {"api_base_url": "oauth://", "api_key": "oauth", "oauth_provider": "openai-codex",
                    "models": {"gpt-image-1": {"type": "image", "access_via": ["codex-images"]}, "gpt-5": {"type": "text", "access_via": ["responses"]}}},
                "emb": {"api_base_url": {"embeddings": "https://e/v1"}, "models": {"e": {}}},
                "chat": {"api_base_url": "https://c/v1", "models": {"c": {}, "t": {"type": "text"}}}
            },
            "models": {
                "image_alias": {"type": "image", "target_groups": [g("g", "in_order", json!([t("codex", "gpt-image-1"), t("codex", "gpt-5")]))]},
                "emb_alias": {"target_groups": [g("g", "in_order", json!([t("chat", "c"), t("emb", "e")]))]},
                "chat_alias": {"target_groups": [g("g", "in_order", json!([t("chat", "c"), t("chat", "t")]))]},
                "api_match": {"priority": "api_match", "target_groups": [g("g", "in_order", json!([t("chat", "c"), t("emb", "e")]))]}
            }
        }));
        assert_eq!(
            ids(
                &resolve_candidates(&rt, "image_alias", Some("images"), None)
                    .await
                    .unwrap()
            ),
            vec!["codex/gpt-image-1"]
        );
        assert_eq!(
            ids(
                &resolve_candidates(&rt, "image_alias", Some("responses"), None)
                    .await
                    .unwrap()
            ),
            vec!["codex/gpt-image-1", "codex/gpt-5"]
        );
        assert_eq!(
            ids(
                &resolve_candidates(&rt, "emb_alias", Some("embeddings"), None)
                    .await
                    .unwrap()
            ),
            vec!["emb/e"]
        );
        // No embeddings-capable target → fall back to all healthy targets.
        assert_eq!(
            ids(
                &resolve_candidates(&rt, "chat_alias", Some("embeddings"), None)
                    .await
                    .unwrap()
            ),
            vec!["chat/c", "chat/t"]
        );
        assert_eq!(
            ids(
                &resolve_candidates(&rt, "api_match", Some("embeddings"), None)
                    .await
                    .unwrap()
            ),
            vec!["emb/e"]
        );
        assert_eq!(
            ids(
                &resolve_candidates(&rt, "api_match", Some("messages"), None)
                    .await
                    .unwrap()
            ),
            vec!["chat/c", "emb/e"]
        );
        assert_eq!(
            ids(&resolve_candidates(&rt, "api_match", Some("chat"), None)
                .await
                .unwrap()),
            vec!["chat/c"]
        );
    }

    #[test]
    fn service_tier_split() {
        let alias = json!({"target_groups": [g("main", "in_order", json!([t("p1", "model-1")]))]});
        let rt = rt_with(json!({"gpt-6-luna": alias.clone()}));
        assert_eq!(
            split_service_tier(&rt, "gpt-6-luna@flex"),
            ServiceTierSplit {
                model: "gpt-6-luna".into(),
                service_tier: Some("flex".into())
            }
        );
        assert_eq!(
            split_service_tier(&rt, "nope@flex"),
            ServiceTierSplit::unchanged("nope@flex")
        );
        assert_eq!(
            split_service_tier(&rt, "gpt-6-luna@turbo"),
            ServiceTierSplit::unchanged("gpt-6-luna@turbo")
        );
        assert_eq!(
            split_service_tier(&rt, "direct/p1/model-1@flex"),
            ServiceTierSplit::unchanged("direct/p1/model-1@flex")
        );
        let mut with_aa = alias.clone();
        with_aa["additional_aliases"] = json!(["luna", "luna@flex"]);
        let rt = rt_with(json!({"gpt-6-luna": with_aa, "gpt-6-luna@flex": alias}));
        assert_eq!(
            split_service_tier(&rt, "luna@priority"),
            ServiceTierSplit {
                model: "luna".into(),
                service_tier: Some("priority".into())
            }
        );
        assert_eq!(
            split_service_tier(&rt, "luna@flex"),
            ServiceTierSplit::unchanged("luna@flex")
        );
        assert_eq!(
            split_service_tier(&rt, "gpt-6-luna@flex"),
            ServiceTierSplit::unchanged("gpt-6-luna@flex")
        );
    }
}
