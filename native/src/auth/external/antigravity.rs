//! Antigravity sign-in: the token exchange, the Cloud project discovery (`loadCodeAssist` /
//! `onboardUser`) and the model list (`fetchAvailableModels`).
//!
//! This is not [`crate::auth::session_import`]. That module only *reads* what Antigravity wrote to
//! `~/.gemini` and can never renew it; this one signs in with Antigravity's own OAuth client, gets
//! its own refresh token, and keeps working across restarts.
//!
//! The provider is a Google installed app: a client secret, PKCE (which Google accepts alongside
//! the secret, and which the secret alone cannot replace — it ships in a desktop app), and a
//! `loadCodeAssist` call after the token is issued. That last call is what makes an Antigravity
//! account usable at all — every generation request carries a `project`, and a token without one
//! is refused. Two calls exist because the answer depends on the account:
//!
//! * `loadCodeAssist` returns the project directly for an account that already used Antigravity;
//! * otherwise `onboardUser` stamps one, which is an asynchronous job and has to be polled.
//!
//! Every request here sends `anticraft-native` as its `User-Agent`. We do not imitate
//! Antigravity's own client string: `ideType: ANTIGRAVITY` is part of the protocol (the service
//! labels the request with it), while a forged `antigravity/hub/…` agent would be impersonation
//! for no functional gain.

use serde_json::Value;

use super::login::{post_exchange, Completed, Enrichment, LoginAttempt};
use super::valid_model_id;
use crate::auth::AuthError;

/// Identifies this client to the Cloud Code service. Versioned so a log line matches a build;
/// `(antigravity-oauth)` says which sign-in family it came from without naming another client.
pub const USER_AGENT: &str = concat!("anticraft-native/", env!("CARGO_PKG_VERSION"), " (antigravity-oauth)");

/// How many times `onboardUser` is polled before giving up. Five attempts two seconds apart is the
/// window the provider's own client uses; a project usually appears on the first or second.
const ONBOARD_POLLS: usize = 5;
/// Wait between `onboardUser` polls.
const ONBOARD_POLL_WAIT: std::time::Duration = std::time::Duration::from_secs(2);
/// One `loadCodeAssist` / `onboardUser` request.
const API_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// The model list is small and best-effort: a slow one must not hold up a sign-in.
const MODEL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// The tier asked for when onboarding a user who has never used Antigravity.
const DEFAULT_TIER: &str = "free-tier";

/// The protocol label this service expects on `loadCodeAssist`. It is not a claim about who the
/// client is: it selects the code-assist product surface.
const IDE_TYPE: &str = "ANTIGRAVITY";

/// Exchange the authorization code, read the email, and discover the Cloud project.
pub(crate) async fn exchange(
    http: &reqwest::Client,
    attempt: &LoginAttempt,
    code: &str,
) -> Result<Completed, AuthError> {
    // No `state` in the body: Google checked it on the redirect it already answered. The PKCE
    // verifier goes in through `post_exchange`, as for the other two.
    let (resp, _value) = post_exchange(http, attempt, code, None).await?;
    let token = resp.access_token.clone();
    let base = attempt.endpoints().project_api_url.clone();

    // The email comes from userinfo rather than the ID token: it is a plain GET, it needs no
    // signature to trust (over TLS from the issuer we just spoke to), and it is the field the
    // account id is derived from. A failure here is fatal — an account with no identity cannot be
    // named, and `finish_login` would refuse it anyway.
    let email = fetch_email(http, &attempt.endpoints().userinfo_url, &token).await?;
    let project_id = discover_project(http, &base, &token).await?;
    Ok(Completed { resp, enrich: Enrichment::Antigravity { project_id, email } })
}

/// `GET {userinfo}` with the bearer. `Display` of the email is the epilogue's job.
async fn fetch_email(http: &reqwest::Client, url: &str, token: &str) -> Result<Option<String>, AuthError> {
    if url.trim().is_empty() {
        return Ok(None);
    }
    let call = http
        .get(url)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .bearer_auth(token)
        .timeout(API_TIMEOUT)
        .send();
    let resp = call.await.map_err(|e| AuthError::Http(e.without_url().to_string()))?;
    if !resp.status().is_success() {
        return Err(AuthError::Denied(format!("could not read the account email (HTTP {})", resp.status())));
    }
    let bytes = resp.bytes().await.map_err(|e| AuthError::Http(e.without_url().to_string()))?;
    let value: Value = serde_json::from_slice(&bytes)?;
    Ok(value.get("email").and_then(|v| v.as_str()).map(str::to_owned))
}

