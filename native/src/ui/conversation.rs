//! Conversation screen (DESIGN.md 2.5): transcript column with the docked composer.
//!
//! Layout, top to bottom: optional banner, "Load older messages", the items (user bubbles,
//! turn headers, thoughts, tool cards, assistant markdown), the inline confirmation card at
//! the end of the stream, then the composer docked 8px above the bottom edge with a 24px
//! fade. The view sticks to the bottom unless the user scrolled up; then a "Jump to latest"
//! pill appears above the composer.

use std::collections::HashSet;

use egui::{Align2, Color32, Rect, Sense, Stroke, Ui, Vec2};
use egui_commonmark::CommonMarkCache;

use super::artifacts_panel::{self, ArtifactsPanel, PanelAction};
use super::taskgroups::{self, Derived};
use super::{composer, confirm, format, icons, markdown, progress_panel, theme, widgets, Banner, ChatItem, ToolCard, UiAction, UiState, ITEM_PAGE};
use crate::agent::artifacts::comments_to_feedback;
use crate::agent::orchestrate::{self, ReviewOutcome};
use crate::app::AppCommand;
use crate::auth::external::AccountProvider;


/// Minimum gutter on each side of the centered reading column.
const COLUMN_MIN_GUTTER: f32 = 24.0;
/// Reading column width: the shared 768 token (DESIGN.md 2.5), same as Home and History.
const COLUMN_MAX: f32 = theme::COLUMN_WIDTH;
/// Narrowest column kept next to a side panel (the panel shrinks first).
const COLUMN_BESIDE_PANEL_MIN: f32 = 480.0;
/// Sticky header strip height.
const HEADER_H: f32 = 33.0;
/// Gap between the panel top and the header strip (border at reference y 84).
const HEADER_TOP: f32 = 12.0;

/// Left-anchored reading column inside the main panel.
#[derive(Clone, Copy)]
struct Col {
    pad: f32,
    w: f32,
}

impl Col {
    /// Centered column: at most [`COLUMN_MAX`] wide, at least [`COLUMN_MIN_GUTTER`] per side.
    fn for_width(avail: f32) -> Self {
        let w = (avail - 2.0 * COLUMN_MIN_GUTTER).min(COLUMN_MAX).max(200.0_f32.min(avail));
        Col { pad: ((avail - w) / 2.0).max(0.0), w }
    }

    /// Run `add` in a child ui at the column's x range.
    fn show<R>(self, ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            ui.add_space(self.pad);
            ui.vertical(|ui| {
                ui.set_width(self.w);
                ui.spacing_mut().item_spacing = Vec2::new(8.0, 6.0);
                add(ui)
            })
            .inner
        })
        .inner
    }
}

/// Conversation screen: sticky header strip, transcript column plus the docked composer
/// and inline confirmation.
pub fn show(ui: &mut Ui, state: &mut UiState, cache: &mut CommonMarkCache, commands: &mut Vec<UiAction>) {
    let mut jump = false;
    let pinned_id = ui.make_persistent_id("transcript_pinned");
    let ctx = ui.ctx().clone();

    // Task groups are derived from the transcript every frame (one pass over the items; no
    // Markdown parsing). Artifacts are derived once per transcript state and share their
    // bodies (`Arc<str>`) instead of copying them; an unchanged transcript reuses the memo.
    let mut groups = taskgroups::derive(&state.items, state.busy, state.pending_confirm.is_some());
    // The agent's own status updates (e.g. `task_complete` closing a group) refine the
    // transcript-derived cards.
    taskgroups::apply_updates(&mut groups, &state.task_updates);
    let where_ = artifacts_panel::Where { conversation: state.current, busy: state.busy, fingerprint: transcript_key(&state.items) };
    let mut apanel = artifacts_panel::load(&ctx, state.current);
    let derived = artifacts_panel::with_states(&ctx, state.current, |states| {
        artifacts_panel::derive(&state.items, state.busy, states, where_)
    });
    let (artifacts, review) = (derived.artifacts, derived.pending);
    apanel.idle = !state.busy;
    apanel.sync(artifacts, review.as_ref());

    // The Progress panel, opened from the toolbar, at the window edge under its toggle. It comes
    // first, so an open artifacts panel goes beside it rather than under the toggle.
    if state.progress_open {
        let avail = ui.available_width();
        let w = progress_panel::WIDTH.min((avail - COLUMN_BESIDE_PANEL_MIN).max(220.0));
        let closed = egui::Panel::right("progress_panel")
            .exact_size(w)
            .resizable(false)
            .show_separator_line(false)
            .frame(egui::Frame::NONE.fill(theme::SIDEBAR).inner_margin(egui::Margin::same(12)))
            .show(ui, |ui| {
                let r = ui.max_rect().expand(12.0);
                ui.painter().vline(r.left(), r.y_range(), Stroke::new(1.0, theme::BORDER));
                progress_panel::show(ui, state, &groups)
            })
            .inner;
        if closed {
            state.progress_open = false;
        }
    }

    // Artifacts side panel on the right; the reading column centers in what is left.
    if apanel.open {
        let avail = ui.available_width();
        // Shrink the panel before the column; below that the panel keeps its 260 minimum.
        let w = (avail * 0.45).clamp(260.0, artifacts_panel::PANEL_WIDTH).min((avail - COLUMN_BESIDE_PANEL_MIN).max(260.0));
        let act = egui::Panel::right("artifacts_panel")
            .exact_size(w)
            .resizable(false)
            .show_separator_line(false)
            .frame(egui::Frame::NONE.fill(theme::SIDEBAR).inner_margin(egui::Margin::same(12)))
            .show(ui, |ui| {
                let r = ui.max_rect().expand(12.0);
                ui.painter().vline(r.left(), r.y_range(), Stroke::new(1.0, theme::BORDER));
                artifacts_panel::show(ui, &mut apanel, cache)
            })
            .inner;
        panel_action(act, &mut apanel, state, &ctx, commands);
    }

    // The column depends on the room the side panels leave, never on what the turn is doing:
    // a turn's steps are in the Progress panel, so running tools leave the column as it was.
    let panel = ui.available_rect_before_wrap();
    let col = Col::for_width(panel.width());

    // Docked composer at the bottom, 8 from the edge, in the reading column.
    let dock = egui::Panel::bottom("composer_dock")
        .show_separator_line(false)
        .frame(egui::Frame::NONE.fill(theme::BG).inner_margin(egui::Margin { left: 0, right: 0, top: 4, bottom: 8 }))
        .show(ui, |ui| {
            // Place the box on the exact column rect (reference x 314..1013), independent of
            // the panel's own cursor.
            let top = ui.cursor().top();
            let rect = Rect::from_min_size(egui::pos2(ui.max_rect().left() + col.pad, top), Vec2::new(col.w, 400.0));
            ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
                ui.set_width(rect.width());
                // The error that ended a turn sits on the composer, in view. At the top of the
                // transcript a long turn had scrolled it away before it was ever seen.
                if let Some(b) = state.banner.clone() {
                    banner(ui, state, &b, commands);
                    ui.add_space(6.0);
                }
                // Bypass Mode's band sits above everything else that reaches the composer: it
                // describes what happens to the message about to be typed.
                if super::bypass::band(ui, state) {
                    ui.add_space(6.0);
                }
                if apanel.pending_review.is_some() {
                    review_banner(ui, &mut apanel, state, &ctx, commands);
                    ui.add_space(6.0);
                }
                composer::show_docked(ui, state, commands);
            });
        });
    let dock_top = dock.response.rect.top();
    // A bottom panel is laid out at the height it had last frame, so a composer that just
    // gained or lost a line of text would be drawn cut off at the window edge (or floating
    // above it) until the next repaint. When the height moved, this frame is laid out again.
    let dock_h = dock.response.rect.height();
    let dock_h_id = ui.make_persistent_id("composer_dock_height");
    if ui.data(|d| d.get_temp::<f32>(dock_h_id)).is_none_or(|h| (h - dock_h).abs() > 0.5) {
        ui.data_mut(|d| d.insert_temp(dock_h_id, dock_h));
        ui.ctx().request_discard("the composer changed height");
    }

    // Sticky header strip: conversation title on a card-colored band (reference y 85..118).
    egui::Panel::top("conversation_header")
        .show_separator_line(false)
        .exact_size(HEADER_TOP + HEADER_H + 12.0)
        .frame(egui::Frame::NONE.fill(theme::BG))
        .show(ui, |ui| {
            let m = ui.max_rect();
            // The strip spans the reading column only (reference x 313..1013).
            let left = m.left() + col.pad;
            let strip = Rect::from_min_size(egui::pos2(left, m.top() + HEADER_TOP), Vec2::new(col.w, HEADER_H));
            // Bottom border only; no fill, so the strip does not read as another card.
            ui.painter().hline(strip.x_range(), strip.bottom() - 0.5, Stroke::new(1.0, theme::CARD_BORDER));
            let title = if state.current_title.is_empty() { "Untitled Conversation" } else { state.current_title.as_str() };
            // Artifacts toggle at the right end of the strip, with the artifact count.
            let n = apanel.artifacts.len();
            let reviewing = apanel.pending_review.is_some();
            let btn = Rect::from_center_size(egui::pos2(strip.right() - 12.0, strip.center().y), Vec2::splat(24.0));
            let tip = if reviewing { "Review pending" } else if apanel.open { "Hide artifacts" } else { "Show artifacts" };
            if widgets::overlay_icon(ui, btn, ui.scope_id().with("artifacts_toggle"), icons::FILE_TEXT, tip, n > 0 || apanel.open).clicked() {
                apanel.open = !apanel.open;
            }
            if reviewing {
                ui.painter().circle_filled(btn.center() + Vec2::new(6.0, -6.0), 3.0, theme::WARNING);
            }
            let mut right = btn.left() - 4.0;
            if n > 0 {
                let c = if reviewing { theme::WARNING } else { theme::FG_MUTED };
                let r = ui.painter().text(egui::pos2(right, strip.center().y), Align2::RIGHT_CENTER, n.to_string(), theme::font(12.0), c);
                right = r.left() - 12.0;
            }
            if state.busy {
                let r = ui.painter().text(egui::pos2(right, strip.center().y), Align2::RIGHT_CENTER, "Working...", theme::font(12.0), theme::FG_MUTED);
                let sp = Rect::from_center_size(egui::pos2(r.left() - 10.0, strip.center().y), Vec2::splat(12.0));
                egui::Spinner::new().size(12.0).color(theme::FG_MUTED).paint_at(ui, sp);
                right = sp.left() - 12.0;
            }
            let tr = Rect::from_min_max(strip.min, egui::pos2(right, strip.bottom()));
            widgets::paint_truncated(ui, tr, title, theme::medium(13.0), theme::FG);
        });

    let mut scroll = egui::ScrollArea::vertical().id_salt("transcript").stick_to_bottom(true).auto_shrink([false, false]);
    if ui.data_mut(|d| d.remove_temp::<bool>(pinned_id.with("jump"))).unwrap_or(false) {
        scroll = scroll.vertical_scroll_offset(f32::MAX);
    }
    let out = scroll.show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 6.0;
        transcript(ui, state, cache, commands, col, &groups, &mut apanel);
        if state.pending_confirm.is_some() {
            ui.add_space(8.0);
            col.show(ui, |ui| confirm::inline(ui, state));
        }
        ui.add_space(theme::SECTION_GAP);
    });

    let r = out.inner_rect;
    let max = (out.content_size.y - r.height()).max(0.0);
    let offset = out.state.clamped_offset().y;
    let at_bottom = offset >= max - 4.0;
    // Back at the bottom: drop the extra pages loaded with "Load older messages".
    if at_bottom && state.visible_items > ITEM_PAGE && ui.ctx().dragged_id().is_none() {
        state.visible_items = ITEM_PAGE;
    }

    // 24px bg fade above the composer.
    let fade = Rect::from_min_max(egui::pos2(r.left(), dock_top - 24.0), egui::pos2(r.right() - 8.0, dock_top));
    paint_fade(ui, fade);

    // Header separator only when scrolled.
    if offset > 1.0 {
        ui.painter().line_segment([r.left_top(), r.right_top()], Stroke::new(1.0, theme::BORDER));
    }

    artifacts_panel::store(&ctx, state.current, apanel);

    // "Jump to latest" pill when scrolled up.
    if !at_bottom && max > 0.0 {
        let label = "Jump to latest";
        let g = ui.painter().layout_no_wrap(label.to_owned(), theme::font(12.0), theme::FG);
        let size = Vec2::new(g.size().x + 40.0, 28.0);
        let rect = Rect::from_center_size(egui::pos2(r.center().x, dock_top - 22.0), size);
        let resp = ui.interact(rect, ui.scope_id().with("jump_latest"), Sense::click());
        let p = ui.painter();
        p.rect(rect, 14, if resp.hovered() { theme::SECONDARY } else { theme::CARD }, Stroke::new(1.0, theme::CARD_BORDER), egui::StrokeKind::Inside);
        p.text(egui::pos2(rect.left() + 14.0, rect.center().y), Align2::LEFT_CENTER, icons::ARROW_DOWN, theme::icon(13.0), theme::FG_SECONDARY);
        p.galley(egui::pos2(rect.left() + 30.0, rect.center().y - g.size().y / 2.0), g, theme::FG);
        if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
            jump = true;
        }
    }
    if jump {
        ui.data_mut(|d| d.insert_temp(pinned_id.with("jump"), true));
        ui.ctx().request_repaint();
    }
}

