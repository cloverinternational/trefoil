//! Explicit disposable-Postgres integration test; never uses DATABASE_URL.
use sqlx::Row;
use trefoil_backend::database;

/// Inference routes authenticate against configured API keys only (Plexus
/// parity: the admin key is not an inference credential), so the dispatch
/// cases register a key whose secret is the `x-api-key` they send.
fn inference_keys(
    secret: &str,
) -> std::collections::HashMap<String, trefoil_backend::state::KeyConfig> {
    [(
        "integration".to_owned(),
        trefoil_backend::state::KeyConfig {
            secret: secret.into(),
            ..Default::default()
        },
    )]
    .into_iter()
    .collect()
}

#[tokio::test]
#[ignore = "requires disposable TREFOIL_TEST_DATABASE_URL and TREFOIL_CREDENTIAL_KEY"]
async fn oauth_tokens_are_encrypted_and_round_trip_with_timestamps() {
    let url = std::env::var("TREFOIL_TEST_DATABASE_URL").expect("disposable test URL required");
    let pool = database::connect(&url).await.unwrap();
    database::migrate(&pool).await.unwrap();
    database::upsert_oauth_credential(
        &pool,
        "openrouter",
        "integration",
        "test-secret",
        "",
        i64::MAX,
    )
    .await
    .unwrap();
    let row = sqlx::query("SELECT access_token,refresh_token,created_at,updated_at FROM oauth_credentials WHERE account_id='integration'").fetch_one(&pool).await.unwrap();
    let access: String = row.get("access_token");
    let refresh: String = row.get("refresh_token");
    assert!(access.starts_with("v1:") && !access.contains("test-secret"));
    assert!(refresh.starts_with("v1:") && !refresh.is_empty());
    assert!(row.get::<i64, _>("created_at") > 0);
    assert!(row.get::<i64, _>("updated_at") > 0);
    let loaded = database::load_oauth_credentials(&pool).await.unwrap();
    assert!(loaded.iter().any(|(p, a, t, r, e)| p == "openrouter"
        && a == "integration"
        && t == "test-secret"
        && r.is_empty()
        && *e == i64::MAX));
    sqlx::query("UPDATE oauth_credentials SET account_id='moved' WHERE account_id='integration'")
        .execute(&pool)
        .await
        .unwrap();
    assert!(database::load_oauth_credentials(&pool).await.is_err());
}

