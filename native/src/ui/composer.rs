//! Composer card shared by Home and the docked conversation input (DESIGN.md 2.4,
//! `_ref/ui/01-home.png`; ORCHESTRATION.md D): multiline input, `+` menu, model chip with the
//! model picker (2.6), the Planning/Fast mode chip, `@` mention and `/` command popups,
//! queued-message chips (Queue / Steer), edit-last-message, send / stop, and the "Local" tray.
//!
//! Composer-only state ([`ComposerState`]) lives in egui temp memory under one fixed id, so
//! `UiState` and the screen signatures stay unchanged. Backend events for it are routed with
//! [`on_queue`], [`set_workspace_index`] and [`set_workflows`]. Nothing here blocks: the file
//! index and the workflow list are built on worker threads and polled with `try_lock`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use egui::text::{CCursor, CCursorRange};
use egui::{Align2, Color32, CornerRadius, Event, Key, Rect, Sense, Stroke, Ui, Vec2};
use uuid::Uuid;

use super::mentions::{self, IndexJob, MentionPopup, INDEX_STALE_AFTER};
use super::queue::{self, ChipAction, LocalQueue, QueueMode};
use super::slash::{self, Builtin, SlashItem};
use super::{format, icons, theme, widgets, ChatItem, UiAction, UiState};
use crate::agent::context::Workflow;
use crate::agent::policy::AgentMode;
use crate::app::AppCommand;

/// Card radius measured on the reference (the bottom tray follows it).
const RADIUS: u8 = 12;
/// Height of the bottom "Local" tray.
const TRAY_H: f32 = 31.0;
/// Tray fill (`#252525`, measured on the reference).
const TRAY_FILL: Color32 = Color32::from_rgb(0x25, 0x25, 0x25);
const PLACEHOLDER_TEXT: &str = "Ask anything, @ to mention, / for workflows";
const PLACEHOLDER_BUSY: &str = "Type to queue a follow-up (Alt+Enter steers the running turn)";
const PLACEHOLDER_REVIEW: &str = "Reply to the agent, or use the review buttons";
/// Review placeholder while the artifacts panel (which then holds the buttons) is open.
const PLACEHOLDER_REVIEW_PANEL: &str = "Reply to the agent, or review in the panel";
/// Popup row height and width.
const POPUP_ROW_H: f32 = 28.0;
const POPUP_W: f32 = 380.0;

/// Plain Enter (no Shift) sends; Shift+Enter inserts a newline.
pub fn is_send_key(enter_pressed: bool, shift: bool) -> bool {
    enter_pressed && !shift
}

// ---------------------------------------------------------------------------------------------
// Composer state (egui temp memory)

/// Finished-once background result slot.
#[derive(Debug, Clone, Default)]
struct Slot<T>(Arc<Mutex<Option<T>>>);

impl<T> Slot<T> {
    fn take(&self) -> Option<T> {
        self.0.try_lock().ok().and_then(|mut g| g.take())
    }
}

/// Tracks an IME composition from the raw event stream. While a preedit is active, Enter
/// belongs to the IME (it commits the composition) and must not send the prompt.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ImeTracker {
    pub composing: bool,
}

impl ImeTracker {
    pub fn observe(&mut self, events: &[Event]) {
        for e in events {
            if let Event::Ime(ime) = e {
                match ime {
                    egui::ImeEvent::Preedit { text, .. } => self.composing = !text.is_empty(),
                    egui::ImeEvent::Commit(_) => self.composing = false,
                    _ => {}
                }
            }
        }
    }
}

/// Composer-only state shared by the Home and docked composers.
#[derive(Debug, Clone, Default)]
pub struct ComposerState {
    pub mentions: MentionPopup,
    index_job: Option<IndexJob>,
    index_root: Option<PathBuf>,
    index_built: Option<Instant>,
    /// The backend supplied the index; local walks stop.
    index_from_backend: bool,
    pub workflows: Vec<Workflow>,
    workflows_job: Option<Slot<Vec<Workflow>>>,
    workflows_loaded: Option<(Option<PathBuf>, Instant)>,
    workflows_from_backend: bool,
    pub slash_selected: usize,
    slash_dismissed: bool,
    pub queue: LocalQueue,
    queue_conv: Option<Uuid>,
    /// Mode chip per conversation (`None`: the unsaved new chat).
    modes: HashMap<Option<Uuid>, AgentMode>,
    last_conv: Option<Uuid>,
    /// Original text of the last user message while it is being edited.
    pub editing: Option<String>,
    was_busy: bool,
    pub ime: ImeTracker,
    /// One-line hint under the input (cleared when the input changes).
    hint: Option<(String, String)>,
    /// One pending autofocus request: set when the composer is created or the conversation
    /// changes, cleared on the first frame that can act on it. Asking again every frame used
    /// to pull the caret back after a mouse click elsewhere or a Tab focus change.
    focus_request: bool,
}

fn state_id() -> egui::Id {
    egui::Id::unique("anticraft_composer_state")
}

/// Lend the composer state to `f`, then put it back.
///
/// The state is moved out of egui temp memory for the call rather than borrowed in place:
/// `ctx.data_mut` holds the context's write lock, and `f` renders widgets that take that lock
/// again (`artifacts_panel::load`, `ctx.input`, `ui.memory`). egui's lock is not reentrant, so
/// borrowing in place deadlocked the first frame that drew the composer. Moving costs nothing
/// (no clone); only a nested `with_state` inside `f` would see a fresh state, and none exists.
fn with_state<R>(ctx: &egui::Context, f: impl FnOnce(&mut ComposerState) -> R) -> R {
    // A fresh state arms the one autofocus request (the app just opened).
    let mut cs = ctx
        .data_mut(|d| d.remove_temp::<ComposerState>(state_id()))
        .unwrap_or_else(|| ComposerState { focus_request: true, ..ComposerState::default() });
    let out = f(&mut cs);
    ctx.data_mut(|d| d.insert_temp(state_id(), cs));
    out
}

/// The state lives in egui temp memory between frames: callers lend it out (`with_state`)
/// instead of cloning it in and writing it back every frame.
fn load(ctx: &egui::Context) -> ComposerState {
    ctx.data(|d| d.get_temp::<ComposerState>(state_id())).unwrap_or_default()
}

/// Route `AppEvent::Queue`: the backend now owns the queue; the chips mirror it.
#[allow(dead_code)] // integration hook, see INTERFACES.md (ui-composer)
pub fn on_queue(ctx: &egui::Context, q: Vec<(u64, String)>) {
    with_state(ctx, |cs| cs.queue.mirror(q));
}

/// Route `AppEvent::WorkspaceIndex`: replaces the local index (local walks stop).
#[allow(dead_code)] // integration hook, see INTERFACES.md (ui-composer)
pub fn set_workspace_index(ctx: &egui::Context, index: Vec<String>) {
    with_state(ctx, |cs| {
        cs.mentions.set_index(index);
        cs.index_job = None;
        cs.index_built = Some(Instant::now());
        cs.index_from_backend = true;
    });
}

/// Route `AppEvent::Workflows`: replaces the locally loaded workflow list.
#[allow(dead_code)] // integration hook, see INTERFACES.md (ui-composer)
pub fn set_workflows(ctx: &egui::Context, w: Vec<Workflow>) {
    with_state(ctx, |cs| {
        cs.workflows = w;
        cs.workflows_job = None;
        cs.workflows_from_backend = true;
    });
}

/// Put the last user message into the composer for editing (pencil on the last bubble).
/// No-op while busy or when there is no user message.
#[allow(dead_code)] // integration hook, see INTERFACES.md (ui-composer)
pub fn begin_edit_last(ctx: &egui::Context, state: &mut UiState) {
    with_state(ctx, |cs| {
        start_edit(cs, state);
    });
}

/// Mode chip value for the open conversation.
#[allow(dead_code)] // integration hook, see INTERFACES.md (ui-composer)
pub fn current_mode(ctx: &egui::Context, state: &UiState) -> AgentMode {
    load(ctx).modes.get(&state.current).copied().unwrap_or(state.settings.agent_mode)
}

fn start_edit(cs: &mut ComposerState, state: &mut UiState) -> bool {
    if state.busy || cs.editing.is_some() {
        return false;
    }
    let Some(last) = last_user_text(&state.items) else { return false };
    let last = last.to_owned();
    state.input = last.clone();
    cs.editing = Some(last);
    true
}

