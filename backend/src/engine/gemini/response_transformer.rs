//! `transformers/gemini/response-transformer.ts`: provider response → unified.
use super::malformed::detect_gemini_malformed_function_call;
use super::utils::*;
use crate::engine::js::*;
use crate::engine::transformer::EngineResult;
use crate::engine::usage::normalize_gemini;
use serde_json::{json, Value};

/// `transformGeminiResponse`.
pub fn transform_gemini_response(response: &Value) -> EngineResult<Value> {
    let candidate = read(Some(response), "candidates")?.and_then(|c| get(c, &["0"]));
    let parts = or(candidate.and_then(|c| get(c, &["content", "parts"])), None)
        .unwrap_or_else(|| json!([]));
    let malformed = detect_gemini_malformed_function_call(response);

    let Value::Array(parts) = parts else {
        return Err(type_error("parts.forEach is not a function"));
    };
    let mut content = String::new();
    let mut reasoning_content = String::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    let mut thought_signature: Option<Value> = None;

    for part in &parts {
        if part.is_null() {
            return Err(read_error(Some(part), "text"));
        }
        if truthy(part.get("text")) {
            let text = to_string(part.get("text"));
            if part.get("thought") == Some(&Value::Bool(true)) {
                reasoning_content.push_str(&text)
            } else {
                content.push_str(&text)
            }
        }
        if truthy(part.get("functionCall")) {
            let fc = &part["functionCall"];
            let id = if truthy(fc.get("name")) {
                fc["name"].clone()
            } else {
                json!(random_call_id())
            };
            tool_calls.push(json!({"id": id, "type": "function", "function": obj(vec![
                ("name", fc.get("name").cloned()),
                ("arguments", stringify_opt(fc.get("args"))),
            ])}));
        }
        // Gap 6: only base64-valid signatures are kept.
        if truthy(part.get("thoughtSignature"))
            && is_valid_thought_signature_value(part.get("thoughtSignature"))
        {
            thought_signature = part.get("thoughtSignature").cloned();
        }
    }

    let usage = response
        .get("usageMetadata")
        .filter(|u| truthy(Some(u)))
        .map(normalize_gemini);
    let raw_finish = match candidate
        .and_then(|c| c.get("finishReason"))
        .filter(|f| !f.is_null())
    {
        Some(f) => Some(lower(f, "candidate.finishReason")?).filter(|s| !s.is_empty()),
        None => None,
    };
    let finish = if !tool_calls.is_empty() && raw_finish.as_deref() == Some("stop") {
        Some("tool_calls".to_owned())
    } else {
        raw_finish
    };

    let id = if truthy(response.get("responseId")) {
        response["responseId"].clone()
    } else {
        json!(format!("gemini-{}", now_ms()))
    };
    let model =
        or(response.get("modelVersion"), Some(&json!("gemini-model"))).unwrap_or(Value::Null);
    let opt_str = |s: String| {
        if s.is_empty() {
            Value::Null
        } else {
            Value::String(s)
        }
    };
    Ok(obj(vec![
        ("id", Some(id)),
        ("model", Some(model)),
        ("content", Some(opt_str(content))),
        (
            "reasoning_content",
            Some(opt_str(reasoning_content.clone())),
        ),
        (
            "thinking",
            thought_signature.map(|sig| json!({"content": reasoning_content, "signature": sig})),
        ),
        (
            "tool_calls",
            if tool_calls.is_empty() {
                None
            } else {
                Some(Value::Array(tool_calls))
            },
        ),
        (
            "finishReason",
            Some(finish.map(Value::String).unwrap_or(Value::Null)),
        ),
        ("usage", usage),
        ("clientError", malformed.map(|m| m.client_error())),
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::gemini::malformed::GEMINI_MALFORMED_FUNCTION_CALL_MESSAGE;

    #[test]
    fn text_reasoning_tools_and_usage() {
        let r = transform_gemini_response(&json!({
            "candidates": [{"content": {"parts": [
                {"text": "think ", "thought": true},
                {"text": "hello"},
                {"functionCall": {"name": "get_weather", "args": {"city": "SF"}}, "thoughtSignature": "c2ln"},
                {"thoughtSignature": "bad!"}
            ]}, "finishReason": "STOP"}],
            "usageMetadata": {"promptTokenCount": 100, "candidatesTokenCount": 50, "totalTokenCount": 150, "cachedContentTokenCount": 25, "thoughtsTokenCount": 5},
            "responseId": "r1", "modelVersion": "gemini-2.5"
        })).unwrap();
        assert_eq!(
            r,
            json!({
                "id": "r1", "model": "gemini-2.5", "content": "hello", "reasoning_content": "think ",
                "thinking": {"content": "think ", "signature": "c2ln"},
                "tool_calls": [{"id": "get_weather", "type": "function", "function": {"name": "get_weather", "arguments": "{\"city\":\"SF\"}"}}],
                "finishReason": "tool_calls",
                "usage": {"input_tokens": 75, "output_tokens": 50, "total_tokens": 150, "reasoning_tokens": 5, "cached_tokens": 25, "cache_creation_tokens": 0}
            })
        );
    }

    #[test]
    fn empty_and_malformed() {
        let r = transform_gemini_response(&json!({})).unwrap();
        assert_eq!(r["content"], Value::Null);
        assert_eq!(r["finishReason"], Value::Null);
        assert_eq!(r["model"], "gemini-model");
        assert!(r["id"].as_str().unwrap().starts_with("gemini-"));
        let r = transform_gemini_response(&json!({"candidates": [{"content": {"parts": [{"text": "call:default_api:bash{}"}]}, "finishReason": "MALFORMED_FUNCTION_CALL"}]})).unwrap();
        assert_eq!(r["finishReason"], "malformed_function_call");
        assert_eq!(
            r["clientError"],
            json!({"statusCode": 503, "code": "MALFORMED_FUNCTION_CALL", "message": GEMINI_MALFORMED_FUNCTION_CALL_MESSAGE})
        );
    }
}
