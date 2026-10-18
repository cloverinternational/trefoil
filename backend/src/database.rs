//! PostgreSQL connection and migration bootstrap.
//!
//! The table layout is intentionally based on Plexus's PostgreSQL schema.
//! Route repositories will be added incrementally; this module establishes a
//! single production connection contract without silently falling back to the
//! old in-memory state.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;
use sqlx::{postgres::PgPoolOptions, PgPool, Row};

use crate::state::{
    AdapterEntry, CooldownEntry, KeyConfig, ModelConfig, ModelTargetGroup, ProviderConfig,
    QuotaCheckerConfig, QuotaDefinition, RawPassthroughConfig,
};

pub const DEFAULT_MAX_CONNECTIONS: u32 = 16;

/// Credential lifecycle timestamps `(created_at, updated_at, expires_at)`
/// without reading any token (Plexus `getOAuthCredentialTimestamps`).
pub async fn oauth_credential_timestamps(
    pool: &PgPool,
    provider: &str,
    account: &str,
) -> Result<Option<(i64, i64, i64)>, sqlx::Error> {
    let row = sqlx::query("SELECT created_at, updated_at, expires_at FROM oauth_credentials WHERE oauth_provider_type = $1 AND account_id = $2 LIMIT 1")
        .bind(provider).bind(account).fetch_optional(pool).await?;
    row.map(|row| Ok((row.try_get("created_at")?, row.try_get("updated_at")?, row.try_get("expires_at")?))).transpose()
}

/// Delete one persisted OAuth credential; true when a row was removed.
pub async fn delete_oauth_credential(pool: &PgPool, provider: &str, account: &str) -> Result<bool, sqlx::Error> {
    sqlx::query("DELETE FROM oauth_credentials WHERE oauth_provider_type = $1 AND account_id = $2")
        .bind(provider).bind(account).execute(pool).await.map(|result| result.rows_affected() > 0)
}

/// `(provider, account, expires_at)` for every persisted credential, without
/// decrypting tokens.
pub async fn list_oauth_credential_accounts(pool: &PgPool) -> Result<Vec<(String, String, i64)>, sqlx::Error> {
    let rows = sqlx::query("SELECT oauth_provider_type, account_id, expires_at FROM oauth_credentials").fetch_all(pool).await?;
    rows.into_iter().map(|row| Ok((row.try_get("oauth_provider_type")?, row.try_get("account_id")?, row.try_get("expires_at")?))).collect()
}

pub async fn upsert_oauth_credential(
    pool: &PgPool,
    provider: &str,
    account: &str,
    access_token: &str,
    refresh_token: &str,
    expires_at: i64,
) -> Result<(), sqlx::Error> {
    let key = crate::credential_crypto::CredentialKey::from_env()
        .map_err(|e| sqlx::Error::Protocol(e.to_string()))?;
    let access_token =
        crate::credential_crypto::encrypt(&key, provider, account, "access_token", access_token)
            .map_err(|e| sqlx::Error::Protocol(e.to_string()))?;
    let refresh_token =
        crate::credential_crypto::encrypt(&key, provider, account, "refresh_token", refresh_token)
            .map_err(|e| sqlx::Error::Protocol(e.to_string()))?;
    let now = chrono::Utc::now().timestamp_millis();
    sqlx::query("INSERT INTO oauth_credentials (oauth_provider_type,account_id,access_token,refresh_token,expires_at,created_at,updated_at) VALUES ($1,$2,$3,$4,$5,$6,$6) ON CONFLICT (oauth_provider_type,account_id) DO UPDATE SET access_token=EXCLUDED.access_token,refresh_token=EXCLUDED.refresh_token,expires_at=EXCLUDED.expires_at,updated_at=EXCLUDED.updated_at")
        .bind(provider).bind(account).bind(access_token).bind(refresh_token)
        .bind(expires_at).bind(now).execute(pool).await.map(|_| ())
}

pub async fn load_oauth_credentials(
    pool: &PgPool,
) -> Result<Vec<(String, String, String, String, i64)>, sqlx::Error> {
    let key = crate::credential_crypto::CredentialKey::from_env()
        .map_err(|e| sqlx::Error::Protocol(e.to_string()))?;
    let rows = sqlx::query(
        "SELECT oauth_provider_type, account_id, access_token, refresh_token, expires_at FROM oauth_credentials",
    )
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|row| {
            let provider: String = row.try_get("oauth_provider_type")?;
            let account: String = row.try_get("account_id")?;
            let access: String = row.try_get("access_token")?;
            let refresh: String = row.try_get("refresh_token")?;
            let access = crate::credential_crypto::decrypt(
                &key,
                &provider,
                &account,
                "access_token",
                &access,
            )
            .map_err(|e| sqlx::Error::Protocol(e.to_string()))?;
            let refresh = crate::credential_crypto::decrypt(
                &key,
                &provider,
                &account,
                "refresh_token",
                &refresh,
            )
            .map_err(|e| sqlx::Error::Protocol(e.to_string()))?;
            Ok((
                provider,
                account,
                access,
                refresh,
                row.try_get("expires_at")?,
            ))
        })
        .collect()
}

pub async fn connect(database_url: &str) -> Result<PgPool, sqlx::Error> {
    PgPoolOptions::new()
        .max_connections(
            std::env::var("DATABASE_MAX_CONNECTIONS")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(DEFAULT_MAX_CONNECTIONS),
        )
        .connect(database_url)
        .await
}

pub async fn connect_from_env() -> Result<PgPool, String> {
    let url = std::env::var("DATABASE_URL")
        .map_err(|_| "DATABASE_URL is required for PostgreSQL-backed startup".to_string())?;
    connect(&url)
        .await
        .map_err(|error| format!("failed to connect to PostgreSQL: {error}"))
}

pub async fn migrate(pool: &PgPool) -> Result<(), sqlx::migrate::MigrateError> {
    sqlx::migrate!().run(pool).await
}

/// `parseStringArray`: string entries, trimmed, empties dropped.
fn json_string_list(value: Option<String>) -> Vec<String> {
    value
        .and_then(|raw| serde_json::from_str::<Vec<Value>>(&raw).ok())
        .map(|items| items.iter().filter_map(Value::as_str).map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()).collect())
        .unwrap_or_default()
}

/// Load the canonical Plexus `api_keys` rows into the authentication cache.
/// Mutating repositories will replace this cache incrementally; loading from
/// PostgreSQL here ensures credentials survive a process restart immediately.
pub async fn load_keys(pool: &PgPool) -> Result<HashMap<String, KeyConfig>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT name, secret, comment, quota_names, quota_name, allowed_models,
                allowed_providers, excluded_models, excluded_providers,
                allow_raw_passthrough, allowed_ips, expires_at, disabled_at, permission_group, generation
           FROM api_keys",
    )
    .fetch_all(pool)
    .await?;

    let mut keys = HashMap::new();
    for row in rows {
        let quota_names_raw: Option<String> = row.try_get("quota_names")?;
        let quotas_defined = quota_names_raw.is_some();
        let quotas = json_string_list(quota_names_raw);
        let legacy_quota = quotas.is_empty() && !quotas_defined;
        let legacy_defined = legacy_quota && row.try_get::<Option<String>, _>("quota_name").ok().flatten().is_some_and(|q| !q.is_empty());
        let quotas = if legacy_quota {
            row.try_get::<Option<String>, _>("quota_name")
                .ok()
                .flatten()
                .filter(|quota| !quota.is_empty())
                .map(|quota| vec![quota])
                .unwrap_or_default()
        } else {
            quotas
        };
        keys.insert(
            row.try_get("name")?,
            KeyConfig {
                secret: crate::secret_crypto::decrypt(&row.try_get::<String, _>("secret")?).map_err(sqlx::Error::Protocol)?,
                permission_group: row.try_get("permission_group")?,
                comment: row.try_get("comment")?,
                quota_names: quotas,
                allowed_models: json_string_list(row.try_get("allowed_models")?),
                allowed_providers: json_string_list(row.try_get("allowed_providers")?),
                excluded_models: json_string_list(row.try_get("excluded_models")?),
                excluded_providers: json_string_list(row.try_get("excluded_providers")?),
                allow_raw_passthrough: row.try_get("allow_raw_passthrough")?,
                allowed_ips: json_string_list(row.try_get("allowed_ips")?),
                expires_at: row.try_get("expires_at")?,
                disabled_at: row.try_get("disabled_at")?,
                quotas_defined: quotas_defined || legacy_defined,
                default_service_tier: row.try_get::<Option<Value>, _>("generation")?
                    .and_then(|g| g.get("serviceTier").and_then(Value::as_str).map(|t| t.trim().to_lowercase()))
                    .filter(|t| ["auto", "default", "standard", "flex", "priority", "fast", "ultrafast"].contains(&t.as_str())),
            },
        );
    }
    Ok(keys)
}

