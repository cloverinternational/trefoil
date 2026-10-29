//! `services/models/model-behaviors.ts`: alias-level `advanced` behaviors.
//!
//! Some behaviors also apply implicitly: `inject_cache_control` is enabled for
//! any alias that dispatches to Anthropic's own API (see `resolve_behaviors`),
//! because a translated request can never carry breakpoints of its own.
use super::cache_control_injection::{
    inject_cache_control_breakpoints, CacheControlInjectionOptions,
};
use super::util::del;
use crate::engine::js::*;
use serde_json::{json, Value};

/// `BehaviorContext`: context passed to every behavior so they can inspect the
/// full request situation.
#[derive(Debug, Clone, Copy, Default)]
pub struct BehaviorContext<'a> {
    /// The original API type the client used (e.g. `messages`, `chat`).
    pub incoming_api_type: &'a str,
    /// The canonical alias name (key in `config.models`).
    pub canonical_model: &'a str,
    /// The resolved wire API type of the target (e.g. `messages`, `chat`).
    pub target_api_type: &'a str,
    /// True when the resolved route dispatches to Anthropic's own API. Aliases
    /// that don't configure `inject_cache_control` get it implicitly here.
    pub auto_enable_cache_control: bool,
    /// True when the native Anthropic path rebuilds `system` after behaviors
    /// run (OAuth and Claude-masking routes). A breakpoint placed there would
    /// be discarded, so the injector skips that site.
    pub system_rewritten_by_masking: bool,
}

/// `DEFAULT_INJECT_CACHE_CONTROL_BEHAVIOR`: the schema defaults materialised,
/// for aliases that get the behavior implicitly (auto-enable on
/// Anthropic-owned routes).
fn default_inject_cache_control_behavior() -> Value {
    json!({
        "type": "inject_cache_control",
        "enabled": true,
        "tools": true,
        "system": true,
        "message_breakpoints": 2,
    })
}

/// `applyModelBehaviors(payload, behaviors, ctx)`. Unknown behavior types are
/// ignored (TS logs a warning).
pub fn apply_model_behaviors(
    payload: Value,
    behaviors: Option<&Value>,
    ctx: &BehaviorContext<'_>,
) -> Value {
    let effective = resolve_behaviors(behaviors, ctx);
    if effective.is_empty() {
        return payload;
    }
    let mut result = payload;
    for behavior in &effective {
        if behavior.get("enabled") == Some(&Value::Bool(false)) {
            continue;
        }
        result = apply_behavior(result, behavior, ctx);
    }
    result
}

/// Expands the configured behavior list with the implicit ones. An alias that
/// configures `inject_cache_control` at all (even with `enabled: false`)
/// always wins over the auto-enable below.
fn resolve_behaviors(behaviors: Option<&Value>, ctx: &BehaviorContext<'_>) -> Vec<Value> {
    let mut configured: Vec<Value> = behaviors
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if !ctx.auto_enable_cache_control
        || configured
            .iter()
            .any(|b| get_str(b, &["type"]) == Some("inject_cache_control"))
    {
        return configured;
    }
    configured.push(default_inject_cache_control_behavior());
    configured
}

fn apply_behavior(payload: Value, behavior: &Value, ctx: &BehaviorContext<'_>) -> Value {
    match get_str(behavior, &["type"]) {
        Some("strip_adaptive_thinking") => apply_strip_adaptive_thinking(payload, ctx),
        Some("inject_cache_control") => apply_inject_cache_control(payload, behavior, ctx),
        _ => payload,
    }
}

fn apply_strip_adaptive_thinking(payload: Value, ctx: &BehaviorContext<'_>) -> Value {
    if ctx.incoming_api_type != "messages" {
        return payload;
    }
    let adaptive = payload
        .get("thinking")
        .filter(|t| t.is_object())
        .is_some_and(|t| get_str(t, &["type"]) == Some("adaptive"));
    if !adaptive {
        return payload;
    }
    let mut result = payload.as_object().cloned().unwrap_or_default();
    del(&mut result, "thinking");
    Value::Object(result)
}

