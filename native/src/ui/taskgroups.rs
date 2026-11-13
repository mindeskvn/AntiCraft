//! Task group cards (ORCHESTRATION.md section E.1).
//!
//! A task group is opened by the agent's `task_boundary` call. It owns the regular tool
//! cards that follow it in the same turn until the next boundary with another name. The
//! card shows a status mark, the title, a phase tag, the progress steps (one per distinct
//! summary; the latest is bright), edited-file pills, and the tool calls, which stay
//! collapsed by default.
//!
//! Groups are derived from the transcript ([`derive`]) instead of being stored: the
//! `task_boundary` call reaches the UI as an ordinary tool card (live and when a stored
//! conversation is reloaded), so no extra state has to be kept in sync. The agent's
//! `TaskUpdate` events ([`TaskUpdate`]) refine that derivation while a turn runs: they carry
//! statuses the transcript cannot show yet (the group closed by `task_complete`, a group
//! that waits on a confirmation) and the groups `Stop` cancelled. Status is settled from the
//! turn's outcome: a group still open when its turn ended with "Stopped." becomes
//! [`TaskStatus::Cancelled`], one superseded by a later group or by a normal finish becomes
//! [`TaskStatus::Completed`], and the live group shows "Waiting for you" while a
//! confirmation or review is pending.

use std::collections::HashMap;

use egui::{Align2, Rect, Sense, Stroke, Ui, Vec2};

use super::{icons, theme, widgets, ChatItem, ToolCard};
use crate::agent::orchestrate::{self, TaskBoundary, TaskPhase, TaskStatus};

/// Steps shown while the card is collapsed (older ones are behind the toggle).
const COLLAPSED_STEPS: usize = 3;
/// Left indent shared by steps, file pills, the tool caption and the tool rows (the status
/// mark column).
pub const LEFT_INDENT: f32 = 22.0;

/// UI model of one task group.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskGroup {
    pub title: String,
    pub status: TaskStatus,
    pub phase: TaskPhase,
    /// Distinct summaries in arrival order; the last one is the current step.
    pub steps: Vec<String>,
    /// Indices (into the chat item list) of the tool calls run while this group was active.
    pub tools: Vec<usize>,
    /// Workspace files edited inside the group (file pills), deduplicated.
    pub edited_files: Vec<String>,
    /// Tool calls of `tools` that have a result (progress numerator).
    pub tools_done: usize,
    /// Index of the `task_boundary` item that opened the group (stable key for expansion).
    pub anchor: usize,
}

impl TaskGroup {
    pub fn from_boundary(b: &TaskBoundary) -> Self {
        let mut g = Self {
            title: b.task_name.trim().to_owned(),
            status: b.status,
            phase: b.phase,
            steps: Vec::new(),
            tools: Vec::new(),
            edited_files: Vec::new(),
            tools_done: 0,
            anchor: 0,
        };
        g.update(b);
        g
    }

    /// Apply a later boundary with the same name.
    pub fn update(&mut self, b: &TaskBoundary) {
        self.status = b.status;
        self.phase = b.phase;
        let s = b.summary.trim();
        if !s.is_empty() && self.steps.last().map(String::as_str) != Some(s) {
            self.steps.push(s.to_owned());
        }
    }

    /// Take the status of an agent update (phase and steps come from the transcript cards,
    /// which are authoritative for what the group contains).
    pub fn update_status(&mut self, b: &TaskBoundary) {
        self.status = b.status;
    }

    /// Mark an unfinished group as cancelled (Stop pressed).
    pub fn cancel(&mut self) {
        if self.is_open() {
            self.status = TaskStatus::Cancelled;
        }
    }

    pub fn is_open(&self) -> bool {
        matches!(self.status, TaskStatus::InProgress | TaskStatus::WaitingForUser)
    }

    /// Attach tool item `idx`; edits (not failed ones) add a file pill.
    pub fn add_tool(&mut self, idx: usize, card: &ToolCard) {
        self.tools.push(idx);
        if card.result.is_some() {
            self.tools_done += 1;
        }
        if !matches!(card.name.as_str(), "write_file" | "replace") || matches!(card.result, Some((false, _))) {
            return;
        }
        if let Some(p) = card.args.get("file_path").and_then(|v| v.as_str()).map(str::trim).filter(|p| !p.is_empty()) {
            if !self.edited_files.iter().any(|f| f == p) {
                self.edited_files.push(p.to_owned());
            }
        }
    }
}

