//! `services/oauth/oauth-auth-manager.ts` (`getApiKey`, `getBaseUrl`) over
//! Trefoil's encrypted `oauth_credentials` table, plus the pi-ai per-provider
//! `refresh` / `toAuth` behavior the manager delegates to.
//!
//! Plexus caches credentials loaded at startup; Trefoil reads the table on
//! each lookup (it is also written by the login sessions and the background
//! refresh scheduler), so the observable state is the same.
use crate::state::AppState;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

const LEGACY_ACCOUNT_ID: &str = "legacy";
const DEFAULT_OAUTH_EXPIRY_BUFFER_MS: i64 = 60 * 1000;
const REFRESH_RETRY_BACKOFF_INITIAL_MS: i64 = 60 * 1000;
const REFRESH_RETRY_BACKOFF_MAX_MS: i64 = 15 * 60 * 1000;

#[derive(Debug, Clone)]
struct Stored { access: String, refresh: String, expires: i64 }

#[derive(Default)]
struct ManagerState {
    backoffs: HashMap<String, (u32, i64)>,
    gates: HashMap<String, Arc<tokio::sync::Mutex<()>>>,
}

fn manager() -> &'static Mutex<ManagerState> {
    static M: OnceLock<Mutex<ManagerState>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(ManagerState::default()))
}

fn now_ms() -> i64 { chrono::Utc::now().timestamp_millis() }

/// Every `{account: credential}` stored for `provider` (row order kept).
async fn load_provider(state: &AppState, provider: &str) -> Vec<(String, Stored)> {
    let Some(pool) = state.inner.database.as_ref() else { return Vec::new() };
    let Ok(rows) = crate::database::load_oauth_credentials(pool).await else { return Vec::new() };
    rows.into_iter().filter(|(p, ..)| p == provider)
        .map(|(_, account, access, refresh, expires)| (account, Stored { access, refresh, expires })).collect()
}

/// `resolveAccountId(provider, accountId)`.
fn resolve_account_id(accounts: &[(String, Stored)], account_id: Option<&str>) -> Option<String> {
    if let Some(trimmed) = account_id.map(str::trim).filter(|a| !a.is_empty()) { return Some(trimmed.to_owned()); }
    if accounts.is_empty() { return None; }
    if accounts.iter().any(|(a, _)| a == LEGACY_ACCOUNT_ID) { return Some(LEGACY_ACCOUNT_ID.into()); }
    (accounts.len() == 1).then(|| accounts[0].0.clone())
}

/// pi-ai `oauth.refresh(credential)` per provider: `(access, storedRefresh, expires)`.
async fn refresh_provider(provider: &str, stored_refresh: &str) -> Result<(String, String, i64), String> {
    match provider {
        "github-copilot" => crate::copilot_oauth::refresh_stored(stored_refresh).await,
        "kimi-coding" => crate::kimi_oauth::refresh(stored_refresh).await,
        "meta" => crate::meta_oauth::mint(stored_refresh).await,
        "openai" => crate::openai_chatgpt_oauth::refresh(stored_refresh).await,
        "xai" => crate::xai_oauth::refresh(stored_refresh).await,
        _ => {
            let token = crate::oauth_transport::refresh(&crate::oauth_transport::ReqwestTransport, provider, stored_refresh).await
                .map_err(|e| format!("{e:?}"))?;
            let parsed = crate::oauth_refresh::parse_refresh_response(
                &serde_json::json!({"access_token": token.access_token, "refresh_token": token.refresh_token, "expires_in": token.expires_in}),
                stored_refresh, now_ms(),
            ).map_err(str::to_owned)?;
            Ok((parsed.access_token, parsed.refresh_token, parsed.expires_at_ms))
        }
    }
}

async fn refresh_credentials(state: &AppState, provider: &str, account: &str, current: &Stored, key: &str) -> Result<Stored, String> {
    match refresh_provider(provider, &current.refresh).await {
        Ok((access, refresh, expires)) if !access.is_empty() => {
            let refreshed = Stored { access, refresh: if refresh.is_empty() { current.refresh.clone() } else { refresh }, expires };
            manager().lock().unwrap().backoffs.remove(key);
            if let Some(pool) = state.inner.database.as_ref() {
                let _ = crate::database::upsert_oauth_credential(pool, provider, account, &refreshed.access, &refreshed.refresh, refreshed.expires).await;
            }
            Ok(refreshed)
        }
        other => {
            let error = match other { Err(e) => e, Ok(_) => format!("OAuth: refresh returned incomplete credentials for {provider}/{account}") };
            let mut m = manager().lock().unwrap();
            let failures = m.backoffs.get(key).map_or(0, |b| b.0) + 1;
            let delay = (REFRESH_RETRY_BACKOFF_INITIAL_MS * 2i64.pow(failures - 1)).min(REFRESH_RETRY_BACKOFF_MAX_MS);
            m.backoffs.insert(key.to_owned(), (failures, now_ms() + delay));
            Err(error)
        }
    }
}

