//! Menu bar (AntiCraft / File / View / Window) and the toolbar row: sidebar toggle, back,
//! forward, search, the `Project / Title` breadcrumb, and at the right the overflow menu,
//! "Open IDE" and the side-panel toggles. Both rows sit on the chrome background the sidebar
//! shares (DESIGN.md 2.1, 2.2).

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

/// Title area: the 32px menu bar row over the 36px toolbar row (DESIGN.md 2.1, 2.2). The panel
/// that holds it is filled with the chrome color, so nothing is painted here.
pub fn show(ui: &mut Ui, state: &mut UiState, commands: &mut Vec<UiAction>) {
    ui.spacing_mut().item_spacing.y = 0.0;
    let w = ui.available_width();
    ui.allocate_ui_with_layout(Vec2::new(w, theme::TOP_BAR_HEIGHT), egui::Layout::left_to_right(egui::Align::Center), |ui| {
        ui.set_min_size(Vec2::new(w, theme::TOP_BAR_HEIGHT));
        ui.add_space(8.0);
        menu_bar(ui, state, commands);
    });
    // Toolbar strip centered at y=49 like the reference; it ends at the card's right edge.
    ui.add_space(1.0);
    ui.allocate_ui_with_layout(Vec2::new(w, STRIP), egui::Layout::left_to_right(egui::Align::Center), |ui| {
        ui.set_min_size(Vec2::new(w, STRIP));
        ui.add_space(6.0);
        let right = f32::from(theme::SHELL_GUTTER) + 4.0;
        ui.allocate_ui_with_layout(Vec2::new(ui.available_width() - right, STRIP), egui::Layout::left_to_right(egui::Align::Center), |ui| {
            toolbar(ui, state, commands);
        });
    });
}

