//! `utils/gemini-malformed-function-call.ts`.
use crate::engine::js::*;
use regex::Regex;
use serde_json::Value;
use std::sync::OnceLock;

pub const GEMINI_MALFORMED_FUNCTION_CALL_CODE: &str = "MALFORMED_FUNCTION_CALL";
pub const GEMINI_MALFORMED_FUNCTION_CALL_MESSAGE: &str =
    "Upstream Gemini returned MALFORMED_FUNCTION_CALL (model emitted a text tool-call leak). This is a transient model defect — please retry your request. [503]";

/// `/(?:print\()?call:\s*default_api[.:]|default_api\.\w+\s*\(/` (JS `\w` is ASCII).
fn leak_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?:print\()?call:\s*default_api[.:]|default_api\.[A-Za-z0-9_]+\s*\(")
            .expect("valid regex")
    })
}

/// `/^(data:\s*)(.*)$/` without the `m`/`s` flags: `.` stops at JS line
/// terminators, so a trailing `\r` makes the line not match.
fn data_line_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new("^(data:\\s*)([^\\r\\n\\u{2028}\\u{2029}]*)$").expect("valid regex")
    })
}

#[derive(Debug, Clone, PartialEq)]
pub struct GeminiMalformedFunctionCall {
    pub code: &'static str,
    pub message: &'static str,
    pub status_code: u16,
    pub text_leak_detected: bool,
}

impl GeminiMalformedFunctionCall {
    /// `{ statusCode, code, message }` as carried on unified chunks/responses.
    pub fn client_error(&self) -> Value {
        serde_json::json!({"statusCode": self.status_code, "code": self.code, "message": self.message})
    }
}

/// `detectGeminiMalformedFunctionCall`.
pub fn detect_gemini_malformed_function_call(
    payload: &Value,
) -> Option<GeminiMalformedFunctionCall> {
    if !matches!(payload, Value::Object(_) | Value::Array(_)) {
        return None;
    }
    let candidate = get(payload, &["candidates", "0"])?;
    if get_str(candidate, &["finishReason"]) != Some(GEMINI_MALFORMED_FUNCTION_CALL_CODE) {
        return None;
    }
    let text_leak_detected = get_arr(candidate, &["content", "parts"]).is_some_and(|parts| {
        parts
            .iter()
            .any(|part| get_str(part, &["text"]).is_some_and(|t| leak_pattern().is_match(t)))
    });
    Some(GeminiMalformedFunctionCall {
        code: GEMINI_MALFORMED_FUNCTION_CALL_CODE,
        message: GEMINI_MALFORMED_FUNCTION_CALL_MESSAGE,
        status_code: 503,
        text_leak_detected,
    })
}

/// `rewriteGeminiMalformedFunctionCallStream`: a line-based pass-through
/// rewriter that stamps `finishMessage` on malformed terminal frames and
/// leaves every other byte untouched. Feed decoded text, then `flush()`;
/// detected defects accumulate in `detected` (the `onDetected` callback).
#[derive(Default)]
pub struct MalformedFunctionCallRewriter {
    buffer: String,
    pub detected: Vec<GeminiMalformedFunctionCall>,
}

impl MalformedFunctionCallRewriter {
    pub fn new() -> Self {
        Self::default()
    }

    fn rewrite_line(&mut self, line: &str) -> String {
        let Some(caps) = data_line_pattern().captures(line) else {
            return line.to_owned();
        };
        let (prefix, body) = (&caps[1], &caps[2]);
        if body == "[DONE]" {
            return line.to_owned();
        }
        let Some(mut payload) = parse(body) else {
            return line.to_owned();
        };
        let Some(defect) = detect_gemini_malformed_function_call(&payload) else {
            return line.to_owned();
        };
        if let Some(c) = payload
            .get_mut("candidates")
            .and_then(|c| c.get_mut(0))
            .and_then(Value::as_object_mut)
        {
            c.insert(
                "finishMessage".into(),
                Value::String(defect.message.to_owned()),
            );
        }
        self.detected.push(defect);
        format!("{prefix}{}", stringify(&payload))
    }

