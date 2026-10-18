//! `transformers/image.ts`, `transformers/images/{gemini,openrouter,codex}.ts`
//! and `services/dispatch/image-transformer-factory.ts`.
//!
//! Requests are the unified image IR as JSON (`UnifiedImageGenerationRequest`).
//! Errors are TS-shaped values (`{message, name?, routingContext?}`).
use super::form::{FormPart, ImagePayload};
use crate::engine::config::api_format::base_type;
use crate::engine::js::*;
use base64::Engine as _;
use serde_json::{json, Value};

const IMAGE_RESOLUTIONS: [&str; 4] = ["512", "1K", "2K", "4K"];
const IMAGE_ASPECT_RATIOS: [&str; 22] = [
    "1:1", "1:2", "1:4", "1:8", "2:1", "2:3", "3:2", "3:4", "4:1", "4:3", "4:5", "5:4", "8:1",
    "9:16", "16:9", "9:19.5", "19.5:9", "9:20", "20:9", "9:21", "21:9", "auto",
];
const IMAGE_QUALITIES: [&str; 4] = ["auto", "low", "medium", "high"];
const IMAGE_OUTPUT_FORMATS: [&str; 4] = ["png", "jpeg", "webp", "svg"];
const IMAGE_BACKGROUNDS: [&str; 3] = ["auto", "transparent", "opaque"];
const MAX_IMAGE_REFERENCES: usize = 16;
const MAX_REMOTE_IMAGE_BYTES: usize = 20 * 1024 * 1024;
const REMOTE_IMAGE_TIMEOUT_MS: u64 = 15_000;

/// `ImageRequestValidationError(message, statusCode = 400)`.
pub fn validation_error(message: impl Into<String>, status: u16) -> Value {
    let code = match status {
        400 => "invalid_request_error",
        501 => "unsupported_image_feature",
        _ => "upstream_invalid_response",
    };
    json!({"message": message.into(), "name": "ImageRequestValidationError", "routingContext": {"statusCode": status, "code": code}})
}

fn invalid(message: impl Into<String>) -> Value {
    validation_error(message, 400)
}

/// `TIER_SIZE_PATTERN`.
pub fn is_tier_size(size: &str) -> bool {
    IMAGE_RESOLUTIONS.contains(&size)
}

fn is_pixel_size(size: &str) -> bool {
    let Some((w, h)) = size.split_once('x') else {
        return false;
    };
    !w.is_empty()
        && !h.is_empty()
        && w.bytes().all(|b| b.is_ascii_digit())
        && h.bytes().all(|b| b.is_ascii_digit())
}

/// `OPENAI_TIER_SIZES`.
pub fn openai_tier_size(tier: &str) -> Option<&'static str> {
    match tier {
        "512" => Some("512x512"),
        "1K" => Some("1024x1024"),
        "2K" => Some("2048x2048"),
        "4K" => Some("4096x4096"),
        _ => None,
    }
}

fn openai_aspect_ratio_size(ratio: &str) -> Option<&'static str> {
    match ratio {
        "1:1" => Some("1024x1024"),
        "2:3" | "3:4" | "4:5" | "9:16" => Some("1024x1792"),
        "3:2" | "4:3" | "5:4" | "16:9" | "21:9" => Some("1792x1024"),
        _ => None,
    }
}

/// Node `Buffer.from(s, 'base64')`: lenient (standard or URL-safe alphabet,
/// non-alphabet characters skipped, decoding stops at the first `=`).
pub fn decode_base64_lenient(s: &str) -> Vec<u8> {
    let mut clean: String = s
        .chars()
        .take_while(|c| *c != '=')
        .filter_map(|c| match c {
            '-' => Some('+'),
            '_' => Some('/'),
            c if c.is_ascii_alphanumeric() || c == '+' || c == '/' => Some(c),
            _ => None,
        })
        .collect();
    if clean.len() % 4 == 1 {
        clean.pop();
    }
    base64::engine::general_purpose::STANDARD_NO_PAD
        .decode(clean.as_bytes())
        .unwrap_or_default()
}

pub fn encode_base64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// `parseImageDataUrl` → `(mimeType, data)`; `Ok(None)` when not an image data URL.
pub fn parse_image_data_url(url: &str) -> Result<Option<(String, Vec<u8>)>, Value> {
    let Some(rest) = url.strip_prefix("data:") else {
        return Ok(None);
    };
    let Some(comma) = rest.find(',') else {
        return Ok(None);
    };
    let header = &rest[..comma];
    let segments: Vec<&str> = header.split(';').collect();
    if segments.len() < 2
        || segments.iter().any(|s| s.is_empty())
        || *segments.last().unwrap() != "base64"
    {
        return Ok(None);
    }
    let mime = segments[0];
    if !mime.starts_with("image/") {
        return Ok(None);
    }
    let data = decode_base64_lenient(&rest[comma + 1..]);
    if data.is_empty() {
        return Err(invalid("Image data URL contains no image bytes"));
    }
    Ok(Some((mime.to_owned(), data)))
}

fn is_unsafe_remote_image_host(hostname: &str) -> bool {
    let host = hostname.to_lowercase();
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host == "localhost" || host.ends_with(".local") || host == "::1" {
        return true;
    }
    if host.starts_with("10.")
        || host.starts_with("192.168.")
        || host.starts_with("127.")
        || host.starts_with("169.254.")
    {
        return true;
    }
    let parts: Vec<&str> = host.split('.').collect();
    if parts.len() != 4
        || parts
            .iter()
            .any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()))
    {
        return false;
    }
    let first: f64 = parts[0].parse().unwrap_or(f64::NAN);
    let second: f64 = parts[1].parse().unwrap_or(f64::NAN);
    first == 0.0 || (first == 172.0 && (16.0..=31.0).contains(&second))
}

