//! Reuse a Google sign-in that Antigravity or the gemini CLI already made on this machine.
//!
//! Both tools store an installed-app OAuth credential file at `~/.gemini/oauth_creds.json`.
//! Reading it means the user does not have to sign in a second time just to try AntiCraft.
//! The session is only used after an explicit choice (Customizations > Account).
//!
//! **This module is a read-only consumer.** It never writes to another application's files and
//! never copies a token into AntiCraft's own credential store: the file is re-read each time a
//! fresh token is needed, so logging out of Antigravity also ends this session. Only the plain
//! JSON files [`find_credential_files_in`] lists are read, never another application's
//! encrypted or database storage.
//!
//! **An imported session can usually not be renewed, and that is not a bug.** A Google refresh
//! token is bound to the OAuth client that issued it. Antigravity ships its own client ID, which
//! differs from the gemini-cli client AntiCraft uses for this *import* path, so a session
//! Antigravity issued runs here in access-token-only mode: it works until the token expires
//! (about an hour). Near the end the file is re-read, because the app that owns the session may
//! have renewed it; once nothing valid is left the session ends with a reason
//! ([`ImportedTokenSource::watch`]). [`ImportSummary::explain`] says all of this up front instead
//! of failing with a 401 later.
//!
//! **We can renew with Antigravity's client now, though — just not from here.** AntiCraft can
//! sign in to Antigravity directly (Customizations > Account > Accounts), using that same client
//! with its own secret. A first-party Antigravity account refreshes itself forever, so the
//! Account tab hides this import row while one is signed in: two Antigravity rows would be a
//! permanent "which one sends?" question. The rule is simply: **a signed-in account wins over an
//! import**, because the account can renew and the import cannot. Everything in *this* module
//! stays read-only — it reads another app's file and never mints anything.
//!
//! Secrets never leave this module: [`inspect`] describes a session without any token value,
//! [`ImportedCreds`] and [`ImportedTokenSource`] print every token as `<redacted>` in `Debug`,
//! and no error message or end reason includes one.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use chrono::{DateTime, Local, Utc};
use futures::future::BoxFuture;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use super::oauth::refresh_token;
use super::{AuthConfig, AuthError, TokenProvider, DEFAULT_CLIENT_ID, REFRESH_TIMEOUT};

/// Largest credential file read. The real files are a few kilobytes; anything larger is not one
/// of ours, and refusing it keeps an unrelated large file from being parsed into memory.
pub const MAX_CREDS_BYTES: u64 = 64 * 1024;

/// A token is not handed out when it expires inside this window, so a call cannot start against
/// a token that expires while the request is in flight.
pub const MIN_REMAINING: Duration = Duration::from_secs(60);

/// A token AntiCraft cannot renew counts as nearing expiry inside this window: its file is
/// re-read, since the app that owns the session may have renewed it.
pub const REREAD_WINDOW: Duration = Duration::from_secs(5 * 60);

/// Least time between two re-reads of the file while the token is still valid.
pub const REREAD_INTERVAL: Duration = Duration::from_secs(30);

/// Longest single wait in [`ImportedTokenSource::watch`]: a computer that slept past the expiry
/// notices within this time of waking up.
pub const WATCH_TICK_MAX: Duration = Duration::from_secs(60);

/// OAuth scope Code Assist needs.
pub const CODE_ASSIST_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";

/// Antigravity's own installed-app client ID, as the `aud` claim of the `id_token` in its
/// credential file names it. Only used to say which app issued a session. First-party
/// Antigravity sign-in is a separate module (`auth::external::antigravity`) with its own client
/// table; the importer here still cannot renew a session it did not mint.
pub const ANTIGRAVITY_CLIENT_ID: &str = "1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com";

/// The OAuth client that issued an imported session (the `aud` claim of its `id_token`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Issuer {
    /// gemini-cli's installed-app client, which AntiCraft signs in with as well.
    AntiCraft,
    /// Antigravity's own sign-in client.
    Antigravity,
    /// Any other client, or a file without an `id_token`.
    Unknown,
}

impl Issuer {
    pub fn from_audience(audience: Option<&str>) -> Self {
        match audience {
            Some(DEFAULT_CLIENT_ID) => Self::AntiCraft,
            Some(ANTIGRAVITY_CLIENT_ID) => Self::Antigravity,
            _ => Self::Unknown,
        }
    }

    /// Name of the issuing client for a label.
    pub fn label(self) -> &'static str {
        match self {
            Self::AntiCraft => "AntiCraft / Gemini CLI",
            Self::Antigravity => "Antigravity",
            Self::Unknown => "Unknown app",
        }
    }

    /// The issuing client inside a sentence ("issued to ...").
    fn client_phrase(self) -> &'static str {
        match self {
            Self::AntiCraft => "the Gemini CLI sign-in client that AntiCraft also uses",
            Self::Antigravity => "Antigravity's own sign-in client",
            Self::Unknown => "a sign-in client AntiCraft does not know",
        }
    }

    /// The app that can renew the session, inside a sentence.
    fn app_phrase(self) -> &'static str {
        match self {
            Self::AntiCraft => "the Gemini CLI",
            Self::Antigravity => "Antigravity",
            Self::Unknown => "the other app",
        }
    }

    /// "Antigravity session" without an article.
    pub fn session_noun(self) -> &'static str {
        match self {
            Self::AntiCraft => "Gemini CLI session",
            Self::Antigravity => "Antigravity session",
            Self::Unknown => "imported session",
        }
    }
}

/// Credentials read from another application's store.
#[derive(Clone, PartialEq, Eq)]
pub struct ImportedCreds {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expiry: Option<DateTime<Utc>>,
    pub scopes: Vec<String>,
    /// `aud` from the (unverified) `id_token` payload: the OAuth client that issued the token.
    pub audience: Option<String>,
    /// Account email: the `email` claim of the `id_token`, else the active entry of
    /// `google_accounts.json` next to the file.
    pub account: Option<String>,
    /// File the credentials came from.
    pub source: PathBuf,
}

impl std::fmt::Debug for ImportedCreds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImportedCreds")
            .field("access_token", &"<redacted>")
            .field("has_refresh_token", &self.refresh_token.is_some())
            .field("expiry", &self.expiry)
            .field("scopes", &self.scopes)
            .field("audience", &self.audience)
            .field("account", &self.account)
            .field("source", &self.source)
            .finish()
    }
}

impl ImportedCreds {
    pub fn issuer(&self) -> Issuer {
        Issuer::from_audience(self.audience.as_deref())
    }

    /// Whether the refresh token can be renewed by *this* application. True only when the token
    /// was issued for our client; see the module docs.
    pub fn refreshable(&self) -> bool {
        self.refresh_token.is_some() && self.audience.as_deref() == Some(DEFAULT_CLIENT_ID)
    }

    /// True when the access token is not about to expire.
    pub fn has_live_token(&self, now: DateTime<Utc>) -> bool {
        match self.expiry {
            Some(exp) => exp - now > chrono::Duration::from_std(MIN_REMAINING).unwrap_or_default(),
            // No expiry recorded: the token is usable until the server rejects it.
            None => true,
        }
    }

