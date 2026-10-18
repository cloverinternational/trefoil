//! `services/observability/debug-manager.ts` (`DebugManager`) and
//! `services/inspectors/debug-logging.ts` (`DebugLoggingInspector` stream
//! reconstruction), backed by Trefoil's debug toggles in `AppState` and the
//! `debug_logs` table.
use crate::engine::js::*;
use crate::engine::observability::costs::{provider_cost_from_sse_comments, provider_energy_from_sse_comments};
use crate::engine::observability::DebugHooks;
use crate::state::AppState;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};

tokio::task_local! {
    /// `setCurrentRequestId` request context (read by the cooldown path).
    pub static REQUEST_ID: Option<String>;
}

const MAX_DEBUG_BUFFER_SIZE: usize = 10 * 1024 * 1024;

#[derive(Debug, Clone, Default)]
struct DebugLog {
    api_key: Option<String>,
    model_alias: Option<String>,
    raw_request: Option<Value>,
    transformed_request: Option<Value>,
    raw_response: Option<Value>,
    transformed_response: Option<Value>,
    raw_response_snapshot: Option<Value>,
    transformed_response_snapshot: Option<Value>,
    request_headers: Option<Value>,
    response_headers: Option<Value>,
    response_status: Option<u16>,
    provider: Option<String>,
    created_at: i64,
    defer_payload_capture: bool,
    force_persist: bool,
}

#[derive(Default)]
struct Tap { chunks: Vec<String>, total: usize, truncated: bool, finalized: bool }

#[derive(Default)]
struct Store {
    pending: HashMap<String, DebugLog>,
    ephemeral: HashSet<String>,
    /// `(requestId, 'raw' | 'transformed')` capture buffers.
    taps: HashMap<(String, &'static str), Tap>,
}

fn store() -> &'static Mutex<Store> {
    static S: OnceLock<Mutex<Store>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(Store::default()))
}

fn current_key_name() -> Option<String> { super::engine::KEY_NAME.try_with(|k| k.clone()).ok().flatten() }

/// `DebugManager.getInstance()` over Trefoil's toggles.
#[derive(Clone)]
pub struct DebugManager { pub state: AppState }

impl DebugManager {
    pub fn new(state: &AppState) -> Self { Self { state: state.clone() } }

    fn enabled_global(&self) -> bool { *self.state.inner.debug_enabled_global.read().unwrap() }
    fn capture_on_error(&self) -> bool { self.state.inner.settings.read().unwrap().get("debug.captureOnError").and_then(Value::as_bool).unwrap_or(false) }
    fn key_dim(&self, key: Option<&str>) -> bool { key.is_some_and(|k| !k.is_empty() && self.state.inner.debug_enabled_keys.read().unwrap().contains(k)) }
    fn alias_dim(&self, alias: Option<&str>) -> bool { alias.is_some_and(|a| !a.is_empty() && self.state.inner.debug_enabled_aliases.read().unwrap().contains(a)) }
    fn provider_dim(&self, p: Option<&str>) -> bool { p.is_some_and(|p| !p.is_empty() && self.state.inner.debug_enabled_providers.read().unwrap().contains(p)) }
    fn aliases_any(&self) -> bool { !self.state.inner.debug_enabled_aliases.read().unwrap().is_empty() }
    fn providers_any(&self) -> bool { !self.state.inner.debug_enabled_providers.read().unwrap().is_empty() }

    /// `isCaptureEnabled()`.
    fn is_capture_enabled(&self) -> bool {
        self.capture_on_error() || self.enabled_global() || self.key_dim(current_key_name().as_deref()) || self.aliases_any() || self.providers_any()
    }

    fn should_persist(&self, log: &DebugLog) -> bool {
        self.enabled_global() || self.key_dim(log.api_key.as_deref()) || self.alias_dim(log.model_alias.as_deref()) || self.provider_dim(log.provider.as_deref()) || log.force_persist
    }

