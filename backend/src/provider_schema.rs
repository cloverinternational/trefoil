//! Transcription of pinned Plexus `ProviderConfigSchema` (`config.ts`), its
//! shared sub-schemas (`@plexus/shared` provider/provider-presets) and the
//! quota-checker option schemas, on the zod emulation in `crate::zod`.
//! Keep declaration order identical to the reference: it determines issue
//! order and output key order.
use crate::zod::*;
use serde_json::{json, Value};
use std::sync::OnceLock;

fn is_oauth_placeholder(value: &str) -> bool {
    value.trim().to_lowercase().starts_with("oauth://")
}
fn valid_url_or_oauth(value: &Value) -> bool {
    value
        .as_str()
        .is_some_and(|v| is_oauth_placeholder(v) || url::Url::parse(v).is_ok())
}
fn map_has_oauth(value: &Value) -> bool {
    value.as_object().is_some_and(|m| {
        m.values()
            .any(|v| v.as_str().is_some_and(is_oauth_placeholder))
    })
}
/// `isOAuthProviderConfig` over the parsed output.
pub fn is_oauth_provider_config(value: &Value) -> bool {
    match value.get("api_base_url") {
        Some(Value::String(s)) => is_oauth_placeholder(s),
        Some(map) => map_has_oauth(map),
        None => false,
    }
}

fn pricing_range() -> Schema {
    object(vec![
        ("lower_bound", number().min(0.0).default(0)),
        (
            "upper_bound",
            number().nullable().default(Value::Null).transform(|v| v),
        ),
        ("input_per_m", number().min(0.0)),
        ("output_per_m", number().min(0.0)),
        ("cached_per_m", number().min(0.0).optional()),
        ("cache_write_per_m", number().min(0.0).optional()),
    ])
}

fn pricing() -> Schema {
    discriminated(
        "source",
        vec![
            object(vec![
                ("source", literal("openrouter")),
                ("slug", string()),
                ("discount", number().min(0.0).max(1.0).optional()),
            ]),
            object(vec![
                ("source", literal("defined")),
                ("range", array(pricing_range()).min(1.0)),
            ]),
            object(vec![
                ("source", literal("simple")),
                ("input", number().min(0.0)),
                ("output", number().min(0.0)),
                ("cached", number().min(0.0).optional()),
                ("cache_write", number().min(0.0).optional()),
            ]),
            object(vec![
                ("source", literal("per_request")),
                ("amount", number().min(0.0)),
            ]),
        ],
    )
}

/// `AdapterConfigSchema`: preprocess legacy string/single forms, then
/// `z.array(AdapterEntrySchema).optional()`.
pub fn adapter() -> Schema {
    preprocess(
        |value| {
            let value = value?;
            if value.is_null() {
                return None;
            }
            let items = match value {
                Value::Array(items) => items.clone(),
                other => vec![other.clone()],
            };
            Some(Value::Array(
                items
                    .into_iter()
                    .map(|entry| match entry {
                        Value::String(name) => {
                            json!({"name": name, "options": {}, "enabled": true})
                        }
                        Value::Object(ref map) if map.contains_key("name") => {
                            let options = map
                                .get("options")
                                .filter(|v| !v.is_null())
                                .cloned()
                                .unwrap_or(json!({}));
                            let enabled = map
                                .get("enabled")
                                .filter(|v| !v.is_null())
                                .cloned()
                                .unwrap_or(json!(true));
                            json!({"name": map["name"], "options": options, "enabled": enabled})
                        }
                        other => other,
                    })
                    .collect(),
            ))
        },
        array(object(vec![
            ("name", string().min(1.0)),
            ("options", record(string(), any()).default(json!({}))),
            ("enabled", boolean().default(true)),
        ]))
        .optional(),
    )
}

fn api_format() -> Schema {
    object(vec![
        ("type", trimmed().min(1.0)),
        ("subtype", trimmed().min(1.0).optional()),
    ])
}

fn model_provider_config() -> Schema {
    object(vec![
        (
            "pricing",
            pricing().default(json!({"source": "simple", "input": 0, "output": 0})),
        ),
        (
            "access_via",
            array(union(vec![trimmed().min(1.0), api_format()])).optional(),
        ),
        (
            "type",
            enumeration(&[
                "text",
                "embeddings",
                "transcriptions",
                "speech",
                "image",
                "decisions",
            ])
            .optional(),
        ),
        ("extraBody", record(string(), any()).optional()),
        ("adapter", adapter()),
        ("auto_compat", boolean().optional()),
        (
            "maxConcurrency",
            number().int().positive().nullable().optional(),
        ),
        ("pi_ai_model_id", string().optional()),
    ])
}

