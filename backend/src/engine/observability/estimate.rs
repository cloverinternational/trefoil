//! `utils/estimate-tokens.ts`: input-token estimation over the client
//! request and output/reasoning estimation over a reconstructed response.
//!
//! Plexus counts text with `js-tiktoken` (o200k_base) and falls back to a
//! character-density heuristic. No o200k tokenizer ships with Trefoil, so the
//! tokenizer is a pluggable [`TextTokenizer`]; without one every count uses
//! the heuristic (exactly Plexus's encoder-failed path). Lengths are UTF-16
//! code units, as in JavaScript.
use crate::engine::js::{self, get, get_str, Obj};
use regex::Regex;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::{OnceLock, RwLock};

const MAX_TOKENIZE_CHARS: usize = 256_000;
const ENTROPY_SAMPLE_CHARS: usize = 512;
const MIN_UNIQUE_CHARS: usize = 8;
/// `Number.MAX_SAFE_INTEGER`, the fail-closed sentinel.
pub const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

/// o200k_base token counter (`enc.encode(text).length`). `None` means the
/// encoder failed and the heuristic is used.
pub trait TextTokenizer: Send + Sync {
    fn count(&self, text: &str) -> Option<usize>;
}

/// js-tiktoken `getEncoding('o200k_base')`. `encode(text)` throws on special
/// tokens (disallowedSpecial "all"), which Plexus catches → heuristic.
pub struct O200k(tiktoken_rs::CoreBPE);

impl O200k {
    pub fn load() -> Option<Self> { tiktoken_rs::o200k_base().ok().map(Self) }
}

impl TextTokenizer for O200k {
    fn count(&self, text: &str) -> Option<usize> {
        if self.0.special_tokens().iter().any(|t| text.contains(*t)) { return None; }
        Some(self.0.encode_ordinary(text).len())
    }
}

fn tokenizer_cell() -> &'static RwLock<Option<std::sync::Arc<dyn TextTokenizer>>> {
    static CELL: OnceLock<RwLock<Option<std::sync::Arc<dyn TextTokenizer>>>> = OnceLock::new();
    CELL.get_or_init(|| RwLock::new(None))
}

/// Install the process-wide tokenizer (Plexus's lazy encoder singleton).
pub fn set_tokenizer(tokenizer: Option<std::sync::Arc<dyn TextTokenizer>>) {
    if let Ok(mut g) = tokenizer_cell().write() { *g = tokenizer; }
}

fn utf16_len(s: &str) -> usize { s.encode_utf16().count() }

/// First `n` UTF-16 code units of `s` (`text.slice(0, n)`; a split surrogate
/// pair is dropped rather than producing invalid UTF-8).
fn utf16_prefix(s: &str, n: usize) -> &str {
    let mut units = 0;
    for (i, c) in s.char_indices() {
        let w = c.len_utf16();
        if units + w > n { return &s[..i]; }
        units += w;
    }
    s
}

fn looks_repetitive(text: &str) -> bool {
    let sample = if utf16_len(text) > ENTROPY_SAMPLE_CHARS { utf16_prefix(text, ENTROPY_SAMPLE_CHARS) } else { text };
    let mut unique = HashSet::new();
    for ch in sample.chars() {
        unique.insert(ch);
        if unique.len() >= MIN_UNIQUE_CHARS { return false; }
    }
    true
}

/// `estimateTokens(text)`.
pub fn estimate_tokens(text: &str) -> f64 {
    let tokenizer = tokenizer_cell().read().ok().and_then(|g| g.clone());
    estimate_tokens_with(text, tokenizer.as_deref())
}

/// `estimateTokens` with an explicit tokenizer (`None` = encoder failed).
pub fn estimate_tokens_with(text: &str, tokenizer: Option<&dyn TextTokenizer>) -> f64 {
    if text.is_empty() { return 0.0; }
    if utf16_len(text) > MAX_TOKENIZE_CHARS || looks_repetitive(text) { return estimate_tokens_heuristic(text); }
    if let Some(n) = tokenizer.and_then(|t| t.count(text)) { return n as f64; }
    estimate_tokens_heuristic(text)
}

struct HeuristicRes { ws: Regex, brackets: Regex, punct: Regex, numbers: Regex, urls: Regex, compares: Regex, fncalls: Regex, indent: Regex, special: Regex }