fn last_user_text(items: &[ChatItem]) -> Option<&str> {
    items.iter().rev().find_map(|i| if let ChatItem::User(t) = i { Some(t.as_str()) } else { None })
}

// ---------------------------------------------------------------------------------------------
// Pure helpers (unit tested)

/// Navigation keys the open popup takes from the event stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavKey {
    Up,
    Down,
    Accept,
    Dismiss,
}

/// Whether the open popup has a row that Accept (Enter / Tab) can pick. With no selectable
/// row the keys must stay with the composer: Enter sends (or inserts a newline with Shift)
/// and Tab leaves the field, instead of being swallowed by an empty list.
pub fn accept_available(rows: usize) -> bool {
    rows > 0
}

/// Remove the popup's navigation key presses (Up, Down, Enter / Tab without Shift, Escape)
/// and return them in order. Every other event, Backspace and `Text` included, keeps its
/// relative order, so Telex rewrites (Backspace x N, then the toned text) are never reordered.
/// Releases of the taken keys are removed too so no widget sees a stray release.
#[allow(dead_code)] // thin wrapper kept for callers that always take Accept
pub fn take_nav_keys(events: &mut Vec<Event>) -> Vec<NavKey> {
    take_nav_keys_with(events, true)
}

/// [`take_nav_keys`], where `accept` false leaves Enter and Tab in the stream for the
/// TextEdit (only Up, Down and Escape are taken).
#[allow(dead_code)] // the empty-list behaviour is unit tested through this entry point
pub fn take_nav_keys_with(events: &mut Vec<Event>, accept: bool) -> Vec<NavKey> {
    let mut out = Vec::new();
    events.retain(|e| {
        let Event::Key { key, pressed, modifiers, .. } = e else { return true };
        if modifiers.ctrl || modifiers.alt || modifiers.command {
            return true;
        }
        let nav = match key {
            Key::ArrowUp => NavKey::Up,
            Key::ArrowDown => NavKey::Down,
            Key::Enter | Key::Tab if !modifiers.shift && accept => NavKey::Accept,
            Key::Escape => NavKey::Dismiss,
            _ => return true,
        };
        if *pressed {
            out.push(nav);
        }
        false
    });
    out
}

/// Number of rows the open popup can select now (`None` popup: zero).
fn popup_rows(cs: &mut ComposerState, popup: &Popup) -> usize {
    match popup {
        Popup::None => 0,
        Popup::Mention { query, .. } => cs.mentions.rows(query).len(),
        Popup::Slash { query } => slash::items(query, &cs.workflows).len(),
    }
}

/// Byte offset of char index `ci` in `s` (clamped to the end).
pub fn char_to_byte(s: &str, ci: usize) -> usize {
    s.char_indices().nth(ci).map(|(b, _)| b).unwrap_or(s.len())
}

/// Reference model of how the text editor applies key/text events (insert at the caret,
/// Backspace deletes the char before it). Used by tests to check that event sequences from
/// Vietnamese input tools produce the intended text once the composer has taken its keys.
#[cfg(test)]
pub fn apply_edit_events(text: &mut String, caret: &mut usize, events: &[Event]) {
    for e in events {
        match e {
            Event::Text(t) if t != "\n" && t != "\r" => {
                let b = char_to_byte(text, *caret);
                text.insert_str(b, t);
                *caret += t.chars().count();
            }
            Event::Key { key: Key::Backspace, pressed: true, .. } if *caret > 0 => {
                let b0 = char_to_byte(text, *caret - 1);
                let b1 = char_to_byte(text, *caret);
                text.replace_range(b0..b1, "");
                *caret -= 1;
            }
            _ => {}
        }
    }
}

/// What submitting the composer text should do.
#[derive(Debug, Clone, PartialEq)]
pub enum Submit {
    /// Normal prompt (`bubble` shown, `prompt` sent).
    Prompt { bubble: String, prompt: String, mode: Option<AgentMode> },
    /// Built-in that does not send a prompt.
    Local(Builtin),
    /// `/fast` with no text: switch the chip.
    SetMode(AgentMode),
}

/// Classify submitted text: built-ins, workflows (`/name rest`), or a plain prompt.
pub fn classify(text: &str, workflows: &[Workflow]) -> Submit {
    let plain = || Submit::Prompt { bubble: text.to_owned(), prompt: text.to_owned(), mode: None };
    let Some(inv) = slash::parse(text) else { return plain() };
    if let Some(b) = slash::builtin(inv.name) {
        return match b {
            Builtin::Plan => {
                let prompt = if inv.rest.is_empty() { slash::BARE_PLAN_PROMPT.to_owned() } else { inv.rest.to_owned() };
                Submit::Prompt { bubble: text.to_owned(), prompt, mode: Some(AgentMode::Planning) }
            }
            Builtin::Fast if inv.rest.is_empty() => Submit::SetMode(AgentMode::Fast),
            Builtin::Fast => Submit::Prompt { bubble: text.to_owned(), prompt: inv.rest.to_owned(), mode: Some(AgentMode::Fast) },
            other => Submit::Local(other),
        };
    }
    match workflows.iter().find(|w| w.name == inv.name) {
        Some(w) => Submit::Prompt { bubble: text.to_owned(), prompt: slash::workflow_prompt(w, inv.rest), mode: None },
        None => plain(),
    }
}

/// The one-shot mode an edited prompt was sent with: `/plan <text>` and `/fast <text>` force a
/// mode for that prompt only, so editing must send it again with the same mode. The composer
/// shows the original text (`Submit::Prompt.bubble`), so the command is still in it.
pub fn edit_mode(original: &str) -> Option<AgentMode> {
    match classify(original, &[]) {
        Submit::Prompt { mode, .. } => mode,
        _ => None,
    }
}

/// Whether the previous turn ended normally (no stop/rejection notice, no error banner).
fn ended_normally(state: &UiState) -> bool {
    state.banner.is_none() && !matches!(state.items.last(), Some(ChatItem::Notice(_)))
}

// ---------------------------------------------------------------------------------------------
// Rendering

/// Docked conversation composer (`_ref/ui/07-conversation.png`): one `#1c1c1c` box (1px
/// `#252525`, radius 8, 12px margin) with the input and the controls row inside it.
///
/// The input starts at one line and grows with its text, as Home's does. A fixed tall input
/// made the composer triple in height the moment the first prompt opened the conversation.
pub fn show_docked(ui: &mut Ui, state: &mut UiState, commands: &mut Vec<UiAction>) {
    egui::Frame::new()
        .fill(theme::CARD)
        .stroke(Stroke::new(1.0, theme::CARD_BORDER))
        .corner_radius(8)
        .inner_margin(12)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing = Vec2::ZERO;
            if state.pending_confirm.is_some() {
                super::confirm::tray_actions(ui, state);
                ui.add_space(6.0);
                let r = ui.available_rect_before_wrap();
                ui.painter().hline(r.x_range(), r.top(), Stroke::new(1.0, theme::CARD_BORDER));
                ui.add_space(8.0);
            }
            input_impl(ui, state, commands, true, true);
        });
}

/// Tallest the input grows with its text before it scrolls.
const INPUT_MAX_H: f32 = 200.0;

pub fn show(ui: &mut Ui, state: &mut UiState, commands: &mut Vec<UiAction>, autofocus: bool) {
    egui::Frame::new()
        .fill(theme::CARD)
        .stroke(Stroke::new(1.0, theme::CARD_BORDER))
        .corner_radius(RADIUS)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing = Vec2::ZERO;
            egui::Frame::new().inner_margin(egui::Margin { left: 12, right: 8, top: 12, bottom: 4 }).show(ui, |ui| {
                ui.set_width(ui.available_width());
                input_impl(ui, state, commands, autofocus, false);
            });
            tray(ui, state);
        });
}

/// Which popup is open this frame.
#[derive(Debug, Clone, PartialEq)]
enum Popup {
    None,
    /// `@query` at byte `at`, ending at `end`.
    Mention { at: usize, end: usize, query: String },
    Slash { query: String },
}

