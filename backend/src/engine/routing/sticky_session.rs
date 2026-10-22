//! `routing/sticky-session-manager.ts`: in-memory LRU mapping
//! `${alias}:${apiType}:${sessionKey}` → last (provider, model).
use crate::engine::js::{get_arr, stringify, to_string, truthy};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

/// `DEFAULT_STICKY_TTL_MS`.
pub const DEFAULT_STICKY_TTL_MS: i64 = 5 * 60 * 1000;
const MAX_ENTRIES: usize = 10_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StickyEntry {
    pub provider: String,
    pub model: String,
}

#[derive(Default)]
struct Entries {
    map: HashMap<String, (StickyEntry, i64, u64)>,
    /// Insertion sequence → key (first = oldest), mirrors JS `Map` iteration order.
    order: BTreeMap<u64, String>,
    seq: u64,
}

impl Entries {
    fn delete(&mut self, key: &str) {
        if let Some((_, _, seq)) = self.map.remove(key) {
            self.order.remove(&seq);
        }
    }
    fn insert(&mut self, key: String, entry: StickyEntry, expires_at: i64) {
        self.delete(&key);
        self.seq += 1;
        self.order.insert(self.seq, key.clone());
        self.map.insert(key, (entry, expires_at, self.seq));
    }
}

/// `StickySessionManager`. Trefoil should keep one process-wide instance
/// (`global()`), shared by the router (`get`) and the dispatcher (`set`).
#[derive(Default)]
pub struct StickySessionManager {
    entries: Mutex<Entries>,
}

impl StickySessionManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// `StickySessionManager.getInstance()`.
    pub fn global() -> &'static StickySessionManager {
        static INSTANCE: std::sync::OnceLock<StickySessionManager> = std::sync::OnceLock::new();
        INSTANCE.get_or_init(StickySessionManager::new)
    }

    /// `computeSessionKey(req)` over a unified request:
    /// `r:${previousResponseId}` when truthy, else `m:${Bun.hash(JSON.stringify(messages.slice(0, 2))).toString(16)}`
    /// for requests with at least two messages, else `None`.
    pub fn compute_session_key(req: &Value) -> Option<String> {
        if truthy(req.get("previousResponseId")) {
            return Some(format!("r:{}", to_string(req.get("previousResponseId"))));
        }
        let messages = get_arr(req, &["messages"])?;
        if messages.len() < 2 {
            return None;
        }
        let anchor = stringify(&Value::Array(messages[..2].to_vec()));
        Some(format!("m:{:x}", wyhash(0, anchor.as_bytes())))
    }

    fn make_key(alias: &str, api_type: &str, session_key: &str) -> String {
        format!("{alias}:{}:{session_key}", api_type.trim().to_lowercase())
    }

    /// `get(alias, apiType, sessionKey)` at `Date.now()`.
    pub fn get(&self, alias: &str, api_type: &str, session_key: &str) -> Option<StickyEntry> {
        self.get_at(alias, api_type, session_key, crate::engine::js::now_ms())
    }

    /// `get` with an explicit `now` (ms).
    pub fn get_at(
        &self,
        alias: &str,
        api_type: &str,
        session_key: &str,
        now: i64,
    ) -> Option<StickyEntry> {
        let key = Self::make_key(alias, api_type, session_key);
        let mut e = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        let entry = e.map.get(&key).cloned()?;
        if entry.1 <= now {
            e.delete(&key);
            return None;
        }
        e.insert(key, entry.0.clone(), entry.1);
        Some(entry.0)
    }

    /// `set(alias, apiType, sessionKey, provider, model, expiresAt = now + DEFAULT_STICKY_TTL_MS)`.
    pub fn set(
        &self,
        alias: &str,
        api_type: &str,
        session_key: &str,
        provider: &str,
        model: &str,
        expires_at: Option<i64>,
    ) {
        let now = crate::engine::js::now_ms();
        self.set_at(
            alias,
            api_type,
            session_key,
            provider,
            model,
            expires_at.unwrap_or(now + DEFAULT_STICKY_TTL_MS),
            now,
        );
    }

    /// `set` with an explicit `now` (ms).
    #[allow(clippy::too_many_arguments)]
    pub fn set_at(
        &self,
        alias: &str,
        api_type: &str,
        session_key: &str,
        provider: &str,
        model: &str,
        expires_at: i64,
        now: i64,
    ) {
        let key = Self::make_key(alias, api_type, session_key);
        let mut e = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        e.delete(&key);
        if expires_at <= now {
            return;
        }
        e.insert(
            key,
            StickyEntry {
                provider: provider.into(),
                model: model.into(),
            },
            expires_at,
        );
        if e.map.len() > MAX_ENTRIES {
            if let Some(oldest) = e.order.values().next().cloned() {
                e.delete(&oldest);
            }
        }
    }

    /// Test helper.
    pub fn clear(&self) {
        let mut e = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        e.map.clear();
        e.order.clear();
    }

    /// Test helper.
    pub fn size(&self) -> usize {
        self.entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .map
            .len()
    }
}