fn res() -> &'static HeuristicRes {
    static R: OnceLock<HeuristicRes> = OnceLock::new();
    R.get_or_init(|| {
        let r = |p: &str| Regex::new(p).expect("valid regex");
        // JS `\w`/`\d` are ASCII; `\s` is Unicode whitespace (plus U+FEFF).
        HeuristicRes {
            ws: r(r"[\s\x{FEFF}]"),
            brackets: r(r"[{}\[\]]"),
            punct: r(r"[.,;:!?]"),
            numbers: r(r"[0-9]+"),
            urls: r(r"https?://[^\s\x{FEFF}]+"),
            compares: r(r"[=<>!&|]{2}"),
            fncalls: r(r"[A-Za-z0-9_]{1,64}\("),
            indent: r(r"\n {2,}"),
            special: r(r#"[^A-Za-z0-9_\s\x{FEFF}.,;:!?'"()\[\]{}<>/\\\-]"#),
        }
    })
}

/// `estimateTokensHeuristic(text)`.
pub fn estimate_tokens_heuristic(text: &str) -> f64 {
    if text.is_empty() { return 0.0; }
    let r = res();
    let char_count = utf16_len(text) as f64;
    let mut estimate = char_count / 4.0;
    let count = |re: &Regex| re.find_iter(text).count() as f64;
    let whitespace_ratio = count(&r.ws) / char_count;
    if whitespace_ratio > 0.15 { estimate *= 0.95; } else if whitespace_ratio < 0.1 { estimate *= 1.1; }
    estimate += count(&r.brackets) * 0.5;
    estimate += count(&r.punct) * 0.3;
    estimate += count(&r.numbers) * 0.2;
    estimate += count(&r.urls) * 2.0;
    let code_indicators = count(&r.compares) + count(&r.fncalls) + count(&r.indent);
    if code_indicators > char_count / 100.0 { estimate *= 1.08; }
    // Without the `u` flag an astral character is two matches (two surrogates).
    let special: usize = r.special.find_iter(text).map(|m| utf16_len(m.as_str())).sum();
    estimate += special as f64 * 0.4;
    let unique: HashSet<char> = text.chars().collect();
    if (unique.len() as f64 / char_count) < 0.05 { estimate *= 0.9; }
    js_round(estimate)
}

/// `Math.round` (half rounds toward +∞).
fn js_round(x: f64) -> f64 { (x + 0.5).floor() }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageDimensions { pub width: u32, pub height: u32 }

/// Node `Buffer.from(s, 'base64')`: lenient (skips invalid characters,
/// accepts URL-safe alphabet, stops at padding, keeps partial groups).
fn lenient_base64(s: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let mut acc: u32 = 0;
    let mut bits = 0;
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            _ => continue,
        };
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    out
}

/// `decodeURIComponent` (None on malformed escapes / invalid UTF-8).
fn decode_uri_component(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn data_uri_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"(?s)^data:([^;,]+)?(?:;([^,]*))?,(.*)$").expect("valid regex"))
}

/// `getImageDimensionsFromDataUri`.
pub fn image_dimensions_from_data_uri(data_uri: &str) -> Option<ImageDimensions> {
    if !data_uri.starts_with("data:") { return None; }
    let caps = data_uri_re().captures(data_uri)?;
    let params = caps.get(2).map(|m| m.as_str()).unwrap_or("");
    let payload = caps.get(3).map(|m| m.as_str()).unwrap_or("");
    if payload.is_empty() { return None; }
    let head = utf16_prefix(payload, 4096);
    let bytes = if params.split(';').any(|p| p.trim().eq_ignore_ascii_case("base64")) {
        lenient_base64(&head.split_whitespace().collect::<String>())
    } else {
        // `Buffer.from(decoded, 'binary')`: latin-1, one byte per code unit.
        decode_uri_component(head)?.encode_utf16().map(|u| u as u8).collect()
    };
    image_dimensions_from_bytes(&bytes)
}

fn be16(b: &[u8], i: usize) -> u32 { ((b[i] as u32) << 8) | b[i + 1] as u32 }
fn le16(b: &[u8], i: usize) -> u32 { b[i] as u32 | ((b[i + 1] as u32) << 8) }
fn be32(b: &[u8], i: usize) -> u32 { u32::from_be_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]) }