    /// `startLog(requestId, rawRequest, sanitizeHeaders(headers))`.
    pub fn start_log(&self, request_id: &str, raw_request: Option<&Value>, headers: &[(String, String)]) {
        let alias = raw_request.and_then(|r| r.get("model")).and_then(Value::as_str).map(str::to_owned);
        let key = current_key_name();
        let capture = self.capture_on_error() || self.enabled_global() || self.key_dim(key.as_deref()) || self.providers_any() || self.alias_dim(alias.as_deref());
        let defer = !capture && self.aliases_any() && !self.providers_any();
        if !capture && !defer { return; }
        let log = DebugLog {
            api_key: key, model_alias: alias,
            raw_request: if capture { raw_request.cloned() } else { None },
            request_headers: if capture { Some(sanitize(headers)) } else { None },
            created_at: now_ms(), defer_payload_capture: defer, ..Default::default()
        };
        store().lock().unwrap().pending.insert(request_id.to_owned(), log);
        let (this, rid) = (self.clone(), request_id.to_owned());
        if let Ok(h) = tokio::runtime::Handle::try_current() {
            h.spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(300)).await;
                if store().lock().unwrap().pending.contains_key(&rid) { this.flush(&rid); }
            });
        }
    }

    fn with_payload_log(&self, request_id: &str, f: impl FnOnce(&mut DebugLog)) {
        if !self.is_capture_enabled() { return; }
        let mut s = store().lock().unwrap();
        if s.pending.get(request_id).is_some_and(|l| l.defer_payload_capture) { return; }
        f(ensure(&mut s, request_id));
    }

    /// `addTransformedRequest(requestId, payload)`.
    pub fn add_transformed_request(&self, request_id: &str, payload: &Value) { self.with_payload_log(request_id, |l| l.transformed_request = Some(payload.clone())); }
    /// `addRawResponse(requestId, payload)`.
    pub fn add_raw_response(&self, request_id: &str, payload: Value) { self.with_payload_log(request_id, |l| l.raw_response = Some(payload)); }
    /// `addResponseMeta(requestId, status, headers)`.
    pub fn add_response_meta(&self, request_id: &str, status: u16, headers: &Value) {
        self.with_payload_log(request_id, |l| { l.response_status = Some(status); l.response_headers = Some(headers.clone()); });
    }
    /// `addReconstructedRawResponse(requestId, payload)` (always kept in memory).
    pub fn add_reconstructed_raw_response(&self, request_id: &str, payload: Value) {
        ensure(&mut store().lock().unwrap(), request_id).raw_response_snapshot = Some(payload);
    }
    fn add_transformed_snapshot(&self, request_id: &str, payload: Value) {
        ensure(&mut store().lock().unwrap(), request_id).transformed_response_snapshot = Some(payload);
    }

    /// `markForcePersist(getCurrentRequestId())` from the cooldown path.
    pub fn mark_force_persist_current(&self) {
        if let Some(rid) = REQUEST_ID.try_with(|r| r.clone()).ok().flatten() { DebugHooks::mark_force_persist(self, &rid); }
    }

    fn save(&self, request_id: &str, log: DebugLog) {
        let Some(pool) = self.state.inner.database.clone() else { return };
        let ser = |v: &Option<Value>| -> Option<String> {
            match v { None | Some(Value::Null) => None, Some(Value::String(s)) => if s.is_empty() { None } else { Some(s.clone()) }, Some(Value::Bool(false)) => None, Some(other) => Some(stringify(other)) }
        };
        let rid = request_id.to_owned();
        let cols = [ser(&log.raw_request), ser(&log.transformed_request), ser(&log.raw_response), ser(&log.transformed_response), ser(&log.raw_response_snapshot), ser(&log.transformed_response_snapshot), ser(&log.request_headers), ser(&log.response_headers)];
        let created = if log.created_at != 0 { log.created_at } else { now_ms() };
        tokio::spawn(async move {
            let _ = sqlx::query("INSERT INTO debug_logs (request_id, api_key, raw_request, transformed_request, raw_response, transformed_response, raw_response_snapshot, transformed_response_snapshot, request_headers, response_headers, response_status, created_at) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)")
                .bind(rid).bind(log.api_key).bind(cols[0].clone()).bind(cols[1].clone()).bind(cols[2].clone()).bind(cols[3].clone()).bind(cols[4].clone()).bind(cols[5].clone()).bind(cols[6].clone()).bind(cols[7].clone())
                .bind(log.response_status.map(|s| s as i32)).bind(created).execute(&pool).await;
        });
    }

    fn finalize_tap(&self, request_id: &str, mode: &'static str, api_type: &str) {
        let body = {
            let mut s = store().lock().unwrap();
            let tap = s.taps.entry((request_id.to_owned(), mode)).or_default();
            if tap.finalized { return; }
            tap.finalized = true;
            let body = tap.chunks.join("");
            s.taps.remove(&(request_id.to_owned(), mode));
            body
        };
        let snapshot = reconstruct(api_type, &body);
        if mode == "raw" {
            self.add_reconstructed_raw_response(request_id, snapshot.unwrap_or(Value::Null));
            self.add_raw_response(request_id, Value::String(body));
        } else {
            self.add_transformed_snapshot(request_id, snapshot.unwrap_or(Value::Null));
            self.with_payload_log(request_id, |l| l.transformed_response = Some(Value::String(body)));
        }
    }

    fn capture(&self, request_id: &str, mode: &'static str, text: &str) {
        let mut s = store().lock().unwrap();
        let tap = s.taps.entry((request_id.to_owned(), mode)).or_default();
        if tap.finalized || tap.truncated { return; }
        if tap.total + text.encode_utf16().count() > MAX_DEBUG_BUFFER_SIZE {
            tap.truncated = true;
            tap.chunks.push("\n\n[DEBUG OUTPUT TRUNCATED - Exceeded 10MB limit]".into());
            return;
        }
        tap.total += text.encode_utf16().count();
        tap.chunks.push(text.to_owned());
    }
}