    /// Usable now, either because the token is live or because it can be refreshed.
    pub fn is_usable(&self, now: DateTime<Utc>) -> bool {
        self.has_live_token(now) || self.refreshable()
    }

    /// `Some(false)` when the file lists its scopes and the one Code Assist needs is missing;
    /// `None` when the file does not say.
    pub fn code_assist_scope(&self) -> Option<bool> {
        (!self.scopes.is_empty()).then(|| self.scopes.iter().any(|s| s == CODE_ASSIST_SCOPE))
    }

    /// The access token expires inside [`REREAD_WINDOW`].
    fn near_expiry(&self, now: DateTime<Utc>) -> bool {
        self.expiry.is_some_and(|exp| exp - now <= chrono::Duration::from_std(REREAD_WINDOW).unwrap_or_default())
    }
}

/// Directories [`find_credential_files_in`] searches. [`SearchRoots::user`] is the real set;
/// tests point every root at a temp folder, so no test reads the files of the machine it runs on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchRoots {
    /// AntiCraft's own data folder. The app leaves it unset: its own sign-in belongs to
    /// `AuthManager`, and offering it here as "another tool's session" would only confuse.
    pub data_dir: Option<PathBuf>,
    /// The user's home folder, which holds `.gemini`.
    pub home: Option<PathBuf>,
    /// Application-data folders that may hold an `Antigravity` folder.
    pub app_data: Vec<PathBuf>,
}

impl SearchRoots {
    /// The current user's home and application-data folders.
    pub fn user() -> Self {
        Self {
            data_dir: None,
            home: dirs::home_dir(),
            app_data: [dirs::data_dir(), dirs::data_local_dir()].into_iter().flatten().collect(),
        }
    }
}

/// Every credential file worth trying, most authoritative first.
///
/// `data_dir` is AntiCraft's own directory: a credential file there (written by an earlier
/// explicit login) wins over anything another application left behind, because a stale token in
/// a lower-ranked file must not shadow a live one.
pub fn find_credential_files(data_dir: Option<&Path>) -> Vec<PathBuf> {
    find_credential_files_in(&SearchRoots { data_dir: data_dir.map(Path::to_path_buf), ..SearchRoots::user() })
}

/// [`find_credential_files`] over explicit roots. Only these plain JSON files are ever read.
pub fn find_credential_files_in(roots: &SearchRoots) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let push = |p: PathBuf, out: &mut Vec<PathBuf>| {
        if p.is_file() && !out.contains(&p) {
            out.push(p);
        }
    };
    if let Some(d) = &roots.data_dir {
        push(d.join("oauth_creds.json"), &mut out);
    }
    if let Some(home) = &roots.home {
        push(home.join(".gemini").join("oauth_creds.json"), &mut out);
        push(home.join(".gemini").join("antigravity").join("oauth_creds.json"), &mut out);
    }
    // Antigravity's own app-data directories: the desktop build keeps its state here.
    for base in &roots.app_data {
        let dir = base.join("Antigravity");
        for name in ["oauth_creds.json", "credentials.json"] {
            push(dir.join(name), &mut out);
        }
        push(dir.join("User").join("oauth_creds.json"), &mut out);
    }
    out
}

/// `google_accounts.json` next to a credential file, when it exists.
fn accounts_file_for(creds: &Path) -> Option<PathBuf> {
    let p = creds.parent()?.join("google_accounts.json");
    p.is_file().then_some(p)
}

/// The `active` account from `google_accounts.json`. Returns `None` when the file is missing or
/// has an unexpected shape: the account name is only a label, never a reason to fail.
pub fn active_account(creds_path: &Path) -> Option<String> {
    let path = accounts_file_for(creds_path)?;
    let bytes = read_capped(&path)?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    v.get("active").and_then(|a| a.as_str()).filter(|s| !s.trim().is_empty()).map(str::to_owned)
}

/// Parse one credential file. `None` means "not usable, try the next one": a partial or corrupt
/// file is not an error worth surfacing, and the caller has other candidates.
pub fn parse_file(path: &Path) -> Option<ImportedCreds> {
    let bytes = read_capped(path)?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let access_token = v.get("access_token").and_then(|t| t.as_str()).filter(|t| !t.is_empty())?;
    let refresh_token = v.get("refresh_token").and_then(|t| t.as_str()).filter(|t| !t.is_empty()).map(str::to_owned);
    let expiry = v
        .get("expiry_date")
        .and_then(serde_json::Value::as_i64)
        .and_then(chrono::DateTime::from_timestamp_millis);
    let scopes = v
        .get("scope")
        .and_then(|s| s.as_str())
        .map(|s| s.split_whitespace().map(str::to_owned).collect())
        .unwrap_or_default();
    let claims = v.get("id_token").and_then(|t| t.as_str()).and_then(id_token_claims).unwrap_or_default();
    Some(ImportedCreds {
        access_token: access_token.to_owned(),
        refresh_token,
        expiry,
        scopes,
        audience: claims.audience,
        account: claims.email.or_else(|| active_account(path)),
        source: path.to_path_buf(),
    })
}

/// The `id_token` claims this module reads.
#[derive(Debug, Default)]
struct IdClaims {
    audience: Option<String>,
    email: Option<String>,
}

/// `aud` and `email` claims of a JWT. The signature is deliberately not verified: the claims only
/// name the issuing client and the account and decide whether *we* may refresh; nothing is
/// authorized on them.
fn id_token_claims(id_token: &str) -> Option<IdClaims> {
    let mut parts = id_token.split('.');
    let _header = parts.next()?;
    let payload = parts.next()?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    let text = |key: &str| v.get(key).and_then(|a| a.as_str()).map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned);
    Some(IdClaims { audience: text("aud"), email: text("email") })
}

/// `aud` claim of a JWT (unverified, see [`id_token_claims`]).
pub fn id_token_audience(id_token: &str) -> Option<String> {
    id_token_claims(id_token)?.audience
}

/// First usable credential file, in [`find_credential_files`] order.
pub fn load(data_dir: Option<&Path>) -> Option<ImportedCreds> {
    let now = chrono::Utc::now();
    for path in find_credential_files(data_dir) {
        match parse_file(&path) {
            Some(creds) if creds.is_usable(now) => return Some(creds),
            Some(_) => tracing::debug!("credential file {} is expired and cannot be refreshed", path.display()),
            None => tracing::debug!("credential file {} is not usable", path.display()),
        }
    }
    None
}

fn read_capped(path: &Path) -> Option<Vec<u8>> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_CREDS_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    (bytes.len() as u64 <= MAX_CREDS_BYTES).then_some(bytes)
}

// ---------------------------------------------------------------------------
// Summary for the UI
// ---------------------------------------------------------------------------