pub fn menu_bar(ui: &mut Ui, state: &mut UiState, commands: &mut Vec<UiAction>) {
    ui.horizontal_centered(|ui| {
        ui.spacing_mut().item_spacing.x = 7.0;
        menu_root(ui, "AntiCraft", |ui| {
            if item(ui, "Settings", "Ctrl+,", true) {
                state.open_customize(crate::ui::CustomizeTab::General);
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
            if item(ui, "Settings", "Ctrl+,", true) {
                state.open_customize(crate::ui::CustomizeTab::General);
            }
            ui.separator();
            if item(ui, "Quit", "", true) {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            }
        });
        menu_root(ui, "View", |ui| {
            if item(ui, "Toggle Sidebar", "Ctrl+B", true) {
                state.toggle_sidebar(ui.ctx());
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
        // Sidebar toggle (36x24), filled while the sidebar is open; the 28px buttons that
        // follow sit on a 30px pitch.
        let (rect, r) = ui.allocate_exact_size(Vec2::new(36.0, 24.0), Sense::click());
        if r.hovered() {
            ui.painter().rect_filled(rect, 6, theme::SECONDARY);
        }
        let c = if r.hovered() { theme::FG } else { theme::FG_SECONDARY };
        paint_panel_icon(ui.painter(), rect.center(), PanelSide::Left, state.sidebar_shown, c);
        let tip = if state.sidebar_shown { "Hide sidebar (Ctrl+B)" } else { "Show sidebar (Ctrl+B)" };
        if r.on_hover_text(tip).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
            state.toggle_sidebar(ui.ctx());
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
        if !state.sidebar_shown
            && widgets::icon_button(ui, icons::NOTE_PENCIL, 28.0, "New conversation (Ctrl+N)", !state.busy).clicked()
        {
            state.new_chat();
            commands.push(AppCommand::NewChat);
        }

        // Breadcrumb, aligned with the card's content (x 247 with the sidebar open).
        let show_title = state.view == View::Chat && state.current.is_some() && !state.items.is_empty();
        let content_x = if state.sidebar_shown { theme::SIDEBAR_WIDTH + 6.0 } else { ui.cursor().left() + 12.0 };
        // The overflow button, "Open IDE" and the two panel toggles, with their gaps.
        let right_w = 24.0 + 6.0 + 78.0 + 8.0 + 24.0 + 4.0 + 24.0 + 12.0;
        let left = content_x.max(ui.cursor().left() + 12.0);
        let max_w = (ui.max_rect().right() - right_w - left - 12.0).max(40.0);
        ui.add_space(left - ui.cursor().left());
        if show_title {
            breadcrumb(ui, state, commands, max_w);
        } else if let Some(page) = page_title(state) {
            let r = ui.painter().layout_no_wrap(page.to_owned(), theme::font(13.0), theme::FG_SECONDARY);
            let (rect, _) = ui.allocate_exact_size(Vec2::new(r.size().x + 12.0, 28.0), Sense::hover());
            ui.painter().galley(egui::pos2(rect.left() + 6.0, rect.center().y - r.size().y / 2.0), r, theme::FG_SECONDARY);
        }

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            if show_title {
                // Side-panel toggles at the edge, filled while their panel is open: Progress, then
                // Artifacts; then "Open IDE" and the overflow menu.
                let open = state.progress_open;
                let (tr, tresp) = ui.allocate_exact_size(Vec2::splat(24.0), Sense::click());
                if tresp.hovered() {
                    ui.painter().rect_filled(tr, 6, theme::SECONDARY);
                }
                let c = if open || tresp.hovered() { theme::FG } else { theme::FG_SECONDARY };
                paint_panel_icon(ui.painter(), tr.center(), PanelSide::Right, open, c);
                let tip = if open { "Hide progress" } else { "Show progress" };
                if tresp.on_hover_text(tip).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    state.progress_open = !open;
                }
                ui.add_space(4.0);
                artifacts_toggle(ui, state);
                ui.add_space(8.0);
                open_ide_button(ui, state, commands);
                ui.add_space(6.0);
                let (pr, r) = ui.allocate_exact_size(Vec2::splat(24.0), Sense::click());
                if r.hovered() {
                    ui.painter().rect_filled(pr, 6, theme::SECONDARY);
                }
                let c = if r.hovered() { theme::FG } else { theme::FG_SECONDARY };
                ui.painter().text(pr.center(), Align2::CENTER_CENTER, icons::DOTS_THREE_VERTICAL, theme::icon(15.0), c);
                let r = r.on_hover_text("More");
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
            } else {
                open_ide_button(ui, state, commands);
            }
        });
    });
}

/// Name shown in the toolbar on the pages that are not a conversation.
fn page_title(state: &UiState) -> Option<&'static str> {
    match state.view {
        View::History => Some("Conversation History"),
        View::Chat => None,
    }
}

/// Which edge a panel icon's filled side is on.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PanelSide {
    Left,
    Right,
}

/// 16x13 window-with-panel glyph: an outline whose panel side is filled while that panel is
/// open, as the reference draws its sidebar and side-panel buttons.
pub fn paint_panel_icon(p: &egui::Painter, center: egui::Pos2, side: PanelSide, filled: bool, color: egui::Color32) {
    let frame = egui::Rect::from_center_size(center, Vec2::new(16.0, 13.0));
    p.rect_stroke(frame, 3, Stroke::new(1.3, color), egui::StrokeKind::Inside);
    let pane_w = 5.5;
    let pane = match side {
        PanelSide::Left => egui::Rect::from_min_max(frame.min, egui::pos2(frame.left() + pane_w, frame.bottom())),
        PanelSide::Right => egui::Rect::from_min_max(egui::pos2(frame.right() - pane_w, frame.top()), frame.max),
    };
    if filled {
        let cr = match side {
            PanelSide::Left => egui::CornerRadius { nw: 3, sw: 3, ne: 0, se: 0 },
            PanelSide::Right => egui::CornerRadius { nw: 0, sw: 0, ne: 3, se: 3 },
        };
        p.rect_filled(pane, cr, color);
    } else {
        let x = match side {
            PanelSide::Left => pane.right(),
            PanelSide::Right => pane.left(),
        };
        p.vline(x, pane.y_range(), Stroke::new(1.3, color));
    }
}

