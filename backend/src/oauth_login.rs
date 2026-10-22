//! OAuth provider login sessions — a port of pinned Plexus
//! `services/oauth/oauth-login-session.ts` (`OAuthLoginSessionManager`) plus
//! the pi-ai flow primitives the provider logins share (`device-code.js`,
//! `callback-server.js`, `pkce.js`).
//!
//! Sessions are in-memory only (Plexus never persists them), identified by a
//! random UUID, and expire 20 minutes after creation. Each session runs its
//! provider's `login(interaction)` in a background task. The interaction lets
//! the flow `notify` (auth URL, device code, progress) and `prompt` (text,
//! manual code, select); prompts surface on the session as
//! `awaiting_prompt` / `awaiting_manual_code` / `awaiting_select` and are
//! answered through the management routes. On success the credentials are
//! persisted through a [`CredentialStore`] before the session reports
//! `success`, exactly like `OAuthAuthManager.setCredentials`.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::RngCore;
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::sync::{oneshot, watch};

pub const DEFAULT_SESSION_TTL_MS: i64 = 20 * 60 * 1000;
const MAX_PROGRESS: usize = 100;

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// `crypto.randomUUID()` (RFC 4122 version 4).
pub fn random_uuid() -> String {
    let mut b = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let x: String = b.iter().map(|v| format!("{v:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &x[0..8],
        &x[8..12],
        &x[12..16],
        &x[16..20],
        &x[20..32]
    )
}

pub fn random_bytes(len: usize) -> Vec<u8> {
    let mut b = vec![0u8; len];
    rand::thread_rng().fill_bytes(&mut b);
    b
}

/// pi-ai `generatePKCE`: 32 random bytes, base64url verifier, S256 challenge.
pub fn generate_pkce() -> (String, String) {
    let verifier = URL_SAFE_NO_PAD.encode(random_bytes(32));
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

/// `new URLSearchParams(pairs).toString()` (application/x-www-form-urlencoded).
pub fn search_params(pairs: &[(&str, &str)]) -> String {
    let mut s = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in pairs {
        s.append_pair(k, v);
    }
    s.finish()
}

/// `new URLSearchParams(text).get(key)` — a single leading `?` is ignored.
pub fn search_param(text: &str, key: &str) -> Option<String> {
    let text = text.strip_prefix('?').unwrap_or(text);
    url::form_urlencoded::parse(text.as_bytes())
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
}

/// `new URL(value).searchParams.get(key)`.
pub fn url_param(url: &url::Url, key: &str) -> Option<String> {
    url.query_pairs()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
}

/// `JSON.stringify(value)` for a parsed body (`null` when absent).
pub fn js_stringify(value: Option<&serde_json::Value>) -> String {
    match value {
        Some(v) => serde_json::to_string(v).unwrap_or_else(|_| "null".into()),
        None => "null".into(),
    }
}

/// pi-ai `getProviderEnvValue("PI_OAUTH_CALLBACK_HOST") || "127.0.0.1"`.
pub fn callback_host() -> String {
    std::env::var("PI_OAUTH_CALLBACK_HOST")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "127.0.0.1".into())
}

/// Status text a `fetch` Response would expose (canonical reason phrase).
pub fn status_text(status: reqwest::StatusCode) -> &'static str {
    status.canonical_reason().unwrap_or("")
}

// ─── Credentials ────────────────────────────────────────────────────────────

/// pi-ai `OAuthCredentials`. Provider-specific extras that Plexus keeps on the
/// credential object are carried as typed fields; only `enterprise_url` and
/// `client_id` affect persistence (see [`OAuthCredentials::stored_refresh`]).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OAuthCredentials {
    pub access: String,
    pub refresh: String,
    pub expires: i64,
    /// GitHub Copilot Enterprise domain (`enterpriseUrl`).
    pub enterprise_url: Option<String>,
    /// OpenAI ChatGPT dynamically issued client id (`clientId`).
    pub client_id: Option<String>,
    /// OpenAI ChatGPT granted scopes (`scopes`, memory only in Plexus).
    pub scopes: Vec<String>,
    /// OpenAI Codex ChatGPT account id (`accountId`, re-derivable from the JWT).
    pub account_id: Option<String>,
    /// GitHub Copilot models available to the account (memory only in Plexus).
    pub available_model_ids: Vec<String>,
}

impl OAuthCredentials {
    pub fn new(access: impl Into<String>, refresh: impl Into<String>, expires: i64) -> Self {
        Self {
            access: access.into(),
            refresh: refresh.into(),
            expires,
            ..Default::default()
        }
    }

    /// The value written to `oauth_credentials.refresh_token`. Trefoil has no
    /// `enterprise_url`/`client_id` columns, so the extras that refresh needs
    /// are prefixed to the encrypted refresh token as `<extra>\n<refresh>`
    /// (OpenAI ChatGPT: issued client id; Copilot: enterprise domain). Plain
    /// refresh tokens never contain a newline.
    pub fn stored_refresh(&self) -> String {
        if let Some(client) = self.client_id.as_deref().filter(|v| !v.is_empty()) {
            return format!("{client}\n{}", self.refresh);
        }
        if let Some(domain) = self.enterprise_url.as_deref().filter(|v| !v.is_empty()) {
            return format!("{domain}\n{}", self.refresh);
        }
        self.refresh.clone()
    }
}

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// `OAuthAuthManager.setCredentials`: must durably persist before success.
pub type CredentialStore =
    Arc<dyn Fn(String, String, OAuthCredentials) -> BoxFuture<Result<(), String>> + Send + Sync>;

