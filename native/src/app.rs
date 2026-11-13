//! Glue between the egui UI and the async backend (auth, Code Assist client, agent, store).
//!
//! The UI thread never blocks: it sends [`AppCommand`]s over an unbounded channel and drains
//! [`AppEvent`]s with `try_recv` each frame. The backend runs on the tokio runtime.
//!
//! A turn runs on exactly one backend, chosen in this order: a signed-in subscription account
//! (`Settings::active_account`), a provider profile (`Settings::active_provider`), else Google
//! Code Assist. `Settings::sanitized` keeps the first two exclusive, so which backend sends a turn
//! never depends on which field a screen happens to read.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::agent::artifacts::ArtifactMeta;
use crate::agent::mcp::{self, McpHub, McpServerConfig};
use crate::agent::plugins::{self, Plugin, PluginChoice, PluginSource};
use crate::agent::policy::{AgentMode, SharedPolicy};
use crate::agent::{
    context, prompt, Agent, AgentConfig, AgentEvent, FinishReason, History, SharedModel, SteerHandle, ToolLimits, ToolRegistry, Workspace,
};
use crate::api::{ApiConfig, CodeAssistClient, ModelBackend};
use crate::auth::antigravity::{self, ImportCheck, ImportSummary, ImportedTokenSource, SearchRoots, SessionInfo, Watch};
use crate::auth::external::auth::ExpiredSink;
use crate::auth::external::login::LoginAttempt;
use crate::auth::external::registry::Accounts;
use crate::auth::external::{self, AccountProvider, AccountStatus, AccountView};
use crate::auth::{AuthConfig, AuthError, AuthManager, AuthStatus, ClientSource, CredentialStore, TokenProvider};
use crate::providers::{AccountMeta, ProviderBackend, ProviderProfile};
use crate::settings::{AccountEntry, Settings};
use crate::store::{Conversation, ConversationStore, ConversationSummary};

/// Startup options resolved from CLI flags.
#[derive(Debug, Clone)]
pub struct AppOptions {
    pub data_dir: PathBuf,
    pub workspace: Option<PathBuf>,
    pub model: Option<String>,
    /// `--mode`: applied on top of settings.json for this run.
    pub mode: Option<crate::ExecutionMode>,
    /// `--dangerously-skip-permissions`: auto-approve every tool call except denylisted commands.
    pub skip_permissions: bool,
}

/// UI -> backend.
#[derive(Debug)]
#[allow(dead_code)] // `Retry`, `OpenArtifact` and `ResolveFileChange` have no UI entry point yet
pub enum AppCommand {
    Login,
    CancelLogin,
    Logout,
    NewChat,
    OpenConversation(Uuid),
    RenameConversation(Uuid, String),
    DeleteConversation(Uuid),
    /// Open the native folder picker (async, off the UI thread).
    PickWorkspace,
    SetWorkspace(PathBuf),
    SetModel(String),
    /// The UI's settings edits. `base` is the copy they were made from: only the fields that
    /// differ from it are applied, so a field the backend changed meanwhile (a plugin choice,
    /// the workspace history) is not written back from the UI's older copy. `seq` numbers the
    /// saves; every `AppEvent::Settings` carries the last one applied. Boxed: two inline copies
    /// would make every command this large.
    SaveSettings { seq: u64, base: Box<Settings>, settings: Box<Settings> },
    /// Route turns through one of `Settings.providers` (`Some(id)`), or back to Google Code
    /// Assist (`None`). Unknown ids are refused with an error; the profiles themselves are saved
    /// through [`AppCommand::SaveSettings`].
    SetActiveProvider(Option<String>),
    /// "Load models" in the provider dialog: list what `profile`'s endpoint offers, with its key.    /// The profile may not be saved yet (the Add dialog), so it travels whole. Answered with
    /// [`AppEvent::ProviderModels`] carrying the same `request`.
    LoadProviderModels { request: u64, profile: ProviderProfile },
    /// Picker click on a provider's model: set the profile's model and put the profile in use.
    UseProviderModel { profile: String, model: String },
    /// Bypass Mode's "Add .md file…": open a native file picker for instruction files. Answered
    /// with [`AppEvent::BypassFiles`] carrying what was chosen (nothing when it was cancelled).
    PickBypassFiles,
    SendPrompt(String),
    /// Stop the in-flight turn.
    Stop,
    Shutdown,
    /// Internal: a login task finished (`id` identifies the attempt).
    LoginFinished(u64, AuthStatus),
    /// Internal: a logout task finished.
    LogoutFinished(Option<String>, AuthStatus),
    /// Store the OAuth client secret in `<data_dir>/oauth_client.json` and rebuild auth.
    SetOAuthClientSecret(String),
    /// Delete the stored client secret (falls back to the env var, if set).
    ClearOAuthClientSecret,
    /// Look again for a Google session another tool saved on this computer (startup looks once).
    CheckImportedSession,
    /// Use the session the last look found for Code Assist. Refused while signed in with
    /// AntiCraft's own credentials and while a sign-in runs.
    UseImportedSession,
    /// Stop using the imported session. The other app's file and sign-in are left as they are.
    StopImportedSession,
    /// Internal: the imported session of this generation was renewed or ended.
    ImportedSessionChanged(u64, Watch),
    // --- Orchestration / chat control (ORCHESTRATION.md D, E).
    /// Busy: append to the backend queue; runs after the current turn ends normally.
    QueuePrompt(String),
    /// Busy: inject into the running turn at its next model request.
    SteerPrompt(String),
    /// Move one queued prompt into the running turn ("Steer" on its chip); when the turn no
    /// longer takes it, the prompt runs next. Nothing happens when the entry already left the
    /// queue: it ran, or was removed.
    SteerQueued(u64),
    /// Drop one queued prompt by id (`None`: all).
    ClearQueue(Option<u64>),
    /// Internal: a turn ended normally, run the first queued prompt. Sent through the command
    /// channel rather than run at once, so chip actions the user sent before it come first.
    RunNextQueued,
    /// Send with a one-shot mode override (`/plan`, `/fast`).
    SendPromptWithMode(String, crate::agent::policy::AgentMode),
    /// Set the conversation mode chip.
    SetAgentMode(crate::agent::policy::AgentMode),
    /// Truncate history before the last user message and send `text` instead. The mode is the
    /// one-shot `/plan` or `/fast` the edited prompt was sent with, so editing does not lose it.
    EditLastPrompt(String, Option<crate::agent::policy::AgentMode>),
    /// Re-run the last user message (history truncated to before it).
    Retry,
    /// Summarize older history now (`/compact`).
    Compact,
    /// Reset the open conversation's history (`/clear`).
    ClearHistory,
    /// Restore the conversation to the checkpoint taken before user message `user_index`.
    /// The agent is torn down; the next turn starts fresh with the restored history.
    RewindCheckpoint(usize),
    /// Build the `@` mention file index for the current workspace (off the UI thread).
    IndexWorkspace,
    /// Reload workflows for the `/` popup.
    LoadWorkflows,
    /// Load one artifact's content.
    OpenArtifact(String),
    /// Accept or reject one edited file from the change list (`false`: restore baseline).
    ResolveFileChange { path: PathBuf, accept: bool },
    /// Switch one plugin on or off (`id` is the plugin's directory name).
    SetPluginChoice(String, PluginChoice),
    /// Re-read the MCP configuration and reconnect every server.
    ReloadMcp,
    /// Internal: async MCP connection round finished. Carries the reload it belongs to, the new
    /// hub and the server statuses.
    McpConnected(u64, Option<Arc<McpHub>>, Vec<(String, bool, usize)>),
    // --- Subscription accounts (Customizations > Account > Accounts).
    /// Start a subscription sign-in. Refused while a turn runs or another sign-in (Google or
    /// account) is in flight.
    SignInAccount(AccountProvider),
    /// Cancel the attempt with this id. An id that no longer matches cancels nothing, so a Cancel
    /// drawn for an earlier attempt cannot stop a newer one.
    CancelAccountLogin(u64),
    /// Finish the retained attempt `login` with a redirect URL the user pasted (built from the
    /// text field with `.into()`).
    PasteAccountLink { login: u64, link: PastedLink },
    /// Drop the retained attempt `id` (the paste popup's Cancel, and after a success).
    ForgetAccountLogin(u64),
    /// Sign out of this account: its file is deleted and it is no longer listed. Refused while a
    /// turn runs.
    SignOutAccount(String),
    /// "Use" on an account row: turns go to this account.
    UseAccount(String),
    /// Picker click: activate the account and set its model.
    UseAccountModel { account: String, model: String },
    /// Change the account's model only; which backend is in use stays as it is.
    SetAccountModel { account: String, model: String },
    /// "Check again" in the models popup.
    CheckAccountModels(String),
    /// Internal: an attempt finished. `Ok` carries the view; `Err` the message shown on the row.
    AccountLoginFinished(u64, AccountProvider, Result<AccountView, String>),
    /// Internal: `CheckAccountModels` finished.
    AccountModelsFetched(String, Result<Vec<String>, String>),
    /// Internal: a refresh found this account's session dead (`invalid_grant`).
    AccountExpired(String),
}

/// Backend -> UI.
#[derive(Debug)]
#[allow(dead_code)]
pub enum AppEvent {
    Auth(AuthStatus),
    Conversations(Vec<ConversationSummary>),
    /// Conversation opened or created; `messages` is the stored history.
    ConversationLoaded(Conversation),
    WorkspaceChanged(Option<PathBuf>),
    /// The settings in effect, after the `SaveSettings` numbered by the first field (0: none
    /// yet). Launch flags are included; they are not written to settings.json.
    Settings(u64, Settings),
    /// Untagged agent event (used by unit tests to drive the UI state directly).
    #[cfg_attr(not(test), allow(dead_code))]
    Agent(AgentEvent),
    /// Agent event of a running turn, tagged with its conversation. The UI drops events for
    /// a conversation that is no longer shown (only `Finished` still clears the busy state).
    TurnEvent { conversation: Uuid, event: AgentEvent },
    /// The prompt was not started (no workspace, not logged in, ...). Clears the busy state.
    TurnRejected(String),
    /// Non-agent error for the banner. Never changes the busy state.
    Error(String),
    /// Where the OAuth client secret comes from (`None`: not configured). Never the value.
    OAuthClientSource(Option<ClientSource>),
    /// What the last look for another tool's Google session found. Holds no token.
    ImportCheck(ImportCheck),
    /// The imported session in use changed. Holds no token.
    ImportedSession(ImportedSessionEvent),
    // --- Orchestration / chat control (ORCHESTRATION.md D, E).
    /// Backend queue contents after a change: (id, text).
    Queue(Vec<(u64, String)>),
    /// Workspace-relative paths for the `@` popup.
    WorkspaceIndex(Vec<String>),
    /// Workflows for the `/` popup.
    Workflows(Vec<crate::agent::context::Workflow>),
    /// Artifact content loaded on request.
    Artifact { conversation: Uuid, name: String, content: String },
    /// Files changed in the current conversation (change list with accept/reject).
    FileChanges(Vec<FileChange>),
    /// The backend started a prompt on its own (next queued message, or a steer that
    /// arrived after the turn ended). The UI shows the bubble and goes busy.
    PromptStarted { conversation: Uuid, text: String },
    /// Discovered plugins, in id order.
    Plugins(Vec<PluginInfo>),
    /// `(server, connected, tool count)` for every configured MCP server.
    McpStatus(Vec<(String, bool, usize)>),
    // --- Subscription accounts.
    /// Every account settings lists, newest state, in the Account tab's order. Holds no token.
    ///
    /// While a sign-in runs, the list ends with one more entry for its provider: an empty `id` and
    /// [`AccountStatus::SigningIn`]. No account exists yet (which one it becomes is known only
    /// when it finishes), so that provider's Sign in row shows the spinner and Cancel; the
    /// attempt's id for Cancel is in [`AppEvent::AccountLogin`].
    Accounts(Vec<AccountView>),
    /// Popup state for one account. Carries the whole state so a stale `true` cannot survive a
    /// dropped event: the UI assigns it, it does not merge it.
    AccountModels { id: String, state: AccountModelsState },
    /// The answer to [`AppCommand::LoadProviderModels`] with this `request`.
    ProviderModels { request: u64, result: Result<crate::providers::models::LoadedModels, String> },
    /// The `.md` files picked for Bypass Mode's instruction list. Empty when the picker was
    /// cancelled; the UI adds them and saves.
    BypassFiles(Vec<PathBuf>),
    /// The sign-in attempt the paste popup belongs to. `Some` while one runs, and after one
    /// stopped without signing in (`error` says why, `None` after a cancel) so a pasted redirect
    /// link can still finish it; `None` once it signed in or was dismissed.
    AccountLogin { login: Option<AccountLoginInfo> },
}

/// One discovered plugin, flattened for the UI.
///
/// Kept separate from [`Plugin`] because that type owns canonical paths and parsed MCP
/// documents the customizations page never reads, and discovery runs on another thread: the
/// event carries only what is displayed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PluginInfo {
    pub id: String,
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub enabled: bool,
    /// `"workspace"` or `"global"`: which discovery root found it.
    pub source: String,
    pub skill_count: usize,
    /// Number of MCP servers the plugin declares (configured, not necessarily connected).
    pub mcp_servers: usize,
}

impl From<&Plugin> for PluginInfo {
    fn from(p: &Plugin) -> Self {
        Self {
            id: p.id.clone(),
            name: p.name.clone(),
            version: p.version.clone(),
            description: p.description.clone(),
            enabled: p.enabled,
            source: match p.source {
                PluginSource::Workspace => "workspace".to_owned(),
                PluginSource::Global => "global".to_owned(),
            },
            skill_count: p.skill_dirs.len(),
            // Parsing is bounded and skips broken entries, so a malformed plugin config just
            // counts zero rather than failing discovery.
            mcp_servers: p.mcp_servers().len(),
        }
    }
}

/// A change of the imported session (Customizations > Account). Holds no token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportedSessionEvent {
    /// In use for Code Assist. Sent when it starts and whenever its token is replaced.
    Active(SessionInfo),
    /// No longer used: the user stopped it, or AntiCraft's own sign-in took over.
    Stopped,
    /// Nothing valid is left; the text says why and what to do.
    Ended(String),
}

/// One account sign-in attempt, as the Account tab needs it: its id (for Cancel and for a pasted
/// link) and the page to open. Never a token, never the PKCE verifier.
#[derive(Clone, PartialEq, Eq)]
pub struct AccountLoginInfo {
    pub id: u64,
    pub provider: AccountProvider,
    /// The attempt's authorization page, to open or copy.
    pub auth_url: String,
    /// Why the browser round trip stopped; `None` while it runs and after a cancel. Either way a
    /// pasted redirect link can still finish the attempt.
    pub error: Option<String>,
}

/// Hand-written: the authorization URL carries the attempt's `state`, which `LoginAttempt`'s own
/// `Debug` hides as well.
impl std::fmt::Debug for AccountLoginInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountLoginInfo")
            .field("id", &self.id)
            .field("provider", &self.provider)
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

/// Popup state of one account's model list. The backend keeps one per account id and always
/// sends it whole ([`AppEvent::AccountModels`]), so the UI never has to merge it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AccountModelsState {
    /// A fetch is running: the refresh button is disabled and shows a spinner.
    pub checking: bool,
    /// The last fetch's error, shown inside the popup. Cached chips stay visible.
    pub error: Option<String>,
}

/// A redirect URL pasted to finish a sign-in.
///
/// It carries an authorization code, so its `Debug` never prints it: [`AppCommand`] derives
/// `Debug`, and a code that reaches a log can be exchanged by whoever reads it in time
/// (Antigravity's flow has no PKCE verifier to stop that). Built from the text field with
/// `.into()`.
#[derive(Clone, PartialEq, Eq)]
pub struct PastedLink(String);

impl PastedLink {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for PastedLink {
    fn from(link: String) -> Self {
        Self(link)
    }
}

impl From<&str> for PastedLink {
    fn from(link: &str) -> Self {
        Self(link.to_owned())
    }
}

impl std::fmt::Debug for PastedLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PastedLink(<redacted>)")
    }
}

/// Shows a sign-in's authorization page. It is handed the attempt as well, so a test's fake
/// browser can answer on the port that attempt bound.
type AccountOpener = Arc<dyn Fn(&LoginAttempt, url::Url) + Send + Sync>;

/// The app's opener: the default browser. `url::Url` is not `AsRef<OsStr>`, so `open::that` is
/// given its string.
fn open_in_browser(_: &LoginAttempt, url: url::Url) {
    if let Err(e) = open::that(url.as_str()) {
        tracing::warn!("could not open browser: {e}");
    }
}

/// A session reused from another Google tool's credential file.
struct ImportedSession {
    tokens: Arc<ImportedTokenSource>,
    /// Matches the watcher's reports to this session.
    generation: u64,
    /// Stops the watcher task.
    stop: CancellationToken,
}

/// One entry of the change list.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub struct FileChange {
    pub path: PathBuf,
    pub added: usize,
    pub removed: usize,
    pub is_new: bool,
}

/// Channels held by the UI.
pub struct UiChannels {
    pub commands: mpsc::UnboundedSender<AppCommand>,
    pub events: mpsc::UnboundedReceiver<AppEvent>,
}

/// Completion report from a spawned turn task.
#[derive(Debug)]
struct TurnDone {
    turn_id: u64,
    conversation_id: Uuid,
    /// Conversation as it was when the prompt was sent (fallback when it is not current).
    snapshot: Option<Conversation>,
    /// Final history when the agent reported `Finished`.
    history: Option<Vec<crate::api::types::Content>>,
    /// Why the turn ended (`None`: no `Finished` was seen).
    reason: Option<FinishReason>,
    /// Artifact metadata of the agent after the turn (persisted with the conversation).
    artifacts: Option<Vec<ArtifactMeta>>,
    /// Steering text that arrived too late for the turn (it runs as the next prompt, so a
    /// steer is never lost even when the UI already showed it as sent).
    steer: Vec<String>,
    /// Artifacts an edit/retry rolled back: the store must delete the files written by the
    /// dropped turn.
    restore: Vec<ArtifactMeta>,
}

/// What a turn task runs.
#[derive(Debug)]
enum TurnKind {
    /// Plain prompt; a leading `/plan` or `/fast` is honored by the agent.
    Prompt(String),
    PromptWithMode(String, AgentMode),
    /// Replace the last user prompt, keeping its one-shot mode.
    Edit(String, Option<AgentMode>),
    Retry,
    Compact,
}

/// Backend state owned by the tokio task.
pub struct Backend {
    opts: AppOptions,
    settings: Settings,
    auth: Option<Arc<AuthManager>>,
    client: Option<CodeAssistClient>,
    /// Shared HTTP client, reused when auth is rebuilt.
    http: reqwest::Client,
    client_source: Option<ClientSource>,
    store: ConversationStore,
    current: Option<Conversation>,
    agent: Option<Arc<Mutex<Agent>>>,
    /// Model handle of `agent`; changed without taking the agent lock.
    agent_model: Option<SharedModel>,
    /// The backend `agent` was built for. A turn still running on it after a switch keeps a model
    /// of that backend; `None` only for an agent a test installed by hand.
    agent_sends_to: Option<TurnBackend>,
    /// Steering sender of `agent` (taken at build time: a running turn holds the lock).
    agent_steer: Option<SteerHandle>,
    /// Live approval policy shared with every agent; rebuilt from settings + `conv_mode`.
    policy: SharedPolicy,
    /// Mode of the open conversation (composer chip; defaults to `settings.agent_mode`).
    conv_mode: AgentMode,
    /// Discovered plugins, in id order. Empty until `run` discovers them and when there are none.
    plugins: Vec<Plugin>,
    /// Connected MCP servers. `None` until a reconnect ran and when no server contributes a tool.
    mcp: Option<Arc<McpHub>>,
    /// Modes chosen per conversation this session (the composer chip is per conversation).
    modes: HashMap<Uuid, AgentMode>,
    /// Messages queued while a turn runs: (id, text). Belongs to the open conversation.
    queue: VecDeque<(u64, String)>,
    queue_seq: u64,
    /// Conversations deleted while a turn may still finish; their results are not saved.
    deleted: HashSet<Uuid>,
    turn_cancel: Option<CancellationToken>,
    /// Id of the in-flight turn; completions for older turns are ignored.
    turn_id: u64,
    turn_done_tx: mpsc::UnboundedSender<TurnDone>,
    turn_done_rx: Option<mpsc::UnboundedReceiver<TurnDone>>,
    login_cancel: Option<CancellationToken>,
    login_id: u64,
    shutdown: CancellationToken,
    events: mpsc::UnboundedSender<AppEvent>,
    self_tx: mpsc::UnboundedSender<AppCommand>,
    /// Where to look for another tool's Google session. Empty in tests, so no test reads the
    /// credential files of the machine it runs on.
    import_roots: SearchRoots,
    /// What the last look found; [`AppCommand::UseImportedSession`] takes its session.
    import_check: Option<ImportCheck>,
    /// The session reused from another tool, while it is in use.
    imported: Option<ImportedSession>,
    /// Generation of the latest imported session; reports about older ones are ignored.
    import_gen: u64,
    /// settings.json exists but could not be read: every save would replace it with defaults,
    /// so none is written this session. Set at startup only.
    keep_settings_file: bool,
    /// Why settings.json was not used at startup, reported once the UI is up.
    settings_problem: Option<String>,
    /// What `--model` and `--mode` changed for this run, kept out of settings.json.
    launch: LaunchOverrides,
    /// Number of the last `SaveSettings` applied, sent with every settings echo.
    settings_seq: u64,
    /// Number of the latest MCP reload; a connect that belongs to an older one is shut down
    /// when it reports instead of being installed.
    mcp_gen: u64,
    /// The agent must be rebuilt, but a turn holds it; dropped when that turn ends. Dropping it
    /// mid-turn would cut "Send now" off from the running turn.
    agent_stale: bool,
    /// The run loop ended: a turn that reports now is saved, and nothing new starts.
    shutting_down: bool,
    /// Subscription accounts: the files under `accounts/` and the live token source of each.
    accounts: Arc<Accounts>,
    /// What the Account tab shows for each account settings lists. Holds no token.
    account_views: Vec<AccountView>,
    /// The one account sign-in in flight: one at a time, like the Google one. Its id is the
    /// stale-report guard (attempt ids never repeat, so no separate generation is kept), and its
    /// own token is what Cancel and shutdown cancel.
    account_login: Option<Arc<LoginAttempt>>,
    /// The last attempt that stopped without signing in. Its state and verifier can still finish
    /// it from a pasted redirect link. Never set while `account_login` is.
    account_paste: Option<Arc<LoginAttempt>>,
    /// Stops the exchange of a pasted link in flight. Its own token: the attempt's was usually
    /// cancelled already, by the browser round trip the paste replaced.
    account_paste_cancel: Option<CancellationToken>,
    /// Model-list popup state per account id, mirrored to the UI.
    account_models: HashMap<String, AccountModelsState>,
    /// Opens a sign-in's page. The browser in the app; a no-op in tests unless one installs a
    /// fake, so no test opens a window.
    account_opener: AccountOpener,
    /// Test hook: account sign-ins and account turns go to this mock server, never to a provider.
    /// Unroutable until a test sets its own, so a test that forgets cannot reach the network.
    #[cfg(test)]
    account_mock: Option<String>,
}

/// Fields `--model` and `--mode` set for this run only: their values in settings.json, and the
/// flags' values. A save writes the file's value back while a field still holds the flag's, so
/// one launch with `--mode auto` does not turn auto-run on for good; a field the user changes
/// this session is saved as chosen.
struct LaunchOverrides {
    file: Settings,
    flags: Settings,
}

impl LaunchOverrides {
    /// `settings` as settings.json should hold it.
    fn for_file(&self, settings: &Settings) -> Settings {
        let mut out = settings.clone();
        macro_rules! restore {
            ($($field:ident),* $(,)?) => {
                $(
                    if out.$field == self.flags.$field {
                        out.$field = self.file.$field.clone();
                    }
                )*
            };
        }
        // Every field `ExecutionMode::apply` and `--model` set.
        restore!(model, agent_mode, auto_approve_edits, terminal_policy, auto_approve_read_tools);
        out
    }
}

/// What the next turn's agent is built from ([`Backend::build_agent`] reads these settings). A
/// save that changes none of them keeps the agent, and with it the conversation's "Always
/// allow" choices; the policy fields reach a live agent through the shared policy instead.
fn agent_inputs_changed(old: &Settings, new: &Settings) -> bool {
    profile_backend(old) != profile_backend(new)
        || old.active_account != new.active_account
        || account_backend(old) != account_backend(new)
        || old.current_workspace() != new.current_workspace()
        || old.shell_timeout_secs != new.shell_timeout_secs
        || old.shell_output_cap_bytes != new.shell_output_cap_bytes
        || old.auto_approve_read_tools != new.auto_approve_read_tools
        || old.preview_access != new.preview_access
        || old.latest_flash_ga != new.latest_flash_ga
        // The route is read when the agent is built, so a change to it (or to the profile it
        // names) must rebuild one; the next turn then starts on the new route. A turn that is
        // running keeps the route it began with, which is what "the first N requests of this
        // message" means.
        || old.bypass != new.bypass
        || bypass_profile(old) != bypass_profile(new)
}

/// The bypass profile as [`Backend::build_agent`] takes it: the whole profile except the list its
/// model is picked from, which the route never reads.
///
/// The model itself is compared, unlike the profile in use: the route bakes its backend and model
/// in when the agent is built (it cannot follow a shared handle), so picking another model for the
/// bypass profile must rebuild the agent or every request would keep sending the old id.
/// `None` when the mode is off or names no profile.
fn bypass_profile(s: &Settings) -> Option<ProviderProfile> {
    if !s.bypass.routes() {
        return None;
    }
    let id = s.bypass.provider.as_deref()?;
    s.providers.iter().find(|p| p.id == id).map(|p| ProviderProfile { models: Vec::new(), ..p.clone() })
}

