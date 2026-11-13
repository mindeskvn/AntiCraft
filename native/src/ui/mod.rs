//! eframe/egui desktop UI. All backend work goes through [`crate::app`] channels; the UI
//! thread only drains events (`try_recv`) and sends commands, so it never blocks.
//!
//! [`UiState`] is a plain reducer over [`AppEvent`]s so UI logic is unit-testable without egui.
//! Layout follows DESIGN.md: menu bar, toolbar row, fixed 256px sidebar, and a central view
//! (`View::Chat` renders the Home composer while the conversation is empty).

pub mod account_models;
#[allow(dead_code)] // orchestration skeleton (ORCHESTRATION.md)
pub mod artifacts_panel;
pub mod bypass;
pub mod composer;
pub mod confirm;
pub mod conversation;
pub mod customizations;
pub mod demo;
pub mod format;
pub mod history;
pub mod home;
pub mod icons;
pub mod login;
pub mod markdown;
#[allow(dead_code)] // orchestration skeleton (ORCHESTRATION.md)
pub mod mentions;
pub mod model_picker;
pub mod progress_panel;
#[allow(dead_code)] // orchestration skeleton (ORCHESTRATION.md)
pub mod queue;
pub mod sidebar;
#[allow(dead_code)] // orchestration skeleton (ORCHESTRATION.md)
pub mod slash;
#[allow(dead_code)] // orchestration skeleton (ORCHESTRATION.md)
pub mod taskgroups;
pub mod theme;
pub mod titlebar;
pub mod widgets;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::agent::orchestrate::{ReviewOutcome, ReviewRequest};
use crate::agent::{AgentEvent, ConfirmOutcome, ConfirmationRequest, FinishReason};
use crate::api::{CaError, Content, Role, UsageMetadata};
use crate::app::{AccountLoginInfo, AccountModelsState, AppCommand, AppEvent, UiChannels};
use crate::auth::external::{AccountProvider, AccountStatus, AccountView};
use crate::auth::AuthStatus;
use crate::providers::ProviderKind;
use crate::settings::Settings;

/// The Add / Edit provider dialog of the Providers tab. Kept in [`UiState`] so it survives a tab
/// switch, and so the answer to "Load models" finds it.
#[derive(Clone)]
pub struct ProviderDialog {
    /// The profile being edited; `None` for a new one ("Add provider").
    pub editing: Option<String>,
    pub kind: ProviderKind,
    /// Empty means a default label (the kind's name) is chosen on Save.
    pub label: String,
    /// Empty means the kind's default endpoint.
    pub base_url: String,
    /// The key typed. While editing, empty keeps the saved key, which is never shown.
    pub api_key: String,
    /// Optional: the model new turns send, picked from `models` or typed. Without one, it is
    /// picked later from the composer's model menu.
    pub model: String,
    /// The Model field was typed in since the dialog opened or a model was picked from the list:
    /// its text narrows the list. A model the dialog opens with never hides the list.
    pub filtering: bool,
    /// What the provider lists: the profile's saved list, then what "Load models" returned.
    pub models: Vec<String>,
    /// The URL the last load listed from.
    pub models_from: Option<String>,
    /// Request number of the load in flight. Only its answer is taken; an edit of the endpoint
    /// clears it, so an answer about the old one is dropped.
    pub loading: Option<u64>,
    /// Why the last load failed.
    pub load_error: Option<String>,
    /// Why the last Save was refused.
    pub error: Option<String>,
}

impl ProviderDialog {
    /// The dialog for a new profile of `kind`.
    pub fn add(kind: ProviderKind) -> Self {
        Self {
            editing: None,
            kind,
            label: String::new(),
            base_url: String::new(),
            api_key: String::new(),
            model: String::new(),
            filtering: false,
            models: Vec::new(),
            models_from: None,
            loading: None,
            load_error: None,
            error: None,
        }
    }

    /// The dialog for `profile`, filled from it except for its key.
    pub fn edit(profile: &crate::providers::ProviderProfile) -> Self {
        Self {
            editing: Some(profile.id.clone()),
            label: profile.label.clone(),
            base_url: profile.base_url.clone().unwrap_or_default(),
            model: profile.model.clone(),
            models: profile.models.clone(),
            ..Self::add(profile.kind)
        }
    }
}

/// Hand-written so the typed key never reaches a log line or a panic message.
impl std::fmt::Debug for ProviderDialog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderDialog")
            .field("editing", &self.editing)
            .field("kind", &self.kind)
            .field("label", &self.label)
            .field("base_url", &self.base_url)
            .field("api_key", &if self.api_key.is_empty() { "<empty>" } else { "<redacted>" })
            .field("model", &self.model)
            .field("models", &self.models.len())
            .field("loading", &self.loading)
            .field("load_error", &self.load_error)
            .field("error", &self.error)
            .finish()
    }
}
use crate::store::ConversationSummary;

/// State of one tool call card.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCard {
    pub call_id: String,
    pub name: String,
    pub args: serde_json::Value,
    /// `None` while running.
    pub result: Option<(bool, String)>,
}

/// One rendered item in the chat pane.
#[derive(Debug, Clone, PartialEq)]
pub enum ChatItem {
    User(String),
    /// Assistant markdown, appended to while streaming.
    Assistant(String),
    /// Collapsible thinking block.
    Thought { subject: Option<String>, text: String },
    Tool(ToolCard),
    /// Inline notice (stopped, loop detected, ...).
    Notice(String),
    /// "Working..." / "Worked for 1m" header of a live turn. `duration` is set on `Finished`.
    TurnHeader { duration: Option<Duration> },
}

/// Kind of error banner shown above the transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Banner {
    Quota(String),
    Auth(String),
    Error(String),
}

/// A confirmation waiting for the user. Holds the reply channel.
pub struct PendingConfirm {
    pub request: ConfirmationRequest,
}

/// One plugin as the Customizations page needs it.
///
/// The UI keeps its own flat copy instead of holding [`crate::agent::plugins::Plugin`]: that
/// type owns `PathBuf`s and parsed MCP documents the page never reads, and discovery runs on
/// another thread, so a small owned row is the cheapest thing to send across.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PluginRow {
    /// Directory-name identity, also the settings key [`AppCommand::SetPluginChoice`] takes.
    pub id: String,
    /// Display name from the manifest, else the id.
    pub name: String,
    /// Manifest version, display only.
    pub version: Option<String>,
    pub description: Option<String>,
    /// Resolved state (`false` when a manifest disables it and the user made no choice).
    pub enabled: bool,
    /// "workspace" or "global": which discovery root found it.
    pub source: String,
    pub skill_count: usize,
    pub mcp_servers: usize,
}

/// The backend sends plugins as a flat tuple because discovery owns `PathBuf`s the page never
/// shows; this conversion is the one place the wire shape is spelled out.
impl From<(String, String, Option<String>, Option<String>, bool, String, usize, usize)> for PluginRow {
    #[allow(clippy::type_complexity)]
    fn from(t: (String, String, Option<String>, Option<String>, bool, String, usize, usize)) -> Self {
        let (id, name, version, description, enabled, source, skill_count, mcp_servers) = t;
        Self { id, name, version, description, enabled, source, skill_count, mcp_servers }
    }
}

/// Top-level page in the central area.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum View {
    /// Conversation; shows the Home layout while it has no items (see [`UiState::is_home`]).
    #[default]
    Chat,
    History,
    Customizations,
}

/// Screen shown in the central area; derived from [`View`] by [`UiState::screen`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Home,
    Conversation,
    History,
    Customizations,
}

/// What UI code emits each frame. Widgets push backend commands directly (DESIGN.md 6.1), so
/// this is an alias of [`AppCommand`] rather than a separate type.
pub type UiAction = AppCommand;

/// Tabs of the Customizations page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CustomizeTab {
    #[default]
    General,
    Agent,
    Workspaces,
    Providers,
    Account,
}

/// Number of chat items revealed per "Load older messages" step.
pub const ITEM_PAGE: usize = 200;

/// Pure UI state; updated by [`UiState::apply`].
pub struct UiState {
    pub auth: AuthStatus,
    pub conversations: Vec<ConversationSummary>,
    pub current: Option<Uuid>,
    pub current_title: String,
    pub workspace: Option<PathBuf>,
    pub settings: Settings,
    pub items: Vec<ChatItem>,
    pub busy: bool,
    pub banner: Option<Banner>,
    pub pending_confirm: Option<PendingConfirm>,
    pub input: String,
    /// Conversation being renamed in the sidebar: (id, edit buffer).
    pub renaming: Option<(Uuid, String)>,
    /// The active rename editor is the toolbar title (not the sidebar/history row).
    pub rename_in_toolbar: bool,
    /// True between `take_prompt` and the first agent event; a backend error in this window
    /// means the turn never started, so `busy` must be cleared.
    pub awaiting_turn: bool,

