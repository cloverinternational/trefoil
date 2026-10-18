//! Claude Code fingerprinting for native Anthropic OAuth / Claude-masking
//! routes: `transformers/oauth/oauth-claude.ts` (`applyClaudeOAuthTransform`,
//! `canonicalizeOAuthToolName`) and `transformers/oauth/masking/*`
//! (`applyClaudeCodeMasking`, `getStainlessHeaders`, `reverseToolRenames`).
//!
//! `applyClaudeOAuthTransform` writes a billing header + CCH that the masking
//! step's `injectClaudeCodeIdentity` always discards (it rebuilds `system[]`
//! and filters billing-prefixed blocks), so only its observable effects are
//! ported: the effort clamp, the TitleCase tool remap, the system rebuild and
//! the forwarded-system reminder.
use crate::engine::anthropic::thinking_clamp::clamp_anthropic_effort_and_thinking;
use crate::engine::dispatch::util::sha256_hex;
use crate::engine::js::*;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

pub const REQUIRED_BETAS: &[&str] = &[
    "oauth-2025-04-20",
    "claude-code-20250219",
    "interleaved-thinking-2025-05-14",
    "thinking-token-count-2026-05-13",
    "advanced-tool-use-2025-11-20",
    "context-management-2025-06-27",
    "prompt-caching-scope-2026-01-05",
    "mid-conversation-system-2026-04-07",
    "advisor-tool-2026-03-01",
    "effort-2025-11-24",
    "fast-mode-2026-02-01",
];

const BILLING_HASH_SALT: &str = "59cf53e54c78";
const BILLING_HASH_INDICES: [usize; 3] = [4, 7, 20];
pub const BILLING_HEADER_PREFIX: &str = "x-anthropic-billing-header:";
pub const CLAUDE_CODE_IDENTITY_TEXT: &str = "You are Claude Code, Anthropic's official CLI for Claude.";
const STATIC_CLAUDE_CODE_PROMPT: &str = include_str!("cc_static_prompt.txt");
const NON_CC_TOOL_NAMESPACE: &str = "mcp__client__";
const MAX_TOOL_NAME_LENGTH: usize = 64;
/// Bun reports Node's `process.version` as this for the pinned runtime.
const RUNTIME_VERSION: &str = "v24.3.0";

/// `[original, renamed, note?]`.
pub type RenamePair = (String, String, Option<String>);

const OAUTH_TOOL_RENAME_MAP: &[(&str, &str)] = &[
    ("bash", "Bash"), ("read", "Read"), ("write", "Write"), ("edit", "Edit"), ("glob", "Glob"), ("grep", "Grep"),
    ("task", "Task"), ("webfetch", "WebFetch"), ("todowrite", "TodoWrite"), ("question", "Question"), ("skill", "Skill"),
    ("ls", "LS"), ("todoread", "TodoRead"), ("notebookedit", "NotebookEdit"), ("askuserquestion", "AskUserQuestion"),
    ("enterplanmode", "EnterPlanMode"), ("exitplanmode", "ExitPlanMode"), ("killshell", "KillShell"),
    ("taskoutput", "TaskOutput"), ("websearch", "WebSearch"),
];

const CC_TOOL_REFERENCE: &[(&str, &[&str])] = &[
    ("Agent", &["description", "prompt"]),
    ("Bash", &["command"]),
    ("CronCreate", &["cron", "prompt"]),
    ("CronDelete", &["id"]),
    ("CronList", &[]),
    ("DesignSync", &["method"]),
    ("Edit", &["file_path", "old_string", "new_string"]),
    ("EnterWorktree", &[]),
    ("ExitWorktree", &["action"]),
    ("ListAgents", &[]),
    ("ListMcpResourcesTool", &[]),
    ("Monitor", &["description", "timeout_ms"]),
    ("NotebookEdit", &["notebook_path", "new_source"]),
    ("PushNotification", &["message", "status"]),
    ("Read", &["file_path"]),
    ("ReadMcpResourceDirTool", &["server", "uri"]),
    ("ReadMcpResourceTool", &["server", "uri"]),
    ("ReportFindings", &["findings"]),
    ("ScheduleWakeup", &[]),
    ("SendMessage", &["to", "message"]),
    ("Skill", &["skill"]),
    ("TaskStop", &[]),
    ("WaitForMcpServers", &[]),
    ("WebFetch", &["url", "prompt"]),
    ("WebSearch", &["query"]),
    ("Workflow", &[]),
    ("Write", &["file_path", "content"]),
];

