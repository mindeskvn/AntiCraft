//! Menu bar (AntiCraft / File / View / Window) and the toolbar row (sidebar toggle, back,
//! forward, search, new conversation, conversation title, overflow menu, "Open IDE").

use egui::{Align2, Sense, Stroke, Ui, Vec2};

use super::{icons, theme, widgets, UiAction, UiState, View};
use crate::app::AppCommand;

/// Flat text menu button (13/medium, muted; hover fills `secondary`).
fn menu_root(ui: &mut Ui, label: &str, add: impl FnOnce(&mut Ui)) {
    let galley = ui.painter().layout_no_wrap(label.to_owned(), theme::medium(13.0), theme::FG_MUTED);
    let size = Vec2::new(galley.size().x + 16.0, 28.0);
    let (rect, r) = ui.allocate_exact_size(size, Sense::click());
    let open = egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(&r));
    let hot = r.hovered() || open;
    if hot {
        ui.painter().rect_filled(rect, 6, theme::SECONDARY);
    }
    let color = if hot { theme::FG } else { theme::FG_MUTED };
    ui.painter().galley(rect.center() - galley.size() / 2.0, galley, color);
    egui::Popup::menu(&r)
        .gap(4.0)
        .frame(widgets::card(10, 6))
        .width(220.0)
        .show(|ui| {
            ui.set_min_width(200.0);
            add(ui)
        });
}

/// Menu entry with an optional right-aligned shortcut.
fn item(ui: &mut Ui, label: &str, shortcut: &str, enabled: bool) -> bool {
    let b = egui::Button::new(egui::RichText::new(label).font(theme::font(13.0)))
        .shortcut_text(egui::RichText::new(shortcut).font(theme::font(12.0)).color(theme::FG_MUTED))
        .min_size(Vec2::new(200.0, 28.0))
        .frame_when_inactive(false);
    let r = ui.add_enabled(enabled, b);
    if r.clicked() {
        ui.close();
    }
    r.clicked()
}

/// Height of the clickable toolbar strip; the rest of the 40px row is padding.
const STRIP: f32 = 32.0;

/// Title area: the 34px menu bar row over the 40px toolbar row (DESIGN.md 2.1, 2.2).
pub fn show(ui: &mut Ui, state: &mut UiState, commands: &mut Vec<UiAction>) {
    ui.spacing_mut().item_spacing.y = 0.0;
    let full = ui.max_rect();
    let w = ui.available_width();
    let p = ui.painter();
    let band = egui::Rect::from_min_size(full.min, Vec2::new(full.width(), theme::TOP_BAR_HEIGHT));
    p.rect_filled(band, 0, theme::SIDEBAR);
    if state.sidebar_open {
        // Continue the sidebar column up through the toolbar row, like the reference.
        let col = egui::Rect::from_min_max(egui::pos2(full.left(), band.bottom()), egui::pos2(full.left() + theme::SIDEBAR_WIDTH, full.bottom()));
        p.rect_filled(col, 0, theme::SIDEBAR);
        let x = col.right() - 0.5;
        p.line_segment([egui::pos2(x, col.top()), egui::pos2(x, col.bottom())], Stroke::new(1.0, theme::EDGE));
    }
    ui.allocate_ui_with_layout(Vec2::new(w, theme::TOP_BAR_HEIGHT), egui::Layout::left_to_right(egui::Align::Center), |ui| {
        ui.set_min_size(Vec2::new(w, theme::TOP_BAR_HEIGHT));
        ui.add_space(8.0);
        menu_bar(ui, state, commands);
    });
    // Toolbar strip centered at y=50 (reference pill spans y=37..62); 18px right margin.
    ui.add_space(2.0);
    ui.allocate_ui_with_layout(Vec2::new(w, STRIP), egui::Layout::left_to_right(egui::Align::Center), |ui| {
        ui.set_min_size(Vec2::new(w, STRIP));
        ui.add_space(6.0);
        ui.allocate_ui_with_layout(Vec2::new(ui.available_width() - 18.0, STRIP), egui::Layout::left_to_right(egui::Align::Center), |ui| {
            toolbar(ui, state, commands);
        });
    });
}

