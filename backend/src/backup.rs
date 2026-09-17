//! Portable database backup export.
//!
//! The full form deliberately uses PostgreSQL's JSON representation for rows:
//! this keeps the archive independent of Rust's database type mappings while
//! preserving every column (including columns added by later migrations).

use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Router,
};
use flate2::{write::GzEncoder, Compression};
use serde::Deserialize;
use sqlx::Row;
use std::io::Write;
use tar::Builder;

use crate::{
    auth::{resolve_principal, Principal},
    AppState,
};

const TABLES: &[&str] = &[
    "request_usage",
    "provider_cooldowns",
    "debug_logs",
    "inference_errors",
    "provider_performance",
    "quota_state",
    "meter_snapshots",
    "quota_snapshots",
    "mcp_request_usage",
    "mcp_debug_logs",
    "responses",
    "conversations",
    "response_items",
];

#[derive(Deserialize, Default)]
struct BackupQuery {
    full: Option<bool>,
}

pub fn router() -> Router<AppState> {
    Router::new().route("/v0/management/backup", axum::routing::get(export))
}

async fn export(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<BackupQuery>,
) -> Response {
    match resolve_principal(
        &state,
        headers.get("x-admin-key").and_then(|v| v.to_str().ok()),
    ) {
        Some(Principal::Admin) => {}
        Some(Principal::Limited { .. }) => {
            return (StatusCode::FORBIDDEN, "Admin privileges required").into_response()
        }
        None => return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response(),
    }
    let Some(pool) = &state.inner.database else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Backup requires a database",
        )
            .into_response();
    };
    let config = serde_json::json!({
        "providers": state.inner.providers.read().unwrap().clone(),
        "models": state.inner.aliases.read().unwrap().clone(),
        "keys": state.inner.keys.read().unwrap().clone(),
        "user_quotas": state.inner.user_quotas.read().unwrap().clone(),
        "mcp_servers": {}, "mcp_keys": [],
        "settings": state.inner.settings.read().unwrap().clone(),
        "oauth_credentials": [],
    });
    if !query.full.unwrap_or(false) {
        return axum::Json(serde_json::json!({
            "plexus_backup": true, "version": 1,
            "created_at": chrono::Utc::now().to_rfc3339(), "dialect": "postgres", "data": config
        }))
        .into_response();
    }
    match build_archive(pool, config).await {
        Ok(bytes) => (
            [
                ("content-type", "application/gzip"),
                ("content-disposition", "attachment; filename=backup.tar.gz"),
            ],
            bytes,
        )
            .into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to create backup: {error}"),
        )
            .into_response(),
    }
}

async fn build_archive(
    pool: &sqlx::PgPool,
    config: serde_json::Value,
) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
    let mut compressed = GzEncoder::new(Vec::new(), Compression::default());
    {
        let mut archive = Builder::new(&mut compressed);
        let manifest = serde_json::json!({"plexus_backup": true, "version": 1, "created_at": chrono::Utc::now().to_rfc3339(), "dialect": "postgres", "tables": TABLES});
        append_bytes(
            &mut archive,
            "manifest.json",
            serde_json::to_vec_pretty(&manifest)?.as_slice(),
        )?;
        append_bytes(
            &mut archive,
            "config.json",
            serde_json::to_vec_pretty(&config)?.as_slice(),
        )?;
        for table in TABLES {
            let rows = sqlx::query(&format!(
                "SELECT row_to_json(t)::text AS row FROM {table} t"
            ))
            .fetch_all(pool)
            .await?;
            let mut objects = Vec::with_capacity(rows.len());
            for row in rows {
                objects.push(serde_json::from_str::<
                    serde_json::Map<String, serde_json::Value>,
                >(&row.try_get::<String, _>("row")?)?);
            }
            let mut csv = String::new();
            if let Some(first) = objects.first() {
                let columns: Vec<String> = first.keys().cloned().collect();
                csv.push_str(
                    &columns
                        .iter()
                        .map(|c| csv_escape(&serde_json::Value::String(c.clone())))
                        .collect::<Vec<_>>()
                        .join(","),
                );
                csv.push('\n');
                for object in objects {
                    csv.push_str(
                        &columns
                            .iter()
                            .map(|c| csv_escape(object.get(c).unwrap_or(&serde_json::Value::Null)))
                            .collect::<Vec<_>>()
                            .join(","),
                    );
                    csv.push('\n');
                }
            }
            let mut csv_compressed = GzEncoder::new(Vec::new(), Compression::default());
            csv_compressed.write_all(csv.as_bytes())?;
            let csv_bytes = csv_compressed.finish()?;
            append_bytes(&mut archive, &format!("{table}.csv.gz"), &csv_bytes)?;
        }
        archive.finish()?;
    }
    Ok(compressed.finish()?)
}

fn append_bytes<W: Write>(
    archive: &mut Builder<W>,
    name: &str,
    data: &[u8],
) -> Result<(), std::io::Error> {
    let mut header = tar::Header::new_gnu();
    header.set_size(data.len() as u64);
    header.set_mode(0o600);
    header.set_cksum();
    archive.append_data(&mut header, name, data)
}

fn csv_escape(value: &serde_json::Value) -> String {
    let text = match value {
        serde_json::Value::Null => "\\N".to_string(),
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    if text == "\\N" {
        return "\\\\N".into();
    }
    if text.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", text.replace('"', "\"\""))
    } else {
        text
    }
}