fn active_popup(cs: &ComposerState, input: &str, caret: usize, has_review_or_confirm: bool) -> Popup {
    if has_review_or_confirm {
        return Popup::None;
    }
    if let Some(q) = slash::active_query(input, caret) {
        if !cs.slash_dismissed {
            return Popup::Slash { query: q.to_owned() };
        }
        return Popup::None;
    }
    if let Some((at, q)) = mentions::active_query(input, caret) {
        if cs.mentions.dismissed != Some(at) {
            return Popup::Mention { at, end: caret, query: q.to_owned() };
        }
    }
    Popup::None
}

fn caret_byte(ctx: &egui::Context, id: egui::Id, text: &str) -> usize {
    egui::text_edit::TextEditState::load(ctx, id)
        .and_then(|s| s.cursor.char_range())
        .map(|r| char_to_byte(text, r.primary.index.0))
        .unwrap_or(text.len())
}

fn set_caret(ctx: &egui::Context, id: egui::Id, text: &str, byte: usize) {
    let mut s = egui::text_edit::TextEditState::load(ctx, id).unwrap_or_default();
    let ci = text.get(..byte).map(|h| h.chars().count()).unwrap_or_else(|| text.chars().count());
    s.cursor.set_char_range(Some(CCursorRange::one(CCursor::new(ci))));
    s.store(ctx, id);
}

/// Keep the mention index fresh (background walk) and poll finished jobs.
fn maintain_index(ctx: &egui::Context, cs: &mut ComposerState, workspace: Option<&PathBuf>, wanted: bool) {
    if let Some(job) = &cs.index_job {
        if let Some(idx) = job.poll() {
            let root = job.root.clone();
            cs.index_job = None;
            if workspace == Some(&root) && !cs.index_from_backend {
                cs.mentions.set_index(idx);
                cs.index_root = Some(root);
                cs.index_built = Some(Instant::now());
            }
        }
    }
    if cs.index_root.as_ref() != workspace && !cs.index_from_backend {
        cs.mentions.set_index(Vec::new());
        cs.index_root = None;
        cs.index_built = None;
    }
    let stale = cs.index_built.is_none_or(|t| t.elapsed() > INDEX_STALE_AFTER);
    if wanted && stale && cs.index_job.is_none() && !cs.index_from_backend {
        if let Some(ws) = workspace {
            let c = ctx.clone();
            cs.index_job = Some(IndexJob::spawn(ws.clone(), move || c.request_repaint()));
        }
    }
}

/// Load workflows in the background when the `/` popup opens (unless the backend sends them).
fn maintain_workflows(ctx: &egui::Context, cs: &mut ComposerState, workspace: Option<&PathBuf>, wanted: bool) {
    if let Some(slot) = &cs.workflows_job {
        if let Some(w) = slot.take() {
            cs.workflows_job = None;
            if !cs.workflows_from_backend {
                cs.workflows = w;
            }
        }
    }
    if !wanted || cs.workflows_from_backend || cs.workflows_job.is_some() {
        return;
    }
    let fresh = cs.workflows_loaded.as_ref().is_some_and(|(ws, t)| ws.as_ref() == workspace && t.elapsed() < INDEX_STALE_AFTER);
    if fresh {
        return;
    }
    cs.workflows_loaded = Some((workspace.cloned(), Instant::now()));
    let slot: Slot<Vec<Workflow>> = Slot::default();
    let out = slot.0.clone();
    let ws = workspace.cloned();
    let c = ctx.clone();
    let ok = std::thread::Builder::new()
        .name("workflow-load".into())
        .spawn(move || {
            let w = crate::agent::context::load_workflows(ws.as_deref(), &crate::settings::default_data_dir());
            if let Ok(mut g) = out.lock() {
                *g = Some(w);
            }
            c.request_repaint();
        })
        .is_ok();
    if ok {
        cs.workflows_job = Some(slot);
    }
}

/// Input plus controls row. `docked` also puts the "Local" chip in the row (Home has it in the
/// tray under the card).
fn input_impl(ui: &mut Ui, state: &mut UiState, commands: &mut Vec<UiAction>, autofocus: bool, docked: bool) {
    let ctx = ui.ctx().clone();
    with_state(&ctx, |cs| input_body(ui, cs, state, commands, autofocus, docked));
}

