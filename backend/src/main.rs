use trefoil_backend::{app, database, AppState};

#[tokio::main]
async fn main() {
    let pool = database::connect_from_env().await.unwrap_or_else(|error| {
        eprintln!("{error}");
        std::process::exit(1);
    });
    database::migrate(&pool).await.unwrap_or_else(|error| {
        eprintln!("failed to run PostgreSQL migrations: {error}");
        std::process::exit(1);
    });
    let keys = database::load_keys(&pool).await.unwrap_or_else(|error| {
        eprintln!("failed to load API keys from PostgreSQL: {error}");
        std::process::exit(1);
    });
    let providers = database::load_providers(&pool).await.unwrap_or_else(|error| {
        eprintln!("failed to load providers from PostgreSQL: {error}");
        std::process::exit(1);
    });
    let aliases = database::load_aliases(&pool).await.unwrap_or_else(|error| {
        eprintln!("failed to load model aliases from PostgreSQL: {error}");
        std::process::exit(1);
    });
    let user_quotas = database::load_user_quotas(&pool).await.unwrap_or_else(|error| {
        eprintln!("failed to load user quotas from PostgreSQL: {error}");
        std::process::exit(1);
    });
    let settings = database::load_settings(&pool).await.unwrap_or_else(|error| {
        eprintln!("failed to load system settings from PostgreSQL: {error}");
        std::process::exit(1);
    });
    let cooldowns = database::load_cooldowns(&pool).await.unwrap_or_else(|error| {
        eprintln!("failed to load provider cooldowns from PostgreSQL: {error}");
        std::process::exit(1);
    });
    let state = AppState::new(std::env::var("ADMIN_KEY").ok(), keys)
        .with_database(pool);
    *state.inner.providers.write().unwrap() = providers;
    *state.inner.aliases.write().unwrap() = aliases;
    *state.inner.user_quotas.write().unwrap() = user_quotas;
    *state.inner.settings.write().unwrap() = settings;
    *state.inner.cooldowns.write().unwrap() = cooldowns;

    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080")
        .await
        .expect("failed to bind port 8080");
    axum::serve(listener, app(state))
        .await
        .expect("server error");
}
