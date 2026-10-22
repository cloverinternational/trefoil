//! `transformers/gemini` (and the `transformers/gemini.ts` re-export): the
//! Gemini `generateContent` protocol. Submodules mirror the TS files:
//! - `request_parser`: client Gemini → unified
//! - `request_builder`: unified → provider Gemini
//! - `response_transformer`: provider → unified
//! - `response_formatter`: unified → client Gemini
//! - `stream_transformer` / `stream_formatter`: the streaming counterparts
//! - `part_mapper`, `utils` (`utils/*`), `malformed` (`utils/gemini-malformed-function-call.ts`)
pub mod malformed;
pub mod part_mapper;
pub mod request_builder;
pub mod request_parser;
pub mod response_formatter;
pub mod response_transformer;
pub mod stream_formatter;
pub mod stream_transformer;
pub mod utils;

pub use malformed::{
    detect_gemini_malformed_function_call, GeminiMalformedFunctionCall,
    MalformedFunctionCallRewriter, GEMINI_MALFORMED_FUNCTION_CALL_CODE,
    GEMINI_MALFORMED_FUNCTION_CALL_MESSAGE,
};
pub use part_mapper::{convert_gemini_parts_to_unified, convert_unified_parts_to_gemini};
pub use request_builder::{build_gemini_request, gemini_thinking_level};
pub use request_parser::parse_gemini_request;
pub use response_formatter::format_gemini_response;
pub use response_transformer::transform_gemini_response;
pub use stream_formatter::GeminiStreamFormatter;
pub use stream_transformer::GeminiStreamTransformer;
pub use utils::{
    append_user_after_text_only_model_tail, ensure_contents_end_with_user,
    is_text_only_model_content, is_valid_thought_signature, normalize_json_schema_types,
    sanitize_thought_signature,
};

use super::js::*;
use super::transformer::*;
use super::usage::normalize_gemini;
use serde_json::{json, Value};

pub struct GeminiTransformer;

/// `getEndpoint`: `/v1beta/{models/|tunedModels/}<model>:generateContent`
/// or `:streamGenerateContent?alt=sse` when streaming.
pub fn gemini_endpoint(request: &Value) -> String {
    let action = if truthy(request.get("stream")) {
        "streamGenerateContent?alt=sse"
    } else {
        "generateContent"
    };
    // TS throws on a non-string model; `String(model)` is the closest total form.
    let model = to_string(request.get("model"));
    let model = if model.starts_with("models/") || model.starts_with("tunedModels/") {
        model
    } else {
        format!("models/{model}")
    };
    format!("/v1beta/{model}:{action}")
}