/// Persist into Trefoil's encrypted `oauth_credentials` table.
pub fn database_store(state: crate::AppState) -> CredentialStore {
    Arc::new(
        move |provider: String, account: String, credentials: OAuthCredentials| {
            let state = state.clone();
            Box::pin(async move {
                if account.trim().is_empty() {
                    return Err("OAuth: accountId is required to store credentials".to_string());
                }
                let Some(pool) = state.inner.database.as_ref() else {
                    return Err("PostgreSQL is required to persist OAuth credentials".to_string());
                };
                crate::database::upsert_oauth_credential(
                    pool,
                    &provider,
                    &account,
                    &credentials.access,
                    &credentials.stored_refresh(),
                    credentials.expires,
                )
                .await
                .map_err(|error| match error {
                    sqlx::Error::Protocol(message) => message,
                    other => other.to_string(),
                })?;
                let mut accounts = state.inner.oauth_accounts.write().unwrap();
                if !accounts
                    .iter()
                    .any(|(p, a)| p == &provider && a == &account)
                {
                    accounts.push((provider, account));
                }
                Ok(())
            }) as BoxFuture<Result<(), String>>
        },
    )
}

// ─── Provider descriptors ───────────────────────────────────────────────────

pub type LoginFn =
    Arc<dyn Fn(Interaction) -> BoxFuture<Result<OAuthCredentials, String>> + Send + Sync>;

#[derive(Clone)]
pub struct OAuthProviderDescriptor {
    pub id: String,
    pub name: String,
    pub uses_callback_server: bool,
    pub login: LoginFn,
}

pub type ProviderResolver = Arc<dyn Fn(&str) -> Option<OAuthProviderDescriptor> + Send + Sync>;

/// Pinned Plexus `listOAuthProviders()` order: the custom (anthropic copy-code)
/// entry first, then pi-ai built-ins with `auth.oauth`, minus `radius`.
pub const PROVIDERS: &[(&str, &str, bool)] = &[
    ("anthropic", "Anthropic (Claude Pro/Max)", false),
    ("github-copilot", "GitHub Copilot", false),
    ("kimi-coding", "Kimi Code (subscription)", false),
    ("meta", "Meta (Muse subscription)", false),
    ("openai", "OpenAI (ChatGPT subscription)", false),
    ("openai-codex", "OpenAI (ChatGPT Plus/Pro)", true),
    ("openrouter", "OpenRouter OAuth", false),
    ("xai", "xAI (Grok/X subscription)", false),
];

fn login_fn<F, Fut>(f: F) -> LoginFn
where
    F: Fn(Interaction) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<OAuthCredentials, String>> + Send + 'static,
{
    Arc::new(move |interaction| Box::pin(f(interaction)))
}

/// `getOAuthProviderAuth(providerId)` for the real provider flows.
pub fn builtin_provider(id: &str) -> Option<OAuthProviderDescriptor> {
    let (id, name, uses_callback_server) = PROVIDERS.iter().find(|(pid, _, _)| *pid == id)?;
    let login: LoginFn = match *id {
        "anthropic" => login_fn(|i| {
            crate::anthropic_oauth::login(i, crate::anthropic_oauth::Endpoints::default())
        }),
        "github-copilot" => {
            login_fn(|i| crate::copilot_oauth::login(i, crate::copilot_oauth::Endpoints::default()))
        }
        "kimi-coding" => {
            login_fn(|i| crate::kimi_oauth::login(i, crate::kimi_oauth::Endpoints::default()))
        }
        "meta" => {
            login_fn(|i| crate::meta_oauth::login(i, crate::meta_oauth::Endpoints::default()))
        }
        "openai" => login_fn(|i| {
            crate::openai_chatgpt_oauth::login(i, crate::openai_chatgpt_oauth::Endpoints::default())
        }),
        "openai-codex" => {
            login_fn(|i| crate::codex_oauth::login(i, crate::codex_oauth::Endpoints::default()))
        }
        "openrouter" => login_fn(|i| {
            crate::openrouter_oauth::login(i, crate::openrouter_oauth::Endpoints::default())
        }),
        "xai" => login_fn(|i| crate::xai_oauth::login(i, crate::xai_oauth::Endpoints::default())),
        _ => return None,
    };
    Some(OAuthProviderDescriptor {
        id: id.to_string(),
        name: name.to_string(),
        uses_callback_server: *uses_callback_server,
        login,
    })
}

// ─── Session shape ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OAuthSessionStatus {
    InProgress,
    AwaitingAuth,
    AwaitingPrompt,
    AwaitingManualCode,
    AwaitingSelect,
    Success,
    Error,
    Cancelled,
}

