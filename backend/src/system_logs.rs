//! Runtime system log endpoints.

use crate::{
    auth::{resolve_principal, Principal},
    AppState,
};
use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{
        sse::{Event, Sse},
        IntoResponse, Response,
    },
    Json, Router,
};
use futures_util::stream;
use serde::Deserialize;
use serde::Serialize;
use std::{convert::Infallible, time::Duration};
use tokio::sync::broadcast;

/// A structured entry delivered to `/v0/system/logs/stream`.
#[derive(Debug, Clone, Serialize)]
pub struct SystemLog {
    pub level: String,
    pub message: String,
    pub timestamp: String,
    #[serde(flatten)]
    pub context: serde_json::Map<String, serde_json::Value>,
}

/// Publish a system log entry to all currently connected admin clients.
pub fn publish(state: &AppState, log: SystemLog) {
    if let Some(sender) = &state.inner.system_logs {
        let _ = sender.send(log);
    }
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/v0/system/logs/recent", axum::routing::get(recent))
        .route("/v0/system/logs/stream", axum::routing::get(stream_logs))
}

async fn stream_logs(State(state): State<AppState>, headers: HeaderMap) -> Response {
    match resolve_principal(
        &state,
        headers.get("x-admin-key").and_then(|v| v.to_str().ok()),
    ) {
        Some(Principal::Admin) => {}
        Some(Principal::Limited { .. }) => {
            return (StatusCode::FORBIDDEN, "forbidden").into_response()
        }
        None => return (StatusCode::UNAUTHORIZED, "unauthorized").into_response(),
    }

    let Some(sender) = &state.inner.system_logs else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "system log stream unavailable",
        )
            .into_response();
    };
    let receiver = sender.subscribe();
    let stream = stream::unfold(
        (receiver, tokio::time::interval(Duration::from_secs(10))),
        |(mut receiver, mut interval)| async move {
            tokio::select! {
                result = receiver.recv() => {
                    let event = match result {
                        Ok(log) => Event::default().event("syslog").json_data(log)
                            .unwrap_or_else(|_| Event::default().event("syslog").data("{}")),
                        Err(broadcast::error::RecvError::Lagged(_)) => Event::default().event("syslog").data("{\"level\":\"warn\",\"message\":\"system log subscriber lagged\"}"),
                        Err(broadcast::error::RecvError::Closed) => return None,
                    };
                    Some((Ok::<Event, Infallible>(event), (receiver, interval)))
                }
                _ = interval.tick() => {
                    Some((Ok::<Event, Infallible>(Event::default().event("ping").data("pong")), (receiver, interval)))
                }
            }
        },
    );
    let mut response = Sse::new(stream).into_response();
    response
        .headers_mut()
        .insert("cache-control", "no-cache".parse().unwrap());
    response
        .headers_mut()
        .insert("connection", "keep-alive".parse().unwrap());
    response
}
#[derive(Deserialize)]
struct Params {
    limit: Option<usize>,
    offset: Option<usize>,
}
async fn recent(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(p): Query<Params>,
) -> Response {
    match resolve_principal(
        &state,
        headers.get("x-admin-key").and_then(|v| v.to_str().ok()),
    ) {
        Some(Principal::Admin) => {}
        Some(Principal::Limited { .. }) => {
            return (StatusCode::FORBIDDEN, "forbidden").into_response()
        }
        None => return (StatusCode::UNAUTHORIZED, "unauthorized").into_response(),
    }
    let limit = p.limit.unwrap_or(100).clamp(1, 1000);
    let offset = p.offset.unwrap_or(0);
    Json(serde_json::json!({"data":[],"total":0,"limit":limit,"offset":offset})).into_response()
}
