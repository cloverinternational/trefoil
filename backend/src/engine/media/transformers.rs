//! `transformers/embeddings/{openai,gemini}.ts`,
//! `services/dispatch/embeddings-transformer-factory.ts`,
//! `transformers/speech.ts` and `transformers/transcriptions.ts`.
use super::form::FormPart;
use crate::engine::js::*;
use serde_json::{json, Value};

/// `EmbeddingsTransformerFactory` targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbeddingsTarget { OpenAI, Gemini }

impl EmbeddingsTarget {
    /// `resolveTransformer(providerTypes)`: dedicated `gemini` first, else OpenAI.
    pub fn resolve(provider_types: &[String]) -> Self {
        if provider_types.iter().any(|t| t.to_lowercase() == "gemini") { Self::Gemini } else { Self::OpenAI }
    }

    pub fn endpoint(self, request: &Value) -> String {
        match self {
            Self::OpenAI => "/embeddings".into(),
            Self::Gemini => {
                let action = if is_batch(request) { "batchEmbedContents" } else { "embedContent" };
                format!("/v1beta/{}:{action}", gemini_model(request))
            }
        }
    }

    pub fn auth_header(self, api_key: &str) -> Option<(String, String)> {
        match self { Self::Gemini => Some(("x-goog-api-key".into(), api_key.into())), Self::OpenAI => None }
    }

    /// `transformRequest(requestWithModel)`.
    pub fn transform_request(self, request: &Value) -> Result<Value, Value> {
        match self {
            Self::OpenAI => {
                let mut out = request.get("originalBody").and_then(Value::as_object).cloned().unwrap_or_default();
                set_opt(&mut out, "model", request.get("model").cloned());
                if request.get("model").is_none() { out.shift_remove("model"); }
                Ok(Value::Object(out))
            }
            Self::Gemini => {
                let model = gemini_model(request);
                let input = request.get("input");
                if is_batch(request) {
                    let reqs: Vec<Value> = input.and_then(Value::as_array).unwrap().iter().map(|t| gemini_payload(&model, t, request)).collect();
                    return Ok(json!({"requests": reqs}));
                }
                let text = match input { Some(Value::Array(a)) => a.first(), other => other };
                let Some(text) = text else { return Err(json!({"message": "Gemini embeddings input array must contain at least one item"})) };
                Ok(gemini_payload(&model, text, request))
            }
        }
    }

    /// `transformResponse(response, requestWithModel)`.
    pub fn transform_response(self, response: &Value, request: &Value) -> Value {
        match self {
            Self::OpenAI => {
                let mut out = response.as_object().cloned().unwrap_or_default();
                out.insert("object".into(), json!("list"));
                for k in ["data", "model", "usage"] {
                    match response.get(k) { Some(v) => { out.insert(k.into(), v.clone()); } None => { out.shift_remove(k); } }
                }
                Value::Object(out)
            }
            Self::Gemini => {
                let model = coalesce(request.get("model"), None).unwrap_or(json!(""));
                let usage = response.get("usageMetadata").filter(|u| truthy(Some(u))).map(|u| {
                    json!({
                        "prompt_tokens": coalesce(u.get("promptTokenCount"), None).unwrap_or(json!(0)),
                        "total_tokens": coalesce(u.get("totalTokenCount"), None).unwrap_or(json!(0)),
                    })
                });
                let data: Vec<Value> = if let Some(embeddings) = response.get("embeddings").filter(|e| truthy(Some(e))) {
                    embeddings.as_array().cloned().unwrap_or_default().iter().enumerate()
                        .map(|(i, item)| obj(vec![("object", Some(json!("embedding"))), ("embedding", item.get("values").cloned()), ("index", Some(json!(i)))]))
                        .collect()
                } else {
                    let values = coalesce(get(response, &["embedding", "values"]), None).unwrap_or(json!([]));
                    vec![json!({"object": "embedding", "embedding": values, "index": 0})]
                };
                obj(vec![("object", Some(json!("list"))), ("data", Some(Value::Array(data))), ("model", Some(model)), ("usage", usage)])
            }
        }
    }
}