/// What [`Backend::build_agent`] takes from the account in use: which account (its token source)
/// and the wire it speaks.
///
/// Not its model, and nothing of the accounts not in use: a kept agent follows the model through
/// its shared handle, so picking another model of the same account — or signing in to a second
/// account — keeps the conversation's "Always allow" choices.
fn account_backend(s: &Settings) -> Option<(&str, AccountProvider)> {
    s.active_account_entry().map(|e| (e.id.as_str(), e.provider))
}

/// What [`Backend::build_agent`] takes from the profile in use: all of it except its model, which
/// a kept agent follows through its shared handle as it does an account's, and the list that
/// model is picked from.
fn profile_backend(s: &Settings) -> Option<ProviderProfile> {
    s.active_provider_profile().map(|p| ProviderProfile { model: String::new(), models: Vec::new(), ..p.clone() })
}

/// Which backend sends turns: the account in use, else the provider profile in use, else Code
/// Assist (both `None`).
///
/// Coarser than [`agent_inputs_changed`]: it ignores how a backend is set up. An agent may be
/// handed another model of the backend it was built for, never a model of another one: a Codex id
/// sent to Code Assist is a failed turn.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TurnBackend {
    account: Option<String>,
    profile: Option<String>,
}

impl TurnBackend {
    fn of(s: &Settings) -> Self {
        Self {
            account: s.active_account_entry().map(|e| e.id.clone()),
            profile: s.active_provider_profile().map(|p| p.id.clone()),
        }
    }
}

/// The account list after a settings save: the backend's own entries, each with the model the UI
/// picked for it when the UI changed that model (relative to `base`, the copy it edited).
///
/// Entries are added by a finished sign-in and removed by a sign-out, and both happen here, never
/// in a save. The UI's copy can predate either, so taking its list whole would bring back an
/// account the user signed out of, or drop one that has just signed in.
fn saved_accounts(current: &[AccountEntry], base: &[AccountEntry], edited: &[AccountEntry]) -> Vec<AccountEntry> {
    current
        .iter()
        .map(|entry| {
            let before = base.iter().find(|b| b.id == entry.id).map(|b| b.model.as_str());
            match edited.iter().find(|e| e.id == entry.id) {
                Some(e) if Some(e.model.as_str()) != before && external::valid_model_id(&e.model) => {
                    AccountEntry { model: e.model.trim().to_owned(), ..entry.clone() }
                }
                _ => entry.clone(),
            }
        })
        .collect()
}

/// The transient profile an account's turns run on.
///
/// The id encodes the account, so the Anthropic dialect's reasoning binding separates two
/// accounts and one account's signed `thinking` blocks are never replayed to another. The window
/// is the wire's own: without it compaction would keep to Gemini's million tokens, and alias
/// resolution (which `build_agent` turns off whenever a window is set) would rewrite an account
/// model named `auto` into a Gemini id.
fn account_profile(entry: &AccountEntry, plan: Option<&str>) -> ProviderProfile {
    let kind = entry.provider.wire_kind();
    let label = entry.label.trim();
    let model = entry.model.trim().to_owned();
    // The model's own window where the catalog knows it — they range from 114K to a million on
    // one account — else the wire kind's conservative default.
    let window = external::catalog::context_window(entry.provider, &model, plan).unwrap_or_else(|| kind.default_context_window());
    ProviderProfile {
        id: format!("acct:{}", entry.id),
        label: if label.is_empty() { entry.provider.label().to_owned() } else { label.to_owned() },
        kind,
        model,
        base_url: None,
        api_key: String::new(),
        max_output_tokens: None,
        context_window: Some(window),
        models: Vec::new(),
    }
}

/// The row label of a newly listed account: its email, else the provider's name.
fn account_label(view: &AccountView) -> String {
    view.email.clone().unwrap_or_else(|| view.provider.label().to_owned())
}

/// The settings entry for an account settings did not list yet.
fn new_account_entry(view: &AccountView) -> AccountEntry {
    AccountEntry { id: view.id.clone(), provider: view.provider, label: account_label(view), model: view.model.clone() }
}

/// What the UI is told about an attempt.
fn login_info(attempt: &LoginAttempt, error: Option<String>) -> AccountLoginInfo {
    AccountLoginInfo { id: attempt.id, provider: attempt.provider, auth_url: attempt.auth_url.to_string(), error }
}

/// What a turn runs on: the backend, the model it starts with, and the input window compaction
/// keeps to (`None`: the Code Assist model's own, with its aliases resolved).
struct AgentBackend {
    backend: Arc<dyn ModelBackend>,
    model: String,
    context_window: Option<u64>,
    /// Bypass Mode's route for the first requests of each message, when the mode routes.
    bypass: Option<crate::agent::bypass::BypassRoute>,
}

/// Code Assist client over `tokens` (AntiCraft's own sign-in or an imported session).
fn code_assist_client(http: &reqwest::Client, tokens: Arc<dyn TokenProvider>, settings: &Settings) -> CodeAssistClient {
    CodeAssistClient::new(
        http.clone(),
        tokens,
        ApiConfig { user_project: settings.gcp_project.clone(), latest_flash_ga: settings.latest_flash_ga, ..ApiConfig::default() },
    )
}

/// Build auth and the Code Assist client from the resolved OAuth client secret.
fn build_auth(
    data_dir: &std::path::Path,
    settings: &Settings,
    http: &reqwest::Client,
) -> (Option<Arc<AuthManager>>, Option<CodeAssistClient>, Option<ClientSource>) {
    let (auth, src) = match AuthConfig::load(data_dir) {
        Ok((cfg, src)) => (Some(AuthManager::new(cfg, CredentialStore::in_dir(data_dir), http.clone())), Some(src)),
        Err(e) => {
            tracing::warn!("auth disabled: {e}");
            (None, None)
        }
    };
    let client = auth.as_ref().map(|a| code_assist_client(http, a.clone(), settings));
    (auth, client, src)
}

/// Spawn the backend on `rt` and return the UI side of the channels, plus the backend task's
/// handle: `main` waits on it at exit so queued commands (the last `SaveSettings`) finish
/// before the runtime stops. `repaint` is called whenever an event is queued so egui wakes up.
pub fn spawn_backend(
    rt: &tokio::runtime::Handle,
    opts: AppOptions,
    shutdown: CancellationToken,
    repaint: Arc<dyn Fn() + Send + Sync>,
) -> (UiChannels, tokio::task::JoinHandle<()>) {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (raw_tx, mut raw_rx) = mpsc::unbounded_channel::<AppEvent>();
    let (ui_tx, ui_rx) = mpsc::unbounded_channel();
    // Forwarder that requests a repaint for every event.
    rt.spawn(async move {
        while let Some(ev) = raw_rx.recv().await {
            if ui_tx.send(ev).is_err() {
                break;
            }
            repaint();
        }
    });
    let backend = Backend::new(opts, shutdown, raw_tx, cmd_tx.clone());
    let task = rt.spawn(backend.run(cmd_rx));
    (UiChannels { commands: cmd_tx, events: ui_rx }, task)
}

impl Backend {
    pub(crate) fn new(
        opts: AppOptions,
        shutdown: CancellationToken,
        events: mpsc::UnboundedSender<AppEvent>,
        self_tx: mpsc::UnboundedSender<AppCommand>,
    ) -> Self {
        let (file, problem) = Settings::load_reporting(&opts.data_dir);
        let mut settings = file.clone();
        if let Some(m) = &opts.model {
            settings.model = m.clone();
        }
        if let Some(mode) = opts.mode {
            mode.apply(&mut settings);
        }
        let launch = LaunchOverrides { file, flags: settings.clone() };
        settings.skip_permissions = opts.skip_permissions;
        if let Some(ws) = &opts.workspace {
            settings.remember_workspace(ws);
        }
        // connect_timeout bounds connection setup; read_timeout bounds each read, so a hung
        // request or a stalled stream ends instead of blocking Stop forever.
        let http = reqwest::Client::builder()
            .user_agent(ApiConfig::default().user_agent)
            .connect_timeout(Duration::from_secs(20))
            .read_timeout(Duration::from_secs(180))
            .build()
            .unwrap_or_default();
        let (auth, client, client_source) = build_auth(&opts.data_dir, &settings, &http);
        let (turn_done_tx, turn_done_rx) = mpsc::unbounded_channel();
        let conv_mode = settings.agent_mode;
        let policy = settings.approval_policy(conv_mode).shared();
        // A refresh that finds an account's session dead reports it as a command. `Accounts::new`
        // wraps this sink so the dead token source leaves the registry first: by the time
        // `AccountExpired` is handled, `accounts.auth(id)` already answers `None`.
        let on_expired: ExpiredSink = {
            let tx = self_tx.clone();
            Arc::new(move |id: String| {
                let _ = tx.send(AppCommand::AccountExpired(id));
            })
        };
        let accounts = Accounts::new(opts.data_dir.clone(), http.clone(), on_expired);
        // No test may open a browser window; one that needs a browser installs a fake.
        let account_opener: AccountOpener = if cfg!(test) { Arc::new(|_: &LoginAttempt, _: url::Url| {}) } else { Arc::new(open_in_browser) };
        Self {
            agent_steer: None,
            policy,
            conv_mode,
            plugins: Vec::new(),
            mcp: None,
            modes: HashMap::new(),
            queue: VecDeque::new(),
            queue_seq: 0,
            store: ConversationStore::new(&opts.data_dir),
            turn_id: 0,
            turn_done_tx,
            turn_done_rx: Some(turn_done_rx),
            opts,
            settings,
            auth,
            client,
            http,
            client_source,
            current: None,
            agent: None,
            agent_model: None,
            agent_sends_to: None,
            deleted: HashSet::new(),
            turn_cancel: None,
            login_cancel: None,
            login_id: 0,
            shutdown,
            events,
            self_tx,
            import_roots: if cfg!(test) { SearchRoots::default() } else { SearchRoots::user() },
            import_check: None,
            imported: None,
            import_gen: 0,
            keep_settings_file: problem.as_ref().is_some_and(|p| p.keep_file),
            settings_problem: problem.map(|p| p.message),
            launch,
            settings_seq: 0,
            mcp_gen: 0,
            agent_stale: false,
            shutting_down: false,
            accounts,
            account_views: Vec::new(),
            account_login: None,
            account_paste: None,
            account_paste_cancel: None,
            account_models: HashMap::new(),
            account_opener,
            #[cfg(test)]
            account_mock: Some("http://127.0.0.1:1".into()),
        }
    }

    fn emit(&self, ev: AppEvent) {
        let _ = self.events.send(ev);
    }

    /// Main loop: initial state, then commands until shutdown.
    pub async fn run(mut self, mut commands: mpsc::UnboundedReceiver<AppCommand>) {
        self.emit(AppEvent::Settings(self.settings_seq, self.settings.clone()));
        self.emit(AppEvent::WorkspaceChanged(self.settings.current_workspace().map(Into::into)));
        self.refresh_list();
        self.emit(AppEvent::OAuthClientSource(self.client_source));
        // The backend owns the message queue from the start.
        self.emit_queue();
        match &self.auth {
            Some(a) => {
                let _ = a.load_cached().await;
                self.emit(AppEvent::Auth(a.status().await));
            }
            None => self.emit(AppEvent::Auth(AuthStatus::Error("OAuth client is not configured".into()))),
        }
        // After the sign-in state, so a sign-in banner does not hide it: the next save rewrites
        // settings.json from the defaults the app started with.
        if let Some(problem) = self.settings_problem.take() {
            self.emit(AppEvent::Error(problem));
        }
        // Report a session another Google tool saved; it is only used on an explicit choice.
        self.check_imported_session().await;
        // Subscription accounts: their files are the truth, settings only list them.
        let views = self.accounts.load_all().await;
        self.reconcile_accounts(views);
        // Read plugin directories and any MCP configuration once, before the loop: both are
        // file IO, and the prompt content built at the first turn must already include them.
        self.discover_plugins().await;
        self.reload_mcp().await;
        let mut done_rx = self.turn_done_rx.take().expect("run called once");
        loop {
            // Biased, in this order: a finished turn is settled before the next command, so a
            // prompt sent right after the UI saw `Finished` does not meet a stale running turn;
            // and every queued command runs before the shutdown token is looked at. `on_exit`
            // sends `SaveSettings` and `Shutdown`, then cancels the token at once. An unbiased
            // select could take the cancelled branch first and drop that last save.
            let cmd = tokio::select! {
                biased;
                Some(done) = done_rx.recv() => {
                    self.turn_finished(done);
                    continue;
                }
                c = commands.recv() => c,
                _ = self.shutdown.cancelled() => None,
            };
            let Some(cmd) = cmd else { break };
            if matches!(cmd, AppCommand::Shutdown) {
                break;
            }
            self.handle(cmd).await;
        }
        self.shutting_down = true;
        let running = self.turn_cancel.is_some();
        self.cancel_all();
        if running {
            // A turn is saved only when it reports (`turn_finished`), and a new chat has no file
            // before that, so closing the window mid-turn would lose the prompt and everything
            // the turn did. The cancelled turn usually reports within milliseconds; hooks that
            // run while it unwinds can take longer, so the wait is capped well inside the 5 s
            // `main` gives this task, leaving room for the MCP shutdown below.
            match tokio::time::timeout(SHUTDOWN_TURN_WAIT, done_rx.recv()).await {
                Ok(Some(done)) => self.turn_finished(done),
                _ => tracing::warn!("the running turn did not stop in time; its last messages are not saved"),
            }
        }
        // Close MCP server processes. Dropping the hub alone would leak a stdio child until the
        // process exits, and an HTTP server would keep a live connection.
        if let Some(hub) = self.mcp.take() {
            hub.shutdown().await;
        }
    }

    /// Discover plugins off the runtime worker and tell the UI.
    ///
    /// Discovery walks the workspace and data directories and canonicalizes every candidate, so
    /// it is ordinary blocking file IO. Failures are logged and leave the previous list in
    /// place: a plugin directory that cannot be read must not clear one that already works.
    async fn discover_plugins(&mut self) {
        let ws = self.active_workspace();
        let data = self.opts.data_dir.clone();
        let choices = self.settings.plugins.clone();
        let found = tokio::task::spawn_blocking(move || plugins::discover(ws.as_deref(), Some(&data), &choices)).await;
        match found {
            Ok(list) => self.plugins = list,
            Err(e) => tracing::warn!("plugin discovery failed: {e}"),
        }
        self.emit_plugins();
    }

    fn emit_plugins(&self) {
        self.emit(AppEvent::Plugins(self.plugins.iter().map(PluginInfo::from).collect()));
    }

