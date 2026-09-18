//! Portable database backup export.
//!
//! The full form deliberately uses PostgreSQL's JSON representation for rows:
//! this keeps the archive independent of Rust's database type mappings while
//! preserving every column (including columns added by later migrations).

use axum::{
    body::Bytes,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Router,
};
use flate2::{write::GzEncoder, Compression};
use serde::Deserialize;
use sqlx::Row;
use std::collections::HashMap;
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
    Router::new()
        .route("/v0/management/backup", axum::routing::get(export))
        .route("/v0/management/restore", axum::routing::post(restore))
}

const MAX_BACKUP_BYTES: usize = 256 * 1024 * 1024;
const MAX_ENTRY_BYTES: u64 = 128 * 1024 * 1024;
const MAX_UNCOMPRESSED_BACKUP_BYTES: u64 = 512 * 1024 * 1024;

async fn restore(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    match resolve_principal(
        &state,
        headers.get("x-admin-key").and_then(|v| v.to_str().ok()),
    ) {
        Some(Principal::Admin) => {}
        Some(Principal::Limited { .. }) => {
            return restore_error(StatusCode::FORBIDDEN, "Admin privileges required")
        }
        None => return restore_error(StatusCode::UNAUTHORIZED, "Unauthorized"),
    }
    if body.len() > MAX_BACKUP_BYTES {
        return restore_error(StatusCode::PAYLOAD_TOO_LARGE, "Backup is too large");
    }
    let Some(pool) = &state.inner.database else {
        return restore_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Restore requires a database",
        );
    };
    let result = if headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .contains("json")
    {
        let value: serde_json::Value = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(_) => return restore_error(StatusCode::BAD_REQUEST, "Invalid backup JSON"),
        };
        restore_config(pool, &state, &value)
            .await
            .map(|_| serde_json::json!({"success":true,"config":true}))
    } else {
        restore_archive(pool, &state, &body)
            .await
            .map(|n| serde_json::json!({"success":true,"restored":n}))
    };
    match result {
        Ok(value) => axum::Json(value).into_response(),
        Err(error) => restore_error(StatusCode::BAD_REQUEST, &format!("Invalid backup: {error}")),
    }
}

fn restore_error(status: StatusCode, message: &str) -> Response {
    (status, axum::Json(serde_json::json!({"error": message}))).into_response()
}

async fn restore_archive(
    pool: &sqlx::PgPool,
    state: &AppState,
    input: &[u8],
) -> Result<HashMap<String, u64>, String> {
    use flate2::read::GzDecoder;
    let mut archive = tar::Archive::new(GzDecoder::new(input));
    let mut files = HashMap::new();
    let mut total_size = 0u64;
    for item in archive.entries().map_err(|e| e.to_string())? {
        let mut entry = item.map_err(|e| e.to_string())?;
        let path = entry.path().map_err(|e| e.to_string())?.to_path_buf();
        let name = path.to_str().ok_or("invalid archive path")?.to_string();
        if !valid_archive_entry(
            &path,
            entry.header().entry_type(),
            entry.size(),
            total_size,
            files.contains_key(&name),
        ) {
            return Err("unsafe, unknown, duplicate, or oversized archive entry".into());
        }
        let mut data = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut data).map_err(|e| e.to_string())?;
        total_size += data.len() as u64;
        files.insert(name, data);
    }
    let manifest: serde_json::Value = serde_json::from_slice(
        files
            .get("manifest.json")
            .ok_or("manifest.json is required")?,
    )
    .map_err(|_| "invalid manifest")?;
    if manifest.get("plexus_backup") != Some(&serde_json::Value::Bool(true))
        || manifest.get("version").and_then(|v| v.as_i64()) != Some(1)
    {
        return Err("unsupported backup version".into());
    }
    let config: serde_json::Value =
        serde_json::from_slice(files.get("config.json").ok_or("config.json is required")?)
            .map_err(|_| "invalid config")?;
    let mut parsed = HashMap::new();
    for table in TABLES {
        if let Some(data) = files.get(&format!("{table}.csv.gz")) {
            parsed.insert(*table, parse_csv(data)?);
        }
    }
    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
    for table in TABLES.iter().rev() {
        sqlx::query(&format!("DELETE FROM {table}"))
            .execute(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
    }
    let mut counts = HashMap::new();
    for table in TABLES {
        let rows = parsed.remove(table).unwrap_or_default();
        for row in &rows {
            sqlx::query(&format!(
                "INSERT INTO {table} SELECT * FROM jsonb_populate_record(NULL::{table}, $1::jsonb)"
            ))
            .bind(serde_json::to_string(row).unwrap())
            .execute(&mut *tx)
            .await
            .map_err(|e| format!("{table}: {e}"))?;
        }
        counts.insert((*table).to_string(), rows.len() as u64);
    }
    tx.commit().await.map_err(|e| e.to_string())?;
    restore_config(pool, state, &config).await?;
    Ok(counts)
}

fn valid_archive_entry(
    path: &std::path::Path,
    entry_type: tar::EntryType,
    size: u64,
    total_size: u64,
    duplicate: bool,
) -> bool {
    let Some(name) = path.to_str() else {
        return false;
    };
    entry_type.is_file()
        && path.components().count() == 1
        && (name == "manifest.json"
            || name == "config.json"
            || TABLES.iter().any(|table| name == format!("{table}.csv.gz")))
        && size <= MAX_ENTRY_BYTES
        && total_size.saturating_add(size) <= MAX_UNCOMPRESSED_BACKUP_BYTES
        && !duplicate
}

