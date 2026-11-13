//! Subscription sign-ins used as a chat backend: Antigravity (a Google account signed in with
//! Antigravity's own OAuth client), ChatGPT/Codex and Claude Code.
//!
//! This is a different thing from [`crate::auth::session_import`], which only *reads* another
//! app's Google session and can never renew it. Everything here refreshes itself, so a signed-in
//! account keeps working across restarts.
//!
//! The three flows differ in a dozen small ways (endpoints, PKCE, the shape of the token POST,
//! how long before expiry to renew). Those live in one table, [`ProviderSpec`], instead of in
//! three copies of the same code; everything that does not differ lives in the shared modules.
//!
//! Nothing in this tree prints a token: the store's `Debug` redacts, `ProviderSpec`'s `Debug`
//! hides the Antigravity client secret, and no error string carries a credential.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::providers::ProviderKind;

pub mod store;

pub mod jwt;

// Filled by the `auth-flows` phase (`contract.md` §2). Declared here, in one place, so a module
// nobody declares cannot be written by mistake: an undeclared file is not part of the crate and
// its tests never run.
pub mod antigravity;
pub mod auth;
pub mod catalog;
pub mod claude;
pub mod codex;
pub mod login;
pub mod registry;

pub use self::store::valid_id;

/// Which subscription an account signs in with.
///
/// Per-variant renames, not `rename_all`: `ClaudeCode` would become `claude-code`, but `as_str`,
/// the settings value and the account file prefix are all `claude`. Mirrors the reasoning for
/// [`crate::providers::ProviderKind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AccountProvider {
    #[serde(rename = "antigravity")]
    Antigravity,
    #[serde(rename = "codex")]
    Codex,
    #[serde(rename = "claude")]
    ClaudeCode,
}

impl AccountProvider {
    /// `"antigravity"` | `"codex"` | `"claude"`. Also the account id prefix and file prefix.
    pub fn as_str(self) -> &'static str {
        match self {
            AccountProvider::Antigravity => "antigravity",
            AccountProvider::Codex => "codex",
            AccountProvider::ClaudeCode => "claude",
        }
    }

    /// What the user sees.
    pub fn label(self) -> &'static str {
        match self {
            AccountProvider::Antigravity => "Antigravity",
            AccountProvider::Codex => "ChatGPT (Codex)",
            AccountProvider::ClaudeCode => "Claude Code",
        }
    }

    /// The one table of everything that differs between the flows.
    pub fn spec(self) -> &'static ProviderSpec {
        match self {
            AccountProvider::Antigravity => &ANTIGRAVITY_SPEC,
            AccountProvider::Codex => &CODEX_SPEC,
            AccountProvider::ClaudeCode => &CLAUDE_SPEC,
        }
    }

    /// Whether this build can run the provider's sign-in. Only Antigravity's client needs a
    /// secret, and that secret comes from the build (see `ANTIGRAVITY_CLIENT_SECRET`); Codex and
    /// Claude Code use public client IDs alone.
    pub fn sign_in_available(self) -> bool {
        self.spec().has_client(matches!(self, AccountProvider::Antigravity))
    }

    /// Inverse of [`Self::as_str`], accepting the common hand-typed spellings as well: settings
    /// files are edited by people, and `claude_code` or `chatgpt` should not silently drop an
    /// account entry.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "antigravity" | "antigravity-" => Some(AccountProvider::Antigravity),
            "codex" | "chatgpt" | "openai" => Some(AccountProvider::Codex),
            "claude" | "claude_code" | "claudecode" | "claude-code" => Some(AccountProvider::ClaudeCode),
            _ => None,
        }
    }

    /// The wire family this account speaks. Claude Code has no dialect of its own: its turns are
    /// Anthropic Messages requests with a bearer instead of a key.
    pub fn wire_kind(self) -> ProviderKind {
        match self {
            AccountProvider::Antigravity => ProviderKind::Antigravity,
            AccountProvider::Codex => ProviderKind::Codex,
            AccountProvider::ClaudeCode => ProviderKind::Anthropic,
        }
    }
}

/// How a token request body is spelled. Gemini/Google and OpenAI take form encoding; Anthropic's
/// OAuth endpoint takes JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenEncoding {
    Form,
    Json,
}

