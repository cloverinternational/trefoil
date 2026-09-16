//! PostgreSQL connection and migration bootstrap.
//!
//! The table layout is intentionally based on Plexus's PostgreSQL schema.
//! Route repositories will be added incrementally; this module establishes a
//! single production connection contract without silently falling back to the
//! old in-memory state.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use sqlx::{postgres::PgPoolOptions, PgPool, Row};

use crate::state::{
    AdapterEntry, CooldownEntry, KeyConfig, ModelConfig, ModelTargetGroup, ProviderConfig,
    QuotaDefinition, RawPassthroughConfig,
};

pub const DEFAULT_MAX_CONNECTIONS: u32 = 16;

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

fn json_string_list(value: Option<String>) -> Vec<String> {
    value
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

/// Load the canonical Plexus `api_keys` rows into the authentication cache.
/// Mutating repositories will replace this cache incrementally; loading from
/// PostgreSQL here ensures credentials survive a process restart immediately.
pub async fn load_keys(pool: &PgPool) -> Result<HashMap<String, KeyConfig>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT name, secret, comment, quota_names, quota_name, allowed_models,
                allowed_providers, excluded_models, excluded_providers,
                allow_raw_passthrough, allowed_ips, expires_at, disabled_at
           FROM api_keys",
    )
    .fetch_all(pool)
    .await?;

    let mut keys = HashMap::new();
    for row in rows {
        let quotas = json_string_list(row.try_get("quota_names")?);
        let quotas = if quotas.is_empty() {
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
                secret: row.try_get("secret")?,
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
            },
        );
    }
    Ok(keys)
}

fn json_list(values: &[String]) -> String {
    serde_json::to_string(values).expect("string lists are serializable")
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
           expires_at, disabled_at, created_at, updated_at)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$13)
         ON CONFLICT (name) DO UPDATE SET
           secret=EXCLUDED.secret, comment=EXCLUDED.comment,
           quota_names=EXCLUDED.quota_names, allowed_models=EXCLUDED.allowed_models,
           allowed_providers=EXCLUDED.allowed_providers,
           excluded_models=EXCLUDED.excluded_models,
           excluded_providers=EXCLUDED.excluded_providers,
           allow_raw_passthrough=EXCLUDED.allow_raw_passthrough,
           allowed_ips=EXCLUDED.allowed_ips, expires_at=EXCLUDED.expires_at,
           disabled_at=EXCLUDED.disabled_at, updated_at=EXCLUDED.updated_at",
    )
    .bind(name)
    .bind(&config.secret)
    .bind(&config.comment)
    .bind(json_list(&config.quota_names))
    .bind(json_list(&config.allowed_models))
    .bind(json_list(&config.allowed_providers))
    .bind(json_list(&config.excluded_models))
    .bind(json_list(&config.excluded_providers))
    .bind(config.allow_raw_passthrough)
    .bind(json_list(&config.allowed_ips))
    .bind(config.expires_at)
    .bind(config.disabled_at)
    .bind(now)
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

