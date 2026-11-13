//! [`AccountAuth`]: the per-account token source handed to the provider driver.
//!
//! One signed-in account owns one file (`accounts/<id>.json`) and one of these. It implements
//! [`TokenProvider`], so the driver holds an account exactly as it holds an API-key profile: it
//! asks for a token and never sees where the token came from.
//!
//! **The one rule this module exists to keep: a refresh token is spent at most once.** Claude's
//! refresh token is single-use and Codex's rotates; presenting one that has already been rotated
//! away is at best an `invalid_grant`, and at worst the provider revokes the whole token family.
//! Two turns that both get a 401 at the same moment are the realistic way that happens, so:
//!
//! * [`TokenProvider::force_refresh`] receives the token the server refused and returns the
//!   current token **without a request** when it is already newer. The second 401 therefore
//!   finds a fresh token and stops.
//! * Every refresh runs under one gate and re-checks that condition *after* taking it — the race
//!   is between the check and the lock, not between two callers of the same check.
//! * A transport error is never retried, for the same reason: a timed-out refresh may well have
//!   been processed. The one retry that exists is a 429, which the provider did **not** process.
//!
//! Two more invariants: a login or logout that happens while a refresh is in flight wins (the
//! refresh compares the refresh token it sent against the stored one, and writes nothing if they
//! differ), and `invalid_grant` deletes the account — but only when the token that failed is
//! still the stored one.
//!
//! No token is ever printed. `Debug` is hand-written, every error is an [`AuthError`] (whose
//! variants carry no credential), and the only strings that leave this module are the account id
//! and the provider's own error text, scrubbed by `oauth`.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use futures::future::BoxFuture;
use tokio::sync::{Mutex, RwLock};

use super::login::ProviderEndpoints;
use super::store::{AccountRecord, AccountStore};
use super::{default_model, display_text, AccountProvider, AccountStatus, AccountView, ProviderSpec, TokenEncoding};
use crate::auth::oauth::{send_token_request, split_token_body};
use crate::auth::store::Credentials;
use crate::auth::{AuthError, TokenProvider, TokenResponse};

/// Called with the account id when a refresh finds `invalid_grant`, i.e. the stored refresh token
/// is dead. The registry wraps this so the dead object also leaves its `live` map first.
pub type ExpiredSink = Arc<dyn Fn(String) + Send + Sync>;

/// Refresh lead used when the token's lifetime is unknown, before [`ProviderSpec::refresh_skew`]
/// is applied. Without a ceiling, Codex's 96-hour request against an unknown lifetime would mean
/// "refresh on every turn".
const UNKNOWN_LIFETIME_LEAD: Duration = Duration::from_secs(30 * 60);

/// Per-account [`TokenProvider`]. Owns the in-memory record and the file behind it.
pub struct AccountAuth {
    /// This object's own `Arc`, so a refresh can run on a task of its own (see
    /// [`AccountAuth::refresh_detached`]). Weak, or the object would keep itself alive.
    me: std::sync::Weak<AccountAuth>,
    id: String,
    provider: AccountProvider,
    spec: &'static ProviderSpec,
    store: AccountStore,
    http: reqwest::Client,
    /// The stored record; `None` after a logout.
    record: RwLock<Option<AccountRecord>>,
    /// Set by `logout`, read under the gate by a refresh that is already past its network call,
    /// so a refresh cannot resurrect a file that was deliberately deleted.
    removed: std::sync::atomic::AtomicBool,
    /// Single-flight refresh **and** login/logout writes.
    gate: Mutex<()>,
    /// Immutable copies so the sync accessors can read them without a lock.
    wire_account_id: Option<String>,
    project_id: Option<String>,
    on_expired: ExpiredSink,
    /// Overridable for tests only; the spec's URL in production.
    endpoints: ProviderEndpoints,
    /// The last `AccountView` built from the record. `view()` is sync — the app calls it from a
    /// command handler, where awaiting a lock is not allowed — so the display fields are kept
    /// here and refreshed wherever the record changes.
    snapshot: std::sync::Mutex<AccountView>,
    /// The refresh token this object knows the file by: the one it read the record with, or last
    /// wrote. A file holding another one was written by a newer sign-in of the same account, and
    /// this object must neither overwrite nor delete it (see [`Self::file_is_foreign`]).
    on_disk: std::sync::Mutex<Option<String>>,
    /// The object a new sign-in of the same account installed in this one's place. Set once, by
    /// the registry; from then on it answers every token request made of this one.
    successor: std::sync::OnceLock<Arc<AccountAuth>>,
}

impl std::fmt::Debug for AccountAuth {
    /// The account id, the provider and when the token expires — never a token, never a secret.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountAuth")
            .field("id", &self.id)
            .field("provider", &self.provider)
            .field("has_wire_account_id", &self.wire_account_id.is_some())
            .field("has_project_id", &self.project_id.is_some())
            .finish_non_exhaustive()
    }
}

impl AccountAuth {
    pub fn new(rec: AccountRecord, store: AccountStore, http: reqwest::Client, on_expired: ExpiredSink) -> Arc<Self> {
        let endpoints = ProviderEndpoints::for_spec(rec.provider.spec());
        Self::with_endpoints(rec, store, http, on_expired, endpoints)
    }

    /// [`Self::new`] against explicit endpoints: the ones the sign-in that minted the record used.
    /// In the app those are the spec's, the same as [`Self::new`]; a test's are a mock server's,
    /// so an account it signed in never refreshes against the real provider.
    pub fn with_endpoints(
        rec: AccountRecord,
        store: AccountStore,
        http: reqwest::Client,
        on_expired: ExpiredSink,
        endpoints: ProviderEndpoints,
    ) -> Arc<Self> {
        let id = rec.account_id.clone();
        let provider = rec.provider;
        let wire_account_id = rec.wire_account_id.clone();
        let project_id = rec.project_id.clone();
        let snapshot = Self::view_of(&rec, provider.spec());
        let on_disk = rec.creds.refresh_token.clone();
        Arc::new_cyclic(|me| Self {
            me: me.clone(),
            id,
            provider,
            spec: provider.spec(),
            store,
            http,
            record: RwLock::new(Some(rec)),
            removed: std::sync::atomic::AtomicBool::new(false),
            gate: Mutex::new(()),
            wire_account_id,
            project_id,
            on_expired,
            endpoints,
            snapshot: std::sync::Mutex::new(snapshot),
            on_disk: std::sync::Mutex::new(on_disk),
            successor: std::sync::OnceLock::new(),
        })
    }

