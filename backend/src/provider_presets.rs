//! Ported contract: `GET /v0/management/provider-presets` from Plexus
//! (`packages/backend/src/routes/management/providers.ts` +
//! `services/provider-presets.ts`).
//!
//! Serves the pre-configured provider preset catalog used by the Add
//! Provider flow. Plexus layers three sources (operator-edited disk file >
//! remote catalog > embedded copy); Trefoil serves the embedded copy from
//! `backend/data/provider-presets.json` with `source: "local"`. The disk
//! override and remote refresh are deferred until Trefoil has an operator
//! data-directory convention.

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json, Router,
};

use crate::auth::{resolve_principal, Principal};
use crate::state::AppState;

/// Embedded preset catalog (`{ "presets": [...] }`), mirroring Plexus's
/// built-in `packages/backend/data/provider-presets.json`.
const PRESETS_JSON: &str = include_str!("../data/provider-presets.json");

pub fn router() -> Router<AppState> {
    Router::new().route(
        "/v0/management/provider-presets",
        axum::routing::get(list_presets),
    )
}

async fn list_presets(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let provided_key = headers.get("x-admin-key").and_then(|v| v.to_str().ok());
    match resolve_principal(&state, provided_key) {
        Some(Principal::Admin) => {}
        Some(Principal::Limited { .. }) => {
            return (StatusCode::FORBIDDEN, Json(serde_json::json!({
                "error": "Admin key required"
            })))
            .into_response();
        }
        None => {
            return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({
                "error": "Unauthorized"
            })))
            .into_response();
        }
    }

    let parsed: Result<serde_json::Value, _> = serde_json::from_str(PRESETS_JSON);
    match parsed {
        Ok(value) => {
            let presets = value
                .get("presets")
                .cloned()
                .unwrap_or(value);
            Json(serde_json::json!({ "data": presets, "source": "local" })).into_response()
        }
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": format!("Provider presets failed to load: {error}")
            })),
        )
            .into_response(),
    }
}