pub fn status_label(s: TaskStatus) -> (&'static str, egui::Color32) {
    match s {
        TaskStatus::InProgress => ("In progress", theme::ACCENT_BLUE),
        TaskStatus::WaitingForUser => ("Waiting for you", theme::WARNING),
        TaskStatus::Completed => ("Done", theme::SUCCESS),
        TaskStatus::Failed => ("Failed", theme::ERROR),
        TaskStatus::Cancelled => ("Cancelled", theme::FG_SECONDARY),
    }
}

pub fn phase_label(p: TaskPhase) -> &'static str {
    match p {
        TaskPhase::Planning => "Planning",
        TaskPhase::Executing => "Executing",
        TaskPhase::Verifying => "Verifying",
    }
}

/// Task groups of a transcript and which items they absorb.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Derived {
    pub groups: Vec<TaskGroup>,
    /// Item index -> group index, for boundaries and the tool cards a group owns.
    pub owner: HashMap<usize, usize>,
    /// Groups of the last turn (`groups[last_turn..]`).
    pub last_turn: usize,
}

/// Turn-ending notices that mean the turn did not finish normally.
fn is_stop_notice(t: &str) -> bool {
    t.starts_with("Stopped") || t.starts_with("Tool call rejected")
}

/// Settle the open groups of one turn. `active` is the group that was current at the end.
fn settle(groups: &mut [TaskGroup], active: Option<usize>, ended: bool, stopped: bool, waiting: bool) {
    for (k, g) in groups.iter_mut().enumerate() {
        if !g.is_open() {
            continue;
        }
        if ended {
            if stopped {
                g.cancel();
            } else {
                g.status = TaskStatus::Completed;
            }
        } else if Some(k) != active {
            g.status = TaskStatus::Completed;
        } else if waiting {
            g.status = TaskStatus::WaitingForUser;
        }
    }
}

/// One `AgentEvent::TaskUpdate` as the UI remembers it: the boundary plus the turn it arrived
/// in (the [`turn_of`] index at that moment).
#[derive(Debug, Clone, PartialEq)]
pub struct TaskUpdate {
    pub boundary: TaskBoundary,
    /// Turn index of the update (see [`turn_of`]).
    pub turn: usize,
}

/// Turn index a new update belongs to: `n-1` after `n` user prompts, 0 before the first.
pub fn turn_of(items: &[ChatItem]) -> usize {
    items.iter().filter(|i| matches!(i, ChatItem::User(_))).count().saturating_sub(1)
}

/// Apply the agent's updates on top of the transcript-derived groups.
///
/// The transcript decides what a group *is* (name, steps, tools); an update decides what it
/// *shows* when the transcript cannot know yet: the `Completed` status `task_complete` set,
/// which the card can show before the turn-ending items arrive. Only the live turn is open to
/// updates — an older turn is settled by the transcript (Stop notices, later groups) — and the
/// last group with that name is the one that changes. Updates never add or remove groups.
pub fn apply_updates(d: &mut Derived, updates: &[TaskUpdate]) {
    for u in updates.iter().filter(|u| u.turn == d.last_turn) {
        let name = u.boundary.task_name.trim();
        if let Some(g) = d.groups.iter_mut().rev().find(|g| g.title == name) {
            g.update_status(&u.boundary);
        }
    }
}