    /// Hand the account over to `next`, the object a new sign-in of the same account installed.
    ///
    /// A turn still holding this object keeps working — every token request is answered by
    /// `next` from now on — and this object writes nothing again: the file is `next`'s.
    pub(crate) fn retire(&self, next: Arc<AccountAuth>) {
        if std::ptr::eq(self, Arc::as_ptr(&next)) {
            return;
        }
        let _ = self.successor.set(next);
        self.removed.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Whether the file now belongs to a newer sign-in: it holds a refresh token other than the
    /// one this object read or last wrote. A missing or unreadable file belongs to nobody.
    ///
    /// This is what keeps an object that outlived a new sign-in — held by a running turn, or
    /// signing out behind a slow refresh — from writing its old tokens over the new ones, or
    /// deleting the file the new sign-in just wrote.
    fn file_is_foreign(&self) -> bool {
        let known = self.on_disk.lock().unwrap_or_else(|e| e.into_inner()).clone();
        matches!(self.store.load(&self.id), Ok(Some(rec)) if rec.creds.refresh_token != known)
    }

    /// Save `rec` and remember it as what the file holds.
    fn save_record(&self, rec: &AccountRecord) -> std::io::Result<()> {
        self.store.save(rec)?;
        *self.on_disk.lock().unwrap_or_else(|e| e.into_inner()) = rec.creds.refresh_token.clone();
        Ok(())
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn provider(&self) -> AccountProvider {
        self.provider
    }

    /// `Chatgpt-Account-Id` for the Codex dialect. Sync by design: the driver's `dialect()` is
    /// sync, and the value is immutable after construction, so no lock is involved and the
    /// header cannot disagree with the token next to it.
    pub fn wire_account_id(&self) -> Option<&str> {
        self.wire_account_id.as_deref()
    }

    /// The Antigravity Cloud project for the request body. Sync, for the same reason.
    pub fn project_id(&self) -> Option<&str> {
        self.project_id.as_deref()
    }

    /// A copy of the stored record (the registry persists model lists through it).
    pub async fn record(&self) -> Option<AccountRecord> {
        self.record.read().await.clone()
    }

    /// What the Account tab shows for a live account.
    ///
    /// Sync by contract: the app asks for it while handling a command, so it reads the snapshot
    /// instead of awaiting the record lock. The chosen model is the first entry of the fetched
    /// list, else the spec's first default — `model` is non-empty whenever the status is
    /// `SignedIn`, which the row and the driver's "no model chosen" message both rely on.
    pub fn view(&self) -> AccountView {
        self.snapshot.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Build a view from a record. One place, so the snapshot and any caller agree.
    fn view_of(rec: &AccountRecord, spec: &ProviderSpec) -> AccountView {
        let models = rec.models.clone();
        let model = default_model(spec, &models).unwrap_or_default();
        AccountView {
            id: rec.account_id.clone(),
            provider: rec.provider,
            email: rec.creds.email.as_deref().and_then(display_text),
            plan: rec.plan.as_deref().and_then(display_text),
            model,
            models,
            models_fetched_at: rec.models_fetched_at.and_then(DateTime::<Utc>::from_timestamp_millis),
            status: AccountStatus::SignedIn,
        }
    }

    /// Refresh the snapshot from a record this object now holds. Called under the gate, or from
    /// construction.
    fn publish(&self, rec: &AccountRecord) {
        *self.snapshot.lock().unwrap_or_else(|e| e.into_inner()) = Self::view_of(rec, self.spec);
    }

    /// Persist a freshly fetched model list — file first, then the in-memory copy, so the two
    /// cannot drift and a failed write leaves the old list in both places.
    pub async fn store_models(&self, models: Vec<String>, at: DateTime<Utc>) -> std::io::Result<()> {
        let _gate = self.gate.lock().await;
        let mut guard = self.record.write().await;
        let Some(rec) = guard.as_mut() else {
            return Err(std::io::Error::new(std::io::ErrorKind::NotFound, "signed out"));
        };
        if self.file_is_foreign() {
            // Saving would put this object's old tokens back over a newer sign-in's.
            return Err(std::io::Error::other("the account was signed in again; the new sign-in's file is kept"));
        }
        rec.models = models;
        rec.models_fetched_at = Some(at.timestamp_millis());
        self.save_record(rec)?;
        self.publish(rec);
        Ok(())
    }

    /// Clear memory and delete the file, under the gate.
    ///
    /// Taking the gate is what makes an in-flight refresh harmless: it re-checks `removed` after
    /// its network call and writes nothing, so the file cannot come back. A signed-out account
    /// that a turn still holds fails with `NotLoggedIn` rather than minting tokens from memory.
    ///
    /// The file goes only while it is this object's. Waiting for the gate can take a while (a
    /// refresh sitting out a rate limit), and a sign-in that finished meanwhile wrote a file of
    /// its own: that one is the account the user now has, and it stays.
    pub async fn logout(&self) -> Result<(), AuthError> {
        let _gate = self.gate.lock().await;
        self.removed.store(true, std::sync::atomic::Ordering::SeqCst);
        *self.record.write().await = None;
        {
            let mut snap = self.snapshot.lock().unwrap_or_else(|e| e.into_inner());
            snap.status = AccountStatus::SignedOut;
            snap.models.clear();
            snap.models_fetched_at = None;
        }
        if self.successor.get().is_some() || self.file_is_foreign() {
            tracing::info!("account {} was signed in again; its new file is kept", self.id);
            return Ok(());
        }
        self.store.delete(&self.id)?;
        Ok(())
    }

    /// [`TokenProvider::access_token`] as an inherent method, so a caller holding a concrete
    /// `AccountAuth` (the registry does) does not have to import the trait to ask for a bearer.
    pub async fn access_token_owned(&self) -> Result<String, AuthError> {
        <Self as TokenProvider>::access_token(self).await
    }

    /// Test hook: the access token currently held.
    #[cfg(test)]
    pub async fn peek_access_token(&self) -> Option<String> {
        self.record.read().await.as_ref().and_then(|r| r.creds.access_token.clone())
    }

    /// The refresh lead for this token: the spec's wish, clamped to half the token's own lifetime.
    ///
    /// A requested lead larger than the lifetime is not a small mistake — it means "refresh on
    /// every turn", once per turn, against a rotating token. Codex asks for 96 hours and its
    /// access tokens live for about a hundred days, so the clamp is what makes that a real lead
    /// rather than an infinite one.
    pub(crate) fn effective_skew(rec: &AccountRecord, spec: &ProviderSpec) -> Duration {
        match rec.lifetime_secs.filter(|s| *s > 0) {
            Some(secs) => spec.refresh_skew.min(Duration::from_secs(secs as u64 / 2)),
            None => spec.refresh_skew.min(UNKNOWN_LIFETIME_LEAD),
        }
    }

    /// The stored access token, when it is usable for at least the refresh lead.
    fn fresh(rec: &AccountRecord, spec: &ProviderSpec) -> Option<String> {
        if rec.creds.needs_refresh(Self::effective_skew(rec, spec)) {
            return None;
        }
        rec.creds.access_token.clone()
    }

    /// Refresh the record. The caller holds the gate.
    ///
    /// `failed` is the token the server refused, or `None` for "it is merely near expiry". Both
    /// cases re-check under the gate, because another caller may have refreshed while this one
    /// waited.
    async fn refresh_locked(&self, failed: Option<&str>) -> Result<String, AuthError> {
        let current = self.record.read().await.clone();

        match failed {
            Some(failed) => {
                let Some(rec) = current else { return Err(AuthError::NotLoggedIn) };
                // Already replaced: return it and make no request. This is the whole
                // single-use guarantee, and the reason the rejected token travels here.
                match rec.creds.access_token.as_deref() {
                    Some(t) if t != failed => return Ok(t.to_owned()),
                    None => return Err(AuthError::NotLoggedIn),
                    _ => {}
                }
            }
            None => {
                let Some(rec) = current else { return Err(AuthError::NotLoggedIn) };
                if let Some(t) = Self::fresh(&rec, self.spec) {
                    return Ok(t);
                }
            }
        }

        let Some(rec) = self.record.read().await.clone() else { return Err(AuthError::NotLoggedIn) };
        let Some(refresh) = rec.creds.refresh_token.clone().filter(|t| !t.is_empty()) else {
            // Nothing to spend. Reported as a re-auth, but the file stays: it is not *dead*,
            // there is simply no refresh token in it (an imported access-token-only record).
            return Err(AuthError::ReauthRequired);
        };

        let reply = self.request_refresh(&refresh).await;
        self.finish_refresh(&refresh, reply).await?;

        let access = self.record.read().await.as_ref().and_then(|r| r.creds.access_token.clone());
        access.ok_or(AuthError::NotLoggedIn)
    }

    /// Take the gate and refresh, on a task of its own, and wait for it.
    ///
    /// The provider spends the refresh token the moment it receives the request, and the future
    /// waiting here belongs to a turn — which the user can stop, or the window can close, at any
    /// point. Run inline, dropping that future mid-request would discard the response carrying
    /// the rotated token after the old one had already died (Claude's is single-use, Codex's
    /// rotates), and the account would be lost at its next refresh. On its own task the response
    /// is always applied and saved; a waiter that goes away only stops listening.
    async fn refresh_detached(&self, failed: Option<String>) -> Result<String, AuthError> {
        // Only empty while the last `Arc` is being dropped, when nothing can be waiting.
        let Some(me) = self.me.upgrade() else { return Err(AuthError::NotLoggedIn) };
        let task = tokio::spawn(async move {
            let _gate = me.gate.lock().await;
            me.refresh_locked(failed.as_deref()).await
        });
        task.await
            .unwrap_or_else(|e| Err(AuthError::Http(format!("the token refresh stopped before it finished: {e}"))))
    }

    /// The network half, with the one retry that is safe: a 429 is the single response that
    /// proves the provider did *not* spend the refresh token.
    async fn request_refresh(&self, refresh: &str) -> Result<(TokenResponse, serde_json::Value), AuthError> {
        let is_json = self.spec.token_encoding == TokenEncoding::Json;
        let (status, headers, bytes) = self.send_refresh(refresh, is_json).await?;
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let wait = crate::auth::oauth::retry_after(&headers);
            tracing::warn!("refresh for {} was rate limited; retrying in {} s", self.id, wait.as_secs());
            // Slept under the gate (the caller holds it), so a queued refresh cannot re-fire
            // immediately and turn one rate limit into a storm.
            tokio::time::sleep(wait).await;
            let (status, headers, bytes) = self.send_refresh(refresh, is_json).await?;
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                return Err(AuthError::TokenEndpoint(format!(
                    "the token endpoint is rate limiting sign-in; try again in {} s",
                    crate::auth::oauth::retry_after(&headers).as_secs()
                )));
            }
            return split_token_body(status, &bytes);
        }
        split_token_body(status, &bytes)
    }

    async fn send_refresh(
        &self,
        refresh: &str,
        is_json: bool,
    ) -> Result<(reqwest::StatusCode, reqwest::header::HeaderMap, Vec<u8>), AuthError> {
        let body = self.refresh_body(refresh);
        if is_json {
            let text = serde_json::to_string(&body)?;
            send_token_request(&self.http, &self.endpoints.token_url, "application/json", text).await
        } else {
            send_token_request(&self.http, &self.endpoints.token_url, "application/x-www-form-urlencoded", body.to_form()).await
        }
    }

    /// One refresh grant in this provider's field set.
    ///
    /// A struct rather than a map for the JSON path: the endpoint mirrors a client that
    /// serialises a struct, so the key order is part of what it expects.
    fn refresh_body<'a>(&self, refresh: &'a str) -> RefreshBody<'a> {
        RefreshBody {
            grant_type: "refresh_token",
            refresh_token: refresh,
            client_id: self.spec.client_id,
            client_secret: self.spec.client_secret,
            scope: self.spec.scopes.join(" "),
        }
    }

    /// Apply a token response, honouring both write guards. The caller holds the gate.
    async fn finish_refresh(
        &self,
        sent: &str,
        result: Result<(TokenResponse, serde_json::Value), AuthError>,
    ) -> Result<(), AuthError> {
        // Signed out while the request was in flight: nothing may be written, and the file must
        // stay gone.
        if self.removed.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(AuthError::NotLoggedIn);
        }
        let mut guard = self.record.write().await;
        let Some(rec) = guard.as_mut() else { return Err(AuthError::NotLoggedIn) };
        // Swap detection: a login replaced the record. Its tokens are newer than anything this
        // request could write, so ours is discarded — not merged.
        if rec.creds.refresh_token.as_deref() != Some(sent) {
            return Ok(());
        }
        // A new sign-in of this account wrote the file while the request was out. The file is
        // its now: our answer still serves this object's callers, but is not written back, and a
        // dead token here says nothing about the new sign-in's.
        let foreign = self.file_is_foreign();
        match result {
            Ok((resp, _)) => {
                apply_token(&mut rec.creds, &resp);
                if let Some(secs) = resp.expires_in.filter(|s| *s > 0) {
                    rec.lifetime_secs = Some(secs);
                }
                if foreign {
                    tracing::info!("account {} was signed in again; the refresh is not written over it", self.id);
                } else if let Err(e) = self.save_record(rec) {
                    // The token in memory is valid, so the turn must not fail over this; the next
                    // refresh tries again. Logged without the record.
                    tracing::warn!("could not save the refreshed token for {}: {e}", self.id);
                }
                let snap = Self::view_of(rec, self.spec);
                drop(guard);
                *self.snapshot.lock().unwrap_or_else(|e| e.into_inner()) = snap;
                Ok(())
            }
            Err(AuthError::ReauthRequired) => {
                // The refresh token that failed is still the stored one (checked above), so it is
                // dead. Clear memory, delete the file and let the registry announce it — unless the
                // file is a newer sign-in's, which this token's death says nothing about.
                rec.creds.access_token = None;
                rec.creds.refresh_token = None;
                if foreign {
                    return Err(AuthError::ReauthRequired);
                }
                let _ = self.store.delete(&self.id);
                drop(guard);
                (self.on_expired)(self.id.clone());
                Err(AuthError::ReauthRequired)
            }
            Err(e) => Err(e),
        }
    }
}

