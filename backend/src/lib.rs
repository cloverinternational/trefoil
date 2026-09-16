pub mod auth;
pub mod aliases;
pub mod keys;
pub mod providers;
pub mod state;

pub use state::AppState;

use axum::{routing::get, Router};

pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .merge(auth::router())
        .merge(aliases::router())
        .merge(keys::router())
        .merge(providers::router())
        .with_state(state)
}

async fn healthz() -> &'static str {
    "ok"
}
