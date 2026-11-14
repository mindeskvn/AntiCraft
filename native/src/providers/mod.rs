//! External model providers as a drop-in alternative to Google Code Assist.
//!
//! The agent loop talks to a `ModelBackend` and reads `CaGenerateResponse` chunks; it knows
//! nothing about HTTP or wire formats. Each provider here is therefore two translators wrapped
//! around one shared driver: our `GenerateContentRequest` becomes the provider's JSON, and each
//! streamed provider event becomes the chunk shape the loop already understands. Tools, history
//! layout and cancellation semantics stay exactly as they are for Code Assist.
//!
//! The driver lives in [`ProviderBackend::stream_turn`], so retry, idle and size limits are
//! enforced once. Dialects only fill in [`common::Dialect`].

pub mod anthropic;
pub mod antigravity;
pub mod codex;
pub mod common;
pub mod gemini;
pub mod models;
pub mod openai;

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use futures::stream::BoxStream;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::api::sse::SseParser;
use crate::api::{CaError, CaGenerateResponse, GenerateContentRequest, ModelBackend};
use crate::auth::TokenProvider;

use common::{Dialect, Decoder, WireAuth, IDLE, MAX_BODY_BYTES, MAX_EVENT_BYTES};

/// Bytes of a non-success response body kept for the error message. Large enough to carry a
/// provider's explanation, small enough that a proxy's HTML error page cannot bloat a log line.
const MAX_ERROR_BODY_BYTES: usize = 8 * 1024;

/// Characters of that body kept in [`CaError::Http`].
const MAX_ERROR_CHARS: usize = 512;

// ---------------------------------------------------------------------------
// Provider kind
// ---------------------------------------------------------------------------

/// Which wire format a profile speaks.
///
/// The settings names are pinned per variant instead of derived with `rename_all = "kebab-case"`:
/// serde would render `OpenAi` as `open-ai`, but the key users and settings files already carry is
/// `openai`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ProviderKind {
    /// Gemini Developer API (`generativelanguage.googleapis.com`), the closest match to our own
    /// types, so its dialect is mostly a field rename.
    #[serde(rename = "gemini-api")]
    GeminiApi,
    /// Any OpenAI-compatible chat-completions endpoint, including local servers that keep the
    /// request shape but serve from `localhost`.
    #[serde(rename = "openai")]
    OpenAi,
    /// Anthropic Messages API.
    #[serde(rename = "anthropic")]
    Anthropic,
    /// Google's Antigravity backend (`daily-cloudcode-pa`), spoken with the OAuth token a
    /// subscription sign-in produces. Account-only: it has no API key, so a hand-written profile
    /// of this kind is dropped by `Settings::sanitized`.
    #[serde(rename = "antigravity")]
    Antigravity,
    /// The ChatGPT subscription's Codex backend (the OpenAI Responses API). Account-only for the
    /// same reason as [`Self::Antigravity`].
    #[serde(rename = "codex")]
    Codex,
}

impl ProviderKind {
    /// Stable spelling used by settings, by the UI's picker, and by
    /// [`crate::api::ProviderBlock::provider`] to tag reasoning a dialect issued.
    pub fn as_str(&self) -> &'static str {
        match self {
            ProviderKind::GeminiApi => "gemini-api",
            ProviderKind::OpenAi => "openai",
            ProviderKind::Anthropic => "anthropic",
            ProviderKind::Antigravity => "antigravity",
            ProviderKind::Codex => "codex",
        }
    }

    /// Display name, also the default label of a new profile. The two account kinds are named
    /// because the match is exhaustive; no profile of theirs can be created.
    pub const fn label(&self) -> &'static str {
        match self {
            ProviderKind::GeminiApi => "Gemini Developer API",
            ProviderKind::OpenAi => "OpenAI-compatible",
            ProviderKind::Anthropic => "Anthropic",
            ProviderKind::Antigravity => "Antigravity",
            ProviderKind::Codex => "ChatGPT (Codex)",
        }
    }

    /// Inverse of [`Self::as_str`]. Accepts the common spellings users type so a hand-edited
    /// settings file is not rejected over a hyphen. `chatgpt` is accepted for Codex because that
    /// is what the subscription is called in the provider's own UI.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "gemini-api" | "gemini_api" | "gemini" => Some(ProviderKind::GeminiApi),
            "openai" | "openai-compatible" | "open-ai" => Some(ProviderKind::OpenAi),
            "anthropic" | "claude" => Some(ProviderKind::Anthropic),
            "antigravity" => Some(ProviderKind::Antigravity),
            "codex" | "chatgpt" => Some(ProviderKind::Codex),
            _ => None,
        }
    }

    /// Input window assumed when the profile does not set `context_window`. Gemini models take
    /// a million tokens; OpenAI-compatible servers are often far smaller (gpt-4o and many local
    /// models: 128K), and every current Claude model takes at least 200K.
    ///
    /// The two account kinds have no configurable window: the value here is what compaction keeps
    /// the conversation under, and it is taken from the provider's own published limit.
    pub fn default_context_window(&self) -> u64 {
        match self {
            ProviderKind::GeminiApi => crate::agent::context::DEFAULT_TOKEN_LIMIT,
            ProviderKind::OpenAi => 128_000,
            ProviderKind::Anthropic => 200_000,
            ProviderKind::Antigravity => 1_000_000,
            ProviderKind::Codex => 400_000,
        }
    }

    /// Endpoint used when the profile does not override `base_url`.
    pub fn default_base_url(&self) -> &'static str {
        match self {
            ProviderKind::GeminiApi => "https://generativelanguage.googleapis.com/v1beta",
            ProviderKind::OpenAi => "https://api.openai.com/v1",
            ProviderKind::Anthropic => "https://api.anthropic.com/v1",
            // Read from the sign-in spec so the flow and the dialect cannot drift apart.
            ProviderKind::Antigravity => crate::auth::external::ANTIGRAVITY_SPEC.base_url,
            ProviderKind::Codex => crate::auth::external::CODEX_SPEC.base_url,
        }
    }

    /// True for kinds a user cannot add by hand: they are reached by signing in on the Account tab,
    /// and their credential is an OAuth token, not a key a profile could carry.
    ///
    /// `Settings::sanitized` drops such profiles before `validate()`, so a hand-edited settings
    /// file cannot pretend that an API key is a subscription.
    pub fn is_account_only(&self) -> bool {
        matches!(self, ProviderKind::Antigravity | ProviderKind::Codex)
    }
}

impl fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Profile
// ---------------------------------------------------------------------------

/// One configured provider, as stored in settings.
///
/// Written in camelCase; the snake_case spelling the rest of `settings.json` uses is read too,
/// so a hand-written `base_url` is not dropped silently (the key would go to the kind's
/// default host instead).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderProfile {
    /// Stable key used by settings; never shown.
    pub id: String,
    /// What the user sees.
    pub label: String,
    pub kind: ProviderKind,
    /// Model id sent on the wire (`gemini-2.5-pro`, `gpt-4o-mini`, ...), typed or picked from
    /// `models`. Empty until one is chosen: such a profile is kept, but sends no turn.
    pub model: String,
    /// `None` means [`ProviderKind::default_base_url`].
    #[serde(default, skip_serializing_if = "Option::is_none", alias = "base_url")]
    pub base_url: Option<String>,
    /// Secret. Deliberately never formatted, logged or serialized into an error.
    #[serde(alias = "api_key")]
    pub api_key: String,
    /// Provider-side cap on the answer, applied only when the request sets none.
    #[serde(default, skip_serializing_if = "Option::is_none", alias = "max_output_tokens")]
    pub max_output_tokens: Option<u32>,
    /// Input window of the model, in tokens, which automatic compaction keeps the conversation
    /// under. `None` means [`ProviderKind::default_context_window`].
    #[serde(default, skip_serializing_if = "Option::is_none", alias = "context_window")]
    pub context_window: Option<u64>,
    /// What the provider listed the last time its models were loaded (Settings >
    /// Providers > Load models). The composer's model menu offers these.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
}

/// Hand-written so a panic dump, a `dbg!` or a tracing field can never carry the key.
impl fmt::Debug for ProviderProfile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderProfile")
            .field("id", &self.id)
            .field("label", &self.label)
            .field("kind", &self.kind)
            .field("model", &self.model)
            .field("base_url", &self.base_url)
            .field("api_key", &"<redacted>")
            .field("max_output_tokens", &self.max_output_tokens)
            .field("context_window", &self.context_window)
            .field("models", &self.models.len())
            .finish()
    }
}

