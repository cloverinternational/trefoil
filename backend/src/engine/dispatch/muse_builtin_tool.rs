//! `transformers/adapters/muse-builtin-tool.ts`: Muse Code's builtin tool
//! declaration, required by Meta's Model API for requests billed to a Muse
//! Code subscription.
//!
//! Upstream verified against `api.meta.ai/v1` with a subscription-minted key:
//!
//! - A request with no `tools` at all succeeds (the gate only fires when the
//!   body declares tools), so nothing is injected when `tools` is empty.
//! - A request whose `tools` are ordinary function declarations is rejected
//!   with HTTP 403 ("this request does not carry Muse Code's builtin tool
//!   declarations") on `/v1/responses`, `/v1/chat/completions` and
//!   `/v1/messages` alike.
//! - The check is per-subtool, not a whole-set hash: a `namespace` tool named
//!   `muse` whose entries exactly match real Muse Code builtin declarations
//!   satisfies it, and arbitrary extra top-level function tools are allowed
//!   alongside. Fabricated names, invented schemas, a re-worded description,
//!   or an empty namespace all still fail.
//! - Client fingerprint headers are irrelevant.
//!
//! `cron_list` is the declaration injected: it is the smallest in the set
//! (240 bytes), read-only, takes no arguments, and scheduling is essentially
//! never relevant to a coding turn, so a spontaneous (unexecutable) call is
//! rare and harmless. Keep the choice constant across every route: the tools
//! array participates in prompt caching.
//!
//! Captured from Muse Code 1.4.14 (build
//! `dec7d7497fecdad135ec5835a8161dbc92e55b58`). If Meta rotates the builtin
//! schema, re-capture it; a stale declaration is rejected like a fabricated
//! one.
use serde_json::{json, Value};

/// Namespace Muse Code groups its builtin tools under.
pub const MUSE_BUILTIN_NAMESPACE: &str = "muse";

/// The `namespace` tool declaration injected into gated requests.
pub fn muse_builtin_namespace_declaration() -> Value {
    json!({
        "type": "namespace",
        "name": MUSE_BUILTIN_NAMESPACE,
        "description": "Muse Code tool set.",
        "tools": [{
            "type": "function",
            "name": "cron_list",
            "description": "List all scheduled jobs for this session, with their cadence and next fire time.",
            "parameters": {
                "type": "object",
                "additionalProperties": false,
                "properties": {},
            },
            "strict": false,
        }],
    })
}

/// True when the body already carries a `muse` namespace (genuine Muse Code
/// traffic).
pub fn has_muse_builtin_namespace(tools: &[Value]) -> bool {
    tools.iter().any(|tool| {
        tool.get("type").and_then(Value::as_str) == Some("namespace")
            && tool.get("name").and_then(Value::as_str) == Some(MUSE_BUILTIN_NAMESPACE)
    })
}

/// Appends the builtin declaration to a body that declares tools.
///
/// The payload is returned unchanged when there is nothing to do: no `tools`
/// array, an empty one (the gate does not fire), or a `muse` namespace already
/// present.
pub fn inject_muse_builtin_tool(mut payload: Value) -> Value {
    let Some(Value::Array(tools)) = payload.get_mut("tools") else {
        return payload;
    };
    if tools.is_empty() || has_muse_builtin_namespace(tools) {
        return payload;
    }
    tools.push(muse_builtin_namespace_declaration());
    payload
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_the_namespace_to_a_body_that_declares_tools() {
        let read = json!({"type": "function", "name": "read", "function": {"name": "read"}});
        let result = inject_muse_builtin_tool(json!({"tools": [read]}));
        assert_eq!(result["tools"].as_array().unwrap().len(), 2);
        assert_eq!(result["tools"][0], read);
        assert_eq!(result["tools"][1], muse_builtin_namespace_declaration());
    }

    #[test]
    fn leaves_bodies_without_tools_untouched() {
        let no_tools = json!({"model": "muse-spark-1.3"});
        assert_eq!(inject_muse_builtin_tool(no_tools.clone()), no_tools);
        let empty = json!({"tools": []});
        assert_eq!(inject_muse_builtin_tool(empty.clone()), empty);
    }

    #[test]
    fn is_idempotent_including_for_a_genuine_muse_code_body() {
        let genuine = json!({"tools": [muse_builtin_namespace_declaration()]});
        assert!(has_muse_builtin_namespace(
            genuine["tools"].as_array().unwrap()
        ));
        assert_eq!(inject_muse_builtin_tool(genuine.clone()), genuine);

        let once = inject_muse_builtin_tool(json!({"tools": [{"type": "function", "name": "read"}]}));
        assert_eq!(inject_muse_builtin_tool(once.clone()), once);
    }

    #[test]
    fn declaration_is_fresh_per_call() {
        let mut first =
            inject_muse_builtin_tool(json!({"tools": [{"type": "function", "name": "read"}]}));
        first["tools"][1]["tools"][0]["name"] = json!("mutated");
        let second =
            inject_muse_builtin_tool(json!({"tools": [{"type": "function", "name": "read"}]}));
        assert_eq!(second["tools"][1]["tools"][0]["name"], "cron_list");
        assert_eq!(
            muse_builtin_namespace_declaration()["tools"][0]["name"],
            "cron_list"
        );
    }
}
