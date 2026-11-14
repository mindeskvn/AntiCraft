//! User settings at `{data_dir}/settings.json`, plus data-dir resolution.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::agent::plugins::PluginChoice;
pub use crate::agent::policy::{AgentMode, ArtifactReviewPolicy, PermissionMode, TerminalPolicy};
use crate::agent::policy::{validate_rule, ApprovalPolicy, MAX_RULES};
use crate::auth::external::{self, AccountProvider};
use crate::providers::{ProviderKind, ProviderProfile};
use crate::auth::store::atomic_write_private;
pub use crate::ui::queue::QueueMode;

pub const DEFAULT_MODEL: &str = "gemini-2.5-pro";
/// Smallest `shell_output_cap_bytes` accepted by [`Settings::sanitized`].
pub const MIN_SHELL_OUTPUT_CAP_BYTES: usize = 1024;
/// Largest `shell_output_cap_bytes` accepted by [`Settings::sanitized`].
pub const MAX_SHELL_OUTPUT_CAP_BYTES: usize = 1024 * 1024;

/// Model dropdown contents (PROTOCOL 3.11).
pub const MODEL_CHOICES: &[&str] = &[
    "auto",
    "gemini-2.5-pro",
    "gemini-3-pro-preview",
    "gemini-3.1-pro-preview",
    "gemini-3-flash-preview",
    "gemini-3.5-flash",
    "gemini-3.1-flash-lite",
];
/// Model aliases accepted in settings and the dropdown (PROTOCOL 3.11).
pub const ALIAS_AUTO: &str = "auto";
pub const ALIAS_PRO: &str = "pro";
pub const ALIAS_FLASH: &str = "flash";
pub const ALIAS_FLASH_LITE: &str = "flash-lite";
pub const PREVIEW_PRO_MODEL: &str = "gemini-3-pro-preview";
pub const PREVIEW_FLASH_MODEL: &str = "gemini-3-flash-preview";
pub const BASE_FLASH_MODEL: &str = "gemini-3.5-flash";
pub const LATEST_FLASH_MODEL: &str = "gemini-3.8-flash";
pub const LEGACY_CA_FLASH_MODEL: &str = "gemini-3-flash";
pub const BASE_FLASH_LITE_MODEL: &str = "gemini-3.1-flash-lite";

/// Resolve an alias (`auto`, `pro`, `flash`, `flash-lite`) to a concrete model ID
/// (PROTOCOL 3.11). Concrete IDs pass through unchanged (trimmed).
///
/// `preview_access`: whether the account may use preview models.
/// `latest_flash_ga`: whether the latest Flash GA model is enabled.
pub fn resolve_model(requested: &str, preview_access: bool, latest_flash_ga: bool) -> String {
    let m = requested.trim();
    match m {
        "" => DEFAULT_MODEL.to_owned(),
        ALIAS_AUTO | ALIAS_PRO if !preview_access => DEFAULT_MODEL.to_owned(),
        ALIAS_AUTO | ALIAS_PRO => PREVIEW_PRO_MODEL.to_owned(),
        ALIAS_FLASH if latest_flash_ga => LATEST_FLASH_MODEL.to_owned(),
        ALIAS_FLASH if preview_access => PREVIEW_FLASH_MODEL.to_owned(),
        ALIAS_FLASH => BASE_FLASH_MODEL.to_owned(),
        ALIAS_FLASH_LITE => BASE_FLASH_LITE_MODEL.to_owned(),
        other => other.to_owned(),
    }
}

/// Model ID as sent to Code Assist (PROTOCOL 3.11 backend remapping): `gemini-3.5-flash` goes
/// out as `gemini-3-flash` unless latest-flash GA is enabled, in which case both become
/// `gemini-3.8-flash`.
pub fn backend_model_id(model: &str, latest_flash_ga: bool) -> String {
    match model {
        BASE_FLASH_MODEL | LEGACY_CA_FLASH_MODEL if latest_flash_ga => LATEST_FLASH_MODEL.to_owned(),
        BASE_FLASH_MODEL => LEGACY_CA_FLASH_MODEL.to_owned(),
        other => other.to_owned(),
    }
}

/// Current `settings.json` schema version.
///
/// 0 (absent): written before the policy fields. 1: policy fields. 2: `providers` and
/// `active_provider`. 3: `accounts` and `active_account`. 4: `bypass`. 5: `appearance`. Nothing
/// branches on the
/// number — a missing field is covered by `#[serde(default)]` on the struct, and the version is
/// rewritten on the next save — but a bump records that an older file silently gains the defaults
/// instead of being mistaken for current.
pub const SETTINGS_VERSION: u32 = 5;

/// Old or CLI-style key -> current key, applied by [`Settings::from_json`].
const LEGACY_KEYS: &[(&str, &str)] = &[
    ("agentMode", "agent_mode"),
    ("artifactReviewPolicy", "artifact_review"),
    ("artifact_review_policy", "artifact_review"),
    ("toolPermission", "terminal_policy"),
    ("terminal_auto_execution", "terminal_policy"),
    ("allowList", "terminal_allowlist"),
    ("denyList", "terminal_denylist"),
    ("queuedMessages", "queued_messages"),
];

/// Max remembered workspaces.
pub const WORKSPACE_HISTORY_LIMIT: usize = 10;

/// Max plugin choices persisted. Discovery itself caps at [`crate::agent::plugins::MAX_PLUGINS`],
/// so anything past this could never name a plugin the user can see: a hand-edited file that
/// lists thousands of ids would otherwise be carried around and rewritten on every save.
pub const MAX_PLUGIN_CHOICES: usize = 256;

/// Max external provider profiles kept in settings. Each profile is a handful of short strings, so
/// this only bounds a hand-edited file; a person has no reason to configure more than a few.
pub const MAX_PROVIDER_PROFILES: usize = 32;

/// Most extra instruction files Bypass Mode keeps. Each one is read into every bypass request, so
/// the list is short on purpose.
pub const MAX_BYPASS_FILES: usize = 16;

/// Most bytes read from one Bypass Mode instruction file.
pub const MAX_BYPASS_FILE_BYTES: usize = 32 * 1024;

/// Largest request count Bypass Mode accepts per message.
pub const MAX_BYPASS_REQUESTS: u32 = 50;

/// Bypass Mode: the first requests of each message go to a model of the user's own choosing
/// before the conversation reverts to the model in use.
///
/// It is a routing setting, nothing more: the same tools, history and approvals apply, and the
/// conversation continues on the normal model after the count. Nothing here is hidden from the
/// user — the chat shows a permanent red band while it is on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BypassConfig {
    /// The mode is in use for new messages.
    pub enabled: bool,
    /// `ProviderProfile.id` the first requests go to; `None` keeps every request on the normal
    /// model even while `enabled` is set.
    pub provider: Option<String>,
    /// Requests sent through that profile per user message, counted from the start of the
    /// message. At least one, at most [`MAX_BYPASS_REQUESTS`].
    pub requests: u32,
    /// Markdown files whose text is added to the bypass requests' system instruction.
    pub files: Vec<PathBuf>,
}

impl Default for BypassConfig {
    fn default() -> Self {
        Self { enabled: false, provider: None, requests: 1, files: Vec::new() }
    }
}

impl BypassConfig {
    /// The mode would route: switched on, and naming a profile that still exists.
    pub fn routes(&self) -> bool {
        self.enabled && self.provider.as_deref().is_some_and(|id| !id.trim().is_empty())
    }
}

/// Why `settings.json` was not used. The app starts on the defaults and shows `message`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadProblem {
    /// What happened and where the original went, for the banner.
    pub message: String,
    /// The file is still the only copy of the user's settings (it could not be read, or could
    /// not be copied aside), so a save must not replace it.
    pub keep_file: bool,
}

/// Copy a `settings.json` that does not parse to `settings.json.<time>.bad` beside it, owner-only
/// like the original (it holds API keys).
fn set_aside(path: &Path, bytes: &[u8], why: &str) -> LoadProblem {
    let name = format!("settings.json.{}.bad", chrono::Local::now().format("%Y%m%d-%H%M%S"));
    match atomic_write_private(&path.with_file_name(&name), bytes) {
        Ok(()) => LoadProblem {
            message: format!(
                "settings.json is not valid JSON ({why}). AntiCraft started with default settings; the file was copied to {name} in the data folder."
            ),
            keep_file: false,
        },
        Err(e) => {
            tracing::warn!("could not copy settings.json aside: {e}");
            LoadProblem {
                message: format!(
                    "settings.json is not valid JSON ({why}) and could not be copied aside ({e}). AntiCraft started with default settings and leaves the file as it is; changes made now are not saved."
                ),
                keep_file: true,
            }
        }
    }
}

/// `%APPDATA%\AntiCraft\native` (falls back to the current dir if APPDATA is unknown).
pub fn default_data_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("AntiCraft")
        .join("native")
}

/// Take `{data_dir}/anticraft.lock` for the life of the process. An error of kind
/// `WouldBlock` means another AntiCraft already uses this data folder; any other error means
/// the folder cannot be locked at all (a file system without locks).
///
/// Every save writes the whole settings file from memory, so two instances on one folder undo
/// each other's changes: the last to save erases the provider profiles and keys the other added.
/// The lock goes with the process, also after a crash, so a stale file never blocks a start.
pub fn lock_data_dir(data_dir: &Path) -> std::io::Result<std::fs::File> {
    let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(data_dir.join("anticraft.lock"))?;
    file.try_lock().map_err(|e| match e {
        std::fs::TryLockError::WouldBlock => {
            std::io::Error::new(std::io::ErrorKind::WouldBlock, "another AntiCraft window is using this data folder")
        }
        std::fs::TryLockError::Error(e) => e,
    })?;
    Ok(file)
}

