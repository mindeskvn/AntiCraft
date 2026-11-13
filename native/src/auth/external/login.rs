//! Interactive sign-in shared by the three providers: [`LoginAttempt`], the loopback run, the
//! pasted redirect link, and the epilogue that turns a token response into a stored account.
//!
//! The three flows differ in three things only: how the code is exchanged, which extra fetch
//! names the account, and what else is worth keeping. Everything else — state, PKCE, the loopback
//! server, the race against cancel and timeout, the store write, the first model list — is here
//! once, so a fix to one of them is a fix to all three.
//!
//! **Loopback and paste are one flow with two ways to receive the code.** The fixed port a
//! provider was registered with can be occupied (another instance, a crashed previous run), and
//! the user may have finished the browser flow on a phone. A failed bind is therefore not the end
//! of the attempt: it keeps its state and its PKCE verifier, and `complete_with_pasted` finishes
//! it later. That is why [`LoginAttempt`] owns its state rather than taking one as a parameter.
//!
//! **The state check is the security boundary**, and it runs on both paths before a code is
//! exchanged: a pasted code from a different flow, or a forged loopback callback, must never be
//! exchanged. The pasted text itself is never echoed into an error — it is a credential.

use std::sync::Arc;
use std::time::Instant;

use chrono::Utc;
use tokio_util::sync::CancellationToken;

use super::auth::ExpiredSink;
use super::store::{AccountRecord, AccountStore};
use super::{account_id, default_model, display_text, AccountProvider, AccountView, ProviderSpec};
use crate::auth::oauth::{constant_time_eq, generate_state, parse_pasted_link, PastedLink, BAD_PASTE_MESSAGE};
use crate::auth::store::Credentials;
use crate::auth::{AuthError, Pkce, TokenResponse, LOGIN_TIMEOUT};

/// The endpoints one flow talks to.
///
/// A plain value rather than four `&str` parameters: every flow function takes one of these, so
/// pointing a sign-in at a mock server is one field, not a rewrite. Production builds it from the
/// spec, and nothing else may change the URLs — a redirect URI is registered per provider, and a
/// token endpoint that does not match the authorize endpoint is a bug that looks like a 401.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderEndpoints {
    pub auth_url: String,
    pub token_url: String,
    /// `GET` with the bearer to learn the account email (Antigravity).
    pub userinfo_url: String,
    /// `GET` with the bearer for the account/organization block (Claude). Empty when unused.
    pub profile_url: String,
    /// The API that owns `loadCodeAssist`/`onboardUser` (Antigravity).
    pub project_api_url: String,
}

impl ProviderEndpoints {
    pub fn for_spec(spec: &ProviderSpec) -> Self {
        Self {
            auth_url: spec.auth_url.to_owned(),
            token_url: spec.token_url.to_owned(),
            userinfo_url: spec.userinfo_url.to_owned(),
            profile_url: spec.profile_url.to_owned(),
            project_api_url: spec.base_url.to_owned(),
        }
    }
    /// All endpoints under one base URL: `{base}/authorize`, `{base}/token`, `{base}/userinfo`,
    /// `{base}/profile`, `{base}/api`. Tests use it; the app never does.
    pub fn with_base(base: &str) -> Self {
        let b = base.trim_end_matches('/');
        Self {
            auth_url: format!("{b}/authorize"),
            token_url: format!("{b}/token"),
            userinfo_url: format!("{b}/userinfo"),
            profile_url: format!("{b}/profile"),
            project_api_url: format!("{b}/api"),
        }
    }
}

/// One in-flight interactive login.
///
/// It survives a failed bind or a timeout so the paste path can still finish it: the verifier held
/// here is the one the authorization URL the user opened was built with, and rebuilding it would
/// produce a code the token endpoint refuses. No token is ever in here — the verifier is a
/// credential, though, so `Debug` prints only its presence.
pub struct LoginAttempt {
    pub id: u64,
    pub provider: AccountProvider,
    /// The URL the browser should visit. Shown and copied by the paste popup.
    pub auth_url: url::Url,
    /// The string registered with the provider, byte for byte.
    pub redirect_uri: String,
    pub cancel: CancellationToken,
    pub(crate) endpoints: ProviderEndpoints,
    /// The port `run_loopback` binds. The spec's registered port in production; 0 in a test, so
    /// the OS assigns one and two tests can never collide.
    bind_port: u16,
    /// The port the server actually bound, readable through `open` so a test browser dials the
    /// right place. 0 until `run_loopback` has bound.
    bound_port: std::sync::atomic::AtomicU16,
    state: String,
    pkce: Option<Pkce>,
    started: Instant,
}