impl OAuthSessionStatus {
    pub fn is_active(self) -> bool {
        matches!(
            self,
            Self::InProgress
                | Self::AwaitingAuth
                | Self::AwaitingPrompt
                | Self::AwaitingManualCode
                | Self::AwaitingSelect
        )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OAuthAuthInfo {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthPrompt {
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    pub allow_empty: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OAuthSelectOption {
    pub id: String,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OAuthSelect {
    pub message: String,
    pub options: Vec<OAuthSelectOption>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OAuthManualCode {
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
}

/// The public session shape (`stripInternal`). Field order and omission of
/// undefined members match the Plexus JSON.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthSession {
    pub id: String,
    pub provider_id: String,
    pub account_id: String,
    pub status: OAuthSessionStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_info: Option<OAuthAuthInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<OAuthPrompt>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub select: Option<OAuthSelect>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manual_code: Option<OAuthManualCode>,
    pub progress: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

// ─── Interaction events ─────────────────────────────────────────────────────

/// pi-ai `ProviderAuthInteraction.notify` events.
#[derive(Debug, Clone)]
pub enum AuthEvent {
    AuthUrl {
        url: String,
        instructions: Option<String>,
    },
    DeviceCode {
        user_code: String,
        verification_uri: String,
    },
    Progress(String),
    Info(String),
}

/// pi-ai `ProviderAuthInteraction.prompt` requests.
#[derive(Debug, Clone)]
pub enum AuthPrompt {
    Text {
        message: String,
        placeholder: Option<String>,
    },
    ManualCode {
        message: String,
        placeholder: Option<String>,
    },
    Select {
        message: String,
        options: Vec<OAuthSelectOption>,
    },
}

type Reply = oneshot::Sender<Result<String, String>>;

struct SessionInternal {
    public: OAuthSession,
    resolve_prompt: Option<Reply>,
    resolve_manual_code: Option<Reply>,
    resolve_select: Option<Reply>,
    abort: watch::Sender<bool>,
    completion: watch::Receiver<bool>,
    expires_at: i64,
    saving_credentials: bool,
}

impl SessionInternal {
    fn touch(&mut self) {
        self.public.updated_at = now_ms();
    }

    /// `releaseSession`: abort the login and reject any pending prompt so
    /// callback servers close (their ports are released when the login
    /// future is dropped).
    fn release(&mut self, reason: &str) {
        let _ = self.abort.send(true);
        for reply in [
            self.resolve_prompt.take(),
            self.resolve_manual_code.take(),
            self.resolve_select.take(),
        ]
        .into_iter()
        .flatten()
        {
            let _ = reply.send(Err(reason.to_string()));
        }
    }
}

struct ManagerInner {
    sessions: Mutex<HashMap<String, SessionInternal>>,
    resolver: RwLock<ProviderResolver>,
    cleanup_started: std::sync::atomic::AtomicBool,
}

/// `OAuthLoginSessionManager`. Cheap to clone; clones share sessions.
#[derive(Clone)]
pub struct OAuthLoginSessionManager {
    inner: Arc<ManagerInner>,
}

impl std::fmt::Debug for OAuthLoginSessionManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthLoginSessionManager")
            .field(
                "sessions",
                &self.inner.sessions.lock().map(|s| s.len()).unwrap_or(0),
            )
            .finish()
    }
}

impl Default for OAuthLoginSessionManager {
    fn default() -> Self {
        Self::new(Arc::new(builtin_provider))
    }
}

/// Interaction handed to a provider `login`. Prompts register on the session
/// synchronously when `prompt` is called, then the returned future waits.
#[derive(Clone)]
pub struct Interaction {
    inner: Arc<ManagerInner>,
    session_id: String,
    abort: watch::Receiver<bool>,
}

impl Interaction {
    pub fn is_aborted(&self) -> bool {
        *self.abort.borrow()
    }

    /// Resolves once the session's abort signal fires.
    pub async fn cancelled(&self) {
        let mut rx = self.abort.clone();
        let _ = rx.wait_for(|aborted| *aborted).await;
    }

    fn with_session<R>(&self, f: impl FnOnce(&mut SessionInternal) -> R) -> Option<R> {
        let mut sessions = self.inner.sessions.lock().unwrap();
        sessions.get_mut(&self.session_id).map(f)
    }

    pub fn notify(&self, event: AuthEvent) {
        self.with_session(|session| {
            match event {
                AuthEvent::AuthUrl { url, instructions } => {
                    session.public.auth_info = Some(OAuthAuthInfo { url, instructions });
                    session.public.status = OAuthSessionStatus::AwaitingAuth;
                }
                AuthEvent::DeviceCode {
                    user_code,
                    verification_uri,
                } => {
                    session.public.auth_info = Some(OAuthAuthInfo {
                        url: verification_uri,
                        instructions: Some(format!("Enter code: {user_code}")),
                    });
                    session.public.status = OAuthSessionStatus::AwaitingAuth;
                }
                AuthEvent::Progress(message) | AuthEvent::Info(message) => {
                    session.public.progress.push(message);
                    if session.public.progress.len() > MAX_PROGRESS {
                        session.public.progress.remove(0);
                    }
                }
            }
            session.touch();
        });
    }

    /// Register a prompt on the session now; the future yields the answer or
    /// the rejection reason (cancel/expiry).
    pub fn prompt(
        &self,
        prompt: AuthPrompt,
    ) -> impl Future<Output = Result<String, String>> + Send + 'static {
        let (tx, rx) = oneshot::channel();
        let registered: Result<(), String> = match prompt {
            AuthPrompt::Select { message, options } => {
                if options.is_empty() {
                    Err("Login cancelled: no options to select".to_string())
                } else {
                    self.with_session(|session| {
                        session.public.select = Some(OAuthSelect { message, options });
                        session.public.status = OAuthSessionStatus::AwaitingSelect;
                        session.touch();
                        session.resolve_select = Some(tx);
                    })
                    .ok_or_else(|| "OAuth login session expired".to_string())
                }
            }
            AuthPrompt::Text {
                message,
                placeholder,
            } => self.await_user_input(false, message, placeholder, tx),
            AuthPrompt::ManualCode {
                message,
                placeholder,
            } => self.await_user_input(true, message, placeholder, tx),
        };
        async move {
            registered?;
            rx.await
                .unwrap_or_else(|_| Err("OAuth login session expired".to_string()))
        }
    }

    /// Plexus `awaitUserInput`: manual-code entry renders as the dedicated
    /// manual-code input (no generic prompt); other prompts allow empty input.
    fn await_user_input(
        &self,
        manual: bool,
        message: String,
        placeholder: Option<String>,
        tx: Reply,
    ) -> Result<(), String> {
        self.with_session(|session| {
            session.public.prompt = if manual {
                None
            } else {
                Some(OAuthPrompt {
                    message: message.clone(),
                    placeholder: placeholder.clone(),
                    allow_empty: true,
                })
            };
            session.public.manual_code = if manual {
                Some(OAuthManualCode {
                    message,
                    placeholder,
                })
            } else {
                None
            };
            session.public.status = if manual {
                OAuthSessionStatus::AwaitingManualCode
            } else {
                OAuthSessionStatus::AwaitingPrompt
            };
            session.touch();
            if manual {
                session.resolve_manual_code = Some(tx);
            } else {
                session.resolve_prompt = Some(tx);
            }
        })
        .ok_or_else(|| "OAuth login session expired".to_string())
    }
}

impl OAuthLoginSessionManager {
    pub fn new(resolver: ProviderResolver) -> Self {
        Self {
            inner: Arc::new(ManagerInner {
                sessions: Mutex::new(HashMap::new()),
                resolver: RwLock::new(resolver),
                cleanup_started: std::sync::atomic::AtomicBool::new(false),
            }),
        }
    }

    /// Swap the provider resolver (tests inject mock flows).
    pub fn set_resolver(&self, resolver: ProviderResolver) {
        *self.inner.resolver.write().unwrap() = resolver;
    }

    /// `listProviders()` — descriptor metadata only.
    pub fn list_providers(&self) -> Vec<(String, String, bool)> {
        PROVIDERS
            .iter()
            .map(|(id, name, cb)| (id.to_string(), name.to_string(), *cb))
            .collect()
    }

    pub fn get_session(&self, session_id: &str) -> Option<OAuthSession> {
        self.cleanup_expired();
        self.inner
            .sessions
            .lock()
            .unwrap()
            .get(session_id)
            .map(|s| s.public.clone())
    }

    /// `createSession`. Callback-server providers supersede an in-flight
    /// login for the same provider and wait for it to settle (port release).
    pub async fn create_session(
        &self,
        provider_id: &str,
        account_id: &str,
        store: CredentialStore,
    ) -> Result<OAuthSession, String> {
        self.cleanup_expired();
        self.start_cleanup();
        if account_id.trim().is_empty() {
            return Err("accountId is required".into());
        }
        let resolver = self.inner.resolver.read().unwrap().clone();
        let Some(provider) = resolver(provider_id) else {
            return Err(format!("Unknown OAuth provider: {provider_id}"));
        };

        if provider.uses_callback_server {
            let completion = {
                let mut sessions = self.inner.sessions.lock().unwrap();
                let active = sessions
                    .values_mut()
                    .filter(|s| s.public.provider_id == provider_id && s.public.status.is_active())
                    .min_by_key(|s| s.public.created_at);
                active.map(|active| {
                    if !active.saving_credentials {
                        active.public.status = OAuthSessionStatus::Cancelled;
                        active.public.error = Some("Superseded by a new login".into());
                        active.release("Superseded by a new login");
                        active.touch();
                    }
                    active.completion.clone()
                })
            };
            if let Some(mut completion) = completion {
                let _ = completion.wait_for(|done| *done).await;
            }
        }

        let now = now_ms();
        let id = random_uuid();
        let (abort_tx, abort_rx) = watch::channel(false);
        let (done_tx, done_rx) = watch::channel(false);
        let public = OAuthSession {
            id: id.clone(),
            provider_id: provider_id.to_string(),
            account_id: account_id.to_string(),
            status: OAuthSessionStatus::InProgress,
            auth_info: None,
            prompt: None,
            select: None,
            manual_code: None,
            progress: vec![],
            error: None,
            created_at: now,
            updated_at: now,
        };
        self.inner.sessions.lock().unwrap().insert(
            id.clone(),
            SessionInternal {
                public: public.clone(),
                resolve_prompt: None,
                resolve_manual_code: None,
                resolve_select: None,
                abort: abort_tx,
                completion: done_rx,
                expires_at: now + DEFAULT_SESSION_TTL_MS,
                saving_credentials: false,
            },
        );
        let interaction = Interaction {
            inner: self.inner.clone(),
            session_id: id.clone(),
            abort: abort_rx,
        };
        let inner = self.inner.clone();
        tokio::spawn(async move {
            run_login(inner, provider, interaction, store).await;
            let _ = done_tx.send(true);
        });
        Ok(public)
    }

    pub async fn submit_prompt(
        &self,
        session_id: &str,
        value: String,
    ) -> Result<OAuthSession, String> {
        self.with_internal(session_id, |session| {
            let reply = session
                .resolve_prompt
                .take()
                .ok_or("No prompt is awaiting input")?;
            let _ = reply.send(Ok(value));
            session.public.prompt = None;
            if session.public.status == OAuthSessionStatus::AwaitingPrompt {
                session.public.status = OAuthSessionStatus::InProgress;
            }
            session.touch();
            Ok(session.public.clone())
        })
    }

    pub async fn submit_manual_code(
        &self,
        session_id: &str,
        value: String,
    ) -> Result<OAuthSession, String> {
        self.with_internal(session_id, |session| {
            let reply = session
                .resolve_manual_code
                .take()
                .ok_or("No manual code input is awaiting input")?;
            let _ = reply.send(Ok(value));
            session.public.manual_code = None;
            if session.public.status == OAuthSessionStatus::AwaitingManualCode {
                session.public.status = OAuthSessionStatus::InProgress;
            }
            session.touch();
            Ok(session.public.clone())
        })
    }

    pub async fn submit_select(
        &self,
        session_id: &str,
        value: String,
    ) -> Result<OAuthSession, String> {
        self.with_internal(session_id, |session| {
            if session.resolve_select.is_none() {
                return Err("No selection is awaiting input".into());
            }
            if !session
                .public
                .select
                .as_ref()
                .is_some_and(|s| s.options.iter().any(|o| o.id == value))
            {
                return Err(format!("Unknown option: {value}"));
            }
            let _ = session.resolve_select.take().unwrap().send(Ok(value));
            session.public.select = None;
            if session.public.status == OAuthSessionStatus::AwaitingSelect {
                session.public.status = OAuthSessionStatus::InProgress;
            }
            session.touch();
            Ok(session.public.clone())
        })
    }

    pub async fn cancel(&self, session_id: &str) -> Result<OAuthSession, String> {
        enum Next {
            Done(OAuthSession),
            Wait(watch::Receiver<bool>),
        }
        let next = self.with_internal(session_id, |session| {
            if matches!(
                session.public.status,
                OAuthSessionStatus::Success | OAuthSessionStatus::Error
            ) {
                return Ok(Next::Done(session.public.clone()));
            }
            if session.saving_credentials {
                // Persistence cannot be cancelled; report the committed outcome.
                return Ok(Next::Wait(session.completion.clone()));
            }
            session.public.status = OAuthSessionStatus::Cancelled;
            session.public.error = Some("Login cancelled".into());
            session.release("Login cancelled");
            session.touch();
            Ok(Next::Done(session.public.clone()))
        })?;
        match next {
            Next::Done(session) => Ok(session),
            Next::Wait(mut completion) => {
                let _ = completion.wait_for(|done| *done).await;
                self.inner
                    .sessions
                    .lock()
                    .unwrap()
                    .get(session_id)
                    .map(|s| s.public.clone())
                    .ok_or_else(|| "OAuth session not found".into())
            }
        }
    }

    /// Release every session (Plexus `dispose`).
    pub fn dispose(&self) {
        let mut sessions = self.inner.sessions.lock().unwrap();
        for session in sessions.values_mut() {
            session.release("Login manager disposed");
        }
        sessions.clear();
    }

    fn with_internal<R>(
        &self,
        session_id: &str,
        f: impl FnOnce(&mut SessionInternal) -> Result<R, String>,
    ) -> Result<R, String> {
        self.cleanup_expired();
        let mut sessions = self.inner.sessions.lock().unwrap();
        let session = sessions
            .get_mut(session_id)
            .ok_or("OAuth session not found")?;
        f(session)
    }

    fn start_cleanup(&self) {
        if self
            .inner
            .cleanup_started
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }
        let weak = Arc::downgrade(&self.inner);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
                let Some(inner) = weak.upgrade() else { break };
                cleanup(&inner);
            }
        });
    }

    fn cleanup_expired(&self) {
        cleanup(&self.inner);
    }

    #[cfg(test)]
    pub(crate) fn expire_now(&self, session_id: &str) {
        if let Some(session) = self.inner.sessions.lock().unwrap().get_mut(session_id) {
            session.expires_at = now_ms() - 1;
        }
    }
}

