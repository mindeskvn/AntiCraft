//! Artifacts side panel (ORCHESTRATION.md section E.2).
//!
//! Lists the conversation's artifacts with versions, renders the selected one as Markdown,
//! and keeps comments per artifact. While a `request_review` is pending it shows the review
//! bar (Proceed / Request changes (N)); outside a review, collected comments can be sent as
//! one feedback message ([`crate::agent::artifacts::comments_to_feedback`]).
//!
//! Artifacts are derived from the transcript ([`derive`]): every successful `write_artifact`
//! tool card carries the name, summary and full content in its args, both live and after a
//! stored conversation is reloaded. The panel never reads files on the UI thread.
//!
//! Panel state (open flag, selection, drafts, comments) lives in egui temp memory
//! ([`load`] / [`store`]) so it needs no `UiState` field. A review answer chosen in the panel
//! or in the composer banner is posted with [`post_review_answer`]; the owner of the
//! `ReviewRequest` reply channel collects it with [`take_review_answer`] (see INTERFACES.md).

use std::sync::Arc;

use egui::{Align2, Rect, Sense, Stroke, Ui, Vec2};
use egui_commonmark::CommonMarkCache;

use super::{icons, markdown, theme, widgets, ChatItem};
use crate::agent::artifacts::{ArtifactComment, ArtifactKind, ReviewState};
use crate::agent::orchestrate::{self, ReviewOutcome};

/// Default panel width; it shrinks on narrow windows.
pub const PANEL_WIDTH: f32 = 380.0;

/// One artifact as known to the UI.
#[derive(Debug, Clone, PartialEq)]
pub struct ArtifactView {
    pub name: String,
    pub kind: ArtifactKind,
    pub summary: String,
    pub version: u32,
    pub review: ReviewState,
    /// Latest Markdown content (shared: a derivation hands the same buffer to the panel
    /// without copying it again).
    pub content: Arc<str>,
}

impl ArtifactView {
    /// Display title: "Implementation Plan", "Task", "Walkthrough" or the raw name.
    pub fn title(&self) -> String {
        match self.kind {
            ArtifactKind::TaskList => "Task".into(),
            ArtifactKind::ImplementationPlan => "Implementation Plan".into(),
            ArtifactKind::Walkthrough => "Walkthrough".into(),
            ArtifactKind::Other => self.name.clone(),
        }
    }
}

/// A `request_review` call waiting for the user.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PendingReview {
    pub call_id: String,
    pub artifacts: Vec<String>,
    pub message: String,
}

/// Panel state kept in egui memory (see [`load`]).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ArtifactsPanel {
    pub open: bool,
    pub artifacts: Vec<ArtifactView>,
    pub selected: Option<String>,
    /// Draft comment for the selected artifact.
    pub draft: String,
    /// Comments not yet sent.
    pub comments: Vec<ArtifactComment>,
    /// Call id of the pending `request_review`, if any.
    pub pending_review: Option<String>,
    /// Message of the pending review.
    pub review_message: String,
    /// Artifact names the pending review asks about.
    pub review_artifacts: Vec<String>,
    /// Review the panel was auto-opened for (opens once per review).
    pub auto_opened: Option<String>,
    /// Review already answered from the UI (hidden until the agent's reply arrives).
    pub answered: Option<String>,
    /// No turn is running, so feedback outside a review can be sent as a prompt.
    pub idle: bool,
    /// Checklist form of the body drawn last (see [`ChecklistCache`]).
    pub(crate) checklist: ChecklistCache,
}

/// What the user did in the panel this frame.
#[derive(Debug, Clone, PartialEq)]
pub enum PanelAction {
    None,
    Approve,
    RequestChanges(Vec<ArtifactComment>),
    Close,
}

impl ArtifactsPanel {
    /// Insert or replace by name; selects new artifacts.
    pub fn upsert(&mut self, a: ArtifactView) {
        match self.artifacts.iter_mut().find(|x| x.name == a.name) {
            Some(x) => *x = a,
            None => {
                self.selected = Some(a.name.clone());
                self.artifacts.push(a);
            }
        }
    }

    pub fn add_comment(&mut self) {
        let text = self.draft.trim().to_owned();
        if let (false, Some(name)) = (text.is_empty(), self.selected.clone()) {
            self.comments.push(ArtifactComment { artifact: name, anchor: None, text });
            self.draft.clear();
        }
    }