fn ensure<'a>(s: &'a mut Store, request_id: &str) -> &'a mut DebugLog {
    s.pending.entry(request_id.to_owned()).or_insert_with(|| DebugLog { api_key: current_key_name(), created_at: now_ms(), ..Default::default() })
}

/// `sanitizeHeaders(request.headers)`.
pub fn sanitize(headers: &[(String, String)]) -> Value {
    let mut map = Obj::new();
    for (k, v) in headers {
        match map.get_mut(k) {
            Some(Value::String(existing)) if k == "set-cookie" => { let prev = existing.clone(); map.insert(k.clone(), json!([prev, v])); }
            Some(Value::Array(a)) => a.push(json!(v)),
            Some(Value::String(existing)) => { let joined = format!("{existing}, {v}"); map.insert(k.clone(), json!(joined)); }
            _ => { map.insert(k.clone(), json!(v)); }
        }
    }
    Value::Object(crate::engine::dispatch::errors::sanitize_headers(&map))
}

impl DebugHooks for DebugManager {
    fn set_provider_for_request(&self, request_id: &str, provider: &str) {
        if let Some(l) = store().lock().unwrap().pending.get_mut(request_id) { l.provider = Some(provider.to_owned()); }
    }
    fn set_model_alias_for_request(&self, request_id: &str, alias: Option<&str>, original_request: Option<&Value>, headers: &[(String, String)]) {
        let Some(alias) = alias.filter(|a| !a.is_empty()) else { return };
        let alias_dim = self.alias_dim(Some(alias));
        if let Some(l) = store().lock().unwrap().pending.get_mut(request_id) {
            l.model_alias = Some(alias.to_owned());
            if l.defer_payload_capture && alias_dim {
                l.defer_payload_capture = false;
                l.raw_request = original_request.cloned();
                l.request_headers = Some(sanitize(headers));
            }
        }
    }
    fn is_enabled(&self) -> bool { self.enabled_global() }
    fn set_enabled(&self, enabled: bool) {
        *self.state.inner.debug_enabled_global.write().unwrap() = enabled;
        crate::system_logs::event(&self.state, "warn", format!("Debug mode (global) {}", if enabled { "enabled" } else { "disabled" }), [("module".into(), json!("debug-manager")), ("functionName".into(), json!("setEnabled"))]);
    }
    fn mark_ephemeral(&self, request_id: &str) { store().lock().unwrap().ephemeral.insert(request_id.to_owned()); }
    fn discard_ephemeral(&self, request_id: &str) {
        let mut s = store().lock().unwrap();
        if !s.ephemeral.contains(request_id) { return; }
        if s.pending.get(request_id).is_some_and(|l| self.should_persist(l)) { return; }
        s.pending.remove(request_id);
        s.ephemeral.remove(request_id);
    }
    fn add_transformed_response(&self, request_id: &str, body: &Value) { self.with_payload_log(request_id, |l| l.transformed_response = Some(body.clone())); }
    fn flush(&self, request_id: &str) {
        let log = {
            let mut s = store().lock().unwrap();
            let Some(log) = s.pending.get(request_id).cloned() else { return };
            if !self.should_persist(&log) {
                s.pending.remove(request_id);
                s.ephemeral.remove(request_id);
                return;
            }
            if self.state.inner.database.is_none() { return; }
            s.pending.remove(request_id);
            s.ephemeral.remove(request_id);
            log
        };
        self.save(request_id, log);
    }
    fn mark_force_persist(&self, request_id: &str) {
        if !self.capture_on_error() || request_id.is_empty() { return; }
        if let Some(l) = store().lock().unwrap().pending.get_mut(request_id) { l.force_persist = true; }
    }
    fn reconstructs(&self) -> bool { true }
    fn reconstructed_raw_response(&self, request_id: &str) -> Option<Value> {
        store().lock().unwrap().pending.get(request_id).and_then(|l| l.raw_response_snapshot.clone()).filter(|v| truthy(Some(v)))
    }
    fn transformed_response_snapshot(&self, request_id: &str) -> Option<Value> {
        store().lock().unwrap().pending.get(request_id).and_then(|l| l.transformed_response_snapshot.clone())
    }
    fn raw_chunk(&self, request_id: &str, _provider_api_type: &str, text: &str) { self.capture(request_id, "raw", text); }
    fn raw_end(&self, request_id: &str, provider_api_type: &str) { self.finalize_tap(request_id, "raw", provider_api_type); }
    fn transformed_chunk(&self, request_id: &str, _api_type: &str, text: &str) { self.capture(request_id, "transformed", text); }
    fn transformed_end(&self, request_id: &str, api_type: &str) { self.finalize_tap(request_id, "transformed", api_type); }
}