/// 24x24 toggle with a glyph; filled while its panel is open.
fn paint_toggle(ui: &Ui, rect: egui::Rect, r: &egui::Response, icon: &str, open: bool) {
    let hot = r.hovered();
    if open || hot {
        ui.painter().rect_filled(rect, 6, if hot { theme::PRESSED } else { theme::SEND_BG });
    }
    let c = if open || hot { theme::FG } else { theme::FG_SECONDARY };
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, icon, theme::icon(14.0), c);
}

/// The Artifacts panel toggle, with an amber dot while a review waits. Its state lives with
/// the panel (egui memory), so the toolbar reads last frame's artifact count.
fn artifacts_toggle(ui: &mut Ui, state: &UiState) {
    let mut panel = super::artifacts_panel::load(ui.ctx(), state.current);
    let n = panel.artifacts.len();
    let reviewing = panel.pending_review.is_some();
    let enabled = n > 0 || panel.open;
    let (rect, r) = ui.allocate_exact_size(Vec2::splat(24.0), if enabled { Sense::click() } else { Sense::hover() });
    if enabled {
        paint_toggle(ui, rect, &r, icons::FILE_TEXT, panel.open);
    } else {
        ui.painter().text(rect.center(), Align2::CENTER_CENTER, icons::FILE_TEXT, theme::icon(14.0), theme::FG_SECONDARY.gamma_multiply(0.4));
    }
    if reviewing {
        ui.painter().circle_filled(rect.center() + Vec2::new(6.0, -6.0), 3.0, theme::WARNING);
    }
    let tip = if reviewing {
        "Review pending".to_owned()
    } else if n == 0 {
        "No artifacts yet".to_owned()
    } else if panel.open {
        format!("Hide artifacts ({n})")
    } else {
        format!("Show artifacts ({n})")
    };
    if r.on_hover_text(tip).clicked() && enabled {
        panel.open = !panel.open;
        super::artifacts_panel::store(ui.ctx(), state.current, panel);
    }
}

/// `Project / Title`: the conversation's project (folder name) and its title; click the title to
/// rename it inline (shares the sidebar rename buffer).
fn breadcrumb(ui: &mut Ui, state: &mut UiState, commands: &mut Vec<UiAction>, max_w: f32) {
    let project = state
        .current
        .and_then(|id| state.conversations.iter().find(|c| c.id == id))
        .and_then(|c| c.workspace.as_deref())
        .or(state.workspace.as_deref())
        .map(super::format::project_label);
    let mut max_w = max_w;
    if let Some(project) = project {
        let g = ui.painter().layout_no_wrap(project, theme::font(13.0), theme::FG_MUTED);
        let w = g.size().x.min(max_w * 0.4);
        let (rect, _) = ui.allocate_exact_size(Vec2::new(w + 6.0, 28.0), Sense::hover());
        let text = g.text().to_owned();
        widgets::paint_truncated(ui, egui::Rect::from_min_max(egui::pos2(rect.left() + 6.0, rect.top()), rect.max), &text, theme::font(13.0), theme::FG_MUTED);
        let (slash, _) = ui.allocate_exact_size(Vec2::new(14.0, 28.0), Sense::hover());
        ui.painter().text(slash.center(), Align2::CENTER_CENTER, "/", theme::font(13.0), theme::FG_DIM);
        max_w = (max_w - w - 20.0).max(40.0);
    }
    title(ui, state, commands, max_w);
}

