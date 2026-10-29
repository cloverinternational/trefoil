//! Cross-module tests. Goldens marked "TS" were produced by running the
//! pinned Plexus TypeScript (`applyRegistryAutoCompat`, bun) on the same
//! inputs and compare serialized output byte-for-byte (key order included).
use super::auto_compat::*;
use super::cache_key_injection::derive_cache_injection_key;
use super::request_payload::*;
use super::util::Headers;
use super::*;
use crate::engine::config::Config;
use crate::engine::js::stringify;
use crate::engine::routing::RouteResult;
use crate::engine::transformer::{EngineResult, Transformer};
use serde_json::{json, Value};

fn route(model: &str, config: Value, model_config: Option<Value>) -> RouteResult {
    RouteResult {
        provider: "p".into(),
        model: model.into(),
        config,
        model_config,
        incoming_model_alias: None,
        canonical_model: None,
    }
}

fn req(extra: Value) -> Value {
    let mut base = json!({"model": "alias", "messages": [{"role": "user", "content": "hi"}], "incomingApiType": "chat"});
    for (k, v) in extra.as_object().unwrap() {
        base[k] = v.clone();
    }
    base
}

#[test]
fn auto_compat_matches_typescript_goldens() {
    let cases: Vec<(&str, Value, Value, RouteResult, &str, &str)> = vec![
        (
            "opus5-off",
            json!({"model": "claude-opus-5", "messages": [], "max_tokens": 99999999, "temperature": 0.3}),
            req(json!({"reasoning": {"enabled": false}})),
            route(
                "claude-opus-5",
                json!({"auto_compat": true, "pi_ai_provider": "anthropic"}),
                None,
            ),
            "messages",
            r#"{"model":"claude-opus-5","messages":[],"max_tokens":128000,"thinking":{"type":"adaptive","display":"summarized"},"output_config":{"effort":"low"}}"#,
        ),
        (
            "sonnet45-high",
            json!({"model": "claude-sonnet-4-5", "messages": [], "temperature": 0.3}),
            req(json!({"reasoning": {"effort": "high", "summary": "detailed"}})),
            route(
                "claude-sonnet-4-5",
                json!({"auto_compat": true, "pi_ai_provider": "anthropic"}),
                None,
            ),
            "messages",
            r#"{"model":"claude-sonnet-4-5","messages":[],"thinking":{"type":"enabled","budget_tokens":16384,"display":"summarized"}}"#,
        ),
        (
            "glm-zai",
            json!({"model": "glm-4.7", "messages": [], "reasoning_effort": "high", "max_tokens": 10}),
            req(json!({})),
            route(
                "glm-4.7",
                json!({"auto_compat": true, "pi_ai_provider": "zai"}),
                None,
            ),
            "chat",
            r#"{"model":"glm-4.7","messages":[],"max_tokens":10,"thinking":{"type":"enabled","clear_thinking":false}}"#,
        ),
        (
            "gpt5-resp",
            json!({"model": "gpt-5", "input": [], "reasoning": {"effort": "minimal"}, "include": ["x"], "text": {"verbosity": "low"}}),
            req(json!({"incomingApiType": "responses"})),
            route(
                "gpt-5",
                json!({"auto_compat": true, "pi_ai_provider": "openai"}),
                None,
            ),
            "responses",
            r#"{"model":"gpt-5","input":[],"reasoning":{"effort":"minimal","summary":"auto"},"include":["x","reasoning.encrypted_content"],"text":{"verbosity":"low"}}"#,
        ),
        (
            "gemini",
            json!({"contents": [], "generationConfig": {"maxOutputTokens": 5}}),
            req(json!({"reasoning": {"effort": "low"}})),
            route(
                "gemini-2.5-pro",
                json!({"auto_compat": true, "pi_ai_provider": "google"}),
                None,
            ),
            "gemini",
            r#"{"contents":[],"generationConfig":{"maxOutputTokens":5,"thinkingConfig":{"includeThoughts":true,"thinkingBudget":2048}}}"#,
        ),
        (
            "gemini3",
            json!({"contents": []}),
            req(json!({"reasoning": {"effort": "high"}})),
            route(
                "gemini-3-pro-preview",
                json!({"auto_compat": true, "pi_ai_provider": "google"}),
                None,
            ),
            "gemini",
            r#"{"contents":[]}"#,
        ),
        (
            "inline",
            json!({"model": "x", "messages": [], "reasoning": {"effort": "high"}, "temperature": 0.4, "max_tokens": 256, "service_tier": "flex"}),
            req(json!({"originalBody": {"service_tier": "flex"}})),
            route(
                "upstream/special",
                json!({"auto_compat": true, "pi_ai_quirks": {"chat": {"api": "openai-completions", "reasoning": true, "thinkingLevelMap": {"off": "none", "high": "high"}, "serviceTierMap": {"standard": "default"}, "compat": {"thinkingFormat": "openrouter", "maxTokensField": "max_completion_tokens", "supportsTemperature": false}, "models": {"upstream/special": {"thinkingLevelMap": {"high": "hard"}, "compat": {"supportsTemperature": true}, "maxTokens": 64}}}}}),
                None,
            ),
            "chat",
            r#"{"model":"x","messages":[],"reasoning":{"effort":"hard"},"temperature":0.4,"service_tier":"default","max_completion_tokens":64}"#,
        ),
        (
            "qwen-ct",
            json!({"model": "q", "messages": [], "reasoning": {"effort": "low"}, "chat_template_kwargs": {"a": 1}}),
            req(json!({})),
            route(
                "q",
                json!({"auto_compat": true, "pi_ai_quirks": {"chat": {"api": "openai-completions", "reasoning": true, "thinkingLevelMap": {"low": "L", "off": "O"}, "compat": {"thinkingFormat": "chat-template", "chatTemplateKwargs": {"enable": {"$var": "thinking.enabled"}, "level": {}, "gone": {"omitWhenOff": true}, "lit": 5}}}}}),
                None,
            ),
            "chat",
            r#"{"model":"q","messages":[],"chat_template_kwargs":{"a":1,"enable":true,"level":"L","gone":"L","lit":5}}"#,
        ),
        (
            "speed",
            json!({"model": "claude-x", "messages": [], "speed": "fast", "service_tier": "priority"}),
            req(json!({"incomingApiType": "messages", "serviceTier": "priority"})),
            route(
                "claude-x",
                json!({"auto_compat": true, "pi_ai_quirks": {"messages": {"api": "anthropic-messages", "serviceTierMap": {"priority": "fast"}}}}),
                None,
            ),
            "messages",
            r#"{"model":"claude-x","messages":[],"speed":"fast"}"#,
        ),
    ];
    for (name, payload, request, r, api, expected) in cases {
        assert_eq!(
            stringify(&apply_registry_auto_compat(payload, &request, &r, api)),
            expected,
            "case {name}"
        );
    }
}