/// WHATWG `new URL(...)` subset used by the image helpers.
fn parse_url(url: &str) -> Option<url::Url> {
    url::Url::parse(url).ok()
}

/// `resolveImageReference(reference)` → `(mimeType, data)`.
pub async fn resolve_image_reference(reference: &Value) -> Result<(String, Vec<u8>), Value> {
    let raw = get_str(reference, &["image_url", "url"])
        .unwrap_or_default()
        .to_owned();
    if let Some(inline) = parse_image_data_url(&raw)? {
        return Ok(inline);
    }
    let url = parse_url(&raw).ok_or_else(|| invalid("Image reference URL is invalid"))?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(invalid("Image reference URL must use http or https"));
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || is_unsafe_remote_image_host(url.host_str().unwrap_or_default())
    {
        return Err(invalid("Image reference URL is not allowed"));
    }
    let client = remote_image_client();
    let sent = client
        .get(url.as_str())
        .header("accept", "image/*")
        .header("user-agent", crate::engine::execute::DEFAULT_USER_AGENT)
        .timeout(std::time::Duration::from_millis(REMOTE_IMAGE_TIMEOUT_MS))
        .send()
        .await;
    let response = match sent {
        Ok(r) => r,
        Err(e) if e.is_timeout() => return Err(invalid("Image reference download timed out")),
        Err(_) => return Err(invalid("Image reference could not be downloaded")),
    };
    // `redirect: 'error'` rejects the fetch on any redirect.
    if response.status().is_redirection() {
        return Err(invalid("Image reference could not be downloaded"));
    }
    if !response.status().is_success() {
        return Err(invalid(format!(
            "Image reference URL returned HTTP {}",
            response.status().as_u16()
        )));
    }
    if let Some(len) = response
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<f64>().ok())
    {
        if len > MAX_REMOTE_IMAGE_BYTES as f64 {
            return Err(invalid("Image reference exceeds the maximum allowed size"));
        }
    }
    let header_mime = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(';').next().unwrap_or_default().trim().to_owned());
    let data = match response.bytes().await {
        Ok(b) => b.to_vec(),
        Err(e) if e.is_timeout() => return Err(invalid("Image reference download timed out")),
        Err(_) => return Err(invalid("Image reference could not be downloaded")),
    };
    if data.is_empty() || data.len() > MAX_REMOTE_IMAGE_BYTES {
        return Err(invalid("Image reference contains no usable image bytes"));
    }
    let media_type = get_str(reference, &["media_type"])
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .or(header_mime);
    match media_type {
        Some(m) if m.starts_with("image/") => Ok((m, data)),
        _ => Err(invalid("Image reference response is not an image")),
    }
}

fn remote_image_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("http client")
    })
}

fn require_non_empty_string(value: Option<&Value>, field: &str) -> Result<(), Value> {
    match value {
        Some(Value::String(s)) if !s.trim().is_empty() => Ok(()),
        _ => Err(invalid(format!("{field} must be a non-empty string"))),
    }
}

fn validate_reference(reference: &Value, index: usize) -> Result<(), Value> {
    if !truthy(Some(reference)) || !reference.is_object() && !reference.is_array() {
        return Err(invalid(format!(
            "input_references[{index}] must be an image reference"
        )));
    }
    if reference.get("type") != Some(&json!("image_url")) || !truthy(reference.get("image_url")) {
        return Err(invalid(format!(
            "input_references[{index}] must use type 'image_url'"
        )));
    }
    require_non_empty_string(
        get(reference, &["image_url", "url"]),
        &format!("input_references[{index}].image_url.url"),
    )?;
    let url = get_str(reference, &["image_url", "url"]).unwrap();
    if url.starts_with("data:") {
        if parse_image_data_url(url)?.is_none() {
            return Err(invalid(format!(
                "input_references[{index}] must contain an image data URL"
            )));
        }
        return Ok(());
    }
    let parsed = parse_url(url).ok_or_else(|| {
        invalid(format!(
            "input_references[{index}] must contain a valid URL"
        ))
    })?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err(invalid(format!(
            "input_references[{index}] URL must use http, https, or a data URL"
        )));
    }
    Ok(())
}

fn validate_provider_preferences(provider: Option<&Value>) -> Result<(), Value> {
    let Some(value) = provider else { return Ok(()) };
    if !value.is_object() {
        return Err(invalid("provider must be an object"));
    }
    for field in ["only", "ignore", "order"] {
        let Some(v) = value.get(field) else { continue };
        let ok = v
            .as_array()
            .is_some_and(|a| a.iter().all(|e| e.as_str().is_some_and(|s| !s.is_empty())));
        if !ok {
            return Err(invalid(format!(
                "provider.{field} must be an array of strings"
            )));
        }
    }
    if value
        .get("allow_fallbacks")
        .is_some_and(|v| !v.is_boolean())
    {
        return Err(invalid("provider.allow_fallbacks must be a boolean"));
    }
    if let Some(sort) = value.get("sort") {
        if !sort.is_string() && !sort.is_object() {
            return Err(invalid("provider.sort must be a string or object"));
        }
    }
    if value.get("options").is_some_and(|v| !v.is_object()) {
        return Err(invalid("provider.options must be an object"));
    }
    Ok(())
}