/// `OAuthAuthManager.getApiKey(provider, accountId, {forceRefresh})`.
pub async fn get_api_key(state: &AppState, provider: &str, account_id: Option<&str>, force_refresh: bool) -> Result<String, String> {
    let accounts = load_provider(state, provider).await;
    if accounts.is_empty() {
        return Err(format!("OAuth: Not authenticated for provider '{provider}'. Please run OAuth login for this provider."));
    }
    let Some(account) = resolve_account_id(&accounts, account_id) else {
        return Err(format!("OAuth: accountId is required to resolve credentials for provider '{provider}'."));
    };
    let Some(mut current) = accounts.iter().find(|(a, _)| *a == account).map(|(_, c)| c.clone()) else {
        return Err(format!("OAuth: Not authenticated for provider '{provider}' and account '{account}'. Please run OAuth login for this account."));
    };
    if crate::oauth_login::builtin_provider(provider).is_none() {
        return Err(format!("OAuth: provider '{provider}' does not support OAuth login."));
    }
    let key = format!("{provider}/{account}");
    let now = now_ms();
    if force_refresh || current.expires - DEFAULT_OAUTH_EXPIRY_BUFFER_MS <= now {
        let backoff = manager().lock().unwrap().backoffs.get(&key).copied();
        match backoff {
            Some((_, retry_at)) if retry_at > now => {
                if force_refresh || current.expires <= now {
                    let at = chrono::DateTime::from_timestamp_millis(retry_at).unwrap_or_default().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
                    return Err(format!("OAuth: refresh for {provider}/{account} is backed off until {at} after a previous failure."));
                }
            }
            _ => {
                let gate = manager().lock().unwrap().gates.entry(key.clone()).or_default().clone();
                let _guard = gate.lock().await;
                match refresh_credentials(state, provider, &account, &current, &key).await {
                    Ok(refreshed) => current = refreshed,
                    Err(error) => if force_refresh || current.expires <= now_ms() { return Err(error); },
                }
            }
        }
    }
    // pi-ai `toAuth`: every provider but kimi-coding exposes `apiKey: access`.
    if provider == "kimi-coding" || current.access.is_empty() {
        return Err(format!("OAuth: could not derive an API key for provider '{provider}' and account '{account}'."));
    }
    Ok(current.access)
}

/// `OAuthAuthManager.getBaseUrl(provider, accountId)` (only Copilot's `toAuth`
/// reports one: the token's `proxy-ep`, else the enterprise domain).
pub async fn get_base_url(state: &AppState, provider: &str, account_id: Option<&str>) -> Option<String> {
    let accounts = load_provider(state, provider).await;
    let account = resolve_account_id(&accounts, account_id)?;
    let current = accounts.into_iter().find(|(a, _)| *a == account)?.1;
    if provider != "github-copilot" { return None; }
    let enterprise = current.refresh.split_once('\n').map(|(d, _)| d.to_owned()).filter(|d| !d.is_empty());
    Some(copilot_base_url(&current.access, enterprise.as_deref()))
}

/// pi-ai `getGitHubCopilotBaseUrl(token, enterpriseDomain)`.
pub fn copilot_base_url(token: &str, enterprise_domain: Option<&str>) -> String {
    if let Some(url) = base_url_from_token(token) { return url; }
    match enterprise_domain { Some(d) => format!("https://copilot-api.{d}"), None => "https://api.individual.githubcopilot.com".into() }
}

/// `proxy-ep=<host>` → `https://<host with proxy. → api.>`.
pub fn base_url_from_token(token: &str) -> Option<String> {
    let start = token.find("proxy-ep=")? + "proxy-ep=".len();
    let host: String = token[start..].chars().take_while(|c| *c != ';').collect();
    if host.is_empty() { return None; }
    Some(format!("https://{}", host.strip_prefix("proxy.").map(|h| format!("api.{h}")).unwrap_or(host)))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn account_resolution_and_copilot_urls() {
        let s = |a: &str| (a.to_owned(), Stored { access: String::new(), refresh: String::new(), expires: 0 });
        assert_eq!(resolve_account_id(&[s("a"), s("legacy")], None).as_deref(), Some("legacy"));
        assert_eq!(resolve_account_id(&[s("a")], Some(" b ")).as_deref(), Some("b"));
        assert_eq!(resolve_account_id(&[s("a"), s("b")], None), None);
        assert_eq!(copilot_base_url("tid=1;proxy-ep=proxy.enterprise.githubcopilot.com;x", None), "https://api.enterprise.githubcopilot.com");
        assert_eq!(copilot_base_url("tid=1", Some("corp.example")), "https://copilot-api.corp.example");
        assert_eq!(copilot_base_url("tid=1", None), "https://api.individual.githubcopilot.com");
    }
}