impl ProviderProfile {
    /// Input window used for automatic compaction (see `context_window`).
    pub fn context_window(&self) -> u64 {
        self.context_window.filter(|w| *w > 0).unwrap_or_else(|| self.kind.default_context_window())
    }

    /// Effective base URL, trimmed of trailing slashes so dialects can join paths with `/`.
    pub fn resolved_base_url(&self) -> String {
        let raw = match self.base_url.as_deref().map(str::trim) {
            Some(url) if !url.is_empty() => url,
            _ => self.kind.default_base_url(),
        };
        raw.trim_end_matches('/').to_owned()
    }

    /// Check what the settings UI is about to save. Messages are user-facing and never quote the
    /// key itself, only say whether it is present.
    ///
    /// A profile reached this way always carries its own key, so the key check belongs here. The
    /// account path has no key at all and calls [`Self::validate_shape`] directly.
    pub fn validate(&self) -> Result<(), String> {
        self.validate_shape()?;
        if self.api_key.trim().is_empty() {
            return Err("API key is empty".into());
        }
        Ok(())
    }

    /// Field checks only: id, label, URL, caps. Used by the OAuth path, where `api_key` is empty by
    /// construction because the credential comes from the account's token.
    ///
    /// The model is not checked: a profile is saved before one is chosen (its models are loaded
    /// from the provider), and [`ProviderBackend::stream_turn`] refuses to send without one.
    pub fn validate_shape(&self) -> Result<(), String> {
        if self.id.trim().is_empty() {
            return Err("provider id is empty".into());
        }
        if self.label.trim().is_empty() {
            return Err("provider label is empty".into());
        }
        if let Some(base) = self.base_url.as_deref().map(str::trim).filter(|b| !b.is_empty()) {
            let url = url::Url::parse(base).map_err(|e| format!("base URL is not a valid URL: {e}"))?;
            if !matches!(url.scheme(), "http" | "https") {
                return Err(format!("base URL must be http or https, not {}", url.scheme()));
            }
            if url.host_str().unwrap_or_default().is_empty() {
                return Err("base URL has no host".into());
            }
        }
        if self.max_output_tokens == Some(0) {
            return Err("max output tokens must be greater than zero".into());
        }
        if self.context_window == Some(0) {
            return Err("context window must be greater than zero".into());
        }
        Ok(())
    }

    /// Copy with `model` replaced.
    ///
    /// The picker's choice is the id this turn actually sends, and it may differ from the stored
    /// default; a dialect built from this copy therefore builds its body from the picked value. A
    /// blank argument keeps the profile's own model rather than sending an empty id.
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        let model = model.into();
        if !model.trim().is_empty() {
            self.model = model;
        }
        self
    }
}

// ---------------------------------------------------------------------------
// Backend
// ---------------------------------------------------------------------------

/// A fact about the signed-in account that a request body or header needs.
///
/// Kept beside the credential rather than read from settings at request time: `dialect()` is
/// synchronous and is built once per turn, and a settings read there could disagree with the token
/// the same turn sends.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AccountMeta {
    /// Codex `chatgpt_account_id`, sent as `Chatgpt-Account-Id`.
    pub account_id: Option<String>,
    /// Antigravity's discovered Cloud project, sent in the request envelope.
    pub project_id: Option<String>,
}

/// How the backend authenticates.
#[derive(Clone)]
pub enum AuthMode {
    /// Today's profile: its `api_key` is the credential.
    ApiKey(String),
    /// A signed-in subscription account.
    OAuth(Arc<dyn TokenProvider>),
}

impl fmt::Debug for AuthMode {
    /// Never derive this: the api-key arm holds a secret and the OAuth arm's provider may hold a
    /// token, so only the mode itself is printed.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AuthMode::ApiKey(_) => f.write_str("ApiKey(<redacted>)"),
            AuthMode::OAuth(_) => f.write_str("OAuth(<account>)"),
        }
    }
}

/// A `ModelBackend` backed by one provider profile.
pub struct ProviderBackend {
    http: reqwest::Client,
    profile: ProviderProfile,
    auth: AuthMode,
    /// Empty for a key-based profile; the account facts a dialect needs otherwise.
    meta: AccountMeta,
}

impl ProviderBackend {
    pub fn new(http: reqwest::Client, profile: ProviderProfile) -> Self {
        let auth = AuthMode::ApiKey(profile.api_key.trim().to_owned());
        Self { http, profile, auth, meta: AccountMeta::default() }
    }

    /// A backend driven by a signed-in subscription account.
    ///
    /// The credential is the account's token source, not `profile.api_key` (which is empty by
    /// construction): the driver resolves a token per attempt and refreshes it once on a 401.
    pub fn with_oauth(
        http: reqwest::Client,
        profile: ProviderProfile,
        tokens: Arc<dyn TokenProvider>,
        meta: AccountMeta,
    ) -> Self {
        Self { http, profile, auth: AuthMode::OAuth(tokens), meta }
    }

    pub fn profile(&self) -> &ProviderProfile {
        &self.profile
    }

    /// Construct the dialect for this request.
    ///
    /// `model` is the id this turn actually sends, which may differ from `profile.model` (the
    /// picker's choice); the dialect builds its body from it. One place, so adding a provider is
    /// one match arm.
    fn dialect(&self, model: &str) -> Box<dyn Dialect> {
        let profile = self.profile.clone().with_model(model);
        match profile.kind {
            ProviderKind::GeminiApi => Box::new(gemini::Wire::new(&profile)),
            ProviderKind::OpenAi => Box::new(openai::Wire::new(&profile)),
            ProviderKind::Anthropic => match self.auth {
                // The bearer path needs a different header set and a required system block, so the
                // dialect is told at construction rather than guessing from an empty key.
                AuthMode::OAuth(_) => Box::new(anthropic::Wire::with_bearer(&profile)),
                AuthMode::ApiKey(_) => Box::new(anthropic::Wire::new(&profile)),
            },
            ProviderKind::Antigravity => {
                Box::new(antigravity::Wire::new(&profile, self.meta.project_id.as_deref()))
            }
            ProviderKind::Codex => Box::new(codex::Wire::new(&profile, self.meta.account_id.as_deref())),
        }
    }
}

impl fmt::Debug for ProviderBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderBackend")
            .field("profile", &self.profile)
            .field("auth", &self.auth)
            .field("meta", &self.meta)
            .finish()
    }
}

/// Build one attempt's request. The token is borrowed only for the duration of this call.
fn post(
    http: &reqwest::Client,
    dialect: &dyn Dialect,
    url: &str,
    body: &serde_json::Value,
    mode: bool,
    token: &str,
) -> reqwest::RequestBuilder {
    let auth = if mode { WireAuth::Bearer(token) } else { WireAuth::Key(token) };
    dialect.apply_headers(http.post(url).json(body), auth)
}

/// POST the built body; on 401 refresh once and resend.
///
/// `None` means the turn was cancelled before either attempt was made — the caller yields nothing,
/// which is the contract [`cancelled_token_ends_the_stream_without_a_request`] encodes. Every
/// token this used is appended to `seen` so the caller can scrub all of them out of any error.
async fn send(
    http: &reqwest::Client,
    auth: &AuthMode,
    dialect: &dyn Dialect,
    url: &str,
    body: &serde_json::Value,
    cancel: &CancellationToken,
    seen: &mut Vec<String>,
) -> Option<Result<reqwest::Response, CaError>> {
    let oauth = matches!(auth, AuthMode::OAuth(_));
    let token = match auth {
        AuthMode::ApiKey(key) => key.clone(),
        // A token source that cannot produce one is an authentication failure, reported the same
        // way a refused request is: as the stream's first item.
        AuthMode::OAuth(tokens) => match tokens.access_token().await {
            Ok(token) => token,
            Err(e) => return Some(Err(CaError::Auth(e.to_string()))),
        },
    };
    seen.push(token.clone());

    let sent = tokio::select! {
        biased;
        _ = cancel.cancelled() => return None,
        sent = post(http, dialect, url, body, oauth, &token).send() => sent,
    };
    let resp = match sent {
        Ok(resp) => resp,
        // `without_url` keeps any URL-borne secret out of the message.
        Err(e) => return Some(Err(CaError::Transport(e.without_url().to_string()))),
    };
    if resp.status() != reqwest::StatusCode::UNAUTHORIZED {
        return Some(Ok(resp));
    }

    let AuthMode::OAuth(tokens) = auth else {
        // A key that was refused will not become valid by being sent again: unchanged behaviour,
        // so the 401 response goes on to be classified exactly as it is today.
        return Some(Ok(resp));
    };
    drop(resp);

    let next = match tokens.force_refresh(&token).await {
        Ok(token) => token,
        Err(e) => return Some(Err(CaError::Auth(e.to_string()))),
    };
    seen.push(next.clone());

    // The second attempt is guarded too: a cancel that lands during the refresh must not send a
    // request the user has already walked away from.
    let sent = tokio::select! {
        biased;
        _ = cancel.cancelled() => return None,
        sent = post(http, dialect, url, body, true, &next).send() => sent,
    };
    Some(sent.map_err(|e| CaError::Transport(e.without_url().to_string())))
}