fn parse_csv(data: &[u8]) -> Result<Vec<serde_json::Map<String, serde_json::Value>>, String> {
    use flate2::read::GzDecoder;
    use std::io::Read;
    let mut text = String::new();
    GzDecoder::new(data)
        .take(MAX_ENTRY_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(|_| "invalid compressed CSV")?;
    if text.len() as u64 > MAX_ENTRY_BYTES {
        return Err("compressed CSV expands beyond entry limit".into());
    }
    let mut reader = csv::Reader::from_reader(text.as_bytes());
    let headers = reader.headers().map_err(|_| "invalid CSV headers")?.clone();
    let mut rows = Vec::new();
    for record in reader.records() {
        let record = record.map_err(|_| "invalid CSV row")?;
        if record.len() != headers.len() {
            return Err("CSV column mismatch".into());
        }
        let mut row = serde_json::Map::new();
        for (key, value) in headers.iter().zip(record.iter()) {
            row.insert(
                key.into(),
                if value == "\\N" {
                    serde_json::Value::Null
                } else {
                    serde_json::from_str(value)
                        .unwrap_or_else(|_| serde_json::Value::String(value.into()))
                },
            );
        }
        rows.push(row);
    }
    Ok(rows)
}

async fn restore_config(
    pool: &sqlx::PgPool,
    state: &AppState,
    envelope: &serde_json::Value,
) -> Result<(), String> {
    let data = envelope.get("data").ok_or("missing data")?;
    let providers: HashMap<String, crate::state::ProviderConfig> =
        serde_json::from_value(data.get("providers").cloned().ok_or("missing providers")?)
            .map_err(|_| "invalid providers")?;
    let aliases: HashMap<String, crate::state::ModelConfig> =
        serde_json::from_value(data.get("models").cloned().ok_or("missing models")?)
            .map_err(|_| "invalid models")?;
    let keys: HashMap<String, crate::state::KeyConfig> =
        serde_json::from_value(data.get("keys").cloned().ok_or("missing keys")?)
            .map_err(|_| "invalid keys")?;

    // A restore is a replacement, not an upsert.  Clear records that are not
    // present in the backup so removed providers, models, and keys do not
    // survive a full restore.
    crate::database::delete_all_aliases(pool)
        .await
        .map_err(|e| e.to_string())?;
    crate::database::delete_all_providers(pool)
        .await
        .map_err(|e| e.to_string())?;
    crate::database::delete_all_keys(pool)
        .await
        .map_err(|e| e.to_string())?;
    for (name, config) in &keys {
        crate::database::upsert_key(pool, name, config)
            .await
            .map_err(|e| e.to_string())?;
    }
    for (name, config) in &providers {
        crate::database::upsert_provider(pool, name, config)
            .await
            .map_err(|e| e.to_string())?;
    }
    for (name, config) in &aliases {
        crate::database::upsert_alias(pool, name, config)
            .await
            .map_err(|e| e.to_string())?;
    }
    *state.inner.keys.write().unwrap() = keys;
    *state.inner.providers.write().unwrap() = providers;
    *state.inner.aliases.write().unwrap() = aliases;
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::{valid_archive_entry, StatusCode};
    use std::path::Path;

    #[test]
    fn rejects_traversal_links_duplicates_and_oversized_entries() {
        assert!(valid_archive_entry(
            Path::new("config.json"),
            tar::EntryType::Regular,
            10,
            0,
            false
        ));
        for path in ["../config.json", "/config.json", "nested/config.json"] {
            assert!(!valid_archive_entry(
                Path::new(path),
                tar::EntryType::Regular,
                10,
                0,
                false
            ));
        }
        assert!(!valid_archive_entry(
            Path::new("config.json"),
            tar::EntryType::Symlink,
            10,
            0,
            false
        ));
        assert!(!valid_archive_entry(
            Path::new("config.json"),
            tar::EntryType::Regular,
            10,
            0,
            true
        ));
        assert!(!valid_archive_entry(
            Path::new("config.json"),
            tar::EntryType::Regular,
            super::MAX_ENTRY_BYTES + 1,
            0,
            false
        ));
        assert!(!valid_archive_entry(
            Path::new("config.json"),
            tar::EntryType::Regular,
            1,
            super::MAX_UNCOMPRESSED_BACKUP_BYTES,
            false
        ));
    }

    #[test]
    fn restore_errors_use_reference_json_envelope() {
        let response = super::restore_error(StatusCode::BAD_REQUEST, "bad backup");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(response.headers()["content-type"], "application/json");
    }

    #[tokio::test]
    async fn restore_error_envelope_contains_message() {
        let response = super::restore_error(StatusCode::UNAUTHORIZED, "Unauthorized");
        let body = response.into_body();
        let bytes = http_body_util::BodyExt::collect(body)
            .await
            .expect("error response body");
        let value: serde_json::Value = serde_json::from_slice(&bytes.to_bytes()).unwrap();
        assert_eq!(value, serde_json::json!({"error": "Unauthorized"}));
    }
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
