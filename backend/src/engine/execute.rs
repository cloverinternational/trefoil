//! `services/dispatch/upstream-execution.ts`, `utils/fetch-sessions.ts` and
//! `services/probes/stream-probe.ts`: the upstream HTTP call and the
//! pre-commit probe of a streaming response.
use super::js::*;
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use serde_json::{json, Value};
use std::pin::Pin;
use std::time::Duration;

/// Bun's `fetch` sends this user agent when the caller sets none.
pub const DEFAULT_USER_AGENT: &str = "Bun/1.3.6";

pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, String>> + Send>>;

/// Error with a Plexus-style `routingContext` (and the ad-hoc fields the TS
/// attaches to `Error` objects, e.g. `isStreamError`, `cooldownDuration`).
#[derive(Debug, Clone, Default)]
pub struct AttemptError {
    pub message: String,
    pub routing_context: Option<Value>,
    pub is_stream_error: bool,
    pub cooldown_duration: Option<i64>,
    /// `error.code` (Node-style transport codes such as `ECONNREFUSED`).
    pub code: Option<String>,
    pub stack: Option<String>,
}

impl AttemptError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            ..Default::default()
        }
    }
    pub fn with_context(message: impl Into<String>, context: Value) -> Self {
        Self {
            message: message.into(),
            routing_context: Some(context),
            ..Default::default()
        }
    }
    pub fn status_code(&self) -> Option<u16> {
        self.routing_context
            .as_ref()
            .and_then(|c| c.get("statusCode"))
            .and_then(Value::as_u64)
            .map(|s| s as u16)
    }
    pub fn context_str(&self, key: &str) -> Option<&str> {
        self.routing_context
            .as_ref()
            .and_then(|c| c.get(key))
            .and_then(Value::as_str)
    }
    /// `error.stack`: the TS runtime renders `Error: <message>` plus frames;
    /// only the header is reproducible.
    pub fn stack_header(&self) -> String {
        self.stack
            .clone()
            .unwrap_or_else(|| format!("Error: {}", self.message))
    }
}

impl From<super::transformer::EngineError> for AttemptError {
    fn from(e: super::transformer::EngineError) -> Self {
        let mut ctx = serde_json::Map::new();
        if let Some(s) = e.status {
            ctx.insert("statusCode".into(), json!(s));
        }
        if let Some(c) = &e.code {
            ctx.insert("code".into(), json!(c));
        }
        Self {
            message: e.message,
            routing_context: (!ctx.is_empty()).then_some(Value::Object(ctx)),
            ..Default::default()
        }
    }
}

/// `StallConfig` from `services/inspectors/stall-inspector.ts`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StallConfig {
    pub ttfb_ms: Option<i64>,
    pub ttfb_bytes: i64,
    pub min_bps: Option<i64>,
    pub window_ms: i64,
    pub grace_period_ms: i64,
}

pub fn client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .pool_idle_timeout(Duration::from_secs(30))
            .pool_max_idle_per_host(8)
            .build()
            .expect("http client")
    })
}

/// `executeUpstreamRequest`: POST JSON with exactly the given headers.
pub async fn execute_upstream(
    url: &str,
    headers: &[(String, String)],
    payload: &Value,
    timeout: Option<Duration>,
) -> Result<reqwest::Response, AttemptError> {
    let mut req = client().post(url).body(stringify(payload));
    let mut has_ua = false;
    let mut has_ct = false;
    for (k, v) in headers {
        if k.eq_ignore_ascii_case("user-agent") {
            has_ua = true;
        }
        if k.eq_ignore_ascii_case("content-type") {
            has_ct = true;
        }
        req = req.header(k.as_str(), v.as_str());
    }
    if !has_ua {
        req = req.header("user-agent", DEFAULT_USER_AGENT);
    }
    // fetch() with a string body defaults to text/plain;charset=UTF-8.
    if !has_ct {
        req = req.header("content-type", "text/plain;charset=UTF-8");
    }
    if let Some(t) = timeout {
        req = req.timeout(t);
    }
    req.send().await.map_err(transport_error)
}