// ── DebugLoggingInspector reconstruction ──────────────────────────────────

fn reconstruct(api_type: &str, body: &str) -> Option<Value> {
    match api_type {
        "chat" | "bedrock-converse" => reconstruct_with(body, true, update_chat),
        "responses" => reconstruct_with(body, true, update_responses),
        "messages" => reconstruct_with(body, false, update_messages),
        "gemini" => reconstruct_with(body, true, update_gemini),
        "oauth" => Some(reconstruct_oauth(body)),
        _ => None,
    }
}

fn attach_comments(snapshot: &mut Value, body: &str) {
    if let Some(m) = snapshot.as_object_mut() {
        if let Some(c) = provider_cost_from_sse_comments(body) { m.insert("providerReportedCost".into(), c); }
        if let Some(e) = provider_energy_from_sse_comments(body) { m.insert("providerReportedEnergy".into(), e); }
    }
}

fn reconstruct_with(body: &str, skip_done: bool, update: fn(Option<Value>, Value) -> Option<Option<Value>>) -> Option<Value> {
    let trimmed = body.trim();
    if trimmed.is_empty() { return None; }
    if trimmed.starts_with('{') && trimmed.ends_with('}') {
        if let Some(mut parsed) = parse(trimmed) { attach_comments(&mut parsed, body); return Some(parsed); }
    }
    let mut snapshot: Option<Value> = None;
    for line in body.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)) {
        let Some(rest) = line.strip_prefix("data:") else { continue };
        let json_str = rest.trim();
        if json_str.is_empty() || (skip_done && json_str == "[DONE]") { continue; }
        let Some(chunk) = parse(json_str) else { continue };
        // `None` = the TS update threw (chunk skipped, snapshot kept).
        if let Some(next) = update(snapshot.clone(), chunk) { snapshot = next; }
    }
    if let Some(s) = snapshot.as_mut() { attach_comments(s, body); }
    snapshot
}

fn spread_obj(v: &Value) -> Obj { v.as_object().cloned().unwrap_or_default() }

/// Upper bound on a stream-supplied array index during reconstruction. JS
/// assigns sparse indices, but padding a Vec up to an arbitrary upstream index
/// would allocate without bound, so such a chunk is skipped like a TS throw.
const MAX_RECONSTRUCT_INDEX: usize = 4096;

fn set_index(arr: &mut Vec<Value>, idx: usize, v: Value) -> Option<()> {
    if idx > MAX_RECONSTRUCT_INDEX { return None; }
    while arr.len() <= idx { arr.push(Value::Null); }
    arr[idx] = v;
    Some(())
}

