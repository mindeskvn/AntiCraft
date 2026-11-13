//! [`Accounts`]: the store plus the live [`AccountAuth`] of every signed-in account.
//!
//! This is the app's single door to subscription accounts. The backend asks it for a token source,
//! starts and finishes sign-ins, signs out and requests model lists; it never touches a file or an
//! `AccountAuth` directly.
//!
//! **The expiry ordering here is load-bearing.** When a refresh finds `invalid_grant`, the account
//! object is dead but callers may still hold it. The sink installed in [`Accounts::new`] removes
//! the id from `live` *first* and only then forwards to the app, so by the time the app handles
//! "this account expired", [`Accounts::auth`] already answers `None` and the UI can honestly say
//! "needs signing in again" instead of handing out a dead token source for ever.
//!
//! Login ids come from one counter: they are the stale-report guard (the app drops a completion
//! whose id no longer matches the attempt it remembers), so they must never repeat.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use chrono::{DateTime, Utc};

use super::auth::{AccountAuth, ExpiredSink};
use super::catalog;
use super::login::{self, Completed, LoginAttempt, ProviderEndpoints};
use super::store::{AccountRecord, AccountStore};
use super::{AccountProvider, AccountView};
use crate::auth::AuthError;

/// Every account known to the app: the store, plus the live auth object of each.
pub struct Accounts {
    http: reqwest::Client,
    store: AccountStore,
    data_dir: PathBuf,
    /// id -> auth. A `BTreeMap` so iteration is stable (the Account tab shows a fixed order).
    live: std::sync::Mutex<BTreeMap<String, Arc<AccountAuth>>>,
    /// The sink handed to each `AccountAuth`: removes from `live`, then calls the app's own sink
    /// (captured in the closure, never stored — an `Arc<Self>` field would be the cycle).
    sink: ExpiredSink,
    next_login: AtomicU64,
}

impl std::fmt::Debug for Accounts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let n = self.live.lock().map(|l| l.len()).unwrap_or(0);
        f.debug_struct("Accounts").field("live", &n).field("dir", &self.data_dir).finish_non_exhaustive()
    }
}

impl Accounts {
    /// Build the registry. `on_expired` is wrapped so the dead object leaves `live` before the app
    /// is told anything — see the module docs.
    pub fn new(data_dir: PathBuf, http: reqwest::Client, on_expired: ExpiredSink) -> Arc<Self> {
        let caller_sink = on_expired;
        Arc::new_cyclic(|weak| {
            let inner: std::sync::Weak<Accounts> = weak.clone();
            let forward = caller_sink.clone();
            // The wrapper cannot hold a strong `Arc<Self>` (that is the cycle the design warns
            // about), so it upgrades a weak one at call time. If the registry is gone the account
            // is gone too and there is nobody to tell.
            let sink: ExpiredSink = Arc::new(move |id: String| {
                if let Some(accounts) = inner.upgrade() {
                    accounts.forget_live(&id);
                }
                forward(id);
            });
            Accounts {
                http: http.clone(),
                store: AccountStore::in_dir(&data_dir),
                data_dir,
                live: std::sync::Mutex::new(BTreeMap::new()),
                sink,
                next_login: AtomicU64::new(1),
            }
        })
    }