impl std::fmt::Debug for LoginAttempt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoginAttempt")
            .field("id", &self.id)
            .field("provider", &self.provider)
            .field("redirect_uri", &self.redirect_uri)
            .field("has_pkce", &self.pkce.is_some())
            .finish_non_exhaustive()
    }
}

impl LoginAttempt {
    /// Build the attempt: state, PKCE when the spec has it, and the authorization URL.
    ///
    /// Nothing is bound and nothing is opened here — [`run_loopback`] does that, and the paste
    /// path needs the attempt to exist even when binding is what failed.
    pub fn new(id: u64, provider: AccountProvider, endpoints: ProviderEndpoints) -> Result<Self, AuthError> {
        // A build without Antigravity's client (see build.rs) stops here, with a message the
        // account row shows, instead of sending Google an empty client.
        if !provider.sign_in_available() {
            return Err(AuthError::AntigravityClientNotBuilt);
        }
        let spec = provider.spec();
        let state = generate_state();
        // 96 bytes is a 128-character verifier, which is what Codex and Claude both expect; the
        // length is a property of their endpoint, not a preference of ours.
        let pkce = spec.pkce.then(|| Pkce::generate_bytes(96));
        let redirect_uri = spec.redirect_uri();
        let auth_url = crate::auth::oauth::build_auth_url_for(spec, &redirect_uri, &state, pkce.as_ref(), &[])?;
        Ok(Self {
            id,
            provider,
            auth_url,
            redirect_uri,
            cancel: CancellationToken::new(),
            endpoints,
            bind_port: spec.redirect_port,
            bound_port: std::sync::atomic::AtomicU16::new(0),
            state,
            pkce,
            started: Instant::now(),
        })
    }

    /// Bind on `port` instead of the registered one. `0` means "whatever is free".
    ///
    /// Production never calls this: the redirect URI a provider accepts is the one registered
    /// with it. A test must not hold ports 51121/1455/54545 — a running app, or a parallel test,
    /// would make it flaky — so it asks the OS for one and reads it back with
    /// [`Self::bound_port`]. Passing a probed "free" port would be the race it looks like: the
    /// probe releases the port before the bind takes it.
    pub fn with_bind_port(mut self, port: u16) -> Self {
        self.bind_port = port;
        self
    }

    /// The port [`run_loopback`] will bind, as configured.
    pub fn bind_port(&self) -> u16 {
        self.bind_port
    }

    /// The port the loopback server actually bound, once [`run_loopback`] has started. `None`
    /// before that, and for a flow that failed to bind.
    pub fn bound_port(&self) -> Option<u16> {
        match self.bound_port.load(std::sync::atomic::Ordering::SeqCst) {
            0 => None,
            p => Some(p),
        }
    }

    /// [`LOGIN_TIMEOUT`] has elapsed since the attempt started. The paste path checks this too:
    /// an attempt left in a popup for an hour must not still be able to sign in.
    pub fn expired(&self) -> bool {
        self.started.elapsed() >= LOGIN_TIMEOUT
    }

    /// The flow's own state. Crate-visible so the flows can compare against it and tests can forge
    /// a callback with it.
    pub(crate) fn state(&self) -> &str {
        &self.state
    }

    pub(crate) fn verifier(&self) -> Option<&str> {
        self.pkce.as_ref().map(|p| p.verifier.as_str())
    }

    pub(crate) fn endpoints(&self) -> &ProviderEndpoints {
        &self.endpoints
    }
}

/// Everything a provider's login adds on top of the token response.
///
/// A concrete enum, not a trait object: the flows write one account at a time and the epilogue is
/// the only consumer, so indirection would buy nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Enrichment {
    /// Antigravity: the Cloud project discovered by `loadCodeAssist`/`onboardUser`, and the email
    /// read back from `userinfo` (the ID token does not name it reliably for this client).
    Antigravity { project_id: String, email: Option<String> },
    /// Codex: claims read out of the ID token.
    Codex { account_id: Option<String>, plan: Option<String>, email: Option<String> },
    /// Claude: values from the token response's `account`/`organization`, refined by the profile
    /// `GET`.
    Claude { email: Option<String>, account_uuid: Option<String>, org_uuid: Option<String>, org_name: Option<String> },
}

