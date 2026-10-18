//! Minimal zod v4 (4.6) semantics for porting Plexus request schemas.
//!
//! Plexus validates management bodies with zod and returns `error.issues`
//! verbatim, and stores the parsed output (defaults applied, unknown keys
//! stripped). This module reproduces both for the subset of combinators the
//! ported schemas use, so a schema transcription yields identical issues and
//! output. Semantics followed (zod `v4/core/schemas.js`):
//! - base type errors (`invalid_type`, `invalid_value`, unions, strict keys)
//!   abort; check errors (`too_small`, `too_big`, formats, refinements) do not;
//! - checks/refinements are skipped once the payload has an aborting issue;
//! - a union returns the single non-aborted branch's issues, else
//!   `invalid_union` listing every branch;
//! - objects validate shape keys in declaration order and strip unknown keys.
use serde_json::{Map, Value};

pub type Path = Vec<Value>;

#[derive(Debug, Clone)]
pub struct Issue {
    pub fields: Map<String, Value>,
    pub abort: bool,
}

impl Issue {
    fn new(code: &str, path: &Path, message: String, abort: bool) -> Self {
        let mut fields = Map::new();
        fields.insert("code".into(), code.into());
        fields.insert("path".into(), Value::Array(path.clone()));
        fields.insert("message".into(), message.into());
        Issue { fields, abort }
    }
    fn with(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.fields.insert(key.into(), value.into());
        self
    }
    pub fn custom(path: &Path, message: impl Into<String>) -> Self {
        Issue::new("custom", path, message.into(), false)
    }
    pub fn to_json(&self) -> Value {
        Value::Object(self.fields.clone())
    }
    fn relative_to(&self, base: usize) -> Value {
        let mut fields = self.fields.clone();
        if let Some(Value::Array(path)) = fields.get("path") {
            fields.insert(
                "path".into(),
                Value::Array(path[base.min(path.len())..].to_vec()),
            );
        }
        Value::Object(fields)
    }
}

pub type Refine = Box<dyn Fn(&Value) -> bool + Send + Sync>;
pub type SuperRefine = Box<dyn Fn(&Value, &Path, &mut Vec<Issue>) + Send + Sync>;
pub type Pre = Box<dyn Fn(Option<&Value>) -> Option<Value> + Send + Sync>;
pub type Transform = Box<dyn Fn(Value) -> Value + Send + Sync>;

pub enum Check {
    Min(f64, bool, Option<String>),
    Max(f64, bool),
    Int,
    Url,
    Refine(Refine, String, Path),
    Super(SuperRefine),
}