pub async fn load_providers(pool: &PgPool) -> Result<HashMap<String, ProviderConfig>, sqlx::Error> {
    let rows = sqlx::query("SELECT slug, display_name, api_base_url, api_key, oauth_provider_type, enabled, disable_cooldown, stall_cooldown, allow_100_percent_utilization, discount, estimate_tokens, use_claude_masking, gemini_thinking_enabled, auto_compat, adapter, model_autosync_enabled, model_autosync_interval, raw_passthrough FROM providers")
        .fetch_all(pool).await?;
    let mut result = HashMap::new();
    for row in rows {
        let base: serde_json::Value = row.try_get("api_base_url")?;
        let adapter: Option<serde_json::Value> = row.try_get("adapter")?;
        let raw: Option<serde_json::Value> = row.try_get("raw_passthrough")?;
        result.insert(row.try_get("slug")?, ProviderConfig {
            display_name: row.try_get("display_name")?, api_base_url: base.as_str().unwrap_or_default().to_string(),
            api_key: row.try_get("api_key")?, oauth_provider: row.try_get("oauth_provider_type")?, oauth_account: None,
            enabled: row.try_get("enabled")?, disable_cooldown: row.try_get("disable_cooldown")?, stall_cooldown: row.try_get("stall_cooldown")?,
            allow_100_percent_utilization: row.try_get("allow_100_percent_utilization")?, discount: row.try_get("discount")?, estimate_tokens: row.try_get("estimate_tokens")?,
            use_claude_masking: row.try_get("use_claude_masking")?, gemini_thinking_enabled: row.try_get("gemini_thinking_enabled")?, auto_compat: row.try_get("auto_compat")?,
            adapter: adapter.and_then(|v| serde_json::from_value::<Vec<AdapterEntry>>(v).ok()).unwrap_or_default(),
            model_autosync: crate::state::ModelAutosync { enabled: row.try_get("model_autosync_enabled")?, interval_minutes: row.try_get::<i32, _>("model_autosync_interval")? as u32 },
            raw_passthrough: raw.and_then(|v| serde_json::from_value::<RawPassthroughConfig>(v).ok()),
        });
    }
    Ok(result)
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
          (slug, display_name, api_base_url, api_key, oauth_provider_type, enabled,
           disable_cooldown, stall_cooldown, allow_100_percent_utilization,
           discount, estimate_tokens, use_claude_masking, gemini_thinking_enabled,
           auto_compat, adapter, model_autosync_enabled, model_autosync_interval,
           raw_passthrough, created_at, updated_at)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$19)
         ON CONFLICT (slug) DO UPDATE SET
           display_name=EXCLUDED.display_name, api_base_url=EXCLUDED.api_base_url,
           api_key=EXCLUDED.api_key, oauth_provider_type=EXCLUDED.oauth_provider_type,
           enabled=EXCLUDED.enabled, disable_cooldown=EXCLUDED.disable_cooldown,
           stall_cooldown=EXCLUDED.stall_cooldown,
           allow_100_percent_utilization=EXCLUDED.allow_100_percent_utilization,
           discount=EXCLUDED.discount, estimate_tokens=EXCLUDED.estimate_tokens,
           use_claude_masking=EXCLUDED.use_claude_masking,
           gemini_thinking_enabled=EXCLUDED.gemini_thinking_enabled,
           auto_compat=EXCLUDED.auto_compat, adapter=EXCLUDED.adapter,
           model_autosync_enabled=EXCLUDED.model_autosync_enabled,
           model_autosync_interval=EXCLUDED.model_autosync_interval,
           raw_passthrough=EXCLUDED.raw_passthrough, updated_at=EXCLUDED.updated_at",
    )
    .bind(slug)
    .bind(&config.display_name)
    .bind(serde_json::Value::String(config.api_base_url.clone()))
    .bind(&config.api_key)
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
    .bind(now)
    .execute(pool)
    .await
    .map(|_| ())
}

pub async fn delete_provider(pool: &PgPool, slug: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM providers WHERE slug = $1")
        .bind(slug)
        .execute(pool)
        .await
        .map(|_| ())
}

fn json_list_opt(value: &Option<Vec<String>>) -> Option<String> {
    value.as_ref().map(|v| serde_json::to_string(v).expect("string lists are serializable"))
}

fn parse_json_list_opt(value: Option<String>) -> Option<Vec<String>> {
    value.and_then(|raw| serde_json::from_str(&raw).ok())
}

/// Load all model aliases and their target groups from PostgreSQL.
pub async fn load_aliases(pool: &PgPool) -> Result<HashMap<String, ModelConfig>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, slug, priority::text, additional_aliases, use_image_fallthrough, enforce_limits,
                sticky_session, preferred_api, target_groups, advanced, pi_model,
                extra_body, compaction, model_type
           FROM model_aliases",
    )
    .fetch_all(pool)
    .await?;

    let mut result = HashMap::new();
    for row in rows {
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
                preferred_api: preferred_api
                    .and_then(|v| serde_json::from_value::<Vec<String>>(v).ok()),
                model_type: row.try_get("model_type")?,
                advanced,
                metadata: None,
                pi_model,
                extra_body,
                compaction,
            },
        );
    }
    Ok(result)
}