impl Transformer for GeminiTransformer {
    fn name(&self) -> &'static str {
        "gemini"
    }
    fn default_endpoint(&self) -> &'static str {
        "/v1beta/models/:modelAndAction"
    }
    fn endpoint(&self, request: &Value) -> String {
        gemini_endpoint(request)
    }

    fn parse_request(&self, input: &Value) -> EngineResult<Value> {
        parse_gemini_request(input)
    }
    fn transform_request(&self, request: &Value) -> EngineResult<Value> {
        build_gemini_request(request)
    }
    fn transform_response(&self, response: &Value) -> EngineResult<Value> {
        transform_gemini_response(response)
    }
    fn format_response(&self, response: &Value) -> EngineResult<Value> {
        format_gemini_response(response)
    }

    fn stream_transformer(&self) -> Option<Box<dyn StreamTransformer>> {
        Some(Box::new(GeminiStreamTransformer::new()))
    }
    fn stream_formatter(&self) -> Option<Box<dyn StreamFormatter>> {
        Some(Box::new(GeminiStreamFormatter::new()))
    }

    /// `extractUsage`: usage from `usageMetadata` of one event's data.
    fn extract_usage(&self, data: &str) -> Option<Value> {
        let data = parse(data)?;
        let um = data.get("usageMetadata").filter(|u| truthy(Some(u)))?;
        let u = normalize_gemini(um);
        Some(
            json!({"input_tokens": u["input_tokens"], "output_tokens": u["output_tokens"], "cached_tokens": u["cached_tokens"], "reasoning_tokens": u["reasoning_tokens"]}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::openai::OpenAIStreamFormatter;
    use crate::engine::sse::{SseEvent, SseParser};

    #[test]
    fn endpoint() {
        let t = GeminiTransformer;
        assert_eq!(t.name(), "gemini");
        assert_eq!(
            t.endpoint(&json!({"model": "gemini-2.5-flash"})),
            "/v1beta/models/gemini-2.5-flash:generateContent"
        );
        assert_eq!(
            t.endpoint(&json!({"model": "models/gemini-2.5-flash", "stream": true})),
            "/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse"
        );
        assert_eq!(
            t.endpoint(&json!({"model": "tunedModels/x", "stream": false})),
            "/v1beta/tunedModels/x:generateContent"
        );
    }

    #[test]
    fn extract_usage() {
        let t = GeminiTransformer;
        let u = t.extract_usage(r#"{"candidates":[],"usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":1405,"totalTokenCount":2201,"thoughtsTokenCount":789,"cachedContentTokenCount":0}}"#).unwrap();
        assert_eq!(
            u,
            json!({"input_tokens": 7, "output_tokens": 1405, "cached_tokens": 0, "reasoning_tokens": 789})
        );
        let u = t.extract_usage(r#"{"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":20,"totalTokenCount":30}}"#).unwrap();
        assert_eq!(
            u,
            json!({"input_tokens": 10, "output_tokens": 20, "cached_tokens": 0, "reasoning_tokens": 0})
        );
        let u = t.extract_usage(r#"{"usageMetadata":{"promptTokenCount":100,"candidatesTokenCount":50,"totalTokenCount":150,"cachedContentTokenCount":25}}"#).unwrap();
        assert_eq!(
            u,
            json!({"input_tokens": 75, "output_tokens": 50, "cached_tokens": 25, "reasoning_tokens": 0})
        );
        let u = t.extract_usage(r#"{"usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":336,"totalTokenCount":1027,"thoughtsTokenCount":684,"cachedContentTokenCount":50}}"#).unwrap();
        assert_eq!(
            u,
            json!({"input_tokens": 7, "output_tokens": 336, "cached_tokens": 50, "reasoning_tokens": 684})
        );
        assert!(t.extract_usage(r#"{"candidates":[]}"#).is_none());
        assert!(t.extract_usage("not valid json").is_none());
    }

    /// `__tests__/gemini-openai-stream-regression.test.ts`: Gemini stream →
    /// unified → OpenAI chat chunks.
    fn gemini_to_openai(events: &[String]) -> Vec<Value> {
        let mut st = GeminiStreamTransformer::new();
        let mut fmt = OpenAIStreamFormatter::default();
        let mut wire = String::new();
        for e in events {
            for chunk in st.on_event(&SseEvent {
                event: None,
                data: e.clone(),
                id: None,
            }) {
                for s in fmt.on_chunk(&chunk) {
                    wire.push_str(&s);
                }
            }
        }
        for chunk in st.finish() {
            for s in fmt.on_chunk(&chunk) {
                wire.push_str(&s);
            }
        }
        for s in fmt.finish() {
            wire.push_str(&s);
        }
        SseParser::new()
            .feed(&wire)
            .into_iter()
            .map(|e| {
                if e.data == "[DONE]" {
                    json!("[DONE]")
                } else {
                    parse(&e.data).unwrap()
                }
            })
            .collect()
    }

    fn tool_call_chunk(chunks: &[Value]) -> Option<&Value> {
        chunks.iter().find(|c| {
            get_arr(c, &["choices", "0", "delta", "tool_calls"]).is_some_and(|t| !t.is_empty())
        })
    }

    #[test]
    fn openai_regression_indexed_tool_calls() {
        let chunks = gemini_to_openai(&[
            stringify(
                &json!({"candidates": [{"content": {"role": "model", "parts": [{"functionCall": {"name": "search_web", "args": {"query": "today's top news headlines"}}}]}, "index": 0}], "responseId": "resp_123", "modelVersion": "gemini-3.1-flash-lite-preview"}),
            ),
            stringify(
                &json!({"candidates": [{"content": {"role": "model", "parts": [{"text": ""}]}, "finishReason": "STOP", "index": 0}], "responseId": "resp_123", "modelVersion": "gemini-3.1-flash-lite-preview"}),
            ),
            "[DONE]".into(),
        ]);
        let tc = tool_call_chunk(&chunks).unwrap();
        assert_eq!(
            tc["choices"][0]["delta"]["tool_calls"][0],
            json!({"index": 0, "id": "call_1", "type": "function", "function": {"name": "search_web", "arguments": "{\"query\":\"today's top news headlines\"}"}})
        );
        assert!(chunks
            .iter()
            .any(|c| c["choices"][0]["finish_reason"] == "tool_calls"));
        assert_eq!(chunks.last().unwrap(), &json!("[DONE]"));
    }

    #[test]
    fn openai_regression_malformed_function_call() {
        let chunks = gemini_to_openai(&[
            stringify(
                &json!({"candidates": [{"content": {"role": "model", "parts": [{"text": "Formattingcall:default_api:bash{command:bun run format}"}]}, "finishReason": "MALFORMED_FUNCTION_CALL", "index": 0}], "responseId": "resp_malformed", "modelVersion": "gemini-3.6-flash"}),
            ),
            "[DONE]".into(),
        ]);
        let err = chunks.iter().find(|c| c.get("error").is_some()).unwrap();
        assert_eq!(err["error"]["type"], "server_error");
        assert_eq!(err["error"]["code"], "upstream_malformed_function_call");
        assert!(err["error"]["message"]
            .as_str()
            .unwrap()
            .contains("please retry your request"));
        assert!(!chunks.contains(&json!("[DONE]")));
    }

    #[test]
    fn openai_regression_preserves_upstream_ids() {
        let chunks = gemini_to_openai(&[stringify(
            &json!({"candidates": [{"content": {"role": "model", "parts": [{"functionCall": {"id": "zq87ju01", "name": "search_web", "args": {"query": "top news headlines today"}}}]}, "finishReason": "STOP", "index": 0}], "responseId": "resp_456", "modelVersion": "gemini-3.1-flash-lite-preview"}),
        )]);
        let tc = tool_call_chunk(&chunks).unwrap();
        assert_eq!(tc["choices"][0]["delta"]["tool_calls"][0]["index"], 0);
        assert_eq!(tc["choices"][0]["delta"]["tool_calls"][0]["id"], "zq87ju01");
        assert!(chunks
            .iter()
            .any(|c| c["choices"][0]["finish_reason"] == "tool_calls"));
    }

    /// Key order is part of the wire format (serde_json `preserve_order`):
    /// these strings are what the TS `JSON.stringify` produces.
    #[test]
    fn wire_key_order_matches_ts() {
        let t = GeminiTransformer;
        let original = json!({"model": "g", "labels": {"b": 1, "a": 2}, "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
            "generationConfig": {"topK": 4, "maxOutputTokens": 9, "temperature": 0.1}, "toolConfig": {"functionCallingConfig": {"mode": "AUTO"}}});
        let mut unified = t.parse_request(&original).unwrap();
        assert_eq!(
            stringify(&unified),
            r#"{"messages":[{"role":"user","content":"hi"}],"model":"g","max_tokens":9,"temperature":0.1,"stream":false,"toolConfig":{"mode":"AUTO"}}"#
        );
        unified["incomingApiType"] = json!("gemini");
        unified["originalBody"] = original;
        unified.as_object_mut().unwrap().remove("temperature");
        assert_eq!(
            stringify(&t.transform_request(&unified).unwrap()),
            r#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}],"toolConfig":{"functionCallingConfig":{"mode":"AUTO"}},"generationConfig":{"topK":4,"maxOutputTokens":9},"labels":{"b":1,"a":2}}"#
        );
        let resp = t.transform_response(&json!({"responseId": "r", "modelVersion": "m", "candidates": [{"finishReason": "STOP", "content": {"parts": [{"functionCall": {"name": "f", "args": {"z": 1, "a": 2}}}]}}]})).unwrap();
        assert_eq!(
            stringify(&resp),
            r#"{"id":"r","model":"m","content":null,"reasoning_content":null,"tool_calls":[{"id":"f","type":"function","function":{"name":"f","arguments":"{\"z\":1,\"a\":2}"}}],"finishReason":"tool_calls"}"#
        );
        assert_eq!(
            stringify(&t.format_response(&resp).unwrap()),
            r#"{"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"f","args":{"z":1,"a":2}}}]},"finishReason":"STOP","index":0}],"modelVersion":"m","responseId":"r"}"#
        );
        let mut fmt = GeminiStreamFormatter::new();
        let wire = fmt.on_chunk(&json!({"id": "r", "model": "m", "delta": {"content": "x"}, "finish_reason": "stop", "usage": {"input_tokens": 1, "output_tokens": 2, "total_tokens": 3, "reasoning_tokens": 1, "cached_tokens": 1}}));
        assert_eq!(
            wire,
            vec![format!(
                "data: {}\n\n",
                r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"x"}]},"finishReason":"STOP","index":0}],"usageMetadata":{"promptTokenCount":2,"candidatesTokenCount":2,"totalTokenCount":3,"thoughtsTokenCount":1,"cachedContentTokenCount":1},"modelVersion":"m","responseId":"r"}"#
            )]
        );
    }

    #[test]
    fn unified_round_trip_through_transformer() {
        let t = GeminiTransformer;
        let unified = t.parse_request(&json!({"model": "gemini-2.5-flash", "contents": [{"role": "user", "parts": [{"text": "hi"}]}]})).unwrap();
        assert_eq!(
            t.transform_request(&unified).unwrap(),
            json!({"contents": [{"role": "user", "parts": [{"text": "hi"}]}], "generationConfig": {}})
        );
        let resp = t.transform_response(&json!({"candidates": [{"content": {"parts": [{"text": "yo"}]}, "finishReason": "STOP"}], "responseId": "r", "modelVersion": "m"})).unwrap();
        let formatted = t.format_response(&resp).unwrap();
        assert_eq!(
            formatted,
            json!({"candidates": [{"content": {"role": "model", "parts": [{"text": "yo"}]}, "finishReason": "STOP", "index": 0}], "modelVersion": "m", "responseId": "r"})
        );
    }
}