    pub fn feed(&mut self, chunk: &str) -> String {
        self.buffer.push_str(chunk);
        let buffer = std::mem::take(&mut self.buffer);
        let mut lines: Vec<&str> = buffer.split('\n').collect();
        let rest = lines.pop().unwrap_or("").to_owned();
        let mut out = String::new();
        for line in lines {
            out.push_str(&self.rewrite_line(line));
            out.push('\n');
        }
        self.buffer = rest;
        out
    }

    pub fn flush(&mut self) -> String {
        let buffer = std::mem::take(&mut self.buffer);
        if buffer.is_empty() {
            String::new()
        } else {
            self.rewrite_line(&buffer)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rewrite(input: &str) -> (String, usize) {
        let mut r = MalformedFunctionCallRewriter::new();
        let mid = input
            .char_indices()
            .nth(input.chars().count() / 2)
            .map(|(i, _)| i)
            .unwrap_or(0);
        let mut out = r.feed(&input[..mid]);
        out.push_str(&r.feed(&input[mid..]));
        out.push_str(&r.flush());
        (out, r.detected.len())
    }

    #[test]
    fn detection() {
        let d = detect_gemini_malformed_function_call(&json!({"candidates": [{"finishReason": "MALFORMED_FUNCTION_CALL", "content": {"parts": [{"text": "Format repository files with biome formatcall:default_api:bash{command:bun run format}"}]}}]})).unwrap();
        assert_eq!(
            d,
            GeminiMalformedFunctionCall {
                code: "MALFORMED_FUNCTION_CALL",
                message: GEMINI_MALFORMED_FUNCTION_CALL_MESSAGE,
                status_code: 503,
                text_leak_detected: true
            }
        );
        assert!(detect_gemini_malformed_function_call(&json!({"candidates": [{"finishReason": "STOP", "content": {"parts": [{"text": "call:default_api:bash{}"}]}}]})).is_none());
        let no_leak = detect_gemini_malformed_function_call(
            &json!({"candidates": [{"finishReason": "MALFORMED_FUNCTION_CALL"}]}),
        )
        .unwrap();
        assert!(!no_leak.text_leak_detected);
        assert!(detect_gemini_malformed_function_call(&json!({"candidates": [{"finishReason": "MALFORMED_FUNCTION_CALL", "content": {"parts": [{"text": "x default_api.run ("}]}}]})).unwrap().text_leak_detected);
    }

    #[test]
    fn rewriter_preserves_bytes_and_rewrites_malformed() {
        let input = "data: {\"candidates\":[{\"finishReason\":\"STOP\"}]}\n\ndata: [DONE]\n\n";
        assert_eq!(rewrite(input), (input.to_owned(), 0));
        let crlf =
            "data: {\"candidates\":[{\"finishReason\":\"STOP\"}]}\r\n\r\ndata: [DONE]\r\n\r\n";
        assert_eq!(rewrite(crlf), (crlf.to_owned(), 0));
        let payload = json!({"candidates": [{"content": {"parts": [{"text": "call:default_api:bash{command:bun run format}"}]}, "finishReason": "MALFORMED_FUNCTION_CALL", "finishMessage": "Malformed function call: Failed to parse function call"}]});
        let (out, n) = rewrite(&format!("data: {}\n\n", stringify(&payload)));
        assert_eq!(n, 1);
        let rewritten = parse(out["data: ".len()..].trim()).unwrap();
        assert_eq!(
            rewritten["candidates"][0]["finishReason"],
            "MALFORMED_FUNCTION_CALL"
        );
        assert_eq!(
            rewritten["candidates"][0]["finishMessage"],
            GEMINI_MALFORMED_FUNCTION_CALL_MESSAGE
        );
    }
}