fn pi_model(overrides: Value) -> Value {
    let mut m = json!({"id": "registry-model", "provider": "openai", "api": "openai-completions", "reasoning": true,
        "thinkingLevelMap": {"off": "none", "low": "low", "medium": "medium", "high": "high"},
        "compat": {"supportsReasoningEffort": true, "supportsTemperature": true}, "maxTokens": 4096});
    for (k, v) in overrides.as_object().unwrap() {
        m[k] = v.clone();
    }
    m
}

fn mocked(model: Value) -> impl Fn(&str, &str) -> Option<Value> {
    move |_, _| Some(model.clone())
}

fn default_route() -> RouteResult {
    route(
        "provider-model",
        json!({"api_base_url": "https://example.test/v1", "api_key": "k", "auto_compat": true, "pi_ai_provider": "openai"}),
        Some(json!({"pi_ai_model_id": "registry-model"})),
    )
}

#[test]
fn auto_compat_dialects_with_mocked_registry() {
    let body = |extra: Value| {
        let mut b =
            json!({"model": "alias-model", "messages": [{"role": "user", "content": "hello"}]});
        for (k, v) in extra.as_object().unwrap() {
            b[k] = v.clone();
        }
        b
    };
    // default format: reasoning object → reasoning_effort
    let p = body(json!({"reasoning": {"effort": "high"}}));
    let out = apply_registry_auto_compat_with(
        p,
        &req(json!({})),
        &default_route(),
        "chat",
        &mocked(pi_model(json!({}))),
    );
    assert_eq!(out.get("reasoning"), None);
    assert_eq!(out["reasoning_effort"], "high");
    // openrouter format
    let p = body(json!({"reasoning_effort": "medium"}));
    let out = apply_registry_auto_compat_with(
        p,
        &req(json!({})),
        &default_route(),
        "chat",
        &mocked(pi_model(
            json!({"compat": {"supportsReasoningEffort": true, "thinkingFormat": "openrouter"}}),
        )),
    );
    assert_eq!(out["reasoning"], json!({"effort": "medium"}));
    assert_eq!(out.get("reasoning_effort"), None);
    // qwen
    let p = body(json!({"reasoning": {"effort": "low"}, "reasoning_effort": "low"}));
    let out = apply_registry_auto_compat_with(
        p,
        &req(json!({})),
        &default_route(),
        "chat",
        &mocked(pi_model(
            json!({"compat": {"supportsReasoningEffort": true, "thinkingFormat": "qwen"}}),
        )),
    );
    assert_eq!(out["enable_thinking"], true);
    assert!(out.get("reasoning").is_none() && out.get("reasoning_effort").is_none());
    // ant-ling disable it cannot express
    let p = body(json!({"reasoning": {"enabled": false}}));
    let out = apply_registry_auto_compat_with(
        p,
        &req(json!({})),
        &default_route(),
        "chat",
        &mocked(pi_model(
            json!({"compat": {"supportsReasoningEffort": true, "thinkingFormat": "ant-ling"}}),
        )),
    );
    assert!(out.get("reasoning").is_none() && out.get("reasoning_effort").is_none());
    // default passes client reasoning_effort through when support unknown
    let p = body(json!({"reasoning_effort": "high"}));
    let model =
        pi_model(json!({"compat": {}, "thinkingLevelMap": {"off": "none", "high": "high"}}));
    let out = apply_registry_auto_compat_with(
        p,
        &req(json!({})),
        &default_route(),
        "chat",
        &mocked(model),
    );
    assert_eq!(out["reasoning_effort"], "high");
    // temperature dropped when unsupported
    let p = body(json!({"temperature": 0.7}));
    let out = apply_registry_auto_compat_with(
        p,
        &req(json!({})),
        &default_route(),
        "chat",
        &mocked(pi_model(
            json!({"reasoning": false, "compat": {"supportsTemperature": false}}),
        )),
    );
    assert!(out.get("temperature").is_none());
    // auto_compat off → untouched (but key-default tier still stripped)
    let mut r = default_route();
    r.config["auto_compat"] = json!(false);
    let p = body(json!({"reasoning_effort": "high"}));
    assert_eq!(
        apply_registry_auto_compat_with(
            p.clone(),
            &req(json!({})),
            &r,
            "chat",
            &mocked(pi_model(json!({})))
        ),
        p
    );
}

