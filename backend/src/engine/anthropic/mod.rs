//! `transformers/anthropic/index.ts` (+ the `transformers/anthropic.ts`
//! re-export): composition layer delegating to one module per direction.
//!
//! - `request_parser`: client Anthropic → unified
//! - `request_builder`: unified → provider Anthropic
//! - `response_transformer`: provider → unified
//! - `response_formatter`: unified → client Anthropic
//! - `stream_transformer`: provider stream → unified chunks
//! - `stream_formatter`: unified chunks → client Anthropic SSE
//! - `content_mapper`, `tool_mapper`, `thinking_clamp`,
//!   `synthetic_safeguards`: shared utilities
pub mod content_mapper;
pub mod helpers;
pub mod request_builder;
pub mod request_parser;
pub mod response_formatter;
pub mod response_transformer;
pub mod stream_formatter;
pub mod stream_transformer;
pub mod synthetic_safeguards;
pub mod thinking_clamp;
pub mod tool_mapper;

pub use content_mapper::convert_anthropic_content;
pub use request_builder::{build_anthropic_request, to_anthropic_image_source};
pub use request_parser::parse_anthropic_request;
pub use response_formatter::format_anthropic_response;
pub use response_transformer::transform_anthropic_response;
pub use stream_formatter::AnthropicStreamFormatter;
pub use stream_transformer::AnthropicStreamTransformer;
pub use thinking_clamp::{anthropic_model_supports_disable, clamp_anthropic_effort_and_thinking};
pub use tool_mapper::{
    convert_anthropic_tools_to_unified, convert_unified_tools_to_anthropic,
    ANTHROPIC_BUILTIN_TOOL_TYPES,
};

use super::js::{get, or, parse, truthy};
use super::transformer::*;
use serde_json::{json, Value};

pub struct AnthropicTransformer;

/// `a || b || 0` over optional values.
fn first_truthy(candidates: &[Option<&Value>]) -> Value {
    candidates
        .iter()
        .copied()
        .find(|c| truthy(*c))
        .flatten()
        .cloned()
        .unwrap_or_else(|| json!(0))
}

impl Transformer for AnthropicTransformer {
    fn name(&self) -> &'static str {
        "messages"
    }
    fn default_endpoint(&self) -> &'static str {
        "/messages"
    }

    fn parse_request(&self, input: &Value) -> EngineResult<Value> {
        parse_anthropic_request(input)
    }
    fn transform_request(&self, request: &Value) -> EngineResult<Value> {
        build_anthropic_request(request)
    }
    fn transform_response(&self, response: &Value) -> EngineResult<Value> {
        transform_anthropic_response(response)
    }
    fn format_response(&self, response: &Value) -> EngineResult<Value> {
        format_anthropic_response(response)
    }

    fn stream_transformer(&self) -> Option<Box<dyn StreamTransformer>> {
        Some(Box::new(AnthropicStreamTransformer::new()))
    }
    fn stream_formatter(&self) -> Option<Box<dyn StreamFormatter>> {
        Some(Box::new(AnthropicStreamFormatter::new()))
    }

    /// Usage from Anthropic event data: `message_start` (`message.usage`) and
    /// `message_delta` (`usage`). Parse errors yield `None`.
    fn extract_usage(&self, data: &str) -> Option<Value> {
        let data = parse(data)?;
        if data.is_null() {
            return None;
        }
        let kind = get(&data, &["type"]).and_then(Value::as_str);
        if kind == Some("message_start") && truthy(get(&data, &["message", "usage"])) {
            let u = &data["message"]["usage"];
            return Some(json!({
                "input_tokens": first_truthy(&[u.get("input_tokens")]),
                "output_tokens": first_truthy(&[u.get("output_tokens")]),
                "cached_tokens": first_truthy(&[u.get("cache_read_input_tokens"), u.get("cache_creation_input_tokens")]),
                "reasoning_tokens": first_truthy(&[u.get("thinkingTokens")]),
            }));
        }
        if kind == Some("message_delta") && truthy(data.get("usage")) {
            let u = &data["usage"];
            return Some(json!({
                "input_tokens": or(u.get("input_tokens"), Some(&json!(0))),
                "output_tokens": or(u.get("output_tokens"), Some(&json!(0))),
                "cached_tokens": or(u.get("cache_read_input_tokens"), Some(&json!(0))),
                "reasoning_tokens": or(u.get("thinkingTokens"), Some(&json!(0))),
            }));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::js::stringify;
    use crate::engine::sse::SseParser;

    #[test]
    fn transformer_identity_and_usage() {
        let t = AnthropicTransformer;
        assert_eq!((t.name(), t.default_endpoint()), ("messages", "/messages"));
        assert_eq!(t.endpoint(&json!({})), "/messages");
        let u = t.extract_usage(r#"{"type":"message_start","message":{"usage":{"input_tokens":5,"cache_creation_input_tokens":7}}}"#).unwrap();
        assert_eq!(
            stringify(&u),
            r#"{"input_tokens":5,"output_tokens":0,"cached_tokens":7,"reasoning_tokens":0}"#
        );
        let u = t.extract_usage(r#"{"type":"message_delta","usage":{"output_tokens":9,"cache_creation_input_tokens":7}}"#).unwrap();
        assert_eq!(
            stringify(&u),
            r#"{"input_tokens":0,"output_tokens":9,"cached_tokens":0,"reasoning_tokens":0}"#
        );
        assert!(t.extract_usage("nope").is_none());
        assert!(t.extract_usage("null").is_none());
        assert!(t.extract_usage(r#"{"type":"message_stop"}"#).is_none());
    }

    /// Provider SSE → unified → client SSE round trip.
    #[test]
    fn stream_round_trip() {
        let t = AnthropicTransformer;
        let mut st = t.stream_transformer().unwrap();
        let mut sf = t.stream_formatter().unwrap();
        let raw = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"claude\",\"usage\":{\"input_tokens\":3,\"output_tokens\":1}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        );
        let mut parser = SseParser::new();
        let mut out = String::new();
        for event in parser.feed(raw) {
            for chunk in st.on_event(&event) {
                out.extend(sf.on_chunk(&chunk));
            }
        }
        for chunk in st.finish() {
            out.extend(sf.on_chunk(&chunk));
        }
        out.extend(sf.finish());
        assert_eq!(out, concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":3,\"output_tokens\":1,\"thinkingTokens\":0,\"cache_read_input_tokens\":0,\"cache_creation_input_tokens\":0}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"input_tokens\":0,\"output_tokens\":2,\"thinkingTokens\":0,\"cache_read_input_tokens\":0,\"cache_creation_input_tokens\":0}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ));
    }
}
