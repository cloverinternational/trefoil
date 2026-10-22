//! `transformers/ollama.ts`: native Ollama `/api/chat` (NDJSON streaming).
use crate::engine::image_rendering::compose_content;
use crate::engine::js::*;
use crate::engine::sse::{self, SseEvent};
use crate::engine::transformer::{EngineResult, StreamFormatter, StreamTransformer, Transformer};
use serde_json::{json, Value};

pub struct OllamaTransformer;

fn created(v: Option<&Value>) -> Value {
    match v.filter(|c| truthy(Some(c))).map(|c| to_string(Some(c))) {
        Some(s) => match chrono::DateTime::parse_from_rfc3339(&s) {
            Ok(d) => json!(d.timestamp_millis().div_euclid(1000)),
            Err(_) => Value::Null, // NaN
        },
        None => json!(now_secs()),
    }
}

fn usage_from_response(data: &Value) -> Option<Value> {
    let prompt = coalesce(
        data.get("prompt_eval_count"),
        data.get("prompt_eval_duration"),
    );
    let eval = coalesce(data.get("eval_count"), data.get("eval_duration"));
    if prompt.is_none() && eval.is_none() {
        return None;
    }
    let p = prompt.filter(|v| !v.is_null()).unwrap_or(json!(0));
    let e = eval.filter(|v| !v.is_null()).unwrap_or(json!(0));
    let total =
        num(to_number(Some(&p)).unwrap_or(f64::NAN) + to_number(Some(&e)).unwrap_or(f64::NAN));
    Some(
        json!({"input_tokens": p, "output_tokens": e, "total_tokens": total, "reasoning_tokens": 0, "cached_tokens": 0, "cache_creation_tokens": 0}),
    )
}

fn client_usage(u: &Value) -> Value {
    let input = to_number(u.get("input_tokens")).unwrap_or(f64::NAN);
    let cached = if truthy(u.get("cached_tokens")) {
        to_number(u.get("cached_tokens")).unwrap_or(0.0)
    } else {
        0.0
    };
    obj(vec![
        ("prompt_tokens", Some(num(input + cached))),
        ("completion_tokens", u.get("output_tokens").cloned()),
        ("total_tokens", u.get("total_tokens").cloned()),
        ("reasoning_tokens", u.get("reasoning_tokens").cloned()),
    ])
}

/// `parseOllamaChunk(data)`.
fn parse_chunk(data: &Value) -> Value {
    let message = data
        .get("message")
        .filter(|m| truthy(Some(m)))
        .cloned()
        .unwrap_or(json!({}));
    let content = message.get("content").filter(|c| truthy(Some(c))).cloned();
    let is_done = data.get("done") == Some(&Value::Bool(true));
    let done_reason = data
        .get("done_reason")
        .filter(|r| truthy(Some(r)))
        .cloned()
        .or_else(|| is_done.then(|| json!("stop")));
    let usage = is_done.then(|| {
        let p = coalesce(data.get("prompt_eval_count"), None).filter(|v| !v.is_null()).unwrap_or(json!(0));
        let e = coalesce(data.get("eval_count"), None).filter(|v| !v.is_null()).unwrap_or(json!(0));
        let total = num(to_number(Some(&p)).unwrap_or(f64::NAN) + to_number(Some(&e)).unwrap_or(f64::NAN));
        json!({"input_tokens": p, "output_tokens": e, "total_tokens": total, "reasoning_tokens": 0, "cached_tokens": 0, "cache_creation_tokens": 0})
    });
    obj(vec![
        (
            "id",
            Some(
                data.get("id")
                    .filter(|v| truthy(Some(v)))
                    .cloned()
                    .unwrap_or_else(|| json!(format!("ollama-{}", now_ms()))),
            ),
        ),
        ("model", data.get("model").cloned()),
        ("created", Some(created(data.get("created_at")))),
        (
            "delta",
            Some(obj(vec![
                (
                    "role",
                    Some(
                        message
                            .get("role")
                            .filter(|r| truthy(Some(r)))
                            .cloned()
                            .unwrap_or(json!("assistant")),
                    ),
                ),
                ("content", content),
                ("tool_calls", message.get("tool_calls").cloned()),
            ])),
        ),
        ("finish_reason", Some(done_reason.unwrap_or(Value::Null))),
        ("usage", usage),
    ])
}