const CACHE_KEY_INJECTION: &[&str] = &[
    "off",
    "prompt_cache_key",
    "session_id",
    "x-client-request-id",
    "x-session-affinity",
    "x-session-id",
    "x-prompt-cache-isolation-key",
    "x-multi-turn-session-id",
];
pub const RESPONSES_EXTENSIONS: &[&str] = &[
    "namespace_tools",
    "namespaced_calls",
    "dotted_calls",
    "custom_tools",
    "custom_calls",
    "additional_tools",
    "tool_search",
];

fn optional_nullable_strings(keys: &[&'static str]) -> Schema {
    object(
        keys.iter()
            .map(|k| (*k, string().nullable().optional()))
            .collect(),
    )
    .strict()
}
fn quirk_traits() -> Schema {
    object(vec![
        ("reasoning", boolean().optional()),
        (
            "thinkingLevelMap",
            optional_nullable_strings(&["off", "minimal", "low", "medium", "high", "xhigh", "max"])
                .optional(),
        ),
        (
            "serviceTierMap",
            optional_nullable_strings(&["auto", "standard", "flex", "priority", "ultrafast"])
                .optional(),
        ),
        ("maxTokens", number().int().positive().optional()),
        (
            "compat",
            object(vec![
                (
                    "thinkingFormat",
                    enumeration(&[
                        "zai",
                        "qwen",
                        "qwen-chat-template",
                        "deepseek",
                        "openrouter",
                        "ant-ling",
                        "together",
                        "string-thinking",
                    ])
                    .optional(),
                ),
                ("supportsTemperature", boolean().optional()),
                (
                    "maxTokensField",
                    enumeration(&["max_tokens", "max_completion_tokens"]).optional(),
                ),
                ("forceAdaptiveThinking", boolean().optional()),
                ("supportsReasoningEffort", boolean().optional()),
                (
                    "serviceTierFormat",
                    enumeration(&["service-tier", "anthropic-speed"]).optional(),
                ),
            ])
            .strict()
            .optional(),
        ),
    ])
    .strict()
}
fn quirk_target(api: &'static str) -> Schema {
    quirk_traits().extend(vec![
        ("api", literal(api)),
        (
            "models",
            record(string().min(1.0), quirk_traits()).optional(),
        ),
    ])
}
pub fn pi_ai_quirks() -> Schema {
    object(vec![
        ("chat", quirk_target("openai-completions").optional()),
        ("completions", quirk_target("openai-completions").optional()),
        ("messages", quirk_target("anthropic-messages").optional()),
        ("responses", quirk_target("openai-responses").optional()),
        ("gemini", quirk_target("google-generative-ai").optional()),
    ])
    .strict()
    .refine(
        |q| q.as_object().is_some_and(|m| !m.is_empty()),
        "piAiQuirks must define at least one target API",
    )
    .super_refine(|quirks, path, issues| {
        let Some(targets) = quirks.as_object() else {
            return;
        };
        for (target, definition) in targets {
            let reasoning = definition.get("reasoning");
            if definition.get("thinkingLevelMap").is_some()
                && reasoning == Some(&Value::Bool(false))
            {
                let mut p = path.clone();
                p.extend([json!(target), json!("thinkingLevelMap")]);
                issues.push(Issue::custom(
                    &p,
                    "thinkingLevelMap cannot accompany reasoning: false on this target",
                ));
            }
            if let Some(models) = definition.get("models").and_then(Value::as_object) {
                for (model_id, model) in models {
                    let effective = model.get("reasoning").or(reasoning);
                    if model.get("thinkingLevelMap").is_some()
                        && effective == Some(&Value::Bool(false))
                    {
                        let mut p = path.clone();
                        p.extend([
                            json!(target),
                            json!("models"),
                            json!(model_id),
                            json!("thinkingLevelMap"),
                        ]);
                        issues.push(Issue::custom(
                            &p,
                            "thinkingLevelMap cannot accompany reasoning: false for this model",
                        ));
                    }
                }
            }
        }
    })
}

pub fn compaction_override() -> Schema {
    object(vec![
        ("enabled", boolean().optional()),
        ("strategy", enumeration(&["native", "headroom"]).optional()),
        ("triggerRatio", number().min(0.0).max(1.0).optional()),
        (
            "absoluteTriggerTokens",
            number().int().positive().nullable().optional(),
        ),
        ("minTokens", number().int().min(0.0).optional()),
        ("protectRecent", number().int().min(0.0).optional()),
        (
            "native",
            object(vec![
                ("maxArrayItems", number().int().positive().optional()),
                ("maxStringChars", number().int().positive().optional()),
            ])
            .optional(),
        ),
        (
            "headroom",
            object(vec![
                ("baseUrl", string().url().optional()),
                ("apiKey", string().optional()),
                (
                    "targetRatio",
                    number().min(0.0).max(1.0).nullable().optional(),
                ),
                (
                    "timeoutMs",
                    number().int().min(100.0).max(60000.0).optional(),
                ),
            ])
            .optional(),
        ),
    ])
}

fn raw_passthrough() -> Schema {
    object(vec![
        ("enabled", boolean().default(false)),
        (
            "base_url",
            string().url().refine(
                |v| {
                    v.as_str()
                        .and_then(|s| url::Url::parse(s).ok())
                        .is_some_and(|u| {
                            matches!(u.scheme(), "http" | "https")
                                && u.query().is_none()
                                && u.fragment().is_none()
                        })
                },
                "raw_passthrough.base_url must be HTTP(S) without a query or fragment",
            ),
        ),
        (
            "auth",
            enumeration(&["bearer", "x-api-key", "x-goog-api-key"]).default("bearer"),
        ),
    ])
}

fn build_provider_config() -> Schema {
    object(vec![
        ("display_name", string().optional()),
        ("api_base_url", union(vec![
            string().refine(valid_url_or_oauth, "api_base_url must be a valid URL or oauth://"),
            record(string(), string()).refine(|m| !map_has_oauth(m), "api_base_url map entries must be real URLs; use the string form api_base_url: 'oauth://' for OAuth providers"),
        ])),
        ("api_key", string().optional()),
        ("oauth_provider", string().refine(|v| v.as_str().is_some_and(crate::oauth::is_supported_provider), "oauth_provider must be a supported OAuth provider id").optional()),
        ("oauth_account", string().min(1.0).optional()),
        ("enabled", boolean().default(true).optional()),
        ("disable_cooldown", boolean().optional().default(false)),
        ("stall_cooldown", boolean().optional().default(false)),
        ("allow_100_percent_utilization", boolean().optional().default(false)),
        ("discount", number().min(0.0).max(1.0).optional()),
        ("models", union(vec![array(string()), record(string(), model_provider_config())]).optional()),
        ("headers", record(string(), string()).optional()),
        ("extraBody", record(string(), any()).optional()),
        ("estimateTokens", boolean().optional().default(false)),
        ("useClaudeMasking", boolean().optional().default(false)),
        ("cache_key_injection", enumeration(CACHE_KEY_INJECTION).optional()),
        ("responses_extensions", array(enumeration(RESPONSES_EXTENSIONS)).optional()),
        ("quota_checker", object(vec![
            ("type", trimmed().min(1.0)),
            ("enabled", boolean().default(true)),
            ("intervalMinutes", number().min(1.0).default(30)),
            ("id", trimmed().min(1.0).optional()),
            ("options", record(string(), any()).default(json!({}))),
        ]).optional()),
        ("model_autosync", object(vec![("enabled", boolean().default(false)), ("intervalMinutes", number().int().min(1.0).default(60))]).optional()),
        ("geminiThinkingEnabled", boolean().optional()),
        ("adapter", adapter()),
        ("auto_compat", boolean().optional()),
        ("timeoutMs", number().int().positive().optional()),
        ("maxConcurrency", number().int().positive().nullable().optional()),
        ("stallTtfbMs", number().int().min(5000.0).max(120000.0).nullable().optional()),
        ("stallTtfbBytes", number().int().min(50.0).max(10000.0).nullable().optional()),
        ("stallMinBps", number().int().min(50.0).max(5000.0).nullable().optional()),
        ("stallWindowMs", number().int().min(3000.0).max(30000.0).nullable().optional()),
        ("stallGracePeriodMs", number().int().min(0.0).max(120000.0).nullable().optional()),
        ("pi_ai_provider", string().optional()),
        ("pi_ai_quirks", pi_ai_quirks().optional()),
        ("compaction", compaction_override().optional()),
        ("raw_passthrough", raw_passthrough().optional()),
    ])
    .refine(|d| d.get("api_key").and_then(Value::as_str).is_some_and(|k| !k.is_empty()) || is_oauth_provider_config(d), "'api_key' must be specified for provider")
    .refine(|d| !is_oauth_provider_config(d) || d.get("oauth_provider").and_then(Value::as_str).is_some_and(|p| !p.is_empty()), "'oauth_provider' must be specified when using oauth://")
    .refine(|d| d.pointer("/raw_passthrough/enabled") != Some(&Value::Bool(true)) || !is_oauth_provider_config(d), "raw_passthrough currently supports static API-key providers only")
}

pub fn provider_config() -> &'static Schema {
    static SCHEMA: OnceLock<Schema> = OnceLock::new();
    SCHEMA.get_or_init(build_provider_config)
}

