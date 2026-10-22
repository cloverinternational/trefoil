//! `transformers/responses/normalization.ts`: in-place repairs of Responses
//! request bodies observed from real clients. Each returns the repair count.
use serde_json::Value;

pub const OPENAI_RESPONSES_CALL_ID_MAX_LENGTH: usize = 64;
pub const OPENAI_RESPONSES_REASONING_CONTENT_MAX_ITEMS: usize = 0;

fn input_items(body: &mut Value) -> Option<&mut Vec<Value>> {
    body.as_object_mut()?.get_mut("input")?.as_array_mut()
}

/// JS `string.length` (UTF-16 code units).
fn js_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// `normalizeCompositeResponsesCallIds`: `call_…|fc_…` composite ids longer
/// than the OpenAI limit are cut back to the model-generated `call_…` id.
pub fn normalize_composite_responses_call_ids(body: &mut Value) -> usize {
    let Some(items) = input_items(body) else {
        return 0;
    };
    let mut count = 0;
    for item in items.iter_mut() {
        let Some(map) = item.as_object_mut() else {
            continue;
        };
        let Some(call_id) = map.get("call_id").and_then(Value::as_str) else {
            continue;
        };
        if js_len(call_id) <= OPENAI_RESPONSES_CALL_ID_MAX_LENGTH {
            continue;
        }
        let Some(sep) = call_id.find('|').filter(|i| *i > 0) else {
            continue;
        };
        let (head, tail) = (&call_id[..sep], &call_id[sep + 1..]);
        if !head.starts_with("call_") || !tail.starts_with("fc_") {
            continue;
        }
        let head = head.to_owned();
        map.insert("call_id".into(), Value::String(head));
        count += 1;
    }
    count
}

/// `normalizeResponsesFunctionCallItemIds`: drops a `function_call` item's
/// `id` when it is a `call_…` id (strict providers require `fc…`).
pub fn normalize_responses_function_call_item_ids(body: &mut Value) -> usize {
    let Some(items) = input_items(body) else {
        return 0;
    };
    let mut count = 0;
    for item in items.iter_mut() {
        let Some(map) = item.as_object_mut() else {
            continue;
        };
        if map.get("type").and_then(Value::as_str) != Some("function_call") {
            continue;
        }
        if !map
            .get("id")
            .and_then(Value::as_str)
            .is_some_and(|id| id.starts_with("call_"))
        {
            continue;
        }
        map.shift_remove("id");
        count += 1;
    }
    count
}

/// `normalizeResponsesReasoningContent`: empties replayed plaintext
/// reasoning `content` arrays (some providers enforce max length 0).
pub fn normalize_responses_reasoning_content(body: &mut Value) -> usize {
    let Some(items) = input_items(body) else {
        return 0;
    };
    let mut count = 0;
    for item in items.iter_mut() {
        let Some(map) = item.as_object_mut() else {
            continue;
        };
        if map.get("type").and_then(Value::as_str) != Some("reasoning") {
            continue;
        }
        let Some(content) = map.get("content").and_then(Value::as_array) else {
            continue;
        };
        #[allow(clippy::absurd_extreme_comparisons)]
        let within_limit = content.len() <= OPENAI_RESPONSES_REASONING_CONTENT_MAX_ITEMS;
        if within_limit {
            continue;
        }
        map.insert("content".into(), Value::Array(Vec::new()));
        count += 1;
    }
    count
}

/// `normalizeResponsesNullEntries`: removes null entries from `input`, and
/// from each item's `content` / `summary` arrays.
pub fn normalize_responses_null_entries(body: &mut Value) -> usize {
    let Some(items) = input_items(body) else {
        return 0;
    };
    let mut removed = 0;
    let mut without_nulls = |entries: &mut Vec<Value>| {
        let before = entries.len();
        entries.retain(|e| !e.is_null());
        removed += before - entries.len();
    };
    without_nulls(items);
    for item in items.iter_mut() {
        // `typeof item !== 'object'` skips primitives; arrays have no
        // `content`/`summary` members.
        let Some(map) = item.as_object_mut() else {
            continue;
        };
        if let Some(Value::Array(content)) = map.get_mut("content") {
            without_nulls(content);
        }
        if let Some(Value::Array(summary)) = map.get_mut("summary") {
            without_nulls(summary);
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn composite_call_ids() {
        let long =
            "call_enS4L7YycCRyOiWOg31Xpvwm|fc_0281edd961557cf2016a4b062d87948195968b8fa6c46b8c7a";
        let mut body = json!({"input": [
            {"type": "function_call", "call_id": "call_short|fc_short"},
            {"type": "function_call", "call_id": long},
            {"type": "function_call_output", "call_id": long},
            {"type": "function_call", "call_id": "call_plain"},
            {"type": "function_call", "call_id": "custom|id"},
        ]});
        assert_eq!(normalize_composite_responses_call_ids(&mut body), 2);
        let ids: Vec<&str> = body["input"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["call_id"].as_str().unwrap())
            .collect();
        assert_eq!(
            ids,
            [
                "call_short|fc_short",
                "call_enS4L7YycCRyOiWOg31Xpvwm",
                "call_enS4L7YycCRyOiWOg31Xpvwm",
                "call_plain",
                "custom|id"
            ]
        );
    }

    #[test]
    fn item_ids_reasoning_and_nulls() {
        let mut body = json!({"input": [{"type": "function_call", "id": "call_1", "call_id": "call_1"}, {"type": "function_call", "id": "fc_1"}]});
        assert_eq!(normalize_responses_function_call_item_ids(&mut body), 1);
        assert_eq!(
            body["input"][0],
            json!({"type": "function_call", "call_id": "call_1"})
        );
        let mut body = json!({"input": [{"type": "reasoning", "content": [{"type": "reasoning_text", "text": "x"}], "summary": []}]});
        assert_eq!(normalize_responses_reasoning_content(&mut body), 1);
        assert_eq!(body["input"][0]["content"], json!([]));
        let mut body = json!({"model": "gpt-4o", "input": [null, {"type": "message", "role": "user", "content": [null, {"type": "input_text", "text": "hi"}]}, {"type": "reasoning", "summary": [null, {"type": "summary_text", "text": "t"}]}]});
        assert_eq!(normalize_responses_null_entries(&mut body), 3);
        assert_eq!(
            body["input"],
            json!([{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}, {"type": "reasoning", "summary": [{"type": "summary_text", "text": "t"}]}])
        );
        let mut s = json!({"input": "hi"});
        assert_eq!(normalize_responses_null_entries(&mut s), 0);
    }
}
