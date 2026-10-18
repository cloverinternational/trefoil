//! `services/inspectors/stall-inspector.ts`: client-bound throughput
//! monitoring (DISPATCHED → GRACE_PERIOD → MONITORING → THROUGHPUT_STALLED)
//! plus the `UsageStorageService` in-flight registry behind the dashboard's
//! `progress` events (`getProgressUpdates`).
//!
//! Timers are lazy: the stream pump asks [`StallHandle::next_deadline`] when
//! to wake and calls [`StallHandle::poll`], which returns the abort message
//! once a stall is detected (the TS `abortController.abort(...)`).
use crate::engine::execute::StallConfig;
use crate::engine::js::*;
use crate::engine::observability::StallObserver;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};

const MAX_SAMPLES: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq)]
enum State { Dispatched, GracePeriod, Monitoring, ThroughputStalled }

impl State {
    fn as_str(self) -> &'static str {
        match self { State::Dispatched => "DISPATCHED", State::GracePeriod => "GRACE_PERIOD", State::Monitoring => "MONITORING", State::ThroughputStalled => "THROUGHPUT_STALLED" }
    }
}

#[derive(Debug, Clone, Copy)]
struct Sample { timestamp: i64, cumulative: f64, semantic: f64 }

#[derive(Debug)]
struct Core {
    config: StallConfig,
    state: State,
    total: f64,
    semantic: f64,
    start: i64,
    progress_api_type: Option<String>,
    sse_buffer: String,
    samples: Vec<Sample>,
    grace_end: Option<i64>,
    next_check: Option<i64>,
    stalled: Option<String>,
}

/// `DEFAULT_STALL_CONFIG` (the disabled skeleton).
pub fn default_stall_config() -> StallConfig { StallConfig { ttfb_ms: None, ttfb_bytes: 100, min_bps: None, window_ms: 10_000, grace_period_ms: 30_000 } }

impl Core {
    fn is_responses(&self) -> bool { self.progress_api_type.as_deref().is_some_and(|t| t.contains("responses")) }

    fn consume_response_chunk(&mut self, text: &str) {
        static SEP: OnceLock<regex::Regex> = OnceLock::new();
        let sep = SEP.get_or_init(|| regex::Regex::new(r"\r?\n\r?\n").unwrap());
        self.sse_buffer.push_str(text);
        while let Some(m) = sep.find(&self.sse_buffer) {
            let block: String = self.sse_buffer[..m.end()].to_owned();
            self.sse_buffer = self.sse_buffer[m.end()..].to_owned();
            self.consume_response_block(&block);
        }
    }

