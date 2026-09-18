//! Claude Code request identity used by Anthropic OAuth/masking routes.

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub const CLAUDE_CODE_VERSION: &str = "2.1.258";
const IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";
const BILLING_PREFIX: &str = "x-anthropic-billing-header:";
const SALT: &str = "59cf53e54c78";

pub fn user_agent() -> String {
    format!("claude-cli/{CLAUDE_CODE_VERSION} (external, cli)")
}

fn first_user_text(body: &Value) -> String {
    body.get("messages")
        .and_then(Value::as_array)
        .and_then(|messages| {
            messages
                .iter()
                .find(|m| m.get("role").and_then(Value::as_str) == Some("user"))
        })
        .and_then(|message| match message.get("content") {
            Some(Value::String(text)) => Some(text.clone()),
            Some(Value::Array(parts)) => parts
                .iter()
                .find_map(|part| part.get("text").and_then(Value::as_str).map(str::to_owned)),
            _ => None,
        })
        .unwrap_or_default()
}

fn billing_header(body: &Value) -> String {
    let text = first_user_text(body);
    let chars: String = [4usize, 7, 20]
        .iter()
        .map(|&i| text.chars().nth(i).unwrap_or('0'))
        .collect();
    let mut hash = Sha256::new();
    hash.update(format!("{SALT}{chars}{CLAUDE_CODE_VERSION}"));
    let digest = hex::encode(hash.finalize());
    format!(
        "{BILLING_PREFIX} cc_version={CLAUDE_CODE_VERSION}.{}; cc_entrypoint=cli; cch=00000;",
        &digest[..3]
    )
}

pub fn mask_body(body: &Value) -> Value {
    let mut result = body.clone();
    let had_system = body
        .get("system")
        .and_then(Value::as_array)
        .is_some_and(|items| !items.is_empty());
    result["system"] = json!([
        {"type":"text", "text": billing_header(body)},
        {"type":"text", "text": IDENTITY},
        {"type":"text", "text": "You are an interactive agent that helps users with software engineering tasks. Keep responses concise and focused on the user's request."}
    ]);
    if had_system {
        if let Some(messages) = result.get_mut("messages").and_then(Value::as_array_mut) {
            if let Some(message) = messages
                .iter_mut()
                .find(|m| m.get("role").and_then(Value::as_str) == Some("user"))
            {
                let reminder = "<system-reminder>\nUse the available tools when needed to help with software engineering tasks.\nKeep responses concise and focused on the user's request.\n</system-reminder>\n";
                match message.get("content") {
                    Some(Value::Array(content)) => {
                        let mut updated = vec![json!({"type":"text", "text": reminder})];
                        updated.extend(content.clone());
                        message["content"] = Value::Array(updated);
                    }
                    Some(Value::String(content)) => {
                        message["content"] = json!(format!("{reminder}{content}"))
                    }
                    _ => message["content"] = json!([{"type":"text", "text": reminder}]),
                }
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn masks_identity_without_mutating_input() {
        let input = json!({"system":[{"type":"text","text":"third party"}],"messages":[{"role":"user","content":"hello"}]});
        let output = mask_body(&input);
        assert_eq!(user_agent(), "claude-cli/2.1.258 (external, cli)");
        assert!(output["system"][0]["text"]
            .as_str()
            .unwrap()
            .contains("cc_version=2.1.258."));
        assert_eq!(output["system"][1]["text"], IDENTITY);
        assert!(output["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("system-reminder"));
        assert_eq!(input["system"][0]["text"], "third party");
    }
}