/// `size.split('x').map(Number)` → first two entries.
fn split_dims(s: &str, sep: char) -> (f64, f64) {
    let mut it = s
        .split(sep)
        .map(|p| to_number(Some(&json!(p))).unwrap_or(f64::NAN));
    (it.next().unwrap_or(f64::NAN), it.next().unwrap_or(f64::NAN))
}

fn js_falsy_num(n: f64) -> bool {
    n == 0.0 || n.is_nan()
}

fn resolution_tier_for_size(size: &str) -> Option<&'static str> {
    if let Some(t) = IMAGE_RESOLUTIONS.iter().find(|t| **t == size) {
        return Some(t);
    }
    let (w, h) = split_dims(size, 'x');
    if js_falsy_num(w) || js_falsy_num(h) {
        return None;
    }
    let longest = w.max(h);
    Some(if longest <= 512.0 {
        "512"
    } else if longest <= 1024.0 {
        "1K"
    } else if longest <= 2048.0 {
        "2K"
    } else {
        "4K"
    })
}

fn validate_size_and_aspect_ratio(
    size: Option<&Value>,
    resolution: Option<&Value>,
    aspect: Option<&Value>,
) -> Result<(), Value> {
    let size_s = size.filter(|v| truthy(Some(v))).map(|v| to_string(Some(v)));
    let res_s = resolution
        .filter(|v| truthy(Some(v)))
        .map(|v| to_string(Some(v)));
    if let (Some(s), Some(r)) = (&size_s, &res_s) {
        if resolution_tier_for_size(s) != Some(r.as_str()) {
            return Err(invalid(format!(
                "size '{s}' conflicts with resolution '{r}'"
            )));
        }
    }
    let Some(s) = size_s else { return Ok(()) };
    let Some(a) = aspect
        .filter(|v| truthy(Some(v)))
        .map(|v| to_string(Some(v)))
    else {
        return Ok(());
    };
    if a == "auto" || is_tier_size(&s) {
        return Ok(());
    }
    let (w, h) = split_dims(&s, 'x');
    let (rw, rh) = split_dims(&a, ':');
    if [w, h, rw, rh].iter().any(|n| js_falsy_num(*n)) {
        return Ok(());
    }
    if ((w / h) - (rw / rh)).abs() > 0.01 {
        return Err(invalid(format!(
            "size '{s}' conflicts with aspect_ratio '{a}'"
        )));
    }
    Ok(())
}

fn is_integer(v: &Value) -> bool {
    v.as_f64()
        .is_some_and(|n| n.is_finite() && n.fract() == 0.0)
}

fn str_in(v: &Value, set: &[&str]) -> bool {
    v.as_str().is_some_and(|s| set.contains(&s))
}

/// `validateOpenRouterRequest(input)`.
pub fn validate_openrouter_request(input: &Value) -> Result<(), Value> {
    require_non_empty_string(input.get("model"), "model")?;
    require_non_empty_string(input.get("prompt"), "prompt")?;
    if let Some(n) = input.get("n") {
        if !is_integer(n) || n.as_f64().unwrap() < 1.0 || n.as_f64().unwrap() > 10.0 {
            return Err(invalid("n must be an integer between 1 and 10"));
        }
    }
    if input
        .get("resolution")
        .is_some_and(|v| !str_in(v, &IMAGE_RESOLUTIONS))
    {
        return Err(invalid("resolution must be one of 512, 1K, 2K, or 4K"));
    }
    if input
        .get("aspect_ratio")
        .is_some_and(|v| !str_in(v, &IMAGE_ASPECT_RATIOS))
    {
        return Err(invalid("aspect_ratio is not supported"));
    }
    if let Some(size) = input.get("size") {
        let ok = size
            .as_str()
            .is_some_and(|s| is_pixel_size(s) || is_tier_size(s));
        if !ok {
            return Err(invalid(
                "size must be a resolution tier or pixel dimensions such as 1024x1024",
            ));
        }
    }
    if input
        .get("quality")
        .is_some_and(|v| !str_in(v, &IMAGE_QUALITIES))
    {
        return Err(invalid("quality must be one of auto, low, medium, or high"));
    }
    if input
        .get("output_format")
        .is_some_and(|v| !str_in(v, &IMAGE_OUTPUT_FORMATS))
    {
        return Err(invalid("output_format must be png, jpeg, webp, or svg"));
    }
    if input
        .get("background")
        .is_some_and(|v| !str_in(v, &IMAGE_BACKGROUNDS))
    {
        return Err(invalid("background must be auto, transparent, or opaque"));
    }
    if let Some(c) = input.get("output_compression") {
        if !is_integer(c) || c.as_f64().unwrap() < 0.0 || c.as_f64().unwrap() > 100.0 {
            return Err(invalid(
                "output_compression must be an integer between 0 and 100",
            ));
        }
    }
    if input.get("seed").is_some_and(|v| !is_integer(v)) {
        return Err(invalid("seed must be an integer"));
    }
    if input
        .get("response_format")
        .is_some_and(|v| !str_in(v, &["url", "b64_json"]))
    {
        return Err(invalid("response_format must be url or b64_json"));
    }
    if let Some(refs) = input.get("input_references") {
        match refs.as_array() {
            Some(a) if a.len() <= MAX_IMAGE_REFERENCES => {
                for (i, r) in a.iter().enumerate() {
                    validate_reference(r, i)?;
                }
            }
            _ => {
                return Err(invalid(format!(
                    "input_references must contain at most {MAX_IMAGE_REFERENCES} images"
                )))
            }
        }
    }
    validate_size_and_aspect_ratio(
        input.get("size"),
        input.get("resolution"),
        input.get("aspect_ratio"),
    )?;
    validate_provider_preferences(input.get("provider"))?;
    if input.get("stream") == Some(&Value::Bool(true)) {
        return Err(validation_error(
            "Image streaming is not supported yet",
            501,
        ));
    }
    Ok(())
}