/// `getImageDimensionsFromBuffer` (PNG / GIF / WebP / JPEG sniffing).
pub fn image_dimensions_from_bytes(b: &[u8]) -> Option<ImageDimensions> {
    if b.len() < 12 { return None; }
    let dims = |width, height| Some(ImageDimensions { width, height });
    if b[..8] == [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a] && b.len() >= 24 {
        return dims(be32(b, 16), be32(b, 20));
    }
    if b[..4] == [0x47, 0x49, 0x46, 0x38] && (b[4] == 0x37 || b[4] == 0x39) && b[5] == 0x61 && b.len() >= 10 {
        return dims(le16(b, 6), le16(b, 8));
    }
    if b.len() >= 30 && &b[..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        let chunk = &b[12..16];
        if chunk == b"VP8 " {
            return dims(le16(b, 26) & 0x3fff, le16(b, 28) & 0x3fff);
        }
        if chunk == b"VP8L" && b[20] == 0x2f {
            let (b0, b1, b2, b3) = (b[21] as u32, b[22] as u32, b[23] as u32, b[24] as u32);
            return dims(1 + (((b1 & 0x3f) << 8) | b0), 1 + (((b3 & 0x0f) << 10) | (b2 << 2) | ((b1 & 0xc0) >> 6)));
        }
        if chunk == b"VP8X" {
            let w = 1 + (b[24] as u32 | ((b[25] as u32) << 8) | ((b[26] as u32) << 16));
            let h = 1 + (b[27] as u32 | ((b[28] as u32) << 8) | ((b[29] as u32) << 16));
            return dims(w, h);
        }
    }
    if b[0] == 0xff && b[1] == 0xd8 && b[2] == 0xff {
        let len = b.len();
        let mut i = 2usize;
        while i + 9 < len {
            if b[i] != 0xff { i += 1; continue; }
            while i < len && b[i] == 0xff { i += 1; }
            if i >= len { break; }
            let marker = b[i];
            i += 1;
            if marker == 0xd8 || marker == 0xd9 || (0xd0..=0xd7).contains(&marker) || marker == 0x01 { continue; }
            if i + 1 >= len { break; }
            let seg_len = be16(b, i) as usize;
            let is_sof = (0xc0..=0xc3).contains(&marker) || (0xc5..=0xc7).contains(&marker) || (0xc9..=0xcb).contains(&marker) || (0xcd..=0xcf).contains(&marker);
            if is_sof && i + 6 < len { return Some(ImageDimensions { height: be16(b, i + 3), width: be16(b, i + 5) }); }
            i += seg_len;
        }
    }
    None
}

const CLAUDE_DEFAULT_IMAGE_TOKENS: f64 = 1600.0;
const OPENAI_DEFAULT_IMAGE_TOKENS: f64 = 1100.0;
const GEMINI_IMAGE_TOKENS: f64 = 258.0;
const OPENAI_LOW_DETAIL_TOKENS: f64 = 85.0;
const OPENAI_TILE_TOKENS: f64 = 170.0;

/// `NormalizedImage`.
#[derive(Debug, Clone, Default)]
pub struct NormalizedImage {
    pub data_uri: Option<String>,
    pub url: Option<String>,
    pub detail: Option<String>,
    pub base64: Option<String>,
    pub media_type: Option<String>,
}

fn dimensions_for(image: &NormalizedImage) -> Option<ImageDimensions> {
    if let Some(uri) = image.data_uri.as_deref().filter(|s| !s.is_empty()) { return image_dimensions_from_data_uri(uri); }
    if let Some(b64) = image.base64.as_deref().filter(|s| !s.is_empty()) {
        return image_dimensions_from_bytes(&lenient_base64(&utf16_prefix(b64, 4096).split_whitespace().collect::<String>()));
    }
    None
}

fn claude_image_tokens(image: &NormalizedImage) -> f64 {
    match dimensions_for(image) {
        Some(d) if d.width > 0 && d.height > 0 => CLAUDE_DEFAULT_IMAGE_TOKENS.min(((d.width as f64) * (d.height as f64) / 750.0).ceil()),
        _ => CLAUDE_DEFAULT_IMAGE_TOKENS,
    }
}