    // Navigation and view-only state.
    pub view: View,
    pub back: Vec<View>,
    pub forward: Vec<View>,
    pub sidebar_open: bool,
    /// The Progress panel at the right of a conversation, opened from the toolbar.
    pub progress_open: bool,
    pub projects_collapsed: bool,
    /// Expanded tool cards / thoughts and collapsed turn headers, keyed by item.
    pub expanded: HashSet<String>,
    pub history_query: String,
    pub history_project: Option<PathBuf>,
    pub focus_search: bool,
    pub customize_tab: CustomizeTab,
    /// Conversation awaiting an inline delete confirmation.
    pub confirm_delete: Option<Uuid>,
    /// How many trailing items are shown (grows by [`ITEM_PAGE`]).
    pub visible_items: usize,
    /// Settings edited on the Customizations page but not yet sent.
    pub settings_dirty: bool,
    /// What `settings` was edited from: the last backend copy, or the last copy sent. A field
    /// that differs from it is an edit not sent yet ([`UiState::apply`] keeps those).
    pub settings_base: Settings,
    /// Number of the last `SaveSettings` sent (0: none yet). An echo the backend sent before it
    /// handled that save carries a lower number and is ignored.
    pub settings_seq: u64,

    // Turn bookkeeping.
    pub turn_started: Option<Instant>,
    pub last_usage: Option<UsageMetadata>,
    /// Conversation whose turn is running (sidebar spinner).
    pub running: Option<Uuid>,

    /// Where the OAuth client secret came from (never the value itself).
    pub oauth_source: Option<crate::auth::ClientSource>,
    /// UI-only buffer for entering the OAuth client secret; cleared once sent.
    pub secret_input: String,
    /// Open the model picker on the next frame (demo screenshots).
    pub open_model_menu: bool,
    /// `request_review` the agent is waiting on. Answered from the artifacts panel or the
    /// review banner (see [`UiState::answer_review`]); dropped (= dismissed) on finish.
    pub pending_review: Option<ReviewRequest>,
    /// `AgentEvent::TaskUpdate`s of this session, used to refine the task group cards
    /// ([`taskgroups::apply_updates`]).
    pub task_updates: Vec<taskgroups::TaskUpdate>,
    /// Discovered plugins, shown in Customizations. Empty until the backend reports them.
    pub plugins: Vec<PluginRow>,
    /// MCP servers as `(name, connected, tool_count)`, from the hub's `statuses()`.
    pub mcp_status: Vec<(String, bool, usize)>,
    /// The Add / Edit provider dialog, while it is open.
    pub provider_dialog: Option<ProviderDialog>,
    /// The open dialog belongs to Bypass Mode's section (it was opened from there), so it is
    /// drawn on that tab, and a profile it saves becomes the mode's provider.
    pub bypass_dialog: bool,
    /// Number of the last "Load models" sent ([`AppCommand::LoadProviderModels`]).
    pub provider_models_seq: u64,
    /// Profile whose Delete button was clicked once; the next click on it deletes, any other
    /// click disarms it.
    pub provider_delete_armed: Option<String>,
    /// What the backend's last look for another tool's Google session found (no token).
    pub import_check: Option<crate::auth::antigravity::ImportCheck>,
    /// The imported session in use, if any (no token). `auth` is `LoggedIn` meanwhile.
    pub imported_session: Option<crate::auth::antigravity::SessionInfo>,
    /// Why the last imported session ended, shown on the Account tab until the next check.
    pub import_ended: Option<String>,
    /// The backend reported that no OAuth client secret is configured.
    pub oauth_client_missing: bool,
    /// Every account, as the backend last reported it. Holds no token. While a sign-in runs the
    /// list ends with a placeholder for its provider (empty id, `SigningIn`); see
    /// [`UiState::account_signing_in`].
    pub accounts: Vec<AccountView>,
    /// A failed sign-in, per provider, shown under that provider's row until the next attempt.
    /// The single source of truth for that message; it is filled from `AccountLoginInfo.error`
    /// when the backend reports one, so a re-emitted `AccountLogin` cannot clear it.
    pub account_errors: HashMap<AccountProvider, String>,
    /// Popup state per account id. The count line's timestamp comes from `AccountView`.
    pub accounts_models: HashMap<String, AccountModelsState>,
    /// Retained attempt for the paste popup (its id and auth URL, never a token).
    pub account_login: Option<AccountLoginInfo>,
    /// Text of the paste popup's field. It holds an authorization code once something is pasted,
    /// so it is emptied when it is sent, cancelled, or its attempt is gone.
    pub account_link_input: String,
    /// Open the models popup for this account on the next frame (demo screenshots only).
    pub open_account_models: Option<String>,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            auth: AuthStatus::LoggedOut,
            conversations: Vec::new(),
            current: None,
            current_title: String::new(),
            workspace: None,
            settings: Settings::default(),
            items: Vec::new(),
            busy: false,
            banner: None,
            pending_confirm: None,
            input: String::new(),
            renaming: None,
            rename_in_toolbar: false,
            awaiting_turn: false,
            view: View::Chat,
            back: Vec::new(),
            forward: Vec::new(),
            sidebar_open: true,
            progress_open: false,
            projects_collapsed: false,
            expanded: HashSet::new(),
            history_query: String::new(),
            history_project: None,
            focus_search: false,
            customize_tab: CustomizeTab::General,
            confirm_delete: None,
            visible_items: ITEM_PAGE,
            settings_dirty: false,
            settings_base: Settings::default(),
            settings_seq: 0,
            turn_started: None,
            last_usage: None,
            running: None,
            oauth_source: None,
            secret_input: String::new(),
            open_model_menu: false,
            pending_review: None,
            task_updates: Vec::new(),
            plugins: Vec::new(),
            mcp_status: Vec::new(),
            provider_dialog: None,
            bypass_dialog: false,
            provider_models_seq: 0,
            provider_delete_armed: None,
            import_check: None,
            imported_session: None,
            import_ended: None,
            oauth_client_missing: false,
            accounts: Vec::new(),
            account_errors: HashMap::new(),
            accounts_models: HashMap::new(),
            account_login: None,
            account_link_input: String::new(),
            open_account_models: None,
        }
    }
}

/// Convert stored history to chat items (thoughts are not stored, tool calls become cards).
pub fn items_from_history(messages: &[Content]) -> Vec<ChatItem> {
    let mut items = Vec::new();
    for c in messages {
        for p in &c.parts {
            if let Some(fc) = &p.function_call {
                items.push(ChatItem::Tool(ToolCard {
                    call_id: fc.id.clone().unwrap_or_default(),
                    name: fc.name.clone(),
                    args: fc.args.clone(),
                    result: None,
                }));
            } else if let Some(fr) = &p.function_response {
                let id = fr.id.clone().unwrap_or_default();
                let ok = fr.response.get("error").is_none();
                if let Some(ChatItem::Tool(card)) =
                    items.iter_mut().rev().find(|i| matches!(i, ChatItem::Tool(t) if t.call_id == id))
                {
                    card.result = Some((ok, if ok { "done".into() } else { "error".into() }));
                }
            } else if let Some(t) = p.text.as_deref().filter(|t| !t.is_empty()) {
                items.push(match c.role {
                    Some(Role::Model) => ChatItem::Assistant(t.to_owned()),
                    _ => ChatItem::User(t.to_owned()),
                });
            }
        }
    }
    items
}

/// Number of user bubbles [`items_from_history`] makes from `messages`: the non-empty text
/// parts of non-model contents (prompts and steering; function responses make none). Rewind
/// checkpoints are keyed by it, so the position of a bubble in the transcript names the
/// snapshot taken just before it.
pub fn user_bubble_count(messages: &[Content]) -> usize {
    messages
        .iter()
        .filter(|c| !matches!(c.role, Some(Role::Model)))
        .flat_map(|c| &c.parts)
        .filter(|p| p.function_call.is_none() && p.function_response.is_none() && p.text.as_deref().is_some_and(|t| !t.is_empty()))
        .count()
}

impl UiState {
    /// Apply one backend event. A replaced confirmation is answered with `Cancel`.
    /// Push an `AgentEvent` into the transcript, keeping the group updates it carries.
    fn apply_agent(&mut self, ev: AgentEvent) {
        if let AgentEvent::TaskUpdate(b) = &ev {
            let turn = taskgroups::turn_of(&self.items);
            self.task_updates.push(taskgroups::TaskUpdate { boundary: b.clone(), turn });
            return;
        }
        self.apply_agent_event(ev)
    }