/// `stringifyStringArray`-style: trimmed, empties dropped.
fn json_list(values: &[String]) -> String {
    let normalized: Vec<&str> = values.iter().map(|v| v.trim()).filter(|v| !v.is_empty()).collect();
    serde_json::to_string(&normalized).expect("string lists are serializable")
}

pub async fn upsert_key(pool: &PgPool, name: &str, config: &KeyConfig) -> Result<(), sqlx::Error> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before Unix epoch")
        .as_millis() as i64;
    sqlx::query(
        "INSERT INTO api_keys
          (name, secret, comment, quota_names, allowed_models, allowed_providers,
           excluded_models, excluded_providers, allow_raw_passthrough, allowed_ips,
           expires_at, disabled_at, created_at, updated_at, permission_group, generation)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$13,$14,$15)
         ON CONFLICT (name) DO UPDATE SET
           secret=EXCLUDED.secret, comment=EXCLUDED.comment,
           quota_names=EXCLUDED.quota_names, allowed_models=EXCLUDED.allowed_models,
           allowed_providers=EXCLUDED.allowed_providers,
           excluded_models=EXCLUDED.excluded_models,
           excluded_providers=EXCLUDED.excluded_providers,
           allow_raw_passthrough=EXCLUDED.allow_raw_passthrough,
           allowed_ips=EXCLUDED.allowed_ips, expires_at=EXCLUDED.expires_at,
           disabled_at=EXCLUDED.disabled_at, updated_at=EXCLUDED.updated_at,
           permission_group=EXCLUDED.permission_group, generation=EXCLUDED.generation",
    )
    .bind(name)
    .bind(crate::secret_crypto::encrypt(&config.secret))
    .bind(&config.comment)
    .bind((config.quotas_defined || !config.quota_names.is_empty()).then(|| json_list(&config.quota_names)))
    .bind(json_list(&config.allowed_models))
    .bind(json_list(&config.allowed_providers))
    .bind(json_list(&config.excluded_models))
    .bind(json_list(&config.excluded_providers))
    .bind(config.allow_raw_passthrough)
    .bind(json_list(&config.allowed_ips))
    .bind(config.expires_at)
    .bind(config.disabled_at)
    .bind(now)
    .bind(&config.permission_group)
    .bind(config.default_service_tier.as_ref().map(|tier| serde_json::json!({"serviceTier": tier})))
    .execute(pool)
    .await
    .map(|_| ())
}

pub async fn delete_key(pool: &PgPool, name: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM api_keys WHERE name = $1")
        .bind(name)
        .execute(pool)
        .await
        .map(|_| ())
}

pub async fn delete_all_keys(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM api_keys")
        .execute(pool)
        .await
        .map(|_| ())
}

pub async fn load_providers(pool: &PgPool) -> Result<HashMap<String, ProviderConfig>, sqlx::Error> {
    let rows = sqlx::query("SELECT slug, version, display_name, api_base_url, api_base_urls, models, api_key, oauth_provider_type, oauth_account, enabled, disable_cooldown, stall_cooldown, allow_100_percent_utilization, discount, estimate_tokens, use_claude_masking, gemini_thinking_enabled, auto_compat, adapter, model_autosync_enabled, model_autosync_interval, raw_passthrough, quota_checker_type, quota_checker_id, quota_checker_enabled, quota_checker_interval, quota_checker_options, retry_policy, config FROM providers")
        .fetch_all(pool).await?;
    let mut result = HashMap::new();
    for row in rows {
        if let Some(stored) = row.try_get::<Option<Value>, _>("config")? {
            let api_key = row.try_get::<Option<String>, _>("api_key")?.map(|v| crate::secret_crypto::decrypt(&v)).transpose().map_err(sqlx::Error::Protocol)?;
            let view = loaded_provider_view(stored, api_key).map_err(sqlx::Error::Protocol)?;
            result.insert(row.try_get("slug")?, crate::provider_view::from_view(view, row.try_get("version")?));
            continue;
        }
        let base: serde_json::Value = row.try_get("api_base_url")?;
        let adapter: Option<serde_json::Value> = row.try_get("adapter")?;
        let raw: Option<serde_json::Value> = row.try_get("raw_passthrough")?;
        result.insert(
            row.try_get("slug")?,
            ProviderConfig {
                version: row.try_get("version")?,
                display_name: row.try_get("display_name")?,
                api_base_url: base.as_str().unwrap_or_default().to_string(),
                api_base_urls: row.try_get::<Option<Value>, _>("api_base_urls")?.and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default(),
                models: row.try_get::<Option<Value>, _>("models")?.and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default(),
                api_key: row.try_get::<Option<String>, _>("api_key")?.map(|v| crate::secret_crypto::decrypt(&v)).transpose().map_err(sqlx::Error::Protocol)?,
                oauth_provider: row.try_get("oauth_provider_type")?,
                oauth_account: row.try_get("oauth_account")?,
                enabled: row.try_get("enabled")?,
                disable_cooldown: row.try_get("disable_cooldown")?,
                stall_cooldown: row.try_get("stall_cooldown")?,
                allow_100_percent_utilization: row.try_get("allow_100_percent_utilization")?,
                discount: row.try_get("discount")?,
                estimate_tokens: row.try_get("estimate_tokens")?,
                use_claude_masking: row.try_get("use_claude_masking")?,
                gemini_thinking_enabled: row.try_get("gemini_thinking_enabled")?,
                auto_compat: row.try_get("auto_compat")?,
                adapter: adapter
                    .and_then(|v| serde_json::from_value::<Vec<AdapterEntry>>(v).ok())
                    .unwrap_or_default(),
                model_autosync: crate::state::ModelAutosync {
                    enabled: row.try_get("model_autosync_enabled")?,
                    interval_minutes: row.try_get::<i32, _>("model_autosync_interval")? as u32,
                },
                raw_passthrough: raw
                    .and_then(|v| serde_json::from_value::<RawPassthroughConfig>(v).ok()),
                quota_checker: row
                    .try_get::<Option<String>, _>("quota_checker_type")?
                    .map(|checker_type| -> Result<QuotaCheckerConfig, sqlx::Error> {
                        Ok(QuotaCheckerConfig {
                            checker_type: Some(checker_type),
                            id: row.try_get("quota_checker_id")?,
                            enabled: row.try_get("quota_checker_enabled")?,
                            interval: row.try_get::<i32, _>("quota_checker_interval")? as u64,
                            options: row
                                .try_get::<Option<String>, _>("quota_checker_options")?
                                .map(|text| serde_json::from_str::<Value>(&text).map_err(|e| {
                                    sqlx::Error::Protocol(format!(
                                        "providers.quota_checker_options is not valid JSON: {e}"
                                    ))
                                }))
                                .transpose()?
                                .unwrap_or(Value::Null),
                        })
                    })
                    .transpose()?,
                retry_policy: row
                    .try_get::<Option<serde_json::Value>, _>("retry_policy")?
                    .and_then(|v| serde_json::from_value(v).ok())
                    .unwrap_or_default(),
                view: None,
            },
        );
    }
    Ok(result)
}