    /// Scan the folder and build a live object for every record. Runs the scan on a blocking
    /// thread: it is a handful of small file reads and must not park a runtime worker.
    pub async fn load_all(&self) -> Vec<AccountView> {
        let store = self.store.clone();
        let records = match tokio::task::spawn_blocking(move || store.load_all()).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("could not scan the accounts folder: {e}");
                return Vec::new();
            }
        };
        // Build the objects first and take the map once: `install` locks it too, and
        // `std::sync::Mutex` would deadlock if this loop held it while calling back in.
        let mut live_now: Vec<Arc<AccountAuth>> = Vec::new();
        for rec in records {
            // A record whose id is not a legal id is skipped rather than renamed: the store
            // already refuses such files, and inventing an id would file a token under a name
            // the settings entry does not know.
            if !super::valid_id(&rec.account_id) {
                continue;
            }
            live_now.push(AccountAuth::new(rec, self.store.clone(), self.http.clone(), self.sink.clone()));
        }
        let replaced = {
            let mut live = self.live.lock().unwrap_or_else(|e| e.into_inner());
            let replaced = std::mem::take(&mut *live);
            for auth in &live_now {
                live.insert(auth.id().to_owned(), auth.clone());
            }
            replaced
        };
        // An object a caller still holds hands over to the one now in `live` for its id.
        for (id, old) in replaced {
            if let Some(next) = live_now.iter().find(|a| a.id() == id) {
                old.retire(next.clone());
            }
        }
        // `view()` is async, so it is awaited without the lock held.
        let mut out = Vec::with_capacity(live_now.len());
        for auth in live_now {
            out.push(auth.view());
        }
        out
    }

    /// Create and register the live object for a record, replacing any previous one for the id.
    ///
    /// `endpoints` are the ones the sign-in that wrote the record used, so its tokens are renewed
    /// at the endpoint that issued them. In the app those are the spec's, exactly what
    /// [`AccountAuth::new`] would pick; a test's sign-in runs against a mock server, and its
    /// account must not then refresh against the real provider.
    ///
    /// The object it replaces is retired into the new one: a turn that still holds it gets the
    /// new session's tokens, and it never writes the account file again.
    fn install(&self, rec: AccountRecord, endpoints: ProviderEndpoints) -> Arc<AccountAuth> {
        let auth = AccountAuth::with_endpoints(rec.clone(), self.store.clone(), self.http.clone(), self.sink.clone(), endpoints);
        let old = self.live.lock().unwrap_or_else(|e| e.into_inner()).insert(rec.account_id.clone(), auth.clone());
        if let Some(old) = old {
            old.retire(auth.clone());
        }
        auth
    }

    fn forget_live(&self, id: &str) {
        let mut live = self.live.lock().unwrap_or_else(|e| e.into_inner());
        live.remove(id);
    }

    /// [`Self::forget_live`], but only while `auth` is still the object listed for `id`: a sign-in
    /// that finished meanwhile listed one of its own, and that one stays.
    fn forget_live_if(&self, id: &str, auth: &Arc<AccountAuth>) {
        let mut live = self.live.lock().unwrap_or_else(|e| e.into_inner());
        if live.get(id).is_some_and(|current| Arc::ptr_eq(current, auth)) {
            live.remove(id);
        }
    }

    /// The live auth for an id, if it is still signed in.
    pub fn auth(&self, id: &str) -> Option<Arc<AccountAuth>> {
        self.live.lock().unwrap_or_else(|e| e.into_inner()).get(id).cloned()
    }

    /// Generate state, PKCE (when the spec has it) and the authorize URL. Does **not** bind.
    ///
    /// Nothing is retained: the returned attempt owns everything the flow needs, so an abandoned
    /// sign-in cannot leave half a flow behind in here. Tests use it to point the attempt at a
    /// mock server as well.
    pub async fn begin_login(&self, provider: AccountProvider) -> Result<Arc<LoginAttempt>, AuthError> {
        self.begin_login_against(provider, ProviderEndpoints::for_spec(provider.spec()))
    }

    /// [`Self::begin_login`] with explicit endpoints — the only way to drive a whole sign-in
    /// against a mock server, and the reason every flow function takes its endpoints as a value.
    pub fn begin_login_against(
        &self,
        provider: AccountProvider,
        endpoints: ProviderEndpoints,
    ) -> Result<Arc<LoginAttempt>, AuthError> {
        let id = self.next_login.fetch_add(1, Ordering::SeqCst);
        Ok(Arc::new(LoginAttempt::new(id, provider, endpoints)?))
    }

    /// Bind the fixed port, open the browser, race callback/cancel/timeout, exchange, enrich,
    /// save, install and fetch models.
    pub async fn run_loopback(
        &self,
        attempt: Arc<LoginAttempt>,
        open: impl FnOnce(url::Url),
    ) -> Result<AccountView, AuthError> {
        let view = login::run_loopback(&attempt, &self.store, &self.http, &self.sink, open).await?;
        self.after_login(&view, attempt.endpoints());
        Ok(view)
    }

    /// Finish a retained attempt with a pasted redirect URL. Same state check, same verifier;
    /// `cancel` stops the exchange (see [`login::complete_with_pasted`]).
    pub async fn complete_with_pasted(
        &self,
        attempt: &Arc<LoginAttempt>,
        link: &str,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<AccountView, AuthError> {
        let view = login::complete_with_pasted(attempt, &self.store, &self.http, &self.sink, link, cancel).await?;
        self.after_login(&view, attempt.endpoints());
        Ok(view)
    }

    /// Re-install the record a finished sign-in wrote, so `live` and the file agree.
    ///
    /// `finish_login` saved the record but built its `AccountAuth` only to fetch the model list;
    /// this replaces whatever `live` held for that id with an object over the saved record, which
    /// is also what renews a session by signing in again.
    fn after_login(&self, view: &AccountView, endpoints: &ProviderEndpoints) {
        if let Ok(Some(rec)) = self.store.load(&view.id) {
            self.install(rec, endpoints.clone());
        }
    }

    pub async fn logout(&self, id: &str) -> Result<(), AuthError> {
        // Under the object's gate, so a refresh in flight cannot write the file back. That wait
        // can be long; a sign-in that finishes during it keeps its file and its live object.
        if let Some(auth) = self.auth(id) {
            auth.logout().await?;
            self.forget_live_if(id, &auth);
        } else {
            self.store.delete(id)?;
        }
        Ok(())
    }

    /// Model list for the popup. `refresh == false` returns the cached list without a request.
    ///
    /// The bearer always comes from the live object, so a fetch for an account that was just
    /// signed out fails here instead of sending a dead token.
    pub async fn models(&self, id: &str, refresh: bool) -> Result<Vec<String>, AuthError> {
        let Some(auth) = self.auth(id) else { return Err(AuthError::NotLoggedIn) };
        let Some(record) = auth.record().await else { return Err(AuthError::NotLoggedIn) };
        if !refresh {
            return Ok(record.models);
        }
        let provider = auth.provider();
        let fetched = fetch_models(&auth, provider, &self.http, provider.spec().base_url).await?;
        if !fetched.is_empty() {
            auth.store_models(fetched.clone(), Utc::now()).await?;
        }
        Ok(fetched)
    }

    /// Persist a fetched list (registry-side, so the file stays the source of truth).
    pub async fn store_models(&self, id: &str, models: Vec<String>, at: DateTime<Utc>) -> std::io::Result<()> {
        match self.auth(id) {
            Some(auth) => auth.store_models(models, at).await,
            None => Err(std::io::Error::new(std::io::ErrorKind::NotFound, "not signed in")),
        }
    }

    /// The folder accounts live in, for the row's "File: …" line and for tests.
    pub fn store(&self) -> &AccountStore {
        &self.store
    }

    /// The client every flow here uses. Exposed so a caller that drives an attempt itself (tests)
    /// does not have to build a second one with different timeouts.
    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

}