fn cc_reference(name: &str) -> Option<&'static [&'static str]> {
    CC_TOOL_REFERENCE.iter().find(|(n, _)| *n == name).map(|(_, r)| *r)
}

fn synthetic_tools() -> Vec<Value> {
    vec![
        json!({"name": "Agent", "description": "Launch a subagent for complex tasks", "input_schema": {"type": "object", "properties": {"prompt": {"type": "string", "description": "Task description"}}, "required": ["prompt"]}}),
        json!({"name": "NotebookEdit", "description": "Edit notebook cells", "input_schema": {"type": "object", "properties": {"notebook_path": {"type": "string"}, "cell_index": {"type": "integer"}}, "required": ["notebook_path"]}}),
    ]
}

/// Per-process ids (`INSTANCE_SESSION_ID`, `DEVICE_ID`, `SESSION_ID`).
fn instance_session_id() -> &'static str {
    static ID: OnceLock<String> = OnceLock::new();
    ID.get_or_init(uuid)
}
fn device_id() -> &'static str {
    static ID: OnceLock<String> = OnceLock::new();
    ID.get_or_init(|| { use rand::RngCore; let mut b = [0u8; 32]; rand::thread_rng().fill_bytes(&mut b); hex::encode(b) })
}
fn metadata_session_id() -> &'static str {
    static ID: OnceLock<String> = OnceLock::new();
    ID.get_or_init(uuid)
}

/// `getStainlessHeaders()` (insertion order kept).
pub fn stainless_headers() -> Vec<(String, String)> {
    let arch = match std::env::consts::ARCH { "x86_64" => "x64", "aarch64" => "arm64", other => other };
    let os = match std::env::consts::OS { "macos" => "macOS", "windows" => "Windows", "linux" => "Linux", other => other };
    [
        ("user-agent", format!("claude-cli/{} (external, cli)", crate::cli_versions::claude_code_version())),
        ("x-app", "cli".into()),
        ("x-claude-code-session-id", instance_session_id().into()),
        ("x-stainless-arch", arch.into()),
        ("x-stainless-lang", "js".into()),
        ("x-stainless-os", os.into()),
        ("x-stainless-package-version", "0.112.1".into()),
        ("x-stainless-runtime", "node".into()),
        ("x-stainless-runtime-version", RUNTIME_VERSION.into()),
        ("x-stainless-retry-count", "0".into()),
        ("x-stainless-timeout", "600".into()),
        ("anthropic-dangerous-direct-browser-access", "true".into()),
    ].into_iter().map(|(k, v)| (k.to_owned(), v)).collect()
}

/// `canonicalizeOAuthToolName(name)`.
pub fn canonicalize_oauth_tool_name(name: &str) -> String {
    let lower = name.to_lowercase();
    OAUTH_TOOL_RENAME_MAP.iter().find(|(k, _)| *k == lower).map(|(_, v)| (*v).to_owned()).unwrap_or_else(|| name.to_owned())
}

fn should_remap(name: &str) -> bool {
    let lower = name.to_lowercase();
    OAUTH_TOOL_RENAME_MAP.iter().any(|(k, _)| *k == lower)
}

fn remap_name_field(obj: &mut Value, key: &str) {
    let Some(name) = obj.get(key).and_then(Value::as_str).filter(|n| !n.is_empty()) else { return };
    if should_remap(name) { let c = canonicalize_oauth_tool_name(name); obj[key] = json!(c); }
}

