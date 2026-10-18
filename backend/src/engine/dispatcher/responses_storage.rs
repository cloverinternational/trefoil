//! `services/responses/responses-storage.ts` over the `responses`,
//! `response_items` and `conversations` tables.
use crate::engine::js::*;
use serde_json::{json, Value};
use sqlx::{PgPool, Row};

fn json_or_null(v: Option<&Value>) -> Option<String> { v.filter(|x| truthy(Some(x))).map(stringify) }
fn str_or_null(v: Option<&Value>) -> Option<String> { v.filter(|x| truthy(Some(x))).map(|x| to_string(Some(x))) }
fn int_or_zero(v: Option<&Value>) -> i64 { v.filter(|x| truthy(Some(x))).and_then(Value::as_f64).unwrap_or(0.0) as i64 }

/// `storeResponse(response, request)`.
pub async fn store_response(pool: &PgPool, response: &Value, request: &Value) -> Result<(), sqlx::Error> {
    let id = to_string(response.get("id"));
    let output = response.get("output").cloned().unwrap_or(Value::Null);
    let usage = response.get("usage");
    let conversation = match request.get("conversation") {
        Some(Value::String(s)) => Some(s.clone()),
        Some(c) => str_or_null(c.get("id")),
        None => None,
    };
    let num_or_null = |k: &str| request.get(k).filter(|v| !v.is_null()).and_then(Value::as_f64);
    sqlx::query("INSERT INTO responses (id, object, created_at, completed_at, status, model, output_items, instructions, temperature, top_p, max_output_tokens, top_logprobs, parallel_tool_calls, tool_choice, tools, text_config, reasoning_config, usage_input_tokens, usage_output_tokens, usage_reasoning_tokens, usage_cached_tokens, usage_total_tokens, previous_response_id, conversation_id, store, background, truncation, incomplete_details, error, safety_identifier, service_tier, prompt_cache_key, prompt_cache_retention, metadata, plexus_provider, plexus_target_model, plexus_api_type, plexus_canonical_model) VALUES ($1, 'response', $2, $3, $4, $5, $6, $7, $8::double precision, $9::double precision, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, $22, $23, $24, $25, $26, $27, $28, $29, $30, $31, $32, $33, $34, $35, $36, $37)")
        .bind(&id)
        .bind(response.get("created_at").and_then(Value::as_f64).map(|v| v as i64).unwrap_or(0))
        .bind(response.get("completed_at").filter(|v| truthy(Some(v))).and_then(Value::as_f64).map(|v| v as i64))
        .bind(to_string(response.get("status")))
        .bind(to_string(response.get("model")))
        .bind(stringify(&output))
        .bind(str_or_null(request.get("instructions")))
        .bind(num_or_null("temperature"))
        .bind(num_or_null("top_p"))
        .bind(num_or_null("max_output_tokens").map(|v| v as i32))
        .bind(num_or_null("top_logprobs").map(|v| v as i32))
        .bind(if truthy(request.get("parallel_tool_calls")) { 1i32 } else { 0 })
        .bind(json_or_null(request.get("tool_choice")))
        .bind(json_or_null(request.get("tools")))
        .bind(json_or_null(request.get("text")))
        .bind(json_or_null(request.get("reasoning")))
        .bind(int_or_zero(usage.and_then(|u| u.get("input_tokens"))) as i32)
        .bind(int_or_zero(usage.and_then(|u| u.get("output_tokens"))) as i32)
        .bind(int_or_zero(usage.and_then(|u| get(u, &["output_tokens_details", "reasoning_tokens"]))) as i32)
        .bind(int_or_zero(usage.and_then(|u| get(u, &["input_tokens_details", "cached_tokens"]))) as i32)
        .bind(int_or_zero(usage.and_then(|u| u.get("total_tokens"))) as i32)
        .bind(str_or_null(request.get("previous_response_id")))
        .bind(conversation)
        .bind(if request.get("store") != Some(&Value::Bool(false)) { 1i32 } else { 0 })
        .bind(if truthy(request.get("background")) { 1i32 } else { 0 })
        .bind(str_or_null(request.get("truncation")).unwrap_or_else(|| "disabled".into()))
        .bind(json_or_null(response.get("incomplete_details")))
        .bind(json_or_null(response.get("error")))
        .bind(str_or_null(request.get("safety_identifier")))
        .bind(str_or_null(request.get("service_tier")).unwrap_or_else(|| "auto".into()))
        .bind(str_or_null(request.get("prompt_cache_key")))
        .bind(str_or_null(request.get("prompt_cache_retention")))
        .bind(json_or_null(request.get("metadata")))
        .bind(str_or_null(get(response, &["plexus", "provider"])))
        .bind(str_or_null(get(response, &["plexus", "model"])))
        .bind(str_or_null(get(response, &["plexus", "apiType"])))
        .bind(str_or_null(get(response, &["plexus", "canonicalModel"])))
        .execute(pool).await?;
    if let Some(items) = output.as_array() {
        for (i, item) in items.iter().enumerate() {
            if !truthy(Some(item)) { continue; }
            let item_id = str_or_null(item.get("id")).unwrap_or_else(|| format!("item_{id}_{i}"));
            sqlx::query("INSERT INTO response_items (id, response_id, item_index, item_type, item_data) VALUES ($1, $2, $3, $4, $5)")
                .bind(item_id).bind(&id).bind(i as i32).bind(to_string(item.get("type"))).bind(stringify(item))
                .execute(pool).await?;
        }
    }
    Ok(())
}