    /// Replace the artifact list with the transcript's and track the pending review. A new
    /// review opens the panel once and selects the first artifact it names.
    pub fn sync(&mut self, artifacts: Vec<ArtifactView>, pending: Option<&PendingReview>) {
        for a in artifacts {
            self.upsert(a);
        }
        if self.selected.as_ref().is_some_and(|s| !self.artifacts.iter().any(|a| &a.name == s)) {
            self.selected = self.artifacts.last().map(|a| a.name.clone());
        }
        match pending.filter(|p| self.answered.as_deref() != Some(p.call_id.as_str())) {
            Some(p) => {
                self.pending_review = Some(p.call_id.clone());
                self.review_message = p.message.clone();
                self.review_artifacts = p.artifacts.clone();
                if self.auto_opened.as_deref() != Some(p.call_id.as_str()) {
                    self.auto_opened = Some(p.call_id.clone());
                    self.open = true;
                    if let Some(n) = p.artifacts.iter().find(|n| self.artifacts.iter().any(|a| &a.name == *n)) {
                        self.selected = Some(n.clone());
                    }
                }
            }
            None => {
                self.pending_review = None;
                self.review_message.clear();
                self.review_artifacts.clear();
            }
        }
    }

    /// Selected artifact, if any.
    pub fn current(&self) -> Option<&ArtifactView> {
        self.selected.as_ref().and_then(|n| self.artifacts.iter().find(|a| &a.name == n))
    }

    pub fn comment_count(&self, name: &str) -> usize {
        self.comments.iter().filter(|c| c.artifact == name).count()
    }
}

/// Artifacts and the pending review derived from the transcript. Versions count the
/// successful writes of each name; artifacts named by a pending review are `Pending`, those
/// named by an answered review `Approved` unless the answer was an error.
///
/// Bodies are shared (`Arc<str>`), and a transcript that did not change since the last call
/// returns the memoized result instead of being walked again. `states` belongs to one
/// conversation and is kept in egui temp memory by [`with_states`].
pub fn derive(items: &[ChatItem], busy: bool, states: &mut PriorStates, where_: Where) -> DerivedArtifacts {
    if let Some(hit) = states.cached(where_) {
        return hit;
    }
    let out = derive_tracked(items, busy);
    states.remember(where_, &out);
    out
}

/// [`derive`] with no previous writes (version 1 on the first write in `items`).
pub fn derive_fresh(items: &[ChatItem], busy: bool) -> DerivedArtifacts {
    derive_tracked(items, busy)
}

fn derive_tracked(items: &[ChatItem], busy: bool) -> DerivedArtifacts {
    let mut out: Vec<ArtifactView> = Vec::new();
    let mut pending = None;
    for item in items {
        let ChatItem::Tool(card) = item else { continue };
        // Only write/review cards can change an artifact.
        if card.name != orchestrate::WRITE_ARTIFACT && card.name != orchestrate::REQUEST_REVIEW {
            continue;
        }
        let args = args_map(&card.args);
        match card.name.as_str() {
            orchestrate::WRITE_ARTIFACT => {
                if matches!(card.result, Some((false, _))) {
                    continue;
                }
                let Some(name) = args.get("name").and_then(|v| v.as_str()) else { continue };
                if crate::agent::artifacts::validate_name(name).is_err() {
                    continue;
                }
                let content = shared_str(args, "content");
                let summary = args.get("summary").and_then(|v| v.as_str()).unwrap_or("").to_owned();
                match out.iter_mut().find(|a| a.name == name) {
                    Some(a) => {
                        a.version += 1;
                        a.content = content;
                        a.summary = summary;
                        a.review = ReviewState::None;
                    }
                    None => out.push(ArtifactView {
                        kind: ArtifactKind::from_stem(name),
                        name: name.to_owned(),
                        summary,
                        version: 1,
                        review: ReviewState::None,
                        content,
                    }),
                }
            }
            orchestrate::REQUEST_REVIEW => {
                let names: Vec<String> = args
                    .get("artifacts")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect())
                    .unwrap_or_default();
                let state = match &card.result {
                    None if busy => ReviewState::Pending,
                    None => ReviewState::None,
                    Some((true, d)) if d.contains("changes") => ReviewState::ChangesRequested,
                    Some((true, _)) => ReviewState::Approved,
                    Some((false, _)) => ReviewState::None,
                };
                for a in out.iter_mut().filter(|a| names.contains(&a.name)) {
                    a.review = state;
                }
                let message = args.get("message").and_then(|v| v.as_str()).unwrap_or("").to_owned();
                pending = (state == ReviewState::Pending).then(|| PendingReview { call_id: card.call_id.clone(), artifacts: names, message });
            }
            _ => {}
        }
    }
    DerivedArtifacts { artifacts: out, pending }
}

/// Tool arguments as an object (`write_artifact` and `request_review` both take one).
fn args_map(args: &serde_json::Value) -> &serde_json::Map<String, serde_json::Value> {
    static EMPTY: std::sync::OnceLock<serde_json::Map<String, serde_json::Value>> = std::sync::OnceLock::new();
    args.as_object().unwrap_or_else(|| EMPTY.get_or_init(serde_json::Map::new))
}

/// `args[key]` as shared text: an `Arc<str>` handed to each derivation and the panel without
/// copying the (possibly large) body again.
fn shared_str(args: &serde_json::Map<String, serde_json::Value>, key: &str) -> Arc<str> {
    match args.get(key) {
        Some(serde_json::Value::String(s)) => Arc::from(s.as_str()),
        _ => Arc::from(""),
    }
}

