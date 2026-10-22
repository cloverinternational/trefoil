//! Extra JS-semantics helpers used by the Responses port (beyond `engine::js`).
use crate::engine::js::{self, Obj};
use crate::engine::transformer::EngineError;
use serde_json::Value;

/// JS `WhiteSpace` + `LineTerminator` (what `String.prototype.trim` strips).
pub fn is_js_whitespace(c: char) -> bool {
    (c.is_whitespace() && c != '\u{85}') || c == '\u{feff}'
}

/// `s.trim()`.
pub fn js_trim(s: &str) -> &str {
    s.trim_matches(is_js_whitespace)
}

/// `s.trimStart()`.
pub fn js_trim_start(s: &str) -> &str {
    s.trim_start_matches(is_js_whitespace)
}

/// `a === b` for possibly-undefined JSON values. Objects/arrays are distinct
/// references in every place this is used, so they never compare equal.
pub fn strict_eq(a: Option<&Value>, b: Option<&Value>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(Value::Null), Some(Value::Null)) => true,
        (Some(Value::Bool(x)), Some(Value::Bool(y))) => x == y,
        (Some(Value::Number(x)), Some(Value::Number(y))) => x.as_f64() == y.as_f64(),
        (Some(Value::String(x)), Some(Value::String(y))) => x == y,
        _ => false,
    }
}

/// A JS `Set` of (possibly undefined) primitive values, insertion ordered.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ValueSet(Vec<Option<Value>>);

impl ValueSet {
    pub fn has(&self, v: Option<&Value>) -> bool {
        self.0.iter().any(|x| strict_eq(x.as_ref(), v))
    }
    pub fn has_str(&self, s: &str) -> bool {
        self.has(Some(&Value::String(s.to_owned())))
    }
    pub fn add(&mut self, v: Option<&Value>) {
        if !self.has(v) {
            self.0.push(v.cloned());
        }
    }
    pub fn clear(&mut self) {
        self.0.clear();
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn iter(&self) -> impl Iterator<Item = Option<&Value>> {
        self.0.iter().map(Option::as_ref)
    }
}

/// Bun/JavaScriptCore's `TypeError: <null|undefined> is not an object
/// (evaluating '<expr>')` (Plexus runs on Bun).
pub fn read_error(target: Option<&Value>, expr: &str) -> EngineError {
    let what = if target.is_none() {
        "undefined"
    } else {
        "null"
    };
    EngineError::new(format!("{what} is not an object (evaluating '{expr}')"))
}

/// `<expr>` = `target.key` (key = last segment of `expr`), throwing like
/// JS when `target` is null/undefined.
pub fn prop<'a>(target: Option<&'a Value>, expr: &str) -> Result<Option<&'a Value>, EngineError> {
    let key = expr.rsplit('.').next().unwrap_or(expr);
    match target {
        None | Some(Value::Null) => Err(read_error(target, expr)),
        Some(v) => Ok(v.get(key)),
    }
}

/// `for (const x of v)` with Bun/JavaScriptCore semantics: arrays yield
/// their items, strings their characters; anything else throws JSC's message.
pub fn iterate(v: Option<&Value>, expr: &str) -> Result<Vec<Value>, EngineError> {
    match v {
        Some(Value::Array(items)) => Ok(items.clone()),
        Some(Value::String(s)) => Ok(s.chars().map(|c| Value::String(c.to_string())).collect()),
        None | Some(Value::Null) => Err(read_error(v, expr)),
        Some(Value::Object(_)) => Err(EngineError::new("{} is not iterable")),
        Some(Value::Number(_)) => Err(EngineError::new("number is not iterable")),
        Some(Value::Bool(b)) => Err(EngineError::new(format!("{b} is not iterable"))),
    }
}

/// `[...v]` with JSC semantics (strings spread to characters).
pub fn spread_iter(v: &Value) -> Result<Vec<Value>, EngineError> {
    match v {
        Value::Array(_) | Value::String(_) => iterate(Some(v), ""),
        _ => Err(EngineError::new(
            "Spread syntax requires ...iterable[Symbol.iterator] to be a function",
        )),
    }
}

/// `JSON.stringify(v)` where `undefined` stringifies to `undefined`.
pub fn stringify_opt(v: Option<&Value>) -> Option<String> {
    v.map(js::stringify)
}

/// `Array.prototype.join(sep)` (null/undefined elements become '').
pub fn join(items: &[Option<&Value>], sep: &str) -> String {
    items
        .iter()
        .map(|v| match v {
            None | Some(Value::Null) => String::new(),
            Some(v) => js::to_string(Some(v)),
        })
        .collect::<Vec<_>>()
        .join(sep)
}

/// True for a canonical array-index key (`"0"`, `"17"`, not `"01"`).
fn array_index(key: &str) -> Option<u32> {
    if key.is_empty()
        || (key.len() > 1 && key.starts_with('0'))
        || !key.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    key.parse::<u32>().ok().filter(|n| *n != u32::MAX)
}

/// `Object.values(obj)` in JS property order: integer-like keys ascending,
/// then string keys in insertion order.
pub fn object_values(map: &Obj) -> Vec<&Value> {
    let mut indexed: Vec<(u32, &Value)> = map
        .iter()
        .filter_map(|(k, v)| array_index(k).map(|i| (i, v)))
        .collect();
    indexed.sort_by_key(|(i, _)| *i);
    let mut out: Vec<&Value> = indexed.into_iter().map(|(_, v)| v).collect();
    out.extend(
        map.iter()
            .filter(|(k, _)| array_index(k).is_none())
            .map(|(_, v)| v),
    );
    out
}

/// `n.toString(36)` for non-negative integers.
pub fn to_base36(mut n: u64) -> String {
    const ALPHABET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if n == 0 {
        return "0".into();
    }
    let mut out = Vec::new();
    while n > 0 {
        out.push(ALPHABET[(n % 36) as usize]);
        n /= 36;
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

/// `Math.random().toString(36).substring(2, 15)`: the fractional base-36
/// digits of a double — 11 characters in ~70% of draws (the modal length).
pub fn random_fraction_base36() -> String {
    js::random_base36(11)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn helpers_follow_js() {
        assert_eq!(js_trim("\u{feff} a \n"), "a");
        assert_eq!(js_trim("\u{85}a"), "\u{85}a");
        assert_eq!(to_base36(0), "0");
        assert_eq!(to_base36(1_700_000_000_000), "loyw3v28");
        let m = json!({"b": 1, "2": 2, "a": 3, "1": 4, "01": 5});
        let vals: Vec<&Value> = object_values(m.as_object().unwrap());
        assert_eq!(
            vals,
            vec![&json!(4), &json!(2), &json!(1), &json!(3), &json!(5)]
        );
        assert_eq!(
            join(
                &[Some(&json!("a")), None, Some(&Value::Null), Some(&json!(1))],
                "\n"
            ),
            "a\n\n\n1"
        );
        assert!(strict_eq(Some(&json!(1)), Some(&json!(1.0))));
        assert!(!strict_eq(Some(&json!({})), Some(&json!({}))));
    }
}