impl Enrichment {
    /// The name to file the account under, best first.
    ///
    /// Codex leads with the workspace id and only falls back to the email: one ChatGPT login sees
    /// every workspace the person belongs to, and two of them under one email have separate plans
    /// and separate tokens. Keying on the email would make the second sign-in overwrite the first
    /// record — and with it that workspace's refresh token.
    ///
    /// The other two lead with the email: a person has one of them per address, and the email is
    /// the name they recognise in the Account list.
    fn identity(&self) -> Option<&str> {
        fn nonblank(s: &Option<String>) -> Option<&str> {
            s.as_deref().filter(|s| !s.trim().is_empty())
        }
        match self {
            Enrichment::Antigravity { email, .. } => nonblank(email),
            Enrichment::Codex { account_id, email, .. } => nonblank(account_id).or_else(|| nonblank(email)),
            Enrichment::Claude { email, account_uuid, .. } => nonblank(email).or_else(|| nonblank(account_uuid)),
        }
    }
}

/// A token response plus what its provider added. Produced by `super::antigravity::exchange` and
/// friends, consumed by [`finish_login`]; never shown to the UI.
pub struct Completed {
    pub resp: TokenResponse,
    pub enrich: Enrichment,
}

impl std::fmt::Debug for Completed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Completed").field("resp", &self.resp).field("enrich", &self.enrich).finish()
    }
}

/// Bind the fixed port, open the browser, race callback/cancel/timeout, exchange, enrich, store.
///
/// The attempt stays usable for [`complete_with_pasted`] after any error: a bind failure, a
/// timeout or a cancel leaves its state and verifier intact, so the caller can offer the paste
/// path rather than a dead end.
pub async fn run_loopback(
    attempt: &Arc<LoginAttempt>,
    store: &AccountStore,
    http: &reqwest::Client,
    open_expired: &ExpiredSink,
    open: impl FnOnce(url::Url),
) -> Result<AccountView, AuthError> {
    let spec = attempt.provider.spec();
    let server = crate::auth::oauth::LoopbackServer::bind_pair(Some(attempt.bind_port), spec.redirect_path)
        .await
        .map_err(bind_error)?;
    // Published before the browser is opened: that is the whole point — the opener has to know
    // where to send the redirect.
    attempt.bound_port.store(server.port(), std::sync::atomic::Ordering::SeqCst);
    open(attempt.auth_url.clone());
    let outcome = tokio::select! {
        r = server.wait_for_callback(attempt.state()) => r?,
        _ = attempt.cancel.cancelled() => return Err(AuthError::Cancelled),
        _ = tokio::time::sleep(LOGIN_TIMEOUT) => return Err(AuthError::Timeout),
    };
    let code = match outcome {
        crate::auth::oauth::CallbackOutcome::Code(code) => code,
        crate::auth::oauth::CallbackOutcome::Denied(msg) => return Err(AuthError::Denied(msg)),
        crate::auth::oauth::CallbackOutcome::StateMismatch => return Err(AuthError::StateMismatch),
        crate::auth::oauth::CallbackOutcome::WrongPath
        | crate::auth::oauth::CallbackOutcome::NoCode
        | crate::auth::oauth::CallbackOutcome::Ignored => {
            return Err(AuthError::Denied("No authorization code received".into()))
        }
    };
    let done = tokio::select! {
        r = super::registry::exchange(http, attempt, &code, None) => r?,
        _ = attempt.cancel.cancelled() => return Err(AuthError::Cancelled),
    };
    finish_login(store, http, attempt, done, open_expired).await
}

/// The bind failure, as the user sees it.
///
/// `AddrInUse` already carries a message that names the advertised host and points at the paste
/// path (it is built in `oauth`), so it becomes a `Denied` the row can show rather than an opaque
/// I/O error. Anything else is a real I/O problem and stays one.
fn bind_error(e: std::io::Error) -> AuthError {
    if e.kind() == std::io::ErrorKind::AddrInUse {
        AuthError::Denied(e.to_string())
    } else {
        AuthError::Io(e)
    }
}