fn openai_image_tokens(image: &NormalizedImage) -> f64 {
    if image.detail.as_deref() == Some("low") { return OPENAI_LOW_DETAIL_TOKENS; }
    let Some(d) = dimensions_for(image).filter(|d| d.width > 0 && d.height > 0) else { return OPENAI_DEFAULT_IMAGE_TOKENS };
    if image.detail.as_deref() == Some("auto") && d.width <= 512 && d.height <= 512 { return OPENAI_LOW_DETAIL_TOKENS; }
    let (mut w, mut h) = (d.width as f64, d.height as f64);
    if w > 2048.0 || h > 2048.0 {
        let scale = 2048.0 / w.max(h);
        w = js_round(w * scale);
        h = js_round(h * scale);
    }
    let short = w.min(h);
    if short > 768.0 {
        let scale = 768.0 / short;
        w = js_round(w * scale);
        h = js_round(h * scale);
    }
    let tiles = (w / 512.0).ceil() * (h / 512.0).ceil();
    OPENAI_LOW_DETAIL_TOKENS + OPENAI_TILE_TOKENS * tiles
}

/// `estimateImageTokens(image, apiType)`.
pub fn estimate_image_tokens(image: &NormalizedImage, api_type: &str) -> f64 {
    match api_type.to_lowercase().as_str() {
        "gemini" => GEMINI_IMAGE_TOKENS,
        "messages" => claude_image_tokens(image),
        "chat" | "responses" => openai_image_tokens(image),
        _ => CLAUDE_DEFAULT_IMAGE_TOKENS,
    }
}

fn tokens_for_string_or_json(v: Option<&Value>) -> f64 {
    match v {
        None | Some(Value::Null) => 0.0,
        Some(Value::String(s)) => estimate_tokens(s),
        Some(other) => estimate_tokens(&js::stringify(other)),
    }
}

fn str_opt(v: Option<&Value>) -> Option<String> { v.and_then(Value::as_str).map(str::to_owned) }

fn walk_openai_content(content: Option<&Value>, api_type: &str) -> f64 {
    match content {
        None | Some(Value::Null) => 0.0,
        Some(Value::String(s)) => estimate_tokens(s),
        Some(Value::Array(parts)) => parts.iter().map(|p| {
            if !p.is_object() { return 0.0; }
            match get_str(p, &["type"]) {
                Some("text" | "input_text" | "output_text") => estimate_tokens(get_str(p, &["text"]).unwrap_or("")),
                Some("image_url" | "input_image") => {
                    let img = js::coalesce(p.get("image_url"), Some(p)).unwrap_or(Value::Null);
                    let url = match &img { Value::String(s) => Some(s.clone()), other => str_opt(other.get("url")) };
                    let detail = str_opt(js::coalesce(img.get("detail"), p.get("detail")).as_ref());
                    match url {
                        Some(u) if u.starts_with("data:") => estimate_image_tokens(&NormalizedImage { data_uri: Some(u), detail, ..Default::default() }, api_type),
                        u => estimate_image_tokens(&NormalizedImage { url: u, detail, ..Default::default() }, api_type),
                    }
                }
                _ => tokens_for_string_or_json(Some(p)),
            }
        }).sum(),
        other => tokens_for_string_or_json(other),
    }
}

fn walk_anthropic_content(content: Option<&Value>) -> f64 {
    match content {
        None | Some(Value::Null) => 0.0,
        Some(Value::String(s)) => estimate_tokens(s),
        Some(Value::Array(parts)) => parts.iter().map(|p| {
            if !p.is_object() { return 0.0; }
            match get_str(p, &["type"]) {
                Some("text") => estimate_tokens(get_str(p, &["text"]).unwrap_or("")),
                Some("image") => {
                    let source = js::coalesce(p.get("source"), None).unwrap_or_else(|| Value::Object(Obj::new()));
                    let st = get_str(&source, &["type"]);
                    if st == Some("base64") && source.get("data").is_some_and(Value::is_string) {
                        estimate_image_tokens(&NormalizedImage { base64: str_opt(source.get("data")), media_type: str_opt(source.get("media_type")), ..Default::default() }, "messages")
                    } else if matches!(st, Some("url" | "image_url")) && js::truthy(source.get("url")) {
                        estimate_image_tokens(&NormalizedImage { url: str_opt(source.get("url")), ..Default::default() }, "messages")
                    } else {
                        estimate_image_tokens(&NormalizedImage::default(), "messages")
                    }
                }
                Some("thinking") if p.get("thinking").is_some_and(Value::is_string) => estimate_tokens(get_str(p, &["thinking"]).unwrap_or("")),
                _ => tokens_for_string_or_json(Some(p)),
            }
        }).sum(),
        other => tokens_for_string_or_json(other),
    }
}