    /// Read every `mcp_config.json` that applies, connect the servers and report their status.
    ///
    /// Sources are the workspace's `.agents/mcp_config.json`, the data directory's
    /// `mcp_config.json`, then the enabled plugins. The workspace file comes first because it is
    /// the most specific, and the first server with a given name wins, so a repository cannot
    /// shadow a server the user configured by declaring the same name later in the list.
    ///
    /// A plugin's servers require the same explicit user choice its hooks do: a plugin that is
    /// merely enabled by default came with the checkout, and starting a process it names would
    /// be executing repository-supplied code without consent.
    ///
    /// Every failure is logged and the connection carries on with what is left: MCP is an add-on,
    /// and no configuration can stop the agent from running.
    async fn reload_mcp(&mut self) {
        // Numbered before anything can return early: a connect still running from an earlier
        // reload is out of date even when this one finds no server to start.
        self.mcp_gen += 1;
        let gen = self.mcp_gen;
        let mut configs: Vec<McpServerConfig> = Vec::new();
        let ws = self.active_workspace();
        let data = self.opts.data_dir.clone();
        // Read the documents on the blocking pool, parse on the async side (the same split the
        // rest of the backend uses for file IO).
        let docs = tokio::task::spawn_blocking(move || {
            let mut docs: Vec<(serde_json::Value, String)> = Vec::new();
            if let Some(ws) = ws {
                let p = ws.join(".agents").join("mcp_config.json");
                if let Some(doc) = read_json_file(&p) {
                    docs.push((doc, p.display().to_string()));
                }
            }
            let p = data.join("mcp_config.json");
            if let Some(doc) = read_json_file(&p) {
                docs.push((doc, p.display().to_string()));
            }
            docs
        })
        .await;
        let docs = match docs {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("could not read the MCP configuration: {e}");
                Vec::new()
            }
        };
        for (doc, source) in docs {
            configs.extend(mcp::parse_servers(&doc, &source));
        }
        for plugin in self.plugins.iter().filter(|p| p.is_user_enabled()) {
            for (i, doc) in plugin.mcp_configs.iter().enumerate() {
                let source = format!("plugin {} (#{i})", plugin.id);
                configs.extend(mcp::parse_servers(doc, &source));
            }
        }
        // First name wins: a later document cannot redirect a server an earlier one configured.
        let mut seen = HashSet::new();
        configs.retain(|c| seen.insert(c.name.clone()));
        configs.truncate(mcp::config::MAX_SERVERS);
        // Shut down the previous hub before spawning the new connect. A connect that is still
        // running has no hub here yet; `McpConnected` shuts it down when it reports, because
        // its number is older than this reload's.
        let previous = self.mcp.take();
        if let Some(old) = previous {
            tokio::spawn(async move { old.shutdown().await });
        }
        if configs.is_empty() {
            self.emit(AppEvent::McpStatus(Vec::new()));
            return;
        }
        // `McpHub::connect` makes HTTP or stdio round-trips that can take several seconds. Spawn
        // it so the command loop stays responsive during the handshake; the result comes back
        // as `McpConnected`, which installs the hub and reports status on the next loop pass.
        let self_tx = self.self_tx.clone();
        tokio::spawn(async move {
            let hub = McpHub::connect(configs).await;
            let statuses = hub.statuses();
            let hub = if hub.is_empty() {
                // No server answered with a tool: an empty hub would still add an empty
                // declaration list and a status row to every request, so it is not kept.
                tracing::info!("no MCP server exposed a tool; MCP stays disabled");
                None
            } else {
                Some(hub)
            };
            let _ = self_tx.send(AppCommand::McpConnected(gen, hub, statuses));
        });
    }

    /// A spawned turn task completed: clear the running state if it is still the current turn,
    /// and persist the history. The save happens here, on the backend loop, from the current
    /// conversation (single source of truth), so renames during the turn are kept and deleted
    /// conversations are not recreated.
    fn turn_finished(&mut self, done: TurnDone) {
        if done.turn_id == self.turn_id {
            self.turn_cancel = None;
            // A rebuild asked for while the turn held the agent happens now, before anything
            // can start the next turn on it.
            if std::mem::take(&mut self.agent_stale) {
                self.drop_agent();
            }
        }
        // The turn closed its steering input (in its task, before it reported); text that
        // arrived after that is in `done.steer` below and runs as the next prompt.
        if !done.restore.is_empty() {
            // Edit/retry rollback: drop what the replaced turn wrote (blocking file I/O).
            let store = self.store.artifacts(done.conversation_id);
            let names: Vec<String> = done.restore.iter().map(|m| m.name.clone()).collect();
            tokio::task::spawn_blocking(move || {
                for n in names {
                    if let Err(e) = store.remove(&n) {
                        tracing::warn!("could not roll back artifact {n}: {e}");
                    }
                }
            });
        }
        if let Some(history) = done.history {
            if self.deleted.contains(&done.conversation_id) {
                tracing::info!("turn finished for a deleted conversation; not saved");
            } else if let Some(conv) = self.current.as_mut().filter(|c| c.id == done.conversation_id) {
                conv.messages = history;
                if let Some(a) = done.artifacts.clone() {
                    conv.artifacts = a;
                }
                if let Err(e) = self.store.save(conv) {
                    self.emit(AppEvent::Error(format!("could not save conversation: {e}")));
                }
            } else {
                // Not shown any more: merge the messages into what is on disk.
                let mut conv = self.store.load(done.conversation_id).ok().or(done.snapshot);
                if let Some(c) = conv.as_mut() {
                    c.messages = history;
                    if let Some(a) = done.artifacts.clone() {
                        c.artifacts = a;
                    }
                    if let Err(e) = self.store.save(c) {
                        self.emit(AppEvent::Error(format!("could not save conversation: {e}")));
                    }
                }
            }
        }
        self.refresh_list();
        if self.shutting_down {
            // Saved above; the app is closing, so nothing queued starts.
            return;
        }
        // Settle the queue of the open conversation. Every queued entry is one the user
        // already sent: after a normal finish the next one runs; after Stop, a rejection or an
        // error the queue is kept (not run) for the user to decide. A steer that reached the
        // backend too late to be injected leads the queue, because it is injected text that
        // never made it into the transcript.
        let current = self.current.as_ref().is_some_and(|c| c.id == done.conversation_id);
        if done.turn_id != self.turn_id || !current || self.deleted.contains(&done.conversation_id) {
            return;
        }
        let late: VecDeque<String> = done.steer.into_iter().filter(|t| !t.trim().is_empty()).collect();
        // A late steer means the user saw the text as sent: it runs even after a Stop.
        let late_steer = !late.is_empty();
        if late_steer {
            let mut merged: VecDeque<(u64, String)> = VecDeque::with_capacity(late.len() + self.queue.len());
            for t in late {
                self.queue_seq += 1;
                merged.push_back((self.queue_seq, t));
            }
            merged.extend(std::mem::take(&mut self.queue));
            self.queue = merged;
            self.emit_queue();
        }
        if self.turn_cancel.is_none() && !self.queue.is_empty() && (done.reason == Some(FinishReason::Completed) || late_steer) {
            // Not at once: a Remove or Steer on the first chip may already wait in the command
            // channel (the user acted before the UI heard the turn end); it goes first, and
            // what is first in the queue after it runs.
            let _ = self.self_tx.send(AppCommand::RunNextQueued);
        }
    }

    /// "Send immediately" while a turn runs. The text is injected into the running turn at its
    /// next request boundary when possible; otherwise (no turn, or the turn already closed its
    /// input) it is queued directly behind the current turn, so it is delivered exactly once:
    /// never dropped, and never run twice by both the agent and the queue.
    fn steer(&mut self, text: String) {
        if text.trim().is_empty() {
            return;
        }
        // While the turn still reads steering, hand it the text; once it closed (it is
        // finishing), `send` refuses and the text is queued instead of being lost.
        if self.turn_cancel.is_some() && self.agent_steer.as_ref().is_some_and(|h| h.send(text.clone())) {
            return;
        }
        if self.queue.len() >= MAX_QUEUE {
            self.emit(AppEvent::Error(format!("At most {MAX_QUEUE} messages can wait in the queue.")));
            return;
        }
        self.queue_seq += 1;
        let (id, text) = (self.queue_seq, text);
        if self.turn_cancel.is_some() {
            // Runs right after this turn (also when it ends Error or Stopped: the text was
            // already shown as sent).
            self.queue.push_back((id, text));
            self.emit_queue();
            return;
        }
        // Nothing runs: go now.
        self.queue.push_back((id, text));
        self.emit_queue();
        self.run_next_queued();
    }

    /// Pop the first queued message and run it in the open conversation.
    fn run_next_queued(&mut self) {
        let Some((_, text)) = self.queue.pop_front() else { return };
        self.emit_queue();
        if let Some(id) = self.current.as_ref().map(|c| c.id) {
            self.emit(AppEvent::PromptStarted { conversation: id, text: text.clone() });
        }
        self.send_prompt(TurnKind::Prompt(text));
    }

    fn emit_queue(&self) {
        self.emit(AppEvent::Queue(self.queue.iter().cloned().collect()));
    }

    /// Rebuild the shared policy from settings and the conversation mode. A running turn
    /// sees the change at its next request or tool call.
    fn sync_policy(&self) {
        *self.policy.write().unwrap_or_else(|e| e.into_inner()) = self.settings.approval_policy(self.conv_mode);
    }

    /// Workspace of the open conversation, else the current one.
    fn active_workspace(&self) -> Option<PathBuf> {
        self.current.as_ref().and_then(|c| c.workspace.clone()).or_else(|| self.settings.current_workspace().map(Into::into))
    }

    fn cancel_all(&mut self) {
        if let Some(c) = self.turn_cancel.take() {
            c.cancel();
        }
        if let Some(c) = self.login_cancel.take() {
            c.cancel();
        }
        // Frees the provider's callback port now rather than when the runtime stops.
        if let Some(a) = self.account_login.take() {
            a.cancel.cancel();
        }
    }

    fn refresh_list(&self) {
        match self.store.list() {
            Ok(list) => self.emit(AppEvent::Conversations(list)),
            Err(e) => self.emit(AppEvent::Error(e.to_string())),
        }
    }

    async fn handle(&mut self, cmd: AppCommand) {
        match cmd {
            AppCommand::Login => {
                let Some(auth) = self.auth.clone() else { return };
                // Only one flow at a time: cancel the previous one; its result is ignored.
                if let Some(c) = self.login_cancel.take() {
                    c.cancel();
                }
                let cancel = self.shutdown.child_token();
                self.login_cancel = Some(cancel.clone());
                self.login_id += 1;
                let id = self.login_id;
                self.emit(AppEvent::Auth(AuthStatus::LoggingIn));
                let tx = self.self_tx.clone();
                tokio::spawn(async move {
                    let status = auth.login(cancel).await.unwrap_or_else(|e| AuthStatus::Error(e.to_string()));
                    let _ = tx.send(AppCommand::LoginFinished(id, status));
                });
            }
            AppCommand::LoginFinished(id, status) => {
                if id != self.login_id {
                    return; // superseded by a newer login attempt
                }
                self.login_cancel = None;
                if matches!(status, AuthStatus::LoggedIn { .. }) {
                    // AntiCraft's own sign-in takes over from a reused session.
                    if self.stop_imported_session() {
                        self.emit(AppEvent::ImportedSession(ImportedSessionEvent::Stopped));
                    }
                    self.reset_account_state().await;
                } else if self.imported.is_some() {
                    // The reused session stays in use: report the failure, keep the signed-in state.
                    if let AuthStatus::Error(e) = &status {
                        if *e != AuthError::Cancelled.to_string() {
                            self.emit(AppEvent::Error(format!("Google sign-in failed: {e}")));
                        }
                    }
                    self.emit(AppEvent::Auth(self.auth_status().await));
                    return;
                }
                self.emit(AppEvent::Auth(status));
            }
            AppCommand::CancelLogin => {
                if let Some(c) = self.login_cancel.take() {
                    c.cancel();
                }
            }
            AppCommand::Logout => {
                // Signing out of a reused session only lets go of it: the other app's file and
                // its own sign-in stay as they are.
                if self.stop_imported_session() {
                    self.reset_account_state().await;
                    self.emit(AppEvent::ImportedSession(ImportedSessionEvent::Stopped));
                    self.emit(AppEvent::Auth(self.auth_status().await));
                    return;
                }
                // Off the main loop: logout waits for the credential lock.
                if let Some(a) = self.auth.clone() {
                    let tx = self.self_tx.clone();
                    tokio::spawn(async move {
                        let err = a.logout().await.err().map(|e| e.to_string());
                        let status = a.status().await;
                        let _ = tx.send(AppCommand::LogoutFinished(err, status));
                    });
                }
            }
            AppCommand::LogoutFinished(err, status) => {
                if let Some(e) = err {
                    self.emit(AppEvent::Error(e));
                }
                self.reset_account_state().await;
                // A session reused meanwhile keeps the signed-in state.
                let status = if self.imported.is_some() { self.auth_status().await } else { status };
                self.emit(AppEvent::Auth(status));
            }
            AppCommand::CheckImportedSession => self.check_imported_session().await,
            AppCommand::UseImportedSession => self.use_imported_session().await,
            AppCommand::StopImportedSession => {
                if self.stop_imported_session() {
                    self.reset_account_state().await;
                    self.emit(AppEvent::ImportedSession(ImportedSessionEvent::Stopped));
                    self.emit(AppEvent::Auth(self.auth_status().await));
                }
            }
            AppCommand::ImportedSessionChanged(generation, change) => self.imported_session_changed(generation, change).await,
            AppCommand::NewChat => {
                let conv = Conversation::new(self.settings.current_workspace().map(Into::into), self.settings.model.clone());
                self.open(conv);
            }
            AppCommand::OpenConversation(id) => match self.store.load(id) {
                Ok(conv) => self.open(conv),
                Err(e) => self.emit(AppEvent::Error(e.to_string())),
            },
            AppCommand::RenameConversation(id, title) => {
                if let Err(e) = self.store.rename(id, &title) {
                    self.emit(AppEvent::Error(e.to_string()));
                }
                if let Some(c) = self.current.as_mut().filter(|c| c.id == id) {
                    c.title = title;
                }
                self.refresh_list();
            }
            AppCommand::DeleteConversation(id) => {
                if let Err(e) = self.store.delete(id) {
                    self.emit(AppEvent::Error(e.to_string()));
                }
                self.deleted.insert(id);
                if self.current.as_ref().is_some_and(|c| c.id == id) {
                    // Stop the running turn; its completion will not be saved.
                    if let Some(c) = &self.turn_cancel {
                        c.cancel();
                    }
                    self.current = None;
                    self.drop_agent();
                    self.queue.clear();
                    self.emit_queue();
                }
                self.refresh_list();
            }
            AppCommand::PickWorkspace => {
                let tx = self.self_tx.clone();
                tokio::spawn(async move {
                    if let Some(h) = rfd::AsyncFileDialog::new().pick_folder().await {
                        let _ = tx.send(AppCommand::SetWorkspace(h.path().to_path_buf()));
                    }
                });
            }
            AppCommand::PickBypassFiles => {
                let events = self.events.clone();
                tokio::spawn(async move {
                    let picked = rfd::AsyncFileDialog::new()
                        .add_filter("Markdown", &["md"])
                        .set_title("Instruction files for Bypass Mode")
                        .pick_files()
                        .await
                        .unwrap_or_default();
                    // Paths only: nothing is read here, the agent reads them when a turn is built.
                    let files: Vec<PathBuf> = picked.into_iter().map(|h| h.path().to_path_buf()).collect();
                    if !files.is_empty() {
                        let _ = events.send(AppEvent::BypassFiles(files));
                    }
                });
            }
            AppCommand::SetWorkspace(p) => {
                self.settings.remember_workspace(&p);
                self.save_settings();
                self.rebuild_agent();
                self.emit(AppEvent::WorkspaceChanged(Some(p)));
                // Plugins and MCP servers are workspace-scoped; refresh both after the switch.
                self.discover_plugins().await;
                self.reload_mcp().await;
            }
            AppCommand::SetModel(m) => {
                self.settings.model = m;
                self.save_settings();
                // With an account or a provider active its model is what a turn actually sends, so
                // the chip keeps showing that; `settings.model` is remembered for Code Assist.
                // Never lock the agent here: a running turn holds it. The turn reads the shared
                // model at its next request.
                self.sync_model();
            }
            AppCommand::SetActiveProvider(id) => {
                let id = id.map(|i| i.trim().to_owned()).filter(|i| !i.is_empty());
                if let Some(id) = &id {
                    if !self.settings.providers.iter().any(|p| p.id == *id) {
                        self.emit(AppEvent::Error(format!("unknown provider {id:?}")));
                        return;
                    }
                }
                // Exclusive: a profile (or Code Assist, with `None`) also stops an account from
                // sending turns. Assigning `active_provider` alone would leave an active account in
                // front of it, and "back to Code Assist" would change nothing at all.
                if !self.settings.select_profile(id) {
                    return;
                }
                self.save_settings();
                // The next turn must build its backend from the new selection.
                self.rebuild_agent();
                // A provider profile carries its own model id, so the chip follows the switch.
                self.sync_model();
            }
            AppCommand::LoadProviderModels { request, profile } => {
                // Off the loop: a list is a few round trips at most, but a dead host takes the
                // whole timeout, and nothing else may wait on it. Nothing is stored here; the
                // dialog keeps the list and a save brings it back with the profile.
                let (http, events) = (self.http.clone(), self.events.clone());
                tokio::spawn(async move {
                    let result = crate::providers::models::list_models(&http, &profile).await;
                    let _ = events.send(AppEvent::ProviderModels { request, result });
                });
            }
            AppCommand::UseProviderModel { profile, model } => self.use_provider_model(profile, model),
            AppCommand::SaveSettings { seq, base, settings } => {
                // Only what the UI changed: its other fields may be older than the backend's
                // (a plugin choice or a workspace this loop applied after the UI's last echo).
                let mut next = crate::settings::merge_unsent(&base, &settings, &self.settings);
                // Accounts come and go only through a sign-in or a sign-out; a save brings just
                // the models the UI picked for them.
                next.accounts = saved_accounts(&self.settings.accounts, &base.accounts, &settings.accounts);
                // The selection this save made wins. `sanitized` keeps an active account over a
                // profile, so a profile picked on the Providers tab (which edits `active_provider`
                // alone) while an account is in use would otherwise be dropped without a word.
                // `select_profile` refuses an id the backend no longer has, keeping the account.
                if settings.active_provider != base.active_provider && settings.active_account == base.active_account {
                    next.select_profile(settings.active_provider.clone());
                }
                let next = next.sanitized();
                let rebuild = agent_inputs_changed(&self.settings, &next);
                let accounts_changed = self.settings.accounts != next.accounts;
                self.settings = next;
                self.settings_seq = self.settings_seq.max(seq);
                self.sync_policy();
                self.save_settings();
                if rebuild {
                    self.rebuild_agent();
                }
                if accounts_changed {
                    self.emit_accounts();
                }
                // The composer chip shows the model turns use; keep the open conversation on it,
                // and a kept agent too while it still sends to that backend (a running turn reads
                // the shared model at its next request).
                self.sync_model();
            }
            AppCommand::SendPrompt(text) => self.send_prompt(TurnKind::Prompt(text)),
            AppCommand::SendPromptWithMode(text, mode) => self.send_prompt(TurnKind::PromptWithMode(text, mode)),
            AppCommand::EditLastPrompt(text, mode) => self.send_prompt(TurnKind::Edit(text, mode)),
            AppCommand::Retry => self.send_prompt(TurnKind::Retry),
            AppCommand::Compact => self.send_prompt(TurnKind::Compact),
            AppCommand::Stop => {
                // Cancel only; the turn task reports completion (with the saved history) itself.
                if let Some(c) = &self.turn_cancel {
                    c.cancel();
                }
            }
            AppCommand::SetOAuthClientSecret(secret) => {
                if self.turn_cancel.is_some() || self.login_cancel.is_some() {
                    self.emit(AppEvent::Error("Finish the running turn or sign-in before changing the client secret.".into()));
                    return;
                }
                let dir = self.opts.data_dir.clone();
                let res = tokio::task::spawn_blocking(move || crate::auth::client_config::save(&dir, &secret))
                    .await
                    .unwrap_or_else(|e| Err(std::io::Error::other(e.to_string())));
                if let Err(e) = res {
                    self.emit(AppEvent::Error(format!("could not save the client secret: {e}")));
                    return;
                }
                self.rebuild_auth().await;
            }
            AppCommand::ClearOAuthClientSecret => {
                if self.turn_cancel.is_some() || self.login_cancel.is_some() {
                    self.emit(AppEvent::Error("Finish the running turn or sign-in before changing the client secret.".into()));
                    return;
                }
                let dir = self.opts.data_dir.clone();
                let res = tokio::task::spawn_blocking(move || crate::auth::client_config::delete(&dir))
                    .await
                    .unwrap_or_else(|e| Err(std::io::Error::other(e.to_string())));
                if let Err(e) = res {
                    self.emit(AppEvent::Error(format!("could not delete the client secret: {e}")));
                }
                self.rebuild_auth().await;
            }
            AppCommand::Shutdown => {}
            AppCommand::QueuePrompt(text) => {
                if self.turn_cancel.is_none() {
                    // The turn ended before the message arrived: run it now.
                    self.start_backend_prompt(text);
                    return;
                }
                if self.queue.len() >= MAX_QUEUE {
                    self.emit(AppEvent::Error(format!("At most {MAX_QUEUE} messages can wait in the queue.")));
                } else {
                    self.queue_seq += 1;
                    self.queue.push_back((self.queue_seq, text));
                }
                self.emit_queue();
            }
            AppCommand::SteerPrompt(text) => self.steer(text),
            AppCommand::SteerQueued(id) => {
                // One command, so the entry cannot run from the queue and be steered as well.
                let Some(pos) = self.queue.iter().position(|(i, _)| *i == id) else { return };
                let Some((id, text)) = self.queue.remove(pos) else { return };
                if self.turn_cancel.is_some() && self.agent_steer.as_ref().is_some_and(|h| h.send(text.clone())) {
                    self.emit_queue();
                    return;
                }
                // The turn is finishing, or nothing runs (a queue kept after Stop): this message
                // goes first, ahead of the ones queued before it, and runs now if nothing runs.
                self.queue.push_front((id, text));
                self.emit_queue();
                if self.turn_cancel.is_none() {
                    self.run_next_queued();
                }
            }
            AppCommand::ClearQueue(id) => {
                match id {
                    Some(id) => self.queue.retain(|(i, _)| *i != id),
                    None => self.queue.clear(),
                }
                self.emit_queue();
            }
            AppCommand::RunNextQueued => {
                if self.turn_cancel.is_none() {
                    self.run_next_queued();
                }
            }
            AppCommand::SetAgentMode(m) => {
                if let Some(id) = self.current.as_ref().map(|c| c.id) {
                    self.modes.insert(id, m);
                }
                if self.conv_mode != m {
                    self.conv_mode = m;
                    self.sync_policy();
                }
            }
            AppCommand::ClearHistory => {
                if self.turn_cancel.is_some() {
                    self.emit(AppEvent::Error("Stop the running turn before clearing the conversation.".into()));
                    return;
                }
                if let Some(a) = &self.agent {
                    if let Ok(mut a) = a.try_lock() {
                        a.clear_history();
                    }
                }
                if let Some(conv) = self.current.as_mut() {
                    conv.messages.clear();
                    if let Err(e) = self.store.save(conv) {
                        self.emit(AppEvent::Error(format!("could not save conversation: {e}")));
                    }
                }
                self.refresh_list();
            }
            AppCommand::RewindCheckpoint(user_index) => {
                if self.turn_cancel.is_some() {
                    self.emit(AppEvent::Error("Stop the running turn before rewinding.".into()));
                    return;
                }
                let Some(id) = self.current.as_ref().map(|c| c.id) else { return };
                match self.store.load_checkpoint(id, user_index) {
                    Some(cp) => {
                        if let Some(conv) = self.current.as_mut() {
                            conv.messages = cp.messages;
                        }
                        // Snapshot for ConversationLoaded event; save first with a separate borrow.
                        if let Some(conv) = &mut self.current {
                            if let Err(e) = self.store.save(conv) {
                                self.emit(AppEvent::Error(format!("could not save after rewind: {e}")));
                                return;
                            }
                        }
                        self.drop_agent();
                        let loaded = self.current.clone();
                        if let Some(conv) = loaded {
                            self.emit(AppEvent::ConversationLoaded(conv));
                        }
                        self.refresh_list();
                    }
                    None => {
                        self.emit(AppEvent::Error(format!("Checkpoint not found (before message {user_index}).")));
                    }
                }
            }
            AppCommand::IndexWorkspace => {
                let Some(ws) = self.active_workspace() else { return };
                let tx = self.events.clone();
                tokio::task::spawn_blocking(move || {
                    let idx = crate::ui::mentions::build_index(&ws, crate::ui::mentions::MAX_INDEX_ENTRIES);
                    let _ = tx.send(AppEvent::WorkspaceIndex(idx));
                });
            }
            AppCommand::LoadWorkflows => {
                let ws = self.active_workspace();
                let data = self.opts.data_dir.clone();
                let tx = self.events.clone();
                tokio::task::spawn_blocking(move || {
                    let _ = tx.send(AppEvent::Workflows(context::load_workflows(ws.as_deref(), &data)));
                });
            }
            AppCommand::OpenArtifact(name) => {
                let Some(id) = self.current.as_ref().map(|c| c.id) else { return };
                let store = self.store.artifacts(id);
                let tx = self.events.clone();
                tokio::task::spawn_blocking(move || match store.read(&name) {
                    Ok(content) => {
                        let _ = tx.send(AppEvent::Artifact { conversation: id, name, content });
                    }
                    Err(e) => {
                        let _ = tx.send(AppEvent::Error(format!("could not open artifact {name}: {e}")));
                    }
                });
            }
            AppCommand::ResolveFileChange { .. } => {
                tracing::debug!("file change list is not implemented yet");
            }
            AppCommand::SetPluginChoice(id, choice) => {
                let id = id.trim().to_owned();
                if id.is_empty() {
                    return;
                }
                if choice == PluginChoice::Default {
                    self.settings.plugins.remove(&id);
                } else {
                    self.settings.plugins.insert(id, choice);
                }
                let settings = std::mem::take(&mut self.settings);
                self.settings = settings.sanitized();
                self.save_settings();
                // The choice changes prompt content and can start or stop servers, so both
                // follow it; the agent is rebuilt so a running conversation picks it up next.
                self.discover_plugins().await;
                self.reload_mcp().await;
                self.rebuild_agent();
            }
            AppCommand::ReloadMcp => {
                self.reload_mcp().await;
            }
            AppCommand::McpConnected(gen, hub, statuses) => {
                if gen != self.mcp_gen {
                    // A newer reload replaced this connect (a plugin switched off again, the
                    // workspace changed): its servers are no longer configured.
                    if let Some(hub) = hub {
                        tokio::spawn(async move { hub.shutdown().await });
                    }
                    return;
                }
                if let Some(old) = std::mem::replace(&mut self.mcp, hub) {
                    tokio::spawn(async move { old.shutdown().await });
                }
                self.emit(AppEvent::McpStatus(statuses));
                // The next turn sees the new hub's declarations.
                self.rebuild_agent();
            }
            AppCommand::SignInAccount(provider) => self.sign_in_account(provider).await,
            AppCommand::CancelAccountLogin(id) => {
                // Only the attempt the button was drawn for: an older id cancels nothing. The
                // `Cancelled` it then reports is not shown as a failure.
                if let Some(a) = self.account_login.as_ref().filter(|a| a.id == id) {
                    a.cancel.cancel();
                    if let Some(paste) = &self.account_paste_cancel {
                        paste.cancel();
                    }
                }
            }
            AppCommand::PasteAccountLink { login, link } => self.paste_account_link(login, link),
            AppCommand::ForgetAccountLogin(id) => {
                if self.account_paste.as_ref().is_some_and(|a| a.id == id) {
                    self.account_paste = None;
                }
                // A sign-in that still runs keeps its own entry on screen.
                if self.account_login.is_none() && self.account_paste.is_none() {
                    self.emit(AppEvent::AccountLogin { login: None });
                }
            }
            AppCommand::SignOutAccount(id) => self.sign_out_account(id).await,
            AppCommand::UseAccount(id) => {
                if !self.settings.accounts.iter().any(|e| e.id == id) {
                    self.emit(AppEvent::Error(format!("unknown account {id:?}")));
                    return;
                }
                if self.settings.select_account(&id) {
                    self.save_settings();
                    // The next turn must build its backend from the new selection.
                    self.rebuild_agent();
                }
                self.sync_model();
            }
            AppCommand::UseAccountModel { account, model } => self.set_account_model(account, model, true),
            AppCommand::SetAccountModel { account, model } => self.set_account_model(account, model, false),
            AppCommand::CheckAccountModels(id) => self.check_account_models(id),
            AppCommand::AccountLoginFinished(id, _, result) => self.account_login_finished(id, result).await,
            AppCommand::AccountModelsFetched(id, result) => {
                let state = AccountModelsState { checking: false, error: result.err() };
                // Signed out while the fetch ran: nothing is kept for an account settings dropped,
                // but the popup still hears that the check is over.
                if self.settings.accounts.iter().any(|e| e.id == id) {
                    self.account_models.insert(id.clone(), state.clone());
                }
                self.emit(AppEvent::AccountModels { id, state });
                // A successful fetch was written to the account file; the rows read it from there.
                self.emit_accounts();
            }
            AppCommand::AccountExpired(id) => self.account_expired(id),
        }
    }

    /// Re-resolve the client secret and rebuild auth + client, then report the new state.
    async fn rebuild_auth(&mut self) {
        // build_auth reads oauth_client.json with blocking std::fs; keep it off the runtime worker.
        let (dir, settings, http) = (self.opts.data_dir.clone(), self.settings.clone(), self.http.clone());
        let built = tokio::task::spawn_blocking(move || build_auth(&dir, &settings, &http)).await;
        let (auth, client, src) = match built {
            Ok(v) => v,
            Err(e) => {
                self.emit(AppEvent::Error(format!("could not reload credentials: {e}")));
                return;
            }
        };
        self.auth = auth;
        self.client = client;
        self.client_source = src;
        self.drop_agent();
        self.emit(AppEvent::OAuthClientSource(src));
        if let Some(a) = &self.auth {
            let _ = a.load_cached().await;
        }
        if self.imported.is_some() {
            if self.own_sign_in().await {
                // AntiCraft's own sign-in takes over from a reused session.
                self.stop_imported_session();
                self.emit(AppEvent::ImportedSession(ImportedSessionEvent::Stopped));
            } else if let Some(s) = &self.imported {
                // The reused session stays in use; the new client must keep using it.
                self.client = Some(code_assist_client(&self.http, s.tokens.clone(), &self.settings));
            }
        }
        self.emit(AppEvent::Auth(self.auth_status().await));
        // Whether AntiCraft can renew a Gemini CLI session depends on the client secret.
        self.check_imported_session().await;
    }

    /// Drop the agent (rebuilt at the next prompt). A running turn keeps its own reference.
    fn drop_agent(&mut self) {
        self.agent = None;
        self.agent_model = None;
        self.agent_sends_to = None;
        self.agent_steer = None;
    }

    /// The next turn needs a new agent (settings it is built from, plugins or MCP changed).
    /// While a turn runs the agent stays until it ends ([`Self::turn_finished`]): dropping it
    /// now would also drop the steering handle, and "Send now" would queue instead.
    fn rebuild_agent(&mut self) {
        if self.turn_cancel.is_some() {
            self.agent_stale = true;
        } else {
            self.drop_agent();
        }
    }

    /// After login/logout: forget the cached project and session approvals of the old account.
    /// A turn running during the switch keeps its agent; the next turn gets one built for the
    /// new account.
    async fn reset_account_state(&mut self) {
        if let Some(c) = &self.client {
            c.clear_setup_cache().await;
        }
        self.rebuild_agent();
    }

    /// Write settings.json, then echo the settings in effect to the UI. Launch flags stay out of
    /// the file ([`LaunchOverrides`]).
    fn save_settings(&self) {
        if self.keep_settings_file {
            // Writing would replace the file AntiCraft could not read with defaults.
            self.emit(AppEvent::Error(
                "settings.json was not saved: it could not be read when AntiCraft started. Restart AntiCraft once the file can be read.".into(),
            ));
        } else if let Err(e) = self.launch.for_file(&self.settings).save(&self.opts.data_dir) {
            self.emit(AppEvent::Error(format!("could not save settings: {e}")));
        }
        self.emit(AppEvent::Settings(self.settings_seq, self.settings.clone()));
    }

    fn open(&mut self, mut conv: Conversation) {
        self.drop_agent();
        // An opened conversation gets its chosen mode (else the default) and an empty queue.
        self.conv_mode = self.modes.get(&conv.id).copied().unwrap_or(self.settings.agent_mode);
        self.sync_policy();
        if !self.queue.is_empty() {
            self.queue.clear();
            self.emit_queue();
        }
        // Single source of truth: the model chip is what turns use, so an opened conversation
        // adopts it instead of silently keeping its stored model. With a provider active that is
        // the profile's model, which is what the provider will actually send.
        conv.model = self.active_model();
        self.emit(AppEvent::ConversationLoaded(conv.clone()));
        self.current = Some(conv);
    }

    /// The model id shown in the composer chip and handed to the agent, from the backend that
    /// sends turns: the active account's chosen model, else the active provider profile's, else
    /// `settings.model` for Code Assist. A model of one backend is never shown for another.
    fn active_model(&self) -> String {
        match self.settings.active_account_entry() {
            Some(e) => e.model.trim().to_owned(),
            None => match self.settings.active_provider_profile() {
                Some(p) => p.model.trim().to_owned(),
                None => self.settings.model.clone(),
            },
        }
    }

    /// Show [`Self::active_model`] on the open conversation, and hand it to the kept agent when
    /// that agent sends to the backend in use; a running turn reads it at its next request.
    ///
    /// After a switch the kept agent belongs to the backend in use before, so its next request
    /// must not carry the new backend's model id. A turn running on it ends with the model it
    /// has; the next turn gets an agent built for the new backend, with this model.
    fn sync_model(&mut self) {
        let m = self.active_model();
        let same_backend = self.agent_sends_to.as_ref().is_none_or(|b| *b == TurnBackend::of(&self.settings));
        if same_backend {
            if let Some(h) = &self.agent_model {
                *h.lock().unwrap_or_else(|e| e.into_inner()) = m.clone();
            }
        }
        if let Some(c) = &mut self.current {
            c.model = m;
        }
    }

    /// What the next turn runs on, in the order the module docs give: the active account, else
    /// the active provider profile, else Code Assist.
    ///
    /// An account or a profile replaces Code Assist entirely, login included: a user with only an
    /// API key or a subscription has no reason to authorize a Google account.
    fn agent_backend(&self, conv: &Conversation) -> Result<AgentBackend, String> {
        let bypass = self.bypass_route()?;
        if let Some(entry) = self.settings.active_account_entry() {
            let label = entry.provider.label();
            // Selected, but no live token source: a refresh found the session dead and removed
            // its file. Said as itself — "not logged in" would send the user to the Google
            // sign-in, which cannot fix it.
            let Some(auth) = self.accounts.auth(&entry.id) else {
                return Err(format!("The {label} account needs signing in again (Customizations > Account)."));
            };
            let model = entry.model.trim().to_owned();
            if model.is_empty() {
                return Err(format!("The {label} account has no model chosen. Open Customizations > Account and pick one."));
            }
            let profile = self.account_turn_profile(entry, auth.view().plan.as_deref());
            // The account's own window: without one, compaction keeps to Gemini's million tokens
            // and alias resolution would rewrite an account model named `auto`.
            let context_window = Some(profile.context_window());
            // Read from the same object that supplies the token, so the header and the body can
            // never name another account than the bearer does.
            let meta = AccountMeta {
                account_id: auth.wire_account_id().map(str::to_owned),
                project_id: auth.project_id().map(str::to_owned),
            };
            let backend = ProviderBackend::with_oauth(self.http.clone(), profile, auth, meta);
            return Ok(AgentBackend { backend: Arc::new(backend), model, context_window, bypass });
        }
        // A profile reaches `settings` through `Settings::sanitized`, which drops the ones that
        // would fail `validate()`, so an unusable profile cannot be selected here.
        if let Some(profile) = self.settings.active_provider_profile().cloned() {
            let model = profile.model.trim().to_owned();
            // Saved before a model was chosen: said with the way out, before any request.
            if model.is_empty() {
                return Err(format!(
                    "{} has no model chosen. Pick one from the model menu in the composer, after loading its models (Customizations > Providers > Edit).",
                    profile.label.trim()
                ));
            }
            let context_window = Some(profile.context_window());
            return Ok(AgentBackend { backend: Arc::new(ProviderBackend::new(self.http.clone(), profile)), model, context_window, bypass });
        }
        let client = self.client.clone().ok_or("not logged in")?;
        Ok(AgentBackend { backend: Arc::new(client), model: conv.model.clone(), context_window: None, bypass })
    }

    /// Bypass Mode's route for the next turn, built where the settings are.
    ///
    /// `Ok(None)` is the ordinary case: the mode is off, or switched on with no profile to route
    /// to (the UI says so rather than the turn failing). An error is a mode that was set up and
    /// cannot run — no model chosen for the profile, say — and it stops the message before any
    /// request: silently sending it on the normal model would be the opposite of what the mode
    /// was switched on for.
    fn bypass_route(&self) -> Result<Option<crate::agent::bypass::BypassRoute>, String> {
        let cfg = &self.settings.bypass;
        if !cfg.routes() {
            return Ok(None);
        }
        let id = cfg.provider.as_deref().unwrap_or_default();
        let Some(profile) = self.settings.providers.iter().find(|p| p.id == id).cloned() else {
            return Ok(None);
        };
        let model = profile.model.trim().to_owned();
        if model.is_empty() {
            return Err(format!(
                "Bypass Mode uses {}, which has no model chosen. Load its models and pick one in Customizations > Providers > Edit.",
                profile.label.trim()
            ));
        }
        // The profile reaches settings only through `sanitized`, which drops the ones that would
        // fail `validate()`, so its key and endpoint are known to be usable.
        let backend = ProviderBackend::new(self.http.clone(), profile.clone());
        Ok(Some(crate::agent::bypass::BypassRoute {
            backend: Arc::new(backend),
            model,
            label: profile.label.trim().to_owned(),
            requests: cfg.requests,
            instructions: crate::agent::bypass::load_instructions(&cfg.files),
            context_window: profile.context_window(),
        }))
    }

    /// The profile an account's turns run on. Tests send them to their mock server.
    fn account_turn_profile(&self, entry: &AccountEntry, plan: Option<&str>) -> ProviderProfile {
        #[cfg_attr(not(test), allow(unused_mut))]
        let mut profile = account_profile(entry, plan);
        #[cfg(test)]
        if let Some(base) = &self.account_mock {
            profile.base_url = Some(base.clone());
        }
        profile
    }

    fn build_agent(&self, conv: &Conversation) -> Result<Agent, String> {
        // First: a backend that cannot be used is the error worth reporting, ahead of a missing
        // workspace.
        let AgentBackend { backend, model, context_window, bypass } = self.agent_backend(conv)?;
        let ws_path = conv.workspace.clone().or_else(|| self.settings.current_workspace().map(Into::into)).ok_or("choose a workspace folder first")?;
        let ws = Workspace::new(&ws_path).map_err(|e| format!("workspace unavailable: {e}"))?;
        let limits = ToolLimits {
            shell_timeout: std::time::Duration::from_secs(self.settings.shell_timeout_secs),
            shell_output_cap: self.settings.shell_output_cap_bytes,
        };
        let mut system_prompt = prompt::system_prompt(&prompt::PromptContext::gather(ws.root()));
        let rules = context::rules_section(&context::load_rules(ws.root()), ws.root());
        if !rules.is_empty() {
            system_prompt.push_str("\n\n");
            system_prompt.push_str(&rules);
        }
        let skills = context::load_skills_with_plugins(Some(ws.root()), &self.opts.data_dir, &self.plugins);
        let skills_sec = context::skills_section(&skills);
        if !skills_sec.is_empty() {
            system_prompt.push_str("\n\n");
            system_prompt.push_str(&skills_sec);
        }
        let decision_rules =
            context::load_decision_rules_with_plugins(Some(ws.root()), &self.opts.data_dir, &self.plugins);
        let drules_sec = context::decision_rules_section(&decision_rules);
        if !drules_sec.is_empty() {
            system_prompt.push_str("\n\n");
            system_prompt.push_str(&drules_sec);
        }
        let mut cfg = AgentConfig::new(model, system_prompt, conv.id.to_string());
        // An account or a provider is sent its model as typed (`pro` on a gateway is not Gemini
        // Pro), and compaction keeps to its window instead of Gemini's million tokens.
        if context_window.is_some() {
            cfg.resolve_aliases = false;
            cfg.context_window = context_window;
        }
        cfg.skills = skills;
        cfg.decision_rules = decision_rules;
        cfg.hooks = crate::agent::hooks::load_hooks_with_plugins(
            Some(ws.root()),
            Some(&self.opts.data_dir),
            &self.plugins,
        );
        cfg.mcp = self.mcp.clone();
        cfg.bypass = bypass;
        cfg.auto_approve_read_tools = self.settings.auto_approve_read_tools;
        cfg.preview_access = self.settings.preview_access;
        cfg.latest_flash_ga = self.settings.latest_flash_ga;
        let mut agent = Agent::new(backend, ToolRegistry::new(ws, limits), cfg, History::from_contents(conv.messages.clone()))
            .with_policy(self.policy.clone())
            .with_artifact_store(self.store.artifacts(conv.id));
        agent.restore_artifacts(conv.artifacts.clone());
        Ok(agent)
    }

    /// A prompt the UI did not show yet (late queue entry or steer): tell the UI, then run.
    fn start_backend_prompt(&mut self, text: String) {
        if self.current.is_none() {
            let conv = Conversation::new(self.settings.current_workspace().map(Into::into), self.settings.model.clone());
            self.open(conv);
        }
        if let Some(id) = self.current.as_ref().map(|c| c.id) {
            self.emit(AppEvent::PromptStarted { conversation: id, text: text.clone() });
        }
        self.send_prompt(TurnKind::Prompt(text));
    }

    fn send_prompt(&mut self, kind: TurnKind) {
        if self.turn_cancel.is_some() {
            self.emit(AppEvent::TurnRejected("a turn is already running".into()));
            return;
        }
        if self.current.is_none() {
            if matches!(kind, TurnKind::Retry | TurnKind::Compact) {
                self.emit(AppEvent::TurnRejected("open a conversation first".into()));
                return;
            }
            let conv = Conversation::new(self.settings.current_workspace().map(Into::into), self.settings.model.clone());
            self.open(conv);
        }
        self.start_turn(kind);
    }

    /// Start a turn task for the open conversation (no turn may be running).
    fn start_turn(&mut self, kind: TurnKind) {
        let Some(conv) = self.current.as_mut() else { return };
        if conv.workspace.is_none() {
            conv.workspace = self.settings.current_workspace().map(Into::into);
        }
        if let TurnKind::Prompt(t) | TurnKind::PromptWithMode(t, _) | TurnKind::Edit(t, _) = &kind {
            conv.ensure_title(t);
        }
        // Save a checkpoint of the history *before* this turn changes it. The key is the number
        // of user bubbles the history already shows (prompts and steering, never tool results):
        // exactly the position the transcript gives this turn's bubble, so its rewind button
        // finds this snapshot. Checkpoints are best-effort; a failure is logged and never
        // surfaces to the user.
        let user_index = crate::ui::user_bubble_count(&conv.messages);
        if !conv.messages.is_empty() {
            let store = self.store.clone();
            let conv_id = conv.id;
            let messages = conv.messages.clone();
            tokio::task::spawn_blocking(move || {
                if let Err(e) = store.save_checkpoint(conv_id, user_index, &messages) {
                    tracing::debug!("checkpoint save failed: {e}");
                }
            });
        }
        let conv_snapshot = conv.clone();
        if self.agent.is_none() {
            match self.build_agent(&conv_snapshot) {
                Ok(a) => {
                    self.agent_model = Some(a.model_handle());
                    self.agent_sends_to = Some(TurnBackend::of(&self.settings));
                    self.agent_steer = Some(a.steer_sender());
                    self.agent = Some(Arc::new(Mutex::new(a)));
                }
                Err(e) => {
                    self.emit(AppEvent::TurnRejected(e));
                    return;
                }
            }
        }
        let Some(agent) = self.agent.clone() else { return };
        let cancel = self.shutdown.child_token();
        self.turn_cancel = Some(cancel.clone());
        self.turn_id += 1;
        let turn_id = self.turn_id;
        let done = self.turn_done_tx.clone();
        let ui = self.events.clone();
        let threshold = self.settings.compact_threshold;
        tokio::spawn(async move {
            let conversation_id = conv_snapshot.id;
            let mut final_history = None;
            let mut reason = None;
            let (tx, mut rx) = mpsc::unbounded_channel();
            let run = async move {
                let mut a = agent.lock().await;
                match kind {
                    TurnKind::Prompt(text) => {
                        let text = expand_mentions(a.workspace().clone(), text).await;
                        a.compact_if_needed(threshold, &tx, &cancel).await;
                        a.run_prompt(text, tx, cancel).await;
                    }
                    TurnKind::PromptWithMode(text, mode) => {
                        let text = expand_mentions(a.workspace().clone(), text).await;
                        a.compact_if_needed(threshold, &tx, &cancel).await;
                        a.run_prompt_with_mode(text, Some(mode), tx, cancel).await;
                    }
                    TurnKind::Edit(text, mode) => {
                        let text = expand_mentions(a.workspace().clone(), text).await;
                        a.edit_last_prompt(text, mode, tx, cancel).await;
                    }
                    TurnKind::Retry => a.retry_last_prompt(tx, cancel).await,
                    TurnKind::Compact => a.compact(tx, cancel).await,
                }
                // Anything the turn did not inject yet runs as the next prompt.
                (a.artifacts(), a.take_pending_artifact_restore(), a.take_pending_steer())
            };
            let forward = async {
                while let Some(ev) = rx.recv().await {
                    if let AgentEvent::Finished { history, reason: r } = &ev {
                        final_history = Some(history.clone());
                        reason = Some(r.clone());
                    }
                    let _ = ui.send(AppEvent::TurnEvent { conversation: conversation_id, event: ev });
                }
            };
            let ((artifacts, restore, steer), ()) = tokio::join!(run, forward);
            let leftover_steer = steer;
            // Saving happens on the backend loop (see turn_finished).
            let _ = done.send(TurnDone {
                turn_id,
                conversation_id,
                snapshot: Some(conv_snapshot),
                history: final_history,
                reason,
                artifacts: Some(artifacts),
                restore,
                steer: leftover_steer,
            });
        });
        self.refresh_list();
    }

    /// Test hook: use `client` (e.g. pointed at a mock server) instead of the OAuth one.
    #[cfg(test)]
    pub(crate) fn set_client_for_test(&mut self, client: CodeAssistClient) {
        self.client = Some(client);
        self.drop_agent();
    }
}