    pub fn apply(&mut self, ev: AppEvent) {
        match ev {
            AppEvent::Auth(s) => {
                if let AuthStatus::Error(e) = &s {
                    // This is the Gemini CLI sign-in's state, and it is a banner only when that is
                    // what stops the next turn. A missing client secret never does: the Antigravity
                    // sign-in offered everywhere needs none, and the Account tab says what the
                    // Gemini CLI sign-in needs. With an account or a provider profile in use, no
                    // Google sign-in is needed at all, and the banner's "Sign in again" would
                    // point at that account instead of at Google.
                    let optional = e.contains("not configured") || self.any_backend_ready();
                    if !optional {
                        self.banner = Some(Banner::Auth(e.clone()));
                    }
                }
                self.auth = s;
            }
            AppEvent::Conversations(list) => {
                if let Some(c) = self.current.and_then(|id| list.iter().find(|c| c.id == id)) {
                    self.current_title = c.title.clone();
                }
                self.conversations = list;
            }
            AppEvent::ConversationLoaded(c) => {
                // A prompt sent with no open conversation makes the backend create one; keep
                // the optimistic user item and turn header instead of wiping them.
                let keep = self.awaiting_turn && self.current.is_none() && c.messages.is_empty();
                self.current = Some(c.id);
                self.current_title = c.title.clone();
                if keep {
                    self.running = Some(c.id);
                } else {
                    self.items = items_from_history(&c.messages);
                    self.expanded.clear();
                    self.visible_items = ITEM_PAGE;
                    self.banner = None;
                    self.task_updates.clear();
                }
                self.navigate(View::Chat);
            }
            AppEvent::WorkspaceChanged(w) => self.workspace = w,
            // An echo the backend sent before it handled the last save from here is older than
            // what the UI holds: taken, it would show again what the user just changed (a deleted
            // profile), and the next save would send that back. The save's own echo follows.
            AppEvent::Settings(applied, s) if applied >= self.settings_seq => {
                // The backend echoes every save, but runs commands one at a time: the echo of an
                // earlier save can arrive after the user edited again (it was queued behind a
                // slow command). Edits not sent yet are kept on top of the echo, and stay dirty.
                if self.settings_dirty {
                    self.settings = crate::settings::merge_unsent(&self.settings_base, &self.settings, &s);
                    self.settings_dirty = self.settings != s;
                } else {
                    self.settings = s.clone();
                }
                self.settings_base = s;
            }
            AppEvent::Settings(..) => {}
            // Generic errors (settings, list refresh, save) never change the busy state.
            AppEvent::OAuthClientSource(src) => {
                self.oauth_source = src;
                self.oauth_client_missing = src.is_none();
                if src.is_some() && matches!(&self.banner, Some(Banner::Auth(e)) if e.contains("not configured")) {
                    self.banner = None;
                }
            }
            AppEvent::ImportCheck(check) => self.import_check = Some(check),
            AppEvent::ImportedSession(change) => match change {
                crate::app::ImportedSessionEvent::Active(info) => {
                    self.imported_session = Some(info);
                    self.import_ended = None;
                }
                crate::app::ImportedSessionEvent::Stopped => self.imported_session = None,
                crate::app::ImportedSessionEvent::Ended(reason) => {
                    self.imported_session = None;
                    // The session only served Code Assist: with an account or a provider in use
                    // nothing stopped working, so the Account tab alone says why.
                    if !self.any_backend_ready() {
                        self.banner = Some(Banner::Auth(reason.clone()));
                    }
                    self.import_ended = Some(reason);
                }
            },
            // Plugins and MCP status are passive: they only feed the Customizations rows, so a
            // late arrival never has to re-render anything else.
            AppEvent::Plugins(list) => self.plugins = list.into_iter().map(PluginRow::from).collect(),
            AppEvent::McpStatus(status) => self.mcp_status = status,
            // Subscription accounts. The backend always sends whole states, so each arm assigns:
            // a dropped event cannot leave a stale `checking: true` behind.
            AppEvent::Accounts(list) => self.accounts = list,
            AppEvent::AccountModels { id, state } => {
                self.accounts_models.insert(id, state);
            }
            // Only the load the open dialog waits for: a dialog closed or reopened meanwhile, or
            // an endpoint edited since, does not take an answer about something else.
            AppEvent::BypassFiles(files) => {
                // Added in the order the picker lists them; `sanitized` drops duplicates and
                // empty paths on the way to settings.json, so this only has to append.
                let list = &mut self.settings.bypass.files;
                for f in files {
                    if list.len() >= crate::settings::MAX_BYPASS_FILES {
                        break;
                    }
                    if !list.contains(&f) {
                        list.push(f);
                    }
                }
                self.settings_dirty = true;
            }
            AppEvent::ProviderModels { request, result } => {                if let Some(d) = self.provider_dialog.as_mut().filter(|d| d.loading == Some(request)) {
                    d.loading = None;
                    match result {
                        Ok(loaded) => {
                            d.models = loaded.models;
                            d.models_from = Some(loaded.from);
                            d.load_error = None;
                        }
                        // The list loaded before stays under the message.
                        Err(e) => d.load_error = Some(e),
                    }
                }
            }
            AppEvent::AccountLogin { login } => {
                // The row's error lives in `account_errors`. A report with an error sets it; one
                // without (an attempt starting, or one that was cancelled) clears it; `None` (the
                // attempt is gone) leaves it on the row until the next attempt.
                if let Some(info) = &login {
                    match &info.error {
                        Some(e) => {
                            self.account_errors.insert(info.provider, e.clone());
                        }
                        None => {
                            self.account_errors.remove(&info.provider);
                        }
                    }
                } else {
                    // No attempt is left for a pasted link to finish; a link typed for one must
                    // not linger (it carries an authorization code).
                    self.account_link_input.clear();
                }
                self.account_login = login;
            }
            AppEvent::Error(e) => self.banner = Some(Banner::Error(e)),
            // Queue, index and workflows need the egui context; `drain_events` routes them to
            // the composer before they reach here.
            AppEvent::Queue(_)
            | AppEvent::WorkspaceIndex(_)
            | AppEvent::Workflows(_)
            | AppEvent::Artifact { .. }
            | AppEvent::FileChanges(_) => {}
            AppEvent::PromptStarted { conversation, text } => {
                // The user did not send this one just now (a queued message, a late steer), so
                // the page stays: they may be typing in a Customizations field.
                if self.current == Some(conversation) && !self.busy {
                    let draft = std::mem::replace(&mut self.input, text);
                    self.push_prompt();
                    self.input = draft;
                }
            }
            AppEvent::TurnRejected(e) => {
                self.awaiting_turn = false;
                self.busy = false;
                self.turn_started = None;
                self.running = None;
                if matches!(self.items.last(), Some(ChatItem::TurnHeader { duration: None })) {
                    self.items.pop();
                }
                self.banner = Some(Banner::Error(e));
            }
            AppEvent::Agent(a) => {
                self.awaiting_turn = false;
                self.apply_agent(a)
            }
            AppEvent::TurnEvent { conversation, event } => {
                self.awaiting_turn = false;
                let finished = matches!(event, AgentEvent::Finished { .. });
                self.running = if finished { None } else { Some(conversation) };
                if self.current == Some(conversation) {
                    self.apply_agent(event);
                } else if finished {
                    // A turn of a conversation that is no longer shown ended: only clear busy.
                    self.busy = false;
                    self.turn_started = None;
                }
                // Other events of a hidden conversation are dropped; a dropped confirmation
                // closes its reply channel, which the agent treats as a rejection.
            }
        }
    }

    fn apply_agent_event(&mut self, ev: AgentEvent) {
        match ev {
            AgentEvent::Content(t) => match self.items.last_mut() {
                Some(ChatItem::Assistant(s)) => s.push_str(&t),
                _ => self.items.push(ChatItem::Assistant(t)),
            },
            AgentEvent::Thought { subject, text } => match self.items.last_mut() {
                Some(ChatItem::Thought { text: existing, subject: s }) if subject.is_none() || *s == subject => {
                    existing.push_str(&text)
                }
                _ => self.items.push(ChatItem::Thought { subject, text }),
            },
            AgentEvent::ToolCallRequest { call_id, name, args } => {
                self.items.push(ChatItem::Tool(ToolCard { call_id, name, args, result: None }))
            }
            AgentEvent::ToolCallResponse { call_id, ok, display, .. } => {
                if let Some(ChatItem::Tool(card)) =
                    self.items.iter_mut().rev().find(|i| matches!(i, ChatItem::Tool(t) if t.call_id == call_id))
                {
                    card.result = Some((ok, display));
                }
            }
            AgentEvent::Confirmation(req) => {
                if let Some(old) = self.pending_confirm.replace(PendingConfirm { request: req }) {
                    let _ = old.request.respond.send(ConfirmOutcome::Cancel);
                }
            }
            AgentEvent::Retry { attempt } => self.items.push(ChatItem::Notice(format!("Retrying (attempt {attempt})"))),
            AgentEvent::Usage(u) => self.last_usage = Some(u),
            AgentEvent::Error(e) => {
                self.banner = Some(match &e {
                    CaError::TerminalQuota { .. } | CaError::Retryable { .. } => Banner::Quota(e.to_string()),
                    // A turn's auth error comes from the backend in use. For an account it is the
                    // provider's own sentence, so it is said with the account's name: read bare,
                    // it looks like a Google sign-in problem.
                    CaError::Auth(_) => Banner::Auth(match self.settings.active_account_entry() {
                        Some(a) => format!("{} account: {e}", a.provider.label()),
                        None => e.to_string(),
                    }),
                    _ => Banner::Error(e.to_string()),
                })
            }
            AgentEvent::ReviewRequested(req) => {
                // A newer review replaces an older one (the older is dismissed).
                if let Some(old) = self.pending_review.replace(req) {
                    let _ = old.respond.send(ReviewOutcome::Cancelled);
                }
            }
            AgentEvent::Notify(t) => self.items.push(ChatItem::Notice(t)),
            AgentEvent::SteerInjected(t) => self.items.push(ChatItem::User(t)),
            AgentEvent::Compacted { removed, .. } => {
                self.items.push(ChatItem::Notice(format!("Condensed {removed} older messages into a summary.")))
            }
            AgentEvent::TaskComplete { result } => {
                // The result closes the turn; show it unless the model already said it.
                let shown = matches!(self.items.last(), Some(ChatItem::Assistant(t)) if t.trim() == result.trim());
                if !result.trim().is_empty() && !shown {
                    self.items.push(ChatItem::Assistant(result));
                }
            }
            // Task groups and artifacts are derived from the tool cards (ui::taskgroups,
            // ui::artifacts_panel), so these need no extra state.
            AgentEvent::TaskUpdate(_) | AgentEvent::ArtifactChanged { .. } => {}
            AgentEvent::Finished { reason, .. } => {
                self.busy = false;
                self.pending_confirm = None;
                self.pending_review = None;
                self.running = None;
                let elapsed = self.turn_started.take().map(|t| t.elapsed());
                if let Some(ChatItem::TurnHeader { duration }) =
                    self.items.iter_mut().rev().find(|i| matches!(i, ChatItem::TurnHeader { .. }))
                {
                    if duration.is_none() {
                        *duration = Some(elapsed.unwrap_or_default());
                    }
                }
                let notice = match reason {
                    FinishReason::Completed | FinishReason::Error => None,
                    FinishReason::Cancelled => Some("Stopped.".to_owned()),
                    FinishReason::ToolRejected => Some("Tool call rejected.".to_owned()),
                    FinishReason::MaxTurns => Some("Stopped: turn limit reached.".to_owned()),
                    FinishReason::LoopDetected(why) => Some(format!("Stopped: loop detected ({why}).")),
                };
                if let Some(n) = notice {
                    self.items.push(ChatItem::Notice(n));
                }
            }
        }
    }