/// Conversation title; click to rename inline (shares the sidebar rename buffer).
fn title(ui: &mut Ui, state: &mut UiState, commands: &mut Vec<UiAction>, max_w: f32) {
    let current = state.current;
    let in_toolbar = state.rename_in_toolbar;
    if let Some((id, buf)) = state.renaming.as_mut().filter(|(id, _)| in_toolbar && Some(*id) == current) {
        let id = *id;
        let r = ui.add(egui::TextEdit::singleline(buf).font(theme::font(13.0)).desired_width(max_w.min(420.0)));
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
    let galley = ui.painter().layout_no_wrap(text.to_owned(), theme::font(13.0), theme::FG_SECONDARY);
    let w = galley.size().x.min(max_w);
    let (rect, r) = ui.allocate_exact_size(Vec2::new(w + 12.0, 28.0), Sense::click());
    let color = if r.hovered() { theme::FG } else { theme::FG_SECONDARY };
    if r.hovered() {
        ui.painter().rect_filled(rect, 6, theme::SECONDARY);
    }
    widgets::paint_truncated(ui, rect.shrink2(Vec2::new(6.0, 0.0)), text, theme::font(13.0), color);
    if r.on_hover_text("Rename").clicked() {
        if let Some(id) = state.current {
            state.renaming = Some((id, state.current_title.clone()));
            state.rename_in_toolbar = true;
        }
    }
}

/// The project folder "Open IDE" opens: the open conversation's, else the selected one.
fn project_folder(state: &UiState) -> Option<std::path::PathBuf> {
    state
        .current
        .and_then(|id| state.conversations.iter().find(|c| c.id == id))
        .and_then(|c| c.workspace.clone())
        .or_else(|| state.workspace.clone())
}

/// Outlined "Open IDE" pill: opens the project folder in VS Code, or in Explorer when VS Code
/// is not installed (the backend decides, see `AppCommand::OpenInIde`).
fn open_ide_button(ui: &mut Ui, state: &UiState, commands: &mut Vec<UiAction>) {
    let folder = project_folder(state);
    let enabled = folder.is_some();
    let g = ui.painter().layout_no_wrap("Open IDE".to_owned(), theme::font(12.5), theme::FG_SECONDARY);
    let (rect, r) = ui.allocate_exact_size(Vec2::new(g.size().x + 26.0, 24.0), if enabled { Sense::click() } else { Sense::hover() });
    let p = ui.painter();
    let hot = enabled && r.hovered();
    let fill = if hot { theme::SECONDARY } else { egui::Color32::TRANSPARENT };
    p.rect(rect, 12, fill, Stroke::new(1.0, theme::POPUP_BORDER), egui::StrokeKind::Inside);
    let c = if !enabled {
        theme::FG_SECONDARY.gamma_multiply(0.4)
    } else if hot {
        theme::FG
    } else {
        theme::FG_SECONDARY
    };
    p.galley(rect.center() - g.size() / 2.0, g, c);
    let tip = match &folder {
        Some(f) => format!("Open {} in VS Code (or Explorer)", super::format::project_label(f)),
        None => "Open a project folder first".to_owned(),
    };
    let r = r.on_hover_text(tip);
    if r.clicked() {
        if let Some(f) = folder {
            commands.push(AppCommand::OpenInIde(f));
        }
    }
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
        // The toggles are painted shapes: the Artifacts toggle, then the Progress toggle, follow
        // "Open IDE" at the right end of the row.
        let ide = texts.iter().find(|(t, _)| t == "Open IDE").expect("Open IDE").1;
        let at = egui::pos2(ide.right() + 13.0 + 8.0 + 24.0 + 4.0 + 12.0, ide.center().y);
        assert!(at.x > 1300.0, "at the window edge: {at:?}");
        assert!(texts.iter().any(|(t, _)| t == "Greeting"), "the breadcrumb names the conversation");
        for open in [true, false] {
            painted(&ctx, vec![egui::Event::PointerMoved(at), click(at, true)], |ui| show(ui, &mut state, &mut commands));
            painted(&ctx, vec![click(at, false)], |ui| show(ui, &mut state, &mut commands));
            assert_eq!(state.progress_open, open);
        }
        assert!(commands.is_empty());
    }
}