/// Where an account's supported-model list comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelList {
    /// `POST {base}/v1internal:fetchAvailableModels` (Antigravity only).
    Fetch,
    /// The static catalog *is* the supported set; no request is made.
    Static,
}

/// Everything that differs between the three flows, in one table. One `const` per provider.
///
/// `Debug` is hand-written, never derived: `client_secret` is Antigravity's real installed-app
/// secret, and a derived one plus a single `tracing::debug!(?spec)` would put it in the log — the
/// same reason `AuthConfig` redacts.
pub struct ProviderSpec {
    pub client_id: &'static str,
    pub client_secret: Option<&'static str>,
    pub scopes: &'static [&'static str],
    pub auth_url: &'static str,
    pub token_url: &'static str,
    /// Where a refresh grant goes (the same as `token_url` for all three today).
    pub refresh_url: &'static str,
    /// Advertised host is always `localhost`; the listener binds both loopback stacks.
    pub redirect_host: &'static str,
    pub redirect_port: u16,
    pub redirect_path: &'static str,
    pub pkce: bool,
    pub extra_auth_params: &'static [(&'static str, &'static str)],
    /// Requested refresh lead. The effective lead is `min(this, lifetime/2)`.
    pub refresh_skew: Duration,
    pub token_encoding: TokenEncoding,
    /// Non-empty by construction. The login epilogue guarantees a non-empty chosen model.
    pub default_models: &'static [&'static str],
    pub model_list: ModelList,
    /// Where its models are served, for the dialect's default base URL.
    pub base_url: &'static str,
    /// `GET` with the bearer to read the account email back (Antigravity only; the others name
    /// the account from the token response). Empty when unused, and then never requested.
    pub userinfo_url: &'static str,
    /// `GET` with the bearer for the account/organization block (Claude only). Empty when unused.
    pub profile_url: &'static str,
}

impl std::fmt::Debug for ProviderSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderSpec")
            .field("client_id", &self.client_id)
            .field("has_client_secret", &self.client_secret.is_some())
            .field("scopes", &self.scopes)
            .field("auth_url", &self.auth_url)
            .field("token_url", &self.token_url)
            .field("redirect", &format_args!("{}:{}{}", self.redirect_host, self.redirect_port, self.redirect_path))
            .field("pkce", &self.pkce)
            .field("token_encoding", &self.token_encoding)
            .field("default_models", &self.default_models)
            .field("model_list", &self.model_list)
            .field("base_url", &self.base_url)
            .field("userinfo_url", &self.userinfo_url)
            .field("profile_url", &self.profile_url)
            .finish()
    }
}

impl ProviderSpec {
    /// `http://{redirect_host}:{redirect_port}{redirect_path}` — the string registered with the
    /// provider. Must match byte-for-byte what the provider expects; never derived from the
    /// bound socket address.
    pub fn redirect_uri(&self) -> String {
        format!("http://{}:{}{}", self.redirect_host, self.redirect_port, self.redirect_path)
    }

    /// Whether the spec names its OAuth client: an ID, plus a secret when the flow needs one.
    pub fn has_client(&self, needs_secret: bool) -> bool {
        !self.client_id.is_empty() && (!needs_secret || self.client_secret.is_some_and(|s| !s.is_empty()))
    }
}

/// Sign-in state of one account. `Debug + Clone` are required because it travels in app events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountStatus {
    SignedOut,
    SigningIn,
    SignedIn,
    /// Why the last attempt failed, as shown on the row.
    Error(String),
    /// `invalid_grant` was seen: the stored refresh token is dead. The account file is gone; the
    /// settings entry survives so the row can say so and offer Sign in. **Session-only**: a
    /// restart drops the entry (there is no file), so this status is never loaded from disk and
    /// is synthesised by the backend.
    Expired(String),
}

/// Everything the UI shows about one account. **Never holds a token.**
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountView {
    pub id: String,
    pub provider: AccountProvider,
    pub email: Option<String>,
    /// Codex `chatgpt_plan_type`; Claude organization name.
    pub plan: Option<String>,
    /// Chosen for turns. Non-empty whenever `status` is `SignedIn`.
    pub model: String,
    /// Cached supported list, newest fetch. Empty until the first fetch succeeds.
    pub models: Vec<String>,
    pub models_fetched_at: Option<DateTime<Utc>>,
    pub status: AccountStatus,
}

