//! Left sidebar (DESIGN.md 2.3): nav rows, Projects section grouped by workspace, account row.

use egui::{Align2, Rect, Sense, Ui, Vec2};

use super::format;
use super::{icons, login, theme, widgets, UiAction, UiState, View};
use crate::app::AppCommand;

/// Single-line row pitch from the reference: 28px row plus a 2px gap.
const CONV_ROW_H: f32 = 30.0;
/// Visual height of a conversation row inside its pitch.
const CONV_ROW_VIS: f32 = 28.0;
/// Space between workspace groups.
const GROUP_GAP: f32 = 30.0;


pub fn show(ui: &mut Ui, state: &mut UiState, commands: &mut Vec<UiAction>) {
    let panel = ui.clip_rect();
    ui.painter().rect_filled(panel, 0, theme::SIDEBAR);
    let x = panel.right() - 0.5;
    ui.painter().line_segment([egui::pos2(x, panel.top()), egui::pos2(x, panel.bottom())], egui::Stroke::new(1.0, theme::EDGE));
    // Reference nav pitch is 35 (row 30 + gap 5); the first selected row spans y=79..109.
    ui.spacing_mut().item_spacing.y = 5.0;
    ui.add_space(-1.0);
    let home = state.is_home();
    // Outside Home the reference draws "New Conversation" as a boxed field (8px inset, 30 tall).
    if widgets::nav_row_boxed(ui, icons::NOTE_PENCIL, "New Conversation", home, !state.busy).clicked() {
        state.new_chat();
        commands.push(AppCommand::NewChat);
    }
    if widgets::nav_row(ui, icons::CLOCK_COUNTER_CLOCKWISE, "Conversation History", state.view == View::History, true).clicked() {
        state.navigate(View::History);
    }
    widgets::nav_row(ui, icons::LIGHTNING, "Automations", false, false);
    if widgets::nav_row(ui, icons::SLIDERS_HORIZONTAL, "Customizations", state.view == View::Customizations, true).clicked() {
        state.navigate(View::Customizations);
    }
    // Projects header text lands at the reference y 242..251.
    ui.add_space(18.0);

    // Bottom account row is laid out first so the project list takes the remaining space.
    let bottom_h = 44.0;
    let list_h = (ui.available_height() - bottom_h).max(60.0);
    ui.allocate_ui(Vec2::new(ui.available_width(), list_h), |ui| {
        projects_header(ui, state, commands);
        if !state.projects_collapsed {
            egui::ScrollArea::vertical().id_salt("projects").auto_shrink([false, false]).show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                projects(ui, state, commands);
            });
        }
    });
    ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
        login::account_row(ui, state, commands);
    });
}

fn projects_header(ui: &mut Ui, state: &mut UiState, commands: &mut Vec<UiAction>) {
    let w = ui.available_width();
    let (rect, r) = ui.allocate_exact_size(Vec2::new(w, 20.0), Sense::click());
    let hovered = ui.rect_contains_pointer(rect);
    let p = ui.painter();
    p.text(rect.left_center() + Vec2::new(8.0, 0.0), Align2::LEFT_CENTER, "Projects", theme::medium(11.5), egui::Color32::from_rgb(0x8a, 0x8a, 0x8a));
    if hovered {
        let caret = if state.projects_collapsed { icons::CARET_RIGHT } else { icons::CARET_DOWN };
        p.text(rect.left_center() + Vec2::new(66.0, 0.0), Align2::LEFT_CENTER, caret, theme::icon(11.0), theme::FG_MUTED);
    }
    let add = Rect::from_min_size(egui::pos2(rect.right() - 24.0, rect.top()), Vec2::splat(20.0));
    if widgets::overlay_icon(ui, add, r.id.with("add"), icons::PLUS, "Open folder...", !state.busy).clicked() {
        commands.push(AppCommand::PickWorkspace);
    } else if r.clicked() {
        state.projects_collapsed = !state.projects_collapsed;
    }
}

