//! `transformers/image-rendering.ts`.
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::Value;

pub const MAX_INLINE_IMAGE_BASE64_CHARS: usize = 8 * 1024 * 1024;

fn sniff(base64_result: &str) -> &'static str {
    let prefix: String = base64_result.chars().take(24).collect();
    let head = STANDARD
        .decode(prefix.trim_end_matches('='))
        .or_else(|_| {
            base64::engine::general_purpose::STANDARD_NO_PAD.decode(prefix.trim_end_matches('='))
        })
        .unwrap_or_default();
    if head.len() >= 4 && head[..4] == [0x89, 0x50, 0x4e, 0x47] {
        return "png";
    }
    if head.len() >= 2 && head[0] == 0xff && head[1] == 0xd8 {
        return "jpeg";
    }
    if head.len() >= 12 && &head[0..4] == b"RIFF" && &head[8..12] == b"WEBP" {
        return "webp";
    }
    if head.len() >= 4 && &head[0..4] == b"GIF8" {
        return "gif";
    }
    "png"
}

fn render(result: Option<&Value>) -> Option<String> {
    let result = result?.as_str().filter(|r| !r.is_empty())?;
    if result.len() > MAX_INLINE_IMAGE_BASE64_CHARS {
        return Some(format!(
            "[generated image omitted: {:.1} MB exceeds inline limit]",
            (result.len() as f64 * 3.0) / 4.0 / (1024.0 * 1024.0)
        ));
    }
    Some(format!(
        "![generated image](data:image/{};base64,{result})",
        sniff(result)
    ))
}

/// `imageGenerationCallMarkdown`.
pub fn image_generation_call_markdown(item: &Value) -> Option<String> {
    if item.get("type").and_then(Value::as_str) != Some("image_generation_call") {
        return None;
    }
    render(item.get("result"))
}

/// `composeContentWithImageMarkdown`: returns the content unchanged (incl.
/// `None` for undefined) when there is no image markdown.
pub fn compose_content(content: Option<&Value>, calls: Option<&Value>) -> Option<Value> {
    let segments: Vec<String> = calls
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|c| render(c.get("result")))
        .collect();
    if segments.is_empty() {
        return content.cloned();
    }
    let mut parts = Vec::new();
    if let Some(text) = content.and_then(Value::as_str).filter(|t| !t.is_empty()) {
        parts.push(text.to_owned());
    }
    parts.extend(segments);
    Some(Value::String(parts.join("\n")))
}