/// `getResponse(id)` → the stored row (already formatted fields kept raw).
pub async fn get_response(pool: &PgPool, id: &str) -> Result<Option<sqlx::postgres::PgRow>, sqlx::Error> {
    sqlx::query("SELECT * FROM responses WHERE id = $1 LIMIT 1").bind(id).fetch_optional(pool).await
}

/// `deleteResponse(id)` (always `true` unless the DB throws).
pub async fn delete_response(pool: &PgPool, id: &str) -> Result<bool, sqlx::Error> {
    sqlx::query("DELETE FROM response_items WHERE response_id = $1").bind(id).execute(pool).await?;
    sqlx::query("DELETE FROM responses WHERE id = $1").bind(id).execute(pool).await?;
    Ok(true)
}

/// `updateConversation(id, outputItems, inputItems)`.
pub async fn update_conversation(pool: &PgPool, id: &str, output_items: &Value, input_items: &[Value]) -> Result<(), sqlx::Error> {
    let now = now_secs();
    let mut all: Vec<Value> = input_items.to_vec();
    if let Some(o) = output_items.as_array() { all.extend(o.iter().cloned()); }
    let existing: Option<String> = sqlx::query_scalar("SELECT items FROM conversations WHERE id = $1 LIMIT 1").bind(id).fetch_optional(pool).await?;
    match existing {
        None => {
            sqlx::query("INSERT INTO conversations (id, created_at, updated_at, items, metadata, plexus_account_id) VALUES ($1, $2, $2, $3, NULL, NULL)")
                .bind(id).bind(now).bind(stringify(&Value::Array(all))).execute(pool).await?;
        }
        Some(items) => {
            let mut merged = parse(if items.is_empty() { "[]" } else { &items }).and_then(|v| v.as_array().cloned()).unwrap_or_default();
            merged.extend(all);
            sqlx::query("UPDATE conversations SET updated_at = $2, items = $3 WHERE id = $1").bind(id).bind(now).bind(stringify(&Value::Array(merged))).execute(pool).await?;
        }
    }
    Ok(())
}

/// `getConversation(id)`: `(items, metadata, created_at, updated_at)`.
pub async fn get_conversation(pool: &PgPool, id: &str) -> Result<Option<sqlx::postgres::PgRow>, sqlx::Error> {
    sqlx::query("SELECT * FROM conversations WHERE id = $1 LIMIT 1").bind(id).fetch_optional(pool).await
}

fn parse_col(row: &sqlx::postgres::PgRow, col: &str) -> Option<Value> {
    row.try_get::<Option<String>, _>(col).ok().flatten().filter(|s| !s.is_empty()).and_then(|s| parse(&s))
}

fn real_js(v: Option<f64>) -> Value {
    // `real` columns come back as float4; Plexus (postgres-js) widens them to JS numbers.
    v.map(num).unwrap_or(Value::Null)
}

