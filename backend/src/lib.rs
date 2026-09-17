pub mod aliases;
pub mod auth;
pub mod backup;
pub mod concurrency;
pub mod config_routes;
pub mod cooldowns;
pub mod database;
pub mod debug;
pub mod debug_logs;
pub mod errors;
pub mod inference;
pub mod keys;
pub mod logging;
pub mod mcp;
pub mod mcp_logs;
pub mod mcp_oauth;
pub mod mcp_proxy;
pub mod oauth;
pub mod oauth_server;
pub mod openapi;
pub mod performance;
pub mod providers;
pub mod quota_management;
pub mod raw;
pub mod restart;
pub mod self_routes;
pub mod state;
pub mod system_logs;
pub mod system_settings;
pub mod usage;
pub mod user_quotas;

pub use state::AppState;

use axum::{routing::get, Router};

pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .merge(auth::router())
        .merge(aliases::router())
        .merge(backup::router())
        .merge(config_routes::router())
        .merge(concurrency::router())
        .merge(cooldowns::router())
        .merge(debug::router())
        .merge(debug_logs::router())
        .merge(errors::router())
        .merge(inference::router())
        .merge(logging::router())
        .merge(mcp::router())
        .merge(mcp_proxy::router())
        .merge(mcp_logs::router())
        .merge(mcp_oauth::router())
        .merge(oauth::router())
        .merge(oauth_server::router())
        .merge(openapi::router())
        .merge(performance::router())
        .merge(keys::router())
        .merge(providers::router())
        .merge(restart::router())
        .merge(raw::router())
        .merge(quota_management::router())
        .merge(self_routes::router())
        .merge(user_quotas::router())
        .merge(system_settings::router())
        .merge(system_logs::router())
        .merge(usage::router())
        .with_state(state)
}

async fn healthz() -> &'static str {
    "ok"
}