/// Build the task groups of `items`. `busy`: the last turn is still running; `waiting`: a
/// confirmation or review is pending in it.
pub fn derive(items: &[ChatItem], busy: bool, waiting: bool) -> Derived {
    let mut d = Derived::default();
    let mut turn_start = 0usize;
    let mut active: Option<usize> = None;
    let mut stopped = false;
    for (i, item) in items.iter().enumerate() {
        match item {
            ChatItem::User(_) => {
                settle(&mut d.groups[turn_start..], active.map(|a| a - turn_start), true, stopped, false);
                turn_start = d.groups.len();
                active = None;
                stopped = false;
            }
            ChatItem::Notice(t) if is_stop_notice(t) => stopped = true,
            ChatItem::Tool(card) if card.name == orchestrate::TASK_BOUNDARY => {
                let Ok(b) = orchestrate::parse_task_boundary(&card.args) else { continue };
                let name = b.task_name.trim();
                let found = (turn_start..d.groups.len()).rev().find(|&g| d.groups[g].title == name);
                let g = match found {
                    Some(g) => {
                        d.groups[g].update(&b);
                        g
                    }
                    None => {
                        let mut g = TaskGroup::from_boundary(&b);
                        g.anchor = i;
                        d.groups.push(g);
                        d.groups.len() - 1
                    }
                };
                d.owner.insert(i, g);
                active = Some(g);
            }
            ChatItem::Tool(card) if !orchestrate::is_orchestration_tool(&card.name) => {
                if let Some(g) = active {
                    d.groups[g].add_tool(i, card);
                    d.owner.insert(i, g);
                }
            }
            _ => {}
        }
    }
    settle(&mut d.groups[turn_start..], active.map(|a| a - turn_start), !busy, stopped, waiting);
    d.last_turn = turn_start;
    d
}

/// Status mark at the start of the header (spinner while running).
pub fn paint_status(ui: &Ui, center: egui::Pos2, status: TaskStatus) {
    let (_, color) = status_label(status);
    match status {
        TaskStatus::InProgress => {
            egui::Spinner::new().size(12.0).color(color).paint_at(ui, Rect::from_center_size(center, Vec2::splat(12.0)));
        }
        TaskStatus::WaitingForUser => {
            ui.painter().circle_stroke(center, 5.5, Stroke::new(1.5, color));
            ui.painter().circle_filled(center, 2.5, color);
        }
        TaskStatus::Completed => {
            ui.painter().text(center, Align2::CENTER_CENTER, icons::CHECK_CIRCLE, theme::icon(15.0), color);
        }
        TaskStatus::Failed => {
            ui.painter().text(center, Align2::CENTER_CENTER, icons::X, theme::icon(14.0), color);
        }
        TaskStatus::Cancelled => {
            ui.painter().text(center, Align2::CENTER_CENTER, icons::PROHIBIT, theme::icon(14.0), color);
        }
    }
}