    /// Take the input as a prompt if sendable. Pushes the user item plus a turn header, marks
    /// busy and shows the conversation.
    pub fn take_prompt(&mut self) -> Option<String> {
        let text = self.push_prompt()?;
        self.navigate(View::Chat);
        Some(text)
    }

    /// [`UiState::take_prompt`] without leaving the current page.
    fn push_prompt(&mut self) -> Option<String> {
        let text = self.input.trim().to_owned();
        if text.is_empty() || self.busy {
            return None;
        }
        self.input.clear();
        self.items.push(ChatItem::User(text.clone()));
        // Updates of the previous turn are settled by the transcript from here on.
        self.task_updates.retain(|u| u.turn >= taskgroups::turn_of(&self.items));
        self.items.push(ChatItem::TurnHeader { duration: None });
        self.busy = true;
        self.awaiting_turn = true;
        self.banner = None;
        self.turn_started = Some(Instant::now());
        self.running = self.current;
        Some(text)
    }

    /// Answer the pending review if `call_id` matches it (a stale answer is ignored).
    pub fn answer_review(&mut self, call_id: &str, outcome: ReviewOutcome) -> bool {
        if self.pending_review.as_ref().is_some_and(|r| r.call_id == call_id) {
            if let Some(r) = self.pending_review.take() {
                return r.respond.send(outcome).is_ok();
            }
        }
        false
    }

    /// Answer the pending confirmation.
    pub fn answer_confirm(&mut self, outcome: ConfirmOutcome) {
        if let Some(p) = self.pending_confirm.take() {
            let _ = p.request.respond.send(outcome);
        }
    }

    /// Reset the chat pane for a new conversation.
    pub fn new_chat(&mut self) {
        self.current = None;
        self.current_title.clear();
        self.items.clear();
        self.task_updates.clear();
        self.expanded.clear();
        self.visible_items = ITEM_PAGE;
        self.banner = None;
        self.navigate(View::Chat);
    }

    /// Screen to render for the current view.
    pub fn screen(&self) -> Screen {
        match self.view {
            View::Chat if self.is_home() => Screen::Home,
            View::Chat => Screen::Conversation,
            View::History => Screen::History,
            View::Customizations => Screen::Customizations,
        }
    }

    /// The Chat view shows the Home composer (no transcript yet).
    pub fn is_home(&self) -> bool {
        self.view == View::Chat && self.items.is_empty() && !self.busy
    }

    /// Go to `v`, pushing the current view on the back stack.
    pub fn navigate(&mut self, v: View) {
        if self.view != v {
            self.back.push(self.view);
            self.back.truncate(50);
            self.forward.clear();
            self.view = v;
        }
    }

    pub fn go_back(&mut self) {
        if let Some(v) = self.back.pop() {
            self.forward.push(self.view);
            self.view = v;
        }
    }

    pub fn go_forward(&mut self) {
        if let Some(v) = self.forward.pop() {
            self.back.push(self.view);
            self.view = v;
        }
    }

    /// Toggle membership of `key` in [`Self::expanded`].
    pub fn toggle(&mut self, key: String) {
        if !self.expanded.remove(&key) {
            self.expanded.insert(key);
        }
    }

    /// The settings to send with `SaveSettings`. The copy sent becomes the base later edits
    /// are compared with, so a late echo of an older save cannot undo them.
    pub fn take_settings_save(&mut self) -> Settings {
        self.settings_dirty = false;
        self.settings_seq += 1;
        self.settings_base = self.settings.clone();
        self.settings.clone()
    }

    /// `SaveSettings` for the unsent edits: numbered, and with the copy they were made from, so
    /// the backend applies only the fields changed here.
    pub fn settings_save_command(&mut self) -> AppCommand {
        let base = self.settings_base.clone();
        let settings = self.take_settings_save();
        AppCommand::SaveSettings { seq: self.settings_seq, base: Box::new(base), settings: Box::new(settings) }
    }

    /// The OAuth client secret is missing (sign-in cannot work until it is configured). Also
    /// true while a reused session keeps `auth` signed in.
    pub fn oauth_not_configured(&self) -> bool {
        self.oauth_source.is_none()
            && (self.oauth_client_missing || matches!(&self.auth, AuthStatus::Error(e) if e.contains("not configured")))
    }

    /// Signed in with AntiCraft's own Google credentials (not a reused session).
    pub fn own_sign_in(&self) -> bool {
        matches!(self.auth, AuthStatus::LoggedIn { .. }) && self.imported_session.is_none()
    }

    /// A turn can be sent without a Google sign-in: an account or a provider profile drives it.
    pub fn any_backend_ready(&self) -> bool {
        self.settings.active_account_entry().is_some() || self.settings.active_provider_profile().is_some()
    }

    /// The provider whose account sign-in is running.
    ///
    /// No account exists until a sign-in finishes, so the backend lists one placeholder row for
    /// it meanwhile: an empty id and [`AccountStatus::SigningIn`]. That row is the only signal;
    /// `account_login` alone cannot tell a running attempt from one kept for a pasted link.
    pub fn account_signing_in(&self) -> Option<AccountProvider> {
        self.accounts.iter().find(|v| v.id.is_empty() && v.status == AccountStatus::SigningIn).map(|v| v.provider)
    }

    /// The attempt a Cancel stops, while one runs. Cancel always goes by this attempt id, never
    /// by the placeholder row's account id, which is empty.
    pub fn running_account_login(&self) -> Option<&AccountLoginInfo> {
        let provider = self.account_signing_in()?;
        self.account_login.as_ref().filter(|l| l.provider == provider)
    }

    /// Signed-in accounts, in the backend's order (never the sign-in placeholder).
    pub fn signed_in_accounts(&self) -> impl Iterator<Item = &AccountView> {
        self.accounts.iter().filter(|v| !v.id.is_empty() && v.status == AccountStatus::SignedIn)
    }

    /// The account in use, when its session ended (a refresh found it dead). Settings keep it
    /// selected for the rest of the session, so [`Self::any_backend_ready`] still says yes while
    /// every turn is refused until it is signed in again.
    pub fn lapsed_active_account(&self) -> Option<&AccountView> {
        let id = self.settings.active_account_entry()?.id.as_str();
        self.accounts.iter().find(|v| v.id == id && !matches!(v.status, AccountStatus::SignedIn | AccountStatus::SigningIn))
    }

    /// The account the sidebar and Home name: the one in use, else the first signed in.
    pub fn shown_account(&self) -> Option<&AccountView> {
        let active = self.settings.active_account_entry().map(|e| e.id.as_str());
        self.signed_in_accounts().find(|v| Some(v.id.as_str()) == active).or_else(|| self.signed_in_accounts().next())
    }

    /// Transcript as Markdown (for "Copy as Markdown").
    pub fn transcript_markdown(&self) -> String {
        let mut out = String::new();
        for i in &self.items {
            match i {
                ChatItem::User(t) => out.push_str(&format!("**You:** {t}\n\n")),
                ChatItem::Assistant(t) => out.push_str(&format!("{t}\n\n")),
                ChatItem::Tool(c) => out.push_str(&format!("> tool `{}`\n\n", c.name)),
                ChatItem::Notice(t) => out.push_str(&format!("_{t}_\n\n")),
                ChatItem::Thought { .. } | ChatItem::TurnHeader { .. } => {}
            }
        }
        out
    }
}

/// eframe application.
pub struct AntiCraftApp {
    state: UiState,
    channels: UiChannels,
    md_cache: egui_commonmark::CommonMarkCache,
    shutdown: CancellationToken,
    /// Fixture mode: save a screenshot here after a few frames, then close.
    snapshot: Option<PathBuf>,
    frames: u32,
}

impl AntiCraftApp {
    pub fn new(cc: &eframe::CreationContext<'_>, channels: UiChannels, shutdown: CancellationToken) -> Self {
        Self::with_state(cc, channels, shutdown, UiState::default(), None)
    }

    /// Start with a prepared state (fixture screenshots). No backend is involved.
    pub fn with_state(
        cc: &eframe::CreationContext<'_>,
        channels: UiChannels,
        shutdown: CancellationToken,
        state: UiState,
        snapshot: Option<PathBuf>,
    ) -> Self {
        theme::apply(&cc.egui_ctx);
        Self { state, channels, md_cache: egui_commonmark::CommonMarkCache::default(), shutdown, snapshot, frames: 0 }
    }

