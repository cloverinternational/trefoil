//! `transformers/gemini/part-mapper.ts`.
use super::utils::{read_error, type_error};
use crate::engine::js::*;
use crate::engine::transformer::EngineResult;
use serde_json::{json, Value};

/// `convertGeminiPartsToUnified(parts, onThinking?)`: text, inlineData,
/// fileData and thought parts → unified `MessageContent[]`. Thought parts go
/// to `on_thinking(text, thoughtSignature)` when a callback is given (and are
/// otherwise emitted as text).
pub fn convert_gemini_parts_to_unified(
    parts: &Value,
    mut on_thinking: Option<&mut dyn FnMut(&Value, Option<&Value>)>,
) -> EngineResult<Vec<Value>> {
    let Value::Array(parts) = parts else { return Err(type_error("parts.forEach is not a function")) };
    let mut out = Vec::new();
    for part in parts {
        if part.is_null() { return Err(read_error(Some(part), "text")); }
        if truthy(part.get("text")) {
            let text = &part["text"];
            match on_thinking.as_mut() {
                Some(cb) if truthy(part.get("thought")) => cb(text, part.get("thoughtSignature")),
                _ => out.push(json!({"type": "text", "text": text})),
            }
        } else if truthy(part.get("inlineData")) {
            let mime = get(part, &["inlineData", "mimeType"]);
            let data = get(part, &["inlineData", "data"]);
            out.push(obj(vec![
                ("type", Some(json!("image_url"))),
                ("image_url", Some(json!({"url": format!("data:{};base64,{}", to_string(mime), to_string(data))}))),
                ("media_type", mime.cloned()),
            ]));
        } else if truthy(part.get("fileData")) {
            let uri = or(get(part, &["fileData", "fileUri"]), Some(&json!(""))).unwrap_or(Value::Null);
            out.push(obj(vec![
                ("type", Some(json!("image_url"))),
                ("image_url", Some(json!({"url": uri}))),
                ("media_type", get(part, &["fileData", "mimeType"]).cloned()),
            ]));
        }
    }
    Ok(out)
}

/// `convertUnifiedPartsToGemini(content)`. Note the TS behaviour of always
/// labelling data-URL images `image/jpeg` is preserved.
pub fn convert_unified_parts_to_gemini(content: Option<&Value>) -> EngineResult<Vec<Value>> {
    let mut parts = Vec::new();
    match content {
        Some(Value::String(s)) => parts.push(json!({"text": s})),
        Some(Value::Array(items)) => {
            for c in items {
                if c.is_null() { return Err(read_error(Some(c), "type")); }
                match get_str(c, &["type"]) {
                    Some("text") => parts.push(obj(vec![("text", c.get("text").cloned())])),
                    Some("image_url") => {
                        let image_url = c.get("image_url");
                        if nullish(image_url) { return Err(read_error(image_url, "url")); }
                        let url = match get(c, &["image_url", "url"]) {
                            Some(Value::String(u)) => u,
                            other @ (None | Some(Value::Null)) => return Err(read_error(other, "startsWith")),
                            Some(_) => return Err(type_error("c.image_url.url.startsWith is not a function")),
                        };
                        if url.starts_with("data:") {
                            let data = url.split(',').nth(1).unwrap_or("");
                            parts.push(json!({"inlineData": {"mimeType": "image/jpeg", "data": data}}));
                        } else {
                            let mime = or(c.get("media_type"), Some(&json!("image/jpeg"))).unwrap_or(Value::Null);
                            parts.push(json!({"fileData": {"mimeType": mime, "fileUri": url}}));
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    Ok(parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gemini_to_unified_parts() {
        let parts = json!([
            {"text": "hi"},
            {"text": "think", "thought": true, "thoughtSignature": "aGVsbG8="},
            {"inlineData": {"mimeType": "image/png", "data": "AAA"}},
            {"fileData": {"fileUri": "gs://x"}},
            {"text": ""}
        ]);
        let mut seen = Vec::new();
        let mut cb = |t: &Value, s: Option<&Value>| seen.push((t.clone(), s.cloned()));
        let out = convert_gemini_parts_to_unified(&parts, Some(&mut cb)).unwrap();
        assert_eq!(out, vec![
            json!({"type": "text", "text": "hi"}),
            json!({"type": "image_url", "image_url": {"url": "data:image/png;base64,AAA"}, "media_type": "image/png"}),
            json!({"type": "image_url", "image_url": {"url": "gs://x"}}),
        ]);
        assert_eq!(seen, vec![(json!("think"), Some(json!("aGVsbG8=")))]);
        // Without a callback thought parts are plain text.
        assert_eq!(convert_gemini_parts_to_unified(&json!([{"text": "t", "thought": true}]), None).unwrap(), vec![json!({"type": "text", "text": "t"})]);
        assert!(convert_gemini_parts_to_unified(&json!({}), None).is_err());
    }

    #[test]
    fn unified_to_gemini_parts() {
        assert_eq!(convert_unified_parts_to_gemini(Some(&json!("x"))).unwrap(), vec![json!({"text": "x"})]);
        let out = convert_unified_parts_to_gemini(Some(&json!([
            {"type": "text", "text": "a"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,QUJD"}},
            {"type": "image_url", "image_url": {"url": "https://x/y.png"}, "media_type": "image/png"},
            {"type": "image_url", "image_url": {"url": "data:nocomma"}},
            {"type": "other"}
        ]))).unwrap();
        assert_eq!(out, vec![
            json!({"text": "a"}),
            json!({"inlineData": {"mimeType": "image/jpeg", "data": "QUJD"}}),
            json!({"fileData": {"mimeType": "image/png", "fileUri": "https://x/y.png"}}),
            json!({"inlineData": {"mimeType": "image/jpeg", "data": ""}}),
        ]);
    }
}