impl TokenProvider for AccountAuth {
    /// Fast path outside the gate: a valid token comes from memory. Only an expiring or missing
    /// one takes the gate, so a burst of turns is not serialised on every request.
    fn access_token(&self) -> BoxFuture<'_, Result<String, AuthError>> {
        Box::pin(async move {
            if let Some(next) = self.successor.get() {
                return next.access_token().await;
            }
            {
                let guard = self.record.read().await;
                let Some(rec) = guard.as_ref() else { return Err(AuthError::NotLoggedIn) };
                if let Some(t) = Self::fresh(rec, self.spec) {
                    return Ok(t);
                }
            }
            match (self.refresh_detached(None).await, self.successor.get()) {
                // Retired while the refresh ran: the new sign-in answers.
                (Err(_), Some(next)) => next.access_token().await,
                (result, _) => result,
            }
        })
    }

    fn force_refresh<'a>(&'a self, failed: &'a str) -> BoxFuture<'a, Result<String, AuthError>> {
        Box::pin(async move {
            // Retired: the new sign-in's token is not the one that failed, so it comes back
            // without a request.
            if let Some(next) = self.successor.get() {
                return next.force_refresh(failed).await;
            }
            {
                // Cheap pre-check outside the gate: if a refresh already replaced `failed`, the
                // answer is the current token and no request is needed at all.
                let guard = self.record.read().await;
                let Some(rec) = guard.as_ref() else { return Err(AuthError::NotLoggedIn) };
                match rec.creds.access_token.as_deref() {
                    Some(current) if current != failed => return Ok(current.to_owned()),
                    None => return Err(AuthError::NotLoggedIn),
                    _ => {}
                }
            }
            // Re-checked under the gate, which is what makes two parallel 401s spend one token.
            match (self.refresh_detached(Some(failed.to_owned())).await, self.successor.get()) {
                (Err(_), Some(next)) => next.force_refresh(failed).await,
                (result, _) => result,
            }
        })
    }
}

