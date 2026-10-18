//! `services/dispatch/transformer-factory.ts`: a fresh transformer per call
//! (Responses / completions keep per-request state).
use super::completions::OpenAICompletionTransformer;
use crate::engine::anthropic::AnthropicTransformer;
use crate::engine::config::api_format::base_type;
use crate::engine::gemini::GeminiTransformer;
use crate::engine::openai::OpenAITransformer;
use crate::engine::responses::ResponsesTransformer;
use crate::engine::transformer::{EngineError, EngineResult, Transformer};
use std::sync::Arc;

/// `TransformerFactory.getTransformer(providerType)`.
pub fn get_transformer(provider_type: &str) -> EngineResult<Arc<dyn Transformer>> {
    Ok(match base_type(provider_type).as_str() {
        "messages" => Arc::new(AnthropicTransformer),
        "gemini" => Arc::new(GeminiTransformer),
        "chat" => Arc::new(OpenAITransformer),
        "completions" => Arc::new(OpenAICompletionTransformer::new()),
        "responses" => Arc::new(ResponsesTransformer::new()),
        "ollama" => Arc::new(super::ollama::OllamaTransformer),
        "images" => Arc::new(ImageBridgeTransformer),
        _ => return Err(EngineError::new(format!(
            "Unsupported provider type: {provider_type}. Only 'messages', 'gemini', 'chat', 'completions', 'responses', 'ollama', and 'images' are allowed."
        ))),
    })
}

/// `transformers/image-bridge.ts`: identity transformer for the chat → image
/// auto-bridge, whose already-unified chunks travel as SSE `data:` JSON.
pub struct ImageBridgeTransformer;

impl Transformer for ImageBridgeTransformer {
    fn name(&self) -> &'static str { "images" }
    fn default_endpoint(&self) -> &'static str { "" }
    fn parse_request(&self, _input: &serde_json::Value) -> EngineResult<serde_json::Value> {
        Err(EngineError::new("'images' is not a wire protocol: bridged image requests are never parsed."))
    }
    fn transform_request(&self, _request: &serde_json::Value) -> EngineResult<serde_json::Value> {
        Err(EngineError::new("'images' is not a wire protocol: bridged image requests are dispatched through the image pipeline."))
    }
    fn transform_response(&self, response: &serde_json::Value) -> EngineResult<serde_json::Value> { Ok(response.clone()) }
    fn format_response(&self, response: &serde_json::Value) -> EngineResult<serde_json::Value> { Ok(response.clone()) }
    fn stream_transformer(&self) -> Option<Box<dyn crate::engine::transformer::StreamTransformer>> { Some(Box::new(UnifiedPassthrough)) }
}

struct UnifiedPassthrough;
impl crate::engine::transformer::StreamTransformer for UnifiedPassthrough {
    fn on_event(&mut self, event: &crate::engine::sse::SseEvent) -> Vec<serde_json::Value> {
        crate::engine::js::parse(&event.data).into_iter().collect()
    }
}

/// `TransformerRegistry` for the response handler's provider-side streams.
pub struct Registry;

impl crate::engine::observability::TransformerRegistry for Registry {
    fn transformer(&self, api_type: &str) -> Option<Arc<dyn Transformer>> { get_transformer(api_type).ok() }
}