pub fn menu_bar(ui: &mut Ui, state: &mut UiState, commands: &mut Vec<UiAction>) {
    ui.horizontal_centered(|ui| {
        ui.spacing_mut().item_spacing.x = 7.0;
        menu_root(ui, "AntiCraft", |ui| {
            if item(ui, "Customizations", "", true) {
                state.navigate(View::Customizations);
            }
            let signed_in = matches!(state.auth, crate::auth::AuthStatus::LoggedIn { .. });
            if item(ui, "Sign out", "", signed_in) {
                commands.push(AppCommand::Logout);
            }
        });
        menu_root(ui, "File", |ui| {
            if item(ui, "New Conversation", "Ctrl+N", !state.busy) {
                state.new_chat();
                commands.push(AppCommand::NewChat);
            }
            if item(ui, "Open Workspace...", "", !state.busy) {
                commands.push(AppCommand::PickWorkspace);
            }
            ui.separator();
            if item(ui, "Conversation History", "", true) {
                state.navigate(View::History);
            }
            if item(ui, "Customizations", "", true) {
                state.navigate(View::Customizations);
            }
            ui.separator();
            if item(ui, "Quit", "", true) {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            }
        });
        menu_root(ui, "View", |ui| {
            if item(ui, "Toggle Sidebar", "Ctrl+B", true) {
                state.sidebar_open = !state.sidebar_open;
            }
            ui.separator();
            if item(ui, "Zoom In", "Ctrl+=", true) {
                egui::gui_zoom::zoom_in(ui.ctx());
            }
            if item(ui, "Zoom Out", "Ctrl+-", true) {
                egui::gui_zoom::zoom_out(ui.ctx());
            }
            if item(ui, "Reset Zoom", "Ctrl+0", true) {
                ui.ctx().set_zoom_factor(1.0);
            }
        });
        menu_root(ui, "Window", |ui| {
            if item(ui, "Minimize", "", true) {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Minimized(true));
            }
        });
    });
}

