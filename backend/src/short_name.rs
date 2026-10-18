//! Port of Plexus `services/models/short-name.ts` (`deriveShortName`, PR #999).
//!
//! Deterministic short-name derivation for model display names: a compact
//! label (`Sonnet 5.5`, `DS 4.1 F`, `GPT 6.1 Sol Pro`) from the OpenRouter
//! (`Vendor: Name …`), models.dev (bare names, `(latest)` tags) and OAuth /
//! pi-ai (bare kebab-case ids) naming dialects.
//!
//! JS semantics are reproduced exactly: the upstream regexes carry no `u`
//! flag, so `\d`, `\b` and `/i` are ASCII-only, while `\s` and `trim()` use
//! the ECMAScript whitespace set (which differs from Rust's `White_Space` by
//! U+FEFF / U+0085). `token[0].toUpperCase()` works on a UTF-16 code unit, so
//! a token starting with an astral character keeps its case. The plain-object
//! `SLUG_ACRONYMS[token]` lookup also hits `Object.prototype.constructor`.
use std::sync::OnceLock;

use regex::Regex;

use crate::engine::responses::js_ext::{is_js_whitespace, js_trim};

/// ECMAScript `\s` (WhiteSpace + LineTerminator), as a class body.
const WS: &str = r"\t\n\x0B\x0C\r \x{A0}\x{1680}\x{2000}-\x{200A}\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}\x{FEFF}";

const MISTRAL_DISTINCT_FAMILIES: [&str; 10] =
    ["codestral", "devstral", "ministral", "mixtral", "voxtral", "magistral", "saba", "nemo", "pixtral", "leanstral"];

/// `String(Object)` under Bun/JavaScriptCore: what `SLUG_ACRONYMS['constructor']`
/// stringifies to when joined.
const OBJECT_CONSTRUCTOR: &str = "function Object() {\n    [native code]\n}";

macro_rules! re {
    ($($arg:tt)*) => {{
        static RE: OnceLock<Regex> = OnceLock::new();
        RE.get_or_init(|| Regex::new(&format!($($arg)*)).unwrap())
    }};
}

fn ascii_digits(s: &str) -> bool { !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) }

fn has_digit(s: &str) -> bool { s.bytes().any(|b| b.is_ascii_digit()) }

/// `/^word$/i` without the `u` flag.
fn is_word(token: &str, word: &str) -> bool { token.eq_ignore_ascii_case(word) }

/// `/\s+/g` → `' '`.
fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_ws = false;
    for c in s.chars() {
        if is_js_whitespace(c) {
            if !in_ws { out.push(' '); }
            in_ws = true;
        } else {
            out.push(c);
            in_ws = false;
        }
    }
    out
}

/// `.split(/[\s\-_]+/).filter(Boolean)`.
fn split_tokens(s: &str) -> Vec<String> {
    s.split(|c: char| is_js_whitespace(c) || c == '-' || c == '_').filter(|t| !t.is_empty()).map(str::to_owned).collect()
}

/// `looksLikeSlug`.
fn looks_like_slug(value: &str) -> bool {
    if value.contains(' ') { return false; }
    value.contains(['/', '.', '_']) || (value == value.to_lowercase() && value.contains('-'))
}

fn slug_acronym(lower: &str) -> Option<&'static str> {
    Some(match lower {
        "gpt" => "GPT",
        "glm" => "GLM",
        "chatgpt" => "ChatGPT",
        "oss" => "OSS",
        "api" => "API",
        "ai" => "AI",
        "llm" => "LLM",
        "tts" => "TTS",
        "constructor" => OBJECT_CONSTRUCTOR,
        _ => return None,
    })
}

/// `` `${token[0]?.toUpperCase()}${token.slice(1)}` `` for a non-empty token.
fn upper_first(token: &str) -> String {
    let mut chars = token.chars();
    match chars.next() {
        Some(first) if (first as u32) <= 0xFFFF => first.to_uppercase().collect::<String>() + chars.as_str(),
        _ => token.to_owned(),
    }
}