fn walk_gemini_part(part: &Value) -> f64 {
    if !part.is_object() { return 0.0; }
    if let Some(t) = get_str(part, &["text"]) { return estimate_tokens(t); }
    if js::truthy(get(part, &["inlineData", "data"])) {
        return estimate_image_tokens(&NormalizedImage { base64: str_opt(get(part, &["inlineData", "data"])), media_type: str_opt(get(part, &["inlineData", "mimeType"])), ..Default::default() }, "gemini");
    }
    if js::truthy(part.get("fileData")) {
        return estimate_image_tokens(&NormalizedImage { url: str_opt(get(part, &["fileData", "fileUri"])), ..Default::default() }, "gemini");
    }
    tokens_for_string_or_json(Some(part))
}

fn walk_messages(messages: Option<&Value>, walker: &dyn Fn(Option<&Value>) -> f64) -> f64 {
    let Some(Value::Array(msgs)) = messages else { return 0.0 };
    msgs.iter().filter(|m| m.is_object()).map(|m| {
        let mut t = walker(m.get("content"));
        if js::truthy(m.get("tool_calls")) { t += tokens_for_string_or_json(m.get("tool_calls")); }
        if let Some(name) = get_str(m, &["name"]).filter(|s| !s.is_empty()) { t += estimate_tokens(name); }
        if let Some(role) = get_str(m, &["role"]) { t += estimate_tokens(role); }
        t
    }).sum()
}

/// `estimateInputTokens(originalBody, apiType)`.
pub fn estimate_input_tokens(body: &Value, api_type: &str) -> f64 {
    if !matches!(body, Value::Object(_) | Value::Array(_)) { return 0.0; }
    let t = api_type.to_lowercase();
    let mut total = 0.0;
    match t.as_str() {
        "chat" => {
            total += walk_messages(body.get("messages"), &|c| walk_openai_content(c, "chat"));
            if js::truthy(body.get("tools")) { total += tokens_for_string_or_json(body.get("tools")); }
        }
        "messages" => {
            total += walk_messages(body.get("messages"), &walk_anthropic_content);
            if !js::nullish(body.get("system")) {
                total += if body["system"].is_array() { walk_anthropic_content(body.get("system")) } else { tokens_for_string_or_json(body.get("system")) };
            }
            if js::truthy(body.get("tools")) { total += tokens_for_string_or_json(body.get("tools")); }
        }
        "gemini" => {
            if let Some(Value::Array(contents)) = body.get("contents") {
                for item in contents.iter().filter(|i| i.is_object()) {
                    if let Some(Value::Array(parts)) = item.get("parts") { total += parts.iter().map(walk_gemini_part).sum::<f64>(); }
                }
            }
            if js::truthy(body.get("systemInstruction")) {
                let sys = &body["systemInstruction"];
                total += match sys {
                    Value::String(s) => estimate_tokens(s),
                    _ => match sys.get("parts") {
                        Some(Value::Array(parts)) => parts.iter().map(walk_gemini_part).sum(),
                        _ => tokens_for_string_or_json(Some(sys)),
                    },
                };
            }
            if js::truthy(body.get("tools")) { total += tokens_for_string_or_json(body.get("tools")); }
        }
        "responses" => {
            match body.get("input") {
                Some(Value::Array(items)) => for it in items.iter().filter(|i| i.is_object()) {
                    total += if it.get("content").is_some() { walk_openai_content(it.get("content"), "responses") } else { tokens_for_string_or_json(Some(it)) };
                },
                Some(Value::String(s)) => total += estimate_tokens(s),
                Some(Value::Null) | None => {}
                other => total += tokens_for_string_or_json(other),
            }
            if !js::nullish(body.get("instructions")) {
                total += match &body["instructions"] { Value::String(s) => estimate_tokens(s), other => tokens_for_string_or_json(Some(other)) };
            }
            if js::truthy(body.get("tools")) { total += tokens_for_string_or_json(body.get("tools")); }
        }
        _ => return estimate_tokens(&js::stringify(body)),
    }
    total
}