/// [`input_impl`] with the composer state already borrowed from egui temp memory.
fn input_body(ui: &mut Ui, cs: &mut ComposerState, state: &mut UiState, commands: &mut Vec<UiAction>, autofocus: bool, docked: bool) {
    let ctx = ui.ctx().clone();
    let id = ui.make_persistent_id("prompt_input");
    let (review, review_in_panel) = {
        let p = super::artifacts_panel::load(&ctx, state.current);
        (p.pending_review.is_some(), p.open)
    };

    // Conversation bookkeeping: the new chat's mode follows it once the backend assigns an
    // id; a local queue belongs to one conversation.
    if cs.last_conv != state.current {
        if cs.last_conv.is_none() {
            if let Some(m) = cs.modes.remove(&None) {
                cs.modes.insert(state.current, m);
            }
        }
        if !cs.queue.backend_owned {
            if cs.queue_conv.is_none() {
                cs.queue_conv = state.current;
            } else if cs.queue_conv != state.current {
                cs.queue.items.clear();
                cs.queue_conv = state.current;
            }
        }
        if cs.editing.is_some() && state.current.is_some() {
            cs.editing = None;
        }
        if cs.last_conv != state.current {
            // New or switched conversation: the composer may take focus once more.
            cs.focus_request = true;
            cs.last_conv = state.current;
        }
    }

    // Local queue: run the next entry after a normal finish.
    if cs.was_busy && !state.busy {
        if let Some(next) = cs.queue.next_after_turn(ended_normally(state)) {
            let draft = std::mem::replace(&mut state.input, next.text.clone());
            if state.take_prompt().is_some() {
                commands.push(AppCommand::SendPrompt(next.text));
            }
            state.input = draft;
        }
    }
    cs.was_busy = state.busy;

    let mut focused = ui.memory(|m| m.has_focus(id));
    // A pending confirmation owns Enter/Esc: drop composer focus so the confirm shortcuts fire.
    if focused && state.pending_confirm.is_some() {
        ui.memory_mut(|m| m.surrender_focus(id));
        focused = false;
    }
    ui.input(|i| cs.ime.observe(&i.events));

    // Popup keys are taken before the TextEdit sees the events.
    let caret0 = caret_byte(&ctx, id, &state.input);
    let blocked = state.pending_confirm.is_some();
    let popup0 = if focused { active_popup(cs, &state.input, caret0, blocked) } else { Popup::None };
    let mut nav = Vec::new();
    if popup0 != Popup::None && !cs.ime.composing {
        // An empty list keeps Enter/Tab for the composer: sending or moving focus must not
        // silently do nothing because "No matching files" is on screen.
        let accept = accept_available(popup_rows(cs, &popup0));
        nav = ui.input_mut(|i| take_nav_keys_with(&mut i.events, accept));
    }
    // Edit-last: Up in an empty, idle composer; Esc cancels an edit.
    let mut edit_keys = (false, false);
    if focused && popup0 == Popup::None && !cs.ime.composing {
        edit_keys = ui.input_mut(|i| {
            let up = state.input.is_empty() && !state.busy && i.consume_key(egui::Modifiers::NONE, Key::ArrowUp);
            let esc = cs.editing.is_some() && i.consume_key(egui::Modifiers::NONE, Key::Escape);
            (up, esc)
        });
    }
    if edit_keys.0 && start_edit(cs, state) {
        let len = state.input.len();
        set_caret(&ctx, id, &state.input, len);
    }
    if edit_keys.1 {
        cs.editing = None;
        state.input.clear();
    }
    // Plain Enter sends (Alt+Enter flips queue/steer while busy); Shift+Enter is a newline.
    let (enter, alt) = if popup0 == Popup::None && !cs.ime.composing {
        ui.input_mut(|i| {
            let send = is_send_key(focused && i.key_pressed(Key::Enter), i.modifiers.shift);
            let alt = i.modifiers.alt;
            if send {
                i.consume_key(egui::Modifiers::NONE, Key::Enter);
            }
            (send, alt)
        })
    } else {
        (false, false)
    };

    // Editing banner and queued chips sit above the input.
    if let Some(orig) = cs.editing.clone() {
        if state.busy || last_user_text(&state.items) != Some(orig.as_str()) {
            cs.editing = None;
        } else {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.label(egui::RichText::new(icons::PENCIL_SIMPLE).font(theme::icon(12.0)).color(theme::FG_MUTED));
                ui.label(egui::RichText::new("Editing your last message. Sending replaces it and runs again.").font(theme::font(11.5)).color(theme::FG_MUTED));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if widgets::text_link(ui, "Cancel (Esc)", 11.5).clicked() {
                        cs.editing = None;
                        state.input.clear();
                    }
                });
            });
            ui.add_space(6.0);
        }
    }
    if let Some(act) = queue::show(ui, &cs.queue.items) {
        chip_action(cs, act, commands);
    }
    if !cs.queue.items.is_empty() {
        ui.add_space(6.0);
    }

    let before = state.input.clone();
    let placeholder = if review && review_in_panel {
        PLACEHOLDER_REVIEW_PANEL
    } else if review {
        PLACEHOLDER_REVIEW
    } else if state.busy {
        PLACEHOLDER_BUSY
    } else {
        PLACEHOLDER_TEXT
    };
    let mut input_rect = Rect::NOTHING;
    // The popup hangs above the whole card (input row included), so it keeps its place when
    // the scroll area inside the composer scrolls the TextEdit.
    let card_top = ui.max_rect().top();
    egui::ScrollArea::vertical().id_salt("composer_scroll").max_height(INPUT_MAX_H).show(ui, |ui| {
        let edit = egui::TextEdit::multiline(&mut state.input)
            .id(id)
            .frame(egui::Frame::NONE)
            .margin(egui::Margin { left: 2, right: 2, top: 0, bottom: 0 })
            .font(theme::font(13.0))
            .text_color(theme::FG)
            .hint_text(egui::RichText::new(placeholder).font(theme::font(13.0)).color(theme::PLACEHOLDER))
            .desired_rows(1)
            .desired_width(f32::INFINITY);
        let out = edit.show(ui);
        input_rect = out.response.rect;
        // Autofocus is one shot: it waits out a popup or a pending confirmation, then either
        // takes focus or gives up. Nothing is retried on later frames, so a click elsewhere
        // (or Tab) keeps the focus the user gave it.
        if autofocus
            && cs.focus_request
            && state.pending_confirm.is_none()
            && !egui::Popup::is_any_open(ui.ctx())
        {
            if !ui.ctx().text_edit_focused() {
                out.response.request_focus();
            }
            cs.focus_request = false;
        }
    });
    if state.input != before {
        cs.hint = None;
        if !state.input.starts_with('/') {
            cs.slash_dismissed = false;
        }
        if cs.mentions.dismissed.is_some_and(|at| state.input.get(at..at + 1) != Some("@")) {
            cs.mentions.dismissed = None;
        }
    }

    // Popup after the edit (rows follow the new text and caret).
    let focused_now = ui.memory(|m| m.has_focus(id));
    let caret = caret_byte(&ctx, id, &state.input);
    let popup = if focused_now || popup0 != Popup::None { active_popup(cs, &state.input, caret, blocked) } else { Popup::None };
    maintain_index(&ctx, cs, state.workspace.as_ref(), matches!(popup, Popup::Mention { .. }));
    maintain_workflows(&ctx, cs, state.workspace.as_ref(), matches!(popup, Popup::Slash { .. }));
    let mut submit_now = false;
    // Anchor for the popup: the card's top edge (the panel rect begins above the input), so
    // scrolling inside the composer never drags the popup with the text.
    let anchor = Rect::from_min_max(egui::pos2(input_rect.left(), card_top), input_rect.max);
    match popup.clone() {
        Popup::Mention { at, end, query } => {
            let rows = cs.mentions.rows(&query).to_vec();
            let n = rows.len();
            for k in &nav {
                match k {
                    NavKey::Up if n > 0 => cs.mentions.selected = (cs.mentions.selected + n - 1) % n,
                    NavKey::Down if n > 0 => cs.mentions.selected = (cs.mentions.selected + 1) % n,
                    NavKey::Dismiss => cs.mentions.dismissed = Some(at),
                    _ => {}
                }
            }
            let mut pick = nav.contains(&NavKey::Accept).then_some(cs.mentions.selected).filter(|&i| i < n);
            if cs.mentions.dismissed != Some(at) {
                let empty_msg = if state.workspace.is_none() {
                    Some("Open a folder to mention files")
                } else if cs.index_job.is_some() && cs.mentions.index.is_empty() {
                    Some("Indexing workspace…")
                } else if n == 0 {
                    Some("No matching files")
                } else {
                    None
                };
                let list: Vec<PopupRow> = rows
                    .iter()
                    .map(|p| {
                        let dir = p.ends_with('/');
                        let name = p.trim_end_matches('/').rsplit('/').next().unwrap_or(p).to_owned();
                        PopupRow { icon: if dir { icons::FOLDER } else { icons::FILE_TEXT }, label: name, detail: p.clone(), tag: "" }
                    })
                    .collect();
                if let Some(i) = popup_list(&ctx, "mention_popup", anchor, "Files and folders", &list, cs.mentions.selected, empty_msg) {
                    pick = Some(i);
                }
            }
            if let Some(i) = pick {
                let end = end.min(state.input.len());
                let caret = mentions::insert(&mut state.input, at, end, &rows[i]);
                set_caret(&ctx, id, &state.input, caret);
                ui.memory_mut(|m| m.request_focus(id));
            }
        }
        Popup::Slash { query } => {
            let rows = slash::items(&query, &cs.workflows);
            let n = rows.len();
            if cs.slash_selected >= n {
                cs.slash_selected = 0;
            }
            for k in &nav {
                match k {
                    NavKey::Up if n > 0 => cs.slash_selected = (cs.slash_selected + n - 1) % n,
                    NavKey::Down if n > 0 => cs.slash_selected = (cs.slash_selected + 1) % n,
                    NavKey::Dismiss => cs.slash_dismissed = true,
                    _ => {}
                }
            }
            let mut pick = nav.contains(&NavKey::Accept).then_some(cs.slash_selected).filter(|&i| i < n);
            if !cs.slash_dismissed {
                let list: Vec<PopupRow> = rows
                    .iter()
                    .map(|it| PopupRow {
                        icon: if matches!(it, SlashItem::Builtin(_)) { icons::LIGHTNING } else { icons::NOTE_PENCIL },
                        label: format!("/{}", it.name()),
                        detail: it.description().to_owned(),
                        tag: it.tag(),
                    })
                    .collect();
                let empty = (n == 0).then_some("No matching commands");
                if let Some(i) = popup_list(&ctx, "slash_popup", anchor, "Commands and workflows", &list, cs.slash_selected, empty) {
                    pick = Some(i);
                }
            } else if nav.contains(&NavKey::Accept) {
                // Popup dismissed: Accept means send.
                submit_now = true;
            }
            if let Some(i) = pick {
                let it = &rows[i];
                let runs_now = matches!(it, SlashItem::Builtin(b) if !b.takes_args());
                state.input = format!("/{}{}", it.name(), if runs_now { "" } else { " " });
                let len = state.input.len();
                set_caret(&ctx, id, &state.input, len);
                cs.slash_selected = 0;
                ui.memory_mut(|m| m.request_focus(id));
                submit_now = runs_now;
            }
        }
        Popup::None => {}
    }

    if let Some((text, _)) = &cs.hint {
        ui.add_space(4.0);
        ui.label(egui::RichText::new(text).font(theme::font(11.5)).color(theme::WARNING));
    } else if state.busy && !state.input.trim().is_empty() && !review {
        ui.add_space(4.0);
        let q = state.settings.queued_messages;
        let (enter_does, alt_does) = match q {
            QueueMode::Queue => ("queue", "steer"),
            QueueMode::SendImmediately => ("steer", "queue"),
        };
        ui.label(egui::RichText::new(format!("Enter to {enter_does} · Alt+Enter to {alt_does}")).font(theme::font(11.5)).color(theme::FG_MUTED));
    }

    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        // Home pulls the `+` 4px into the card margin (its centering accounts for it); the
        // docked box keeps its edge on the column.
        if !docked {
            ui.add_space(-4.0);
        }
        plus_menu(ui, state, commands, &ctx, id);
        ui.add_space(2.0);
        mode_chip(ui, cs, state, commands);
        ui.add_space(2.0);
        super::model_picker::show(ui, state, commands);
        if docked {
            ui.add_space(4.0);
            local_chip(ui, state);
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            let ready = !state.input.trim().is_empty();
            ui.add_space(0.0);
            let clicked = widgets::send_button(ui, state.busy, ready).clicked();
            if clicked && state.busy {
                commands.push(AppCommand::Stop);
            } else if clicked || enter || submit_now {
                submit(cs, state, commands, alt);
            }
            soon_icon(ui, icons::MICROPHONE);
        });
    });
}

