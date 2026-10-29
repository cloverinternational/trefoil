//! `services/models/cache-control-injection.ts`: prompt-cache breakpoint
//! injection for translated Messages requests.
//!
//! An Anthropic prompt cache is a prefix cache keyed on `cache_control`
//! breakpoints. A request that arrives on `/v1/chat/completions` or
//! `/v1/responses` and is translated to Messages never carries breakpoints
//! (the incoming formats have no way to express them), so every turn pays full
//! input price. This module fills those breakpoints in at the highest-value
//! positions.
//!
//! Placement follows Anthropic's render order — `tools` → `system` →
//! `messages` — and the documented cap of four breakpoints per request:
//!
//! 1. the last tool definition (caches the whole tools block, the most stable
//!    and most expensive part of an agent prompt),
//! 2. the last `system` text block,
//! 3. the trailing message(s), walking back from the end of the conversation.
//!
//! The third position is the "walking" breakpoint: it needs no per-turn state.
//! Anthropic hashes cumulative prefixes, so a breakpoint that is always on the
//! current turn advances by itself. Two trailing marks are used by default
//! because each breakpoint only looks 20 blocks back for a previously written
//! prefix: `messages[-1]` anchors the current turn, while `messages[-2]` still
//! points at the previous turn's end and therefore matches last turn's write
//! exactly, even when a turn emits more than 20 blocks (large tool results).
//!
//! Guard rails, all deliberate:
//! - only translated requests targeting Messages are touched; a native
//!   Messages client owns its own breakpoints,
//! - a request that already carries any breakpoint (message-level,
//!   block-level, tool-level, or Anthropic's top-level automatic-caching
//!   field) is left completely untouched,
//! - only cacheable block types are marked (`thinking` / `redacted_thinking`
//!   are skipped), and nothing is ever mutated in place.
use super::util::{base_type, js_trim};
use serde_json::{json, Value};

/// Anthropic's documented maximum number of `cache_control` breakpoints per
/// request.
pub const MAX_CACHE_BREAKPOINTS: usize = 4;

/// Default trailing (walking) breakpoint count — see the module comment.
pub const DEFAULT_MESSAGE_BREAKPOINTS: f64 = 2.0;

/// `CacheControlInjectionOptions`.
#[derive(Debug, Clone, Default)]
pub struct CacheControlInjectionOptions {
    /// Mark the last tool definition. `None` (or anything but `false`) marks.
    pub tools: Option<bool>,
    /// Mark the last system text block. `None` (or anything but `false`) marks.
    pub system: Option<bool>,
    /// Number of trailing messages to mark. `None` means the default (2).
    pub message_breakpoints: Option<f64>,
    /// Cache TTL (`"5m"` / `"1h"`). Anything else leaves Anthropic's default.
    pub ttl: Option<String>,
}

/// `CacheControlInjectionResult`.
#[derive(Debug, Clone, PartialEq)]
pub struct CacheControlInjectionResult {
    pub payload: Value,
    /// Human-readable positions that were marked, in the order they were
    /// marked.
    pub sites: Vec<String>,
}

/// Block types Anthropic accepts `cache_control` on. `thinking` and
/// `redacted_thinking` are deliberately absent, so a message ending on a
/// thinking block walks back to its last cacheable block instead.
const CACHEABLE_BLOCK_TYPES: [&str; 5] = ["text", "image", "document", "tool_use", "tool_result"];

fn is_nil(value: Option<&Value>) -> bool {
    matches!(value, None | Some(Value::Null))
}

fn has_breakpoint(value: &Value) -> bool {
    value.is_object() && !is_nil(value.get("cache_control"))
}

/// `countCacheBreakpoints(payload)`: every `cache_control` breakpoint already
/// present in a Messages payload, including the top-level (automatic caching)
/// field, which Anthropic counts against the same four-breakpoint budget.
pub fn count_cache_breakpoints(payload: &Value) -> usize {
    if !payload.is_object() {
        return 0;
    }
    let mut count = usize::from(has_breakpoint(payload));
    for key in ["tools", "system"] {
        if let Some(Value::Array(items)) = payload.get(key) {
            count += items.iter().filter(|i| has_breakpoint(i)).count();
        }
    }
    if let Some(Value::Array(messages)) = payload.get("messages") {
        for message in messages {
            if has_breakpoint(message) {
                count += 1;
            }
            if let Some(Value::Array(blocks)) = message.get("content") {
                count += blocks.iter().filter(|b| has_breakpoint(b)).count();
            }
        }
    }
    count
}