/// `echo` with every field `local` changed since `base` taken from `local`.
///
/// Field by field, so a change made elsewhere to one field (the workspace history, a plugin
/// choice) survives an edit of another. The UI applies it to a backend echo that arrives while
/// it has edits not sent yet; the backend applies it to a save, whose fields the UI did not touch
/// may be older than its own.
pub fn merge_unsent(base: &Settings, local: &Settings, echo: &Settings) -> Settings {
    // Fails to compile when a field is added to `Settings`: add it to the list below as well.
    let Settings {
        version: _,
        model: _,
        theme: _,
        appearance: _,
        workspace_history: _,
        auto_approve_read_tools: _,
        shell_timeout_secs: _,
        shell_output_cap_bytes: _,
        gcp_project: _,
        preview_access: _,
        latest_flash_ga: _,
        agent_mode: _,
        auto_approve_edits: _,
        artifact_review: _,
        terminal_policy: _,
        terminal_allowlist: _,
        terminal_denylist: _,
        skip_permissions: _,
        queued_messages: _,
        compact_threshold: _,
        plugins: _,
        providers: _,
        active_provider: _,
        accounts: _,
        active_account: _,
        bypass: _,
    } = base;
    let mut out = echo.clone();
    macro_rules! keep_unsent {
        ($($field:ident),* $(,)?) => {
            $(
                if local.$field != base.$field {
                    out.$field = local.$field.clone();
                }
            )*
        };
    }
    keep_unsent!(
        version,
        model,
        theme,
        appearance,
        workspace_history,
        auto_approve_read_tools,
        shell_timeout_secs,
        shell_output_cap_bytes,
        gcp_project,
        preview_access,
        latest_flash_ga,
        agent_mode,
        auto_approve_edits,
        artifact_review,
        terminal_policy,
        terminal_allowlist,
        terminal_denylist,
        skip_permissions,
        queued_messages,
        compact_threshold,
        plugins,
        providers,
        active_provider,
        accounts,
        active_account,
        bypass,
    );
    out
}

/// Parse the `providers` array one entry at a time, so an entry with an unknown kind or a
/// missing field drops only itself and the next save does not erase the other profiles.
/// `kind` accepts the spellings of [`ProviderKind::parse`] (`claude`, `gemini`, ...).
///
/// Warnings name the entry by position only: serde's message can quote the value, and an
/// entry carries an API key.
fn parse_providers(value: Option<serde_json::Value>) -> Vec<ProviderProfile> {
    let entries = match value {
        None | Some(serde_json::Value::Null) => return Vec::new(),
        Some(serde_json::Value::Array(entries)) => entries,
        Some(_) => {
            tracing::warn!("settings.json field \"providers\" is not a list, ignoring it");
            return Vec::new();
        }
    };
    // Both spellings: the profile writes camelCase, the rest of the file is snake_case.
    const KNOWN: &[&str] = &[
        "id", "label", "kind", "model", "baseUrl", "base_url", "apiKey", "api_key", "maxOutputTokens",
        "max_output_tokens", "contextWindow", "context_window", "models",
    ];
    let mut out = Vec::new();
    for (i, mut entry) in entries.into_iter().enumerate() {
        let unknown = entry.as_object().map_or(0, |o| o.keys().filter(|k| !KNOWN.contains(&k.as_str())).count());
        if unknown > 0 {
            tracing::warn!("settings.json provider #{i} has {unknown} unknown field(s); they are ignored");
        }
        if let Some(kind) = entry.get_mut("kind") {
            if let Some(parsed) = kind.as_str().and_then(ProviderKind::parse) {
                *kind = serde_json::Value::String(parsed.as_str().to_owned());
            }
        }
        match serde_json::from_value::<ProviderProfile>(entry) {
            Ok(profile) => out.push(profile),
            Err(_) => tracing::warn!("settings.json provider #{i} is invalid, dropping it"),
        }
    }
    out
}

/// One signed-in subscription account as settings remember it. Display only: never a token.
///
/// The tokens live in `<data_dir>/accounts/{id}.json`; this is the row the Account tab draws and
/// the model the next turn sends. Keeping the list here means the UI knows the account exists
/// before the backend has finished loading its file.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct AccountEntry {
    /// `^(antigravity|codex|claude)-[0-9a-f]{8}$`; also the file name under `accounts/`.
    pub id: String,
    pub provider: AccountProvider,
    /// What the row shows; falls back to [`AccountProvider::label`].
    pub label: String,
    /// The model this account sends. Never empty once sanitized.
    pub model: String,
}

/// Parse the `accounts` array one entry at a time, so an entry with an unknown provider or a
/// missing field drops only itself and the next save does not erase the other accounts.
/// `provider` accepts the spellings of [`AccountProvider::parse`] (`claude_code`, `chatgpt`, ...).
///
/// Warnings name the entry by position only: serde's message can quote the value.
fn parse_accounts(value: Option<serde_json::Value>) -> Vec<AccountEntry> {
    let entries = match value {
        None | Some(serde_json::Value::Null) => return Vec::new(),
        Some(serde_json::Value::Array(entries)) => entries,
        Some(_) => {
            tracing::warn!("settings.json field \"accounts\" is not a list, ignoring it");
            return Vec::new();
        }
    };
    const KNOWN: &[&str] = &["id", "provider", "label", "model"];
    let mut out = Vec::new();
    for (i, mut entry) in entries.into_iter().enumerate() {
        let unknown = entry.as_object().map_or(0, |o| o.keys().filter(|k| !KNOWN.contains(&k.as_str())).count());
        if unknown > 0 {
            tracing::warn!("settings.json account #{i} has {unknown} unknown field(s); they are ignored");
        }
        if let Some(provider) = entry.get_mut("provider") {
            if let Some(parsed) = provider.as_str().and_then(AccountProvider::parse) {
                *provider = serde_json::Value::String(parsed.as_str().to_owned());
            }
        }
        match serde_json::from_value::<AccountEntry>(entry) {
            Ok(entry) => out.push(entry),
            Err(_) => tracing::warn!("settings.json account #{i} is invalid, dropping it"),
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    #[default]
    Dark,
    Light,
}

/// Settings > Appearance: how wide the conversation is and which font, at which size, the
/// window's text uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Appearance {
    pub width: ConversationWidth,
    pub font: UiFont,
    pub font_size: FontSize,
}

/// Widest the reading column gets: 768 px, 960 px (the default, as in the reference), or the
/// whole card.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ConversationWidth {
    Default,
    #[default]
    Wide,
    Fill,
}

impl ConversationWidth {
    /// Column width in points, `None` for the whole card.
    pub fn max(self) -> Option<f32> {
        match self {
            ConversationWidth::Default => Some(768.0),
            ConversationWidth::Wide => Some(960.0),
            ConversationWidth::Fill => None,
        }
    }
}

/// Text font. Only System UI (Segoe UI) ships with Windows; the others are used when they are
/// installed, and AntiCraft never bundles them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum UiFont {
    #[default]
    System,
    Inter,
    GoogleSans,
}

/// Interface text size: 12, 14 (default) or 16 px body text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum FontSize {
    Small,
    #[default]
    Medium,
    Large,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Schema version, see [`SETTINGS_VERSION`].
    pub version: u32,
    pub model: String,
    pub theme: Theme,
    /// Conversation width and text font/size (Settings > Appearance).
    pub appearance: Appearance,
    /// Most recent first, deduplicated.
    pub workspace_history: Vec<PathBuf>,
    /// Read/search tools run without confirmation.
    pub auto_approve_read_tools: bool,
    pub shell_timeout_secs: u64,
    /// Max bytes of shell output returned to the model.
    pub shell_output_cap_bytes: usize,
    /// Optional user GCP project for Code Assist.
    pub gcp_project: Option<String>,
    /// The account can use preview models (`auto`/`pro` resolve to the preview pro model).
    pub preview_access: bool,
    /// Use the latest Flash GA model for `gemini-3.5-flash` (PROTOCOL 3.11 remapping).
    pub latest_flash_ga: bool,
    /// Default conversation mode (docs: Planning / Fast).
    pub agent_mode: AgentMode,
    /// Apply `write_file` / `replace` without asking (diff and hash pinning still apply).
    pub auto_approve_edits: bool,
    /// Docs: "artifact review policy".
    pub artifact_review: ArtifactReviewPolicy,
    /// Docs (legacy): "Terminal command auto execution".
    pub terminal_policy: TerminalPolicy,
    /// Commands that never ask (token prefix or `regex:`).
    pub terminal_allowlist: Vec<String>,
    /// Commands that are always rejected; wins over everything.
    pub terminal_denylist: Vec<String>,
    /// `--dangerously-skip-permissions`: auto-approve all tool calls except denylist commands.
    /// Must be an explicit opt-in by the launcher; never set by loading settings.json.
    #[serde(skip)]
    pub skip_permissions: bool,
    /// Docs: "Queued Messages" (`queue` / `send-immediately`).
    pub queued_messages: QueueMode,
    /// Compact history when the prompt reaches this fraction of the model limit (0.3..0.95).
    pub compact_threshold: f32,
    /// Per-plugin user choice, keyed by plugin id (the directory name under the plugins root).
    ///
    /// Only explicit choices are stored: a plugin the user never touched stays out of the map
    /// and follows its manifest, so a `"disabled": true` added upstream takes effect without
    /// this file claiming the user decided otherwise. `#[serde(default)]` keeps a file written
    /// before plugins existed parseable.
    #[serde(default)]
    pub plugins: BTreeMap<String, PluginChoice>,
    /// External model providers the user configured (Gemini Developer API, OpenAI-compatible,
    /// Anthropic). Empty means the IDE is on Google Code Assist, as it always was.
    ///
    /// An `api_key` sits here in cleartext: this file is the profile's only home, so the key is
    /// written with the rest of the settings (see `Settings::save`). [`ProviderProfile`]'s `Debug`
    /// redacts it, so a panic dump or a log line never carries it.
    #[serde(default)]
    pub providers: Vec<ProviderProfile>,
    /// `ProviderProfile.id` of the provider in use; `None` keeps the Google Code Assist path.
    #[serde(default)]
    pub active_provider: Option<String>,
    /// Signed-in subscription accounts, in the order the Account tab shows them.
    ///
    /// Display only, like the provider profiles' labels: the tokens are in
    /// `<data_dir>/accounts/{id}.json` and never here. An entry whose file is gone is dropped at
    /// startup, so this list and the folder agree after a restart.
    #[serde(default)]
    pub accounts: Vec<AccountEntry>,
    /// `AccountEntry.id` in use for turns; `None` keeps the Google Code Assist path (or the
    /// selected provider profile). Never set together with `active_provider`: one backend sends a
    /// turn, and which one it is must not depend on which field a screen happens to read.
    #[serde(default)]
    pub active_account: Option<String>,
    /// Bypass Mode: the first requests of each message go to a profile of the user's choosing.
    ///
    /// Separate from `active_provider` on purpose. That field says which backend sends the turn;
    /// this one says which backend sends the first few requests of a turn and then hands back.
    /// Keeping them apart means switching Bypass Mode off restores exactly the backend that was
    /// in use before, with no field to put back.
    #[serde(default)]
    pub bypass: BypassConfig,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: SETTINGS_VERSION,
            model: DEFAULT_MODEL.into(),
            theme: Theme::Dark,
            appearance: Appearance::default(),
            workspace_history: Vec::new(),
            auto_approve_read_tools: true,
            shell_timeout_secs: 120,
            shell_output_cap_bytes: 64 * 1024,
            gcp_project: None,
            preview_access: true,
            latest_flash_ga: false,
            auto_approve_edits: false,
            skip_permissions: false,
            // A new install starts on Manual (DESIGN.md 2.6): read and search tools still run
            // on their own (`auto_approve_read_tools` above), but every edit shows a diff and
            // every command asks. `AgentMode::default()` stays Planning for the code that
            // needs "the planning variant"; the settings default is the one the first chip
            // reads, and Manual is Fast.
            agent_mode: AgentMode::Fast,
            artifact_review: ArtifactReviewPolicy::default(),
            terminal_policy: TerminalPolicy::default(),
            terminal_allowlist: Vec::new(),
            terminal_denylist: vec!["rm -rf".into(), "format".into(), "git push --force".into()],
            queued_messages: QueueMode::default(),
            compact_threshold: crate::agent::context::COMPACT_THRESHOLD,
            plugins: BTreeMap::new(),
            providers: Vec::new(),
            active_provider: None,
            accounts: Vec::new(),
            active_account: None,
            bypass: BypassConfig::default(),
        }
    }
}