    /// Send a command; errors only happen after backend shutdown and are ignored.
    pub fn send(&self, cmd: AppCommand) {
        let _ = self.channels.commands.send(cmd);
    }

    fn drain_events(&mut self, ctx: &egui::Context) {
        while let Ok(ev) = self.channels.events.try_recv() {
            match ev {
                AppEvent::Queue(q) => composer::on_queue(ctx, q),
                AppEvent::WorkspaceIndex(i) => composer::set_workspace_index(ctx, i),
                AppEvent::Workflows(w) => composer::set_workflows(ctx, w),
                ev => self.state.apply(ev),
            }
        }
        // Hand a review answer posted by the panel or banner to the waiting agent.
        if let Some((id, outcome)) = artifacts_panel::take_review_answer(ctx) {
            self.state.answer_review(&id, outcome);
        }
    }

    fn shortcuts(&mut self, ctx: &egui::Context, commands: &mut Vec<UiAction>) {
        let (new, side) = ctx.input_mut(|i| {
            (
                i.consume_shortcut(&egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::N)),
                i.consume_shortcut(&egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::B)),
            )
        });
        if new && !self.state.busy {
            self.state.new_chat();
            commands.push(AppCommand::NewChat);
        }
        if side {
            self.state.sidebar_open = !self.state.sidebar_open;
        }
    }

    fn snapshot_step(&mut self, ctx: &egui::Context) {
        let Some(path) = self.snapshot.clone() else { return };
        self.frames += 1;
        ctx.request_repaint();
        // Wait for popup fade-in and hover animations to settle before capturing.
        if self.frames == 30 {
            let c = ctx.clone();
            ctx.request_screenshot(move |img| {
                if let Err(e) = demo::save_png(&path, &img) {
                    eprintln!("snapshot failed: {e}");
                }
                c.send_viewport_cmd(egui::ViewportCommand::Close);
            });
        }
    }
}

/// Drop phantom key presses produced by injected characters (Unikey/EVKey send `VK_PACKET`,
/// whose scancode makes winit report a bogus physical key, e.g. space -> `Key::D`). A press is
/// dropped when it is immediately followed by `Text` that the key cannot have produced and no
/// command modifier is held. Editing keys are always kept. Text order is never changed.
pub fn sanitize_injected_keys(events: &mut Vec<egui::Event>) {
    use egui::{Event, Key};
    const KEEP: &[Key] = &[
        Key::Backspace, Key::Delete, Key::Enter, Key::Tab, Key::Escape, Key::ArrowLeft, Key::ArrowRight,
        Key::ArrowUp, Key::ArrowDown, Key::Home, Key::End, Key::PageUp, Key::PageDown,
    ];
    let mut drop = vec![false; events.len()];
    for i in 0..events.len().saturating_sub(1) {
        if let (Event::Key { key, pressed: true, modifiers, .. }, Event::Text(t)) = (&events[i], &events[i + 1]) {
            if KEEP.contains(key) || modifiers.ctrl || modifiers.alt || modifiers.command {
                continue;
            }
            let produced = match key {
                Key::Space => t == " ",
                k => {
                    let n = k.symbol_or_name();
                    n.eq_ignore_ascii_case(t) || k.name().eq_ignore_ascii_case(t)
                }
            };
            drop[i] = !produced;
        }
    }
    let mut i = 0;
    events.retain(|_| {
        i += 1;
        !drop[i - 1]
    });
}

impl eframe::App for AntiCraftApp {
    /// Input order trace (debug builds, `ANTICRAFT_KEYTRACE=1`) and the injected-key filter.
    /// The trace records typed text, so it never exists in release builds.
    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        #[cfg(debug_assertions)]
        if std::env::var_os("ANTICRAFT_KEYTRACE").is_some() {
            for e in &raw_input.events {
                match e {
                    egui::Event::Key { key, pressed, .. } => tracing::debug!(target: "keytrace", "Key({key:?}, pressed={pressed})"),
                    egui::Event::Text(t) => tracing::debug!(target: "keytrace", "Text({t:?})"),
                    egui::Event::Ime(i) => tracing::debug!(target: "keytrace", "Ime({i:?})"),
                    _ => {}
                }
            }
        }
        sanitize_injected_keys(&mut raw_input.events);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.drain_events(&ctx);
        let mut commands = Vec::new();
        self.shortcuts(&ctx, &mut commands);
        let state = &mut self.state;

        egui::Panel::top("titlebar")
            .exact_size(theme::TOP_BAR_HEIGHT + theme::TOOLBAR_HEIGHT)
            .show_separator_line(false)
            .frame(egui::Frame::NONE.fill(theme::BG))
            .show(ui, |ui| titlebar::show(ui, state, &mut commands));
        if state.sidebar_open {
            egui::Panel::left("sidebar")
                .exact_size(theme::SIDEBAR_WIDTH)
                .resizable(false)
                .show_separator_line(false)
                .frame(egui::Frame::NONE.fill(theme::SIDEBAR).inner_margin(egui::Margin { left: 8, right: 8, top: 8, bottom: 8 }))
                .show(ui, |ui| sidebar::show(ui, state, &mut commands));
        }
        let md_cache = &mut self.md_cache;
        egui::CentralPanel::default().frame(egui::Frame::NONE.fill(theme::BG)).show(ui, |ui| match state.screen() {
            Screen::Home => home::show(ui, state, &mut commands),
            Screen::Conversation => conversation::show(ui, state, md_cache, &mut commands),
            Screen::History => history::show(ui, state, &mut commands),
            Screen::Customizations => customizations::show(ui, state, &mut commands),
        });

        // Customizations apply immediately, but drags and text edits are flushed only once
        // the pointer is released and no text field is focused, so the file is not rewritten
        // every frame.
        if state.settings_dirty && !ctx.input(|i| i.pointer.any_down()) && !ctx.text_edit_focused() {
            commands.push(state.settings_save_command());
        }
        for c in commands {
            self.send(c);
        }
        self.snapshot_step(&ctx);
    }

    fn on_exit(&mut self) {
        self.state.answer_confirm(ConfirmOutcome::Cancel);
        if let Some(r) = self.state.pending_review.take() {
            let _ = r.respond.send(ReviewOutcome::Cancelled);
        }
        if self.state.settings_dirty {
            let save = self.state.settings_save_command();
            self.send(save);
        }
        self.send(AppCommand::Shutdown);
        self.shutdown.cancel();
    }
}

/// Headless rendering helpers shared by the UI tests.
#[cfg(test)]
pub(crate) mod test_util {
    use egui::{Rect, Vec2};

    /// A context with the app theme, as `AntiCraftApp` sets it up.
    pub fn context() -> egui::Context {
        let ctx = egui::Context::default();
        super::theme::apply(&ctx);
        ctx
    }

    /// Run one 1400x1600 frame and return every painted text with its screen rect, so a test
    /// can check what a view shows and find where to click.
    pub fn painted(ctx: &egui::Context, events: Vec<egui::Event>, add: impl FnMut(&mut egui::Ui)) -> Vec<(String, Rect)> {
        painted_in(ctx, Vec2::new(1400.0, 1600.0), events, add)
    }