/// `/^(\d[\d.]*[bmtk]|[a-z]\d+[bmtk])$/i`.
fn is_size_token(token: &str) -> bool {
    let b = token.as_bytes();
    let n = b.len();
    if n < 2 || !matches!(b[n - 1].to_ascii_lowercase(), b'b' | b'm' | b't' | b'k') { return false; }
    let mid = &b[1..n - 1];
    (b[0].is_ascii_digit() && mid.iter().all(|c| c.is_ascii_digit() || *c == b'.'))
        || (b[0].is_ascii_alphabetic() && !mid.is_empty() && mid.iter().all(u8::is_ascii_digit))
}

/// `humanizeSlug`.
fn humanize_slug(value: &str) -> String {
    let mut segment = value.rsplit('/').next().unwrap_or(value);
    if let Some((first, rest)) = segment.split_once('.') {
        if !first.is_empty() && first.bytes().all(|b| b.is_ascii_alphabetic()) && rest.contains(|c: char| c == '-' || c.is_ascii_digit()) {
            segment = rest;
        }
    }
    let mut tokens: Vec<String> = Vec::new();
    for token in segment.split(['-', '_']).filter(|t| !t.is_empty()) {
        match tokens.last_mut() {
            Some(prev) if ascii_digits(prev) && ascii_digits(token) => { prev.push('.'); prev.push_str(token); }
            _ => tokens.push(token.to_owned()),
        }
    }
    tokens.iter().map(|token| {
        let token = upper_first(token);
        if let Some(acronym) = slug_acronym(&token.to_lowercase()) { return acronym.to_owned(); }
        if is_size_token(&token) { return token.to_ascii_uppercase(); }
        token
    }).collect::<Vec<_>>().join(" ")
}

/// `hasTechnicalImageSignal`.
fn has_technical_image_signal(tokens: &[String]) -> bool {
    let joined = tokens.join(" ").to_lowercase();
    has_digit(&joined) && ["gemini", "gpt", "image", "flash", "pro", "claude", "opus", "sonnet", "haiku"].iter().any(|w| joined.contains(w))
}

