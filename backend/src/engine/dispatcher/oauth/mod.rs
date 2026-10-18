//! OAuth inference path: `services/oauth/{oauth-native-request,
//! oauth-auth-manager}.ts`, `transformers/oauth/*` and the OAuth halves of
//! `buildRequestPayload` / `refreshOAuthRoute` (request-payload-builder.ts).
//! The sync payload builder returns an [`OAuthPlan`]; [`prepare`] resolves
//! credentials and builds the stashed wire request (`NATIVE_OAUTH_STASH`).
pub mod auth;
pub mod masking;
pub mod native;

use crate::engine::config::oauth_provider_of;
use crate::engine::dispatch::request_payload::OAuthPlan;
use crate::engine::js::*;
use crate::engine::routing::RouteResult;
use crate::state::AppState;
pub use native::PreparedOAuthRequest;
use serde_json::Value;

fn oauth_account(route: &RouteResult) -> Option<String> {
    get_str(&route.config, &["oauth_account"])
        .map(|a| crate::engine::dispatch::util::js_trim(a).to_owned())
}

/// The native/generic preparation step of `buildRequestPayload`.
pub async fn prepare(
    state: &AppState,
    route: &RouteResult,
    plan: &OAuthPlan,
    payload: Value,
    request: &Value,
    target_api_type: &str,
) -> Result<PreparedOAuthRequest, String> {
    let streaming = truthy(request.get("stream"));
    let account = oauth_account(route);
    if plan.native {
        let opt_str = |k: &str| get_str(request, &[k]).map(str::to_owned);
        native::prepare_native_oauth_dispatch(
            state,
            native::NativeParams {
                provider: &plan.provider,
                model_id: &route.model,
                native_body: payload,
                streaming,
                oauth_account: account.as_deref(),
                masking_api_key: plan.masking_route.then(|| {
                    get(&route.config, &["api_key"])
                        .filter(|v| !nullish(Some(v)))
                        .map(|v| to_string(Some(v)))
                        .unwrap_or_default()
                }),
                codex_passthrough: plan.codex_passthrough,
                api_type: target_api_type,
                anthropic: native::AnthropicOptions {
                    caller_betas: opt_str("anthropicBeta"),
                    claude_passthrough: plan.claude_passthrough,
                    caller_user_agent: opt_str("userAgent"),
                    caller_session_id: opt_str("claudeCodeSessionId"),
                },
            },
        )
        .await
    } else {
        native::prepare_generic_oauth_dispatch(
            state,
            &plan.provider,
            &route.model,
            payload,
            streaming,
            target_api_type,
            account.as_deref(),
            route.config.get("headers"),
            false,
        )
        .await
    }
}

/// `refreshOAuthRoute(route, targetApiType)` after an upstream 401: `Ok(true)`
/// when the stash was refreshed (retry the same target), `Ok(false)` when the
/// route is not refreshable.
pub async fn refresh(
    state: &AppState,
    route: &RouteResult,
    plan: &OAuthPlan,
    stash: &mut PreparedOAuthRequest,
    target_api_type: &str,
) -> Result<bool, String> {
    if plan.masking_route {
        return Ok(false);
    }
    let provider = oauth_provider_of(&route.provider, &route.config).to_owned();
    let account = oauth_account(route);
    if !plan.native {
        let streaming = stash.headers.iter().any(|(k, v)| {
            k == "Accept" && v.as_str().is_some_and(|a| a.contains("text/event-stream"))
        });
        *stash = native::prepare_generic_oauth_dispatch(
            state,
            &provider,
            &route.model,
            stash.body.clone(),
            streaming,
            target_api_type,
            account.as_deref(),
            route.config.get("headers"),
            true,
        )
        .await?;
        return Ok(true);
    }
    let token = auth::get_api_key(state, &provider, account.as_deref(), true).await?;
    stash.set_header("Authorization", Some(format!("Bearer {token}")));
    if provider == "openai-codex" {
        stash.set_header(
            "chatgpt-account-id",
            native::extract_chatgpt_account_id(&token),
        );
    } else if provider == "github-copilot" {
        let base = auth::get_base_url(state, &provider, account.as_deref())
            .await
            .filter(|b| !b.is_empty())
            .unwrap_or_else(|| auth::copilot_base_url(&token, None));
        stash.url = format!(
            "{}{}",
            base.strip_suffix('/').unwrap_or(&base),
            native::copilot_endpoint(target_api_type)
        );
    }
    Ok(true)
}