/// `remapOAuthToolNames(body)`.
fn remap_oauth_tool_names(result: &mut Value) {
    if let Some(tools) = result.get_mut("tools").and_then(Value::as_array_mut) {
        for tool in tools {
            let ty = tool.get("type");
            if truthy(ty) && ty.and_then(Value::as_str) != Some("function") { continue; }
            remap_name_field(tool, "name");
        }
    }
    if matches!(get_str(result, &["tool_choice", "type"]), Some("tool") | Some("function")) {
        if let Some(tc) = result.get_mut("tool_choice") { remap_name_field(tc, "name"); }
    }
    if let Some(messages) = result.get_mut("messages").and_then(Value::as_array_mut) {
        for msg in messages {
            let Some(content) = msg.get_mut("content").and_then(Value::as_array_mut) else { continue };
            for part in content {
                match get_str(part, &["type"]) {
                    Some("tool_use") => remap_name_field(part, "name"),
                    Some("tool_reference") => remap_name_field(part, "tool_name"),
                    Some("tool_result") => {
                        if let Some(nested) = part.get_mut("content").and_then(Value::as_array_mut) {
                            for n in nested { if get_str(n, &["type"]) == Some("tool_reference") { remap_name_field(n, "tool_name"); } }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

const SANITIZED_SYSTEM: &str = "Use the available tools when needed to help with software engineering tasks.\nKeep responses concise and focused on the user's request.\nPrefer acting on the user's task over describing product-specific workflows.";

fn reminder_block(text: &str) -> String {
    format!("<system-reminder>\nAs you answer the user's questions, you can use the following context from the system:\n{text}\n\nIMPORTANT: this context may or may not be relevant to your tasks. You should not respond to this context unless it is highly relevant to your task.\n</system-reminder>\n")
}

/// Both `prependToFirstUserMessage` variants (same observable result).
fn prepend_to_first_user_message(body: &mut Value, text: &str) {
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else { return };
    let Some(msg) = messages.iter_mut().find(|m| get_str(m, &["role"]) == Some("user")) else { return };
    let block = reminder_block(text);
    match msg.get_mut("content") {
        Some(Value::Array(items)) => items.insert(0, json!({"type": "text", "text": block})),
        Some(Value::String(s)) => *s = format!("{block}{s}"),
        _ => {}
    }
}

fn static_system_blocks(billing: String) -> Value {
    json!([
        {"type": "text", "text": billing},
        {"type": "text", "text": CLAUDE_CODE_IDENTITY_TEXT},
        {"type": "text", "text": STATIC_CLAUDE_CODE_PROMPT},
    ])
}

/// `injectClaudeCodeSystemPrompt(payload, {oauthMode: true})`.
fn inject_claude_code_system_prompt(result: &mut Value) {
    if get_str(result, &["system", "0", "text"]).is_some_and(|t| t.starts_with(BILLING_HEADER_PREFIX)) { return; }
    let mut parts: Vec<String> = Vec::new();
    match result.get("system") {
        Some(Value::Array(items)) => for part in items {
            if get_str(part, &["type"]) != Some("text") { continue; }
            if let Some(t) = get_str(part, &["text"]).map(crate::engine::dispatch::util::js_trim).filter(|t| !t.is_empty()) { parts.push(t.to_owned()); }
        },
        Some(Value::String(s)) => { let t = crate::engine::dispatch::util::js_trim(s); if !t.is_empty() { parts.push(t.to_owned()); } }
        _ => {}
    }
    // The billing text written here is always dropped by the masking step.
    if let Some(m) = result.as_object_mut() { m.insert("system".into(), static_system_blocks(format!("{BILLING_HEADER_PREFIX} cc_version=2.1.63.000; cc_entrypoint=cli; cch=00000;"))); }
    if !parts.is_empty() { prepend_to_first_user_message(result, SANITIZED_SYSTEM); }
}

/// `applyClaudeOAuthTransform(payload, apiKey, {oauthMode: true, ...})`.
pub fn apply_claude_oauth_transform(payload: Value, api_key: &str) -> Value {
    if !api_key.contains("sk-ant-oat") { return payload; }
    let model = get_str(&payload, &["model"]).map(str::to_owned);
    let mut result = clamp_anthropic_effort_and_thinking(payload, model.as_deref());
    remap_oauth_tool_names(&mut result);
    inject_claude_code_system_prompt(&mut result);
    result
}

// ─── masking pipeline ─────────────────────────────────────────────────────

/// `namespaceNonCcToolName(name)`: invalid UTF-16 code units → `_`.
fn namespace_non_cc_tool_name(name: &str) -> String {
    let mut sanitized = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' { sanitized.push(c); } else { for _ in 0..c.len_utf16() { sanitized.push('_'); } }
    }
    sanitized.truncate((MAX_TOOL_NAME_LENGTH - NON_CC_TOOL_NAMESPACE.len()).max(1));
    format!("{NON_CC_TOOL_NAMESPACE}{}", if sanitized.is_empty() { "tool" } else { &sanitized })
}

struct ToolDescriptor { name: Option<String>, parameters: Option<Value>, ty: Option<Value> }

fn sorted_unique(v: &[&str]) -> Vec<String> {
    let mut out: Vec<String> = v.iter().map(|s| (*s).to_owned()).collect::<HashSet<_>>().into_iter().collect();
    out.sort();
    out
}

/// `buildToolRenamePairs(tools)`: collision shape, then namespace shape.
fn build_tool_rename_pairs(tools: &[ToolDescriptor]) -> Vec<RenamePair> {
    let mut pairs = Vec::new();
    let mut claimed: HashSet<String> = HashSet::new();
    for tool in tools {
        let Some(name) = tool.name.as_deref() else { continue };
        let Some(reference) = cc_reference(name) else { continue };
        let required: Vec<&str> = tool.parameters.as_ref().and_then(|p| p.get("required")).and_then(Value::as_array)
            .map(|r| r.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
        if sorted_unique(reference) == sorted_unique(&required) { continue; }
        pairs.push((name.to_owned(), namespace_non_cc_tool_name(name), Some(format!("ALWAYS USE THIS TOOL INSTEAD OF {name}."))));
        claimed.insert(name.to_owned());
    }
    let remaining: Vec<&ToolDescriptor> = tools.iter().filter(|t| !t.name.as_ref().is_some_and(|n| claimed.contains(n))).collect();
    let mut taken: HashSet<String> = remaining.iter().filter_map(|t| t.name.clone()).collect();
    for tool in remaining {
        if let Some(ty) = tool.ty.as_ref().and_then(Value::as_str) { if !ty.is_empty() && ty != "custom" && ty != "function" { continue; } }
        let Some(name) = tool.name.as_deref() else { continue };
        if cc_reference(name).is_some() || name.starts_with("mcp__") { continue; }
        let mut renamed = namespace_non_cc_tool_name(name);
        if taken.contains(&renamed) {
            let mut suffix = 2;
            while taken.contains(&format!("{renamed}_{suffix}")) { suffix += 1; }
            renamed = format!("{renamed}_{suffix}");
        }
        taken.insert(renamed.clone());
        pairs.push((name.to_owned(), renamed, None));
    }
    pairs
}

fn rename_field(obj: &mut Value, key: &str, map: &HashMap<String, String>) -> bool {
    let Some(renamed) = obj.get(key).and_then(Value::as_str).and_then(|n| map.get(n)).cloned() else { return false };
    obj[key] = json!(renamed);
    true
}

/// `applyToolRenames(body, pairs)`.
fn apply_tool_renames(body: &mut Value, pairs: &[RenamePair]) {
    if pairs.is_empty() { return; }
    let mut map: HashMap<String, String> = HashMap::new();
    for (from, to, _) in pairs { map.insert(from.clone(), to.clone()); }
    if let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) {
        for t in tools { if t.is_object() { rename_field(t, "name", &map); } }
    }
    if get_str(body, &["tool_choice", "type"]) == Some("tool") {
        if let Some(tc) = body.get_mut("tool_choice") { rename_field(tc, "name", &map); }
    }
    if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
        for msg in messages {
            let Some(content) = msg.get_mut("content").and_then(Value::as_array_mut) else { continue };
            for block in content {
                match get_str(block, &["type"]) {
                    Some("tool_use") => { rename_field(block, "name", &map); }
                    Some("tool_reference") => { rename_field(block, "tool_name", &map); }
                    Some("tool_result") => {
                        if let Some(nested) = block.get_mut("content").and_then(Value::as_array_mut) {
                            for inner in nested { if get_str(inner, &["type"]) == Some("tool_reference") { rename_field(inner, "tool_name", &map); } }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

/// `stripDescriptionsAndInjectSyntheticTools(body, pairs)`.
fn inject_synthetic_tools(body: &mut Value, pairs: &[RenamePair]) {
    let Some(tools) = body.get("tools").and_then(Value::as_array) else { return };
    let notes: HashMap<&str, &str> = pairs.iter().filter_map(|(_, to, note)| note.as_deref().map(|n| (to.as_str(), n))).collect();
    let preserved: Vec<Value> = tools.iter().map(|t| {
        if get_str(t, &["type"]).is_some_and(|ty| ty != "custom") { return t.clone(); }
        let Some(note) = get_str(t, &["name"]).and_then(|n| notes.get(n)) else { return t.clone() };
        let original = get_str(t, &["description"]).unwrap_or("");
        let mut m = t.as_object().cloned().unwrap_or_default();
        m.insert("description".into(), json!(if original.is_empty() { (*note).to_owned() } else { format!("{original}\n\n{note}") }));
        Value::Object(m)
    }).collect();
    let mut all = synthetic_tools();
    all.extend(preserved);
    body["tools"] = Value::Array(all);
}

/// `dedupeSyntheticToolCollisions(body)`: keep the last occurrence.
fn dedupe_synthetic_tool_collisions(body: &mut Value) {
    let Some(tools) = body.get("tools").and_then(Value::as_array) else { return };
    let mut seen = HashSet::new();
    if !tools.iter().filter_map(|t| get_str(t, &["name"])).any(|n| !seen.insert(n)) { return; }
    let mut last: HashMap<&str, usize> = HashMap::new();
    for (i, t) in tools.iter().enumerate() { if let Some(n) = get_str(t, &["name"]) { last.insert(n, i); } }
    let kept: Vec<Value> = tools.iter().enumerate().filter(|(i, t)| get_str(t, &["name"]).is_none_or(|n| last.get(n) == Some(i))).map(|(_, t)| t.clone()).collect();
    body["tools"] = Value::Array(kept);
}

/// `extractFirstUserText(body)`.
fn first_user_text(body: &Value) -> String {
    let Some(messages) = body.get("messages").and_then(Value::as_array) else { return String::new() };
    let Some(first) = messages.iter().find(|m| get_str(m, &["role"]) == Some("user")) else { return String::new() };
    match first.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items.iter().find(|c| get_str(c, &["type"]) == Some("text")).and_then(|c| get_str(c, &["text"])).unwrap_or("").to_owned(),
        _ => String::new(),
    }
}

/// `buildBillingHeaderText(body)`: build hash over UTF-16 code units 4, 7, 20.
fn billing_header_text(body: &Value) -> String {
    let units: Vec<u16> = first_user_text(body).encode_utf16().collect();
    let chars: String = BILLING_HASH_INDICES.iter().map(|i| match units.get(*i) {
        Some(u) => char::from_u32(*u as u32).unwrap_or('\u{fffd}'),
        None => '0',
    }).collect();
    let version = crate::cli_versions::claude_code_version();
    let hash = sha256_hex(format!("{BILLING_HASH_SALT}{chars}{version}").as_bytes());
    format!("{BILLING_HEADER_PREFIX} cc_version={version}.{}; cc_entrypoint=cli; cch=00000;", &hash[..3])
}

/// `injectClaudeCodeIdentity(body)`.
fn inject_claude_code_identity(body: &mut Value) {
    let mut parts: Vec<String> = Vec::new();
    if let Some(items) = body.get("system").and_then(Value::as_array) {
        for part in items {
            if get_str(part, &["type"]) != Some("text") { continue; }
            let txt = get_str(part, &["text"]).map(crate::engine::dispatch::util::js_trim).unwrap_or("");
            if txt.is_empty() || txt.starts_with(BILLING_HEADER_PREFIX) || txt == CLAUDE_CODE_IDENTITY_TEXT { continue; }
            parts.push(txt.to_owned());
        }
    }
    let billing = billing_header_text(body);
    if let Some(m) = body.as_object_mut() { m.insert("system".into(), static_system_blocks(billing)); }
    if !parts.is_empty() { prepend_to_first_user_message(body, SANITIZED_SYSTEM); }
}

/// `injectClaudeCodeMetadata(body)`.
fn inject_claude_code_metadata(body: &mut Value) {
    let user_id = stringify(&json!({"device_id": device_id(), "session_id": metadata_session_id()}));
    let mut meta: Map<String, Value> = body.get("metadata").and_then(Value::as_object).cloned().unwrap_or_default();
    meta.insert("user_id".into(), json!(user_id));
    if let Some(m) = body.as_object_mut() { m.insert("metadata".into(), Value::Object(meta)); }
}

/// `signBillingHeader(body)`.
fn sign_billing_header(body: &mut Value) {
    let Some(text) = get_str(body, &["system", "0", "text"]) else { return };
    if !text.starts_with(BILLING_HEADER_PREFIX) || !text.contains("cch=00000;") { return; }
    let cch = sha256_hex(stringify(body).as_bytes())[..5].to_owned();
    let signed = text.replacen("cch=00000;", &format!("cch={cch};"), 1);
    // `body.system[0].text = signed`: `get_str` above also matches a `"0"`
    // key on an object `system`, so mirror JS property access rather than
    // indexing (which would panic on a non-array).
    let first = match body.get_mut("system") {
        Some(Value::Array(items)) => items.get_mut(0),
        Some(Value::Object(map)) => map.get_mut("0"),
        _ => None,
    };
    if let Some(Value::Object(first)) = first { first.insert("text".into(), json!(signed)); }
}

/// `applyClaudeCodeMasking(JSON.stringify(payload))` → `(payload, toolRenamePairs)`.
pub fn apply_claude_code_masking(payload: &Value) -> (Value, Vec<RenamePair>) {
    let mut body = parse(&stringify(payload)).unwrap_or(Value::Null);
    let descriptors: Vec<ToolDescriptor> = body.get("tools").and_then(Value::as_array).map(|tools| tools.iter().map(|t| ToolDescriptor {
        name: get_str(t, &["name"]).map(str::to_owned),
        parameters: t.get("input_schema").cloned(),
        ty: t.get("type").cloned(),
    }).collect()).unwrap_or_default();
    let pairs = build_tool_rename_pairs(&descriptors);
    apply_tool_renames(&mut body, &pairs);
    inject_synthetic_tools(&mut body, &pairs);
    dedupe_synthetic_tool_collisions(&mut body);
    inject_claude_code_identity(&mut body);
    inject_claude_code_metadata(&mut body);
    sign_billing_header(&mut body);
    (body, pairs)
}

/// `reverseToolRenames(text, pairs)` over `[original, wireName]` pairs.
pub fn reverse_tool_renames(text: &str, pairs: &[(String, String)]) -> String {
    let mut out = text.to_owned();
    for (orig, renamed) in pairs {
        for key in ["name", "tool_name"] {
            out = out.replace(&format!("\"{key}\":\"{renamed}\""), &format!("\"{key}\":\"{orig}\""));
            out = out.replace(&format!("\\\"{key}\\\":\\\"{renamed}\\\""), &format!("\\\"{key}\\\":\\\"{orig}\\\""));
        }
    }
    out
}

/// `buildFrameReverser(callerToolNames, maskingPairs)` as the reverse pairs.
pub fn frame_reverse_pairs(caller_tool_names: &[String], masking_pairs: &[RenamePair]) -> Vec<(String, String)> {
    let mask: HashMap<&str, &str> = masking_pairs.iter().map(|(f, t, _)| (f.as_str(), t.as_str())).collect();
    caller_tool_names.iter().filter_map(|original| {
        let canonical = canonicalize_oauth_tool_name(original);
        let wire = mask.get(canonical.as_str()).map(|s| (*s).to_owned()).unwrap_or(canonical);
        (wire != *original).then(|| (original.clone(), wire))
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespace_and_collision_shapes() {
        let tools = vec![
            ToolDescriptor { name: Some("Bash".into()), parameters: Some(json!({"required": ["command"]})), ty: None },
            ToolDescriptor { name: Some("Read".into()), parameters: Some(json!({"required": ["path"]})), ty: None },
            ToolDescriptor { name: Some("web search!".into()), parameters: None, ty: None },
            ToolDescriptor { name: Some("mcp__x__y".into()), parameters: None, ty: None },
            ToolDescriptor { name: Some("web_search".into()), parameters: None, ty: Some(json!("web_search_20250305")) },
        ];
        let pairs = build_tool_rename_pairs(&tools);
        assert_eq!(pairs, vec![
            ("Read".into(), "mcp__client__Read".into(), Some("ALWAYS USE THIS TOOL INSTEAD OF Read.".into())),
            ("web search!".into(), "mcp__client__web_search_".into(), None),
        ]);
        assert_eq!(namespace_non_cc_tool_name("é😀"), "mcp__client_____");
    }

    #[test]
    fn reverse_renames_frames() {
        let pairs = frame_reverse_pairs(&["bash".into(), "read".into(), "custom".into()], &[("Read".into(), "mcp__client__Read".into(), None), ("custom".into(), "mcp__client__custom".into(), None)]);
        assert_eq!(pairs, vec![("bash".into(), "Bash".into()), ("read".into(), "mcp__client__Read".into()), ("custom".into(), "mcp__client__custom".into())]);
        let frame = r#"data: {"content_block":{"type":"tool_use","name":"mcp__client__Read"},"x":"{\"name\":\"Bash\"}"}"#;
        assert_eq!(reverse_tool_renames(frame, &pairs), r#"data: {"content_block":{"type":"tool_use","name":"read"},"x":"{\"name\":\"bash\"}"}"#);
    }
}