/// Bun/undici-style transport errors (`fetch failed` with a Node code).
pub fn transport_error(e: reqwest::Error) -> AttemptError {
    if e.is_timeout() {
        return AttemptError::with_context(
            "Upstream timeout",
            json!({"statusCode": 504, "code": "upstream_timeout"}),
        );
    }
    // Bun fetch: refused and unresolvable hosts both reject with code
    // `ConnectionRefused`; a socket closed mid-exchange rejects with ECONNRESET.
    let text = format!("{e:?}").to_lowercase();
    if !e.is_connect()
        && (text.contains("reset")
            || text.contains("closed")
            || text.contains("incomplete")
            || e.is_body()
            || e.is_decode())
    {
        return AttemptError { message: "The socket connection was closed unexpectedly. For more information, pass `verbose: true` in the second argument to fetch()".into(), code: Some("ECONNRESET".into()), ..Default::default() };
    }
    AttemptError {
        message: "Unable to connect. Is the computer able to access the url?".into(),
        code: Some("ConnectionRefused".into()),
        ..Default::default()
    }
}

/// Response headers as a lowercase map (`extractResponseHeaders`).
/// `Object.fromEntries(response.headers.entries())`: Fetch `Headers` iterate
/// sorted by lowercase name with repeated values joined by ", ".
pub fn response_headers(resp: &reqwest::Response) -> Value {
    let mut entries: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    for (k, v) in resp.headers() {
        let v = v.to_str().unwrap_or_default();
        entries
            .entry(k.as_str().to_owned())
            .and_modify(|e| {
                e.push_str(", ");
                e.push_str(v);
            })
            .or_insert_with(|| v.to_owned());
    }
    Value::Object(entries.into_iter().map(|(k, v)| (k, json!(v))).collect())
}

