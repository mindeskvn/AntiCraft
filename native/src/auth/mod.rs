//! Google OAuth (installed-app) authentication for AntiCraft (PROTOCOL.md section 1).
//!
//! [`AuthManager`] owns the cached [`store::Credentials`], refreshes them before expiry and
//! hands out bearer tokens through [`AuthManager::access_token`]. Token values never appear
//! in `Debug`/`Display` output or in logs.

pub mod antigravity;
pub mod client_config;
pub mod external;
pub mod oauth;
pub mod store;

/// The same module under a disambiguating name: [`external::antigravity`] is first-party OAuth
/// sign-in, while this one is the read-only importer of another app's Google session.
pub use self::antigravity as session_import;

use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

pub use client_config::ClientSource;
pub use oauth::{OAuthEndpoints, Pkce, TokenResponse};
pub use store::{CredentialStore, Credentials};

/// Refresh the access token when it expires within this window (PROTOCOL 1.5).
pub const REFRESH_SKEW: Duration = Duration::from_secs(5 * 60);
/// Overall timeout of the interactive browser login (PROTOCOL 1.2 step 6).
pub const LOGIN_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// Timeout of one token-endpoint refresh request.
pub const REFRESH_TIMEOUT: Duration = Duration::from_secs(30);

/// OAuth scopes requested at login (PROTOCOL 1.1).
pub const SCOPES: &[&str] = &[
    "https://www.googleapis.com/auth/cloud-platform",
    "https://www.googleapis.com/auth/userinfo.email",
    "https://www.googleapis.com/auth/userinfo.profile",
];

/// Public installed-app client ID used by gemini-cli (PROTOCOL 1.1).
pub const DEFAULT_CLIENT_ID: &str =
    "681255809395-oo8ft2oprdrnp9e3aqf6av3hmdib135j.apps.googleusercontent.com";

/// Env var (build time or run time) that supplies the installed-app client secret.
pub const CLIENT_SECRET_ENV: &str = "ANTICRAFT_OAUTH_CLIENT_SECRET";

/// Errors from login, refresh and credential storage. `Display` never contains secrets.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    /// No stored credentials; the user must log in.
    #[error("not logged in")]
    NotLoggedIn,
    /// Refresh token revoked or expired (`invalid_grant`); a new login is required.
    #[error("session expired, please log in again")]
    ReauthRequired,
    /// The browser flow was cancelled by the user or by shutdown.
    #[error("login cancelled")]
    Cancelled,
    /// The browser flow did not complete within [`LOGIN_TIMEOUT`].
    #[error("login timed out")]
    Timeout,
    /// The callback `state` did not match (possible CSRF).
    #[error("state mismatch, possible CSRF attack")]
    StateMismatch,
    /// Google redirected back with an `error` parameter, or without a code.
    #[error("authorization failed: {0}")]
    Denied(String),
    /// The token endpoint returned an OAuth error (only `error` / `error_description`).
    #[error("token endpoint error: {0}")]
    TokenEndpoint(String),
    /// The OAuth client secret was not configured.
    #[error("OAuth client is not configured (set {CLIENT_SECRET_ENV})")]
    MissingClientConfig,
    /// This build carries no Antigravity OAuth client secret (see `native/build.rs`).
    #[error("this build has no Antigravity account sign-in: build it with ANTICRAFT_ANTIGRAVITY_CLIENT_SECRET set (see README)")]
    AntigravityClientNotBuilt,
    /// Network failure; the message has the URL stripped.
    #[error("network error: {0}")]
    Http(String),
    #[error("credential storage error: {0}")]
    Io(#[from] std::io::Error),
    #[error("credential format error: {0}")]
    Json(#[from] serde_json::Error),
}

impl From<reqwest::Error> for AuthError {
    fn from(e: reqwest::Error) -> Self {
        // reqwest errors may embed the URL (tokeninfo uses a query token); strip it.
        AuthError::Http(e.without_url().to_string())
    }
}

/// OAuth client identity plus the endpoint set (overridable for mock-server tests).
#[derive(Clone)]
pub struct AuthConfig {
    pub client_id: String,
    /// Installed-app client secret. Not confidential per Google, but never logged.
    pub client_secret: String,
    pub endpoints: OAuthEndpoints,
    /// Fixed callback port (`OAUTH_CALLBACK_PORT`); `None` picks a random free port.
    pub callback_port: Option<u16>,
}

impl std::fmt::Debug for AuthConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthConfig")
            .field("client_id", &self.client_id)
            .field("client_secret", &"<redacted>")
            .field("endpoints", &self.endpoints)
            .field("callback_port", &self.callback_port)
            .finish()
    }
}