#[test]
fn auto_compat_service_tiers() {
    let model = mocked(pi_model(json!({})));
    let overlay = |quirks: Value| {
        route(
            "provider-model",
            json!({"api_base_url": {"chat": "https://e/v1"}, "api_key": "k", "auto_compat": true, "pi_ai_provider": "openai", "pi_ai_quirks": quirks}),
            Some(json!({"pi_ai_model_id": "registry-model"})),
        )
    };
    let r = overlay(
        json!({"chat": {"api": "openai-completions", "serviceTierMap": {"standard": "default", "priority": "priority"}}}),
    );
    let out = apply_registry_auto_compat_with(
        json!({"model": "m", "messages": [], "service_tier": "flex"}),
        &req(json!({"originalBody": {"service_tier": "flex"}})),
        &r,
        "chat",
        &model,
    );
    assert_eq!(out["service_tier"], "default");
    for (suffix, expected) in [
        ("standard", "default"),
        ("ultrafast", "priority"),
        ("fast", "priority"),
    ] {
        let out = apply_registry_auto_compat_with(
            json!({"model": "m", "messages": [], "service_tier": suffix}),
            &req(json!({"serviceTier": suffix, "originalBody": {"service_tier": suffix}})),
            &default_route(),
            "chat",
            &model,
        );
        assert_eq!(out["service_tier"], expected, "{suffix}");
    }
    let key_default = req(
        json!({"originalBody": {"messages": []}, "metadata": {"plexus_metadata": {"defaultServiceTier": "fast"}}}),
    );
    let out = apply_registry_auto_compat_with(
        json!({"model": "m", "messages": [], "service_tier": "priority"}),
        &key_default,
        &default_route(),
        "chat",
        &model,
    );
    assert!(out.get("service_tier").is_none());
    let out = apply_registry_auto_compat_with(
        json!({"model": "m", "messages": [], "service_tier": "flex"}),
        &req(json!({"originalBody": {"service_tier": "flex"}})),
        &default_route(),
        "chat",
        &model,
    );
    assert_eq!(out["service_tier"], "flex");
    let null_map = overlay(json!({"chat": {"serviceTierMap": {"flex": null}}}));
    let kd = req(
        json!({"originalBody": {}, "metadata": {"plexus_metadata": {"defaultServiceTier": "flex"}}}),
    );
    let mut plain = null_map.clone();
    plain.config["auto_compat"] = json!(false);
    assert!(apply_registry_auto_compat_with(
        json!({"service_tier": "flex"}),
        &kd,
        &plain,
        "chat",
        &model
    )
    .get("service_tier")
    .is_none());
}

