//! `services/dispatch/empty-completion.ts`: visible-output detection for
//! unified responses / stream chunks (JSON values).
use crate::engine::js::*;
use serde_json::Value;

pub const EMPTY_COMPLETION_REASON: &str = "Empty completion (no visible output)";

/// `CompletionVisibilitySignals` / `StreamVisibilityTracker`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VisibilitySignals {
    pub has_text: bool,
    pub has_reasoning: bool,
    pub tool_call_count: usize,
    pub annotation_count: usize,
    pub image_count: usize,
    pub client_tool_call_count: usize,
}

/// `isEmptyCompletion` / `isStreamEmpty`.
pub fn is_empty_completion(s: &VisibilitySignals) -> bool {
    !(s.has_text || s.has_reasoning || s.tool_call_count > 0 || s.annotation_count > 0 || s.image_count > 0 || s.client_tool_call_count > 0)
}

fn has_visible_text(v: Option<&Value>) -> bool { v.and_then(Value::as_str).is_some_and(|s| !s.trim_matches(super::util::is_js_ws).is_empty()) }

/// `x?.length ?? 0` for a JS array-or-string-ish field (arrays only here).
fn len_of(v: Option<&Value>) -> usize {
    match v {
        Some(Value::Array(a)) => a.len(),
        Some(Value::String(s)) => s.encode_utf16().count(),
        _ => 0,
    }
}

fn arr_len(v: Option<&Value>) -> usize { v.and_then(Value::as_array).map_or(0, Vec::len) }

/// `getResponseVisibilitySignals(response)`.
pub fn response_visibility_signals(response: &Value) -> VisibilitySignals {
    let typed_images = get_arr(response, &["image_generation_calls"])
        .map_or(0, |calls| calls.iter().filter(|c| get_str(c, &["result"]).is_some_and(|r| !r.is_empty())).count());
    let raw_images = get_arr(response, &["rawResponse", "output"])
        .map_or(0, |items| items.iter().filter(|i| get_str(i, &["type"]) == Some("image_generation_call")).count());
    VisibilitySignals {
        has_text: has_visible_text(response.get("content")),
        has_reasoning: has_visible_text(response.get("reasoning_content")) || has_visible_text(get(response, &["thinking", "content"])),
        tool_call_count: len_of(response.get("tool_calls")),
        annotation_count: len_of(response.get("annotations")),
        image_count: typed_images + arr_len(response.get("images")) + raw_images,
        client_tool_call_count: arr_len(response.get("client_tool_calls")),
    }
}

/// `isEmptyUnifiedResponse`: empty only with a normal finish reason
/// (absent/null/`stop`/`end_turn`).
pub fn is_empty_unified_response(response: &Value) -> bool {
    let normal = match response.get("finishReason") {
        None | Some(Value::Null) => true,
        Some(Value::String(s)) => s == "stop" || s == "end_turn",
        _ => false,
    };
    normal && is_empty_completion(&response_visibility_signals(response))
}

/// `observeStreamChunk(tracker, chunk)`.
pub fn observe_stream_chunk(tracker: &mut VisibilitySignals, chunk: &Value) {
    let Some(delta) = chunk.get("delta").filter(|d| truthy(Some(d))) else { return };
    if has_visible_text(delta.get("content")) { tracker.has_text = true; }
    if has_visible_text(delta.get("reasoning_content")) || has_visible_text(get(delta, &["thinking", "content"])) {
        tracker.has_reasoning = true;
    }
    tracker.tool_call_count += arr_len(delta.get("tool_calls"));
    tracker.annotation_count += arr_len(chunk.get("annotations"));
    tracker.image_count += arr_len(chunk.get("images"));
    tracker.client_tool_call_count += arr_len(chunk.get("client_tool_calls"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn empty_detection() {
        assert!(is_empty_unified_response(&json!({"content": "  ", "finishReason": "stop"})));
        assert!(!is_empty_unified_response(&json!({"content": "", "finishReason": "length"})));
        assert!(!is_empty_unified_response(&json!({"content": null, "tool_calls": [{}]})));
        assert!(!is_empty_unified_response(&json!({"thinking": {"content": "hm"}})));
        assert!(!is_empty_unified_response(&json!({"image_generation_calls": [{"result": "b64"}]})));
        assert!(is_empty_unified_response(&json!({"image_generation_calls": [{"result": ""}]})));
        assert!(!is_empty_unified_response(&json!({"rawResponse": {"output": [{"type": "image_generation_call"}]}})));
        assert!(!is_empty_unified_response(&json!({"client_tool_calls": [{"type": "tool_search_call"}]})));
        assert!(!is_empty_unified_response(&json!({"annotations": [{"url": "x"}], "finishReason": "end_turn"})));
    }
    #[test]
    fn stream_tracker() {
        let mut t = VisibilitySignals::default();
        observe_stream_chunk(&mut t, &json!({"delta": {"content": " "}}));
        assert!(is_empty_completion(&t));
        observe_stream_chunk(&mut t, &json!({"delta": {}, "client_tool_calls": [{}]}));
        assert!(!is_empty_completion(&t));
        let mut t = VisibilitySignals::default();
        observe_stream_chunk(&mut t, &json!({"client_tool_calls": [{}]}));
        assert!(is_empty_completion(&t));
    }
}