/// OAuth login sessions are in-memory only (Plexus parity); completed logins
/// persist through `upsert_oauth_credential`, and the management routes read
/// timestamps / delete rows without decrypting tokens.
#[tokio::test]
#[ignore = "requires disposable TREFOIL_TEST_DATABASE_URL and TREFOIL_CREDENTIAL_KEY"]
async fn credential_timestamps_listing_and_delete_do_not_need_tokens() {
    let pool = database::connect(&std::env::var("TREFOIL_TEST_DATABASE_URL").unwrap())
        .await
        .unwrap();
    database::migrate(&pool).await.unwrap();
    database::upsert_oauth_credential(
        &pool,
        "github-copilot",
        "ts-test",
        "copilot",
        "corp.ghe.com\ngh-token",
        1234,
    )
    .await
    .unwrap();
    let (created, updated, expires) =
        database::oauth_credential_timestamps(&pool, "github-copilot", "ts-test")
            .await
            .unwrap()
            .unwrap();
    assert!(created > 0 && updated >= created);
    assert_eq!(expires, 1234);
    assert!(database::list_oauth_credential_accounts(&pool)
        .await
        .unwrap()
        .iter()
        .any(|(p, a, e)| p == "github-copilot" && a == "ts-test" && *e == 1234));
    assert!(
        database::delete_oauth_credential(&pool, "github-copilot", "ts-test")
            .await
            .unwrap()
    );
    assert!(
        !database::delete_oauth_credential(&pool, "github-copilot", "ts-test")
            .await
            .unwrap()
    );
    assert!(
        database::oauth_credential_timestamps(&pool, "github-copilot", "ts-test")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
#[ignore = "requires disposable TREFOIL_TEST_DATABASE_URL"]
async fn chat_failover_audit_has_contiguous_candidate_numbers() {
    use axum::{body::Body, http::Request, routing::post, Json, Router};
    use tower::ServiceExt;
    use trefoil_backend::{
        state::{ModelConfig, ModelTarget, ModelTargetGroup, ProviderConfig},
        AppState,
    };
    let pool = database::connect(&std::env::var("TREFOIL_TEST_DATABASE_URL").unwrap())
        .await
        .unwrap();
    database::migrate(&pool).await.unwrap();
    let app = Router::new().route(
        "/chat/completions",
        post(|Json(body): Json<serde_json::Value>| async move {
            if body["model"] == "third" {
                (
                    axum::http::StatusCode::OK,
                    Json(serde_json::json!({"choices":[],"usage":{}})),
                )
            } else {
                (
                    axum::http::StatusCode::SERVICE_UNAVAILABLE,
                    Json(serde_json::json!({"error":{"message":"retry"}})),
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let state = AppState::new(Some("admin".into()), inference_keys("inference-secret"))
        .with_database(pool.clone());
    for name in ["first", "second", "third"] {
        state.inner.providers.write().unwrap().insert(
            name.into(),
            ProviderConfig {
                api_base_url: format!("http://{addr}"),
                api_key: Some("fixture-key".into()),
                enabled: true,
                ..Default::default()
            },
        );
    }
    state.inner.aliases.write().unwrap().insert(
        "alias".into(),
        ModelConfig {
            target_groups: vec![ModelTargetGroup {
                name: "ordered".into(),
                selector: "in_order".into(),
                targets: ["first", "second", "third"]
                    .into_iter()
                    .map(|name| ModelTarget {
                        provider: Some(name.into()),
                        model: Some(name.into()),
                        enabled: true,
                        ..Default::default()
                    })
                    .collect(),
            }],
            ..Default::default()
        },
    );
    let response = trefoil_backend::app(state)
        .oneshot(
            Request::post("/v1/chat/completions")
                .header("x-api-key", "inference-secret")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model":"alias","messages":[{"role":"user","content":"hello"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let id = response.headers()["x-request-id"].to_str().unwrap();
    // Plexus parity: the failover audit is the usage record's retry history
    // (written asynchronously after the response), not a separate table.
    let mut row = None;
    for _ in 0..50 {
        row = sqlx::query(
            "SELECT attempt_count, retry_history FROM request_usage WHERE request_id=$1",
        )
        .bind(id)
        .fetch_optional(&pool)
        .await
        .unwrap();
        if row.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let row = row.expect("usage row for the request");
    assert_eq!(row.get::<Option<i32>, _>("attempt_count"), Some(3));
    let history: serde_json::Value =
        serde_json::from_str(&row.get::<String, _>("retry_history")).unwrap();
    let attempts: Vec<(i64, &str)> = history
        .as_array()
        .unwrap()
        .iter()
        .map(|a| (a["index"].as_i64().unwrap(), a["status"].as_str().unwrap()))
        .collect();
    assert_eq!(attempts, vec![(1, "failed"), (2, "failed"), (3, "success")]);
    server.abort();
}

#[tokio::test]
#[ignore = "requires disposable TREFOIL_TEST_DATABASE_URL"]
async fn responses_route_enforces_request_quota_before_upstream_selection() {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use tower::ServiceExt;
    let pool = database::connect(&std::env::var("TREFOIL_TEST_DATABASE_URL").unwrap())
        .await
        .unwrap();
    database::migrate(&pool).await.unwrap();
    let definition = serde_json::from_value(
        serde_json::json!({"type":"daily","limitType":"requests","limit":0}),
    )
    .unwrap();
    database::upsert_user_quota(&pool, "response-zero", &definition)
        .await
        .unwrap();
    // The enforcer reads the in-memory registry (loaded from the database at
    // startup), so mirror the stored definition into this state.
    let state = trefoil_backend::AppState::new(
        Some("admin".into()),
        [(
            "quota-key".into(),
            trefoil_backend::state::KeyConfig {
                secret: "quota-secret".into(),
                quota_names: vec!["response-zero".into()],
                ..Default::default()
            },
        )]
        .into_iter()
        .collect(),
    )
    .with_database(pool);
    state
        .inner
        .user_quotas
        .write()
        .unwrap()
        .insert("response-zero".into(), definition);
    let response = trefoil_backend::app(state)
        .oneshot(
            Request::post("/v1/responses")
                .header("x-api-key", "quota-secret")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"model":"absent/model","input":"hello"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
#[ignore = "requires disposable TREFOIL_TEST_DATABASE_URL"]
async fn provider_selected_oauth_account_survives_database_reload() {
    let pool = database::connect(&std::env::var("TREFOIL_TEST_DATABASE_URL").unwrap())
        .await
        .unwrap();
    database::migrate(&pool).await.unwrap();
    let mut provider = trefoil_backend::state::ProviderConfig {
        api_base_url: "oauth://openrouter".into(),
        oauth_provider: Some("openrouter".into()),
        oauth_account: Some("work-account".into()),
        enabled: true,
        ..Default::default()
    };
    database::upsert_provider(&pool, "oauth-provider-test", &provider)
        .await
        .unwrap();
    assert_eq!(
        database::load_providers(&pool).await.unwrap()["oauth-provider-test"]
            .oauth_account
            .as_deref(),
        Some("work-account")
    );
    provider.oauth_account = Some("personal-account".into());
    database::upsert_provider(&pool, "oauth-provider-test", &provider)
        .await
        .unwrap();
    assert_eq!(
        database::load_providers(&pool).await.unwrap()["oauth-provider-test"]
            .oauth_account
            .as_deref(),
        Some("personal-account")
    );
}

#[tokio::test]
#[ignore = "requires disposable TREFOIL_TEST_DATABASE_URL and TREFOIL_CREDENTIAL_KEY"]
async fn encrypted_openrouter_key_is_used_by_real_inference_dispatch() {
    let pool = database::connect(&std::env::var("TREFOIL_TEST_DATABASE_URL").unwrap())
        .await
        .unwrap();
    database::migrate(&pool).await.unwrap();
    // Each integration case owns its account; remove deliberately corrupt rows
    // left by the account-binding test before exercising the reader.
    sqlx::query("DELETE FROM oauth_credentials WHERE account_id='moved'")
        .execute(&pool)
        .await
        .unwrap();
    database::upsert_oauth_credential(&pool, "openrouter", "dispatch", "fixture-key", "", i64::MAX)
        .await
        .unwrap();
    // Plexus parity: OAuth routes (`oauth://` base URLs) dispatch to the pi-ai
    // catalog host, never a configured http base URL, so assert the decrypted
    // credential the dispatcher would send instead of contacting OpenRouter.
    let state = trefoil_backend::AppState::new(Some("admin".into()), Default::default())
        .with_database(pool);
    let token = trefoil_backend::engine::dispatcher::oauth::auth::get_api_key(
        &state,
        "openrouter",
        Some("dispatch"),
        false,
    )
    .await
    .unwrap();
    assert_eq!(token, "fixture-key");
}

#[tokio::test]
#[ignore = "requires disposable TREFOIL_TEST_DATABASE_URL"]
async fn checker_snapshots_persist_reported_utilization_and_iso_reset() {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use tower::ServiceExt;
    let pool = database::connect(&std::env::var("TREFOIL_TEST_DATABASE_URL").unwrap())
        .await
        .unwrap();
    database::migrate(&pool).await.unwrap();
    let state = trefoil_backend::AppState::new(Some("admin".into()), Default::default())
        .with_database(pool.clone());
    state
        .inner
        .providers
        .write()
        .unwrap()
        .insert("fixture".into(), Default::default());
    let body = serde_json::json!({"provider":"fixture","code":"return [{key:'meter',kind:'allowance',unit:'percentage',used:25,remaining:75,utilizationPercent:25,status:'ok',resetsAt:'2026-09-18T12:00:00Z'}]"});
    let response = trefoil_backend::app(state)
        .oneshot(
            Request::post("/v0/management/custom-checkers/fixture-checker/check")
                .header("x-admin-key", "admin")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let row=sqlx::query("SELECT utilization_state,utilization_percent,resets_at FROM meter_snapshots WHERE checker_id='fixture-checker' AND meter_key='meter'").fetch_one(&pool).await.unwrap();
    assert_eq!(row.get::<String, _>("utilization_state"), "reported");
    assert_eq!(row.get::<f32, _>("utilization_percent"), 25.0);
    assert_eq!(row.get::<i64, _>("resets_at"), 1789732800000);
}

#[tokio::test]
#[ignore = "requires disposable TREFOIL_TEST_DATABASE_URL"]
async fn empty_checker_success_replaces_stale_quota_result() {
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let pool = database::connect(&std::env::var("TREFOIL_TEST_DATABASE_URL").unwrap())
        .await
        .unwrap();
    database::migrate(&pool).await.unwrap();
    let state = trefoil_backend::AppState::new(Some("admin".into()), Default::default())
        .with_database(pool.clone());
    state.inner.providers.write().unwrap().insert("empty-provider".into(),trefoil_backend::state::ProviderConfig {
        quota_checker:Some(serde_json::from_value(serde_json::json!({"id":"empty-checker","type":"custom-runtime","enabled":true,"intervalMinutes":30})).unwrap()),..Default::default()
    });
    let response = trefoil_backend::app(state.clone())
        .oneshot(
            Request::post("/v0/management/custom-checkers/empty-checker/check")
                .header("x-admin-key", "admin")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"provider":"empty-provider","code":"return []"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(response.status().is_success());
    let count:i64=sqlx::query_scalar("SELECT count(*) FROM meter_snapshots WHERE checker_id='empty-checker' AND meter_key='_empty' AND success=true").fetch_one(&pool).await.unwrap();
    assert_eq!(count, 1);
    let response = trefoil_backend::app(state)
        .oneshot(
            Request::get("/v0/management/quotas/empty-checker")
                .header("x-admin-key", "admin")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body: serde_json::Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body["success"], true);
    assert_eq!(body["checkerType"], "custom-runtime");
    assert_eq!(body["meters"], serde_json::json!([]));
}

#[tokio::test]
#[ignore = "requires disposable TREFOIL_TEST_DATABASE_URL"]
async fn latest_quota_uses_public_meter_contract_not_storage_rows() {
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let pool = database::connect(&std::env::var("TREFOIL_TEST_DATABASE_URL").unwrap())
        .await
        .unwrap();
    database::migrate(&pool).await.unwrap();
    sqlx::query("INSERT INTO meter_snapshots(checker_id,checker_type,provider,meter_key,kind,unit,label,used,remaining,utilization_state,utilization_percent,status,resets_at,checked_at,created_at) VALUES('contract-checker','claude-code','provider','five_hour','allowance','percentage','5-hour quota',25,75,'reported',25,'ok',1789732800000,1789730000000,1789730000000)").execute(&pool).await.unwrap();
    let state = trefoil_backend::AppState::new(Some("admin".into()), Default::default())
        .with_database(pool);
    let response = trefoil_backend::app(state)
        .oneshot(
            Request::get("/v0/management/quotas/contract-checker")
                .header("x-admin-key", "admin")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body: serde_json::Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert!(body["checkedAt"].is_string());
    let meter = &body["meters"][0];
    assert_eq!(meter["key"], "five_hour");
    assert_eq!(meter["utilizationPercent"], 25.0);
    assert_eq!(meter["resetsAt"], "2026-09-18T12:00:00.000Z");
    assert!(
        meter.get("id").is_none()
            && meter.get("meterKey").is_none()
            && meter.get("limit").is_none()
    );
}

#[tokio::test]
#[ignore = "requires disposable TREFOIL_TEST_DATABASE_URL and TREFOIL_CREDENTIAL_KEY"]
async fn encrypted_copilot_token_and_client_headers_reach_inference() {
    use trefoil_backend::engine::{dispatch::request_payload::OAuthPlan, routing::RouteResult};
    let pool = database::connect(&std::env::var("TREFOIL_TEST_DATABASE_URL").unwrap())
        .await
        .unwrap();
    database::migrate(&pool).await.unwrap();
    // Each integration case owns its account; remove deliberately corrupt rows
    // left by the account-binding test before exercising the reader.
    sqlx::query("DELETE FROM oauth_credentials WHERE account_id='moved'")
        .execute(&pool)
        .await
        .unwrap();
    database::upsert_oauth_credential(
        &pool,
        "github-copilot",
        "copilot-dispatch",
        "fixture-key",
        "",
        i64::MAX,
    )
    .await
    .unwrap();
    // Build the native Copilot wire request offline (Plexus parity: the URL is
    // derived from the token / enterprise domain, not a configured base URL).
    let state = trefoil_backend::AppState::new(Some("admin".into()), Default::default())
        .with_database(pool);
    let route = RouteResult {
        provider: "oauth".into(),
        model: "gpt-4o".into(),
        config: serde_json::json!({"api_base_url":"oauth://github-copilot","oauth_provider":"github-copilot","oauth_account":"copilot-dispatch"}),
        model_config: None,
        incoming_model_alias: None,
        canonical_model: None,
    };
    let plan = OAuthPlan {
        native: true,
        provider: "github-copilot".into(),
        masking_route: false,
        codex_passthrough: false,
        claude_passthrough: false,
    };
    let prepared = trefoil_backend::engine::dispatcher::oauth::prepare(
        &state,
        &route,
        &plan,
        serde_json::json!({"model":"gpt-4o","messages":[{"role":"user","content":"hello"}]}),
        &serde_json::json!({}),
        "chat",
    )
    .await
    .unwrap();
    assert_eq!(
        prepared.url,
        "https://api.individual.githubcopilot.com/chat/completions"
    );
    let header = |name: &str| {
        prepared
            .headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
    };
    assert_eq!(
        header("Authorization"),
        Some(serde_json::json!("Bearer fixture-key"))
    );
    assert_eq!(
        header("Copilot-Integration-Id"),
        Some(serde_json::json!("vscode-chat"))
    );
}
