//! Left sidebar (DESIGN.md 2.3): "New Conversation", the nav rows, Projects grouped by
//! workspace with a status mark per conversation, and Settings (or the sign-in pill) at the
//! bottom. Conversation History has no row of its own: the toolbar's search button opens it.

use egui::{Align2, Rect, Sense, Ui, Vec2};

use super::format;
use super::{icons, login, theme, widgets, CustomizeTab, UiAction, UiState, View};
use crate::app::AppCommand;
use crate::auth::AuthStatus;

/// Single-line row pitch from the reference: 28px row plus a 2px gap.
const CONV_ROW_H: f32 = 30.0;
/// Visual height of a conversation row inside its pitch.
const CONV_ROW_VIS: f32 = 28.0;
/// Extra space between workspace groups (reference: last row to next header is 34px).
const GROUP_GAP: f32 = 4.0;
/// Conversations a project lists before "See all (N)".
pub const PROJECT_ROWS: usize = 5;
/// Text inset of nav rows, group headers and conversation rows (reference x 35).
const TEXT_X: f32 = 27.0;

/// What a conversation row shows at its right edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowStatus {
    /// Its turn is running.
    Working,
    /// Its turn waits for an approval or a review.
    ActionRequired,
    /// Its last turn ended while it was not on screen.
    Unread,
    Idle,
}

impl RowStatus {
    fn label(self) -> Option<&'static str> {
        match self {
            RowStatus::Working => Some("Working"),
            RowStatus::ActionRequired => Some("Action required"),
            RowStatus::Unread => Some("Unread"),
            RowStatus::Idle => None,
        }
    }
}

pub fn row_status(state: &UiState, id: uuid::Uuid) -> RowStatus {
    if state.running == Some(id) {
        let waiting = state.current == Some(id) && (state.pending_confirm.is_some() || state.pending_review.is_some());
        if waiting {
            RowStatus::ActionRequired
        } else {
            RowStatus::Working
        }
    } else if state.unread.contains(&id) {
        RowStatus::Unread
    } else {
        RowStatus::Idle
    }
}

pub fn show(ui: &mut Ui, state: &mut UiState, commands: &mut Vec<UiAction>) {
    // Reference nav pitch is 34 (row 30 + gap 4); "New Conversation" starts at the toolbar's
    // bottom edge.
    ui.spacing_mut().item_spacing.y = 4.0;
    let home = state.is_home();
    if widgets::nav_row_boxed(ui, icons::PLUS, "New Conversation", home, !state.busy).clicked() {
        state.new_chat();
        commands.push(AppCommand::NewChat);
    }
    ui.add_space(1.0);
    widgets::nav_row(ui, icons::CLOCK_COUNTER_CLOCKWISE, "Automations", false, false);
    // Projects header text lands at the reference y 194..206.
    ui.add_space(14.0);

    // The bottom row is laid out first so the project list takes the remaining space.
    let bottom_h = 40.0;
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
        bottom_row(ui, state, commands);
    });
}

/// "Settings" once any account is signed in (the account itself is under Settings >
/// Account); before that, the sign-in pill with its gear.
fn bottom_row(ui: &mut Ui, state: &mut UiState, commands: &mut Vec<UiAction>) {
    let account = state
        .shown_account()
        .map(|v| v.email.clone().unwrap_or_else(|| v.provider.label().to_owned()))
        .or_else(|| match &state.auth {
            AuthStatus::LoggedIn { email } => Some(email.clone().unwrap_or_else(|| "Google account".to_owned())),
            _ => None,
        });
    let Some(account) = account else {
        login::account_row(ui, state, commands);
        return;
    };
    let selected = state.customize_open && state.customize_tab == CustomizeTab::General;
    let r = widgets::nav_row(ui, icons::GEAR_SIX, "Settings", selected, true).on_hover_text(format!("Settings · signed in as {account}"));
    if r.clicked() {
        state.open_customize(CustomizeTab::General);
    }
}

fn projects_header(ui: &mut Ui, state: &mut UiState, commands: &mut Vec<UiAction>) {
    let w = ui.available_width();
    let (rect, r) = ui.allocate_exact_size(Vec2::new(w, 22.0), Sense::click());
    let hovered = ui.rect_contains_pointer(rect);
    let p = ui.painter();
    let label = p.text(rect.left_center() + Vec2::new(8.0, 0.0), Align2::LEFT_CENTER, "Projects", theme::font(12.5), theme::FG_MUTED);
    if hovered {
        let caret = if state.projects_collapsed { icons::CARET_RIGHT } else { icons::CARET_DOWN };
        p.text(egui::pos2(label.right() + 6.0, rect.center().y), Align2::LEFT_CENTER, caret, theme::icon(11.0), theme::FG_MUTED);
    }
    let add = Rect::from_min_size(egui::pos2(rect.right() - 24.0, rect.center().y - 11.0), Vec2::splat(22.0));
    if widgets::overlay_icon(ui, add, r.id.with("add"), icons::FOLDER_PLUS, "Open folder...", !state.busy).clicked() {
        commands.push(AppCommand::PickWorkspace);
    } else if r.clicked() {
        state.projects_collapsed = !state.projects_collapsed;
    }
    ui.add_space(4.0);
}