fn chip_action(cs: &mut ComposerState, act: ChipAction, commands: &mut Vec<UiAction>) {
    match act {
        ChipAction::Remove(id) => {
            cs.queue.take(id);
            commands.push(AppCommand::ClearQueue(Some(id)));
        }
        ChipAction::Steer(id) => {
            // The backend takes the text from its own queue: if the turn ended and ran the entry
            // meanwhile, nothing is steered (a ClearQueue + SteerPrompt pair would run it twice).
            if cs.queue.take(id).is_some() {
                commands.push(AppCommand::SteerQueued(id));
            }
        }
        ChipAction::ClearAll => {
            cs.queue.items.clear();
            commands.push(AppCommand::ClearQueue(None));
        }
    }
}

/// Submit the composer text: slash commands, edit-last, queue/steer while busy, or send.
fn submit(cs: &mut ComposerState, state: &mut UiState, commands: &mut Vec<UiAction>, alt: bool) {
    let text = state.input.trim().to_owned();
    if text.is_empty() {
        return;
    }
    let what = classify(&text, &cs.workflows);
    if state.busy {
        let Submit::Prompt { prompt, mode: None, .. } = what else {
            cs.hint = Some(("Commands run once the current turn finishes.".into(), text));
            return;
        };
        state.input.clear();
        match state.settings.queued_messages.for_send(alt) {
            QueueMode::Queue => {
                if !cs.queue.backend_owned {
                    cs.queue.push(prompt.clone());
                    cs.queue_conv = state.current;
                }
                commands.push(AppCommand::QueuePrompt(prompt));
            }
            QueueMode::SendImmediately => commands.push(AppCommand::SteerPrompt(prompt)),
        }
        return;
    }
    match what {
        Submit::Prompt { bubble, prompt, mode } => {
            let editing = cs.editing.take();
            let edit_mode = editing.as_deref().and_then(edit_mode);
            if editing.is_some() {
                // Replace the last exchange: cut at the same place the backend truncates
                // (the last user bubble), shared through `orchestrate::edit_cut`.
                if let Some(i) = crate::agent::orchestrate::edit_cut(&state.items) {
                    state.items.truncate(i);
                }
                // Task updates of the replaced turn go with it (the turn count drops by one).
                let turn = crate::ui::taskgroups::turn_of(&state.items);
                state.task_updates.retain(|u| u.turn < turn);
            }
            state.input = bubble;
            if state.take_prompt().is_none() {
                return;
            }
            commands.push(match (editing, mode) {
                // Keep the edited prompt's one-shot mode instead of finishing in the default.
                (Some(_), _) => AppCommand::EditLastPrompt(prompt, edit_mode),
                (None, Some(m)) => AppCommand::SendPromptWithMode(prompt, m),
                (None, None) => AppCommand::SendPrompt(prompt),
            });
        }
        Submit::SetMode(m) => {
            state.input.clear();
            set_mode(cs, state, commands, m);
        }
        Submit::Local(b) => {
            state.input.clear();
            cs.editing = None;
            match b {
                Builtin::New => {
                    state.new_chat();
                    commands.push(AppCommand::NewChat);
                }
                Builtin::Clear => {
                    state.items.clear();
                    state.expanded.clear();
                    commands.push(AppCommand::ClearHistory);
                }
                Builtin::Compact => {
                    state.items.push(ChatItem::Notice("Compacting older messages…".into()));
                    commands.push(AppCommand::Compact);
                }
                Builtin::Plan | Builtin::Fast => {}
            }
        }
    }
}

fn set_mode(cs: &mut ComposerState, state: &UiState, commands: &mut Vec<UiAction>, m: AgentMode) {
    cs.modes.insert(state.current, m);
    commands.push(AppCommand::SetAgentMode(m));
}

/// One popup row.
struct PopupRow {
    icon: &'static str,
    label: String,
    detail: String,
    tag: &'static str,
}

/// Floating list above the input. Returns the clicked row.
fn popup_list(ctx: &egui::Context, salt: &str, anchor: Rect, title: &str, rows: &[PopupRow], selected: usize, empty: Option<&str>) -> Option<usize> {
    if !anchor.is_finite() {
        return None;
    }
    let mut clicked = None;
    let w = POPUP_W.min(anchor.width().max(240.0));
    egui::Area::new(egui::Id::unique("anticraft_composer_popup").with(salt))
        .order(egui::Order::Foreground)
        .fixed_pos(anchor.left_top() + Vec2::new(-4.0, -8.0))
        .pivot(Align2::LEFT_BOTTOM)
        .constrain(true)
        .show(ctx, |ui| {
            widgets::card(8, 4).shadow(ui.visuals().popup_shadow).show(ui, |ui| {
                ui.set_width(w);
                ui.spacing_mut().item_spacing.y = 0.0;
                let (hr, _) = ui.allocate_exact_size(Vec2::new(w, 20.0), Sense::hover());
                ui.painter().text(egui::pos2(hr.left() + 8.0, hr.center().y), Align2::LEFT_CENTER, title, theme::font(11.0), theme::FG_MUTED);
                if let Some(msg) = empty.filter(|_| rows.is_empty()) {
                    let (r, _) = ui.allocate_exact_size(Vec2::new(w, POPUP_ROW_H), Sense::hover());
                    ui.painter().text(egui::pos2(r.left() + 8.0, r.center().y), Align2::LEFT_CENTER, msg, theme::font(12.5), theme::FG_SECONDARY);
                }
                for (i, row) in rows.iter().enumerate() {
                    let (rect, r) = ui.allocate_exact_size(Vec2::new(w, POPUP_ROW_H), Sense::click());
                    let p = ui.painter();
                    let sel = i == selected;
                    if sel || r.hovered() {
                        p.rect_filled(rect, 4, if sel { theme::ROW_SELECTED } else { theme::SECONDARY });
                    }
                    p.text(egui::pos2(rect.left() + 8.0, rect.center().y), Align2::LEFT_CENTER, row.icon, theme::icon(14.0), theme::FG_SECONDARY);
                    let l = p.text(egui::pos2(rect.left() + 30.0, rect.center().y), Align2::LEFT_CENTER, &row.label, theme::font(13.0), theme::FG);
                    let mut right = rect.right() - 8.0;
                    if !row.tag.is_empty() {
                        let t = p.text(egui::pos2(right, rect.center().y), Align2::RIGHT_CENTER, row.tag, theme::font(11.0), theme::FG_MUTED);
                        right = t.left() - 8.0;
                    }
                    if right - l.right() > 30.0 && !row.detail.is_empty() && row.detail != row.label {
                        let dr = Rect::from_min_max(egui::pos2(l.right() + 10.0, rect.top()), egui::pos2(right, rect.bottom()));
                        widgets::paint_truncated(ui, dr, &row.detail, theme::font(11.5), theme::FG_MUTED);
                    }
                    if r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        clicked = Some(i);
                    }
                }
            });
        });
    clicked
}

/// Mode chip: Planning / Fast for this conversation.
fn mode_chip(ui: &mut Ui, cs: &mut ComposerState, state: &UiState, commands: &mut Vec<UiAction>) {
    let mode = cs.modes.get(&state.current).copied().unwrap_or(state.settings.agent_mode);
    let (icon, label) = mode_label(mode);
    let r = widgets::pill_chip(ui, Some(icon), label, true, true, false);
    let open = egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(&r));
    let r = if open { r } else { r.on_hover_text("Conversation mode") };
    let mut pick = None;
    egui::Popup::menu(&r)
        .align(egui::RectAlign::BOTTOM_START)
        .align_alternatives(&[egui::RectAlign::TOP_START])
        .gap(4.0)
        .width(280.0)
        .frame(widgets::card(8, 4).shadow(ui.visuals().popup_shadow))
        .show(|ui| {
            ui.set_width(272.0);
            ui.spacing_mut().item_spacing.y = 0.0;
            for (m, desc) in [
                (AgentMode::Planning, "Research, write a plan you can review, then build it"),
                (AgentMode::Fast, "Act right away, for quick and simple tasks"),
            ] {
                if mode_row(ui, m, desc, m == mode) {
                    pick = Some(m);
                    ui.close();
                }
            }
        });
    if let Some(m) = pick.filter(|&m| m != mode) {
        set_mode(cs, state, commands, m);
    }
}