fn update_chat(acc: Option<Value>, chunk: Value) -> Option<Option<Value>> {
    let Some(acc) = acc else { return Some(Some(Value::Object(spread_obj(&chunk)))) };
    let mut result = spread_obj(&acc);
    for (k, v) in spread_obj(&chunk) { result.insert(k, v); }
    if let Some(choices) = chunk.get("choices").filter(|c| truthy(Some(c))) {
        let mut out: Vec<Value> = acc.get("choices").and_then(Value::as_array).cloned().unwrap_or_default();
        for cc in choices.as_array().cloned().unwrap_or_default() {
            let idx = cc.get("index").filter(|v| !v.is_null()).and_then(Value::as_u64).unwrap_or(0) as usize;
            if !truthy(out.get(idx)) { set_index(&mut out, idx, json!({"index": idx, "delta": {}}))?; }
            let Some(delta) = cc.get("delta").filter(|d| truthy(Some(d))) else { continue };
            let acc_choice = out[idx].as_object_mut()?;
            let acc_delta = acc_choice.get_mut("delta").and_then(Value::as_object_mut)?;
            if truthy(delta.get("role")) { acc_delta.insert("role".into(), delta["role"].clone()); }
            for key in ["content", "reasoning", "reasoning_content", "refusal"] {
                if let Some(s) = delta.get(key).and_then(Value::as_str) {
                    let prev = acc_delta.get(key).filter(|v| truthy(Some(v))).map(|v| to_string(Some(v))).unwrap_or_default();
                    acc_delta.insert(key.into(), json!(prev + s));
                }
            }
            if truthy(cc.get("finish_reason")) { acc_choice.insert("finish_reason".into(), cc["finish_reason"].clone()); }
            if truthy(cc.get("usage")) && !truthy(result.get("usage")) { result.insert("usage".into(), cc["usage"].clone()); }
            let acc_delta = out[idx].get_mut("delta").and_then(Value::as_object_mut)?;
            if let Some(tools) = delta.get("tool_calls").filter(|t| truthy(Some(t))).and_then(Value::as_array) {
                if !truthy(acc_delta.get("tool_calls")) { acc_delta.insert("tool_calls".into(), json!([])); }
                let list = acc_delta.get_mut("tool_calls").and_then(Value::as_array_mut)?;
                for nt in tools {
                    let t_idx = nt.get("index").and_then(Value::as_u64)? as usize;
                    if !truthy(list.get(t_idx)) { set_index(list, t_idx, json!({"function": {"name": "", "arguments": ""}}))?; }
                    let at = list[t_idx].as_object_mut()?;
                    if !truthy(at.get("function")) { at.insert("function".into(), json!({"name": "", "arguments": ""})); }
                    if !at["function"]["arguments"].is_string() { at.get_mut("function").and_then(Value::as_object_mut)?.insert("arguments".into(), json!("")); }
                    if truthy(nt.get("id")) { at.insert("id".into(), nt["id"].clone()); }
                    if truthy(nt.get("type")) { at.insert("type".into(), nt["type"].clone()); }
                    let f = at.get_mut("function").and_then(Value::as_object_mut)?;
                    if truthy(get(nt, &["function", "name"])) { f.insert("name".into(), nt["function"]["name"].clone()); }
                    if let Some(a) = get_str(nt, &["function", "arguments"]) { let prev = to_string(f.get("arguments")); f.insert("arguments".into(), json!(prev + a)); }
                }
            }
        }
        result.insert("choices".into(), Value::Array(out));
    }
    Some(Some(Value::Object(result)))
}