/// `loadCodeAssist`, then `onboardUser` when it names no project.
///
/// Returns the project id as the request body wants it (`projects/…` or a bare id — both spellings
/// appear in the wild and both are sent back verbatim, since we only echo what we were given).
pub(crate) async fn discover_project(http: &reqwest::Client, base: &str, token: &str) -> Result<String, AuthError> {
    let load = api_post(
        http,
        &format!("{}/v1internal:loadCodeAssist", base.trim_end_matches('/')),
        token,
        &serde_json::json!({ "metadata": { "ideType": IDE_TYPE } }),
        API_TIMEOUT,
    )
    .await?;
    if let Some(p) = project_of(&load) {
        return Ok(p);
    }
    // No project: this account has not been onboarded to the code-assist product yet.
    let tier = tier_of(&load).unwrap_or_else(|| DEFAULT_TIER.to_owned());
    onboard(http, base, token, &tier).await
}

/// Poll `onboardUser` until it says `done` and names a project.
///
/// A `done: true` with no project is not success, and a request error is retried rather than
/// returned: onboarding is a background job on the provider's side, and the first poll routinely
/// answers before the job has stamped anything. Only running out of polls is an error.
async fn onboard(http: &reqwest::Client, base: &str, token: &str, tier: &str) -> Result<String, AuthError> {
    let body = serde_json::json!({
        "tier_id": tier,
        "metadata": { "ide_type": IDE_TYPE, "ide_name": "anticraft" },
    });
    let url = format!("{}/v1internal:onboardUser", base.trim_end_matches('/'));
    let mut last: Option<AuthError> = None;
    for attempt in 0..ONBOARD_POLLS {
        if attempt > 0 {
            tokio::time::sleep(ONBOARD_POLL_WAIT).await;
        }
        match api_post(http, &url, token, &body, API_TIMEOUT).await {
            Ok(value) => {
                // The project sits under `response` once the job is done; `project_of` looks there
                // itself.
                if value.get("done").and_then(|d| d.as_bool()).unwrap_or(true) {
                    if let Some(p) = project_of(&value) {
                        return Ok(p);
                    }
                }
            }
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| {
        AuthError::Denied("this account has no Cloud project yet; sign in again in a minute".into())
    }))
}

/// The project id out of any of the shapes the service has used.
///
/// `cloudaicompanionProject` is the documented one; `projectId` and `project` cover older
/// responses. Each may be a plain string **or** an object carrying the id (`{"id": …}`) — the
/// onboard response is known to answer the object form under the documented name, and reading
/// only strings there is how a sign-in ends up with no project. An empty string counts as absent:
/// a blank project in a request body is a 400, not a default.
pub(crate) fn project_of(value: &Value) -> Option<String> {
    let text = |v: &Value| v.as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned);
    let id_of = |v: &Value| text(v).or_else(|| text(&v["id"]));
    id_of(&value["cloudaicompanionProject"])
        .or_else(|| id_of(&value["projectId"]))
        .or_else(|| id_of(&value["project"]))
        .or_else(|| value.get("response").and_then(project_of))
}

/// The tier to onboard into: the default allowed tier, else the current one, else the service's
/// own default string. A wrong guess is not fatal — the call returns the tier it used.
fn tier_of(value: &Value) -> Option<String> {
    if let Some(tiers) = value.get("allowedTiers").and_then(|t| t.as_array()) {
        if let Some(id) = tiers
            .iter()
            .find(|t| t.get("isDefault").and_then(|d| d.as_bool()).unwrap_or(false))
            .and_then(|t| t.get("id"))
            .and_then(|v| v.as_str())
        {
            return Some(id.to_owned());
        }
    }
    value
        .get("currentTier")
        .and_then(|t| t.get("id"))
        .and_then(|v| v.as_str())
        .map(str::to_owned)
}

/// `POST {base}/v1internal:fetchAvailableModels` — the account's real model list.
///
/// The response is an object keyed by model id (not an array), which is why a list endpoint that
/// looks like every other one needs its own parser. Ids are lowercased, deduped and sorted, and
/// anything that is not a legal model id is dropped: these strings are pasted into request bodies.
pub(crate) async fn fetch_models(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    project: &str,
) -> Result<Vec<String>, AuthError> {
    let value = api_post(
        http,
        &format!("{}/v1internal:fetchAvailableModels", base.trim_end_matches('/')),
        token,
        &serde_json::json!({ "project": project }),
        MODEL_TIMEOUT,
    )
    .await?;
    Ok(models_of(&value))
}

/// Ids `fetchAvailableModels` lists that are not chat models a person can pick: internal
/// experiments and the editor's tab-completion models. Same list as CLIProxyAPI's model fetcher
/// (`cmd/fetch_antigravity_models/main.go`), which skips exactly these.
const INTERNAL_MODELS: &[&str] = &[
    "chat_20706",
    "chat_23310",
    "tab_flash_lite_preview",
    "tab_jump_flash_lite_preview",
    "gemini-2.5-flash-thinking",
    "gemini-2.5-pro",
];

