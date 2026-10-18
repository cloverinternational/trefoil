//! `transformers/gemini/response-formatter.ts`: unified response → client Gemini.
use super::utils::*;
use crate::engine::image_rendering::compose_content;
use crate::engine::js::*;
use crate::engine::transformer::EngineResult;
use serde_json::{json, Map, Value};

/// Unified usage → Gemini `usageMetadata` (non-stream shape).
fn usage_metadata(usage: &Value) -> Value {
    obj(vec![
        (
            "promptTokenCount",
            Some(js_plus(
                usage.get("input_tokens"),
                Some(&or(usage.get("cached_tokens"), Some(&json!(0))).unwrap_or(Value::Null)),
            )),
        ),
        ("candidatesTokenCount", usage.get("output_tokens").cloned()),
        ("totalTokenCount", usage.get("total_tokens").cloned()),
        ("thoughtsTokenCount", usage.get("reasoning_tokens").cloned()),
        (
            "cachedContentTokenCount",
            usage.get("cached_tokens").cloned(),
        ),
    ])
}

/// `formatGeminiResponse`.
pub fn format_gemini_response(response: &Value) -> EngineResult<Value> {
    let mut parts: Vec<Value> = Vec::new();
    let thinking_sig = get(response, &["thinking", "signature"]).filter(|s| truthy(Some(s)));

    if truthy(response.get("reasoning_content")) {
        let mut part = json!({"text": response["reasoning_content"], "thought": true});
        if let Some(sig) = thinking_sig {
            part["thoughtSignature"] = sig.clone();
        }
        parts.push(part);
    }

    let text_content = compose_content(
        response.get("content"),
        response.get("image_generation_calls"),
    );
    if let Some(text) = text_content.filter(|t| truthy(Some(t))) {
        parts.push(json!({"text": text}));
    }

    if truthy(response.get("tool_calls")) {
        let Some(tcs) = get_arr(response, &["tool_calls"]) else {
            return Err(type_error("response.tool_calls.forEach is not a function"));
        };
        for (index, tc) in tcs.iter().enumerate() {
            let f = read(Some(tc), "function")?;
            let name = read(f, "name")?.cloned();
            let args = json_parse_js(read(f, "arguments")?)?;
            let mut part = Map::new();
            part.insert(
                "functionCall".into(),
                obj(vec![("name", name), ("args", Some(args))]),
            );
            let sig = or(
                get(tc, &["thinking", "signature"]),
                tc.get("thought_signature"),
            )
            .filter(|s| truthy(Some(s)));
            if let Some(sig) = sig {
                part.insert("thoughtSignature".into(), sig);
            } else if index == 0 && !truthy(response.get("reasoning_content")) {
                if let Some(s) = thinking_sig {
                    part.insert("thoughtSignature".into(), s.clone());
                }
            }
            parts.push(Value::Object(part));
        }
    }

    // Gemini always uses STOP (TOOL_USE is not a valid Gemini finish reason).
    let mut result = obj(vec![
        (
            "candidates",
            Some(
                json!([{"content": {"role": "model", "parts": parts}, "finishReason": "STOP", "index": 0}]),
            ),
        ),
        (
            "usageMetadata",
            response
                .get("usage")
                .filter(|u| truthy(Some(u)))
                .map(usage_metadata),
        ),
        ("modelVersion", response.get("model").cloned()),
    ]);
    if truthy(response.get("id")) {
        result["responseId"] = response["id"].clone();
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_parts_signatures_and_usage() {
        let r = format_gemini_response(&json!({
            "id": "r1", "model": "gemini-2.5", "content": "hello", "reasoning_content": "hmm",
            "thinking": {"content": "hmm", "signature": "c2ln"},
            "tool_calls": [
                {"id": "a", "type": "function", "function": {"name": "a", "arguments": "{\"x\":1}"}},
                {"id": "b", "type": "function", "function": {"name": "b", "arguments": "{}"}, "thought_signature": "b3du"}
            ],
            "usage": {"input_tokens": 100, "output_tokens": 50, "total_tokens": 170, "reasoning_tokens": 0, "cached_tokens": 20}
        })).unwrap();
        assert_eq!(
            r,
            json!({
                "candidates": [{"content": {"role": "model", "parts": [
                    {"text": "hmm", "thought": true, "thoughtSignature": "c2ln"},
                    {"text": "hello"},
                    {"functionCall": {"name": "a", "args": {"x": 1}}},
                    {"functionCall": {"name": "b", "args": {}}, "thoughtSignature": "b3du"}
                ]}, "finishReason": "STOP", "index": 0}],
                "usageMetadata": {"promptTokenCount": 120, "candidatesTokenCount": 50, "totalTokenCount": 170, "thoughtsTokenCount": 0, "cachedContentTokenCount": 20},
                "modelVersion": "gemini-2.5",
                "responseId": "r1"
            })
        );
    }

    #[test]
    fn signature_falls_back_to_first_tool_call_without_reasoning() {
        let r = format_gemini_response(&json!({"content": null, "thinking": {"signature": "c2ln"}, "tool_calls": [
            {"function": {"name": "a", "arguments": "{}"}}, {"function": {"name": "b", "arguments": "{}"}}
        ]})).unwrap();
        let parts = &r["candidates"][0]["content"]["parts"];
        assert_eq!(parts[0]["thoughtSignature"], "c2ln");
        assert!(parts[1].get("thoughtSignature").is_none());
        assert!(r.get("responseId").is_none() && r.get("usageMetadata").is_none());
    }
}