/// What [`derive`] found this frame.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DerivedArtifacts {
    pub artifacts: Vec<ArtifactView>,
    pub pending: Option<PendingReview>,
}

/// Which transcript a derivation was computed for. Two derivations with the same `Where`
/// (same conversation, busy flag and transcript fingerprint) yield the same artifacts.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Where {
    pub conversation: Option<uuid::Uuid>,
    pub busy: bool,
    /// Identity of the transcript (equal fingerprints hold equal items).
    pub fingerprint: u64,
}

impl Where {
    fn id(self) -> egui::Id {
        egui::Id::unique("anticraft_artifacts_derive").with((self.conversation, self.busy, self.fingerprint))
    }
}

/// Memo of the last derivation of one conversation, kept in egui temp memory so a frame that
/// changes nothing (repaint, hover, scroll) reuses the artifacts instead of walking the
/// transcript and rebuilding every view again.
#[derive(Debug, Clone, Default)]
pub struct PriorStates {
    memo: Option<(egui::Id, DerivedArtifacts)>,
}

fn states_id(conversation: Option<uuid::Uuid>) -> egui::Id {
    egui::Id::unique("anticraft_artifacts_derive").with(conversation)
}

/// Run `f` with this conversation's [`PriorStates`], kept in egui temp memory.
///
/// The states are moved out for the call and put back, not borrowed in place: `ctx.data_mut`
/// holds the context's write lock, which is not reentrant, so an `f` that touched the context
/// would deadlock the frame (the composer's `with_state` did exactly that).
pub fn with_states<R>(ctx: &egui::Context, conversation: Option<uuid::Uuid>, f: impl FnOnce(&mut PriorStates) -> R) -> R {
    let id = states_id(conversation);
    let mut states = ctx.data_mut(|d| d.remove_temp::<PriorStates>(id)).unwrap_or_default();
    let out = f(&mut states);
    ctx.data_mut(|d| d.insert_temp(id, states));
    out
}

impl PriorStates {
    /// The memoized result for `where_`, or `None` when it has to be derived.
    pub fn cached(&self, where_: Where) -> Option<DerivedArtifacts> {
        self.memo.as_ref().filter(|(id, _)| *id == where_.id()).map(|(_, d)| d.clone())
    }

    /// Remember `derived` as the result for `where_`.
    pub fn remember(&mut self, where_: Where, derived: &DerivedArtifacts) {
        self.memo = Some((where_.id(), derived.clone()));
    }
}

fn panel_id() -> egui::Id {
    egui::Id::unique("anticraft_artifacts_panel")
}
fn answer_id() -> egui::Id {
    egui::Id::unique("anticraft_review_answer")
}

/// Panel state for this conversation (`conversation` keys it, so switching chats resets it).
pub fn load(ctx: &egui::Context, conversation: Option<uuid::Uuid>) -> ArtifactsPanel {
    ctx.data_mut(|d| d.get_temp::<ArtifactsPanel>(panel_id().with(conversation))).unwrap_or_default()
}

pub fn store(ctx: &egui::Context, conversation: Option<uuid::Uuid>, panel: ArtifactsPanel) {
    ctx.data_mut(|d| d.insert_temp(panel_id().with(conversation), panel));
}

/// Post the user's answer to the review `call_id`.
pub fn post_review_answer(ctx: &egui::Context, call_id: &str, outcome: ReviewOutcome) {
    ctx.data_mut(|d| d.insert_temp(answer_id(), Some((call_id.to_owned(), outcome))));
}

/// Take the answer posted by [`post_review_answer`] (at most one per call).
pub fn take_review_answer(ctx: &egui::Context) -> Option<(String, ReviewOutcome)> {
    ctx.data_mut(|d| d.remove_temp::<Option<(String, ReviewOutcome)>>(answer_id())).flatten()
}

/// Render `[/]` (in progress) checklist items, which CommonMark does not know.
pub fn checklist_markdown(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 16);
    for line in text.lines() {
        let trimmed = line.trim_start();
        let indent = &line[..line.len() - trimmed.len()];
        match trimmed.strip_prefix("- [/] ").or_else(|| trimmed.strip_prefix("* [/] ")) {
            // Half-filled circle in front of the item marks it as in progress.
            Some(rest) => out.push_str(&format!("{indent}- [ ] \u{25d0} {rest}")),
            None => out.push_str(line),
        }
        out.push('\n');
    }
    out
}

/// The Markdown body of an artifact in the forms the panel needs, computed once per content:
/// [`checklist_markdown`] when it differs from the content itself, otherwise the content.
#[derive(Debug, Clone, PartialEq)]
pub struct Checklist {
    pub rendered: String,
    /// The content had no `[/]` item, so `rendered` is just the content.
    pub plain: bool,
}