/// One refresh grant. A struct, not a map: the JSON path sends it in this order (the endpoint
/// mirrors a client that serialises a struct), which a `serde_json::Map` would sort instead.
#[derive(serde::Serialize)]
struct RefreshBody<'a> {
    grant_type: &'static str,
    refresh_token: &'a str,
    client_id: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    client_secret: Option<&'static str>,
    scope: String,
}

impl RefreshBody<'_> {
    /// Google takes the form encoding; a client secret exists only for Antigravity.
    fn to_form(&self) -> String {
        let mut pairs: Vec<(&str, &str)> = vec![
            ("grant_type", self.grant_type),
            ("refresh_token", self.refresh_token),
            ("client_id", self.client_id),
            ("scope", &self.scope),
        ];
        if let Some(s) = self.client_secret {
            pairs.push(("client_secret", s));
        }
        url::form_urlencoded::Serializer::new(String::new()).extend_pairs(pairs).finish()
    }
}

/// Fold a token response into stored credentials.
///
/// The refresh token is kept when the response omits one: providers only rotate it sometimes, and
/// dropping it would sign the user out at the next expiry for no reason. A response without
/// `expires_in` is not "valid forever" — an hour is the common case and refreshing a little early
/// is harmless.
pub(crate) fn apply_token(creds: &mut Credentials, resp: &TokenResponse) {
    let now = Utc::now().timestamp_millis();
    creds.access_token = Some(resp.access_token.clone());
    if let Some(rt) = resp.refresh_token.as_deref().filter(|t| !t.is_empty()) {
        creds.refresh_token = Some(rt.to_owned());
    }
    creds.expiry_date = Some(now + resp.expires_in.filter(|s| *s > 0).unwrap_or(3600) * 1000);
    if resp.scope.is_some() {
        creds.scope = resp.scope.clone();
    }
    if resp.token_type.is_some() {
        creds.token_type = resp.token_type.clone();
    }
    if resp.id_token.is_some() {
        creds.id_token = resp.id_token.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::external::store::AccountRecord;
    use crate::auth::store::Credentials;
    use axum::{extract::State, http::StatusCode, routing::post, Router};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// The mock token endpoint: counts refreshes and hands out a rotating refresh token.
    #[derive(Default)]
    struct Mock {
        refreshes: AtomicUsize,
        /// When true, every refresh answers `invalid_grant`.
        reject: std::sync::atomic::AtomicBool,
        /// When set, the first N refreshes answer 429 with this `Retry-After`.
        rate_limited: AtomicUsize,
        /// What each answer hands out; a rotation is only observable if it changes.
        next_access: std::sync::Mutex<Vec<String>>,
        /// The refresh tokens the endpoint was presented, in order.
        seen: std::sync::Mutex<Vec<String>>,
        /// When set, the handler waits for this before answering, so a refresh can be held open.
        hold: std::sync::Mutex<Option<Arc<tokio::sync::Notify>>>,
        /// Signalled once the handler has received a request.
        entered: Arc<tokio::sync::Notify>,
    }

    async fn token(
        State(m): State<Arc<Mock>>,
        body: String,
    ) -> (StatusCode, axum::http::HeaderMap, axum::Json<serde_json::Value>) {
        if body.trim_start().starts_with('{') {
            let v: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
            if v["grant_type"] == "refresh_token" {
                m.seen.lock().unwrap().push(v["refresh_token"].as_str().unwrap_or_default().to_owned());
            }
        } else {
            let form: std::collections::HashMap<String, String> =
                url::form_urlencoded::parse(body.as_bytes()).into_owned().collect();
            m.seen.lock().unwrap().push(form.get("refresh_token").cloned().unwrap_or_default());
        }
        m.entered.notify_waiters();
        let held = m.hold.lock().unwrap().clone();
        if let Some(note) = held {
            // The test releases us once logout has completed.
            note.notified().await;
        }
        if m.reject.load(Ordering::SeqCst) {
            return (
                StatusCode::BAD_REQUEST,
                axum::http::HeaderMap::new(),
                axum::Json(serde_json::json!({"error": {"type": "invalid_grant", "message": "revoked"}})),
            );
        }
        if m.rate_limited.load(Ordering::SeqCst) > 0 {
            m.rate_limited.fetch_sub(1, Ordering::SeqCst);
            // `retry-after: 1` — the reader clamps it up to the 5 s floor, which is asserted on
            // its own; the retry path is exercised for the count, not for the wall clock.
            let mut headers = axum::http::HeaderMap::new();
            headers.insert("retry-after", "1".parse().unwrap());
            return (
                StatusCode::TOO_MANY_REQUESTS,
                headers,
                axum::Json(serde_json::json!({"error": "rate_limited"})),
            );
        }
        let n = m.refreshes.fetch_add(1, Ordering::SeqCst) + 1;
        let access = m.next_access.lock().unwrap().pop().unwrap_or_else(|| format!("at-refresh-{n}"));
        (
            StatusCode::OK,
            axum::http::HeaderMap::new(),
            axum::Json(serde_json::json!({
                "access_token": access,
                "refresh_token": format!("rt-rotated-{n}"),
                "expires_in": 3600,
            })),
        )
    }

    /// Upper bound on every wait on the mock token endpoint. A refresh that never returns must
    /// fail the test in seconds; `REFRESH_TIMEOUT` alone would let it stall the suite for 30 s.
    const WAIT: Duration = Duration::from_secs(20);

    /// `f`, or a panic once [`WAIT`] has passed.
    async fn bounded<T>(f: impl std::future::Future<Output = T>) -> T {
        tokio::time::timeout(WAIT, f).await.expect("the token request finished in time")
    }

    async fn mock() -> (Arc<Mock>, String) {
        let state = Arc::new(Mock::default());
        let app = Router::new().route("/token", post(token)).with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (state, format!("http://{addr}"))
    }

    /// A record whose token expires in `in_secs` (negative: already expired).
    fn record(provider: AccountProvider, access: &str, refresh: Option<&str>, in_secs: i64) -> AccountRecord {
        AccountRecord {
            creds: Credentials {
                access_token: Some(access.to_owned()),
                refresh_token: refresh.map(str::to_owned),
                expiry_date: Some(Utc::now().timestamp_millis() + in_secs * 1000),
                email: Some("me@example.com".into()),
                ..Default::default()
            },
            provider,
            account_id: crate::auth::external::account_id(provider, "me@example.com"),
            label: None,
            plan: None,
            wire_account_id: None,
            org_uuid: None,
            project_id: None,
            models: Vec::new(),
            models_fetched_at: None,
            lifetime_secs: Some(3600),
        }
    }

    fn auth(rec: AccountRecord, dir: &std::path::Path, base: &str) -> (Arc<AccountAuth>, Arc<AtomicUsize>, AccountStore) {
        let store = AccountStore::in_dir(dir);
        store.save(&rec).unwrap();
        let expired = Arc::new(AtomicUsize::new(0));
        let sink: ExpiredSink = {
            let e = expired.clone();
            Arc::new(move |_id| {
                e.fetch_add(1, Ordering::SeqCst);
            })
        };
        let endpoints = ProviderEndpoints::with_base(base);
        let auth = AccountAuth::with_endpoints(rec, store.clone(), reqwest::Client::new(), sink, endpoints);
        (auth, expired, store)
    }

    #[tokio::test]
    async fn a_fresh_token_is_returned_without_a_request() {
        let (m, base) = mock().await;
        let dir = tempfile::tempdir().unwrap();
        let (auth, _e, _s) = auth(record(AccountProvider::ClaudeCode, "at-live", Some("rt-1"), 3600), dir.path(), &base);
        assert_eq!(bounded(auth.access_token()).await.unwrap(), "at-live");
        assert_eq!(m.refreshes.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn an_expiring_token_is_refreshed_once_and_saved() {
        let (m, base) = mock().await;
        let dir = tempfile::tempdir().unwrap();
        let (auth, _e, store) = auth(record(AccountProvider::ClaudeCode, "at-old", Some("rt-1"), 10), dir.path(), &base);
        assert_eq!(bounded(auth.access_token()).await.unwrap(), "at-refresh-1");
        assert_eq!(bounded(auth.access_token()).await.unwrap(), "at-refresh-1", "cached after the refresh");
        assert_eq!(m.refreshes.load(Ordering::SeqCst), 1);
        let saved = store.load(auth.id()).unwrap().unwrap();
        assert_eq!(saved.creds.access_token.as_deref(), Some("at-refresh-1"));
        assert_eq!(saved.creds.refresh_token.as_deref(), Some("rt-rotated-1"), "the rotation is kept");
    }

    #[tokio::test]
    async fn a_rejected_token_that_was_already_replaced_makes_no_request() {
        // The single-use guarantee: the second parallel 401 must not spend the token again.
        let (m, base) = mock().await;
        let dir = tempfile::tempdir().unwrap();
        let (auth, _e, _s) = auth(record(AccountProvider::ClaudeCode, "at-new", Some("rt-1"), 3600), dir.path(), &base);
        // The caller holds a stale token the server refused.
        assert_eq!(bounded(auth.force_refresh("at-old")).await.unwrap(), "at-new");
        assert_eq!(m.refreshes.load(Ordering::SeqCst), 0, "no request was made");
    }

    #[tokio::test]
    async fn two_parallel_401s_spend_one_refresh_token() {
        let (m, base) = mock().await;
        let dir = tempfile::tempdir().unwrap();
        let (auth, _e, _s) = auth(record(AccountProvider::ClaudeCode, "at-old", Some("rt-1"), 3600), dir.path(), &base);
        let a = auth.clone();
        let b = auth.clone();
        let (r1, r2) = bounded(async move {
            tokio::join!(
                async move { a.force_refresh("at-old").await },
                async move { b.force_refresh("at-old").await }
            )
        })
        .await;
        assert_eq!(r1.unwrap(), "at-refresh-1");
        assert_eq!(r2.unwrap(), "at-refresh-1", "the second sees the replaced token");
        assert_eq!(m.refreshes.load(Ordering::SeqCst), 1, "exactly one token request");
        assert_eq!(m.seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_rejected_refresh_token_deletes_the_account_and_reports_it() {
        let (m, base) = mock().await;
        m.reject.store(true, Ordering::SeqCst);
        let dir = tempfile::tempdir().unwrap();
        let (auth, expired, store) = auth(record(AccountProvider::ClaudeCode, "at-old", Some("rt-1"), 0), dir.path(), &base);
        let err = bounded(auth.force_refresh("at-old")).await.unwrap_err();
        assert!(matches!(err, AuthError::ReauthRequired), "{err:?}");
        assert_eq!(expired.load(Ordering::SeqCst), 1, "the registry is told");
        assert!(store.load(auth.id()).unwrap().is_none(), "the file is gone");
        assert!(auth.peek_access_token().await.is_none(), "and so is the memory");
    }

    #[tokio::test]
    async fn an_account_with_no_refresh_token_is_not_deleted() {
        // An imported, access-token-only record is not *invalid* — there is simply nothing to
        // spend, and deleting it would lose an account that still works until the token expires.
        let (m, base) = mock().await;
        let dir = tempfile::tempdir().unwrap();
        let (auth, expired, store) = auth(record(AccountProvider::Codex, "at-old", None, 0), dir.path(), &base);
        assert!(matches!(bounded(auth.access_token()).await, Err(AuthError::ReauthRequired)));
        assert_eq!(m.refreshes.load(Ordering::SeqCst), 0);
        assert_eq!(expired.load(Ordering::SeqCst), 0, "an expiry is not an invalidation");
        assert!(store.load(auth.id()).unwrap().is_some(), "the file stays");
    }

    #[tokio::test]
    async fn the_refresh_token_is_kept_when_the_response_omits_one() {
        // Some providers only rotate sometimes; dropping the old one signs the user out at the
        // next expiry for no reason.
        let mut rec = record(AccountProvider::ClaudeCode, "at-old", Some("rt-keep"), 0);
        rec.lifetime_secs = Some(3600);
        crate::auth::external::auth::apply_token(
            &mut rec.creds,
            &crate::auth::TokenResponse {
                access_token: "at-new".into(),
                expires_in: Some(100),
                refresh_token: None,
                scope: None,
                token_type: None,
                id_token: None,
            },
        );
        assert_eq!(rec.creds.refresh_token.as_deref(), Some("rt-keep"));
        assert_eq!(rec.creds.access_token.as_deref(), Some("at-new"));
    }

    #[tokio::test]
    async fn a_logout_during_a_refresh_leaves_no_file() {
        // A refresh that is past its network call when logout wins must not write the file back.
        // The mock holds its answer until the refresh has started, so the two really do overlap.
        let (m, base) = mock().await;
        let release = Arc::new(tokio::sync::Notify::new());
        *m.hold.lock().unwrap() = Some(release.clone());
        let dir = tempfile::tempdir().unwrap();
        let (auth, _e, store) = auth(record(AccountProvider::ClaudeCode, "at-old", Some("rt-1"), 0), dir.path(), &base);

        let refresher = {
            let auth = auth.clone();
            tokio::spawn(async move { auth.access_token().await })
        };
        // Wait until the request is inside the handler, then release it and log out. Both orders
        // are covered by the two assertions below; this one is the interesting race.
        tokio::time::timeout(std::time::Duration::from_secs(10), m.entered.notified()).await.unwrap();
        *m.hold.lock().unwrap() = None;
        release.notify_waiters();
        // Give the refresh a moment to finish its write, then log out. Either way the file must
        // be gone at the end: logout is the last word.
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), refresher).await.unwrap().unwrap();
        assert!(outcome.is_ok(), "the refresh itself succeeded: {outcome:?}");
        bounded(auth.logout()).await.unwrap();
        assert!(store.load(auth.id()).unwrap().is_none(), "the file is gone");
        assert!(md_none(&auth).await);
    }

    #[tokio::test]
    async fn a_refresh_whose_turn_is_stopped_still_saves_the_rotated_token() {
        // The turn waiting on a refresh is dropped while the request is at the provider. The
        // provider has already spent `rt-1`; losing its answer would leave a dead token on disk.
        let (m, base) = mock().await;
        let release = Arc::new(tokio::sync::Notify::new());
        *m.hold.lock().unwrap() = Some(release.clone());
        let dir = tempfile::tempdir().unwrap();
        let (auth, expired, store) = auth(record(AccountProvider::ClaudeCode, "at-old", Some("rt-1"), 0), dir.path(), &base);

        let entered = m.entered.notified();
        let turn = {
            let auth = auth.clone();
            tokio::spawn(async move { auth.access_token().await })
        };
        bounded(entered).await;
        turn.abort(); // the user pressed Stop
        assert!(turn.await.unwrap_err().is_cancelled());
        *m.hold.lock().unwrap() = None;
        release.notify_waiters();

        // The refresh finishes on its own and records what the provider handed out.
        bounded(async {
            while store.load(auth.id()).unwrap().and_then(|r| r.creds.refresh_token) != Some("rt-rotated-1".into()) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert_eq!(auth.peek_access_token().await.as_deref(), Some("at-refresh-1"));
        // The next turn uses it, with no second request and no "session expired".
        assert_eq!(bounded(auth.access_token()).await.unwrap(), "at-refresh-1");
        assert_eq!(m.seen.lock().unwrap().as_slice(), ["rt-1"], "rt-1 was presented once");
        assert_eq!(expired.load(Ordering::SeqCst), 0);
    }

    /// What a sign-in of the same account writes while another object is still alive.
    fn signed_in_again(store: &AccountStore) {
        let mut rec = record(AccountProvider::ClaudeCode, "at-new-login", Some("rt-new-login"), 3600);
        rec.label = Some("new".into());
        store.save(&rec).unwrap();
    }

    /// Hold the mock's next answer; returns the release handle once the request is inside it.
    async fn hold_next(m: &Mock) -> Arc<tokio::sync::Notify> {
        let release = Arc::new(tokio::sync::Notify::new());
        *m.hold.lock().unwrap() = Some(release.clone());
        release
    }

    #[tokio::test]
    async fn a_refresh_that_ends_after_a_new_sign_in_does_not_write_over_it() {
        let (m, base) = mock().await;
        let release = hold_next(&m).await;
        let dir = tempfile::tempdir().unwrap();
        let (old, expired, store) = auth(record(AccountProvider::ClaudeCode, "at-old", Some("rt-1"), 0), dir.path(), &base);
        let entered = m.entered.notified();
        let turn = {
            let old = old.clone();
            tokio::spawn(async move { old.access_token().await })
        };
        bounded(entered).await;
        signed_in_again(&store);
        *m.hold.lock().unwrap() = None;
        release.notify_waiters();

        // The turn that asked still gets a token…
        assert_eq!(bounded(turn).await.unwrap().unwrap(), "at-refresh-1");
        // …but the file is the new sign-in's, untouched.
        let file = store.load(old.id()).unwrap().unwrap();
        assert_eq!(file.creds.refresh_token.as_deref(), Some("rt-new-login"));
        assert_eq!(expired.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_dead_token_found_after_a_new_sign_in_neither_deletes_nor_reports() {
        // The old session dies (signing in again can revoke it) while the new one is on disk:
        // deleting the file and reporting "expired" would sign the user out of the account they
        // just signed in to.
        let (m, base) = mock().await;
        m.reject.store(true, Ordering::SeqCst);
        let release = hold_next(&m).await;
        let dir = tempfile::tempdir().unwrap();
        let (old, expired, store) = auth(record(AccountProvider::ClaudeCode, "at-old", Some("rt-1"), 0), dir.path(), &base);
        let entered = m.entered.notified();
        let turn = {
            let old = old.clone();
            tokio::spawn(async move { old.access_token().await })
        };
        bounded(entered).await;
        signed_in_again(&store);
        *m.hold.lock().unwrap() = None;
        release.notify_waiters();

        assert!(matches!(bounded(turn).await.unwrap(), Err(AuthError::ReauthRequired)));
        assert!(store.load(old.id()).unwrap().is_some(), "the new sign-in's file stays");
        assert_eq!(expired.load(Ordering::SeqCst), 0, "nothing is reported");
    }

    #[tokio::test]
    async fn a_sign_out_that_waited_behind_a_new_sign_in_keeps_the_new_file() {
        let (m, base) = mock().await;
        let dir = tempfile::tempdir().unwrap();
        let (old, _e, store) = auth(record(AccountProvider::ClaudeCode, "at-old", Some("rt-1"), 3600), dir.path(), &base);
        signed_in_again(&store);
        bounded(old.logout()).await.unwrap();
        let file = store.load(old.id()).unwrap().expect("kept");
        assert_eq!(file.creds.refresh_token.as_deref(), Some("rt-new-login"));
        assert_eq!(m.refreshes.load(Ordering::SeqCst), 0);
        // Its own file, on the other hand, goes.
        let dir = tempfile::tempdir().unwrap();
        let (mine, _e, store) = auth(record(AccountProvider::ClaudeCode, "at-old", Some("rt-1"), 3600), dir.path(), &base);
        bounded(mine.logout()).await.unwrap();
        assert!(store.load(mine.id()).unwrap().is_none());
    }

    #[tokio::test]
    async fn a_retired_object_answers_with_its_successors_tokens() {
        let (m, base) = mock().await;
        let dir = tempfile::tempdir().unwrap();
        let (old, expired, store) = auth(record(AccountProvider::ClaudeCode, "at-old", Some("rt-1"), 0), dir.path(), &base);
        let (new, _e2, _s2) = auth(record(AccountProvider::ClaudeCode, "at-successor", Some("rt-2"), 3600), dir.path(), &base);
        old.retire(new.clone());
        old.retire(old.clone()); // retiring into itself is ignored
        // Expired as it is, the old object makes no request of its own.
        assert_eq!(bounded(old.access_token()).await.unwrap(), "at-successor");
        assert_eq!(bounded(old.force_refresh("at-old")).await.unwrap(), "at-successor", "not the token that failed");
        assert_eq!(m.refreshes.load(Ordering::SeqCst), 0);
        // Its sign-out leaves the successor's file.
        bounded(old.logout()).await.unwrap();
        assert!(store.load(new.id()).unwrap().is_some());
        assert_eq!(expired.load(Ordering::SeqCst), 0);
    }

    /// `peek_access_token` as a bool, so the assertion reads as intent.
    async fn md_none(auth: &AccountAuth) -> bool {
        auth.peek_access_token().await.is_none()
    }

    #[tokio::test]
    async fn a_logout_before_a_refresh_makes_the_refresh_a_no_op() {
        let (m, base) = mock().await;
        let dir = tempfile::tempdir().unwrap();
        let (auth, _e, store) = auth(record(AccountProvider::ClaudeCode, "at-old", Some("rt-1"), 0), dir.path(), &base);
        bounded(auth.logout()).await.unwrap();
        assert!(matches!(bounded(auth.access_token()).await, Err(AuthError::NotLoggedIn)));
        assert!(store.load(auth.id()).unwrap().is_none(), "the file stays gone");
        assert_eq!(m.refreshes.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn the_refresh_lead_is_clamped_to_half_the_lifetime() {
        // Codex asks for 96 h; a one-hour token would otherwise be refreshed on every turn.
        let spec = AccountProvider::Codex.spec();
        let mut rec = record(AccountProvider::Codex, "at", Some("rt"), 3600);
        rec.lifetime_secs = Some(3600);
        assert_eq!(AccountAuth::effective_skew(&rec, spec), Duration::from_secs(1800));
        // A 100-hour token: half of it (50 h) is still below the 96 h request, so the lifetime
        // wins; above 192 hours the request is the smaller of the two.
        rec.lifetime_secs = Some(100 * 3600);
        assert_eq!(AccountAuth::effective_skew(&rec, spec), Duration::from_secs(50 * 3600));
        rec.lifetime_secs = Some(400 * 3600);
        assert_eq!(AccountAuth::effective_skew(&rec, spec), spec.refresh_skew, "never above the request");
        rec.lifetime_secs = None;
        assert_eq!(AccountAuth::effective_skew(&rec, spec), Duration::from_secs(30 * 60), "unknown lifetime");
    }

    #[test]
    fn the_debug_output_holds_no_token() {
        let dir = tempfile::tempdir().unwrap();
        let rec = record(AccountProvider::Codex, "AT-SECRET-VALUE", Some("RT-SECRET-VALUE"), 0);
        let store = AccountStore::in_dir(dir.path());
        let sink: ExpiredSink = Arc::new(|_id| {});
        let auth = AccountAuth::new(rec, store, reqwest::Client::new(), sink);
        let text = format!("{auth:?}");
        assert!(!text.contains("AT-SECRET-VALUE"), "{text}");
        assert!(!text.contains("RT-SECRET-VALUE"), "{text}");
        assert!(text.contains("codex"), "{text}");
    }

    #[test]
    fn the_refresh_body_is_a_form_with_google_s_field_set() {
        let dir = tempfile::tempdir().unwrap();
        let rec = record(AccountProvider::Antigravity, "at", Some("rt"), 3600);
        let store = AccountStore::in_dir(dir.path());
        let sink: ExpiredSink = Arc::new(|_id| {});
        let auth = AccountAuth::new(rec, store, reqwest::Client::new(), sink);
        let form = auth.refresh_body("rt-x").to_form();
        assert!(form.contains("grant_type=refresh_token"), "{form}");
        assert!(form.contains("refresh_token=rt-x"), "{form}");
        // The client is the one the build carries (placeholders in a test build without it).
        let spec = AccountProvider::Antigravity.spec();
        assert!(form.contains(&format!("client_id={}", spec.client_id)), "{form}");
        assert!(form.contains("client_secret="), "Google wants the secret back: {form}");
    }
}