/// Read one persisted provider through the existing JSONB/text decoder.
/// This avoids serving a stale process-local config after another instance edits it.
pub async fn load_provider(pool: &PgPool, slug: &str) -> Result<Option<ProviderConfig>, sqlx::Error> {
    Ok(load_providers(pool).await?.remove(slug))
}

/// The repository view persisted in `providers.config`. The API key lives in
/// its own encrypted column; headers and quota-checker options are encrypted
/// like Plexus's `encryptJsonField` when `ENCRYPTION_KEY` is set.
fn stored_provider_config(config: &ProviderConfig) -> Value {
    let mut view = config.public_view();
    if let Some(map) = view.as_object_mut() {
        map.shift_remove("api_key");
        if crate::secret_crypto::enabled() {
            if let Some(headers) = map.get_mut("headers") { *headers = Value::String(crate::secret_crypto::encrypt(&headers.to_string())); }
            if let Some(options) = map.get_mut("quota_checker").and_then(|q| q.get_mut("options")) { *options = Value::String(crate::secret_crypto::encrypt(&options.to_string())); }
        }
    }
    view
}

fn loaded_provider_view(mut view: Value, api_key: Option<String>) -> Result<Value, String> {
    let decrypt_json = |value: &mut Value| -> Result<(), String> {
        if let Value::String(text) = value { *value = serde_json::from_str(&crate::secret_crypto::decrypt(text)?).map_err(|e| e.to_string())?; }
        Ok(())
    };
    if let Some(map) = view.as_object_mut() {
        if let Some(headers) = map.get_mut("headers") { decrypt_json(headers)?; }
        if let Some(options) = map.get_mut("quota_checker").and_then(|q| q.get_mut("options")) { decrypt_json(options)?; }
        // Key order mirrors rowToProviderConfig (api_key follows display_name).
        if let Some(key) = api_key.filter(|k| !k.is_empty()) { map.insert("api_key".into(), Value::String(key)); }
    }
    Ok(view)
}

pub async fn upsert_provider(
    pool: &PgPool,
    slug: &str,
    config: &ProviderConfig,
) -> Result<(), sqlx::Error> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before Unix epoch")
        .as_millis() as i64;
    sqlx::query(
        "INSERT INTO providers
          (slug, display_name, api_base_url, api_base_urls, models, api_key, oauth_provider_type, enabled,
           disable_cooldown, stall_cooldown, allow_100_percent_utilization,
           discount, estimate_tokens, use_claude_masking, gemini_thinking_enabled,
           auto_compat, adapter, model_autosync_enabled, model_autosync_interval,
           raw_passthrough, quota_checker_type, quota_checker_id, quota_checker_enabled,
           quota_checker_interval, quota_checker_options, retry_policy, created_at, updated_at, oauth_account, config)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21,$22,$23,$24,$25,$26,$27,$28,$29,$30::text::json)
         ON CONFLICT (slug) DO UPDATE SET
           display_name=EXCLUDED.display_name, api_base_url=EXCLUDED.api_base_url,
           api_base_urls=EXCLUDED.api_base_urls, models=EXCLUDED.models,
           api_key=EXCLUDED.api_key, oauth_provider_type=EXCLUDED.oauth_provider_type, oauth_account=EXCLUDED.oauth_account,
           enabled=EXCLUDED.enabled, disable_cooldown=EXCLUDED.disable_cooldown,
           stall_cooldown=EXCLUDED.stall_cooldown,
           allow_100_percent_utilization=EXCLUDED.allow_100_percent_utilization,
           discount=EXCLUDED.discount, estimate_tokens=EXCLUDED.estimate_tokens,
           use_claude_masking=EXCLUDED.use_claude_masking,
           gemini_thinking_enabled=EXCLUDED.gemini_thinking_enabled,
           auto_compat=EXCLUDED.auto_compat, adapter=EXCLUDED.adapter,
           model_autosync_enabled=EXCLUDED.model_autosync_enabled,
           model_autosync_interval=EXCLUDED.model_autosync_interval,
           raw_passthrough=EXCLUDED.raw_passthrough,
           quota_checker_type=EXCLUDED.quota_checker_type,
           quota_checker_id=EXCLUDED.quota_checker_id,
           quota_checker_enabled=EXCLUDED.quota_checker_enabled,
           quota_checker_interval=EXCLUDED.quota_checker_interval,
           quota_checker_options=EXCLUDED.quota_checker_options,
           retry_policy=EXCLUDED.retry_policy, config=EXCLUDED.config,
           version=providers.version+1,
           updated_at=EXCLUDED.updated_at",
    )
    .bind(slug)
    .bind(&config.display_name)
    .bind(serde_json::Value::String(config.api_base_url.clone()))
    .bind(serde_json::to_value(&config.api_base_urls).unwrap())
    .bind(serde_json::to_value(&config.models).unwrap())
    .bind(crate::secret_crypto::encrypt_opt(&config.api_key))
    .bind(&config.oauth_provider)
    .bind(config.enabled)
    .bind(config.disable_cooldown)
    .bind(config.stall_cooldown)
    .bind(config.allow_100_percent_utilization)
    .bind(config.discount)
    .bind(config.estimate_tokens)
    .bind(config.use_claude_masking)
    .bind(config.gemini_thinking_enabled)
    .bind(config.auto_compat)
    .bind(serde_json::to_value(&config.adapter).unwrap())
    .bind(config.model_autosync.enabled)
    .bind(config.model_autosync.interval_minutes as i32)
    .bind(serde_json::to_value(&config.raw_passthrough).unwrap())
    .bind(config.quota_checker.as_ref().and_then(|q| q.checker_type.clone()))
    .bind(config.quota_checker.as_ref().and_then(|q| q.id.clone()))
    .bind(config.quota_checker.as_ref().map(|q| q.enabled).unwrap_or(true))
    .bind(config.quota_checker.as_ref().map(|q| q.interval as i32).unwrap_or(30))
    .bind(config.quota_checker.as_ref().map(|q| q.options.to_string()))
    .bind(serde_json::to_value(&config.retry_policy).unwrap())
    .bind(now)
    .bind(now)
    .bind(&config.oauth_account)
    .bind(crate::engine::js::stringify(&stored_provider_config(config)))
    .execute(pool)
    .await
    .map(|_| ())
}

