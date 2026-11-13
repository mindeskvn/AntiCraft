//! Conversation History page (DESIGN.md 2.7): notice card, search, flat recency-sorted rows.
//! Layout follows `_ref/ui/05-history.png`: a 554px centered column, borderless 48px rows
//! with the title and relative time on line 1 and the workspace on line 2.

use chrono::{DateTime, Utc};
use egui::{Align2, Rect, Sense, Stroke, StrokeKind, Ui, Vec2};

use super::{format, icons, theme, widgets, UiAction, UiState, View};
use crate::app::AppCommand;
use crate::store::ConversationSummary;

/// Column width measured on the reference (x 540..1094).
const COLUMN: f32 = 554.0;
/// Row height / pitch (reference rows repeat every ~48px).
const ROW_H: f32 = 48.0;
/// Text inset inside the column (title x = column + 11).
const INSET: f32 = 11.0;

pub fn show(ui: &mut Ui, state: &mut UiState, commands: &mut Vec<UiAction>) {
    egui::ScrollArea::vertical().id_salt("history_page").auto_shrink([false, false]).show(ui, |ui| {
        ui.add_space(24.0);
        widgets::centered_column(ui, COLUMN, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            ui.horizontal(|ui| {
                ui.add_space(INSET);
                ui.label(egui::RichText::new("Conversation History").font(theme::medium(16.0)).color(theme::FG));
            });
            ui.add_space(18.0);
            notice(ui, state);
            ui.add_space(15.0);
            search_box(ui, state);
            project_filter(ui, state);
            ui.add_space(17.0);
            list(ui, state, commands);
        });
        ui.add_space(32.0);
    });
}

/// Info card above the search box (reference: three lines on a 20px pitch; lines 2-3 hang
/// 22px in, under a leading icon).
fn notice(ui: &mut Ui, state: &mut UiState) {
    widgets::card(8, 0).inner_margin(egui::Margin { left: 12, right: 12, top: 8, bottom: 7 }).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 0.0;
        let line = |ui: &mut Ui, add: &mut dyn FnMut(&mut Ui)| {
            let w = ui.available_width();
            ui.allocate_ui_with_layout(Vec2::new(w, 20.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.set_min_size(Vec2::new(w, 20.0));
                add(ui);
            });
        };
        line(ui, &mut |ui| {
            ui.label(egui::RichText::new("Your conversations are saved on this computer").font(theme::medium(13.0)).color(theme::FG));
        });
        line(ui, &mut |ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            let (ir, _) = ui.allocate_exact_size(Vec2::new(22.0, 20.0), Sense::hover());
            ui.painter().text(egui::pos2(ir.left() + 7.0, ir.center().y), Align2::CENTER_CENTER, icons::INFO, theme::icon(14.0), theme::FG_SECONDARY);
            ui.add(
                egui::Label::new(
                    egui::RichText::new("Open one to pick up where you left off.").font(theme::font(12.0)).color(theme::FG_SECONDARY),
                )
                .truncate(),
            );
        });
        let n = state.conversations.len();
        line(ui, &mut |ui| {
            ui.add_space(22.0);
            ui.spacing_mut().item_spacing.x = 4.0;
            let count = if n == 1 { "1 conversation".to_owned() } else { format!("{n} conversations") };
            ui.label(egui::RichText::new(count).font(theme::font(12.0)).color(theme::FG_SECONDARY));
            ui.label(egui::RichText::new("·").font(theme::font(12.0)).color(theme::FG_SECONDARY));
            let r = ui.add(egui::Label::new(egui::RichText::new("Start a new conversation").font(theme::font(12.0)).color(theme::LINK)).sense(Sense::click()));
            if r.hovered() {
                let y = r.rect.bottom() - 1.0;
                ui.painter().line_segment([egui::pos2(r.rect.left(), y), egui::pos2(r.rect.right(), y)], Stroke::new(1.0, theme::LINK));
            }
            if r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() && !state.busy {
                state.new_chat();
                state.navigate(View::Chat);
            }
        });
    });
}