/// Sessions reused from another Google tool's credential file (Customizations > Account).
///
/// Such a session is used only after the user chose it, never while AntiCraft's own sign-in is
/// in use, and its tokens stay in memory: nothing is written to AntiCraft's credential store or
/// to the other app's file.
impl Backend {
    /// Signed in with AntiCraft's own credentials.
    async fn own_sign_in(&self) -> bool {
        match &self.auth {
            Some(a) => matches!(a.status().await, AuthStatus::LoggedIn { .. }),
            None => false,
        }
    }

    /// Sign-in state for the UI. A reused session counts as signed in.
    async fn auth_status(&self) -> AuthStatus {
        if let Some(s) = &self.imported {
            return AuthStatus::LoggedIn { email: s.tokens.info().await.account };
        }
        match &self.auth {
            Some(a) => a.status().await,
            None => AuthStatus::Error("OAuth client is not configured".into()),
        }
    }

    /// Look for a session another Google tool saved and report it (file IO, off the runtime
    /// worker).
    async fn check_imported_session(&mut self) {
        let roots = self.import_roots.clone();
        let configured = self.auth.is_some();
        match tokio::task::spawn_blocking(move || antigravity::inspect(&roots, configured, chrono::Utc::now())).await {
            Ok(check) => {
                self.import_check = Some(check.clone());
                self.emit(AppEvent::ImportCheck(check));
            }
            Err(e) => tracing::warn!("could not look for an existing Google session: {e}"),
        }
    }

    /// Use the session the last look found. Its file is read again first, since it may have
    /// changed in the meantime.
    async fn use_imported_session(&mut self) {
        if self.imported.is_some() {
            return;
        }
        if self.login_cancel.is_some() {
            self.emit(AppEvent::Error("Finish or cancel the Google sign-in first.".into()));
            return;
        }
        if self.own_sign_in().await {
            self.emit(AppEvent::Error(
                "You are signed in with AntiCraft's own Google sign-in. Sign out first to use another app's session.".into(),
            ));
            return;
        }
        let Some(ImportCheck::Found(found)) = self.import_check.clone() else {
            self.emit(AppEvent::Error("No existing Google session was found. Choose Check again on the Account tab.".into()));
            return;
        };
        let path = found.source.clone();
        let creds = tokio::task::spawn_blocking(move || antigravity::parse_file(&path)).await.ok().flatten();
        let refresh = self.auth.as_ref().map(|a| a.config().clone());
        let now = chrono::Utc::now();
        let creds = match creds {
            Some(c) if ImportSummary::of(&c, refresh.is_some(), now).usable => c,
            other => {
                let why = match &other {
                    Some(c) => ImportSummary::of(c, refresh.is_some(), now).explain(now),
                    None => format!("{} no longer holds a session.", found.source.display()),
                };
                self.check_imported_session().await;
                self.emit(AppEvent::Error(format!("Could not use the existing session. {why}")));
                return;
            }
        };
        let tokens = ImportedTokenSource::new(creds, refresh, self.http.clone());
        self.import_gen += 1;
        let generation = self.import_gen;
        let stop = self.shutdown.child_token();
        // Reports renewals and the end of the session; stops with `stop` (or at shutdown).
        let (watched, watch_stop, tx) = (tokens.clone(), stop.clone(), self.self_tx.clone());
        tokio::spawn(async move {
            while let Some(change) = watched.watch(&watch_stop).await {
                let ended = matches!(change, Watch::Ended(_));
                if tx.send(AppCommand::ImportedSessionChanged(generation, change)).is_err() || ended {
                    break;
                }
            }
        });
        self.client = Some(code_assist_client(&self.http, tokens.clone(), &self.settings));
        // An agent built earlier holds the previous client; the next turn gets a new one.
        self.rebuild_agent();
        self.imported = Some(ImportedSession { tokens: tokens.clone(), generation, stop });
        self.reset_account_state().await;
        let info = tokens.info().await;
        tracing::info!("using the {} session from {}", info.issuer.label(), info.source.display());
        self.emit(AppEvent::Auth(AuthStatus::LoggedIn { email: info.account.clone() }));
        self.emit(AppEvent::ImportedSession(ImportedSessionEvent::Active(info)));
    }

    /// Stop using the imported session and go back to AntiCraft's own client. Returns whether
    /// one was in use. A running turn finishes with the token it has.
    fn stop_imported_session(&mut self) -> bool {
        let Some(s) = self.imported.take() else { return false };
        s.stop.cancel();
        self.client = self.auth.as_ref().map(|a| code_assist_client(&self.http, a.clone(), &self.settings));
        // The agent holds the client it was built with: no later turn may keep the session.
        self.rebuild_agent();
        true
    }

    /// The watcher reported a renewal or the end of the session `generation`.
    async fn imported_session_changed(&mut self, generation: u64, change: Watch) {
        let Some(s) = self.imported.as_ref().filter(|s| s.generation == generation) else { return };
        match change {
            Watch::Renewed(_) => {
                let info = s.tokens.info().await;
                self.emit(AppEvent::ImportedSession(ImportedSessionEvent::Active(info)));
            }
            Watch::Ended(mut reason) => {
                if self.auth.is_none() {
                    reason.push_str(" Google sign-in first needs the OAuth client secret: see Customizations > Account.");
                }
                self.stop_imported_session();
                self.reset_account_state().await;
                // Auth first: its own banner (a missing client secret) must not hide the reason.
                self.emit(AppEvent::Auth(self.auth_status().await));
                self.emit(AppEvent::ImportedSession(ImportedSessionEvent::Ended(reason)));
                // Report what the file holds now.
                self.check_imported_session().await;
            }
        }
    }
}

/// Subscription accounts (Customizations > Account > Accounts).
///
/// The registry owns the account files and their token sources; settings say which accounts are
/// listed, in what order, which one sends turns and with which model. Every change to either goes
/// through here and ends with an `Accounts` event carrying every row whole, so the Account tab
/// never merges anything.
impl Backend {
    /// Merge the account files into settings at startup.
    ///
    /// The files are the truth. A settings entry whose file is gone is dropped, and
    /// `active_account` with it — the rule `active_provider` follows. A file settings does not
    /// list (settings.json was replaced, say) is listed again: its token is on disk, and only a
    /// sign-out may forget it. The file's model list wins; the chosen model stays the one settings
    /// remember, unless they have none.
    ///
    /// A file that is there but could not be read is not a file that is gone. It may be held open
    /// by another program, or damaged; either way the entry stays — its model and its selection
    /// with it — and the row says why the account cannot be used. The next start reads the file
    /// again, and signing in again replaces it.
    fn reconcile_accounts(&mut self, mut files: Vec<AccountView>) {
        let (before, before_active) = (self.settings.accounts.clone(), self.settings.active_account.clone());
        let mut entries = Vec::with_capacity(files.len());
        let mut unreadable = Vec::new();
        for entry in &self.settings.accounts {
            let Some(pos) = files.iter().position(|v| v.id == entry.id) else {
                if self.accounts.store().is_missing(&entry.id) {
                    tracing::info!("account {} has no file any more; it is no longer listed", entry.id);
                } else {
                    tracing::warn!("the file of account {} could not be read; it stays listed", entry.id);
                    unreadable.push(account_row(entry.provider, &entry.id, AccountStatus::Error(UNREADABLE_ACCOUNT.into())));
                    entries.push(entry.clone());
                }
                continue;
            };
            let view = files.remove(pos);
            let mut entry = entry.clone();
            // The id names the file, and the file knows its provider.
            entry.provider = view.provider;
            if !external::valid_model_id(&entry.model) {
                entry.model = view.model.clone();
            }
            entries.push(entry);
        }
        entries.extend(files.iter().map(new_account_entry));
        self.settings.accounts = entries;
        for row in unreadable {
            self.account_views.retain(|v| v.id != row.id);
            self.account_views.push(row);
        }
        // The cap, the duplicate rule and a selection that names no entry any more.
        self.settings = std::mem::take(&mut self.settings).sanitized();
        // Only the account fields decide: launch flags already differ from the file, and they
        // are no reason to write it.
        if self.settings.accounts != before || self.settings.active_account != before_active {
            if self.keep_settings_file {
                // Nothing is written this session, and startup already said why; a second banner
                // would only hide that one.
                self.emit(AppEvent::Settings(self.settings_seq, self.settings.clone()));
            } else {
                self.save_settings();
            }
        }
        self.emit_accounts();
    }

    /// Rebuild the account rows and send them.
    ///
    /// Which accounts, and the model each sends, come from settings; the sign-in state, email,
    /// plan and model list from the live token source. An account without one keeps the status it
    /// was last given (`Expired` after a failed refresh), so its row can say why.
    fn emit_accounts(&mut self) {
        let previous = std::mem::take(&mut self.account_views);
        self.account_views = self
            .settings
            .accounts
            .iter()
            .map(|entry| {
                let mut view = match self.accounts.auth(&entry.id) {
                    Some(auth) => auth.view(),
                    None => previous
                        .iter()
                        .find(|v| v.id == entry.id)
                        .cloned()
                        .unwrap_or_else(|| account_row(entry.provider, &entry.id, AccountStatus::SignedOut)),
                };
                view.model = entry.model.clone();
                view
            })
            .collect();
        let mut rows = self.account_views.clone();
        // A sign-in in flight has no account yet (which one it becomes is known only when it
        // finishes), so its provider gets one more row: that row carries the spinner.
        if let Some(a) = &self.account_login {
            rows.push(account_row(a.provider, "", AccountStatus::SigningIn));
        }
        self.emit(AppEvent::Accounts(rows));
    }

    /// A new attempt. Under test it goes to the test's mock server and binds whatever port is
    /// free: no test reaches a provider or holds a registered callback port (1455, 51121, 54545).
    async fn begin_account_login(&self, provider: AccountProvider) -> Result<Arc<LoginAttempt>, AuthError> {
        #[cfg(test)]
        if let Some(base) = &self.account_mock {
            let endpoints = crate::auth::external::login::ProviderEndpoints::with_base(base);
            let attempt = self.accounts.begin_login_against(provider, endpoints)?;
            let attempt = Arc::try_unwrap(attempt).expect("a fresh attempt has one owner").with_bind_port(0);
            return Ok(Arc::new(attempt));
        }
        self.accounts.begin_login(provider).await
    }

    /// Start a subscription sign-in: one at a time, Google's included, and never mid-turn.
    async fn sign_in_account(&mut self, provider: AccountProvider) {
        if self.turn_cancel.is_some() {
            self.emit(AppEvent::Error("Stop the running turn first".into()));
            return;
        }
        if self.account_login.is_some() || self.login_cancel.is_some() {
            self.emit(AppEvent::Error("Finish or cancel the current sign-in first.".into()));
            return;
        }
        // Only the newest attempt can finish: one kept for a pasted link is let go.
        self.account_paste = None;
        let attempt = match self.begin_account_login(provider).await {
            Ok(a) => a,
            Err(e) => {
                self.emit(AppEvent::Error(format!("Could not start the {} sign-in: {e}", provider.label())));
                return;
            }
        };
        self.account_login = Some(attempt.clone());
        self.emit(AppEvent::AccountLogin { login: Some(login_info(&attempt, None)) });
        self.emit_accounts();
        let (accounts, opener, tx) = (self.accounts.clone(), self.account_opener.clone(), self.self_tx.clone());
        tokio::spawn(async move {
            let (id, shown) = (attempt.id, attempt.clone());
            let result = accounts.run_loopback(attempt, move |url| opener(&shown, url)).await;
            let _ = tx.send(AppCommand::AccountLoginFinished(id, provider, result.map_err(|e| e.to_string())));
        });
    }

    /// Finish the kept attempt `login` with a redirect link the user pasted.
    ///
    /// The attempt's own state and verifier are the ones the page the user opened was built
    /// with, and the state is compared before any code is exchanged. The link carries an
    /// authorization code: it goes to the exchange and nowhere else (no log, no event).
    ///
    /// Cancellable like the browser round trip, with a token of its own: an Antigravity exchange
    /// includes the project discovery, which can poll for a while.
    fn paste_account_link(&mut self, login: u64, link: PastedLink) {
        let Some(attempt) = self.account_paste.take_if(|a| a.id == login) else { return };
        // In flight like any sign-in: one at a time, with its id as the stale-report guard.
        self.account_login = Some(attempt.clone());
        let cancel = CancellationToken::new();
        self.account_paste_cancel = Some(cancel.clone());
        self.emit(AppEvent::AccountLogin { login: Some(login_info(&attempt, None)) });
        self.emit_accounts();
        let (accounts, tx) = (self.accounts.clone(), self.self_tx.clone());
        tokio::spawn(async move {
            let result = accounts.complete_with_pasted(&attempt, link.as_str(), &cancel).await;
            let _ = tx.send(AppCommand::AccountLoginFinished(attempt.id, attempt.provider, result.map_err(|e| e.to_string())));
        });
    }

    /// An attempt reported. Only the one in flight counts: a report whose id no longer matches
    /// comes from an attempt that was replaced, and is dropped.
    async fn account_login_finished(&mut self, id: u64, result: Result<AccountView, String>) {
        let Some(attempt) = self.account_login.take_if(|a| a.id == id) else { return };
        self.account_paste_cancel = None;
        match result {
            Ok(view) => self.account_signed_in(view).await,
            Err(e) => {
                // Cancelling is not a failure: the row goes back to "Sign in" without a message.
                let error = (e != AuthError::Cancelled.to_string()).then_some(e);
                self.emit(AppEvent::AccountLogin { login: Some(login_info(&attempt, error)) });
                if attempt.expired() {
                    // Past the sign-in timeout a pasted link is refused too, so none is offered:
                    // the row keeps the message, and Sign in starts over.
                    self.emit(AppEvent::AccountLogin { login: None });
                } else {
                    // Kept whatever else stopped it: a busy port, or a browser on another
                    // machine, still ends in a redirect link the user can paste.
                    self.account_paste = Some(attempt);
                }
                self.emit_accounts();
            }
        }
    }

    /// A sign-in finished: list the account, or renew its entry, and use it when it is the first
    /// one or nothing else was chosen.
    async fn account_signed_in(&mut self, view: AccountView) {
        self.account_paste = None;
        let id = view.id.clone();
        let first = self.settings.accounts.is_empty();
        let renewed = self.settings.active_account.as_deref() == Some(id.as_str());
        match self.settings.accounts.iter().position(|e| e.id == id) {
            // Signing in again renews the session; the model chosen for the account stays.
            Some(i) => {
                let entry = &mut self.settings.accounts[i];
                entry.provider = view.provider;
                if !external::valid_model_id(&entry.model) {
                    entry.model = view.model.clone();
                }
            }
            None if self.settings.accounts.len() >= external::MAX_ACCOUNTS => {
                // Settings keep only this many, so the next save would drop the new entry while
                // its token file stayed. Signed out again rather than kept where no row shows it.
                if let Err(e) = self.accounts.logout(&id).await {
                    tracing::warn!("could not remove account {id}, which is over the limit: {e}");
                }
                self.emit(AppEvent::Error(format!(
                    "At most {} accounts can be signed in. Sign out of one first.",
                    external::MAX_ACCOUNTS
                )));
                self.emit(AppEvent::AccountLogin { login: None });
                self.emit_accounts();
                return;
            }
            None => self.settings.accounts.push(new_account_entry(&view)),
        }
        let selected = (first || (self.settings.active_account.is_none() && self.settings.active_provider_profile().is_none()))
            && self.settings.select_account(&id);
        self.settings = std::mem::take(&mut self.settings).sanitized();
        self.account_views.retain(|v| v.id != id);
        self.account_views.push(view);
        // The agent holds the token source it was built with. Signing in again replaced that
        // object in the registry, and one left behind would keep refreshing a session the new
        // sign-in superseded; a newly selected account needs its own backend anyway. Any other
        // account is not in the agent, so its "Always allow" choices stay. A turn that runs keeps
        // its agent until it ends.
        if renewed || selected {
            self.rebuild_agent();
            self.sync_model();
        }
        self.emit_accounts();
        self.emit(AppEvent::AccountLogin { login: None });
        self.save_settings();
    }