pub fn toolbar(ui: &mut Ui, state: &mut UiState, commands: &mut Vec<UiAction>) {
    ui.horizontal_centered(|ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        // Sidebar toggle (36x24); the 28px buttons that follow sit on a 30px pitch.
        let (rect, r) = ui.allocate_exact_size(Vec2::new(36.0, 24.0), Sense::click());
        widgets::paint_icon_button(ui, rect, &r, icons::SIDEBAR_SIMPLE, true, theme::FG_SECONDARY);
        if r.on_hover_text("Toggle sidebar (Ctrl+B)").clicked() {
            state.sidebar_open = !state.sidebar_open;
        }
        if widgets::icon_button(ui, icons::ARROW_LEFT, 28.0, "Back", !state.back.is_empty()).clicked() {
            state.go_back();
        }
        if widgets::icon_button(ui, icons::ARROW_RIGHT, 28.0, "Forward", !state.forward.is_empty()).clicked() {
            state.go_forward();
        }
        if widgets::icon_button(ui, icons::MAGNIFYING_GLASS, 28.0, "Search conversations", true).clicked() {
            state.navigate(View::History);
            state.focus_search = true;
        }
        if widgets::icon_button(ui, icons::NOTE_PENCIL, 28.0, "New conversation (Ctrl+N)", !state.busy).clicked() {
            state.new_chat();
            commands.push(AppCommand::NewChat);
        }

        // Conversation title, aligned with the content area (x 272 with the sidebar open).
        let show_title = state.view == View::Chat && state.current.is_some() && !state.items.is_empty();
        let content_x = if state.sidebar_open { theme::SIDEBAR_WIDTH + 16.0 } else { ui.cursor().left() + 12.0 };
        // "Open IDE", the overflow button and the panel toggle, with their gaps and a margin.
        let right_w = 86.0 + 6.0 + 22.0 + 4.0 + 22.0 + 12.0;
        if show_title {
            let left = content_x.max(ui.cursor().left() + 12.0);
            let max_w = (ui.max_rect().right() - right_w - left - 12.0).max(40.0);
            ui.add_space(left - ui.cursor().left());
            title(ui, state, commands, max_w);
        }

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            if show_title {
                // Reference conversation toolbar: panel toggle at the edge, then a filled
                // 22x22 `#2d2d2d` overflow button, then "Open IDE". The toggle opens the
                // Progress panel under it, and is filled while the panel is open.
                let open = state.progress_open;
                let (tr, tresp) = ui.allocate_exact_size(Vec2::splat(22.0), Sense::click());
                let hot = tresp.hovered();
                if open || hot {
                    ui.painter().rect_filled(tr, 6, if hot { theme::PRESSED } else { theme::SEND_BG });
                }
                let c = if open || hot { theme::FG } else { theme::FG_SECONDARY };
                ui.painter().text(tr.center(), Align2::CENTER_CENTER, icons::SIDEBAR_SIMPLE, theme::icon(14.0), c);
                let tip = if open { "Hide progress" } else { "Show progress" };
                if tresp.on_hover_text(tip).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    state.progress_open = !open;
                }
                ui.add_space(4.0);
                let (pr, r) = ui.allocate_exact_size(Vec2::splat(22.0), Sense::click());
                let fill = if r.hovered() { theme::PRESSED } else { theme::SEND_BG };
                ui.painter().rect_filled(pr, 6, fill);
                ui.painter().text(pr.center(), Align2::CENTER_CENTER, icons::DOTS_THREE, theme::icon(14.0), theme::FG);
                ui.add_space(6.0);
                open_ide_button(ui);
                egui::Popup::menu(&r).frame(widgets::card(10, 6)).gap(4.0).show(|ui| {
                    ui.set_min_width(200.0);
                    if item(ui, "Rename", "", true) {
                        if let Some(id) = state.current {
                            state.renaming = Some((id, state.current_title.clone()));
                            state.rename_in_toolbar = true;
                        }
                    }
                    if item(ui, "Copy as Markdown", "", true) {
                        ui.ctx().copy_text(state.transcript_markdown());
                    }
                    ui.separator();
                    // Two-step delete: the first click arms, the second deletes. Disabled while
                    // a turn is running, like the History page.
                    let armed = state.current.is_some() && state.confirm_delete == state.current;
                    let can_delete = state.current.is_some() && !state.busy;
                    let label = if armed { "Click again to delete" } else { "Delete" };
                    let b = egui::Button::new(egui::RichText::new(label).font(theme::font(13.0)).color(if armed { theme::ERROR } else { theme::FG }))
                        .min_size(Vec2::new(200.0, 28.0))
                        .frame_when_inactive(false);
                    let r = ui.add_enabled(can_delete, b);
                    let r = if state.busy { r.on_disabled_hover_text("Stop the running turn first") } else { r };
                    if r.clicked() {
                        if let Some(id) = state.current {
                            if armed {
                                state.confirm_delete = None;
                                state.new_chat();
                                commands.push(AppCommand::DeleteConversation(id));
                                ui.close();
                            } else {
                                state.confirm_delete = Some(id);
                            }
                        }
                    }
                });
            } else if matches!(state.view, View::History | View::Customizations) {
                // Outlined variant on settings-style pages; ends flush at x≈1392.
                ui.add_space(-10.0);
                open_ide_button_outlined(ui);
            } else {
                open_ide_button(ui);
            }
        });
    });
}

/// Conversation title; click to rename inline (shares the sidebar rename buffer).
fn title(ui: &mut Ui, state: &mut UiState, commands: &mut Vec<UiAction>, max_w: f32) {
    let current = state.current;
    let in_toolbar = state.rename_in_toolbar;
    if let Some((id, buf)) = state.renaming.as_mut().filter(|(id, _)| in_toolbar && Some(*id) == current) {
        let id = *id;
        let r = ui.add(egui::TextEdit::singleline(buf).font(theme::medium(14.0)).desired_width(max_w.min(420.0)));
        if r.lost_focus() {
            let t = buf.trim().to_owned();
            state.rename_in_toolbar = false;
            if !t.is_empty() && !ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                state.current_title = t.clone();
                commands.push(AppCommand::RenameConversation(id, t));
            }
            state.renaming = None;
        } else {
            r.request_focus();
        }
        return;
    }
    let text = if state.current_title.is_empty() { "Untitled Conversation" } else { &state.current_title };
    let galley = ui.painter().layout_no_wrap(text.to_owned(), theme::medium(14.0), theme::FG);
    let w = galley.size().x.min(max_w);
    let (rect, r) = ui.allocate_exact_size(Vec2::new(w + 12.0, 28.0), Sense::click());
    if r.hovered() {
        ui.painter().rect_filled(rect, 6, theme::ROW_HOVER);
    }
    widgets::paint_truncated(ui, rect.shrink2(Vec2::new(6.0, 0.0)), text, theme::medium(14.0), theme::FG);
    if r.on_hover_text("Rename").clicked() {
        if let Some(id) = state.current {
            state.renaming = Some((id, state.current_title.clone()));
            state.rename_in_toolbar = true;
        }
    }
}