/// [`checklist_markdown`] only when the text has an item it would rewrite.
pub fn checklist_for(content: &str) -> Checklist {
    if content.contains("[/]") {
        Checklist { rendered: checklist_markdown(content), plain: false }
    } else {
        Checklist { rendered: String::new(), plain: true }
    }
}

/// FNV-1a over bytes: cheap fingerprint for cache keys of long text.
pub fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    h
}

/// One artifact's [`Checklist`], kept in panel state so a later frame that draws the same
/// content again does not re-scan it and allocate another copy.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ChecklistCache {
    name: Option<String>,
    version: u32,
    fingerprint: u64,
    value: Option<Checklist>,
}

impl ChecklistCache {
    fn get(&mut self, name: &str, version: u32, content: &str) -> &Checklist {
        let fp = fnv1a(content.as_bytes());
        if self.name.as_deref() != Some(name) || self.version != version || self.fingerprint != fp || self.value.is_none() {
            self.value = Some(checklist_for(content));
            self.name = Some(name.to_owned());
            self.version = version;
            self.fingerprint = fp;
        }
        self.value.as_ref().expect("just filled")
    }
}

fn kind_icon(k: ArtifactKind) -> &'static str {
    match k {
        ArtifactKind::TaskList => icons::CHECK_CIRCLE,
        ArtifactKind::ImplementationPlan => icons::NOTE_PENCIL,
        ArtifactKind::Walkthrough => icons::CHATS_CIRCLE,
        ArtifactKind::Other => icons::FILE_TEXT,
    }
}

/// Short badge: glyph, label and color. Pending (amber eye) and changes requested (blue
/// pencil) differ in hue and glyph.
fn review_badge(r: ReviewState) -> Option<(&'static str, &'static str, egui::Color32)> {
    match r {
        ReviewState::None => None,
        ReviewState::Pending => Some((icons::EYE, "Needs review", theme::WARNING)),
        ReviewState::Approved => Some((icons::CHECK, "Approved", theme::SUCCESS)),
        ReviewState::ChangesRequested => Some((icons::PENCIL_SIMPLE, "Changes", theme::LINK)),
    }
}

/// Banner title for the pending review: "<Title> ready for review" for one artifact, "N
/// artifacts ready for review" for several.
pub fn review_title(panel: &ArtifactsPanel) -> String {
    let named: Vec<&ArtifactView> = panel.review_artifacts.iter().filter_map(|n| panel.artifacts.iter().find(|a| &a.name == n)).collect();
    match named.as_slice() {
        [] => "Ready for review".to_owned(),
        [a] => format!("{} ready for review", a.title()),
        many => format!("{} artifacts ready for review", many.len()),
    }
}

/// "Request changes (N)" for a pending review, shared by the composer banner and the panel's
/// review bar: disabled until a comment exists.
/// Every pending comment is sent; when they span several artifacts the label says so.
pub fn request_changes_button(ui: &mut Ui, panel: &mut ArtifactsPanel) -> Option<PanelAction> {
    let n = panel.comments.len();
    let m = comment_artifacts(panel);
    let label = if m > 1 { format!("Request changes ({n} on {m} artifacts)") } else { format!("Request changes ({n})") };
    let r = widgets::ghost_button(ui, None, &label, theme::FG, n > 0);
    if n == 0 {
        r.on_disabled_hover_text("Add a comment first");
        return None;
    }
    let tip = format!("Send {n} comment{} on {m} artifact{} to the agent", if n == 1 { "" } else { "s" }, if m == 1 { "" } else { "s" });
    r.on_hover_text(tip).clicked().then(|| PanelAction::RequestChanges(std::mem::take(&mut panel.comments)))
}

/// Number of distinct artifacts that have pending comments.
fn comment_artifacts(panel: &ArtifactsPanel) -> usize {
    let mut names: Vec<&str> = panel.comments.iter().map(|c| c.artifact.as_str()).collect();
    names.sort_unstable();
    names.dedup();
    names.len()
}

/// Row height of [`artifact_row`].
const ROW_H: f32 = 44.0;