fn update_messages(acc: Option<Value>, chunk: Value) -> Option<Option<Value>> {
    let kind = get_str(&chunk, &["type"]).map(str::to_owned);
    let Some(mut acc) = acc else {
        if kind.as_deref() == Some("message_start") {
            let mut a = spread_obj(chunk.get("message").unwrap_or(&Value::Null));
            if !truthy(a.get("content")) { a.insert("content".into(), json!([])); }
            if !truthy(a.get("usage")) { a.insert("usage".into(), json!({})); }
            return Some(Some(Value::Object(a)));
        }
        return Some(Some(chunk));
    };
    match kind.as_deref() {
        Some("message_start") => {
            let mut a = spread_obj(&acc);
            for (k, v) in spread_obj(chunk.get("message").unwrap_or(&Value::Null)) { a.insert(k, v); }
            if !truthy(a.get("content")) { a.insert("content".into(), json!([])); }
            acc = Value::Object(a);
        }
        Some("content_block_start") => {
            let idx = chunk.get("index").and_then(Value::as_u64)? as usize;
            let block = chunk.get("content_block").cloned().unwrap_or(Value::Null);
            let mut b = spread_obj(&block);
            match get_str(&block, &["type"]) {
                Some("tool_use") => { b.insert("partial_json".into(), json!("")); b.insert("input".into(), json!({})); }
                Some(t @ ("thinking" | "thought")) => { let k = t.to_owned(); if !truthy(b.get(&k)) { b.insert(k, json!("")); } }
                Some("text") => { if !truthy(b.get("text")) { b.insert("text".into(), json!("")); } }
                _ => {}
            }
            let content = acc.get_mut("content").and_then(Value::as_array_mut)?;
            set_index(content, idx, Value::Object(b))?;
        }
        Some("content_block_delta") => {
            let idx = chunk.get("index").and_then(Value::as_u64)? as usize;
            let delta = chunk.get("delta").cloned().unwrap_or(Value::Null);
            let dtype = get_str(&delta, &["type"]).unwrap_or("").to_owned();
            let content = acc.get_mut("content").and_then(Value::as_array_mut)?;
            if !truthy(content.get(idx)) {
                let init = match dtype.as_str() {
                    "input_json_delta" => json!({"type": "tool_use", "partial_json": "", "input": {}}),
                    "thinking_delta" => json!({"type": "thinking", "thinking": ""}),
                    "thought_delta" => json!({"type": "thought", "thought": ""}),
                    _ => json!({"type": "text", "text": ""}),
                };
                set_index(content, idx, init)?;
            }
            let target = content[idx].as_object_mut()?;
            let append = |target: &mut Obj, key: &str, add: Option<&Value>| {
                let prev = target.get(key).filter(|v| truthy(Some(v))).map(|v| to_string(Some(v))).unwrap_or_default();
                target.insert(key.into(), json!(prev + &to_string(add)));
            };
            match dtype.as_str() {
                "text_delta" => append(target, "text", delta.get("text")),
                "thinking_delta" => append(target, "thinking", delta.get("thinking")),
                "thought_delta" => append(target, "thought", delta.get("thought")),
                "input_json_delta" => {
                    append(target, "partial_json", delta.get("partial_json"));
                    if let Some(p) = parse(&to_string(target.get("partial_json"))) { target.insert("input".into(), p); }
                }
                _ => {}
            }
        }
        Some("message_delta") => {
            let a = acc.as_object_mut()?;
            if let Some(d) = chunk.get("delta").filter(|d| truthy(Some(d))) {
                if truthy(d.get("stop_reason")) { a.insert("stop_reason".into(), d["stop_reason"].clone()); }
                if truthy(d.get("stop_sequence")) { a.insert("stop_sequence".into(), d["stop_sequence"].clone()); }
            }
            if let Some(u) = chunk.get("usage").filter(|u| truthy(Some(u))) {
                let mut merged = spread_obj(a.get("usage").unwrap_or(&Value::Null));
                for (k, v) in spread_obj(u) { merged.insert(k, v); }
                a.insert("usage".into(), Value::Object(merged));
            }
        }
        _ => {}
    }
    Some(Some(acc))
}

fn update_gemini(acc: Option<Value>, chunk: Value) -> Option<Option<Value>> {
    let Some(mut acc) = acc else {
        let mut a = spread_obj(&chunk);
        if !truthy(a.get("candidates")) { a.insert("candidates".into(), json!([])); }
        return Some(Some(Value::Object(a)));
    };
    let a = acc.as_object_mut()?;
    for k in ["modelVersion", "responseId", "usageMetadata"] { if truthy(chunk.get(k)) { a.insert(k.into(), chunk[k].clone()); } }
    if let Some(cands) = chunk.get("candidates").and_then(Value::as_array).filter(|c| !c.is_empty()) {
        if !truthy(a.get("candidates")) { a.insert("candidates".into(), json!([])); }
        let acc_cands = a.get_mut("candidates").and_then(Value::as_array_mut)?;
        for (index, cc) in cands.iter().enumerate() {
            if !truthy(acc_cands.get(index)) { set_index(acc_cands, index, json!({"content": {"parts": [], "role": "model"}, "index": index}))?; }
            let ac = acc_cands[index].as_object_mut()?;
            if truthy(cc.get("finishReason")) { ac.insert("finishReason".into(), cc["finishReason"].clone()); }
            let Some(parts) = get_arr(cc, &["content", "parts"]).filter(|_| truthy(cc.get("content"))) else { continue };
            if !truthy(ac.get("content")) { ac.insert("content".into(), json!({"parts": [], "role": "model"})); }
            let content = ac.get_mut("content").and_then(Value::as_object_mut)?;
            if !truthy(content.get("parts")) { content.insert("parts".into(), json!([])); }
            let acc_parts = content.get_mut("parts").and_then(Value::as_array_mut)?;
            for part in parts {
                let last_is_text = acc_parts.last().is_some_and(|l| l.get("text").is_some() && !truthy(l.get("functionCall")));
                if truthy(part.get("text")) {
                    if last_is_text {
                        let last = acc_parts.last_mut().and_then(Value::as_object_mut)?;
                        let prev = to_string(last.get("text"));
                        last.insert("text".into(), json!(prev + &to_string(part.get("text"))));
                        if truthy(part.get("thought")) { last.insert("thought".into(), json!(true)); }
                    } else { acc_parts.push(Value::Object(spread_obj(part))); }
                } else { acc_parts.push(Value::Object(spread_obj(part))); }
            }
        }
    }
    Some(Some(acc))
}

