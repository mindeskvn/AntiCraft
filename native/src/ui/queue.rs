//! Messages sent while the agent is busy (ORCHESTRATION.md section D.1).
//!
//! Default is "Queue": the message waits until the turn ends, then runs as the next prompt.
//! "Steer" (docs: "Send Immediately") injects the text into the running turn at the next model
//! request (`AppCommand::SteerPrompt`). Alt+Enter sends one message with the other behavior.
//! Queued messages show as chips above the composer; each chip can be removed or promoted
//! to steer.
//!
//! The backend owns the authoritative queue once it reports one (`AppEvent::Queue`, routed
//! through `composer::on_queue`). Until then the composer keeps a local mirror and runs the
//! next entry itself after a turn that ended normally ([`LocalQueue::next_after_turn`]).

use egui::{Align2, Sense, Stroke, Ui, Vec2};
use serde::{Deserialize, Serialize};

use super::{icons, theme, widgets};

/// Settings value (`Settings.queued_messages`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum QueueMode {
    #[default]
    Queue,
    SendImmediately,
}

impl QueueMode {
    pub fn flipped(self) -> Self {
        match self {
            QueueMode::Queue => QueueMode::SendImmediately,
            QueueMode::SendImmediately => QueueMode::Queue,
        }
    }

    /// Mode for one send: Alt flips the setting.
    pub fn for_send(self, alt: bool) -> Self {
        if alt {
            self.flipped()
        } else {
            self
        }
    }
}

/// One queued message as shown in the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedPrompt {
    pub id: u64,
    pub text: String,
}

/// Chip actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChipAction {
    Remove(u64),
    Steer(u64),
    ClearAll,
}

/// UI-side queue mirror.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalQueue {
    pub items: Vec<QueuedPrompt>,
    /// The backend reported its queue at least once: it runs entries, the UI only mirrors.
    pub backend_owned: bool,
    next_id: u64,
}

impl LocalQueue {
    /// Add a locally queued prompt; returns its id.
    pub fn push(&mut self, text: String) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        self.items.push(QueuedPrompt { id, text });
        id
    }

    /// Replace the mirror with the backend's queue.
    pub fn mirror(&mut self, q: Vec<(u64, String)>) {
        self.backend_owned = true;
        self.next_id = self.next_id.max(q.iter().map(|x| x.0).max().unwrap_or(0));
        self.items = q.into_iter().map(|(id, text)| QueuedPrompt { id, text }).collect();
    }

    /// Remove and return one entry.
    pub fn take(&mut self, id: u64) -> Option<QueuedPrompt> {
        let i = self.items.iter().position(|q| q.id == id)?;
        Some(self.items.remove(i))
    }

    /// After a turn ends: the next prompt to run locally. Nothing when the backend owns the
    /// queue or the turn did not end normally (Stop, rejection, error keep the queue).
    pub fn next_after_turn(&mut self, ended_normally: bool) -> Option<QueuedPrompt> {
        if self.backend_owned || !ended_normally || self.items.is_empty() {
            return None;
        }
        Some(self.items.remove(0))
    }
}

/// Shorten `text` to one line of at most `max` chars.
pub fn chip_label(text: &str, max: usize) -> String {
    let line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    let short: String = line.chars().take(max).collect();
    if short.chars().count() < line.chars().count() || text.trim().lines().count() > 1 {
        format!("{short}…")
    } else {
        short
    }
}

/// Draw the queued list; returns the clicked action.
pub fn show(ui: &mut Ui, queued: &[QueuedPrompt]) -> Option<ChipAction> {
    if queued.is_empty() {
        return None;
    }
    let mut out = None;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let n = queued.len();
        ui.label(egui::RichText::new(if n == 1 { "1 queued message".to_owned() } else { format!("{n} queued messages") }).font(theme::font(11.5)).color(theme::FG_MUTED));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if n > 1 && widgets::text_link(ui, "Clear all", 11.5).clicked() {
                out = Some(ChipAction::ClearAll);
            }
        });
    });
    ui.add_space(2.0);
    for q in queued {
        let (rect, r) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 26.0), Sense::hover());
        let p = ui.painter();
        p.rect(rect, 6, theme::FIELD_BG, Stroke::new(1.0, theme::CARD_BORDER), egui::StrokeKind::Inside);
        p.text(egui::pos2(rect.left() + 8.0, rect.center().y), Align2::LEFT_CENTER, icons::CLOCK_COUNTER_CLOCKWISE, theme::icon(12.0), theme::FG_MUTED);
        let x_rect = egui::Rect::from_center_size(egui::pos2(rect.right() - 13.0, rect.center().y), Vec2::splat(20.0));
        let steer_w = 46.0;
        let steer_rect = egui::Rect::from_min_size(egui::pos2(x_rect.left() - steer_w - 2.0, rect.top() + 3.0), Vec2::new(steer_w, 20.0));
        let text_rect = egui::Rect::from_min_max(egui::pos2(rect.left() + 26.0, rect.top()), egui::pos2(steer_rect.left() - 6.0, rect.bottom()));
        widgets::paint_truncated(ui, text_rect, &chip_label(&q.text, 200), theme::font(12.0), theme::FG_SECONDARY);
        let sr = ui.interact(steer_rect, ui.scope_id().with(("queue_steer", q.id)), Sense::click());
        let p = ui.painter();
        if sr.hovered() {
            p.rect_filled(steer_rect, 4, theme::SECONDARY);
        }
        p.text(steer_rect.center(), Align2::CENTER_CENTER, "Steer", theme::font(11.5), if sr.hovered() { theme::FG } else { theme::FG_SECONDARY });
        if sr.on_hover_text("Send now into the running turn").clicked() {
            out = Some(ChipAction::Steer(q.id));
        }
        if widgets::overlay_icon(ui, x_rect, ui.scope_id().with(("queue_x", q.id)), icons::X, "Remove", true).clicked() {
            out = Some(ChipAction::Remove(q.id));
        }
        r.on_hover_text(&q.text);
        ui.add_space(3.0);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flip() {
        assert_eq!(QueueMode::Queue.flipped(), QueueMode::SendImmediately);
        assert_eq!(QueueMode::Queue.for_send(true), QueueMode::SendImmediately);
        assert_eq!(QueueMode::SendImmediately.for_send(false), QueueMode::SendImmediately);
    }

    #[test]
    fn local_queue_runs_only_after_normal_end() {
        let mut q = LocalQueue::default();
        let a = q.push("a".into());
        q.push("b".into());
        assert_eq!(q.next_after_turn(false), None, "stop keeps the queue");
        assert_eq!(q.next_after_turn(true).map(|p| p.id), Some(a));
        assert_eq!(q.items.len(), 1);
        let b = q.items[0].id;
        assert_eq!(q.take(b).map(|p| p.text), Some("b".into()));
        assert!(q.items.is_empty());
    }

    #[test]
    fn backend_mirror_takes_over() {
        let mut q = LocalQueue::default();
        q.push("local".into());
        q.mirror(vec![(7, "x".into())]);
        assert!(q.backend_owned);
        assert_eq!(q.next_after_turn(true), None);
        assert!(q.push("y".into()) > 7);
    }

    #[test]
    fn labels() {
        assert_eq!(chip_label("short", 40), "short");
        assert_eq!(chip_label("abcdef", 3), "abc…");
        assert_eq!(chip_label("one\ntwo", 40), "one…");
    }
}