fn content_texts(reconstructed: &Value, api_type: &str) -> Option<(String, String)> {
    let (mut output, mut reasoning) = (String::new(), String::new());
    match api_type {
        "chat" => if let Some(Value::Array(choices)) = reconstructed.get("choices") {
            for choice in choices {
                let delta = choice.get("delta").filter(|d| js::truthy(Some(d))).cloned().unwrap_or(Value::Object(Obj::new()));
                if let Some(c) = get_str(&delta, &["content"]) { output.push_str(c); }
                if let Some(r) = get_str(&delta, &["reasoning_content"]) { reasoning.push_str(r); }
                if let Some(Value::Array(calls)) = delta.get("tool_calls") {
                    for call in calls {
                        if js::truthy(get(call, &["function", "arguments"])) { output.push_str(&js::to_string(get(call, &["function", "arguments"]))); }
                    }
                }
            }
        },
        "messages" => if let Some(Value::Array(blocks)) = reconstructed.get("content") {
            for b in blocks {
                match get_str(b, &["type"]) {
                    Some("text") if js::truthy(b.get("text")) => output.push_str(&js::to_string(b.get("text"))),
                    Some("thinking") if js::truthy(b.get("thinking")) => reasoning.push_str(&js::to_string(b.get("thinking"))),
                    Some("thought") if js::truthy(b.get("thought")) => reasoning.push_str(&js::to_string(b.get("thought"))),
                    Some("tool_use") if js::truthy(b.get("input")) => output.push_str(&js::stringify(&b["input"])),
                    _ => {}
                }
            }
        },
        "gemini" => if let Some(Value::Array(cands)) = reconstructed.get("candidates") {
            for c in cands {
                let Some(Value::Array(parts)) = get(c, &["content", "parts"]) else { continue };
                for p in parts {
                    if js::truthy(p.get("text")) {
                        if p.get("thought") == Some(&Value::Bool(true)) { reasoning.push_str(&js::to_string(p.get("text"))); } else { output.push_str(&js::to_string(p.get("text"))); }
                    } else if js::truthy(p.get("functionCall")) {
                        output.push_str(&js::stringify(&p["functionCall"]));
                    }
                }
            }
        },
        "oauth" => {
            if let Some(c) = get_str(reconstructed, &["content"]) { output.push_str(c); }
            if let Some(r) = get_str(reconstructed, &["reasoning_content"]) { reasoning.push_str(r); }
            if let Some(Value::Array(calls)) = reconstructed.get("tool_calls") {
                for call in calls {
                    if js::truthy(get(call, &["function", "arguments"])) { output.push_str(&js::to_string(get(call, &["function", "arguments"]))); }
                }
            }
        }
        _ => return None,
    }
    Some((output, reasoning))
}