/// Send `text` as a new user turn (only when idle; `take_prompt` refuses while busy). The
/// composer draft is kept.
fn send_feedback(state: &mut UiState, text: String, commands: &mut Vec<UiAction>) -> bool {
    let saved = std::mem::replace(&mut state.input, text);
    let sent = state.take_prompt();
    state.input = saved;
    match sent {
        Some(p) => {
            commands.push(AppCommand::SendPrompt(p));
            true
        }
        None => false,
    }
}

/// Answer the pending review once; later clicks on the same review are ignored.
fn answer_review(apanel: &mut ArtifactsPanel, ctx: &egui::Context, outcome: ReviewOutcome) {
    if let Some(id) = apanel.pending_review.take() {
        artifacts_panel::post_review_answer(ctx, &id, outcome);
        apanel.answered = Some(id);
        apanel.review_message.clear();
    }
}

/// Apply what the user did in the artifacts panel or the review banner.
fn panel_action(act: PanelAction, apanel: &mut ArtifactsPanel, state: &mut UiState, ctx: &egui::Context, commands: &mut Vec<UiAction>) {
    match act {
        PanelAction::None => {}
        PanelAction::Close => apanel.open = false,
        PanelAction::Approve => answer_review(apanel, ctx, ReviewOutcome::Approved),
        PanelAction::RequestChanges(comments) if apanel.pending_review.is_some() => {
            answer_review(apanel, ctx, ReviewOutcome::ChangesRequested(comments));
        }
        PanelAction::RequestChanges(comments) => {
            // Outside a review the comments become an ordinary user turn; keep them if the
            // turn could not start.
            if !send_feedback(state, comments_to_feedback(&comments), commands) {
                apanel.comments = comments;
            }
        }
    }
}

/// Review banner above the docked composer (ORCHESTRATION.md E.3). Enter in the composer
/// never approves; only these buttons and the panel's review bar do. With the panel open the
/// banner collapses to a hint, so there is a single set of review buttons on screen.
fn review_banner(ui: &mut Ui, apanel: &mut ArtifactsPanel, state: &mut UiState, ctx: &egui::Context, commands: &mut Vec<UiAction>) {
    let mut act = PanelAction::None;
    let mut open = false;
    let title = artifacts_panel::review_title(apanel);
    let msg = apanel.review_message.trim().to_owned();
    widgets::review_frame(ui, |ui| {
        ui.spacing_mut().item_spacing = Vec2::new(8.0, 4.0);
        // Narrow composer: the buttons get their own row under the title.
        let narrow = ui.available_width() < 520.0;
        let w = ui.available_width();
        let (row, _) = ui.allocate_exact_size(Vec2::new(w, 28.0), Sense::hover());
        let btn_row = if narrow && !apanel.open { ui.allocate_exact_size(Vec2::new(w, 28.0), Sense::hover()).0 } else { row };
        // Buttons first (right to left: Proceed is rightmost), then the title gets what is left.
        let mut bu = ui.new_child(egui::UiBuilder::new().max_rect(btn_row).layout(egui::Layout::right_to_left(egui::Align::Center)));
        bu.spacing_mut().item_spacing.x = 8.0;
        if apanel.open {
            bu.label(egui::RichText::new("Review in panel").font(theme::font(12.0)).color(theme::FG_MUTED));
        } else {
            if widgets::primary_button(&mut bu, "Proceed", true).clicked() {
                act = PanelAction::Approve;
            }
            if let Some(a) = artifacts_panel::request_changes_button(&mut bu, apanel) {
                act = a;
            }
            if widgets::ghost_button(&mut bu, Some(icons::ARROW_SQUARE_OUT), "Open", theme::FG_SECONDARY, true).clicked() {
                open = true;
            }
        }
        let title_right = if btn_row == row { bu.min_rect().left() - 10.0 } else { row.right() };
        let y = row.center().y;
        ui.painter().text(egui::pos2(row.left() + 8.0, y), Align2::CENTER_CENTER, icons::EYE, theme::icon(15.0), theme::WARNING);
        let tr = Rect::from_min_max(egui::pos2(row.left() + 23.0, row.top()), egui::pos2(title_right.max(row.left() + 23.0), row.bottom()));
        widgets::paint_truncated(ui, tr, &title, theme::medium(13.0), theme::FG);
        ui.interact(tr, ui.scope_id().with("review_title"), Sense::hover()).on_hover_text(&title);
        if !msg.is_empty() && !apanel.open {
            // Second line: up to two wrapped rows; the full text is the tooltip.
            let indent = 23.0;
            let mut job = egui::text::LayoutJob::single_section(msg.clone(), egui::TextFormat::simple(theme::font(12.5), theme::FG_SECONDARY));
            job.wrap = egui::text::TextWrapping { max_width: (ui.available_width() - indent).max(40.0), max_rows: 2, break_anywhere: false, overflow_character: Some('\u{2026}') };
            let g = ui.painter().layout_job(job);
            ui.horizontal(|ui| {
                ui.add_space(indent - 8.0);
                ui.add(egui::Label::new(g).sense(Sense::hover())).on_hover_text(&msg);
            });
        }
    });
    if open {
        apanel.open = true;
    }
    panel_action(act, apanel, state, ctx, commands);
}