pub async fn create_provider_if_absent(
    pool: &PgPool,
    slug: &str,
    config: &ProviderConfig,
) -> Result<bool, sqlx::Error> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before Unix epoch")
        .as_millis() as i64;
    sqlx::query(
        "INSERT INTO providers
          (slug, display_name, api_base_url, api_base_urls, models, api_key, oauth_provider_type, enabled,
           disable_cooldown, stall_cooldown, allow_100_percent_utilization,
           discount, estimate_tokens, use_claude_masking, gemini_thinking_enabled,
           auto_compat, adapter, model_autosync_enabled, model_autosync_interval,
           raw_passthrough, quota_checker_type, quota_checker_id, quota_checker_enabled,
           quota_checker_interval, quota_checker_options, retry_policy, created_at, updated_at, oauth_account, config)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21,$22,$23,$24,$25,$26,$27,$28,$29,$30::text::json)
         ON CONFLICT (slug) DO NOTHING",
    )
    .bind(slug)
    .bind(&config.display_name)
    .bind(serde_json::Value::String(config.api_base_url.clone()))
    .bind(serde_json::to_value(&config.api_base_urls).unwrap())
    .bind(serde_json::to_value(&config.models).unwrap())
    .bind(crate::secret_crypto::encrypt_opt(&config.api_key))
    .bind(&config.oauth_provider)
    .bind(config.enabled)
    .bind(config.disable_cooldown)
    .bind(config.stall_cooldown)
    .bind(config.allow_100_percent_utilization)
    .bind(config.discount)
    .bind(config.estimate_tokens)
    .bind(config.use_claude_masking)
    .bind(config.gemini_thinking_enabled)
    .bind(config.auto_compat)
    .bind(serde_json::to_value(&config.adapter).unwrap())
    .bind(config.model_autosync.enabled)
    .bind(config.model_autosync.interval_minutes as i32)
    .bind(serde_json::to_value(&config.raw_passthrough).unwrap())
    .bind(config.quota_checker.as_ref().and_then(|q| q.checker_type.clone()))
    .bind(config.quota_checker.as_ref().and_then(|q| q.id.clone()))
    .bind(config.quota_checker.as_ref().map(|q| q.enabled).unwrap_or(true))
    .bind(config.quota_checker.as_ref().map(|q| q.interval as i32).unwrap_or(30))
    .bind(config.quota_checker.as_ref().map(|q| q.options.to_string()))
    .bind(serde_json::to_value(&config.retry_policy).unwrap())
    .bind(now)
    .bind(now)
    .bind(&config.oauth_account)
    .bind(crate::engine::js::stringify(&stored_provider_config(config)))
    .execute(pool)
    .await
    .map(|result| result.rows_affected() == 1)
}

/// Conditional full-record update; one SQL statement wins or returns a conflict.
/// All editable fields and the revision change atomically in PostgreSQL.
pub async fn update_provider_if_version(
    pool: &PgPool, slug: &str, config: &ProviderConfig, expected_version: i64,
) -> Result<Option<i64>, sqlx::Error> {
    let row = sqlx::query("UPDATE providers SET display_name=$2,api_base_url=$3,api_base_urls=$4,models=$5,api_key=$6,oauth_provider_type=$7,oauth_account=$8,enabled=$9,disable_cooldown=$10,stall_cooldown=$11,allow_100_percent_utilization=$12,discount=$13,estimate_tokens=$14,use_claude_masking=$15,gemini_thinking_enabled=$16,auto_compat=$17,adapter=$18,model_autosync_enabled=$19,model_autosync_interval=$20,raw_passthrough=$21,quota_checker_type=$22,quota_checker_id=$23,quota_checker_enabled=$24,quota_checker_interval=$25,quota_checker_options=$26,retry_policy=$27,updated_at=$28,config=$30::text::json,version=version+1 WHERE slug=$1 AND version=$29 RETURNING version")
        .bind(slug).bind(&config.display_name)
        .bind(serde_json::Value::String(config.api_base_url.clone()))
        .bind(serde_json::to_value(&config.api_base_urls).unwrap())
        .bind(serde_json::to_value(&config.models).unwrap())
        .bind(crate::secret_crypto::encrypt_opt(&config.api_key)).bind(&config.oauth_provider).bind(&config.oauth_account)
        .bind(config.enabled).bind(config.disable_cooldown).bind(config.stall_cooldown)
        .bind(config.allow_100_percent_utilization).bind(config.discount)
        .bind(config.estimate_tokens).bind(config.use_claude_masking)
        .bind(config.gemini_thinking_enabled).bind(config.auto_compat)
        .bind(serde_json::to_value(&config.adapter).unwrap())
        .bind(config.model_autosync.enabled).bind(config.model_autosync.interval_minutes as i32)
        .bind(serde_json::to_value(&config.raw_passthrough).unwrap())
        .bind(config.quota_checker.as_ref().and_then(|q| q.checker_type.clone()))
        .bind(config.quota_checker.as_ref().and_then(|q| q.id.clone()))
        .bind(config.quota_checker.as_ref().map(|q| q.enabled).unwrap_or(true))
        .bind(config.quota_checker.as_ref().map(|q| q.interval as i32).unwrap_or(30))
        .bind(config.quota_checker.as_ref().map(|q| q.options.to_string()))
        .bind(serde_json::to_value(&config.retry_policy).unwrap())
        .bind(chrono::Utc::now().timestamp_millis()).bind(expected_version).bind(crate::engine::js::stringify(&stored_provider_config(config)))
        .fetch_optional(pool).await?;
    Ok(row.map(|row| sqlx::Row::get::<i64,_>(&row, "version")))
}

pub async fn delete_provider(pool: &PgPool, slug: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM providers WHERE slug = $1")
        .bind(slug)
        .execute(pool)
        .await
        .map(|_| ())
}

pub async fn delete_all_providers(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM providers")
        .execute(pool)
        .await
        .map(|_| ())
}

fn json_list_opt(value: &Option<Vec<String>>) -> Option<String> {
    value
        .as_ref()
        .map(|v| serde_json::to_string(v).expect("string lists are serializable"))
}

fn parse_json_list_opt(value: Option<String>) -> Option<Vec<String>> {
    value.and_then(|raw| serde_json::from_str(&raw).ok())
}

/// Load all model aliases and their target groups from PostgreSQL.
pub async fn load_aliases(pool: &PgPool) -> Result<HashMap<String, ModelConfig>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, slug, priority::text, additional_aliases, use_image_fallthrough, enforce_limits,
                synthetic_safeguard_approval,
                sticky_session, preferred_api, target_groups, advanced, pi_model,
                extra_body, compaction, model_type, config
           FROM model_aliases",
    )
    .fetch_all(pool)
    .await?;

    let mut result = HashMap::new();
    for row in rows {
        if let Some(view) = row.try_get::<Option<Value>, _>("config")? {
            result.insert(row.try_get("slug")?, crate::alias_view::from_view(view));
            continue;
        }
        let additional_aliases: Option<serde_json::Value> = row.try_get("additional_aliases")?;
        let preferred_api: Option<serde_json::Value> = row.try_get("preferred_api")?;
        let target_groups: Option<serde_json::Value> = row.try_get("target_groups")?;
        let advanced: Option<serde_json::Value> = row.try_get("advanced")?;
        let pi_model: Option<serde_json::Value> = row.try_get("pi_model")?;
        let extra_body: Option<serde_json::Value> = row.try_get("extra_body")?;
        let compaction: Option<serde_json::Value> = row.try_get("compaction")?;

        result.insert(
            row.try_get("slug")?,
            ModelConfig {
                priority: row
                    .try_get::<Option<String>, _>("priority")?
                    .unwrap_or_else(|| "selector".to_string()),
                target_groups: target_groups
                    .and_then(|v| serde_json::from_value::<Vec<ModelTargetGroup>>(v).ok())
                    .unwrap_or_default(),
                additional_aliases: additional_aliases
                    .and_then(|v| serde_json::from_value::<Vec<String>>(v).ok())
                    .unwrap_or_default(),
                use_image_fallthrough: row.try_get("use_image_fallthrough")?,
                enforce_limits: row.try_get("enforce_limits")?,
                sticky_session: row.try_get("sticky_session")?,
                synthetic_safeguard_approval: row.try_get("synthetic_safeguard_approval")?,
                preferred_api: preferred_api
                    .and_then(|v| serde_json::from_value::<Vec<String>>(v).ok()),
                model_type: row.try_get("model_type")?,
                advanced,
                metadata: None,
                pi_model,
                extra_body,
                compaction,
                view: None,
            },
        );
    }
    Ok(result)
}