/// `estimateTokensFromReconstructed(reconstructed, apiType)` → `(output, reasoning)`.
pub fn estimate_tokens_from_reconstructed(reconstructed: Option<&Value>, api_type: &str) -> (f64, f64) {
    let Some(r) = reconstructed.filter(|r| js::truthy(Some(r))) else { return (0.0, 0.0) };
    match content_texts(r, &api_type.to_lowercase()) {
        Some((o, reasoning)) => (estimate_tokens(&o), estimate_tokens(&reasoning)),
        None => (0.0, 0.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn heuristic_counts() {
        assert_eq!(estimate_tokens(""), 0.0);
        // 11 chars, 1 space (ratio .09 → ×1.1): 2.75*1.1 = 3.025 → 3
        assert_eq!(estimate_tokens_heuristic("hello world"), 3.0);
        // Repetitive input routes to the heuristic: 1000 'a' → 250*1.1*0.9 = 247.5 → 248
        assert_eq!(estimate_tokens(&"a".repeat(1000)), 248.0);
        // Code-ish text gets the code multiplier and bracket weights.
        let code = "function foo(a, b) { return a && b; }";
        assert!(estimate_tokens_heuristic(code) > 10.0);
    }

    #[test]
    fn tokenizer_hook_is_used_for_normal_text() {
        struct Fixed;
        impl TextTokenizer for Fixed { fn count(&self, _: &str) -> Option<usize> { Some(7) } }
        assert_eq!(estimate_tokens_with("The quick brown fox jumps", Some(&Fixed)), 7.0);
        // Repetitive text still bypasses the encoder.
        assert_eq!(estimate_tokens_with(&"a".repeat(1000), Some(&Fixed)), 248.0);
    }

    fn png(w: u32, h: u32) -> String {
        let mut b = vec![0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 13, b'I', b'H', b'D', b'R'];
        b.extend_from_slice(&w.to_be_bytes());
        b.extend_from_slice(&h.to_be_bytes());
        b.extend_from_slice(&[8, 6, 0, 0, 0]);
        use base64::Engine;
        format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(b))
    }

    #[test]
    fn image_dimensions_and_tokens() {
        assert_eq!(image_dimensions_from_data_uri(&png(1024, 768)), Some(ImageDimensions { width: 1024, height: 768 }));
        let gif = [b'G', b'I', b'F', b'8', b'9', b'a', 0x40, 0x01, 0xf0, 0x00, 0, 0];
        assert_eq!(image_dimensions_from_bytes(&gif), Some(ImageDimensions { width: 320, height: 240 }));
        let img = NormalizedImage { data_uri: Some(png(1024, 768)), ..Default::default() };
        // Claude: ceil(1024*768/750) = 1049
        assert_eq!(estimate_image_tokens(&img, "messages"), 1049.0);
        // OpenAI high: 1024x768 → short side 768 → 2x2 tiles → 85 + 170*4
        assert_eq!(estimate_image_tokens(&img, "chat"), 765.0);
        assert_eq!(estimate_image_tokens(&NormalizedImage { detail: Some("low".into()), ..img.clone() }, "chat"), 85.0);
        assert_eq!(estimate_image_tokens(&img, "gemini"), 258.0);
        assert_eq!(estimate_image_tokens(&NormalizedImage::default(), "chat"), 1100.0);
        assert_eq!(estimate_image_tokens(&NormalizedImage::default(), "oauth"), 1600.0);
        let small = NormalizedImage { data_uri: Some(png(256, 256)), detail: Some("auto".into()), ..Default::default() };
        assert_eq!(estimate_image_tokens(&small, "responses"), 85.0);
    }

    #[test]
    fn input_and_reconstructed_estimation() {
        let chat = json!({"messages": [{"role": "user", "content": [{"type": "text", "text": "hello world"}, {"type": "image_url", "image_url": {"url": png(1024, 768)}}]}]});
        assert_eq!(estimate_input_tokens(&chat, "chat"), 3.0 + 765.0 + estimate_tokens("user"));
        let msgs = json!({"system": "hello world", "messages": [{"role": "user", "content": "hello world"}]});
        assert_eq!(estimate_input_tokens(&msgs, "messages"), 6.0 + estimate_tokens("user"));
        assert_eq!(estimate_input_tokens(&json!({"input": "hello world"}), "responses"), 3.0);
        assert_eq!(estimate_input_tokens(&json!("x"), "chat"), 0.0);
        let rec = json!({"choices": [{"delta": {"content": "hello world", "reasoning_content": "hello world"}}]});
        assert_eq!(estimate_tokens_from_reconstructed(Some(&rec), "chat"), (3.0, 3.0));
        assert_eq!(estimate_tokens_from_reconstructed(Some(&rec), "responses"), (0.0, 0.0));
        let gem = json!({"candidates": [{"content": {"parts": [{"text": "hello world", "thought": true}, {"text": "hello world"}]}}]});
        assert_eq!(estimate_tokens_from_reconstructed(Some(&gem), "gemini"), (3.0, 3.0));
    }
}

#[cfg(test)]
mod o200k_tests {
    use super::*;
    #[test]
    fn o200k_counts_like_js_tiktoken() {
        let enc = O200k::load().unwrap();
        assert_eq!(enc.count("Hello world"), Some(2));
        assert_eq!(enc.count("Ünïcödé 😀 日本語 code(){return 1;}"), Some(14));
        assert_eq!(enc.count("  multiple   spaces\n\nnewlines\t"), Some(8));
        assert_eq!(enc.count("x <|endoftext|>"), None);
    }
}