/// Header row: status mark, title, then the meta on the right: a muted phase followed by
/// the status word in its status color (a tinted pill while waiting or failed), and the
/// caret. The title keeps at least 55% of the row; when the meta does not fit, the phase is
/// dropped first and the status stays. Returns the click response.
fn header(ui: &mut Ui, group: &TaskGroup, expanded: bool) -> egui::Response {
    let w = ui.available_width();
    let (rect, r) = ui.allocate_exact_size(Vec2::new(w, 24.0), Sense::click());
    let y = rect.center().y;
    paint_status(ui, egui::pos2(rect.left() + 7.0, y), group.status);
    let caret = if expanded { icons::CARET_DOWN } else { icons::CARET_RIGHT };
    let caret_color = if r.hovered() { theme::FG_SECONDARY } else { theme::FG_MUTED };
    let cr = ui.painter().text(egui::pos2(rect.right(), y + 1.0), Align2::RIGHT_CENTER, caret, theme::icon(10.0), caret_color);
    let title_left = rect.left() + LEFT_INDENT;
    let meta_left = title_left + (rect.right() - title_left) * 0.55 + 10.0;
    let mut right = cr.left() - 8.0;

    let (label, color) = status_label(group.status);
    let pill = matches!(group.status, TaskStatus::WaitingForUser | TaskStatus::Failed);
    let sg = ui.painter().layout_no_wrap(label.to_owned(), theme::medium(12.0), color);
    let status_w = sg.size().x + if pill { 16.0 } else { 0.0 };
    if right - status_w >= meta_left {
        let sr = Rect::from_min_max(egui::pos2(right - status_w, y - 9.0), egui::pos2(right, y + 9.0));
        if pill {
            ui.painter().rect_filled(sr, 9, color.gamma_multiply(0.16));
        }
        ui.painter().galley(egui::pos2(sr.center().x - sg.size().x / 2.0, y - sg.size().y / 2.0), sg, color);
        right = sr.left() - 6.0;
        let pg = ui.painter().layout_no_wrap(format!("{} \u{b7}", phase_label(group.phase)), theme::font(12.0), theme::FG_MUTED);
        if right - pg.size().x >= meta_left {
            let x = right - pg.size().x;
            ui.painter().galley(egui::pos2(x, y - pg.size().y / 2.0), pg, theme::FG_MUTED);
            right = x;
        }
    }
    let tr = Rect::from_min_max(egui::pos2(title_left, rect.top()), egui::pos2(right - 10.0, rect.bottom()));
    let tc = if group.is_open() { theme::FG } else { theme::FG_SECONDARY };
    widgets::paint_truncated(ui, tr, &group.title, theme::medium(13.5), tc);
    r.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// 2px progress bar with a "done/total" tool-call count under the header (only when the
/// group has tool calls).
fn progress(ui: &mut Ui, group: &TaskGroup) {
    let total = group.tools.len();
    if total == 0 {
        return;
    }
    let done = group.tools_done.min(total);
    let g = ui.painter().layout_no_wrap(format!("{done}/{total}"), theme::font(11.5), theme::FG_MUTED);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 14.0), Sense::hover());
    let p = ui.painter();
    let gx = rect.right() - g.size().x;
    p.galley(egui::pos2(gx, rect.center().y - g.size().y / 2.0), g, theme::FG_MUTED);
    let track = Rect::from_min_max(egui::pos2(rect.left() + LEFT_INDENT, rect.center().y - 1.0), egui::pos2(gx - 10.0, rect.center().y + 1.0));
    if track.width() <= 0.0 {
        return;
    }
    p.rect_filled(track, 1, theme::SECONDARY);
    let bar = if group.is_open() { status_label(group.status).1 } else { theme::FG_MUTED };
    let fill = Rect::from_min_max(track.min, egui::pos2(track.left() + track.width() * done as f32 / total as f32, track.max.y));
    if fill.width() > 0.0 {
        p.rect_filled(fill, 1, bar);
    }
}

/// Progress steps as a small timeline; the current step is brighter. Collapsed: the last
/// [`COLLAPSED_STEPS`] steps (only the last one for finished groups). Returns true when the
/// "N earlier updates" row was clicked.
fn steps(ui: &mut Ui, group: &TaskGroup, expanded: bool) -> bool {
    let keep = if group.is_open() { COLLAPSED_STEPS } else { 1 };
    let skip = if expanded { 0 } else { group.steps.len().saturating_sub(keep) };
    let mut toggled = false;
    if skip > 0 {
        let label = format!("{skip} earlier update{}", if skip == 1 { "" } else { "s" });
        let g = ui.painter().layout_no_wrap(label.clone(), theme::font(12.0), theme::FG_MUTED);
        let (rect, r) = ui.allocate_exact_size(Vec2::new(LEFT_INDENT + g.size().x + 20.0, 20.0), Sense::click());
        let c = if r.hovered() { theme::FG_SECONDARY } else { theme::FG_MUTED };
        let p = ui.painter();
        let t = p.text(egui::pos2(rect.left() + LEFT_INDENT, rect.center().y), Align2::LEFT_CENTER, label, theme::font(12.0), c);
        p.text(egui::pos2(t.right() + 6.0, rect.center().y + 1.0), Align2::LEFT_CENTER, icons::CARET_RIGHT, theme::icon(10.0), c);
        toggled = r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked();
    }
    let font = theme::font(13.0);
    let row_h = ui.fonts_mut(|f| f.row_height(&font));
    let gap = ui.spacing().item_spacing.y;
    let n = group.steps.len();
    let dim = !group.is_open();
    for (i, step) in group.steps.iter().enumerate().skip(skip) {
        let last = i + 1 == n;
        let x = ui.cursor().left();
        let r = ui
            .horizontal_top(|ui| {
                ui.add_space(LEFT_INDENT);
                let color = if last && !dim { theme::FG } else { theme::FG_SECONDARY };
                ui.add(egui::Label::new(egui::RichText::new(step).font(font.clone()).color(color)).wrap());
            })
            .response
            .rect;
        let cx = x + 7.0;
        let cy = r.top() + row_h / 2.0;
        let p = ui.painter();
        if !last {
            // Stop just above the next row's dot.
            let next_cy = r.bottom() + gap + row_h / 2.0;
            p.line_segment([egui::pos2(cx, cy + 4.0), egui::pos2(cx, next_cy - 3.0)], Stroke::new(1.0, theme::FG_MUTED.gamma_multiply(0.5)));
        }
        let dot = if last && !dim { theme::FG_SECONDARY } else { theme::FG_MUTED };
        p.circle_filled(egui::pos2(cx, cy), if last { 3.0 } else { 2.5 }, dot);
    }
    toggled
}