/// Finish a retained attempt with a pasted redirect URL. Same state check, same verifier.
///
/// `cancel` is the paste's own token, not the attempt's: the attempt's was very likely cancelled
/// already — that is how a browser round trip ends when the user switches to pasting.
pub async fn complete_with_pasted(
    attempt: &Arc<LoginAttempt>,
    store: &AccountStore,
    http: &reqwest::Client,
    open_expired: &ExpiredSink,
    link: &str,
    cancel: &CancellationToken,
) -> Result<AccountView, AuthError> {
    if attempt.expired() {
        return Err(AuthError::Timeout);
    }
    let (code, state) = match parse_pasted_link(link) {
        PastedLink::Code { code, state } => (code, state),
        PastedLink::Denied(_) => return Err(AuthError::Denied("the provider refused the sign-in".into())),
        // One fixed message: the input never appears in an error.
        PastedLink::Unreadable => return Err(AuthError::Denied(BAD_PASTE_MESSAGE.into())),
    };
    // The state check happens before any exchange, on this path exactly as on the loopback one.
    // A link without a state cannot be attributed to this flow, so it is refused rather than
    // trusted. For Claude the fragment's state has already won inside `parse_pasted_link`.
    match state {
        Some(s) if constant_time_eq(&s, attempt.state()) => {}
        _ => return Err(AuthError::StateMismatch),
    }
    // Claude's exchange body carries the state it was given, and the fragment is what it must see;
    // the other two never send one.
    let fragment = link.split_once('#').map(|(_, f)| f.trim().to_owned()).unwrap_or_default();
    // Raced against the cancel, as the loopback path's exchange is: Antigravity's includes the
    // project discovery, which can poll for a while. A code the provider already took is then
    // spent for nothing, which is what cancelling asks for; signing in again starts over.
    let done = tokio::select! {
        r = super::registry::exchange(http, attempt, &code, Some(&fragment)) => r?,
        _ = cancel.cancelled() => return Err(AuthError::Cancelled),
    };
    finish_login(store, http, attempt, done, open_expired).await
}

/// Turn a token response into a stored account, then describe it. Shared by both finish paths.
///
/// Order matters at two points: the account **id** is derived before anything is written (a login
/// with no identity is refused rather than filed under a name nobody can find), and the model
/// list is fetched only *after* the record is saved, because that fetch is a request that can
/// fail and must never cost the user a completed sign-in.
pub async fn finish_login(
    store: &AccountStore,
    http: &reqwest::Client,
    attempt: &LoginAttempt,
    done: Completed,
    open_expired: &ExpiredSink,
) -> Result<AccountView, AuthError> {
    let provider = attempt.provider;
    let spec = provider.spec();
    let Completed { resp, enrich } = done;

    // The lifetime is read before `from_token_response` consumes the response; it is what clamps
    // the refresh lead later.
    let lifetime_secs = resp.expires_in.filter(|s| *s > 0);

    // 1. Every network-sourced display string goes through one door.
    let raw_email = match &enrich {
        Enrichment::Codex { email, .. }
        | Enrichment::Claude { email, .. }
        | Enrichment::Antigravity { email, .. } => email.clone(),
    };
    let email = raw_email.as_deref().and_then(display_text);
    let raw_plan = match &enrich {
        Enrichment::Codex { plan, .. } => plan.clone(),
        Enrichment::Claude { org_name, .. } => org_name.clone(),
        Enrichment::Antigravity { .. } => None,
    };
    let plan = raw_plan.as_deref().and_then(display_text);

    // 2. The id. An identity-less sign-in would give every such account the same file name —
    //    the worst possible collision — so it is refused instead. The enrichment leads: for Codex
    //    that is the workspace id, and the email is only the fallback (see `Enrichment::identity`).
    let identity = enrich
        .identity()
        .and_then(display_text)
        .or_else(|| email.clone())
        .ok_or_else(|| AuthError::Denied("this account has no email or account id to name it by".into()))?;
    let id = account_id(provider, &identity);

    // 3. The record.
    let mut creds = Credentials::from_token_response(resp);
    creds.email = email.clone();
    let mut rec = AccountRecord {
        creds,
        provider,
        account_id: id.clone(),
        label: email.clone(),
        plan,
        wire_account_id: None,
        org_uuid: None,
        project_id: None,
        models: Vec::new(),
        models_fetched_at: None,
        lifetime_secs,
    };
    match enrich {
        Enrichment::Antigravity { project_id, .. } => rec.project_id = display_text(&project_id),
        Enrichment::Codex { account_id, .. } => rec.wire_account_id = account_id.as_deref().and_then(display_text),
        Enrichment::Claude { org_uuid, .. } => rec.org_uuid = org_uuid.as_deref().and_then(display_text),
    }

    // 4. Save before fetching, so a fetch failure cannot lose the account.
    store.save(&rec)?;

    // 5. Install the live object, then fetch and record the model list (best effort). Its tokens
    //    renew at the endpoint that issued them: the spec's in the app, a mock's in a test.
    let auth = super::auth::AccountAuth::with_endpoints(
        rec.clone(),
        store.clone(),
        http.clone(),
        open_expired.clone(),
        attempt.endpoints.clone(),
    );
    let fetched = match super::registry::fetch_models(&auth, provider, http, &attempt.endpoints.project_api_url).await {
        Ok(models) => models,
        Err(e) => {
            // Logged, not returned: the sign-in succeeded, and an empty list only means the row
            // shows defaults until the user presses Check again.
            tracing::warn!("could not fetch the model list for {id}: {e}");
            Vec::new()
        }
    };
    let at = Utc::now();
    if !fetched.is_empty() {
        if let Err(e) = auth.store_models(fetched.clone(), at).await {
            tracing::warn!("could not save the model list for {id}: {e}");
        }
    }

    // 6. The chosen model, never empty: the driver refuses an account without one, and the row's
    //    picker would have nothing to show. The spec's preference order decides, not the order
    //    the provider happened to list its ids in.
    let model = default_model(spec, &fetched).unwrap_or_default();

    let view = auth.view();
    Ok(AccountView { model, models: fetched, models_fetched_at: Some(at), ..view })
}