#[test]
fn quirk_overlay_semantics() {
    let base = json!({"id": "b", "api": "openai-completions", "reasoning": true, "thinkingLevelMap": {"high": "high"}, "compat": {"a": 1}});
    let out = apply_quirk_overlay(
        &base,
        Some(&json!({"chat": {"reasoning": false, "compat": {"b": 2}}})),
        "chat",
        "b",
    );
    assert_eq!(
        out,
        json!({"id": "b", "api": "openai-completions", "reasoning": false, "compat": {"a": 1, "b": 2}})
    );
    let out = apply_quirk_overlay(
        &base,
        Some(
            &json!({"chat": {"serviceTierMap": {"priority": "p"}, "models": {"b": {"maxTokens": 9}}}}),
        ),
        "chat:sub",
        "b",
    );
    assert_eq!(out["maxTokens"], 9);
    assert_eq!(out["thinkingLevelMap"], json!({"high": "high"}));
    assert_eq!(apply_quirk_overlay(&base, None, "chat", "b"), base);
    let inline = resolve_inline_quirks(Some(&json!({"chat": {"reasoning": true, "thinkingLevelMap": {}, "models": {"m": {"reasoning": false}}}})), "chat", "m").unwrap();
    assert_eq!(inline, json!({"reasoning": false, "compat": {}}));
}