    fn consume_response_block(&mut self, block: &str) {
        static EVENT: OnceLock<regex::Regex> = OnceLock::new();
        if block.is_empty() { return; }
        let event = EVENT.get_or_init(|| regex::Regex::new(r"(?m)^event:\s*(.+)$").unwrap()).captures(block).map(|c| c[1].trim().to_owned()).filter(|e| !e.is_empty());
        let data: Vec<&str> = block.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).filter(|l| l.starts_with("data:")).map(|l| l[5..].trim_start()).collect();
        let data = data.join("\n");
        let payload_type = if data.is_empty() { None } else { parse(&data).and_then(|p| p.get("type").and_then(Value::as_str).map(str::to_owned)) };
        if event.or(payload_type).is_some_and(|t| t.ends_with(".delta")) { self.semantic += block.len() as f64; }
    }

    /// Grace timer / periodic interval firings that are due by `now`.
    fn advance(&mut self, now: i64) {
        if self.state == State::GracePeriod && self.grace_end.is_some_and(|g| now >= g) { self.on_grace_end(); }
        while self.state == State::Monitoring && self.next_check.is_some_and(|t| now >= t) {
            let at = self.next_check.unwrap();
            self.next_check = Some(at + self.check_interval());
            self.check_throughput(at);
        }
    }

    fn check_interval(&self) -> i64 { 1000.max(self.config.window_ms / 2) }

    fn transition_to_grace(&mut self, now: i64) {
        self.state = State::GracePeriod;
        if self.config.grace_period_ms > 0 { self.grace_end = Some(now + self.config.grace_period_ms); } else { self.on_grace_end_at(now); }
    }

    fn on_grace_end(&mut self) { let at = self.grace_end.unwrap_or_else(now_ms); self.on_grace_end_at(at); }

    fn on_grace_end_at(&mut self, at: i64) {
        if self.state != State::GracePeriod { return; }
        self.state = State::Monitoring;
        self.grace_end = None;
        if self.config.min_bps.is_some() { self.next_check = Some(at + self.check_interval()); }
    }

    fn check_throughput(&mut self, now: i64) {
        let Some(min_bps) = self.config.min_bps else { return };
        let window_start = now - self.config.window_ms;
        while self.samples.len() > 1 && self.samples[0].timestamp < window_start { self.samples.remove(0); }
        if self.samples.len() < 2 { return; }
        let oldest = self.samples[0];
        let span = now - oldest.timestamp;
        if span <= 0 { return; }
        let throughput = (self.total - oldest.cumulative) / span as f64 * 1000.0;
        if throughput < min_bps as f64 {
            self.state = State::ThroughputStalled;
            self.grace_end = None;
            self.next_check = None;
            self.stalled = Some(format!("Stream stalled: throughput {} B/s below threshold {min_bps} B/s", to_fixed0(throughput)));
        }
    }

    fn on_chunk(&mut self, chunk: &str, now: i64) {
        self.advance(now);
        if self.state == State::ThroughputStalled { return; }
        self.total += chunk.len() as f64;
        if self.is_responses() { self.consume_response_chunk(chunk); } else { self.semantic += chunk.len() as f64; }
        self.samples.push(Sample { timestamp: now, cumulative: self.total, semantic: self.semantic });
        if self.samples.len() > MAX_SAMPLES { let drop = self.samples.len() - MAX_SAMPLES; self.samples.drain(..drop); }
        if self.state == State::Dispatched && self.config.min_bps.is_some() { self.transition_to_grace(now); }
        if self.state == State::Monitoring { self.check_throughput(now); }
    }

    fn rate(&self, now: i64, semantic: bool) -> Option<f64> {
        if self.samples.len() < 2 { return None; }
        let window_start = now - self.config.window_ms;
        let mut oldest = self.samples[0];
        for i in 1..self.samples.len() {
            if self.samples[i].timestamp >= window_start { oldest = self.samples[i - 1]; break; }
        }
        let pick = |s: &Sample| if semantic { s.semantic } else { s.cumulative };
        let current = pick(self.samples.last().unwrap());
        let span = now - oldest.timestamp;
        (span > 0).then(|| (current - pick(&oldest)) / span as f64 * 1000.0)
    }

    fn stats(&self, now: i64) -> Value {
        json!({
            "state": self.state.as_str(),
            "bytesReceived": num(self.total),
            "bytesPerSec": self.rate(now, false).map(num).unwrap_or(Value::Null),
            "semanticBytesReceived": num(self.semantic),
            "semanticBytesPerSec": self.rate(now, true).map(num).unwrap_or(Value::Null),
            "elapsedMs": now - self.start,
        })
    }
}

/// `Number.prototype.toFixed(0)`.
fn to_fixed0(x: f64) -> String { format!("{}", (x + 0.5).floor() as i64) }

struct Entry { core: Weak<Mutex<Core>>, api_key: Option<String>, is_streamed: bool }

fn registry() -> &'static Mutex<HashMap<String, Entry>> {
    static R: OnceLock<Mutex<HashMap<String, Entry>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Insertion order of the TS `Map` registry.
fn order() -> &'static Mutex<Vec<String>> {
    static O: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
    O.get_or_init(|| Mutex::new(Vec::new()))
}

/// `registerInFlight(requestId, inspector, apiKey, isStreamed)` metadata half
/// (the inspector itself registered on `setRequestId`).
pub fn register_in_flight(request_id: &str, api_key: Option<&str>, is_streamed: bool) {
    if let Some(e) = registry().lock().unwrap().get_mut(request_id) { e.api_key = api_key.map(str::to_owned); e.is_streamed = is_streamed; }
}

/// `deregisterInFlight(requestId)`.
pub fn deregister_in_flight(request_id: &str) {
    registry().lock().unwrap().remove(request_id);
    order().lock().unwrap().retain(|r| r != request_id);
}

/// `getProgressUpdates()`.
pub fn progress_updates() -> Vec<Value> {
    let now = now_ms();
    let reg = registry().lock().unwrap();
    order().lock().unwrap().iter().filter_map(|rid| {
        let e = reg.get(rid)?;
        let core = e.core.upgrade()?;
        let stats = core.lock().unwrap().stats(now);
        let mut out = json!({"requestId": rid, "apiKey": e.api_key, "isStreamed": e.is_streamed});
        if let (Some(o), Some(s)) = (out.as_object_mut(), stats.as_object()) { for (k, v) in s { o.insert(k.clone(), v.clone()); } }
        Some(out)
    }).collect()
}