/// One 44px artifact row: icon, title and version on top; summary and the review badge
/// below, both lines ending at the same right edge.
fn artifact_row(ui: &mut Ui, a: &ArtifactView, selected: bool, comments: usize) -> bool {
    let (rect, r) = ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_H), Sense::click());
    let p = ui.painter();
    if selected {
        p.rect_filled(rect, 6, theme::ROW_SELECTED);
        let bar = Rect::from_min_max(egui::pos2(rect.left(), rect.top() + 8.0), egui::pos2(rect.left() + 2.0, rect.bottom() - 8.0));
        p.rect_filled(bar, 1, theme::PRIMARY);
    } else if r.hovered() {
        p.rect_filled(rect, 6, theme::SECONDARY.gamma_multiply(0.35));
    }
    let x = rect.left() + 10.0;
    p.text(egui::pos2(x + 7.0, rect.center().y), Align2::CENTER_CENTER, kind_icon(a.kind), theme::icon(16.0), theme::FG_SECONDARY);
    let edge = rect.right() - 10.0;
    let top_y = rect.top() + 14.0;
    let low_y = rect.top() + 32.0;
    let vr = p.text(egui::pos2(edge, top_y), Align2::RIGHT_CENTER, format!("v{}", a.version), theme::font(12.0), theme::FG_MUTED);
    let tx = x + 24.0;
    widgets::paint_truncated(ui, Rect::from_min_max(egui::pos2(tx, top_y - 10.0), egui::pos2(vr.left() - 8.0, top_y + 10.0)), &a.title(), theme::medium(13.0), theme::FG);
    let mut sub_right = edge;
    if let Some((glyph, label, color)) = review_badge(a.review) {
        let mut job = egui::text::LayoutJob::default();
        job.append(glyph, 0.0, egui::TextFormat::simple(theme::icon(12.0), color));
        job.append(label, 4.0, egui::TextFormat::simple(theme::font(12.0), color));
        let g = ui.painter().layout_job(job);
        let tag = Rect::from_min_size(egui::pos2(edge - g.size().x - 12.0, low_y - 9.0), Vec2::new(g.size().x + 12.0, 18.0));
        let p = ui.painter();
        p.rect_filled(tag, 9, color.gamma_multiply(0.14));
        p.galley(tag.center() - g.size() / 2.0, g, color);
        sub_right = tag.left() - 8.0;
    }
    let mut sub = if a.summary.is_empty() { format!("{}.md", a.name) } else { a.summary.clone() };
    if comments > 0 {
        sub = format!("{sub}  \u{b7}  {comments} comment{}", if comments == 1 { "" } else { "s" });
    }
    widgets::paint_truncated(ui, Rect::from_min_max(egui::pos2(tx, low_y - 9.0), egui::pos2(sub_right, low_y + 9.0)), &sub, theme::font(12.0), if selected { theme::FG_SECONDARY } else { theme::FG_MUTED });
    r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
}

/// One pending comment as a small card; the remove button sits at its top-right corner.
fn comment_card(ui: &mut Ui, text: &str, id: egui::Id) -> bool {
    let w = ui.available_width();
    let r = egui::Frame::new()
        .fill(theme::CARD)
        .stroke(Stroke::new(1.0, theme::CARD_BORDER))
        .corner_radius(6)
        .inner_margin(egui::Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.set_width(w - 20.0);
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.label(egui::RichText::new(icons::CHATS_CIRCLE).font(theme::icon(13.0)).color(theme::FG_MUTED));
                let tw = (ui.available_width() - 24.0).max(40.0);
                ui.allocate_ui_with_layout(Vec2::new(tw, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                    ui.set_width(tw);
                    ui.add(egui::Label::new(egui::RichText::new(text).font(theme::font(12.5)).color(theme::FG_SECONDARY)).wrap());
                });
            });
        })
        .response;
    let x = Rect::from_min_size(egui::pos2(r.rect.right() - 26.0, r.rect.top() + 5.0), Vec2::splat(22.0));
    widgets::overlay_icon(ui, x, id, icons::X, "Remove comment", true).clicked()
}