impl Settings {
    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join("settings.json")
    }

    /// [`Settings::load_reporting`] without the report.
    pub fn load(data_dir: &Path) -> Self {
        Self::load_reporting(data_dir).0
    }

    /// Load `settings.json`, or the defaults when it does not exist. Older or hand-edited files
    /// are migrated by [`Settings::from_json`]: unknown keys are ignored and a field with an
    /// invalid value falls back to its default without discarding the rest.
    ///
    /// A file that exists but cannot be read or parsed also gives the defaults, plus a
    /// [`LoadProblem`] for the banner: the next save rewrites the file from those defaults, and
    /// the provider profiles and keys in it would be gone. A file that does not parse is first
    /// copied to `settings.json.<time>.bad`; one that cannot be read cannot be copied, so the
    /// problem asks the caller not to overwrite it.
    pub fn load_reporting(data_dir: &Path) -> (Self, Option<LoadProblem>) {
        let path = Self::path(data_dir);
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (Self::default(), None),
            Err(e) => {
                tracing::warn!("settings.json could not be read, using defaults: {e}");
                let message = format!(
                    "settings.json could not be read ({e}). AntiCraft started with default settings and leaves the file as it is; changes made now are not saved."
                );
                return (Self::default(), Some(LoadProblem { message, keep_file: true }));
            }
        };
        // Windows PowerShell 5.1 writes UTF-8 with a byte order mark, which serde_json refuses.
        let text = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes);
        // serde_json's syntax errors name a position, never the text there (a key, perhaps).
        let parsed = match serde_json::from_slice::<serde_json::Value>(text) {
            Ok(v) if v.is_object() => Ok(v),
            Ok(_) => Err("it is not a JSON object".to_owned()),
            Err(e) => Err(e.to_string()),
        };
        match parsed {
            Ok(v) => (Self::from_json(v), None),
            Err(why) => {
                tracing::warn!("settings.json is invalid, using defaults: {why}");
                (Self::default(), Some(set_aside(&path, &bytes, &why)))
            }
        }
    }

    /// Migrate a settings document of any earlier version, then sanitize. Missing policy
    /// fields (version 0 files) take their defaults, including the built-in denylist.
    pub fn from_json(mut v: serde_json::Value) -> Self {
        let Some(obj) = v.as_object_mut() else {
            tracing::warn!("settings.json is not an object, using defaults");
            return Self::default();
        };
        // Legacy / CLI-style key names.
        for (old, new) in LEGACY_KEYS {
            if let Some(val) = obj.remove(*old) {
                obj.entry(*new).or_insert(val);
            }
        }
        // Lists written as one comma or newline separated string.
        for key in ["terminal_allowlist", "terminal_denylist"] {
            if let Some(serde_json::Value::String(text)) = obj.get(key) {
                let list: Vec<serde_json::Value> = text
                    .split([',', '\n'])
                    .map(str::trim)
                    .filter(|r| !r.is_empty())
                    .map(|r| serde_json::Value::String(r.to_owned()))
                    .collect();
                obj.insert(key.to_owned(), serde_json::Value::Array(list));
            }
        }
        // One element of the wrong type fails its whole field in the per-field pass below, and
        // the field's default replaces every entry (the built-in denylist, an empty plugin map).
        // Drop just that element.
        for key in ["terminal_allowlist", "terminal_denylist", "workspace_history"] {
            if let Some(serde_json::Value::Array(items)) = obj.get_mut(key) {
                let before = items.len();
                items.retain(serde_json::Value::is_string);
                if items.len() < before {
                    tracing::warn!("settings.json field {key:?}: dropped {} entries that are not text", before - items.len());
                }
            }
        }
        if let Some(serde_json::Value::Object(choices)) = obj.get_mut("plugins") {
            let before = choices.len();
            choices.retain(|_, choice| serde_json::from_value::<PluginChoice>(choice.clone()).is_ok());
            if choices.len() < before {
                tracing::warn!("settings.json field \"plugins\": dropped {} invalid choices", before - choices.len());
            }
        }
        // Profiles are parsed per entry, before the per-field pass below: as one field, a single
        // bad entry would fail the whole list. That pass's warning can also quote the value, and
        // a profile carries an API key.
        let providers = parse_providers(obj.remove("providers"));
        // Accounts share the reason: one bad entry must not wipe the list, and the per-field pass
        // below would drop the whole array for it.
        let accounts = parse_accounts(obj.remove("accounts"));
        // Drop each field that does not deserialize on its own, keeping the others.
        let keys: Vec<String> = obj.keys().cloned().collect();
        for k in keys {
            let mut one = serde_json::Map::new();
            one.insert(k.clone(), obj[&k].clone());
            if let Err(e) = serde_json::from_value::<Self>(serde_json::Value::Object(one)) {
                tracing::warn!("settings.json field {k:?} is invalid, using its default: {e}");
                obj.remove(&k);
            }
        }
        let mut settings = serde_json::from_value::<Self>(v).unwrap_or_default();
        settings.providers = providers;
        settings.accounts = accounts;
        settings.sanitized()
    }

    /// Policy snapshot for `mode` (a conversation's mode may differ from the default).
    pub fn approval_policy(&self, mode: AgentMode) -> ApprovalPolicy {
        ApprovalPolicy {
            mode,
            review: self.artifact_review,
            terminal: self.terminal_policy,
            auto_approve_read_tools: self.auto_approve_read_tools,
            auto_approve_edits: self.auto_approve_edits,
            allowlist: self.terminal_allowlist.clone(),
            denylist: self.terminal_denylist.clone(),
            skip_permissions: self.skip_permissions,
        }
    }

    /// The permission mode the message box chip shows, derived from the four policy fields.
    ///
    /// `Turbo` without `skip_permissions` is not one of the five modes; it reads as
    /// [`PermissionMode::Auto`], so the chip never claims less than the policy does (a bare
    /// Turbo runs everything the denylist allows, which is Auto's floor with edits). [`Self::load`]
    /// already normalizes that state to `Auto`; a struct built in memory is the only way to get
    /// here. A `Planning` conversation with a permissive terminal or edits reads as
    /// [`PermissionMode::Auto`], not Plan: the chip must never name a stricter mode than the
    /// policy in force (Plan's promise is that commands and edits ask). `artifact_review` is a
    /// separate setting and does not change the mode.
    pub fn permission_mode(&self) -> PermissionMode {
        self.permission_mode_for(self.agent_mode)
    }

    /// [`Self::permission_mode`] for a conversation whose agent mode may differ from the
    /// default (`/fast` or a model-chip pick on a conversation that never picked a mode).
    /// The terminal, edit and skip fields still come from this `Settings`, which is where the
    /// backend's policy reads them too.
    pub fn permission_mode_for(&self, agent_mode: AgentMode) -> PermissionMode {
        match (self.skip_permissions, agent_mode, self.terminal_policy, self.auto_approve_edits) {
            (true, ..) => PermissionMode::Bypass,
            (false, AgentMode::Planning, TerminalPolicy::Off, false) => PermissionMode::Plan,
            // Turbo is Auto's floor plus edits, so it is never reported as the milder
            // EditAuto or Manual. Auto without edits runs commands by itself with no edit
            // auto-apply, which no mode names; Auto describes it without overstating the rest.
            // A Planning conversation lands here too when its terminal or edits are permissive.
            (false, _, TerminalPolicy::Turbo | TerminalPolicy::Auto, _) => PermissionMode::Auto,
            (false, _, TerminalPolicy::Off, true) => PermissionMode::EditAuto,
            (false, _, TerminalPolicy::Off, false) => PermissionMode::Manual,
        }
    }

    /// Write the four policy fields of `m`.
    ///
    /// Bypass is stored as Auto plus `skip_permissions`: the two fields a save writes
    /// (`auto_approve_edits`, `terminal_policy`) are Auto's, so a restart reads Auto and runs
    /// commands under Auto. Its permissive half lives only in `skip_permissions`, which carries
    /// `#[serde(skip)]` and is never written. Storing Turbo with edits on would let a stricter
    /// looking chip hide an unlisted-command policy that outlived the process.
    pub fn set_permission_mode(&mut self, m: PermissionMode) {
        self.agent_mode = m.agent_mode();
        self.auto_approve_edits = m.auto_approve_edits();
        // Bypass persists as Auto (see above); its accessor stays Turbo for the live policy.
        self.terminal_policy = if m == PermissionMode::Bypass { TerminalPolicy::Auto } else { m.terminal() };
        self.skip_permissions = m.skip_permissions();
    }

    /// Clamp out-of-range values to something usable.
    pub fn sanitized(mut self) -> Self {
        // Turbo without the skip flag is not one of the modes the chip offers: it is the stored
        // half of a Bypass pick (or a legacy file's "always proceed"). Left as is, a restart would
        // read Auto-looking fields while every unlisted command ran unattended, so it becomes
        // Auto. Bypass itself is `skip_permissions`, which a file never sets.
        if self.terminal_policy == TerminalPolicy::Turbo && !self.skip_permissions {
            self.terminal_policy = TerminalPolicy::Auto;
        }
        if self.model.trim().is_empty() {
            self.model = DEFAULT_MODEL.into();
        }
        self.shell_timeout_secs = self.shell_timeout_secs.clamp(1, 3600);
        // Clamp to a usable window. A hand-edited or corrupt value of 0 would discard all
        // output and an enormous one would let a single shell command buffer gigabytes.
        self.shell_output_cap_bytes =
            self.shell_output_cap_bytes.clamp(MIN_SHELL_OUTPUT_CAP_BYTES, MAX_SHELL_OUTPUT_CAP_BYTES);
        let mut seen = Vec::new();
        self.workspace_history.retain(|p| {
            let keep = !p.as_os_str().is_empty() && !seen.contains(p);
            if keep {
                seen.push(p.clone());
            }
            keep
        });
        self.workspace_history.truncate(WORKSPACE_HISTORY_LIMIT);
        self.gcp_project = self.gcp_project.filter(|p| !p.trim().is_empty());
        if !self.compact_threshold.is_finite() {
            self.compact_threshold = crate::agent::context::COMPACT_THRESHOLD;
        }
        self.compact_threshold = self.compact_threshold.clamp(0.3, 0.95);
        for list in [&mut self.terminal_allowlist, &mut self.terminal_denylist] {
            let mut seen: Vec<String> = Vec::new();
            list.retain_mut(|r| {
                *r = r.trim().to_owned();
                let key = r.to_lowercase();
                let keep = validate_rule(r).is_ok() && !seen.contains(&key);
                if keep {
                    seen.push(key);
                }
                keep
            });
            list.truncate(MAX_RULES);
        }
        // Plugin ids are directory names written into a map keyed by them: trim, drop what is
        // left empty, and cap the count. Keys come from a sorted map, so which ones the cap
        // keeps is deterministic and does not depend on how the file was ordered.
        let mut plugins = BTreeMap::new();
        for (id, choice) in std::mem::take(&mut self.plugins) {
            let id = id.trim();
            if id.is_empty() || plugins.len() >= MAX_PLUGIN_CHOICES {
                continue;
            }
            plugins.insert(id.to_owned(), choice);
        }
        self.plugins = plugins;
        // Provider profiles: same cap-and-drop shape as the lists above, in file order (the list
        // is ordered, and the UI shows them in that order). A profile missing an id, a label or a
        // key, or carrying an unusable base URL, could only fail at request time with an error the
        // UI renders as an authentication failure, so it is dropped here instead. One without a
        // model is kept: its models are loaded and one is picked after it is added.
        // `validate()`'s message is user-facing and never quotes the key.
        let mut seen: Vec<String> = Vec::new();
        let mut kept = 0usize;
        self.providers.retain_mut(|p| {
            p.id = p.id.trim().to_owned();
            p.label = p.label.trim().to_owned();
            p.model = p.model.trim().to_owned();
            p.models = crate::providers::models::normalize(std::mem::take(&mut p.models));
            if p.id.is_empty() || kept >= MAX_PROVIDER_PROFILES || seen.contains(&p.id) {
                return false;
            }
            // A profile can only exist with a kind a person can pick. The account-only kinds
            // (`antigravity`, `codex`) are reached through an account, never through this list, so
            // a hand-edited file must not be able to pretend that an API key is a subscription.
            if p.kind.is_account_only() {
                return false;
            }
            if p.validate().is_err() {
                return false;
            }
            seen.push(p.id.clone());
            kept += 1;
            true
        });
        self.providers.truncate(MAX_PROVIDER_PROFILES);
        // A selection that names no surviving profile falls back to Code Assist rather than
        // leaving the IDE on a backend that cannot be built.
        self.active_provider = self
            .active_provider
            .take()
            .map(|id| id.trim().to_owned())
            .filter(|id| !id.is_empty() && self.providers.iter().any(|p| p.id == *id));
        // Account entries: the same cap-and-drop shape, but the id is a file name, so it must be
        // `valid_id` before anything joins it into a path. A label or a model that trims empty
        // falls back rather than dropping the row: the account exists (its file is what proves
        // that), and a row with no model would only fail at request time.
        let mut seen_accounts: Vec<String> = Vec::new();
        self.accounts.retain_mut(|a| {
            a.id = a.id.trim().to_owned();
            a.label = a.label.trim().to_owned();
            a.model = a.model.trim().to_owned();
            if !external::valid_id(&a.id) || seen_accounts.len() >= external::MAX_ACCOUNTS || seen_accounts.contains(&a.id) {
                return false;
            }
            if a.label.is_empty() {
                a.label = a.provider.label().to_owned();
            }
            if a.model.is_empty() {
                // `default_models` is non-empty by construction, so this cannot index past the end.
                a.model = a.provider.spec().default_models[0].to_owned();
            }
            seen_accounts.push(a.id.clone());
            true
        });
        self.accounts.truncate(external::MAX_ACCOUNTS);
        self.active_account = self
            .active_account
            .take()
            .map(|id| id.trim().to_owned())
            .filter(|id| !id.is_empty() && self.accounts.iter().any(|a| a.id == *id));
        // The one place the exclusivity rule lives. `active_provider_profile()` then keeps
        // meaning "a key-based profile is active", so its existing callers need no new branch.
        if self.active_account.is_some() {
            self.active_provider = None;
        }
        // Bypass Mode names a profile, so it is cleaned the same way: an id no profile has falls
        // back to "switched on, nothing to route to", which the UI explains instead of leaving a
        // mode that silently sends nothing.
        self.bypass.provider = self
            .bypass
            .provider
            .take()
            .map(|id| id.trim().to_owned())
            .filter(|id| !id.is_empty() && self.providers.iter().any(|p| p.id == *id));
        self.bypass.requests = self.bypass.requests.clamp(1, MAX_BYPASS_REQUESTS);
        // Instruction files: no blanks, no duplicates, and a generous cap that a hand-edited file
        // cannot use to read the whole disk into every request.
        let mut seen_files: Vec<PathBuf> = Vec::new();
        self.bypass.files.retain(|p| {
            let keep = !p.as_os_str().is_empty() && !seen_files.contains(p) && seen_files.len() < MAX_BYPASS_FILES;
            if keep {
                seen_files.push(p.clone());
            }
            keep
        });
        self.version = SETTINGS_VERSION;
        self
    }

    /// The account `active_account` names, if it still exists.
    pub fn active_account_entry(&self) -> Option<&AccountEntry> {
        let id = self.active_account.as_deref()?.trim();
        self.accounts.iter().find(|a| a.id == id)
    }

    /// Use `id` for turns. Returns whether anything changed.
    ///
    /// An id that names no entry changes nothing: switching to an account the file no longer has
    /// would leave the next turn with no backend at all.
    pub fn select_account(&mut self, id: &str) -> bool {
        let id = id.trim();
        if id.is_empty() || !self.accounts.iter().any(|a| a.id == id) {
            return false;
        }
        let changed = self.active_account.as_deref() != Some(id) || self.active_provider.is_some();
        self.active_account = Some(id.to_owned());
        self.active_provider = None;
        changed
    }

    /// Use a provider profile for turns (or Code Assist, with `None`). Returns whether anything
    /// changed. An unknown id is refused, exactly as [`Self::select_account`] refuses one.
    pub fn select_profile(&mut self, id: Option<String>) -> bool {
        let id = id.map(|i| i.trim().to_owned()).filter(|i| !i.is_empty());
        if let Some(id) = &id {
            if !self.providers.iter().any(|p| p.id == *id) {
                return false;
            }
        }
        let changed = self.active_provider != id || self.active_account.is_some();
        self.active_provider = id;
        self.active_account = None;
        changed
    }

    /// Back to the Google Code Assist path: neither a profile nor an account sends turns.
    /// Returns whether anything changed.
    pub fn select_code_assist(&mut self) -> bool {
        let changed = self.active_provider.is_some() || self.active_account.is_some();
        self.active_provider = None;
        self.active_account = None;
        changed
    }

    /// The profile `active_provider` names, if it still exists. Callers use this instead of
    /// indexing so a stale id simply means "use Code Assist".
    pub fn active_provider_profile(&self) -> Option<&ProviderProfile> {
        let id = self.active_provider.as_deref()?.trim();
        self.providers.iter().find(|p| p.id == id)
    }

    /// Write `settings.json` owner-only, like the OAuth credential files: it holds the provider
    /// profiles' API keys, and a plain write would leave it readable by other accounts.
    pub fn save(&self, data_dir: &Path) -> std::io::Result<()> {
        let json = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        atomic_write_private(&Self::path(data_dir), &json)
    }

    /// Move `ws` to the front of the history, deduplicated and capped.
    pub fn remember_workspace(&mut self, ws: &Path) {
        self.workspace_history.retain(|p| p != ws);
        self.workspace_history.insert(0, ws.to_path_buf());
        self.workspace_history.truncate(WORKSPACE_HISTORY_LIMIT);
    }

    pub fn current_workspace(&self) -> Option<&Path> {
        self.workspace_history.first().map(PathBuf::as_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_history() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Settings::load(dir.path()), Settings::default());
        let mut s = Settings::default();
        s.remember_workspace(Path::new("C:/a"));
        s.remember_workspace(Path::new("C:/b"));
        s.remember_workspace(Path::new("C:/a"));
        assert_eq!(s.workspace_history, vec![PathBuf::from("C:/a"), PathBuf::from("C:/b")]);
        s.save(dir.path()).unwrap();
        assert_eq!(Settings::load(dir.path()), s);
    }

    #[test]
    fn partial_file_uses_defaults() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(Settings::path(dir.path()), br#"{"model":"gemini-3.5-flash"}"#).unwrap();
        let s = Settings::load(dir.path());
        assert_eq!(s.model, "gemini-3.5-flash");
        assert_eq!(s.shell_timeout_secs, 120);
    }

    #[test]
    fn corrupt_and_out_of_range() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(Settings::path(dir.path()), b"garbage").unwrap();
        assert_eq!(Settings::load(dir.path()), Settings::default());
        std::fs::write(
            Settings::path(dir.path()),
            br#"{"model":"","theme":"light","shell_timeout_secs":0,"gcp_project":" ","workspace_history":["C:/a","C:/a",""]}"#,
        )
        .unwrap();
        let s = Settings::load(dir.path());
        assert_eq!(s.model, DEFAULT_MODEL);
        assert_eq!(s.theme, Theme::Light);
        assert_eq!(s.shell_timeout_secs, 1);
        assert_eq!(s.gcp_project, None);
        assert_eq!(s.workspace_history, vec![PathBuf::from("C:/a")]);
    }

    #[test]
    fn shell_output_cap_is_bounded() {
        let cap = |v: usize| Settings { shell_output_cap_bytes: v, ..Settings::default() }.sanitized().shell_output_cap_bytes;
        assert_eq!(cap(usize::MAX), MAX_SHELL_OUTPUT_CAP_BYTES);
        assert_eq!(cap(0), MIN_SHELL_OUTPUT_CAP_BYTES);
        assert_eq!(cap(4096), 4096);
        assert_eq!(Settings::default().sanitized().shell_output_cap_bytes, 64 * 1024);
    }

    #[test]
    fn aliases_resolve_to_concrete_ids() {
        assert_eq!(resolve_model("auto", true, false), PREVIEW_PRO_MODEL);
        assert_eq!(resolve_model("pro", false, false), DEFAULT_MODEL);
        assert_eq!(resolve_model("auto", false, false), DEFAULT_MODEL);
        assert_eq!(resolve_model("flash", true, false), PREVIEW_FLASH_MODEL);
        assert_eq!(resolve_model("flash", false, false), BASE_FLASH_MODEL);
        assert_eq!(resolve_model("flash", true, true), LATEST_FLASH_MODEL);
        assert_eq!(resolve_model("flash-lite", true, false), BASE_FLASH_LITE_MODEL);
        assert_eq!(resolve_model(" gemini-2.5-pro ", true, false), "gemini-2.5-pro");
        for m in MODEL_CHOICES {
            assert_ne!(resolve_model(m, true, false), "auto");
        }
    }

    #[test]
    fn backend_remaps_flash() {
        assert_eq!(backend_model_id("gemini-3.5-flash", false), "gemini-3-flash");
        assert_eq!(backend_model_id("gemini-3.5-flash", true), "gemini-3.8-flash");
        assert_eq!(backend_model_id("gemini-3-flash", true), "gemini-3.8-flash");
        assert_eq!(backend_model_id("gemini-2.5-pro", true), "gemini-2.5-pro");
    }

    #[test]
    fn history_is_capped() {
        let mut s = Settings::default();
        for i in 0..(WORKSPACE_HISTORY_LIMIT + 5) {
            s.remember_workspace(Path::new(&format!("C:/w{i}")));
        }
        assert_eq!(s.workspace_history.len(), WORKSPACE_HISTORY_LIMIT);
        assert_eq!(s.current_workspace(), Some(Path::new(&format!("C:/w{}", WORKSPACE_HISTORY_LIMIT + 4))));
    }

    #[test]
    fn migrates_v0_file_without_policy_fields() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            Settings::path(dir.path()),
            br#"{"model":"gemini-3.5-flash","auto_approve_read_tools":false,"shell_timeout_secs":30}"#,
        )
        .unwrap();
        let s = Settings::load(dir.path());
        assert_eq!(s.version, SETTINGS_VERSION);
        assert_eq!(s.model, "gemini-3.5-flash");
        assert!(!s.auto_approve_read_tools);
        assert_eq!(s.terminal_policy, TerminalPolicy::Off);
        assert_eq!(s.artifact_review, ArtifactReviewPolicy::RequestReview);
        assert_eq!(s.terminal_denylist, Settings::default().terminal_denylist);
        s.save(dir.path()).unwrap();
        assert_eq!(Settings::load(dir.path()), s);
    }

    #[test]
    fn bad_field_keeps_the_rest() {
        let s = Settings::from_json(serde_json::json!({
            "model": "gemini-3-pro-preview",
            "terminal_policy": "nonsense",
            "artifact_review": 7,
            "shell_timeout_secs": 45,
            "unknown_future_key": true,
        }));
        assert_eq!(s.model, "gemini-3-pro-preview");
        assert_eq!(s.shell_timeout_secs, 45);
        assert_eq!(s.terminal_policy, TerminalPolicy::Off);
        assert_eq!(s.artifact_review, ArtifactReviewPolicy::RequestReview);
        assert_eq!(Settings::from_json(serde_json::json!([1, 2])), Settings::default());
    }

    #[test]
    fn legacy_keys_and_values() {
        let s = Settings::from_json(serde_json::json!({
            "agentMode": "plan",
            "toolPermission": "always-proceed",
            "artifact_review_policy": "always_proceed",
            "allowList": "cargo test, git status\n  ",
            "denyList": ["rm -rf", "RM -RF", "regex:(", "  ", "format"],
            "queuedMessages": "send-immediately",
        }));
        assert_eq!(s.agent_mode, AgentMode::Planning);
        // The legacy spelling still parses to Turbo on its own; a loaded file cannot keep it
        // (see `sanitized`), because Turbo without the skip flag would run unlisted commands
        // while the chip said Auto.
        let raw: Settings = serde_json::from_value(serde_json::json!({"terminal_policy": "always-proceed"})).unwrap();
        assert_eq!(raw.terminal_policy, TerminalPolicy::Turbo);
        assert_eq!(s.terminal_policy, TerminalPolicy::Auto);
        assert_eq!(Settings::from_json(serde_json::json!({"toolPermission": "always-proceed"})).terminal_policy, TerminalPolicy::Auto);
        assert_eq!(s.artifact_review, ArtifactReviewPolicy::AlwaysProceed);
        assert_eq!(s.terminal_allowlist, vec!["cargo test".to_owned(), "git status".to_owned()]);
        assert_eq!(s.terminal_denylist, vec!["rm -rf".to_owned(), "format".to_owned()]);
        assert_eq!(s.queued_messages, QueueMode::SendImmediately);
        // An explicit empty denylist stays empty (the user removed the defaults).
        let s = Settings::from_json(serde_json::json!({"terminal_denylist": []}));
        assert!(s.terminal_denylist.is_empty());
    }

    #[test]
    fn plugin_choices_parse_missing_and_are_sanitized() {
        // A file written before plugins existed has no key at all.
        let s = Settings::from_json(serde_json::json!({"model": "gemini-3.5-flash"}));
        assert!(s.plugins.is_empty());
        let s = Settings::from_json(serde_json::json!({
            "plugins": {"  kept  ": "enabled", "": "disabled", "other": "disabled"}
        }));
        assert_eq!(
            s.plugins,
            BTreeMap::from([
                ("kept".to_owned(), PluginChoice::Enabled),
                ("other".to_owned(), PluginChoice::Disabled),
            ])
        );
        // The cap bounds a hand-edited file without dropping the rest of the settings.
        let many: BTreeMap<String, PluginChoice> =
            (0..(MAX_PLUGIN_CHOICES + 10)).map(|i| (format!("p{i:04}"), PluginChoice::Enabled)).collect();
        let s = Settings { plugins: many, ..Settings::default() }.sanitized();
        assert_eq!(s.plugins.len(), MAX_PLUGIN_CHOICES);
        assert!(s.plugins.contains_key("p0000"));
        assert!(!s.plugins.contains_key(&format!("p{:04}", MAX_PLUGIN_CHOICES + 9)));
    }

    fn profile(id: &str) -> ProviderProfile {
        ProviderProfile {
            id: id.into(),
            label: format!("Provider {id}"),
            kind: crate::providers::ProviderKind::OpenAi,
            model: "gpt-4o-mini".into(),
            base_url: None,
            api_key: "sk-test-key".into(),
            max_output_tokens: Some(4096),
            context_window: None,
            models: Vec::new(),
        }
    }

    #[test]
    fn provider_profiles_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Settings { providers: vec![profile("p1"), profile("p2")], ..Settings::default() };
        s.active_provider = Some("p2".into());
        let s = s.sanitized();
        s.save(dir.path()).unwrap();
        let loaded = Settings::load(dir.path());
        assert_eq!(loaded, s);
        assert_eq!(loaded.providers.len(), 2);
        assert_eq!(loaded.active_provider.as_deref(), Some("p2"));
        assert_eq!(loaded.active_provider_profile().map(|p| p.id.as_str()), Some("p2"));
        // The profile's own spelling of the key is what a person would hand-edit.
        let raw: serde_json::Value = serde_json::from_slice(&std::fs::read(Settings::path(dir.path())).unwrap()).unwrap();
        assert_eq!(raw["providers"][0]["apiKey"], "sk-test-key");
        assert_eq!(raw["providers"][0]["kind"], "openai");
        assert_eq!(raw["active_provider"], "p2");
    }

    #[test]
    fn provider_profiles_are_sanitized() {
        // A file written before providers existed has neither key.
        let s = Settings::from_json(serde_json::json!({"model": "gemini-3.5-flash"}));
        assert!(s.providers.is_empty());
        assert_eq!(s.active_provider, None);
        assert_eq!(s.active_provider_profile(), None);

        let mut dup = profile("p1");
        dup.label = "second".into();
        let mut no_key = profile("p2");
        no_key.api_key = "  ".into();
        let mut bad_url = profile("p3");
        bad_url.base_url = Some("ftp://example.com".into());
        let mut empty_id = profile("   ");
        empty_id.label = "  ".into();
        let mut spaced = profile("  p4  ");
        spaced.model = "  gpt-4o  ".into();
        let s = Settings {
            providers: vec![profile("p1"), dup, no_key, bad_url, empty_id, spaced],
            active_provider: Some("  p1  ".into()),
            ..Settings::default()
        }
        .sanitized();
        assert_eq!(s.providers.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(), ["p1", "p4"]);
        assert_eq!(s.providers[0].label, "Provider p1", "the first of a duplicate id wins");
        assert_eq!(s.providers[1].model, "gpt-4o", "ids and models are trimmed");
        assert_eq!(s.active_provider.as_deref(), Some("p1"));

        // A selection whose profile did not survive falls back to Code Assist.
        let s = Settings {
            providers: vec![profile("p1")],
            active_provider: Some("gone".into()),
            ..Settings::default()
        }
        .sanitized();
        assert_eq!(s.active_provider, None);
        assert_eq!(s.active_provider_profile(), None);
        let s = Settings { providers: Vec::new(), active_provider: Some("p1".into()), ..Settings::default() }.sanitized();
        assert_eq!(s.active_provider, None);
    }

    /// A profile saved before its model was chosen is kept (it sends no turn until one is), and its
    /// loaded list is kept as clean as the list a load produces.
    #[test]
    fn a_profile_may_have_no_model_and_keeps_a_clean_list() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = profile("p1");
        p.model = "   ".into();
        p.models = vec![" claude-opus-5-5 ".into(), "claude-opus-5-5".into(), "has space".into(), String::new(), "\u{202e}x".into(), "claude-haiku-5-5".into()];
        let s = Settings { providers: vec![p], active_provider: Some("p1".into()), ..Settings::default() }.sanitized();
        assert_eq!(s.providers.len(), 1);
        assert_eq!(s.providers[0].model, "");
        assert_eq!(s.providers[0].models, ["claude-opus-5-5", "claude-haiku-5-5"]);
        assert_eq!(s.active_provider.as_deref(), Some("p1"), "it can be in use: picking its model is how it gets one");
        s.save(dir.path()).unwrap();
        assert_eq!(Settings::load(dir.path()), s, "the list round-trips");
        // A file written before lists existed has none; a profile without one writes none.
        let raw: serde_json::Value = serde_json::from_slice(&std::fs::read(Settings::path(dir.path())).unwrap()).unwrap();
        assert_eq!(raw["providers"][0]["models"], serde_json::json!(["claude-opus-5-5", "claude-haiku-5-5"]));
        let s = Settings::from_json(serde_json::json!({"providers": [profile_json("p2", "openai")]}));
        assert!(s.providers[0].models.is_empty());
        let mut bare = Settings { providers: vec![profile("p3")], ..Settings::default() }.sanitized();
        bare.save(dir.path()).unwrap();
        let raw: serde_json::Value = serde_json::from_slice(&std::fs::read(Settings::path(dir.path())).unwrap()).unwrap();
        assert!(raw["providers"][0].get("models").is_none());
        bare.providers[0].models = (0..crate::providers::models::MAX_MODELS + 5).map(|i| format!("m{i}")).collect();
        assert_eq!(bare.sanitized().providers[0].models.len(), crate::providers::models::MAX_MODELS);
    }

    #[test]
    fn provider_profiles_are_capped() {
        let many: Vec<ProviderProfile> = (0..(MAX_PROVIDER_PROFILES + 8)).map(|i| profile(&format!("p{i:03}"))).collect();
        let s = Settings { providers: many, active_provider: Some("p039".into()), ..Settings::default() }.sanitized();
        assert_eq!(s.providers.len(), MAX_PROVIDER_PROFILES);
        assert_eq!(s.providers[0].id, "p000");
        assert_eq!(s.active_provider, None, "the selection past the cap did not survive");
        let s = Settings { providers: (0..(MAX_PROVIDER_PROFILES + 8)).map(|i| profile(&format!("p{i:03}"))).collect(), active_provider: Some("p001".into()), ..Settings::default() }.sanitized();
        assert_eq!(s.active_provider.as_deref(), Some("p001"));
    }

    #[test]
    fn version_one_file_keeps_loading_and_gains_the_provider_defaults() {
        let s = Settings::from_json(serde_json::json!({"version": 1, "model": "gemini-3.5-flash"}));
        assert_eq!(s.version, SETTINGS_VERSION);
        assert_eq!(s.model, "gemini-3.5-flash");
        assert!(s.providers.is_empty());
        assert_eq!(s.active_provider, None);
        // A file with an unusable providers array keeps every other field.
        let s = Settings::from_json(serde_json::json!({"model": "gemini-3.5-flash", "providers": 7}));
        assert_eq!(s.model, "gemini-3.5-flash");
        assert!(s.providers.is_empty());
        let s = Settings::from_json(serde_json::json!({"model": "gemini-3.5-flash", "providers": null}));
        assert_eq!(s.model, "gemini-3.5-flash");
        assert!(s.providers.is_empty());
    }

    fn profile_json(id: &str, kind: &str) -> serde_json::Value {
        serde_json::json!({"id": id, "label": format!("Provider {id}"), "kind": kind, "model": "m", "apiKey": format!("k-{id}")})
    }

    #[test]
    fn one_bad_provider_entry_drops_only_itself() {
        // A kind written by a newer build, a missing key, and a non-object entry: each one is
        // dropped on its own, and the profiles around them survive.
        let mut no_key = profile_json("b", "openai");
        no_key.as_object_mut().unwrap().remove("apiKey");
        let s = Settings::from_json(serde_json::json!({
            "model": "gemini-3.5-flash",
            "providers": [profile_json("a", "openai"), profile_json("x", "mistral"), no_key, 7, profile_json("c", "anthropic")],
            "active_provider": "c",
        }));
        assert_eq!(s.providers.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(), ["a", "c"]);
        assert_eq!(s.active_provider.as_deref(), Some("c"), "the selection survives a bad sibling");
        assert_eq!(s.model, "gemini-3.5-flash");
        assert!(s.providers.iter().all(|p| p.api_key == format!("k-{}", p.id)), "keys are kept");

        // Hand-edited kind spellings that `ProviderKind::parse` accepts are read, not dropped.
        let s = Settings::from_json(serde_json::json!({
            "providers": [profile_json("a", "claude"), profile_json("b", " Gemini "), profile_json("c", "openai-compatible")],
            "active_provider": "b",
        }));
        assert_eq!(
            s.providers.iter().map(|p| p.kind).collect::<Vec<_>>(),
            [ProviderKind::Anthropic, ProviderKind::GeminiApi, ProviderKind::OpenAi]
        );
        assert_eq!(s.active_provider.as_deref(), Some("b"));
    }

    #[test]
    fn a_bad_provider_entry_does_not_erase_the_others_on_save() {
        let dir = tempfile::tempdir().unwrap();
        let file = serde_json::json!({
            "providers": [profile_json("a", "openai"), profile_json("x", "claude-next"), profile_json("c", "claude")],
            "active_provider": "a",
        });
        std::fs::write(Settings::path(dir.path()), serde_json::to_vec(&file).unwrap()).unwrap();
        let loaded = Settings::load(dir.path());
        // Any later change rewrites the file from what was loaded.
        loaded.save(dir.path()).unwrap();
        let reloaded = Settings::load(dir.path());
        assert_eq!(reloaded.providers.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(), ["a", "c"]);
        assert_eq!(reloaded.providers[1].kind, ProviderKind::Anthropic);
        assert_eq!(reloaded.active_provider.as_deref(), Some("a"));
        assert_eq!(reloaded, loaded);
    }

    /// Files in `dir` whose name ends with `.bad`.
    fn set_aside(dir: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.ends_with(".bad")))
            .collect()
    }

    #[test]
    fn a_file_that_does_not_parse_is_copied_aside_before_the_defaults_are_used() {
        let dir = tempfile::tempdir().unwrap();
        // A trailing comma from a hand edit: serde_json refuses the whole file.
        let original = br#"{"model":"gemini-3.5-flash","providers":[{"id":"a","label":"A","kind":"openai","model":"m","apiKey":"k-a"}],}"#;
        std::fs::write(Settings::path(dir.path()), original).unwrap();
        assert_eq!(Settings::load(dir.path()), Settings::default());
        let kept = set_aside(dir.path());
        assert_eq!(kept.len(), 1, "one copy of the file is kept before the next save replaces it");
        assert!(std::fs::read(&kept[0]).unwrap() == original, "the copy holds the original bytes");
    }

    #[test]
    fn a_byte_order_mark_does_not_discard_the_file() {
        // Windows PowerShell 5.1 `Set-Content -Encoding UTF8` writes one.
        let dir = tempfile::tempdir().unwrap();
        let mut bytes = b"\xEF\xBB\xBF".to_vec();
        bytes.extend_from_slice(br#"{"model":"gemini-3.5-flash"}"#);
        std::fs::write(Settings::path(dir.path()), bytes).unwrap();
        assert_eq!(Settings::load(dir.path()).model, "gemini-3.5-flash");
        assert!(set_aside(dir.path()).is_empty());
    }

    #[test]
    fn provider_keys_spelled_like_the_rest_of_the_file_are_read() {
        // Every other key in settings.json is snake_case, so a hand-added profile often is too.
        let s = Settings::from_json(serde_json::json!({
            "providers": [
                {"id": "gw", "label": "Gateway", "kind": "openai", "model": "m", "api_key": "k-gw", "base_url": "https://gateway.example/v1", "max_output_tokens": 2048},
            ],
        }));
        assert_eq!(s.providers.len(), 1, "a snake_case api_key does not drop the profile");
        assert_eq!(s.providers[0].base_url.as_deref(), Some("https://gateway.example/v1"), "the key is not sent to the kind's default host");
        assert_eq!(s.providers[0].max_output_tokens, Some(2048));
        assert!(s.providers[0].api_key == "k-gw");
        // Saved back in the profile's own spelling, so the next load reads the same profile.
        let raw = serde_json::to_value(&s).unwrap();
        assert_eq!(raw["providers"][0]["baseUrl"], "https://gateway.example/v1");
        assert_eq!(Settings::from_json(raw), s);
    }

    #[test]
    fn one_element_of_the_wrong_type_drops_only_itself() {
        let s = Settings::from_json(serde_json::json!({
            "terminal_allowlist": ["cargo test", 7],
            "terminal_denylist": ["rm -rf", "git reset --hard", 5],
            "workspace_history": ["C:/a", null],
            "plugins": {"a": "enabled", "b": true},
        }));
        assert_eq!(s.terminal_allowlist, vec!["cargo test".to_owned()]);
        assert_eq!(s.terminal_denylist, vec!["rm -rf".to_owned(), "git reset --hard".to_owned()], "the user's rules stay, not the built-in list");
        assert_eq!(s.workspace_history, vec![PathBuf::from("C:/a")]);
        assert_eq!(s.plugins, BTreeMap::from([("a".to_owned(), PluginChoice::Enabled)]));
    }

    #[test]
    fn a_second_instance_cannot_take_the_data_folder() {
        let dir = tempfile::tempdir().unwrap();
        let first = lock_data_dir(dir.path()).expect("the first instance takes the folder");
        let second = lock_data_dir(dir.path()).map(|_| ()).expect_err("a second one is refused");
        assert_eq!(second.kind(), std::io::ErrorKind::WouldBlock, "told apart from a folder that cannot be locked");
        drop(first);
        assert!(lock_data_dir(dir.path()).is_ok(), "the folder is free again once the first exits");
    }

    /// The five modes are the combinations the UI offers; the others collapse onto the nearest
    /// one, as the cases below show, and writing a mode and reading it back gives the same mode.
    #[test]
    fn permission_modes_round_trip() {
        // A new install derives Manual: Fast, no auto-edits, commands ask. Planning is one
        // explicit mode (Plan), not the default.
        assert_eq!(Settings::default().permission_mode(), PermissionMode::Manual);
        assert_eq!(Settings { agent_mode: AgentMode::Planning, ..Settings::default() }.permission_mode(), PermissionMode::Plan);
        for m in [PermissionMode::Manual, PermissionMode::EditAuto, PermissionMode::Plan, PermissionMode::Auto, PermissionMode::Bypass] {
            let mut s = Settings::default();
            // Start from a mode far from `m`, so the write has to set every field.
            s.set_permission_mode(if m == PermissionMode::Bypass { PermissionMode::Manual } else { PermissionMode::Bypass });
            s.set_permission_mode(m);
            assert_eq!(s.permission_mode(), m, "{m:?}");
            assert_eq!(s.agent_mode, m.agent_mode(), "{m:?}");
            assert_eq!(s.auto_approve_edits, m.auto_approve_edits(), "{m:?}");
            // Bypass stores Auto's terminal fields: the skip flag alone carries what it adds.
            let stored_terminal = if m == PermissionMode::Bypass { TerminalPolicy::Auto } else { m.terminal() };
            assert_eq!(s.terminal_policy, stored_terminal, "{m:?}");
            assert_eq!(s.skip_permissions, m.skip_permissions(), "{m:?}");
        }
        // Turbo without the skip flag is not a mode: in memory it reads as Auto, never as the
        // milder-looking Manual or EditAuto. `sanitized` turns it into Auto, so a file cannot
        // hold it either.
        let turbo = Settings { agent_mode: AgentMode::Fast, terminal_policy: TerminalPolicy::Turbo, ..Settings::default() };
        assert_eq!(turbo.permission_mode(), PermissionMode::Auto);
        assert_eq!(Settings { auto_approve_edits: true, ..turbo }.permission_mode(), PermissionMode::Auto);
        let stored = Settings { agent_mode: AgentMode::Fast, terminal_policy: TerminalPolicy::Turbo, ..Settings::default() }.sanitized();
        assert_eq!(stored.terminal_policy, TerminalPolicy::Auto, "a stored Turbo cannot outlive a restart");
    }

    /// Bypass is `skip_permissions`, and that field is never read from the file: a settings.json
    /// asking for it (hand-edited, or a launcher's own copy) starts in Manual, not Bypass.
    #[test]
    fn a_file_cannot_turn_on_bypass() {
        let s = Settings::from_json(serde_json::json!({"skip_permissions": true, "model": "gemini-3.5-flash"}));
        assert!(!s.skip_permissions);
        assert_ne!(s.permission_mode(), PermissionMode::Bypass);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(Settings::path(dir.path()), br#"{"skip_permissions":true}"#).unwrap();
        let s = Settings::load(dir.path());
        assert_ne!(s.permission_mode(), PermissionMode::Bypass);
        // A mode stored through the setter comes back the same, Bypass excepted: what it saves
        // is Auto's fields, so the reloaded file reads as Auto and runs commands under Auto; the
        // skip flag that made it Bypass is not written.
        let mut s = Settings::default();
        s.set_permission_mode(PermissionMode::Auto);
        s.save(dir.path()).unwrap();
        assert_eq!(Settings::load(dir.path()).permission_mode(), PermissionMode::Auto);
        s.set_permission_mode(PermissionMode::Bypass);
        s.save(dir.path()).unwrap();
        let reloaded = Settings::load(dir.path());
        assert_eq!(reloaded.permission_mode(), PermissionMode::Auto, "Bypass is not written");
        assert_ne!(reloaded.terminal_policy, TerminalPolicy::Turbo, "the skip flag's permissiveness is not stored");
        assert!(!reloaded.skip_permissions);
        // The live policy of a pick is still Bypass, before and after the save.
        assert_eq!(s.permission_mode(), PermissionMode::Bypass);
    }

    #[test]
    fn approval_policy_snapshot() {
        let s = Settings {
            terminal_policy: TerminalPolicy::Turbo,
            terminal_allowlist: vec!["cargo".into()],
            ..Settings::default()
        };
        let p = s.approval_policy(AgentMode::Fast);
        assert_eq!(p.mode, AgentMode::Fast);
        assert_eq!(p.terminal, TerminalPolicy::Turbo);
        assert_eq!(p.allowlist, s.terminal_allowlist);
        assert!(matches!(p.decide_command("git push --force"), crate::agent::policy::Decision::Deny(_)));
    }

    fn entry(id: &str, provider: AccountProvider) -> AccountEntry {
        AccountEntry { id: id.into(), provider, label: format!("Account {id}"), model: "m".into() }
    }

    #[test]
    fn settings_keep_account_and_profile_selection_exclusive() {
        let account = entry("claude-0123abcd", AccountProvider::ClaudeCode);
        let mut s = Settings { accounts: vec![account.clone()], providers: vec![profile("p1")], ..Settings::default() };
        assert_eq!(s.active_account, None);
        assert_eq!(s.active_account_entry(), None);

        assert!(s.select_account(&account.id));
        assert_eq!(s.active_account.as_deref(), Some("claude-0123abcd"));
        assert_eq!(s.active_provider, None, "selecting an account clears the profile");
        assert_eq!(s.active_account_entry().map(|a| a.id.as_str()), Some("claude-0123abcd"));
        assert!(!s.select_account(&account.id), "selecting again changes nothing");
        assert!(!s.select_account("codex-eeeeeeee"), "an id that names no entry changes nothing");

        assert!(s.select_profile(Some("p1".into())));
        assert_eq!(s.active_provider.as_deref(), Some("p1"));
        assert_eq!(s.active_account, None, "selecting a profile clears the account");
        assert!(!s.select_profile(Some("gone".into())), "an unknown profile id is refused");

        s.select_account(&account.id);
        assert!(s.select_code_assist());
        assert_eq!((s.active_provider.as_deref(), s.active_account.as_deref()), (None, None));
        assert!(!s.select_code_assist());

        // A hand-edited file cannot have both: `sanitized` is the single place the rule lives.
        let s = Settings {
            accounts: vec![account],
            providers: vec![profile("p1")],
            active_provider: Some("p1".into()),
            active_account: Some("claude-0123abcd".into()),
            ..Settings::default()
        }
        .sanitized();
        assert_eq!(s.active_account.as_deref(), Some("claude-0123abcd"));
        assert_eq!(s.active_provider, None);
        assert_eq!(s.active_provider_profile(), None, "so a key-based profile is not 'active'");
        // The other direction alone keeps the profile selection.
        let s = Settings { providers: vec![profile("p1")], active_provider: Some("p1".into()), ..Settings::default() }
            .sanitized();
        assert_eq!(s.active_provider.as_deref(), Some("p1"));
        assert_eq!(s.active_account, None);
    }

    #[test]
    fn settings_drop_accounts_with_bad_ids_and_cap_the_list() {
        let mut entries: Vec<AccountEntry> = (0..10)
            .map(|i| entry(&format!("claude-{i:08x}"), AccountProvider::ClaudeCode))
            .collect();
        // A duplicate id, and an id that is a path: neither may survive.
        let mut bad = entry("antigravity-aaaaaaaa", AccountProvider::Antigravity);
        bad.label = "  ".into();
        bad.model = "  ".into();
        entries.push(bad.clone());
        entries.push(bad);
        entries.push(entry("../x", AccountProvider::Codex));
        let s = Settings { accounts: entries, ..Settings::default() }.sanitized();
        assert_eq!(s.accounts.len(), external::MAX_ACCOUNTS, "capped");
        assert!(s.accounts.iter().all(|a| external::valid_id(&a.id)), "{:?}", s.accounts);
        assert!(!s.accounts.iter().any(|a| a.id == "../x"));

        // Trimmed, and an empty label/model falls back instead of dropping the row.
        let mut a = entry(" antigravity-bbbbbbbb ", AccountProvider::Antigravity);
        a.label = "  ".into();
        a.model = String::new();
        let s = Settings { accounts: vec![a], active_account: Some("  antigravity-bbbbbbbb  ".into()), ..Settings::default() }
            .sanitized();
        assert_eq!(s.accounts[0].id, "antigravity-bbbbbbbb");
        assert_eq!(s.accounts[0].label, "Antigravity");
        assert_eq!(s.accounts[0].model, AccountProvider::Antigravity.spec().default_models[0]);
        assert_eq!(s.active_account.as_deref(), Some("antigravity-bbbbbbbb"), "trimmed before it is matched");

        // A selected account that did not survive falls back to Code Assist.
        let s = Settings {
            accounts: vec![entry("claude-0123abcd", AccountProvider::ClaudeCode)],
            active_account: Some("../x".into()),
            ..Settings::default()
        }
        .sanitized();
        assert_eq!(s.active_account, None);
    }

    #[test]
    fn one_bad_account_entry_drops_only_itself() {
        let good = |id: &str| serde_json::json!({"id": id, "provider": "claude", "label": "L", "model": "claude-sonnet-4-6"});
        let s = Settings::from_json(serde_json::json!({
            "model": "gemini-3.5-flash",
            "accounts": [good("claude-0123abcd"), 7, {"id": "x", "provider": "mistral", "label": "L", "model": "m"}, good("codex-deadbeef")],
            "active_account": "codex-deadbeef",
        }));
        assert_eq!(s.accounts.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(), ["claude-0123abcd", "codex-deadbeef"]);
        assert_eq!(s.active_account.as_deref(), Some("codex-deadbeef"), "the selection survives a bad sibling");
        assert_eq!(s.model, "gemini-3.5-flash");

        // Hand-edited provider spellings that `AccountProvider::parse` accepts are read.
        let s = Settings::from_json(serde_json::json!({
            "accounts": [
                {"id": "claude-11111111", "provider": "claude_code", "label": "A", "model": "m"},
                {"id": "codex-22222222", "provider": "ChatGPT", "label": "B", "model": "m"},
            ],
        }));
        assert_eq!(
            s.accounts.iter().map(|a| a.provider).collect::<Vec<_>>(),
            [AccountProvider::ClaudeCode, AccountProvider::Codex]
        );
        // A file written before accounts existed, and a nonsense value for the field.
        assert!(Settings::from_json(serde_json::json!({"model": "x"})).accounts.is_empty());
        assert!(Settings::from_json(serde_json::json!({"accounts": 7})).accounts.is_empty());
    }

    #[test]
    fn accounts_round_trip_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let s = Settings {
            accounts: vec![entry("claude-0123abcd", AccountProvider::ClaudeCode), entry("codex-deadbeef", AccountProvider::Codex)],
            active_account: Some("codex-deadbeef".into()),
            ..Settings::default()
        }
        .sanitized();
        assert_eq!(s.version, SETTINGS_VERSION);
        s.save(dir.path()).unwrap();
        assert_eq!(Settings::load(dir.path()), s);
        let raw: serde_json::Value = serde_json::from_slice(&std::fs::read(Settings::path(dir.path())).unwrap()).unwrap();
        assert_eq!(raw["version"], SETTINGS_VERSION);
        assert_eq!(raw["accounts"][0]["provider"], "claude");
        assert_eq!(raw["active_account"], "codex-deadbeef");
        // Nothing token-shaped is written here: the file holds display data only.
        assert!(raw["accounts"][0].get("access_token").is_none());
    }

    #[test]
    fn account_only_provider_kinds_cannot_be_added_by_hand() {
        // The kinds exist only as an account; a profile claiming one would be sent with an API
        // key against a backend that wants a subscription token.
        let s = Settings::from_json(serde_json::json!({
            "providers": [profile_json("a", "antigravity"), profile_json("b", "codex"), profile_json("c", "openai")],
            "active_provider": "b",
        }));
        assert_eq!(s.providers.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(), ["c"]);
        assert_eq!(s.active_provider, None);
    }

    #[test]
    fn merge_unsent_keeps_an_unsent_account_selection() {
        let base = Settings::default();
        let mut local = base.clone();
        local.accounts = vec![entry("claude-0123abcd", AccountProvider::ClaudeCode)];
        local.active_account = Some("claude-0123abcd".into());
        // The backend's echo was written before the UI's edit.
        let echo = Settings::default();
        let merged = merge_unsent(&base, &local, &echo);
        assert_eq!(merged.accounts, local.accounts);
        assert_eq!(merged.active_account, local.active_account);
        // A field neither side changed stays as the echo has it.
        assert_eq!(merged.model, echo.model);
    }

    #[test]
    fn bypass_settings_round_trip_and_are_sanitized() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Settings { providers: vec![profile("p1")], ..Settings::default() };
        s.bypass.enabled = true;
        s.bypass.provider = Some("p1".into());
        s.bypass.requests = 3;
        s.bypass.files = vec![PathBuf::from("C:/rules/a.md")];
        let s = s.sanitized();
        s.save(dir.path()).unwrap();
        assert_eq!(Settings::load(dir.path()), s);
        assert!(s.bypass.routes());
        let raw: serde_json::Value = serde_json::from_slice(&std::fs::read(Settings::path(dir.path())).unwrap()).unwrap();
        assert_eq!(raw["version"], SETTINGS_VERSION);
        assert_eq!(raw["bypass"]["requests"], 3);

        // A file written before the mode existed, and a nonsense value for it.
        let s = Settings::from_json(serde_json::json!({"model": "gemini-3.5-flash"}));
        assert!(!s.bypass.enabled && s.bypass.requests == 1 && s.bypass.files.is_empty());
        let s = Settings::from_json(serde_json::json!({"bypass": 7}));
        assert_eq!(s.bypass, BypassConfig::default());
    }

    /// The mode never names a profile that does not exist, never asks for zero requests, and
    /// never carries a duplicate or blank file path.
    #[test]
    fn bypass_settings_are_clamped() {
        let s = Settings {
            providers: vec![profile("p1")],
            bypass: BypassConfig {
                enabled: true,
                provider: Some("  gone  ".into()),
                requests: 0,
                files: vec![PathBuf::from(""), PathBuf::from("C:/a.md"), PathBuf::from("C:/a.md")],
            },
            ..Settings::default()
        }
        .sanitized();
        assert_eq!(s.bypass.provider, None, "an id no profile has is dropped");
        assert!(!s.bypass.routes(), "so the mode silently does nothing, which the UI explains");
        assert_eq!(s.bypass.requests, 1);
        assert_eq!(s.bypass.files, vec![PathBuf::from("C:/a.md")]);
        let s = Settings { providers: vec![profile("p1")], bypass: BypassConfig { provider: Some("p1".into()), requests: 99, ..BypassConfig::default() }, ..Settings::default() }
            .sanitized();
        assert_eq!(s.bypass.requests, MAX_BYPASS_REQUESTS);
        let many: Vec<PathBuf> = (0..(MAX_BYPASS_FILES + 4)).map(|i| PathBuf::from(format!("C:/f{i}.md"))).collect();
        let s = Settings { bypass: BypassConfig { files: many, ..BypassConfig::default() }, ..Settings::default() }.sanitized();
        assert_eq!(s.bypass.files.len(), MAX_BYPASS_FILES);
    }

    /// `settings.json` holds API keys, so it gets the credential files' owner-only ACL.
    #[cfg(windows)]
    #[test]
    fn settings_file_acl_is_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        Settings::default().save(dir.path()).unwrap();
        let out = std::process::Command::new("icacls").arg(Settings::path(dir.path())).output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout).to_lowercase();
        // Exactly one ACE, not inherited (no "(i)").
        let aces: Vec<_> = text.lines().filter(|l| l.contains(":(")).collect();
        assert_eq!(aces.len(), 1, "{text}");
        assert!(!aces[0].contains("(i)"), "{text}");
    }

    #[cfg(unix)]
    #[test]
    fn settings_file_mode_is_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        Settings::default().save(dir.path()).unwrap();
        assert_eq!(std::fs::metadata(Settings::path(dir.path())).unwrap().permissions().mode() & 0o777, 0o600);
    }
}