/// `parseInitialStreamError`.
pub fn parse_initial_stream_error(bytes: &[u8]) -> Option<AttemptError> {
    let text = String::from_utf8_lossy(bytes).trim().to_owned();
    if text.is_empty() {
        return None;
    }
    let mut event_type = String::new();
    let mut data_lines: Vec<String> = Vec::new();
    for raw in text.split('\n') {
        let line = raw.trim();
        if let Some(rest) = line.strip_prefix("event:") {
            event_type = rest.trim().to_owned();
        } else if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.trim().to_owned());
        }
    }
    let mut candidates = data_lines.clone();
    if data_lines.len() > 1 {
        candidates.push(data_lines.join("\n"));
    }
    if text.starts_with('{') || text.starts_with('[') {
        candidates.push(text.clone());
    }
    for candidate in candidates {
        if candidate.is_empty() || candidate == "[DONE]" {
            continue;
        }
        let Some(parsed) = parse(&candidate) else {
            continue;
        };
        if !parsed.is_object() && !parsed.is_array() {
            continue;
        }
        let normal_choice = get_arr(&parsed, &["choices"])
            .is_some_and(|c| !c.is_empty() && c[0].get("delta").is_some());
        let normal_anthropic = matches!(
            get_str(&parsed, &["type"]),
            Some(
                "message_start"
                    | "content_block_start"
                    | "content_block_delta"
                    | "message_delta"
                    | "message_stop"
            )
        );
        if normal_choice || normal_anthropic {
            continue;
        }
        let has_error = !nullish(parsed.get("error"));
        let is_error_event = event_type == "error"
            || get_str(&parsed, &["type"]) == Some("error")
            || get_str(&parsed, &["event"]) == Some("error")
            || get_str(&parsed, &["type"]) == Some("response.failed");
        let is_error_status = get_f64(&parsed, &["code"]).is_some_and(|c| c >= 400.0)
            || get_f64(&parsed, &["status"]).is_some_and(|c| c >= 400.0);
        let provider_returned = get_str(&parsed, &["message"]) == Some("Provider returned error")
            || get_str(&parsed, &["error", "message"]) == Some("Provider returned error");
        if !has_error && !is_error_event && !is_error_status && !provider_returned {
            continue;
        }
        let error_obj = match parsed.get("error") {
            Some(e) if e.is_object() => e.clone(),
            _ => parsed.clone(),
        };
        let meta = error_obj
            .get("metadata")
            .filter(|m| truthy(Some(m)))
            .or_else(|| parsed.get("metadata"))
            .cloned();
        let raw_msg = meta
            .as_ref()
            .and_then(|m| m.get("raw"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let message = raw_msg
            .map(str::to_owned)
            .or_else(|| {
                or(error_obj.get("message"), parsed.get("message")).map(|v| to_string(Some(&v)))
            })
            .unwrap_or_else(|| {
                parsed
                    .get("error")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| "Stream error from upstream provider".into())
            });
        let lower = message.to_lowercase();
        let status = if let Some(c) = error_obj.get("code").and_then(Value::as_f64) {
            c
        } else if let Some(c) = parsed.get("code").and_then(Value::as_f64) {
            c
        } else if let Some(c) = error_obj.get("status").and_then(Value::as_f64) {
            c
        } else if let Some(c) = error_obj
            .get("code")
            .and_then(Value::as_str)
            .and_then(|s| s.trim().parse::<f64>().ok())
            .filter(|_| {
                error_obj
                    .get("code")
                    .and_then(Value::as_str)
                    .is_some_and(|s| !s.trim().is_empty())
            })
        {
            c
        } else if lower.contains("rate-limited")
            || lower.contains("rate limit")
            || get_str(&error_obj, &["type"]) == Some("rate_limit_error")
        {
            429.0
        } else {
            502.0
        };
        let mut cooldown = None;
        let retry_sec = meta
            .as_ref()
            .and_then(|m| {
                coalesce(
                    m.get("retry_after_seconds"),
                    m.get("retry_after_seconds_raw"),
                )
            })
            .filter(|v| !v.is_null());
        if let Some(sec) = retry_sec.and_then(|v| {
            v.as_f64()
                .or_else(|| to_string(Some(&v)).trim().parse::<f64>().ok())
        }) {
            if sec.is_finite() && sec > 0.0 {
                cooldown = Some((sec * 1000.0).ceil() as i64);
            }
        }
        if cooldown.is_none() {
            let header = meta
                .as_ref()
                .and_then(|m| m.get("headers"))
                .and_then(|h| or(h.get("Retry-After"), h.get("retry-after")));
            if let Some(sec) = header.and_then(|h| to_string(Some(&h)).trim().parse::<f64>().ok()) {
                if sec.is_finite() && sec > 0.0 {
                    cooldown = Some((sec * 1000.0).ceil() as i64);
                }
            }
        }
        return Some(AttemptError {
            message,
            routing_context: Some(
                json!({"statusCode": num(status), "providerResponse": text, "cooldownTriggered": true}),
            ),
            is_stream_error: true,
            cooldown_duration: cooldown,
            ..Default::default()
        });
    }
    None
}

pub enum Probe {
    Ok {
        status: u16,
        headers: Value,
        stream: ByteStream,
    },
    Failed {
        error: AttemptError,
        stream_started: bool,
    },
}

fn body_stream(resp: reqwest::Response) -> ByteStream {
    Box::pin(resp.bytes_stream().map(|r| r.map_err(|e| e.to_string())))
}

fn replay(prefix: Vec<Bytes>, rest: ByteStream) -> ByteStream {
    Box::pin(futures_util::stream::iter(prefix.into_iter().map(Ok)).chain(rest))
}