#[test]
fn strip_and_retry_planners() {
    assert_eq!(
        match_unsupported_parameter(r#"{"detail":"Unsupported parameter: safety_identifier"}"#)
            .as_deref(),
        Some("safety_identifier")
    );
    assert_eq!(
        match_unsupported_parameter(
            r#"{"error":{"message":"Unknown parameter: 'messages[0].name'"}}"#
        )
        .as_deref(),
        Some("messages.0.name")
    );
    assert_eq!(
        match_unsupported_parameter("unknown parameter `reasoning`").as_deref(),
        Some("reasoning")
    );
    let mut state = UnsupportedParamStripState::default();
    assert_eq!(
        plan_unsupported_param_strip("Unsupported parameter: messages", &mut state),
        None
    );
    assert_eq!(
        plan_unsupported_param_strip("Unsupported parameter: input[2]", &mut state),
        None
    );
    assert_eq!(state.attempts, 0);
    assert_eq!(
        plan_unsupported_param_strip("Unsupported parameter: a", &mut state).as_deref(),
        Some("a")
    );
    assert_eq!(
        plan_unsupported_param_strip("Unsupported parameter: a", &mut state),
        None
    );
    assert_eq!(
        plan_unsupported_param_strip("Unsupported parameter: b", &mut state).as_deref(),
        Some("b")
    );
    assert_eq!(
        plan_unsupported_param_strip("Unsupported parameter: c", &mut state),
        None
    );

    let p = json!({"messages": [{"role": "user", "name": "x"}, {"role": "assistant"}], "tools": [1, 2, 3], "reasoning": {"summary": "auto", "effort": "high"}});
    assert_eq!(
        delete_dotted_path(p.clone(), "messages.0.name").0["messages"][0],
        json!({"role": "user"})
    );
    assert_eq!(
        delete_dotted_path(p.clone(), "tools.1").0["tools"],
        json!([1, 3])
    );
    assert_eq!(
        delete_dotted_path(p.clone(), "reasoning.summary").0["reasoning"],
        json!({"effort": "high"})
    );
    assert_eq!(
        delete_dotted_path(p.clone(), "tools.01"),
        (p.clone(), false)
    );
    assert_eq!(
        delete_dotted_path(p.clone(), "tools.length"),
        (p.clone(), false)
    );
    assert_eq!(
        delete_dotted_path(p.clone(), "__proto__.x"),
        (p.clone(), false)
    );
    assert_eq!(
        delete_dotted_path(p.clone(), "missing.x"),
        (p.clone(), false)
    );

    let sig = "messages.3.content.0: Invalid `signature` in `thinking` block";
    let mut st = auto_compat::StripState::default();
    let msgs = json!({"messages": [
        {"role": "user", "content": "q"},
        {"role": "assistant", "content": [{"type": "thinking", "thinking": "t", "signature": "s"}]},
        {"role": "user", "content": "q2"},
        {"role": "assistant", "content": [{"type": "redacted_thinking"}, {"type": "text", "text": "a"}]},
        {"role": "assistant", "content": [{"type": "thinking"}]},
    ]});
    assert!(plan_thinking_signature_strip(sig, &msgs, &mut st));
    assert!(!plan_thinking_signature_strip(sig, &msgs, &mut st));
    let (out, n) = strip_thinking_signature_blocks(msgs);
    assert_eq!(n, 3);
    assert_eq!(
        out["messages"],
        json!([
            {"role": "user", "content": "q"},
            {"role": "assistant", "content": [{"type": "text", "text": "[reasoning elided]"}]},
            {"role": "user", "content": "q2"},
            {"role": "assistant", "content": [{"type": "text", "text": "a"}]},
        ])
    );
    refund_strip(&mut st);
    assert_eq!(st.attempts, 0);

    let adv = json!({"messages": [
        {"role": "user", "content": "q"},
        {"role": "assistant", "content": [{"type": "server_tool_use", "id": "srvtoolu_1", "name": "advisor"}, {"type": "advisor_tool_result", "tool_use_id": "srvtoolu_1"}]},
        {"role": "user", "content": "next"},
    ]});
    assert!(match_advisor_result_error(
        "Advisor tool result content could not be processed."
    ));
    let (out, n) = strip_advisor_result_blocks(adv);
    assert_eq!(n, 1);
    assert_eq!(
        out["messages"][1],
        json!({"role": "assistant", "content": [{"type": "text", "text": "[advisor result elided]"}]})
    );

    let (out, n) = strip_lite_unsupported_tools(
        json!({"tools": [{"type": "function"}, {"type": "web_search"}, {"type": "tool_search"}, {"type": "custom"}]}),
    );
    assert_eq!((out["tools"].as_array().unwrap().len(), n), (3, 1));
    let mut lite = auto_compat::StripState::default();
    assert!(plan_lite_tool_strip("X-OpenAI-Internal-Codex-Responses-Lite only supports function tools, custom tools, and client-executed tool search.", &mut lite));
}

#[test]
fn session_key_matches_bun_hash() {
    // TS: `m:${Bun.hash(JSON.stringify(messages.slice(0, 2))).toString(16)}`
    let r = json!({"messages": [{"role": "user", "content": "a"}, {"role": "assistant", "content": "b"}, {"role": "user", "content": "c"}]});
    assert_eq!(
        derive_cache_injection_key(&r).as_deref(),
        Some("m:6f84e05be1570769")
    );
}

#[test]
fn tool_id_hash_matches_node() {
    // node: createHash('sha256').update(Buffer.from(id, 'utf16le')).digest('hex').slice(0, 8)
    assert_eq!(
        adapters::sanitize_anthropic_tool_id(Some(&json!("functions.WebSearch:3"))),
        "functions_WebSearch_3_0ad6c974"
    );
}

// ─── request payload builder ─────────────────────────────────────────────

struct FakeTransformer;
impl Transformer for FakeTransformer {
    fn name(&self) -> &'static str {
        "chat"
    }
    fn default_endpoint(&self) -> &'static str {
        "/chat/completions"
    }
    fn endpoint(&self, request: &Value) -> String {
        if request.get("stream") == Some(&json!(true)) {
            "/stream".into()
        } else {
            "/chat/completions".into()
        }
    }
    fn parse_request(&self, input: &Value) -> EngineResult<Value> {
        Ok(input.clone())
    }
    fn transform_request(&self, request: &Value) -> EngineResult<Value> {
        Ok(
            json!({"model": request["model"], "messages": request["messages"], "seen_oauth_provider": request["metadata"]["plexus_metadata"]["oauthProvider"], "stream": request.get("stream").cloned().unwrap_or(json!(false)), "tools": [{"name": "t"}]}),
        )
    }
    fn transform_response(&self, r: &Value) -> EngineResult<Value> {
        Ok(r.clone())
    }
    fn format_response(&self, r: &Value) -> EngineResult<Value> {
        Ok(r.clone())
    }
}