/// `Bun.hash(input)` — Zig `std.hash.Wyhash.hash(seed, input)` (wyhash final v4).
pub fn wyhash(seed: u64, input: &[u8]) -> u64 {
    const S: [u64; 4] = [
        0xa0761d6478bd642f,
        0xe7037ed1a0b428db,
        0x8ebc6af09c88c6e3,
        0x589965cc75374cc3,
    ];
    fn mum(a: u64, b: u64) -> (u64, u64) {
        let x = (a as u128).wrapping_mul(b as u128);
        (x as u64, (x >> 64) as u64)
    }
    fn mix(a: u64, b: u64) -> u64 {
        let (x, y) = mum(a, b);
        x ^ y
    }
    fn r8(b: &[u8]) -> u64 {
        u64::from_le_bytes(b[..8].try_into().unwrap_or([0; 8]))
    }
    fn r4(b: &[u8]) -> u64 {
        u32::from_le_bytes(b[..4].try_into().unwrap_or([0; 4])) as u64
    }

    let mut state = [seed ^ mix(S[0], S[1]); 3];
    let len = input.len();
    let (a, b);
    if len <= 16 {
        if len >= 4 {
            let end = len - 4;
            let quarter = (len >> 3) << 2;
            a = (r4(input) << 32) | r4(&input[quarter..]);
            b = (r4(&input[end..]) << 32) | r4(&input[end - quarter..]);
        } else if len > 0 {
            a = ((input[0] as u64) << 16) | ((input[len >> 1] as u64) << 8) | input[len - 1] as u64;
            b = 0;
        } else {
            a = 0;
            b = 0;
        }
    } else {
        let mut i = 0;
        if len >= 48 {
            while i + 48 < len {
                for j in 0..3 {
                    let x = r8(&input[i + 16 * j..]);
                    let y = r8(&input[i + 16 * j + 8..]);
                    state[j] = mix(x ^ S[j + 1], y ^ state[j]);
                }
                i += 48;
            }
            state[0] ^= state[1] ^ state[2];
        }
        let rest = &input[i..];
        let mut k = 0;
        while k + 16 < rest.len() {
            state[0] = mix(r8(&rest[k..]) ^ S[1], r8(&rest[k + 8..]) ^ state[0]);
            k += 16;
        }
        a = r8(&input[len - 16..]);
        b = r8(&input[len - 8..]);
    }
    let (a, b) = mum(a ^ S[1], b ^ state[0]);
    mix(a ^ S[0] ^ len as u64, b ^ S[1])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn wyhash_matches_bun() {
        // Vectors generated with `Bun.hash(s).toString(16)` (Bun 1.x).
        let cases: [(&str, &str); 12] = [
            ("", "409638ee2bde459"),
            ("a", "28d2053309d28531"),
            ("abc", "2a4f1d7cb516c72"),
            ("message digest", "41d032e1df79b67e"),
            ("abcdefghijklmnopqrstuvwxyz", "774fa8c21ed6acd2"),
            ("ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789", "369bcbe3f0f0c0d"),
            ("12345678901234567890123456789012345678901234567890123456789012345678901234567890", "48662e17cabfab37"),
            ("xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx", "971f37626c3afdce"),
            ("xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx", "f38f5ae335462355"),
            ("xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx", "cc01984f3dfa1ac2"),
            ("xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx", "f6caf671d6ce6be2"),
            ("héllo wörld 0123456789", "85388a895b4cfbf2"),
        ];
        for (input, expected) in cases {
            assert_eq!(
                format!("{:x}", wyhash(0, input.as_bytes())),
                expected,
                "{input:?}"
            );
        }
    }

    #[test]
    fn session_key() {
        let req = json!({"messages": [{"role": "system", "content": "You are helpful"}, {"role": "user", "content": "hi"}, {"role": "assistant", "content": "x"}]});
        assert_eq!(
            StickySessionManager::compute_session_key(&req).as_deref(),
            Some("m:1d48d3e5a8e2f5bb")
        );
        assert_eq!(
            StickySessionManager::compute_session_key(&json!({"messages": [{"role": "user"}]})),
            None
        );
        assert_eq!(StickySessionManager::compute_session_key(&json!({})), None);
        assert_eq!(
            StickySessionManager::compute_session_key(
                &json!({"previousResponseId": "resp_1", "messages": []})
            )
            .as_deref(),
            Some("r:resp_1")
        );
    }

    #[test]
    fn get_set_expiry_and_lru() {
        let m = StickySessionManager::new();
        m.set_at("a", " Chat ", "k", "p", "m", 2000, 1000);
        assert_eq!(
            m.get_at("a", "chat", "k", 1500),
            Some(StickyEntry {
                provider: "p".into(),
                model: "m".into()
            })
        );
        assert_eq!(m.get_at("a", "responses", "k", 1500), None);
        assert_eq!(m.get_at("b", "chat", "k", 1500), None);
        assert_eq!(m.get_at("a", "chat", "k", 2000), None);
        assert_eq!(m.size(), 0);
        m.set_at("a", "chat", "k", "p", "m", 1000, 1000);
        assert_eq!(m.size(), 0);
        for i in 0..=MAX_ENTRIES {
            m.set_at("a", "chat", &i.to_string(), "p", "m", 5000, 1000);
        }
        assert_eq!(m.size(), MAX_ENTRIES);
        assert_eq!(m.get_at("a", "chat", "0", 1000), None);
        assert!(m.get_at("a", "chat", "1", 1000).is_some());
    }
}