/// `normalizeImageUsage(usage)`.
pub fn normalize_image_usage(usage: Option<&Value>) -> Option<Value> {
    let u = usage.filter(|u| u.is_object() || u.is_array())?;
    let mut out = u.as_object().cloned().unwrap_or_default();
    let input = coalesce(u.get("input_tokens"), u.get("prompt_tokens"));
    let output = coalesce(u.get("output_tokens"), u.get("completion_tokens"));
    if let Some(i) = input {
        out.insert("input_tokens".into(), i.clone());
        out.insert(
            "prompt_tokens".into(),
            coalesce(u.get("prompt_tokens"), Some(&i)).unwrap(),
        );
    }
    if let Some(o) = output {
        out.insert("output_tokens".into(), o.clone());
        out.insert(
            "completion_tokens".into(),
            coalesce(u.get("completion_tokens"), Some(&o)).unwrap(),
        );
    }
    if let Some(t) = u.get("total_tokens") {
        out.insert("total_tokens".into(), t.clone());
    }
    if let Some(c) = u.get("cost") {
        out.insert("cost".into(), c.clone());
    }
    Some(Value::Object(out))
}

/// Copies `url`, `b64_json`, `media_type`, `revised_prompt` when defined.
fn image_item(item: &Value) -> Value {
    let mut out = Obj::new();
    for k in ["url", "b64_json", "media_type", "revised_prompt"] {
        if let Some(v) = item.get(k) {
            out.insert(k.into(), v.clone());
        }
    }
    Value::Object(out)
}

fn created_or_now(response: &Value) -> Value {
    coalesce(response.get("created"), None).unwrap_or_else(|| json!(now_secs()))
}

/// `formatOpenRouterImageResponse(response)`.
pub async fn format_openrouter_image_response(response: &Value) -> Result<Value, Value> {
    let mut data = Vec::new();
    for item in get_arr(response, &["data"]).cloned().unwrap_or_default() {
        if item.get("b64_json").is_some() || item.get("url").is_none() {
            data.push(item);
            continue;
        }
        let mut reference = json!({"type": "image_url", "image_url": {"url": item["url"].clone()}});
        if let Some(m) = item.get("media_type") {
            reference["media_type"] = m.clone();
        }
        let (mime, bytes) = resolve_image_reference(&reference).await?;
        let mut out = obj(vec![
            ("b64_json", Some(json!(encode_base64(&bytes)))),
            ("media_type", Some(json!(mime))),
        ]);
        if let Some(rp) = item.get("revised_prompt") {
            out["revised_prompt"] = rp.clone();
        }
        data.push(out);
    }
    let usage = response.get("usage").filter(|u| truthy(Some(u)));
    let formatted = usage.map(|u| {
        let mut m = Obj::new();
        if let Some(p) = u.get("prompt_tokens") {
            m.insert("prompt_tokens".into(), p.clone());
        } else if let Some(p) = u.get("input_tokens") {
            m.insert("prompt_tokens".into(), p.clone());
        }
        if let Some(c) = u.get("completion_tokens") {
            m.insert("completion_tokens".into(), c.clone());
        } else if let Some(c) = u.get("output_tokens") {
            m.insert("completion_tokens".into(), c.clone());
        }
        if let Some(t) = u.get("total_tokens") {
            m.insert("total_tokens".into(), t.clone());
        }
        if let Some(c) = u.get("cost") {
            m.insert("cost".into(), c.clone());
        }
        m
    });
    let mut out = Obj::new();
    set_opt(&mut out, "created", response.get("created").cloned());
    out.insert("data".into(), Value::Array(data));
    if let Some(f) = formatted.filter(|f| !f.is_empty()) {
        out.insert("usage".into(), Value::Object(f));
    }
    Ok(Value::Object(out))
}

/// `resolveOpenAISize(request)`.
pub fn resolve_openai_size(request: &Value) -> Result<Option<Value>, Value> {
    if truthy(request.get("size")) {
        let size = to_string(request.get("size"));
        if let Some(px) = openai_tier_size(&size) {
            return Ok(Some(json!(px)));
        }
        return Ok(request.get("size").cloned());
    }
    if truthy(request.get("aspect_ratio")) && request.get("aspect_ratio") != Some(&json!("auto")) {
        let ratio = to_string(request.get("aspect_ratio"));
        return match openai_aspect_ratio_size(&ratio) {
            Some(s) => Ok(Some(json!(s))),
            None => Err(invalid(format!(
                "OpenAI-compatible image targets cannot map aspect_ratio '{ratio}'"
            ))),
        };
    }
    if truthy(request.get("resolution")) {
        return Ok(openai_tier_size(&to_string(request.get("resolution"))).map(|s| json!(s)));
    }
    Ok(None)
}

fn append_form_value(parts: &mut Vec<FormPart>, key: &str, value: Option<&Value>) {
    if let Some(v) = value.filter(|v| !v.is_null()) {
        parts.push(FormPart::text(key, to_string(Some(v))));
    }
}

