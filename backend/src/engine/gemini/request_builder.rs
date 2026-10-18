//! `transformers/gemini/request-builder.ts`: unified request → Gemini
//! `GenerateContentRequest`.
use super::part_mapper::convert_unified_parts_to_gemini;
use super::utils::*;
use crate::engine::js::*;
use crate::engine::transformer::EngineResult;
use serde_json::{json, Map, Value};

/// `GEMINI_THINKING_LEVEL`: unified effort → Gemini ThinkingLevel (xhigh/max clamp to HIGH).
pub fn gemini_thinking_level(effort: &str) -> Option<&'static str> {
    Some(match effort {
        "none" => "NONE",
        "minimal" => "MINIMAL",
        "low" => "LOW",
        "medium" => "MEDIUM",
        "high" | "xhigh" | "max" => "HIGH",
        _ => return None,
    })
}

/// `typeof v === 'string' ? v : JSON.stringify(v)`.
fn text_or_json(v: Option<&Value>) -> Option<Value> {
    match v { Some(Value::String(s)) => Some(Value::String(s.clone())), other => stringify_opt(other) }
}

fn function_call_part(tc: &Value) -> EngineResult<Map<String, Value>> {
    let f = read(Some(tc), "function")?;
    let name = read(f, "name")?.cloned();
    let args = json_parse_js(read(f, "arguments")?)?;
    let mut part = Map::new();
    part.insert("functionCall".into(), obj(vec![("name", name), ("args", Some(args))]));
    Ok(part)
}

