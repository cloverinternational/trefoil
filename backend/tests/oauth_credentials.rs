//! Explicit disposable-Postgres integration test; never uses DATABASE_URL.
use trefoil_backend::database;
use sqlx::Row;

#[tokio::test]
#[ignore = "requires disposable TREFOIL_TEST_DATABASE_URL and TREFOIL_CREDENTIAL_KEY"]
async fn oauth_tokens_are_encrypted_and_round_trip_with_timestamps() {
    let url = std::env::var("TREFOIL_TEST_DATABASE_URL").expect("disposable test URL required");
    let pool = database::connect(&url).await.unwrap();
    database::migrate(&pool).await.unwrap();
    database::upsert_oauth_credential(&pool, "openrouter", "integration", "test-secret", "", i64::MAX).await.unwrap();
    let row = sqlx::query("SELECT access_token,refresh_token,created_at,updated_at FROM oauth_credentials WHERE account_id='integration'").fetch_one(&pool).await.unwrap();
    let access: String = row.get("access_token");
    let refresh: String = row.get("refresh_token");
    assert!(access.starts_with("v1:") && !access.contains("test-secret"));
    assert!(refresh.starts_with("v1:") && !refresh.is_empty());
    assert!(row.get::<i64,_>("created_at") > 0);
    assert!(row.get::<i64,_>("updated_at") > 0);
    let loaded = database::load_oauth_credentials(&pool).await.unwrap();
    assert!(loaded.iter().any(|(p,a,t,r,e)|p=="openrouter" && a=="integration" && t=="test-secret" && r.is_empty() && *e==i64::MAX));
    sqlx::query("UPDATE oauth_credentials SET account_id='moved' WHERE account_id='integration'").execute(&pool).await.unwrap();
    assert!(database::load_oauth_credentials(&pool).await.is_err());
}

#[tokio::test]
#[ignore = "requires disposable TREFOIL_TEST_DATABASE_URL and TREFOIL_CREDENTIAL_KEY"]
async fn cancelled_exchange_cannot_write_credentials_and_completion_is_once_only() {
    let pool = database::connect(&std::env::var("TREFOIL_TEST_DATABASE_URL").unwrap()).await.unwrap();
    database::migrate(&pool).await.unwrap();
    let now = chrono::Utc::now().timestamp_millis();
    for (id, status) in [("cancel-test", "cancelled"), ("complete-test", "exchanging")] {
        sqlx::query("INSERT INTO oauth_login_sessions(id,provider_id,account_id,status,created_at,updated_at,expires_at) VALUES($1,'openrouter',$1,$2,$3,$3,$4)").bind(id).bind(status).bind(now).bind(now+60000).execute(&pool).await.unwrap();
    }
    assert!(!database::complete_oauth_exchange(&pool,"cancel-test","openrouter","cancel-test","secret","",i64::MAX).await.unwrap());
    assert!(database::complete_oauth_exchange(&pool,"complete-test","openrouter","complete-test","secret","",i64::MAX).await.unwrap());
    assert!(!database::complete_oauth_exchange(&pool,"complete-test","openrouter","complete-test","replacement","",i64::MAX).await.unwrap());
    let count:i64 = sqlx::query_scalar("SELECT count(*) FROM oauth_credentials WHERE account_id='cancel-test'").fetch_one(&pool).await.unwrap();
    assert_eq!(count,0);
}

#[tokio::test]
#[ignore = "requires disposable TREFOIL_TEST_DATABASE_URL and TREFOIL_CREDENTIAL_KEY"]
async fn concurrent_claim_has_exactly_one_winner() {
    let pool = database::connect(&std::env::var("TREFOIL_TEST_DATABASE_URL").unwrap()).await.unwrap();
    database::migrate(&pool).await.unwrap();
    let now = chrono::Utc::now().timestamp_millis();
    sqlx::query("INSERT INTO oauth_login_sessions(id,provider_id,account_id,status,created_at,updated_at,expires_at,pkce_verifier) VALUES('claim-race','openrouter','claim-race','waiting',$1,$1,$2,'verifier')").bind(now).bind(now+60000).execute(&pool).await.unwrap();
    let (first, second) = tokio::join!(database::claim_oauth_exchange(&pool,"claim-race"), database::claim_oauth_exchange(&pool,"claim-race"));
    assert_ne!(first.unwrap(), second.unwrap());
    let verifier:Option<String> = sqlx::query_scalar("SELECT pkce_verifier FROM oauth_login_sessions WHERE id='claim-race'").fetch_one(&pool).await.unwrap();
    assert!(verifier.is_none());
}