pub enum Kind {
    Str { trim: bool },
    Num,
    Bool,
    Any,
    Literal(Value),
    Enum(Vec<&'static str>),
    Arr(Box<Schema>),
    Record(Box<Schema>, Box<Schema>),
    Object(Vec<(&'static str, Schema)>, bool),
    Union(Vec<Schema>),
    DUnion(&'static str, Vec<Schema>),
    Optional(Box<Schema>),
    Nullable(Box<Schema>),
    Default(Box<Schema>, Value),
    Pre(Pre, Box<Schema>),
    Transform(Box<Schema>, Transform),
}

pub struct Schema {
    pub kind: Kind,
    pub checks: Vec<Check>,
}

fn s(kind: Kind) -> Schema {
    Schema {
        kind,
        checks: vec![],
    }
}
pub fn string() -> Schema {
    s(Kind::Str { trim: false })
}
pub fn trimmed() -> Schema {
    s(Kind::Str { trim: true })
}
pub fn number() -> Schema {
    s(Kind::Num)
}
pub fn boolean() -> Schema {
    s(Kind::Bool)
}
pub fn any() -> Schema {
    s(Kind::Any)
}
pub fn literal(value: impl Into<Value>) -> Schema {
    s(Kind::Literal(value.into()))
}
pub fn enumeration(values: &[&'static str]) -> Schema {
    s(Kind::Enum(values.to_vec()))
}
pub fn array(item: Schema) -> Schema {
    s(Kind::Arr(Box::new(item)))
}
pub fn record(key: Schema, value: Schema) -> Schema {
    s(Kind::Record(Box::new(key), Box::new(value)))
}
pub fn object(shape: Vec<(&'static str, Schema)>) -> Schema {
    s(Kind::Object(shape, false))
}
pub fn union(options: Vec<Schema>) -> Schema {
    s(Kind::Union(options))
}
pub fn discriminated(key: &'static str, options: Vec<Schema>) -> Schema {
    s(Kind::DUnion(key, options))
}
pub fn preprocess(
    f: impl Fn(Option<&Value>) -> Option<Value> + Send + Sync + 'static,
    inner: Schema,
) -> Schema {
    s(Kind::Pre(Box::new(f), Box::new(inner)))
}

impl Schema {
    pub fn min(mut self, n: f64) -> Self {
        self.checks.push(Check::Min(n, true, None));
        self
    }
    pub fn min_msg(mut self, n: f64, message: &str) -> Self {
        self.checks.push(Check::Min(n, true, Some(message.into())));
        self
    }
    pub fn gt(mut self, n: f64) -> Self {
        self.checks.push(Check::Min(n, false, None));
        self
    }
    pub fn positive(self) -> Self {
        self.gt(0.0)
    }
    pub fn max(mut self, n: f64) -> Self {
        self.checks.push(Check::Max(n, true));
        self
    }
    pub fn lt(mut self, n: f64) -> Self {
        self.checks.push(Check::Max(n, false));
        self
    }
    pub fn int(mut self) -> Self {
        self.checks.push(Check::Int);
        self
    }
    pub fn url(mut self) -> Self {
        self.checks.push(Check::Url);
        self
    }
    pub fn refine(
        mut self,
        f: impl Fn(&Value) -> bool + Send + Sync + 'static,
        message: &str,
    ) -> Self {
        self.checks
            .push(Check::Refine(Box::new(f), message.into(), vec![]));
        self
    }
    pub fn refine_at(
        mut self,
        f: impl Fn(&Value) -> bool + Send + Sync + 'static,
        message: &str,
        path: Path,
    ) -> Self {
        self.checks
            .push(Check::Refine(Box::new(f), message.into(), path));
        self
    }
    pub fn super_refine(
        mut self,
        f: impl Fn(&Value, &Path, &mut Vec<Issue>) + Send + Sync + 'static,
    ) -> Self {
        self.checks.push(Check::Super(Box::new(f)));
        self
    }
    pub fn strict(mut self) -> Self {
        if let Kind::Object(_, strict) = &mut self.kind {
            *strict = true;
        }
        self
    }
    pub fn optional(self) -> Self {
        s(Kind::Optional(Box::new(self)))
    }
    pub fn nullable(self) -> Self {
        s(Kind::Nullable(Box::new(self)))
    }
    pub fn default(self, value: impl Into<Value>) -> Self {
        s(Kind::Default(Box::new(self), value.into()))
    }
    pub fn transform(self, f: impl Fn(Value) -> Value + Send + Sync + 'static) -> Self {
        s(Kind::Transform(Box::new(self), Box::new(f)))
    }
    /// Extend an object schema with more keys (zod `.extend`, later keys win).
    pub fn extend(mut self, extra: Vec<(&'static str, Schema)>) -> Self {
        if let Kind::Object(shape, _) = &mut self.kind {
            for (key, schema) in extra {
                if let Some(slot) = shape.iter_mut().find(|(k, _)| *k == key) {
                    slot.1 = schema;
                } else {
                    shape.push((key, schema));
                }
            }
        }
        self
    }

    /// Parse like `schema.safeParse(input)`; `Err` holds finalized issues.
    pub fn safe_parse(&self, input: &Value) -> Result<Value, Vec<Value>> {
        let mut issues = Vec::new();
        let out = self.run(Some(input), &vec![], &mut issues);
        if issues.is_empty() {
            Ok(out.unwrap_or(Value::Null))
        } else {
            Err(issues.iter().map(Issue::to_json).collect())
        }
    }

    fn run(&self, input: Option<&Value>, path: &Path, issues: &mut Vec<Issue>) -> Option<Value> {
        let start = issues.len();
        let out = self.parse_kind(input, path, issues);
        if !self.checks.is_empty() {
            for check in &self.checks {
                if issues[start..].iter().any(|i| i.abort) {
                    break;
                }
                if let Some(value) = &out {
                    run_check(check, value, path, issues);
                }
            }
        }
        out
    }

    fn parse_kind(
        &self,
        input: Option<&Value>,
        path: &Path,
        issues: &mut Vec<Issue>,
    ) -> Option<Value> {
        match &self.kind {
            Kind::Optional(inner) => match input {
                None => {
                    if matches!(inner.kind, Kind::Default(..)) {
                        inner.run(None, path, issues)
                    } else {
                        None
                    }
                }
                some => inner.run(some, path, issues),
            },
            Kind::Nullable(inner) => match input {
                Some(Value::Null) => Some(Value::Null),
                other => inner.run(other, path, issues),
            },
            Kind::Default(inner, value) => match input {
                None => Some(value.clone()),
                some => inner.run(some, path, issues),
            },
            Kind::Pre(f, inner) => {
                let pre = f(input);
                inner.run(pre.as_ref(), path, issues)
            }
            Kind::Transform(inner, f) => {
                let start = issues.len();
                let out = inner.run(input, path, issues);
                if issues.len() == start {
                    out.map(f)
                } else {
                    out
                }
            }
            Kind::Any => input.cloned(),
            Kind::Str { trim } => match input {
                Some(Value::String(v)) => Some(Value::String(if *trim {
                    crate::engine::responses::js_ext::js_trim(v).to_owned()
                } else {
                    v.clone()
                })),
                other => {
                    issues.push(invalid_type("string", other, path));
                    None
                }
            },
            Kind::Num => match input {
                Some(Value::Number(_)) => input.cloned(),
                other => {
                    issues.push(invalid_type("number", other, path));
                    None
                }
            },
            Kind::Bool => match input {
                Some(Value::Bool(_)) => input.cloned(),
                other => {
                    issues.push(invalid_type("boolean", other, path));
                    None
                }
            },
            Kind::Literal(expected) => {
                if input == Some(expected) {
                    input.cloned()
                } else {
                    issues.push(
                        Issue::new(
                            "invalid_value",
                            path,
                            format!("Invalid input: expected {}", js_repr(expected)),
                            true,
                        )
                        .with("values", Value::Array(vec![expected.clone()])),
                    );
                    None
                }
            }
            Kind::Enum(values) => match input {
                Some(Value::String(v)) if values.contains(&v.as_str()) => input.cloned(),
                _ => {
                    let list = values
                        .iter()
                        .map(|v| format!("\"{v}\""))
                        .collect::<Vec<_>>()
                        .join("|");
                    issues.push(
                        Issue::new(
                            "invalid_value",
                            path,
                            format!("Invalid option: expected one of {list}"),
                            true,
                        )
                        .with(
                            "values",
                            Value::Array(values.iter().map(|v| Value::from(*v)).collect()),
                        ),
                    );
                    None
                }
            },
            Kind::Arr(item) => match input {
                Some(Value::Array(items)) => Some(Value::Array(
                    items
                        .iter()
                        .enumerate()
                        .map(|(i, v)| {
                            let mut p = path.clone();
                            p.push(Value::from(i));
                            item.run(Some(v), &p, issues).unwrap_or(Value::Null)
                        })
                        .collect(),
                )),
                other => {
                    issues.push(invalid_type("array", other, path));
                    None
                }
            },
            Kind::Record(key, value) => match input {
                Some(Value::Object(map)) => {
                    let mut out = Map::new();
                    for (k, v) in map {
                        let mut p = path.clone();
                        p.push(Value::from(k.clone()));
                        let mut key_issues = Vec::new();
                        let key_out =
                            key.run(Some(&Value::String(k.clone())), &vec![], &mut key_issues);
                        if !key_issues.is_empty() {
                            issues.push(
                                Issue::new("invalid_key", &p, "Invalid key in record".into(), true)
                                    .with("origin", "record")
                                    .with(
                                        "issues",
                                        Value::Array(
                                            key_issues.iter().map(Issue::to_json).collect(),
                                        ),
                                    ),
                            );
                            continue;
                        }
                        let key_name = key_out
                            .and_then(|k| k.as_str().map(str::to_owned))
                            .unwrap_or_else(|| k.clone());
                        if let Some(o) = value.run(Some(v), &p, issues) {
                            out.insert(key_name, o);
                        }
                    }
                    Some(Value::Object(out))
                }
                other => {
                    issues.push(invalid_type("record", other, path));
                    None
                }
            },
            Kind::Object(shape, strict) => match input {
                Some(Value::Object(map)) => {
                    let mut out = Map::new();
                    for (key, schema) in shape {
                        let mut p = path.clone();
                        p.push(Value::from(*key));
                        if let Some(o) = schema.run(map.get(*key), &p, issues) {
                            out.insert((*key).into(), o);
                        }
                    }
                    if *strict {
                        let unknown: Vec<&String> = map
                            .keys()
                            .filter(|k| !shape.iter().any(|(s, _)| s == k))
                            .collect();
                        if !unknown.is_empty() {
                            let quoted = unknown
                                .iter()
                                .map(|k| format!("\"{k}\""))
                                .collect::<Vec<_>>()
                                .join(", ");
                            let message = if unknown.len() == 1 {
                                format!("Unrecognized key: {quoted}")
                            } else {
                                format!("Unrecognized keys: {quoted}")
                            };
                            issues.push(
                                Issue::new("unrecognized_keys", path, message, true).with(
                                    "keys",
                                    Value::Array(
                                        unknown
                                            .into_iter()
                                            .map(|k| Value::from(k.clone()))
                                            .collect(),
                                    ),
                                ),
                            );
                        }
                    }
                    Some(Value::Object(out))
                }
                other => {
                    issues.push(invalid_type("object", other, path));
                    None
                }
            },
            Kind::Union(options) => {
                let mut results = Vec::new();
                for option in options {
                    let mut branch = Vec::new();
                    let out = option.run(input, path, &mut branch);
                    if branch.is_empty() {
                        return out;
                    }
                    results.push((out, branch));
                }
                let nonaborted: Vec<usize> = results
                    .iter()
                    .enumerate()
                    .filter(|(_, (_, b))| !b.iter().any(|i| i.abort))
                    .map(|(i, _)| i)
                    .collect();
                if nonaborted.len() == 1 {
                    let (out, branch) = results.swap_remove(nonaborted[0]);
                    issues.extend(branch);
                    return out;
                }
                let errors = results
                    .iter()
                    .map(|(_, b)| {
                        Value::Array(b.iter().map(|i| i.relative_to(path.len())).collect())
                    })
                    .collect();
                issues.push(
                    Issue::new("invalid_union", path, "Invalid input".into(), true)
                        .with("errors", Value::Array(errors)),
                );
                None
            }
            Kind::DUnion(key, options) => {
                let Some(Value::Object(map)) = input else {
                    issues.push(invalid_type("object", input, path));
                    return None;
                };
                let literal_of = |schema: &Schema| -> Option<Value> {
                    if let Kind::Object(shape, _) = &schema.kind {
                        shape.iter().find(|(k, _)| k == key).and_then(|(_, s)| {
                            if let Kind::Literal(v) = &s.kind {
                                Some(v.clone())
                            } else {
                                None
                            }
                        })
                    } else {
                        None
                    }
                };
                if let Some(option) = options
                    .iter()
                    .find(|o| literal_of(o).as_ref() == map.get(*key))
                {
                    return option.run(input, path, issues);
                }
                let values: Vec<Value> = options.iter().filter_map(literal_of).collect();
                let names = values
                    .iter()
                    .map(|v| format!("'{}'", v.as_str().unwrap_or_default()))
                    .collect::<Vec<_>>()
                    .join(" | ");
                let mut p = path.clone();
                p.push(Value::from(*key));
                issues.push(
                    Issue::new(
                        "invalid_union",
                        &p,
                        format!("Invalid discriminator value. Expected {names}"),
                        true,
                    )
                    .with("errors", Value::Array(vec![]))
                    .with("note", "No matching discriminator")
                    .with("discriminator", *key)
                    .with("options", Value::Array(values)),
                );
                None
            }
        }
    }
}

fn run_check(check: &Check, value: &Value, path: &Path, issues: &mut Vec<Issue>) {
    match check {
        Check::Min(n, inclusive, _) | Check::Max(n, inclusive) => {
            let is_min = matches!(check, Check::Min(..));
            let (origin, size, unit) = match value {
                Value::Number(v) => ("number", v.as_f64().unwrap_or(0.0), ""),
                Value::String(v) => ("string", v.encode_utf16().count() as f64, " characters"),
                Value::Array(v) => ("array", v.len() as f64, " items"),
                _ => return,
            };
            let ok = match (is_min, inclusive) {
                (true, true) => size >= *n,
                (true, false) => size > *n,
                (false, true) => size <= *n,
                (false, false) => size < *n,
            };
            if ok {
                return;
            }
            let op = match (is_min, inclusive) {
                (true, true) => ">=",
                (true, false) => ">",
                (false, true) => "<=",
                (false, false) => "<",
            };
            let (code, bound, word) = if is_min {
                ("too_small", "minimum", "Too small")
            } else {
                ("too_big", "maximum", "Too big")
            };
            let verb = if origin == "number" {
                "to be"
            } else {
                "to have"
            };
            let custom = if let Check::Min(_, _, Some(message)) = check {
                Some(message.clone())
            } else {
                None
            };
            issues.push(
                Issue::new(
                    code,
                    path,
                    custom.unwrap_or_else(|| {
                        format!("{word}: expected {origin} {verb} {op}{}{unit}", js_num(*n))
                    }),
                    false,
                )
                .with("origin", origin)
                .with(bound, num_value(*n))
                .with("inclusive", *inclusive),
            );
        }
        Check::Int => {
            let Some(v) = value.as_f64() else { return };
            if v.fract() != 0.0 {
                issues.push(
                    Issue::new(
                        "invalid_type",
                        path,
                        "Invalid input: expected int, received number".into(),
                        false,
                    )
                    .with("expected", "int")
                    .with("format", "safeint"),
                );
            } else if v.abs() > 9007199254740991.0 {
                let (code, bound, word, op) = if v > 0.0 {
                    ("too_big", "maximum", "Too big", "<=")
                } else {
                    ("too_small", "minimum", "Too small", ">=")
                };
                let limit = if v > 0.0 {
                    9007199254740991i64
                } else {
                    -9007199254740991i64
                };
                issues.push(
                    Issue::new(
                        code,
                        path,
                        format!("{word}: expected int to be {op}{limit}"),
                        false,
                    )
                    .with("origin", "int")
                    .with(bound, limit)
                    .with("inclusive", true)
                    .with("note", "Integers must be within the safe integer range."),
                );
            }
        }
        Check::Url => {
            if value
                .as_str()
                .is_some_and(|v| url::Url::parse(v.trim()).is_err())
            {
                issues.push(
                    Issue::new("invalid_format", path, "Invalid URL".into(), false)
                        .with("format", "url"),
                );
            }
        }
        Check::Refine(f, message, at) => {
            if !f(value) {
                let mut p = path.clone();
                p.extend(at.iter().cloned());
                issues.push(Issue::custom(&p, message.clone()));
            }
        }
        Check::Super(f) => f(value, path, issues),
    }
}

fn received(value: Option<&Value>) -> &'static str {
    match value {
        None => "undefined",
        Some(Value::Null) => "null",
        Some(Value::Bool(_)) => "boolean",
        Some(Value::Number(_)) => "number",
        Some(Value::String(_)) => "string",
        Some(Value::Array(_)) => "array",
        Some(Value::Object(_)) => "object",
    }
}

fn invalid_type(expected: &str, value: Option<&Value>, path: &Path) -> Issue {
    Issue::new(
        "invalid_type",
        path,
        format!(
            "Invalid input: expected {expected}, received {}",
            received(value)
        ),
        true,
    )
    .with("expected", expected)
}

/// JavaScript `String(number)` for the bounds used in messages.
fn js_num(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e21 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}
fn num_value(n: f64) -> Value {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        Value::from(n as i64)
    } else {
        Value::from(n)
    }
}
fn js_repr(value: &Value) -> String {
    match value {
        Value::String(s) => format!("\"{s}\""),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn first(schema: &Schema, input: Value) -> Value {
        schema.safe_parse(&input).unwrap_err()[0].clone()
    }
    #[test]
    fn issue_shapes_match_zod_4_6() {
        assert_eq!(
            first(&number().int(), json!(1.5)),
            json!({"expected":"int","format":"safeint","code":"invalid_type","path":[],"message":"Invalid input: expected int, received number"})
        );
        assert_eq!(
            first(&number().int().positive(), json!(-1)),
            json!({"origin":"number","code":"too_small","minimum":0,"inclusive":false,"path":[],"message":"Too small: expected number to be >0"})
        );
        assert_eq!(
            first(&number().min(0.0).max(1.0), json!(2)),
            json!({"origin":"number","code":"too_big","maximum":1,"inclusive":true,"path":[],"message":"Too big: expected number to be <=1"})
        );
        assert_eq!(
            first(&trimmed().min(1.0), json!("  ")),
            json!({"origin":"string","code":"too_small","minimum":1,"inclusive":true,"path":[],"message":"Too small: expected string to have >=1 characters"})
        );
        assert_eq!(
            first(&enumeration(&["a", "b"]), json!("c")),
            json!({"code":"invalid_value","values":["a","b"],"path":[],"message":"Invalid option: expected one of \"a\"|\"b\""})
        );
        assert_eq!(
            first(
                &object(vec![("a", string().optional())]).strict(),
                json!({"b":1})
            ),
            json!({"code":"unrecognized_keys","keys":["b"],"path":[],"message":"Unrecognized key: \"b\""})
        );
        assert_eq!(
            first(&union(vec![string(), record(string(), string())]), json!(5)),
            json!({"code":"invalid_union","errors":[[{"expected":"string","code":"invalid_type","path":[],"message":"Invalid input: expected string, received number"}],[{"expected":"record","code":"invalid_type","path":[],"message":"Invalid input: expected record, received number"}]],"path":[],"message":"Invalid input"})
        );
        let du = discriminated(
            "t",
            vec![
                object(vec![("t", literal("a"))]),
                object(vec![("t", literal("b")), ("n", number())]),
            ],
        );
        assert_eq!(
            first(&du, json!({"t":"c"})),
            json!({"code":"invalid_union","errors":[],"note":"No matching discriminator","discriminator":"t","options":["a","b"],"path":["t"],"message":"Invalid discriminator value. Expected 'a' | 'b'"})
        );
        assert_eq!(
            first(&record(string().min(1.0), number()), json!({"":1})),
            json!({"code":"invalid_key","origin":"record","issues":[{"origin":"string","code":"too_small","minimum":1,"inclusive":true,"path":[],"message":"Too small: expected string to have >=1 characters"}],"path":[""],"message":"Invalid key in record"})
        );
        assert_eq!(
            object(vec![
                ("a", boolean().default(true)),
                ("b", number().optional().default(1))
            ])
            .safe_parse(&json!({"x":1}))
            .unwrap(),
            json!({"a":true,"b":1})
        );
        // A refine-only failure in one union branch is returned directly.
        let u = union(vec![
            string(),
            record(string(), string()).refine(|_| false, "nope"),
        ]);
        assert_eq!(
            first(&u, json!({"a":"b"})),
            json!({"code":"custom","path":[],"message":"nope"})
        );
    }
}