fn mode_label(m: AgentMode) -> (&'static str, &'static str) {
    match m {
        AgentMode::Planning => (icons::NOTE_PENCIL, "Planning"),
        AgentMode::Fast => (icons::LIGHTNING, "Fast"),
    }
}

fn mode_row(ui: &mut Ui, m: AgentMode, desc: &str, current: bool) -> bool {
    let (rect, r) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 44.0), Sense::click());
    let p = ui.painter();
    if r.hovered() || current {
        p.rect_filled(rect, 4, if current { theme::ROW_SELECTED } else { theme::SECONDARY });
    }
    let (icon, label) = mode_label(m);
    p.text(egui::pos2(rect.left() + 8.0, rect.top() + 14.0), Align2::LEFT_CENTER, icon, theme::icon(14.0), theme::FG_SECONDARY);
    p.text(egui::pos2(rect.left() + 30.0, rect.top() + 14.0), Align2::LEFT_CENTER, label, theme::font(13.0), theme::FG);
    let dr = Rect::from_min_max(egui::pos2(rect.left() + 30.0, rect.top() + 22.0), egui::pos2(rect.right() - 26.0, rect.bottom() - 4.0));
    widgets::paint_truncated(ui, dr, desc, theme::font(11.5), theme::FG_MUTED);
    if current {
        ui.painter().text(egui::pos2(rect.right() - 10.0, rect.top() + 14.0), Align2::RIGHT_CENTER, icons::CHECK, theme::icon(13.0), theme::FG);
    }
    r.clicked()
}

/// `+` button: menu with "Mention a file", "Run a workflow" and "Open folder...".
fn plus_menu(ui: &mut Ui, state: &mut UiState, commands: &mut Vec<UiAction>, ctx: &egui::Context, input_id: egui::Id) {
    let r = widgets::icon_button(ui, icons::PLUS, 28.0, "", true);
    let open = egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(&r));
    let r = if open { r } else { r.on_hover_text("Add context") };
    let mut focus_end = false;
    egui::Popup::menu(&r)
        .align(egui::RectAlign::BOTTOM_START)
        .align_alternatives(&[egui::RectAlign::TOP_START])
        .gap(4.0)
        .width(220.0)
        .frame(widgets::card(8, 4).shadow(ui.visuals().popup_shadow))
        .show(|ui| {
            ui.set_width(212.0);
            if menu_row(ui, icons::FILE_TEXT, "Mention a file", "@", true) {
                if !state.input.is_empty() && !state.input.ends_with([' ', '\n']) {
                    state.input.push(' ');
                }
                state.input.push('@');
                focus_end = true;
                ui.close();
            }
            if menu_row(ui, icons::LIGHTNING, "Run a command", "/", state.input.trim().is_empty()) {
                state.input = "/".into();
                focus_end = true;
                ui.close();
            }
            if menu_row(ui, icons::FOLDER_OPEN, "Open folder...", "", !state.busy) {
                commands.push(AppCommand::PickWorkspace);
                ui.close();
            }
        });
    if focus_end {
        set_caret(ctx, input_id, &state.input, state.input.len());
        ctx.memory_mut(|m| m.request_focus(input_id));
    }
}

/// 26px menu row: icon, label, right-aligned hint.
fn menu_row(ui: &mut Ui, icon: &str, label: &str, hint: &str, enabled: bool) -> bool {
    let (rect, r) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 26.0), if enabled { Sense::click() } else { Sense::hover() });
    let p = ui.painter();
    if enabled && r.hovered() {
        p.rect_filled(rect, 4, theme::SECONDARY);
    }
    let a = if enabled { 1.0 } else { 0.4 };
    p.text(egui::pos2(rect.left() + 8.0, rect.center().y), Align2::LEFT_CENTER, icon, theme::icon(14.0), theme::FG_SECONDARY.gamma_multiply(a));
    p.text(egui::pos2(rect.left() + 30.0, rect.center().y), Align2::LEFT_CENTER, label, theme::font(13.0), theme::FG.gamma_multiply(a));
    if !hint.is_empty() {
        p.text(egui::pos2(rect.right() - 8.0, rect.center().y), Align2::RIGHT_CENTER, hint, theme::font(12.0), theme::FG_MUTED);
    }
    enabled && r.clicked()
}

/// Visual-only 28x28 icon: tooltip "Coming soon".
fn soon_icon(ui: &mut Ui, icon: &str) {
    let (rect, r) = ui.allocate_exact_size(Vec2::splat(28.0), Sense::hover());
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, icon, theme::icon(15.0), theme::FG_SECONDARY.gamma_multiply(0.7));
    r.on_hover_text(widgets::COMING_SOON);
}

/// Bottom tray of the card (30 h, lighter fill, rounded bottom corners) with the "Local" chip.
fn tray(ui: &mut Ui, state: &UiState) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), TRAY_H), Sense::hover());
    let r = RADIUS - 1;
    let fill_rect = Rect::from_min_max(rect.min + Vec2::new(1.0, 0.0), rect.max - Vec2::splat(1.0));
    ui.painter().rect_filled(fill_rect, CornerRadius { nw: 0, ne: 0, sw: r, se: r }, TRAY_FILL);
    let mut child = ui.new_child(
        egui::UiBuilder::new().max_rect(rect.shrink2(Vec2::new(8.0, 3.0))).layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    local_chip(&mut child, state);
}

/// Workspace chip above the Home composer: folder name + caret, menu of recent workspaces.
pub fn workspace_chip(ui: &mut Ui, state: &mut UiState, commands: &mut Vec<UiAction>) {
    let label = state.workspace.as_deref().map(format::project_label).unwrap_or_else(|| "Choose a folder".into());
    let enabled = !state.busy;
    let font = theme::font(14.0);
    let gw = ui.painter().layout_no_wrap(label.clone(), font.clone(), theme::FG_MUTED).size().x;
    let (rect, r) = ui.allocate_exact_size(Vec2::new(gw + 30.0, 24.0), if enabled { Sense::click() } else { Sense::hover() });
    let open = egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(&r));
    let hot = enabled && (r.hovered() || open);
    let p = ui.painter();
    if hot {
        p.rect_filled(rect, 6, theme::SECONDARY.gamma_multiply(0.6));
    }
    let c = if hot { theme::FG } else { theme::FG_MUTED };
    p.text(egui::pos2(rect.left() + 8.0, rect.center().y), Align2::LEFT_CENTER, &label, font, c);
    p.text(egui::pos2(rect.left() + 12.0 + gw, rect.center().y + 1.0), Align2::LEFT_CENTER, icons::CARET_DOWN, theme::icon(11.0), c);
    let tip = state.workspace.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "Pick a workspace folder".into());
    let r = if open { r } else { r.on_hover_text(tip) };
    egui::Popup::menu(&r).frame(widgets::card(8, 4).shadow(ui.visuals().popup_shadow)).gap(4.0).width(300.0).show(|ui| {
        ui.set_width(292.0);
        let history = state.settings.workspace_history.clone();
        if !history.is_empty() {
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.add_space(8.0);
                ui.label(egui::RichText::new("Recent").font(theme::medium(11.0)).color(theme::FG_MUTED));
            });
            ui.add_space(2.0);
        }
        for ws in &history {
            let cur = state.workspace.as_deref() == Some(ws.as_path());
            if workspace_row(ui, &format::project_label(ws), &ws.display().to_string(), cur) {
                commands.push(AppCommand::SetWorkspace(ws.clone()));
                ui.close();
            }
        }
        if !history.is_empty() {
            ui.add_space(2.0);
            ui.separator();
            ui.add_space(2.0);
        }
        if menu_row(ui, icons::FOLDER_OPEN, "Open folder...", "", true) {
            commands.push(AppCommand::PickWorkspace);
            ui.close();
        }
    });
}