/// The pump-side handle of one request's inspector.
#[derive(Clone)]
pub struct StallHandle(Arc<Mutex<Core>>);

impl StallHandle {
    /// Next instant (epoch ms) a timer would fire, if any.
    pub fn next_deadline(&self) -> Option<i64> {
        let c = self.0.lock().unwrap();
        match c.state { State::GracePeriod => c.grace_end, State::Monitoring => c.next_check, _ => None }
    }
    /// Run due timers; `Some(message)` once a throughput stall fired.
    pub fn poll(&self) -> Option<String> {
        let mut c = self.0.lock().unwrap();
        c.advance(now_ms());
        c.stalled.take()
    }
    /// A stall detected while processing a chunk.
    pub fn take_stall(&self) -> Option<String> { self.0.lock().unwrap().stalled.take() }
}

/// `new StallInspector(...)` (+ `updateConfig(merged ?? DEFAULT)`).
pub struct StallInspector { core: Arc<Mutex<Core>> }

impl StallInspector {
    pub fn new(config: StallConfig) -> (Self, StallHandle) {
        let core = Arc::new(Mutex::new(Core {
            config, state: State::Dispatched, total: 0.0, semantic: 0.0, start: now_ms(), progress_api_type: None,
            sse_buffer: String::new(), samples: Vec::new(), grace_end: None, next_check: None, stalled: None,
        }));
        (Self { core: core.clone() }, StallHandle(core))
    }
}

impl StallObserver for StallInspector {
    fn set_request_id(&mut self, request_id: &str) {
        registry().lock().unwrap().insert(request_id.to_owned(), Entry { core: Arc::downgrade(&self.core), api_key: None, is_streamed: false });
        let mut o = order().lock().unwrap();
        o.retain(|r| r != request_id);
        o.push(request_id.to_owned());
    }
    fn set_progress_api_type(&mut self, api_type: &str) {
        let mut c = self.core.lock().unwrap();
        c.progress_api_type = Some(api_type.to_lowercase());
        if c.total == 0.0 && c.is_responses() { c.semantic = 0.0; }
    }
    fn on_chunk(&mut self, chunk: &str) { self.core.lock().unwrap().on_chunk(chunk, now_ms()); }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn cfg(min: i64, grace: i64) -> StallConfig { StallConfig { ttfb_ms: None, ttfb_bytes: 100, min_bps: Some(min), window_ms: 2000, grace_period_ms: grace } }
    #[test]
    fn detects_throughput_stall() {
        let (mut i, h) = StallInspector::new(cfg(1000, 0));
        let mut c = i.core.lock().unwrap();
        c.on_chunk("aaaa", 0);
        assert_eq!(c.state, State::Monitoring);
        c.on_chunk("bbbb", 1000);
        assert_eq!(c.state, State::ThroughputStalled);
        assert_eq!(c.stalled.as_deref(), Some("Stream stalled: throughput 4 B/s below threshold 1000 B/s"));
        drop(c);
        let _ = (&mut i, h);
    }
    #[test]
    fn periodic_check_catches_silence() {
        let (i, _h) = StallInspector::new(cfg(100, 100));
        let mut c = i.core.lock().unwrap();
        c.on_chunk(&"x".repeat(100), 0);
        c.on_chunk(&"x".repeat(100), 50);
        assert_eq!(c.state, State::GracePeriod);
        c.advance(100);
        assert_eq!((c.state, c.next_check), (State::Monitoring, Some(1100)));
        c.advance(1099);
        assert_eq!(c.state, State::Monitoring);
        c.advance(1100);
        assert_eq!(c.state, State::ThroughputStalled);
    }
    #[test]
    fn responses_counts_delta_blocks() {
        let (mut i, _h) = StallInspector::new(default_stall_config());
        i.set_progress_api_type("responses");
        i.on_chunk("event: response.output_text.delta\ndata: {}\n\nevent: response.completed\ndata: {}\n\n");
        let c = i.core.lock().unwrap();
        assert_eq!(c.semantic, "event: response.output_text.delta\ndata: {}\n\n".len() as f64);
    }
}
