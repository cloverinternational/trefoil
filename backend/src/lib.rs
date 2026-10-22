pub mod alias_view;
pub mod aliases;
pub mod analytics;
pub mod anthropic_masking;
pub mod anthropic_oauth;
pub mod auth;
pub mod backup;
pub mod catalog_organization;
pub mod cli_versions;
pub mod codex_masking;
pub mod codex_oauth;
pub mod concurrency;
pub mod config_routes;
pub mod cooldown_manager;
pub mod cooldowns;
pub mod copilot_oauth;
pub mod credential_crypto;
pub mod custom_checkers;
pub mod database;
pub mod debug;
pub mod debug_logs;
pub mod decisions;
pub mod engine;
pub mod errors;
pub mod http_compat;
pub mod keys;
pub mod kimi_oauth;
pub mod logging;
pub mod mcp;
pub mod mcp_local;
pub mod mcp_logs;
pub mod mcp_oauth;
pub mod mcp_plexus;
pub mod mcp_proxy;
pub mod mcp_stdio;
pub mod media_routes;
pub mod meta_oauth;
pub mod metadata;
pub mod metrics;
pub mod models_routes;
pub mod oauth;
pub mod oauth_login;
pub mod oauth_masking;
pub mod oauth_refresh;
pub mod oauth_server;
pub mod oauth_transport;
pub mod openai_chatgpt_oauth;
pub mod openapi;
pub mod openrouter_oauth;
pub mod performance;
pub mod pi_catalog;
pub mod pi_resolve;
pub mod provider_presets;
pub mod provider_schema;
pub mod provider_view;
pub mod providers;
pub mod quota_management;
pub mod raw;
pub mod restart;
pub mod secret_crypto;
pub mod self_routes;
pub mod short_name;
pub mod state;
pub mod system_logs;
pub mod system_settings;
pub mod usage;
pub mod user_quotas;
pub mod vision;
pub mod xai_oauth;
pub mod zod;

pub use state::AppState;

use axum::{routing::get, Router};

pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .merge(auth::router())
        .merge(aliases::router())
        .merge(analytics::router())
        .merge(backup::router())
        .merge(config_routes::router())
        .merge(catalog_organization::router())
        .merge(concurrency::router())
        .merge(custom_checkers::router())
        .merge(cooldowns::router())
        .merge(debug::router())
        .merge(debug_logs::router())
        .merge(errors::router())
        .merge(decisions::router())
        .merge(engine::dispatcher::routes::router())
        .merge(media_routes::router())
        .merge(models_routes::router())
        .merge(logging::router())
        .merge(mcp::router())
        .merge(mcp_proxy::router())
        .merge(mcp_plexus::router())
        .merge(mcp_logs::router())
        .merge(metrics::router())
        .merge(mcp_oauth::router())
        .merge(oauth::router())
        .merge(oauth_server::router())
        .merge(openapi::router())
        .merge(performance::router())
        .merge(pi_catalog::router())
        .merge(pi_resolve::router())
        .merge(keys::router())
        .merge(provider_presets::router())
        .merge(providers::router())
        .merge(restart::router())
        .merge(raw::router())
        .merge(quota_management::router())
        .merge(self_routes::router())
        .merge(user_quotas::router())
        .merge(system_settings::router())
        .merge(system_logs::router())
        .merge(usage::router())
        .route("/health", get(http_compat::health))
        .route("/", get(http_compat::root_redirect))
        .route("/ui/", get(http_compat::ui_index))
        .fallback(http_compat::fallback)
        .layer(axum::middleware::from_fn(
            http_compat::method_not_allowed_as_404,
        ))
        .layer(axum::middleware::from_fn(http_compat::body_parser))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            log_request,
        ))
        .with_state(state)
}

/// Pinned Plexus `middleware/log.ts`: every request is logged at debug
/// (module `log`) before routing and authentication.
async fn log_request(
    axum::extract::State(state): axum::extract::State<AppState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let path = request
        .uri()
        .path_and_query()
        .map(|value| value.as_str())
        .unwrap_or("/");
    system_logs::event(
        &state,
        "debug",
        format!("{} {path}", request.method()),
        [("module".into(), serde_json::json!("log"))],
    );
    next.run(request).await
}

async fn healthz() -> &'static str {
    "ok"
}