#[test]
fn payload_passthrough_and_merges() {
    let config = Config::new(
        json!({"models": {"alias": {"extraBody": {"z": 3, "a": "alias"}, "advanced": [{"type": "strip_adaptive_thinking"}]}}}),
    );
    let mut r = route(
        "prov-model",
        json!({"api_base_url": "https://x/v1", "api_key": "k", "extraBody": {"a": "provider", "b": 1}}),
        Some(json!({"extraBody": {"b": 2}})),
    );
    r.canonical_model = Some("alias".into());
    let request = json!({"model": "alias", "incomingApiType": "chat", "metadata": {"user": "u", "plexus_metadata": {"x": 1}},
        "originalBody": {"model": "alias", "messages": [], "reasoning_effort": "low"}});
    let out = build_request_payload(&request, &r, &FakeTransformer, "chat", &[], &config).unwrap();
    assert!(out.bypass_transformation);
    assert_eq!(
        stringify(&out.payload),
        r#"{"model":"prov-model","messages":[],"reasoning_effort":"low","metadata":{"user":"u"},"a":"alias","b":2,"z":3}"#
    );
}

#[test]
fn payload_transformed_with_adapters_and_lite() {
    let config = Config::default();
    let r = route(
        "gpt-5",
        json!({"api_base_url": "https://x/v1", "api_key": "k", "adapter": [{"name": "reasoning_content"}]}),
        None,
    );
    let adapters = resolve_adapters(&r, Some("responses:lite"));
    let request =
        json!({"model": "alias", "incomingApiType": "chat", "messages": [], "stream": false});
    let out = build_request_payload(
        &request,
        &r,
        &FakeTransformer,
        "responses:lite",
        &adapters,
        &config,
    )
    .unwrap();
    assert!(!out.bypass_transformation);
    assert_eq!(out.payload["seen_oauth_provider"], "p");
    assert_eq!(out.payload["parallel_tool_calls"], false);
    assert_eq!(out.payload["tools"], json!([]));
    assert_eq!(out.payload["reasoning"], json!({"context": "all_turns"}));
}