pub async fn upsert_alias(
    pool: &PgPool,
    slug: &str,
    config: &ModelConfig,
) -> Result<(), sqlx::Error> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before Unix epoch")
        .as_millis() as i64;
    let additional_aliases_json = if config.additional_aliases.is_empty() {
        None
    } else {
        Some(serde_json::to_value(&config.additional_aliases).expect("serializable"))
    };
    let target_groups_json = if config.target_groups.is_empty() {
        None
    } else {
        Some(serde_json::to_value(&config.target_groups).expect("serializable"))
    };
    let preferred_api_json = config
        .preferred_api
        .as_ref()
        .map(|v| serde_json::to_value(v).expect("serializable"));

    let row = sqlx::query(
        "INSERT INTO model_aliases
          (slug, priority, additional_aliases, use_image_fallthrough, enforce_limits, synthetic_safeguard_approval,
           sticky_session, preferred_api, target_groups, advanced, pi_model, extra_body,
           compaction, model_type, created_at, updated_at, config)
         VALUES ($1,$2::alias_priority,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$15,$16::text::json)
         ON CONFLICT (slug) DO UPDATE SET
           priority=EXCLUDED.priority, additional_aliases=EXCLUDED.additional_aliases,
           use_image_fallthrough=EXCLUDED.use_image_fallthrough,
           enforce_limits=EXCLUDED.enforce_limits, sticky_session=EXCLUDED.sticky_session,
           synthetic_safeguard_approval=EXCLUDED.synthetic_safeguard_approval,
           preferred_api=EXCLUDED.preferred_api, target_groups=EXCLUDED.target_groups,
           advanced=EXCLUDED.advanced, pi_model=EXCLUDED.pi_model,
           extra_body=EXCLUDED.extra_body, compaction=EXCLUDED.compaction,
           model_type=EXCLUDED.model_type, updated_at=EXCLUDED.updated_at, config=EXCLUDED.config
         RETURNING id",
    )
    .bind(slug)
    .bind(&config.priority)
    .bind(additional_aliases_json)
    .bind(config.use_image_fallthrough)
    .bind(config.enforce_limits)
    .bind(config.synthetic_safeguard_approval)
    .bind(config.sticky_session)
    .bind(preferred_api_json)
    .bind(target_groups_json)
    .bind(&config.advanced)
    .bind(&config.pi_model)
    .bind(&config.extra_body)
    .bind(&config.compaction)
    .bind(&config.model_type)
    .bind(timestamp)
    // Bound as text: a serde_json bind is JSONB and would reorder keys.
    .bind(crate::engine::js::stringify(&config.public_view()))
    .fetch_one(pool)
    .await?;

    let alias_id: i32 = row.try_get("id")?;
    sqlx::query("DELETE FROM model_alias_targets WHERE alias_id = $1")
        .bind(alias_id)
        .execute(pool)
        .await?;

    // One running sort index across groups, as Plexus `saveAlias` does.
    let mut idx = 0usize;
    for group in &config.target_groups {
        for target in group.targets.iter() {
            idx += 1;
            let idx = idx - 1;
            sqlx::query(
                "INSERT INTO model_alias_targets
                  (alias_id, provider_slug, model_name, target_alias_slug, enabled,
                   group_name, sort_order)
                 VALUES ($1,$2,$3,$4,$5,$6,$7)
                 ON CONFLICT DO NOTHING",
            )
            .bind(alias_id)
            .bind(&target.provider)
            .bind(&target.model)
            .bind(&target.alias)
            .bind(target.enabled)
            .bind(&group.name)
            .bind(idx as i32)
            .execute(pool)
            .await?;
        }
    }
    Ok(())
}

pub async fn delete_alias(pool: &PgPool, slug: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM model_aliases WHERE slug = $1")
        .bind(slug)
        .execute(pool)
        .await
        .map(|_| ())
}

pub async fn delete_all_aliases(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM model_aliases")
        .execute(pool)
        .await
        .map(|_| ())
}

/// Load all user quota definitions from PostgreSQL.
pub async fn load_user_quotas(
    pool: &PgPool,
) -> Result<HashMap<String, QuotaDefinition>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT name, quota_type::text, limit_type::text, limit_value, duration, allowed_models,
                allowed_providers, excluded_models, excluded_providers, shared, warn_at
           FROM user_quota_definitions",
    )
    .fetch_all(pool)
    .await?;

    let mut result = HashMap::new();
    for row in rows {
        result.insert(
            row.try_get("name")?,
            QuotaDefinition {
                quota_type: row.try_get("quota_type")?,
                limit_type: row.try_get("limit_type")?,
                limit: row.try_get::<f64, _>("limit_value")?,
                duration: row.try_get("duration")?,
                allowed_providers: parse_json_list_opt(row.try_get("allowed_providers")?),
                excluded_providers: parse_json_list_opt(row.try_get("excluded_providers")?),
                allowed_models: parse_json_list_opt(row.try_get("allowed_models")?),
                excluded_models: parse_json_list_opt(row.try_get("excluded_models")?),
                shared: row.try_get::<Option<bool>, _>("shared")?,
                warn_at: row.try_get::<Option<f64>, _>("warn_at")?,
            },
        );
    }
    Ok(result)
}

pub async fn upsert_user_quota(
    pool: &PgPool,
    name: &str,
    quota: &QuotaDefinition,
) -> Result<(), sqlx::Error> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before Unix epoch")
        .as_millis() as i64;
    sqlx::query(
        "INSERT INTO user_quota_definitions
          (name, quota_type, limit_type, limit_value, duration, allowed_models,
           allowed_providers, excluded_models, excluded_providers, shared, warn_at,
           created_at, updated_at)
         VALUES ($1,$2::quota_type,$3::limit_type,$4,$5,$6,$7,$8,$9,$10,$11,$12,$12)
         ON CONFLICT (name) DO UPDATE SET
           quota_type=EXCLUDED.quota_type, limit_type=EXCLUDED.limit_type,
           limit_value=EXCLUDED.limit_value, duration=EXCLUDED.duration,
           allowed_models=EXCLUDED.allowed_models, allowed_providers=EXCLUDED.allowed_providers,
           excluded_models=EXCLUDED.excluded_models, excluded_providers=EXCLUDED.excluded_providers,
           shared=EXCLUDED.shared, warn_at=EXCLUDED.warn_at, updated_at=EXCLUDED.updated_at",
    )
    .bind(name)
    .bind(&quota.quota_type)
    .bind(&quota.limit_type)
    .bind(quota.limit)
    .bind(&quota.duration)
    .bind(json_list_opt(&quota.allowed_models))
    .bind(json_list_opt(&quota.allowed_providers))
    .bind(json_list_opt(&quota.excluded_models))
    .bind(json_list_opt(&quota.excluded_providers))
    .bind(quota.shared.unwrap_or(false))
    .bind(quota.warn_at)
    .bind(timestamp)
    .execute(pool)
    .await
    .map(|_| ())
}

pub async fn delete_user_quota(pool: &PgPool, name: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM user_quota_definitions WHERE name = $1")
        .bind(name)
        .execute(pool)
        .await
        .map(|_| ())
}

/// Load all system settings from PostgreSQL, unwrapping the `{"value": ...}`
/// envelope Plexus's `SystemSettingsRepository` wraps every value in.
pub async fn load_settings(
    pool: &PgPool,
) -> Result<HashMap<String, serde_json::Value>, sqlx::Error> {
    let rows = sqlx::query("SELECT key, value FROM system_settings")
        .fetch_all(pool)
        .await?;
    let mut result = HashMap::new();
    for row in rows {
        let raw: Option<serde_json::Value> = row.try_get("value")?;
        let unwrapped = match raw {
            Some(serde_json::Value::Object(mut obj)) if obj.contains_key("value") => {
                obj.remove("value").unwrap_or(serde_json::Value::Null)
            }
            Some(other) => other,
            None => serde_json::Value::Null,
        };
        result.insert(row.try_get("key")?, unwrapped);
    }
    Ok(result)
}