async fn build_openai_reference_request(request: &Value) -> Result<ImagePayload, Value> {
    let references = get_arr(request, &["input_references"])
        .cloned()
        .unwrap_or_default();
    if references.len() != 1 {
        return Err(invalid(
            "OpenAI-compatible image editing targets support exactly one input reference",
        ));
    }
    let (mime, data) = resolve_image_reference(&references[0]).await?;
    let mut parts = Vec::new();
    append_form_value(&mut parts, "model", request.get("model"));
    append_form_value(&mut parts, "prompt", request.get("prompt"));
    append_form_value(&mut parts, "n", request.get("n"));
    append_form_value(&mut parts, "size", resolve_openai_size(request)?.as_ref());
    append_form_value(
        &mut parts,
        "response_format",
        request.get("response_format"),
    );
    let quality = request.get("quality").filter(|q| *q != &json!("auto"));
    append_form_value(&mut parts, "quality", quality);
    append_form_value(&mut parts, "user", request.get("user"));
    append_form_value(&mut parts, "output_format", request.get("output_format"));
    append_form_value(&mut parts, "background", request.get("background"));
    append_form_value(
        &mut parts,
        "output_compression",
        request.get("output_compression"),
    );
    append_form_value(&mut parts, "seed", request.get("seed"));
    parts.push(FormPart::blob("image", data, &mime, "reference.png"));
    if let Some(mask) = request.get("mask").filter(|m| truthy(Some(m))) {
        let (mime, data) = resolve_image_reference(mask).await?;
        parts.push(FormPart::blob("mask", data, &mime, "mask.png"));
    }
    Ok(ImagePayload::Form(parts))
}

/// Magic-byte sniff (`sniffImageMediaType`).
fn sniff_image_media_type(data: &[u8]) -> &'static str {
    if data.len() >= 4 && data[..4] == [0x89, 0x50, 0x4e, 0x47] {
        return "image/png";
    }
    if data.len() >= 2 && data[0] == 0xff && data[1] == 0xd8 {
        return "image/jpeg";
    }
    if data.len() >= 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        return "image/webp";
    }
    if data.len() >= 4 && &data[..4] == b"GIF8" {
        return "image/gif";
    }
    "image/png"
}

/// `imageReferenceFromBuffer(data, mimeType)`.
pub fn image_reference_from_buffer(data: &[u8], mime_type: Option<&str>) -> Value {
    let media_type = match mime_type {
        Some(m) if m.starts_with("image/") => m.to_owned(),
        _ => sniff_image_media_type(data).to_owned(),
    };
    json!({"type": "image_url", "image_url": {"url": format!("data:{media_type};base64,{}", encode_base64(data))}, "media_type": media_type})
}

fn build_openai_generation_request(request: &Value) -> Result<Value, Value> {
    let mut p = Obj::new();
    set_opt(&mut p, "model", request.get("model").cloned());
    set_opt(&mut p, "prompt", request.get("prompt").cloned());
    if let Some(size) = resolve_openai_size(request)? {
        p.insert("size".into(), size);
    }
    for k in ["n", "response_format"] {
        set_opt(&mut p, k, request.get(k).cloned());
    }
    if let Some(q) = request.get("quality").filter(|q| *q != &json!("auto")) {
        p.insert("quality".into(), q.clone());
    }
    for k in [
        "style",
        "user",
        "output_format",
        "background",
        "output_compression",
        "seed",
    ] {
        set_opt(&mut p, k, request.get(k).cloned());
    }
    Ok(Value::Object(p))
}

/// `ImageTransformer.parseGenerationRequest(input)`.
pub fn parse_generation_request(input: &Value) -> Value {
    let mut out = Obj::new();
    for k in [
        "model",
        "prompt",
        "n",
        "size",
        "resolution",
        "aspect_ratio",
        "response_format",
        "quality",
        "style",
        "output_format",
        "background",
        "output_compression",
        "seed",
        "stream",
        "input_references",
        "provider",
        "user",
        "requestId",
        "incomingApiType",
        "originalBody",
    ] {
        set_opt(&mut out, k, input.get(k).cloned());
    }
    Value::Object(out)
}

/// `ImageTransformer.parseOpenRouterGenerationRequest(input)`.
pub fn parse_openrouter_generation_request(input: &Value) -> Result<Value, Value> {
    validate_openrouter_request(input)?;
    let mut parsed = parse_generation_request(input);
    parsed["response_format"] =
        coalesce(input.get("response_format"), None).unwrap_or(json!("b64_json"));
    parsed["incomingApiType"] =
        coalesce(input.get("incomingApiType"), None).unwrap_or(json!("images"));
    parsed["originalBody"] =
        coalesce(input.get("originalBody"), None).unwrap_or_else(|| input.clone());
    Ok(parsed)
}

fn has_references(request: &Value) -> bool {
    get_arr(request, &["input_references"]).is_some_and(|a| !a.is_empty())
}

fn prefix_model(model: &str) -> String {
    if model.starts_with("models/") || model.starts_with("tunedModels/") {
        model.to_owned()
    } else {
        format!("models/{model}")
    }
}

/// Target-protocol transformers (`ImageGenerationTransformerFactory`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageTarget {
    OpenAI,
    Gemini,
    OpenRouter,
    Codex,
}