/// Conversations grouped by workspace (most recently active group first), newest first within a group.
fn projects(ui: &mut Ui, state: &mut UiState, commands: &mut Vec<UiAction>) {
    if state.conversations.is_empty() {
        ui.add_space(6.0);
        ui.label(egui::RichText::new("No conversations yet. Use + to open a folder.").font(theme::font(12.0)).color(theme::FG_MUTED));
        return;
    }
    // Borrow the list out of `state` (rows need `&mut state`) instead of cloning it, and group
    // by index with a hash map so each frame is O(n log n) over plain indices.
    let list = std::mem::take(&mut state.conversations);
    let now = chrono::Utc::now();
    let mut order: Vec<usize> = (0..list.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(list[i].updated_at));
    let mut slot: std::collections::HashMap<Option<&std::path::Path>, usize> = std::collections::HashMap::new();
    let mut groups: Vec<(Option<&std::path::Path>, Vec<usize>)> = Vec::new();
    for i in order {
        let ws = list[i].workspace.as_deref();
        let g = *slot.entry(ws).or_insert_with(|| {
            groups.push((ws, Vec::new()));
            groups.len() - 1
        });
        groups[g].1.push(i);
    }
    // Conversations without a workspace go last.
    groups.sort_by_key(|(ws, _)| ws.is_none());
    for (gi, (ws, convs)) in groups.iter().enumerate() {
        if gi > 0 {
            ui.add_space(GROUP_GAP);
        }
        let label = ws.map(format::project_label).unwrap_or_else(|| format::OTHER_GROUP.to_owned());
        group_header(ui, &label, *ws);
        for &i in convs {
            // Rows scrolled out of view only reserve their slot.
            let slot = Rect::from_min_size(ui.cursor().min, Vec2::new(ui.available_width(), CONV_ROW_H));
            if !ui.is_rect_visible(slot) {
                ui.allocate_space(slot.size());
                continue;
            }
            conversation_row(ui, state, commands, &list[i], now);
        }
    }
    // Rows never replace the list; a fresh one only arrives through `apply` between frames.
    if state.conversations.is_empty() {
        state.conversations = list;
    }
}

/// 30px workspace header: folder icon at x+8 and a muted 12.5px medium label.
fn group_header(ui: &mut Ui, label: &str, path: Option<&std::path::Path>) {
    let w = ui.available_width();
    let (rect, r) = ui.allocate_exact_size(Vec2::new(w, CONV_ROW_H), Sense::hover());
    let p = ui.painter();
    let c = rect.left_center();
    p.text(c + Vec2::new(8.0, 0.0), Align2::LEFT_CENTER, icons::FOLDER, theme::icon(14.0), theme::FG_SECONDARY);
    let text_rect = Rect::from_min_max(egui::pos2(rect.left() + 28.0, rect.top()), egui::pos2(rect.right() - 10.0, rect.bottom()));
    widgets::paint_truncated(ui, text_rect, label, theme::medium(12.5), theme::FG_SECONDARY);
    if let Some(path) = path {
        r.on_hover_text(path.display().to_string());
    }
}