fn cleanup(inner: &ManagerInner) {
    let now = now_ms();
    let mut sessions = inner.sessions.lock().unwrap();
    sessions.retain(|_, session| {
        if session.expires_at <= now {
            session.release("OAuth login session expired");
            false
        } else {
            true
        }
    });
}

async fn run_login(
    inner: Arc<ManagerInner>,
    provider: OAuthProviderDescriptor,
    interaction: Interaction,
    store: CredentialStore,
) {
    let id = interaction.session_id.clone();
    let abort = interaction.clone();
    let login = (provider.login)(interaction);
    // Aborting drops the login future, which closes any callback server.
    let result = tokio::select! {
        result = login => result,
        _ = abort.cancelled() => Err("Login cancelled".to_string()),
    };
    let set_error = |message: String| {
        if let Some(session) = inner.sessions.lock().unwrap().get_mut(&id) {
            if session.public.status != OAuthSessionStatus::Cancelled {
                session.public.status = OAuthSessionStatus::Error;
                session.public.error = Some(message);
                session.public.select = None;
                session.public.manual_code = None;
                session.touch();
            }
        }
    };
    let credentials = match result {
        Ok(credentials) => credentials,
        Err(message) => return set_error(message),
    };
    let (provider_id, account_id) = {
        let mut sessions = inner.sessions.lock().unwrap();
        let Some(session) = sessions.get_mut(&id) else {
            return;
        };
        if session.public.status == OAuthSessionStatus::Cancelled {
            return;
        }
        session.saving_credentials = true;
        (
            session.public.provider_id.clone(),
            session.public.account_id.clone(),
        )
    };
    let saved = store(provider_id, account_id, credentials).await;
    if let Some(session) = inner.sessions.lock().unwrap().get_mut(&id) {
        session.saving_credentials = false;
    }
    match saved {
        Ok(()) => {
            if let Some(session) = inner.sessions.lock().unwrap().get_mut(&id) {
                session.public.status = OAuthSessionStatus::Success;
                session.public.error = None;
                session.public.prompt = None;
                session.public.select = None;
                session.public.manual_code = None;
                session.resolve_prompt = None;
                session.resolve_manual_code = None;
                session.resolve_select = None;
                session.touch();
            }
        }
        Err(message) => set_error(message),
    }
}