/// The model list for one account, by provider.
///
/// Antigravity asks its own endpoint (it is the only one with one). The other two are
/// `ModelList::Static`: their catalog for the account's plan *is* the supported set, so it is
/// stamped now and no request is made — inventing an endpoint for them would be a 404 that reads
/// like a bug.
///
/// `base` is where the fetch goes: the spec's URL in production, the attempt's during a login (so
/// a test drives the whole epilogue against a mock server and no sign-in ever reaches the live
/// API from a test binary).
pub(crate) async fn fetch_models(
    auth: &AccountAuth,
    provider: AccountProvider,
    http: &reqwest::Client,
    base: &str,
) -> Result<Vec<String>, AuthError> {
    match provider.spec().model_list {
        super::ModelList::Fetch => {
            let Some(project) = auth.project_id() else {
                return Err(AuthError::Denied(
                    "this Antigravity account has no Cloud project yet; sign in again".into(),
                ));
            };
            let token = auth.access_token_owned().await?;
            super::antigravity::fetch_models(http, base, &token, project).await
        }
        super::ModelList::Static => {
            // The catalog for this account's plan (a free ChatGPT plan sees fewer models). A
            // previous list is not merged in: it may hold ids a plan change or a catalog update
            // has since dropped, and keeping them would make the account offer models it cannot
            // run.
            let plan = auth.record().await.and_then(|rec| rec.plan);
            Ok(catalog::models_for(provider, plan.as_deref()))
        }
    }
}