pub async fn upsert_alias(pool: &PgPool, slug: &str, config: &ModelConfig) -> Result<(), sqlx::Error> {
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
          (slug, priority, additional_aliases, use_image_fallthrough, enforce_limits,
           sticky_session, preferred_api, target_groups, advanced, pi_model, extra_body,
           compaction, model_type, created_at, updated_at)
         VALUES ($1,$2::alias_priority,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$14)
         ON CONFLICT (slug) DO UPDATE SET
           priority=EXCLUDED.priority, additional_aliases=EXCLUDED.additional_aliases,
           use_image_fallthrough=EXCLUDED.use_image_fallthrough,
           enforce_limits=EXCLUDED.enforce_limits, sticky_session=EXCLUDED.sticky_session,
           preferred_api=EXCLUDED.preferred_api, target_groups=EXCLUDED.target_groups,
           advanced=EXCLUDED.advanced, pi_model=EXCLUDED.pi_model,
           extra_body=EXCLUDED.extra_body, compaction=EXCLUDED.compaction,
           model_type=EXCLUDED.model_type, updated_at=EXCLUDED.updated_at
         RETURNING id",
    )
    .bind(slug)
    .bind(&config.priority)
    .bind(additional_aliases_json)
    .bind(config.use_image_fallthrough)
    .bind(config.enforce_limits)
    .bind(config.sticky_session)
    .bind(preferred_api_json)
    .bind(target_groups_json)
    .bind(&config.advanced)
    .bind(&config.pi_model)
    .bind(&config.extra_body)
    .bind(&config.compaction)
    .bind(&config.model_type)
    .bind(timestamp)
    .fetch_one(pool)
    .await?;

    let alias_id: i32 = row.try_get("id")?;
    sqlx::query("DELETE FROM model_alias_targets WHERE alias_id = $1")
        .bind(alias_id)
        .execute(pool)
        .await?;

    for group in &config.target_groups {
        for (idx, target) in group.targets.iter().enumerate() {
            sqlx::query(
                "INSERT INTO model_alias_targets
                  (alias_id, provider_slug, model_name, target_alias_slug, enabled,
                   group_name, sort_order)
                 VALUES ($1,$2,$3,$4,$5,$6,$7)",
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
pub async fn load_user_quotas(pool: &PgPool) -> Result<HashMap<String, QuotaDefinition>, sqlx::Error> {
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
                limit: row.try_get::<i32, _>("limit_value")? as f64,
                duration: row.try_get("duration")?,
                allowed_providers: parse_json_list_opt(row.try_get("allowed_providers")?),
                excluded_providers: parse_json_list_opt(row.try_get("excluded_providers")?),
                allowed_models: parse_json_list_opt(row.try_get("allowed_models")?),
                excluded_models: parse_json_list_opt(row.try_get("excluded_models")?),
                shared: row.try_get::<Option<bool>, _>("shared")?,
                warn_at: row.try_get::<Option<f32>, _>("warn_at")?.map(|v| v as f64),
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
    .bind(quota.limit as i32)
    .bind(&quota.duration)
    .bind(json_list_opt(&quota.allowed_models))
    .bind(json_list_opt(&quota.allowed_providers))
    .bind(json_list_opt(&quota.excluded_models))
    .bind(json_list_opt(&quota.excluded_providers))
    .bind(quota.shared.unwrap_or(false))
    .bind(quota.warn_at.map(|v| v as f32))
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
pub async fn load_settings(pool: &PgPool) -> Result<HashMap<String, serde_json::Value>, sqlx::Error> {
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
           expiry=EXCLUDED.expiry, consecutive_failures=EXCLUDED.consecutive_failures,
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

pub async fn delete_cooldowns_by_provider(pool: &PgPool, provider: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM provider_cooldowns WHERE provider = $1")
        .bind(provider)
        .execute(pool)
        .await
        .map(|_| ())
}

pub async fn delete_cooldown(pool: &PgPool, provider: &str, model: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM provider_cooldowns WHERE provider = $1 AND model = $2")
        .bind(provider)
        .bind(model)
        .execute(pool)
        .await
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::DEFAULT_MAX_CONNECTIONS;

    #[test]
    fn pool_default_is_bounded() {
        assert_eq!(DEFAULT_MAX_CONNECTIONS, 16);
    }
}