fn update_responses(acc: Option<Value>, event: Value) -> Option<Option<Value>> {
    let kind = get_str(&event, &["type"]).map(str::to_owned);
    let mut acc = match acc {
        None if kind.as_deref() == Some("response.created") => {
            let mut a = spread_obj(event.get("response").unwrap_or(&Value::Null));
            if !truthy(a.get("output")) { a.insert("output".into(), json!([])); }
            return Some(Some(Value::Object(a)));
        }
        None => json!({"output": []}),
        Some(a) => a,
    };
    let assign = |acc: &mut Value, src: Option<&Value>| {
        if let (Some(a), Some(s)) = (acc.as_object_mut(), src.and_then(Value::as_object)) { for (k, v) in s { a.insert(k.clone(), v.clone()); } }
    };
    match kind.as_deref() {
        Some("response.created") | Some("response.in_progress") | Some("response.failed") | Some("response.incomplete") => {
            if truthy(event.get("response")) { assign(&mut acc, event.get("response")); }
        }
        Some("response.output_item.added") => {
            let len = acc.get("output").and_then(Value::as_array).map(|o| o.len())?;
            let idx = event.get("output_index").filter(|v| !v.is_null()).and_then(Value::as_u64).map(|i| i as usize).unwrap_or(len);
            if truthy(event.get("item")) { set_index(acc.get_mut("output").and_then(Value::as_array_mut)?, idx, Value::Object(spread_obj(&event["item"])))?; }
        }
        Some("response.output_text.delta") => {
            let oi = event.get("output_index").and_then(Value::as_u64).map(|i| i as usize);
            let ci = event.get("content_index").and_then(Value::as_u64).map(|i| i as usize);
            if let (Some(oi), Some(ci)) = (oi, ci) {
                let output = acc.get_mut("output").and_then(Value::as_array_mut)?;
                if let Some(part) = output.get_mut(oi).filter(|i| truthy(Some(i))).and_then(|i| i.get_mut("content")).and_then(Value::as_array_mut).and_then(|c| c.get_mut(ci)).filter(|p| truthy(Some(p))).and_then(Value::as_object_mut) {
                    let prev = part.get("text").filter(|t| truthy(Some(t))).map(|t| to_string(Some(t))).unwrap_or_default();
                    part.insert("text".into(), json!(prev + &to_string(event.get("delta"))));
                }
            }
        }
        Some("response.function_call_arguments.delta") => {
            if let Some(oi) = event.get("output_index").and_then(Value::as_u64) {
                let output = acc.get_mut("output").and_then(Value::as_array_mut)?;
                if let Some(item) = output.get_mut(oi as usize).filter(|i| get_str(i, &["type"]) == Some("function_call")).and_then(Value::as_object_mut) {
                    let prev = item.get("arguments").filter(|t| truthy(Some(t))).map(|t| to_string(Some(t))).unwrap_or_default();
                    item.insert("arguments".into(), json!(prev + &to_string(event.get("delta"))));
                }
            }
        }
        Some("response.output_item.done") => {
            if let (true, Some(oi)) = (truthy(event.get("item")), event.get("output_index").filter(|v| !v.is_null()).and_then(Value::as_u64)) {
                set_index(acc.get_mut("output").and_then(Value::as_array_mut)?, oi as usize, Value::Object(spread_obj(&event["item"])))?;
            }
        }
        Some("response.completed") => {
            if truthy(event.get("response")) {
                let accumulated = acc.get("output").cloned();
                assign(&mut acc, event.get("response"));
                let acc_nonempty = accumulated.as_ref().and_then(Value::as_array).is_some_and(|o| !o.is_empty());
                let resp_empty = get_arr(&event, &["response", "output"]).is_some_and(|o| o.is_empty());
                if acc_nonempty && resp_empty { acc["output"] = accumulated.unwrap(); }
            }
        }
        _ => {}
    }
    Some(Some(acc))
}

fn map_oauth_usage(u: Option<&Value>) -> Option<Value> {
    let u = u.filter(|u| truthy(Some(u)))?;
    let n = |k: &str| u.get(k).filter(|v| truthy(Some(v))).cloned().unwrap_or(json!(0));
    Some(json!({"input_tokens": n("input"), "output_tokens": n("output"), "total_tokens": n("totalTokens"), "reasoning_tokens": 0, "cached_tokens": n("cacheRead"), "cache_creation_tokens": n("cacheWrite")}))
}