/// inject_cache_control
///
/// Only active when a chat/responses request is translated to Messages. Fills
/// `cache_control` breakpoints into the static prefix (tools, system) and the
/// conversation tail. See `cache_control_injection`.
fn apply_inject_cache_control(
    payload: Value,
    behavior: &Value,
    ctx: &BehaviorContext<'_>,
) -> Value {
    // Individual fields can be missing at runtime: `advanced` is persisted as
    // raw JSON and read back without re-parsing through the schema, so the
    // injector applies the documented defaults rather than assuming them.
    let options = CacheControlInjectionOptions {
        tools: behavior.get("tools").and_then(Value::as_bool),
        // The masking transform rebuilds `system` from fixed Claude Code
        // blocks after this runs, dropping any marker placed here — and its
        // billing header is payload-derived, so that prefix would not cache
        // reliably anyway. Skipping the site keeps the debug log honest; the
        // freed slot is left unused rather than spent on an extra trailing
        // breakpoint.
        system: Some(
            behavior.get("system") != Some(&Value::Bool(false)) && !ctx.system_rewritten_by_masking,
        ),
        message_breakpoints: behavior.get("message_breakpoints").and_then(Value::as_f64),
        ttl: behavior
            .get("ttl")
            .and_then(Value::as_str)
            .map(str::to_owned),
    };
    inject_cache_control_breakpoints(
        payload,
        ctx.incoming_api_type,
        ctx.target_api_type,
        &options,
    )
    .payload
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx<'a>(incoming: &'a str, target: &'a str, auto: bool) -> BehaviorContext<'a> {
        BehaviorContext {
            incoming_api_type: incoming,
            canonical_model: "a",
            target_api_type: target,
            auto_enable_cache_control: auto,
            system_rewritten_by_masking: false,
        }
    }

    fn translated() -> Value {
        json!({
            "model": "claude-sonnet-4-6",
            "system": [{"type": "text", "text": "sys"}],
            "tools": [{"name": "t", "input_schema": {"type": "object"}}],
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
        })
    }

    #[test]
    fn strip_adaptive() {
        let b = json!([{"type": "strip_adaptive_thinking"}]);
        let p = json!({"model": "m", "thinking": {"type": "adaptive"}, "x": 1});
        let messages = ctx("messages", "messages", false);
        let chat = ctx("chat", "messages", false);
        assert_eq!(
            apply_model_behaviors(p.clone(), Some(&b), &messages),
            json!({"model": "m", "x": 1})
        );
        assert_eq!(apply_model_behaviors(p.clone(), Some(&b), &chat), p);
        let disabled = json!([{"type": "strip_adaptive_thinking", "enabled": false}]);
        assert_eq!(
            apply_model_behaviors(p.clone(), Some(&disabled), &messages),
            p
        );
        let enabled = json!({"model": "m", "thinking": {"type": "enabled", "budget_tokens": 5}});
        assert_eq!(
            apply_model_behaviors(enabled.clone(), Some(&b), &messages),
            enabled
        );
    }

    #[test]
    fn no_behaviors_and_no_auto_enable_is_a_noop() {
        let p = translated();
        let c = ctx("chat", "messages", false);
        assert_eq!(apply_model_behaviors(p.clone(), None, &c), p);
        assert_eq!(apply_model_behaviors(p.clone(), Some(&json!([])), &c), p);
        // Unknown behavior types are ignored.
        assert_eq!(
            apply_model_behaviors(p.clone(), Some(&json!([{"type": "mystery"}])), &c),
            p
        );
    }

    #[test]
    fn explicit_inject_cache_control_applies_with_configured_options() {
        let b = json!([{"type": "inject_cache_control", "tools": false, "message_breakpoints": 1, "ttl": "1h"}]);
        let out = apply_model_behaviors(translated(), Some(&b), &ctx("chat", "messages", false));
        let marker = json!({"type": "ephemeral", "ttl": "1h"});
        assert!(out["tools"][0].get("cache_control").is_none());
        assert_eq!(out["system"][0]["cache_control"], marker);
        assert_eq!(out["messages"][0]["content"][0]["cache_control"], marker);
    }

    #[test]
    fn auto_enables_for_anthropic_routes_with_schema_defaults() {
        let out = apply_model_behaviors(translated(), None, &ctx("chat", "messages", true));
        let marker = json!({"type": "ephemeral"});
        assert_eq!(out["tools"][0]["cache_control"], marker);
        assert_eq!(out["system"][0]["cache_control"], marker);
        assert_eq!(out["messages"][0]["content"][0]["cache_control"], marker);
        // A native Messages client keeps its own breakpoints.
        let native = apply_model_behaviors(translated(), None, &ctx("messages", "messages", true));
        assert_eq!(native, translated());
    }

    #[test]
    fn explicit_configuration_overrides_the_auto_enable() {
        let off = json!([{"type": "inject_cache_control", "enabled": false}]);
        let c = ctx("chat", "messages", true);
        assert_eq!(
            apply_model_behaviors(translated(), Some(&off), &c),
            translated()
        );
        // Explicit-with-options is honoured and not duplicated by the default.
        let tools_only =
            json!([{"type": "inject_cache_control", "system": false, "message_breakpoints": 0}]);
        let out = apply_model_behaviors(translated(), Some(&tools_only), &c);
        assert!(out["tools"][0].get("cache_control").is_some());
        assert!(out["system"][0].get("cache_control").is_none());
        assert!(out["messages"][0]["content"][0]
            .get("cache_control")
            .is_none());
    }

    #[test]
    fn masking_routes_skip_the_system_breakpoint() {
        let mut c = ctx("chat", "messages", true);
        c.system_rewritten_by_masking = true;
        let out = apply_model_behaviors(translated(), None, &c);
        assert!(out["system"][0].get("cache_control").is_none());
        assert!(out["tools"][0].get("cache_control").is_some());
        assert!(out["messages"][0]["content"][0]
            .get("cache_control")
            .is_some());
    }
}