fn checker_schema(kind: &str) -> Option<Schema> {
    let endpoint = || ("endpoint", string().url().optional());
    let key = |message: &'static str| ("apiKey", string().min_msg(1.0, message));
    let oauth = || {
        vec![
            ("apiKey", string().optional()),
            ("oauthAccountId", string().optional()),
            ("oauthProvider", string().optional()),
            endpoint(),
        ]
    };
    let trimmed_opt = || trimmed().min(1.0).optional();
    Some(object(match kind {
        "apertis" => vec![
            key("Apertis API key is required"),
            endpoint(),
            (
                "mode",
                enumeration(&["subscription", "payg"])
                    .optional()
                    .default("subscription"),
            ),
        ],
        "claude-code" => oauth(),
        "cline" => vec![key("Cline API key is required"), endpoint()],
        "copilot" => {
            let mut v = oauth();
            v.extend([
                ("userAgent", trimmed_opt()),
                ("editorVersion", trimmed_opt()),
                ("apiVersion", trimmed_opt()),
                ("timeoutMs", number().int().positive().optional()),
            ]);
            v
        }
        "crof" | "exedev" | "routing-run" => vec![endpoint()],
        "deepseek" => vec![key("DeepSeek API key is required"), endpoint()],
        "devpass" => vec![
            (
                "session",
                trimmed().min_msg(1.0, "DevPass session cookie is required"),
            ),
            endpoint(),
        ],
        "hyper" => vec![key("Hyper API key is required"), endpoint()],
        "kilo" => vec![
            key("Kilo API key is required"),
            endpoint(),
            ("organizationId", trimmed_opt()),
        ],
        "kimi-code" => vec![key("Kimi Code API key is required"), endpoint()],
        "minimax" => vec![
            (
                "groupid",
                trimmed().min_msg(1.0, "MiniMax groupid is required"),
            ),
            (
                "token",
                trimmed().min_msg(1.0, "MiniMax _token cookie value is required"),
            ),
        ],
        "minimax-coding" => vec![key("MiniMax Coding API key is required"), endpoint()],
        "moonshot" => vec![key("Moonshot API key is required"), endpoint()],
        "muse-code" => {
            let mut v = oauth();
            v.extend([
                ("model", string().optional()),
                ("timeoutMs", number().int().positive().optional()),
            ]);
            v
        }
        "naga" => vec![
            key("Naga provisioning key is required"),
            ("max", number().positive().optional()),
            endpoint(),
        ],
        "nanogpt" => vec![key("NanoGPT API key is required"), endpoint()],
        "neuralwatt" => vec![key("Neuralwatt API key is required"), endpoint()],
        "novita" => vec![key("Novita API key is required"), endpoint()],
        "ollama" => vec![
            (
                "sessionCookie",
                string().min_msg(1.0, "Ollama session cookie is required"),
            ),
            endpoint(),
        ],
        "openai-codex" => {
            let mut v = oauth();
            v.extend([
                ("userAgent", trimmed_opt()),
                ("timeoutMs", number().int().positive().optional()),
            ]);
            v
        }
        "opencode-go" => vec![
            (
                "workspaceId",
                string().min_msg(1.0, "OpenCode Go workspace ID is required"),
            ),
            (
                "authCookie",
                string().min_msg(1.0, "OpenCode Go auth cookie is required"),
            ),
            endpoint(),
        ],
        "openrouter" => vec![key("OpenRouter management key is required"), endpoint()],
        "poe" => vec![key("POE API key is required"), endpoint()],
        "sakana" => vec![
            (
                "sessionCookie",
                trimmed().min_msg(1.0, "Sakana session cookie is required"),
            ),
            endpoint(),
        ],
        "synthetic" => vec![
            ("apiKey", string().optional()),
            endpoint(),
            (
                "maxUtilizationPercent",
                number().min(1.0).max(100.0).optional(),
            ),
        ],
        "wisdomgate" => vec![
            (
                "session",
                trimmed().min_msg(1.0, "Session cookie is required"),
            ),
            endpoint(),
        ],
        "zai" => vec![key("ZAI API key is required"), endpoint()],
        "zenmux" => vec![
            (
                "managementApiKey",
                string().min_msg(1.0, "Zenmux management API key is required"),
            ),
            endpoint(),
        ],
        _ => return None,
    }))
}