/// Cap on rows in `Settings::accounts`.
pub const MAX_ACCOUNTS: usize = 8;

/// `format!("{}-{}", provider.as_str(), &hex(sha256(identity))[..8])` over the trimmed identity
/// (a Google or Claude email, a Codex `chatgpt_account_id`). Re-signing in to the same identity
/// updates the same file in place instead of adding a second row for one person.
///
/// `sha2` is already a dependency and there is no `hex` crate, so the digest is formatted by
/// hand; eight hex digits is 32 bits, which is plenty to tell a handful of accounts apart and
/// keeps the id short enough to read in a log.
pub fn account_id(provider: AccountProvider, identity: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(identity.trim().as_bytes());
    let digest = h.finalize();
    let mut hex = String::with_capacity(8);
    for b in digest.iter().take(4) {
        hex.push_str(&format!("{b:02x}"));
    }
    format!("{}-{hex}", provider.as_str())
}

/// Network-sourced display string made safe to render and to keep: strips C0/C1 controls and the
/// bidi/overwrite set (U+202A–202E, U+2066–2069, U+200B–200F, U+FEFF), trims, truncates to 60
/// chars on a char boundary. `None` when nothing printable is left.
///
/// An email or a plan name arrives from a provider, and every UI toolkit will happily draw a
/// right-to-left override in the middle of a row. This is the single door that text comes through.
pub fn display_text(raw: &str) -> Option<String> {
    const MAX_CHARS: usize = 60;
    let cleaned: String = raw
        .chars()
        .filter(|c| {
            let u = *c as u32;
            !(u < 0x20
                || (0x7f..=0x9f).contains(&u)
                || (0x200b..=0x200f).contains(&u)
                || (0x202a..=0x202e).contains(&u)
                || (0x2066..=0x2069).contains(&u)
                || u == 0xfeff)
        })
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return None;
    }
    // `chars().take(..)` cuts on a char boundary and never splits a code point.
    Some(trimmed.chars().take(MAX_CHARS).collect::<String>().trim_end().to_owned())
}

/// Model ids round-trip into request bodies: `[A-Za-z0-9._:/@+-]{1,80}` after trimming.
pub fn valid_model_id(raw: &str) -> bool {
    let m = raw.trim();
    !m.is_empty()
        && m.len() <= 80
        && m.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '/' | '@' | '+' | '-'))
}

/// The model a new account starts on, given the ids it actually offers.
///
/// The spec's defaults are in preference order — the first one the account serves is the answer.
/// Taking the fetched list's first entry instead is how an account lands on whatever sorts first
/// in its provider's response (`aqa`, `chat_…`, a preview), which is rarely what anyone wants and
/// is sometimes not even a general-purpose model. Comparison is case-insensitive because providers
/// spell ids back with their own casing; the returned string is the spec's spelling so the picker
/// and the request body agree.
///
/// With no default on offer, the first usable entry of the fetched list wins, and with no list at
/// all the first default is used — `None` only when the spec has no defaults, which its own test
/// forbids.
pub fn default_model(spec: &ProviderSpec, fetched: &[String]) -> Option<String> {
    if let Some(d) = spec
        .default_models
        .iter()
        .find(|d| fetched.iter().any(|m| m.trim().eq_ignore_ascii_case(d)))
    {
        return Some((*d).to_owned());
    }
    fetched
        .iter()
        .find(|m| valid_model_id(m))
        .map(|m| m.trim().to_owned())
        .or_else(|| spec.default_models.first().map(|m| (*m).to_owned()))
}

// ---------------------------------------------------------------------------
// The three specs
// ---------------------------------------------------------------------------
//
// Values re-checked against CLIProxyAPI's `internal/auth/{antigravity,codex,claude}/*.go`. The
// Antigravity secret is an installed-app secret that ships inside the desktop app it belongs to;
// it is not a secret of ours, and it is still never printed.