/// One session found by [`inspect`], without any token value: safe for the UI and the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportSummary {
    pub source: PathBuf,
    pub issuer: Issuer,
    pub account: Option<String>,
    /// Expiry of the access token (`None`: the file does not say).
    pub expiry: Option<DateTime<Utc>>,
    pub has_refresh_token: bool,
    /// `Some(false)` when the file's scopes leave out the one Code Assist needs.
    pub code_assist_scope: Option<bool>,
    /// AntiCraft's OAuth client secret was configured when the summary was made.
    pub client_configured: bool,
    /// AntiCraft can renew the access token itself.
    pub can_refresh: bool,
    /// A request can be sent with it now (a live token, or one AntiCraft can renew).
    pub usable: bool,
}

impl ImportSummary {
    pub fn of(creds: &ImportedCreds, client_configured: bool, now: DateTime<Utc>) -> Self {
        let can_refresh = client_configured && creds.refreshable();
        let code_assist_scope = creds.code_assist_scope();
        Self {
            source: creds.source.clone(),
            issuer: creds.issuer(),
            account: creds.account.clone(),
            expiry: creds.expiry,
            has_refresh_token: creds.refresh_token.is_some(),
            code_assist_scope,
            client_configured,
            can_refresh,
            usable: code_assist_scope != Some(false) && (creds.has_live_token(now) || can_refresh),
        }
    }

    /// "Antigravity session · user@example.com".
    pub fn title(&self) -> String {
        session_title(self.issuer, self.account.as_deref())
    }

    /// What the session is, whether AntiCraft can use and renew it, and what to do instead when
    /// it cannot.
    pub fn explain(&self, now: DateTime<Utc>) -> String {
        let advice = if self.client_configured {
            "Sign in with Google using the same account: it gives the same Code Assist access."
        } else {
            "Sign in with Google using the same account once the OAuth client secret is set below: it gives the same Code Assist access."
        };
        let client = self.issuer.client_phrase();
        if self.code_assist_scope == Some(false) {
            return format!(
                "This session was issued to {client} without the Google Cloud access Code Assist needs, so AntiCraft cannot use it. {advice}"
            );
        }
        if self.usable {
            if self.can_refresh {
                return format!("Issued to {client}, so AntiCraft can renew it while you use it.");
            }
            return match self.expiry {
                Some(exp) => format!(
                    "Issued to {client}. Valid until {} ({} min left). AntiCraft cannot renew it: near the end it re-reads the file in case {} renewed it, and otherwise asks you to sign in.",
                    local_time(exp),
                    minutes_left(exp, now),
                    self.issuer.app_phrase(),
                ),
                None => format!("Issued to {client}. The file does not say when it expires, so AntiCraft uses it until Google rejects it."),
            };
        }
        let when = self.expiry.map(|e| when_expired(e, now)).unwrap_or_else(|| "earlier".to_owned());
        match self.issuer {
            Issuer::AntiCraft if self.has_refresh_token => format!(
                "This session was issued to {client} and expired {when}. AntiCraft can renew it once the OAuth client secret is set below, or you can sign in with Google."
            ),
            Issuer::AntiCraft => format!(
                "This session was issued to {client} and expired {when}. Its file holds no refresh token, so AntiCraft cannot renew it. {advice}"
            ),
            _ => format!(
                "This session was issued to {client} and expired {when}. AntiCraft cannot renew it, because only {} can renew a session issued to its sign-in client. Signing in to {} directly on the Account tab gives a session that renews itself. {advice}",
                self.issuer.app_phrase(),
                self.issuer.app_phrase(),
            ),
        }
    }
}

/// Result of looking for a session another Google tool saved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportCheck {
    /// No credential file in any place AntiCraft looks.
    NotFound,
    /// Credential files exist, but none holds an access token AntiCraft can read.
    Unreadable(Vec<PathBuf>),
    /// The best session found: a usable one first, else the one that expired last.
    Found(ImportSummary),
}

impl ImportCheck {
    /// The session found, when a request can be sent with it now.
    pub fn usable(&self) -> Option<&ImportSummary> {
        match self {
            Self::Found(s) if s.usable => Some(s),
            _ => None,
        }
    }
}

/// Look for a session another Google tool saved under `roots` and describe the best one.
/// Blocking file IO: run it off the async runtime's workers. `client_configured` says whether
/// AntiCraft's own OAuth client secret is set, which renewing a gemini-cli session needs.
pub fn inspect(roots: &SearchRoots, client_configured: bool, now: DateTime<Utc>) -> ImportCheck {
    let files = find_credential_files_in(roots);
    if files.is_empty() {
        return ImportCheck::NotFound;
    }
    // Usable before unusable, renewable before not; among unusable ones the later expiry is the
    // most telling. Ties keep the earlier, more authoritative file.
    let rank = |s: &ImportSummary| {
        let expiry = if s.usable { i64::MIN } else { s.expiry.map_or(i64::MAX, |e| e.timestamp_millis()) };
        (s.usable, s.can_refresh, expiry)
    };
    let mut best: Option<ImportSummary> = None;
    let mut unreadable = Vec::new();
    for path in files {
        match parse_file(&path) {
            Some(creds) => {
                let s = ImportSummary::of(&creds, client_configured, now);
                if best.as_ref().is_none_or(|b| rank(&s) > rank(b)) {
                    best = Some(s);
                }
            }
            None => unreadable.push(path),
        }
    }
    match best {
        Some(s) => ImportCheck::Found(s),
        None => ImportCheck::Unreadable(unreadable),
    }
}

/// The imported session in use, without any token value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub source: PathBuf,
    pub issuer: Issuer,
    pub account: Option<String>,
    pub expiry: Option<DateTime<Utc>>,
    pub can_refresh: bool,
}

impl SessionInfo {
    /// "Antigravity session · user@example.com".
    pub fn title(&self) -> String {
        session_title(self.issuer, self.account.as_deref())
    }

    /// One line on how the session is used and how long it lasts.
    pub fn describe(&self, now: DateTime<Utc>) -> String {
        let from = format!("Code Assist turns use the {} from {}.", self.issuer.session_noun(), self.source.display());
        if self.can_refresh {
            return format!("{from} AntiCraft renews it as needed.");
        }
        match self.expiry {
            Some(exp) => format!(
                "{from} Valid until {} ({} min left). AntiCraft cannot renew it, so it re-reads the file before then in case {} renewed it.",
                local_time(exp),
                minutes_left(exp, now),
                self.issuer.app_phrase(),
            ),
            None => format!("{from} The file does not say when it expires; AntiCraft uses it until Google rejects it."),
        }
    }
}

fn session_title(issuer: Issuer, account: Option<&str>) -> String {
    let name = match issuer {
        Issuer::AntiCraft => "Gemini CLI session",
        Issuer::Antigravity => "Antigravity session",
        Issuer::Unknown => "Session from another app",
    };
    match account {
        Some(a) => format!("{name} · {a}"),
        None => name.to_owned(),
    }
}

/// "Antigravity session for user@example.com", for the end reasons.
fn session_words(creds: &ImportedCreds) -> String {
    let noun = creds.issuer().session_noun();
    match creds.account.as_deref() {
        Some(a) => format!("{noun} for {a}"),
        None => noun.to_owned(),
    }
}