impl ImageTarget {
    /// `ImageGenerationTransformerFactory.resolveTransformer(providerType)`.
    pub fn resolve(provider_type: Option<&str>) -> Result<Self, Value> {
        let t = provider_type.filter(|s| !s.is_empty()).unwrap_or("chat");
        match base_type(t).as_str() {
            "gemini" => Ok(Self::Gemini),
            "openrouter-images" => Ok(Self::OpenRouter),
            "codex-images" => Ok(Self::Codex),
            "chat" | "completions" | "openai-images" | "openai" => Ok(Self::OpenAI),
            _ => Err(
                json!({"message": format!("Unsupported image provider type: {t}. Supported image targets are OpenAI-compatible, OpenRouter, Codex, and Gemini.")}),
            ),
        }
    }

    /// `transformer.getAuthHeaders` (when defined) or `None` for the Bearer default.
    pub fn auth_header(self, api_key: &str) -> Option<(String, String)> {
        match self {
            Self::Gemini => Some(("x-goog-api-key".into(), api_key.into())),
            _ => None,
        }
    }

    pub fn get_endpoint(self, request: &Value) -> Result<String, Value> {
        let streaming = truthy(request.get("stream"));
        match self {
            Self::OpenAI => Ok(if has_references(request) {
                "/images/edits"
            } else {
                "/images/generations"
            }
            .into()),
            Self::Gemini => {
                if streaming {
                    return Err(validation_error(
                        "Image streaming is not supported yet",
                        501,
                    ));
                }
                Ok(format!(
                    "/v1beta/{}:generateContent",
                    prefix_model(&to_string(request.get("model")))
                ))
            }
            Self::OpenRouter => {
                if streaming {
                    return Err(validation_error(
                        "Image streaming is not supported yet",
                        501,
                    ));
                }
                Ok("/images".into())
            }
            Self::Codex => {
                if streaming {
                    return Err(validation_error(
                        "Image streaming is not supported yet",
                        501,
                    ));
                }
                Ok(if has_references(request) {
                    "/images/edits"
                } else {
                    "/images/generations"
                }
                .into())
            }
        }
    }

    pub async fn transform_request(self, request: &Value) -> Result<ImagePayload, Value> {
        match self {
            Self::OpenAI => {
                if has_references(request) {
                    build_openai_reference_request(request).await
                } else {
                    Ok(ImagePayload::Json(build_openai_generation_request(
                        request,
                    )?))
                }
            }
            Self::Gemini => gemini_request(request).await.map(ImagePayload::Json),
            Self::OpenRouter => openrouter_request(request).map(ImagePayload::Json),
            Self::Codex => codex_request(request).await.map(ImagePayload::Json),
        }
    }

    pub fn transform_response(self, response: &Value) -> Result<Value, Value> {
        match self {
            Self::OpenAI => {
                let data = get_arr(response, &["data"])
                    .map(|a| a.iter().map(image_item).collect())
                    .unwrap_or_default();
                Ok(image_response(
                    created_or_now(response),
                    data,
                    normalize_image_usage(response.get("usage")),
                ))
            }
            Self::OpenRouter => {
                let data: Vec<Value> = get_arr(response, &["data"])
                    .map(|a| a.iter().map(image_item).collect())
                    .unwrap_or_default();
                if data.is_empty() {
                    return Err(validation_error(
                        "OpenRouter image generation returned no image data",
                        502,
                    ));
                }
                Ok(image_response(
                    created_or_now(response),
                    data,
                    normalize_image_usage(response.get("usage")),
                ))
            }
            Self::Gemini => {
                let mut data = Vec::new();
                for candidate in get_arr(response, &["candidates"])
                    .cloned()
                    .unwrap_or_default()
                {
                    for part in get_arr(&candidate, &["content", "parts"])
                        .cloned()
                        .unwrap_or_default()
                    {
                        let inline = coalesce(part.get("inlineData"), part.get("inline_data"));
                        let Some(inline) = inline.filter(|i| truthy(i.get("data"))) else {
                            continue;
                        };
                        let mime = coalesce(inline.get("mimeType"), inline.get("mime_type"))
                            .unwrap_or(json!("image/png"));
                        data.push(json!({"b64_json": inline["data"].clone(), "media_type": mime}));
                    }
                }
                if data.is_empty() {
                    return Err(validation_error(
                        "Gemini image generation returned no image data",
                        502,
                    ));
                }
                let usage = response
                    .get("usageMetadata")
                    .filter(|u| truthy(Some(u)))
                    .map(|u| {
                        obj(vec![
                            ("input_tokens", u.get("promptTokenCount").cloned()),
                            ("output_tokens", u.get("candidatesTokenCount").cloned()),
                            ("total_tokens", u.get("totalTokenCount").cloned()),
                        ])
                    });
                Ok(image_response(created_or_now(response), data, usage))
            }
            Self::Codex => {
                let media_type = response
                    .get("output_format")
                    .and_then(Value::as_str)
                    .and_then(|f| match f.to_lowercase().as_str() {
                        "png" => Some("image/png"),
                        "jpeg" | "jpg" => Some("image/jpeg"),
                        "webp" => Some("image/webp"),
                        _ => None,
                    });
                let mut data = Vec::new();
                for item in get_arr(response, &["data"]).cloned().unwrap_or_default() {
                    let Some(b64) = item
                        .get("b64_json")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                    else {
                        continue;
                    };
                    data.push(obj(vec![
                        ("b64_json", Some(json!(b64))),
                        ("media_type", media_type.map(|m| json!(m))),
                    ]));
                }
                if data.is_empty() {
                    return Err(validation_error(
                        "Codex image generation returned no image data",
                        502,
                    ));
                }
                Ok(image_response(
                    created_or_now(response),
                    data,
                    normalize_image_usage(response.get("usage")),
                ))
            }
        }
    }
}