/// `formatResponse` (both embeddings transformers).
pub fn format_embeddings_response(response: &Value) -> Value {
    obj(vec![
        ("object", response.get("object").cloned()),
        ("data", response.get("data").cloned()),
        ("model", response.get("model").cloned()),
        ("usage", response.get("usage").cloned()),
    ])
}

fn is_batch(request: &Value) -> bool { request.get("input").and_then(Value::as_array).is_some_and(|a| a.len() > 1) }

fn gemini_model(request: &Value) -> String {
    let model = to_string(request.get("model"));
    if model.starts_with("models/") || model.starts_with("tunedModels/") { model } else { format!("models/{model}") }
}

fn gemini_payload(model: &str, text: &Value, request: &Value) -> Value {
    let mut p = Obj::new();
    p.insert("model".into(), json!(model));
    p.insert("content".into(), json!({"parts": [{"text": text}]}));
    let ob = request.get("originalBody");
    if let Some(t) = ob.and_then(|o| o.get("taskType")) { p.insert("taskType".into(), t.clone()); }
    if let Some(t) = ob.and_then(|o| o.get("title")) { p.insert("title".into(), t.clone()); }
    let dims = coalesce(ob.and_then(|o| o.get("outputDimensionality")), request.get("dimensions"));
    if let Some(d) = dims { p.insert("outputDimensionality".into(), d); }
    Value::Object(p)
}

/// `SpeechTransformer.transformRequest(request)`.
pub fn speech_request(request: &Value) -> Value {
    let response_format = if truthy(request.get("response_format")) { request["response_format"].clone() } else { json!("mp3") };
    let speed = if truthy(request.get("speed")) { request["speed"].clone() } else { json!(1) };
    obj(vec![
        ("model", request.get("model").cloned()),
        ("input", request.get("input").cloned()),
        ("voice", request.get("voice").cloned()),
        ("instructions", request.get("instructions").cloned()),
        ("response_format", Some(response_format)),
        ("speed", Some(speed)),
    ])
}

/// `SpeechTransformer.getMimeType(response_format)`.
pub fn speech_mime_type(response_format: Option<&Value>) -> &'static str {
    let key = if truthy(response_format) { to_string(response_format) } else { "mp3".into() };
    match key.as_str() {
        "mp3" => "audio/mpeg",
        "opus" => "audio/opus",
        "aac" => "audio/aac",
        "flac" => "audio/flac",
        "wav" => "audio/wav",
        "pcm" => "audio/basic",
        _ => "audio/mpeg",
    }
}

/// `UnifiedTranscriptionRequest` (the audio bytes are not JSON).
#[derive(Debug, Clone, Default)]
pub struct TranscriptionRequest {
    pub file: Vec<u8>,
    pub filename: Option<String>,
    pub mime_type: Option<String>,
    pub model: String,
    pub language: Option<String>,
    pub prompt: Option<String>,
    pub response_format: String,
    pub temperature: Option<f64>,
    pub request_id: String,
    /// `metadata` (key access policy), when attached.
    pub metadata: Option<Value>,
}

impl TranscriptionRequest {
    /// The JSON view the routing helpers read (`model`, `metadata`).
    pub fn routing_view(&self) -> Value {
        obj(vec![("model", Some(json!(self.model))), ("metadata", self.metadata.clone())])
    }
}

/// `TranscriptionsTransformer.transformRequest(request)`.
pub fn transcription_form(request: &TranscriptionRequest, model: &str) -> Vec<FormPart> {
    // `new File([bytes], filename, {type})`: an undefined name becomes "undefined".
    let filename = request.filename.clone().unwrap_or_else(|| "undefined".into());
    let mut parts = vec![FormPart::blob("file", request.file.clone(), request.mime_type.as_deref().unwrap_or(""), &filename)];
    parts.push(FormPart::text("model", model));
    if let Some(l) = request.language.as_deref().filter(|s| !s.is_empty()) { parts.push(FormPart::text("language", l)); }
    if let Some(p) = request.prompt.as_deref().filter(|s| !s.is_empty()) { parts.push(FormPart::text("prompt", p)); }
    if !request.response_format.is_empty() { parts.push(FormPart::text("response_format", request.response_format.clone())); }
    if let Some(t) = request.temperature { parts.push(FormPart::text("temperature", js_num_string_any(t))); }
    parts
}

