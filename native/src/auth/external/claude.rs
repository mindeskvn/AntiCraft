//! Claude Code sign-in: the JSON token exchange, the `code#state` paste form and the profile GET.
//!
//! Two things distinguish this flow from the other two:
//!
//! * The token endpoint takes **JSON**, not a form, and it is a different host from the authorize
//!   page. The body's field order is fixed by the struct in `super::login`, because the client
//!   this endpoint was built against serialises a struct.
//! * The state travels in an unusual place. The redirect the browser lands on carries
//!   `?code=<code>#<state>` — the state is behind the fragment separator *inside* the code
//!   parameter. The loopback server never sees it (a fragment is not sent to a server), so the
//!   loopback exchange sends the flow's own generated state; the paste path sends the fragment,
//!   after comparing it like any other state.
//!
//! Signing in also fetches the profile: the token response carries `account` and `organization`,
//! but the profile endpoint is the one that is authoritative about which organization an account
//! belongs to, and that name is what the row shows as the plan.
//!
//! The profile request names this app as its `User-Agent`, never the official CLI's
//! `claude-cli/…`: the one thing this sign-in reproduces from that client is the system identity
//! block the inference endpoint requires (see `providers::anthropic`), and nothing else about it.

use serde_json::Value;

use super::login::{post_exchange, Completed, Enrichment, LoginAttempt};
use crate::auth::AuthError;

/// Identifies this client on the requests this flow makes itself. Versioned so a log line matches
/// a build; `(claude-oauth)` says which sign-in family it came from without naming another client.
pub const USER_AGENT: &str = concat!("anticraft-native/", env!("CARGO_PKG_VERSION"), " (claude-oauth)");

/// One profile `GET`. Small and best effort — the token response already named the account.
const PROFILE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Exchange the code and read the account/organization block.
///
/// `fragment_state` is the state from a pasted `code#state` link; empty on the loopback path,
/// where the flow's own state is sent instead. The endpoint verifies whatever it is given against
/// the flow it issued, so the value here must be the one that flow used.
pub(crate) async fn exchange(
    http: &reqwest::Client,
    attempt: &LoginAttempt,
    code: &str,
    fragment_state: &str,
) -> Result<Completed, AuthError> {
    let sent_state = if fragment_state.trim().is_empty() { attempt.state() } else { fragment_state.trim() };
    let (resp, value) = post_exchange(http, attempt, code, Some(sent_state)).await?;

    // The token response's own block first; the profile `GET` refines it when it answers.
    let mut email = value.pointer("/account/email_address").and_then(|v| v.as_str()).map(str::to_owned);
    let mut account_uuid = value.pointer("/account/uuid").and_then(|v| v.as_str()).map(str::to_owned);
    let mut org_uuid = value.pointer("/organization/uuid").and_then(|v| v.as_str()).map(str::to_owned);
    let mut org_name = value.pointer("/organization/name").and_then(|v| v.as_str()).map(str::to_owned);

    if let Some(profile) = fetch_profile(http, &attempt.endpoints().profile_url, &resp.access_token).await {
        email = pick(email, profile.pointer("/account/email").and_then(|v| v.as_str()));
        account_uuid = pick(account_uuid, profile.pointer("/account/uuid").and_then(|v| v.as_str()));
        org_uuid = pick(org_uuid, profile.pointer("/organization/uuid").and_then(|v| v.as_str()));
        org_name = pick(org_name, profile.pointer("/organization/name").and_then(|v| v.as_str()));
    }

    if email.is_none() && account_uuid.is_none() {
        return Err(AuthError::Denied(
            "Anthropic did not name an account for this sign-in, so it cannot be filed. Try again.".into(),
        ));
    }
    Ok(Completed {
        resp,
        enrich: Enrichment::Claude {
            email: email.and_then(|s| super::display_text(&s)),
            account_uuid: account_uuid.and_then(|s| super::display_text(&s)),
            org_uuid,
            org_name: org_name.and_then(|s| super::display_text(&s)),
        },
    })
}

