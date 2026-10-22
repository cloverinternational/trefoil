//! JavaScript value semantics for the Plexus engine port.
//!
//! Engine code works on `serde_json::Value` and mirrors the TypeScript
//! source line by line. Conventions:
//! - `undefined` is an absent key (or `None`); `null` is `Value::Null`.
//!   `JSON.stringify` drops undefined members, so builders skip `None`.
//! - `get(v, &["a", "b"])` is `v?.a?.b`: `None` when any step is missing or a
//!   non-object/array intermediate; a final `null` is `Some(&Value::Null)`.
//! - `truthy` / `nullish` follow JS rules; `or` is `||`, `coalesce` is `??`.
use rand::Rng;
use serde_json::{Map, Value};

pub type Obj = Map<String, Value>;

/// `v?.a?.b…` (array steps accept numeric strings, like `arr[0]`).
pub fn get<'a>(v: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut cur = v;
    for key in path {
        cur = match cur {
            Value::Object(map) => map.get(*key)?,
            Value::Array(items) => items.get(key.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(cur)
}

/// `v?.a?.b` as a string (`typeof === 'string'`).
pub fn get_str<'a>(v: &'a Value, path: &[&str]) -> Option<&'a str> {
    get(v, path).and_then(Value::as_str)
}
pub fn get_f64(v: &Value, path: &[&str]) -> Option<f64> {
    get(v, path).and_then(Value::as_f64)
}
pub fn get_arr<'a>(v: &'a Value, path: &[&str]) -> Option<&'a Vec<Value>> {
    get(v, path).and_then(Value::as_array)
}
pub fn get_obj<'a>(v: &'a Value, path: &[&str]) -> Option<&'a Obj> {
    get(v, path).and_then(Value::as_object)
}

/// JS truthiness of a possibly-undefined value.
pub fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

/// `v === undefined || v === null`.
pub fn nullish(v: Option<&Value>) -> bool {
    matches!(v, None | Some(Value::Null))
}

/// `a || b` (owned).
pub fn or(a: Option<&Value>, b: Option<&Value>) -> Option<Value> {
    if truthy(a) {
        a.cloned()
    } else {
        b.cloned()
    }
}

/// `a ?? b` (owned).
pub fn coalesce(a: Option<&Value>, b: Option<&Value>) -> Option<Value> {
    if nullish(a) {
        b.cloned()
    } else {
        a.cloned()
    }
}

/// Object literal that drops `undefined` (`None`) members, like `JSON.stringify`.
pub fn obj(pairs: Vec<(&str, Option<Value>)>) -> Value {
    let mut map = Map::new();
    for (k, v) in pairs {
        if let Some(v) = v {
            map.insert(k.to_owned(), v);
        }
    }
    Value::Object(map)
}

/// Insert unless `undefined`.
pub fn set_opt(map: &mut Obj, key: &str, value: Option<Value>) {
    if let Some(v) = value {
        map.insert(key.to_owned(), v);
    }
}

/// `{...a, ...b}` (shallow, b wins); non-objects spread nothing.
pub fn spread(a: &Value, b: &Value) -> Value {
    let mut out = a.as_object().cloned().unwrap_or_default();
    if let Some(bm) = b.as_object() {
        for (k, v) in bm {
            out.insert(k.clone(), v.clone());
        }
    }
    Value::Object(out)
}

/// `typeof v` for JSON values (`undefined` for `None`).
pub fn type_of(v: Option<&Value>) -> &'static str {
    match v {
        None => "undefined",
        Some(Value::Null) | Some(Value::Object(_)) | Some(Value::Array(_)) => "object",
        Some(Value::Bool(_)) => "boolean",
        Some(Value::Number(_)) => "number",
        Some(Value::String(_)) => "string",
    }
}

/// A JS number as a JSON value (integral → integer form, like `JSON.stringify`).
pub fn num(n: f64) -> Value {
    if n.fract() == 0.0 && n.abs() < 9_007_199_254_740_992.0 {
        Value::from(n as i64)
    } else {
        serde_json::Number::from_f64(n)
            .map(Value::Number)
            .unwrap_or(Value::Null)
    }
}

/// `Number(v)` with JS coercion for the JSON subset (NaN → None).
pub fn to_number(v: Option<&Value>) -> Option<f64> {
    match v {
        None => None,
        Some(Value::Null) => Some(0.0),
        Some(Value::Bool(b)) => Some(if *b { 1.0 } else { 0.0 }),
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) => {
            let t = s.trim();
            if t.is_empty() {
                Some(0.0)
            } else {
                t.parse::<f64>().ok()
            }
        }
        Some(_) => None,
    }
}

