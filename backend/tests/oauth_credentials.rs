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