fn build_marker(ttl: Option<&str>) -> Value {
    // '5m' is Anthropic's default but is still forwarded explicitly when asked
    // for, so a mixed-TTL admin choice reaches the wire unchanged.
    match ttl {
        Some(t @ ("5m" | "1h")) => json!({"type": "ephemeral", "ttl": t}),
        _ => json!({"type": "ephemeral"}),
    }
}

fn is_blank_text(value: Option<&Value>) -> bool {
    value
        .and_then(Value::as_str)
        .is_none_or(|t| js_trim(t).is_empty())
}

fn is_cacheable_block(block: &Value) -> bool {
    if !block.is_object() || !is_nil(block.get("cache_control")) {
        return false;
    }
    let ty = block.get("type").and_then(Value::as_str);
    if !ty.is_some_and(|t| CACHEABLE_BLOCK_TYPES.contains(&t)) {
        return false;
    }
    !(ty == Some("text") && is_blank_text(block.get("text")))
}

/// `{...block, cache_control: marker}`.
fn with_marker(block: &Value, marker: &Value) -> Value {
    let mut map = block.as_object().cloned().unwrap_or_default();
    map.insert("cache_control".into(), marker.clone());
    Value::Object(map)
}

/// Returns the marked system value, or `None` when there is nothing to mark.
fn mark_system(system: Option<&Value>, marker: &Value) -> Option<Value> {
    match system {
        Some(Value::Array(blocks)) => {
            let mut blocks = blocks.clone();
            for i in (0..blocks.len()).rev() {
                let block = &blocks[i];
                if block.get("type").and_then(Value::as_str) == Some("text")
                    && !is_blank_text(block.get("text"))
                    && is_nil(block.get("cache_control"))
                {
                    blocks[i] = with_marker(block, marker);
                    return Some(Value::Array(blocks));
                }
            }
            None
        }
        // A bare string system prompt has nowhere to hang `cache_control`, so
        // it becomes the single-text-block form Anthropic also accepts.
        Some(Value::String(s)) if !js_trim(s).is_empty() => Some(json!([
            {"type": "text", "text": s, "cache_control": marker}
        ])),
        _ => None,
    }
}

/// Returns the marked message, or `None` when the message cannot take a
/// breakpoint.
fn mark_message(message: &Value, marker: &Value) -> Option<Value> {
    if !message.is_object() || !is_nil(message.get("cache_control")) {
        return None;
    }
    match message.get("content") {
        Some(Value::String(s)) => {
            if js_trim(s).is_empty() {
                return None;
            }
            let mut next = message.as_object().cloned().unwrap_or_default();
            next.insert(
                "content".into(),
                json!([{"type": "text", "text": s, "cache_control": marker}]),
            );
            Some(Value::Object(next))
        }
        Some(Value::Array(blocks)) => {
            let mut content = blocks.clone();
            for i in (0..content.len()).rev() {
                if !is_cacheable_block(&content[i]) {
                    continue;
                }
                content[i] = with_marker(&content[i], marker);
                let mut next = message.as_object().cloned().unwrap_or_default();
                next.insert("content".into(), Value::Array(content));
                return Some(Value::Object(next));
            }
            None
        }
        _ => None,
    }
}