fn image_response(created: Value, data: Vec<Value>, usage: Option<Value>) -> Value {
    obj(vec![
        ("created", Some(created)),
        ("data", Some(Value::Array(data))),
        ("usage", usage),
    ])
}

async fn gemini_request(request: &Value) -> Result<Value, Value> {
    if let Some(f) = request
        .get("output_format")
        .filter(|v| truthy(Some(v)))
        .filter(|v| *v != &json!("png"))
    {
        return Err(invalid(format!(
            "Native Gemini image generation does not support output_format '{}'",
            to_string(Some(f))
        )));
    }
    if let Some(b) = request
        .get("background")
        .filter(|v| truthy(Some(v)))
        .filter(|v| *v != &json!("auto"))
    {
        return Err(invalid(format!(
            "Native Gemini image generation does not support background '{}'",
            to_string(Some(b))
        )));
    }
    if request.get("output_compression").is_some() {
        return Err(invalid(
            "Native Gemini image generation does not support output_compression",
        ));
    }
    if request.get("style").is_some() {
        return Err(invalid(
            "Native Gemini image generation does not support style",
        ));
    }
    if request.get("mask").is_some() {
        return Err(invalid(
            "Native Gemini image generation does not support mask images",
        ));
    }
    let mut parts = vec![obj(vec![("text", request.get("prompt").cloned())])];
    for reference in get_arr(request, &["input_references"])
        .cloned()
        .unwrap_or_default()
    {
        let (mime, data) = resolve_image_reference(&reference).await?;
        parts.push(json!({"inlineData": {"mimeType": mime, "data": encode_base64(&data)}}));
    }
    let resolution = coalesce(request.get("resolution"), None).or_else(|| {
        request
            .get("size")
            .filter(|s| truthy(Some(s)))
            .and_then(|s| gemini_resolution_from_size(&to_string(Some(s))))
            .map(|r| json!(r))
    });
    let mut image_config = Obj::new();
    if let Some(a) = request
        .get("aspect_ratio")
        .filter(|v| truthy(Some(v)))
        .filter(|v| *v != &json!("auto"))
    {
        image_config.insert("aspectRatio".into(), a.clone());
    }
    if let Some(r) = resolution.filter(|r| truthy(Some(r))) {
        image_config.insert("imageSize".into(), r);
    }
    let mut generation = Obj::new();
    generation.insert("responseModalities".into(), json!(["IMAGE"]));
    set_opt(&mut generation, "candidateCount", request.get("n").cloned());
    set_opt(&mut generation, "seed", request.get("seed").cloned());
    if !image_config.is_empty() {
        generation.insert("imageConfig".into(), Value::Object(image_config));
    }
    Ok(json!({"contents": [{"role": "user", "parts": parts}], "generationConfig": generation}))
}

fn gemini_resolution_from_size(size: &str) -> Option<&'static str> {
    if let Some(t) = IMAGE_RESOLUTIONS.iter().find(|t| **t == size) {
        return Some(t);
    }
    if !is_pixel_size(size) {
        return None;
    }
    let (w, h) = split_dims(size, 'x');
    let longest = w.max(h);
    Some(if longest <= 512.0 {
        "512"
    } else if longest <= 1024.0 {
        "1K"
    } else if longest <= 2048.0 {
        "2K"
    } else {
        "4K"
    })
}

fn openrouter_request(request: &Value) -> Result<Value, Value> {
    if request.get("response_format") == Some(&json!("url")) {
        return Err(invalid(
            "OpenRouter image targets return base64 image data rather than URL responses",
        ));
    }
    if request.get("style").is_some() || request.get("user").is_some() {
        return Err(invalid(
            "OpenRouter image targets do not support the legacy style or user fields",
        ));
    }
    if request.get("mask").is_some() {
        return Err(invalid(
            "OpenRouter image targets do not support mask images",
        ));
    }
    let mut p = Obj::new();
    set_opt(&mut p, "model", request.get("model").cloned());
    set_opt(&mut p, "prompt", request.get("prompt").cloned());
    for k in [
        "n",
        "resolution",
        "aspect_ratio",
        "size",
        "quality",
        "output_format",
        "background",
        "output_compression",
        "seed",
        "input_references",
    ] {
        set_opt(&mut p, k, request.get(k).cloned());
    }
    Ok(Value::Object(p))
}

fn codex_aspect_ratio_size(ratio: &str) -> Option<&'static str> {
    match ratio {
        "1:1" => Some("1024x1024"),
        "3:2" | "4:3" | "5:4" | "16:9" | "21:9" => Some("1536x1024"),
        "2:3" | "3:4" | "4:5" | "9:16" => Some("1024x1536"),
        _ => None,
    }
}

fn resolve_codex_size(request: &Value) -> Result<Option<Value>, Value> {
    if truthy(request.get("size")) {
        let size = to_string(request.get("size"));
        if let Some(px) = openai_tier_size(&size) {
            return Ok(Some(json!(px)));
        }
        return Ok(request.get("size").cloned());
    }
    if truthy(request.get("aspect_ratio")) && request.get("aspect_ratio") != Some(&json!("auto")) {
        let ratio = to_string(request.get("aspect_ratio"));
        return match codex_aspect_ratio_size(&ratio) {
            Some(s) => Ok(Some(json!(s))),
            None => Err(invalid(format!(
                "Codex image targets cannot map aspect_ratio '{ratio}'"
            ))),
        };
    }
    if truthy(request.get("resolution")) {
        return Ok(openai_tier_size(&to_string(request.get("resolution"))).map(|s| json!(s)));
    }
    Ok(None)
}

