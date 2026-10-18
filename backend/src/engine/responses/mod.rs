//! `transformers/responses` (OpenAI Responses API): composition layer over
//! the request parser/builder, response transformer/formatter, and stream
//! transformer/formatter. Codex CLI tool extensions (namespace and custom
//! tools) are per-request state shared by one transformer instance between
//! `parse_request` and `format_response` / the stream formatter, exactly as
//! the TS instance field `toolState` is — so create one
//! `ResponsesTransformer` per request, like the TS `new ResponsesTransformer()`.
pub mod extensions;
pub mod js_ext;
pub mod normalization;
pub mod request_builder;
pub mod request_parser;
pub mod response_formatter;
pub mod response_transformer;
pub mod stream_formatter;
pub mod stream_transformer;
pub mod tool_mapper;
pub mod utils;

#[cfg(test)]
mod tests;

pub use extensions::*;
pub use normalization::*;
pub use request_builder::{build_responses_request, project_reasoning_for_responses};
pub use request_parser::{convert_content_parts, convert_input_items_to_messages, map_input_role, normalize_input, normalize_message_content, parse_responses_request};
pub use response_formatter::{convert_chat_response_to_output_items, format_responses_response};
pub use response_transformer::transform_responses_response;
pub use stream_formatter::{normalize_tool_args, ResponsesStreamFormatter};
pub use stream_transformer::ResponsesStreamTransformer;
pub use tool_mapper::*;
pub use utils::*;

use crate::engine::js::*;
use crate::engine::transformer::*;
use crate::engine::usage::normalize_openai_responses;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex, MutexGuard};

/// `ResponsesTransformer`.
#[derive(Debug, Default, Clone)]
pub struct ResponsesTransformer {
    tool_state: Arc<Mutex<ResponsesToolState>>,
}

impl ResponsesTransformer {
    pub fn new() -> Self { Self::default() }

    fn state(&self) -> MutexGuard<'_, ResponsesToolState> { self.tool_state.lock().unwrap_or_else(|e| e.into_inner()) }

    /// Snapshot of the per-request tool state (`namespaceMap`,
    /// `customToolNames`, `toolNames`).
    pub fn tool_state(&self) -> ResponsesToolState { self.state().clone() }

    /// `get namespaceMap()`.
    pub fn namespace_map(&self) -> Vec<(String, NamespaceEntry)> { self.state().namespace_map.clone() }

    /// `get customToolNames()`.
    pub fn custom_tool_names(&self) -> js_ext::ValueSet { self.state().custom_tool_names.clone() }
}

impl Transformer for ResponsesTransformer {
    fn name(&self) -> &'static str { "responses" }
    fn default_endpoint(&self) -> &'static str { "/responses" }

    fn parse_request(&self, input: &Value) -> EngineResult<Value> { parse_responses_request(input, Some(&mut self.state())) }

    fn transform_request(&self, request: &Value) -> EngineResult<Value> { build_responses_request(request) }

    fn transform_response(&self, response: &Value) -> EngineResult<Value> { transform_responses_response(response) }

    fn format_response(&self, response: &Value) -> EngineResult<Value> { format_responses_response(response, Some(&self.state())) }

    fn stream_transformer(&self) -> Option<Box<dyn StreamTransformer>> { Some(Box::new(ResponsesStreamTransformer::new())) }

    fn stream_formatter(&self) -> Option<Box<dyn StreamFormatter>> { Some(Box::new(ResponsesStreamFormatter::new(Some(self.tool_state.clone())))) }

    fn extract_usage(&self, data: &str) -> Option<Value> {
        let event = parse(data)?;
        if get_str(&event, &["type"]) != Some("response.completed") { return None; }
        let usage = get(&event, &["response", "usage"]).filter(|u| truthy(Some(u)))?;
        let n = normalize_openai_responses(usage);
        Some(json!({
            "input_tokens": n["input_tokens"],
            "output_tokens": n["output_tokens"],
            "cached_tokens": n["cached_tokens"],
            "cache_creation_tokens": n["cache_creation_tokens"],
            "reasoning_tokens": n["reasoning_tokens"],
        }))
    }
}