/// `buildGeminiRequest`.
pub fn build_gemini_request(request: &Value) -> EngineResult<Value> {
    let mut contents: Vec<Value> = Vec::new();
    let mut tools: Vec<Value> = Vec::new();

    // Gap 1: explicit systemInstruction.
    let has_system_instruction = truthy(request.get("systemInstruction"));
    let mut system_instruction_content: Option<Value> = None;
    if has_system_instruction {
        let sys_content = get(request, &["systemInstruction", "content"]);
        let sys_parts = match sys_content {
            Some(Value::String(s)) => vec![json!({"text": s})],
            Some(Value::Array(_)) => convert_unified_parts_to_gemini(sys_content)?,
            _ => Vec::new(),
        };
        if !sys_parts.is_empty() { system_instruction_content = Some(json!({"role": "system", "parts": sys_parts})); }
    }

    let messages = match request.get("messages") {
        Some(Value::Array(items)) => items.clone(),
        Some(Value::String(_)) => Vec::new(), // iterates characters; none has a role
        _ => return Err(type_error("request.messages is not iterable")),
    };
    for msg in &messages {
        let msg_role = read(Some(msg), "role")?.and_then(Value::as_str);
        if msg_role == Some("system") && has_system_instruction { continue; }
        let mut role = "";
        let mut parts: Vec<Value> = Vec::new();
        match msg_role {
            Some("system") => {
                role = "user";
                parts.push(obj(vec![("text", text_or_json(msg.get("content")))]));
            }
            Some(r @ ("user" | "assistant")) => {
                role = if r == "assistant" { "model" } else { "user" };
                let thinking_content = get(msg, &["thinking", "content"]);
                if truthy(thinking_content) {
                    parts.push(json!({"text": thinking_content.cloned().unwrap_or(Value::Null), "thought": true}));
                }
                let signature = get(msg, &["thinking", "signature"]).filter(|s| truthy(Some(s)));
                match msg.get("content") {
                    Some(Value::String(s)) => {
                        let mut part = json!({"text": s});
                        if let Some(sig) = signature.filter(|_| !truthy(msg.get("tool_calls"))) { part["thoughtSignature"] = sig.clone(); }
                        parts.push(part);
                    }
                    c @ Some(Value::Array(_)) => parts.extend(convert_unified_parts_to_gemini(c)?),
                    _ => {}
                }
                if truthy(msg.get("tool_calls")) {
                    let Some(tcs) = get_arr(msg, &["tool_calls"]) else { return Err(type_error("msg.tool_calls.forEach is not a function")) };
                    for (index, tc) in tcs.iter().enumerate() {
                        let mut part = function_call_part(tc)?;
                        if index == 0 { if let Some(sig) = signature { part.insert("thoughtSignature".into(), sig.clone()); } }
                        parts.push(Value::Object(part));
                    }
                }
            }
            Some("tool") => {
                role = "user";
                let name = or(msg.get("name"), msg.get("tool_call_id")).filter(|v| truthy(Some(v))).unwrap_or_else(|| json!("unknown_tool"));
                parts.push(json!({"functionResponse": {"name": name, "response": obj(vec![("content", text_or_json(msg.get("content")))])}}));
            }
            _ => {}
        }
        if !role.is_empty() && !parts.is_empty() { contents.push(json!({"role": role, "parts": parts})); }
    }

    // Gemini 3+ rejects histories ending on a text-only model turn
    // (`ensureContentsEndWithUser`). A `{}` part here can only be TS's
    // `{ text: undefined }` (a unified text item without `text`), which has a
    // `text` key in JS and so counts as text-only.
    if !ensure_contents_end_with_user(&mut contents) {
        let undefined_text_tail = contents.last().is_some_and(|last| {
            let mut restored = last.clone();
            if let Some(parts) = restored.get_mut("parts").and_then(Value::as_array_mut) {
                for p in parts.iter_mut().filter(|p| p.as_object().is_some_and(Map::is_empty)) { *p = json!({"text": null}); }
            }
            is_text_only_model_content(&restored)
        });
        if undefined_text_tail { contents.push(json!({"role": "user", "parts": [{"text": "."}]})); }
    }

    // Gap 4 & 5: tools.
    if let Some(list) = get_arr(request, &["tools"]).filter(|t| !t.is_empty()) {
        let mut function_declarations = Vec::new();
        let (mut google_search, mut code_execution, mut url_context) = (false, false, false);
        for t in list {
            let ty = read(Some(t), "type")?.and_then(Value::as_str);
            if ty == Some("function") && truthy(t.get("function")) {
                let f = &t["function"];
                let mut decl = obj(vec![("name", f.get("name").cloned()), ("description", f.get("description").cloned())]);
                if truthy(f.get("parametersJsonSchema")) {
                    decl["parametersJsonSchema"] = f["parametersJsonSchema"].clone();
                } else if truthy(f.get("parameters")) {
                    decl["parameters"] = f["parameters"].clone();
                }
                function_declarations.push(decl);
            } else if matches!(ty, Some("googleSearch" | "web_search_20250305" | "web_search" | "openrouter:web_search")) {
                google_search = true;
            } else if ty == Some("codeExecution") {
                code_execution = true;
            } else if ty == Some("urlContext") {
                url_context = true;
            }
        }
        if !function_declarations.is_empty() { tools.push(json!({"functionDeclarations": function_declarations})); }
        if google_search { tools.push(json!({"googleSearch": {}})); }
        if code_execution { tools.push(json!({"codeExecution": {}})); }
        if url_context { tools.push(json!({"urlContext": {}})); }
    }

    // Gap 3: toolConfig.
    let tool_config = request.get("toolConfig").filter(|tc| truthy(Some(tc))).map(|tc| {
        json!({"functionCallingConfig": obj(vec![
            ("mode", tc.get("mode").cloned()),
            ("functionCallingPreference", tc.get("functionCallingPreference").filter(|p| truthy(Some(p))).cloned()),
        ])})
    });

    // generationConfig keeps explicit-`undefined` members: they matter when
    // spread over the original body's generationConfig below.
    let mut generation_config: Vec<(&str, Option<Value>)> = vec![
        ("maxOutputTokens", request.get("max_tokens").cloned()),
        ("temperature", request.get("temperature").cloned()),
    ];
    if let Some(rf) = request.get("response_format").filter(|v| truthy(Some(v))) {
        if matches!(get_str(rf, &["type"]), Some("json_object" | "json_schema")) {
            generation_config.push(("responseMimeType", Some(json!("application/json"))));
            if truthy(rf.get("json_schema")) { generation_config.push(("responseJsonSchema", rf.get("json_schema").cloned())); }
        }
    }
    if let Some(reasoning) = request.get("reasoning").filter(|v| truthy(Some(v))) {
        if reasoning.get("enabled") != Some(&Value::Bool(false)) {
            let level = reasoning.get("effort").filter(|e| truthy(Some(e))).and_then(Value::as_str).and_then(gemini_thinking_level);
            generation_config.push(("thinkingConfig", Some(obj(vec![
                ("includeThoughts", reasoning.get("enabled").cloned()),
                ("thinkingBudget", reasoning.get("max_tokens").cloned()),
                ("thinkingLevel", level.map(|l| json!(l))),
            ]))));
        }
    }

    let mut req = Map::new();
    req.insert("contents".into(), Value::Array(contents));
    if !tools.is_empty() { req.insert("tools".into(), Value::Array(tools)); }
    set_opt(&mut req, "systemInstruction", system_instruction_content);
    set_opt(&mut req, "toolConfig", tool_config);
    // `generationConfig` keeps its literal position even when reassigned below.
    req.insert("generationConfig".into(), Value::Null);

    // Same-format (gemini → gemini) non-pass-through requests carry
    // Gemini-native fields the unified schema does not model.
    let same_api = get_str(request, &["incomingApiType"]).is_some_and(|t| t.to_lowercase() == "gemini");
    let original = request.get("originalBody").filter(|o| truthy(Some(o)));
    let mut generation = Map::new();
    match original.filter(|_| same_api) {
        Some(original) => {
            for field in ["safetySettings", "cachedContent", "labels"] {
                if let Some(v) = original.get(field) { req.insert(field.into(), v.clone()); }
            }
            // `{ ...originalGenConfig, ...req.generationConfig }`: undefined
            // members of the mapped config override (and so drop) originals.
            if let Some(orig) = original.get("generationConfig").filter(|g| truthy(Some(g))) {
                match orig {
                    Value::Object(m) => generation = m.clone(),
                    Value::Array(items) => { for (i, v) in items.iter().enumerate() { generation.insert(i.to_string(), v.clone()); } }
                    _ => {}
                }
                for (k, v) in generation_config.iter() {
                    match v { Some(v) => { generation.insert((*k).into(), v.clone()); } None => { generation.shift_remove(*k); } }
                }
            } else {
                for (k, v) in generation_config { set_opt(&mut generation, k, v); }
            }
        }
        None => { for (k, v) in generation_config { set_opt(&mut generation, k, v); } }
    }
    req.insert("generationConfig".into(), Value::Object(generation));
    Ok(Value::Object(req))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::gemini::request_parser::parse_gemini_request;

    fn build(v: Value) -> Value { build_gemini_request(&v).unwrap() }

    #[test]
    fn system_instruction() {
        let r = build(json!({"messages": [{"role": "user", "content": "Hello"}], "model": "m", "systemInstruction": {"role": "system", "content": "You are helpful."}}));
        assert_eq!(r["systemInstruction"], json!({"role": "system", "parts": [{"text": "You are helpful."}]}));
        let r = build(json!({"messages": [{"role": "user", "content": "Hi"}], "systemInstruction": {"role": "system", "content": [{"type": "text", "text": "System message"}]}}));
        assert_eq!(r["systemInstruction"]["role"], "system");
        let r = build(json!({"messages": [{"role": "system", "content": "skip"}, {"role": "user", "content": "Hello"}], "systemInstruction": {"role": "system", "content": "x"}}));
        assert_eq!(r["contents"], json!([{"role": "user", "parts": [{"text": "Hello"}]}]));
        let r = build(json!({"messages": [{"role": "system", "content": [{"type": "text", "text": "s"}]}, {"role": "user", "content": "Hello"}]}));
        assert_eq!(r["contents"][0], json!({"role": "user", "parts": [{"text": "[{\"type\":\"text\",\"text\":\"s\"}]"}]}));
        assert_eq!(r["generationConfig"], json!({}));
    }

    #[test]
    fn tool_config() {
        for mode in ["auto", "none", "any"] {
            let r = build(json!({"messages": [{"role": "user", "content": "Hello"}], "toolConfig": {"mode": mode}}));
            assert_eq!(r["toolConfig"], json!({"functionCallingConfig": {"mode": mode}}));
        }
        let r = build(json!({"messages": [], "toolConfig": {"mode": "auto", "functionCallingPreference": "auto"}}));
        assert_eq!(r["toolConfig"]["functionCallingConfig"]["functionCallingPreference"], "auto");
        assert!(build(json!({"messages": []})).get("toolConfig").is_none());
    }

    #[test]
    fn tools() {
        let r = build(json!({"messages": [], "tools": [
            {"type": "function", "function": {"name": "a", "description": "d", "parametersJsonSchema": {"type": "object"}, "parameters": {"type": "object"}}},
            {"type": "function", "function": {"name": "b", "parameters": {"type": "object"}}},
            {"type": "googleSearch", "googleSearch": {}}, {"type": "web_search"},
            {"type": "codeExecution", "codeExecution": {}}, {"type": "urlContext", "urlContext": {}}
        ]}));
        assert_eq!(r["tools"], json!([
            {"functionDeclarations": [{"name": "a", "description": "d", "parametersJsonSchema": {"type": "object"}}, {"name": "b", "parameters": {"type": "object"}}]},
            {"googleSearch": {}}, {"codeExecution": {}}, {"urlContext": {}}
        ]));
    }

    #[test]
    fn thinking_config() {
        let r = build(json!({"messages": [], "reasoning": {"enabled": true, "effort": "medium"}}));
        assert_eq!(r["generationConfig"]["thinkingConfig"], json!({"includeThoughts": true, "thinkingLevel": "MEDIUM"}));
        for effort in ["xhigh", "max"] {
            let r = build(json!({"messages": [], "reasoning": {"enabled": true, "effort": effort}}));
            assert_eq!(r["generationConfig"]["thinkingConfig"]["thinkingLevel"], "HIGH");
        }
        let r = build(json!({"messages": [], "reasoning": {"enabled": true, "max_tokens": 8192}}));
        assert_eq!(r["generationConfig"]["thinkingConfig"]["thinkingBudget"], 8192);
        assert!(build(json!({"messages": []}))["generationConfig"].get("thinkingConfig").is_none());
        assert!(build(json!({"messages": [], "reasoning": {"enabled": false}}))["generationConfig"].get("thinkingConfig").is_none());
        let unified = parse_gemini_request(&json!({"contents": [{"role": "user", "parts": [{"text": "Think"}]}], "model": "m", "generationConfig": {"thinkingConfig": {"include_thoughts": true, "thinking_level": "MEDIUM"}}})).unwrap();
        let rebuilt = build_gemini_request(&unified).unwrap();
        assert_eq!(rebuilt["generationConfig"]["thinkingConfig"], json!({"includeThoughts": true, "thinkingLevel": "MEDIUM"}));
    }

    #[test]
    fn messages_thinking_tool_calls_and_tool_results() {
        let r = build(json!({"messages": [
            {"role": "user", "content": [{"type": "text", "text": "q"}]},
            {"role": "assistant", "content": "answer", "thinking": {"content": "hmm", "signature": "c2ln"}},
            {"role": "assistant", "content": null, "thinking": {"content": "", "signature": "c2ln"}, "tool_calls": [
                {"id": "1", "type": "function", "function": {"name": "a", "arguments": "{\"x\":1}"}},
                {"id": "2", "type": "function", "function": {"name": "b", "arguments": "{}"}}
            ]},
            {"role": "tool", "tool_call_id": "1", "content": "ok"},
            {"role": "tool", "name": "b", "content": {"v": 1}},
            {"role": "developer", "content": "ignored"}
        ], "max_tokens": 5, "temperature": 0.2, "response_format": {"type": "json_schema", "json_schema": {"type": "object"}}}));
        assert_eq!(r["contents"], json!([
            {"role": "user", "parts": [{"text": "q"}]},
            {"role": "model", "parts": [{"text": "hmm", "thought": true}, {"text": "answer", "thoughtSignature": "c2ln"}]},
            {"role": "model", "parts": [{"functionCall": {"name": "a", "args": {"x": 1}}, "thoughtSignature": "c2ln"}, {"functionCall": {"name": "b", "args": {}}}]},
            {"role": "user", "parts": [{"functionResponse": {"name": "1", "response": {"content": "ok"}}}]},
            {"role": "user", "parts": [{"functionResponse": {"name": "b", "response": {"content": "{\"v\":1}"}}}]}
        ]));
        assert_eq!(r["generationConfig"], json!({"maxOutputTokens": 5, "temperature": 0.2, "responseMimeType": "application/json", "responseJsonSchema": {"type": "object"}}));
        assert!(build_gemini_request(&json!({"messages": [{"role": "assistant", "tool_calls": [{"function": {"name": "a", "arguments": "{bad"}}]}]})).is_err());
    }

    #[test]
    fn model_tail() {
        let r = build(json!({"messages": [{"role": "user", "content": "Remember cobalt"}, {"role": "assistant", "content": "stored"}], "model": "gemini-3.8-flash"}));
        assert_eq!(r["contents"].as_array().unwrap().len(), 3);
        assert_eq!(r["contents"][2], json!({"role": "user", "parts": [{"text": "."}]}));
        let r = build(json!({"messages": [{"role": "user", "content": "hello"}]}));
        assert_eq!(r["contents"].as_array().unwrap().len(), 1);
        let r = build(json!({"messages": [{"role": "user", "content": "Look up"}, {"role": "assistant", "content": null, "tool_calls": [{"id": "call-1", "type": "function", "function": {"name": "lookup", "arguments": "{}"}}]}]}));
        let c = r["contents"].as_array().unwrap();
        assert_eq!(c.last().unwrap()["role"], "model");
        assert_eq!(c.len(), 2);
        // `{ text: undefined }` parts still count as text-only in TS.
        let r = build(json!({"messages": [{"role": "assistant", "content": [{"type": "text"}]}]}));
        assert_eq!(r["contents"], json!([{"role": "model", "parts": [{}]}, {"role": "user", "parts": [{"text": "."}]}]));
    }

    fn gemini_request() -> Value {
        json!({
            "contents": [{"role": "user", "parts": [{"text": "Summarize the latest news."}]}],
            "model": "gemini-2.5-flash",
            "generationConfig": {"maxOutputTokens": 1024, "temperature": 0.7, "topP": 0.9, "topK": 40, "stopSequences": ["END"], "responseLogprobs": true, "logprobs": 5},
            "safetySettings": [{"category": "HARM_CATEGORY_HARASSMENT", "threshold": "BLOCK_ONLY_HIGH"}],
            "cachedContent": "cachedContents/example",
            "labels": {"team": "research", "env": "staging"}
        })
    }

    #[test]
    fn same_format_round_trip_preserves_native_fields() {
        let original = gemini_request();
        let mut unified = parse_gemini_request(&original).unwrap();
        unified["incomingApiType"] = json!("gemini");
        unified["originalBody"] = original.clone();
        let built = build(unified.clone());
        assert_eq!(built["safetySettings"], original["safetySettings"]);
        assert_eq!(built["cachedContent"], "cachedContents/example");
        assert_eq!(built["labels"], original["labels"]);
        let gc = &built["generationConfig"];
        assert_eq!((gc["topP"].clone(), gc["topK"].clone(), gc["stopSequences"].clone(), gc["responseLogprobs"].clone(), gc["logprobs"].clone()),
            (json!(0.9), json!(40), json!(["END"]), json!(true), json!(5)));
        unified["max_tokens"] = json!(2048);
        let built = build(unified.clone());
        assert_eq!(built["generationConfig"]["maxOutputTokens"], 2048);
        assert_eq!(built["generationConfig"]["topP"], 0.9);
        // Undefined mapped members drop the original's value (`{...a, ...b}` with b.x = undefined).
        unified.as_object_mut().unwrap().remove("temperature");
        assert!(build(unified)["generationConfig"].get("temperature").is_none());

        let cross = build(parse_gemini_request(&original).unwrap());
        assert!(cross.get("safetySettings").is_none() && cross.get("cachedContent").is_none() && cross.get("labels").is_none());
    }
}
