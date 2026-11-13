//! Progress panel: the steps of the conversation's current turn, at the right of the transcript.
//!
//! It opens from the toolbar, with the panel toggle at the window edge beside "Open IDE", and is
//! closed otherwise. Nothing about progress is drawn over or beside the transcript, so the
//! column a turn starts in keeps its width while tools run. A turn with task groups (Planning
//! mode) lists those groups with their status; any other turn lists its tool steps, each marked
//! once its result is in.

use egui::{Align2, Color32, Rect, RichText, Sense, Stroke, Ui, Vec2};

use super::conversation::tool_summary;
use super::taskgroups::{self, Derived, TaskGroup};
use super::{icons, theme, widgets, ChatItem, ToolCard, UiState};
use crate::agent::orchestrate::{self, TaskStatus};

/// Panel width; it gives way on a narrow window.
pub const WIDTH: f32 = 300.0;
/// Height of a step or task row.
const ROW_H: f32 = 26.0;
/// Text inset of a row: the status mark sits in front of it.
const TEXT_X: f32 = 24.0;

/// Tool steps of the last turn, orchestration calls excluded.
pub fn turn_steps(items: &[ChatItem]) -> Vec<&ToolCard> {
    let start = items.iter().rposition(|i| matches!(i, ChatItem::User(_))).unwrap_or(0);
    items[start..]
        .iter()
        .filter_map(|i| if let ChatItem::Tool(c) = i { Some(c) } else { None })
        .filter(|c| !orchestrate::is_orchestration_tool(&c.name))
        .collect()
}

/// Draw the panel into `ui`, the inside of a right side panel. True when it was closed.
pub fn show(ui: &mut Ui, state: &UiState, groups: &Derived) -> bool {
    let tasks = &groups.groups[groups.last_turn..];
    let steps = turn_steps(&state.items);
    let (done, total) = if tasks.is_empty() {
        (steps.iter().filter(|c| c.result.is_some()).count(), steps.len())
    } else {
        (tasks.iter().filter(|g| g.status == TaskStatus::Completed).count(), tasks.len())
    };
    let mut close = false;
    ui.spacing_mut().item_spacing = Vec2::new(8.0, 4.0);
    ui.horizontal(|ui| {
        ui.set_height(26.0);
        ui.label(RichText::new("Progress").font(theme::medium(13.0)).color(theme::FG));
        if total > 0 {
            ui.label(RichText::new(format!("{done}/{total}")).font(theme::font(12.0)).color(theme::FG_MUTED));
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            close = widgets::icon_button(ui, icons::X, 24.0, "Close", true).clicked();
        });
    });
    ui.add_space(2.0);
    let r = ui.available_rect_before_wrap();
    ui.painter().hline(r.x_range(), r.top(), Stroke::new(1.0, theme::CARD_BORDER));
    ui.add_space(8.0);
    if total == 0 {
        ui.add_space(24.0);
        ui.vertical_centered(|ui| {
            ui.set_max_width(240.0);
            ui.label(RichText::new("No steps yet").font(theme::medium(13.0)).color(theme::FG_SECONDARY));
            let hint = "What the agent reads, searches and runs in this turn is listed here.";
            ui.add(egui::Label::new(RichText::new(hint).font(theme::font(12.5)).color(theme::FG_MUTED)).wrap());
        });
        return close;
    }
    egui::ScrollArea::vertical().id_salt("progress_rows").auto_shrink([false, false]).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        if tasks.is_empty() {
            for card in &steps {
                step_row(ui, card, state.busy);
            }
        } else {
            for group in tasks {
                task_row(ui, group);
            }
        }
    });
    close
}

/// One tool step: a spinner while the turn runs it, a check or a cross once its result is in,
/// then what it did ("Read web/README.md").
fn step_row(ui: &mut Ui, card: &ToolCard, busy: bool) {
    let (rect, r) = ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_H), Sense::hover());
    let mark = egui::pos2(rect.left() + 8.0, rect.center().y);
    match &card.result {
        None if busy => {
            egui::Spinner::new().size(11.0).color(theme::FG_MUTED).paint_at(ui, Rect::from_center_size(mark, Vec2::splat(12.0)));
        }
        // Left over from a turn that ended before the step did.
        None => {
            ui.painter().circle_stroke(mark, 5.0, Stroke::new(1.0, theme::FG_MUTED));
        }
        Some((ok, _)) => {
            let (icon, color) = if *ok { (icons::CHECK, theme::SUCCESS) } else { (icons::X, theme::ERROR) };
            ui.painter().text(mark, Align2::CENTER_CENTER, icon, theme::icon(13.0), color);
        }
    }
    let (verb, target) = tool_summary(card);
    let label = if target.is_empty() { verb.to_owned() } else { format!("{verb} {target}") };
    let text = Rect::from_min_max(egui::pos2(rect.left() + TEXT_X, rect.top()), rect.right_bottom());
    let color = if card.result.is_some() { theme::FG_SECONDARY } else { theme::FG };
    if !paint_fitted(ui, text, &label, theme::font(13.0), color) {
        r.on_hover_text(label);
    }
}

