//! Route, session-manager and per-provider flow tests. Provider HTTP is
//! served by local mock servers; no request leaves the host.
use super::*;
use crate::oauth_login::{
    AuthEvent, AuthPrompt, CredentialStore, Interaction, LoginFn, OAuthCallbackServer,
    OAuthCredentials, OAuthLoginSessionManager, OAuthProviderDescriptor, OAuthSelectOption,
    OAuthSessionStatus,
};
use crate::state::KeyConfig;
use axum::{
    body::Body,
    http::{HeaderMap as Headers, Request, StatusCode as Code},
    routing::{get, post},
};
use http_body_util::BodyExt;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

type Saved = Arc<Mutex<Vec<(String, String, OAuthCredentials)>>>;

fn fixture() -> AppState {
    let mut keys = HashMap::new();
    keys.insert(
        "limited".into(),
        KeyConfig {
            secret: "secret".into(),
            ..Default::default()
        },
    );
    AppState::new(Some("admin".into()), keys)
}

async fn call(state: &AppState, method: &str, uri: &str, body: Option<&str>) -> (Code, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("x-admin-key", "admin");
    if body.is_some() {
        request = request.header("content-type", "application/json");
    }
    let response = router()
        .with_state(state.clone())
        .oneshot(
            request
                .body(
                    body.map(|b| Body::from(b.to_string()))
                        .unwrap_or_else(Body::empty),
                )
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn memory_store() -> (CredentialStore, Saved) {
    let saved: Saved = Arc::default();
    let sink = saved.clone();
    let store: CredentialStore = Arc::new(move |provider, account, credentials| {
        let sink = sink.clone();
        Box::pin(async move {
            sink.lock().unwrap().push((provider, account, credentials));
            Ok(())
        })
    });
    (store, saved)
}

fn manager_with(
    id: &'static str,
    uses_callback_server: bool,
    login: LoginFn,
) -> OAuthLoginSessionManager {
    OAuthLoginSessionManager::new(Arc::new(move |requested: &str| {
        (requested == id).then(|| OAuthProviderDescriptor {
            id: id.into(),
            name: id.into(),
            uses_callback_server,
            login: login.clone(),
        })
    }))
}

fn login<F, Fut>(f: F) -> LoginFn
where
    F: Fn(Interaction) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<OAuthCredentials, String>> + Send + 'static,
{
    Arc::new(move |interaction| Box::pin(f(interaction)))
}

async fn wait_for(
    manager: &OAuthLoginSessionManager,
    id: &str,
    check: impl Fn(&crate::oauth_login::OAuthSession) -> bool,
) -> crate::oauth_login::OAuthSession {
    for _ in 0..400 {
        if let Some(session) = manager.get_session(id) {
            if check(&session) {
                return session;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!(
        "timed out waiting for session {id}: {:?}",
        manager.get_session(id)
    );
}

async fn wait_status(
    manager: &OAuthLoginSessionManager,
    id: &str,
    status: OAuthSessionStatus,
) -> crate::oauth_login::OAuthSession {
    wait_for(manager, id, |s| s.status == status).await
}

async fn serve(app: axum::Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    (
        format!("http://{addr}"),
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() }),
    )
}

fn form(body: &[u8]) -> HashMap<String, String> {
    url::form_urlencoded::parse(body).into_owned().collect()
}

// ─── Routes ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn lists_pinned_oauth_provider_descriptors() {
    let (status, body) = call(&fixture(), "GET", "/v0/management/oauth/providers", None).await;
    assert_eq!(status, Code::OK);
    assert_eq!(body["total"], 8);
    let ids: Vec<&str> = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        [
            "anthropic",
            "github-copilot",
            "kimi-coding",
            "meta",
            "openai",
            "openai-codex",
            "openrouter",
            "xai"
        ]
    );
    assert_eq!(
        body["data"][5],
        json!({"id":"openai-codex","name":"OpenAI (ChatGPT Plus/Pro)","usesCallbackServer":true,"selectable":false})
    );
    assert_eq!(
        body["data"][0],
        json!({"id":"anthropic","name":"Anthropic (Claude Pro/Max)","usesCallbackServer":false,"selectable":true})
    );
    // Legacy Codex stays listed and resolvable for existing configurations.
    assert!(is_supported_provider("openai-codex"));
    assert_eq!(
        body["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|p| p["selectable"] == false)
            .count(),
        1
    );
}

#[tokio::test]
async fn installation_device_id_is_generated_once_and_reused() {
    let state = fixture();
    let first = installation_device_id(&state).await.unwrap();
    assert!(is_uuid(&first));
    assert_eq!(installation_device_id(&state).await.unwrap(), first);
    assert_eq!(
        state.inner.settings.read().unwrap()[INSTALLATION_DEVICE_ID_SETTING],
        json!(first)
    );

    // A stored valid ID (e.g. restored from a backup) is kept; garbage is replaced.
    let restored = "123e4567-e89b-12d3-a456-426614174000";
    state
        .inner
        .settings
        .write()
        .unwrap()
        .insert(INSTALLATION_DEVICE_ID_SETTING.into(), json!(restored));
    assert_eq!(installation_device_id(&state).await.unwrap(), restored);
    state
        .inner
        .settings
        .write()
        .unwrap()
        .insert(INSTALLATION_DEVICE_ID_SETTING.into(), json!("not-a-uuid"));
    let replaced = installation_device_id(&state).await.unwrap();
    assert!(is_uuid(&replaced) && replaced != "not-a-uuid");
}

#[tokio::test]
async fn oauth_routes_are_admin_only() {
    for (credential, expected) in [
        (Some("secret"), Code::FORBIDDEN),
        (None, Code::UNAUTHORIZED),
    ] {
        let mut request = Request::get("/v0/management/oauth/providers");
        if let Some(value) = credential {
            request = request.header("x-admin-key", value);
        }
        let response = router()
            .with_state(fixture())
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
    }
}

#[tokio::test]
async fn create_session_validation_matches_plexus_zod_errors() {
    let state = fixture();
    let (status, body) = call(&state, "POST", "/v0/management/oauth/sessions", None).await;
    assert_eq!(status, Code::BAD_REQUEST);
    assert_eq!(
        body,
        json!({"error":"Invalid request body","details":[{"expected":"object","code":"invalid_type","path":[],"message":"Invalid input: expected object, received undefined"}]})
    );
    let (_, body) = call(&state, "POST", "/v0/management/oauth/sessions", Some("{}")).await;
    assert_eq!(
        body,
        json!({"error":"Invalid request body","details":[
            {"expected":"string","code":"invalid_type","path":["providerId"],"message":"Invalid input: expected string, received undefined"},
            {"expected":"string","code":"invalid_type","path":["accountId"],"message":"Invalid input: expected string, received undefined"}]})
    );
    let (_, body) = call(
        &state,
        "POST",
        "/v0/management/oauth/sessions",
        Some(r#"{"providerId":1,"accountId":""}"#),
    )
    .await;
    assert_eq!(
        body,
        json!({"error":"Invalid request body","details":[
            {"expected":"string","code":"invalid_type","path":["providerId"],"message":"Invalid input: expected string, received number"},
            {"origin":"string","code":"too_small","minimum":1,"inclusive":true,"path":["accountId"],"message":"Too small: expected string to have >=1 characters"}]})
    );
    let (_, body) = call(&state, "POST", "/v0/management/oauth/sessions", Some("[]")).await;
    assert_eq!(
        body["details"][0]["message"],
        "Invalid input: expected object, received array"
    );
    let (status, body) = call(
        &state,
        "POST",
        "/v0/management/oauth/sessions",
        Some(r#"{"providerId":"radius","accountId":"a"}"#),
    )
    .await;
    assert_eq!(
        (status, body),
        (
            Code::BAD_REQUEST,
            json!({"error":"Unknown OAuth provider: radius"})
        )
    );
    let (status, body) = call(
        &state,
        "POST",
        "/v0/management/oauth/sessions",
        Some(r#"{"providerId":"anthropic","accountId":"  "}"#),
    )
    .await;
    assert_eq!(
        (status, body),
        (Code::BAD_REQUEST, json!({"error":"accountId is required"}))
    );
}

#[tokio::test]
async fn unknown_sessions_and_input_validation() {
    let state = fixture();
    assert_eq!(
        call(&state, "GET", "/v0/management/oauth/sessions/nope", None).await,
        (Code::NOT_FOUND, json!({"error":"OAuth session not found"}))
    );
    for action in ["prompt", "manual-code", "select"] {
        let uri = format!("/v0/management/oauth/sessions/nope/{action}");
        assert_eq!(
            call(&state, "POST", &uri, Some(r#"{"value":"x"}"#)).await,
            (
                Code::BAD_REQUEST,
                json!({"error":"OAuth session not found"})
            )
        );
        let (status, body) = call(&state, "POST", &uri, Some(r#"{"value":1}"#)).await;
        assert_eq!(status, Code::BAD_REQUEST);
        assert_eq!(
            body["details"][0]["message"],
            "Invalid input: expected string, received number"
        );
        let (_, body) = call(&state, "POST", &uri, None).await;
        assert_eq!(
            body["details"][0]["message"],
            "Invalid input: expected object, received undefined"
        );
    }
    assert_eq!(
        call(
            &state,
            "POST",
            "/v0/management/oauth/sessions/nope/cancel",
            None
        )
        .await,
        (
            Code::BAD_REQUEST,
            json!({"error":"OAuth session not found"})
        )
    );
}

#[tokio::test]
async fn credential_routes_validate_and_default_without_database() {
    let state = fixture();
    assert_eq!(
        call(
            &state,
            "GET",
            "/v0/management/oauth/credentials/status?providerId=anthropic&accountId=default",
            None
        )
        .await,
        (Code::OK, json!({"data":{"ready":false}}))
    );
    let (status, body) = call(
        &state,
        "GET",
        "/v0/management/oauth/credentials/status",
        None,
    )
    .await;
    assert_eq!(status, Code::BAD_REQUEST);
    assert_eq!(body["error"], "Invalid query parameters");
    assert_eq!(body["details"].as_array().unwrap().len(), 2);
    assert_eq!(
        call(
            &state,
            "DELETE",
            "/v0/management/oauth/credentials",
            Some(r#"{"providerId":"anthropic","accountId":"missing"}"#)
        )
        .await,
        (
            Code::NOT_FOUND,
            json!({"error":"OAuth credentials not found"})
        )
    );
    let (status, body) = call(
        &state,
        "DELETE",
        "/v0/management/oauth/credentials",
        Some("{}"),
    )
    .await;
    assert_eq!(status, Code::BAD_REQUEST);
    assert_eq!(body["details"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn models_route_serves_catalog_and_fallback_warnings() {
    let state = fixture();
    let (status, body) = call(
        &state,
        "GET",
        "/v0/management/oauth/models?providerId=anthropic",
        None,
    )
    .await;
    assert_eq!(status, Code::OK);
    assert_eq!(body["source"], "catalog");
    assert_eq!(body["data"].as_array().unwrap().len(), 17);
    assert!(body["data"][0]["pricing"]["prompt"].is_string());
    assert_eq!(
        call(
            &state,
            "GET",
            "/v0/management/oauth/models?providerId=nope",
            None
        )
        .await
        .1,
        json!({"data":[],"source":"catalog"})
    );
    let (status, body) = call(
        &state,
        "GET",
        "/v0/management/oauth/models?providerId=",
        None,
    )
    .await;
    assert_eq!(status, Code::BAD_REQUEST);
    assert_eq!(body["details"][0]["code"], "too_small");
    let (_, body) = call(
        &state,
        "GET",
        "/v0/management/oauth/models?providerId=meta",
        None,
    )
    .await;
    assert_eq!(body["data"].as_array().unwrap().len(), 6);
    assert_eq!(body["warning"], "Could not reach the Muse model list (OAuth: Not authenticated for provider 'meta'. Please run OAuth login for this provider.); showing the static catalog. Log in with OAuth to see your account's models.");
    let (_, body) = call(
        &state,
        "GET",
        "/v0/management/oauth/models?providerId=openai-codex",
        None,
    )
    .await;
    let data = body["data"].as_array().unwrap();
    assert_eq!(
        data.last().unwrap(),
        &json!({"id":"gpt-image-2.5-sunburst","name":"gpt-image-2.5-sunburst","description":"Codex Images (ChatGPT OAuth)","type":"image","access_via":["codex-images"]})
    );
    assert!(body["warning"].as_str().unwrap().starts_with("Could not reach the Codex model list (OAuth: Not authenticated for provider 'openai-codex'."));
    assert_eq!(js_number(10.0), "10");
    assert_eq!(js_number(0.33), "0.33");
}

/// Live Plexus parity: these providers reach their first interaction without
/// contacting any provider host.
#[tokio::test]
async fn real_flows_reach_first_interaction_like_plexus() {
    std::env::remove_var("TREFOIL_OAUTH_DEVICE_ID");
    let state = fixture();
    let create =
        |provider: &str| format!(r#"{{"providerId":"{provider}","accountId":"paritytest"}}"#);
    let (status, body) = call(
        &state,
        "POST",
        "/v0/management/oauth/sessions",
        Some(&create("anthropic")),
    )
    .await;
    assert_eq!(status, Code::OK);
    assert_eq!(body["data"]["status"], "in_progress");
    assert_eq!(body["data"]["progress"], json!([]));
    assert!(body["data"].get("authInfo").is_none() && body["data"].get("error").is_none());
    let id = body["data"]["id"].as_str().unwrap().to_string();
    assert_eq!(id.len(), 36);
    let manager = state.inner.oauth_sessions.clone();
    let session = wait_status(&manager, &id, OAuthSessionStatus::AwaitingManualCode).await;
    let json = serde_json::to_value(&session).unwrap();
    assert_eq!(
        json["manualCode"],
        json!({"message":"Paste the code Anthropic shows after you sign in:","placeholder":"code#state"})
    );
    assert_eq!(json["authInfo"]["instructions"], "Sign in in your browser. Anthropic shows an authorization code — copy it and paste it below.");
    assert!(json["authInfo"]["url"].as_str().unwrap().starts_with("https://claude.ai/oauth/authorize?code=true&client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e&response_type=code&redirect_uri=https%3A%2F%2Fplatform.claude.com%2Foauth%2Fcode%2Fcallback&scope=org%3Acreate_api_key+user%3Aprofile"));
    assert!(json.get("prompt").is_none());
    let (_, cancelled) = call(
        &state,
        "POST",
        &format!("/v0/management/oauth/sessions/{id}/cancel"),
        None,
    )
    .await;
    assert_eq!(cancelled["data"]["status"], "cancelled");
    assert_eq!(cancelled["data"]["error"], "Login cancelled");
    assert_eq!(cancelled["data"]["manualCode"]["placeholder"], "code#state");

    let (_, body) = call(
        &state,
        "POST",
        "/v0/management/oauth/sessions",
        Some(&create("github-copilot")),
    )
    .await;
    let id = body["data"]["id"].as_str().unwrap().to_string();
    let session =
        serde_json::to_value(wait_status(&manager, &id, OAuthSessionStatus::AwaitingPrompt).await)
            .unwrap();
    assert_eq!(
        session["prompt"],
        json!({"message":"GitHub Enterprise URL/domain (blank for github.com)","placeholder":"company.ghe.com","allowEmpty":true})
    );
    call(
        &state,
        "POST",
        &format!("/v0/management/oauth/sessions/{id}/cancel"),
        None,
    )
    .await;

    let (_, body) = call(
        &state,
        "POST",
        "/v0/management/oauth/sessions",
        Some(&create("openai-codex")),
    )
    .await;
    let id = body["data"]["id"].as_str().unwrap().to_string();
    let session =
        serde_json::to_value(wait_status(&manager, &id, OAuthSessionStatus::AwaitingSelect).await)
            .unwrap();
    assert_eq!(
        session["select"],
        json!({"message":"Select OpenAI Codex login method:","options":[{"id":"browser","label":"Browser login (default)"},{"id":"device_code","label":"Device code login (headless)"}]})
    );
    let (status, body) = call(
        &state,
        "POST",
        &format!("/v0/management/oauth/sessions/{id}/select"),
        Some(r#"{"value":"nope"}"#),
    )
    .await;
    assert_eq!(
        (status, body),
        (Code::BAD_REQUEST, json!({"error":"Unknown option: nope"}))
    );
    call(
        &state,
        "POST",
        &format!("/v0/management/oauth/sessions/{id}/cancel"),
        None,
    )
    .await;

    let (_, body) = call(
        &state,
        "POST",
        "/v0/management/oauth/sessions",
        Some(&create("openrouter")),
    )
    .await;
    let id = body["data"]["id"].as_str().unwrap().to_string();
    let session = serde_json::to_value(
        wait_status(&manager, &id, OAuthSessionStatus::AwaitingManualCode).await,
    )
    .unwrap();
    let progress = session["progress"][0].as_str().unwrap();
    assert!(
        progress.starts_with("Listening for OpenRouter OAuth callback on http://127.0.0.1:"),
        "{progress}"
    );
    assert_eq!(
        session["manualCode"]["message"],
        "Complete sign-in in your browser, or paste the authorization code / redirect URL here:"
    );
    assert!(session["authInfo"]["url"]
        .as_str()
        .unwrap()
        .starts_with("https://openrouter.ai/auth?callback_url=http%3A%2F%2F127.0.0.1%3A"));
    call(
        &state,
        "POST",
        &format!("/v0/management/oauth/sessions/{id}/cancel"),
        None,
    )
    .await;

    let (_, body) = call(
        &state,
        "POST",
        "/v0/management/oauth/sessions",
        Some(&create("openai")),
    )
    .await;
    let id = body["data"]["id"].as_str().unwrap().to_string();
    // Plexus #1040: the login no longer fails for lack of a device ID; the
    // installation's persisted ID is used as the agent host.
    let session = wait_status(&manager, &id, OAuthSessionStatus::AwaitingManualCode).await;
    let device_id = state.inner.settings.read().unwrap()[INSTALLATION_DEVICE_ID_SETTING]
        .as_str()
        .unwrap()
        .to_string();
    assert!(is_uuid(&device_id));
    let url = url::Url::parse(&session.auth_info.unwrap().url).unwrap();
    let host_id = url
        .query_pairs()
        .find(|(key, _)| key == "ext_agent_host_id")
        .map(|(_, value)| value.into_owned());
    assert_eq!(host_id, Some(format!("urn:uuid:{device_id}")));
    let (status, body) = call(
        &state,
        "POST",
        &format!("/v0/management/oauth/sessions/{id}/cancel"),
        None,
    )
    .await;
    assert_eq!(status, Code::OK);
    assert_eq!(body["data"]["status"], "cancelled");
}

// ─── Session manager semantics ────────────────────────────────────────────

#[tokio::test]
async fn prompt_progress_and_success_persist_credentials() {
    let manager = manager_with(
        "mock",
        false,
        login(|i: Interaction| async move {
            let value = i
                .prompt(AuthPrompt::Text {
                    message: "Domain?".into(),
                    placeholder: None,
                })
                .await?;
            i.notify(AuthEvent::Progress(format!("got {value}")));
            i.notify(AuthEvent::DeviceCode {
                user_code: "ABCD".into(),
                verification_uri: "https://example.test/device".into(),
            });
            let code = i
                .prompt(AuthPrompt::ManualCode {
                    message: "Paste".into(),
                    placeholder: Some("code".into()),
                })
                .await?;
            Ok(OAuthCredentials::new(
                format!("access-{code}"),
                "refresh",
                42,
            ))
        }),
    );
    let (store, saved) = memory_store();
    let session = manager.create_session("mock", "acct", store).await.unwrap();
    assert_eq!(session.status, OAuthSessionStatus::InProgress);
    let waiting = wait_status(&manager, &session.id, OAuthSessionStatus::AwaitingPrompt).await;
    assert_eq!(
        serde_json::to_value(&waiting).unwrap()["prompt"],
        json!({"message":"Domain?","allowEmpty":true})
    );
    assert_eq!(
        manager
            .submit_manual_code(&session.id, "x".into())
            .await
            .unwrap_err(),
        "No manual code input is awaiting input"
    );
    assert_eq!(
        manager
            .submit_select(&session.id, "x".into())
            .await
            .unwrap_err(),
        "No selection is awaiting input"
    );
    let after = manager.submit_prompt(&session.id, "".into()).await.unwrap();
    assert_eq!(after.status, OAuthSessionStatus::InProgress);
    assert!(after.prompt.is_none());
    assert_eq!(
        manager
            .submit_prompt(&session.id, "again".into())
            .await
            .unwrap_err(),
        "No prompt is awaiting input"
    );
    let manual = wait_status(
        &manager,
        &session.id,
        OAuthSessionStatus::AwaitingManualCode,
    )
    .await;
    assert_eq!(manual.progress, vec!["got ".to_string()]);
    assert_eq!(
        serde_json::to_value(&manual).unwrap()["authInfo"],
        json!({"url":"https://example.test/device","instructions":"Enter code: ABCD"})
    );
    manager
        .submit_manual_code(&session.id, "c1".into())
        .await
        .unwrap();
    let done = wait_status(&manager, &session.id, OAuthSessionStatus::Success).await;
    assert!(done.error.is_none() && done.manual_code.is_none());
    let saved = saved.lock().unwrap();
    assert_eq!(saved.len(), 1);
    assert_eq!(
        (
            saved[0].0.as_str(),
            saved[0].1.as_str(),
            saved[0].2.access.as_str()
        ),
        ("mock", "acct", "access-c1")
    );
    assert_eq!(
        manager.cancel(&session.id).await.unwrap().status,
        OAuthSessionStatus::Success
    );
}

#[tokio::test]
async fn select_validates_options_and_empty_select_fails() {
    let manager = manager_with(
        "mock",
        false,
        login(|i: Interaction| async move {
            let choice = i
                .prompt(AuthPrompt::Select {
                    message: "Pick".into(),
                    options: vec![OAuthSelectOption {
                        id: "a".into(),
                        label: "A".into(),
                        description: Some("first".into()),
                    }],
                })
                .await?;
            if choice == "a" {
                i.prompt(AuthPrompt::Select {
                    message: "Empty".into(),
                    options: vec![],
                })
                .await?;
            }
            Ok(OAuthCredentials::default())
        }),
    );
    let (store, saved) = memory_store();
    let session = manager.create_session("mock", "acct", store).await.unwrap();
    let waiting = wait_status(&manager, &session.id, OAuthSessionStatus::AwaitingSelect).await;
    assert_eq!(
        serde_json::to_value(&waiting).unwrap()["select"],
        json!({"message":"Pick","options":[{"id":"a","label":"A","description":"first"}]})
    );
    assert_eq!(
        manager
            .submit_select(&session.id, "b".into())
            .await
            .unwrap_err(),
        "Unknown option: b"
    );
    let after = manager
        .submit_select(&session.id, "a".into())
        .await
        .unwrap();
    assert!(after.select.is_none());
    let failed = wait_status(&manager, &session.id, OAuthSessionStatus::Error).await;
    assert_eq!(
        failed.error.as_deref(),
        Some("Login cancelled: no options to select")
    );
    assert!(saved.lock().unwrap().is_empty());
}

#[tokio::test]
async fn cancel_rejects_pending_prompt_and_is_repeatable() {
    let manager = manager_with(
        "mock",
        false,
        login(|i: Interaction| async move {
            i.prompt(AuthPrompt::Text {
                message: "Wait".into(),
                placeholder: Some("p".into()),
            })
            .await?;
            Ok(OAuthCredentials::default())
        }),
    );
    let (store, saved) = memory_store();
    let session = manager.create_session("mock", "acct", store).await.unwrap();
    wait_status(&manager, &session.id, OAuthSessionStatus::AwaitingPrompt).await;
    let cancelled = manager.cancel(&session.id).await.unwrap();
    assert_eq!(
        (cancelled.status, cancelled.error.as_deref()),
        (OAuthSessionStatus::Cancelled, Some("Login cancelled"))
    );
    assert!(
        cancelled.prompt.is_some(),
        "Plexus keeps the stale prompt on a cancelled session"
    );
    assert_eq!(
        manager
            .submit_prompt(&session.id, "late".into())
            .await
            .unwrap_err(),
        "No prompt is awaiting input"
    );
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(
        manager.get_session(&session.id).unwrap().status,
        OAuthSessionStatus::Cancelled
    );
    let again = manager.cancel(&session.id).await.unwrap();
    assert_eq!(again.status, OAuthSessionStatus::Cancelled);
    assert!(saved.lock().unwrap().is_empty());
}

#[tokio::test]
async fn login_and_store_errors_surface_on_session() {
    let manager = manager_with(
        "mock",
        false,
        login(|i: Interaction| async move {
            i.notify(AuthEvent::AuthUrl {
                url: "https://a.test".into(),
                instructions: None,
            });
            Err("boom".to_string())
        }),
    );
    let (store, _) = memory_store();
    let session = manager.create_session("mock", "acct", store).await.unwrap();
    let failed = wait_status(&manager, &session.id, OAuthSessionStatus::Error).await;
    assert_eq!(failed.error.as_deref(), Some("boom"));
    assert_eq!(
        serde_json::to_value(&failed).unwrap()["authInfo"],
        json!({"url":"https://a.test"})
    );

    let manager = manager_with(
        "mock",
        false,
        login(|_| async { Ok(OAuthCredentials::new("a", "r", 1)) }),
    );
    let failing: CredentialStore =
        Arc::new(|_, _, _| Box::pin(async { Err("db down".to_string()) }));
    let session = manager
        .create_session("mock", "acct", failing)
        .await
        .unwrap();
    assert_eq!(
        wait_status(&manager, &session.id, OAuthSessionStatus::Error)
            .await
            .error
            .as_deref(),
        Some("db down")
    );
}

#[tokio::test]
async fn progress_is_capped_at_one_hundred() {
    let manager = manager_with(
        "mock",
        false,
        login(|i: Interaction| async move {
            for n in 0..105 {
                i.notify(AuthEvent::Info(format!("m{n}")));
            }
            i.prompt(AuthPrompt::Text {
                message: "hold".into(),
                placeholder: None,
            })
            .await?;
            Ok(OAuthCredentials::default())
        }),
    );
    let (store, _) = memory_store();
    let session = manager.create_session("mock", "acct", store).await.unwrap();
    let held = wait_status(&manager, &session.id, OAuthSessionStatus::AwaitingPrompt).await;
    assert_eq!(held.progress.len(), 100);
    assert_eq!(held.progress[0], "m5");
}

#[tokio::test]
async fn callback_server_providers_supersede_the_previous_login() {
    let manager = manager_with(
        "cb",
        true,
        login(|i: Interaction| async move {
            i.prompt(AuthPrompt::ManualCode {
                message: "m".into(),
                placeholder: None,
            })
            .await?;
            Ok(OAuthCredentials::default())
        }),
    );
    let (store, _) = memory_store();
    let first = manager
        .create_session("cb", "acct", store.clone())
        .await
        .unwrap();
    wait_status(&manager, &first.id, OAuthSessionStatus::AwaitingManualCode).await;
    let second = manager.create_session("cb", "other", store).await.unwrap();
    let old = manager.get_session(&first.id).unwrap();
    assert_eq!(
        (old.status, old.error.as_deref()),
        (
            OAuthSessionStatus::Cancelled,
            Some("Superseded by a new login")
        )
    );
    assert_eq!(
        wait_status(&manager, &second.id, OAuthSessionStatus::AwaitingManualCode)
            .await
            .account_id,
        "other"
    );
}

#[tokio::test]
async fn expired_sessions_disappear_and_release_their_login() {
    let released = Arc::new(Mutex::new(None));
    let seen = released.clone();
    let manager = manager_with(
        "mock",
        false,
        login(move |i: Interaction| {
            let seen = seen.clone();
            async move {
                let result = i
                    .prompt(AuthPrompt::Text {
                        message: "x".into(),
                        placeholder: None,
                    })
                    .await;
                *seen.lock().unwrap() = Some(result.clone());
                result.map(|_| OAuthCredentials::default())
            }
        }),
    );
    let (store, _) = memory_store();
    let session = manager.create_session("mock", "acct", store).await.unwrap();
    wait_status(&manager, &session.id, OAuthSessionStatus::AwaitingPrompt).await;
    manager.expire_now(&session.id);
    assert!(manager.get_session(&session.id).is_none());
    assert_eq!(
        manager
            .submit_prompt(&session.id, "x".into())
            .await
            .unwrap_err(),
        "OAuth session not found"
    );
}

// ─── Loopback callback server ─────────────────────────────────────────────

#[tokio::test]
async fn callback_server_follows_pi_ai_request_handling() {
    let server = OAuthCallbackServer::start(
        "OpenAI",
        "127.0.0.1",
        0,
        "/auth/callback",
        None,
        Some("st".into()),
        None,
    )
    .await
    .unwrap();
    assert!(
        server.redirect_uri.starts_with("http://127.0.0.1:")
            && server.redirect_uri.ends_with("/auth/callback")
    );
    let base = server
        .redirect_uri
        .trim_end_matches("/auth/callback")
        .to_string();
    let client = reqwest::Client::new();
    let requests = async {
        let r = client.get(format!("{base}/other")).send().await.unwrap();
        assert_eq!(r.status().as_u16(), 404);
        let r = client
            .get(format!("{base}/auth/callback?state=bad&code=c"))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 400);
        assert!(r.text().await.unwrap().contains("State mismatch."));
        let r = client
            .get(format!("{base}/auth/callback?state=st"))
            .send()
            .await
            .unwrap();
        assert!(r
            .text()
            .await
            .unwrap()
            .contains("Missing authorization code."));
        let r = client
            .get(format!("{base}/auth/callback?state=st&code=good"))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 200);
        assert_eq!(r.headers()["cache-control"], "no-store");
        assert!(r
            .text()
            .await
            .unwrap()
            .contains("Signed in to OpenAI. You may now close this page."));
    };
    let (result, _) = tokio::join!(
        server.wait(|code| async move { Ok::<_, String>(format!("done-{code}")) }),
        requests
    );
    assert_eq!(result.unwrap(), "done-good");

    let denied = OAuthCallbackServer::start("OpenRouter", "127.0.0.1", 0, "/cb", None, None, None)
        .await
        .unwrap();
    let url = format!(
        "{}?error=access_denied&error_description=nope",
        denied.redirect_uri
    );
    let (result, response) = tokio::join!(
        denied.wait(|c| async move { Ok::<_, String>(c) }),
        client.get(url).send()
    );
    assert_eq!(result.unwrap_err(), "OpenRouter authorization failed: nope");
    assert_eq!(response.unwrap().status().as_u16(), 400);

    let slow = OAuthCallbackServer::start(
        "OpenRouter",
        "127.0.0.1",
        0,
        "/cb",
        None,
        None,
        Some(std::time::Duration::from_millis(50)),
    )
    .await
    .unwrap();
    assert_eq!(
        slow.wait(|c| async move { Ok::<_, String>(c) })
            .await
            .unwrap_err(),
        "OpenRouter sign-in timed out"
    );
}

// ─── Provider flows against mock provider servers ─────────────────────────

fn manager_for(
    id: &'static str,
    uses_callback_server: bool,
    f: LoginFn,
) -> OAuthLoginSessionManager {
    manager_with(id, uses_callback_server, f)
}

#[tokio::test]
async fn anthropic_copy_code_flow_exchanges_and_validates_state() {
    let app = axum::Router::new().route(
        "/token",
        post(|headers: Headers, body: axum::body::Bytes| async move {
            assert_eq!(headers["content-type"], "application/json");
            let body: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(
                body["redirect_uri"],
                crate::anthropic_oauth::COPY_CODE_REDIRECT_URI
            );
            assert_eq!(body["grant_type"], "authorization_code");
            if body["code"] == "bad" {
                return (Code::BAD_REQUEST, "nope".to_string());
            }
            assert_eq!(body["state"], body["code_verifier"]);
            (
                Code::OK,
                json!({"access_token":"acc","refresh_token":"ref","expires_in":3600}).to_string(),
            )
        }),
    );
    let (base, server) = serve(app).await;
    let endpoints = crate::anthropic_oauth::Endpoints {
        authorize_url: "https://claude.ai/oauth/authorize".into(),
        token_url: format!("{base}/token"),
    };
    let ep = endpoints.clone();
    let manager = manager_for(
        "anthropic",
        false,
        login(move |i| crate::anthropic_oauth::login(i, ep.clone())),
    );
    let (store, saved) = memory_store();

    let session = manager
        .create_session("anthropic", "acct", store.clone())
        .await
        .unwrap();
    let waiting = wait_status(
        &manager,
        &session.id,
        OAuthSessionStatus::AwaitingManualCode,
    )
    .await;
    let url = url::Url::parse(&waiting.auth_info.unwrap().url).unwrap();
    let state = crate::oauth_login::url_param(&url, "state").unwrap();
    manager
        .submit_manual_code(&session.id, format!("good#{state}"))
        .await
        .unwrap();
    let done = wait_status(&manager, &session.id, OAuthSessionStatus::Success).await;
    assert_eq!(
        done.progress,
        vec!["Exchanging authorization code for tokens...".to_string()]
    );
    let creds = saved.lock().unwrap()[0].2.clone();
    assert_eq!(
        (creds.access.as_str(), creds.refresh.as_str()),
        ("acc", "ref")
    );
    assert!(creds.expires > crate::oauth_login::now_ms() + 3_000_000);

    let session = manager
        .create_session("anthropic", "acct", store.clone())
        .await
        .unwrap();
    wait_status(
        &manager,
        &session.id,
        OAuthSessionStatus::AwaitingManualCode,
    )
    .await;
    manager
        .submit_manual_code(&session.id, "good#other".into())
        .await
        .unwrap();
    assert_eq!(
        wait_status(&manager, &session.id, OAuthSessionStatus::Error)
            .await
            .error
            .as_deref(),
        Some("OAuth state mismatch")
    );

    let session = manager
        .create_session("anthropic", "acct", store)
        .await
        .unwrap();
    wait_status(
        &manager,
        &session.id,
        OAuthSessionStatus::AwaitingManualCode,
    )
    .await;
    manager
        .submit_manual_code(&session.id, "bad".into())
        .await
        .unwrap();
    assert_eq!(
        wait_status(&manager, &session.id, OAuthSessionStatus::Error)
            .await
            .error
            .as_deref(),
        Some("Anthropic token exchange failed. status=400; body=nope")
    );
    server.abort();
}

#[tokio::test]
async fn copilot_flow_runs_device_grant_token_and_model_policy() {
    let catalog_id = {
        let catalog: Value =
            serde_json::from_str(include_str!("../assets/pi-catalog.json")).unwrap();
        catalog["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == "github-copilot")
            .unwrap()["models"][0]["id"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let enabled = Arc::new(Mutex::new(Vec::<String>::new()));
    let enabled_sink = enabled.clone();
    let models_id = catalog_id.clone();
    let app = axum::Router::new()
        .route("/login/device/code", post(|body: axum::body::Bytes| async move {
            let f = form(&body);
            assert_eq!((f["client_id"].as_str(), f["scope"].as_str()), ("Iv1.b507a08c87ecfe98", "read:user"));
            axum::Json(json!({"device_code":"dev","user_code":"WXYZ","verification_uri":"https://github.com/login/device","expires_in":600,"interval":1}))
        }))
        .route("/login/oauth/access_token", post(|body: axum::body::Bytes| async move {
            assert_eq!(form(&body)["device_code"], "dev");
            axum::Json(json!({"access_token":"gh-token"}))
        }))
        .route("/copilot_internal/v2/token", get(|headers: Headers| async move {
            assert_eq!(headers["authorization"], "Bearer gh-token");
            assert_eq!(headers["editor-version"], "vscode/1.107.0");
            axum::Json(json!({"token":"tid=1;exp=2","expires_at": chrono::Utc::now().timestamp() + 3600}))
        }))
        .route("/models", get(move |headers: Headers| {
            let id = models_id.clone();
            async move {
                assert_eq!(headers["x-github-api-version"], "2026-06-01");
                axum::Json(json!({"data":[{"id":"picked","model_picker_enabled":true},{"id":id,"model_picker_enabled":true,"policy":{"state":"unconfigured"}}]}))
            }
        }))
        .route("/models/:id/policy", post(move |axum::extract::Path(id): axum::extract::Path<String>, body: String| {
            let sink = enabled_sink.clone();
            async move {
                assert_eq!(body, r#"{"state":"enabled"}"#);
                sink.lock().unwrap().push(id);
                Code::OK
            }
        }));
    let (base, server) = serve(app).await;
    let endpoints = crate::copilot_oauth::Endpoints {
        github_base: Some(base.clone()),
        api_base: Some(base.clone()),
        copilot_base: Some(base.clone()),
    };
    let manager = manager_for(
        "github-copilot",
        false,
        login(move |i| crate::copilot_oauth::login(i, endpoints.clone())),
    );
    let (store, saved) = memory_store();
    let session = manager
        .create_session("github-copilot", "acct", store.clone())
        .await
        .unwrap();
    wait_status(&manager, &session.id, OAuthSessionStatus::AwaitingPrompt).await;
    manager.submit_prompt(&session.id, "".into()).await.unwrap();
    let device = wait_status(&manager, &session.id, OAuthSessionStatus::AwaitingAuth).await;
    assert_eq!(
        serde_json::to_value(&device.auth_info).unwrap(),
        json!({"url":"https://github.com/login/device","instructions":"Enter code: WXYZ"})
    );
    let done = wait_status(&manager, &session.id, OAuthSessionStatus::Success).await;
    assert_eq!(done.progress, vec!["Enabling models...".to_string()]);
    assert_eq!(*enabled.lock().unwrap(), vec![catalog_id.clone()]);
    let creds = saved.lock().unwrap()[0].2.clone();
    assert_eq!(
        (
            creds.access.as_str(),
            creds.refresh.as_str(),
            creds.stored_refresh().as_str()
        ),
        ("tid=1;exp=2", "gh-token", "gh-token")
    );
    assert_eq!(
        creds.available_model_ids,
        vec!["picked".to_string(), catalog_id]
    );

    let session = manager
        .create_session("github-copilot", "acct", store)
        .await
        .unwrap();
    wait_status(&manager, &session.id, OAuthSessionStatus::AwaitingPrompt).await;
    manager
        .submit_prompt(&session.id, "http://".into())
        .await
        .unwrap();
    assert_eq!(
        wait_status(&manager, &session.id, OAuthSessionStatus::Error)
            .await
            .error
            .as_deref(),
        Some("Invalid GitHub Enterprise URL/domain")
    );
    server.abort();
}

#[tokio::test]
async fn kimi_meta_and_xai_device_flows_follow_pinned_contracts() {
    let app = axum::Router::new()
        .route("/kimi/api/oauth/device_authorization", post(|| async {
            axum::Json(json!({"device_code":"kd","user_code":"KIMI","verification_uri":"https://auth.kimi.com/device","verification_uri_complete":"https://auth.kimi.com/device?code=KIMI","interval":1,"expires_in":60}))
        }))
        .route("/kimi/api/oauth/token", post(|body: axum::body::Bytes| async move {
            assert_eq!(form(&body)["grant_type"], "urn:ietf:params:oauth:grant-type:device_code");
            axum::Json(json!({"access_token":"ka","refresh_token":"kr","expires_in":3600}))
        }))
        .route("/meta/device", post(|| async { axum::Json(json!({"device_code":"md","user_code":"META","verification_uri":"https://auth.meta.com/device","interval":1,"expires_in":60})) }))
        .route("/meta/token", post(|| async { axum::Json(json!({"access_token":"identity"})) }))
        .route("/meta/mint", post(|headers: Headers| async move {
            assert_eq!(headers["authorization"], "Bearer identity");
            assert_eq!(headers["x-api-version"], "1.0.0");
            axum::Json(json!({"api_key":"model-key"}))
        }))
        .route("/meta/dead", post(|| async { (Code::UNAUTHORIZED, axum::Json(json!({"detail":"gone"}))) }))
        .route("/xai/device", post(|body: axum::body::Bytes| async move {
            let f = form(&body);
            assert_eq!((f["scope"].as_str(), f["referrer"].as_str()), ("openid profile email offline_access grok-cli:access api:access", "pi"));
            axum::Json(json!({"device_code":"xd","user_code":"XAI","verification_uri":"https://accounts.x.ai/device","verification_uri_complete":"https://accounts.x.ai/device?c=XAI","interval":1,"expires_in":60}))
        }))
        .route("/xai/token", post(|| async { (Code::BAD_REQUEST, axum::Json(json!({"error":"access_denied"}))) }));
    let (base, server) = serve(app).await;
    let (store, saved) = memory_store();

    let kimi = crate::kimi_oauth::Endpoints {
        oauth_host: Some(format!("{base}/kimi/")),
    };
    let manager = manager_for(
        "kimi-coding",
        false,
        login(move |i| crate::kimi_oauth::login(i, kimi.clone())),
    );
    let session = manager
        .create_session("kimi-coding", "acct", store.clone())
        .await
        .unwrap();
    let device = wait_status(&manager, &session.id, OAuthSessionStatus::AwaitingAuth).await;
    assert_eq!(
        device.auth_info.unwrap().url,
        "https://auth.kimi.com/device?code=KIMI"
    );
    wait_status(&manager, &session.id, OAuthSessionStatus::Success).await;
    assert_eq!(saved.lock().unwrap()[0].2.refresh, "kr");

    let meta = crate::meta_oauth::Endpoints {
        device_authorization_url: format!("{base}/meta/device"),
        device_token_url: format!("{base}/meta/token"),
        api_key_mint_url: format!("{base}/meta/mint"),
    };
    let manager = manager_for(
        "meta",
        false,
        login(move |i| crate::meta_oauth::login(i, meta.clone())),
    );
    let session = manager
        .create_session("meta", "acct", store.clone())
        .await
        .unwrap();
    let done = wait_status(&manager, &session.id, OAuthSessionStatus::Success).await;
    assert_eq!(
        done.progress,
        vec!["Enabling Meta Model API access...".to_string()]
    );
    assert_eq!(
        done.auth_info.unwrap().instructions.as_deref(),
        Some("Enter code: META")
    );
    let creds = saved.lock().unwrap()[1].2.clone();
    assert_eq!(
        (creds.access.as_str(), creds.refresh.as_str()),
        ("model-key", "identity")
    );

    let dead = crate::meta_oauth::Endpoints {
        device_authorization_url: format!("{base}/meta/device"),
        device_token_url: format!("{base}/meta/token"),
        api_key_mint_url: format!("{base}/meta/dead"),
    };
    let manager = manager_for(
        "meta",
        false,
        login(move |i| crate::meta_oauth::login(i, dead.clone())),
    );
    let session = manager
        .create_session("meta", "acct", store.clone())
        .await
        .unwrap();
    assert_eq!(
        wait_status(&manager, &session.id, OAuthSessionStatus::Error)
            .await
            .error
            .as_deref(),
        Some("Meta session expired (status 401). Run `/login meta` to sign in again.: gone")
    );

    let xai = crate::xai_oauth::Endpoints {
        device_code_url: format!("{base}/xai/device"),
        token_url: format!("{base}/xai/token"),
    };
    let manager = manager_for(
        "xai",
        false,
        login(move |i| crate::xai_oauth::login(i, xai.clone())),
    );
    let session = manager.create_session("xai", "acct", store).await.unwrap();
    let failed = wait_status(&manager, &session.id, OAuthSessionStatus::Error).await;
    assert_eq!(
        failed.error.as_deref(),
        Some("xAI device authorization was denied")
    );
    assert_eq!(
        failed.auth_info.unwrap().url,
        "https://accounts.x.ai/device?c=XAI"
    );
    server.abort();
}

fn fake_jwt(account: &str) -> String {
    let payload = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD_NO_PAD,
        json!({"https://api.openai.com/auth":{"chatgpt_account_id": account}}).to_string(),
    );
    format!("aGVhZA.{payload}.c2ln")
}

#[tokio::test]
async fn codex_device_and_manual_browser_flows() {
    let jwt = fake_jwt("acct-1");
    let token_jwt = jwt.clone();
    let app = axum::Router::new()
        .route(
            "/api/accounts/deviceauth/usercode",
            post(|body: String| async move {
                assert_eq!(
                    serde_json::from_str::<Value>(&body).unwrap()["client_id"],
                    "app_EMoamEEZ73f0CkXaXp7hrann"
                );
                axum::Json(json!({"device_auth_id":"da","user_code":"CODE-1","interval":"0"}))
            }),
        )
        .route(
            "/api/accounts/deviceauth/token",
            post(|| async { axum::Json(json!({"authorization_code":"ac","code_verifier":"cv"})) }),
        )
        .route(
            "/oauth/token",
            post(move |body: axum::body::Bytes| {
                let jwt = token_jwt.clone();
                async move {
                    let f = form(&body);
                    let expected = if f["code"] == "ac" {
                        ("cv", "https://auth.openai.com/deviceauth/callback")
                    } else {
                        ("", "http://localhost:1455/auth/callback")
                    };
                    if f["code"] == "ac" {
                        assert_eq!(f["code_verifier"], expected.0);
                    }
                    assert_eq!(f["redirect_uri"], expected.1);
                    axum::Json(json!({"access_token": jwt, "refresh_token":"rt","expires_in":3600}))
                }
            }),
        );
    let (base, server) = serve(app).await;
    let endpoints = crate::codex_oauth::Endpoints {
        auth_base: base.clone(),
        callback_port: 0,
    };
    let manager = manager_for(
        "openai-codex",
        true,
        login(move |i| crate::codex_oauth::login(i, endpoints.clone())),
    );
    let (store, saved) = memory_store();

    let session = manager
        .create_session("openai-codex", "acct", store.clone())
        .await
        .unwrap();
    wait_status(&manager, &session.id, OAuthSessionStatus::AwaitingSelect).await;
    manager
        .submit_select(&session.id, "device_code".into())
        .await
        .unwrap();
    let done = wait_status(&manager, &session.id, OAuthSessionStatus::Success).await;
    assert_eq!(
        serde_json::to_value(&done.auth_info).unwrap(),
        json!({"url":"https://auth.openai.com/codex/device","instructions":"Enter code: CODE-1"})
    );
    assert_eq!(
        saved.lock().unwrap()[0].2.account_id.as_deref(),
        Some("acct-1")
    );

    let session = manager
        .create_session("openai-codex", "acct", store.clone())
        .await
        .unwrap();
    wait_status(&manager, &session.id, OAuthSessionStatus::AwaitingSelect).await;
    manager
        .submit_select(&session.id, "browser".into())
        .await
        .unwrap();
    let waiting = wait_status(
        &manager,
        &session.id,
        OAuthSessionStatus::AwaitingManualCode,
    )
    .await;
    assert_eq!(
        serde_json::to_value(&waiting.manual_code).unwrap(),
        json!({"message":"Complete login in your browser, or paste the authorization code / redirect URL here:","placeholder":"http://localhost:1455/auth/callback"})
    );
    assert_eq!(
        waiting.auth_info.as_ref().unwrap().instructions.as_deref(),
        Some("A browser window should open. Complete login to finish.")
    );
    let state = crate::oauth_login::url_param(
        &url::Url::parse(&waiting.auth_info.unwrap().url).unwrap(),
        "state",
    )
    .unwrap();
    manager
        .submit_manual_code(
            &session.id,
            format!("http://localhost:1455/auth/callback?code=bc&state={state}"),
        )
        .await
        .unwrap();
    wait_status(&manager, &session.id, OAuthSessionStatus::Success).await;
    assert_eq!(saved.lock().unwrap().len(), 2);

    let session = manager
        .create_session("openai-codex", "acct", store)
        .await
        .unwrap();
    wait_status(&manager, &session.id, OAuthSessionStatus::AwaitingSelect).await;
    manager
        .submit_select(&session.id, "browser".into())
        .await
        .unwrap();
    wait_status(
        &manager,
        &session.id,
        OAuthSessionStatus::AwaitingManualCode,
    )
    .await;
    manager
        .submit_manual_code(&session.id, "bc#wrong".into())
        .await
        .unwrap();
    assert_eq!(
        wait_status(&manager, &session.id, OAuthSessionStatus::Error)
            .await
            .error
            .as_deref(),
        Some("State mismatch")
    );
    server.abort();
}

#[tokio::test]
async fn openai_chatgpt_flow_with_device_id_and_manual_redirect() {
    let app = axum::Router::new().route("/token", post(|body: axum::body::Bytes| async move {
        let f = form(&body);
        assert_eq!(f["client_id"], "issued");
        assert_eq!(f["code"], "cc");
        axum::Json(json!({"access_token":"a","refresh_token":"r","scope":"openid chatgpt.tokens.use.direct","expires_in":3600,"id_token":"id"}))
    }));
    let (base, server) = serve(app).await;
    let endpoints = crate::openai_chatgpt_oauth::Endpoints {
        authorize_url: "https://auth.openai.com/api/accounts/authorize".into(),
        token_url: format!("{base}/token"),
        callback_port: 0,
        device_id: Some("123e4567-e89b-12d3-a456-426614174000".into()),
    };
    let manager = manager_for(
        "openai",
        false,
        login(move |i| crate::openai_chatgpt_oauth::login(i, endpoints.clone())),
    );
    let (store, saved) = memory_store();
    let session = manager
        .create_session("openai", "acct", store.clone())
        .await
        .unwrap();
    let waiting = wait_status(
        &manager,
        &session.id,
        OAuthSessionStatus::AwaitingManualCode,
    )
    .await;
    let url = url::Url::parse(&waiting.auth_info.unwrap().url).unwrap();
    let keys: Vec<String> = url.query_pairs().map(|(k, _)| k.into_owned()).collect();
    assert_eq!(
        keys,
        [
            "client_id",
            "agent_name_hint",
            "ext_agent_host_id",
            "response_type",
            "redirect_uri",
            "resource",
            "scope",
            "state",
            "code_challenge",
            "code_challenge_method",
            "nonce"
        ]
    );
    assert_eq!(
        crate::oauth_login::url_param(&url, "ext_agent_host_id").unwrap(),
        "urn:uuid:123e4567-e89b-12d3-a456-426614174000"
    );
    let state = crate::oauth_login::url_param(&url, "state").unwrap();
    manager
        .submit_manual_code(
            &session.id,
            format!("http://127.0.0.1:1455/auth/callback?code=cc&state={state}&client_id=issued"),
        )
        .await
        .unwrap();
    let done = wait_status(&manager, &session.id, OAuthSessionStatus::Success).await;
    assert_eq!(
        done.progress,
        vec!["Exchanging authorization code for tokens...".to_string()]
    );
    assert_eq!(saved.lock().unwrap()[0].2.stored_refresh(), "issued\nr");

    let session = manager
        .create_session("openai", "acct", store)
        .await
        .unwrap();
    wait_status(
        &manager,
        &session.id,
        OAuthSessionStatus::AwaitingManualCode,
    )
    .await;
    manager
        .submit_manual_code(&session.id, "not a url".into())
        .await
        .unwrap();
    assert_eq!(
        wait_status(&manager, &session.id, OAuthSessionStatus::Error)
            .await
            .error
            .as_deref(),
        Some("Paste the full callback URL from the browser")
    );
    server.abort();
}

#[tokio::test]
async fn openrouter_callback_and_manual_paths_exchange_for_a_key() {
    let app = axum::Router::new().route(
        "/keys",
        post(|body: String| async move {
            let body: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(body["code_challenge_method"], "S256");
            if body["code"] == "denied" {
                return (
                    Code::FORBIDDEN,
                    axum::Json(json!({"error":{"message":"no"}})),
                );
            }
            (
                Code::OK,
                axum::Json(json!({"key": format!("sk-{}", body["code"].as_str().unwrap())})),
            )
        }),
    );
    let (base, server) = serve(app).await;
    let endpoints = crate::openrouter_oauth::Endpoints {
        authorize_url: "https://openrouter.ai/auth".into(),
        token_url: format!("{base}/keys"),
    };
    let manager = manager_for(
        "openrouter",
        false,
        login(move |i| crate::openrouter_oauth::login(i, endpoints.clone())),
    );
    let (store, saved) = memory_store();

    let session = manager
        .create_session("openrouter", "acct", store.clone())
        .await
        .unwrap();
    let waiting = wait_status(
        &manager,
        &session.id,
        OAuthSessionStatus::AwaitingManualCode,
    )
    .await;
    let callback = waiting.manual_code.unwrap().placeholder.unwrap();
    let response = reqwest::get(format!("{callback}?code=viacb"))
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    assert!(response
        .text()
        .await
        .unwrap()
        .contains("Signed in to OpenRouter."));
    wait_status(&manager, &session.id, OAuthSessionStatus::Success).await;
    let creds = saved.lock().unwrap()[0].2.clone();
    assert_eq!(
        (creds.access.as_str(), creds.refresh.as_str(), creds.expires),
        ("sk-viacb", "", 9_007_199_254_740_991)
    );

    let session = manager
        .create_session("openrouter", "acct", store.clone())
        .await
        .unwrap();
    wait_status(
        &manager,
        &session.id,
        OAuthSessionStatus::AwaitingManualCode,
    )
    .await;
    manager
        .submit_manual_code(
            &session.id,
            "http://127.0.0.1:1/oauth/callback/x?code=pasted".into(),
        )
        .await
        .unwrap();
    let done = wait_status(&manager, &session.id, OAuthSessionStatus::Success).await;
    assert_eq!(
        done.progress.last().unwrap(),
        "Exchanging authorization code for an API key..."
    );
    assert_eq!(saved.lock().unwrap()[1].2.access, "sk-pasted");

    let session = manager
        .create_session("openrouter", "acct", store)
        .await
        .unwrap();
    wait_status(
        &manager,
        &session.id,
        OAuthSessionStatus::AwaitingManualCode,
    )
    .await;
    manager
        .submit_manual_code(&session.id, "denied".into())
        .await
        .unwrap();
    assert_eq!(
        wait_status(&manager, &session.id, OAuthSessionStatus::Error)
            .await
            .error
            .as_deref(),
        Some("OpenRouter OAuth key exchange failed (HTTP 403): no")
    );
    server.abort();
}