// ─── pi-ai device-code.js ───────────────────────────────────────────────────

pub enum DevicePoll<T> {
    Complete(T),
    Pending,
    SlowDown(Option<f64>),
    Failed(String),
}

const DEVICE_MIN_INTERVAL_MS: i64 = 1000;
const DEVICE_SLOW_DOWN_INCREMENT_MS: i64 = 5000;
const DEVICE_TIMEOUT_MESSAGE: &str = "Device flow timed out";
const DEVICE_SLOW_DOWN_TIMEOUT_MESSAGE: &str = "Device flow timed out after one or more slow_down responses. This is often caused by clock drift in WSL or VM environments. Please sync or restart the VM clock and try again.";

/// `pollOAuthDeviceCodeFlow`. Cancellation is handled by the session task
/// dropping this future.
pub async fn poll_device_code_flow<T, F, Fut>(
    interval_seconds: Option<f64>,
    expires_in_seconds: Option<f64>,
    wait_before_first_poll: bool,
    mut poll: F,
) -> Result<T, String>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<DevicePoll<T>, String>>,
{
    let deadline = expires_in_seconds
        .map(|s| now_ms() as f64 + s * 1000.0)
        .unwrap_or(f64::INFINITY);
    let mut interval_ms =
        DEVICE_MIN_INTERVAL_MS.max((interval_seconds.unwrap_or(5.0) * 1000.0).floor() as i64);
    let mut slow_downs = 0;
    let remaining = |deadline: f64| deadline - now_ms() as f64;
    if wait_before_first_poll {
        let left = remaining(deadline);
        if left > 0.0 {
            tokio::time::sleep(Duration::from_millis((interval_ms as f64).min(left) as u64)).await;
        }
    }
    while (now_ms() as f64) < deadline {
        match poll().await? {
            DevicePoll::Complete(value) => return Ok(value),
            DevicePoll::Failed(message) => return Err(message),
            DevicePoll::SlowDown(server) => {
                slow_downs += 1;
                interval_ms = match server.filter(|v| v.is_finite() && *v > 0.0) {
                    Some(seconds) => DEVICE_MIN_INTERVAL_MS.max((seconds * 1000.0).floor() as i64),
                    None => DEVICE_MIN_INTERVAL_MS.max(interval_ms + DEVICE_SLOW_DOWN_INCREMENT_MS),
                };
            }
            DevicePoll::Pending => {}
        }
        let left = remaining(deadline);
        if left <= 0.0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis((interval_ms as f64).min(left) as u64)).await;
    }
    Err(if slow_downs > 0 {
        DEVICE_SLOW_DOWN_TIMEOUT_MESSAGE
    } else {
        DEVICE_TIMEOUT_MESSAGE
    }
    .to_string())
}