/// `probeStreamingStart`.
pub async fn probe_streaming_start(resp: reqwest::Response, stall: Option<&StallConfig>) -> Probe {
    let status = resp.status().as_u16();
    let headers = response_headers(&resp);
    let mut stream = body_stream(resp);
    if let Some(stall) = stall.filter(|s| s.ttfb_ms.is_some()) {
        let ttfb_ms = stall.ttfb_ms.unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(ttfb_ms.max(0) as u64);
        let mut chunks = Vec::new();
        let mut total = 0i64;
        let mut started = false;
        while total < stall.ttfb_bytes {
            match tokio::time::timeout_at(deadline, stream.next()).await {
                Err(_) => {
                    return Probe::Failed { error: AttemptError::new(format!("Stream stalled: TTFB timeout — received {total} bytes in {ttfb_ms}ms (threshold: {} bytes within {ttfb_ms}ms)", stall.ttfb_bytes)), stream_started: started };
                }
                Ok(None) => break,
                Ok(Some(Ok(chunk))) => {
                    total += chunk.len() as i64;
                    chunks.push(chunk);
                    started = true;
                }
                Ok(Some(Err(e))) => {
                    return Probe::Failed {
                        error: AttemptError::new(e),
                        stream_started: started,
                    }
                }
            }
        }
        if !chunks.is_empty() {
            let joined: Vec<u8> = chunks.iter().flat_map(|c| c.iter().copied()).collect();
            if let Some(error) = parse_initial_stream_error(&joined) {
                return Probe::Failed {
                    error,
                    stream_started: false,
                };
            }
        }
        return Probe::Ok {
            status,
            headers,
            stream: replay(chunks, stream),
        };
    }
    match tokio::time::timeout(Duration::from_millis(100), stream.next()).await {
        // No first chunk within 100ms: hand the stream through unprobed.
        Err(_) => Probe::Ok {
            status,
            headers,
            stream,
        },
        Ok(None) => Probe::Ok {
            status,
            headers,
            stream: Box::pin(futures_util::stream::empty()),
        },
        Ok(Some(Err(e))) => Probe::Failed {
            error: AttemptError::new(e),
            stream_started: false,
        },
        Ok(Some(Ok(first))) => {
            // A Bun `reader.read()` returns everything already buffered.
            let mut buf = first.to_vec();
            let mut tail = Vec::new();
            let mut ended = false;
            while let Some(more) = futures_util::FutureExt::now_or_never(stream.next()) {
                match more {
                    Some(Ok(b)) => buf.extend_from_slice(&b),
                    Some(Err(e)) => {
                        tail.push(Err(e));
                        break;
                    }
                    None => {
                        ended = true;
                        break;
                    }
                }
            }
            let first = bytes::Bytes::from(buf);
            if let Some(error) = parse_initial_stream_error(&first) {
                return Probe::Failed {
                    error,
                    stream_started: false,
                };
            }
            let rest: ByteStream = if ended {
                Box::pin(futures_util::stream::iter(tail))
            } else if tail.is_empty() {
                stream
            } else {
                Box::pin(futures_util::stream::iter(tail).chain(stream))
            };
            Probe::Ok {
                status,
                headers,
                stream: replay(vec![first], rest),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn detects_initial_stream_errors() {
        let e = parse_initial_stream_error(b"data: {\"error\":{\"message\":\"Rate limit hit\",\"metadata\":{\"retry_after_seconds\":2}}}\n\n").unwrap();
        assert_eq!(
            (e.message.as_str(), e.status_code(), e.cooldown_duration),
            ("Rate limit hit", Some(429), Some(2000))
        );
        assert!(parse_initial_stream_error(b"data: {\"choices\":[{\"delta\":{}}]}\n\n").is_none());
        assert_eq!(
            parse_initial_stream_error(b"event: error\ndata: {\"code\":503,\"message\":\"down\"}")
                .unwrap()
                .status_code(),
            Some(503)
        );
    }
}