/// `Number.prototype.toString()` including NaN.
pub fn js_num_string_any(n: f64) -> String {
    if n.is_nan() { "NaN".into() } else if n.is_infinite() { if n > 0.0 { "Infinity".into() } else { "-Infinity".into() } } else { js_number_text(n) }
}

fn transcription_usage(response: &Value) -> Option<Value> {
    let u = response.get("usage").filter(|u| truthy(Some(u)))?;
    let n = |k: &str| if truthy(u.get(k)) { u[k].clone() } else { json!(0) };
    Some(json!({"input_tokens": n("input_tokens"), "output_tokens": n("output_tokens"), "total_tokens": n("total_tokens")}))
}

/// `TranscriptionsTransformer.transformResponse(response, format)`.
pub fn transcription_response(response: &Value, format: &str) -> Value {
    let text = |r: &Value| if truthy(r.get("text")) { r["text"].clone() } else { json!("") };
    if format == "text" {
        return match response { Value::String(_) => json!({"text": response}), r => json!({"text": text(r)}) };
    }
    if format == "json" {
        return obj(vec![("text", Some(text(response))), ("usage", transcription_usage(response))]);
    }
    obj(vec![
        ("text", Some(text(response))),
        ("language", response.get("language").cloned()),
        ("duration", response.get("duration").cloned()),
        ("segments", response.get("segments").cloned()),
        ("usage", transcription_usage(response)),
    ])
}

/// `TranscriptionsTransformer.formatResponse(response, format)`.
pub fn format_transcription_response(response: &Value, format: &str) -> Value {
    if format == "text" { return response.get("text").cloned().unwrap_or(Value::Null); }
    let mut out = Obj::new();
    set_opt(&mut out, "text", response.get("text").cloned());
    if format == "verbose_json" {
        for k in ["language", "duration", "segments"] { set_opt(&mut out, k, response.get(k).cloned()); }
    }
    if let Some(u) = response.get("usage").filter(|u| truthy(Some(u))) { out.insert("usage".into(), u.clone()); }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn embeddings_targets() {
        let req = json!({"model": "text-embedding-004", "input": ["a", "b"], "originalBody": {"taskType": "X"}});
        assert_eq!(EmbeddingsTarget::Gemini.endpoint(&req), "/v1beta/models/text-embedding-004:batchEmbedContents");
        let body = EmbeddingsTarget::Gemini.transform_request(&req).unwrap();
        assert_eq!(body["requests"][1], json!({"model": "models/text-embedding-004", "content": {"parts": [{"text": "b"}]}, "taskType": "X"}));
        let r = EmbeddingsTarget::OpenAI.transform_request(&json!({"model": "up", "originalBody": {"model": "alias", "input": "x"}})).unwrap();
        assert_eq!(stringify(&r), r#"{"model":"up","input":"x"}"#);
        assert!(EmbeddingsTarget::Gemini.transform_request(&json!({"model": "m", "input": []})).is_err());
    }
    #[test]
    fn speech_and_transcriptions() {
        assert_eq!(stringify(&speech_request(&json!({"model": "m", "input": "hi", "voice": "alloy"}))), r#"{"model":"m","input":"hi","voice":"alloy","response_format":"mp3","speed":1}"#);
        assert_eq!(speech_mime_type(Some(&json!("pcm"))), "audio/basic");
        assert_eq!(format_transcription_response(&transcription_response(&json!({"text": "t", "language": "en"}), "verbose_json"), "verbose_json"), json!({"text": "t", "language": "en"}));
        assert_eq!(format_transcription_response(&transcription_response(&json!("plain"), "text"), "text"), json!("plain"));
    }
}