#[test]
fn lite_payload_enforces_reasoning_context() {
    let original_input = json!([
        {"type": "additional_tools", "role": "developer", "tools": []},
        {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}
    ]);
    let cases: [(&str, Option<Value>); 5] = [
        ("omitted", None),
        ("null", Some(Value::Null)),
        ("empty", Some(json!({}))),
        (
            "incompatible",
            Some(json!({"effort": "high", "summary": "auto", "context": "last_turn"})),
        ),
        ("valid", Some(json!({"effort": "high", "context": "all_turns"}))),
    ];
    for (label, reasoning) in cases {
        let r = route(
            "gpt-6.1-terra",
            json!({"api_base_url": "https://x/v1", "api_key": "k"}),
            None,
        );
        let mut body = json!({"model": "gpt-6.1-terra", "input": original_input});
        if let Some(reasoning) = reasoning.clone() {
            body["reasoning"] = reasoning;
        }
        let request = json!({"model": "alias", "messages": [], "incomingApiType": "responses:lite", "originalBody": body});
        let out = build_request_payload(
            &request,
            &r,
            &FakeTransformer,
            "responses:lite",
            &[],
            &Config::default(),
        )
        .unwrap();
        assert!(out.bypass_transformation, "{label}");
        let mut expected = reasoning
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default();
        expected.insert("context".into(), json!("all_turns"));
        assert_eq!(out.payload["reasoning"], Value::Object(expected), "{label}");
        assert_eq!(out.payload["input"], original_input, "{label}");
        // The caller's original body is never mutated.
        assert_eq!(request["originalBody"], body, "{label}");
    }
}

#[test]
fn base_responses_target_does_not_get_lite_reasoning() {
    let r = route(
        "gpt-5",
        json!({"api_base_url": "https://x/v1", "api_key": "k"}),
        None,
    );
    let request = json!({"model": "alias", "messages": [], "incomingApiType": "responses",
        "originalBody": {"model": "gpt-5", "input": []}});
    let out = build_request_payload(
        &request,
        &r,
        &FakeTransformer,
        "responses",
        &[],
        &Config::default(),
    )
    .unwrap();
    assert!(out.payload.get("reasoning").is_none());
}