pub async fn upsert_settings_bulk(
    pool: &PgPool,
    entries: &serde_json::Map<String, serde_json::Value>,
) -> Result<(), sqlx::Error> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before Unix epoch")
        .as_millis() as i64;
    let mut tx = pool.begin().await?;
    for (key, value) in entries {
        let wrapped = serde_json::json!({ "value": value });
        sqlx::query(
            "INSERT INTO system_settings (key, value, updated_at)
             VALUES ($1,$2,$3)
             ON CONFLICT (key) DO UPDATE SET value=EXCLUDED.value, updated_at=EXCLUDED.updated_at",
        )
        .bind(key)
        .bind(wrapped)
        .bind(timestamp)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await
}

/// Load active provider cooldowns from PostgreSQL.
pub async fn load_cooldowns(
    pool: &PgPool,
) -> Result<HashMap<(String, String), CooldownEntry>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT provider, model, expiry, consecutive_failures, last_error FROM provider_cooldowns",
    )
    .fetch_all(pool)
    .await?;
    let mut result = HashMap::new();
    for row in rows {
        let provider: String = row.try_get("provider")?;
        let model: String = row.try_get("model")?;
        result.insert(
            (provider, model),
            CooldownEntry {
                expiry_ms: row.try_get("expiry")?,
                consecutive_failures: row.try_get::<i64, _>("consecutive_failures")? as u32,
                last_error: row.try_get("last_error")?,
            },
        );
    }
    Ok(result)
}

pub async fn upsert_cooldown(
    pool: &PgPool,
    provider: &str,
    model: &str,
    entry: &CooldownEntry,
) -> Result<(), sqlx::Error> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before Unix epoch")
        .as_millis() as i64;
    sqlx::query(
        "INSERT INTO provider_cooldowns (provider, model, expiry, consecutive_failures, last_error, created_at)
         VALUES ($1,$2,$3,$4,$5,$6)
         ON CONFLICT (provider, model) DO UPDATE SET
           expiry=EXCLUDED.expiry, consecutive_failures=provider_cooldowns.consecutive_failures + EXCLUDED.consecutive_failures,
           last_error=EXCLUDED.last_error",
    )
    .bind(provider)
    .bind(model)
    .bind(entry.expiry_ms)
    .bind(entry.consecutive_failures as i64)
    .bind(&entry.last_error)
    .bind(timestamp)
    .execute(pool)
    .await
    .map(|_| ())
}

pub async fn delete_all_cooldowns(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM provider_cooldowns")
        .execute(pool)
        .await
        .map(|_| ())
}

pub async fn delete_cooldowns_by_provider(
    pool: &PgPool,
    provider: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM provider_cooldowns WHERE provider = $1")
        .bind(provider)
        .execute(pool)
        .await
        .map(|_| ())
}

pub async fn delete_cooldown(
    pool: &PgPool,
    provider: &str,
    model: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM provider_cooldowns WHERE provider = $1 AND model = $2")
        .bind(provider)
        .bind(model)
        .execute(pool)
        .await
        .map(|_| ())
}

/// Record the minimal request lifecycle in Plexus's `request_usage` table.
#[allow(clippy::too_many_arguments)] // Mirrors the flat request_usage row fields at one SQL seam.
pub async fn record_request_usage(
    pool: &PgPool,
    request_id: &str,
    api_key: Option<&str>,
    provider: Option<&str>,
    model: Option<&str>,
    incoming_api_type: &str,
    status: &str,
    streamed: bool,
    start_time: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO request_usage
          (request_id, date, api_key, provider, incoming_model_alias,
           incoming_api_type, response_status, is_streamed, start_time, created_at, permission_group)
         SELECT $1,$2,$3,$4,$5,$6,$7,$8,$9,$9,
                (SELECT permission_group FROM api_keys WHERE name = $3)
         ON CONFLICT (request_id) DO UPDATE SET response_status=EXCLUDED.response_status",
    )
    .bind(request_id)
    .bind(start_time.to_string())
    .bind(api_key)
    .bind(provider)
    .bind(model)
    .bind(incoming_api_type)
    .bind(status)
    .bind(if streamed { 1 } else { 0 })
    .bind(start_time)
    .execute(pool)
    .await
    .map(|_| ())
}

pub async fn delete_request_usage(pool: &PgPool, request_id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM request_usage WHERE request_id = $1")
        .bind(request_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn update_request_usage_metrics(
    pool: &PgPool,
    request_id: &str,
    input_tokens: Option<i32>,
    output_tokens: Option<i32>,
    total_cost: Option<f32>,
    duration_ms: i64,
    ttft_ms: Option<i64>,
    tokens_per_sec: Option<f32>,
    e2e_tokens_per_sec: Option<f32>,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE request_usage SET tokens_input = $2, tokens_output = $3, cost_total = $4, duration_ms = $5, ttft_ms = $6, tokens_per_sec = $7, e2e_tokens_per_sec = $8 WHERE request_id = $1")
        .bind(request_id).bind(input_tokens).bind(output_tokens).bind(total_cost).bind(duration_ms).bind(ttft_ms.map(|v| v as f32)).bind(tokens_per_sec).bind(e2e_tokens_per_sec)
        .execute(pool).await.map(|_| ())
}

pub async fn record_provider_performance(pool: &PgPool, provider: &str, model: &str, request_id: &str, ttft_ms: Option<i64>, tokens_per_sec: Option<f32>, e2e_tokens_per_sec: Option<f32>, duration_ms: i64) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO provider_performance (provider,model,request_id,time_to_first_token_ms,tokens_per_sec,e2e_tokens_per_sec,duration_ms,total_tokens,created_at,success_count,failure_count) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,1,0)")
        .bind(provider).bind(model).bind(request_id).bind(ttft_ms.map(|v| v as f32)).bind(tokens_per_sec).bind(e2e_tokens_per_sec).bind(duration_ms as f32).bind(Option::<i32>::None).bind(chrono::Utc::now().timestamp_millis())
        .execute(pool).await.map(|_| ())
}

pub async fn update_request_attempts(
    pool: &PgPool,
    request_id: &str,
    attempts: i32,
    provider: &str,
    model: &str,
    history: &serde_json::Value,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE request_usage SET attempt_count=$2, final_attempt_provider=$3, final_attempt_model=$4, retry_history=$5 WHERE request_id=$1")
        .bind(request_id).bind(attempts).bind(provider).bind(model).bind(history.to_string())
        .execute(pool).await.map(|_| ())
}

/// Persist the complete, ordered provider-attempt history for a request.
/// The JSON column is deliberately append-only from the caller's perspective:
/// a retry updates the same usage row, so a restart or later usage read sees
/// the exact attempts that led to the terminal result.
pub async fn record_request_attempt(
    pool: &PgPool,
    request_id: &str,
    attempts: &[serde_json::Value],
    final_provider: &str,
    final_model: &str,
) -> Result<(), sqlx::Error> {
    let history = serde_json::Value::Array(attempts.to_vec());
    update_request_attempts(
        pool,
        request_id,
        attempts.len() as i32,
        final_provider,
        final_model,
        &history,
    )
    .await
}