/// Comment box, the pending comments of the selected artifact and the review bar.
fn footer(ui: &mut Ui, panel: &mut ArtifactsPanel, action: &mut PanelAction) {
    ui.spacing_mut().item_spacing = Vec2::new(8.0, 6.0);
    let selected = panel.selected.clone();
    if let Some(name) = &selected {
        let mut remove = None;
        let mine = panel.comments.iter().filter(|c| &c.artifact == name).count();
        if mine > 0 {
            // Bounded list so the Markdown body keeps its room.
            egui::ScrollArea::vertical().id_salt(("comment_list", name)).max_height(120.0).auto_shrink([false, true]).show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 4.0;
                for (i, c) in panel.comments.iter().enumerate().filter(|(_, c)| &c.artifact == name) {
                    if comment_card(ui, &c.text, ui.make_persistent_id(("remove_comment", i))) {
                        remove = Some(i);
                    }
                }
            });
        }
        if let Some(i) = remove {
            panel.comments.remove(i);
        }
        let others = panel.comments.iter().filter(|c| &c.artifact != name).count();
        if others > 0 {
            let s = if others == 1 { "" } else { "s" };
            ui.label(egui::RichText::new(format!("+{others} comment{s} on other artifacts")).font(theme::font(12.0)).color(theme::FG_MUTED));
        }
        let edit_id = ui.make_persistent_id(("artifact_comment", name));
        // Enter adds the comment; Shift+Enter inserts a newline.
        let enter = ui.memory(|m| m.has_focus(edit_id)) && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
        let mut add = enter;
        let field = egui::Frame::new()
            .fill(theme::FIELD_BG)
            .stroke(Stroke::new(1.0, theme::CARD_BORDER))
            .corner_radius(6)
            .inner_margin(egui::Margin { left: 8, right: 30, top: 6, bottom: 6 })
            .show(ui, |ui| {
                ui.add(
                    egui::TextEdit::multiline(&mut panel.draft)
                        .id(edit_id)
                        .frame(egui::Frame::NONE)
                        .font(theme::font(12.5))
                        .hint_text(egui::RichText::new("Add a comment\u{2026}").color(theme::PLACEHOLDER))
                        .desired_rows(2)
                        .desired_width(f32::INFINITY),
                );
            })
            .response;
        // Add button inside the field, bottom-right.
        let br = Rect::from_min_size(field.rect.right_bottom() - Vec2::new(26.0, 26.0), Vec2::splat(22.0));
        if widgets::overlay_icon(ui, br, ui.make_persistent_id(("add_comment", name)), icons::ARROW_UP, "Add comment (Enter)", !panel.draft.trim().is_empty()).clicked() {
            add = true;
        }
        if add {
            panel.add_comment();
        }
        if panel.pending_review.is_none() && !panel.comments.is_empty() {
            let n = panel.comments.len();
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if widgets::ghost_button(ui, Some(icons::ARROW_UP), &format!("Send feedback ({n})"), theme::FG, panel.idle).clicked() {
                    *action = PanelAction::RequestChanges(std::mem::take(&mut panel.comments));
                }
            });
        }
    }
    if panel.pending_review.is_some() {
        // No separator above: the review frame is its own divider.
        ui.add_space(8.0);
        widgets::review_frame(ui, |ui| {
            ui.spacing_mut().item_spacing = Vec2::new(8.0, 8.0);
            let msg = if panel.review_message.trim().is_empty() { "The agent is waiting for your review." } else { panel.review_message.trim() };
            ui.horizontal_top(|ui| {
                ui.label(egui::RichText::new(icons::EYE).font(theme::icon(15.0)).color(theme::WARNING));
                let tw = ui.available_width().max(40.0);
                ui.allocate_ui_with_layout(Vec2::new(tw, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                    ui.set_width(tw);
                    ui.add(egui::Label::new(egui::RichText::new(msg).font(theme::font(12.5)).color(theme::FG)).wrap());
                });
            });
            // Same order as the composer banner: Proceed is the rightmost button.
            ui.allocate_ui_with_layout(Vec2::new(ui.available_width(), 28.0), egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if widgets::primary_button(ui, "Proceed", true).clicked() {
                    *action = PanelAction::Approve;
                }
                if let Some(a) = request_changes_button(ui, panel) {
                    *action = a;
                }
            });
        });
    }
}

/// Artifact count as a small pill next to the panel title.
fn count_pill(ui: &mut Ui, n: usize) {
    let g = ui.painter().layout_no_wrap(n.to_string(), theme::font(11.5), theme::FG_SECONDARY);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(g.size().x.max(8.0) + 12.0, 18.0), Sense::hover());
    let p = ui.painter();
    p.rect(rect, 9, theme::SECONDARY.gamma_multiply(0.6), Stroke::new(1.0, theme::CARD_BORDER), egui::StrokeKind::Inside);
    p.galley(rect.center() - g.size() / 2.0, g, theme::FG_SECONDARY);
}