/// `HH:MM` in local time.
fn local_time(t: DateTime<Utc>) -> String {
    t.with_timezone(&Local).format("%H:%M").to_string()
}

/// Whole minutes until `t`, rounded up.
fn minutes_left(t: DateTime<Utc>, now: DateTime<Utc>) -> i64 {
    ((t - now).num_seconds().max(0) + 59) / 60
}

/// "today at 14:05" or "on 2026-10-01" (local time).
fn when_expired(t: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let local = t.with_timezone(&Local);
    if local.date_naive() == now.with_timezone(&Local).date_naive() {
        format!("today at {}", local.format("%H:%M"))
    } else {
        format!("on {}", local.format("%Y-%m-%d"))
    }
}

// ---------------------------------------------------------------------------
// Token sourcing
// ---------------------------------------------------------------------------

/// A change of an imported session worth reporting, from [`ImportedTokenSource::watch`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Watch {
    /// The access token was replaced (a newer one from the file, or a renewal); its expiry.
    Renewed(Option<DateTime<Utc>>),
    /// Nothing valid is left. The text says why and what to do; it holds no token.
    Ended(String),
}

/// Outcome of re-reading the credential file.
enum Reread {
    /// A newer token from the file is now in use.
    Adopted,
    /// Nothing newer: the same token, an expired one or an older one.
    Unchanged,
    /// The file is gone or holds no access token any more.
    Missing,
    /// The file now holds a session for this other account; it is not adopted.
    OtherAccount(String),
}

/// Supplies tokens from imported credentials, refreshing only when that is actually possible and
/// otherwise re-reading the source file as the token nears expiry.
pub struct ImportedTokenSource {
    state: Mutex<ImportedCreds>,
    /// Config used to refresh. `None` means access-token-only mode.
    refresh: Option<AuthConfig>,
    http: reqwest::Client,
    /// Single-flight gate so two concurrent calls cannot both refresh or re-read.
    gate: Mutex<()>,
    /// Cancelled once no valid token is left; `end_reason` says why.
    ended: CancellationToken,
    end_reason: std::sync::Mutex<Option<String>>,
    /// When the file was last re-read (bounds re-reads while the token is still valid).
    last_reread: std::sync::Mutex<Option<Instant>>,
    /// Bumped every time the access token is replaced.
    renewals: AtomicU64,
}

impl std::fmt::Debug for ImportedTokenSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImportedTokenSource")
            .field("can_refresh", &self.can_refresh())
            .field("expires_at", &self.expires_at())
            .field("account", &self.account())
            .field("ended", &self.has_ended())
            .finish_non_exhaustive()
    }
}

impl ImportedTokenSource {
    /// Wrap imported credentials. `refresh` is only used when the token was issued for our
    /// client; passing a config for a token owned by another client has no effect.
    pub fn new(creds: ImportedCreds, refresh: Option<AuthConfig>, http: reqwest::Client) -> Arc<Self> {
        let refresh = refresh.filter(|_| creds.refreshable());
        Arc::new(Self {
            state: Mutex::new(creds),
            refresh,
            http,
            gate: Mutex::new(()),
            ended: CancellationToken::new(),
            end_reason: std::sync::Mutex::new(None),
            last_reread: std::sync::Mutex::new(None),
            renewals: AtomicU64::new(0),
        })
    }

    pub fn expires_at(&self) -> Option<DateTime<Utc>> {
        self.state.try_lock().ok().and_then(|s| s.expiry)
    }

    pub fn can_refresh(&self) -> bool {
        self.refresh.is_some()
    }

    pub fn account(&self) -> Option<String> {
        self.state.try_lock().ok().and_then(|s| s.account.clone())
    }

    /// The file the credentials came from, for a status line.
    pub fn source(&self) -> Option<PathBuf> {
        self.state.try_lock().ok().map(|s| s.source.clone())
    }

    /// Token-free description of the session for the UI.
    pub async fn info(&self) -> SessionInfo {
        let s = self.state.lock().await;
        SessionInfo {
            source: s.source.clone(),
            issuer: s.issuer(),
            account: s.account.clone(),
            expiry: s.expiry,
            can_refresh: self.can_refresh(),
        }
    }

    /// No valid token is left; every further request fails with [`AuthError::ReauthRequired`].
    pub fn has_ended(&self) -> bool {
        self.ended.is_cancelled()
    }