fn reconstruct_oauth(body: &str) -> Value {
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut tools: Vec<Value> = Vec::new();
    let mut usage: Option<Value> = None;
    let mut finish = Value::Null;
    let blank = || json!({"id": "", "type": "function", "function": {"name": "", "arguments": ""}});
    for line in body.split('\n') {
        let t = line.trim();
        if t.is_empty() { continue; }
        let Some(ev) = parse(t) else { continue };
        let idx = ev.get("contentIndex").filter(|v| !v.is_null()).and_then(Value::as_u64).unwrap_or(0) as usize;
        if idx > MAX_RECONSTRUCT_INDEX { continue; }
        match get_str(&ev, &["type"]) {
            Some("text_delta") => content.push_str(&ev.get("delta").filter(|d| truthy(Some(d))).map(|d| to_string(Some(d))).unwrap_or_default()),
            Some("thinking_delta") => reasoning.push_str(&ev.get("delta").filter(|d| truthy(Some(d))).map(|d| to_string(Some(d))).unwrap_or_default()),
            Some("toolcall_start") => { if !truthy(tools.get(idx)) { let _ = set_index(&mut tools, idx, blank()); } }
            Some("toolcall_delta") => {
                if !truthy(tools.get(idx)) { let _ = set_index(&mut tools, idx, blank()); }
                let tc = get(&ev, &["partial", "content", &idx.to_string()]).cloned();
                if let Some(id) = tc.as_ref().and_then(|t| t.get("id")).filter(|v| truthy(Some(v))) { tools[idx]["id"] = id.clone(); }
                if let Some(n) = tc.as_ref().and_then(|t| t.get("name")).filter(|v| truthy(Some(v))) { tools[idx]["function"]["name"] = n.clone(); }
                if let Some(d) = ev.get("delta").and_then(Value::as_str) { let prev = to_string(tools[idx]["function"].get("arguments")); tools[idx]["function"]["arguments"] = json!(prev + d); }
            }
            Some("toolcall_end") => {
                if !truthy(tools.get(idx)) { let _ = set_index(&mut tools, idx, blank()); }
                if let Some(id) = get(&ev, &["toolCall", "id"]).filter(|v| truthy(Some(v))) { tools[idx]["id"] = id.clone(); }
                if let Some(n) = get(&ev, &["toolCall", "name"]).filter(|v| truthy(Some(v))) { tools[idx]["function"]["name"] = n.clone(); }
                if let Some(a) = get(&ev, &["toolCall", "arguments"]).filter(|v| truthy(Some(v))) { tools[idx]["function"]["arguments"] = json!(stringify(a)); }
            }
            Some("done") => { usage = map_oauth_usage(get(&ev, &["message", "usage"])); finish = ev.get("reason").filter(|r| truthy(Some(r))).cloned().unwrap_or(Value::Null); }
            Some("error") => { usage = map_oauth_usage(get(&ev, &["error", "usage"])); finish = ev.get("reason").filter(|r| truthy(Some(r))).cloned().unwrap_or(json!("error")); }
            _ => {}
        }
    }
    let mut snap = obj(vec![("content", Some(json!(content))), ("reasoning_content", Some(json!(reasoning))), ("tool_calls", Some(Value::Array(tools))), ("usage", usage), ("finishReason", Some(finish))]);
    attach_comments(&mut snap, body);
    snap
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reconstructs_streams() {
        let msgs = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"content\":[],\"usage\":{\"input_tokens\":11}}}\n\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":7}}\n\n";
        let r = reconstruct("messages", msgs).unwrap();
        assert_eq!(r["stop_reason"], "end_turn");
        assert_eq!(r["content"][0]["text"], "Hi");
        assert_eq!(r["usage"], json!({"input_tokens": 11, "output_tokens": 7}));
        let chat = "data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\ndata: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hi\"},\"finish_reason\":\"stop\"}]}\n\ndata: {\"id\":\"c\",\"choices\":[],\"usage\":{\"prompt_tokens\":1}}\n\ndata: [DONE]\n\n";
        let r = reconstruct("chat", chat).unwrap();
        assert_eq!(r["choices"][0]["delta"]["content"], "Hi");
        assert_eq!(r["choices"][0]["finish_reason"], "stop");
        assert_eq!(r["usage"]["prompt_tokens"], 1);
    }
}