/// Edited-file pills (file name only; the full path is the tooltip).
fn file_pills(ui: &mut Ui, files: &[String]) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = Vec2::new(6.0, 6.0);
        ui.add_space(LEFT_INDENT);
        for f in files {
            let name = f.rsplit(['/', '\\']).next().unwrap_or(f);
            widgets::pill_chip(ui, Some(icons::FILE_TEXT), name, false, true, true).on_hover_text(f.as_str());
        }
    });
}

/// Draw one task group card. `expanded` shows every step and the tool call list; each tool
/// row is drawn flat (no inner card) by `tool_row(ui, item_index)` at [`LEFT_INDENT`].
/// Finished groups keep the card fill and border, slightly dimmed, with a muted title.
/// Returns true when the expansion toggled.
pub fn show(ui: &mut Ui, group: &TaskGroup, expanded: bool, mut tool_row: impl FnMut(&mut Ui, usize)) -> bool {
    let mut toggled = false;
    let mut frame = widgets::card(theme::RADIUS_CARD, 12);
    if !group.is_open() {
        frame = frame.fill(theme::CARD.gamma_multiply(0.85));
    }
    frame.show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing = Vec2::new(8.0, 4.0);
        toggled |= header(ui, group, expanded).clicked();
        progress(ui, group);
        if !group.steps.is_empty() {
            ui.add_space(4.0);
            toggled |= steps(ui, group, expanded);
        }
        if !group.edited_files.is_empty() {
            ui.add_space(4.0);
            file_pills(ui, &group.edited_files);
        }
        if !group.tools.is_empty() {
            ui.add_space(4.0);
            let n = group.tools.len();
            let caption = format!("{n} tool call{}", if n == 1 { "" } else { "s" });
            let (rect, r) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 22.0), Sense::click());
            let c = if r.hovered() { theme::FG_SECONDARY } else { theme::FG_MUTED };
            let p = ui.painter();
            let g = p.text(egui::pos2(rect.left() + LEFT_INDENT, rect.center().y), Align2::LEFT_CENTER, caption, theme::font(12.0), c);
            let caret = if expanded { icons::CARET_DOWN } else { icons::CARET_RIGHT };
            p.text(egui::pos2(g.right() + 6.0, rect.center().y + 1.0), Align2::LEFT_CENTER, caret, theme::icon(10.0), c);
            toggled |= r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked();
            if expanded {
                ui.horizontal_top(|ui| {
                    ui.add_space(LEFT_INDENT);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        for &t in &group.tools {
                            tool_row(ui, t);
                        }
                    });
                });
            }
        }
    });
    toggled
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool(id: &str, name: &str, args: serde_json::Value, result: Option<(bool, String)>) -> ChatItem {
        ChatItem::Tool(ToolCard { call_id: id.into(), name: name.into(), args, result })
    }
    fn boundary(name: &str, summary: &str) -> ChatItem {
        tool("b", orchestrate::TASK_BOUNDARY, json!({"task_name": name, "summary": summary, "phase": "executing"}), Some((true, "ok".into())))
    }

    #[test]
    fn steps_dedupe_and_cancel() {
        let b = |s: &str| TaskBoundary { task_name: "T".into(), status: TaskStatus::InProgress, summary: s.into(), phase: TaskPhase::Executing };
        let mut g = TaskGroup::from_boundary(&b("a"));
        g.update(&b("a"));
        g.update(&b("b"));
        assert_eq!(g.steps, vec!["a", "b"]);
        g.cancel();
        assert_eq!(g.status, TaskStatus::Cancelled);
    }

    #[test]
    fn groups_own_following_tools_and_files() {
        let items = vec![
            ChatItem::User("go".into()),
            tool("0", "read_file", json!({"file_path": "a.rs"}), Some((true, "".into()))),
            boundary("Edit", "start"),
            tool("1", "replace", json!({"file_path": "src/a.rs"}), Some((true, "".into()))),
            tool("2", "write_file", json!({"file_path": "src/a.rs"}), None),
            tool("3", "write_file", json!({"file_path": "bad.rs"}), Some((false, "denied".into()))),
            tool("4", orchestrate::WRITE_ARTIFACT, json!({"name": "task", "content": "x"}), None),
            boundary("Edit", "half way"),
            boundary("Test", "running tests"),
            tool("5", "run_shell_command", json!({"command": "cargo test"}), None),
        ];
        let d = derive(&items, true, true);
        assert_eq!(d.groups.len(), 2);
        let g = &d.groups[0];
        assert_eq!(g.tools, vec![3, 4, 5]);
        assert_eq!(g.edited_files, vec!["src/a.rs"]);
        assert_eq!(g.steps, vec!["start", "half way"]);
        assert_eq!(g.anchor, 2);
        // Superseded by "Test" while the turn runs; the live group waits on the confirmation.
        assert_eq!(g.status, TaskStatus::Completed);
        assert_eq!(d.groups[1].status, TaskStatus::WaitingForUser);
        assert!(!d.owner.contains_key(&1), "tools before the first boundary stay flat");
        assert!(!d.owner.contains_key(&6), "orchestration cards stay flat");
        assert_eq!(d.owner.get(&9), Some(&1));
    }

    #[test]
    fn stopped_turn_cancels_open_groups_and_new_turn_resets() {
        let items = vec![
            ChatItem::User("go".into()),
            boundary("A", "x"),
            ChatItem::Notice("Stopped.".into()),
            ChatItem::User("again".into()),
            boundary("A", "y"),
        ];
        let d = derive(&items, false, false);
        assert_eq!(d.groups.len(), 2, "same name in a new turn opens a new group");
        assert_eq!(d.groups[0].status, TaskStatus::Cancelled);
        assert_eq!(d.groups[1].status, TaskStatus::Completed);
        assert_eq!(d.last_turn, 1);
        let live = derive(&items, true, false);
        assert_eq!(live.groups[1].status, TaskStatus::InProgress);
    }

    #[test]
    fn agent_updates_refine_the_live_turn_only() {
        let items = vec![
            ChatItem::User("go".into()),
            boundary("A", "x"),
            ChatItem::User("again".into()),
            boundary("A", "y"),
        ];
        let mut d = derive(&items, true, false);
        assert_eq!(d.groups[1].status, TaskStatus::InProgress);
        let b = |status: TaskStatus, summary: &str| TaskBoundary { task_name: "A".into(), status, summary: summary.into(), phase: TaskPhase::Executing };
        // `task_complete` closed the live group before the turn-ending items arrived.
        apply_updates(&mut d, &[TaskUpdate { boundary: b(TaskStatus::Completed, "y"), turn: 1 }]);
        assert_eq!(d.groups[1].status, TaskStatus::Completed);
        assert_eq!(d.groups[0].status, TaskStatus::Completed, "the older turn is settled by the transcript");
        // An update for a turn that is not the live one never touches a card.
        let mut live = derive(&items, true, false);
        apply_updates(&mut live, &[TaskUpdate { boundary: b(TaskStatus::Failed, "y"), turn: 0 }]);
        assert_eq!(live.groups[1].status, TaskStatus::InProgress);
        assert_eq!(turn_of(&items), 1);
        assert_eq!(turn_of(&items[..2]), 0);
        assert_eq!(turn_of(&[]), 0);
    }

    #[test]
    fn bad_boundary_is_ignored() {
        let items = vec![tool("b", orchestrate::TASK_BOUNDARY, json!({"task_name": " "}), None)];
        assert!(derive(&items, true, false).groups.is_empty());
    }
}
