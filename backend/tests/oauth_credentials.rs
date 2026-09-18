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