/// `String(v)` for the JSON subset.
pub fn to_string(v: Option<&Value>) -> String {
    match v {
        None => "undefined".into(),
        Some(Value::Null) => "null".into(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.as_f64().map(js_num_string).unwrap_or_default(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|i| {
                if i.is_null() {
                    String::new()
                } else {
                    to_string(Some(i))
                }
            })
            .collect::<Vec<_>>()
            .join(","),
        Some(Value::Object(_)) => "[object Object]".into(),
    }
}

/// JS `Number.prototype.toString()` for finite numbers.
pub fn js_num_string(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e21 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

/// JS `Number.prototype.toString()` for any finite f64 (shortest round-trip
/// digits; exponent form below 1e-6 and from 1e21, like ECMAScript).
pub fn js_number_text(n: f64) -> String {
    if n == 0.0 {
        return "0".into();
    }
    if !n.is_finite() {
        return "null".into();
    }
    let abs = n.abs();
    if (1e-6..1e21).contains(&abs) {
        let s = format!("{n}");
        return s;
    }
    // Rust `{:e}` gives shortest digits as `d.ddde-7`; JS writes `d.ddde-7` / `de+21`.
    let s = format!("{n:e}");
    let (mantissa, exp) = s.split_once('e').unwrap_or((&s, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    format!(
        "{mantissa}e{}{}",
        if exp >= 0 { "+" } else { "-" },
        exp.abs()
    )
}

struct JsFormatter;
impl serde_json::ser::Formatter for JsFormatter {
    fn write_f64<W: ?Sized + std::io::Write>(
        &mut self,
        writer: &mut W,
        value: f64,
    ) -> std::io::Result<()> {
        writer.write_all(js_number_text(value).as_bytes())
    }
    fn write_f32<W: ?Sized + std::io::Write>(
        &mut self,
        writer: &mut W,
        value: f32,
    ) -> std::io::Result<()> {
        writer.write_all(js_number_text(value as f64).as_bytes())
    }
}

/// `JSON.stringify(v)`: compact, insertion key order, JS number text.
pub fn stringify(v: &Value) -> String {
    let mut out = Vec::new();
    let mut ser = serde_json::Serializer::with_formatter(&mut out, JsFormatter);
    if serde::Serialize::serialize(v, &mut ser).is_err() {
        return String::new();
    }
    String::from_utf8(out).unwrap_or_default()
}

/// `JSON.parse` returning `None` on failure.
pub fn parse(text: &str) -> Option<Value> {
    serde_json::from_str(text).ok()
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}
/// `Math.floor(Date.now() / 1000)`.
pub fn now_secs() -> i64 {
    now_ms().div_euclid(1000)
}
pub fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// `crypto.randomUUID()`.
pub fn uuid() -> String {
    let mut b: [u8; 16] = rand::thread_rng().gen();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

/// `Math.random().toString(36).slice(2, 2 + n)`-style id fragment.
pub fn random_base36(n: usize) -> String {
    const ALPHABET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut rng = rand::thread_rng();
    (0..n)
        .map(|_| ALPHABET[rng.gen_range(0..36)] as char)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn js_semantics() {
        let v = json!({"a": {"b": null, "c": [1, 2]}});
        assert_eq!(get(&v, &["a", "b"]), Some(&Value::Null));
        assert_eq!(get(&v, &["a", "b", "x"]), None);
        assert_eq!(get(&v, &["a", "c", "1"]), Some(&json!(2)));
        assert!(!truthy(get(&v, &["a", "b"])) && truthy(get(&v, &["a", "c"])));
        assert_eq!(
            obj(vec![("x", None), ("y", Some(json!(1)))]),
            json!({"y": 1})
        );
        assert_eq!(or(Some(&json!("")), Some(&json!("b"))), Some(json!("b")));
        assert_eq!(
            coalesce(Some(&json!("")), Some(&json!("b"))),
            Some(json!(""))
        );
        assert_eq!(num(3.0), json!(3));
        assert_eq!(to_number(Some(&json!(" 4 "))), Some(4.0));
        assert_eq!(uuid().len(), 36);
        assert_eq!(
            stringify(&json!({"a": 1.0, "b": 0.5, "c": 1e21, "d": 1e-7, "e": 123456.789})),
            r#"{"a":1,"b":0.5,"c":1e+21,"d":1e-7,"e":123456.789}"#
        );
    }
}