fn paint_fade(ui: &Ui, rect: Rect) {
    let mut mesh = egui::Mesh::default();
    let clear = Color32::from_rgba_premultiplied(0, 0, 0, 0);
    let i = mesh.vertices.len() as u32;
    mesh.colored_vertex(rect.left_top(), clear);
    mesh.colored_vertex(rect.right_top(), clear);
    mesh.colored_vertex(rect.right_bottom(), theme::BG);
    mesh.colored_vertex(rect.left_bottom(), theme::BG);
    mesh.add_triangle(i, i + 1, i + 2);
    mesh.add_triangle(i, i + 2, i + 3);
    ui.painter().add(mesh);
}

/// First item index of the transcript the scroll area still shows.
fn visible_start(state: &UiState) -> usize {
    state.items.len().saturating_sub(state.visible_items)
}


/// Identity of a transcript slice: its length plus an inline fingerprint of every item.
/// Items added, appended to or replaced change it, so a memo keyed by it never serves a
/// derivation computed for different items.
fn transcript_key(items: &[ChatItem]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for it in items {
        h = (h ^ u64::from(item_hash(it))).wrapping_mul(0x1000_0000_01b3);
    }
    h ^ (items.len() as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15)
}

/// FNV-1a of one item's identifying text. Tool arguments are hashed through their JSON text
/// because their content is what a derivation reads.
fn item_hash(it: &ChatItem) -> u32 {
    match it {
        ChatItem::User(t) => 0x11 ^ text_hash(t),
        ChatItem::Assistant(t) => 0x22 ^ text_hash(t),
        ChatItem::Thought { subject, text } => 0x33 ^ text_hash(text) ^ subject.as_deref().map(text_hash).unwrap_or(0),
        ChatItem::Notice(t) => 0x44 ^ text_hash(t),
        ChatItem::TurnHeader { duration } => 0x55 ^ duration.map(|d| d.as_secs() as u32 ^ d.subsec_millis()).unwrap_or(0),
        ChatItem::Tool(c) => {
            let mut h = 0x66u32 ^ text_hash(&c.name) ^ (c.result.is_some() as u32) << 8;
            h = h.wrapping_mul(31) ^ text_hash(&c.call_id);
            if let Some((ok, d)) = &c.result {
                h = h.wrapping_mul(31) ^ u32::from(*ok) ^ text_hash(d);
            }
            h.wrapping_mul(31) ^ text_hash(&c.args.to_string())
        }
    }
}

fn text_hash(t: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in t.as_bytes() {
        h ^= u32::from(*b);
        h = h.wrapping_mul(0x0100_0193);
    }
    h ^ (t.len() as u32).wrapping_mul(0x9e37_79b9)
}

fn transcript(
    ui: &mut Ui,
    state: &mut UiState,
    cache: &mut CommonMarkCache,
    commands: &mut Vec<UiAction>,
    col: Col,
    groups: &Derived,
    apanel: &mut ArtifactsPanel,
) {
    let total = state.items.len();
    let start = visible_start(state);
    if start > 0 {
        col.show(ui, |ui| {
            if widgets::text_link(ui, "Load older messages", 12.0).clicked() {
                state.visible_items += ITEM_PAGE;
            }
        });
        ui.add_space(8.0);
    }
    let last_header = state.items.iter().rposition(|i| matches!(i, ChatItem::TurnHeader { .. }));
    let last_assistant = state.items.iter().rposition(|i| matches!(i, ChatItem::Assistant(_)));
    let mut collapsed_turn = false;
    let mut toggles = Vec::new();
    let mut retry = false;
    // Count from the top of the conversation, not the top of the visible window: "Load older
    // messages" hides a prefix, and the rewind key must still name the message the user sees.
    let mut user_count = state.items[..start].iter().filter(|i| matches!(i, ChatItem::User(_))).count();
    for i in start..total {
        let item = &state.items[i];
        match item {
            ChatItem::User(_) => collapsed_turn = false,
            ChatItem::TurnHeader { duration } => {
                let key = format!("turn{i}");
                collapsed_turn = state.expanded.contains(&key);
                let live = state.busy && Some(i) == last_header;
                if col.show(ui, |ui| turn_header(ui, *duration, collapsed_turn, live)) {
                    toggles.push(key);
                }
                ui.add_space(2.0);
                continue;
            }
            _ => {}
        }
        // Task groups: the boundary that opened a group draws the card with the tools it owns;
        // every other owned item is skipped here.
        if let Some(&g) = groups.owner.get(&i) {
            let group = &groups.groups[g];
            if group.anchor == i && !collapsed_turn {
                let key = format!("group{i}");
                let open = state.expanded.contains(&key);
                let expanded = &state.expanded;
                let items = &state.items;
                let mut inner = Vec::new();
                let toggled = col.show(ui, |ui| {
                    taskgroups::show(ui, group, open, |ui, t| {
                        if let Some(ChatItem::Tool(c)) = items.get(t) {
                            let k = format!("tool{t}");
                            if tool_card(ui, t, c, expanded.contains(&k), true) {
                                inner.push(k);
                            }
                        }
                    })
                });
                if toggled {
                    toggles.push(key);
                }
                toggles.extend(inner);
                ui.add_space(6.0);
            }
            continue;
        }
        // A collapsed turn hides its intermediate steps (thoughts and tools), never the answer.
        if collapsed_turn && matches!(item, ChatItem::Thought { .. } | ChatItem::Tool(_)) {
            continue;
        }
        // The newest item of a live turn is streaming: the answer being written (cursor), or the
        // thought being reasoned (shown open, collapsed as soon as anything follows it).
        let streaming = state.busy
            && i + 1 == total
            && (Some(i) == last_assistant || matches!(item, ChatItem::Thought { .. }));
        let footer = !state.busy && Some(i) == last_assistant;
        // Track user_index (how many user messages appeared before this one) for the rewind
        // button. Counting must happen before show_item so the first user bubble gets index 0.
        let cur_user_index = if matches!(item, ChatItem::User(_)) {
            let idx_before = user_count;
            user_count += 1;
            Some(idx_before)
        } else {
            None
        };
        // Every item, user bubbles included, lives inside the centered reading column.
        let action = col.show(ui, |ui| show_item(ui, i, item, &state.expanded, cache, streaming, footer, cur_user_index));
        match action {
            ItemAction::Toggle(k) => toggles.push(k),
            ItemAction::Retry => retry = true,
            ItemAction::OpenArtifact(name) => {
                apanel.open = true;
                apanel.selected = Some(name);
            }
            ItemAction::Rewind(user_idx) => {
                commands.push(AppCommand::RewindCheckpoint(user_idx));
            }
            ItemAction::None => {}
        }
        let next_is_user = matches!(state.items.get(i + 1), Some(ChatItem::User(_)));
        ui.add_space(if matches!(item, ChatItem::User(_)) {
            theme::MESSAGE_GAP
        } else if next_is_user {
            theme::SECTION_GAP
        } else if matches!(item, ChatItem::Tool(_)) {
            0.0
        } else {
            4.0
        });
    }
    for k in toggles {
        state.toggle(k);
    }
    if retry {
        if let Some(ChatItem::User(t)) = state.items.iter().rev().find(|i| matches!(i, ChatItem::User(_))) {
            let saved = std::mem::replace(&mut state.input, t.clone());
            if let Some(p) = state.take_prompt() {
                commands.push(AppCommand::SendPrompt(p));
            }
            state.input = saved;
        }
    }
}

/// "Working..." / "Worked for 1m" with a caret. Returns true when clicked.
///
/// While the turn runs this is its one status line, as in the reference ("Worked for 33s").
/// There is no "Thinking..." row under it: from the moment a prompt was sent that row only said
/// "Working..." a second time, with a second spinner. The strip at the top says "Working..." as
/// well, so the status stays in view after this header has scrolled away.
fn turn_header(ui: &mut Ui, duration: Option<std::time::Duration>, collapsed: bool, live: bool) -> bool {
    let text = match duration {
        Some(d) => format!("Worked for {}", format::duration_text(d)),
        None if live => "Working...".to_owned(),
        None => "Worked".to_owned(),
    };
    let galley = ui.painter().layout_no_wrap(text, theme::font(12.5), theme::FG_MUTED);
    let (rect, r) = ui.allocate_exact_size(Vec2::new(galley.size().x + 40.0, 24.0), Sense::click());
    let c = if r.hovered() { theme::FG_SECONDARY } else { theme::FG_MUTED };
    let mut x = rect.left();
    if live {
        egui::Spinner::new().size(11.0).color(c).paint_at(ui, Rect::from_min_size(egui::pos2(x, rect.center().y - 6.0), Vec2::splat(12.0)));
        x += 18.0;
    }
    ui.painter().galley_with_override_text_color(egui::pos2(x, rect.center().y - galley.size().y / 2.0), galley.clone(), c);
    let caret = if collapsed { icons::CARET_RIGHT } else { icons::CARET_DOWN };
    ui.painter().text(egui::pos2(x + galley.size().x + 6.0, rect.center().y + 1.0), Align2::LEFT_CENTER, caret, theme::icon(11.0), c);
    r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
}

