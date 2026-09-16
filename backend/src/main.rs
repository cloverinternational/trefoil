use std::collections::HashMap;

use trefoil_backend::{app, state::KeyConfig, AppState};

#[tokio::main]
async fn main() {
    // Real config loading (files/DB) isn't implemented yet for this vertical
    // slice. ADMIN_KEY mirrors Plexus's own env var. The fixed "test-key"
    // fixture matches BASE_CONFIG in
    // packages/backend/src/routes/management/__tests__/admin-auth.test.ts so
    // parity/differential.ts can exercise the same cases against both
    // servers.
    let admin_key = std::env::var("ADMIN_KEY").ok();
    let mut keys = HashMap::new();
    keys.insert(
        "test-key".to_string(),
        KeyConfig {
            secret: "sk-test-secret".to_string(),
            ..Default::default()
        },
    );
    let state = AppState::new(admin_key, keys);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080")
        .await
        .expect("failed to bind port 8080");
    axum::serve(listener, app(state))
        .await
        .expect("server error");
}