/// Insert or refresh one attempt without duplicating it during recovery.
pub async fn insert_inference_attempt(
    pool: &PgPool,
    request_id: &str,
    attempt_index: i32,
    provider: &str,
    model: &str,
    api_type: Option<&str>,
    status: &str,
    reason: Option<&str>,
    status_code: Option<i32>,
    retryable: bool,
    headers: Option<&serde_json::Value>,
    started_at: i64,
    completed_at: Option<i64>,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO inference_attempts (request_id,attempt_index,provider,model,api_type,status,reason,status_code,retryable,provider_response_headers,started_at,completed_at) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12) ON CONFLICT (request_id,attempt_index) DO UPDATE SET status=EXCLUDED.status,reason=EXCLUDED.reason,status_code=EXCLUDED.status_code,retryable=EXCLUDED.retryable,provider_response_headers=EXCLUDED.provider_response_headers,completed_at=EXCLUDED.completed_at")
        .bind(request_id).bind(attempt_index).bind(provider).bind(model).bind(api_type)
        .bind(status).bind(reason).bind(status_code).bind(if retryable { 1 } else { 0 })
        .bind(headers).bind(started_at).bind(completed_at).execute(pool).await.map(|_| ())
}

/// Calendar window start (UTC, epoch ms) for daily/weekly/monthly quota
/// types, matching Plexus's `QuotaEnforcer.getWindowStart` exactly: daily
/// is UTC midnight, weekly is the most recent UTC Monday midnight, monthly
/// is the 1st of the current UTC month. The naive `now_ms % period_ms`
/// approach used previously anchors weekly windows to the Unix epoch
/// (1970-01-01, a Thursday) instead of Monday, and treats every month as
/// a fixed 30 days instead of a real calendar month.
fn calendar_window_start_ms(quota_type: &str, now_ms: i64) -> Option<i64> {
    use chrono::{Datelike, TimeZone, Utc};
    let now = Utc.timestamp_millis_opt(now_ms).single()?;
    match quota_type {
        "daily" => Some(
            now.date_naive()
                .and_hms_opt(0, 0, 0)?
                .and_utc()
                .timestamp_millis(),
        ),
        "weekly" => {
            let days_since_monday = now.weekday().num_days_from_monday() as i64;
            Some(
                (now.date_naive() - chrono::Duration::days(days_since_monday))
                    .and_hms_opt(0, 0, 0)?
                    .and_utc()
                    .timestamp_millis(),
            )
        }
        "monthly" => Some(
            Utc.with_ymd_and_hms(now.year(), now.month(), 1, 0, 0, 0)
                .single()?
                .timestamp_millis(),
        ),
        _ => None,
    }
}

/// End (epoch ms, exclusive) of the calendar window that started at
/// `window_start_ms`, matching Plexus's `QuotaEnforcer.getWindowEnd`:
/// daily is +24h, weekly is +7d, monthly is the 1st of the following
/// UTC month (not a fixed 30/31-day add, so it stays correct across
/// month-length boundaries).
fn calendar_window_end_ms(quota_type: &str, window_start_ms: i64) -> i64 {
    use chrono::{Datelike, TimeZone, Utc};
    match quota_type {
        "daily" => window_start_ms + 24 * 60 * 60 * 1000,
        "weekly" => window_start_ms + 7 * 24 * 60 * 60 * 1000,
        _ => {
            let start = Utc.timestamp_millis_opt(window_start_ms).single().unwrap();
            let (year, month) = if start.month() == 12 {
                (start.year() + 1, 1)
            } else {
                (start.year(), start.month() + 1)
            };
            Utc.with_ymd_and_hms(year, month, 1, 0, 0, 0)
                .single()
                .unwrap()
                .timestamp_millis()
        }
    }
}

fn parse_duration_ms(value: Option<&str>) -> Option<i64> {
    let value = value?.trim();
    let split = value.len().checked_sub(1)?;
    let number: i64 = value[..split].parse().ok()?;
    let multiplier = match &value[split..] {
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => return None,
    };
    number.checked_mul(multiplier)
}

pub async fn reserve_request_quota(
    pool: &PgPool,
    key_name: &str,
    quota_name: &str,
    limit: i32,
    window_start: i64,
    rolling_duration_ms: Option<i64>,
    now_ms: i64,
) -> Result<Option<i64>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1 || ':' || $2, 0))")
        .bind(key_name)
        .bind(quota_name)
        .execute(&mut *tx)
        .await?;
    let current: Option<(f32, i64, Option<i64>, String)> = sqlx::query_as(
        "SELECT current_usage, last_updated, window_start, limit_type FROM quota_state WHERE key_name = $1 AND quota_name = $2 FOR UPDATE",
    )
    .bind(key_name)
    .bind(quota_name)
    .fetch_optional(&mut *tx)
    .await?;
    let (usage, last_updated, stored_window, stored_limit_type) =
        current.unwrap_or((0.0, now_ms, None, "requests".into()));
    let current_usage = if let Some(duration_ms) = rolling_duration_ms {
        if stored_limit_type != "requests" {
            0.0
        } else {
            let leak_per_ms = f64::from(limit) / duration_ms as f64;
            (f64::from(usage) - (now_ms - last_updated).max(0) as f64 * leak_per_ms).max(0.0)
        }
    } else if stored_window == Some(window_start) {
        f64::from(usage)
    } else {
        0.0
    };
    if current_usage >= f64::from(limit) {
        tx.rollback().await?;
        return Ok(Some(current_usage.round() as i64));
    }
    if rolling_duration_ms.is_some() {
        sqlx::query(
            "INSERT INTO quota_state (key_name, quota_name, limit_type, current_usage, last_updated, window_start)
             VALUES ($1,$2,'requests',$3,$4,NULL)
             ON CONFLICT (key_name, quota_name) DO UPDATE SET current_usage=EXCLUDED.current_usage, last_updated=EXCLUDED.last_updated, window_start=NULL, limit_type='requests'",
        )
        .bind(key_name)
        .bind(quota_name)
        .bind((current_usage + 1.0) as f32)
        .bind(now_ms)
        .execute(&mut *tx)
        .await?;
    } else {
        sqlx::query(
            "INSERT INTO quota_state (key_name, quota_name, limit_type, current_usage, last_updated, window_start)
             VALUES ($1,$2,'requests',1,$3,$4)
             ON CONFLICT (key_name, quota_name) DO UPDATE SET current_usage = CASE WHEN quota_state.window_start IS DISTINCT FROM EXCLUDED.window_start THEN 1 ELSE quota_state.current_usage + 1 END, last_updated = EXCLUDED.last_updated, window_start = EXCLUDED.window_start, limit_type='requests'",
        )
        .bind(key_name)
        .bind(quota_name)
        .bind(now_ms)
        .bind(window_start)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(None)
}