/// Dispatch a code exchange to its provider. Shared by the loopback and paste paths so the two
/// cannot drift apart.
pub(crate) async fn exchange(
    http: &reqwest::Client,
    attempt: &LoginAttempt,
    code: &str,
    fragment_state: Option<&str>,
) -> Result<Completed, AuthError> {
    // One provider reads a body the others ignore, and one takes no code at all (its endpoint
    // finds it in the redirect it already answered).
    match attempt.provider {
        AccountProvider::Antigravity => super::antigravity::exchange(http, attempt, code).await,
        AccountProvider::Codex => super::codex::exchange(http, attempt, code).await,
        AccountProvider::ClaudeCode => {
            super::claude::exchange(http, attempt, code, fragment_state.unwrap_or_default()).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::external::AccountStatus;
    use axum::{extract::State, http::StatusCode, routing::{get, post}, Router};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// A Codex-shaped ID token: a real three-segment JWT so `jwt::Claims` is exercised for real,
    /// with the workspace id and the plan inside the namespaced claim, where OpenAI puts them.
    pub(crate) fn codex_id_token() -> String {
        use base64::Engine as _;
        let enc = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        format!(
            "{}.{}.{}",
            enc.encode(br#"{"alg":"none"}"#),
            enc.encode(
                serde_json::to_vec(&serde_json::json!({
                    "email": "codex@example.com",
                    crate::auth::external::jwt::OPENAI_AUTH_CLAIM: {
                        "chatgpt_account_id": "acct-42",
                        "chatgpt_plan_type": "plus",
                    },
                }))
                .unwrap()
            ),
            enc.encode(b"signature"),
        )
    }

    /// What the mock provider saw, and what it should answer.
    #[derive(Default)]
    struct Mock {
        exchanges: AtomicUsize,
        refreshes: AtomicUsize,
        onboard_polls: AtomicUsize,
        model_fetches: AtomicUsize,
        /// `true`: `loadCodeAssist` names the project; `false`: onboarding is needed.
        has_project: AtomicBool,
        /// When set, a refresh answers Anthropic's nested `invalid_grant`.
        reject_refresh: AtomicBool,
        /// Refreshes answered with that `invalid_grant`: proof the refresh reached this mock and
        /// not the provider's real endpoint.
        rejected: AtomicUsize,
    }

    async fn token(State(m): State<Arc<Mock>>, body: String) -> (StatusCode, axum::Json<serde_json::Value>) {
        // One handler for both encodings: a JSON body is Claude's (exchange or refresh), a form
        // body is the other two. Sniffing is what a test needs; a real endpoint accepts only its
        // own.
        let json = body.trim_start().starts_with('{');
        let form: HashMap<String, String> = if json {
            let v: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
            v.as_object()
                .map(|o| o.iter().filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned()))).collect())
                .unwrap_or_default()
        } else {
            url::form_urlencoded::parse(body.as_bytes()).into_owned().collect()
        };
        match form.get("grant_type").map(String::as_str) {
            Some("authorization_code") if json => {
                assert_eq!(form["client_id"], "9d1c250a-e61b-44d9-88ed-5944d1962f5e");
                assert_eq!(form["code_verifier"].len(), 128, "PKCE verifier");
                m.exchanges.fetch_add(1, Ordering::SeqCst);
                (
                    StatusCode::OK,
                    axum::Json(serde_json::json!({
                        "access_token": "at-claude",
                        "expires_in": 3600,
                        "refresh_token": "rt-claude",
                        "account": {"uuid": "acct-uuid", "email_address": "claude@example.com"},
                        "organization": {"uuid": "org-uuid", "name": "Example Org"},
                    })),
                )
            }
            Some("authorization_code") => {
                assert_eq!(form["code"], "good-code");
                // Both form-encoded flows (Antigravity, Codex) send their verifier.
                assert_eq!(form.get("code_verifier").map(String::len), Some(128), "PKCE verifier");
                m.exchanges.fetch_add(1, Ordering::SeqCst);
                (
                    StatusCode::OK,
                    axum::Json(serde_json::json!({
                        "access_token": "at-first",
                        "expires_in": 3600,
                        "refresh_token": "rt-first",
                        "id_token": codex_id_token(),
                    })),
                )
            }
            Some("refresh_token") => {
                if m.reject_refresh.load(Ordering::SeqCst) {
                    m.rejected.fetch_add(1, Ordering::SeqCst);
                    // Anthropic's nested shape: the parser has to read it or the account is never
                    // cleaned up.
                    return (
                        StatusCode::BAD_REQUEST,
                        axum::Json(serde_json::json!({"error": {"type": "invalid_grant", "message": "dead"}})),
                    );
                }
                let n = m.refreshes.fetch_add(1, Ordering::SeqCst) + 1;
                (
                    StatusCode::OK,
                    axum::Json(serde_json::json!({
                        "access_token": format!("at-refresh-{n}"),
                        "refresh_token": format!("rt-rotated-{n}"),
                        "expires_in": 3600,
                    })),
                )
            }
            other => (
                StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({"error": format!("unknown grant {other:?}")})),
            ),
        }
    }

    async fn userinfo(h: axum::http::HeaderMap) -> (StatusCode, axum::Json<serde_json::Value>) {
        let ok = h
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("Bearer at-"));
        if ok {
            (StatusCode::OK, axum::Json(serde_json::json!({"email": "antigravity@example.com"})))
        } else {
            (StatusCode::UNAUTHORIZED, axum::Json(serde_json::json!({})))
        }
    }

    async fn profile() -> axum::Json<serde_json::Value> {
        axum::Json(serde_json::json!({
            "account": {"uuid": "acct-uuid", "email": "claude@example.com"},
            "organization": {"uuid": "org-uuid", "name": "Example Org"},
        }))
    }

    async fn load_code_assist(State(m): State<Arc<Mock>>) -> axum::Json<serde_json::Value> {
        if m.has_project.load(Ordering::SeqCst) {
            axum::Json(serde_json::json!({"cloudaicompanionProject": "projects/known"}))
        } else {
            axum::Json(serde_json::json!({"allowedTiers": [{"id": "standard", "isDefault": true}]}))
        }
    }

    async fn onboard(State(m): State<Arc<Mock>>) -> axum::Json<serde_json::Value> {
        let n = m.onboard_polls.fetch_add(1, Ordering::SeqCst);
        // The first poll is not done: the flow has to poll again, which a non-polling
        // implementation would miss.
        if n == 0 {
            axum::Json(serde_json::json!({"done": false}))
        } else {
            // The object form the service answers here, not the string `loadCodeAssist` uses.
            axum::Json(serde_json::json!({
                "done": true,
                "response": {"cloudaicompanionProject": {"id": "projects/onboarded", "name": "projects/onboarded"}},
            }))
        }
    }

    async fn available_models(State(m): State<Arc<Mock>>) -> axum::Json<serde_json::Value> {
        m.model_fetches.fetch_add(1, Ordering::SeqCst);
        // Listed in an order that puts an internal id and a non-default first, so a sign-in that
        // took the first entry would be caught.
        axum::Json(serde_json::json!({"models": {
            "chat_20706": {}, "Gemini-3-Flash-Lite": {}, "gemini-3-flash": {}, "tab_flash_lite_preview": {},
        }}))
    }

    async fn mock() -> (Arc<Mock>, String) {
        let state = Arc::new(Mock::default());
        state.has_project.store(true, Ordering::SeqCst);
        let app = Router::new()
            .route("/token", post(token))
            .route("/userinfo", get(userinfo))
            .route("/profile", get(profile))
            .route("/api/v1internal:loadCodeAssist", post(load_code_assist))
            .route("/api/v1internal:onboardUser", post(onboard))
            .route("/api/v1internal:fetchAvailableModels", post(available_models))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (state, format!("http://{addr}"))
    }

    fn accounts(dir: &std::path::Path) -> Arc<Accounts> {
        let sink: ExpiredSink = Arc::new(|_id: String| {});
        Accounts::new(dir.to_path_buf(), reqwest::Client::new(), sink)
    }

    /// Upper bound on every wait in this module. A sign-in whose callback never arrives must fail
    /// the test in seconds rather than wait out `LOGIN_TIMEOUT` (an earlier run hung for five
    /// minutes per test).
    const WAIT: std::time::Duration = std::time::Duration::from_secs(20);

    /// `f`, or a panic naming `what` once [`WAIT`] has passed.
    async fn bounded<T>(what: &str, f: impl std::future::Future<Output = T>) -> T {
        tokio::time::timeout(WAIT, f)
            .await
            .unwrap_or_else(|_| panic!("{what} did not finish within {} s", WAIT.as_secs()))
    }

    /// A whole loopback sign-in through the fake browser, bounded.
    async fn sign_in(accounts: &Accounts, a: &Arc<LoginAttempt>) -> Result<AccountView, AuthError> {
        bounded("the sign-in", accounts.run_loopback(a.clone(), browser(a, "good-code"))).await
    }

    /// Simulates the browser: opens the redirect the auth URL names, with the flow's own state.
    ///
    /// `port` is the port the test made the flow bind. Production binds `redirect_uri`'s own port,
    /// so this substitution exists only because a test must not hold the registered one. The host
    /// is `127.0.0.1`, not `localhost`: `localhost` may resolve to `::1`, where another process can
    /// hold the same port number and swallow the callback, and the flow's IPv4 listener is the one
    /// it always has.
    fn browser<'a>(attempt: &'a Arc<LoginAttempt>, code: &'static str) -> impl FnOnce(url::Url) + 'a {
        move |auth_url: url::Url| {
            let q: HashMap<String, String> = auth_url.query_pairs().into_owned().collect();
            let path = url::Url::parse(&q["redirect_uri"]).unwrap().path().to_owned();
            let port = attempt.bound_port().expect("the flow bound a port before opening the browser");
            let target = format!("http://127.0.0.1:{port}{path}?state={}&code={code}", q["state"]);
            tokio::spawn(async move {
                let client = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
                let _ = client.get(target).send().await;
            });
        }
    }

    /// An attempt bound to an OS-assigned port, so parallel tests cannot collide. The port is
    /// read back with `bound_port` inside the browser closure, after the bind.
    fn attempt(accounts: &Accounts, p: AccountProvider, base: &str) -> Arc<LoginAttempt> {
        let attempt = accounts.begin_login_against(p, ProviderEndpoints::with_base(base)).unwrap();
        let attempt = Arc::try_unwrap(attempt)
            .map_err(|_| ())
            .expect("a fresh attempt has one owner")
            .with_bind_port(0);
        Arc::new(attempt)
    }

    #[tokio::test]
    async fn antigravity_login_discovers_an_existing_project_and_stores_the_account() {
        let (m, base) = mock().await;
        let dir = tempfile::tempdir().unwrap();
        let accounts = accounts(dir.path());
        let p = AccountProvider::Antigravity;
        let a = attempt(&accounts, p, &base);
        let view = sign_in(&accounts, &a).await.unwrap();

        assert_eq!(view.provider, p);
        assert_eq!(view.email.as_deref(), Some("antigravity@example.com"));
        assert_eq!(view.status, AccountStatus::SignedIn);
        assert_eq!(view.id, crate::auth::external::account_id(p, "antigravity@example.com"));
        assert!(crate::auth::external::valid_id(&view.id));
        assert_eq!(m.exchanges.load(Ordering::SeqCst), 1);
        assert_eq!(m.onboard_polls.load(Ordering::SeqCst), 0, "the project was in the first answer");
        assert_eq!(m.model_fetches.load(Ordering::SeqCst), 1);

        let rec = accounts.store().load(&view.id).unwrap().unwrap();
        assert_eq!(rec.project_id.as_deref(), Some("projects/known"));
        assert_eq!(rec.models, ["gemini-3-flash", "gemini-3-flash-lite"], "lowercased, sorted, internal ids dropped");
        assert_eq!(view.model, "gemini-3-flash", "the spec's first choice, not the list's first entry");
        assert!(!format!("{rec:?}").contains("at-first"), "the record's Debug is redacted");
        assert!(accounts.store().dir().join(format!("{}.json", view.id)).exists());
    }

    #[tokio::test]
    async fn antigravity_login_onboards_when_no_project_is_named() {
        let (m, base) = mock().await;
        m.has_project.store(false, Ordering::SeqCst);
        let dir = tempfile::tempdir().unwrap();
        let accounts = accounts(dir.path());
        let p = AccountProvider::Antigravity;
        let a = attempt(&accounts, p, &base);
        let view = sign_in(&accounts, &a).await.unwrap();

        let rec = accounts.store().load(&view.id).unwrap().unwrap();
        assert_eq!(rec.project_id.as_deref(), Some("projects/onboarded"));
        let polls = m.onboard_polls.load(Ordering::SeqCst);
        assert!(polls >= 2, "the mock says 'not done' once, so the flow must poll again: {polls}");
        assert!(polls <= 5, "five polls at most: {polls}");
    }

    #[tokio::test]
    async fn codex_login_reads_the_account_id_from_the_id_token() {
        let (_m, base) = mock().await;
        let dir = tempfile::tempdir().unwrap();
        let accounts = accounts(dir.path());
        let p = AccountProvider::Codex;
        let a = attempt(&accounts, p, &base);
        let view = sign_in(&accounts, &a).await.unwrap();

        assert_eq!(view.email.as_deref(), Some("codex@example.com"));
        assert_eq!(view.plan.as_deref(), Some("plus"));
        let rec = accounts.store().load(&view.id).unwrap().unwrap();
        assert_eq!(rec.wire_account_id.as_deref(), Some("acct-42"), "sent as Chatgpt-Account-Id");
        assert_eq!(rec.models, crate::auth::external::catalog::models_for(p, Some("plus")), "the plan's catalog is the list");
        assert_eq!(view.model, "gpt-6.1-sol", "the spec's first choice, which the plan includes");
        assert_eq!(view.models, rec.models);
    }

    #[tokio::test]
    async fn claude_login_reads_the_account_and_organization() {
        let (_m, base) = mock().await;
        let dir = tempfile::tempdir().unwrap();
        let accounts = accounts(dir.path());
        let p = AccountProvider::ClaudeCode;
        let a = attempt(&accounts, p, &base);
        let view = sign_in(&accounts, &a).await.unwrap();

        assert_eq!(view.email.as_deref(), Some("claude@example.com"));
        assert_eq!(view.plan.as_deref(), Some("Example Org"));
        let rec = accounts.store().load(&view.id).unwrap().unwrap();
        assert_eq!(rec.org_uuid.as_deref(), Some("org-uuid"));
        assert!(!view.model.is_empty());
        assert!(rec.models.iter().all(|m| !m.is_empty()));
    }

    #[tokio::test]
    async fn a_forged_callback_state_is_ignored_and_the_real_one_still_lands() {
        let (_m, base) = mock().await;
        let dir = tempfile::tempdir().unwrap();
        let accounts = accounts(dir.path());
        let p = AccountProvider::Codex;
        let a = attempt(&accounts, p, &base);
        let login = accounts.run_loopback(a.clone(), |auth_url: url::Url| {
            let q: HashMap<String, String> = auth_url.query_pairs().into_owned().collect();
            let path = url::Url::parse(&q["redirect_uri"]).unwrap().path().to_owned();
            let port = a.bound_port().expect("bound before the browser opened");
            // 127.0.0.1 for the reason `browser` gives.
            let redirect = format!("http://127.0.0.1:{port}{path}");
            let state = q["state"].clone();
            tokio::spawn(async move {
                let client = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
                // A forged state first: it must not end the flow.
                let _ = client.get(format!("{redirect}?state=forged&code=evil")).send().await;
                let _ = client.get(format!("{redirect}?state={state}&code=good-code")).send().await;
            });
        });
        let view = bounded("the sign-in", login).await.unwrap();
        assert_eq!(view.provider, p);
    }

    #[tokio::test]
    async fn a_busy_port_still_finishes_through_the_paste_path() {
        let (m, base) = mock().await;
        let dir = tempfile::tempdir().unwrap();
        let accounts = accounts(dir.path());
        let p = AccountProvider::Codex;
        let a = accounts
            .begin_login_against(p, ProviderEndpoints::with_base(&base))
            .unwrap();
        assert_eq!(a.bind_port(), p.spec().redirect_port, "the app binds the registered port");
        // Hold a port and point the attempt at it, exactly as a stale instance holds the
        // registered one. Not the registered port itself: a running app or another test may hold
        // that, and this test would then fail over something it is not about.
        let held = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = held.local_addr().unwrap().port();
        let a = Arc::new(
            Arc::try_unwrap(a).map_err(|_| ()).expect("a fresh attempt has one owner").with_bind_port(port),
        );
        let err = bounded("the failed bind", accounts.run_loopback(a.clone(), |_| {})).await.unwrap_err();
        let message = crate::auth::external::login::login_error_message(&err).unwrap();
        assert!(message.contains(&port.to_string()), "the message names the port: {message}");
        assert!(message.to_lowercase().contains("paste"), "and points at the escape hatch: {message}");
        drop(held);

        // The same attempt finishes by hand, and its own state is verified.
        let state = a.auth_url.query_pairs().find(|(k, _)| k == "state").unwrap().1.into_owned();
        let link = format!("{}?state={state}&code=good-code", a.redirect_uri);
        let view = bounded("the pasted sign-in", accounts.complete_with_pasted(&a, &link, &Default::default())).await.unwrap();
        assert_eq!(view.provider, p);
        assert!(accounts.store().load(&view.id).unwrap().is_some());
        assert_eq!(m.exchanges.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_pasted_sign_in_can_be_cancelled_while_its_exchange_hangs() {
        // A token endpoint that takes the connection and never answers.
        let hole = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", hole.local_addr().unwrap());
        let dir = tempfile::tempdir().unwrap();
        let accounts = accounts(dir.path());
        let a = accounts.begin_login_against(AccountProvider::Codex, ProviderEndpoints::with_base(&base)).unwrap();
        // The browser round trip was cancelled first: the paste must not inherit that.
        a.cancel.cancel();
        let state = a.auth_url.query_pairs().find(|(k, _)| k == "state").unwrap().1.into_owned();
        let link = format!("{}?state={state}&code=good-code", a.redirect_uri);
        let cancel = tokio_util::sync::CancellationToken::new();
        let stop = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            stop.cancel();
        });
        let result = bounded("the cancelled paste", accounts.complete_with_pasted(&a, &link, &cancel)).await;
        assert!(matches!(result, Err(AuthError::Cancelled)), "{result:?}");
        assert!(accounts.store().load_all().is_empty(), "nothing was written");
        drop(hole);
    }

    #[tokio::test]
    async fn a_paste_with_the_wrong_state_is_refused_without_an_exchange() {
        let (m, base) = mock().await;
        let dir = tempfile::tempdir().unwrap();
        let accounts = accounts(dir.path());
        let a = accounts
            .begin_login_against(AccountProvider::ClaudeCode, ProviderEndpoints::with_base(&base))
            .unwrap();
        let link = format!("{}?state=forged&code=good-code", a.redirect_uri);
        let result = bounded("the pasted sign-in", accounts.complete_with_pasted(&a, &link, &Default::default())).await;
        assert!(matches!(result, Err(AuthError::StateMismatch)), "{result:?}");
        assert_eq!(m.exchanges.load(Ordering::SeqCst), 0, "no code may be exchanged");
        // A link with no state at all is no better.
        let link = format!("{}?code=good-code", a.redirect_uri);
        let result = bounded("the pasted sign-in", accounts.complete_with_pasted(&a, &link, &Default::default())).await;
        assert!(matches!(result, Err(AuthError::StateMismatch)), "{result:?}");
    }

    #[tokio::test]
    async fn an_unreadable_paste_never_echoes_the_link() {
        let (_m, base) = mock().await;
        let dir = tempfile::tempdir().unwrap();
        let accounts = accounts(dir.path());
        let a = accounts
            .begin_login_against(AccountProvider::Codex, ProviderEndpoints::with_base(&base))
            .unwrap();
        let secret = "SECRET-FRAGMENT-0123456789";
        let err = bounded("the pasted sign-in", accounts.complete_with_pasted(&a, secret, &Default::default())).await.unwrap_err();
        let message = err.to_string();
        assert!(!message.contains(secret), "{message}");
        assert!(message.contains("That does not look like a redirect URL."), "{message}");
    }

    #[tokio::test]
    async fn every_provider_refuses_a_forged_state_and_accepts_its_own() {
        // The loopback path, for all three: a wrong state is `StateMismatch` and a code is only
        // ever produced for the flow's own state. Each provider has its own callback path, so
        // this also proves `parse_callback_at` is fed the right one.
        for p in [AccountProvider::Antigravity, AccountProvider::Codex, AccountProvider::ClaudeCode] {
            let server = crate::auth::oauth::LoopbackServer::bind_pair(None, p.spec().redirect_path).await.unwrap();
            let port = server.port();
            let path = p.spec().redirect_path;
            let state = "the-real-state";
            let c = tokio::spawn(async move {
                let hit = |target: String| async move {
                    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
                    use tokio::io::AsyncWriteExt as _;
                    s.write_all(format!("GET {target} HTTP/1.1\r\nHost: localhost:{port}\r\n\r\n").as_bytes())
                        .await
                        .unwrap();
                };
                hit(format!("{path}?state=forged&code=evil")).await;
                hit(format!("{path}?state={state}&code=real")).await;
            });
            let out = bounded("the callback", server.wait_for_callback(state)).await.unwrap();
            assert_eq!(out, crate::auth::oauth::CallbackOutcome::Code("real".into()), "{p:?}");
            bounded("the fake browser", c).await.unwrap();
        }
    }

    #[tokio::test]
    async fn a_failed_antigravity_model_fetch_is_an_error_not_a_silent_empty_list() {
        // The popup's "Check again" must be able to say why it came back empty, and the login
        // epilogue must survive it — which it does by treating any `Err` as "no list yet" and
        // falling back to the spec's default model. Both halves are asserted here.
        let dir = tempfile::tempdir().unwrap();
        let p = AccountProvider::Antigravity;
        let store = AccountStore::in_dir(dir.path());
        let rec = AccountRecord {
            creds: crate::auth::store::Credentials {
                access_token: Some("at-x".into()),
                refresh_token: Some("rt-x".into()),
                expiry_date: Some(chrono::Utc::now().timestamp_millis() + 3_600_000),
                ..Default::default()
            },
            provider: p,
            account_id: crate::auth::external::account_id(p, "x@example.com"),
            label: Some("x@example.com".into()),
            plan: None,
            wire_account_id: None,
            org_uuid: None,
            project_id: None,
            models: Vec::new(),
            models_fetched_at: None,
            lifetime_secs: Some(3600),
        };
        let sink: ExpiredSink = Arc::new(|_id| {});
        let auth = crate::auth::external::auth::AccountAuth::new(rec, store, reqwest::Client::new(), sink);

        // No project yet: the error names the reason instead of returning an empty list.
        let http = reqwest::Client::new();
        let err = bounded("the model fetch", fetch_models(&auth, p, &http, "http://127.0.0.1:1")).await.unwrap_err();
        assert!(matches!(err, AuthError::Denied(_)), "{err:?}");
        assert!(err.to_string().contains("no Cloud project"), "{err}");

        // And the row it would draw still has a model, so the driver is never handed an empty one.
        let view = auth.view();
        assert_eq!(view.model, p.spec().default_models[0]);
        assert!(view.models.is_empty());
        assert_eq!(view.status, AccountStatus::SignedIn);
    }

    #[tokio::test]
    async fn re_signing_in_to_the_same_email_updates_the_same_file() {
        let (_m, base) = mock().await;
        let dir = tempfile::tempdir().unwrap();
        let accounts = accounts(dir.path());
        let p = AccountProvider::ClaudeCode;
        let mut ids = Vec::new();
        for _ in 0..2 {
            let a = attempt(&accounts, p, &base);
            ids.push(sign_in(&accounts, &a).await.unwrap().id);
        }
        assert_eq!(ids[0], ids[1], "same identity, same id");
        assert_eq!(accounts.store().load_all().len(), 1, "one file, overwritten");
    }

    #[tokio::test]
    async fn logging_out_removes_the_file_and_the_live_object() {
        let (_m, base) = mock().await;
        let dir = tempfile::tempdir().unwrap();
        let accounts = accounts(dir.path());
        let p = AccountProvider::Codex;
        let a = attempt(&accounts, p, &base);
        let view = sign_in(&accounts, &a).await.unwrap();
        assert!(accounts.auth(&view.id).is_some());
        bounded("the sign-out", accounts.logout(&view.id)).await.unwrap();
        assert!(accounts.auth(&view.id).is_none());
        assert!(accounts.store().load(&view.id).unwrap().is_none());
    }

    #[tokio::test]
    async fn load_all_rebuilds_the_live_map_from_disk() {
        let (_m, base) = mock().await;
        let dir = tempfile::tempdir().unwrap();
        let first = accounts(dir.path());
        let p = AccountProvider::ClaudeCode;
        let a = attempt(&first, p, &base);
        let view = sign_in(&first, &a).await.unwrap();

        // A second registry over the same folder, as a restart would build.
        let fresh = accounts(dir.path());
        let views = fresh.load_all().await;
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].id, view.id);
        assert!(fresh.auth(&view.id).is_some());
    }

    #[tokio::test]
    async fn a_stored_account_with_a_bad_id_is_skipped_on_load() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("accounts");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("evil.json"), b"{}").unwrap();
        std::fs::write(folder.join("claude-0123abcd.json"), b"not json").unwrap();
        let accounts = accounts(dir.path());
        assert!(accounts.load_all().await.is_empty());
    }

    #[tokio::test]
    async fn models_false_reads_the_cache_and_true_goes_to_the_provider() {
        let (m, base) = mock().await;
        let dir = tempfile::tempdir().unwrap();
        let accounts = accounts(dir.path());
        let p = AccountProvider::Antigravity;
        let a = attempt(&accounts, p, &base);
        let view = sign_in(&accounts, &a).await.unwrap();
        assert_eq!(m.model_fetches.load(Ordering::SeqCst), 1, "the login fetched once");

        let cached = accounts.models(&view.id, false).await.unwrap();
        assert_eq!(cached, view.models);
        assert_eq!(m.model_fetches.load(Ordering::SeqCst), 1, "the cached path makes no request");

        // `true` goes to the spec's live URL, which this mock does not serve; what matters is
        // that an unknown account is an error rather than an empty list.
        assert!(accounts.models("claude-deadbeef", false).await.is_err(), "an unknown id is an error");
    }

    #[tokio::test]
    async fn the_registry_installs_the_live_object_before_the_model_fetch() {
        // The fetch resolves its bearer through the record, and the epilogue passes the object it
        // built; a registry that only installed after the fetch would lose the list.
        let (_m, base) = mock().await;
        let dir = tempfile::tempdir().unwrap();
        let accounts = accounts(dir.path());
        let p = AccountProvider::Antigravity;
        let a = attempt(&accounts, p, &base);
        let view = sign_in(&accounts, &a).await.unwrap();
        assert!(accounts.auth(&view.id).is_some());
        assert!(!accounts.models(&view.id, false).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn signing_in_again_hands_the_old_object_over_to_the_new_one() {
        use crate::auth::TokenProvider as _;
        let (_m, base) = mock().await;
        let dir = tempfile::tempdir().unwrap();
        let accounts = accounts(dir.path());
        let p = AccountProvider::Codex;
        let view = sign_in(&accounts, &attempt(&accounts, p, &base)).await.unwrap();
        // What a running turn holds.
        let old = accounts.auth(&view.id).unwrap();

        let again = sign_in(&accounts, &attempt(&accounts, p, &base)).await.unwrap();
        assert_eq!(again.id, view.id, "the same workspace is the same account");
        let new = accounts.auth(&view.id).unwrap();
        assert!(!Arc::ptr_eq(&old, &new));
        // The old object answers with the live session…
        assert_eq!(bounded("a token", old.access_token()).await.unwrap(), new.peek_access_token().await.unwrap());
        // …and signing it out, as a sign-out that was waiting behind a refresh would, leaves the
        // new sign-in alone: its object stays listed and its file stays.
        bounded("the old sign-out", old.logout()).await.unwrap();
        accounts.forget_live_if(&view.id, &old);
        assert!(accounts.auth(&view.id).is_some_and(|a| Arc::ptr_eq(&a, &new)));
        assert!(accounts.store().load(&view.id).unwrap().is_some());

        // Signing out of the account itself still works.
        bounded("the sign-out", accounts.logout(&view.id)).await.unwrap();
        assert!(accounts.auth(&view.id).is_none());
        assert!(accounts.store().load(&view.id).unwrap().is_none());
    }

    #[tokio::test]
    async fn an_expired_account_leaves_live_before_the_app_is_told() {
        let (m, base) = mock().await;
        let dir = tempfile::tempdir().unwrap();

        // The app's own sink records whether `auth(id)` was already `None` when it fired.
        let seen: Arc<std::sync::Mutex<Vec<(String, bool)>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let weak: Arc<std::sync::Mutex<std::sync::Weak<Accounts>>> =
            Arc::new(std::sync::Mutex::new(std::sync::Weak::<Accounts>::new()));
        let sink: ExpiredSink = {
            let seen = seen.clone();
            let weak = weak.clone();
            Arc::new(move |id: String| {
                let gone = weak
                    .lock()
                    .unwrap()
                    .upgrade()
                    .map(|a| a.auth(&id).is_none())
                    .unwrap_or(true);
                seen.lock().unwrap().push((id, gone));
            })
        };
        let accounts = Accounts::new(dir.path().to_path_buf(), reqwest::Client::new(), sink);
        *weak.lock().unwrap() = Arc::downgrade(&accounts);

        let p = AccountProvider::ClaudeCode;
        let a = attempt(&accounts, p, &base);
        let view = sign_in(&accounts, &a).await.unwrap();
        let auth = accounts.auth(&view.id).unwrap();

        m.reject_refresh.store(true, Ordering::SeqCst);
        let failed = auth.peek_access_token().await.unwrap();
        let err = bounded("the refresh", crate::auth::TokenProvider::force_refresh(&*auth, &failed)).await;
        assert!(matches!(err, Err(AuthError::ReauthRequired)), "{err:?}");
        // The account a mock sign-in installed renews at the mock: a test never reaches the
        // provider's real token endpoint.
        assert_eq!(m.rejected.load(Ordering::SeqCst), 1, "the refresh went to the mock");

        let events = seen.lock().unwrap().clone();
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0].0, view.id);
        assert!(events[0].1, "the account left `live` before the app was told");
        assert!(accounts.auth(&view.id).is_none());
        assert!(accounts.store().load(&view.id).unwrap().is_none(), "the file is gone");
    }
}