impl AuthConfig {
    /// Google production config. The secret comes from the runtime env, else the build-time env.
    pub fn from_env() -> Result<Self, AuthError> {
        let client_secret = std::env::var(CLIENT_SECRET_ENV)
            .ok()
            .or_else(|| option_env!("ANTICRAFT_OAUTH_CLIENT_SECRET").map(str::to_owned))
            .filter(|s| !s.is_empty())
            .ok_or(AuthError::MissingClientConfig)?;
        Ok(Self::with_secret(client_secret))
    }

    /// Resolve the secret from `<data_dir>\oauth_client.json`, then the runtime env, then the
    /// build-time env. Returns the config and where the secret came from.
    pub fn load(data_dir: &std::path::Path) -> Result<(Self, ClientSource), AuthError> {
        let (secret, src) = client_config::resolve(
            data_dir,
            std::env::var(CLIENT_SECRET_ENV).ok(),
            option_env!("ANTICRAFT_OAUTH_CLIENT_SECRET"),
        )
        .ok_or(AuthError::MissingClientConfig)?;
        Ok((Self::with_secret(secret), src))
    }

    fn with_secret(client_secret: String) -> Self {
        let callback_port = std::env::var("OAUTH_CALLBACK_PORT")
            .ok()
            .and_then(|p| p.parse::<u16>().ok())
            .filter(|p| *p != 0);
        Self {
            client_id: DEFAULT_CLIENT_ID.to_owned(),
            client_secret,
            endpoints: OAuthEndpoints::google(),
            callback_port,
        }
    }
}

/// Snapshot of the login state for the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthStatus {
    LoggedOut,
    LoggingIn,
    LoggedIn { email: Option<String> },
    Error(String),
}

/// Something that can supply a bearer token. Implemented by [`AuthManager`], by the per-account
/// `external::auth::AccountAuth` and by test fakes.
pub trait TokenProvider: Send + Sync {
    /// Return a valid access token, refreshing if needed.
    fn access_token(&self) -> BoxFuture<'_, Result<String, AuthError>>;
    /// Refresh after `failed` was rejected, and return a usable token.
    ///
    /// `failed` is the token the server refused. An implementation that finds the current token is
    /// no longer `failed` returns the current one **without a network call**: refresh tokens are
    /// single-use for some providers and rotating for others, so two parallel 401s must not both
    /// spend the one refresh token.
    ///
    /// One named lifetime for both `&self` and `failed`: the returned future reads `failed`
    /// (and an implementation may hold it across the await), so it cannot outlive either.
    fn force_refresh<'a>(&'a self, failed: &'a str) -> BoxFuture<'a, Result<String, AuthError>>;
}

/// Owns credentials and performs login, refresh and logout.
pub struct AuthManager {
    config: AuthConfig,
    store: CredentialStore,
    http: reqwest::Client,
    /// In-memory credentials. Held only briefly, never across network calls.
    creds: Mutex<Option<Credentials>>,
    /// Single-flight gate for refreshes (and for login writes, so they serialize with refreshes).
    refresh_gate: Mutex<()>,
}

impl AuthManager {
    pub fn new(config: AuthConfig, store: CredentialStore, http: reqwest::Client) -> Arc<Self> {
        Arc::new(Self { config, store, http, creds: Mutex::new(None), refresh_gate: Mutex::new(()) })
    }

    pub fn config(&self) -> &AuthConfig {
        &self.config
    }

    /// Load cached credentials from disk (no network). Returns whether any were found.
    pub async fn load_cached(&self) -> Result<bool, AuthError> {
        let loaded = self.store.load()?;
        let found = loaded.is_some();
        *self.creds.lock().await = loaded;
        Ok(found)
    }

    /// Current status derived from in-memory credentials.
    pub async fn status(&self) -> AuthStatus {
        match &*self.creds.lock().await {
            Some(c) => AuthStatus::LoggedIn { email: c.email.clone() },
            None => AuthStatus::LoggedOut,
        }
    }

    /// Account email, if known.
    pub async fn email(&self) -> Option<String> {
        self.creds.lock().await.as_ref().and_then(|c| c.email.clone())
    }