pub async fn reserve_request_quotas(
    pool: &PgPool,
    key_name: &str,
    windows: &[(String, i32, i64, i64, Option<i64>)],
    now_ms: i64,
) -> Result<Vec<(String, i32, i64, i64)>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let mut blocked = Vec::new();
    let mut usage = Vec::new();
    for (name, limit, start, end, rolling) in windows {
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1 || ':' || $2, 0))")
            .bind(key_name)
            .bind(name)
            .execute(&mut *tx)
            .await?;
        let row: Option<(f32, i64, Option<i64>, String)> = sqlx::query_as(
            "SELECT current_usage,last_updated,window_start,limit_type FROM quota_state WHERE key_name=$1 AND quota_name=$2 FOR UPDATE")
            .bind(key_name).bind(name).fetch_optional(&mut *tx).await?;
        let (old, updated, old_start, kind) = row.unwrap_or((0.0, now_ms, None, "requests".into()));
        let current = if let Some(duration) = rolling {
            if kind != "requests" {
                0.0
            } else {
                (f64::from(old)
                    - (now_ms - updated).max(0) as f64 * f64::from(*limit) / *duration as f64)
                    .max(0.0)
            }
        } else if old_start == Some(*start) {
            f64::from(old)
        } else {
            0.0
        };
        if current >= f64::from(*limit) {
            blocked.push((name.clone(), *limit, current.round() as i64, *end));
        }
        usage.push((name, *start, *rolling, current));
    }
    if !blocked.is_empty() {
        tx.rollback().await?;
        return Ok(blocked);
    }
    for (name, start, rolling, current) in usage {
        if rolling.is_some() {
            sqlx::query("INSERT INTO quota_state (key_name,quota_name,limit_type,current_usage,last_updated,window_start) VALUES ($1,$2,'requests',$3,$4,NULL) ON CONFLICT (key_name,quota_name) DO UPDATE SET current_usage=EXCLUDED.current_usage,last_updated=EXCLUDED.last_updated,window_start=NULL,limit_type='requests'")
                .bind(key_name).bind(name).bind((current + 1.0) as f32).bind(now_ms).execute(&mut *tx).await?;
        } else {
            sqlx::query("INSERT INTO quota_state (key_name,quota_name,limit_type,current_usage,last_updated,window_start) VALUES ($1,$2,'requests',1,$3,$4) ON CONFLICT (key_name,quota_name) DO UPDATE SET current_usage=CASE WHEN quota_state.window_start IS DISTINCT FROM EXCLUDED.window_start THEN 1 ELSE quota_state.current_usage+1 END,last_updated=EXCLUDED.last_updated,window_start=EXCLUDED.window_start,limit_type='requests'")
                .bind(key_name).bind(name).bind(now_ms).bind(start).execute(&mut *tx).await?;
        }
    }
    tx.commit().await?;
    Ok(blocked)
}

pub async fn request_quota_windows(
    pool: &PgPool,
    quota_names: &[String],
    now_ms: i64,
) -> Result<Vec<(String, i32, i64, i64, Option<i64>)>, sqlx::Error> {
    let mut windows = Vec::new();
    for name in quota_names {
        let Some(row) = sqlx::query("SELECT quota_type::text, limit_type::text, limit_value, duration FROM user_quota_definitions WHERE name = $1")
            .bind(name).fetch_optional(pool).await? else { continue };
        if row.try_get::<String, _>("limit_type")? != "requests" {
            continue;
        }
        let kind: String = row.try_get("quota_type")?;
        let limit: f64 = row.try_get("limit_value")?;
        let duration: Option<String> = row.try_get("duration")?;
        let (start, end, rolling_duration) = match kind.as_str() {
            "rolling" => {
                let duration_ms = parse_duration_ms(duration.as_deref()).unwrap_or(0);
                if duration_ms <= 0 {
                    continue;
                }
                (0, now_ms + duration_ms, Some(duration_ms))
            }
            "daily" | "weekly" | "monthly" => match calendar_window_start_ms(&kind, now_ms) {
                Some(start) => (start, calendar_window_end_ms(&kind, start), None),
                None => continue,
            },
            _ => continue,
        };
        windows.push((name.clone(), limit as i32, start, end, rolling_duration));
    }
    Ok(windows)
}

#[cfg(test)]
mod tests {
    use super::{
        calendar_window_start_ms, connect, delete_key, load_keys, migrate, upsert_key,
        DEFAULT_MAX_CONNECTIONS,
    };
    use crate::state::KeyConfig;

    #[test]
    fn pool_default_is_bounded() {
        assert_eq!(DEFAULT_MAX_CONNECTIONS, 16);
    }

    #[test]
    fn weekly_window_anchors_to_utc_monday_not_unix_epoch() {
        // 2024-01-10 is a Wednesday; the most recent Monday is 2024-01-08.
        let wednesday_ms = 1_704_844_800_000; // 2024-01-10T00:00:00Z
        let expected_monday_ms = 1_704_672_000_000; // 2024-01-08T00:00:00Z
        assert_eq!(
            calendar_window_start_ms("weekly", wednesday_ms),
            Some(expected_monday_ms)
        );

        // A naive `now_ms % (7 * 86_400_000)` anchors to 1970-01-01 (a
        // Thursday), so it would NOT equal the real Monday boundary here --
        // this assertion documents exactly the bug being fixed.
        let naive_epoch_anchored = wednesday_ms - wednesday_ms % (7 * 86_400_000);
        assert_ne!(naive_epoch_anchored, expected_monday_ms);
    }

    #[test]
    fn monthly_window_is_first_of_calendar_month_not_fixed_30_days() {
        // 2024-03-15: naive 30-day math and real calendar-month math must
        // diverge here (Jan+Feb 2024 = 60 days, not 2*30), proving the fix
        // isn't a no-op for months that aren't exactly 30 days apart.
        let march_15_ms = 1_710_460_800_000; // 2024-03-15T00:00:00Z
        let expected_march_1_ms = 1_709_251_200_000; // 2024-03-01T00:00:00Z
        assert_eq!(
            calendar_window_start_ms("monthly", march_15_ms),
            Some(expected_march_1_ms)
        );
    }

    #[test]
    fn daily_window_is_utc_midnight() {
        let noon_ms = 1_704_888_000_000; // 2024-01-10T12:00:00Z
        let midnight_ms = 1_704_844_800_000; // 2024-01-10T00:00:00Z
        assert_eq!(
            calendar_window_start_ms("daily", noon_ms),
            Some(midnight_ms)
        );
    }

    #[test]
    fn rolling_type_is_not_a_calendar_window() {
        assert_eq!(calendar_window_start_ms("rolling", 1_704_888_000_000), None);
    }

    #[tokio::test]
    async fn postgres_key_round_trip_is_available_when_configured() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            return;
        };
        let pool = connect(&url).await.expect("DATABASE_URL must be reachable");
        migrate(&pool).await.expect("Plexus migrations must apply");

        let name = format!("trefoil-db-test-{}", std::process::id());
        let config = KeyConfig {
            secret: format!("secret-{name}"),
            comment: Some("database round trip".to_string()),
            quota_names: vec!["daily".to_string()],
            allowed_providers: vec!["openai".to_string()],
            ..Default::default()
        };
        upsert_key(&pool, &name, &config)
            .await
            .expect("key upsert must succeed");

        let loaded = load_keys(&pool).await.expect("key load must succeed");
        assert_eq!(
            loaded.get(&name).map(|key| &key.comment),
            Some(&config.comment)
        );
        assert_eq!(
            loaded.get(&name).map(|key| &key.quota_names),
            Some(&config.quota_names)
        );

        delete_key(&pool, &name)
            .await
            .expect("test key cleanup must succeed");
        let after_cleanup = load_keys(&pool).await.expect("key reload must succeed");
        assert!(!after_cleanup.contains_key(&name));
    }
}

/// Pinned Plexus `encrypt-migration.ts`: with `ENCRYPTION_KEY` set, encrypt
/// legacy plaintext provider API keys and key secrets in place at startup.
pub async fn encrypt_plaintext_secrets(pool: &PgPool) -> Result<(), sqlx::Error> {
    if !crate::secret_crypto::enabled() { return Ok(()); }
    let mut tx = pool.begin().await?;
    for (table, id, column) in [("providers", "slug", "api_key"), ("api_keys", "name", "secret")] {
        let rows: Vec<(String, String)> = sqlx::query_as(&format!("SELECT {id}, {column} FROM {table} WHERE {column} IS NOT NULL AND {column} NOT LIKE 'enc:v1:%' FOR UPDATE"))
            .fetch_all(&mut *tx).await?;
        for (key, value) in rows {
            sqlx::query(&format!("UPDATE {table} SET {column}=$2 WHERE {id}=$1")).bind(key).bind(crate::secret_crypto::encrypt(&value)).execute(&mut *tx).await?;
        }
    }
    tx.commit().await
}
