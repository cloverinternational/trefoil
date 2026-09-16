pub mod aliases;
pub mod auth;
pub mod cooldowns;
pub mod database;
pub mod debug;
pub mod inference;
pub mod keys;
pub mod providers;
pub mod self_routes;
pub mod state;
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
        .merge(cooldowns::router())
        .merge(debug::router())
        .merge(inference::router())
        .merge(keys::router())
        .merge(providers::router())
        .merge(self_routes::router())
        .merge(user_quotas::router())
        .merge(system_settings::router())
        .merge(usage::router())
        .with_state(state)
}

async fn healthz() -> &'static str {
    "ok"
}