// ─── pi-ai utils/oauth-page.js ──────────────────────────────────────────────

const LOGO_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 800 800" aria-hidden="true"><path fill="#F09082" d="M165.29 165.29H517.36V400H400V282.65H165.29Z"/><path fill="#4D9ABF" d="M165.29 282.65H282.65V400H400V517.36H282.65V634.72H165.29Z"/><path fill="#F1BE58" d="M517.36 400H634.72V634.72H517.36Z"/></svg>"##;

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn render_page(title: &str, heading: &str, message: &str, details: Option<&str>) -> String {
    let details = details
        .map(|d| format!("<div class=\"details\">{}</div>", escape_html(d)))
        .unwrap_or_default();
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>{title}</title>
  <style>
    :root {{
      --text: #fafafa;
      --text-dim: #a1a1aa;
      --page-bg: #09090b;
      --font-sans: ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, "Helvetica Neue", Arial, "Noto Sans", sans-serif, "Apple Color Emoji", "Segoe UI Emoji", "Segoe UI Symbol", "Noto Color Emoji";
      --font-mono: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, "Liberation Mono", "Courier New", monospace;
    }}
    * {{ box-sizing: border-box; }}
    html {{ color-scheme: dark; }}
    body {{
      margin: 0;
      min-height: 100vh;
      display: flex;
      align-items: center;
      justify-content: center;
      padding: 24px;
      background: var(--page-bg);
      color: var(--text);
      font-family: var(--font-sans);
      text-align: center;
    }}
    main {{
      width: 100%;
      max-width: 560px;
      display: flex;
      flex-direction: column;
      align-items: center;
      justify-content: center;
    }}
    .logo {{
      width: 72px;
      height: 72px;
      display: block;
      margin-bottom: 24px;
    }}
    h1 {{
      margin: 0 0 10px;
      font-size: 28px;
      line-height: 1.15;
      font-weight: 650;
      color: var(--text);
    }}
    p {{
      margin: 0;
      line-height: 1.7;
      color: var(--text-dim);
      font-size: 15px;
    }}
    .details {{
      margin-top: 16px;
      font-family: var(--font-mono);
      font-size: 13px;
      color: var(--text-dim);
      white-space: pre-wrap;
      word-break: break-word;
    }}
  </style>
</head>
<body>
  <main>
    <div class="logo">{LOGO_SVG}</div>
    <h1>{heading}</h1>
    <p>{message}</p>
    {details}
  </main>
</body>
</html>"#,
        title = escape_html(title),
        heading = escape_html(heading),
        message = escape_html(message),
    )
}