#[allow(clippy::collapsible_match)] // the links must be drawn even when not clicked
pub fn banner(ui: &mut Ui, state: &mut UiState, b: &Banner, commands: &mut Vec<UiAction>) {
    let (color, text) = match b {
        Banner::Quota(t) => (theme::WARNING, t),
        Banner::Auth(t) => (theme::ERROR, t),
        Banner::Error(t) => (theme::ERROR, t),
    };
    let mut close = false;
    egui::Frame::new()
        .fill(color.gamma_multiply(0.1))
        .stroke(Stroke::new(1.0, color.gamma_multiply(0.25)))
        .corner_radius(8)
        .inner_margin(egui::Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(icons::WARNING).font(theme::icon(15.0)).color(color));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if widgets::icon_button(ui, icons::X, 20.0, "Dismiss", true).clicked() {
                        close = true;
                    }
                    // An auth failure is fixed where the backend in use gets its credential: an
                    // account by signing in to that account again, a profile on the Providers
                    // tab (its key was refused), Code Assist by the Antigravity sign-in.
                    let account = state.settings.active_account_entry().map(|e| e.provider);
                    match b {
                        Banner::Auth(_) if account.is_some() => {
                            // Right to left: "Sign in again" at the edge, "Open Account" beside it.
                            let busy = state.running_account_login().is_some();
                            if !busy && widgets::text_link(ui, "Sign in again", 12.5).clicked() {
                                if let Some(p) = account {
                                    state.account_errors.remove(&p);
                                    commands.push(AppCommand::SignInAccount(p));
                                }
                                close = true;
                            }
                            if widgets::text_link(ui, "Open Account", 12.5).clicked() {
                                state.customize_tab = super::CustomizeTab::Account;
                                state.navigate(super::View::Customizations);
                                close = true;
                            }
                        }
                        // A provider's 401/403 means its API key was refused: Google sign-in
                        // cannot fix that, the profile's key can.
                        Banner::Auth(_) if state.settings.active_provider_profile().is_some() => {
                            if widgets::text_link(ui, "Open Providers", 12.5).clicked() {
                                state.customize_tab = super::CustomizeTab::Providers;
                                state.navigate(super::View::Customizations);
                                close = true;
                            }
                        }
                        // Code Assist: the Antigravity sign-in first — it needs no client
                        // secret, and once signed in its account is used for turns, which is what
                        // gets past a Code Assist session that was refused. Named for what it
                        // starts: "Sign in again" would promise the sign-in that just failed.
                        // With the Gemini CLI client configured, renewing that one is offered too.
                        Banner::Auth(_) if state.running_account_login().is_none() => {
                            if widgets::text_link(ui, "Sign in with Antigravity", 12.5).clicked() {
                                state.account_errors.remove(&AccountProvider::Antigravity);
                                commands.push(AppCommand::SignInAccount(AccountProvider::Antigravity));
                                close = true;
                            }
                            let google_running = matches!(state.auth, crate::auth::AuthStatus::LoggingIn);
                            if !state.oauth_not_configured()
                                && !google_running
                                && widgets::text_link(ui, "Sign in to Gemini CLI again", 12.5).clicked()
                            {
                                commands.push(AppCommand::Login);
                                close = true;
                            }
                        }
                        // Flash is a Code Assist fallback. With an account or a provider active,
                        // turns use its model, so switching `settings.model` would change nothing.
                        Banner::Quota(_) if !state.busy && !state.any_backend_ready() => {
                            if widgets::text_link(ui, "Switch to Flash", 12.5).clicked() {
                                state.settings.model = crate::settings::ALIAS_FLASH.into();
                                commands.push(AppCommand::SetModel(crate::settings::ALIAS_FLASH.into()));
                                close = true;
                            }
                        }
                        _ => {}
                    }
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        ui.add(egui::Label::new(egui::RichText::new(text).font(theme::font(13.0)).color(theme::FG)).wrap());
                    });
                });
            });
        });
    if close {
        state.banner = None;
    }
}

enum ItemAction {
    None,
    Toggle(String),
    Retry,
    /// Show this artifact in the side panel.
    OpenArtifact(String),
    /// Rewind the conversation to the checkpoint before user message `user_index`.
    Rewind(usize),
}

/// Draw one item. `streaming`, `footer` and `user_index` are per-item facts the caller works out
/// from the whole transcript, so they stay plain arguments.
#[allow(clippy::too_many_arguments)]
fn show_item(
    ui: &mut Ui,
    idx: usize,
    item: &ChatItem,
    expanded: &HashSet<String>,
    cache: &mut CommonMarkCache,
    streaming: bool,
    footer: bool,
    user_index: Option<usize>,
) -> ItemAction {
    match item {
        ChatItem::User(t) => {
            if let Some(ui_idx) = user_index {
                if user_bubble_with_rewind(ui, idx, t, ui_idx) {
                    return ItemAction::Rewind(ui_idx);
                }
            } else {
                user_bubble(ui, idx, t);
            }
            ItemAction::None
        }
        ChatItem::Assistant(t) => {
            let top = ui.cursor().top();
            let id = ui.scope_id().with(("assistant", idx));
            // Off-screen, settled messages reuse their last measured height instead of being
            // re-split and re-parsed every frame. Keyed by text length and column width.
            let w = ui.available_width();
            let key = (text_hash(t), w.round() as i32);
            let h_id = id.with("measured");
            // Off screen its size is known: skip the Markdown layout entirely. On screen it
            // is laid out, so hover, selection and scrolling behave as before.
            if !streaming && !footer {
                if let Some((k, h)) = ui.data(|d| d.get_temp::<((u32, i32), f32)>(h_id)) {
                    let r = Rect::from_min_size(ui.cursor().min, Vec2::new(w, h));
                    if k == key && !ui.is_rect_visible(r) {
                        ui.allocate_space(r.size());
                        return ItemAction::None;
                    }
                }
            }
            markdown::render_with_id(ui, cache, id, t);
            let h = ui.cursor().top() - top;
            ui.data_mut(|d| d.insert_temp(h_id, (key, h)));
            if streaming {
                streaming_cursor(ui);
            }
            if footer {
                let area = Rect::from_min_max(egui::pos2(ui.max_rect().left(), top), egui::pos2(ui.max_rect().right(), ui.cursor().top() + 28.0));
                if assistant_footer(ui, idx, t, ui.rect_contains_pointer(area)) {
                    return ItemAction::Retry;
                }
            }
            ItemAction::None
        }
        ChatItem::Thought { subject, text } => {
            let key = format!("thought{idx}");
            // The thought being reasoned right now is open, so the user sees it unfold; it
            // collapses by itself once a tool call or the answer follows it (or the turn ends).
            let open = if streaming {
                // Live thought: open unless the user closed it.
                !expanded.contains(&format!("{key}_closed"))
            } else {
                // Finished thought: closed, open only if the user toggled it.
                expanded.contains(&key)
            };
            let label = subject.as_deref().unwrap_or("Thinking");
            let clicked = step_row(ui, icons::BRAIN, label, open);
            if open {
                // The rule sits under the row icon's centre; the text lines up with the label.
                egui::Frame::NONE.inner_margin(egui::Margin { left: 6, right: 0, top: 2, bottom: 4 }).show(ui, |ui| {
                    let r = ui.horizontal_top(|ui| {
                        ui.add_space(14.0);
                        ui.add(egui::Label::new(egui::RichText::new(text).font(theme::font(13.0)).italics().color(theme::FG_SECONDARY)).wrap());
                    });
                    let rr = r.response.rect;
                    ui.painter().line_segment([rr.left_top(), rr.left_bottom()], Stroke::new(1.0, theme::CARD_BORDER));
                });
            }
            if clicked {
                // During streaming a click closes the auto-open state; after it toggles the
                // explicit open key.
                let toggle_key = if streaming { format!("{key}_closed") } else { key };
                ItemAction::Toggle(toggle_key)
            } else {
                ItemAction::None
            }
        }
        ChatItem::Tool(card) if card.name == orchestrate::WRITE_ARTIFACT => {
            // Artifact writes open the side panel instead of expanding the raw args.
            if tool_row(ui, card) {
                let name = card.args.get("name").and_then(|v| v.as_str()).unwrap_or_default().to_owned();
                return ItemAction::OpenArtifact(name);
            }
            ItemAction::None
        }
        ChatItem::Tool(card) => {
            let key = format!("tool{idx}");
            if tool_card(ui, idx, card, expanded.contains(&key), false) {
                ItemAction::Toggle(key)
            } else {
                ItemAction::None
            }
        }
        ChatItem::Notice(t) => {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(icons::PROHIBIT).font(theme::icon(12.0)).color(theme::FG_MUTED));
                ui.label(egui::RichText::new(t).font(theme::font(12.5)).italics().color(theme::FG_MUTED));
            });
            ItemAction::None
        }
        ChatItem::TurnHeader { .. } => ItemAction::None,
    }
}

/// Blinking block cursor under the streaming text.
fn streaming_cursor(ui: &mut Ui) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(8.0, 16.0), Sense::hover());
    let t = ui.input(|i| i.time);
    let a = (((t * std::f64::consts::TAU / 1.1).cos() * 0.5 + 0.5) as f32).max(0.15);
    ui.painter().rect_filled(Rect::from_min_size(rect.min + Vec2::new(0.0, 2.0), Vec2::new(7.0, 14.0)), 1, theme::FG.gamma_multiply(a));
    ui.ctx().request_repaint_after(std::time::Duration::from_millis(50));
}

/// Copy / "Send again" icons under the last assistant message; shown on hover. Returns true
/// when the last user message should be sent again (a new turn; the old answer stays).
fn assistant_footer(ui: &mut Ui, idx: usize, text: &str, hovered: bool) -> bool {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 24.0), Sense::hover());
    if !hovered {
        return false;
    }
    let b1 = Rect::from_min_size(rect.min, Vec2::splat(24.0));
    let b2 = b1.translate(Vec2::new(28.0, 0.0));
    if widgets::overlay_icon(ui, b1, ui.scope_id().with(("copy_assistant", idx)), icons::COPY, "Copy", true).clicked() {
        ui.ctx().copy_text(text.to_owned());
    }
    widgets::overlay_icon(ui, b2, ui.scope_id().with(("retry", idx)), icons::ARROW_CLOCKWISE, "Send again", true).clicked()
}