/// Recent-workspace row: name, muted path (truncated), check when current.
fn workspace_row(ui: &mut Ui, name: &str, path: &str, current: bool) -> bool {
    let (rect, r) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 26.0), Sense::click());
    let p = ui.painter();
    if r.hovered() {
        p.rect_filled(rect, 4, theme::SECONDARY);
    }
    p.text(egui::pos2(rect.left() + 8.0, rect.center().y), Align2::LEFT_CENTER, icons::FOLDER, theme::icon(14.0), theme::FG_SECONDARY);
    let n = p.text(egui::pos2(rect.left() + 30.0, rect.center().y), Align2::LEFT_CENTER, name, theme::font(13.0), theme::FG);
    let right = rect.right() - if current { 30.0 } else { 8.0 };
    if right - n.right() > 30.0 {
        let pr = Rect::from_min_max(egui::pos2(n.right() + 8.0, rect.top()), egui::pos2(right, rect.bottom()));
        widgets::paint_truncated(ui, pr, path, theme::font(11.5), theme::FG_MUTED);
    }
    if current {
        ui.painter().text(egui::pos2(rect.right() - 10.0, rect.center().y), Align2::RIGHT_CENTER, icons::CHECK, theme::icon(13.0), theme::FG);
    }
    r.on_hover_text(path).clicked()
}