/// Draw the panel into `ui` (the inside of a right side panel).
pub fn show(ui: &mut Ui, panel: &mut ArtifactsPanel, cache: &mut CommonMarkCache) -> PanelAction {
    let mut action = PanelAction::None;
    ui.spacing_mut().item_spacing = Vec2::new(8.0, 4.0);
    ui.horizontal(|ui| {
        ui.set_height(26.0);
        ui.label(egui::RichText::new("Artifacts").font(theme::medium(13.0)).color(theme::FG));
        count_pill(ui, panel.artifacts.len());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if widgets::icon_button(ui, icons::X, 24.0, "Close", true).clicked() {
                action = PanelAction::Close;
            }
        });
    });
    ui.add_space(2.0);
    let r = ui.available_rect_before_wrap();
    ui.painter().hline(r.x_range(), r.top(), Stroke::new(1.0, theme::CARD_BORDER));
    ui.add_space(8.0);
    if panel.artifacts.is_empty() {
        ui.add_space(24.0);
        ui.vertical_centered(|ui| {
            ui.set_max_width(260.0);
            ui.label(egui::RichText::new("No artifacts yet").font(theme::medium(13.0)).color(theme::FG_SECONDARY));
            ui.add(egui::Label::new(egui::RichText::new("In Planning mode the agent writes its task list, plan and walkthrough here.").font(theme::font(12.5)).color(theme::FG_MUTED)).wrap());
        });
        return action;
    }
    let mut pick = None;
    // At most 4.5 rows visible (3.5 when the footer holds comments or the review bar) so the
    // body always keeps room.
    let busy_footer = !panel.comments.is_empty() || panel.pending_review.is_some();
    let rows = if busy_footer { 3.5 } else { 4.5 };
    egui::ScrollArea::vertical().id_salt("artifact_list").max_height(rows * ROW_H).auto_shrink([false, true]).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        for a in &panel.artifacts {
            let sel = panel.selected.as_deref() == Some(a.name.as_str());
            if artifact_row(ui, a, sel, panel.comment_count(&a.name)) {
                pick = Some(a.name.clone());
            }
        }
    });
    if let Some(n) = pick {
        if panel.selected.as_ref() != Some(&n) {
            panel.draft.clear();
        }
        panel.selected = Some(n);
    }
    ui.add_space(4.0);
    let r = ui.available_rect_before_wrap();
    ui.painter().hline(r.x_range(), r.top(), Stroke::new(1.0, theme::CARD_BORDER));
    ui.add_space(6.0);

    egui::Panel::bottom("artifact_footer")
        .show_separator_line(false)
        .frame(egui::Frame::NONE.inner_margin(egui::Margin { left: 0, right: 0, top: 8, bottom: 0 }))
        .show(ui, |ui| footer(ui, panel, &mut action));

    if let Some(a) = panel.current() {
        let (name, version) = (a.name.clone(), a.version);
        let content = Arc::clone(&a.content);
        // One pass over the body per content: a frame that redraws it reads the cached form.
        let text = panel.checklist.get(&name, version, &content);
        let body: &str = if text.plain { &content } else { &text.rendered };
        egui::ScrollArea::vertical().id_salt(("artifact_body", &name)).auto_shrink([false, false]).show(ui, |ui| {
            // Same 10px gutter as the artifact rows (icon at left + 10, text edge at right - 10).
            egui::Frame::NONE.inner_margin(egui::Margin::symmetric(10, 0)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                markdown::render_with_id(ui, cache, ui.make_persistent_id(("artifact", &name)), body);
            });
        });
    }
    action
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::ToolCard;
    use serde_json::json;

    fn tool(id: &str, name: &str, args: serde_json::Value, result: Option<(bool, String)>) -> ChatItem {
        ChatItem::Tool(ToolCard { call_id: id.into(), name: name.into(), args, result })
    }

    #[test]
    fn upsert_and_comment() {
        let mut p = ArtifactsPanel::default();
        let v = |c: &str| ArtifactView { name: "task".into(), kind: ArtifactKind::TaskList, summary: String::new(), version: 1, review: ReviewState::None, content: Arc::from(c) };
        p.upsert(v("a"));
        p.upsert(v("b"));
        assert_eq!(p.artifacts.len(), 1);
        assert_eq!(&*p.artifacts[0].content, "b");
        p.draft = " fix step 2 ".into();
        p.add_comment();
        assert_eq!(p.comments[0].text, "fix step 2");
        assert_eq!(p.comment_count("task"), 1);
    }

    #[test]
    fn derive_versions_and_pending_review() {
        let items = vec![
            tool("1", orchestrate::WRITE_ARTIFACT, json!({"name": "implementation_plan", "content": "v1"}), Some((true, "ok".into()))),
            tool("2", orchestrate::WRITE_ARTIFACT, json!({"name": "Bad Name", "content": "x"}), Some((true, "ok".into()))),
            tool("3", orchestrate::WRITE_ARTIFACT, json!({"name": "task", "content": "t"}), Some((false, "err".into()))),
            tool("4", orchestrate::WRITE_ARTIFACT, json!({"name": "implementation_plan", "content": "v2", "summary": "s"}), None),
            tool("5", orchestrate::REQUEST_REVIEW, json!({"artifacts": ["implementation_plan"], "message": "ok?"}), None),
        ];
        let d = derive_fresh(&items, true);
        let (a, p) = (d.artifacts, d.pending);
        assert_eq!(a.len(), 1);
        assert_eq!((a[0].version, &*a[0].content, a[0].summary.as_str()), (2, "v2", "s"));
        assert_eq!(a[0].kind, ArtifactKind::ImplementationPlan);
        assert_eq!(a[0].review, ReviewState::Pending);
        assert_eq!(p.as_ref().map(|p| p.call_id.as_str()), Some("5"));
        // Not busy: an unanswered review is stale, not pending.
        let d = derive_fresh(&items, false);
        assert_eq!(d.artifacts[0].review, ReviewState::None);
        assert!(d.pending.is_none());
    }

    #[test]
    fn sync_opens_once_per_review() {
        let mut p = ArtifactsPanel::default();
        let view = |n: &str| ArtifactView { name: n.into(), kind: ArtifactKind::from_stem(n), summary: String::new(), version: 1, review: ReviewState::None, content: Arc::from("") };
        let review = PendingReview { call_id: "r1".into(), artifacts: vec!["implementation_plan".into()], message: "m".into() };
        p.sync(vec![view("implementation_plan"), view("task")], Some(&review));
        assert!(p.open);
        assert_eq!(p.selected.as_deref(), Some("implementation_plan"));
        p.open = false;
        p.sync(vec![view("implementation_plan"), view("task")], Some(&review));
        assert!(!p.open, "user closed it; the same review does not reopen it");
        p.sync(vec![view("implementation_plan")], None);
        assert!(p.pending_review.is_none());
        p.answered = Some("r1".into());
        p.sync(vec![], Some(&review));
        assert!(p.pending_review.is_none(), "answered review is not shown again");
    }

    #[test]
    fn review_answer_round_trip() {
        let ctx = egui::Context::default();
        assert!(take_review_answer(&ctx).is_none());
        post_review_answer(&ctx, "r1", ReviewOutcome::Approved);
        assert_eq!(take_review_answer(&ctx), Some(("r1".into(), ReviewOutcome::Approved)));
        assert!(take_review_answer(&ctx).is_none());
    }

    #[test]
    fn in_progress_items_render() {
        let md = checklist_markdown("- [x] a\n  - [/] b\n- [ ] c");
        assert_eq!(md, "- [x] a\n  - [ ] \u{25d0} b\n- [ ] c\n");
        // The body cache hands back the rewritten form only when an item needs it; a plain
        // body is drawn as is, and the cached entry is reused until the content changes.
        let c = checklist_for("- [x] a");
        assert!(c.plain && c.rendered.is_empty());
        let mut cache = ChecklistCache::default();
        let first = cache.get("task", 1, "- [/] b").clone();
        assert!(!first.plain);
        assert_eq!(first.rendered, "- [ ] \u{25d0} b\n");
        let again = cache.get("task", 1, "- [/] b").clone();
        assert_eq!(again, first, "same content: the same cached form");
        let changed = cache.get("task", 1, "- [x] b").clone();
        assert!(changed.plain, "new content: invalidated");
    }

    /// The memo answers a repeated frame without walking the transcript, and a changed
    /// transcript (other fingerprint) derives again.
    #[test]
    fn derive_memo_follows_the_transcript() {
        let items = vec![
            tool("1", orchestrate::WRITE_ARTIFACT, json!({"name": "task", "content": "v1"}), Some((true, "ok".into()))),
            tool("2", orchestrate::WRITE_ARTIFACT, json!({"name": "task", "content": "v2"}), Some((true, "ok".into()))),
            ChatItem::User("next".into()),
        ];
        let mut st = PriorStates::default();
        let w = |fp: u64| Where { conversation: None, busy: false, fingerprint: fp };
        let all = derive(&items, false, &mut st, w(1));
        assert_eq!(all.artifacts[0].version, 2);
        assert_eq!(&*all.artifacts[0].content, "v2");
        assert_eq!(st.cached(w(1)), Some(all.clone()), "same transcript: memo hit");
        assert_eq!(derive(&items, false, &mut st, w(1)), all);
        assert!(st.cached(w(2)).is_none(), "a different transcript is derived again");
        // Busy changes what a request_review without a result means, so it is part of the key.
        assert!(st.cached(Where { conversation: None, busy: true, fingerprint: 1 }).is_none());
    }

    /// A derived body does not copy the tool argument, and a repeated derivation (memo hit)
    /// hands the very same buffer to the panel.
    #[test]
    fn artifact_bodies_are_shared() {
        let items = vec![tool("1", orchestrate::WRITE_ARTIFACT, json!({"name": "task", "content": "body"}), Some((true, "ok".into())))];
        let d = derive_fresh(&items, false);
        assert_eq!(&*d.artifacts[0].content, "body");
        let mut st = PriorStates::default();
        let w = Where { conversation: None, busy: false, fingerprint: 3 };
        let a = derive(&items, false, &mut st, w);
        let b = derive(&items, false, &mut st, w);
        assert!(Arc::ptr_eq(&a.artifacts[0].content, &b.artifacts[0].content), "memo hit shares the body buffer");
    }

    /// `derive_fresh` never uses history: versions count the writes in `items` only.
    #[test]
    fn fresh_derive_counts_from_the_slice() {
        let items = vec![tool("1", orchestrate::WRITE_ARTIFACT, json!({"name": "plan", "content": "x"}), Some((true, "ok".into())))];
        let d = derive_fresh(&items, false);
        assert_eq!(d.artifacts[0].version, 1);
    }

    /// `with_states` holds no context lock while `f` runs, and keeps what `f` stored.
    #[test]
    fn with_states_lends_the_states_without_holding_the_context_lock() {
        let ctx = egui::Context::default();
        let conv = Some(uuid::Uuid::new_v4());
        with_states(&ctx, conv, |states| {
            // Borrowed in place, this nested write lock deadlocked (egui panics after 10 s).
            ctx.data_mut(|_| ());
            states.memo = Some((egui::Id::unique("t"), DerivedArtifacts::default()));
        });
        let kept = with_states(&ctx, conv, |states| states.memo.as_ref().map(|(id, _)| *id));
        assert_eq!(kept, Some(egui::Id::unique("t")));
    }
}