impl ModelBackend for ProviderBackend {
    /// Stream one turn through the provider named by the profile.
    ///
    /// `model` is the id the caller picked for this turn; it falls back to the profile's stored
    /// model only when empty. `user_prompt_id` is accepted for trait compatibility and ignored:
    /// external providers have no prompt-id envelope. Everything that can fail before the first
    /// byte, including a rejected request, is delivered as the stream's first item so the caller
    /// has one error path.
    ///
    /// Reasoning blocks another provider issued ([`crate::api::Part::provider_block`]) are removed
    /// first: a history that ran on Anthropic must not hand its signed `thinking` blocks to the
    /// Gemini API (whose body is our request serialized as is) or to an OpenAI-compatible server.
    fn stream_turn(
        &self,
        model: String,
        user_prompt_id: String,
        mut request: GenerateContentRequest,
        cancel: CancellationToken,
    ) -> BoxStream<'static, Result<CaGenerateResponse, CaError>> {
        // An account has no key by construction, so it is checked for shape only; a key profile
        // still has to carry one.
        let valid = match self.auth {
            AuthMode::ApiKey(_) => self.profile.validate(),
            AuthMode::OAuth(_) => self.profile.validate_shape(),
        };
        if let Err(message) = valid {
            return futures::stream::once(async move { Err(CaError::Auth(message)) }).boxed();
        }
        if let Err(message) = validate_target(&self.profile) {
            return futures::stream::once(async move { Err(CaError::Decode(message)) }).boxed();
        }
        crate::api::retain_provider_blocks(&mut request.contents, Some(self.profile.kind.as_str()));

        let model = if model.trim().is_empty() {
            self.profile.model.trim().to_owned()
        } else {
            model.trim().to_owned()
        };
        // A profile saved before its model was chosen. `Backend::agent_backend` refuses such a
        // turn first, with the way out; this keeps an empty id off the wire whatever the caller.
        if model.is_empty() {
            let message = format!("no model is chosen for {}", self.profile.label.trim());
            return futures::stream::once(async move { Err(CaError::Decode(message)) }).boxed();
        }
        let dialect = self.dialect(&model);
        let http = self.http.clone();
        let profile = self.profile.clone();
        let auth = self.auth.clone();
        let _ = user_prompt_id;

        tracing::debug!(
            kind = %profile.kind,
            model = %model,
            oauth = matches!(auth, AuthMode::OAuth(_)),
            "provider turn starting"
        );

        async_stream::stream! {
            let url = dialect.endpoint();
            let body = dialect.build_body(&request);

            // Every credential this turn puts on the wire, so an error that quotes one can have it
            // removed. Rebuilt after `send` because `send` appends the tokens it used.
            let mut seen: Vec<String> = Vec::new();
            let key = profile.api_key.trim().to_owned();
            let secrets = |seen: &[String], key: &str| -> Vec<String> {
                std::iter::once(key.to_owned())
                    .chain(seen.iter().cloned())
                    .filter(|s| s.len() >= 4)
                    .collect()
            };

            let sent = send(&http, &auth, dialect.as_ref(), &url, &body, &cancel, &mut seen).await;
            let resp = match sent {
                None => return,
                Some(Ok(resp)) => resp,
                Some(Err(e)) => {
                    let secrets = secrets(&seen, &key);
                    let refs: Vec<&str> = secrets.iter().map(String::as_str).collect();
                    yield Err(scrub_error(e, &refs));
                    return;
                }
            };

            let status = resp.status();
            if !status.is_success() {
                let retry_after = resp
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.trim().parse::<u64>().ok())
                    .map(Duration::from_secs);
                let bytes = read_limited(resp, MAX_ERROR_BODY_BYTES).await;
                let text = String::from_utf8_lossy(&bytes);
                tracing::warn!(
                    kind = %profile.kind,
                    model = %model,
                    status = status.as_u16(),
                    "provider request rejected"
                );
                let secrets = secrets(&seen, &key);
                let refs: Vec<&str> = secrets.iter().map(String::as_str).collect();
                yield Err(classify(status.as_u16(), retry_after, &text, profile.kind, &refs));
                return;
            }

            let mut parser = SseParser::with_limit(MAX_EVENT_BYTES);
            let mut state = Decoder::new();
            let mut bytes = Box::pin(resp.bytes_stream());
            let mut total: usize = 0;

            loop {
                let next = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        yield Err(CaError::Cancelled);
                        return;
                    }
                    next = tokio::time::timeout(IDLE, bytes.next()) => next,
                };

                let chunk = match next {
                    Ok(Some(Ok(chunk))) => chunk,
                    Ok(Some(Err(e))) => {
                        let secrets = secrets(&seen, &key);
                        let refs: Vec<&str> = secrets.iter().map(String::as_str).collect();
                        yield Err(scrub_error(CaError::Transport(e.without_url().to_string()), &refs));
                        return;
                    }
                    Ok(None) => break,
                    Err(_) => {
                        yield Err(CaError::Transport(format!("no data from provider for {IDLE:?}")));
                        return;
                    }
                };

                total = total.saturating_add(chunk.len());
                if total > MAX_BODY_BYTES {
                    yield Err(CaError::Decode("provider response exceeded the size limit".into()));
                    return;
                }

                let events = match parser.push(&chunk) {
                    Ok(events) => events,
                    Err(e) => {
                        yield Err(e);
                        return;
                    }
                };
                // Dialect errors quote the provider's message; scrubbed here, once, for all of them.
                for payload in events {
                    for item in decode_all(dialect.as_ref(), &mut state, &payload) {
                        let done = item.is_err();
                        let secrets = secrets(&seen, &key);
                        let refs: Vec<&str> = secrets.iter().map(String::as_str).collect();
                        yield item.map_err(|e| scrub_error(e, &refs));
                        if done {
                            return;
                        }
                    }
                }
            }

            // A provider that omits the final blank line leaves one event in the parser.
            if let Some(payload) = parser.finish() {
                for item in decode_all(dialect.as_ref(), &mut state, &payload) {
                    let done = item.is_err();
                    let secrets = secrets(&seen, &key);
                    let refs: Vec<&str> = secrets.iter().map(String::as_str).collect();
                    yield item.map_err(|e| scrub_error(e, &refs));
                    if done {
                        return;
                    }
                }
            }
            for item in finish_all(dialect.as_ref(), &mut state) {
                let done = item.is_err();
                let secrets = secrets(&seen, &key);
                let refs: Vec<&str> = secrets.iter().map(String::as_str).collect();
                yield item.map_err(|e| scrub_error(e, &refs));
                if done {
                    return;
                }
            }
        }
        .boxed()
    }
}

/// Decode one payload, flattening the dialect result into a short iterator-friendly shape.
fn decode_all(
    dialect: &dyn Dialect,
    state: &mut Decoder,
    payload: &str,
) -> Vec<Result<CaGenerateResponse, CaError>> {
    match dialect.decode(state, payload) {
        Ok(chunks) => chunks.into_iter().map(Ok).collect(),
        Err(e) => vec![Err(e)],
    }
}

fn finish_all(dialect: &dyn Dialect, state: &mut Decoder) -> Vec<Result<CaGenerateResponse, CaError>> {
    match dialect.finish(state) {
        Ok(chunks) => chunks.into_iter().map(Ok).collect(),
        Err(e) => vec![Err(e)],
    }
}