pub fn oauth_success_html(message: &str) -> String {
    render_page(
        "Authentication successful",
        "Authentication successful",
        message,
        None,
    )
}

pub fn oauth_error_html(message: &str, details: Option<&str>) -> String {
    render_page(
        "Authentication failed",
        "Authentication failed",
        message,
        details,
    )
}

// ─── Loopback callback server (pi-ai callback-server.js) ────────────────────
//
// Plexus runs pi-ai inside the gateway process, so the loopback listener binds
// on the gateway host. The listener lives inside the login future: dropping
// the future (cancel, supersede, expiry) closes the port synchronously.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// What a request handler decided.
pub enum CallbackDecision<T> {
    /// Answer and keep waiting.
    Respond(u16, String),
    /// Answer and settle the wait with this outcome.
    Finish(u16, String, Result<T, String>),
}

pub struct RawCallbackServer {
    listener: TcpListener,
    pub port: u16,
}

impl RawCallbackServer {
    pub async fn bind(host: &str, port: u16) -> std::io::Result<Self> {
        let listener = TcpListener::bind((host, port)).await?;
        let port = listener.local_addr()?.port();
        Ok(Self { listener, port })
    }
}

fn reason(status: u16) -> &'static str {
    reqwest::StatusCode::from_u16(status)
        .ok()
        .and_then(|s| s.canonical_reason())
        .unwrap_or("")
}

async fn read_request(stream: &mut TcpStream) -> Option<(String, String)> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 2048];
    loop {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > 32 * 1024 {
            break;
        }
    }
    let text = String::from_utf8_lossy(&buf);
    let mut parts = text.lines().next()?.split_whitespace();
    Some((parts.next()?.to_string(), parts.next()?.to_string()))
}

pub async fn write_response(stream: &mut TcpStream, status: u16, html: &str, no_store: bool) {
    let cache = if no_store {
        "cache-control: no-store\r\n"
    } else {
        ""
    };
    let head = format!(
        "HTTP/1.1 {status} {}\r\ncontent-type: text/html; charset=utf-8\r\n{cache}content-length: {}\r\nconnection: close\r\n\r\n",
        reason(status),
        html.len()
    );
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.write_all(html.as_bytes()).await;
    let _ = stream.shutdown().await;
}

/// Serve requests until a handler settles the wait. Connections are handled
/// concurrently so an idle pre-opened browser connection cannot stall it.
/// `handle` receives the method and the request URL resolved against
/// `base` (`new URL(request.url, base)`).
pub async fn serve_until<T, H, Fut>(
    server: &RawCallbackServer,
    base: &str,
    no_store: bool,
    handle: H,
) -> Result<T, String>
where
    H: Fn(String, url::Url) -> Fut,
    Fut: Future<Output = CallbackDecision<T>>,
{
    use futures_util::stream::{FuturesUnordered, StreamExt};
    let base = url::Url::parse(base).map_err(|e| e.to_string())?;
    let mut inflight = FuturesUnordered::new();
    loop {
        tokio::select! {
            accepted = server.listener.accept() => {
                match accepted {
                    Ok((stream, _)) => {
                        let base = base.clone();
                        let handle = &handle;
                        inflight.push(async move {
                            let mut stream = stream;
                            let (method, target) = read_request(&mut stream).await?;
                            let url = base.join(&target).unwrap_or_else(|_| base.clone());
                            match handle(method, url).await {
                                CallbackDecision::Respond(status, html) => { write_response(&mut stream, status, &html, no_store).await; None }
                                CallbackDecision::Finish(status, html, outcome) => { write_response(&mut stream, status, &html, no_store).await; Some(outcome) }
                            }
                        });
                    }
                    Err(error) => return Err(error.to_string()),
                }
            }
            Some(done) = inflight.next(), if !inflight.is_empty() => {
                if let Some(outcome) = done { return outcome; }
            }
        }
    }
}