/// The message an account row shows for a finished sign-in.
///
/// `Cancelled` is not a failure: cancelling is something the user did on purpose, and the row
/// must fall back to its "Sign in" state rather than display an error about it.
pub fn login_error_message(e: &AuthError) -> Option<String> {
    match e {
        AuthError::Cancelled => None,
        other => Some(other.to_string()),
    }
}

/// `POST {token}` for the attempt's provider, with the code. Kept here so the provider modules
/// only add what makes them different.
///
/// The body is a struct per provider because field order is observable on the wire for Claude; the
/// form path builds its pairs in a fixed order for the same reason (the providers' logs quote
/// bodies, and a stably ordered body makes a captured request comparable to the next one).
pub(crate) async fn post_exchange(
    http: &reqwest::Client,
    attempt: &LoginAttempt,
    code: &str,
    state: Option<&str>,
) -> Result<(TokenResponse, serde_json::Value), AuthError> {
    let spec = attempt.provider.spec();
    let body = ExchangeBody {
        grant_type: "authorization_code",
        code,
        redirect_uri: attempt.redirect_uri.as_str(),
        client_id: spec.client_id,
        client_secret: spec.client_secret,
        code_verifier: attempt.verifier(),
        state,
        scope: spec.scopes.join(" "),
    };
    let url = &attempt.endpoints.token_url;
    let (status, _headers, bytes) = match spec.token_encoding {
        super::TokenEncoding::Form => {
            crate::auth::oauth::send_token_request(http, url, "application/x-www-form-urlencoded", body.to_form()).await?
        }
        super::TokenEncoding::Json => {
            let text = serde_json::to_string(&body)?;
            crate::auth::oauth::send_token_request(http, url, "application/json", text).await?
        }
    };
    crate::auth::oauth::split_token_body(status, &bytes)
}

/// One authorization-code exchange. `#[serde(skip_serializing_if)]` keeps each provider's body to
/// the fields its endpoint accepts: Google rejects an unknown field, and Anthropic wants `state`.
#[derive(serde::Serialize)]
struct ExchangeBody<'a> {
    grant_type: &'static str,
    code: &'a str,
    redirect_uri: &'a str,
    client_id: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    client_secret: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    code_verifier: Option<&'a str>,
    /// Claude only: the state from the `code#state` fragment. The others ignore it and must not
    /// receive it (Google rejects unknown parameters on this endpoint).
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<&'a str>,
    /// Form path only; JSON providers take no scope on the exchange.
    #[serde(skip)]
    scope: String,
}