fn conversation_row(
    ui: &mut Ui,
    state: &mut UiState,
    commands: &mut Vec<UiAction>,
    c: &crate::store::ConversationSummary,
    now: chrono::DateTime<chrono::Utc>,
) {
    let w = ui.available_width();
    let in_toolbar = state.rename_in_toolbar;
    if let Some((id, buf)) = state.renaming.as_mut().filter(|(id, _)| !in_toolbar && *id == c.id) {
        let id = *id;
        let r = ui.add_sized(Vec2::new(w, 30.0), egui::TextEdit::singleline(buf).font(theme::font(13.5)));
        if r.lost_focus() {
            let title = buf.trim().to_owned();
            if !title.is_empty() && !ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                commands.push(AppCommand::RenameConversation(id, title));
            }
            state.renaming = None;
        } else {
            r.request_focus();
        }
        return;
    }
    let (slot, r) = ui.allocate_exact_size(Vec2::new(w, CONV_ROW_H), Sense::click());
    // 12px indent under the group header; the 2px remainder of the pitch is the row gap.
    let rect = Rect::from_min_size(egui::pos2(slot.left() + 12.0, slot.top()), Vec2::new(w - 12.0, CONV_ROW_VIS));
    let hovered = ui.rect_contains_pointer(slot);
    let selected = state.current == Some(c.id) && state.view == View::Chat;
    let p = ui.painter();
    if selected {
        p.rect_filled(rect, 6, theme::ROW_SELECTED);
    } else if hovered {
        p.rect_filled(rect, 6, theme::ROW_HOVER);
    }
    let title = if c.title.is_empty() { "Untitled Conversation" } else { c.title.as_str() };
    let right_reserve = if hovered { 82.0 } else { 36.0 };
    let title_rect = Rect::from_min_max(egui::pos2(rect.left() + 8.0, rect.top()), egui::pos2(rect.right() - right_reserve, rect.bottom()));
    widgets::paint_truncated(ui, title_rect, title, theme::font(13.0), theme::FG);

    if state.confirm_delete == Some(c.id) {
        let yes = Rect::from_min_size(egui::pos2(rect.right() - 54.0, rect.center().y - 12.0), Vec2::splat(24.0));
        let no = Rect::from_min_size(egui::pos2(rect.right() - 28.0, rect.center().y - 12.0), Vec2::splat(24.0));
        if widgets::overlay_icon(ui, yes, r.id.with("yes"), icons::CHECK, "Delete", true).clicked() {
            if state.current == Some(c.id) {
                state.new_chat();
            }
            commands.push(AppCommand::DeleteConversation(c.id));
            state.confirm_delete = None;
        } else if widgets::overlay_icon(ui, no, r.id.with("no"), icons::X, "Cancel", true).clicked() {
            state.confirm_delete = None;
        }
        return;
    }

    let mut handled = false;
    if hovered {
        let at = |dx: f32| Rect::from_min_size(egui::pos2(rect.right() - dx, rect.center().y - 12.0), Vec2::splat(24.0));
        widgets::overlay_icon(ui, at(80.0), r.id.with("pin"), icons::PUSH_PIN, widgets::COMING_SOON, false);
        if widgets::overlay_icon(ui, at(54.0), r.id.with("ren"), icons::PENCIL_SIMPLE, "Rename", true).clicked() {
            state.renaming = Some((c.id, c.title.clone()));
            state.rename_in_toolbar = false;
            handled = true;
        }
        let busy_here = state.busy && state.current == Some(c.id);
        let tip = if busy_here { "Stop the running turn first" } else { "Delete" };
        if widgets::overlay_icon(ui, at(28.0), r.id.with("del"), icons::TRASH, tip, !busy_here).clicked() {
            state.confirm_delete = Some(c.id);
            handled = true;
        }
    } else if state.running == Some(c.id) {
        let s = Rect::from_center_size(egui::pos2(rect.right() - 16.0, rect.center().y), Vec2::splat(12.0));
        egui::Spinner::new().size(12.0).color(theme::FG_MUTED).paint_at(ui, s);
        ui.ctx().request_repaint();
    } else {
        p.text(
            egui::pos2(rect.right() - 10.0, rect.center().y),
            Align2::RIGHT_CENTER,
            format::relative_time(c.updated_at, now),
            theme::font(12.0),
            theme::FG_MUTED,
        );
    }
    let r = r.on_hover_text(title);
    if r.clicked() && !handled {
        if state.busy && state.current != Some(c.id) {
            state.banner = Some(super::Banner::Error("Stop the running turn before switching conversations.".into()));
        } else if state.current != Some(c.id) {
            commands.push(AppCommand::OpenConversation(c.id));
        } else {
            state.navigate(View::Chat);
        }
    }
    r.context_menu(|ui| {
        if ui.button("Rename").clicked() {
            state.renaming = Some((c.id, c.title.clone()));
            state.rename_in_toolbar = false;
            ui.close();
        }
        if ui.add_enabled(!(state.busy && state.current == Some(c.id)), egui::Button::new("Delete")).clicked() {
            state.confirm_delete = Some(c.id);
            ui.close();
        }
    });
}