/// `formatStoredResponse(row)`.
pub fn format_stored_response(row: &sqlx::postgres::PgRow) -> Value {
    let s = |c: &str| row.try_get::<Option<String>, _>(c).ok().flatten().map(Value::String).unwrap_or(Value::Null);
    let i = |c: &str| row.try_get::<Option<i32>, _>(c).ok().flatten().map(Value::from).unwrap_or(Value::Null);
    let b = |c: &str| row.try_get::<Option<i64>, _>(c).ok().flatten().map(Value::from).unwrap_or(Value::Null);
    // float4 text form, as postgres-js parses it.
    let f32col = |c: &str| row.try_get::<Option<f32>, _>(c).ok().flatten().and_then(|f| f.to_string().parse::<f64>().ok());
    let conversation = row.try_get::<Option<String>, _>("conversation_id").ok().flatten().filter(|c| !c.is_empty());
    obj(vec![
        ("id", Some(s("id"))),
        ("object", Some(s("object"))),
        ("created_at", Some(b("created_at"))),
        ("completed_at", Some(b("completed_at"))),
        ("status", Some(s("status"))),
        ("model", Some(s("model"))),
        ("output", Some(parse_col(row, "output_items").unwrap_or(Value::Null))),
        ("instructions", Some(s("instructions"))),
        ("temperature", Some(real_js(f32col("temperature")))),
        ("top_p", Some(real_js(f32col("top_p")))),
        ("max_output_tokens", Some(i("max_output_tokens"))),
        ("top_logprobs", Some(i("top_logprobs"))),
        ("parallel_tool_calls", Some(json!(row.try_get::<Option<i32>, _>("parallel_tool_calls").ok().flatten() == Some(1)))),
        ("tool_choice", parse_col(row, "tool_choice")),
        ("tools", Some(parse_col(row, "tools").unwrap_or(json!([])))),
        ("text", parse_col(row, "text_config")),
        ("reasoning", parse_col(row, "reasoning_config")),
        ("usage", Some(json!({
            "input_tokens": i("usage_input_tokens"),
            "input_tokens_details": {"cached_tokens": i("usage_cached_tokens")},
            "output_tokens": i("usage_output_tokens"),
            "output_tokens_details": {"reasoning_tokens": i("usage_reasoning_tokens")},
            "total_tokens": i("usage_total_tokens"),
        }))),
        ("previous_response_id", Some(s("previous_response_id"))),
        ("conversation", Some(conversation.map(|c| json!({"id": c})).unwrap_or(Value::Null))),
        ("store", Some(json!(row.try_get::<Option<i32>, _>("store").ok().flatten() == Some(1)))),
        ("background", Some(json!(row.try_get::<Option<i32>, _>("background").ok().flatten() == Some(1)))),
        ("truncation", Some(s("truncation"))),
        ("incomplete_details", Some(parse_col(row, "incomplete_details").unwrap_or(Value::Null))),
        ("error", Some(parse_col(row, "error").unwrap_or(Value::Null))),
        ("safety_identifier", Some(s("safety_identifier"))),
        ("service_tier", Some(s("service_tier"))),
        ("prompt_cache_key", Some(s("prompt_cache_key"))),
        ("prompt_cache_retention", Some(s("prompt_cache_retention"))),
        ("metadata", Some(parse_col(row, "metadata").unwrap_or(json!({})))),
        ("plexus", Some(json!({"provider": s("plexus_provider"), "model": s("plexus_target_model"), "apiType": s("plexus_api_type"), "canonicalModel": s("plexus_canonical_model")}))),
    ])
}

/// `formatStoredConversation(row)`.
pub fn format_stored_conversation(row: &sqlx::postgres::PgRow) -> Value {
    json!({
        "id": row.try_get::<String, _>("id").unwrap_or_default(),
        "object": "conversation",
        "created_at": row.try_get::<Option<i64>, _>("created_at").ok().flatten(),
        "updated_at": row.try_get::<Option<i64>, _>("updated_at").ok().flatten(),
        "items": parse_col(row, "items").unwrap_or(Value::Null),
        "metadata": parse_col(row, "metadata").unwrap_or(json!({})),
    })
}

/// `JSON.parse(row.outputItems)` / `JSON.parse(row.items)` for context loading.
pub fn stored_items(row: &sqlx::postgres::PgRow, col: &str) -> Vec<Value> {
    parse_col(row, col).and_then(|v| v.as_array().cloned()).unwrap_or_default()
}