/// The secret of Antigravity's own installed-app OAuth client (its public ID is
/// [`crate::auth::antigravity::ANTIGRAVITY_CLIENT_ID`]). It is not in this repository: it is
/// Google's, not ours to publish, and GitHub push protection refuses it. `build.rs` passes it in
/// from `ANTICRAFT_ANTIGRAVITY_CLIENT_SECRET` or the git-ignored `antigravity-client.local.env`.
/// A build without it refuses the Antigravity account sign-in with a message
/// ([`AccountProvider::sign_in_available`]); unit tests get a placeholder, so they run on any
/// checkout.
const ANTIGRAVITY_CLIENT_SECRET: Option<&str> = match option_env!("ANTICRAFT_ANTIGRAVITY_CLIENT_SECRET") {
    Some(secret) => Some(secret),
    None if cfg!(test) => Some("test-client-secret"),
    None => None,
};

/// Google, signed in as Antigravity itself. PKCE on top of the client secret: the secret ships
/// inside a desktop app and protects nothing, while the verifier makes a code caught by anyone
/// else on the loopback useless. Google accepts it from any client, as the Gemini CLI flow in
/// `crate::auth::oauth` already relies on.
pub const ANTIGRAVITY_SPEC: ProviderSpec = ProviderSpec {
    client_id: crate::auth::antigravity::ANTIGRAVITY_CLIENT_ID,
    client_secret: ANTIGRAVITY_CLIENT_SECRET,
    scopes: &[
        "https://www.googleapis.com/auth/cloud-platform",
        "https://www.googleapis.com/auth/userinfo.email",
        "https://www.googleapis.com/auth/userinfo.profile",
        "https://www.googleapis.com/auth/aicode",
        "https://www.googleapis.com/auth/cclog",
        "https://www.googleapis.com/auth/experimentsandconfigs",
    ],
    auth_url: "https://accounts.google.com/o/oauth2/v2/auth",
    token_url: "https://oauth2.googleapis.com/token",
    refresh_url: "https://oauth2.googleapis.com/token",
    redirect_host: "localhost",
    redirect_port: 51121,
    redirect_path: "/oauth-callback",
    pkce: true,
    extra_auth_params: &[("access_type", "offline"), ("prompt", "consent")],
    refresh_skew: Duration::from_secs(5 * 60),
    token_encoding: TokenEncoding::Form,
    // All three are on Antigravity's own list; `gemini-3.5-flash` is not (only its `-lite` is).
    default_models: &["gemini-3-flash", "claude-sonnet-4-6", "gemini-3.5-flash-lite"],
    model_list: ModelList::Fetch,
    base_url: "https://daily-cloudcode-pa.googleapis.com",
    userinfo_url: "https://www.googleapis.com/oauth2/v2/userinfo",
    profile_url: "",
};

/// ChatGPT's consumer subscription. The 96 h lead is a request, not a rule: the effective lead is
/// clamped to half the token's lifetime so a one-hour token is not renewed on every turn.
pub const CODEX_SPEC: ProviderSpec = ProviderSpec {
    client_id: "app_EMoamEEZ73f0CkXaXp7hrann",
    client_secret: None,
    scopes: &["openid", "email", "profile", "offline_access"],
    auth_url: "https://auth.openai.com/oauth/authorize",
    token_url: "https://auth.openai.com/oauth/token",
    refresh_url: "https://auth.openai.com/oauth/token",
    redirect_host: "localhost",
    redirect_port: 1455,
    redirect_path: "/auth/callback",
    pkce: true,
    extra_auth_params: &[
        ("prompt", "login"),
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
    ],
    refresh_skew: Duration::from_secs(96 * 3600),
    token_encoding: TokenEncoding::Form,
    // The everyday coding model first; the free plan lacks it and lands on the second.
    default_models: &["gpt-6.1-sol", "gpt-6.1-terra", "gpt-6-astra"],
    model_list: ModelList::Static,
    base_url: "https://chatgpt.com/backend-api/codex",
    userinfo_url: "",
    profile_url: "",
};