    /// Why the session ended, once it has.
    pub fn end_reason(&self) -> Option<String> {
        self.end_reason.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn end(&self, reason: String) {
        {
            let mut slot = self.end_reason.lock().unwrap_or_else(|e| e.into_inner());
            if slot.is_none() {
                tracing::info!("imported session ended: {reason}");
                *slot = Some(reason);
            }
        }
        self.ended.cancel();
    }

    /// How long until the file may be re-read again (zero: now).
    fn reread_wait(&self) -> Duration {
        match *self.last_reread.lock().unwrap_or_else(|e| e.into_inner()) {
            Some(at) => REREAD_INTERVAL.saturating_sub(at.elapsed()),
            None => Duration::ZERO,
        }
    }

    /// A token AntiCraft cannot renew is near its end and the file is due for a re-read.
    fn reread_wanted(&self, s: &ImportedCreds, now: DateTime<Utc>) -> bool {
        self.refresh.is_none() && s.near_expiry(now) && self.reread_wait().is_zero()
    }

    /// When [`Self::poll`] has something to do next: `None` for a session that renews itself or
    /// has no recorded expiry.
    pub async fn next_check_in(&self, now: DateTime<Utc>) -> Option<Duration> {
        if self.refresh.is_some() || self.has_ended() {
            return None;
        }
        let expiry = self.state.lock().await.expiry?;
        let remaining = (expiry - now).to_std().unwrap_or(Duration::ZERO);
        if remaining > REREAD_WINDOW {
            return Some(remaining - REREAD_WINDOW);
        }
        // Near the end: re-read every REREAD_INTERVAL, and once more when the token runs out.
        Some(self.reread_wait().min(remaining.saturating_sub(MIN_REMAINING)))
    }

    /// Re-read the file of a token that cannot be renewed when it nears expiry, and end the
    /// session once nothing valid is left. Requests do the same; this covers an idle app.
    pub async fn poll(&self) {
        if self.refresh.is_none() && !self.has_ended() {
            let _ = self.obtain(false, None).await;
        }
    }

    /// Wait for the next change worth reporting: the token was replaced, or the session ended.
    /// Returns `None` once `stop` is cancelled.
    pub async fn watch(&self, stop: &CancellationToken) -> Option<Watch> {
        let seen = self.renewals.load(Ordering::SeqCst);
        loop {
            if self.has_ended() {
                let reason = self.end_reason().unwrap_or_else(|| "The imported session ended.".to_owned());
                return Some(Watch::Ended(reason));
            }
            if self.renewals.load(Ordering::SeqCst) != seen {
                return Some(Watch::Renewed(self.state.lock().await.expiry));
            }
            let wait = self.next_check_in(Utc::now()).await.map_or(WATCH_TICK_MAX, |d| d.min(WATCH_TICK_MAX));
            tokio::select! {
                biased;
                _ = stop.cancelled() => return None,
                _ = self.ended.cancelled() => {}
                _ = tokio::time::sleep(wait) => self.poll().await,
            }
        }
    }

    async fn refresh_now(&self) -> Result<String, AuthError> {
        let config = self.refresh.as_ref().ok_or(AuthError::ReauthRequired)?;
        let rt = {
            let s = self.state.lock().await;
            s.refresh_token.clone().ok_or(AuthError::ReauthRequired)?
        };
        let tokens = tokio::time::timeout(REFRESH_TIMEOUT, refresh_token(&self.http, config, &rt))
            .await
            .unwrap_or_else(|_| Err(AuthError::Http("token refresh timed out".into())))?;
        // Kept in memory only: neither the other app's file nor AntiCraft's store is written.
        let mut s = self.state.lock().await;
        s.access_token = tokens.access_token.clone();
        if let Some(secs) = tokens.expires_in {
            s.expiry = chrono::DateTime::from_timestamp_millis(chrono::Utc::now().timestamp_millis() + secs * 1000);
        }
        if let Some(new_rt) = tokens.refresh_token {
            s.refresh_token = Some(new_rt);
        }
        self.renewals.fetch_add(1, Ordering::SeqCst);
        Ok(tokens.access_token)
    }

    /// Re-read the source file and adopt a newer token from it. `rejected`: the server refused
    /// the current token, so any other valid token is newer. `failed` is what the server refused,
    /// which is the current token unless a caller passed an older one.
    async fn reread(&self, rejected: bool, failed: Option<&str>) -> Reread {
        let (path, account) = {
            let s = self.state.lock().await;
            (s.source.clone(), s.account.clone())
        };
        *self.last_reread.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
        let fresh = match tokio::task::spawn_blocking(move || parse_file(&path)).await {
            Ok(Some(c)) => c,
            Ok(None) => return Reread::Missing,
            Err(e) => {
                tracing::warn!("could not re-read the imported session: {e}");
                return Reread::Unchanged;
            }
        };
        // The other app switched accounts: never move AntiCraft to another account silently.
        if let (Some(old), Some(new)) = (account.as_deref(), fresh.account.as_deref()) {
            if !old.eq_ignore_ascii_case(new) {
                return Reread::OtherAccount(new.to_owned());
            }
        }
        let now = Utc::now();
        let mut s = self.state.lock().await;
        // `failed` is the token the caller had refused; comparing against it (rather than against
        // whatever is in memory now) is what lets two parallel 401s agree that the file holds
        // nothing newer than the token that just failed, so neither reports success on a replay.
        let refused = failed.unwrap_or(s.access_token.as_str());
        if fresh.access_token == refused || !fresh.has_live_token(now) {
            return Reread::Unchanged;
        }
        let newer = rejected
            || !s.has_live_token(now)
            || match (fresh.expiry, s.expiry) {
                (Some(f), Some(c)) => f > c,
                _ => true,
            };
        if !newer {
            return Reread::Unchanged;
        }
        // The refresh mode stays as chosen: only the access token and what describes it change.
        s.access_token = fresh.access_token;
        s.expiry = fresh.expiry;
        s.scopes = fresh.scopes;
        if s.account.is_none() {
            s.account = fresh.account;
        }
        self.renewals.fetch_add(1, Ordering::SeqCst);
        Reread::Adopted
    }

    /// A valid access token: the current one, a renewed one, or a newer one from the file.
    /// `rejected` means the server refused the token named by `failed` (a 401); passing the token
    /// that was actually refused is what keeps "the file holds nothing newer" honest.
    async fn obtain(&self, rejected: bool, failed: Option<&str>) -> Result<String, AuthError> {
        if self.has_ended() {
            return Err(AuthError::ReauthRequired);
        }
        if !rejected {
            let s = self.state.lock().await;
            let now = Utc::now();
            if s.has_live_token(now) && !self.reread_wanted(&s, now) {
                return Ok(s.access_token.clone());
            }
        }
        let _gate = self.gate.lock().await;
        if self.has_ended() {
            return Err(AuthError::ReauthRequired);
        }
        if !rejected {
            // Another caller may have renewed or re-read while this one waited.
            let s = self.state.lock().await;
            let now = Utc::now();
            if s.has_live_token(now) && !self.reread_wanted(&s, now) {
                return Ok(s.access_token.clone());
            }
        }
        if self.refresh.is_some() {
            return match self.refresh_now().await {
                Err(AuthError::ReauthRequired) => {
                    let s = self.state.lock().await;
                    let reason = format!(
                        "Google refused to renew the {} (it may have been signed out or revoked). Sign in with Google to continue.",
                        session_words(&s)
                    );
                    drop(s);
                    self.end(reason);
                    Err(AuthError::ReauthRequired)
                }
                other => other,
            };
        }
        let outcome = self.reread(rejected, failed).await;
        let s = self.state.lock().await;
        let now = Utc::now();
        if matches!(outcome, Reread::Adopted) || (!rejected && s.has_live_token(now)) {
            return Ok(s.access_token.clone());
        }
        let name = session_words(&s);
        let reason = match outcome {
            Reread::OtherAccount(other) => format!(
                "The {name} ended: its file now holds a session for {other}. Choose Check again on the Account tab to use that one, or sign in with Google."
            ),
            Reread::Missing => format!(
                "The {name} ended: {} no longer holds a session (the app may have signed out). Sign in with Google to continue.",
                s.source.display()
            ),
            _ if rejected => format!(
                "Google no longer accepts the {name} (it may have been signed out or revoked), and its file holds no newer token. Sign in with Google to continue."
            ),
            _ => {
                // A token is given up in its last minute, so the expiry may still be ahead.
                let when = match s.expiry {
                    Some(e) if e > now => format!("expires at {}", local_time(e)),
                    Some(e) => format!("expired {}", when_expired(e, now)),
                    None => "expired".to_owned(),
                };
                format!("The {name} {when} and its file holds no newer token. AntiCraft cannot renew it. Sign in with Google to continue.")
            }
        };
        drop(s);
        self.end(reason);
        Err(AuthError::ReauthRequired)
    }
}

impl TokenProvider for ImportedTokenSource {
    fn access_token(&self) -> BoxFuture<'_, Result<String, AuthError>> {
        Box::pin(self.obtain(false, None))
    }

    fn force_refresh<'a>(&'a self, failed: &'a str) -> BoxFuture<'a, Result<String, AuthError>> {
        Box::pin(self.obtain(true, Some(failed)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{extract::State, routing::post, Router};
    use std::sync::atomic::AtomicUsize;

    fn write_creds(dir: &Path, json: serde_json::Value) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let p = dir.join("oauth_creds.json");
        std::fs::write(&p, serde_json::to_vec(&json).unwrap()).unwrap();
        p
    }

    fn jwt(claims: serde_json::Value) -> String {
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"{\"alg\":\"none\"}");
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap());
        format!("{header}.{payload}.sig")
    }