/// One task group: its status mark and title. The live group also shows its current step.
fn task_row(ui: &mut Ui, group: &TaskGroup) {
    let step = group.steps.last().filter(|_| group.is_open());
    let h = if step.is_some() { ROW_H + 16.0 } else { ROW_H };
    let (rect, r) = ui.allocate_exact_size(Vec2::new(ui.available_width(), h), Sense::hover());
    let y = rect.top() + ROW_H / 2.0;
    taskgroups::paint_status(ui, egui::pos2(rect.left() + 8.0, y), group.status);
    let line = |top: f32, bottom: f32| Rect::from_min_max(egui::pos2(rect.left() + TEXT_X, top), egui::pos2(rect.right(), bottom));
    let color = if group.is_open() { theme::FG } else { theme::FG_SECONDARY };
    let mut fits = paint_fitted(ui, line(y - 9.0, y + 9.0), &group.title, theme::font(13.0), color);
    if let Some(step) = step {
        fits &= paint_fitted(ui, line(y + 9.0, y + 25.0), step, theme::font(12.0), theme::FG_MUTED);
    }
    if !fits {
        let tip = step.map_or_else(|| group.title.clone(), |s| format!("{}\n{s}", group.title));
        r.on_hover_text(tip);
    }
}

/// Paint `text` on one line, cut to `rect`. False when it had to be cut.
fn paint_fitted(ui: &Ui, rect: Rect, text: &str, font: egui::FontId, color: Color32) -> bool {
    widgets::paint_truncated(ui, rect, text, font.clone(), color);
    ui.painter().layout_no_wrap(text.to_owned(), font, color).size().x <= rect.width()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::test_util::{click, context, painted};

    fn step(id: &str, path: &str, result: Option<bool>) -> ChatItem {
        ChatItem::Tool(ToolCard {
            call_id: id.into(),
            name: "read_file".into(),
            args: serde_json::json!({"file_path": path}),
            result: result.map(|ok| (ok, String::new())),
        })
    }

    fn boundary(id: &str, name: &str, summary: &str) -> ChatItem {
        ChatItem::Tool(ToolCard {
            call_id: id.into(),
            name: orchestrate::TASK_BOUNDARY.into(),
            args: serde_json::json!({"task_name": name, "summary": summary, "phase": "executing"}),
            result: Some((true, "ok".into())),
        })
    }

    /// The panel as a 300 px side panel draws it; the texts, and whether it was closed.
    fn drawn(ctx: &egui::Context, state: &UiState, events: Vec<egui::Event>) -> (Vec<(String, Rect)>, bool) {
        let groups = taskgroups::derive(&state.items, state.busy, false);
        let mut closed = false;
        let texts = painted(ctx, events, |ui| {
            let rect = Rect::from_min_size(egui::pos2(1100.0, 0.0), Vec2::new(WIDTH - 24.0, 900.0));
            ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| closed |= show(ui, state, &groups));
        });
        (texts, closed)
    }

    fn has(texts: &[(String, Rect)], text: &str) -> bool {
        texts.iter().any(|(t, _)| t == text)
    }

    /// Every step of the turn is listed, not only the five the old card had room for, with how
    /// many are done. The steps of an earlier turn are not.
    #[test]
    fn the_panel_lists_every_step_of_the_turn() {
        let ctx = context();
        let mut items = vec![ChatItem::User("first".into()), step("0", "old.rs", Some(true)), ChatItem::User("second".into())];
        items.extend((1..=8).map(|i| step(&i.to_string(), &format!("src/f{i}.rs"), Some(i != 4))));
        items.push(step("9", "src/f9.rs", None));
        let state = UiState { items, busy: true, ..UiState::default() };
        let (texts, closed) = drawn(&ctx, &state, Vec::new());
        assert!(!closed);
        assert!(has(&texts, "Progress") && has(&texts, "8/9"), "{texts:?}");
        for i in 1..=8 {
            assert!(has(&texts, &format!("Read src/f{i}.rs")), "step {i}: {texts:?}");
        }
        assert!(has(&texts, "Reading src/f9.rs"), "the running step: {texts:?}");
        assert!(!has(&texts, "Read old.rs"), "an earlier turn's step: {texts:?}");
    }

    /// In Planning mode the task groups are listed, the live one with its current step; their
    /// tool calls are not.
    #[test]
    fn a_planning_turn_lists_its_task_groups() {
        let ctx = context();
        let items = vec![
            ChatItem::User("add retries".into()),
            boundary("b1", "Research the client", "Reading the client"),
            step("1", "src/client.rs", Some(true)),
            boundary("b2", "Add RetryPolicy", "Wrapping send_chunk"),
            step("2", "src/retry.rs", None),
        ];
        let state = UiState { items, busy: true, ..UiState::default() };
        let (texts, _) = drawn(&ctx, &state, Vec::new());
        assert!(has(&texts, "1/2"), "{texts:?}");
        assert!(has(&texts, "Research the client") && has(&texts, "Add RetryPolicy"), "{texts:?}");
        assert!(has(&texts, "Wrapping send_chunk"), "the live group's step: {texts:?}");
        assert!(!has(&texts, "Reading the client"), "a finished group's step: {texts:?}");
        assert!(!has(&texts, "Read src/client.rs"), "{texts:?}");
    }

    /// A turn without steps says so, and the close button closes the panel.
    #[test]
    fn an_empty_turn_says_so_and_the_panel_closes() {
        let ctx = context();
        let items = vec![ChatItem::User("hi".into()), ChatItem::TurnHeader { duration: None }, ChatItem::Assistant("Hello!".into())];
        let state = UiState { items, ..UiState::default() };
        let (texts, _) = drawn(&ctx, &state, Vec::new());
        assert!(has(&texts, "No steps yet"), "{texts:?}");
        assert!(!texts.iter().any(|(t, _)| t.contains('/')), "no count without steps: {texts:?}");
        let x = texts.iter().find(|(t, _)| t == icons::X).map(|(_, r)| r.center()).expect("the close button");
        drawn(&ctx, &state, vec![egui::Event::PointerMoved(x), click(x, true)]);
        let (_, closed) = drawn(&ctx, &state, vec![click(x, false)]);
        assert!(closed);
    }
}