/// Reject a profile whose stored endpoint is unusable before spending a round trip on it.
fn validate_target(profile: &ProviderProfile) -> Result<(), String> {
    let base = profile.resolved_base_url();
    let url = url::Url::parse(&base).map_err(|e| format!("invalid provider base URL: {e}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!("provider base URL must be http or https, not {}", url.scheme()));
    }
    Ok(())
}

/// Map a non-success status onto the error vocabulary the agent loop already retries on.
///
/// A 429 that waiting cannot clear ends the turn with the provider's reason instead of retrying
/// for minutes and then showing a generic banner: a used-up daily quota (Gemini's `google.rpc`
/// details, read as for Code Assist), an account out of credit (OpenAI `insufficient_quota`,
/// Anthropic `enforced_spend_limit_reached`), or a `Retry-After` longer than the loop honours.
///
/// `kind` is taken rather than the profile because the daily-quota branch is a property of the
/// wire, not of one configuration: Antigravity speaks the same `google.rpc` errors as Gemini and
/// must not be retried for minutes either.
fn classify(status: u16, retry_after: Option<Duration>, body: &str, kind: ProviderKind, secrets: &[&str]) -> CaError {
    let honoured = retry_after.filter(|d| *d <= crate::api::client::MAX_RETRY_DELAY);
    match status {
        401 | 403 => CaError::Auth(scrub(body, secrets)),
        429 if retry_after.is_some() && honoured.is_none() => {
            let reason = scrub(&provider_message(body), secrets);
            let reason = if reason.trim().is_empty() { wait_reason(retry_after.unwrap_or_default()) } else { reason };
            CaError::TerminalQuota { reason }
        }
        429 if body.contains("insufficient_quota") || body.contains("enforced_spend_limit_reached") => {
            CaError::TerminalQuota { reason: scrub(&provider_message(body), secrets) }
        }
        429 if matches!(kind, ProviderKind::GeminiApi | ProviderKind::Antigravity) => {
            match crate::api::client::classify_error(429, body) {
                CaError::TerminalQuota { reason } => CaError::TerminalQuota { reason: scrub(&reason, secrets) },
                _ => CaError::Retryable { delay: honoured },
            }
        }
        429 => CaError::Retryable { delay: honoured },
        404 => CaError::NotFound,
        // The provider's own sentence, not its JSON: it is shown as the turn's error, and it is
        // what says which part of the request was refused.
        400..=499 => {
            let message = provider_message(body);
            let shown = if message.trim().is_empty() { body } else { message.as_str() };
            CaError::Http { status, body_redacted: scrub(shown, secrets) }
        }
        // A server that asks for a longer pause than the loop allows gets the loop's own backoff.
        _ => CaError::Retryable { delay: honoured },
    }
}

/// The reason shown for a `Retry-After` the loop does not wait for, when the body says nothing.
fn wait_reason(wait: Duration) -> String {
    let minutes = wait.as_secs().div_ceil(60);
    let wait = if minutes >= 120 { format!("{} hours", minutes.div_ceil(60)) } else { format!("{minutes} minutes") };
    format!("the provider asks to wait {wait} before the next request")
}

/// `error.message` of a JSON error body (OpenAI, Anthropic and Google all use that shape), else
/// the body itself.
fn provider_message(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("error")?.get("message")?.as_str().map(str::to_owned))
        .unwrap_or_else(|| body.to_owned())
}

/// [`scrub`] every message an error carries, for errors a dialect built from the stream.
fn scrub_error(e: CaError, secrets: &[&str]) -> CaError {
    match e {
        CaError::Auth(m) => CaError::Auth(scrub(&m, secrets)),
        CaError::Decode(m) => CaError::Decode(scrub(&m, secrets)),
        CaError::Transport(m) => CaError::Transport(scrub(&m, secrets)),
        CaError::TerminalQuota { reason } => CaError::TerminalQuota { reason: scrub(&reason, secrets) },
        CaError::Http { status, body_redacted } => CaError::Http { status, body_redacted: scrub(&body_redacted, secrets) },
        other => other,
    }
}

/// Truncate and redact a server message before it can reach a log, the UI or an error report.
///
/// No credential should ever appear: we write it into a header or a query parameter the server
/// does not echo. This runs anyway because a misconfigured gateway may echo the request line or
/// quote the failing credential in `error.message`, and a key or a bearer in a screenshot is a
/// leaked credential. `secrets` is every credential this turn could have put on the wire — the
/// profile's key and every token the driver resolved, which for an account means the token that
/// was refused and the one that replaced it. Both spellings are removed: as typed, and
/// percent-encoded as the Gemini dialect puts the key in the URL. Redaction happens before
/// truncation, so a secret near the end of a long body is still removed.
fn scrub(body: &str, secrets: &[&str]) -> String {
    let mut without_secrets = body.to_owned();
    for secret in secrets.iter().map(|s| s.trim()).filter(|s| s.len() >= 4) {
        for form in [secret.to_owned(), common::percent_encode(secret)] {
            if without_secrets.contains(&form) {
                without_secrets = without_secrets.replace(&form, "<redacted>");
            }
        }
    }
    let trimmed = without_secrets.trim();
    if trimmed.chars().count() > MAX_ERROR_CHARS {
        let mut out: String = trimmed.chars().take(MAX_ERROR_CHARS).collect();
        out.push_str("...");
        out
    } else {
        trimmed.to_owned()
    }
}