/// `validateProviderQuotaChecker`: checker options plus inherited
/// credentials, validated against the checker type's schema. Custom checker
/// types accept any options.
pub fn validate_quota_checker(config: &Value, custom_types: &[String]) -> Result<(), Vec<Value>> {
    let Some(checker) = config.get("quota_checker").filter(|v| v.is_object()) else {
        return Ok(());
    };
    let kind = checker["type"].as_str().unwrap_or_default();
    let mut options = checker
        .get("options")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if let Some(key) = config
        .get("api_key")
        .and_then(Value::as_str)
        .filter(|k| !k.is_empty() && k.to_lowercase() != "oauth")
    {
        options.entry("apiKey").or_insert_with(|| json!(key));
    }
    if let Some(provider) = config
        .get("oauth_provider")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
    {
        options
            .entry("oauthProvider")
            .or_insert_with(|| json!(provider));
    }
    if let Some(account) = config
        .get("oauth_account")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
    {
        options
            .entry("oauthAccountId")
            .or_insert_with(|| json!(account));
    }
    if custom_types.iter().any(|t| t == kind) {
        return Ok(());
    }
    match checker_schema(kind) {
        Some(schema) => schema.safe_parse(&Value::Object(options)).map(|_| ()),
        None => Err(vec![
            json!({"code": "custom", "path": ["type"], "message": format!("Unknown quota checker type '{kind}'")}),
        ]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_with_reference_defaults_and_issues() {
        let out = provider_config().safe_parse(&json!({"api_base_url":"https://b.example/v1","api_key":"sk","models":{"x":{}},"junk":1})).unwrap();
        assert_eq!(
            out,
            json!({"api_base_url":"https://b.example/v1","api_key":"sk","enabled":true,"disable_cooldown":false,"stall_cooldown":false,"allow_100_percent_utilization":false,"models":{"x":{"pricing":{"source":"simple","input":0,"output":0}}},"estimateTokens":false,"useClaudeMasking":false})
        );
        let issues = provider_config()
            .safe_parse(&json!({"api_base_url":{"chat":"oauth://"},"api_key":"sk"}))
            .unwrap_err();
        assert_eq!(
            issues,
            vec![
                json!({"code":"custom","message":"api_base_url map entries must be real URLs; use the string form api_base_url: 'oauth://' for OAuth providers","path":["api_base_url"]}),
                json!({"code":"custom","message":"'oauth_provider' must be specified when using oauth://","path":[]}),
            ]
        );
    }
}

#[cfg(test)]
mod corpus {
    /// `PROVIDER_SCHEMA_CORPUS=<file>`: `[{input, expected:{ok}|{issues}}]`
    /// produced by running the pinned Plexus schema under Bun.
    #[test]
    #[ignore = "needs a corpus generated from the pinned Plexus checkout"]
    fn matches_reference_corpus() {
        let path = std::env::var("PROVIDER_SCHEMA_CORPUS").expect("PROVIDER_SCHEMA_CORPUS");
        let cases: Vec<serde_json::Value> =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let mut failures = 0;
        for case in &cases {
            let actual = match super::provider_config().safe_parse(&case["input"]) {
                Ok(ok) => serde_json::json!({"ok": ok}),
                Err(issues) => serde_json::json!({"issues": issues}),
            };
            if actual != case["expected"] {
                failures += 1;
                eprintln!(
                    "INPUT {}\n  plexus  {}\n  trefoil {}",
                    case["input"], case["expected"], actual
                );
            }
        }
        assert_eq!(
            failures,
            0,
            "{failures}/{} corpus cases differ",
            cases.len()
        );
    }
}