/// The `models` object's keys, normalised, without [`INTERNAL_MODELS`]. Split out so a test can
/// drive it without HTTP.
pub(crate) fn models_of(value: &Value) -> Vec<String> {
    let mut out: Vec<String> = value
        .get("models")
        .and_then(|m| m.as_object())
        .map(|obj| {
            obj.keys()
                .map(|k| k.trim().to_ascii_lowercase())
                .filter(|k| valid_model_id(k) && !INTERNAL_MODELS.contains(&k.as_str()))
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out.dedup();
    out
}

/// One authenticated JSON POST to the Cloud Code API. A non-success status is an error whose text
/// carries the service's own message (never the bearer).
async fn api_post(
    http: &reqwest::Client,
    url: &str,
    token: &str,
    body: &Value,
    timeout: std::time::Duration,
) -> Result<Value, AuthError> {
    let resp = http
        .post(url)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .bearer_auth(token)
        .timeout(timeout)
        .json(body)
        .send()
        .await
        .map_err(|e| AuthError::Http(e.without_url().to_string()))?;
    let status = resp.status();
    let bytes = resp.bytes().await.map_err(|e| AuthError::Http(e.without_url().to_string()))?;
    if !status.is_success() {
        // The service's error text is user-facing ("PERMISSION_DENIED: …"), so it is kept, but
        // only as its `message`, never as the whole body.
        let message = serde_json::from_slice::<Value>(&bytes)
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.get("message")).and_then(|m| m.as_str()).map(str::to_owned))
            .unwrap_or_else(|| format!("HTTP {status}"));
        return Err(AuthError::Denied(format!("the Antigravity service refused: {}", truncate(&message, 200))));
    }
    Ok(serde_json::from_slice(&bytes)?)
}

/// Cut provider text to something a row can show, on a char boundary.
fn truncate(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &text[..i]),
        None => text.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::external::{display_text, AccountProvider};

    #[test]
    fn the_project_is_found_in_every_response_shape() {
        for body in [
            serde_json::json!({"cloudaicompanionProject": "projects/123"}),
            serde_json::json!({"cloudaicompanionProject": {"id": "projects/123"}}),
            serde_json::json!({"projectId": "projects/123"}),
            serde_json::json!({"projectId": {"id": "projects/123"}}),
            serde_json::json!({"project": "projects/123"}),
            serde_json::json!({"project": {"id": "projects/123"}}),
            serde_json::json!({"response": {"cloudaicompanionProject": "projects/123"}}),
            // The onboard answer: `done`, and an object under the documented name.
            serde_json::json!({"done": true, "response": {"cloudaicompanionProject": {"id": "projects/123"}}}),
        ] {
            assert_eq!(project_of(&body).as_deref(), Some("projects/123"), "{body}");
        }
        assert_eq!(project_of(&serde_json::json!({"cloudaicompanionProject": "  "})), None);
        assert_eq!(project_of(&serde_json::json!({"cloudaicompanionProject": {"id": "  "}})), None);
        assert_eq!(project_of(&serde_json::json!({"cloudaicompanionProject": {}, "projectId": "projects/9"})), Some("projects/9".into()));
        assert_eq!(project_of(&serde_json::json!({})), None);
    }

    #[test]
    fn the_tier_prefers_the_default_allowed_one() {
        let v = serde_json::json!({
            "allowedTiers": [{"id": "legacy"}, {"id": "standard", "isDefault": true}],
            "currentTier": {"id": "legacy"},
        });
        assert_eq!(tier_of(&v).as_deref(), Some("standard"));
        let v = serde_json::json!({"currentTier": {"id": "legacy"}});
        assert_eq!(tier_of(&v).as_deref(), Some("legacy"));
        assert_eq!(tier_of(&serde_json::json!({})), None);
    }

    #[test]
    fn the_model_list_is_read_from_the_object_keys() {
        let v = serde_json::json!({
            "models": { "gemini-3-flash": {"maxTokens": 1}, "Gemini-3-Flash-Lite": {}, "bad id": {} },
            "webSearchModelIds": ["gemini-3-flash"],
        });
        assert_eq!(models_of(&v), ["gemini-3-flash", "gemini-3-flash-lite"]);
        assert!(models_of(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn internal_and_tab_completion_models_are_not_offered() {
        let v = serde_json::json!({
            "models": {
                "chat_20706": {}, "chat_23310": {}, "tab_flash_lite_preview": {},
                "tab_jump_flash_lite_preview": {}, "Gemini-2.5-Pro": {}, "gemini-2.5-flash-thinking": {},
                "claude-sonnet-4-6": {}, "gemini-3-flash": {},
            },
        });
        assert_eq!(models_of(&v), ["claude-sonnet-4-6", "gemini-3-flash"]);
    }

    #[test]
    fn the_user_agent_names_this_app() {
        assert!(USER_AGENT.starts_with("anticraft-native/"), "{USER_AGENT}");
        assert!(USER_AGENT.contains("antigravity-oauth"), "{USER_AGENT}");
        assert!(!USER_AGENT.contains("antigravity/hub"), "never impersonate their client: {USER_AGENT}");
    }

    #[test]
    fn every_default_model_is_a_legal_id() {
        for m in AccountProvider::Antigravity.spec().default_models {
            assert!(valid_model_id(m) && display_text(m).is_some(), "{m}");
        }
    }
}