/// The rows a project lists: its newest [`PROJECT_ROWS`] (plus the open conversation when it
/// is older than those), or all of them once "See all" was chosen. Returns the rows and
/// whether some were left out.
pub fn shown_rows(convs: &[usize], current: Option<usize>, expanded: bool) -> (Vec<usize>, bool) {
    if expanded || convs.len() <= PROJECT_ROWS {
        return (convs.to_vec(), false);
    }
    let mut rows: Vec<usize> = convs[..PROJECT_ROWS].to_vec();
    if let Some(c) = current.filter(|c| convs[PROJECT_ROWS..].contains(c)) {
        rows.push(c);
    }
    (rows, true)
}

/// Conversations grouped by workspace (most recently active group first), newest first within a group.
fn projects(ui: &mut Ui, state: &mut UiState, commands: &mut Vec<UiAction>) {
    if state.conversations.is_empty() {
        ui.add_space(6.0);
        ui.label(egui::RichText::new("No conversations yet. Use the folder button to open a project.").font(theme::font(12.0)).color(theme::FG_MUTED));
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
        let key = ws.map(std::path::Path::to_path_buf);
        let expanded = state.expanded_projects.contains(&key);
        let current = state.current.and_then(|id| convs.iter().copied().find(|&i| list[i].id == id));
        let (rows, more) = shown_rows(convs, current, expanded);
        for i in rows {
            // Rows scrolled out of view only reserve their slot.
            let slot = Rect::from_min_size(ui.cursor().min, Vec2::new(ui.available_width(), CONV_ROW_H));
            if !ui.is_rect_visible(slot) {
                ui.allocate_space(slot.size());
                continue;
            }
            conversation_row(ui, state, commands, &list[i], now);
        }
        if more || (expanded && convs.len() > PROJECT_ROWS) {
            let label = if more { format!("See all ({})", convs.len()) } else { "Show less".to_owned() };
            if see_all_row(ui, &label).clicked() {
                if more {
                    state.expanded_projects.insert(key);
                } else {
                    state.expanded_projects.remove(&key);
                }
            }
        }
    }
    // Rows never replace the list; a fresh one only arrives through `apply` between frames.
    if state.conversations.is_empty() {
        state.conversations = list;
    }
}