    /// Interactive browser login (PROTOCOL 1.2). Ends on `cancel` or after [`LOGIN_TIMEOUT`].
    pub async fn login(&self, cancel: CancellationToken) -> Result<AuthStatus, AuthError> {
        self.login_with(cancel, |url| {
            if let Err(e) = open::that(url.as_str()) {
                tracing::warn!("could not open browser: {e}");
            }
        })
        .await
    }

    /// [`Self::login`] with an injectable browser opener (used by tests and a "copy link" UI).
    pub async fn login_with(
        &self,
        cancel: CancellationToken,
        open_browser: impl FnOnce(url::Url),
    ) -> Result<AuthStatus, AuthError> {
        // Without a stored refresh token, ask for consent so Google issues a new one.
        let force_consent = self.creds.lock().await.as_ref().and_then(|c| c.refresh_token.as_ref()).is_none();
        let tokens =
            oauth::browser_login_with(&self.http, &self.config, force_consent, cancel.clone(), open_browser).await?;
        let email = tokio::select! {
            e = oauth::fetch_user_email(&self.http, &self.config.endpoints, &tokens.access_token) => e,
            _ = cancel.cancelled() => return Err(AuthError::Cancelled),
        };
        let mut update = Credentials::from_token_response(tokens);
        update.email = email;
        // Serialize with refreshes: an in-flight refresh of the old account cannot merge over
        // this login, and the file write and memory update happen under one lock.
        let _gate = self.refresh_gate.lock().await;
        let mut slot = self.creds.lock().await;
        let merged = self.store.save_merged(&update)?;
        let email = merged.email.clone();
        *slot = Some(merged);
        Ok(AuthStatus::LoggedIn { email })
    }

    /// Delete stored credentials and clear memory (PROTOCOL 1.8). No server-side revoke.
    pub async fn logout(&self) -> Result<(), AuthError> {
        let mut slot = self.creds.lock().await;
        *slot = None;
        self.store.delete()?;
        Ok(())
    }

    /// Return a valid access token, refreshing when it expires within [`REFRESH_SKEW`].
    pub async fn access_token(&self) -> Result<String, AuthError> {
        if let Some(t) = self.fresh_token().await? {
            return Ok(t);
        }
        let _gate = self.refresh_gate.lock().await;
        // Another caller may have refreshed while we waited.
        if let Some(t) = self.fresh_token().await? {
            return Ok(t);
        }
        self.refresh().await
    }

    /// Cached access token if it is not close to expiry.
    async fn fresh_token(&self) -> Result<Option<String>, AuthError> {
        let guard = self.creds.lock().await;
        let creds = guard.as_ref().ok_or(AuthError::NotLoggedIn)?;
        Ok(if creds.needs_refresh(REFRESH_SKEW) { None } else { creds.access_token.clone() })
    }