    /// [`painted`] in a window of `size`.
    pub fn painted_in(ctx: &egui::Context, size: Vec2, events: Vec<egui::Event>, mut add: impl FnMut(&mut egui::Ui)) -> Vec<(String, Rect)> {
        let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, size)), events, ..Default::default() };
        let mut out = ctx.run_ui(input, |ui| add(ui));
        // Nothing renders here: unapplied texture deltas panic when dropped (debug builds).
        out.textures_delta.clear();
        let mut texts = Vec::new();
        for clipped in &out.shapes {
            collect_texts(&clipped.shape, &mut texts);
        }
        texts
    }

    fn collect_texts(shape: &egui::Shape, out: &mut Vec<(String, Rect)>) {
        match shape {
            egui::Shape::Text(t) => out.push((t.galley.text().to_owned(), t.visual_bounding_rect())),
            egui::Shape::Vec(shapes) => shapes.iter().for_each(|s| collect_texts(s, out)),
            _ => {}
        }
    }

    pub fn click(pos: egui::Pos2, pressed: bool) -> egui::Event {
        egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE }
    }

    pub fn key(key: egui::Key, modifiers: egui::Modifiers) -> egui::Event {
        egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers }
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::agent::ConfirmationDetails;

    fn provider(id: &str) -> crate::providers::ProviderProfile {
        crate::providers::ProviderProfile {
            id: id.into(),
            label: format!("Profile {id}"),
            kind: ProviderKind::OpenAi,
            model: format!("m-{id}"),
            base_url: None,
            api_key: format!("k-{id}"),
            max_output_tokens: None,
            context_window: None,
            models: Vec::new(),
        }
    }

    /// A UI that has received the backend's first settings, with two profiles.
    fn synced_state() -> UiState {
        let mut s = UiState::default();
        let settings = Settings { providers: vec![provider("a"), provider("b")], active_provider: Some("a".into()), ..Settings::default() };
        s.apply(AppEvent::Settings(0, settings));
        assert!(!s.settings_dirty);
        s
    }

    #[test]
    fn a_late_settings_echo_keeps_edits_not_sent_yet() {
        let mut s = synced_state();
        // "Use" on b is flushed at once; its SaveSettings waits behind a slow command
        // (an MCP reload).
        s.settings.active_provider = Some("b".into());
        s.settings_dirty = true;
        let sent = s.take_settings_save();
        // Meanwhile b's model is typed: dirty, and not flushed while the field has focus.
        s.settings.providers[1].model = "gpt-4o".into();
        s.settings_dirty = true;
        // The reload ends and the save's echo arrives, with a workspace the backend added on its
        // own in the meantime.
        let mut echo = sent.clone().sanitized();
        echo.workspace_history = vec![PathBuf::from("C:/ws")];
        s.apply(AppEvent::Settings(s.settings_seq, echo));
        assert_eq!(s.settings.providers[1].model, "gpt-4o", "the typed model survives the echo");
        assert_eq!(s.settings.active_provider.as_deref(), Some("b"));
        assert_eq!(s.settings.workspace_history, vec![PathBuf::from("C:/ws")], "the backend's own change still arrives");
        assert!(s.settings_dirty, "the kept edit is sent with the next flush");

        // That flush carries both; once it is echoed nothing is pending.
        let sent = s.take_settings_save();
        assert_eq!(sent.providers[1].model, "gpt-4o");
        assert_eq!(sent.workspace_history, vec![PathBuf::from("C:/ws")]);
        s.apply(AppEvent::Settings(s.settings_seq, sent.clone().sanitized()));
        assert!(!s.settings_dirty);
        assert_eq!(s.settings, sent.sanitized());
    }

    #[test]
    fn an_echo_older_than_the_last_save_keeps_the_typed_edit() {
        // An echo the backend emitted before the save arrives after it, while a field is typed.
        let mut s = synced_state();
        s.settings.active_provider = Some("b".into());
        s.settings_dirty = true;
        let sent = s.take_settings_save();
        s.settings.providers[1].model = "gpt-4o".into();
        s.settings_dirty = true;
        let mut older = s.settings_base.clone();
        older.active_provider = Some("a".into());
        s.apply(AppEvent::Settings(0, older));
        assert_eq!(s.settings.providers[1].model, "gpt-4o", "the unsent edit is kept");
        assert!(s.settings_dirty);
        // The save's own echo follows and puts its selection back.
        s.apply(AppEvent::Settings(1, sent.sanitized()));
        assert_eq!(s.settings.active_provider.as_deref(), Some("b"));
        assert_eq!(s.settings.providers[1].model, "gpt-4o");
        assert!(s.settings_dirty);
    }

    #[test]
    fn a_settings_echo_without_unsent_edits_is_taken_as_is() {
        let mut s = synced_state();
        let mut echo = s.settings.clone();
        echo.model = "gemini-3.5-flash".into();
        echo.active_provider = None;
        s.apply(AppEvent::Settings(0, echo.clone()));
        assert_eq!(s.settings, echo);
        assert!(!s.settings_dirty);

        // An edit undone before the flush is no edit: the echo wins and nothing is pending.
        s.settings.compact_threshold = 0.5;
        s.settings_dirty = true;
        s.settings.compact_threshold = echo.compact_threshold;
        let mut newer = echo.clone();
        newer.preview_access = !newer.preview_access;
        s.apply(AppEvent::Settings(0, newer.clone()));
        assert_eq!(s.settings, newer);
        assert!(!s.settings_dirty);
    }

    #[test]
    fn an_echo_sent_before_the_last_save_does_not_bring_back_a_deleted_profile() {
        let mut s = synced_state();
        // What the backend holds until it handles the save below.
        let stale = s.settings.clone();
        s.settings.providers.retain(|p| p.id != "a");
        s.settings.active_provider = None;
        s.settings_dirty = true;
        let sent = s.take_settings_save();
        // A plugin toggle queued before the save echoes the backend's copy first, while nothing
        // is unsent.
        s.apply(AppEvent::Settings(0, stale));
        assert!(!s.settings.providers.iter().any(|p| p.id == "a"), "the deleted profile is not shown again");
        // The next flush does not carry it (and its key) back either.
        s.settings.theme = crate::settings::Theme::Light;
        s.settings_dirty = true;
        let next = s.take_settings_save();
        assert!(!next.providers.iter().any(|p| p.id == "a"));
        // The first save's echo is older than the second save; the second's is taken as usual.
        s.apply(AppEvent::Settings(1, sent.sanitized()));
        assert!(!s.settings.providers.iter().any(|p| p.id == "a"));
        s.apply(AppEvent::Settings(2, next.sanitized()));
        assert!(!s.settings.providers.iter().any(|p| p.id == "a"));
        assert_eq!(s.settings.theme, crate::settings::Theme::Light);
        assert!(!s.settings_dirty);
    }

    #[test]
    fn a_backend_started_prompt_does_not_leave_the_page_the_user_is_on() {
        // A queued message starts while the user types in a Customizations field.
        let mut s = UiState::default();
        let id = Uuid::new_v4();
        s.current = Some(id);
        s.navigate(View::Customizations);
        s.apply(AppEvent::PromptStarted { conversation: id, text: "queued one".into() });
        assert_eq!(s.view, View::Customizations);
        assert!(s.busy);
        assert_eq!(s.items[0], ChatItem::User("queued one".into()));
    }

    #[test]
    fn a_missing_oauth_client_is_never_an_error() {
        let mut s = synced_state();
        assert!(s.settings.active_provider_profile().is_some());
        s.apply(AppEvent::Auth(AuthStatus::Error("OAuth client is not configured".into())));
        assert_eq!(s.banner, None, "turns go to the provider, which needs no Google sign-in");
        // Changed with the Antigravity-first sign-in: without a provider it is not reported either.
        // The sign-in offered everywhere is Antigravity's, which needs no client secret; only the
        // Gemini CLI sign-in does, and the Account tab says so.
        let mut s = UiState::default();
        s.apply(AppEvent::Auth(AuthStatus::Error("OAuth client is not configured".into())));
        assert_eq!(s.banner, None);
        assert!(s.oauth_not_configured(), "the Account tab still knows the secret is missing");
        // Any other Google error still is, while turns go to Code Assist.
        s.apply(AppEvent::Auth(AuthStatus::Error("access denied".into())));
        assert_eq!(s.banner, Some(Banner::Auth("access denied".into())));
    }

    fn account_entry(id: &str, provider: AccountProvider) -> crate::settings::AccountEntry {
        crate::settings::AccountEntry { id: id.into(), provider, label: format!("{id}@example.com"), model: "gpt-5.1".into() }
    }

    fn view(id: &str, provider: AccountProvider, status: AccountStatus) -> AccountView {
        AccountView {
            id: id.into(),
            provider,
            email: Some(format!("{id}@example.com")),
            plan: None,
            model: "gpt-5.1".into(),
            models: Vec::new(),
            models_fetched_at: None,
            status,
        }
    }

    #[test]
    fn an_account_in_use_is_a_ready_backend() {
        let mut s = UiState::default();
        assert!(!s.any_backend_ready(), "Code Assist needs a Google sign-in");
        s.settings.accounts = vec![account_entry("codex-0123abcd", AccountProvider::Codex)];
        assert!(!s.any_backend_ready(), "listed is not in use");
        s.settings.active_account = Some("codex-0123abcd".into());
        assert!(s.any_backend_ready());
        // A Google error is then no banner: its "Sign in again" would point at the account.
        s.apply(AppEvent::Auth(AuthStatus::Error("access denied".into())));
        assert_eq!(s.banner, None);
        // A turn's auth error names the account, so it is not read as a Google one.
        s.apply(AppEvent::Agent(AgentEvent::Error(CaError::Auth("invalid token".into()))));
        assert_eq!(s.banner, Some(Banner::Auth("ChatGPT (Codex) account: authentication failed: invalid token".into())));
        // A reused Google session that ends stopped nothing either.
        s.banner = None;
        s.apply(AppEvent::ImportedSession(crate::app::ImportedSessionEvent::Ended("gone".into())));
        assert_eq!(s.banner, None);
        assert_eq!(s.import_ended.as_deref(), Some("gone"));
    }

    #[test]
    fn the_sign_in_placeholder_is_running_but_never_an_account() {
        let mut s = UiState::default();
        let signed = view("antigravity-0123abcd", AccountProvider::Antigravity, AccountStatus::SignedIn);
        s.apply(AppEvent::Accounts(vec![signed.clone()]));
        assert_eq!(s.account_signing_in(), None);
        assert_eq!(s.shown_account().map(|v| v.id.as_str()), Some("antigravity-0123abcd"));

        // A sign-in starts: the attempt first, then the rows with the placeholder.
        let info = AccountLoginInfo { id: 7, provider: AccountProvider::Codex, auth_url: "https://auth.example/".into(), error: None };
        s.apply(AppEvent::AccountLogin { login: Some(info.clone()) });
        s.apply(AppEvent::Accounts(vec![signed.clone(), view("", AccountProvider::Codex, AccountStatus::SigningIn)]));
        assert_eq!(s.account_signing_in(), Some(AccountProvider::Codex));
        assert_eq!(s.running_account_login().map(|l| l.id), Some(7), "Cancel goes by the attempt id");
        assert_eq!(s.signed_in_accounts().count(), 1, "the placeholder is not an account");

        // It stopped with an error: kept for a pasted link, no longer running.
        s.account_link_input = "http://localhost:1455/auth/callback?code=c&state=s".into();
        s.apply(AppEvent::AccountLogin { login: Some(AccountLoginInfo { error: Some("port busy".into()), ..info }) });
        s.apply(AppEvent::Accounts(vec![signed]));
        assert_eq!(s.account_signing_in(), None);
        assert!(s.running_account_login().is_none());
        assert_eq!(s.account_errors.get(&AccountProvider::Codex).map(String::as_str), Some("port busy"));
        assert!(!s.account_link_input.is_empty(), "the paste popup keeps its text while the attempt is kept");
        // The attempt is dropped: the error stays on the row, the pasted link goes.
        s.apply(AppEvent::AccountLogin { login: None });
        assert_eq!(s.account_errors.get(&AccountProvider::Codex).map(String::as_str), Some("port busy"));
        assert!(s.account_link_input.is_empty());
    }

    #[test]
    fn injected_phantom_keys_are_dropped_in_order() {
        use egui::{Event, Key, Modifiers};
        let key = |k| Event::Key { key: k, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE };
        let mut ev = vec![key(Key::D), Event::Text(" ".into()), key(Key::C), Event::Text("c".into()), key(Key::Backspace), Event::Text("à".into())];
        sanitize_injected_keys(&mut ev);
        assert_eq!(ev.len(), 5);
        assert!(matches!(&ev[0], Event::Text(t) if t == " "));
        assert!(matches!(&ev[1], Event::Key { key: Key::C, .. }));
        assert!(matches!(&ev[3], Event::Key { key: Key::Backspace, .. }));
    }

    #[test]
    fn streaming_text_appends() {
        let mut s = UiState::default();
        s.input = "hi".into();
        assert_eq!(s.take_prompt().as_deref(), Some("hi"));
        assert!(s.busy);
        assert!(s.take_prompt().is_none());
        s.apply(AppEvent::Agent(AgentEvent::Content("Hel".into())));
        s.apply(AppEvent::Agent(AgentEvent::Content("lo".into())));
        assert_eq!(s.items.last(), Some(&ChatItem::Assistant("Hello".into())));
        s.apply(AppEvent::Agent(AgentEvent::Finished { reason: FinishReason::Cancelled, history: vec![] }));
        assert!(!s.busy);
        assert_eq!(s.items.last(), Some(&ChatItem::Notice("Stopped.".into())));
    }

    #[test]
    fn turn_header_gets_duration_on_finish() {
        let mut s = UiState::default();
        s.input = "go".into();
        s.take_prompt().unwrap();
        assert_eq!(s.items[1], ChatItem::TurnHeader { duration: None });
        assert!(s.turn_started.is_some());
        s.apply(AppEvent::Agent(AgentEvent::Content("ok".into())));
        s.apply(AppEvent::Agent(AgentEvent::Finished { reason: FinishReason::Completed, history: vec![] }));
        assert!(matches!(s.items[1], ChatItem::TurnHeader { duration: Some(_) }));
        assert!(s.turn_started.is_none());
    }

    #[test]
    fn rejected_turn_removes_header() {
        let mut s = UiState::default();
        s.input = "go".into();
        s.take_prompt().unwrap();
        s.apply(AppEvent::TurnRejected("not logged in".into()));
        assert_eq!(s.items, vec![ChatItem::User("go".into())]);
        assert!(s.running.is_none() && s.turn_started.is_none());
    }

    #[test]
    fn first_prompt_survives_backend_created_conversation() {
        let mut s = UiState::default();
        s.input = "hello".into();
        s.take_prompt().unwrap();
        let conv = crate::store::Conversation::new(None, "auto");
        let id = conv.id;
        s.apply(AppEvent::ConversationLoaded(conv));
        assert_eq!(s.items.len(), 2);
        assert_eq!(s.current, Some(id));
        assert_eq!(s.running, Some(id));
    }

    #[test]
    fn running_marker_follows_turn_events() {
        let mut s = UiState::default();
        let a = Uuid::new_v4();
        s.current = Some(a);
        s.busy = true;
        s.apply(AppEvent::TurnEvent { conversation: a, event: AgentEvent::Content("x".into()) });
        assert_eq!(s.running, Some(a));
        s.apply(AppEvent::TurnEvent { conversation: a, event: AgentEvent::Finished { reason: FinishReason::Completed, history: vec![] } });
        assert_eq!(s.running, None);
    }

    #[test]
    fn review_answer_reaches_agent_and_finish_dismisses() {
        let mut s = UiState::default();
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        s.apply(AppEvent::Agent(AgentEvent::ReviewRequested(ReviewRequest {
            call_id: "r1".into(),
            artifacts: vec!["implementation_plan".into()],
            message: "Plan ready".into(),
            respond: tx,
        })));
        assert!(!s.answer_review("other", ReviewOutcome::Approved), "stale id is ignored");
        assert!(s.answer_review("r1", ReviewOutcome::Approved));
        assert_eq!(rx.try_recv().unwrap(), ReviewOutcome::Approved);

        let (tx, mut rx) = tokio::sync::oneshot::channel();
        s.apply(AppEvent::Agent(AgentEvent::ReviewRequested(ReviewRequest { call_id: "r2".into(), artifacts: vec![], message: "?".into(), respond: tx })));
        s.apply(AppEvent::Agent(AgentEvent::Finished { reason: FinishReason::Cancelled, history: vec![] }));
        assert!(s.pending_review.is_none());
        assert!(rx.try_recv().is_err(), "dropped sender means dismissed");
    }

    #[test]
    fn backend_started_prompt_shows_bubble() {
        let mut s = UiState::default();
        let id = Uuid::new_v4();
        s.current = Some(id);
        s.input = "draft".into();
        s.apply(AppEvent::PromptStarted { conversation: id, text: "queued one".into() });
        assert!(s.busy);
        assert_eq!(s.items[0], ChatItem::User("queued one".into()));
        assert_eq!(s.input, "draft", "the composer draft is kept");
    }

    #[test]
    fn usage_is_kept() {
        let mut s = UiState::default();
        let u = UsageMetadata { total_token_count: 42, ..Default::default() };
        s.apply(AppEvent::Agent(AgentEvent::Usage(u)));
        assert_eq!(s.last_usage.unwrap().total_token_count, 42);
    }

    #[test]
    fn navigation_stack() {
        let mut s = UiState::default();
        s.navigate(View::History);
        s.navigate(View::Customizations);
        s.go_back();
        assert_eq!(s.view, View::History);
        s.go_forward();
        assert_eq!(s.view, View::Customizations);
        s.go_back();
        s.navigate(View::Chat);
        assert!(s.forward.is_empty());
        assert_eq!(s.back, vec![View::Chat, View::History]);
    }

    #[test]
    fn conversation_list_updates_current_title() {
        let mut s = UiState::default();
        let id = Uuid::new_v4();
        s.current = Some(id);
        s.apply(AppEvent::Conversations(vec![ConversationSummary {
            id,
            title: "Fix build".into(),
            workspace: None,
            model: "auto".into(),
            updated_at: chrono::Utc::now(),
        }]));
        assert_eq!(s.current_title, "Fix build");
    }

    #[test]
    fn home_is_derived_from_empty_chat() {
        let mut s = UiState::default();
        assert!(s.is_home());
        s.items.push(ChatItem::User("x".into()));
        assert!(!s.is_home());
        s.new_chat();
        assert!(s.is_home());
        s.view = View::History;
        assert!(!s.is_home());
    }

    #[test]
    fn tool_card_lifecycle_and_quota_banner() {
        let mut s = UiState::default();
        s.apply(AppEvent::Agent(AgentEvent::ToolCallRequest { call_id: "c1".into(), name: "read_file".into(), args: serde_json::json!({}) }));
        s.apply(AppEvent::Agent(AgentEvent::ToolCallResponse { call_id: "c1".into(), name: "read_file".into(), ok: true, display: "3 lines".into() }));
        assert!(matches!(&s.items[0], ChatItem::Tool(t) if t.result == Some((true, "3 lines".into()))));
        s.apply(AppEvent::Agent(AgentEvent::Error(CaError::TerminalQuota { reason: "daily".into() })));
        assert!(matches!(s.banner, Some(Banner::Quota(_))));
    }

    #[test]
    fn confirmation_answer_reaches_agent() {
        let mut s = UiState::default();
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        s.apply(AppEvent::Agent(AgentEvent::Confirmation(ConfirmationRequest {
            call_id: "c".into(),
            tool_name: "run_shell_command".into(),
            details: ConfirmationDetails::Shell { command: "ls".into(), dir: ".".into(), root_command: "ls".into(), description: None, simple: true },
            respond: tx,
        })));
        s.answer_confirm(ConfirmOutcome::ProceedAlways);
        assert_eq!(rx.try_recv().unwrap(), ConfirmOutcome::ProceedAlways);
        assert!(s.pending_confirm.is_none());
    }

    fn shell_confirm(id: &str) -> (ConfirmationRequest, tokio::sync::oneshot::Receiver<ConfirmOutcome>) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let req = ConfirmationRequest {
            call_id: id.into(),
            tool_name: "run_shell_command".into(),
            details: ConfirmationDetails::Shell { command: "ls".into(), dir: ".".into(), root_command: "ls".into(), description: None, simple: true },
            respond: tx,
        };
        (req, rx)
    }

    #[test]
    fn replaced_confirmation_is_cancelled() {
        let mut s = UiState::default();
        let (a, mut rx_a) = shell_confirm("a");
        let (b, _rx_b) = shell_confirm("b");
        s.apply(AppEvent::Agent(AgentEvent::Confirmation(a)));
        s.apply(AppEvent::Agent(AgentEvent::Confirmation(b)));
        assert_eq!(rx_a.try_recv().unwrap(), ConfirmOutcome::Cancel);
        assert_eq!(s.pending_confirm.as_ref().unwrap().request.call_id, "b");
    }

    #[test]
    fn early_backend_error_clears_busy() {
        let mut s = UiState::default();
        s.input = "do it".into();
        s.take_prompt().unwrap();
        s.apply(AppEvent::TurnRejected("choose a workspace folder first".into()));
        assert!(!s.busy);
        assert!(matches!(s.banner, Some(Banner::Error(_))));

        // Once the turn has started, a non-agent error does not end it.
        s.input = "again".into();
        s.take_prompt().unwrap();
        s.apply(AppEvent::Agent(AgentEvent::Content("ok".into())));
        s.apply(AppEvent::Error("could not save conversation".into()));
        assert!(s.busy);

        // An unrelated error between take_prompt and the first event keeps the turn running.
        s.apply(AppEvent::Agent(AgentEvent::Finished { reason: FinishReason::Completed, history: vec![] }));
        s.input = "third".into();
        s.take_prompt().unwrap();
        s.apply(AppEvent::Error("could not refresh list".into()));
        assert!(s.busy);
    }

    #[test]
    fn events_of_other_conversations_are_dropped() {
        let mut s = UiState::default();
        let shown = Uuid::new_v4();
        let other = Uuid::new_v4();
        s.current = Some(shown);
        s.busy = true;
        s.apply(AppEvent::TurnEvent { conversation: other, event: AgentEvent::Content("stray".into()) });
        assert!(s.items.is_empty());
        assert!(s.busy);
        s.apply(AppEvent::TurnEvent { conversation: other, event: AgentEvent::Finished { reason: FinishReason::Cancelled, history: vec![] } });
        assert!(!s.busy);
        assert!(s.items.is_empty());
        s.busy = true;
        s.apply(AppEvent::TurnEvent { conversation: shown, event: AgentEvent::Content("ok".into()) });
        assert_eq!(s.items.last(), Some(&ChatItem::Assistant("ok".into())));
    }

    #[test]
    fn whitespace_prompt_is_ignored() {
        let mut s = UiState::default();
        s.input = "  \n ".into();
        assert!(s.take_prompt().is_none());
        assert!(!s.busy);
        assert!(s.items.is_empty());
    }

    #[test]
    fn thoughts_merge_by_subject() {
        let mut s = UiState::default();
        let th = |subj: Option<&str>, t: &str| AppEvent::Agent(AgentEvent::Thought { subject: subj.map(Into::into), text: t.into() });
        s.apply(th(Some("Plan"), "a"));
        s.apply(th(None, "b"));
        s.apply(th(Some("Other"), "c"));
        assert_eq!(s.items.len(), 2);
        assert_eq!(s.items[0], ChatItem::Thought { subject: Some("Plan".into()), text: "ab".into() });
    }

    #[test]
    fn auth_errors_set_auth_banner() {
        let mut s = UiState::default();
        s.apply(AppEvent::Auth(AuthStatus::Error("denied".into())));
        assert!(matches!(s.banner, Some(Banner::Auth(_))));
        s.banner = None;
        s.apply(AppEvent::Agent(AgentEvent::Error(CaError::Auth("expired".into()))));
        assert!(matches!(s.banner, Some(Banner::Auth(_))));
    }

    #[test]
    fn history_becomes_items_with_tool_results() {
        let v: Vec<Content> = serde_json::from_value(serde_json::json!([
            {"role": "user", "parts": [{"text": "list files"}]},
            {"role": "model", "parts": [{"functionCall": {"id": "t1", "name": "list_directory", "args": {"path": "."}}}]},
            {"role": "user", "parts": [{"functionResponse": {"id": "t1", "name": "list_directory", "response": {"output": "a"}}}]},
            {"role": "model", "parts": [{"text": "Here they are."}]}
        ]))
        .unwrap();
        let items = items_from_history(&v);
        assert_eq!(items.len(), 3);
        assert_eq!(items[0], ChatItem::User("list files".into()));
        assert!(matches!(&items[1], ChatItem::Tool(t) if t.name == "list_directory" && t.result == Some((true, "done".into()))));
        assert_eq!(items[2], ChatItem::Assistant("Here they are.".into()));
    }

    /// Rewind checkpoints are keyed by `user_bubble_count` and the transcript numbers its user
    /// bubbles from `items_from_history`: the two must agree, and tool results never count.
    #[test]
    fn user_bubble_count_matches_the_transcript() {
        let v: Vec<Content> = serde_json::from_value(serde_json::json!([
            {"role": "user", "parts": [{"text": "list files"}]},
            {"role": "model", "parts": [{"functionCall": {"id": "t1", "name": "list_directory", "args": {"path": "."}}}]},
            {"role": "user", "parts": [{"functionResponse": {"id": "t1", "name": "list_directory", "response": {"output": "a"}}}]},
            {"role": "model", "parts": [{"functionCall": {"id": "t2", "name": "read_file", "args": {"file_path": "a"}}}]},
            {"role": "user", "parts": [{"functionResponse": {"id": "t2", "name": "read_file", "response": {"output": "x"}}}, {"text": "also check b"}]},
            {"role": "model", "parts": [{"text": "Done."}]},
            {"role": "user", "parts": [{"text": "thanks"}]}
        ]))
        .unwrap();
        let bubbles = |h: &[Content]| items_from_history(h).iter().filter(|i| matches!(i, ChatItem::User(_))).count();
        // Two prompts and one steering message; the two function-response contents add none.
        assert_eq!(user_bubble_count(&v), 3);
        for n in 0..=v.len() {
            assert_eq!(user_bubble_count(&v[..n]), bubbles(&v[..n]), "prefix {n}");
        }
    }

    #[test]
    fn new_chat_resets_pane() {
        let mut s = UiState::default();
        s.current = Some(Uuid::new_v4());
        s.items.push(ChatItem::Notice("x".into()));
        s.new_chat();
        assert!(s.current.is_none() && s.items.is_empty());
    }

    #[test]
    fn enter_sends_only_without_shift() {
        assert!(composer::is_send_key(true, false));
        assert!(!composer::is_send_key(true, true));
        assert!(!composer::is_send_key(false, false));
    }

    #[test]
    fn not_configured_detection() {
        let mut s = UiState::default();
        s.apply(AppEvent::Auth(AuthStatus::Error("OAuth client is not configured".into())));
        assert!(s.oauth_not_configured());
        s.apply(AppEvent::Auth(AuthStatus::LoggedOut));
        assert!(!s.oauth_not_configured());
    }
}