fn user_bubble(ui: &mut Ui, idx: usize, text: &str) {
    user_bubble_with_rewind(ui, idx, text, usize::MAX);
}

/// User bubble with an optional "Rewind" icon on hover. Returns `true` when rewind was clicked.
fn user_bubble_with_rewind(ui: &mut Ui, idx: usize, text: &str, user_index: usize) -> bool {
    let show_rewind = user_index != usize::MAX;
    // 32px on the left stay free for the hover copy icon.
    let max_w = (ui.available_width() - 32.0).max(40.0) * 0.8;
    let wrap_w = (max_w - 28.0).max(20.0);
    // Lay the text out once; the same galley sizes the bubble and is painted. Prose wraps at
    // word boundaries; only an unbroken token (path, URL) that still overflows falls back to
    // breaking anywhere.
    let layout = |anywhere: bool| {
        let mut job = egui::text::LayoutJob::single_section(text.to_owned(), egui::TextFormat::simple(theme::font(14.0), theme::FG));
        job.wrap = egui::text::TextWrapping { max_width: wrap_w, break_anywhere: anywhere, ..Default::default() };
        ui.painter().layout_job(job)
    };
    let mut galley = layout(false);
    if galley.size().x > wrap_w + 0.5 {
        galley = layout(true);
    }
    let col = ui.max_rect();
    let col_left = col.left();
    let rewound = ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
        let mut clicked = false;
        let r = egui::Frame::new()
            .fill(theme::SECONDARY)
            .corner_radius(theme::RADIUS_BUBBLE)
            .inner_margin(egui::Margin { left: 14, right: 14, top: 10, bottom: 10 })
            .show(ui, |ui| {
                ui.set_width(galley.size().x.ceil());
                ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                    ui.add(egui::Label::new(galley.clone()).selectable(true));
                });
            })
            .response;
        // Copy icon on hover, just left of the bubble's bottom edge (kept inside the column).
        if ui.rect_contains_pointer(r.rect.expand2(Vec2::new(36.0, 4.0)).intersect(col)) {
            let x = (r.rect.left() - 28.0).max(col_left);
            let copy_rect = Rect::from_min_size(egui::pos2(x, r.rect.bottom() - 24.0), Vec2::splat(24.0));
            if widgets::overlay_icon(ui, copy_rect, ui.scope_id().with(("copy_user", idx)), icons::COPY, "Copy", true).clicked() {
                ui.ctx().copy_text(text.to_owned());
            }
            // Rewind icon above the copy icon, shown only for non-first messages (user_index > 0).
            if show_rewind && user_index > 0 {
                let rewind_rect = Rect::from_min_size(egui::pos2(x, r.rect.bottom() - 52.0), Vec2::splat(24.0));
                let tooltip = format!("Rewind to before message {}", user_index + 1);
                if widgets::overlay_icon(ui, rewind_rect, ui.scope_id().with(("rewind", idx)), icons::ARROW_CLOCKWISE, &tooltip, true).clicked() {
                    clicked = true;
                }
            }
        }
        clicked
    });
    rewound.inner
}

/// Compact row for a thought: icon, label, caret. Same size, colour and columns as
/// [`tool_row`], so reasoning steps read as quiet annotations under the answer text.
/// Returns clicked.
fn step_row(ui: &mut Ui, icon: &str, label: &str, open: bool) -> bool {
    let w = ui.available_width();
    let (rect, r) = ui.allocate_exact_size(Vec2::new(w, 22.0), Sense::click());
    let c = if r.hovered() { theme::FG_SECONDARY } else { theme::FG_MUTED };
    let p = ui.painter();
    p.text(rect.left_center(), Align2::LEFT_CENTER, icon, theme::icon(12.5), c);
    let g = p.layout_no_wrap(label.to_owned(), theme::font(12.0), c);
    let tr = Rect::from_min_max(rect.min + Vec2::new(20.0, 0.0), rect.max - Vec2::new(24.0, 0.0));
    widgets::paint_truncated(ui, tr, label, theme::font(12.0), c);
    let caret = if open { icons::CARET_DOWN } else { icons::CARET_RIGHT };
    let cx = (tr.left() + g.size().x + 6.0).min(tr.right() + 4.0);
    ui.painter().text(egui::pos2(cx, rect.center().y + 1.0), Align2::LEFT_CENTER, caret, theme::icon(9.5), c);
    r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
}

fn tool_icon(name: &str) -> &'static str {
    match name {
        "read_file" => icons::FILE_TEXT,
        "list_directory" => icons::FOLDER_OPEN,
        "glob" | "grep_search" => icons::MAGNIFYING_GLASS,
        "write_file" => icons::FILE_PLUS,
        "replace" => icons::PENCIL_SIMPLE_LINE,
        "run_shell_command" => icons::TERMINAL_WINDOW,
        orchestrate::WRITE_ARTIFACT => icons::NOTE_PENCIL,
        orchestrate::REQUEST_REVIEW => icons::EYE,
        orchestrate::NOTIFY_USER => icons::INFO,
        orchestrate::TASK_COMPLETE => icons::CHECK_CIRCLE,
        orchestrate::TASK_BOUNDARY => icons::LIGHTNING,
        _ => icons::WRENCH,
    }
}

/// Verb + primary argument, e.g. ("Read", "src/main.rs").
pub fn tool_summary(card: &ToolCard) -> (&'static str, String) {
    let arg = |k: &str| card.args.get(k).and_then(|v| v.as_str()).unwrap_or("").to_owned();
    let running = card.result.is_none();
    match card.name.as_str() {
        "read_file" => (if running { "Reading" } else { "Read" }, arg("file_path")),
        "list_directory" => (if running { "Listing" } else { "Listed" }, arg("dir_path")),
        "glob" => (if running { "Searching files" } else { "Searched files" }, arg("pattern")),
        "grep_search" => (if running { "Searching" } else { "Searched" }, arg("pattern")),
        "write_file" => (if running { "Writing" } else { "Wrote" }, arg("file_path")),
        "replace" => (if running { "Editing" } else { "Edited" }, arg("file_path")),
        "run_shell_command" => (if running { "Running" } else { "Ran" }, arg("command").lines().next().unwrap_or("").to_owned()),
        orchestrate::WRITE_ARTIFACT => {
            use crate::agent::artifacts::ArtifactKind;
            let name = arg("name");
            let title = match ArtifactKind::from_stem(&name) {
                ArtifactKind::TaskList => "Task".to_owned(),
                ArtifactKind::ImplementationPlan => "Implementation Plan".to_owned(),
                ArtifactKind::Walkthrough => "Walkthrough".to_owned(),
                ArtifactKind::Other => name,
            };
            (if running { "Writing" } else { "Wrote" }, title)
        }
        orchestrate::REQUEST_REVIEW => (if running { "Waiting for review" } else { "Review" }, first_line(&arg("message"))),
        orchestrate::NOTIFY_USER => ("Note", first_line(&arg("message"))),
        orchestrate::TASK_COMPLETE => ("Completed", first_line(&arg("result"))),
        orchestrate::TASK_BOUNDARY => ("Task", arg("task_name")),
        _ => ("Called", card.name.clone()),
    }
}

fn first_line(s: &str) -> String {
    s.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("").to_owned()
}

/// Line counts for file cards: write_file counts content lines as additions; replace
/// counts old/new string lines.
fn edit_stats(card: &ToolCard) -> Option<(usize, usize)> {
    let lines = |k: &str| card.args.get(k).and_then(|v| v.as_str()).map(|s| s.lines().count());
    match card.name.as_str() {
        "write_file" => Some((lines("content")?, 0)),
        "replace" => Some((lines("new_string")?, lines("old_string")?)),
        _ => None,
    }
}

/// First non-empty line of a result, for the collapsed preview.
fn result_preview(display: &str) -> &str {
    display.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("")
}

/// Collapsed tool step: borderless 24px row (icon, verb, target, status). Returns clicked.
fn tool_row(ui: &mut Ui, card: &ToolCard) -> bool {
    let w = ui.available_width();
    let (rect, r) = ui.allocate_exact_size(Vec2::new(w, 22.0), Sense::click());
    let hot = r.hovered();
    let p = ui.painter();
    let y = rect.center().y;
    let x = rect.left();
    // Icon and verb use muted colour to keep tool rows visually subordinate to assistant text.
    let icon_c = if hot { theme::FG_SECONDARY } else { theme::FG_MUTED };
    let verb_c = if hot { theme::FG_SECONDARY } else { theme::FG_MUTED };
    p.text(egui::pos2(x, y), Align2::LEFT_CENTER, tool_icon(&card.name), theme::icon(12.5), icon_c);
    let (verb, target) = tool_summary(card);
    let vr = p.text(egui::pos2(x + 20.0, y), Align2::LEFT_CENTER, verb, theme::font(12.0), verb_c);
    let mut right = rect.right();
    match &card.result {
        None => {
            let sp = Rect::from_center_size(egui::pos2(right - 7.0, y), Vec2::splat(11.0));
            egui::Spinner::new().size(11.0).color(theme::FG_MUTED).paint_at(ui, sp);
            right -= 20.0;
        }
        Some((false, d)) => {
            let denied = d.contains("rejected") || d.contains("denied") || d.contains("cancel");
            let i = if denied { icons::PROHIBIT } else { icons::X };
            let ir = p.text(egui::pos2(right, y), Align2::RIGHT_CENTER, i, theme::icon(12.0), theme::ERROR);
            right = ir.left() - 8.0;
        }
        Some((true, _)) => {}
    }
    if let Some((a, d)) = edit_stats(card) {
        let p = ui.painter();
        let dr = p.text(egui::pos2(right, y), Align2::RIGHT_CENTER, format!("-{d}"), theme::mono(11.0), theme::ERROR);
        let ar = p.text(egui::pos2(dr.left() - 6.0, y), Align2::RIGHT_CENTER, format!("+{a}"), theme::mono(11.0), theme::SUCCESS);
        right = ar.left() - 12.0;
    }
    let tx = vr.right() + 6.0;
    let tr = Rect::from_min_max(egui::pos2(tx, rect.top()), egui::pos2(right.max(tx + 1.0), rect.bottom()));
    // Target path is muted too — it is context, not the focus of the conversation.
    widgets::paint_truncated(ui, tr, &target, theme::font(12.0), theme::FG_MUTED);
    r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
}