    /// Sign out of an account: its file is deleted, it is no longer listed, and turns go back to
    /// Code Assist when it was the one in use.
    async fn sign_out_account(&mut self, id: String) {
        if self.turn_cancel.is_some() {
            self.emit(AppEvent::Error("Stop the running turn first".into()));
            return;
        }
        // Not listed: nothing to sign out of (a click on a row that was already removed).
        let Some(provider) = self.settings.accounts.iter().find(|e| e.id == id).map(|e| e.provider) else { return };
        // A sign-in of the same provider can write this very file at any moment (signing in again
        // renews a session in place), and deleting it then would lose the account that sign-in
        // just authorized. Ids carry their provider, so no other sign-in can touch it.
        if self.account_login.as_ref().is_some_and(|a| a.provider == provider) {
            self.emit(AppEvent::Error("Finish or cancel the current sign-in first.".into()));
            return;
        }
        // A task of its own, so a wait cut short never stops it half-way: it first waits for a
        // refresh in flight (whose result must not write the file back), then always finishes.
        // The loop waits for it briefly, to report a failure; a refresh sitting out a rate limit
        // must not hold every other command up. Finishing late is safe: a sign-in that completes
        // meanwhile keeps its file and its token source (the old object only removes what is
        // still its own).
        let (accounts, task_id) = (self.accounts.clone(), id.clone());
        let logout = tokio::spawn(async move { accounts.logout(&task_id).await });
        match tokio::time::timeout(ACCOUNT_SIGN_OUT_WAIT, logout).await {
            Ok(Ok(Ok(()))) => {}
            // The tokens are gone from memory either way; the row goes too, and the next start
            // lists the file again if it is still there.
            Ok(Ok(Err(e))) => self.emit(AppEvent::Error(format!("Could not delete the account file: {e}"))),
            Ok(Err(e)) => tracing::warn!("signing out of {id} failed: {e}"),
            Err(_) => tracing::warn!("signing out of {id} waits for a refresh in flight; it finishes on its own"),
        }
        self.settings.accounts.retain(|e| e.id != id);
        let was_active = self.settings.active_account.as_deref() == Some(id.as_str());
        if was_active {
            self.settings.active_account = None;
        }
        self.account_views.retain(|v| v.id != id);
        self.account_models.remove(&id);
        self.save_settings();
        // Only the account in use is in the agent; signing out of another one keeps it.
        if was_active {
            self.rebuild_agent();
            self.sync_model();
        }
        self.emit_accounts();
    }

    /// Set the model an account sends. With `activate` (a pick in the composer's picker) the
    /// account is put in use as well.
    ///
    /// Another model of the account already in use reaches a kept agent through its shared model
    /// handle, as a Code Assist model does, so the conversation keeps its "Always allow" choices;
    /// only a switch of account builds the next turn's agent anew.
    fn set_account_model(&mut self, account: String, model: String, activate: bool) {
        let model = model.trim().to_owned();
        if !external::valid_model_id(&model) {
            self.emit(AppEvent::Error("That is not a valid model id.".into()));
            return;
        }
        let Some(entry) = self.settings.accounts.iter_mut().find(|e| e.id == account) else {
            self.emit(AppEvent::Error(format!("unknown account {account:?}")));
            return;
        };
        let changed = entry.model != model;
        entry.model = model;
        let switched = activate && self.settings.select_account(&account);
        if changed || switched {
            self.save_settings();
        }
        if switched {
            self.rebuild_agent();
        }
        if self.settings.active_account.as_deref() == Some(account.as_str()) {
            self.sync_model();
        }
        if changed {
            self.emit_accounts();
        }
    }

    /// Put `profile` in use with `model`, as a pick in the composer's menu asks. Another model of
    /// the profile already in use reaches a kept agent through its shared model handle.
    fn use_provider_model(&mut self, profile: String, model: String) {
        let model = model.trim().to_owned();
        if !crate::providers::models::valid_model_id(&model) {
            self.emit(AppEvent::Error("That is not a valid model id.".into()));
            return;
        }
        let Some(p) = self.settings.providers.iter_mut().find(|p| p.id == profile) else {
            self.emit(AppEvent::Error(format!("unknown provider {profile:?}")));
            return;
        };
        let changed = p.model != model;
        p.model = model;
        let switched = self.settings.select_profile(Some(profile));
        if changed || switched {
            self.save_settings();
        }
        if switched {
            self.rebuild_agent();
        }
        self.sync_model();
    }

    /// "Check again": fetch the account's model list off the loop. The registry writes the list to
    /// the account file; settings keep only the chosen model.
    fn check_account_models(&mut self, id: String) {
        if !self.settings.accounts.iter().any(|e| e.id == id) {
            return;
        }
        // One fetch per account at a time (the button is disabled meanwhile anyway).
        if self.account_models.get(&id).is_some_and(|s| s.checking) {
            return;
        }
        // Clearing the previous error is the point of checking again.
        let state = AccountModelsState { checking: true, error: None };
        self.account_models.insert(id.clone(), state.clone());
        self.emit(AppEvent::AccountModels { id: id.clone(), state });
        let label = self.settings.accounts.iter().find(|e| e.id == id).map_or("The provider", |e| e.provider.label());
        let (accounts, tx) = (self.accounts.clone(), self.self_tx.clone());
        tokio::spawn(async move {
            // `AuthError`'s messages never carry a credential.
            let result = match accounts.models(&id, true).await {
                // Nothing is stored for an empty answer (the old list stays), so without a message
                // the button would just stop spinning and the popup would look unchanged.
                Ok(models) if models.is_empty() => Err(format!("{label} listed no models for this account. Try again later.")),
                other => other.map_err(|e| e.to_string()),
            };
            let _ = tx.send(AppCommand::AccountModelsFetched(id, result));
        });
    }

    /// A refresh found this account's session dead. The registry already dropped its token source
    /// and deleted the file; the settings entry stays for this session so the row can say why and
    /// offer Sign in. (A restart drops it: there is no file.)
    fn account_expired(&mut self, id: String) {
        // Stale only when settings no longer list the account. `auth(id)` is `None` on every
        // genuine expiry: the registry removes the dead object before it reports.
        let Some(provider) = self.settings.accounts.iter().find(|e| e.id == id).map(|e| e.provider) else { return };
        let status = AccountStatus::Expired(AuthError::ReauthRequired.to_string());
        match self.account_views.iter_mut().find(|v| v.id == id) {
            Some(v) => v.status = status,
            None => self.account_views.push(account_row(provider, &id, status)),
        }
        // The agent holds the dead token source; the next turn must say what happened instead.
        if self.settings.active_account.as_deref() == Some(id.as_str()) {
            self.rebuild_agent();
        }
        self.emit_accounts();
        self.emit(AppEvent::Settings(self.settings_seq, self.settings.clone()));
    }
}

/// The row of an account whose file is there but could not be read at startup.
const UNREADABLE_ACCOUNT: &str = "Its account file could not be read. Sign in again to replace it.";

/// A row for an account with no live token source (or for a sign-in in flight, with an empty
/// id): nothing is known about it beyond its provider and its state.
fn account_row(provider: AccountProvider, id: &str, status: AccountStatus) -> AccountView {
    AccountView {
        id: id.to_owned(),
        provider,
        email: None,
        plan: None,
        model: String::new(),
        models: Vec::new(),
        models_fetched_at: None,
        status,
    }
}

/// Most messages kept in the queue.
const MAX_QUEUE: usize = 20;

/// Longest the command loop waits for a sign-out to report. The sign-out itself always finishes:
/// it runs on a task of its own, after any refresh in flight (each token request is bounded by
/// `REFRESH_TIMEOUT`). Short, because every other command waits with it; an uncontended sign-out
/// is one file delete.
const ACCOUNT_SIGN_OUT_WAIT: Duration = Duration::from_secs(2);

/// Longest wait at exit for a cancelled turn to report, so it is saved. With the MCP grace
/// period after it, this stays inside the 5 s `main` waits for the backend.
const SHUTDOWN_TURN_WAIT: Duration = Duration::from_millis(2500);

/// Read one `mcp_config.json`. A missing file is normal; an unreadable or malformed one is
/// logged and skipped, because one bad document must not disable MCP for the whole session.
fn read_json_file(path: &std::path::Path) -> Option<serde_json::Value> {
    let raw = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::warn!("mcp: cannot read {}: {e}", path.display());
            return None;
        }
    };
    match serde_json::from_slice(&raw) {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!("mcp: {} is not valid JSON: {e}", path.display());
            None
        }
    }
}

/// The customizations page keeps its own flat row; this is the one place the mapping from the
/// discovery-time shape to the displayed fields is written down.
impl From<PluginInfo> for crate::ui::PluginRow {
    fn from(p: PluginInfo) -> Self {
        Self {
            id: p.id,
            name: p.name,
            version: p.version,
            description: p.description,
            enabled: p.enabled,
            source: p.source,
            skill_count: p.skill_count,
            mcp_servers: p.mcp_servers,
        }
    }
}