/// Read at most `limit` bytes of a body without trusting its length header.
async fn read_limited(mut resp: reqwest::Response, limit: usize) -> Vec<u8> {
    let mut out = Vec::new();
    while let Ok(Some(chunk)) = resp.chunk().await {
        let room = limit.saturating_sub(out.len());
        out.extend_from_slice(&chunk[..chunk.len().min(room)]);
        if out.len() >= limit {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::AuthError;

    fn profile() -> ProviderProfile {
        ProviderProfile {
            id: "p1".into(),
            label: "My provider".into(),
            kind: ProviderKind::OpenAi,
            model: "gpt-4o-mini".into(),
            base_url: None,
            api_key: "sk-secret-value".into(),
            max_output_tokens: None,
            context_window: None,
            models: Vec::new(),
        }
    }

    #[test]
    fn kind_round_trips_through_serde_and_parse() {
        for kind in [
            ProviderKind::GeminiApi,
            ProviderKind::OpenAi,
            ProviderKind::Anthropic,
            ProviderKind::Antigravity,
            ProviderKind::Codex,
        ] {
            let json = serde_json::to_string(&kind).unwrap();
            assert_eq!(json, format!("\"{}\"", kind.as_str()));
            assert_eq!(serde_json::from_str::<ProviderKind>(&json).unwrap(), kind);
            assert_eq!(ProviderKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(ProviderKind::parse("gemini"), Some(ProviderKind::GeminiApi));
        assert_eq!(ProviderKind::parse("chatgpt"), Some(ProviderKind::Codex));
        assert_eq!(ProviderKind::parse("nope"), None);
        assert_eq!(ProviderKind::parse(""), None);
        // The account kinds are the ones a person cannot add.
        assert!(ProviderKind::Antigravity.is_account_only());
        assert!(ProviderKind::Codex.is_account_only());
        assert!(!ProviderKind::GeminiApi.is_account_only());
        assert!(!ProviderKind::OpenAi.is_account_only());
        assert!(!ProviderKind::Anthropic.is_account_only());
    }

    #[test]
    fn default_base_urls_have_no_trailing_slash() {
        for kind in [
            ProviderKind::GeminiApi,
            ProviderKind::OpenAi,
            ProviderKind::Anthropic,
            ProviderKind::Antigravity,
            ProviderKind::Codex,
        ] {
            let url = kind.default_base_url();
            assert!(url.starts_with("https://"));
            assert!(!url.ends_with('/'));
        }
        // The account kinds take their host from the sign-in spec, so the two cannot drift.
        assert_eq!(ProviderKind::Antigravity.default_base_url(), crate::auth::external::ANTIGRAVITY_SPEC.base_url);
        assert_eq!(ProviderKind::Codex.default_base_url(), crate::auth::external::CODEX_SPEC.base_url);
        assert_eq!(ProviderKind::Codex.default_context_window(), 400_000);
        assert_eq!(ProviderKind::Antigravity.default_context_window(), 1_000_000);
    }

    #[tokio::test]
    async fn a_profile_without_a_key_is_refused_only_in_api_key_mode() {
        let mut p = profile();
        p.api_key.clear();
        assert!(p.validate().is_err(), "a key profile needs its key");
        assert!(p.validate_shape().is_ok(), "the account path has none by construction");

        // The api-key backend reports it before any request.
        let backend = ProviderBackend::new(reqwest::Client::new(), p.clone());
        let mut stream = backend.stream_turn(
            String::new(),
            String::new(),
            GenerateContentRequest::default(),
            CancellationToken::new(),
        );
        assert!(matches!(bounded(stream.next()).await, Some(Err(CaError::Auth(_)))));

        // The same profile in account mode reaches the token source instead, which is what makes a
        // keyless account work at all.
        let (base, _seen) = recording_server(OPENAI_OK).await;
        let tokens = FakeTokens::new("at-keyless", "at-keyless-2");
        let backend = ProviderBackend::with_oauth(
            reqwest::Client::new(),
            account_profile(&base),
            tokens.clone(),
            AccountMeta::default(),
        );
        let items: Vec<_> = bounded(
            backend
                .stream_turn(String::new(), String::new(), GenerateContentRequest::default(), CancellationToken::new())
                .collect(),
        )
        .await;
        assert!(items.iter().all(Result::is_ok), "{items:?}");
        assert_eq!(tokens.access_calls(), 1);

        // `with_model` is the picker's choice and never produces an empty id.
        let picked = profile().with_model("gpt-5.1-codex");
        assert_eq!(picked.model, "gpt-5.1-codex");
        assert_eq!(profile().with_model("   ").model, "gpt-4o-mini", "a blank pick keeps the stored model");
    }

    #[test]
    fn validation_rejects_empty_fields_and_bad_urls() {
        assert!(profile().validate().is_ok());

        let mut p = profile();
        p.api_key = "   ".into();
        assert!(p.validate().is_err());

        let mut p = profile();
        p.id = String::new();
        assert!(p.validate().is_err());

        // Saved before a model is chosen: kept, and refused at turn time instead (below).
        let mut p = profile();
        p.model = String::new();
        assert!(p.validate().is_ok());

        let mut p = profile();
        p.base_url = Some("ftp://example.com".into());
        assert!(p.validate().is_err());

        let mut p = profile();
        p.base_url = Some("not a url".into());
        assert!(p.validate().is_err());

        let mut p = profile();
        p.base_url = Some("https://example.com/v1".into());
        assert!(p.validate().is_ok());

        let mut p = profile();
        p.max_output_tokens = Some(0);
        assert!(p.validate().is_err());
    }

    /// A profile whose model was never chosen sends nothing: an empty id would only come back as
    /// the provider's 400, read as a broken request.
    #[tokio::test]
    async fn a_profile_without_a_model_sends_no_request() {
        let (base, seen) = recording_server(OPENAI_OK).await;
        let p = ProviderProfile { model: String::new(), base_url: Some(base), ..profile() };
        let backend = ProviderBackend::new(reqwest::Client::new(), p.clone());
        let items: Vec<_> = bounded(
            backend.stream_turn("  ".into(), String::new(), GenerateContentRequest::default(), CancellationToken::new()).collect(),
        )
        .await;
        assert!(matches!(items.as_slice(), [Err(CaError::Decode(m))] if m == "no model is chosen for My provider"), "{items:?}");
        assert!(seen.lock().unwrap().is_none(), "no request was sent");

        // The picked model is enough on its own.
        let backend = ProviderBackend::new(reqwest::Client::new(), p);
        let items: Vec<_> = bounded(
            backend.stream_turn("gpt-4o".into(), String::new(), GenerateContentRequest::default(), CancellationToken::new()).collect(),
        )
        .await;
        assert!(items.iter().all(Result::is_ok), "{items:?}");
        assert!(seen.lock().unwrap().as_deref().is_some_and(|b| b.contains("\"gpt-4o\"")));
    }

    #[test]
    fn resolved_base_url_falls_back_to_the_kind_default() {
        let mut p = profile();
        assert_eq!(p.resolved_base_url(), "https://api.openai.com/v1");
        p.base_url = Some("http://localhost:8080/v1/".into());
        assert_eq!(p.resolved_base_url(), "http://localhost:8080/v1");
        p.base_url = Some("   ".into());
        assert_eq!(p.resolved_base_url(), "https://api.openai.com/v1");
    }

    #[test]
    fn debug_redacts_the_key() {
        let rendered = format!("{:?}", profile());
        assert!(!rendered.contains("sk-secret-value"));
        assert!(rendered.contains("<redacted>"));

        let backend = ProviderBackend::new(reqwest::Client::new(), profile());
        let rendered = format!("{backend:?}");
        assert!(!rendered.contains("sk-secret-value"));
        assert_eq!(backend.profile().id, "p1");
    }

    #[test]
    fn scrub_truncates_and_removes_the_key() {
        let p = profile();
        let secrets = [p.api_key.trim()];
        let long = format!("{} sk-secret-value", "x".repeat(600));
        let out = scrub(&long, &secrets);
        // The key sits past the truncation point, so the placeholder is cut off too; what matters
        // is that no fragment of the key survives.
        assert!(!out.contains("sk-secret-value"));
        assert!(!out.contains("secret"));
        assert!(out.chars().count() <= MAX_ERROR_CHARS + 3);

        let short = format!("sk-secret-value was rejected: {}", "y".repeat(600));
        let out = scrub(&short, &secrets);
        assert!(!out.contains("sk-secret-value"));
        assert!(out.starts_with("<redacted> was rejected"));

        assert_eq!(scrub("  plain  ", &secrets), "plain");
        // Too short to be a real key is left alone rather than mangling ordinary text.
        assert_eq!(scrub("abcdef", &["abc"]), "abcdef");
    }

    #[test]
    fn scrub_removes_every_resolved_token_not_only_the_key() {
        // An account has no key at all, and a provider that echoes the failing credential quotes
        // the bearer. Both the refused token and the refreshed one are removed.
        let secrets = ["", "eyJhbGciOiJIUzI1NiJ9.first", "sk-new-token"];
        let body = "401 for eyJhbGciOiJIUzI1NiJ9.first; retried with sk-new-token";
        let out = scrub(body, &secrets);
        assert!(!out.contains("eyJhbGciOiJIUzI1NiJ9.first"), "{out}");
        assert!(!out.contains("sk-new-token"), "{out}");
        assert_eq!(out.matches("<redacted>").count(), 2, "{out}");
    }

    #[test]
    fn classify_maps_status_to_the_loop_vocabulary() {
        let p = profile();
        let secrets = [p.api_key.trim()];
        assert!(matches!(classify(401, None, "nope", p.kind, &secrets), CaError::Auth(_)));
        assert!(matches!(classify(403, None, "nope", p.kind, &secrets), CaError::Auth(_)));
        assert!(matches!(classify(404, None, "", p.kind, &secrets), CaError::NotFound));
        match classify(429, Some(Duration::from_secs(7)), "slow down", p.kind, &secrets) {
            CaError::Retryable { delay } => assert_eq!(delay, Some(Duration::from_secs(7))),
            other => panic!("expected retryable, got {other:?}"),
        }
        assert!(matches!(classify(503, None, "", p.kind, &secrets), CaError::Retryable { delay: None }));
        match classify(422, None, "bad field", p.kind, &secrets) {
            CaError::Http { status, body_redacted } => {
                assert_eq!(status, 422);
                assert_eq!(body_redacted, "bad field");
            }
            other => panic!("expected http error, got {other:?}"),
        }
    }

    /// A refused request ends the turn with the provider's own sentence, not its JSON: the
    /// sentence is what the banner shows, and it says what was refused.
    #[test]
    fn a_refused_request_shows_the_providers_sentence() {
        let p = profile();
        let secrets = [p.api_key.trim()];
        let body = r#"{"error":{"code":400,"message":"The selected model cannot continue this conversation.","status":"INVALID_ARGUMENT"}}"#;
        assert_eq!(
            classify(400, None, body, ProviderKind::Antigravity, &secrets).to_string(),
            "HTTP 400: The selected model cannot continue this conversation."
        );
        // A body without one is shown as it is, and a blank sentence does not hide the body.
        assert_eq!(classify(400, None, "plain refusal", p.kind, &secrets).to_string(), "HTTP 400: plain refusal");
        let blank = r#"{"error":{"message":" "}}"#;
        assert_eq!(classify(400, None, blank, p.kind, &secrets).to_string(), format!("HTTP 400: {blank}"));
        // The sentence is scrubbed as the body was.
        let echoed = format!(r#"{{"error":{{"message":"bad key {}"}}}}"#, p.api_key);
        assert_eq!(classify(400, None, &echoed, p.kind, &secrets).to_string(), "HTTP 400: bad key <redacted>");
    }

    #[test]
    fn a_429_that_retrying_cannot_clear_ends_the_turn_with_the_reason() {
        let p = profile();
        let secrets = [p.api_key.trim()];
        // OpenAI: the account is out of credit.
        let body = r#"{"error":{"message":"You exceeded your current quota, please check your plan and billing details.","type":"insufficient_quota","code":"insufficient_quota"}}"#;
        match classify(429, None, body, p.kind, &secrets) {
            CaError::TerminalQuota { reason } => assert!(reason.contains("exceeded your current quota"), "{reason}"),
            other => panic!("expected a terminal quota, got {other:?}"),
        }
        // A wait far past what the loop honours. With no message in the body, the reason is the
        // wait itself, so the banner does not end in "quota exhausted: ".
        match classify(429, Some(Duration::from_secs(86_400)), "", p.kind, &secrets) {
            CaError::TerminalQuota { reason } => assert!(reason.contains("24 hours"), "{reason}"),
            other => panic!("expected a terminal quota, got {other:?}"),
        }
        // Gemini Developer API: the daily free-tier quota.
        let body = r#"{"error":{"code":429,"message":"Quota exceeded","status":"RESOURCE_EXHAUSTED","details":[{"@type":"type.googleapis.com/google.rpc.QuotaFailure","violations":[{"quotaId":"GenerateRequestsPerDayPerProjectPerModel-FreeTier"}]}]}}"#;
        assert!(matches!(
            classify(429, None, body, ProviderKind::GeminiApi, &secrets),
            CaError::TerminalQuota { .. }
        ));
        // Antigravity speaks the same google.rpc errors; a subscription's daily limit must not be
        // retried for minutes either.
        assert!(matches!(
            classify(429, None, body, ProviderKind::Antigravity, &secrets),
            CaError::TerminalQuota { .. }
        ));
        // An ordinary rate limit is still retried, after the delay the provider asked for.
        assert!(matches!(classify(429, Some(Duration::from_secs(7)), "slow down", p.kind, &secrets), CaError::Retryable { delay: Some(d) } if d == Duration::from_secs(7)));
    }

    #[test]
    fn scrub_removes_the_key_as_the_url_carries_it() {
        // A key with characters the query string encodes, as a gateway may issue.
        let out = scrub(
            "rejected /models/m:streamGenerateContent?alt=sse&key=gw%20test%2Fkey%2B1",
            &["gw test/key+1"],
        );
        assert!(!out.contains("gw%20test%2Fkey%2B1") && !out.contains("gw test/key+1"));
        assert!(out.contains("<redacted>"));
    }

    #[tokio::test]
    async fn an_error_inside_the_stream_never_carries_the_key() {
        // A gateway answers 200, then reports the upstream failure in the stream, quoting the
        // request line with the key in it.
        let key = "gw test/key+1";
        let body = "data: {\"error\":{\"code\":403,\"message\":\"upstream rejected /v1beta/models/m:streamGenerateContent?alt=sse&key=gw%20test%2Fkey%2B1\"}}\n\n";
        let app = axum::Router::new().fallback(move || async move { body });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let profile = ProviderProfile {
            id: "g".into(),
            label: "Gateway".into(),
            kind: ProviderKind::GeminiApi,
            model: "m".into(),
            base_url: Some(format!("http://{addr}/v1beta")),
            api_key: key.into(),
            max_output_tokens: None,
            context_window: None,
            models: Vec::new(),
        };
        let backend = ProviderBackend::new(reqwest::Client::new(), profile);
        let mut stream = backend.stream_turn("m".into(), "u".into(), GenerateContentRequest::default(), CancellationToken::new());
        let Some(Err(err)) = stream.next().await else { panic!("the gateway's error is reported") };
        let shown = err.to_string();
        assert!(!shown.contains("gw%20test%2Fkey%2B1") && !shown.contains(key));
        assert!(shown.contains("<redacted>"));
    }

    #[tokio::test]
    async fn invalid_profile_errors_before_any_request() {
        let mut p = profile();
        p.api_key.clear();
        let backend = ProviderBackend::new(reqwest::Client::new(), p);
        let mut stream = backend.stream_turn(
            "m".into(),
            "u".into(),
            GenerateContentRequest::default(),
            CancellationToken::new(),
        );
        let first = stream.next().await.expect("one item");
        assert!(matches!(first, Err(CaError::Auth(_))));
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn cancelled_token_ends_the_stream_without_a_request() {
        let backend = ProviderBackend::new(reqwest::Client::new(), profile());
        let cancel = CancellationToken::new();
        cancel.cancel();
        // The endpoint is unreachable and the token is already cancelled; select is biased so the
        // stream ends with no item instead of attempting the POST.
        let mut stream = backend.stream_turn(
            "m".into(),
            "u".into(),
            GenerateContentRequest::default(),
            cancel,
        );
        assert!(stream.next().await.is_none());
    }

    /// A local server that records the body it is sent and answers with `reply`.
    async fn recording_server(reply: &'static str) -> (String, std::sync::Arc<std::sync::Mutex<Option<String>>>) {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let record = seen.clone();
        let app = axum::Router::new().fallback(move |body: String| {
            let record = record.clone();
            async move {
                *record.lock().unwrap() = Some(body);
                reply
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), seen)
    }

    /// A token source that hands out `first`, then `second` after one forced refresh.
    ///
    /// Counts both calls so a test can assert that exactly one refresh happened, and holds `delay`
    /// so a test can cancel the turn in the middle of the refresh.
    struct FakeTokens {
        current: std::sync::Mutex<String>,
        second: String,
        refreshes: std::sync::atomic::AtomicUsize,
        access_calls: std::sync::atomic::AtomicUsize,
        delay: Duration,
    }

    impl FakeTokens {
        fn new(first: &str, second: &str) -> Arc<Self> {
            Arc::new(Self {
                current: std::sync::Mutex::new(first.to_owned()),
                second: second.to_owned(),
                refreshes: std::sync::atomic::AtomicUsize::new(0),
                access_calls: std::sync::atomic::AtomicUsize::new(0),
                delay: Duration::ZERO,
            })
        }

        fn with_delay(self: Arc<Self>, delay: Duration) -> Arc<Self> {
            Arc::new(Self {
                current: std::sync::Mutex::new(self.current.lock().unwrap().clone()),
                second: self.second.clone(),
                refreshes: std::sync::atomic::AtomicUsize::new(0),
                access_calls: std::sync::atomic::AtomicUsize::new(0),
                delay,
            })
        }

        fn refreshes(&self) -> usize {
            self.refreshes.load(std::sync::atomic::Ordering::SeqCst)
        }

        fn access_calls(&self) -> usize {
            self.access_calls.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl crate::auth::TokenProvider for FakeTokens {
        fn access_token(&self) -> futures::future::BoxFuture<'_, Result<String, AuthError>> {
            self.access_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let token = self.current.lock().unwrap().clone();
            Box::pin(async move { Ok(token) })
        }

        fn force_refresh<'a>(&'a self, failed: &'a str) -> futures::future::BoxFuture<'a, Result<String, AuthError>> {
            Box::pin(async move {
                // The single-use guard the real implementation has: a token that is no longer the
                // one that failed is returned without spending the refresh token.
                if *self.current.lock().unwrap() != failed {
                    return Ok(self.current.lock().unwrap().clone());
                }
                if !self.delay.is_zero() {
                    tokio::time::sleep(self.delay).await;
                }
                self.refreshes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                *self.current.lock().unwrap() = self.second.clone();
                Ok(self.second.clone())
            })
        }
    }

    /// Every wait on a mock server in the account tests is bounded: a driver bug (a refresh that
    /// never returns, a resend loop) must fail the test in seconds, not wait out the idle timeout.
    async fn bounded<T>(f: impl std::future::Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(20), f).await.expect("the turn finished in time")
    }

    /// A server that answers the first request with `first` and every later one with `later`,
    /// recording each `Authorization` header it was sent.
    async fn auth_sequence_server(
        first: (u16, &'static str),
        later: (u16, &'static str),
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let record = seen.clone();
        let n = std::sync::Arc::new(AtomicUsize::new(0));
        let app = axum::Router::new().fallback(move |headers: axum::http::HeaderMap| {
            let record = record.clone();
            let n = n.clone();
            async move {
                let auth = headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_owned();
                record.lock().unwrap().push(auth);
                let (status, body) = if n.fetch_add(1, Ordering::SeqCst) == 0 { first } else { later };
                (axum::http::StatusCode::from_u16(status).unwrap(), body)
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), seen)
    }

    /// An account profile: no key, and a kind the account path can reach.
    fn account_profile(base: &str) -> ProviderProfile {
        ProviderProfile {
            id: "acct:codex-0123abcd".into(),
            label: "ChatGPT (Codex)".into(),
            kind: ProviderKind::OpenAi,
            model: "gpt-5.1-codex".into(),
            base_url: Some(base.to_owned()),
            api_key: String::new(),
            max_output_tokens: None,
            context_window: None,
            models: Vec::new(),
        }
    }

    /// The one SSE frame a chat-completions turn needs to finish cleanly.
    const OPENAI_OK: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";

    #[tokio::test]
    async fn an_oauth_backend_refreshes_once_on_401() {
        let (base, seen) = auth_sequence_server((401, "{\"error\":\"token expired\"}"), (200, OPENAI_OK)).await;
        let tokens = FakeTokens::new("at-first", "at-second");
        let backend = ProviderBackend::with_oauth(
            reqwest::Client::new(),
            account_profile(&base),
            tokens.clone(),
            AccountMeta::default(),
        );
        let items: Vec<_> = bounded(
            backend
                .stream_turn("gpt-5.1-codex".into(), "u".into(), GenerateContentRequest::default(), CancellationToken::new())
                .collect(),
        )
        .await;

        assert!(items.iter().all(Result::is_ok), "{items:?}");
        let sent = seen.lock().unwrap().clone();
        assert_eq!(sent, vec!["Bearer at-first".to_owned(), "Bearer at-second".to_owned()]);
        assert_eq!(tokens.refreshes(), 1, "exactly one refresh");
    }

    #[tokio::test]
    async fn a_cancelled_turn_never_sends_the_second_attempt() {
        let (base, seen) = auth_sequence_server((401, "{\"error\":\"token expired\"}"), (200, OPENAI_OK)).await;
        let tokens = FakeTokens::new("at-first", "at-second").with_delay(Duration::from_millis(300));
        let backend = ProviderBackend::with_oauth(
            reqwest::Client::new(),
            account_profile(&base),
            tokens.clone(),
            AccountMeta::default(),
        );
        let cancel = CancellationToken::new();
        let mut stream =
            backend.stream_turn("gpt-5.1-codex".into(), "u".into(), GenerateContentRequest::default(), cancel.clone());

        // Let the first POST happen and the refresh begin, then walk away.
        let first = tokio::time::timeout(Duration::from_millis(100), stream.next()).await;
        assert!(first.is_err(), "the first attempt is refused and the turn waits on the refresh: {first:?}");
        cancel.cancel();
        // The stream ends with nothing: no item, and no second POST.
        let next = tokio::time::timeout(Duration::from_secs(5), stream.next()).await.expect("the stream ends");
        assert!(next.is_none(), "a cancelled turn yields no item, got {next:?}");
        let sent = seen.lock().unwrap().clone();
        assert_eq!(sent, vec!["Bearer at-first".to_owned()], "the refresh was cancelled before the resend");
    }

    #[tokio::test]
    async fn a_token_echoed_in_an_error_is_redacted() {
        // Every variant the driver can build from a provider body, with the bearer quoted back in
        // it: a gateway that echoes the request has to leave no credential in the banner.
        for (status, reply, want) in [
            (401u16, "{\"error\":\"bad token at-first\"}", "auth"),
            (422, "{\"error\":\"bad field at-first\"}", "http"),
            (429, "{\"error\":{\"message\":\"insufficient_quota at-first\",\"type\":\"insufficient_quota\"}}", "quota"),
        ] {
            let (base, _seen) = auth_sequence_server((status, reply), (status, reply)).await;
            let tokens = FakeTokens::new("at-first", "at-second");
            let backend = ProviderBackend::with_oauth(
                reqwest::Client::new(),
                account_profile(&base),
                tokens,
                AccountMeta::default(),
            );
            let mut stream = backend.stream_turn(
                "gpt-5.1-codex".into(),
                "u".into(),
                GenerateContentRequest::default(),
                CancellationToken::new(),
            );
            let Some(Err(err)) = bounded(stream.next()).await else { panic!("{status} is reported") };
            match (want, &err) {
                ("auth", CaError::Auth(_)) | ("http", CaError::Http { .. }) | ("quota", CaError::TerminalQuota { .. }) => {}
                other => panic!("expected {want}, got {other:?}"),
            }
            let shown = err.to_string();
            assert!(!shown.contains("at-first"), "{want}: {shown}");
            assert!(!shown.contains("at-second"), "{want}: {shown}");
            assert!(shown.contains("<redacted>"), "{want}: {shown}");
        }

        // A decode error built from the stream, quoting the token inside the SSE event.
        let reply = "data: {\"error\":{\"type\":\"invalid_request_error\",\"message\":\"rejected at-first\"}}\n\n";
        let (base, _seen) = auth_sequence_server((200, reply), (200, reply)).await;
        let backend = ProviderBackend::with_oauth(
            reqwest::Client::new(),
            account_profile(&base),
            FakeTokens::new("at-first", "at-second"),
            AccountMeta::default(),
        );
        let mut stream = backend.stream_turn(
            "gpt-5.1-codex".into(),
            "u".into(),
            GenerateContentRequest::default(),
            CancellationToken::new(),
        );
        let Some(Err(err)) = bounded(stream.next()).await else { panic!("the stream error is reported") };
        assert!(matches!(err, CaError::Decode(_)), "{err:?}");
        let shown = err.to_string();
        assert!(!shown.contains("at-first"), "{shown}");
        assert!(shown.contains("<redacted>"), "{shown}");
    }

    #[tokio::test]
    async fn an_oauth_backend_needs_no_key_and_sends_no_key_header() {
        // `validate()` would refuse this profile for its empty key; the account path checks shape.
        let (base, seen) = recording_server(OPENAI_OK).await;
        let tokens = FakeTokens::new("at-only", "at-only-2");
        let backend = ProviderBackend::with_oauth(
            reqwest::Client::new(),
            account_profile(&base),
            tokens.clone(),
            AccountMeta::default(),
        );
        let items: Vec<_> = bounded(
            backend
                .stream_turn("gpt-5.1-codex".into(), "u".into(), GenerateContentRequest::default(), CancellationToken::new())
                .collect(),
        )
        .await;
        assert!(items.iter().all(Result::is_ok), "{items:?}");
        assert!(seen.lock().unwrap().is_some(), "the request reached the server");
        assert_eq!(tokens.access_calls(), 1);
        assert_eq!(tokens.refreshes(), 0, "a 200 never refreshes");
    }

    #[tokio::test]
    async fn the_turn_model_reaches_the_body() {
        // The picker's choice, not the profile's stored default.
        let (base, seen) = recording_server(OPENAI_OK).await;
        let backend = ProviderBackend::new(reqwest::Client::new(), ProviderProfile {
            base_url: Some(base),
            ..profile()
        });
        let items: Vec<_> = bounded(
            backend
                .stream_turn("gpt-4o-picked".into(), "u".into(), GenerateContentRequest::default(), CancellationToken::new())
                .collect(),
        )
        .await;
        assert!(items.iter().all(Result::is_ok), "{items:?}");
        let body = seen.lock().unwrap().clone().expect("the request reached the server");
        assert!(body.contains("\"gpt-4o-picked\""), "{body}");
        assert!(!body.contains("gpt-4o-mini"), "the stored default must not win: {body}");
    }

    #[tokio::test]
    async fn another_providers_reasoning_blocks_never_leave_the_driver() {
        use crate::api::{Content, FunctionCall, Part, ProviderBlock};
        // A conversation that ran on Anthropic, continued on another provider.
        let thinking = Part {
            provider_block: Some(ProviderBlock {
                provider: ProviderKind::Anthropic.as_str().into(),
                binding: Some("b1".into()),
                block: serde_json::json!({"type":"thinking","thinking":"plan","signature":"sig-anthropic"}),
            }),
            ..Default::default()
        };
        let call = Part {
            function_call: Some(FunctionCall { id: Some("glob__toolu_1".into()), name: "glob".into(), args: serde_json::json!({}) }),
            ..Default::default()
        };
        let request = GenerateContentRequest {
            contents: vec![Content::user_text("list"), Content::model(vec![thinking, call]), Content::user_text("again")],
            ..Default::default()
        };
        for (kind, reply) in [
            (ProviderKind::GeminiApi, "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"ok\"}]}}]}\n\n"),
            (ProviderKind::OpenAi, "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n"),
        ] {
            let (base, seen) = recording_server(reply).await;
            let backend = ProviderBackend::new(reqwest::Client::new(), ProviderProfile { kind, base_url: Some(base), ..profile() });
            let items: Vec<_> = backend.stream_turn("m".into(), "u".into(), request.clone(), CancellationToken::new()).collect().await;
            assert!(items.iter().all(Result::is_ok), "{kind}: {items:?}");
            let body = seen.lock().unwrap().clone().expect("the request reached the server");
            assert!(!body.contains("sig-anthropic") && !body.contains("providerBlock") && !body.contains("\"plan\""), "{kind}: {body}");
            assert!(body.contains("glob"), "{kind}: the call the block preceded is still sent: {body}");
        }
    }

    /// A thinking model behind an Anthropic-compatible proxy: the first reply thinks, signs and
    /// calls a tool; the second request is refused, as the user's proxy refused it, unless the
    /// assistant turn carries its signed thinking back ahead of the call.
    #[tokio::test]
    async fn a_thinking_models_tool_turn_completes_through_the_agent_loop() {
        use crate::agent::{Agent, AgentConfig, AgentEvent, FinishReason, History, ToolLimits, ToolRegistry, Workspace};
        use std::sync::atomic::{AtomicUsize, Ordering};

        let sse = |events: &[&str]| events.iter().map(|e| format!("data: {e}\n\n")).collect::<String>();
        let first = sse(&[
            r#"{"type":"message_start","message":{"model":"claude-opus-5.5-thinking","usage":{"input_tokens":10}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Read it first."}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig-e2e"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_e2e","name":"read_file","input":{}}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"file_path\":\"a.txt\"}"}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":12}}"#,
            r#"{"type":"message_stop"}"#,
        ]);
        let done = sse(&[
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"done"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}"#,
            r#"{"type":"message_stop"}"#,
        ]);
        let refused = sse(&[
            r#"{"type":"error","error":{"type":"api_error","message":"The selected model cannot continue this conversation. Please select a different model, or start a new conversation, or rewind the current conversation to an earlier step."}}"#,
        ]);
        let hits = std::sync::Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        let app = axum::Router::new().fallback(move |body: String| {
            let n = counter.fetch_add(1, Ordering::SeqCst);
            let (first, done, refused) = (first.clone(), done.clone(), refused.clone());
            async move {
                if n == 0 {
                    return first;
                }
                let body: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
                let thinking = serde_json::json!({"type":"thinking","thinking":"Read it first.","signature":"sig-e2e"});
                let carried = body["messages"][1]["content"][0] == thinking
                    && body["messages"][1]["content"][1]["id"] == "toolu_e2e"
                    && body["messages"][2]["content"][0]["tool_use_id"] == "toolu_e2e";
                if carried { done } else { refused }
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello").unwrap();
        let profile = ProviderProfile {
            kind: ProviderKind::Anthropic,
            model: "claude-opus-5.5-thinking".into(),
            base_url: Some(format!("http://{addr}/")),
            ..profile()
        };
        let backend = std::sync::Arc::new(ProviderBackend::new(reqwest::Client::new(), profile));
        let tools = ToolRegistry::new(Workspace::new(dir.path()).unwrap(), ToolLimits::default());
        let mut cfg = AgentConfig::new("claude-opus-5.5-thinking", "sys", "s");
        cfg.retry = crate::api::RetryPolicy {
            max_attempts: 3,
            initial_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(5),
        };
        let mut agent = Agent::new(backend, tools, cfg, History::new());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        agent.run_prompt("read a.txt".into(), tx, CancellationToken::new()).await;
        let mut finished = None;
        while let Some(event) = rx.recv().await {
            if let AgentEvent::Finished { reason, history } = event {
                finished = Some((reason, history));
            }
        }
        let (reason, history) = finished.expect("the loop reports its end");
        assert_eq!(reason, FinishReason::Completed, "{history:?}");
        assert_eq!(history.last().map(|c| c.text()).as_deref(), Some("done"));
        assert_eq!(hits.load(Ordering::SeqCst), 2, "no retry was needed");
        // The signed block is part of the history the app saves.
        assert!(serde_json::to_string(&history).unwrap().contains("sig-e2e"));
    }

    /// The turn that stopped as soon as its tools ran: Claude on an Antigravity account thinks,
    /// signs and reads two files. The request carrying the results is refused with the 400 the
    /// backend sent, unless the model turn starts with that signed thinking and every result
    /// names its call by the id the backend issued.
    #[tokio::test]
    async fn an_antigravity_claude_tool_round_completes_through_the_agent_loop() {
        use crate::agent::{Agent, AgentConfig, AgentEvent, FinishReason, History, ToolLimits, ToolRegistry, Workspace};
        use axum::response::IntoResponse;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let sse = |events: &[&str]| events.iter().map(|e| format!("data: {e}\n\n")).collect::<String>();
        let first = sse(&[
            r#"{"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"Read both.","thought":true}]}}]}}"#,
            r#"{"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"","thought":true,"thoughtSignature":"sig-ag"}]}}]}}"#,
            r#"{"response":{"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"read_file","args":{"file_path":"a.txt"},"id":"toolu_vrtx_a"}},{"functionCall":{"name":"read_file","args":{"file_path":"b.txt"},"id":"toolu_vrtx_b"}}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":5,"totalTokenCount":15}}}"#,
        ]);
        let done = sse(&[r#"{"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"done"}]},"finishReason":"STOP"}]}}"#]);
        let refused = r#"{"error":{"code":400,"message":"The selected model cannot continue this conversation.","status":"INVALID_ARGUMENT"}}"#;
        let hits = std::sync::Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        let app = axum::Router::new().fallback(move |body: String| {
            let n = counter.fetch_add(1, Ordering::SeqCst);
            let (first, done) = (first.clone(), done.clone());
            async move {
                if n == 0 {
                    return first.into_response();
                }
                let body: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
                let contents = &body["request"]["contents"];
                let thinking = serde_json::json!({"thought": true, "text": "Read both.", "thoughtSignature": "sig-ag"});
                let carried = contents[1]["parts"][0] == thinking
                    && contents[1]["parts"][1]["functionCall"]["id"] == "toolu_vrtx_a"
                    && contents[1]["parts"][2]["functionCall"]["id"] == "toolu_vrtx_b"
                    && contents[2]["parts"][0]["functionResponse"]["id"] == "toolu_vrtx_a"
                    && contents[2]["parts"][1]["functionResponse"]["id"] == "toolu_vrtx_b";
                if carried {
                    done.into_response()
                } else {
                    (axum::http::StatusCode::BAD_REQUEST, refused).into_response()
                }
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "alpha").unwrap();
        std::fs::write(dir.path().join("b.txt"), "beta").unwrap();
        let profile = ProviderProfile {
            id: "acct:antigravity-0123abcd".into(),
            label: "Antigravity".into(),
            kind: ProviderKind::Antigravity,
            model: "claude-opus-5-5-high".into(),
            base_url: Some(format!("http://{addr}")),
            api_key: String::new(),
            ..profile()
        };
        let meta = AccountMeta { account_id: None, project_id: Some("projects/123456".into()) };
        let backend = std::sync::Arc::new(ProviderBackend::with_oauth(
            reqwest::Client::new(),
            profile,
            FakeTokens::new("at-first", "at-second"),
            meta,
        ));
        let tools = ToolRegistry::new(Workspace::new(dir.path()).unwrap(), ToolLimits::default());
        let mut cfg = AgentConfig::new("claude-opus-5-5-high", "sys", "s");
        cfg.retry = crate::api::RetryPolicy {
            max_attempts: 3,
            initial_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(5),
        };
        let mut agent = Agent::new(backend, tools, cfg, History::new());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        bounded(agent.run_prompt("read a.txt and b.txt".into(), tx, CancellationToken::new())).await;
        let (mut finished, mut errors) = (None, Vec::new());
        while let Some(event) = rx.recv().await {
            match event {
                AgentEvent::Finished { reason, history } => finished = Some((reason, history)),
                AgentEvent::Error(e) => errors.push(e.to_string()),
                _ => {}
            }
        }
        let (reason, history) = finished.expect("the loop reports its end");
        assert_eq!(reason, FinishReason::Completed, "{errors:?}");
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(history.last().map(|c| c.text()).as_deref(), Some("done"));
        assert_eq!(hits.load(Ordering::SeqCst), 2, "no retry was needed");
        // The signed thinking is part of the history the app saves, so a reopened conversation
        // can still continue it.
        assert!(serde_json::to_string(&history).unwrap().contains("sig-ag"));
    }
}