fn search_box(ui: &mut Ui, state: &mut UiState) {
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(w, 28.0), Sense::hover());
    let id = ui.make_persistent_id("history_search");
    let focused = ui.ctx().memory(|m| m.has_focus(id));
    let p = ui.painter();
    p.rect_filled(rect, 6, theme::FIELD_BG);
    p.rect_stroke(rect, 6, Stroke::new(1.0, if focused { theme::PRIMARY } else { theme::CARD_BORDER }), StrokeKind::Inside);
    p.text(rect.left_center() + Vec2::new(17.0, 0.0), Align2::CENTER_CENTER, icons::MAGNIFYING_GLASS, theme::icon(14.0), theme::FG_MUTED);
    let edit = Rect::from_min_max(rect.min + Vec2::new(30.0, 0.0), rect.max - Vec2::new(30.0, 0.0));
    let r = ui.put(
        edit,
        egui::TextEdit::singleline(&mut state.history_query)
            .id(id)
            .frame(egui::Frame::NONE)
            .font(theme::font(13.0))
            .align(egui::Align2::LEFT_CENTER)
            .hint_text(egui::RichText::new("Search conversations").color(theme::PLACEHOLDER))
            .desired_width(edit.width()),
    );
    // Focused when the toolbar search icon (or the History nav) asks for it.
    if state.focus_search {
        r.request_focus();
        state.focus_search = false;
    }
    if state.history_query.is_empty() {
        // Trailing filter glyph, 14px in from the right edge (replaced by the clear X).
        ui.painter().text(egui::pos2(rect.right() - 14.0, rect.center().y), Align2::CENTER_CENTER, icons::FUNNEL, theme::icon(14.0), theme::FG_MUTED);
    } else {
        let x = Rect::from_center_size(egui::pos2(rect.right() - 14.0, rect.center().y), Vec2::splat(20.0));
        if widgets::overlay_icon(ui, x, id.with("clear"), icons::X, "Clear search", true).clicked() {
            state.history_query.clear();
        }
    }
}

/// Active project filter (set from the sidebar "See all"): a removable pill.
fn project_filter(ui: &mut Ui, state: &mut UiState) {
    let Some(p) = state.history_project.clone() else { return };
    ui.add_space(10.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.add_space(INSET - 4.0);
        ui.label(egui::RichText::new("Project").font(theme::font(12.0)).color(theme::FG_MUTED));
        let g = ui.painter().layout_no_wrap(format::project_label(&p), theme::font(12.0), theme::FG);
        let (rect, r) = ui.allocate_exact_size(Vec2::new(g.size().x + 36.0, 24.0), Sense::click());
        let painter = ui.painter();
        painter.rect_filled(rect, 12, if r.hovered() { theme::PRESSED } else { theme::SECONDARY });
        painter.galley(egui::pos2(rect.left() + 10.0, rect.center().y - g.size().y / 2.0), g, theme::FG);
        painter.text(egui::pos2(rect.right() - 12.0, rect.center().y), Align2::CENTER_CENTER, icons::X, theme::icon(11.0), theme::FG_SECONDARY);
        if r.on_hover_text("Show all projects").on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
            state.history_project = None;
        }
    });
}

/// Date bucket: Today / Yesterday / This week / Older (local days). Test-only now that the
/// list is flat like the reference.
#[cfg(test)]
pub(crate) fn bucket(t: DateTime<Utc>, now: DateTime<Utc>) -> &'static str {
    let days = (now.with_timezone(&chrono::Local).date_naive() - t.with_timezone(&chrono::Local).date_naive()).num_days();
    match days {
        i64::MIN..=0 => "Today",
        1 => "Yesterday",
        2..=6 => "This week",
        _ => "Older",
    }
}

fn list(ui: &mut Ui, state: &mut UiState, commands: &mut Vec<UiAction>) {
    let now = Utc::now();
    let mut items: Vec<usize> = state
        .conversations
        .iter()
        .enumerate()
        .filter(|(_, c)| format::matches_filter(c, &state.history_query))
        .filter(|(_, c)| state.history_project.is_none() || c.workspace == state.history_project)
        .map(|(i, _)| i)
        .collect();
    items.sort_by_key(|&i| std::cmp::Reverse(state.conversations[i].updated_at));
    if items.is_empty() {
        ui.add_space(56.0);
        ui.vertical_centered(|ui| {
            ui.spacing_mut().item_spacing.y = 8.0;
            ui.label(egui::RichText::new(icons::CHATS_CIRCLE).font(theme::icon(32.0)).color(theme::FG_MUTED));
            let t = if state.conversations.is_empty() { "No conversations yet" } else { "No matching conversations" };
            ui.label(egui::RichText::new(t).font(theme::font(13.0)).color(theme::FG_MUTED));
        });
        return;
    }
    // Flat list sorted by recency (the reference has no date group headers). Zero spacing so
    // rows sit on a strict ROW_H grid.
    ui.spacing_mut().item_spacing.y = 0.0;
    // Borrow the list out of `state` (rows need `&mut state`); off-screen rows only reserve space.
    let list = std::mem::take(&mut state.conversations);
    for &i in &items {
        let slot = Rect::from_min_size(ui.cursor().min, Vec2::new(ui.available_width(), ROW_H));
        if !ui.is_rect_visible(slot) {
            ui.allocate_space(slot.size());
            continue;
        }
        row(ui, state, commands, &list[i], now);
    }
    if state.conversations.is_empty() {
        state.conversations = list;
    }
}

