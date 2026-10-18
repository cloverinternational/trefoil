//! `utils/ip-match.ts`, `utils/ip.ts` (`getClientIp`) and the per-key IP
//! allowlist check of `validatePlexusApiKey`.
//!
//! `trustedProxies` reaches `getConfig()` only once the flat setting has been
//! written; until then (`undefined`) the allowlist trusts every forwarding
//! header like `getClientIp`.
use axum::http::HeaderMap;
use regex::Regex;
use std::net::IpAddr;
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq)]
struct ParsedIp {
    family: u8,
    value: u128,
}

const V4_MAX: u128 = 0xFFFF_FFFF;
const V6_MAX: u128 = u128::MAX;

fn re(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).unwrap())
}

fn strip_addr_decorations(input: &str) -> String {
    static V4_PORT: OnceLock<Regex> = OnceLock::new();
    let mut s = input.trim().to_owned();
    if s.is_empty() {
        return s;
    }
    if s.starts_with('[') {
        if let Some(end) = s.find(']') {
            s = s[1..end].to_owned();
        }
    } else if re(&V4_PORT, r"^\d{1,3}(\.\d{1,3}){3}:\d+$").is_match(&s) {
        s = s[..s.rfind(':').unwrap()].to_owned();
    }
    if let Some(pct) = s.find('%') {
        s.truncate(pct);
    }
    s
}

fn ipv4_value(addr: &str) -> Option<u128> {
    let parts: Vec<&str> = addr.split('.').collect();
    if parts.len() != 4 {
        return None;
    }
    let mut value: u128 = 0;
    for part in parts {
        if part.is_empty() || part.len() > 3 || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let n: u128 = part.parse().ok()?;
        if n > 255 {
            return None;
        }
        value = (value << 8) | n;
    }
    Some(value)
}

fn ipv6_value(addr: &str) -> Option<u128> {
    let mut s = addr.to_owned();
    if s.contains('.') {
        let last = s.rfind(':')?;
        let v4 = ipv4_value(&s[last + 1..])?;
        s = format!("{}{:x}:{:x}", &s[..=last], (v4 >> 16) & 0xffff, v4 & 0xffff);
    }
    let groups: Vec<String> = match s.find("::") {
        Some(i) => {
            if s[i + 1..].contains("::") {
                return None;
            }
            let (b, a) = (&s[..i], &s[i + 2..]);
            let before: Vec<&str> = if b.is_empty() {
                vec![]
            } else {
                b.split(':').collect()
            };
            let after: Vec<&str> = if a.is_empty() {
                vec![]
            } else {
                a.split(':').collect()
            };
            let missing = 8i32 - (before.len() + after.len()) as i32;
            if missing < 1 {
                return None;
            }
            before
                .iter()
                .map(|g| g.to_string())
                .chain((0..missing).map(|_| "0".to_owned()))
                .chain(after.iter().map(|g| g.to_string()))
                .collect()
        }
        None => s.split(':').map(str::to_owned).collect(),
    };
    if groups.len() != 8 {
        return None;
    }
    let mut value: u128 = 0;
    for g in groups {
        if g.is_empty() || g.len() > 4 || !g.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        value = (value << 16) | u128::from_str_radix(&g, 16).ok()?;
    }
    Some(value)
}

fn mapped_ipv4(addr: &str) -> Option<u128> {
    let v = ipv6_value(addr)?;
    (v >> 48 == 0 && (v >> 32) & 0xffff == 0xffff).then_some(v & V4_MAX)
}

/// `ipToBigInt(input)`.
fn ip_value(input: &str) -> Option<ParsedIp> {
    let s = input.trim();
    if s.is_empty() {
        return None;
    }
    if s.contains(':') {
        if let Some(m) = mapped_ipv4(s) {
            return Some(ParsedIp {
                family: 4,
                value: m,
            });
        }
        return ipv6_value(s).map(|value| ParsedIp { family: 6, value });
    }
    ipv4_value(s).map(|value| ParsedIp { family: 4, value })
}

/// `parseRule(rule)` → inclusive `(family, lo, hi)`.
fn parse_rule(rule: &str) -> Option<(u8, u128, u128)> {
    static PREFIX: OnceLock<Regex> = OnceLock::new();
    static OCTET: OnceLock<Regex> = OnceLock::new();
    let r = rule.trim();
    if r.is_empty() {
        return None;
    }
    if let Some(slash) = r.find('/') {
        let prefix_str = &r[slash + 1..];
        if !re(&PREFIX, r"^\d{1,3}$").is_match(prefix_str) {
            return None;
        }
        let prefix: u32 = prefix_str.parse().ok()?;
        let ip = ip_value(&r[..slash])?;
        let bits = if ip.family == 4 { 32 } else { 128 };
        if prefix > bits {
            return None;
        }
        let max = if ip.family == 4 { V4_MAX } else { V6_MAX };
        let host_bits = bits - prefix;
        let host_mask = if host_bits == 0 {
            0
        } else if host_bits == 128 {
            u128::MAX
        } else {
            (1u128 << host_bits) - 1
        };
        let lo = ip.value & (max ^ host_mask);
        return Some((ip.family, lo, lo | host_mask));
    }
    if let Some(dash) = r.find('-') {
        let lo = ip_value(&r[..dash])?;
        let right = r[dash + 1..].trim();
        if lo.family == 4 && re(&OCTET, r"^\d{1,3}$").is_match(right) {
            let last: u128 = right.parse().ok()?;
            if last > 255 {
                return None;
            }
            let hi = (lo.value & (V4_MAX ^ 0xff)) | last;
            if hi < lo.value {
                return None;
            }
            return Some((4, lo.value, hi));
        }
        let hi = ip_value(right)?;
        if hi.family != lo.family || hi.value < lo.value {
            return None;
        }
        return Some((lo.family, lo.value, hi.value));
    }
    let ip = ip_value(r)?;
    Some((ip.family, ip.value, ip.value))
}