/// Claude Code's subscription sign-in.
pub const CLAUDE_SPEC: ProviderSpec = ProviderSpec {
    client_id: "9d1c250a-e61b-44d9-88ed-5944d1962f5e",
    client_secret: None,
    scopes: &[
        "user:profile",
        "user:inference",
        "user:sessions:claude_code",
        "user:mcp_servers",
        "user:file_upload",
    ],
    auth_url: "https://claude.ai/oauth/authorize",
    token_url: "https://platform.claude.com/v1/oauth/token",
    refresh_url: "https://platform.claude.com/v1/oauth/token",
    redirect_host: "localhost",
    redirect_port: 54545,
    redirect_path: "/callback",
    pkce: true,
    extra_auth_params: &[("code", "true")],
    refresh_skew: Duration::from_secs(5 * 60),
    token_encoding: TokenEncoding::Json,
    default_models: &["claude-opus-5-5", "claude-sonnet-5-5", "claude-haiku-5-5"],
    model_list: ModelList::Static,
    base_url: "https://api.anthropic.com/v1",
    userinfo_url: "",
    profile_url: "https://api.anthropic.com/api/oauth/profile",
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_provider_serde_matches_as_str() {
        // The JSON spelling, the settings value, the id prefix and the file prefix are one string.
        for p in [AccountProvider::Antigravity, AccountProvider::Codex, AccountProvider::ClaudeCode] {
            let json = serde_json::to_string(&p).unwrap();
            assert_eq!(json, format!("\"{}\"", p.as_str()));
            assert_eq!(serde_json::from_str::<AccountProvider>(&json).unwrap(), p);
            assert_eq!(AccountProvider::parse(p.as_str()), Some(p));
        }
        // `rename_all = "kebab-case"` would have produced this instead.
        assert_eq!(serde_json::to_string(&AccountProvider::ClaudeCode).unwrap(), "\"claude\"");
        assert_eq!(AccountProvider::parse(" ChatGPT "), Some(AccountProvider::Codex));
        assert_eq!(AccountProvider::parse("claude_code"), Some(AccountProvider::ClaudeCode));
        assert_eq!(AccountProvider::parse("gemini"), None);
    }

    #[test]
    fn each_account_speaks_its_own_wire() {
        assert_eq!(AccountProvider::Antigravity.wire_kind(), ProviderKind::Antigravity);
        assert_eq!(AccountProvider::Codex.wire_kind(), ProviderKind::Codex);
        // Claude Code is the Anthropic dialect on the bearer path, not an account-only kind: the
        // same wire a key profile uses, which is why the binding digest has to tell them apart.
        assert_eq!(AccountProvider::ClaudeCode.wire_kind(), ProviderKind::Anthropic);
        for p in [AccountProvider::Antigravity, AccountProvider::Codex] {
            assert!(p.wire_kind().is_account_only(), "{p:?}");
        }
        // The dialect's default host and the sign-in spec's are one string.
        for p in [AccountProvider::Antigravity, AccountProvider::Codex, AccountProvider::ClaudeCode] {
            assert_eq!(p.wire_kind().default_base_url(), p.spec().base_url, "{p:?}");
        }
    }

    #[test]
    fn account_ids_are_prefixed_random_looking_and_stable() {
        let a = account_id(AccountProvider::ClaudeCode, "me@example.com");
        assert_eq!(a, account_id(AccountProvider::ClaudeCode, "  me@example.com "), "trimmed identity, same id");
        assert!(a.starts_with("claude-"), "{a}");
        assert_eq!(a.len(), "claude-".len() + 8, "{a}");
        assert!(valid_id(&a), "{a} must satisfy valid_id: it becomes a file name");
        assert_ne!(a, account_id(AccountProvider::Codex, "me@example.com"), "the prefix separates providers");
    }

    #[test]
    fn display_text_strips_controls_bidi_and_truncates() {
        assert_eq!(display_text("  me@example.com  ").as_deref(), Some("me@example.com"));
        assert_eq!(display_text("a\u{200b}b\u{202e}c\u{feff}").as_deref(), Some("abc"));
        assert_eq!(display_text("\u{0}\u{7}\u{202e} \t").as_deref(), None);
        assert_eq!(display_text(""), None);
        let long = "x".repeat(100);
        assert_eq!(display_text(&long).unwrap().chars().count(), 60);
    }

    #[test]
    fn model_ids_are_restricted() {
        for ok in ["gemini-3-flash", "gpt-5.1-codex", "claude-sonnet-4-6", "a/b:c@d+e_f"] {
            assert!(valid_model_id(ok), "{ok}");
        }
        for bad in ["", "  ", "has space", "semi;colon", "quote\"", &"x".repeat(81)] {
            assert!(!valid_model_id(bad), "{bad:?}");
        }
        assert!(valid_model_id("  gemini-3-flash  "), "trimmed first");
    }

    #[test]
    fn the_default_model_follows_the_spec_not_the_list_order() {
        let spec = &ANTIGRAVITY_SPEC;
        let first = spec.default_models[0];
        let second = spec.default_models[1];
        // A list that sorts something else first still lands on the spec's preference…
        let fetched = vec!["aaa-preview".to_owned(), second.to_owned(), first.to_ascii_uppercase()];
        assert_eq!(default_model(spec, &fetched).as_deref(), Some(first), "the spec's spelling, its first choice");
        // …the next preference when the first is not served…
        let fetched = vec!["aaa-preview".to_owned(), second.to_owned()];
        assert_eq!(default_model(spec, &fetched).as_deref(), Some(second));
        // …the list's first usable id when it serves none of them…
        let fetched = vec!["bad id".to_owned(), "  other-model ".to_owned()];
        assert_eq!(default_model(spec, &fetched).as_deref(), Some("other-model"));
        // …and the first default before any fetch.
        assert_eq!(default_model(spec, &[]).as_deref(), Some(first));
    }

    /// The Antigravity client secret comes from the build, and a build without it must refuse the
    /// sign-in rather than send Google an incomplete client.
    #[test]
    fn the_antigravity_sign_in_needs_the_built_in_client() {
        let bare = ProviderSpec { client_id: "", client_secret: None, ..ANTIGRAVITY_SPEC };
        assert!(!bare.has_client(true) && !bare.has_client(false));
        let id_only = ProviderSpec { client_id: "x.apps.googleusercontent.com", client_secret: None, ..ANTIGRAVITY_SPEC };
        assert!(!id_only.has_client(true), "Google wants the secret too");
        assert!(id_only.has_client(false));
        let empty_secret = ProviderSpec { client_secret: Some(""), ..id_only };
        assert!(!empty_secret.has_client(true));
        let both = ProviderSpec { client_id: "x.apps.googleusercontent.com", client_secret: Some("s"), ..ANTIGRAVITY_SPEC };
        assert!(both.has_client(true));
        // Codex and Claude Code carry public IDs in the source and need no secret.
        assert!(AccountProvider::Codex.sign_in_available() && AccountProvider::ClaudeCode.sign_in_available());
        // Test builds always have a client (placeholders when build.rs found none).
        assert!(AccountProvider::Antigravity.sign_in_available());
        let attempt = super::login::LoginAttempt::new(1, AccountProvider::Antigravity, super::login::ProviderEndpoints::for_spec(&ANTIGRAVITY_SPEC));
        assert!(attempt.is_ok(), "{:?}", attempt.err());
    }

    #[test]
    fn spec_redirect_uri_is_the_registered_string() {
        assert_eq!(ANTIGRAVITY_SPEC.redirect_uri(), "http://localhost:51121/oauth-callback");
        assert_eq!(CODEX_SPEC.redirect_uri(), "http://localhost:1455/auth/callback");
        assert_eq!(CLAUDE_SPEC.redirect_uri(), "http://localhost:54545/callback");
    }

    #[test]
    fn spec_debug_hides_the_client_secret() {
        let spec = ProviderSpec { client_secret: Some("fake-installed-app-secret"), ..ANTIGRAVITY_SPEC };
        let text = format!("{spec:?}");
        assert!(!text.contains("fake-installed-app-secret"), "{text}");
        assert!(text.contains("has_client_secret: true"), "{text}");
        // Every spec names a non-empty default model list: the login epilogue relies on index 0.
        for p in [AccountProvider::Antigravity, AccountProvider::Codex, AccountProvider::ClaudeCode] {
            assert!(!p.spec().default_models.is_empty(), "{p:?}");
            assert!(p.spec().default_models.iter().all(|m| valid_model_id(m)), "{p:?}");
        }
    }
}