fn row(ui: &mut Ui, state: &mut UiState, commands: &mut Vec<UiAction>, c: &ConversationSummary, now: DateTime<Utc>) {
    let w = ui.available_width();
    if let Some((id, buf)) = state.renaming.as_mut().filter(|(id, _)| *id == c.id) {
        let id = *id;
        let (rect, _) = ui.allocate_exact_size(Vec2::new(w, ROW_H), Sense::hover());
        let field = Rect::from_center_size(rect.center(), Vec2::new(w, 32.0));
        ui.painter().rect_filled(field, 6, theme::FIELD_BG);
        ui.painter().rect_stroke(field, 6, Stroke::new(1.0, theme::PRIMARY), StrokeKind::Inside);
        let r = ui.put(
            field.shrink2(Vec2::new(INSET, 4.0)),
            egui::TextEdit::singleline(buf).frame(egui::Frame::NONE).font(theme::font(13.0)).align(egui::Align2::LEFT_CENTER),
        );
        if r.lost_focus() {
            let t = buf.trim().to_owned();
            if !t.is_empty() && !ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                commands.push(AppCommand::RenameConversation(id, t));
            }
            state.renaming = None;
        } else {
            r.request_focus();
        }
        return;
    }
    let (rect, r) = ui.allocate_exact_size(Vec2::new(w, ROW_H), Sense::click());
    let hovered = ui.rect_contains_pointer(rect);
    let confirming = state.confirm_delete == Some(c.id);
    let t = ui.ctx().animate_bool_with_time(r.id.with("hover"), hovered || confirming, 0.12);
    let selected = state.current == Some(c.id);
    let p = ui.painter();
    if selected {
        p.rect_filled(rect, 6, theme::CARD);
    } else if t > 0.0 {
        p.rect_filled(rect, 6, egui::Color32::from_white_alpha(6).gamma_multiply(t));
    }
    let title_color = theme::FG_SECONDARY.lerp_to_gamma(theme::FG, if selected { 1.0 } else { t });
    let title = if c.title.is_empty() { "Untitled Conversation" } else { c.title.as_str() };
    let line1 = rect.top() + 17.0;
    let line2 = rect.top() + 31.0;
    let tr = Rect::from_min_max(egui::pos2(rect.left() + INSET, line1 - 9.0), egui::pos2(rect.right() - 72.0, line1 + 9.0));
    widgets::paint_truncated(ui, tr, title, theme::font(13.0), title_color);
    let project = c.workspace.as_deref().map(format::project_label).unwrap_or_else(|| format::OTHER_GROUP.into());
    let sub = if state.running == Some(c.id) { format!("{project}  ·  Running") } else { project };
    let sr = Rect::from_min_max(egui::pos2(rect.left() + INSET, line2 - 8.0), egui::pos2(rect.right() - 16.0, line2 + 8.0));
    widgets::paint_truncated(ui, sr, &sub, theme::font(11.0), theme::FG_MUTED);

    let mut handled = false;
    let at = |dx: f32| Rect::from_min_size(egui::pos2(rect.right() - dx, line1 - 12.0), Vec2::splat(24.0));
    if confirming {
        if widgets::overlay_icon(ui, at(60.0), r.id.with("yes"), icons::CHECK, "Delete", true).clicked() {
            if state.current == Some(c.id) {
                state.new_chat();
                state.navigate(View::History);
            }
            commands.push(AppCommand::DeleteConversation(c.id));
            state.confirm_delete = None;
            handled = true;
        }
        if widgets::overlay_icon(ui, at(32.0), r.id.with("no"), icons::X, "Cancel", true).clicked() {
            state.confirm_delete = None;
            handled = true;
        }
    } else if hovered {
        if widgets::overlay_icon(ui, at(60.0), r.id.with("ren"), icons::PENCIL_SIMPLE, "Rename", true).clicked() {
            state.renaming = Some((c.id, c.title.clone()));
            state.rename_in_toolbar = false;
            handled = true;
        }
        let busy_here = state.busy && state.current == Some(c.id);
        let tip = if busy_here { "Stop the running turn first" } else { "Delete" };
        if widgets::overlay_icon(ui, at(32.0), r.id.with("del"), icons::TRASH, tip, !busy_here).clicked() {
            state.confirm_delete = Some(c.id);
            handled = true;
        }
    } else {
        ui.painter().text(
            egui::pos2(rect.right() - INSET, line1),
            Align2::RIGHT_CENTER,
            format::relative_time(c.updated_at, now),
            theme::font(12.0),
            theme::FG_MUTED,
        );
    }
    if r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() && !handled {
        if state.busy && state.current != Some(c.id) {
            state.banner = Some(super::Banner::Error("Stop the running turn before switching conversations.".into()));
        } else if state.current == Some(c.id) {
            state.navigate(View::Chat);
        } else {
            commands.push(AppCommand::OpenConversation(c.id));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    #[test]
    fn buckets() {
        let now = Utc::now();
        assert_eq!(bucket(now, now), "Today");
        assert_eq!(bucket(now - Duration::days(1), now), "Yesterday");
        assert_eq!(bucket(now - Duration::days(4), now), "This week");
        assert_eq!(bucket(now - Duration::days(40), now), "Older");
    }
}