    /// Startup check (PROTOCOL 1.5): obtain a token (refreshing if needed) and confirm it with
    /// tokeninfo. Returns `Ok(false)` when there are no usable credentials. A network failure
    /// is returned as an error and leaves the cached credentials untouched.
    pub async fn verify_cached(&self) -> Result<bool, AuthError> {
        let token = match self.access_token().await {
            Ok(t) => t,
            Err(AuthError::NotLoggedIn | AuthError::ReauthRequired) => return Ok(false),
            Err(e) => return Err(e),
        };
        if oauth::token_is_valid(&self.http, &self.config.endpoints, &token).await? {
            return Ok(true);
        }
        // The access token was revoked; try one forced refresh before giving up.
        match TokenProvider::force_refresh(self, &token).await {
            Ok(t) => oauth::token_is_valid(&self.http, &self.config.endpoints, &t).await,
            Err(AuthError::NotLoggedIn | AuthError::ReauthRequired) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Logout plus best-effort server-side revoke of the refresh (or access) token.
    pub async fn logout_and_revoke(&self) -> Result<(), AuthError> {
        let token = self
            .creds
            .lock()
            .await
            .as_ref()
            .and_then(|c| c.refresh_token.clone().or_else(|| c.access_token.clone()));
        self.logout().await?;
        if let Some(t) = token {
            oauth::revoke_token(&self.http, &self.config.endpoints, &t).await;
        }
        Ok(())
    }

    /// Refresh with the token endpoint. Callers hold `refresh_gate` (single flight); the
    /// credential lock is only taken before and after the network call. If the account
    /// changed meanwhile (logout or a new login), the result is discarded. On `invalid_grant`
    /// the stored credentials are dropped so the UI falls back to a new login.
    async fn refresh(&self) -> Result<String, AuthError> {
        let (rt, email) = {
            let guard = self.creds.lock().await;
            let creds = guard.as_ref().ok_or(AuthError::NotLoggedIn)?;
            (creds.refresh_token.clone().ok_or(AuthError::ReauthRequired)?, creds.email.clone())
        };
        let result = tokio::time::timeout(REFRESH_TIMEOUT, oauth::refresh_token(&self.http, &self.config, &rt))
            .await
            .unwrap_or_else(|_| Err(AuthError::Http("token refresh timed out".into())));
        let mut slot = self.creds.lock().await;
        if slot.as_ref().and_then(|c| c.refresh_token.as_deref()) != Some(rt.as_str()) {
            // Logged out or replaced by a new login while the request was in flight.
            return match slot.as_ref().and_then(|c| c.access_token.clone()) {
                Some(t) => Ok(t),
                None => Err(AuthError::NotLoggedIn),
            };
        }
        let resp = match result {
            Ok(r) => r,
            Err(AuthError::ReauthRequired) => {
                tracing::warn!("refresh token rejected; clearing stored credentials");
                *slot = None;
                if let Err(e) = self.store.delete() {
                    tracing::warn!("could not delete credential file: {e}");
                }
                return Err(AuthError::ReauthRequired);
            }
            Err(e) => return Err(e),
        };
        let mut update = Credentials::from_token_response(resp);
        // A refresh response never carries the email; keep the cached one.
        update.email = email;
        let merged = self.store.save_merged(&update)?;
        let token = merged.access_token.clone().ok_or(AuthError::ReauthRequired)?;
        *slot = Some(merged);
        Ok(token)
    }
}

impl TokenProvider for AuthManager {
    fn access_token(&self) -> BoxFuture<'_, Result<String, AuthError>> {
        Box::pin(AuthManager::access_token(self))
    }

    fn force_refresh<'a>(&'a self, failed: &'a str) -> BoxFuture<'a, Result<String, AuthError>> {
        Box::pin(async move {
            let _gate = self.refresh_gate.lock().await;
            // A parallel 401 already refreshed, or a new login landed: the token in memory is no
            // longer the one that was refused, so it is the answer and no request is needed.
            let current = {
                let guard = self.creds.lock().await;
                guard.as_ref().and_then(|c| c.access_token.clone())
            };
            match current {
                Some(t) if t != failed => Ok(t),
                _ => self.refresh().await,
            }
        })
    }
}

/// Fixed-token provider for tests and the `GOOGLE_CLOUD_ACCESS_TOKEN`-style override.
pub struct StaticToken(pub String);

impl TokenProvider for StaticToken {
    fn access_token(&self) -> BoxFuture<'_, Result<String, AuthError>> {
        Box::pin(async move { Ok(self.0.clone()) })
    }
    /// A fixed token cannot become a different one, so `failed` is irrelevant here.
    fn force_refresh<'a>(&'a self, _failed: &'a str) -> BoxFuture<'a, Result<String, AuthError>> {
        Box::pin(async move { Ok(self.0.clone()) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        extract::{Query, State},
        http::StatusCode,
        routing::{get, post},
        Router,
    };
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[derive(Default)]
    struct Mock {
        refreshes: AtomicUsize,
        exchanges: AtomicUsize,
        revokes: AtomicUsize,
        /// When set, refresh requests fail with invalid_grant.
        reject_refresh: AtomicBool,
    }

    async fn token(State(m): State<Arc<Mock>>, body: String) -> (StatusCode, axum::Json<serde_json::Value>) {
        let form: HashMap<String, String> = url::form_urlencoded::parse(body.as_bytes()).into_owned().collect();
        assert_eq!(form["client_id"], "test-client");
        assert_eq!(form["client_secret"], "test-secret");
        match form["grant_type"].as_str() {
            "authorization_code" => {
                assert_eq!(form["code"], "good-code");
                assert!(form["redirect_uri"].starts_with("http://127.0.0.1:"));
                assert_eq!(form["code_verifier"].len(), 43);
                m.exchanges.fetch_add(1, Ordering::SeqCst);
                let body = serde_json::json!({
                    "access_token": "at-login", "expires_in": 3599, "refresh_token": "rt-login",
                    "scope": "s", "token_type": "Bearer"
                });
                (StatusCode::OK, axum::Json(body))
            }
            "refresh_token" => {
                if m.reject_refresh.load(Ordering::SeqCst) {
                    return (StatusCode::BAD_REQUEST, axum::Json(serde_json::json!({"error": "invalid_grant"})));
                }
                let n = m.refreshes.fetch_add(1, Ordering::SeqCst) + 1;
                let body = serde_json::json!({"access_token": format!("at-refresh-{n}"), "expires_in": 3599});
                (StatusCode::OK, axum::Json(body))
            }
            _ => (StatusCode::BAD_REQUEST, axum::Json(serde_json::json!({"error": "unsupported_grant_type"}))),
        }
    }

    async fn userinfo(h: axum::http::HeaderMap) -> (StatusCode, axum::Json<serde_json::Value>) {
        let ok = h.get("authorization").and_then(|v| v.to_str().ok()) == Some("Bearer at-login");
        if ok {
            (StatusCode::OK, axum::Json(serde_json::json!({"email": "user@example.com"})))
        } else {
            (StatusCode::UNAUTHORIZED, axum::Json(serde_json::json!({})))
        }
    }

    async fn tokeninfo(Query(q): Query<HashMap<String, String>>) -> StatusCode {
        if q.get("access_token").is_some_and(|t| t.starts_with("at-")) {
            StatusCode::OK
        } else {
            StatusCode::BAD_REQUEST
        }
    }

    async fn revoke(State(m): State<Arc<Mock>>, body: String) -> StatusCode {
        assert!(body.starts_with("token="));
        m.revokes.fetch_add(1, Ordering::SeqCst);
        StatusCode::OK
    }

    async fn mock() -> (Arc<Mock>, String) {
        let state = Arc::new(Mock::default());
        let app = Router::new()
            .route("/token", post(token))
            .route("/userinfo", get(userinfo))
            .route("/tokeninfo", get(tokeninfo))
            .route("/revoke", post(revoke))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (state, format!("http://{addr}"))
    }

    fn manager(base: &str, dir: &std::path::Path) -> Arc<AuthManager> {
        let config = AuthConfig {
            client_id: "test-client".into(),
            client_secret: "test-secret".into(),
            endpoints: OAuthEndpoints::with_base(base),
            callback_port: None,
        };
        AuthManager::new(config, CredentialStore::in_dir(dir), reqwest::Client::new())
    }

    /// Simulates the browser: hits redirect_uri with the (optionally tampered) state and a code.
    fn fake_browser(code: &'static str, tamper_state: bool) -> impl FnOnce(url::Url) {
        move |auth_url: url::Url| {
            let q: HashMap<String, String> = auth_url.query_pairs().into_owned().collect();
            let state = if tamper_state { "wrong".to_owned() } else { q["state"].clone() };
            let target = format!("{}?state={state}&code={code}", q["redirect_uri"]);
            tokio::spawn(async move {
                let client = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
                let _ = client.get(target).send().await;
            });
        }
    }

    #[tokio::test]
    async fn login_refresh_logout_roundtrip() {
        let (m, base) = mock().await;
        let dir = tempfile::tempdir().unwrap();
        let auth = manager(&base, dir.path());
        assert!(!auth.load_cached().await.unwrap());
        assert!(matches!(auth.access_token().await, Err(AuthError::NotLoggedIn)));

        let status = auth.login_with(CancellationToken::new(), fake_browser("good-code", false)).await.unwrap();
        assert_eq!(status, AuthStatus::LoggedIn { email: Some("user@example.com".into()) });
        assert_eq!(m.exchanges.load(Ordering::SeqCst), 1);
        assert_eq!(auth.access_token().await.unwrap(), "at-login");
        assert_eq!(m.refreshes.load(Ordering::SeqCst), 0, "fresh token must not refresh");

        // Persisted and reloadable by a new manager.
        let path = dir.path().join("oauth_creds.json");
        assert!(path.exists());
        let auth2 = manager(&base, dir.path());
        assert!(auth2.load_cached().await.unwrap());
        assert_eq!(auth2.email().await.as_deref(), Some("user@example.com"));
        assert!(auth2.verify_cached().await.unwrap());

        // Forced refresh keeps the refresh token and the email.
        assert_eq!(TokenProvider::force_refresh(&*auth, "at-login").await.unwrap(), "at-refresh-1");
        // A token that is not the one that was refused is already newer: no request is made.
        assert_eq!(TokenProvider::force_refresh(&*auth, "at-login").await.unwrap(), "at-refresh-1");
        assert_eq!(m.refreshes.load(Ordering::SeqCst), 1);
        let stored = CredentialStore::in_dir(dir.path()).load().unwrap().unwrap();
        assert_eq!(stored.refresh_token.as_deref(), Some("rt-login"));
        assert_eq!(stored.email.as_deref(), Some("user@example.com"));

        auth.logout_and_revoke().await.unwrap();
        assert!(!path.exists());
        assert_eq!(m.revokes.load(Ordering::SeqCst), 1);
        assert_eq!(auth.status().await, AuthStatus::LoggedOut);
    }

    #[tokio::test]
    async fn refreshes_when_near_expiry() {
        let (m, base) = mock().await;
        let dir = tempfile::tempdir().unwrap();
        let store = CredentialStore::in_dir(dir.path());
        store
            .save_merged(&Credentials {
                access_token: Some("old".into()),
                refresh_token: Some("rt".into()),
                // Expires in 60 s, inside REFRESH_SKEW.
                expiry_date: Some(chrono::Utc::now().timestamp_millis() + 60_000),
                ..Default::default()
            })
            .unwrap();
        let auth = manager(&base, dir.path());
        assert!(auth.load_cached().await.unwrap());
        assert_eq!(auth.access_token().await.unwrap(), "at-refresh-1");
        assert_eq!(auth.access_token().await.unwrap(), "at-refresh-1");
        assert_eq!(m.refreshes.load(Ordering::SeqCst), 1);
        let stored = store.load().unwrap().unwrap();
        assert_eq!(stored.refresh_token.as_deref(), Some("rt"));
        assert!(stored.expiry_date.unwrap() > chrono::Utc::now().timestamp_millis() + 3_000_000);
    }

    #[tokio::test]
    async fn concurrent_callers_share_one_refresh() {
        let (m, base) = mock().await;
        let dir = tempfile::tempdir().unwrap();
        CredentialStore::in_dir(dir.path())
            .save_merged(&Credentials { refresh_token: Some("rt".into()), ..Default::default() })
            .unwrap();
        let auth = manager(&base, dir.path());
        auth.load_cached().await.unwrap();
        let (a, b) = tokio::join!(auth.access_token(), auth.access_token());
        assert_eq!(a.unwrap(), b.unwrap());
        assert_eq!(m.refreshes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn invalid_grant_clears_credentials() {
        let (m, base) = mock().await;
        m.reject_refresh.store(true, Ordering::SeqCst);
        let dir = tempfile::tempdir().unwrap();
        let store = CredentialStore::in_dir(dir.path());
        store
            .save_merged(&Credentials {
                access_token: Some("x".into()),
                refresh_token: Some("rt".into()),
                expiry_date: Some(0),
                ..Default::default()
            })
            .unwrap();
        let auth = manager(&base, dir.path());
        auth.load_cached().await.unwrap();
        assert!(matches!(auth.access_token().await, Err(AuthError::ReauthRequired)));
        assert!(store.load().unwrap().is_none());
        assert_eq!(auth.status().await, AuthStatus::LoggedOut);
        assert!(!auth.verify_cached().await.unwrap());
    }

    #[tokio::test]
    async fn login_state_mismatch_and_cancel() {
        let (m, base) = mock().await;
        let dir = tempfile::tempdir().unwrap();
        let auth = manager(&base, dir.path());
        // A forged state is answered and ignored: the flow keeps waiting for the real
        // callback and never exchanges the injected code.
        let cancel = CancellationToken::new();
        let c1 = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(800)).await;
            c1.cancel();
        });
        let r = auth.login_with(cancel, fake_browser("good-code", true)).await;
        assert!(matches!(r, Err(AuthError::Cancelled)), "{r:?}");
        assert_eq!(m.exchanges.load(Ordering::SeqCst), 0);

        let cancel = CancellationToken::new();
        let c2 = cancel.clone();
        let r = auth.login_with(cancel, move |_| c2.cancel()).await;
        assert!(matches!(r, Err(AuthError::Cancelled)));
        assert!(!dir.path().join("oauth_creds.json").exists());
    }

    #[test]
    fn config_debug_redacts_secret() {
        let c = AuthConfig {
            client_id: "id".into(),
            client_secret: "SUPER-SECRET".into(),
            endpoints: OAuthEndpoints::google(),
            callback_port: None,
        };
        assert!(!format!("{c:?}").contains("SUPER-SECRET"));
    }
}