/// "See all (N)" / "Show less": dim text at the rows' text inset.
fn see_all_row(ui: &mut Ui, label: &str) -> egui::Response {
    let w = ui.available_width();
    let (rect, r) = ui.allocate_exact_size(Vec2::new(w, CONV_ROW_H), Sense::click());
    let c = if r.hovered() { theme::FG_SECONDARY } else { theme::FG_DIM };
    ui.painter().text(egui::pos2(rect.left() + TEXT_X, rect.center().y), Align2::LEFT_CENTER, label, theme::font(12.5), c);
    r.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// 30px workspace header: folder icon and a muted 13px label at the rows' text inset.
fn group_header(ui: &mut Ui, label: &str, path: Option<&std::path::Path>) {
    let w = ui.available_width();
    let (rect, r) = ui.allocate_exact_size(Vec2::new(w, CONV_ROW_H), Sense::hover());
    let p = ui.painter();
    let c = rect.left_center();
    p.text(c + Vec2::new(8.0, 0.0), Align2::LEFT_CENTER, icons::FOLDER, theme::icon(14.0), theme::FG_MUTED);
    let text_rect = Rect::from_min_max(egui::pos2(rect.left() + TEXT_X, rect.top()), egui::pos2(rect.right() - 10.0, rect.bottom()));
    widgets::paint_truncated(ui, text_rect, label, theme::font(13.0), theme::FG_MUTED);
    if let Some(path) = path {
        r.on_hover_text(path.display().to_string());
    }
}

/// Status mark at `at` (the right end of a row): spinner, amber or blue dot.
fn paint_status(ui: &Ui, at: egui::Pos2, status: RowStatus) {
    match status {
        RowStatus::Working => {
            let s = Rect::from_center_size(at, Vec2::splat(12.0));
            egui::Spinner::new().size(12.0).color(theme::FG_MUTED).paint_at(ui, s);
            ui.ctx().request_repaint();
        }
        RowStatus::ActionRequired | RowStatus::Unread => {
            let c = if status == RowStatus::Unread { theme::UNREAD } else { theme::WARNING };
            ui.painter().circle_filled(at, 6.0, c.gamma_multiply(0.18));
            ui.painter().circle_filled(at, 3.5, c);
        }
        RowStatus::Idle => {}
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
        let r = ui.add_sized(Vec2::new(w, 30.0), egui::TextEdit::singleline(buf).font(theme::font(13.0)));
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
    // Full-width highlight; the 2px remainder of the pitch is the row gap.
    let rect = Rect::from_min_size(slot.min, Vec2::new(w, CONV_ROW_VIS));
    let hovered = ui.rect_contains_pointer(slot);
    let selected = state.current == Some(c.id) && state.view == View::Chat;
    let p = ui.painter();
    if selected {
        p.rect(rect, 6, theme::ROW_SELECTED, egui::Stroke::new(1.0, theme::ROW_SELECTED_EDGE), egui::StrokeKind::Inside);
    } else if hovered {
        p.rect_filled(rect, 6, theme::ROW_HOVER);
    }
    let title = if c.title.is_empty() { "Untitled Conversation" } else { c.title.as_str() };
    // The title stops before what the row's right edge shows: the hover actions, or the
    // delete confirmation (armed from the toolbar or History too, with no pointer here).
    let right_reserve = if state.confirm_delete == Some(c.id) {
        58.0
    } else if hovered {
        82.0
    } else {
        40.0
    };
    let title_rect = Rect::from_min_max(egui::pos2(rect.left() + TEXT_X, rect.top()), egui::pos2(rect.right() - right_reserve, rect.bottom()));
    let color = if selected { theme::FG } else { theme::FG_SECONDARY };
    widgets::paint_truncated(ui, title_rect, title, theme::font(13.0), color);

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

    let status = row_status(state, c.id);
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
    } else if status != RowStatus::Idle {
        paint_status(ui, egui::pos2(rect.right() - 14.0, rect.center().y), status);
    } else {
        p.text(
            egui::pos2(rect.right() - 10.0, rect.center().y),
            Align2::RIGHT_CENTER,
            format::relative_time(c.updated_at, now),
            theme::font(12.0),
            theme::FG_MUTED,
        );
    }
    // Hover card: the title, plus the state when there is one (Working, Action required, Unread).
    let tip = match status.label() {
        Some(s) => format!("{title}\n{s}"),
        None => title.to_owned(),
    };
    let r = r.on_hover_text(tip);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::test_util::{click, context, painted};

    #[test]
    fn a_project_lists_five_rows_and_keeps_the_open_one() {
        let convs: Vec<usize> = (0..8).collect();
        assert_eq!(shown_rows(&convs, None, false), (vec![0, 1, 2, 3, 4], true));
        assert_eq!(shown_rows(&convs, Some(6), false), (vec![0, 1, 2, 3, 4, 6], true));
        assert_eq!(shown_rows(&convs, Some(2), false), (vec![0, 1, 2, 3, 4], true));
        assert_eq!(shown_rows(&convs, None, true), (convs.clone(), false));
        assert_eq!(shown_rows(&convs[..5], None, false), (convs[..5].to_vec(), false));
    }

    fn summary(title: &str, ws: &str, age_min: i64) -> crate::store::ConversationSummary {
        crate::store::ConversationSummary {
            id: uuid::Uuid::new_v4(),
            title: title.into(),
            workspace: Some(std::path::PathBuf::from(ws)),
            model: String::new(),
            updated_at: chrono::Utc::now() - chrono::Duration::minutes(age_min),
        }
    }

    /// Conversation History has no sidebar row any more; "See all (N)" opens a project's list.
    #[test]
    fn see_all_expands_a_long_project() {
        let ctx = context();
        let mut state = UiState { conversations: (0..7).map(|i| summary(&format!("Chat {i}"), r"C:\work\app", i)).collect(), ..UiState::default() };
        let mut commands = Vec::new();
        let texts = painted(&ctx, Vec::new(), |ui| show(ui, &mut state, &mut commands));
        assert!(!texts.iter().any(|(t, _)| t == "Conversation History"));
        assert!(texts.iter().any(|(t, _)| t == "Chat 4"));
        assert!(!texts.iter().any(|(t, _)| t == "Chat 5"));
        let at = texts.iter().find(|(t, _)| t == "See all (7)").expect("See all").1.center();
        painted(&ctx, vec![egui::Event::PointerMoved(at), click(at, true)], |ui| show(ui, &mut state, &mut commands));
        painted(&ctx, vec![click(at, false)], |ui| show(ui, &mut state, &mut commands));
        let texts = painted(&ctx, Vec::new(), |ui| show(ui, &mut state, &mut commands));
        assert!(texts.iter().any(|(t, _)| t == "Chat 6"));
        assert!(texts.iter().any(|(t, _)| t == "Show less"));
    }

    #[test]
    fn status_follows_the_turn_and_unread_marks() {
        let mut state = UiState::default();
        let id = uuid::Uuid::new_v4();
        assert_eq!(row_status(&state, id), RowStatus::Idle);
        state.unread.insert(id);
        assert_eq!(row_status(&state, id), RowStatus::Unread);
        state.running = Some(id);
        state.current = Some(id);
        assert_eq!(row_status(&state, id), RowStatus::Working);
        let (tx, _rx) = tokio::sync::oneshot::channel();
        state.pending_review = Some(crate::agent::orchestrate::ReviewRequest {
            call_id: "r".into(),
            artifacts: Vec::new(),
            message: String::new(),
            respond: tx,
        });
        assert_eq!(row_status(&state, id), RowStatus::ActionRequired);
    }
}