/// The profile value wins when the profile has one: it is the fresher of the two.
fn pick(original: Option<String>, from_profile: Option<&str>) -> Option<String> {
    from_profile
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .or(original)
}

/// `GET {profile}` with the bearer. `None` on any failure — the token response already named the
/// account, so this is an improvement, never a requirement.
async fn fetch_profile(http: &reqwest::Client, url: &str, token: &str) -> Option<Value> {
    if url.trim().is_empty() {
        return None;
    }
    let resp = http
        .get(url)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .header(reqwest::header::CACHE_CONTROL, "no-cache")
        .bearer_auth(token)
        .timeout(PROFILE_TIMEOUT)
        .send()
        .await
        .map_err(|e| tracing::warn!("could not read the Claude profile: {}", e.without_url()))
        .ok()?;
    if !resp.status().is_success() {
        tracing::warn!("the Claude profile request returned HTTP {}", resp.status());
        return None;
    }
    resp.json::<Value>().await.ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::external::login::ProviderEndpoints;
    use crate::auth::external::AccountProvider;

    #[test]
    fn the_authorize_url_carries_pkce_and_the_registered_redirect() {
        let p = AccountProvider::ClaudeCode;
        let attempt = LoginAttempt::new(1, p, ProviderEndpoints::for_spec(p.spec())).unwrap();
        let q: std::collections::HashMap<String, String> = attempt.auth_url.query_pairs().into_owned().collect();
        assert_eq!(q["redirect_uri"], "http://localhost:54545/callback");
        assert_eq!(q["code"], "true");
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(
            q["scope"],
            "user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload"
        );
        assert_eq!(attempt.verifier().unwrap().len(), 128);
    }

    #[test]
    fn the_profile_wins_when_it_answers() {
        assert_eq!(pick(Some("a".into()), Some("b")), Some("b".to_owned()));
        assert_eq!(pick(Some("a".into()), Some("  ")), Some("a".to_owned()));
        assert_eq!(pick(None, None), None);
    }

    #[test]
    fn the_user_agent_names_this_app_and_this_flow() {
        assert!(USER_AGENT.starts_with("anticraft-native/"), "{USER_AGENT}");
        assert!(USER_AGENT.ends_with("(claude-oauth)"), "{USER_AGENT}");
        assert!(!USER_AGENT.contains("claude-cli"), "never impersonate the official client: {USER_AGENT}");
        assert!(!USER_AGENT.contains("antigravity"), "another flow's label is the wrong label: {USER_AGENT}");
    }

    #[tokio::test]
    async fn the_profile_request_sends_this_flow_s_user_agent() {
        use axum::{http::HeaderMap, routing::get, Router};
        use std::sync::{Arc, Mutex};

        // The user agent and bearer the provider saw.
        let seen: Arc<Mutex<Option<(String, String)>>> = Arc::new(Mutex::new(None));
        let record = seen.clone();
        let app = Router::new().route(
            "/profile",
            get(move |h: HeaderMap| {
                let record = record.clone();
                async move {
                    let header = |k: &str| h.get(k).and_then(|v| v.to_str().ok()).unwrap_or_default().to_owned();
                    *record.lock().unwrap() = Some((header("user-agent"), header("authorization")));
                    axum::Json(serde_json::json!({"account": {"email": "claude@example.com"}}))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        // The app's shared client carries a default agent of its own; the request's must win.
        let http = reqwest::Client::builder().user_agent("AntiCraft/test (shared client)").build().unwrap();
        let profile = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            fetch_profile(&http, &format!("http://{addr}/profile"), "at-profile"),
        )
        .await
        .expect("the profile request finished in time")
        .expect("the profile answered");
        assert_eq!(profile.pointer("/account/email").and_then(|v| v.as_str()), Some("claude@example.com"));
        let (agent, bearer) = seen.lock().unwrap().clone().expect("the request reached the server");
        assert_eq!(agent, USER_AGENT);
        assert_eq!(bearer, "Bearer at-profile");
    }
}