/// Attach `@` mentions off the runtime worker; the plain text is kept if the task fails.
async fn expand_mentions(ws: Workspace, text: String) -> String {
    let fallback = text.clone();
    tokio::task::spawn_blocking(move || context::expand_mentions(&ws, &text)).await.unwrap_or(fallback)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn backend(dir: &std::path::Path) -> (Backend, mpsc::UnboundedReceiver<AppEvent>, CancellationToken) {
        let (tx, rx) = mpsc::unbounded_channel();
        let (self_tx, _self_rx) = mpsc::unbounded_channel();
        let shutdown = CancellationToken::new();
        let opts = AppOptions {
            data_dir: dir.to_path_buf(),
            workspace: None,
            model: Some("test-model".into()),
            mode: None,
            skip_permissions: false,
        };
        (Backend::new(opts, shutdown.clone(), tx, self_tx), rx, shutdown)
    }

    fn drain(rx: &mut mpsc::UnboundedReceiver<AppEvent>) -> Vec<AppEvent> {
        let mut v = Vec::new();
        while let Ok(e) = rx.try_recv() {
            v.push(e);
        }
        v
    }

    #[tokio::test]
    async fn model_flag_overrides_settings() {
        let dir = tempfile::tempdir().unwrap();
        let (mut b, mut rx, _) = backend(dir.path());
        assert_eq!(b.settings.model, "test-model");
        b.handle(AppCommand::NewChat).await;
        let evs = drain(&mut rx);
        assert!(evs.iter().any(|e| matches!(e, AppEvent::ConversationLoaded(c) if c.model == "test-model")));
    }

    #[tokio::test]
    async fn set_model_while_agent_is_locked_does_not_block() {
        let dir = tempfile::tempdir().unwrap();
        let (mut b, _rx, _) = backend(dir.path());
        let handle: SharedModel = Arc::new(std::sync::Mutex::new("gemini-2.5-pro".into()));
        b.agent_model = Some(handle.clone());
        // Simulate a turn holding the agent lock for the whole call.
        let lock = Arc::new(Mutex::new(()));
        let _held = lock.lock().await;
        tokio::time::timeout(Duration::from_secs(2), b.handle(AppCommand::SetModel("gemini-3.5-flash".into())))
            .await
            .expect("SetModel must not wait for the running turn");
        assert_eq!(*handle.lock().unwrap(), "gemini-3.5-flash");
        tokio::time::timeout(Duration::from_secs(2), b.handle(AppCommand::Stop)).await.unwrap();
    }

    #[tokio::test]
    async fn deleting_running_conversation_cancels_and_is_not_recreated() {
        let dir = tempfile::tempdir().unwrap();
        let (mut b, _rx, _) = backend(dir.path());
        b.handle(AppCommand::NewChat).await;
        let mut conv = b.current.clone().unwrap();
        b.store.save(&mut conv).unwrap();
        let token = b.shutdown.child_token();
        b.turn_cancel = Some(token.clone());
        b.turn_id = 1;
        b.handle(AppCommand::DeleteConversation(conv.id)).await;
        assert!(token.is_cancelled());
        let history = vec![crate::api::types::Content::user_text("hello")];
        b.turn_finished(TurnDone { turn_id: 1, conversation_id: conv.id, snapshot: Some(conv.clone()), history: Some(history), reason: None, artifacts: None, restore: Vec::new(), steer: Vec::new() });
        assert!(b.store.load(conv.id).is_err(), "deleted conversation must not be recreated");
    }

    #[tokio::test]
    async fn an_account_switch_during_a_turn_rebuilds_the_agent_after_it() {
        let dir = tempfile::tempdir().unwrap();
        let (mut b, _rx, _) = backend(dir.path());
        b.turn_cancel = Some(b.shutdown.child_token());
        b.reset_account_state().await;
        assert!(b.agent_stale, "the next turn must not reuse the old account's agent");
        b.turn_cancel = None;
        b.agent_stale = false;
        b.reset_account_state().await;
        assert!(!b.agent_stale, "with no turn running the agent is dropped at once");
    }

    #[tokio::test]
    async fn rename_during_turn_is_kept_on_save() {
        let dir = tempfile::tempdir().unwrap();
        let (mut b, _rx, _) = backend(dir.path());
        b.handle(AppCommand::NewChat).await;
        let mut conv = b.current.clone().unwrap();
        b.store.save(&mut conv).unwrap();
        let snapshot = conv.clone();
        b.turn_id = 1;
        b.handle(AppCommand::RenameConversation(conv.id, "Renamed".into())).await;
        let history = vec![crate::api::types::Content::user_text("hello")];
        b.turn_finished(TurnDone { turn_id: 1, conversation_id: conv.id, snapshot: Some(snapshot), history: Some(history), reason: None, artifacts: None, restore: Vec::new(), steer: Vec::new() });
        let stored = b.store.load(conv.id).unwrap();
        assert_eq!(stored.title, "Renamed");
        assert_eq!(stored.messages.len(), 1);
    }

    #[tokio::test]
    async fn stale_login_result_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let (mut b, mut rx, _) = backend(dir.path());
        b.login_id = 2;
        b.handle(AppCommand::LoginFinished(1, AuthStatus::Error("login cancelled".into()))).await;
        assert!(drain(&mut rx).is_empty());
        b.handle(AppCommand::LoginFinished(2, AuthStatus::LoggedOut)).await;
        assert!(drain(&mut rx).iter().any(|e| matches!(e, AppEvent::Auth(AuthStatus::LoggedOut))));
    }

    /// A backend that never answers; only used to give `Backend::agent` something to hold.
    struct StubBackend;

    impl ModelBackend for StubBackend {
        fn stream_turn(
            &self,
            _model: String,
            _user_prompt_id: String,
            _request: crate::api::GenerateContentRequest,
            _cancel: CancellationToken,
        ) -> futures::stream::BoxStream<'static, Result<crate::api::CaGenerateResponse, crate::api::CaError>> {
            use futures::StreamExt as _;
            futures::stream::empty().boxed()
        }
    }

    fn provider_profile() -> crate::providers::ProviderProfile {        crate::providers::ProviderProfile {
            id: "p1".into(),
            label: "Test provider".into(),
            kind: crate::providers::ProviderKind::OpenAi,
            model: "gpt-4o-mini".into(),
            base_url: None,
            api_key: "sk-test-key".into(),
            max_output_tokens: None,
            context_window: None,
            models: Vec::new(),
        }
    }

    #[tokio::test]
    async fn an_active_provider_needs_no_login_and_supplies_the_model() {
        let dir = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let (mut b, _rx, _) = backend(dir.path());
        // The conversation carries a Code Assist model; a provider must override it.
        let conv = Conversation::new(Some(ws.path().into()), "gemini-2.5-pro");
        // No client and no profile: the Code Assist path keeps its login requirement.
        let err = b.build_agent(&conv).err().expect("Code Assist still requires login");
        assert_eq!(err, "not logged in");

        b.settings.providers.push(provider_profile());
        b.settings.active_provider = Some("p1".into());
        let agent = b.build_agent(&conv).expect("a provider profile must not require login");
        let model = agent.model_handle();
        assert_eq!(*model.lock().unwrap(), "gpt-4o-mini", "the profile's model is what the turn uses");

        // A selection that names nothing is Code Assist again.
        b.settings.active_provider = Some("missing".into());
        assert!(b.build_agent(&conv).is_err());
        b.settings.active_provider = None;
        assert!(b.build_agent(&conv).is_err());
    }

    #[tokio::test]
    async fn set_active_provider_persists_and_drops_the_agent() {
        let dir = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let (mut b, mut rx, _) = backend(dir.path());
        b.settings.remember_workspace(ws.path());
        b.handle(AppCommand::NewChat).await;
        drain(&mut rx);
        b.settings.providers.push(provider_profile());
        // Built on the Code Assist path (no client -> cannot build): the switch must drop it.
        let conv = b.current.clone().unwrap();
        let built = b.build_agent(&conv).err();
        assert_eq!(built, Some("not logged in".to_owned()));
        b.agent = Some(Arc::new(Mutex::new(Agent::new(
            Arc::new(StubBackend),
            ToolRegistry::new(Workspace::new(ws.path()).unwrap(), ToolLimits::default()),
            AgentConfig::new("test-model", String::new(), conv.id.to_string()),
            History::default(),
        ))));

        b.handle(AppCommand::SetActiveProvider(Some("p1".into()))).await;
        assert!(b.agent.is_none(), "switching backends rebuilds the agent");
        assert_eq!(b.settings.active_provider.as_deref(), Some("p1"));
        assert_eq!(b.current.as_ref().unwrap().model, "gpt-4o-mini", "the profile's model");

        b.handle(AppCommand::SetActiveProvider(Some("nope".into()))).await;
        assert_eq!(b.settings.active_provider.as_deref(), Some("p1"), "an unknown id is refused");
        assert!(drain(&mut rx).iter().any(|e| matches!(e, AppEvent::Error(_))));

        b.handle(AppCommand::SetActiveProvider(None)).await;
        assert_eq!(b.settings.active_provider, None);
        assert_eq!(Settings::load(dir.path()).active_provider, None);
        assert_eq!(b.current.as_ref().unwrap().model, "test-model", "back on the Code Assist model");
    }

    #[tokio::test]
    async fn new_chat_and_errors_without_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let (mut b, mut rx, _) = backend(dir.path());
        b.handle(AppCommand::NewChat).await;
        let evs = drain(&mut rx);
        assert!(evs.iter().any(|e| matches!(e, AppEvent::ConversationLoaded(c) if c.model == "test-model")));
        b.handle(AppCommand::SendPrompt("hi".into())).await;
        let evs = drain(&mut rx);
        assert!(evs.iter().any(|e| matches!(e, AppEvent::TurnRejected(_))));
        assert!(b.turn_cancel.is_none(), "no turn should be running");
    }

    #[tokio::test]
    async fn stop_cancels_turn_and_completion_clears_it() {
        let dir = tempfile::tempdir().unwrap();
        let (mut b, mut rx, _) = backend(dir.path());
        b.handle(AppCommand::NewChat).await;
        let conv_id = b.current.as_ref().unwrap().id;
        let token = b.shutdown.child_token();
        b.turn_cancel = Some(token.clone());
        b.turn_id = 3;
        b.handle(AppCommand::Stop).await;
        assert!(token.is_cancelled());
        assert!(b.turn_cancel.is_some(), "stays running until the task reports back");
        // Stale completion is ignored.
        b.turn_finished(TurnDone { turn_id: 2, conversation_id: conv_id, snapshot: None, history: None, reason: None, artifacts: None, restore: Vec::new(), steer: Vec::new() });
        assert!(b.turn_cancel.is_some());
        let history = vec![crate::api::types::Content::user_text("hello")];
        b.turn_finished(TurnDone { turn_id: 3, conversation_id: conv_id, snapshot: None, history: Some(history), reason: None, artifacts: None, restore: Vec::new(), steer: Vec::new() });
        assert!(b.turn_cancel.is_none());
        assert_eq!(b.current.as_ref().unwrap().messages.len(), 1);
        assert!(drain(&mut rx).iter().any(|e| matches!(e, AppEvent::Conversations(_))));
    }

    #[tokio::test]
    async fn steer_reaches_the_running_turn_exactly_once() {
        let dir = tempfile::tempdir().unwrap();
        let (mut b, mut rx, _) = backend(dir.path());
        b.handle(AppCommand::NewChat).await;
        drain(&mut rx);
        let handle = SteerHandle::new();
        b.agent_steer = Some(handle.clone());
        b.turn_cancel = Some(b.shutdown.child_token());
        b.handle(AppCommand::SteerPrompt("use b".into())).await;
        assert_eq!(handle.take(), vec!["use b".to_string()], "handed to the turn, not queued");
        assert!(b.queue.is_empty());
        assert!(drain(&mut rx).is_empty(), "nothing re-sent to the UI");
    }

    #[tokio::test]
    async fn steer_after_the_turn_closed_is_queued_and_not_lost() {
        let dir = tempfile::tempdir().unwrap();
        let (mut b, mut rx, mut self_rx) = backend_with_self_rx(dir.path());
        b.handle(AppCommand::NewChat).await;
        let conv_id = b.current.as_ref().unwrap().id;
        drain(&mut rx);
        let handle = SteerHandle::new();
        b.agent_steer = Some(handle.clone());
        b.turn_id = 1;
        b.turn_cancel = Some(b.shutdown.child_token());
        // The turn is finishing: it closed its input.
        handle.close();
        b.handle(AppCommand::SteerPrompt("too late".into())).await;
        assert!(b.queue.iter().any(|(_, t)| t == "too late"), "queued instead of lost");
        let evs = drain(&mut rx);
        assert!(evs.iter().any(|e| matches!(e, AppEvent::Queue(q) if q.iter().any(|(_, t)| t == "too late"))));
        // The turn ended without the text: it leads the queue, ahead of what was queued before
        // it, and runs even though the turn did not finish with `Completed`.
        b.queue.push_back((99, "queued earlier".into()));
        b.turn_finished(TurnDone {
            turn_id: 1,
            conversation_id: conv_id,
            snapshot: None,
            history: Some(vec![crate::api::types::Content::user_text("hi")]),
            reason: Some(FinishReason::Cancelled),
            artifacts: None,
            restore: Vec::new(),
            steer: vec!["too late".into()],
        });
        while let Ok(cmd) = self_rx.try_recv() {
            b.handle(cmd).await;
        }
        let evs = drain(&mut rx);
        let started: Vec<String> = evs
            .iter()
            .filter_map(|e| match e {
                AppEvent::PromptStarted { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(started, vec!["too late".to_string()], "the late steer runs next");
        // Nothing is running after the stop, so the prompt was tried right away; this test has
        // no workspace, so the backend answers with `TurnRejected` — but the text is not lost:
        // no backend turn ran, and the steer is out of the queue (no second run when a turn
        // does start).
        assert!(evs.iter().any(|e| matches!(e, AppEvent::TurnRejected(_))));
    }

    #[tokio::test]
    async fn rollback_deletes_the_artifacts_of_the_replaced_turn() {
        let dir = tempfile::tempdir().unwrap();
        let (mut b, _rx, _) = backend(dir.path());
        b.handle(AppCommand::NewChat).await;
        let conv_id = b.current.as_ref().unwrap().id;
        let store = b.store.artifacts(conv_id);
        store.write_versioned("walkthrough", "old", None).unwrap();
        b.turn_id = 1;
        b.turn_finished(TurnDone {
            turn_id: 1,
            conversation_id: conv_id,
            snapshot: None,
            history: None,
            reason: None,
            artifacts: None,
            restore: vec![crate::agent::artifacts::ArtifactMeta {
                name: "walkthrough".into(),
                kind: crate::agent::artifacts::ArtifactKind::Walkthrough,
                summary: String::new(),
                version: 1,
                review: crate::agent::artifacts::ReviewState::None,
                updated_at: chrono::Utc::now(),
            }],
            steer: Vec::new(),
        });
        // The deletion runs on a blocking task: wait for the file to disappear.
        for _ in 0..100 {
            if !store.path("walkthrough").unwrap().exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!store.path("walkthrough").unwrap().exists(), "the dropped turn's artifact is gone");
    }

    #[tokio::test]
    async fn shutdown_ends_run_and_cancels_in_flight() {
        let dir = tempfile::tempdir().unwrap();
        let (mut b, _rx, shutdown) = backend(dir.path());
        let turn = shutdown.child_token();
        let login = CancellationToken::new();
        b.turn_cancel = Some(turn.clone());
        b.login_cancel = Some(login.clone());
        let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(b.run(cmd_rx));
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
        assert!(turn.is_cancelled());
        assert!(login.is_cancelled());
    }

    /// `AntiCraftApp::on_exit` sends `SaveSettings` and `Shutdown`, then cancels the token
    /// right away. The save must reach the file every time, not only when the loop happens to
    /// poll the command channel first.
    #[tokio::test]
    async fn settings_saved_at_exit_survive_the_cancelled_token() {
        for round in 0..40 {
            let dir = tempfile::tempdir().unwrap();
            let (b, mut rx, shutdown) = backend(dir.path());
            let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
            let task = tokio::spawn(b.run(cmd_rx));
            // Wait until startup is done and the loop is idle (the MCP status ends startup).
            loop {
                let ev = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.expect("startup timed out").expect("backend gone");
                if matches!(ev, AppEvent::McpStatus(_)) {
                    break;
                }
            }
            let base = Settings::load(dir.path());
            let mut edited = base.clone();
            edited.model = format!("edited-{round}");
            cmd_tx.send(AppCommand::SaveSettings { seq: 1, base: Box::new(base), settings: Box::new(edited) }).unwrap();
            cmd_tx.send(AppCommand::Shutdown).unwrap();
            shutdown.cancel();
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            assert_eq!(Settings::load(dir.path()).model, format!("edited-{round}"), "round {round}: the exit save was lost");
        }
    }

    /// The `SaveSettings` the UI sends for `edited`, made from the backend's last echo.
    fn ui_save(b: &Backend, edited: Settings) -> AppCommand {
        AppCommand::SaveSettings { seq: b.settings_seq + 1, base: Box::new(b.settings.clone()), settings: Box::new(edited) }
    }

    /// [`backend`] that also returns the receiving end of `self_tx`, for commands the backend
    /// sends itself.
    fn backend_with_self_rx(
        dir: &std::path::Path,
    ) -> (Backend, mpsc::UnboundedReceiver<AppEvent>, mpsc::UnboundedReceiver<AppCommand>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let (self_tx, self_rx) = mpsc::unbounded_channel();
        let opts = AppOptions { data_dir: dir.to_path_buf(), workspace: None, model: Some("test-model".into()), mode: None, skip_permissions: false };
        (Backend::new(opts, CancellationToken::new(), tx, self_tx), rx, self_rx)
    }

    /// An agent on a backend that never answers, for the open conversation.
    fn stub_agent(ws: &std::path::Path, conv: &Conversation) -> Arc<Mutex<Agent>> {
        Arc::new(Mutex::new(Agent::new(
            Arc::new(StubBackend),
            ToolRegistry::new(Workspace::new(ws).unwrap(), ToolLimits::default()),
            AgentConfig::new("test-model", String::new(), conv.id.to_string()),
            History::default(),
        )))
    }

    #[tokio::test]
    async fn an_unreadable_settings_file_is_reported_at_startup() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(Settings::path(dir.path()), b"{\"model\": \"kept\",}").unwrap();
        let (b, mut rx, _) = backend(dir.path());
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(b.run(cmd_rx));
        let mut errors = Vec::new();
        loop {
            match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.expect("startup timed out").expect("backend gone") {
                AppEvent::Error(e) => errors.push(e),
                AppEvent::McpStatus(_) => break,
                _ => {}
            }
        }
        cmd_tx.send(AppCommand::Shutdown).unwrap();
        tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
        assert!(errors.iter().any(|e| e.contains("settings.json")), "{errors:?}");
    }

    /// Another process held settings.json without sharing while AntiCraft started.
    #[cfg(windows)]
    #[tokio::test]
    async fn a_settings_file_that_could_not_be_read_is_not_overwritten() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        Settings { model: "file-model".into(), providers: vec![provider_profile()], ..Settings::default() }.save(dir.path()).unwrap();
        let before = std::fs::read(Settings::path(dir.path())).unwrap();
        let held = std::fs::OpenOptions::new().read(true).share_mode(0).open(Settings::path(dir.path())).unwrap();
        let (mut b, mut rx, _) = backend(dir.path());
        drop(held);
        b.handle(AppCommand::SetWorkspace(ws.path().into())).await;
        assert!(std::fs::read(Settings::path(dir.path())).unwrap() == before, "the file AntiCraft could not read is left as it was");
        assert!(drain(&mut rx).iter().any(|e| matches!(e, AppEvent::Error(m) if m.contains("settings.json"))));
    }

    #[tokio::test]
    async fn launch_flags_are_not_written_to_settings_json() {
        let dir = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let file = Settings { model: "file-model".into(), ..Settings::default() };
        file.save(dir.path()).unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        let (self_tx, _self_rx) = mpsc::unbounded_channel();
        let opts = AppOptions {
            data_dir: dir.path().to_path_buf(),
            workspace: None,
            model: Some("cli-model".into()),
            mode: Some(crate::ExecutionMode::Auto),
            skip_permissions: false,
        };
        let mut b = Backend::new(opts, CancellationToken::new(), tx, self_tx);
        assert_eq!(b.settings.terminal_policy, crate::settings::TerminalPolicy::Auto, "the flag applies to this run");
        assert_eq!(b.settings.model, "cli-model");
        // Any save writes the file: here, opening a folder.
        b.handle(AppCommand::SetWorkspace(ws.path().into())).await;
        let saved = Settings::load(dir.path());
        assert_eq!(saved.terminal_policy, file.terminal_policy);
        assert_eq!(saved.auto_approve_edits, file.auto_approve_edits);
        assert_eq!(saved.agent_mode, file.agent_mode);
        assert_eq!(saved.model, "file-model");
        assert_eq!(saved.workspace_history, vec![ws.path().to_path_buf()], "the save itself happened");
        assert_eq!(b.settings.terminal_policy, crate::settings::TerminalPolicy::Auto, "and the run still uses the flag");
        // A field the user changes during the run is saved as chosen; the others stay the file's.
        let mut ui = b.settings.clone();
        ui.terminal_policy = crate::settings::TerminalPolicy::Turbo;
        b.handle(ui_save(&b, ui)).await;
        let saved = Settings::load(dir.path());
        assert_eq!(saved.terminal_policy, crate::settings::TerminalPolicy::Turbo);
        assert_eq!(saved.auto_approve_edits, file.auto_approve_edits);
    }

    #[tokio::test]
    async fn a_save_sent_with_a_plugin_toggle_keeps_the_new_choice() {
        let dir = tempfile::tempdir().unwrap();
        let (mut b, _rx, _) = backend(dir.path());
        // The UI's copy: it never edits plugin choices, so it holds the map from before the
        // toggle. A shell timeout typed in the same frame is flushed with it.
        let base = b.settings.clone();
        let mut ui = base.clone();
        ui.shell_timeout_secs = 300;
        b.handle(AppCommand::SetPluginChoice("tools".into(), PluginChoice::Disabled)).await;
        b.handle(AppCommand::SaveSettings { seq: 1, base: Box::new(base), settings: Box::new(ui) }).await;
        assert_eq!(b.settings.shell_timeout_secs, 300, "the UI's edit is applied");
        assert_eq!(b.settings.plugins.get("tools"), Some(&PluginChoice::Disabled));
        assert_eq!(Settings::load(dir.path()).plugins.get("tools"), Some(&PluginChoice::Disabled));
    }

    #[tokio::test]
    async fn closing_the_window_waits_for_the_running_turn_and_saves_it() {
        let dir = tempfile::tempdir().unwrap();
        let (mut b, _rx, _) = backend(dir.path());
        // A new chat exists only in memory until its first turn is saved.
        b.handle(AppCommand::NewChat).await;
        let conv = b.current.clone().unwrap();
        let token = b.shutdown.child_token();
        b.turn_cancel = Some(token.clone());
        b.turn_id = 1;
        let done = b.turn_done_tx.clone();
        // The turn task: cancelled by the shutdown, it unwinds for a while, then reports.
        tokio::spawn(async move {
            token.cancelled().await;
            tokio::time::sleep(Duration::from_millis(300)).await;
            let history = vec![
                crate::api::types::Content::user_text("first prompt"),
                crate::api::types::Content::model(vec![crate::api::types::Part::text("[response cancelled]")]),
            ];
            let _ = done.send(TurnDone {
                turn_id: 1,
                conversation_id: conv.id,
                snapshot: Some(conv),
                history: Some(history),
                reason: Some(FinishReason::Cancelled),
                artifacts: None,
                restore: Vec::new(),
                steer: Vec::new(),
            });
        });
        let id = b.current.as_ref().unwrap().id;
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(b.run(cmd_rx));
        cmd_tx.send(AppCommand::Shutdown).unwrap();
        tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
        let stored = ConversationStore::new(dir.path()).load(id).expect("the conversation was written");
        assert_eq!(stored.messages.len(), 2);
    }

    #[tokio::test]
    async fn a_save_that_does_not_change_how_the_agent_is_built_keeps_it() {
        let dir = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let (mut b, _rx, _) = backend(dir.path());
        b.handle(AppCommand::NewChat).await;
        let conv = b.current.clone().unwrap();
        b.agent = Some(stub_agent(ws.path(), &conv));
        // Theme and policy: the policy reaches a live agent through the shared handle.
        let mut s = b.settings.clone();
        s.theme = crate::settings::Theme::Light;
        s.terminal_policy = crate::settings::TerminalPolicy::Turbo;
        b.handle(ui_save(&b, s)).await;
        assert!(b.agent.is_some(), "the conversation keeps its agent, and its \"Always allow\" choices with it");
        // A setting the agent is built from rebuilds it.
        let mut s = b.settings.clone();
        s.shell_timeout_secs = 30;
        b.handle(ui_save(&b, s)).await;
        assert!(b.agent.is_none());
    }

    #[tokio::test]
    async fn a_rebuild_asked_for_during_a_turn_waits_for_it_and_steering_still_arrives() {
        let dir = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let (mut b, _rx, _) = backend(dir.path());
        b.handle(AppCommand::NewChat).await;
        let conv = b.current.clone().unwrap();
        b.agent = Some(stub_agent(ws.path(), &conv));
        let steer = SteerHandle::new();
        b.agent_steer = Some(steer.clone());
        b.turn_cancel = Some(b.shutdown.child_token());
        b.turn_id = 1;
        let mut s = b.settings.clone();
        s.shell_timeout_secs = 30;
        b.handle(ui_save(&b, s)).await;
        b.handle(AppCommand::McpConnected(b.mcp_gen, None, Vec::new())).await;
        b.handle(AppCommand::SteerPrompt("use b".into())).await;
        assert_eq!(steer.take(), vec!["use b".to_string()], "\"Send now\" still reaches the running turn");
        assert!(b.queue.is_empty());
        // The next turn gets a fresh agent.
        b.turn_finished(TurnDone { turn_id: 1, conversation_id: conv.id, snapshot: None, history: None, reason: Some(FinishReason::Completed), artifacts: None, restore: Vec::new(), steer: Vec::new() });
        assert!(b.agent.is_none());
    }

    #[tokio::test]
    async fn a_connect_overtaken_by_a_newer_reload_is_shut_down_not_installed() {
        let dir = tempfile::tempdir().unwrap();
        let (mut b, _rx, _) = backend(dir.path());
        // Reload 1 started a connect (a plugin was switched on); reload 2 found nothing to start
        // (switched off again) before that connect answered.
        b.handle(AppCommand::ReloadMcp).await;
        b.handle(AppCommand::ReloadMcp).await;
        let stale = McpHub::test_hub("srv", "tool");
        b.handle(AppCommand::McpConnected(1, Some(stale.clone()), vec![("srv".into(), true, 1)])).await;
        assert!(b.mcp.is_none(), "the configuration no longer has that server");
        let mut shut = false;
        for _ in 0..100 {
            if matches!(stale.call("mcp__srv__tool", serde_json::json!({}), &CancellationToken::new()).await, Err(crate::agent::mcp::McpError::Cancelled)) {
                shut = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(shut, "its servers are closed");
    }

    #[tokio::test]
    async fn a_chip_removed_while_the_turn_ends_does_not_run() {
        let dir = tempfile::tempdir().unwrap();
        let (mut b, mut rx, mut self_rx) = backend_with_self_rx(dir.path());
        b.handle(AppCommand::NewChat).await;
        let conv_id = b.current.as_ref().unwrap().id;
        b.turn_id = 1;
        b.turn_cancel = Some(b.shutdown.child_token());
        b.handle(AppCommand::QueuePrompt("removed".into())).await;
        let id = b.queue.front().unwrap().0;
        drain(&mut rx);
        // The turn ends while the user's Remove click already waits in the command channel.
        b.turn_finished(TurnDone { turn_id: 1, conversation_id: conv_id, snapshot: None, history: None, reason: Some(FinishReason::Completed), artifacts: None, restore: Vec::new(), steer: Vec::new() });
        b.handle(AppCommand::ClearQueue(Some(id))).await;
        while let Ok(cmd) = self_rx.try_recv() {
            b.handle(cmd).await;
        }
        assert!(!drain(&mut rx).iter().any(|e| matches!(e, AppEvent::PromptStarted { .. })), "the removed message never runs");
    }

    #[tokio::test]
    async fn steer_on_a_chip_runs_that_message_next() {
        let dir = tempfile::tempdir().unwrap();
        let (mut b, mut rx, _) = backend(dir.path());
        b.handle(AppCommand::NewChat).await;
        // The turn is finishing: it no longer reads steering.
        let handle = SteerHandle::new();
        handle.close();
        b.agent_steer = Some(handle);
        b.turn_id = 1;
        b.turn_cancel = Some(b.shutdown.child_token());
        b.handle(AppCommand::QueuePrompt("first".into())).await;
        b.handle(AppCommand::QueuePrompt("second".into())).await;
        let second = b.queue.back().unwrap().0;
        b.handle(AppCommand::SteerQueued(second)).await;
        let order: Vec<&str> = b.queue.iter().map(|(_, t)| t.as_str()).collect();
        assert_eq!(order, ["second", "first"], "it runs right after the turn, ahead of the rest");
        // Nothing runs (a queue kept after Stop): Steer runs the message clicked, now.
        b.turn_cancel = None;
        drain(&mut rx);
        let first = b.queue.back().unwrap().0;
        b.handle(AppCommand::SteerQueued(first)).await;
        let started: Vec<String> = drain(&mut rx)
            .into_iter()
            .filter_map(|e| match e {
                AppEvent::PromptStarted { text, .. } => Some(text),
                _ => None,
            })
            .collect();
        assert_eq!(started, ["first"]);
    }

    #[tokio::test]
    async fn a_provider_model_named_like_an_alias_is_sent_as_typed() {
        let dir = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let (mut b, _rx, _) = backend(dir.path());
        let mut p = provider_profile();
        p.model = "pro".into();
        b.settings.providers.push(p);
        b.settings.active_provider = Some("p1".into());
        let agent = b.build_agent(&Conversation::new(Some(ws.path().into()), "gemini-2.5-pro")).unwrap();
        assert_eq!(agent.resolved_model(), "pro", "hooks are told the model the provider is sent");
    }

    /// A profile saved before its model was chosen sends no turn, and says what to do.
    #[tokio::test]
    async fn a_profile_without_a_model_refuses_the_turn_with_the_way_out() {
        let dir = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let (mut b, _rx, _) = backend(dir.path());
        let mut p = provider_profile();
        p.model.clear();
        b.settings.providers.push(p);
        b.settings.active_provider = Some("p1".into());
        let err = b.build_agent(&Conversation::new(Some(ws.path().into()), "gemini-2.5-pro")).err().expect("no model, no turn");
        assert!(err.starts_with("Test provider has no model chosen. Pick one from the model menu in the composer"), "{err}");
    }

    #[tokio::test]
    async fn use_provider_model_sets_the_model_and_puts_the_profile_in_use() {
        let dir = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let (mut b, mut rx, _) = backend(dir.path());
        b.settings.remember_workspace(ws.path());
        b.handle(AppCommand::NewChat).await;
        b.settings.providers.push(provider_profile());
        drain(&mut rx);

        b.handle(AppCommand::UseProviderModel { profile: "p1".into(), model: " gpt-4o ".into() }).await;
        assert_eq!((b.settings.active_provider.as_deref(), b.settings.providers[0].model.as_str()), (Some("p1"), "gpt-4o"));
        assert_eq!(b.current.as_ref().unwrap().model, "gpt-4o", "the chip follows");
        let saved = Settings::load(dir.path());
        assert_eq!((saved.active_provider.as_deref(), saved.providers[0].model.as_str()), (Some("p1"), "gpt-4o"));
        assert!(drain(&mut rx).iter().any(|e| matches!(e, AppEvent::Settings(_, s) if s.providers[0].model == "gpt-4o")));

        // Another model of the profile in use reaches a kept agent without a rebuild.
        let conv = b.current.clone().unwrap();
        let agent = b.build_agent(&conv).unwrap();
        b.agent_model = Some(agent.model_handle());
        b.agent_sends_to = Some(TurnBackend::of(&b.settings));
        b.agent = Some(Arc::new(Mutex::new(agent)));
        b.handle(AppCommand::UseProviderModel { profile: "p1".into(), model: "o3".into() }).await;
        assert!(b.agent.is_some(), "the conversation keeps its agent");
        assert_eq!(*b.agent_model.as_ref().unwrap().lock().unwrap(), "o3");

        // Refused: an unknown profile, an id a request cannot carry.
        for (profile, model) in [("nope", "gpt-4o"), ("p1", "has space"), ("p1", "  ")] {
            b.handle(AppCommand::UseProviderModel { profile: profile.into(), model: model.into() }).await;
            assert!(drain(&mut rx).iter().any(|e| matches!(e, AppEvent::Error(_))), "{profile} / {model:?}");
        }
        assert_eq!(b.settings.providers[0].model, "o3");
    }

    #[test]
    fn a_model_of_the_profile_in_use_is_no_reason_to_rebuild_the_agent() {
        let mut p = provider_profile();
        p.models = vec!["gpt-4o-mini".into()];
        let old = Settings { providers: vec![p], active_provider: Some("p1".into()), ..Settings::default() };
        let mut new = old.clone();
        new.providers[0].model = "gpt-4o".into();
        new.providers[0].models = vec!["gpt-4o".into(), "gpt-4o-mini".into()];
        assert!(!agent_inputs_changed(&old, &new), "the kept agent follows the model through its handle");
        // How it is reached is: the endpoint, the key, the wire.
        for change in [
            |p: &mut ProviderProfile| p.api_key = "sk-other".into(),
            |p: &mut ProviderProfile| p.base_url = Some("http://localhost:9/v1".into()),
            |p: &mut ProviderProfile| p.kind = crate::providers::ProviderKind::Anthropic,
        ] {
            let mut new = old.clone();
            change(&mut new.providers[0]);
            assert!(agent_inputs_changed(&old, &new));
        }
    }

    /// The bypass route bakes its model into the agent, so picking another model for the bypass
    /// profile must rebuild one. It cannot follow a shared handle the way the profile in use does.
    #[test]
    fn a_bypass_profiles_model_is_a_reason_to_rebuild_the_agent() {
        let mut p = provider_profile();
        p.models = vec!["gpt-4o-mini".into()];
        let mut old = Settings { providers: vec![p], ..Settings::default() };
        old.bypass.enabled = true;
        old.bypass.provider = Some("p1".into());
        assert!(old.bypass.routes());

        let mut new = old.clone();
        new.providers[0].model = "gpt-4o".into();
        assert!(agent_inputs_changed(&old, &new), "the route sends the model as typed");

        // The list the model is picked from is not part of the route: loading models alone keeps
        // the agent.
        let mut new = old.clone();
        new.providers[0].models = vec!["gpt-4o".into(), "o3".into()];
        assert!(!agent_inputs_changed(&old, &new));

        // Switching the mode, naming another profile, and changing the count all rebuild.
        for change in [
            |s: &mut Settings| s.bypass.enabled = false,
            |s: &mut Settings| s.bypass.requests = 4,
            |s: &mut Settings| s.bypass.files = vec![std::path::PathBuf::from("C:/a.md")],
        ] {
            let mut new = old.clone();
            change(&mut new);
            assert!(agent_inputs_changed(&old, &new));
        }
        let mut two = old.clone();
        let mut other = provider_profile();
        other.id = "p2".into();
        two.providers.push(other);
        let mut new = two.clone();
        new.bypass.provider = Some("p2".into());
        assert!(agent_inputs_changed(&two, &new), "another profile is another wire");
        // With the mode off, its profile's edits are none of the agent's business.
        let mut off = old.clone();
        off.bypass.enabled = false;
        let mut off_new = off.clone();
        off_new.providers[0].model = "o3".into();
        assert!(!agent_inputs_changed(&off, &off_new));
    }

    #[tokio::test]
    async fn load_provider_models_answers_with_the_list_under_the_same_request() {
        let app = axum::Router::new().route(
            "/v1/models",
            axum::routing::get(|headers: axum::http::HeaderMap| async move {
                let bearer = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or_default().to_owned();
                if bearer == "Bearer sk-test-key" {
                    (axum::http::StatusCode::OK, r#"{"data":[{"id":"gpt-4o"},{"id":"gpt-4o-mini"}]}"#)
                } else {
                    (axum::http::StatusCode::UNAUTHORIZED, r#"{"error":{"message":"bad key"}}"#)
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let dir = tempfile::tempdir().unwrap();
        let (mut b, mut rx, _) = backend(dir.path());
        let mut p = provider_profile();
        p.base_url = Some(format!("http://{addr}/v1"));
        p.model.clear();
        b.handle(AppCommand::LoadProviderModels { request: 7, profile: p.clone() }).await;
        async fn next(rx: &mut mpsc::UnboundedReceiver<AppEvent>) -> (u64, Result<crate::providers::models::LoadedModels, String>) {
            loop {
                if let AppEvent::ProviderModels { request, result } = tokio::time::timeout(Duration::from_secs(20), rx.recv()).await.expect("an answer in time").expect("channel open") {
                    return (request, result);
                }
            }
        }
        let (request, result) = next(&mut rx).await;
        let loaded = result.expect("listed");
        assert_eq!((request, loaded.models.as_slice()), (7, ["gpt-4o".to_owned(), "gpt-4o-mini".to_owned()].as_slice()));
        assert!(b.settings.providers.is_empty(), "nothing is stored: the dialog keeps the list");

        p.api_key = "sk-wrong".into();
        b.handle(AppCommand::LoadProviderModels { request: 8, profile: p }).await;
        let (request, result) = next(&mut rx).await;
        assert_eq!((request, result.unwrap_err().as_str()), (8, "The provider refused the API key (HTTP 401): bad key"));
    }

    #[tokio::test]
    async fn shutdown_command_ends_run() {
        let dir = tempfile::tempdir().unwrap();
        let (b, _rx, _) = backend(dir.path());
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(b.run(cmd_rx));
        cmd_tx.send(AppCommand::Shutdown).unwrap();
        tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
    }

    // --- Subscription accounts. The provider is an axum mock on a port the OS picked, the
    // browser is a closure, and data folders are temporary: no test reaches the network, holds a
    // registered callback port (1455/51121/54545) or opens a window. Every wait is bounded, so a
    // bug fails its test instead of hanging the run.

    use crate::auth::external::store::{AccountRecord, AccountStore};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Upper bound on anything a test waits for.
    const WAIT: Duration = Duration::from_secs(20);

    /// What the mock provider saw.
    #[derive(Default)]
    struct ProviderMock {
        exchanges: AtomicUsize,
        /// Every Codex turn: its `Chatgpt-Account-Id` header and its JSON body.
        turns: std::sync::Mutex<Vec<(Option<String>, serde_json::Value)>>,
    }

    impl ProviderMock {
        /// The `model` of every turn request, in order.
        fn models(&self) -> Vec<String> {
            self.turns.lock().unwrap().iter().map(|(_, body)| body["model"].as_str().unwrap_or_default().to_owned()).collect()
        }
    }

    /// An unsigned Codex ID token naming the account, the way OpenAI's token endpoint returns one:
    /// the workspace id and the plan sit inside the namespaced claim.
    fn codex_id_token(email: &str) -> String {
        use base64::Engine as _;
        let enc = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let claims = serde_json::json!({
            "email": email,
            crate::auth::external::jwt::OPENAI_AUTH_CLAIM: {"chatgpt_account_id": "acct-42", "chatgpt_plan_type": "plus"},
        });
        format!("{}.{}.{}", enc.encode(br#"{"alg":"none"}"#), enc.encode(serde_json::to_vec(&claims).unwrap()), enc.encode(b"sig"))
    }

    /// One Codex answer: a word, then the end of the response.
    const CODEX_OK: &str = concat!(
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
    );

    /// Codex's token endpoint and its Responses API, on one mock server. Only a sign-in reaches
    /// `/token`: every token it hands out is fresh for an hour, so no test ever needs a refresh
    /// (which would go to the provider's real endpoint).
    async fn codex_mock() -> (Arc<ProviderMock>, String) {
        use axum::extract::State;
        use axum::http::{HeaderMap, StatusCode};
        async fn token(State(m): State<Arc<ProviderMock>>, body: String) -> (StatusCode, axum::Json<serde_json::Value>) {
            let form: HashMap<String, String> = url::form_urlencoded::parse(body.as_bytes()).into_owned().collect();
            if form.get("grant_type").map(String::as_str) != Some("authorization_code") || form.get("code").map(String::as_str) != Some("good-code") {
                return (StatusCode::BAD_REQUEST, axum::Json(serde_json::json!({"error": "invalid_request"})));
            }
            m.exchanges.fetch_add(1, Ordering::SeqCst);
            let reply = serde_json::json!({
                "access_token": "at-app-secret",
                "refresh_token": "rt-app-secret",
                "expires_in": 3600,
                "id_token": codex_id_token("codex@example.com"),
            });
            (StatusCode::OK, axum::Json(reply))
        }
        async fn responses(State(m): State<Arc<ProviderMock>>, headers: HeaderMap, body: String) -> ([(&'static str, &'static str); 1], &'static str) {
            let account = headers.get("chatgpt-account-id").and_then(|v| v.to_str().ok()).map(str::to_owned);
            m.turns.lock().unwrap().push((account, serde_json::from_str(&body).unwrap_or_default()));
            ([("content-type", "text/event-stream")], CODEX_OK)
        }
        let state = Arc::new(ProviderMock::default());
        let app = axum::Router::new()
            .route("/token", axum::routing::post(token))
            .route("/responses", axum::routing::post(responses))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (state, format!("http://{addr}"))
    }

    /// The browser: back from the authorization page to the port the attempt bound, with the
    /// attempt's own state and the code the mock accepts. Redirects are not followed — the
    /// loopback server answers with one to the provider's real site.
    fn fake_browser() -> AccountOpener {
        Arc::new(|attempt: &LoginAttempt, page: url::Url| {
            let state = page.query_pairs().find(|(k, _)| k == "state").map(|(_, v)| v.into_owned()).unwrap_or_default();
            let port = attempt.bound_port().expect("the port is bound before the browser opens");
            let mut target = url::Url::parse(&format!("http://127.0.0.1:{port}{}", attempt.provider.spec().redirect_path)).unwrap();
            target.query_pairs_mut().append_pair("state", &state).append_pair("code", "good-code");
            tokio::spawn(async move {
                let client = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
                let _ = client.get(target).send().await;
            });
        })
    }

    /// A browser that never comes back (the user is still on the provider's page).
    fn idle_browser() -> AccountOpener {
        Arc::new(|_: &LoginAttempt, _: url::Url| {})
    }

    /// A backend whose sign-ins and account turns go to `base`, with a browser that completes the
    /// round trip, and with no Google client: an account must work without one.
    fn mock_accounts_backend(
        dir: &std::path::Path,
        base: &str,
    ) -> (Backend, mpsc::UnboundedReceiver<AppEvent>, mpsc::UnboundedReceiver<AppCommand>) {
        let (mut b, rx, self_rx) = backend_with_self_rx(dir);
        b.account_mock = Some(base.to_owned());
        b.account_opener = fake_browser();
        b.auth = None;
        b.client = None;
        b.client_source = None;
        (b, rx, self_rx)
    }

    /// The next command the backend sent itself.
    async fn next_command(self_rx: &mut mpsc::UnboundedReceiver<AppCommand>) -> AppCommand {
        tokio::time::timeout(WAIT, self_rx.recv()).await.expect("the backend reported in time").expect("channel open")
    }

    /// Sign in to `provider` through the mock and hand the report to the backend.
    async fn sign_in(b: &mut Backend, self_rx: &mut mpsc::UnboundedReceiver<AppCommand>, provider: AccountProvider) -> AccountView {
        b.handle(AppCommand::SignInAccount(provider)).await;
        let cmd = next_command(self_rx).await;
        let view = match &cmd {
            AppCommand::AccountLoginFinished(_, _, Ok(view)) => view.clone(),
            other => panic!("the sign-in failed: {other:?}"),
        };
        b.handle(cmd).await;
        view
    }

    /// A signed-in account's file, as a sign-in writes it. Its token is fresh for an hour, so
    /// nothing asks the provider for another one.
    fn account_record(provider: AccountProvider, email: &str) -> AccountRecord {
        AccountRecord {
            creds: crate::auth::Credentials {
                access_token: Some("at-stored-secret".into()),
                refresh_token: Some("rt-stored-secret".into()),
                expiry_date: Some(chrono::Utc::now().timestamp_millis() + 3_600_000),
                email: Some(email.into()),
                ..Default::default()
            },
            provider,
            account_id: external::account_id(provider, email),
            label: Some(email.into()),
            plan: None,
            wire_account_id: Some("acct-stored".into()),
            org_uuid: None,
            project_id: Some("projects/stored".into()),
            models: Vec::new(),
            models_fetched_at: None,
            lifetime_secs: Some(3600),
        }
    }

    #[tokio::test]
    async fn an_unreadable_account_file_keeps_its_entry_and_says_why() {
        let dir = tempfile::tempdir().unwrap();
        let (mut b, mut rx, _self_rx) = backend_with_self_rx(dir.path());
        let id = load_account(&mut b, account_record(AccountProvider::ClaudeCode, "claude@example.com")).await;
        b.handle(AppCommand::UseAccount(id.clone())).await;
        b.settings.accounts[0].model = "claude-sonnet-5-5".into();
        drain(&mut rx);

        // The next start finds the file damaged (or held open by another program).
        let path = b.accounts.store().dir().join(format!("{id}.json"));
        std::fs::write(&path, b"{ half a record").unwrap();
        let files = b.accounts.load_all().await;
        assert!(files.is_empty());
        b.reconcile_accounts(files);
        let evs = drain(&mut rx);
        assert_eq!(b.settings.active_account.as_deref(), Some(id.as_str()), "still the one in use");
        assert_eq!(b.settings.accounts[0].model, "claude-sonnet-5-5", "and its model is remembered");
        let row = account_row_in(&evs, &id).expect("the row stays");
        assert_eq!(row.status, AccountStatus::Error(UNREADABLE_ACCOUNT.into()));
        assert!(b.build_agent(&Conversation::new(None, "m")).err().is_some_and(|m| m.contains("needs signing in again")));

        // A file that is really gone takes its entry with it.
        std::fs::remove_file(&path).unwrap();
        let files = b.accounts.load_all().await;
        b.reconcile_accounts(files);
        assert!(b.settings.accounts.is_empty() && b.settings.active_account.is_none());
    }

    /// Write `rec`, then load the accounts the way startup does. Returns the account id.
    async fn load_account(b: &mut Backend, rec: AccountRecord) -> String {
        let id = rec.account_id.clone();
        b.accounts.store().save(&rec).unwrap();
        let files = b.accounts.load_all().await;
        b.reconcile_accounts(files);
        id
    }

    /// The row `id` in the last `Accounts` event among `evs`.
    fn account_row_in(evs: &[AppEvent], id: &str) -> Option<AccountView> {
        evs.iter().rev().find_map(|e| match e {
            AppEvent::Accounts(rows) => Some(rows.iter().find(|r| r.id == id).cloned()),
            _ => None,
        })?
    }

    /// Wait for the running turn to end, settle it as the run loop would, and say how it ended.
    async fn settle_turn(b: &mut Backend, done_rx: &mut mpsc::UnboundedReceiver<TurnDone>) -> Option<FinishReason> {
        let done = tokio::time::timeout(WAIT, done_rx.recv()).await.expect("the turn ended in time").expect("channel open");
        let reason = done.reason.clone();
        b.turn_finished(done);
        reason
    }

    #[test]
    fn account_provider_change_rebuilds_the_agent() {
        let codex = AccountEntry { id: "codex-0123abcd".into(), provider: AccountProvider::Codex, label: "a".into(), model: "gpt-5.1".into() };
        let claude = AccountEntry {
            id: "claude-0123abcd".into(),
            provider: AccountProvider::ClaudeCode,
            label: "c".into(),
            model: "claude-sonnet-4-6".into(),
        };
        let none = Settings { accounts: vec![codex.clone()], ..Settings::default() };
        let mut active = none.clone();
        active.active_account = Some(codex.id.clone());
        assert!(agent_inputs_changed(&none, &active), "an account now sends the turns");
        let mut other = active.clone();
        other.accounts.push(claude.clone());
        other.active_account = Some(claude.id.clone());
        assert!(agent_inputs_changed(&active, &other), "another account: another token source");
        // What the kept agent follows without a rebuild: another model of the account in use (its
        // shared model handle), and an account that is not in use (it is not in the agent).
        let mut model = active.clone();
        model.accounts[0].model = "gpt-5.1-codex".into();
        assert!(!agent_inputs_changed(&active, &model));
        let mut second = active.clone();
        second.accounts.push(claude);
        assert!(!agent_inputs_changed(&active, &second));
    }

    #[tokio::test]
    async fn sign_in_account_persists_and_builds_the_agent() {
        let (mock, base) = codex_mock().await;
        let dir = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let (mut b, mut rx, mut self_rx) = mock_accounts_backend(dir.path(), &base);

        b.handle(AppCommand::SignInAccount(AccountProvider::Codex)).await;
        let evs = drain(&mut rx);
        let info = evs
            .iter()
            .find_map(|e| match e {
                AppEvent::AccountLogin { login: Some(info) } => Some(info.clone()),
                _ => None,
            })
            .expect("the attempt is reported, so its Cancel has an id");
        assert_eq!((info.provider, info.error.as_deref()), (AccountProvider::Codex, None));
        assert!(
            evs.iter().any(|e| matches!(e, AppEvent::Accounts(rows) if rows.iter().any(|r| r.provider == AccountProvider::Codex && r.status == AccountStatus::SigningIn))),
            "the provider's row shows the sign-in running: {evs:?}"
        );

        let cmd = next_command(&mut self_rx).await;
        let view = match &cmd {
            AppCommand::AccountLoginFinished(id, AccountProvider::Codex, Ok(view)) if *id == info.id => view.clone(),
            other => panic!("expected this attempt's sign-in: {other:?}"),
        };
        b.handle(cmd).await;
        let evs = drain(&mut rx);
        assert_eq!(mock.exchanges.load(Ordering::SeqCst), 1);
        assert_eq!((view.email.as_deref(), view.plan.as_deref()), (Some("codex@example.com"), Some("plus")));

        // Listed and in use (it is the first account), and saved; the token stays in its own file.
        let entry = AccountEntry { id: view.id.clone(), provider: AccountProvider::Codex, label: "codex@example.com".into(), model: view.model.clone() };
        assert!(!entry.model.is_empty(), "an account always has a model");
        assert_eq!(b.settings.accounts, std::slice::from_ref(&entry));
        assert_eq!(b.settings.active_account.as_deref(), Some(view.id.as_str()));
        let saved = Settings::load(dir.path());
        assert_eq!((saved.accounts, saved.active_account), (vec![entry], Some(view.id.clone())));
        assert!(dir.path().join("accounts").join(format!("{}.json", view.id)).exists());
        let file = std::fs::read_to_string(Settings::path(dir.path())).unwrap();
        assert!(!file.contains("at-app-secret") && !file.contains("rt-app-secret"), "no token in settings.json");
        let shown = format!("{evs:?}");
        assert!(!shown.contains("at-app-secret") && !shown.contains("rt-app-secret"), "no token reaches the UI");
        assert!(evs.iter().any(|e| matches!(e, AppEvent::AccountLogin { login: None })));
        assert_eq!(account_row_in(&evs, &view.id).map(|r| r.status), Some(AccountStatus::SignedIn));
        assert!(
            !evs.iter().any(|e| matches!(e, AppEvent::Accounts(rows) if rows.iter().any(|r| r.status == AccountStatus::SigningIn))),
            "no row spins any more"
        );
        assert!(evs.iter().any(|e| matches!(e, AppEvent::Settings(_, s) if s.active_account.as_deref() == Some(view.id.as_str()))));

        // The next turn runs on the account, without any Google sign-in.
        let conv = Conversation::new(Some(ws.path().into()), "gemini-2.5-pro");
        let agent = b.build_agent(&conv).expect("the signed-in account drives the turn");
        assert_eq!(*agent.model_handle().lock().unwrap(), view.model);
    }

    #[tokio::test]
    async fn use_account_model_reaches_the_wire() {
        let (mock, base) = codex_mock().await;
        let dir = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let (mut b, mut rx, mut self_rx) = mock_accounts_backend(dir.path(), &base);
        b.settings.remember_workspace(ws.path());
        let view = sign_in(&mut b, &mut self_rx, AccountProvider::Codex).await;
        let mut done_rx = b.turn_done_rx.take().expect("run is not running");

        // A pick in the composer's picker: the turn sends that id, not the one the account had.
        let picked = "gpt-5.1-codex-mini";
        assert_ne!(view.model, picked);
        b.handle(AppCommand::UseAccountModel { account: view.id.clone(), model: picked.into() }).await;
        assert_eq!(b.settings.accounts[0].model, picked);
        b.handle(AppCommand::SendPrompt("hello".into())).await;
        assert_eq!(b.current.as_ref().map(|c| c.model.as_str()), Some(picked), "the chip shows it");
        let reason = settle_turn(&mut b, &mut done_rx).await;
        assert_eq!(reason, Some(FinishReason::Completed), "{:?}", drain(&mut rx));
        let first = mock.models();
        assert!(!first.is_empty() && first.iter().all(|m| m == picked), "{first:?}");
        let turns = mock.turns.lock().unwrap().clone();
        assert!(turns.iter().all(|(account, _)| account.as_deref() == Some("acct-42")), "the account id comes with the token: {turns:?}");

        // Another model of the same account: the kept agent follows it through the shared handle,
        // so the conversation keeps its "Always allow" choices.
        let agent = b.agent.clone().expect("kept after the turn");
        b.handle(AppCommand::SetAccountModel { account: view.id.clone(), model: "gpt-5.1".into() }).await;
        b.handle(AppCommand::SendPrompt("again".into())).await;
        assert_eq!(settle_turn(&mut b, &mut done_rx).await, Some(FinishReason::Completed));
        let later = mock.models()[first.len()..].to_vec();
        assert!(!later.is_empty() && later.iter().all(|m| m == "gpt-5.1"), "{later:?}");
        assert!(Arc::ptr_eq(&agent, b.agent.as_ref().expect("still kept")), "same account, same agent");
        assert_eq!(Settings::load(dir.path()).accounts[0].model, "gpt-5.1", "the choice is saved");
    }

    #[tokio::test]
    async fn sign_out_falls_back_to_code_assist() {
        let (_mock, base) = codex_mock().await;
        let dir = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let (mut b, mut rx, mut self_rx) = mock_accounts_backend(dir.path(), &base);
        b.settings.remember_workspace(ws.path());
        let view = sign_in(&mut b, &mut self_rx, AccountProvider::Codex).await;
        b.handle(AppCommand::NewChat).await;
        assert_eq!(b.current.as_ref().unwrap().model, view.model, "the chip shows the account's model");
        drain(&mut rx);

        // Not while a turn runs: the turn holds the account's token source.
        b.turn_cancel = Some(b.shutdown.child_token());
        b.handle(AppCommand::SignOutAccount(view.id.clone())).await;
        assert!(drain(&mut rx).iter().any(|e| matches!(e, AppEvent::Error(m) if m == "Stop the running turn first")));
        assert_eq!(b.settings.accounts.len(), 1);
        b.turn_cancel = None;

        b.handle(AppCommand::SignOutAccount(view.id.clone())).await;
        let evs = drain(&mut rx);
        assert!(b.settings.accounts.is_empty() && b.settings.active_account.is_none());
        assert!(b.accounts.auth(&view.id).is_none());
        assert!(!dir.path().join("accounts").join(format!("{}.json", view.id)).exists(), "the token file is deleted");
        let saved = Settings::load(dir.path());
        assert!(saved.accounts.is_empty() && saved.active_account.is_none());
        assert!(evs.iter().any(|e| matches!(e, AppEvent::Accounts(rows) if rows.is_empty())), "{evs:?}");
        // Code Assist again: its model on the chip, and its sign-in requirement for the next turn.
        assert_eq!(b.current.as_ref().unwrap().model, "test-model");
        let conv = b.current.clone().unwrap();
        assert_eq!(b.build_agent(&conv).err().as_deref(), Some("not logged in"));
    }

    #[tokio::test]
    async fn signing_out_waits_for_a_sign_in_that_could_write_the_same_file() {
        let (_mock, base) = codex_mock().await;
        let dir = tempfile::tempdir().unwrap();
        let (mut b, mut rx, mut self_rx) = mock_accounts_backend(dir.path(), &base);
        let view = sign_in(&mut b, &mut self_rx, AccountProvider::Codex).await;
        let file = dir.path().join("accounts").join(format!("{}.json", view.id));
        b.account_opener = idle_browser();
        drain(&mut rx);

        // Signing in to Codex again renews this very account in place: until that ends, its file
        // is not deleted from under it.
        b.handle(AppCommand::SignInAccount(AccountProvider::Codex)).await;
        let renewal = b.account_login.as_ref().expect("in flight").id;
        b.handle(AppCommand::SignOutAccount(view.id.clone())).await;
        assert!(drain(&mut rx).iter().any(|e| matches!(e, AppEvent::Error(m) if m == "Finish or cancel the current sign-in first.")));
        assert!(file.exists() && b.accounts.auth(&view.id).is_some());
        assert_eq!(b.settings.accounts.len(), 1);
        b.handle(AppCommand::CancelAccountLogin(renewal)).await;
        let cmd = next_command(&mut self_rx).await;
        b.handle(cmd).await;

        // A sign-in of another provider cannot produce this id, so it does not hold the sign-out.
        b.handle(AppCommand::SignInAccount(AccountProvider::ClaudeCode)).await;
        let other = b.account_login.as_ref().expect("in flight").id;
        b.handle(AppCommand::SignOutAccount(view.id.clone())).await;
        assert!(!file.exists() && b.accounts.auth(&view.id).is_none());
        assert!(b.settings.accounts.is_empty());
        drain(&mut rx);
        // An account that is no longer listed has nothing left to sign out of.
        b.handle(AppCommand::SignOutAccount(view.id.clone())).await;
        assert!(drain(&mut rx).is_empty());
        b.handle(AppCommand::CancelAccountLogin(other)).await;
        let cmd = next_command(&mut self_rx).await;
        b.handle(cmd).await;
    }

    #[tokio::test]
    async fn an_account_needs_no_google_login() {
        let dir = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let (mut b, mut rx, _self_rx) = backend_with_self_rx(dir.path());
        b.auth = None;
        b.client = None;
        let conv = Conversation::new(Some(ws.path().into()), "gemini-2.5-pro");
        assert_eq!(b.build_agent(&conv).err().as_deref(), Some("not logged in"), "Code Assist still needs its sign-in");

        let id = load_account(&mut b, account_record(AccountProvider::ClaudeCode, "claude@example.com")).await;
        // Listed from its file, but not put in use: loading picks no backend for the user.
        assert_eq!(b.settings.accounts.len(), 1);
        assert_eq!(b.settings.active_account, None);
        assert_eq!(account_row_in(&drain(&mut rx), &id).map(|r| r.status), Some(AccountStatus::SignedIn));

        b.handle(AppCommand::UseAccount(id.clone())).await;
        let model = b.settings.accounts[0].model.clone();
        let agent = b.build_agent(&conv).expect("an account must not require a Google sign-in");
        assert_eq!(*agent.model_handle().lock().unwrap(), model, "the account's model is what the turn uses");
        assert_eq!(b.active_model(), model);
        assert_eq!(Settings::load(dir.path()).active_account.as_deref(), Some(id.as_str()), "the choice is saved");

        // A profile and an account are never both in use, and Code Assist is neither.
        b.settings.providers.push(provider_profile());
        b.handle(AppCommand::SetActiveProvider(Some("p1".into()))).await;
        assert_eq!((b.settings.active_provider.as_deref(), b.settings.active_account.as_deref()), (Some("p1"), None));
        b.handle(AppCommand::UseAccount(id.clone())).await;
        assert_eq!((b.settings.active_provider.as_deref(), b.settings.active_account.as_deref()), (None, Some(id.as_str())));
        b.handle(AppCommand::SetActiveProvider(None)).await;
        assert_eq!((b.settings.active_provider.as_deref(), b.settings.active_account.as_deref()), (None, None));
        assert_eq!(b.build_agent(&conv).err().as_deref(), Some("not logged in"));
    }

    #[tokio::test]
    async fn a_stale_account_login_report_is_ignored() {
        let (_mock, base) = codex_mock().await;
        let dir = tempfile::tempdir().unwrap();
        let (mut b, mut rx, mut self_rx) = mock_accounts_backend(dir.path(), &base);
        b.account_opener = idle_browser();
        b.handle(AppCommand::SignInAccount(AccountProvider::Codex)).await;
        let current = b.account_login.as_ref().expect("in flight").id;
        drain(&mut rx);

        // Reports from attempts this one replaced: nothing changes, and nothing is sent.
        let ghost = AccountView {
            id: external::account_id(AccountProvider::Codex, "ghost@example.com"),
            provider: AccountProvider::Codex,
            email: Some("ghost@example.com".into()),
            plan: None,
            model: "gpt-5.1".into(),
            models: Vec::new(),
            models_fetched_at: None,
            status: AccountStatus::SignedIn,
        };
        b.handle(AppCommand::AccountLoginFinished(current + 1, AccountProvider::Codex, Ok(ghost))).await;
        b.handle(AppCommand::AccountLoginFinished(current + 2, AccountProvider::Codex, Err("login timed out".into()))).await;
        assert!(drain(&mut rx).is_empty(), "a stale report sends nothing");
        assert!(b.settings.accounts.is_empty() && b.settings.active_account.is_none());
        assert_eq!(b.account_login.as_ref().map(|a| a.id), Some(current), "the attempt in flight is still the one");

        // Its own report counts. A cancel is no failure: no message, and the attempt is kept for
        // a pasted link.
        b.handle(AppCommand::CancelAccountLogin(current)).await;
        let cmd = next_command(&mut self_rx).await;
        assert!(matches!(&cmd, AppCommand::AccountLoginFinished(id, _, Err(_)) if *id == current), "{cmd:?}");
        b.handle(cmd).await;
        let evs = drain(&mut rx);
        assert!(b.account_login.is_none());
        assert_eq!(b.account_paste.as_ref().map(|a| a.id), Some(current));
        assert!(evs.iter().any(|e| matches!(e, AppEvent::AccountLogin { login: Some(i) } if i.id == current && i.error.is_none())), "{evs:?}");
        assert!(evs.iter().any(|e| matches!(e, AppEvent::Accounts(rows) if rows.is_empty())), "no row spins any more: {evs:?}");
    }

    #[tokio::test]
    async fn a_second_sign_in_cannot_orphan_the_first_cancel() {
        let (_mock, base) = codex_mock().await;
        let dir = tempfile::tempdir().unwrap();
        let (mut b, mut rx, mut self_rx) = mock_accounts_backend(dir.path(), &base);
        b.account_opener = idle_browser();
        b.handle(AppCommand::SignInAccount(AccountProvider::Codex)).await;
        let first = b.account_login.clone().expect("in flight");
        drain(&mut rx);

        // A second sign-in while the first runs is refused, so the first one's Cancel still reaches
        // it. Google's sign-in counts as one too.
        b.handle(AppCommand::SignInAccount(AccountProvider::ClaudeCode)).await;
        assert!(drain(&mut rx).iter().any(|e| matches!(e, AppEvent::Error(m) if m == "Finish or cancel the current sign-in first.")));
        assert_eq!(b.account_login.as_ref().map(|a| a.id), Some(first.id));
        b.handle(AppCommand::CancelAccountLogin(first.id)).await;
        assert!(first.cancel.is_cancelled());
        let cmd = next_command(&mut self_rx).await;
        b.handle(cmd).await;
        b.login_cancel = Some(CancellationToken::new());
        b.handle(AppCommand::SignInAccount(AccountProvider::ClaudeCode)).await;
        assert!(drain(&mut rx).iter().any(|e| matches!(e, AppEvent::Error(m) if m == "Finish or cancel the current sign-in first.")));
        b.login_cancel = None;

        // A new attempt is a new id: Cancel drawn for the first one stops nothing now.
        b.handle(AppCommand::SignInAccount(AccountProvider::ClaudeCode)).await;
        let second = b.account_login.clone().expect("in flight");
        assert_ne!(second.id, first.id);
        assert!(b.account_paste.is_none(), "the attempt kept for a pasted link is let go");
        b.handle(AppCommand::CancelAccountLogin(first.id)).await;
        assert!(!second.cancel.is_cancelled(), "an old id cancels nothing");
        b.handle(AppCommand::CancelAccountLogin(second.id)).await;
        assert!(second.cancel.is_cancelled());
        let cmd = next_command(&mut self_rx).await;
        assert!(matches!(&cmd, AppCommand::AccountLoginFinished(id, AccountProvider::ClaudeCode, Err(_)) if *id == second.id), "{cmd:?}");
        b.handle(cmd).await;
        assert!(b.account_login.is_none());
    }

    #[tokio::test]
    async fn an_expired_account_says_so_and_does_not_report_not_logged_in() {
        let dir = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let (mut b, mut rx, _self_rx) = backend_with_self_rx(dir.path());
        b.auth = None;
        b.client = None;
        let id = load_account(&mut b, account_record(AccountProvider::ClaudeCode, "claude@example.com")).await;
        b.handle(AppCommand::UseAccount(id.clone())).await;
        b.handle(AppCommand::NewChat).await;
        let conv = b.current.clone().unwrap();
        b.agent = Some(stub_agent(ws.path(), &conv));
        drain(&mut rx);

        // What a refresh that met `invalid_grant` leaves behind: the registry dropped the token
        // source and the file, then reported. (That ordering is the registry's own test.)
        b.accounts.logout(&id).await.unwrap();
        b.handle(AppCommand::AccountExpired("claude-deadbeef".into())).await;
        assert!(drain(&mut rx).is_empty(), "an account settings do not list is a stale report");

        b.handle(AppCommand::AccountExpired(id.clone())).await;
        let evs = drain(&mut rx);
        let row = account_row_in(&evs, &id).expect("the row stays, to say why");
        assert_eq!(row.status, AccountStatus::Expired("session expired, please log in again".into()));
        assert!(evs.iter().any(|e| matches!(e, AppEvent::Settings(..))));
        assert_eq!(b.settings.active_account.as_deref(), Some(id.as_str()), "still the one in use: the next turn says why it cannot run");
        assert!(b.agent.is_none(), "the agent held the dead token source");

        let message = "The Claude Code account needs signing in again (Customizations > Account).";
        assert_eq!(b.build_agent(&Conversation::new(Some(ws.path().into()), "m")).err().as_deref(), Some(message));
        b.handle(AppCommand::SendPrompt("hi".into())).await;
        let evs = drain(&mut rx);
        assert!(evs.iter().any(|e| matches!(e, AppEvent::TurnRejected(m) if m == message)), "{evs:?}");
        assert!(!evs.iter().any(|e| matches!(e, AppEvent::TurnRejected(m) if m == "not logged in")));
    }

    #[tokio::test]
    async fn an_account_uses_its_own_context_window() {
        let dir = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let (mut b, _rx, _self_rx) = backend_with_self_rx(dir.path());
        let id = load_account(&mut b, account_record(AccountProvider::Codex, "codex@example.com")).await;
        b.handle(AppCommand::UseAccount(id)).await;
        // Named like a Code Assist alias: alias resolution would turn it into a Gemini id.
        b.settings.accounts[0].model = "auto".into();
        let conv = Conversation::new(Some(ws.path().into()), "gemini-2.5-pro");
        let AgentBackend { context_window, model, .. } = b.agent_backend(&conv).expect("a live account");
        assert_eq!(context_window, Some(400_000), "an unknown model gets Codex's default, not Gemini's 1 048 576");
        assert_eq!(model, "auto");
        let agent = b.build_agent(&conv).unwrap();
        assert_eq!(agent.resolved_model(), "auto", "an account's model is sent as typed");

        // A model the catalog knows runs with its own window.
        b.settings.accounts[0].model = "gpt-6.1-terra".into();
        assert_eq!(b.agent_backend(&conv).unwrap().context_window, Some(500_000));
        b.settings.accounts[0].model = "gpt-5.6-sol".into();
        assert_eq!(b.agent_backend(&conv).unwrap().context_window, Some(372_000), "no plan on file: the smaller window");
    }

    #[tokio::test]
    async fn a_chatgpt_pro_account_gets_its_plans_window() {
        let dir = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let (mut b, _rx, _self_rx) = backend_with_self_rx(dir.path());
        let mut rec = account_record(AccountProvider::Codex, "codex@example.com");
        rec.plan = Some("pro".into());
        let id = load_account(&mut b, rec).await;
        b.handle(AppCommand::UseAccount(id)).await;
        b.settings.accounts[0].model = "gpt-5.6-sol".into();
        let conv = Conversation::new(Some(ws.path().into()), "m");
        assert_eq!(b.agent_backend(&conv).unwrap().context_window, Some(921_000));
    }

    #[tokio::test]
    async fn a_turn_running_when_the_backend_switches_keeps_a_model_of_its_own_backend() {
        let dir = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let (mut b, _rx, _self_rx) = backend_with_self_rx(dir.path());
        let id = load_account(&mut b, account_record(AccountProvider::Codex, "codex@example.com")).await;
        b.settings.remember_workspace(ws.path());
        b.handle(AppCommand::NewChat).await;
        let conv = b.current.clone().unwrap();
        // A Code Assist turn runs on an agent built for Code Assist.
        let handle: SharedModel = Arc::new(std::sync::Mutex::new("test-model".into()));
        b.agent = Some(stub_agent(ws.path(), &conv));
        b.agent_model = Some(handle.clone());
        b.agent_sends_to = Some(TurnBackend::of(&b.settings));
        b.turn_id = 1;
        b.turn_cancel = Some(b.shutdown.child_token());

        // Mid-turn the account is put in use, given another model, picked again, and the Code
        // Assist model changes as well. None of it reaches the running turn, which would send an
        // account's model id to Code Assist.
        b.handle(AppCommand::UseAccount(id.clone())).await;
        b.handle(AppCommand::SetAccountModel { account: id.clone(), model: "gpt-5.1".into() }).await;
        b.handle(AppCommand::UseAccount(id.clone())).await;
        b.handle(AppCommand::SetModel("gemini-3.5-flash".into())).await;
        assert_eq!(*handle.lock().unwrap(), "test-model");
        assert!(b.agent_stale, "the next turn gets an agent built for the account");
        assert_eq!(b.current.as_ref().unwrap().model, "gpt-5.1", "the chip shows what the next turn sends");

        // The turn ends; the next one runs on the account, with its model.
        b.turn_finished(TurnDone {
            turn_id: 1,
            conversation_id: conv.id,
            snapshot: None,
            history: None,
            reason: Some(FinishReason::Completed),
            artifacts: None,
            restore: Vec::new(),
            steer: Vec::new(),
        });
        assert!(b.agent.is_none() && b.agent_model.is_none());
        let agent = b.build_agent(&b.current.clone().unwrap()).expect("the account drives the next turn");
        assert_eq!(*agent.model_handle().lock().unwrap(), "gpt-5.1");

        // On the backend it was built for, a model change does reach a running turn.
        let handle: SharedModel = Arc::new(std::sync::Mutex::new("gpt-5.1".into()));
        b.agent_model = Some(handle.clone());
        b.agent_sends_to = Some(TurnBackend::of(&b.settings));
        b.handle(AppCommand::SetAccountModel { account: id.clone(), model: "gpt-5.1-codex".into() }).await;
        assert_eq!(*handle.lock().unwrap(), "gpt-5.1-codex");
    }

    #[tokio::test]
    async fn startup_lists_account_files_and_drops_entries_whose_file_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        // settings.json names an account whose file is gone, and has it in use.
        let gone = AccountEntry {
            id: "claude-0123abcd".into(),
            provider: AccountProvider::ClaudeCode,
            label: "gone@example.com".into(),
            model: "claude-sonnet-4-6".into(),
        };
        Settings { accounts: vec![gone.clone()], active_account: Some(gone.id.clone()), ..Settings::default() }.save(dir.path()).unwrap();
        // And a file settings do not list.
        let rec = account_record(AccountProvider::Codex, "codex@example.com");
        let kept = rec.account_id.clone();
        AccountStore::in_dir(dir.path()).save(&rec).unwrap();

        let (b, mut rx, _self_rx) = backend_with_self_rx(dir.path());
        assert_eq!(b.settings.active_account.as_deref(), Some(gone.id.as_str()));
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(b.run(cmd_rx));
        let mut rows = None;
        loop {
            match tokio::time::timeout(WAIT, rx.recv()).await.expect("startup timed out").expect("backend gone") {
                AppEvent::Accounts(r) => rows = Some(r),
                AppEvent::McpStatus(_) => break,
                _ => {}
            }
        }
        cmd_tx.send(AppCommand::Shutdown).unwrap();
        tokio::time::timeout(WAIT, task).await.unwrap().unwrap();

        let rows = rows.expect("startup reports the accounts");
        assert_eq!(rows.iter().map(|r| (r.id.as_str(), &r.status)).collect::<Vec<_>>(), [(kept.as_str(), &AccountStatus::SignedIn)]);
        let saved = Settings::load(dir.path());
        assert_eq!(saved.accounts.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), [kept.as_str()], "the file is the truth");
        assert_eq!(saved.active_account, None, "the account in use is gone, so Code Assist is");
    }

    #[tokio::test]
    async fn a_save_from_an_older_copy_neither_resurrects_nor_drops_accounts() {
        let dir = tempfile::tempdir().unwrap();
        let (mut b, mut rx, _self_rx) = backend_with_self_rx(dir.path());
        let first = load_account(&mut b, account_record(AccountProvider::Codex, "codex@example.com")).await;
        let older = b.settings.clone();
        let second = load_account(&mut b, account_record(AccountProvider::ClaudeCode, "claude@example.com")).await;

        // The UI's copy predates the second account; a model picked in it still applies.
        let mut edited = older.clone();
        edited.accounts[0].model = "gpt-5.1-codex-mini".into();
        b.handle(AppCommand::SaveSettings { seq: 1, base: Box::new(older), settings: Box::new(edited) }).await;
        let ids: Vec<String> = b.settings.accounts.iter().map(|e| e.id.clone()).collect();
        assert_eq!(ids, [first.clone(), second.clone()], "the account the UI had not heard of stays");
        assert_eq!(b.settings.accounts[0].model, "gpt-5.1-codex-mini");
        assert!(drain(&mut rx).iter().any(|e| matches!(e, AppEvent::Accounts(rows) if rows[0].model == "gpt-5.1-codex-mini")));

        // A copy that still lists an account signed out since does not bring it back.
        let stale = b.settings.clone();
        b.handle(AppCommand::SignOutAccount(second.clone())).await;
        let mut edited = stale.clone();
        edited.accounts[0].model = "gpt-5.1".into();
        b.handle(AppCommand::SaveSettings { seq: 2, base: Box::new(stale), settings: Box::new(edited) }).await;
        let ids: Vec<String> = b.settings.accounts.iter().map(|e| e.id.clone()).collect();
        assert_eq!(ids, std::slice::from_ref(&first));
        assert_eq!(b.settings.accounts[0].model, "gpt-5.1");

        // A profile picked on the Providers tab (it edits `active_provider` alone) wins over the
        // account in use instead of being dropped by the exclusivity rule.
        b.handle(AppCommand::UseAccount(first.clone())).await;
        let base = b.settings.clone();
        let mut picked = base.clone();
        picked.providers.push(provider_profile());
        picked.active_provider = Some("p1".into());
        b.handle(AppCommand::SaveSettings { seq: 3, base: Box::new(base), settings: Box::new(picked) }).await;
        assert_eq!((b.settings.active_provider.as_deref(), b.settings.active_account.as_deref()), (Some("p1"), None));
    }

    #[tokio::test]
    async fn a_pasted_redirect_link_finishes_the_sign_in() {
        let (mock, base) = codex_mock().await;
        let dir = tempfile::tempdir().unwrap();
        let (mut b, mut rx, mut self_rx) = mock_accounts_backend(dir.path(), &base);
        // The browser never makes it back to this machine; the user cancels the wait.
        b.account_opener = idle_browser();
        b.handle(AppCommand::SignInAccount(AccountProvider::Codex)).await;
        let info = drain(&mut rx)
            .into_iter()
            .find_map(|e| match e {
                AppEvent::AccountLogin { login: Some(info) } => Some(info),
                _ => None,
            })
            .expect("the attempt is reported");
        b.handle(AppCommand::CancelAccountLogin(info.id)).await;
        let cmd = next_command(&mut self_rx).await;
        b.handle(cmd).await;
        assert_eq!(b.account_paste.as_ref().map(|a| a.id), Some(info.id), "kept for a pasted link");

        // The address the browser landed on, as the user copies it.
        let page = url::Url::parse(&info.auth_url).unwrap();
        let query = |key: &str| page.query_pairs().find(|(k, _)| k == key).map(|(_, v)| v.into_owned()).unwrap();
        let landed = |state: &str| {
            let mut link = url::Url::parse(&query("redirect_uri")).unwrap();
            link.query_pairs_mut().append_pair("state", state).append_pair("code", "good-code");
            PastedLink::from(link.to_string())
        };
        assert!(!format!("{:?}", AppCommand::PasteAccountLink { login: info.id, link: landed("s") }).contains("good-code"), "the code never reaches a log");

        // A link for another attempt is dropped; one with a forged state is refused before any
        // exchange, and the attempt stays for the right link.
        b.handle(AppCommand::PasteAccountLink { login: info.id + 1, link: landed(&query("state")) }).await;
        assert!(b.account_login.is_none());
        b.handle(AppCommand::PasteAccountLink { login: info.id, link: landed("forged") }).await;
        let cmd = next_command(&mut self_rx).await;
        assert!(matches!(&cmd, AppCommand::AccountLoginFinished(id, _, Err(_)) if *id == info.id), "{cmd:?}");
        b.handle(cmd).await;
        assert_eq!(mock.exchanges.load(Ordering::SeqCst), 0, "no code is exchanged for a forged state");
        assert!(drain(&mut rx).iter().any(|e| matches!(e, AppEvent::AccountLogin { login: Some(i) } if i.id == info.id && i.error.is_some())));
        assert_eq!(b.account_paste.as_ref().map(|a| a.id), Some(info.id));

        b.handle(AppCommand::PasteAccountLink { login: info.id, link: landed(&query("state")) }).await;
        assert_eq!(b.account_login.as_ref().map(|a| a.id), Some(info.id), "in flight like any sign-in");
        let cmd = next_command(&mut self_rx).await;
        assert!(matches!(&cmd, AppCommand::AccountLoginFinished(id, _, Ok(_)) if *id == info.id), "{cmd:?}");
        b.handle(cmd).await;
        assert_eq!(mock.exchanges.load(Ordering::SeqCst), 1);
        assert_eq!(b.settings.accounts.len(), 1);
        assert!(b.account_login.is_none() && b.account_paste.is_none());
        assert!(drain(&mut rx).iter().any(|e| matches!(e, AppEvent::AccountLogin { login: None })));
    }

    #[tokio::test]
    async fn checking_models_sends_whole_states_and_keeps_the_list_in_the_account_file() {
        let dir = tempfile::tempdir().unwrap();
        let (mut b, mut rx, mut self_rx) = backend_with_self_rx(dir.path());
        let id = load_account(&mut b, account_record(AccountProvider::Codex, "codex@example.com")).await;
        drain(&mut rx);

        b.handle(AppCommand::CheckAccountModels(id.clone())).await;
        let checking = AccountModelsState { checking: true, error: None };
        assert!(drain(&mut rx).iter().any(|e| matches!(e, AppEvent::AccountModels { id: i, state } if *i == id && *state == checking)));
        b.handle(AppCommand::CheckAccountModels(id.clone())).await;
        assert!(drain(&mut rx).is_empty(), "one check at a time");
        let cmd = next_command(&mut self_rx).await;
        assert!(matches!(&cmd, AppCommand::AccountModelsFetched(i, Ok(list)) if *i == id && !list.is_empty()), "{cmd:?}");
        b.handle(cmd).await;
        let evs = drain(&mut rx);
        assert!(evs.iter().any(|e| matches!(e, AppEvent::AccountModels { id: i, state } if *i == id && *state == AccountModelsState::default())));
        let row = account_row_in(&evs, &id).expect("the rows are sent again");
        assert!(!row.models.is_empty() && row.models_fetched_at.is_some(), "{row:?}");
        let file = AccountStore::in_dir(dir.path()).load(&id).unwrap().unwrap();
        assert_eq!(file.models, row.models, "the list lives in the account file");
        let saved: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(Settings::path(dir.path())).unwrap()).unwrap();
        assert!(saved["accounts"][0].get("models").is_none(), "settings keep the chosen model only: {saved}");

        // A check that fails says why, and leaves `checking` off.
        let mut rec = account_record(AccountProvider::Antigravity, "ag@example.com");
        rec.project_id = None;
        let ag = load_account(&mut b, rec).await;
        b.handle(AppCommand::CheckAccountModels(ag.clone())).await;
        let cmd = next_command(&mut self_rx).await;
        b.handle(cmd).await;
        let state = drain(&mut rx)
            .into_iter()
            .rev()
            .find_map(|e| match e {
                AppEvent::AccountModels { id, state } if id == ag => Some(state),
                _ => None,
            })
            .expect("the popup hears how the check ended");
        assert!(!state.checking);
        assert!(state.error.as_deref().is_some_and(|e| e.contains("no Cloud project")), "{state:?}");
    }
}

/// Reusing a Google session another tool saved (Customizations > Account). Every file lives in a
/// temp folder: tests never read the credential files of the machine they run on.
#[cfg(test)]
mod imported_session_tests {
    use super::*;
    use crate::auth::antigravity::{Issuer, ANTIGRAVITY_CLIENT_ID, CODE_ASSIST_SCOPE};
    use crate::auth::Credentials;
    use base64::Engine as _;

    fn backend(dir: &std::path::Path) -> (Backend, mpsc::UnboundedReceiver<AppEvent>, mpsc::UnboundedReceiver<AppCommand>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let (self_tx, self_rx) = mpsc::unbounded_channel();
        let opts = AppOptions { data_dir: dir.to_path_buf(), workspace: None, model: None, mode: None, skip_permissions: false };
        (Backend::new(opts, CancellationToken::new(), tx, self_tx), rx, self_rx)
    }

    fn drain(rx: &mut mpsc::UnboundedReceiver<AppEvent>) -> Vec<AppEvent> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    /// No OAuth client secret, whatever the environment of the test run provides.
    fn without_client_secret(b: &mut Backend) {
        b.auth = None;
        b.client = None;
        b.client_source = None;
    }

    /// A temp home whose `.gemini` holds an Antigravity session for me@example.com that expires
    /// in `seconds`: the roots to search, and the file.
    fn antigravity_home(home: &std::path::Path, seconds: i64) -> (SearchRoots, PathBuf) {
        let b64 = |v: serde_json::Value| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(&v).unwrap());
        let claims = serde_json::json!({"aud": ANTIGRAVITY_CLIENT_ID, "email": "me@example.com"});
        let id_token = format!("{}.{}.sig", b64(serde_json::json!({"alg": "none"})), b64(claims));
        let dir = home.join(".gemini");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("oauth_creds.json");
        let creds = serde_json::json!({
            "access_token": "at-imported-secret",
            "refresh_token": "rt-imported-secret",
            "expiry_date": chrono::Utc::now().timestamp_millis() + seconds * 1000,
            "scope": CODE_ASSIST_SCOPE,
            "id_token": id_token,
        });
        std::fs::write(&file, serde_json::to_vec(&creds).unwrap()).unwrap();
        (SearchRoots { home: Some(home.to_path_buf()), ..SearchRoots::default() }, file)
    }

    #[tokio::test]
    async fn a_found_session_is_used_only_on_request_and_never_copied() {
        let data = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let (mut b, mut rx, _self_rx) = backend(data.path());
        without_client_secret(&mut b);
        // A test backend searches no real folder.
        b.handle(AppCommand::CheckImportedSession).await;
        assert!(drain(&mut rx).iter().any(|e| matches!(e, AppEvent::ImportCheck(ImportCheck::NotFound))));

        let (roots, file) = antigravity_home(home.path(), 40 * 60);
        b.import_roots = roots;
        b.handle(AppCommand::CheckImportedSession).await;
        let evs = drain(&mut rx);
        let found = evs
            .iter()
            .find_map(|e| match e {
                AppEvent::ImportCheck(ImportCheck::Found(s)) => Some(s.clone()),
                _ => None,
            })
            .expect("the session is reported");
        assert_eq!((found.issuer, found.account.as_deref(), found.usable, found.can_refresh), (Issuer::Antigravity, Some("me@example.com"), true, false));
        assert!(b.imported.is_none() && b.client.is_none(), "nothing is used before the user asks");

        b.handle(AppCommand::UseImportedSession).await;
        let evs = drain(&mut rx);
        assert!(evs.iter().any(|e| matches!(e, AppEvent::Auth(AuthStatus::LoggedIn { email: Some(m) }) if m == "me@example.com")), "{evs:?}");
        assert!(evs.iter().any(|e| matches!(e, AppEvent::ImportedSession(ImportedSessionEvent::Active(i)) if i.issuer == Issuer::Antigravity && !i.can_refresh)));
        assert!(b.client.is_some(), "Code Assist turns run on the imported session");
        assert!(!data.path().join("oauth_creds.json").exists(), "nothing is copied into AntiCraft's credential store");
        assert!(!format!("{evs:?}").contains("imported-secret"), "no token reaches the UI");

        // Signing out lets go of the session; the other app's file stays as it was.
        let before = std::fs::read(&file).unwrap();
        b.handle(AppCommand::Logout).await;
        let evs = drain(&mut rx);
        assert!(evs.iter().any(|e| matches!(e, AppEvent::ImportedSession(ImportedSessionEvent::Stopped))));
        assert!(evs.iter().any(|e| matches!(e, AppEvent::Auth(AuthStatus::Error(m)) if m.contains("not configured"))), "{evs:?}");
        assert!(b.imported.is_none() && b.client.is_none());
        assert_eq!(std::fs::read(&file).unwrap(), before);
    }

    #[tokio::test]
    async fn anticrafts_own_sign_in_takes_precedence() {
        let data = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        crate::auth::client_config::save(data.path(), "test-secret").unwrap();
        let own = Credentials {
            access_token: Some("own-at".into()),
            refresh_token: Some("own-rt".into()),
            expiry_date: Some(chrono::Utc::now().timestamp_millis() + 3_600_000),
            email: Some("own@example.com".into()),
            ..Credentials::default()
        };
        CredentialStore::in_dir(data.path()).save_merged(&own).unwrap();
        let (mut b, mut rx, _self_rx) = backend(data.path());
        let auth = b.auth.clone().expect("the client secret is configured");
        auth.load_cached().await.unwrap();
        (b.import_roots, _) = antigravity_home(home.path(), 40 * 60);
        b.handle(AppCommand::CheckImportedSession).await;
        b.handle(AppCommand::UseImportedSession).await;
        let evs = drain(&mut rx);
        assert!(b.imported.is_none());
        assert!(evs.iter().any(|e| matches!(e, AppEvent::Error(m) if m.contains("AntiCraft's own Google sign-in"))), "{evs:?}");

        // Signed out, the session can be used; a new sign-in of AntiCraft's own takes over again.
        auth.logout().await.unwrap();
        b.handle(AppCommand::UseImportedSession).await;
        assert!(b.imported.is_some());
        drain(&mut rx);
        b.login_id = 7;
        b.handle(AppCommand::LoginFinished(7, AuthStatus::LoggedIn { email: Some("own@example.com".into()) })).await;
        let evs = drain(&mut rx);
        assert!(b.imported.is_none());
        assert!(evs.iter().any(|e| matches!(e, AppEvent::ImportedSession(ImportedSessionEvent::Stopped))));
        assert!(evs.iter().any(|e| matches!(e, AppEvent::Auth(AuthStatus::LoggedIn { email: Some(m) }) if m == "own@example.com")));
    }

    /// A backend that never answers; it only gives the agent something to hold.
    struct Silent;

    impl ModelBackend for Silent {
        fn stream_turn(
            &self,
            _model: String,
            _user_prompt_id: String,
            _request: crate::api::GenerateContentRequest,
            _cancel: CancellationToken,
        ) -> futures::stream::BoxStream<'static, Result<crate::api::CaGenerateResponse, crate::api::CaError>> {
            use futures::StreamExt as _;
            futures::stream::empty().boxed()
        }
    }

    #[tokio::test]
    async fn a_turn_running_when_the_session_stops_does_not_keep_it_for_the_next_turn() {
        let data = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let (mut b, mut rx, _self_rx) = backend(data.path());
        without_client_secret(&mut b);
        (b.import_roots, _) = antigravity_home(home.path(), 40 * 60);
        b.handle(AppCommand::CheckImportedSession).await;
        b.handle(AppCommand::UseImportedSession).await;
        b.handle(AppCommand::NewChat).await;
        let conv = b.current.clone().expect("a conversation");
        // A turn runs on an agent built while the session was in use.
        let tools = ToolRegistry::new(Workspace::new(ws.path()).unwrap(), ToolLimits::default());
        let cfg = AgentConfig::new("m", String::new(), conv.id.to_string());
        b.agent = Some(Arc::new(Mutex::new(Agent::new(Arc::new(Silent), tools, cfg, History::default()))));
        b.turn_id = 1;
        b.turn_cancel = Some(b.shutdown.child_token());
        b.handle(AppCommand::StopImportedSession).await;
        assert!(b.imported.is_none() && b.client.is_none());
        assert!(b.agent.is_some(), "the running turn keeps its agent");
        let done = TurnDone { turn_id: 1, conversation_id: conv.id, snapshot: None, history: None, reason: None, artifacts: None, restore: Vec::new(), steer: Vec::new() };
        b.turn_finished(done);
        assert!(b.agent.is_none(), "the next turn builds a new agent, without the session");
        drain(&mut rx);
    }

    #[tokio::test]
    async fn a_session_with_nothing_valid_left_returns_to_signed_out_with_the_reason() {
        let data = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let (mut b, mut rx, mut self_rx) = backend(data.path());
        without_client_secret(&mut b);
        // Usable for four more seconds: a token is not handed out in its last minute.
        (b.import_roots, _) = antigravity_home(home.path(), 64);
        b.handle(AppCommand::CheckImportedSession).await;
        b.handle(AppCommand::UseImportedSession).await;
        let generation = b.imported.as_ref().expect("in use").generation;
        drain(&mut rx);
        b.handle(AppCommand::ImportedSessionChanged(generation + 1, Watch::Ended("an older session".into()))).await;
        assert!(b.imported.is_some(), "a report about another session is ignored");

        // The watcher re-reads the file, finds nothing newer and reports the end.
        let cmd = tokio::time::timeout(Duration::from_secs(20), self_rx.recv()).await.expect("the end is reported").expect("open");
        assert!(matches!(&cmd, AppCommand::ImportedSessionChanged(g, Watch::Ended(_)) if *g == generation), "{cmd:?}");
        b.handle(cmd).await;
        let evs = drain(&mut rx);
        assert!(b.imported.is_none() && b.client.is_none());
        let reason = evs
            .iter()
            .find_map(|e| match e {
                AppEvent::ImportedSession(ImportedSessionEvent::Ended(r)) => Some(r.clone()),
                _ => None,
            })
            .expect("the end and its reason reach the UI");
        assert!(reason.starts_with("The Antigravity session for me@example.com expires at ") && reason.contains("Sign in with Google"), "{reason}");
        assert!(reason.ends_with("Google sign-in first needs the OAuth client secret: see Customizations > Account."), "{reason}");
        let auth_at = evs.iter().position(|e| matches!(e, AppEvent::Auth(_))).expect("signed-in state");
        let ended_at = evs.iter().position(|e| matches!(e, AppEvent::ImportedSession(ImportedSessionEvent::Ended(_)))).unwrap();
        assert!(auth_at < ended_at, "the reason is the last banner");
        assert!(evs.iter().any(|e| matches!(e, AppEvent::ImportCheck(ImportCheck::Found(s)) if !s.usable)), "the file is looked at again");
    }
}
