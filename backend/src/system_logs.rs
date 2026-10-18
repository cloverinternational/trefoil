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
///
/// Key order follows Plexus's winston entries: `{message, level, timestamp,
/// ...meta}`, or for `logger.error(msg, error)` the error's own properties
/// first, then `{level, message, stack, timestamp, module, functionName}`.
#[derive(Debug, Clone)]
pub struct SystemLog {
    pub message: String,
    pub level: String,
    pub timestamp: String,
    pub context: serde_json::Map<String, serde_json::Value>,
}

impl Serialize for SystemLog {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(None)?;
        if let Some(stack) = self.context.get("stack") {
            const TAIL: [&str; 3] = ["stack", "module", "functionName"];
            for (k, v) in self
                .context
                .iter()
                .filter(|(k, _)| !TAIL.contains(&k.as_str()))
            {
                map.serialize_entry(k, v)?;
            }
            map.serialize_entry("level", &self.level)?;
            map.serialize_entry("message", &self.message)?;
            map.serialize_entry("stack", stack)?;
            map.serialize_entry("timestamp", &self.timestamp)?;
            for key in ["module", "functionName"] {
                if let Some(v) = self.context.get(key) {
                    map.serialize_entry(key, v)?;
                }
            }
        } else {
            map.serialize_entry("message", &self.message)?;
            map.serialize_entry("level", &self.level)?;
            map.serialize_entry("timestamp", &self.timestamp)?;
            for (k, v) in &self.context {
                map.serialize_entry(k, v)?;
            }
        }
        map.end()
    }
}

/// Publish a system log entry to all currently connected admin clients.
pub fn publish(state: &AppState, log: SystemLog) {
    if !crate::logging::enabled(&log.level) {
        return;
    }
    {
        let mut recent = state
            .inner
            .recent_system_logs
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        recent.push_back(log.clone());
        while recent.len() > 1000 {
            recent.pop_front();
        }
    }
    if let Some(sender) = &state.inner.system_logs {
        let _ = sender.send(log);
    }
}

pub fn event(
    state: &AppState,
    level: impl Into<String>,
    message: impl Into<String>,
    context: impl IntoIterator<Item = (String, serde_json::Value)>,
) {
    publish(
        state,
        SystemLog {
            level: level.into(),
            message: message.into(),
            timestamp: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            context: context.into_iter().collect(),
        },
    );
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
        (
            receiver,
            tokio::time::interval_at(
                tokio::time::Instant::now() + Duration::from_secs(10),
                Duration::from_secs(10),
            ),
        ),
        |(mut receiver, mut interval)| async move {
            tokio::select! {
                result = receiver.recv() => {
                    let event = match result {
                        Ok(log) => Event::default().event("syslog").id(chrono::Utc::now().timestamp_millis().to_string()).json_data(log)
                            .unwrap_or_else(|_| Event::default().event("syslog").data("{}")),
                        Err(broadcast::error::RecvError::Lagged(_)) => Event::default().event("syslog").data("{\"level\":\"warn\",\"message\":\"system log subscriber lagged\"}"),
                        Err(broadcast::error::RecvError::Closed) => return None,
                    };
                    Some((Ok::<Event, Infallible>(event), (receiver, interval)))
                }
                _ = interval.tick() => {
                    Some((Ok::<Event, Infallible>(Event::default().event("ping").id(chrono::Utc::now().timestamp_millis().to_string()).data("pong")), (receiver, interval)))
                }
            }
        },
    );
    let mut response = Sse::new(stream).into_response();
    response
        .headers_mut()
        .insert("access-control-allow-origin", "*".parse().unwrap());
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
    let recent = state
        .inner
        .recent_system_logs
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let data: Vec<_> = recent
        .iter()
        .rev()
        .take(limit.saturating_add(offset).min(1000))
        .skip(offset)
        .collect();
    Json(serde_json::json!({"data":data,"total":recent.len(),"limit":limit,"offset":offset}))
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    #[tokio::test]
    async fn recent_logs_are_bounded_newest_first_and_paginated() {
        let state = AppState::new(Some("admin".into()), Default::default());
        for i in 0..1005 {
            event(&state, "error", i.to_string(), []);
        }
        let response = router()
            .with_state(state)
            .oneshot(
                Request::builder()
                    .uri("/v0/system/logs/recent?limit=2&offset=1")
                    .header("x-admin-key", "admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(body["total"], 1000);
        assert_eq!(body["data"][0]["message"], "1003");
        assert_eq!(body["data"][1]["message"], "1002");
    }

    #[tokio::test]
    async fn stream_delivers_log_ids_and_releases_subscription_on_drop() {
        let state = AppState::new(Some("admin".into()), Default::default());
        let response = router()
            .with_state(state.clone())
            .oneshot(
                Request::builder()
                    .uri("/v0/system/logs/stream")
                    .header("x-admin-key", "admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.headers()["access-control-allow-origin"], "*");
        assert_eq!(
            state.inner.system_logs.as_ref().unwrap().receiver_count(),
            1
        );
        event(&state, "error", "live test", []);
        let mut body = response.into_body();
        let frame = tokio::time::timeout(Duration::from_secs(1), body.frame())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let text = std::str::from_utf8(frame.data_ref().unwrap()).unwrap();
        assert!(text.contains("event: syslog"));
        assert!(text.contains("id: "));
        assert!(text.contains("live test"));
        drop(body);
        assert_eq!(
            state.inner.system_logs.as_ref().unwrap().receiver_count(),
            0
        );
    }
}

/// `logger.error('<label>', e)` from a Plexus route `catch` block in `module`.
pub fn route_error(state: &AppState, label: &str, module: &str, error: &serde_json::Value) {
    let message = crate::engine::js::to_string(error.get("message"));
    let mut context: Vec<(String, serde_json::Value)> = Vec::new();
    if let Some(rc) = error.get("routingContext") {
        context.push(("routingContext".into(), rc.clone()));
    }
    let name = crate::engine::js::get_str(error, &["name"]).unwrap_or("Error");
    context.push((
        "stack".into(),
        serde_json::json!(format!("{name}: {message}")),
    ));
    context.push(("module".into(), serde_json::json!(module)));
    event(state, "error", format!("{label} {message}"), context);
}