impl ExchangeBody<'_> {
    /// Form encoding in the field order a captured request shows. `serde(skip)` above keeps the
    /// scope out of the JSON body.
    fn to_form(&self) -> String {
        let mut pairs: Vec<(&str, &str)> = vec![
            ("grant_type", self.grant_type),
            ("code", self.code),
            ("client_id", self.client_id),
            ("redirect_uri", self.redirect_uri),
        ];
        if let Some(s) = self.client_secret {
            pairs.push(("client_secret", s));
        }
        if let Some(v) = self.code_verifier {
            pairs.push(("code_verifier", v));
        }
        if let Some(s) = self.state {
            pairs.push(("state", s));
        }
        pairs.push(("scope", &self.scope));
        url::form_urlencoded::Serializer::new(String::new()).extend_pairs(pairs).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_attempt_holds_no_token_and_hides_its_verifier() {
        let p = AccountProvider::ClaudeCode;
        let attempt = LoginAttempt::new(1, p, ProviderEndpoints::for_spec(p.spec())).unwrap();
        let text = format!("{attempt:?}");
        assert!(text.contains("has_pkce: true"), "{text}");
        assert!(!text.contains(&attempt.state), "the state is a credential too: {text}");
        let v = attempt.verifier().unwrap();
        assert!(!text.contains(v), "the verifier must not print: {text}");
    }

    #[test]
    fn the_attempt_url_matches_the_registered_redirect_and_carries_state() {
        for p in [AccountProvider::Antigravity, AccountProvider::Codex, AccountProvider::ClaudeCode] {
            let attempt = LoginAttempt::new(1, p, ProviderEndpoints::for_spec(p.spec())).unwrap();
            assert_eq!(attempt.redirect_uri, p.spec().redirect_uri(), "{p:?}");
            let q: std::collections::HashMap<String, String> = attempt.auth_url.query_pairs().into_owned().collect();
            assert_eq!(q["redirect_uri"], p.spec().redirect_uri(), "{p:?}");
            assert_eq!(q["state"], attempt.state(), "{p:?}");
            assert_eq!(q.contains_key("code_challenge"), p.spec().pkce, "{p:?}");
            assert_eq!(q["client_id"], p.spec().client_id, "{p:?}");
        }
    }

    #[test]
    fn every_flow_uses_a_128_character_pkce_verifier() {
        // Antigravity included: its client secret ships in a desktop app, so only the verifier
        // keeps a code caught on the loopback from being exchanged by someone else.
        for p in [AccountProvider::Antigravity, AccountProvider::Codex, AccountProvider::ClaudeCode] {
            assert!(p.spec().pkce, "{p:?}");
            let attempt = LoginAttempt::new(1, p, ProviderEndpoints::for_spec(p.spec())).unwrap();
            let v = attempt.verifier().unwrap();
            assert_eq!(v.len(), 128, "{p:?}");
            let q: std::collections::HashMap<String, String> = attempt.auth_url.query_pairs().into_owned().collect();
            assert_eq!(q["code_challenge_method"], "S256", "{p:?}");
        }
    }

    #[test]
    fn an_empty_identity_is_refused_before_anything_is_written() {
        // The epilogue's guard, exercised without a network call: no email and no fallback.
        let enrich = Enrichment::Claude { email: None, account_uuid: None, org_uuid: None, org_name: None };
        assert!(enrich.identity().is_none());
        let enrich = Enrichment::Codex { account_id: Some("acct-1".into()), plan: None, email: None };
        assert_eq!(enrich.identity(), Some("acct-1"));
    }

    #[test]
    fn codex_files_each_workspace_under_its_own_id_not_its_email() {
        // Two workspaces seen by one ChatGPT login share an email and nothing else; keying on the
        // email would make the second overwrite the first record, and its refresh token with it.
        let a = Enrichment::Codex { account_id: Some("acct-1".into()), plan: None, email: Some("me@example.com".into()) };
        let b = Enrichment::Codex { account_id: Some("acct-2".into()), plan: None, email: Some("me@example.com".into()) };
        assert_ne!(a.identity(), b.identity());
        assert_eq!(a.identity(), Some("acct-1"));
        // An account whose ID token carries no workspace id still gets named.
        let c = Enrichment::Codex { account_id: None, plan: None, email: Some(" me@example.com ".into()) };
        assert_eq!(c.identity(), Some(" me@example.com "));
        // Claude and Antigravity stay on the email: one account per address.
        let d = Enrichment::Claude {
            email: Some("me@example.com".into()),
            account_uuid: Some("uuid-1".into()),
            org_uuid: None,
            org_name: None,
        };
        assert_eq!(d.identity(), Some("me@example.com"));
    }
}
