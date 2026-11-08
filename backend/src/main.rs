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
    let encryption_warning = !trefoil_backend::secret_crypto::enabled();
    database::encrypt_plaintext_secrets(&pool)
        .await
        .unwrap_or_else(|error| {
            eprintln!("failed to encrypt plaintext secrets: {error}");
            std::process::exit(1);
        });
    let keys = database::load_keys(&pool).await.unwrap_or_else(|error| {
        eprintln!("failed to load API keys from PostgreSQL: {error}");
        std::process::exit(1);
    });
    let providers = database::load_providers(&pool)
        .await
        .unwrap_or_else(|error| {
            eprintln!("failed to load providers from PostgreSQL: {error}");
            std::process::exit(1);
        });
    let aliases = database::load_aliases(&pool).await.unwrap_or_else(|error| {
        eprintln!("failed to load model aliases from PostgreSQL: {error}");
        std::process::exit(1);
    });
    let user_quotas = database::load_user_quotas(&pool)
        .await
        .unwrap_or_else(|error| {
            eprintln!("failed to load user quotas from PostgreSQL: {error}");
            std::process::exit(1);
        });
    let settings = database::load_settings(&pool)
        .await
        .unwrap_or_else(|error| {
            eprintln!("failed to load system settings from PostgreSQL: {error}");
            std::process::exit(1);
        });
    let cooldowns = database::load_cooldowns(&pool)
        .await
        .unwrap_or_else(|error| {
            eprintln!("failed to load provider cooldowns from PostgreSQL: {error}");
            std::process::exit(1);
        });
    let state = AppState::new(std::env::var("ADMIN_KEY").ok(), keys).with_database(pool);
    if encryption_warning {
        trefoil_backend::system_logs::event(&state, "warn", "ENCRYPTION_KEY not set — sensitive data will be stored in plaintext. Set ENCRYPTION_KEY for encryption at rest.", [("module".into(), serde_json::json!("src"))]);
    }
    trefoil_backend::oauth_refresh::start_scheduler(state.clone());
    *state.inner.providers.write().unwrap() = providers;
    *state.inner.aliases.write().unwrap() = aliases;
    *state.inner.user_quotas.write().unwrap() = user_quotas;
    *state.inner.settings.write().unwrap() = settings;
    *state.inner.cooldowns.write().unwrap() = cooldowns;
    // Plexus index.ts initModelCatalog(): restore the persisted pi.dev overlay,
    // then refresh in the background unless PLEXUS_MODEL_CATALOG_REFRESH=false.
    trefoil_backend::pi_catalog::init(&state).await;
    {
        use trefoil_backend::engine::observability::estimate;
        match estimate::O200k::load() {
            Some(enc) => estimate::set_tokenizer(Some(std::sync::Arc::new(enc))),
            None => trefoil_backend::system_logs::event(
                &state,
                "warn",
                "o200k_base encoder failed to load; falling back to heuristic",
                [("module".to_owned(), serde_json::json!("estimate-tokens"))],
            ),
        }
    }
    trefoil_backend::custom_checkers::start_poller(state.clone());
    // Startup order mirrors pinned Plexus index.ts: restore capture-on-error,
    // schedule metadata, then the CLI version services.
    let capture = state
        .inner
        .settings
        .read()
        .unwrap()
        .get("debug.captureOnError")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    trefoil_backend::system_logs::event(
        &state,
        "warn",
        format!(
            "Debug capture-on-error {}",
            if capture { "enabled" } else { "disabled" }
        ),
        [
            ("module".into(), serde_json::json!("debug-manager")),
            (
                "functionName".into(),
                serde_json::json!("setCaptureOnError"),
            ),
        ],
    );
    let _metadata_refresh = trefoil_backend::metadata::start_scheduler(state.clone());
    trefoil_backend::cli_versions::start(state.clone());

    let port = std::env::var("PORT")
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(8080);
    // Honour an explicit bind address so the service can be confined to loopback
    // (behind a proxy such as `tailscale serve`) instead of every interface.
    let bind_address = std::env::var("TREFOIL_BIND_ADDRESS")
        .unwrap_or_else(|_| "0.0.0.0".to_string());
    let listener = tokio::net::TcpListener::bind((bind_address.as_str(), port))
        .await
        .expect("failed to bind configured port");
    trefoil_backend::system_logs::event(
        &state,
        "info",
        format!("Server listening on http://localhost:{port}"),
        [("module".into(), serde_json::json!("src"))],
    );
    axum::serve(
        listener,
        app(state).into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await
    .expect("server error");
}