/// `deriveShortName(name, modelId)`. `None` mirrors `undefined`; callers also
/// treat `Some("")` (the `return trimmedName` fallbacks) as unset.
pub fn derive_short_name(name: Option<&str>, model_id: Option<&str>) -> Option<String> {
    let trimmed_name = js_trim(name.unwrap_or_default());
    let trimmed_id = js_trim(model_id.unwrap_or_default());

    let mut working = if trimmed_name.is_empty() { trimmed_id.to_owned() } else { trimmed_name.to_owned() };
    // `/:[a-z0-9-]+$/i` on spaceless slugs (OpenRouter `:variant` suffixes).
    if !working.is_empty() && !working.contains(' ') && looks_like_slug(&working) {
        if let Some(i) = working.rfind(':') {
            let suffix = &working[i + 1..];
            if !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') { working.truncate(i); }
        }
    }
    if !working.is_empty() && looks_like_slug(&working) { working = humanize_slug(&working); }
    if working.is_empty() { return None; }

    let working_s = re!(r":(?i-u:batch)[{WS}]*$").replace(&working, "").into_owned();
    let working_s = re!(r"[{WS}]*\((?i-u:batch|free)\)[{WS}]*$").replace(&working_s, "").into_owned();
    let mut working = js_trim(&working_s).to_owned();
    if working.is_empty() { return Some(trimmed_name.to_owned()); }

    let mut vendor = String::new();
    if let Some(c) = re!(r"^([^:]+):[{WS}]+([^\n\r\x{{2028}}\x{{2029}}]+)$").captures(&working) {
        vendor = js_trim(&c[1]).to_owned();
        working = js_trim(&c[2]).to_owned();
    }
    let vendor_is_deepseek = vendor.to_ascii_lowercase().contains("deepseek");

    let banana = re!(r"^((?i-u:nano)[{WS}]+(?i-u:banana)[^()]*)\(([^()]*)\)[{WS}]*$").captures(&working)
        .map(|c| (c[1].to_owned(), c[2].to_owned()));
    if let Some((label, inner)) = banana {
        let inner = js_trim(&inner).to_owned();
        working = if has_technical_image_signal(&split_tokens(&inner)) { inner } else { js_trim(&label).to_owned() };
    } else if !trimmed_id.is_empty() && !working.contains('(') {
        let brand_only = re!(r"^(?i-u:nano)[{WS}]+(?i-u:banana)").find(&working).is_some_and(|m| {
            !working[m.end()..].chars().next().is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
        });
        if brand_only {
            let id_tokens = split_tokens(&humanize_slug(trimmed_id));
            if has_technical_image_signal(&id_tokens) { working = id_tokens.join(" "); }
        }
    }

    let triaged = re!(r"\(([^()]*)\)").replace_all(&working, |c: &regex::Captures| {
        let inner = js_trim(&c[1]);
        let lower = inner.to_lowercase();
        if matches!(lower.as_str(), "latest" | "batch" | "free") { return String::new(); }
        if !inner.is_empty() && inner.chars().all(|ch| ch.is_ascii_digit() || is_js_whitespace(ch) || matches!(ch, '-' | '/' | '.' | ',' | ':')) { return String::new(); }
        if is_word(inner, "thinking") { return String::new(); }
        if re!(r"(?i-u:non)[-{WS}]?(?i-u:reasoning)").is_match(inner) { return "(NR)".into(); }
        if inner.to_ascii_lowercase().contains("reasoning") { return "(R)".into(); }
        c[0].to_owned()
    }).into_owned();
    working = js_trim(&collapse_ws(&triaged)).to_owned();
    if working.is_empty() { return Some(trimmed_name.to_owned()); }

    let deepseek_prefix = re!(r"^(?i-u:deepseek)[{WS}]+");
    let is_deepseek = vendor_is_deepseek || deepseek_prefix.is_match(&working);
    if is_deepseek {
        working = format!("DS {}", deepseek_prefix.replace(&working, ""));
    }

    let mut tokens: Vec<String> = split_tokens(&working).into_iter()
        .filter(|t| !is_word(t, "thinking") && !is_word(t, "instruct"))
        .map(|t| {
            if is_word(&t, "flash") { return "F".into(); }
            if is_word(&t, "nano") { return "Nano".into(); }
            if is_deepseek && is_word(&t, "pro") { return "P".into(); }
            let b = t.as_bytes();
            if is_deepseek && b.len() >= 2 && (b[0] == b'v' || b[0] == b'V') && b[1].is_ascii_digit() { return t[1..].to_owned(); }
            t
        })
        .collect();

    loop {
        if tokens.len() <= 1 { break; }
        let last = tokens.last().unwrap();
        if is_word(last, "latest") || (last.len() >= 3 && last.len() <= 4 && ascii_digits(last) && tokens[..tokens.len() - 1].iter().any(|t| has_digit(t))) {
            tokens.pop();
        } else {
            break;
        }
    }

    if tokens.len() > 1 && is_word(&tokens[0], "claude") { tokens.remove(0); }
    if tokens.len() > 1 && is_word(&tokens[0], "muse") { tokens.remove(0); }
    let distinct = |t: &str| MISTRAL_DISTINCT_FAMILIES.contains(&t.to_lowercase().as_str());
    if tokens.len() > 2 && is_word(&tokens[0], "open") && is_word(&tokens[1], "mistral") && distinct(&tokens[2]) {
        tokens.drain(..2);
    } else if tokens.len() > 1 && is_word(&tokens[0], "mistral") && distinct(&tokens[1]) {
        tokens.remove(0);
    }
    if tokens.len() > 1 && is_word(&tokens[0], "nano") && tokens[1].as_bytes().get(..6).is_some_and(|p| p.eq_ignore_ascii_case(b"banana")) {
        tokens[0] = "Banana".into();
        tokens.remove(1);
    }

    let result = js_trim(&collapse_ws(&tokens.join(" "))).to_owned();
    Some(if result.is_empty() { trimmed_name.to_owned() } else { result })
}