    fn id_token(aud: &str) -> String {
        jwt(serde_json::json!({"aud": aud}))
    }

    fn at(rfc3339: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc3339).unwrap().with_timezone(&Utc)
    }

    /// Search roots with only a temp home folder.
    fn home_roots(home: &Path) -> SearchRoots {
        SearchRoots { home: Some(home.to_path_buf()), ..SearchRoots::default() }
    }

    /// A credential file in `dir` for `account`, with a token valid for `minutes` (negative:
    /// expired) issued to `aud`.
    fn session_file(dir: &Path, token: &str, minutes: i64, aud: &str, account: &str) -> PathBuf {
        write_creds(
            dir,
            serde_json::json!({
                "access_token": token,
                "refresh_token": "rt-secret-value",
                "expiry_date": (Utc::now() + chrono::Duration::minutes(minutes)).timestamp_millis(),
                "scope": format!("openid {CODE_ASSIST_SCOPE}"),
                "id_token": jwt(serde_json::json!({"aud": aud, "email": account})),
            }),
        )
    }

    #[test]
    fn parses_a_typical_file_and_redacts_every_secret() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_creds(
            dir.path(),
            serde_json::json!({
                "access_token": "at-secret-value",
                "refresh_token": "rt-secret-value",
                "expiry_date": chrono::Utc::now().timestamp_millis() + 3_600_000,
                "scope": "openid  https://www.googleapis.com/auth/cloud-platform",
                "id_token": id_token(DEFAULT_CLIENT_ID),
            }),
        );
        let c = parse_file(&p).expect("parses");
        assert_eq!(c.access_token, "at-secret-value");
        assert_eq!(c.refresh_token.as_deref(), Some("rt-secret-value"));
        assert_eq!(c.scopes.len(), 2);
        assert_eq!(c.audience.as_deref(), Some(DEFAULT_CLIENT_ID));
        assert!(c.refreshable());
        let shown = format!("{c:?}");
        for secret in ["at-secret-value", "rt-secret-value"] {
            assert!(!shown.contains(secret), "Debug leaked {secret}: {shown}");
        }
        assert!(shown.contains("<redacted>"));
    }

    #[test]
    fn a_foreign_audience_is_not_refreshable() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_creds(
            dir.path(),
            serde_json::json!({
                "access_token": "at",
                "refresh_token": "rt",
                "id_token": id_token("1071006060591-someoneelse.apps.googleusercontent.com"),
            }),
        );
        let c = parse_file(&p).unwrap();
        assert!(!c.refreshable(), "another client's refresh token cannot be used by us");
        assert_eq!(c.issuer(), Issuer::Unknown);
        assert!(c.has_live_token(chrono::Utc::now()), "no expiry means usable until rejected");
    }

    #[test]
    fn the_issuer_follows_the_audience() {
        assert_eq!(Issuer::from_audience(Some(DEFAULT_CLIENT_ID)), Issuer::AntiCraft);
        assert_eq!(Issuer::from_audience(Some(ANTIGRAVITY_CLIENT_ID)), Issuer::Antigravity);
        assert_eq!(Issuer::from_audience(Some("other.apps.googleusercontent.com")), Issuer::Unknown);
        assert_eq!(Issuer::from_audience(None), Issuer::Unknown);
    }

    #[test]
    fn an_expired_unrefreshable_file_is_not_loaded() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_creds(
            dir.path(),
            serde_json::json!({
                "access_token": "at",
                "expiry_date": chrono::Utc::now().timestamp_millis() - 60_000,
                "id_token": id_token("foreign-client"),
            }),
        );
        assert!(parse_file(&p).is_some());
        // `load` walks real directories; parse + is_usable is the same decision without IO order.
        let c = parse_file(&p).unwrap();
        assert!(!c.is_usable(chrono::Utc::now()));
    }

    #[test]
    fn missing_access_token_and_oversized_files_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_creds(dir.path(), serde_json::json!({"refresh_token": "rt"}));
        assert!(parse_file(&p).is_none(), "no access token: not usable");
        let big = dir.path().join("big.json");
        std::fs::write(&big, vec![b'x'; MAX_CREDS_BYTES as usize + 1]).unwrap();
        assert!(parse_file(&big).is_none(), "an oversized file is refused");
    }

    #[test]
    fn the_account_comes_from_the_id_token_else_the_neighbouring_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_creds(dir.path(), serde_json::json!({"access_token": "at"}));
        std::fs::write(dir.path().join("google_accounts.json"), r#"{"active":"someone@example.com","old":[]}"#).unwrap();
        assert_eq!(parse_file(&p).unwrap().account.as_deref(), Some("someone@example.com"));
        std::fs::write(dir.path().join("google_accounts.json"), "not json").unwrap();
        assert_eq!(parse_file(&p).unwrap().account, None, "a bad label file is not an error");
        let p = write_creds(
            dir.path(),
            serde_json::json!({"access_token": "at", "id_token": jwt(serde_json::json!({"aud": "x", "email": "token@example.com"}))}),
        );
        assert_eq!(parse_file(&p).unwrap().account.as_deref(), Some("token@example.com"));
    }

    #[test]
    fn only_the_listed_plain_files_are_found_in_order() {
        let home = tempfile::tempdir().unwrap();
        let app = tempfile::tempdir().unwrap();
        let roots = SearchRoots { home: Some(home.path().into()), app_data: vec![app.path().into()], ..SearchRoots::default() };
        assert!(find_credential_files_in(&roots).is_empty());
        let a = write_creds(&app.path().join("Antigravity"), serde_json::json!({"access_token": "at"}));
        let g = write_creds(&home.path().join(".gemini"), serde_json::json!({"access_token": "at"}));
        // Other files in the same folders are never picked up.
        std::fs::write(app.path().join("Antigravity").join("state.vscdb"), b"x").unwrap();
        assert_eq!(find_credential_files_in(&roots), vec![g, a]);
    }

    #[test]
    fn nothing_found_and_unreadable_files_are_reported() {
        let home = tempfile::tempdir().unwrap();
        let roots = home_roots(home.path());
        assert_eq!(inspect(&roots, true, Utc::now()), ImportCheck::NotFound);
        let p = write_creds(&home.path().join(".gemini"), serde_json::json!({"refresh_token": "rt"}));
        assert_eq!(inspect(&roots, true, Utc::now()), ImportCheck::Unreadable(vec![p]));
    }

    /// The session this machine has: Antigravity's client, a refresh token AntiCraft cannot use,
    /// and an access token that expired on 2026-10-01.
    #[test]
    fn an_expired_antigravity_session_says_why_and_what_to_do() {
        let home = tempfile::tempdir().unwrap();
        let id = jwt(serde_json::json!({"aud": ANTIGRAVITY_CLIENT_ID, "email": "someone@example.com"}));
        write_creds(
            &home.path().join(".gemini"),
            serde_json::json!({
                "access_token": "at-secret-value",
                "refresh_token": "rt-secret-value",
                "expiry_date": at("2026-10-01T12:00:00Z").timestamp_millis(),
                "scope": format!("openid {CODE_ASSIST_SCOPE}"),
                "id_token": id,
            }),
        );
        let now = at("2026-11-05T11:00:00Z");
        let check = inspect(&home_roots(home.path()), true, now);
        let ImportCheck::Found(s) = &check else { panic!("{check:?}") };
        assert_eq!(s.issuer, Issuer::Antigravity);
        assert_eq!(s.account.as_deref(), Some("someone@example.com"));
        assert!(s.has_refresh_token && !s.can_refresh && !s.usable);
        assert_eq!(check.usable(), None);
        assert_eq!(s.title(), "Antigravity session · someone@example.com");
        let text = s.explain(now);
        assert_eq!(
            text,
            "This session was issued to Antigravity's own sign-in client and expired on 2026-10-01. AntiCraft cannot renew it, \
             because only Antigravity can renew a session issued to its sign-in client. Signing in to Antigravity directly on \
             the Account tab gives a session that renews itself. Sign in with Google using the same account: it gives the \
             same Code Assist access."
        );
        // Without AntiCraft's own client the advice says what sign-in needs first.
        let unconfigured = ImportSummary { client_configured: false, ..s.clone() };
        assert!(unconfigured.explain(now).contains("once the OAuth client secret is set below"));
        let shown = format!("{check:?}{text}");
        for secret in ["at-secret-value", "rt-secret-value", id.as_str()] {
            assert!(!shown.contains(secret), "leaked a token: {shown}");
        }
    }

    #[test]
    fn a_gemini_cli_session_renews_only_with_the_client_secret() {
        let home = tempfile::tempdir().unwrap();
        session_file(&home.path().join(".gemini"), "at-old", -30, DEFAULT_CLIENT_ID, "me@example.com");
        let roots = home_roots(home.path());
        let ImportCheck::Found(without) = inspect(&roots, false, Utc::now()) else { panic!() };
        assert_eq!(without.issuer, Issuer::AntiCraft);
        assert!(!without.can_refresh && !without.usable);
        assert!(without.explain(Utc::now()).contains("can renew it once the OAuth client secret is set below"));
        let check = inspect(&roots, true, Utc::now());
        let with = check.usable().expect("renewable with the secret");
        assert!(with.can_refresh);
        assert!(with.explain(Utc::now()).contains("so AntiCraft can renew it"));
    }

    #[test]
    fn a_live_session_is_preferred_and_one_without_code_assist_access_is_not_usable() {
        let home = tempfile::tempdir().unwrap();
        session_file(&home.path().join(".gemini"), "at-1", -30, ANTIGRAVITY_CLIENT_ID, "me@example.com");
        session_file(&home.path().join(".gemini").join("antigravity"), "at-2", 40, ANTIGRAVITY_CLIENT_ID, "me@example.com");
        let check = inspect(&home_roots(home.path()), true, Utc::now());
        let s = check.usable().expect("the live one wins over the earlier expired one");
        assert!(s.source.ends_with(Path::new("antigravity").join("oauth_creds.json")), "{s:?}");
        assert!(s.explain(Utc::now()).contains("AntiCraft cannot renew it: near the end it re-reads the file"));

        let narrow = tempfile::tempdir().unwrap();
        write_creds(
            &narrow.path().join(".gemini"),
            serde_json::json!({"access_token": "at", "scope": "openid email", "id_token": id_token(ANTIGRAVITY_CLIENT_ID)}),
        );
        let ImportCheck::Found(s) = inspect(&home_roots(narrow.path()), true, Utc::now()) else { panic!() };
        assert_eq!(s.code_assist_scope, Some(false));
        assert!(!s.usable);
        assert!(s.explain(Utc::now()).contains("without the Google Cloud access Code Assist needs"));
    }

    fn creds(source: PathBuf, token: &str, expiry: Option<DateTime<Utc>>, aud: &str, account: Option<&str>) -> ImportedCreds {
        ImportedCreds {
            access_token: token.into(),
            refresh_token: Some("rt-1".into()),
            expiry,
            scopes: vec![],
            audience: Some(aud.into()),
            account: account.map(Into::into),
            source,
        }
    }

    #[tokio::test]
    async fn a_token_near_expiry_is_replaced_by_a_newer_one_from_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = session_file(dir.path(), "at-new", 60, ANTIGRAVITY_CLIENT_ID, "me@example.com");
        let before = std::fs::read(&path).unwrap();
        // 90 s left: still valid, but inside the re-read window.
        let soon = Utc::now() + chrono::Duration::seconds(90);
        let src = ImportedTokenSource::new(creds(path.clone(), "at-old", Some(soon), ANTIGRAVITY_CLIENT_ID, Some("me@example.com")), None, reqwest::Client::new());
        let stop = CancellationToken::new();
        let watched = src.watch(&stop).await;
        assert!(matches!(watched, Some(Watch::Renewed(Some(exp))) if exp > soon), "{watched:?}");
        assert_eq!(src.access_token().await.unwrap(), "at-new");
        assert!(!src.has_ended());
        assert_eq!(std::fs::read(&path).unwrap(), before, "the other app's file is never written");
        stop.cancel();
        assert_eq!(src.watch(&stop).await, None);
    }

    #[tokio::test]
    async fn an_expired_token_with_nothing_newer_ends_the_session_with_a_reason() {
        let dir = tempfile::tempdir().unwrap();
        let path = session_file(dir.path(), "at-old", -5, ANTIGRAVITY_CLIENT_ID, "me@example.com");
        let gone = Utc::now() - chrono::Duration::minutes(5);
        let src = ImportedTokenSource::new(creds(path, "at-old", Some(gone), ANTIGRAVITY_CLIENT_ID, Some("me@example.com")), None, reqwest::Client::new());
        assert!(!src.can_refresh());
        assert!(matches!(src.access_token().await, Err(AuthError::ReauthRequired)));
        assert!(src.has_ended());
        let reason = src.end_reason().unwrap();
        assert!(reason.starts_with("The Antigravity session for me@example.com expired "), "{reason}");
        assert!(reason.contains("Sign in with Google") && !reason.contains("at-old") && !reason.contains("rt-"), "{reason}");
        assert_eq!(src.watch(&CancellationToken::new()).await, Some(Watch::Ended(reason)));
        // A removed file ends a session as well, with its own reason.
        let src = ImportedTokenSource::new(creds(dir.path().join("gone.json"), "at", Some(gone), "foreign", None), None, reqwest::Client::new());
        assert!(src.force_refresh("at").await.is_err());
        assert!(src.end_reason().unwrap().contains("no longer holds a session"));
    }

    #[tokio::test]
    async fn a_rejected_token_takes_a_newer_one_from_the_file_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = session_file(dir.path(), "at-2", 50, ANTIGRAVITY_CLIENT_ID, "me@example.com");
        let later = Utc::now() + chrono::Duration::minutes(55);
        let src = ImportedTokenSource::new(creds(path, "at-1", Some(later), ANTIGRAVITY_CLIENT_ID, Some("me@example.com")), None, reqwest::Client::new());
        assert_eq!(src.access_token().await.unwrap(), "at-1", "a valid token is used without reading the file");
        assert_eq!(src.force_refresh("at-1").await.unwrap(), "at-2", "after a 401 any other valid token is newer");
        assert!(src.force_refresh("at-2").await.is_err(), "the same token rejected again ends the session");
        assert!(src.end_reason().unwrap().contains("Google no longer accepts the Antigravity session"));
    }

    #[tokio::test]
    async fn a_session_for_another_account_is_not_adopted() {
        let dir = tempfile::tempdir().unwrap();
        let path = session_file(dir.path(), "at-other", 60, ANTIGRAVITY_CLIENT_ID, "other@example.com");
        let src = ImportedTokenSource::new(
            creds(path, "at-mine", Some(Utc::now() - chrono::Duration::minutes(1)), ANTIGRAVITY_CLIENT_ID, Some("me@example.com")),
            None,
            reqwest::Client::new(),
        );
        assert!(src.access_token().await.is_err());
        let reason = src.end_reason().unwrap();
        assert!(reason.contains("now holds a session for other@example.com"), "{reason}");
    }

    async fn refresh_server(state: Arc<AtomicUsize>) -> String {
        async fn token(State(n): State<Arc<AtomicUsize>>, body: String) -> axum::Json<serde_json::Value> {
            let form: std::collections::HashMap<String, String> =
                url::form_urlencoded::parse(body.as_bytes()).into_owned().collect();
            assert_eq!(form["grant_type"], "refresh_token");
            assert_eq!(form["refresh_token"], "rt-1");
            n.fetch_add(1, Ordering::SeqCst);
            axum::Json(serde_json::json!({"access_token": "at-refreshed", "expires_in": 3599}))
        }
        let app = Router::new().route("/token", post(token)).with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    fn refresh_config(base: &str) -> AuthConfig {
        AuthConfig {
            client_id: DEFAULT_CLIENT_ID.into(),
            client_secret: "test-secret".into(),
            endpoints: super::super::OAuthEndpoints::with_base(base),
            callback_port: None,
        }
    }

    #[tokio::test]
    async fn a_live_token_is_returned_without_a_refresh() {
        let calls = Arc::new(AtomicUsize::new(0));
        let base = refresh_server(calls.clone()).await;
        let dir = tempfile::tempdir().unwrap();
        let creds = creds(dir.path().join("x.json"), "at-live", Some(Utc::now() + chrono::Duration::hours(1)), DEFAULT_CLIENT_ID, None);
        let src = ImportedTokenSource::new(creds, Some(refresh_config(&base)), reqwest::Client::new());
        assert_eq!(src.access_token().await.unwrap(), "at-live");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn an_expired_refreshable_token_is_refreshed_once_in_memory() {
        let calls = Arc::new(AtomicUsize::new(0));
        let base = refresh_server(calls.clone()).await;
        let dir = tempfile::tempdir().unwrap();
        let path = session_file(dir.path(), "at-stale", -1, DEFAULT_CLIENT_ID, "me@example.com");
        let before = std::fs::read(&path).unwrap();
        let creds = creds(path.clone(), "at-stale", Some(Utc::now() - chrono::Duration::minutes(1)), DEFAULT_CLIENT_ID, None);
        let src = ImportedTokenSource::new(creds, Some(refresh_config(&base)), reqwest::Client::new());
        assert!(src.can_refresh());
        let (a, b) = tokio::join!(src.access_token(), src.access_token());
        assert_eq!(a.unwrap(), "at-refreshed");
        assert_eq!(b.unwrap(), "at-refreshed");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "concurrent callers share one refresh");
        assert_eq!(std::fs::read(&path).unwrap(), before, "the renewed token is kept in memory only");
        assert_eq!(src.next_check_in(Utc::now()).await, None, "a renewable session needs no re-reads");
    }

    #[tokio::test]
    async fn an_expired_token_without_a_refresh_path_asks_for_a_new_login() {
        let dir = tempfile::tempdir().unwrap();
        let creds = ImportedCreds {
            access_token: "at-stale".into(),
            refresh_token: Some("rt-foreign".into()),
            expiry: Some(chrono::Utc::now() - chrono::Duration::minutes(1)),
            scopes: vec![],
            audience: Some("foreign-client".into()),
            account: None,
            source: dir.path().join("missing.json"),
        };
        let src = ImportedTokenSource::new(creds, None, reqwest::Client::new());
        assert!(!src.can_refresh());
        assert!(matches!(src.access_token().await, Err(AuthError::ReauthRequired)));
        assert!(!format!("{src:?}").contains("at-stale"));
    }

    /// A Code Assist request carries the imported token, and a 401 takes the newer token the
    /// other app wrote to its file.
    #[tokio::test]
    async fn code_assist_requests_carry_the_imported_token() {
        use crate::api::{ApiConfig, CodeAssistClient};
        let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
        async fn load(
            State(seen): State<Arc<std::sync::Mutex<Vec<String>>>>,
            headers: axum::http::HeaderMap,
        ) -> (axum::http::StatusCode, axum::Json<serde_json::Value>) {
            let auth = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or_default().to_owned();
            let ok = auth == "Bearer at-new";
            seen.lock().unwrap().push(auth);
            if ok {
                (axum::http::StatusCode::OK, axum::Json(serde_json::json!({"currentTier": {"id": "free-tier"}, "cloudaicompanionProject": "p"})))
            } else {
                (axum::http::StatusCode::UNAUTHORIZED, axum::Json(serde_json::json!({})))
            }
        }
        let app = Router::new().route("/v1internal:loadCodeAssist", post(load)).with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let dir = tempfile::tempdir().unwrap();
        let path = session_file(dir.path(), "at-old", 40, ANTIGRAVITY_CLIENT_ID, "me@example.com");
        let src = ImportedTokenSource::new(parse_file(&path).unwrap(), None, reqwest::Client::new());
        // The other app renews its session after AntiCraft picked it up.
        session_file(dir.path(), "at-new", 60, ANTIGRAVITY_CLIENT_ID, "me@example.com");
        let tokens: Arc<dyn TokenProvider> = src.clone();
        let client = CodeAssistClient::new(reqwest::Client::new(), tokens, ApiConfig { endpoint: format!("http://{addr}"), ..ApiConfig::default() });
        let req = crate::api::LoadCodeAssistRequest { cloudaicompanion_project: None, metadata: crate::api::ClientMetadata::setup(None), mode: None };
        client.load_code_assist(&req).await.expect("the retry with the newer token succeeds");
        assert_eq!(*seen.lock().unwrap(), vec!["Bearer at-old".to_owned(), "Bearer at-new".to_owned()]);
        assert!(!src.has_ended());
    }
}
