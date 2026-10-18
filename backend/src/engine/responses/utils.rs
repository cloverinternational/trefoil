//! `transformers/responses/utils.ts`.
use super::js_ext::{random_fraction_base36, to_base36};
use crate::engine::js::*;
use serde_json::{json, Value};

/// `resp_${Date.now().toString(36)}${Math.random().toString(36).substring(2, 15)}`.
pub fn generate_response_id() -> String { generate_item_id("resp") }

/// `${prefix}_${Date.now().toString(36)}${Math.random().toString(36).substring(2, 15)}`.
pub fn generate_item_id(prefix: &str) -> String {
    format!("{prefix}_{}{}", to_base36(now_ms().max(0) as u64), random_fraction_base36())
}

/// `toUnifiedImageGenerationCall`: id/status only when strings; result
/// carried byte-intact (`String(item.result ?? '')` for non-strings).
pub fn to_unified_image_generation_call(item: &Value) -> Value {
    let result = match item.get("result") {
        Some(Value::String(s)) => s.clone(),
        other => {
            let v = coalesce(other, Some(&json!("")));
            to_string(v.as_ref())
        }
    };
    obj(vec![
        ("id", item.get("id").filter(|v| v.is_string()).cloned()),
        ("status", item.get("status").filter(|v| v.is_string()).cloned()),
        ("result", Some(Value::String(result))),
    ])
}

/// `usage.key || 0` as a JS number.
fn or_zero(usage: &Value, key: &str) -> Value { or(usage.get(key), Some(&json!(0))).unwrap_or(json!(0)) }

/// `buildResponsesUsagePayload`: wire `usage` for every Responses payload.
/// `input_tokens` is the TOTAL input (uncached + cached + cache writes);
/// `image_tokens` appears only when the unified usage carries it.
pub fn build_responses_usage_payload(usage: Option<&Value>) -> Option<Value> {
    let usage = usage.filter(|u| truthy(Some(u)))?;
    let n = |key: &str| if truthy(usage.get(key)) { usage[key].as_f64().unwrap_or(0.0) } else { 0.0 };
    let mut input_details = Obj::new();
    input_details.insert("cached_tokens".into(), or_zero(usage, "cached_tokens"));
    set_opt(&mut input_details, "image_tokens", usage.get("input_image_tokens").cloned());
    let mut output_details = Obj::new();
    output_details.insert("reasoning_tokens".into(), or_zero(usage, "reasoning_tokens"));
    set_opt(&mut output_details, "image_tokens", usage.get("output_image_tokens").cloned());
    Some(obj(vec![
        ("input_tokens", Some(num(n("input_tokens") + n("cached_tokens") + n("cache_creation_tokens")))),
        ("output_tokens", usage.get("output_tokens").cloned()),
        ("total_tokens", usage.get("total_tokens").cloned()),
        ("input_tokens_details", Some(Value::Object(input_details))),
        ("output_tokens_details", Some(Value::Object(output_details))),
    ]))
}

/// `isClientExecutedToolItem`: a built-in `*_call` item whose execution the
/// model delegated to the client (`execution: "client"`), with an id or call_id.
pub fn is_client_executed_tool_item(item: Option<&Value>) -> bool {
    let Some(item @ Value::Object(_)) = item else { return false };
    get_str(item, &["execution"]) == Some("client")
        && get_str(item, &["type"]).is_some_and(|t| t.ends_with("_call"))
        && (item.get("call_id").is_some_and(Value::is_string) || item.get("id").is_some_and(Value::is_string))
}

/// `clientToolCallKey`: `id`, else `call_id` (dedupe key).
pub fn client_tool_call_key(item: Option<&Value>) -> Option<String> {
    let item = item.filter(|i| i.is_object() || i.is_array())?;
    get_str(item, &["id"]).or_else(|| get_str(item, &["call_id"])).map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_payload_totals_input_and_keeps_optional_image_tokens() {
        let u = json!({"input_tokens": 2571, "output_tokens": 416, "total_tokens": 17963, "reasoning_tokens": 0, "cached_tokens": 14976, "cache_creation_tokens": 0});
        assert_eq!(
            stringify(&build_responses_usage_payload(Some(&u)).unwrap()),
            r#"{"input_tokens":17547,"output_tokens":416,"total_tokens":17963,"input_tokens_details":{"cached_tokens":14976},"output_tokens_details":{"reasoning_tokens":0}}"#
        );
        let u = json!({"input_tokens": 1, "input_image_tokens": 7, "output_image_tokens": 0});
        let p = build_responses_usage_payload(Some(&u)).unwrap();
        assert_eq!(p["input_tokens_details"], json!({"cached_tokens": 0, "image_tokens": 7}));
        assert_eq!(p["output_tokens_details"], json!({"reasoning_tokens": 0, "image_tokens": 0}));
        assert!(p.get("output_tokens").is_none());
        assert_eq!(build_responses_usage_payload(None), None);
    }

    #[test]
    fn ids_and_client_items() {
        let id = generate_item_id("fc");
        assert!(id.starts_with("fc_") && id.len() == 3 + 8 + 11);
        assert!(is_client_executed_tool_item(Some(&json!({"type": "tool_search_call", "execution": "client", "id": "x"}))));
        assert!(!is_client_executed_tool_item(Some(&json!({"type": "tool_search", "execution": "client", "id": "x"}))));
        assert!(!is_client_executed_tool_item(Some(&json!({"type": "tool_search_call", "execution": "server", "id": "x"}))));
        assert_eq!(client_tool_call_key(Some(&json!({"call_id": "c"}))), Some("c".into()));
        assert_eq!(to_unified_image_generation_call(&json!({"id": 1, "status": "completed"})), json!({"status": "completed", "result": ""}));
    }
}