/// "Local" chip in the composer tray (informational; tooltip shows the workspace path).
pub fn local_chip(ui: &mut Ui, state: &UiState) {
    let tip = state.workspace.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "No workspace".into());
    let (rect, r) = ui.allocate_exact_size(Vec2::new(64.0, 24.0), Sense::hover());
    let rect = rect.translate(Vec2::new(0.0, -1.0));
    let p = ui.painter();
    p.text(egui::pos2(rect.left() + 6.0, rect.center().y), Align2::LEFT_CENTER, icons::DESKTOP, theme::icon(13.0), theme::FG_MUTED);
    p.text(egui::pos2(rect.left() + 24.0, rect.center().y), Align2::LEFT_CENTER, "Local", theme::font(12.0), theme::FG_MUTED);
    r.on_hover_text(tip);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::context::WorkflowScope;
    use egui::Modifiers;

    fn key(k: Key) -> Event {
        Event::Key { key: k, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE }
    }
    fn text(t: &str) -> Event {
        Event::Text(t.into())
    }
    fn typed(s: &str) -> Vec<Event> {
        s.chars()
            .flat_map(|c| {
                let k = if c == ' ' { Key::Space } else { Key::from_name(&c.to_string()).unwrap_or(Key::A) };
                [key(k), text(&c.to_string())]
            })
            .collect()
    }

    /// Telex "Xin chaof": the tool sends Backspace x2 then "ào" as injected chars, whose
    /// phantom key presses (space -> `Key::D`) are dropped first. The result must be
    /// "Xin chào", with the popup key pass leaving Backspace/Text order intact.
    #[test]
    fn telex_backspace_text_sequence_keeps_order() {
        let mut ev = typed("Xin");
        // Injected space: phantom Key::D (VK_PACKET scancode 0x20) followed by the text.
        ev.push(key(Key::D));
        ev.push(text(" "));
        ev.extend(typed("chao"));
        // User types 'f' (huyền): swallowed by the tool, which rewrites "ao" -> "ào".
        ev.push(key(Key::Backspace));
        ev.push(key(Key::Backspace));
        ev.push(key(Key::A));
        ev.push(text("à"));
        ev.push(key(Key::O));
        ev.push(text("o"));
        super::super::sanitize_injected_keys(&mut ev);
        // A popup is never open here, but run the nav pass anyway: it must not move anything.
        let nav = take_nav_keys(&mut ev);
        assert!(nav.is_empty());
        let (mut s, mut caret) = (String::new(), 0);
        apply_edit_events(&mut s, &mut caret, &ev);
        assert_eq!(s, "Xin chào");
        assert_eq!(caret, s.chars().count());
    }

    /// Same sequence split across frames, with the mention popup open in between: the nav
    /// keys are taken but Backspace and Text stay in order.
    #[test]
    fn nav_pass_only_takes_navigation_keys() {
        let mut ev = vec![text("@"), key(Key::ArrowDown), key(Key::Backspace), text("ư"), key(Key::Enter), text("x")];
        let nav = take_nav_keys(&mut ev);
        assert_eq!(nav, vec![NavKey::Down, NavKey::Accept]);
        assert_eq!(ev.len(), 4);
        assert!(matches!(&ev[1], Event::Key { key: Key::Backspace, .. }));
        let (mut s, mut c) = ("ab".to_owned(), 2);
        apply_edit_events(&mut s, &mut c, &ev);
        // "@" typed, Backspace removes it, then "ư" and "x": order preserved.
        assert_eq!(s, "abưx");
        // Shift+Enter is a newline, not Accept.
        let mut ev = vec![Event::Key { key: Key::Enter, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::SHIFT }];
        assert!(take_nav_keys(&mut ev).is_empty());
        assert_eq!(ev.len(), 1);
    }

    #[test]
    fn backspace_handles_multibyte() {
        let (mut s, mut c) = ("đườ".to_owned(), 3);
        apply_edit_events(&mut s, &mut c, &[key(Key::Backspace), text("ờng")]);
        assert_eq!(s, "đường");
        assert_eq!(char_to_byte("đường", 1), "đ".len());
    }

    #[test]
    fn ime_composition_blocks_enter() {
        let mut t = ImeTracker::default();
        t.observe(&[Event::Ime(egui::ImeEvent::Preedit { text: "chà".into(), active_range_chars: None })]);
        assert!(t.composing);
        t.observe(&[Event::Ime(egui::ImeEvent::Commit("chào".into()))]);
        assert!(!t.composing);
        t.observe(&[Event::Ime(egui::ImeEvent::Preedit { text: "x".into(), active_range_chars: None }), Event::Ime(egui::ImeEvent::Preedit { text: String::new(), active_range_chars: None })]);
        assert!(!t.composing);
    }

    #[test]
    fn classify_commands() {
        let wf = vec![Workflow { name: "deploy".into(), description: "".into(), body: "Ship it".into(), scope: WorkflowScope::Workspace, path: "x".into() }];
        assert_eq!(classify("hello", &wf), Submit::Prompt { bubble: "hello".into(), prompt: "hello".into(), mode: None });
        assert_eq!(classify("/plan add tests", &wf), Submit::Prompt { bubble: "/plan add tests".into(), prompt: "add tests".into(), mode: Some(AgentMode::Planning) });
        assert!(matches!(classify("/plan", &wf), Submit::Prompt { prompt, mode: Some(AgentMode::Planning), .. } if prompt == slash::BARE_PLAN_PROMPT));
        assert_eq!(classify("/fast", &wf), Submit::SetMode(AgentMode::Fast));
        assert!(matches!(classify("/fast go", &wf), Submit::Prompt { mode: Some(AgentMode::Fast), .. }));
        assert_eq!(classify("/new", &wf), Submit::Local(Builtin::New));
        assert!(matches!(classify("/deploy prod", &wf), Submit::Prompt { prompt, mode: None, .. } if prompt.contains("Ship it") && prompt.contains("prod")));
        assert!(matches!(classify("/unknown x", &wf), Submit::Prompt { prompt, .. } if prompt == "/unknown x"));
    }

    fn st() -> UiState {
        UiState::default()
    }

    #[test]
    fn submit_idle_sends_and_busy_queues_or_steers() {
        let mut cs = ComposerState::default();
        let mut s = st();
        let mut cmds = Vec::new();
        s.input = "do it".into();
        submit(&mut cs, &mut s, &mut cmds, false);
        assert!(matches!(&cmds[..], [AppCommand::SendPrompt(p)] if p == "do it"));
        assert!(s.busy);
        cmds.clear();
        s.input = "next".into();
        submit(&mut cs, &mut s, &mut cmds, false);
        assert!(matches!(&cmds[..], [AppCommand::QueuePrompt(p)] if p == "next"));
        assert_eq!(cs.queue.items.len(), 1);
        cmds.clear();
        s.input = "now".into();
        submit(&mut cs, &mut s, &mut cmds, true);
        assert!(matches!(&cmds[..], [AppCommand::SteerPrompt(p)] if p == "now"));
        cmds.clear();
        s.input = "/new".into();
        submit(&mut cs, &mut s, &mut cmds, false);
        assert!(cmds.is_empty() && cs.hint.is_some() && s.input == "/new");
    }

    #[test]
    fn editing_a_plan_prompt_keeps_its_one_shot_mode() {
        assert_eq!(edit_mode("/plan add tests"), Some(AgentMode::Planning));
        assert_eq!(edit_mode("/fast do it"), Some(AgentMode::Fast));
        assert_eq!(edit_mode("/plan"), Some(AgentMode::Planning));
        assert_eq!(edit_mode("plain text"), None);
        assert_eq!(edit_mode("/fast"), None, "a bare /fast sets the mode instead of sending");
        let mut cs = ComposerState::default();
        let mut s = st();
        s.items = vec![ChatItem::User("/plan add tests".into()), ChatItem::Assistant("plan".into())];
        assert!(start_edit(&mut cs, &mut s));
        assert_eq!(s.input, "/plan add tests");
        s.input = "/plan add tests and docs".into();
        let mut cmds = Vec::new();
        submit(&mut cs, &mut s, &mut cmds, false);
        assert!(matches!(&cmds[..], [AppCommand::EditLastPrompt(p, Some(AgentMode::Planning))] if p == "add tests and docs"));
    }

    #[test]
    fn edit_last_truncates_and_resends() {
        let mut cs = ComposerState::default();
        let mut s = st();
        s.items = vec![ChatItem::User("first".into()), ChatItem::Assistant("a".into()), ChatItem::User("second".into()), ChatItem::Assistant("b".into())];
        assert!(start_edit(&mut cs, &mut s));
        assert_eq!(s.input, "second");
        s.input = "second, fixed".into();
        let mut cmds = Vec::new();
        submit(&mut cs, &mut s, &mut cmds, false);
        assert!(matches!(&cmds[..], [AppCommand::EditLastPrompt(p, None)] if p == "second, fixed"));
        assert!(matches!(&s.items[2], ChatItem::User(t) if t == "second, fixed"));
        assert_eq!(s.items.iter().filter(|i| matches!(i, ChatItem::User(_))).count(), 2);
        assert!(cs.editing.is_none());
    }

    #[test]
    fn chip_actions_emit_commands() {
        let mut cs = ComposerState::default();
        let a = cs.queue.push("a".into());
        let b = cs.queue.push("b".into());
        let mut cmds = Vec::new();
        chip_action(&mut cs, ChipAction::Steer(a), &mut cmds);
        // One command: the backend moves the entry only if it is still queued, so a turn that
        // ends meanwhile cannot run it from the queue as well.
        assert!(matches!(&cmds[..], [AppCommand::SteerQueued(x)] if *x == a));
        cmds.clear();
        chip_action(&mut cs, ChipAction::Remove(b), &mut cmds);
        assert!(cs.queue.items.is_empty());
        assert!(matches!(&cmds[..], [AppCommand::ClearQueue(Some(x))] if *x == b));
    }

    /// Nothing selectable: Enter and Tab stay for the composer (send / leave the field) and
    /// an empty list still takes Up/Down/Escape.
    #[test]
    fn empty_popup_keeps_enter_and_tab() {
        assert!(!accept_available(0));
        assert!(accept_available(1));
        let mut ev = vec![key(Key::Enter), key(Key::Tab), key(Key::ArrowDown), key(Key::Escape)];
        let nav = take_nav_keys_with(&mut ev, accept_available(0));
        assert_eq!(nav, vec![NavKey::Down, NavKey::Dismiss]);
        assert_eq!(ev.len(), 2, "Enter and Tab are left for the TextEdit");
        assert!(matches!(&ev[0], Event::Key { key: Key::Enter, .. }));
        assert!(matches!(&ev[1], Event::Key { key: Key::Tab, .. }));
        // With a row to pick, Accept is taken as before.
        let mut ev = vec![key(Key::Enter), key(Key::Tab)];
        let nav = take_nav_keys_with(&mut ev, accept_available(2));
        assert_eq!(nav, vec![NavKey::Accept, NavKey::Accept]);
        assert!(ev.is_empty());
    }

    /// The popup row count follows the list the popup would draw.
    #[test]
    fn popup_rows_follow_the_list() {
        let mut cs = ComposerState::default();
        assert_eq!(popup_rows(&mut cs, &Popup::None), 0);
        assert_eq!(popup_rows(&mut cs, &Popup::Slash { query: "zzzz".into() }), 0, "no command matches");
        assert!(popup_rows(&mut cs, &Popup::Slash { query: String::new() }) > 0, "built-ins are listed");
        assert_eq!(popup_rows(&mut cs, &Popup::Mention { at: 0, end: 1, query: "a".into() }), 0, "empty index");
    }

    /// Autofocus is one shot: a fresh state asks once, and the request is not renewed by the
    /// state round-tripping through egui temp memory.
    #[test]
    fn autofocus_is_requested_once() {
        let ctx = egui::Context::default();
        assert!(with_state(&ctx, |cs| cs.focus_request), "the first use arms autofocus");
        with_state(&ctx, |cs| cs.focus_request = false);
        assert!(!with_state(&ctx, |cs| cs.focus_request), "the request is consumed, not renewed");
        assert!(!load(&ctx).focus_request);
    }

    /// The app's first screen: Home with a default state, drawn for two frames. `with_state`
    /// used to borrow the composer state in place inside `ctx.data_mut`, so the composer's
    /// first lock-taking call (`artifacts_panel::load`) waited on the write lock its own
    /// thread held, and egui's debug lock check panicked after 10 s ("Failed to acquire
    /// RwLock write ... Deadlock?"). egui 0.37 has no `Context::run`; `run_ui` runs the pass.
    #[test]
    fn home_composer_renders_two_frames() {
        let ctx = egui::Context::default();
        theme::apply(&ctx);
        let mut state = UiState::default();
        assert_eq!(state.screen(), super::super::Screen::Home, "a default state starts on Home");
        let mut commands = Vec::new();
        for _ in 0..2 {
            let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(1400.0, 900.0))), ..Default::default() };
            ctx.run_ui(input, |ui| {
                egui::CentralPanel::default().show(ui, |ui| super::super::home::show(ui, &mut state, &mut commands));
            })
            .drop_without_applying_deltas();
        }
        assert!(commands.is_empty(), "drawing alone sends no backend commands");
        // The state went back into temp memory after the first frame: its one autofocus
        // request was used then and was not armed again by a fresh state on the second.
        assert!(!load(&ctx).focus_request);
    }

    /// Height of the docked composer holding `input`, once the frame has settled.
    fn docked_height(input: &str) -> f32 {
        let ctx = egui::Context::default();
        theme::apply(&ctx);
        let mut state = UiState { input: input.to_owned(), ..UiState::default() };
        let mut commands = Vec::new();
        let mut h = 0.0;
        for _ in 0..2 {
            let raw = egui::RawInput { screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(1400.0, 900.0))), ..Default::default() };
            ctx.run_ui(raw, |ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    h = ui.scope(|ui| show_docked(ui, &mut state, &mut commands)).response.rect.height();
                });
            })
            .drop_without_applying_deltas();
        }
        h
    }

    /// The conversation's composer is one line tall until its text needs more, as on Home. It
    /// used to keep a 135 px input whatever it held, so the first prompt tripled its height.
    #[test]
    fn the_docked_composer_grows_with_its_text_from_one_line() {
        let empty = docked_height("");
        assert!(empty < 90.0, "one line plus the controls row: {empty}");
        assert!((docked_height("Xin chào") - empty).abs() < 0.5, "a short line fits the line that is there");
        let five = docked_height("1\n2\n3\n4\n5");
        assert!(five > empty + 50.0, "{empty} -> {five}");
        // Past the cap the input scrolls instead of pushing the transcript up.
        let many = docked_height(&"line\n".repeat(80));
        assert!(many > five && many <= INPUT_MAX_H + 70.0, "{many}");
    }

    #[test]
    fn popup_detection() {
        let cs = ComposerState::default();
        assert_eq!(active_popup(&cs, "/pl", 3, false), Popup::Slash { query: "pl".into() });
        assert_eq!(active_popup(&cs, "see @ma", 7, false), Popup::Mention { at: 4, end: 7, query: "ma".into() });
        assert_eq!(active_popup(&cs, "see @ma", 7, true), Popup::None);
        let mut cs = cs;
        cs.mentions.dismissed = Some(4);
        assert_eq!(active_popup(&cs, "see @ma", 7, false), Popup::None);
    }
}