/// A Google session reused from another tool, as the UI state sees it.
#[cfg(test)]
mod imported_session_tests {
    use super::*;
    use crate::app::ImportedSessionEvent;
    use crate::auth::antigravity::{Issuer, SessionInfo};

    fn info() -> SessionInfo {
        SessionInfo {
            source: PathBuf::from(r"C:\Users\me\.gemini\oauth_creds.json"),
            issuer: Issuer::Antigravity,
            account: Some("me@example.com".into()),
            expiry: None,
            can_refresh: false,
        }
    }

    #[test]
    fn a_reused_session_counts_as_signed_in_until_it_ends() {
        let mut s = UiState::default();
        s.apply(AppEvent::OAuthClientSource(None));
        s.apply(AppEvent::Auth(AuthStatus::LoggedIn { email: Some("me@example.com".into()) }));
        s.apply(AppEvent::ImportedSession(ImportedSessionEvent::Active(info())));
        assert_eq!(s.imported_session, Some(info()));
        assert!(!s.own_sign_in(), "the Account tab still offers AntiCraft's own sign-in");
        assert!(s.oauth_not_configured(), "a reused session does not hide the missing client secret");

        // The backend reports the signed-in state first, then why the session ended.
        s.apply(AppEvent::Auth(AuthStatus::Error("OAuth client is not configured".into())));
        s.apply(AppEvent::ImportedSession(ImportedSessionEvent::Ended("The Antigravity session expired.".into())));
        assert!(s.imported_session.is_none());
        assert_eq!(s.import_ended.as_deref(), Some("The Antigravity session expired."));
        assert_eq!(s.banner, Some(Banner::Auth("The Antigravity session expired.".into())));

        // Using a session again clears the old reason; stopping it signs out.
        s.apply(AppEvent::ImportedSession(ImportedSessionEvent::Active(info())));
        assert_eq!(s.import_ended, None);
        s.apply(AppEvent::ImportedSession(ImportedSessionEvent::Stopped));
        assert!(s.imported_session.is_none());
    }

    #[test]
    fn with_a_provider_in_use_only_the_account_tab_says_why_a_session_ended() {
        let mut s = UiState::default();
        let mut settings = Settings::default();
        // Built from JSON so this test does not depend on the profile's field list.
        let profile = serde_json::json!({"id": "p", "label": "P", "kind": "openai", "model": "m", "apiKey": "k"});
        settings.providers.push(serde_json::from_value(profile).unwrap());
        settings.active_provider = Some("p".into());
        s.settings = settings;
        s.apply(AppEvent::ImportedSession(ImportedSessionEvent::Ended("gone".into())));
        assert_eq!(s.banner, None, "turns go to the provider, so nothing stopped working");
        assert_eq!(s.import_ended.as_deref(), Some("gone"));
    }
}