#[test]
fn payload_plans_oauth_routes() {
    let r = route(
        "m",
        json!({"api_base_url": "oauth://x", "oauth_provider": "anthropic"}),
        None,
    );
    let out = build_request_payload(
        &json!({"incomingApiType": "messages"}),
        &r,
        &FakeTransformer,
        "messages",
        &[],
        &Config::default(),
    )
    .unwrap();
    let plan = out.oauth.unwrap();
    assert!(
        plan.native
            && plan.provider == "anthropic"
            && !plan.masking_route
            && out.bypass_transformation
    );
    let mask = route(
        "m",
        json!({"api_base_url": "https://api.anthropic.com", "useClaudeMasking": true}),
        None,
    );
    let out = build_request_payload(
        &json!({"incomingApiType": "chat"}),
        &mask,
        &FakeTransformer,
        "messages",
        &[],
        &Config::default(),
    )
    .unwrap();
    assert_eq!(out.payload["seen_oauth_provider"], "anthropic");
    assert!(out.oauth.unwrap().masking_route && !out.bypass_transformation);
    let generic = route(
        "grok",
        json!({"api_base_url": "oauth://xai", "oauth_provider": "xai"}),
        None,
    );
    let plan = build_request_payload(
        &json!({}),
        &generic,
        &FakeTransformer,
        "chat",
        &[],
        &Config::default(),
    )
    .unwrap()
    .oauth
    .unwrap();
    assert!(!plan.native && plan.provider == "xai");
}

#[test]
fn payload_anthropic_eager_and_clamp() {
    let r = route(
        "claude-sonnet-4-5",
        json!({"api_base_url": "https://api.anthropic.com/v1", "api_key": "k"}),
        None,
    );
    let request =
        json!({"model": "alias", "incomingApiType": "chat", "messages": [], "stream": true});
    let out = build_request_payload(
        &request,
        &r,
        &FakeTransformer,
        "messages",
        &[],
        &Config::default(),
    )
    .unwrap();
    assert_eq!(
        out.payload["tools"],
        json!([{"name": "t", "eager_input_streaming": true}])
    );
}

#[test]
fn headers_and_url() {
    let r = route(
        "m",
        json!({"api_base_url": "https://api.anthropic.com/v1", "api_key": "sk", "headers": {"Anthropic-Beta": "a,b"}, "cache_key_injection": "x-session-id"}),
        None,
    );
    let request = json!({"stream": true, "anthropicBeta": "ignored-by-config-casing", "cacheRoutingHeaders": {"session_id": "s1", "x-client-request-id": "c1"}});
    let mut h = setup_headers(&r, "messages", &request).unwrap();
    assert_eq!(
        stringify(&Value::Object(h.clone())),
        r#"{"Content-Type":"application/json","Accept":"text/event-stream","x-api-key":"sk","anthropic-version":"2023-06-01","Anthropic-Beta":"a,b","session-id":"s1","x-client-request-id":"c1","anthropic-beta":"ignored-by-config-casing","x-session-id":"s1"}"#
    );
    let mut only_cfg = Headers::new();
    only_cfg.insert("Anthropic-Beta".into(), json!("a, b"));
    apply_auto_anthropic_betas(&mut only_cfg, &json!({"speed": "fast"}), "messages");
    assert_eq!(only_cfg["Anthropic-Beta"], "a,b,fast-mode-2026-02-01");
    apply_auto_anthropic_betas(&mut h, &json!({"speed": "standard"}), "messages");
    assert!(!h.contains_key("x-unused"));

    let g = route(
        "gemini-2.5-pro",
        json!({"api_base_url": "https://generativelanguage.googleapis.com/v1beta", "api_key": "g"}),
        None,
    );
    let gh = setup_headers(&g, "gemini", &json!({})).unwrap();
    assert_eq!(gh["x-goog-api-key"], "g");
    let c = route(
        "m",
        json!({"api_base_url": "https://x/v1", "api_key": "k"}),
        None,
    );
    let ch = setup_headers(&c, "responses:lite", &json!({})).unwrap();
    assert_eq!(ch["Authorization"], "Bearer k");
    assert_eq!(ch["x-openai-internal-codex-responses-lite"], "true");
    assert!(setup_headers(
        &route("m", json!({"api_base_url": "https://x"}), None),
        "chat",
        &json!({})
    )
    .unwrap_err()
    .message
    .contains("No API key configured for provider 'p'"));
    assert_eq!(
        build_request_url(&c, &FakeTransformer, &json!({"stream": true}), "chat").unwrap(),
        "https://x/v1/stream"
    );
}