/// Arguments and output of an expanded tool call.
fn tool_details(ui: &mut Ui, idx: usize, card: &ToolCard) {
    ui.set_width(ui.available_width());
    ui.spacing_mut().item_spacing.y = 4.0;
    widgets::section_label(ui, "Arguments");
    let args = serde_json::to_string_pretty(&card.args).unwrap_or_default();
    ui.add(egui::Label::new(egui::RichText::new(args).font(theme::mono(12.0)).color(theme::FG_SECONDARY)).wrap());
    if let Some((ok, display)) = &card.result {
        ui.add_space(6.0);
        widgets::section_label(ui, if *ok { "Output" } else { "Error" });
        egui::Frame::new().fill(theme::BG).corner_radius(6).inner_margin(8).show(ui, |ui| {
            ui.set_width(ui.available_width());
            egui::ScrollArea::vertical().id_salt(("tool_result", idx)).max_height(240.0).show(ui, |ui| {
                let color = if *ok { theme::FG_SECONDARY } else { theme::ERROR };
                ui.add(egui::Label::new(egui::RichText::new(display).font(theme::mono(12.0)).color(color)).wrap());
            });
        });
    }
}

/// Returns true when the header was clicked. Collapsed: borderless row; expanded: card, or
/// with `flat` (inside a task group card) the row plus the details at the step indent.
fn tool_card(ui: &mut Ui, idx: usize, card: &ToolCard, open: bool, flat: bool) -> bool {
    if !open || flat {
        let clicked = tool_row(ui, card);
        if open {
            let indent = taskgroups::LEFT_INDENT as i8;
            egui::Frame::NONE.inner_margin(egui::Margin { left: indent, right: 0, top: 2, bottom: 8 }).show(ui, |ui| tool_details(ui, idx, card));
        }
        if card.result.is_none() {
            ui.ctx().request_repaint();
        }
        return clicked;
    }
    let mut clicked = false;
    widgets::card(8, 0).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 0.0;
        let w = ui.available_width();
        let (rect, r) = ui.allocate_exact_size(Vec2::new(w, 36.0), Sense::click());
        clicked = r.clicked();
        let p = ui.painter();
        if r.hovered() {
            let rr = if open { egui::CornerRadius { nw: 10, ne: 10, sw: 0, se: 0 } } else { egui::CornerRadius::same(10) };
            p.rect_filled(rect.shrink(1.0), rr, Color32::from_white_alpha(6));
        }
        let y = rect.center().y;
        let x = rect.left() + 12.0;
        p.text(egui::pos2(x, y), Align2::LEFT_CENTER, tool_icon(&card.name), theme::icon(15.0), theme::FG_SECONDARY);
        let (verb, target) = tool_summary(card);
        let vr = p.text(egui::pos2(x + 24.0, y), Align2::LEFT_CENTER, verb, theme::font(13.0), theme::FG_SECONDARY);

        // Right side, from the edge inwards: status, caret, stats or result preview.
        let status = Rect::from_center_size(egui::pos2(rect.right() - 18.0, y), Vec2::splat(14.0));
        match &card.result {
            None => {
                egui::Spinner::new().size(13.0).color(theme::FG_MUTED).paint_at(ui, status);
            }
            Some((true, _)) => {
                ui.painter().text(status.center(), Align2::CENTER_CENTER, icons::CHECK, theme::icon(14.0), theme::SUCCESS);
            }
            Some((false, d)) => {
                let denied = d.contains("rejected") || d.contains("denied") || d.contains("cancel");
                let i = if denied { icons::PROHIBIT } else { icons::X };
                ui.painter().text(status.center(), Align2::CENTER_CENTER, i, theme::icon(14.0), theme::ERROR);
            }
        }
        let caret = if open { icons::CARET_DOWN } else { icons::CARET_RIGHT };
        let cr = ui.painter().text(egui::pos2(status.left() - 8.0, y + 1.0), Align2::RIGHT_CENTER, caret, theme::icon(10.0), theme::FG_MUTED);
        let mut right = cr.left() - 10.0;
        if let Some((a, d)) = edit_stats(card) {
            let p = ui.painter();
            let dr = p.text(egui::pos2(right, y), Align2::RIGHT_CENTER, format!("-{d}"), theme::mono(12.0), theme::ERROR);
            let ar = p.text(egui::pos2(dr.left() - 6.0, y), Align2::RIGHT_CENTER, format!("+{a}"), theme::mono(12.0), theme::SUCCESS);
            right = ar.left() - 12.0;
        } else if let Some((ok, d)) = &card.result {
            let prev = result_preview(d);
            if !prev.is_empty() {
                let max = (right - vr.right() - 16.0) * 0.4;
                let g = ui.painter().layout_no_wrap(prev.to_owned(), theme::font(12.0), theme::FG_MUTED);
                let pw = g.size().x.min(max.max(0.0));
                let pr = Rect::from_min_max(egui::pos2(right - pw, rect.top()), egui::pos2(right, rect.bottom()));
                widgets::paint_truncated(ui, pr, prev, theme::font(12.0), if *ok { theme::FG_MUTED } else { theme::ERROR });
                right = pr.left() - 12.0;
            }
        }
        let tr = Rect::from_min_max(egui::pos2(vr.right() + 8.0, rect.top()), egui::pos2(right.max(vr.right() + 8.0), rect.bottom()));
        widgets::paint_truncated(ui, tr, &target, theme::mono(12.5), theme::FG);
        r.on_hover_cursor(egui::CursorIcon::PointingHand);

        if open {
            ui.painter().line_segment([rect.left_bottom(), rect.right_bottom()], Stroke::new(1.0, theme::CARD_BORDER));
            egui::Frame::NONE.inner_margin(egui::Margin::symmetric(12, 10)).show(ui, |ui| tool_details(ui, idx, card));
        }
    });
    if card.result.is_none() {
        ui.ctx().request_repaint();
    }
    clicked
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(name: &str, args: serde_json::Value, result: Option<(bool, String)>) -> ToolCard {
        ToolCard { call_id: "1".into(), name: name.into(), args, result }
    }

    /// The artifact memo key changes whenever the tail changes: a new, appended or replaced
    /// item never reuses a derivation computed for the old items.
    #[test]
    fn transcript_key_tracks_the_items() {
        let base = vec![ChatItem::User("hi".into()), ChatItem::Assistant("a".into())];
        let mut grown = base.clone();
        grown.push(ChatItem::Assistant("a".into()));
        assert_ne!(transcript_key(&base), transcript_key(&grown), "a new item changes the key");
        let mut same_len = base.clone();
        same_len[1] = ChatItem::Assistant("b".into());
        assert_ne!(transcript_key(&base), transcript_key(&same_len), "a different text changes the key");
        // The streaming case: the last message is appended to.
        let mut streamed = base.clone();
        if let ChatItem::Assistant(t) = &mut streamed[1] {
            t.push_str(" more");
        }
        assert_ne!(transcript_key(&base), transcript_key(&streamed), "an appended delta changes the key");
        let mut tool_a = base.clone();
        tool_a.push(ChatItem::Tool(card("write_artifact", serde_json::json!({"name": "task", "content": "x"}), None)));
        let mut tool_b = base.clone();
        tool_b.push(ChatItem::Tool(card("write_artifact", serde_json::json!({"name": "task", "content": "y"}), None)));
        assert_ne!(transcript_key(&tool_a), transcript_key(&tool_b), "tool arguments change the key");
        // Result arriving on the same card changes the key too.
        let mut resolved = tool_a.clone();
        if let ChatItem::Tool(c) = &mut resolved[2] {
            c.result = Some((true, "ok".into()));
        }
        assert_ne!(transcript_key(&tool_a), transcript_key(&resolved));
        // The same items always hash the same.
        assert_eq!(transcript_key(&base), transcript_key(&base.clone()));
    }

    fn tool_item(done: bool) -> ChatItem {
        ChatItem::Tool(card("read_file", serde_json::json!({"file_path": "a.rs"}), done.then(|| (true, "ok".to_owned()))))
    }

    /// A turn that has only just started: the prompt and its live header.
    fn started() -> Vec<ChatItem> {
        vec![ChatItem::User("Xin chào".into()), ChatItem::TurnHeader { duration: None }]
    }

    /// A running turn says so once, on its header. A "Thinking..." row under it repeated
    /// "Working..." from the moment the prompt was sent, and again after every finished step.
    #[test]
    fn a_running_turn_has_one_status_line() {
        use crate::ui::test_util::{context, painted};
        let ctx = context();
        let mut cache = CommonMarkCache::default();
        let mut commands = Vec::new();
        let count = |texts: &[(String, Rect)], t: &str| texts.iter().filter(|(s, _)| s == t).count();
        for tail in [
            None,
            Some(ChatItem::Thought { subject: None, text: "hmm".into() }),
            Some(tool_item(false)),
            Some(tool_item(true)),
            Some(ChatItem::Notice("Retrying (attempt 1)".into())),
        ] {
            let mut state = UiState { items: started(), busy: true, ..UiState::default() };
            state.items.extend(tail.clone());
            let texts = painted(&ctx, Vec::new(), |ui| {
                egui::CentralPanel::default().show(ui, |ui| show(ui, &mut state, &mut cache, &mut commands));
            });
            assert!(count(&texts, "Working...") >= 1, "{tail:?}: {texts:?}");
            assert_eq!(count(&texts, "Thinking..."), 0, "{tail:?}: {texts:?}");
        }
        assert!(commands.is_empty());
    }

    /// A frame is laid out twice only when the composer changed height in it. Doing it every
    /// frame would double the work of each one (egui paints a warning after three in a row).
    #[test]
    fn only_a_composer_height_change_lays_a_frame_out_twice() {
        let ctx = crate::ui::test_util::context();
        let mut cache = CommonMarkCache::default();
        let mut commands = Vec::new();
        let mut state = UiState { items: started(), busy: true, ..UiState::default() };
        let mut passes = |state: &mut UiState| {
            let raw = egui::RawInput { screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(1400.0, 900.0))), ..Default::default() };
            let out = ctx.run_ui(raw, |ui| {
                egui::CentralPanel::default().show(ui, |ui| show(ui, state, &mut cache, &mut commands));
            });
            let n = out.platform_output.num_completed_passes;
            out.drop_without_applying_deltas();
            n
        };
        passes(&mut state);
        for _ in 0..3 {
            assert_eq!(passes(&mut state), 1, "nothing changed");
        }
        state.input = "one\ntwo\nthree\nfour".into();
        assert_eq!(passes(&mut state), 2, "the composer grew");
        assert_eq!(passes(&mut state), 1);
        state.input.clear();
        assert_eq!(passes(&mut state), 2, "the composer shrank");
        assert_eq!(passes(&mut state), 1);
    }

    /// The error that ended a turn is on screen however long the turn ran. At the top of the
    /// transcript it went out of view as the transcript stuck to its end, and the turn looked
    /// as if it had stopped for no reason.
    #[test]
    fn the_error_that_ended_a_turn_stays_in_view() {
        use crate::ui::test_util::{context, painted};
        let ctx = context();
        let mut cache = CommonMarkCache::default();
        let mut commands = Vec::new();
        let items = (0..40)
            .flat_map(|i| [ChatItem::User(format!("Question {i}")), ChatItem::Assistant(format!("Answer {i}.\n\nA second paragraph."))])
            .collect();
        let message = "HTTP 400: The selected model cannot continue this conversation.";
        let mut state = UiState { items, banner: Some(crate::ui::Banner::Error(message.into())), ..UiState::default() };
        // The transcript sticks to its end from the second frame on.
        let mut texts = Vec::new();
        for _ in 0..4 {
            texts = painted(&ctx, Vec::new(), |ui| {
                egui::CentralPanel::default().show(ui, |ui| show(ui, &mut state, &mut cache, &mut commands));
            });
        }
        assert!(!texts.iter().any(|(t, _)| t == "Question 0"), "the transcript is taller than the window");
        let screen = Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(1400.0, 1600.0));
        let shown = texts.iter().find(|(t, _)| t == message).map(|(_, r)| *r);
        assert!(shown.is_some_and(|r| screen.contains_rect(r)), "the error is not in view: {shown:?}");
        assert!(commands.is_empty());
    }

    /// The visible window starts at the tail the transcript draws.
    #[test]
    fn visible_start_follows_the_page() {
        let many: Vec<ChatItem> = (0..(ITEM_PAGE + 5)).map(|i| ChatItem::User(i.to_string())).collect();
        let mut s = UiState { items: many, ..UiState::default() };
        assert_eq!(visible_start(&s), 5);
        s.visible_items = s.items.len();
        assert_eq!(visible_start(&s), 0);
    }

    #[test]
    fn summary_verbs_follow_status() {
        let c = card("read_file", serde_json::json!({"file_path": "a.rs"}), None);
        assert_eq!(tool_summary(&c), ("Reading", "a.rs".into()));
        let c = card("read_file", serde_json::json!({"file_path": "a.rs"}), Some((true, "x".into())));
        assert_eq!(tool_summary(&c), ("Read", "a.rs".into()));
        let c = card("run_shell_command", serde_json::json!({"command": "ls\necho"}), Some((true, "".into())));
        assert_eq!(tool_summary(&c).1, "ls");
    }

    #[test]
    fn column_is_centered_with_gutter() {
        let c = Col::for_width(1400.0);
        assert_eq!((c.w, c.pad), (COLUMN_MAX, (1400.0 - COLUMN_MAX) / 2.0));
        let c = Col::for_width(500.0);
        assert_eq!((c.w, c.pad), (452.0, 24.0));
        let c = Col::for_width(100.0);
        assert!(c.w <= 100.0 && c.pad >= 0.0);
    }

    /// Running tools never narrow the transcript: a turn's steps and answer are read in the
    /// column it started in. A Progress card beside the column took a lane out of it as soon as
    /// the first tool ran (in a 1160 px window: 768 px down to 532, moved right). The steps are
    /// in the Progress panel now, drawn only when it is opened.
    #[test]
    fn running_tools_never_narrow_the_column() {
        use crate::ui::test_util::{context, painted_in};
        let ctx = context();
        let mut cache = CommonMarkCache::default();
        let mut commands = Vec::new();
        // Where the header strip's title starts (the column's left edge), and whether the
        // Progress panel was drawn.
        let mut column = |items: &[ChatItem], open: bool| {
            let mut state = UiState { items: items.to_vec(), current_title: "Greeting".into(), progress_open: open, ..UiState::default() };
            let mut texts = Vec::new();
            for _ in 0..2 {
                texts = painted_in(&ctx, Vec2::new(1160.0, 800.0), Vec::new(), |ui| {
                    egui::CentralPanel::default().show(ui, |ui| show(ui, &mut state, &mut cache, &mut commands));
                });
            }
            let title = texts.iter().find(|(t, _)| t == "Greeting").map(|(_, r)| r.left()).expect("the header strip");
            (title, texts.iter().any(|(t, _)| t == "Progress"))
        };
        let chat = [ChatItem::User("hello there".into()), ChatItem::TurnHeader { duration: None }, ChatItem::Assistant("Hi!".into())];
        let mut tools = chat.to_vec();
        tools.splice(2..2, [tool_item(true), tool_item(false)]);

        let (before, _) = column(&chat, false);
        assert!((before - (1160.0 - COLUMN_MAX) / 2.0).abs() < 3.0, "a full, centered column: {before}");
        let (during, drawn) = column(&tools, false);
        assert_eq!(during, before, "the column moved when the tools ran");
        assert!(!drawn, "the steps stay in the closed panel");
        // Opened, the panel lists them and the column centers in the room that is left.
        let (open, drawn) = column(&tools, true);
        assert!(drawn);
        assert!(open < before, "{open} vs {before}");
        assert!(commands.is_empty());
    }

    /// With the artifacts panel open as well, the Progress panel stays at the window edge under
    /// its toggle, the artifacts panel goes beside it, and the column keeps clear of both.
    #[test]
    fn the_progress_panel_keeps_the_edge_beside_the_artifacts_panel() {
        use crate::ui::test_util::{context, painted};
        let ctx = context();
        let mut cache = CommonMarkCache::default();
        let mut commands = Vec::new();
        let current = Some(uuid::Uuid::new_v4());
        artifacts_panel::store(&ctx, current, ArtifactsPanel { open: true, ..Default::default() });
        let items = vec![ChatItem::User("go".into()), ChatItem::TurnHeader { duration: None }, tool_item(true)];
        let mut state = UiState { items, current, progress_open: true, ..UiState::default() };
        let mut texts = Vec::new();
        for _ in 0..2 {
            texts = painted(&ctx, Vec::new(), |ui| {
                egui::CentralPanel::default().show(ui, |ui| show(ui, &mut state, &mut cache, &mut commands));
            });
        }
        let at = |t: &str| texts.iter().find(|(s, _)| s == t).map(|(_, r)| *r).unwrap_or_else(|| panic!("no {t:?} in {texts:?}"));
        assert!(at("Progress").left() > at("Artifacts").left(), "{texts:?}");
        assert!(at("go").right() < at("Artifacts").left(), "the column runs under a panel: {texts:?}");
        assert!(texts.iter().any(|(t, _)| t == "Read a.rs"), "the step is listed: {texts:?}");
        assert!(commands.is_empty());
    }

    #[test]
    fn orchestration_summaries() {
        let c = card(orchestrate::WRITE_ARTIFACT, serde_json::json!({"name": "implementation_plan"}), Some((true, "".into())));
        assert_eq!(tool_summary(&c), ("Wrote", "Implementation Plan".into()));
        let c = card(orchestrate::REQUEST_REVIEW, serde_json::json!({"message": "\n Check it\nmore"}), None);
        assert_eq!(tool_summary(&c), ("Waiting for review", "Check it".into()));
    }

    #[test]
    fn feedback_outside_review_is_sent_as_prompt() {
        let ctx = egui::Context::default();
        let mut state = UiState::default();
        let mut panel = ArtifactsPanel::default();
        let mut cmds = Vec::new();
        state.input = "draft".into();
        let c = vec![crate::agent::artifacts::ArtifactComment { artifact: "task".into(), anchor: None, text: "split step 2".into() }];
        panel_action(PanelAction::RequestChanges(c.clone()), &mut panel, &mut state, &ctx, &mut cmds);
        assert!(matches!(&cmds[0], AppCommand::SendPrompt(p) if p.contains("split step 2")));
        assert_eq!(state.input, "draft", "composer draft is kept");
        // Busy now: nothing is sent and the comments stay.
        cmds.clear();
        panel_action(PanelAction::RequestChanges(c), &mut panel, &mut state, &ctx, &mut cmds);
        assert!(cmds.is_empty());
        assert_eq!(panel.comments.len(), 1);
    }

    #[test]
    fn review_answer_is_posted_once() {
        let ctx = egui::Context::default();
        let mut state = UiState::default();
        let mut panel = ArtifactsPanel { pending_review: Some("r1".into()), ..Default::default() };
        let mut cmds = Vec::new();
        panel_action(PanelAction::Approve, &mut panel, &mut state, &ctx, &mut cmds);
        panel_action(PanelAction::Approve, &mut panel, &mut state, &ctx, &mut cmds);
        assert_eq!(artifacts_panel::take_review_answer(&ctx), Some(("r1".into(), ReviewOutcome::Approved)));
        assert!(artifacts_panel::take_review_answer(&ctx).is_none());
        assert_eq!(panel.answered.as_deref(), Some("r1"));
        assert!(cmds.is_empty());
    }

    #[test]
    fn edit_stats_counts_lines() {
        let c = card("replace", serde_json::json!({"old_string": "a\nb", "new_string": "c"}), None);
        assert_eq!(edit_stats(&c), Some((1, 2)));
        assert_eq!(edit_stats(&card("read_file", serde_json::json!({}), None)), None);
        assert_eq!(result_preview("\n  first \nsecond"), "first");
    }

    /// "Switch to Flash" changes the Code Assist model, so it is offered only without a provider.
    #[test]
    fn quota_banner_offers_flash_only_on_code_assist() {
        let ctx = crate::ui::test_util::context();
        let mut state = UiState::default();
        state.settings.providers = vec![crate::providers::ProviderProfile {
            id: "p".into(),
            label: "P".into(),
            kind: crate::providers::ProviderKind::OpenAi,
            model: "gpt-4o-mini".into(),
            base_url: None,
            api_key: "k".into(),
            max_output_tokens: None,
            context_window: None,
            models: Vec::new(),
        }];
        state.settings.active_provider = Some("p".into());
        let b = crate::ui::Banner::Quota("rate limited".into());
        let mut commands = Vec::new();
        let texts = crate::ui::test_util::painted(&ctx, Vec::new(), |ui| banner(ui, &mut state, &b, &mut commands));
        assert!(texts.iter().any(|(t, _)| t == "rate limited"), "the banner is drawn");
        assert!(!texts.iter().any(|(t, _)| t == "Switch to Flash"), "no Flash link with a provider active");

        state.settings.active_provider = None;
        let texts = crate::ui::test_util::painted(&ctx, Vec::new(), |ui| banner(ui, &mut state, &b, &mut commands));
        assert!(texts.iter().any(|(t, _)| t == "Switch to Flash"), "Code Assist keeps the Flash link");
        assert!(commands.is_empty());
    }

    /// A provider's 401/403 is a bad API key: Google sign-in cannot fix it, the Providers tab can.
    #[test]
    fn a_rejected_provider_key_points_to_the_providers_tab() {
        use crate::ui::test_util::{click, context, painted};
        let ctx = context();
        let mut state = UiState::default();
        state.settings.providers = vec![crate::providers::ProviderProfile {
            id: "p".into(),
            label: "P".into(),
            kind: crate::providers::ProviderKind::OpenAi,
            model: "gpt-4o-mini".into(),
            base_url: None,
            api_key: "k".into(),
            max_output_tokens: None,
            context_window: None,
            models: Vec::new(),
        }];
        state.settings.active_provider = Some("p".into());
        let b = crate::ui::Banner::Auth("authentication failed: invalid key".into());
        state.banner = Some(b.clone());
        let mut commands = Vec::new();
        let texts = painted(&ctx, Vec::new(), |ui| banner(ui, &mut state, &b, &mut commands));
        assert!(!texts.iter().any(|(t, _)| t.starts_with("Sign in")), "{texts:?}");
        let link = texts.iter().find(|(t, _)| t == "Open Providers").map(|(_, r)| r.center()).expect("a link to the Providers tab");
        painted(&ctx, vec![egui::Event::PointerMoved(link), click(link, true)], |ui| banner(ui, &mut state, &b, &mut commands));
        painted(&ctx, vec![click(link, false)], |ui| banner(ui, &mut state, &b, &mut commands));
        assert!(commands.is_empty(), "no Google sign-in starts");
        assert_eq!((state.view, state.customize_tab), (crate::ui::View::Customizations, crate::ui::CustomizeTab::Providers));
        assert_eq!(state.banner, None);

        // Code Assist offers the Antigravity sign-in, and — the Gemini CLI client being
        // configured — renewing that sign-in as well.
        state.settings.active_provider = None;
        let texts = painted(&ctx, Vec::new(), |ui| banner(ui, &mut state, &b, &mut commands));
        assert!(texts.iter().any(|(t, _)| t == "Sign in with Antigravity"), "{texts:?}");
        assert!(texts.iter().any(|(t, _)| t == "Sign in to Gemini CLI again"), "{texts:?}");
        let sent = click_link(&ctx, &mut state, &b, "Sign in to Gemini CLI again");
        assert!(matches!(sent.as_slice(), [AppCommand::Login]), "{sent:?}");
    }

    /// Click the link `label` of the banner `b`; the commands sent.
    fn click_link(ctx: &egui::Context, state: &mut UiState, b: &crate::ui::Banner, label: &str) -> Vec<UiAction> {
        use crate::ui::test_util::{click, painted};
        let mut commands = Vec::new();
        let texts = painted(ctx, Vec::new(), |ui| banner(ui, state, b, &mut commands));
        let at = texts.iter().find(|(t, _)| t == label).map(|(_, r)| r.center()).unwrap_or_else(|| panic!("no {label:?} link in {texts:?}"));
        painted(ctx, vec![egui::Event::PointerMoved(at), click(at, true)], |ui| banner(ui, state, b, &mut commands));
        painted(ctx, vec![click(at, false)], |ui| banner(ui, state, b, &mut commands));
        commands
    }

    #[test]
    fn the_auth_banner_signs_in_the_account_not_google() {
        use crate::ui::test_util::{context, painted};
        let ctx = context();
        let b = crate::ui::Banner::Auth("ChatGPT (Codex) account: authentication failed: token revoked".into());
        let mut state = UiState::default();
        state.settings.accounts = vec![crate::settings::AccountEntry {
            id: "codex-0123abcd".into(),
            provider: AccountProvider::Codex,
            label: "dev@example.com".into(),
            model: "gpt-5.1-codex".into(),
        }];
        state.settings.active_account = Some("codex-0123abcd".into());
        state.banner = Some(b.clone());
        let sent = click_link(&ctx, &mut state, &b, "Sign in again");
        assert!(matches!(sent.as_slice(), [AppCommand::SignInAccount(AccountProvider::Codex)]), "{sent:?}");
        assert!(!sent.iter().any(|c| matches!(c, AppCommand::Login)), "never the Google flow");
        assert_eq!(state.banner, None);

        state.banner = Some(b.clone());
        let sent = click_link(&ctx, &mut state, &b, "Open Account");
        assert!(sent.is_empty());
        assert_eq!((state.view, state.customize_tab), (crate::ui::View::Customizations, crate::ui::CustomizeTab::Account));

        // No account and no profile in use: still not the Google flow but the Antigravity
        // sign-in, which works without a client secret.
        let mut state = UiState::default();
        state.apply(crate::app::AppEvent::OAuthClientSource(None));
        assert!(state.oauth_not_configured());
        let b = crate::ui::Banner::Auth("authentication failed: session expired".into());
        let sent = click_link(&ctx, &mut state, &b, "Sign in with Antigravity");
        assert!(matches!(sent.as_slice(), [AppCommand::SignInAccount(AccountProvider::Antigravity)]), "{sent:?}");
        let mut commands = Vec::new();
        let texts = painted(&ctx, Vec::new(), |ui| banner(ui, &mut state, &b, &mut commands));
        assert!(!texts.iter().any(|(t, _)| t == "Sign in to Gemini CLI again"), "no client, no Google sign-in: {texts:?}");

        // While a sign-in runs there is nothing to start.
        state.apply(crate::app::AppEvent::AccountLogin {
            login: Some(crate::app::AccountLoginInfo { id: 3, provider: AccountProvider::Antigravity, auth_url: "https://accounts.example/".into(), error: None }),
        });
        state.apply(crate::app::AppEvent::Accounts(vec![crate::auth::external::AccountView {
            id: String::new(),
            provider: AccountProvider::Antigravity,
            email: None,
            plan: None,
            model: String::new(),
            models: Vec::new(),
            models_fetched_at: None,
            status: crate::auth::external::AccountStatus::SigningIn,
        }]));
        let mut commands = Vec::new();
        let texts = painted(&ctx, Vec::new(), |ui| banner(ui, &mut state, &b, &mut commands));
        assert!(!texts.iter().any(|(t, _)| t.starts_with("Sign in")), "{texts:?}");
    }
}