#[tokio::test]
#[ignore = "requires disposable TREFOIL_TEST_DATABASE_URL"]
async fn chat_failover_audit_has_contiguous_candidate_numbers() {
    use axum::{body::Body, http::Request, routing::post, Json, Router};
    use tower::ServiceExt;
    use trefoil_backend::{AppState, state::{ProviderConfig, ModelConfig, ModelTargetGroup, ModelTarget}};
    let pool = database::connect(&std::env::var("TREFOIL_TEST_DATABASE_URL").unwrap()).await.unwrap();
    database::migrate(&pool).await.unwrap();
    let app = Router::new().route("/chat/completions",post(|Json(body):Json<serde_json::Value>| async move {
        if body["model"]=="third" { (axum::http::StatusCode::OK,Json(serde_json::json!({"choices":[],"usage":{}}))) }
        else { (axum::http::StatusCode::SERVICE_UNAVAILABLE,Json(serde_json::json!({"error":{"message":"retry"}}))) }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr=listener.local_addr().unwrap();
    let server=tokio::spawn(async move {axum::serve(listener,app).await.unwrap()});
    let state=AppState::new(Some("admin".into()),Default::default()).with_database(pool.clone());
    for name in ["first","second","third"] { state.inner.providers.write().unwrap().insert(name.into(),ProviderConfig{api_base_url:format!("http://{addr}"),enabled:true,..Default::default()}); }
    state.inner.aliases.write().unwrap().insert("alias".into(),ModelConfig{target_groups:vec![ModelTargetGroup{name:"ordered".into(),selector:"in_order".into(),targets:["first","second","third"].into_iter().map(|name|ModelTarget{provider:Some(name.into()),model:Some(name.into()),enabled:true,..Default::default()}).collect()}],..Default::default()});
    let response=trefoil_backend::app(state).oneshot(Request::post("/v1/chat/completions").header("x-api-key","admin").header("content-type","application/json").body(Body::from(r#"{"model":"alias","messages":[{"role":"user","content":"hello"}]}"#)).unwrap()).await.unwrap();
    assert_eq!(response.status(),axum::http::StatusCode::OK);
    let id=response.headers()["x-request-id"].to_str().unwrap();
    let attempts:Vec<i32>=sqlx::query_scalar("SELECT attempt_index FROM inference_attempts WHERE request_id=$1 ORDER BY attempt_index").bind(id).fetch_all(&pool).await.unwrap();
    assert_eq!(attempts,vec![1,2,3]);
    server.abort();
}

#[tokio::test]
#[ignore = "requires disposable TREFOIL_TEST_DATABASE_URL"]
async fn responses_route_enforces_request_quota_before_upstream_selection() {
    use axum::{body::Body,http::{Request,StatusCode}};
    use tower::ServiceExt;
    let pool=database::connect(&std::env::var("TREFOIL_TEST_DATABASE_URL").unwrap()).await.unwrap();
    database::migrate(&pool).await.unwrap();
    let definition=serde_json::from_value(serde_json::json!({"type":"daily","limitType":"requests","limit":0})).unwrap();
    database::upsert_user_quota(&pool,"response-zero",&definition).await.unwrap();
    let state=trefoil_backend::AppState::new(Some("admin".into()),[("quota-key".into(),trefoil_backend::state::KeyConfig{secret:"quota-secret".into(),quota_names:vec!["response-zero".into()],..Default::default()})].into_iter().collect()).with_database(pool);
    let response=trefoil_backend::app(state).oneshot(Request::post("/v1/responses").header("x-api-key","quota-secret").header("content-type","application/json").body(Body::from(r#"{"model":"absent/model","input":"hello"}"#)).unwrap()).await.unwrap();
    assert_eq!(response.status(),StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
#[ignore = "requires disposable TREFOIL_TEST_DATABASE_URL"]
async fn provider_selected_oauth_account_survives_database_reload() {
    let pool=database::connect(&std::env::var("TREFOIL_TEST_DATABASE_URL").unwrap()).await.unwrap();
    database::migrate(&pool).await.unwrap();
    let mut provider=trefoil_backend::state::ProviderConfig {api_base_url:"oauth://openrouter".into(),oauth_provider:Some("openrouter".into()),oauth_account:Some("work-account".into()),enabled:true,..Default::default()};
    database::upsert_provider(&pool,"oauth-provider-test",&provider).await.unwrap();
    assert_eq!(database::load_providers(&pool).await.unwrap()["oauth-provider-test"].oauth_account.as_deref(),Some("work-account"));
    provider.oauth_account=Some("personal-account".into());
    database::upsert_provider(&pool,"oauth-provider-test",&provider).await.unwrap();
    assert_eq!(database::load_providers(&pool).await.unwrap()["oauth-provider-test"].oauth_account.as_deref(),Some("personal-account"));
}