async fn codex_request(request: &Value) -> Result<Value, Value> {
    if request.get("response_format") == Some(&json!("url")) {
        return Err(invalid(
            "Codex image targets return base64 image data rather than URL responses",
        ));
    }
    if request.get("output_format").is_some() {
        return Err(invalid(
            "Codex image targets choose their own output_format and cannot honor a requested one",
        ));
    }
    if request.get("output_compression").is_some() {
        return Err(invalid(
            "Codex image targets do not support output_compression",
        ));
    }
    if request.get("seed").is_some() {
        return Err(invalid("Codex image targets do not support seed"));
    }
    if request.get("style").is_some() {
        return Err(invalid("Codex image targets do not support style"));
    }
    if request.get("user").is_some() {
        return Err(invalid("Codex image targets do not support user"));
    }
    if request.get("mask").is_some() {
        return Err(invalid("Codex image targets do not support mask images"));
    }
    if has_references(request) {
        let references = get_arr(request, &["input_references"])
            .cloned()
            .unwrap_or_default();
        if references.len() > 5 {
            return Err(invalid(
                "Codex image editing targets support at most 5 input references",
            ));
        }
        let size = resolve_codex_size(request)?;
        let mut images = Vec::new();
        for r in &references {
            let (mime, data) = resolve_image_reference(r).await?;
            images
                .push(json!({"image_url": format!("data:{mime};base64,{}", encode_base64(&data))}));
        }
        let mut p = Obj::new();
        p.insert("images".into(), Value::Array(images));
        set_opt(&mut p, "prompt", request.get("prompt").cloned());
        set_opt(&mut p, "model", request.get("model").cloned());
        p.insert(
            "background".into(),
            coalesce(request.get("background"), None).unwrap_or(json!("auto")),
        );
        p.insert(
            "quality".into(),
            coalesce(request.get("quality"), None).unwrap_or(json!("auto")),
        );
        p.insert(
            "size".into(),
            size.filter(|s| !s.is_null()).unwrap_or(json!("auto")),
        );
        set_opt(&mut p, "n", request.get("n").cloned());
        return Ok(Value::Object(p));
    }
    let mut p = Obj::new();
    set_opt(&mut p, "prompt", request.get("prompt").cloned());
    set_opt(&mut p, "model", request.get("model").cloned());
    if let Some(size) = resolve_codex_size(request)? {
        p.insert("size".into(), size);
    }
    for k in ["n", "quality", "background"] {
        set_opt(&mut p, k, request.get(k).cloned());
    }
    Ok(Value::Object(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_urls_and_sizes() {
        let (mime, data) = parse_image_data_url("data:image/png;base64,aGVsbG8=")
            .unwrap()
            .unwrap();
        assert_eq!(
            (mime.as_str(), data.as_slice()),
            ("image/png", b"hello".as_slice())
        );
        assert!(parse_image_data_url("data:text/plain;base64,aGk=")
            .unwrap()
            .is_none());
        assert!(parse_image_data_url("data:image/png;base64,").is_err());
        assert_eq!(
            resolve_openai_size(&json!({"size": "2K"})).unwrap(),
            Some(json!("2048x2048"))
        );
        assert_eq!(
            resolve_openai_size(&json!({"aspect_ratio": "16:9"})).unwrap(),
            Some(json!("1792x1024"))
        );
        assert!(resolve_openai_size(&json!({"aspect_ratio": "1:8"})).is_err());
        assert_eq!(
            image_reference_from_buffer(b"GIF89a", Some("application/octet-stream"))["media_type"],
            json!("image/gif")
        );
    }

    #[test]
    fn openrouter_validation_messages() {
        let err = |v: Value| {
            validate_openrouter_request(&v).unwrap_err()["message"]
                .as_str()
                .unwrap()
                .to_owned()
        };
        assert_eq!(
            err(json!({"prompt": "x"})),
            "model must be a non-empty string"
        );
        assert_eq!(
            err(json!({"model": "m", "prompt": "x", "n": 11})),
            "n must be an integer between 1 and 10"
        );
        assert_eq!(
            err(json!({"model": "m", "prompt": "x", "size": "1024x1024", "resolution": "2K"})),
            "size '1024x1024' conflicts with resolution '2K'"
        );
        assert_eq!(
            err(json!({"model": "m", "prompt": "x", "size": "1024x1024", "aspect_ratio": "16:9"})),
            "size '1024x1024' conflicts with aspect_ratio '16:9'"
        );
        let s = validate_openrouter_request(&json!({"model": "m", "prompt": "x", "stream": true}))
            .unwrap_err();
        assert_eq!(
            s["routingContext"],
            json!({"statusCode": 501, "code": "unsupported_image_feature"})
        );
        assert!(validate_openrouter_request(
            &json!({"model": "m", "prompt": "x", "size": "1K", "resolution": "1K"})
        )
        .is_ok());
    }

    #[test]
    fn usage_normalization() {
        assert_eq!(
            normalize_image_usage(Some(&json!({"prompt_tokens": 3}))),
            Some(json!({"prompt_tokens": 3, "input_tokens": 3}))
        );
        assert_eq!(normalize_image_usage(Some(&json!(5))), None);
    }
}