/// pi-ai `startOAuthCallbackServer` handle.
pub struct OAuthCallbackServer {
    raw: RawCallbackServer,
    pub redirect_uri: String,
    provider_name: String,
    path: String,
    state: Option<String>,
    timeout: Option<Duration>,
    claimed: Arc<std::sync::atomic::AtomicBool>,
}

impl OAuthCallbackServer {
    pub async fn start(
        provider_name: &str,
        host: &str,
        port: u16,
        path: &str,
        redirect_host: Option<&str>,
        state: Option<String>,
        timeout: Option<Duration>,
    ) -> Result<Self, String> {
        let raw = RawCallbackServer::bind(host, port)
            .await
            .map_err(|e| e.to_string())?;
        let redirect_host = redirect_host.unwrap_or(host);
        let redirect_host = if redirect_host.contains(':') {
            format!("[{redirect_host}]")
        } else {
            redirect_host.to_string()
        };
        let redirect_uri = format!("http://{redirect_host}:{}{path}", raw.port);
        Ok(Self {
            raw,
            redirect_uri,
            provider_name: provider_name.into(),
            path: path.into(),
            state,
            timeout,
            claimed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        })
    }

    pub fn is_claimed(&self) -> bool {
        self.claimed.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// `wait()` with the provider's `complete(code)` run inside the request.
    pub async fn wait<T, C, Fut>(&self, complete: C) -> Result<T, String>
    where
        C: Fn(String) -> Fut,
        Fut: Future<Output = Result<T, String>>,
    {
        let name = self.provider_name.clone();
        let complete = &complete;
        let serve = serve_until(&self.raw, "http://localhost", true, |method, url| {
            let claimed = self.claimed.clone();
            let name = name.clone();
            let fut = async move {
                if method != "GET" || url.path() != self.path {
                    return CallbackDecision::Respond(
                        404,
                        oauth_error_html("Callback route not found.", None),
                    );
                }
                if let Some(expected) = &self.state {
                    if url_param(&url, "state").as_deref() != Some(expected.as_str()) {
                        return CallbackDecision::Respond(
                            400,
                            oauth_error_html("State mismatch.", None),
                        );
                    }
                }
                if claimed.load(std::sync::atomic::Ordering::SeqCst) {
                    return CallbackDecision::Respond(
                        409,
                        oauth_error_html("This sign-in has already been handled.", None),
                    );
                }
                if let Some(error) = url_param(&url, "error").filter(|e| !e.is_empty()) {
                    let description = url_param(&url, "error_description").unwrap_or(error);
                    return CallbackDecision::Finish(
                        400,
                        oauth_error_html(
                            &format!("{name} authorization failed."),
                            Some(&description),
                        ),
                        Err(format!("{name} authorization failed: {description}")),
                    );
                }
                let Some(code) = url_param(&url, "code").filter(|c| !c.is_empty()) else {
                    return CallbackDecision::Respond(
                        400,
                        oauth_error_html("Missing authorization code.", None),
                    );
                };
                claimed.store(true, std::sync::atomic::Ordering::SeqCst);
                match complete(code).await {
                    Ok(value) => CallbackDecision::Finish(
                        200,
                        oauth_success_html(&format!(
                            "Signed in to {name}. You may now close this page."
                        )),
                        Ok(value),
                    ),
                    Err(message) => CallbackDecision::Finish(
                        502,
                        oauth_error_html(&format!("{name} sign-in failed."), Some(&message)),
                        Err(message),
                    ),
                }
            };
            fut
        });
        match self.timeout {
            Some(timeout) => tokio::time::timeout(timeout, serve)
                .await
                .unwrap_or_else(|_| Err(format!("{} sign-in timed out", self.provider_name))),
            None => serve.await,
        }
    }
}

pub enum CallbackOrManual<T> {
    Callback(T),
    Manual(String),
}

/// pi-ai `waitForCallbackOrManualInput`: race the browser callback against a
/// pasted code / redirect URL. A manual answer that arrives while a callback
/// is already being completed defers to the callback's outcome.
pub async fn wait_for_callback_or_manual_input<T, C, Fut>(
    interaction: &Interaction,
    callback: Option<&OAuthCallbackServer>,
    complete: C,
    message: &str,
    placeholder: &str,
) -> Result<CallbackOrManual<T>, String>
where
    C: Fn(String) -> Fut,
    Fut: Future<Output = Result<T, String>>,
{
    let manual = interaction.prompt(AuthPrompt::ManualCode {
        message: message.into(),
        placeholder: Some(placeholder.into()),
    });
    let Some(callback) = callback else {
        return manual.await.map(CallbackOrManual::Manual);
    };
    let wait = callback.wait(complete);
    tokio::pin!(wait);
    tokio::pin!(manual);
    tokio::select! {
        result = &mut wait => result.map(CallbackOrManual::Callback),
        input = &mut manual => {
            if callback.is_claimed() {
                let outcome = wait.await;
                return match input {
                    Err(error) => Err(error),
                    Ok(_) => outcome.map(CallbackOrManual::Callback),
                };
            }
            input.map(CallbackOrManual::Manual)
        }
    }
}