/// "Open IDE" (visual only): 14px icon plus a 12.5px #9d9d9d label; hover shows a #2a2a2a pill.
fn open_ide_button(ui: &mut Ui) {
    let label = "Open IDE";
    let galley = ui.painter().layout_no_wrap(label.to_owned(), theme::font(12.5), theme::FG_SECONDARY);
    let w = galley.size().x + 14.0 + 6.0 + 16.0;
    let (rect, r) = ui.allocate_exact_size(Vec2::new(w, 24.0), Sense::hover());
    let p = ui.painter();
    if r.hovered() {
        p.rect_filled(rect, 6, egui::Color32::from_rgb(0x2a, 0x2a, 0x2a));
    }
    let icon_c = egui::pos2(rect.left() + 8.0 + 7.0, rect.center().y);
    p.text(icon_c, Align2::CENTER_CENTER, icons::ARROW_SQUARE_OUT, theme::icon(14.0), theme::FG_SECONDARY);
    p.galley(egui::pos2(rect.left() + 8.0 + 14.0 + 6.0, rect.center().y - galley.size().y / 2.0), galley, theme::FG_SECONDARY);
    r.on_hover_text(widgets::COMING_SOON);
}

/// Outlined "Open IDE" (History / Customizations): 70x24, 1px `#1c1c1c` stroke, no fill.
fn open_ide_button_outlined(ui: &mut Ui) {
    let (rect, r) = ui.allocate_exact_size(Vec2::new(70.0, 24.0), Sense::hover());
    // The toolbar strip clips 18px short of the window edge; this pill sits past it.
    let p = ui.painter().with_clip_rect(ui.ctx().content_rect());
    let fill = if r.hovered() { egui::Color32::from_rgb(0x2a, 0x2a, 0x2a) } else { egui::Color32::TRANSPARENT };
    p.rect(rect, 6, fill, Stroke::new(1.0, egui::Color32::from_gray(28)), egui::StrokeKind::Inside);
    p.text(rect.center(), Align2::CENTER_CENTER, "Open IDE", theme::font(12.5), theme::FG_SECONDARY);
    r.on_hover_text(widgets::COMING_SOON);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::test_util::{click, context, painted};
    use crate::ui::ChatItem;

    /// The toggle at the right edge of a conversation's toolbar opens the Progress panel, and
    /// closes it again.
    #[test]
    fn the_panel_toggle_opens_and_closes_the_progress_panel() {
        let ctx = context();
        let mut state = UiState {
            current: Some(uuid::Uuid::new_v4()),
            current_title: "Greeting".into(),
            items: vec![ChatItem::User("hello".into())],
            ..UiState::default()
        };
        let mut commands = Vec::new();
        let texts = painted(&ctx, Vec::new(), |ui| show(ui, &mut state, &mut commands));
        // The sidebar toggle at the start of the row has the same icon; the panel's is last.
        let at = texts
            .iter()
            .filter(|(t, _)| t == icons::SIDEBAR_SIMPLE)
            .map(|(_, r)| r.center())
            .max_by(|a, b| a.x.total_cmp(&b.x))
            .expect("the panel toggle");
        assert!(at.x > 1300.0, "at the window edge: {at:?}");
        for open in [true, false] {
            painted(&ctx, vec![egui::Event::PointerMoved(at), click(at, true)], |ui| show(ui, &mut state, &mut commands));
            painted(&ctx, vec![click(at, false)], |ui| show(ui, &mut state, &mut commands));
            assert_eq!(state.progress_open, open);
        }
        assert!(commands.is_empty());
    }
}
