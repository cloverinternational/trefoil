//! `types/transformer.ts`: one implementation per wire protocol.
//!
//! Requests/responses are JSON values shaped like the TypeScript unified
//! types (`types/unified.ts`); `undefined` members are absent keys.
//! Streams are push-based state machines replacing the TS `ReadableStream`
//! pipes: `StreamTransformer` turns provider SSE events into unified chunks,
//! `StreamFormatter` turns unified chunks into client wire text.
use super::sse::SseEvent;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct EngineError {
    pub message: String,
    /// `routingContext.statusCode` when the TS error carried one.
    pub status: Option<u16>,
    /// `routingContext.code` when the TS error carried one.
    pub code: Option<String>,
}

impl EngineError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            status: None,
            code: None,
        }
    }
    pub fn status(message: impl Into<String>, status: u16, code: &str) -> Self {
        Self {
            message: message.into(),
            status: Some(status),
            code: Some(code.into()),
        }
    }
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

pub type EngineResult<T> = Result<T, EngineError>;

/// Provider stream → unified chunks.
pub trait StreamTransformer: Send {
    /// One parsed SSE event from the provider.
    fn on_event(&mut self, event: &SseEvent) -> Vec<Value>;
    /// Raw text fed before SSE parsing, for transformers that do not use SSE
    /// framing (default: parse as SSE in the caller and ignore here).
    fn on_raw(&mut self, _text: &str) -> Vec<Value> {
        Vec::new()
    }
    /// Provider stream ended.
    fn finish(&mut self) -> Vec<Value> {
        Vec::new()
    }
    /// True when `on_raw` should receive the raw text instead of SSE events.
    fn wants_raw(&self) -> bool {
        false
    }
}

/// Unified chunks → client wire text (already SSE-encoded).
pub trait StreamFormatter: Send {
    fn on_chunk(&mut self, chunk: &Value) -> Vec<String>;
    /// Unified stream ended.
    fn finish(&mut self) -> Vec<String> {
        Vec::new()
    }
}

pub trait Transformer: Send + Sync {
    /// API type name (`chat`, `messages`, `gemini`, `responses`, …).
    fn name(&self) -> &'static str;
    fn default_endpoint(&self) -> &'static str;
    /// `getEndpoint?(request)`; defaults to `default_endpoint`.
    fn endpoint(&self, _request: &Value) -> String {
        self.default_endpoint().to_owned()
    }
    /// Client request (this format) → unified request.
    fn parse_request(&self, input: &Value) -> EngineResult<Value>;
    /// Unified request → provider request (this format).
    fn transform_request(&self, request: &Value) -> EngineResult<Value>;
    /// Provider response (this format) → unified response.
    fn transform_response(&self, response: &Value) -> EngineResult<Value>;
    /// Unified response → client response (this format).
    fn format_response(&self, response: &Value) -> EngineResult<Value>;
    fn stream_transformer(&self) -> Option<Box<dyn StreamTransformer>> {
        None
    }
    fn stream_formatter(&self) -> Option<Box<dyn StreamFormatter>> {
        None
    }
    /// Usage from one SSE event's data (`extractUsage`).
    fn extract_usage(&self, _data: &str) -> Option<Value> {
        None
    }
}