/// `injectCacheControlBreakpoints(payload, incomingApiType, targetApiType,
/// options)`: injects `cache_control` breakpoints into a translated Messages
/// payload.
///
/// Returns the original payload untouched (with an empty `sites` list)
/// whenever the request is out of scope or already carries breakpoints, so
/// callers can apply this unconditionally.
pub fn inject_cache_control_breakpoints(
    payload: Value,
    incoming_api_type: &str,
    target_api_type: &str,
    options: &CacheControlInjectionOptions,
) -> CacheControlInjectionResult {
    let unchanged = |payload: Value| CacheControlInjectionResult {
        payload,
        sites: Vec::new(),
    };
    if !payload.is_object()
        || base_type(target_api_type) != "messages"
        || base_type(incoming_api_type) == "messages"
        || count_cache_breakpoints(&payload) > 0
    {
        return unchanged(payload);
    }

    let marker = build_marker(options.ttl.as_deref());
    let mut sites: Vec<String> = Vec::new();
    let mut result = payload;
    let mut remaining = MAX_CACHE_BREAKPOINTS;

    // 1. Tool definitions. Tools render first, so they anchor every later
    //    prefix.
    if options.tools != Some(false) {
        if let Some(Value::Array(tools)) = result
            .get("tools")
            .filter(|t| t.as_array().is_some_and(|a| !a.is_empty()))
        {
            let mut tools = tools.clone();
            let last = tools.len() - 1;
            tools[last] = with_marker(&tools[last], &marker);
            result["tools"] = Value::Array(tools);
            sites.push("tools".into());
            remaining -= 1;
        }
    }

    // 2. System prompt. Its prefix includes the tools above, so tool churn
    //    already invalidates this one — nothing to do about that here.
    if options.system != Some(false) && remaining > 0 {
        if let Some(system) = mark_system(result.get("system"), &marker) {
            result["system"] = system;
            sites.push("system".into());
            remaining -= 1;
        }
    }

    // 3. Trailing messages: current turn end, then the previous turn end(s).
    let requested = options
        .message_breakpoints
        .unwrap_or(DEFAULT_MESSAGE_BREAKPOINTS);
    let depth = requested.trunc().min(remaining as f64).max(0.0);
    if depth > 0.0 {
        if let Some(Value::Array(existing)) = result.get("messages") {
            let mut messages = existing.clone();
            let len = messages.len();
            // Collected deepest-first so the log reads in conversation order.
            let mut message_sites: Vec<String> = Vec::new();
            let mut placed = 0.0;
            for i in (0..len).rev() {
                if placed >= depth {
                    break;
                }
                let Some(marked) = mark_message(&messages[i], &marker) else {
                    continue;
                };
                messages[i] = marked;
                placed += 1.0;
                message_sites.insert(0, format!("messages[{}]", i as i64 - len as i64));
            }
            if placed > 0.0 {
                result["messages"] = Value::Array(messages);
                sites.extend(message_sites);
            }
        }
    }

    CacheControlInjectionResult {
        payload: result,
        sites,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> CacheControlInjectionOptions {
        CacheControlInjectionOptions::default()
    }

    fn inject(
        payload: Value,
        options: &CacheControlInjectionOptions,
    ) -> CacheControlInjectionResult {
        inject_cache_control_breakpoints(payload, "chat", "messages", options)
    }

    fn eph() -> Value {
        json!({"type": "ephemeral"})
    }

    fn translated() -> Value {
        json!({
            "model": "claude-sonnet-4-6",
            "max_tokens": 1024,
            "system": [{"type": "text", "text": "You are helpful."}],
            "tools": [
                {"name": "a", "input_schema": {"type": "object"}},
                {"name": "b", "input_schema": {"type": "object"}},
            ],
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "first"}]},
                {"role": "assistant", "content": [{"type": "text", "text": "reply"}]},
                {"role": "user", "content": [{"type": "text", "text": "second"}]},
            ],
        })
    }

    #[test]
    fn marks_tools_system_and_two_trailing_messages_by_default() {
        let input = translated();
        let out = inject(input.clone(), &opts());
        assert_eq!(
            out.sites,
            ["tools", "system", "messages[-2]", "messages[-1]"]
        );
        let p = &out.payload;
        assert!(p["tools"][0].get("cache_control").is_none());
        assert_eq!(p["tools"][1]["cache_control"], eph());
        assert_eq!(p["system"][0]["cache_control"], eph());
        assert!(p["messages"][0]["content"][0]
            .get("cache_control")
            .is_none());
        assert_eq!(p["messages"][1]["content"][0]["cache_control"], eph());
        assert_eq!(p["messages"][2]["content"][0]["cache_control"], eph());
        // The caller's payload is never mutated.
        assert_eq!(input, translated());
    }

    #[test]
    fn only_translated_messages_targets_are_touched() {
        let native =
            inject_cache_control_breakpoints(translated(), "messages", "messages", &opts());
        assert!(native.sites.is_empty());
        assert_eq!(native.payload, translated());
        let other = inject_cache_control_breakpoints(translated(), "chat", "responses", &opts());
        assert!(other.sites.is_empty());
        assert_eq!(other.payload, translated());
        let not_object = inject_cache_control_breakpoints(json!([1]), "chat", "messages", &opts());
        assert_eq!(not_object.payload, json!([1]));
    }

    #[test]
    fn existing_breakpoints_anywhere_leave_the_request_untouched() {
        let mut with_tool = translated();
        with_tool["tools"][0]["cache_control"] = eph();
        let mut with_block = translated();
        with_block["messages"][0]["content"][0]["cache_control"] = eph();
        let mut with_message = translated();
        with_message["messages"][0]["cache_control"] = eph();
        let mut with_system = translated();
        with_system["system"][0]["cache_control"] = eph();
        let mut top_level = translated();
        top_level["cache_control"] = eph();
        for payload in [with_tool, with_block, with_message, with_system, top_level] {
            let out = inject(payload.clone(), &opts());
            assert!(out.sites.is_empty());
            assert_eq!(out.payload, payload);
        }
        assert_eq!(count_cache_breakpoints(&translated()), 0);
    }

    #[test]
    fn honours_toggles_and_breakpoint_budget() {
        let out = inject(
            translated(),
            &CacheControlInjectionOptions {
                tools: Some(false),
                system: Some(false),
                message_breakpoints: Some(1.0),
                ..opts()
            },
        );
        assert_eq!(out.sites, ["messages[-1]"]);

        // 0 keeps breakpoints on the static prefix only.
        let out = inject(
            translated(),
            &CacheControlInjectionOptions {
                message_breakpoints: Some(0.0),
                ..opts()
            },
        );
        assert_eq!(out.sites, ["tools", "system"]);

        // Never exceeds Anthropic's four-breakpoint cap.
        let out = inject(
            translated(),
            &CacheControlInjectionOptions {
                message_breakpoints: Some(4.0),
                ..opts()
            },
        );
        assert_eq!(out.sites.len(), 4);
        assert_eq!(count_cache_breakpoints(&out.payload), 4);
    }

    #[test]
    fn ttl_is_forwarded_only_when_recognised() {
        let one_hour = inject(
            translated(),
            &CacheControlInjectionOptions {
                ttl: Some("1h".into()),
                ..opts()
            },
        );
        assert_eq!(
            one_hour.payload["tools"][1]["cache_control"],
            json!({"type": "ephemeral", "ttl": "1h"})
        );
        let five = inject(
            translated(),
            &CacheControlInjectionOptions {
                ttl: Some("5m".into()),
                ..opts()
            },
        );
        assert_eq!(
            five.payload["system"][0]["cache_control"],
            json!({"type": "ephemeral", "ttl": "5m"})
        );
        let bogus = inject(
            translated(),
            &CacheControlInjectionOptions {
                ttl: Some("2h".into()),
                ..opts()
            },
        );
        assert_eq!(bogus.payload["tools"][1]["cache_control"], eph());
    }

    #[test]
    fn string_system_and_string_message_content_are_expanded() {
        let out = inject(
            json!({
                "system": "Be brief.",
                "messages": [{"role": "user", "content": "hello"}],
            }),
            &opts(),
        );
        assert_eq!(out.sites, ["system", "messages[-1]"]);
        assert_eq!(
            out.payload["system"],
            json!([{"type": "text", "text": "Be brief.", "cache_control": eph()}])
        );
        assert_eq!(
            out.payload["messages"][0]["content"],
            json!([{"type": "text", "text": "hello", "cache_control": eph()}])
        );
    }

    #[test]
    fn skips_blank_and_uncacheable_blocks() {
        let out = inject(
            json!({
                "system": [
                    {"type": "text", "text": "keep"},
                    {"type": "text", "text": "   "},
                ],
                "messages": [
                    {"role": "user", "content": [
                        {"type": "text", "text": "question"},
                        {"type": "thinking", "thinking": "hmm", "signature": "s"},
                    ]},
                    {"role": "assistant", "content": ""},
                ],
            }),
            &opts(),
        );
        // The blank trailing system block and the empty assistant message are
        // skipped; the thinking block is walked past.
        assert_eq!(out.sites, ["system", "messages[-2]"]);
        assert_eq!(out.payload["system"][0]["cache_control"], eph());
        assert!(out.payload["system"][1].get("cache_control").is_none());
        assert_eq!(
            out.payload["messages"][0]["content"][0]["cache_control"],
            eph()
        );
        assert!(out.payload["messages"][0]["content"][1]
            .get("cache_control")
            .is_none());
    }
}