/// `isIpAllowed(clientIp, allowlist)`: empty list allows all; otherwise
/// same-family containment, failing closed on an unparseable client.
pub fn is_ip_allowed(client_ip: Option<&str>, allowlist: &[String]) -> bool {
    let rules: Vec<&str> = allowlist
        .iter()
        .map(|r| r.trim())
        .filter(|r| !r.is_empty())
        .collect();
    if rules.is_empty() {
        return true;
    }
    let Some(client) = client_ip
        .filter(|c| !c.is_empty())
        .and_then(|c| ip_value(&strip_addr_decorations(c)))
    else {
        return false;
    };
    rules
        .iter()
        .filter_map(|r| parse_rule(r))
        .any(|(family, lo, hi)| family == client.family && client.value >= lo && client.value <= hi)
}

/// Fastify `request.ip` for a socket peer (dual-stack listeners report IPv4
/// clients in their mapped form).
fn peer_text(peer: IpAddr) -> String {
    peer.to_string()
}

/// `getClientIp(request)`.
pub fn client_ip(headers: &HeaderMap, peer: Option<IpAddr>) -> Option<String> {
    static FORWARDED: OnceLock<Regex> = OnceLock::new();
    let h = |k: &str| {
        headers
            .get(k)
            .and_then(|v| v.to_str().ok())
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
    };
    if let Some(v) = h("cf-connecting-ip")
        .or_else(|| h("true-client-ip"))
        .or_else(|| h("x-real-ip"))
    {
        return Some(v);
    }
    if let Some(xff) = h("x-forwarded-for") {
        let first = xff.split(',').next().unwrap_or("").trim().to_owned();
        return (!first.is_empty()).then_some(first);
    }
    if let Some(v) = h("x-client-ip")
        .or_else(|| h("fastly-client-ip"))
        .or_else(|| h("x-cluster-client-ip"))
    {
        return Some(v);
    }
    if let Some(fwd) = h("forwarded") {
        if let Some(c) = re(&FORWARDED, r#"(?i)for="?([^";,]+)"?"#).captures(&fwd) {
            return Some(c[1].to_owned());
        }
    }
    peer.map(peer_text)
}

/// `getTrustedClientIp(request, trustedProxies)`.
pub fn trusted_client_ip(
    headers: &HeaderMap,
    peer: Option<IpAddr>,
    trusted_proxies: Option<&[String]>,
) -> Option<String> {
    let Some(proxies) = trusted_proxies else {
        return client_ip(headers, peer);
    };
    let rules: Vec<String> = proxies
        .iter()
        .map(|r| r.trim().to_owned())
        .filter(|r| !r.is_empty())
        .collect();
    let peer = peer.map(peer_text);
    if rules.is_empty() || !is_ip_allowed(peer.as_deref(), &rules) {
        return peer;
    }
    let forwarded = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.trim().is_empty());
    if let Some(fwd) = forwarded {
        let chain: Vec<&str> = fwd
            .split(',')
            .map(str::trim)
            .filter(|e| !e.is_empty())
            .collect();
        if let Some(hop) = chain
            .iter()
            .rev()
            .find(|hop| !is_ip_allowed(Some(hop), &rules))
        {
            return Some(hop.to_string());
        }
        if let Some(last) = chain.last() {
            return Some(last.to_string());
        }
    }
    peer
}

/// Peer address from axum's `ConnectInfo` extension.
pub fn peer_ip(extensions: &axum::http::Extensions) -> Option<IpAddr> {
    extensions
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|c| c.0.ip())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn v(l: &[&str]) -> Vec<String> {
        l.iter().map(|s| s.to_string()).collect()
    }
    #[test]
    fn allowlist() {
        assert!(is_ip_allowed(None, &[]));
        assert!(!is_ip_allowed(None, &v(&["10.0.0.0/8"])));
        assert!(is_ip_allowed(Some("10.1.2.3"), &v(&["10.0.0.0/8"])));
        assert!(is_ip_allowed(Some("::ffff:127.0.0.1"), &v(&["127.0.0.1"])));
        assert!(is_ip_allowed(Some("10.1.0.15"), &v(&["10.1.0.10-20"])));
        assert!(!is_ip_allowed(Some("10.1.0.21"), &v(&["10.1.0.10-20"])));
        assert!(is_ip_allowed(
            Some("2001:db8::3"),
            &v(&["2001:db8::1-2001:db8::5"])
        ));
        assert!(!is_ip_allowed(Some("::1"), &v(&["0.0.0.0/0"])));
        assert!(is_ip_allowed(Some("::1"), &v(&["::/0"])));
        assert!(is_ip_allowed(Some("[::1]:443"), &v(&["::1"])));
        assert!(is_ip_allowed(Some("1.2.3.4:80"), &v(&["1.2.3.4/32"])));
    }
}
