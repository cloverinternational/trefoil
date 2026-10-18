//! `transformers/anthropic/content-mapper.ts`.
use super::helpers::{format_base64, type_error};
use crate::engine::js::{get, get_str, obj, truthy};
use crate::engine::transformer::EngineResult;
use serde_json::{json, Value};

/// `convertAnthropicContent`: Anthropic content blocks (text, image) →
/// unified content.
///
/// Returns a plain string for a single text block without (truthy)
/// `cache_control`, `""` when nothing maps, and an array of unified parts
/// otherwise (preserving `cache_control`). `Ok(None)` is JS `undefined`: a
/// lone text block whose `text` is missing.
pub fn convert_anthropic_content(content: &[Value]) -> EngineResult<Option<Value>> {
    let mut parts: Vec<Value> = Vec::new();
    for c in content {
        if c.is_null() { return Err(type_error(Some(c), "c.type")); }
        let kind = get_str(c, &["type"]);
        if kind == Some("text") {
            parts.push(obj(vec![
                ("type", Some(json!("text"))),
                ("text", c.get("text").cloned()),
                ("cache_control", c.get("cache_control").cloned()),
            ]));
        } else if kind == Some("image") && truthy(c.get("source")) {
            let source = &c["source"];
            let url = if get_str(source, &["type"]) == Some("base64") {
                Some(Value::String(format_base64(get(source, &["data"]), get(source, &["media_type"]))?))
            } else {
                get(source, &["url"]).cloned()
            };
            parts.push(obj(vec![
                ("type", Some(json!("image_url"))),
                ("image_url", Some(obj(vec![("url", url)]))),
                ("media_type", get(source, &["media_type"]).cloned()),
                ("cache_control", c.get("cache_control").cloned()),
            ]));
        }
    }
    if parts.is_empty() { return Ok(Some(json!(""))); }
    let first = &parts[0];
    if parts.len() == 1 && get_str(first, &["type"]) == Some("text") && !truthy(first.get("cache_control")) {
        return Ok(first.get("text").cloned());
    }
    Ok(Some(Value::Array(parts)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_text_collapses_to_string() {
        assert_eq!(convert_anthropic_content(&[json!({"type": "text", "text": "hi"})]).unwrap(), Some(json!("hi")));
        assert_eq!(convert_anthropic_content(&[json!({"type": "tool_use"})]).unwrap(), Some(json!("")));
        assert_eq!(convert_anthropic_content(&[json!({"type": "text"})]).unwrap(), None);
    }

    #[test]
    fn keeps_cache_control_and_images() {
        let out = convert_anthropic_content(&[
            json!({"type": "text", "text": "a", "cache_control": {"type": "ephemeral"}}),
            json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "QUJD"}}),
            json!({"type": "image", "source": {"type": "url", "url": "https://x/y.png"}, "cache_control": null}),
        ]).unwrap().unwrap();
        assert_eq!(out, json!([
            {"type": "text", "text": "a", "cache_control": {"type": "ephemeral"}},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,QUJD"}, "media_type": "image/png"},
            {"type": "image_url", "image_url": {"url": "https://x/y.png"}, "cache_control": null},
        ]));
        assert_eq!(crate::engine::js::stringify(&out[1]), r#"{"type":"image_url","image_url":{"url":"data:image/png;base64,QUJD"},"media_type":"image/png"}"#);
    }
}