impl Transformer for OllamaTransformer {
    fn name(&self) -> &'static str {
        "ollama"
    }
    fn default_endpoint(&self) -> &'static str {
        "/api/chat"
    }

    fn parse_request(&self, input: &Value) -> EngineResult<Value> {
        let messages: Vec<Value> = get_arr(input, &["messages"])
            .map(|ms| {
                ms.iter()
                    .map(|m| {
                        obj(vec![
                            ("role", m.get("role").cloned()),
                            ("content", m.get("content").cloned()),
                        ])
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(obj(vec![
            ("messages", Some(Value::Array(messages))),
            ("model", input.get("model").cloned()),
            (
                "max_tokens",
                get(input, &["options", "num_predict"]).cloned(),
            ),
            (
                "temperature",
                get(input, &["options", "temperature"]).cloned(),
            ),
            ("stream", input.get("stream").cloned()),
            ("tools", input.get("tools").cloned()),
        ]))
    }

    fn transform_request(&self, request: &Value) -> EngineResult<Value> {
        let messages: Vec<Value> = get_arr(request, &["messages"])
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|m| {
                let content = match m.get("content") {
                    Some(Value::String(s)) => s.clone(),
                    Some(Value::Null) => String::new(),
                    Some(Value::Array(parts)) => parts
                        .iter()
                        .filter(|p| get_str(p, &["type"]) == Some("text"))
                        .map(|p| to_string(p.get("text")))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    other => to_string(other),
                };
                obj(vec![
                    ("role", m.get("role").cloned()),
                    ("content", Some(json!(content))),
                ])
            })
            .collect();
        let mut out = obj(vec![
            ("model", request.get("model").cloned()),
            ("messages", Some(Value::Array(messages))),
            (
                "stream",
                Some(
                    coalesce(request.get("stream"), None)
                        .filter(|v| !v.is_null())
                        .unwrap_or(json!(false)),
                ),
            ),
        ]);
        let mut options = Obj::new();
        set_opt(
            &mut options,
            "num_predict",
            request.get("max_tokens").cloned(),
        );
        set_opt(
            &mut options,
            "temperature",
            request.get("temperature").cloned(),
        );
        if !options.is_empty() {
            out["options"] = Value::Object(options);
        }
        if let Some(tools) = get_arr(request, &["tools"]).filter(|t| !t.is_empty()) {
            out["tools"] = Value::Array(tools.iter().map(|t| {
                if get_str(t, &["type"]) == Some("function") && truthy(t.get("function")) {
                    let f = &t["function"];
                    json!({"type": "function", "function": obj(vec![
                        ("name", f.get("name").cloned()),
                        ("description", f.get("description").cloned()),
                        ("parameters", or(f.get("parameters"), f.get("parametersJsonSchema"))),
                    ])})
                } else { t.clone() }
            }).collect());
        }
        Ok(out)
    }

    fn transform_response(&self, response: &Value) -> EngineResult<Value> {
        let message = response
            .get("message")
            .filter(|m| truthy(Some(m)))
            .cloned()
            .unwrap_or(json!({}));
        let finish = response
            .get("done_reason")
            .filter(|r| truthy(Some(r)))
            .cloned()
            .unwrap_or_else(|| {
                if truthy(response.get("done")) {
                    json!("stop")
                } else {
                    Value::Null
                }
            });
        Ok(obj(vec![
            (
                "id",
                Some(
                    response
                        .get("id")
                        .filter(|v| truthy(Some(v)))
                        .cloned()
                        .unwrap_or_else(|| json!(format!("ollama-{}", now_ms()))),
                ),
            ),
            ("model", response.get("model").cloned()),
            ("created", Some(created(response.get("created_at")))),
            (
                "content",
                Some(
                    message
                        .get("content")
                        .filter(|c| truthy(Some(c)))
                        .cloned()
                        .unwrap_or(Value::Null),
                ),
            ),
            ("tool_calls", message.get("tool_calls").cloned()),
            ("usage", usage_from_response(response)),
            ("finishReason", Some(finish)),
        ]))
    }

    fn format_response(&self, response: &Value) -> EngineResult<Value> {
        let finish = if truthy(response.get("tool_calls")) {
            json!("tool_calls")
        } else {
            or(response.get("finishReason"), Some(&json!("stop"))).unwrap_or(Value::Null)
        };
        Ok(obj(vec![
            ("id", response.get("id").cloned()),
            ("object", Some(json!("chat.completion"))),
            (
                "created",
                Some(or(response.get("created"), Some(&json!(now_secs()))).unwrap_or(Value::Null)),
            ),
            ("model", response.get("model").cloned()),
            (
                "choices",
                Some(json!([{
                    "index": 0,
                    "message": obj(vec![
                        ("role", Some(json!("assistant"))),
                        ("content", Some(compose_content(response.get("content"), response.get("image_generation_calls")).unwrap_or(Value::Null))),
                        ("tool_calls", response.get("tool_calls").cloned()),
                    ]),
                    "finish_reason": finish,
                }])),
            ),
            (
                "usage",
                response
                    .get("usage")
                    .filter(|u| truthy(Some(u)))
                    .map(client_usage),
            ),
        ]))
    }

    fn stream_transformer(&self) -> Option<Box<dyn StreamTransformer>> {
        Some(Box::new(NdjsonStream {
            buffer: String::new(),
        }))
    }
    fn stream_formatter(&self) -> Option<Box<dyn StreamFormatter>> {
        Some(Box::new(OllamaFormatter))
    }

    fn extract_usage(&self, data: &str) -> Option<Value> {
        parse(data).and_then(|d| usage_from_response(&d))
    }
}

struct NdjsonStream {
    buffer: String,
}

impl StreamTransformer for NdjsonStream {
    fn on_event(&mut self, _event: &SseEvent) -> Vec<Value> {
        Vec::new()
    }
    fn wants_raw(&self) -> bool {
        true
    }
    fn on_raw(&mut self, text: &str) -> Vec<Value> {
        self.buffer.push_str(text);
        let mut lines: Vec<String> = self.buffer.split('\n').map(str::to_owned).collect();
        self.buffer = lines.pop().unwrap_or_default();
        lines
            .iter()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| parse(l))
            .map(|d| parse_chunk(&d))
            .collect()
    }
    fn finish(&mut self) -> Vec<Value> {
        let rest = std::mem::take(&mut self.buffer);
        if rest.trim().is_empty() {
            return Vec::new();
        }
        parse(&rest)
            .map(|d| vec![parse_chunk(&d)])
            .unwrap_or_default()
    }
}

struct OllamaFormatter;

impl StreamFormatter for OllamaFormatter {
    fn on_chunk(&mut self, c: &Value) -> Vec<String> {
        let mut chunk = obj(vec![
            ("id", c.get("id").cloned()),
            ("object", Some(json!("chat.completion.chunk"))),
            (
                "created",
                Some(or(c.get("created"), Some(&json!(now_secs()))).unwrap_or(Value::Null)),
            ),
            ("model", c.get("model").cloned()),
            (
                "choices",
                Some(json!([obj(vec![
                    ("index", Some(json!(0))),
                    ("delta", c.get("delta").cloned()),
                    ("finish_reason", c.get("finish_reason").cloned())
                ])])),
            ),
        ]);
        if let Some(u) = c.get("usage").filter(|u| truthy(Some(u))) {
            chunk["usage"] = client_usage(u);
        }
        vec![sse::data(&stringify(&chunk))]
    }
    fn finish(&mut self) -> Vec<String> {
        vec!["data: [DONE]\n\n".into()]
    }
}
