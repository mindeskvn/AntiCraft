//! Conversation navigator: a rail on the conversation's left edge with one dot per turn.
//!
//! Hover a dot for that turn's prompt, a preview of the reply and when it was sent; click it
//! to jump there. Turns sent on different days are split into chapters by a short gap with a
//! tick, and the dot of the turn being read is brighter.

use std::collections::BTreeMap;

use chrono::{DateTime, Local, Utc};
use egui::{Align2, Rect, Sense, Ui, Vec2};

use super::{theme, ChatItem};

/// Distance between two dots.
const PITCH: f32 = 12.0;
/// Tightest pitch before the rail stops showing every turn.
const MIN_PITCH: f32 = 5.0;
/// Extra space a chapter (a new day) opens before its first dot.
const CHAPTER_GAP: f32 = 10.0;
/// Width of the rail's hover band.
pub const RAIL_W: f32 = 20.0;
/// Fewest turns that get a rail: a single prompt has nothing to navigate.
pub const MIN_TURNS: usize = 2;

/// One user turn of the transcript.
#[derive(Debug, Clone, PartialEq)]
pub struct Turn<'a> {
    /// Index of the user item in the transcript.
    pub item: usize,
    /// Position among the user bubbles (the key of rewind checkpoints and prompt times).
    pub user_index: usize,
    pub prompt: &'a str,
    /// First answer text of the turn, if it has one yet.
    pub reply: Option<&'a str>,
    pub sent: Option<DateTime<Utc>>,
}

/// The turns of `items`, with their reply and send time.
pub fn turns<'a>(items: &'a [ChatItem], sent: &BTreeMap<usize, DateTime<Utc>>) -> Vec<Turn<'a>> {
    let mut out: Vec<Turn<'a>> = Vec::new();
    for (i, item) in items.iter().enumerate() {
        match item {
            ChatItem::User(text) => {
                let user_index = out.len();
                out.push(Turn { item: i, user_index, prompt: text, reply: None, sent: sent.get(&user_index).copied() });
            }
            ChatItem::Assistant(text) if !text.trim().is_empty() => {
                if let Some(t) = out.last_mut().filter(|t| t.reply.is_none()) {
                    t.reply = Some(text);
                }
            }
            _ => {}
        }
    }
    out
}

/// Local calendar day of a send time (chapters are days where the user is).
fn day(t: DateTime<Utc>) -> chrono::NaiveDate {
    t.with_timezone(&Local).date_naive()
}

/// "Today 14:32", "Yesterday 09:10" or "Nov 12, 09:10".
pub fn sent_label(t: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let local = t.with_timezone(&Local);
    let today = now.with_timezone(&Local).date_naive();
    let d = local.date_naive();
    if d == today {
        format!("Today {}", local.format("%H:%M"))
    } else if today.pred_opt() == Some(d) {
        format!("Yesterday {}", local.format("%H:%M"))
    } else {
        local.format("%b %-d, %H:%M").to_string()
    }
}

/// `text` on one line, cut at `max` characters with an ellipsis.
fn preview(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        flat
    } else {
        let mut s: String = flat.chars().take(max).collect();
        s.push('…');
        s
    }
}

/// Vertical positions of the dots inside a rail of `height`, centered, with a gap before
/// each new day. Returns the y of each turn relative to the rail top (`None` for turns that
/// did not fit: the oldest ones go first), and the y of each chapter tick.
pub fn layout(turns: &[Turn], height: f32) -> (Vec<Option<f32>>, Vec<f32>) {
    let n = turns.len();
    if n == 0 {
        return (Vec::new(), Vec::new());
    }
    let new_day: Vec<bool> = turns
        .iter()
        .enumerate()
        .map(|(i, t)| i > 0 && matches!((turns[i - 1].sent, t.sent), (Some(a), Some(b)) if day(a) != day(b)))
        .collect();
    // Keep the newest turns that fit at the tightest pitch.
    let chapters_in = |from: usize| new_day[from + 1..].iter().filter(|&&d| d).count() as f32;
    let mut first = 0;
    while first + 1 < n && (n - first - 1) as f32 * MIN_PITCH + chapters_in(first) * CHAPTER_GAP > height {
        first += 1;
    }
    let shown = n - first;
    let gaps = chapters_in(first) * CHAPTER_GAP;
    let pitch = if shown > 1 { ((height - gaps) / (shown - 1) as f32).clamp(MIN_PITCH, PITCH) } else { PITCH };
    let total = (shown - 1) as f32 * pitch + gaps;
    let mut y = ((height - total) / 2.0).max(0.0);
    let mut ys = vec![None; n];
    let mut ticks = Vec::new();
    for i in first..n {
        if i > first {
            y += pitch;
            if new_day[i] {
                // The break spans `y..=y + CHAPTER_GAP`: the tick goes at its middle.
                ticks.push(y + CHAPTER_GAP / 2.0);
                y += CHAPTER_GAP;
            }
        }
        ys[i] = Some(y);
    }
    (ys, ticks)
}

/// The turn being read: the last one whose bubble starts above the upper third of the
/// viewport. `tops` are bubble tops in scroll-content coordinates, by turn.
pub fn current_turn(tops: &[Option<f32>], offset: f32, viewport_h: f32) -> Option<usize> {
    let line = offset + viewport_h / 3.0;
    let mut current = None;
    for (i, top) in tops.iter().enumerate() {
        match top {
            Some(y) if *y <= line => current = Some(i),
            Some(_) => break,
            None => {}
        }
    }
    current.or_else(|| tops.iter().position(Option::is_some))
}

/// Paint the rail into `rail` and return the turn (its `user_index`) the user clicked.
pub fn show(ui: &mut Ui, rail: Rect, turns: &[Turn], current: Option<usize>) -> Option<usize> {
    if turns.len() < MIN_TURNS {
        return None;
    }
    let (ys, ticks) = layout(turns, rail.height());
    let x = rail.center().x;
    let now = Utc::now();
    let p = ui.painter().clone();
    for t in ticks {
        p.hline((x - 3.0)..=(x + 3.0), rail.top() + t, egui::Stroke::new(1.0, theme::FG_DIM));
    }
    if ys.first().is_some_and(Option::is_none) {
        // Older turns that did not fit.
        p.text(egui::pos2(x, rail.top() - 4.0), Align2::CENTER_BOTTOM, "⋯", theme::font(10.0), theme::FG_DIM);
    }
    // Each dot answers the pointer over its own slice of the rail, however tight the pitch.
    let hit_h = ys.windows(2).filter_map(|w| Some(w[1]? - w[0]?)).fold(PITCH, f32::min);
    let mut clicked = None;
    for (i, (turn, y)) in turns.iter().zip(&ys).enumerate() {
        let Some(y) = y else { continue };
        let c = egui::pos2(x, rail.top() + y);
        let hit = Rect::from_center_size(c, Vec2::new(RAIL_W, hit_h));
        let r = ui.interact(hit, ui.scope_id().with(("turn_dot", turn.user_index)), Sense::click());
        let is_current = current == Some(i);
        let (radius, color) = if r.hovered() {
            (3.5, theme::FG)
        } else if is_current {
            (3.0, theme::FG_SECONDARY)
        } else {
            (2.5, theme::FG_MUTED)
        };
        p.circle_filled(c, radius, color);
        let r = r.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_ui(|ui| {
            ui.set_max_width(320.0);
            ui.label(egui::RichText::new(preview(turn.prompt, 160)).font(theme::font(13.0)).color(theme::FG));
            if let Some(reply) = turn.reply {
                ui.label(egui::RichText::new(preview(reply, 140)).font(theme::font(12.0)).color(theme::FG_MUTED));
            }
            if let Some(sent) = turn.sent {
                ui.label(egui::RichText::new(sent_label(sent, now)).font(theme::font(11.5)).color(theme::FG_DIM));
            }
        });
        if r.clicked() {
            clicked = Some(turn.user_index);
        }
    }
    clicked
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn turns_pair_prompts_with_their_first_answer_and_time() {
        let items = vec![
            ChatItem::User("first".into()),
            ChatItem::TurnHeader { duration: None },
            ChatItem::Assistant("one".into()),
            ChatItem::Assistant("two".into()),
            ChatItem::User("second".into()),
            ChatItem::Notice("Stopped.".into()),
        ];
        let sent = BTreeMap::from([(1, at("2026-11-12T10:00:00Z"))]);
        let t = turns(&items, &sent);
        assert_eq!(t.len(), 2);
        assert_eq!((t[0].item, t[0].user_index, t[0].prompt, t[0].reply, t[0].sent), (0, 0, "first", Some("one"), None));
        assert_eq!((t[1].item, t[1].user_index, t[1].reply), (4, 1, None));
        assert_eq!(t[1].sent, Some(at("2026-11-12T10:00:00Z")));
    }

    fn turn(user_index: usize, sent: Option<DateTime<Utc>>) -> Turn<'static> {
        Turn { item: user_index * 3, user_index, prompt: "p", reply: None, sent }
    }

    #[test]
    fn dots_are_centered_and_a_new_day_opens_a_chapter() {
        let same: Vec<Turn> = (0..3).map(|i| turn(i, None)).collect();
        let (ys, ticks) = layout(&same, 100.0);
        assert_eq!(ys, vec![Some(38.0), Some(50.0), Some(62.0)]);
        assert!(ticks.is_empty());
        // Noon UTC keeps both days apart in every time zone.
        let days = vec![turn(0, Some(at("2026-11-10T12:00:00Z"))), turn(1, Some(at("2026-11-11T12:00:00Z")))];
        let (ys, ticks) = layout(&days, 100.0);
        let (a, b) = (ys[0].unwrap(), ys[1].unwrap());
        assert_eq!(b - a, PITCH + CHAPTER_GAP);
        assert_eq!(ticks.len(), 1);
        assert!(ticks[0] > a && ticks[0] < b);
    }

    #[test]
    fn a_long_conversation_tightens_then_keeps_the_newest_turns() {
        let many: Vec<Turn> = (0..30).map(|i| turn(i, None)).collect();
        let (ys, _) = layout(&many, 200.0);
        assert!(ys.iter().all(Option::is_some));
        let pitch = ys[1].unwrap() - ys[0].unwrap();
        assert!((MIN_PITCH..PITCH).contains(&pitch), "{pitch}");
        let (ys, _) = layout(&many, 60.0);
        assert!(ys[0].is_none(), "the oldest turns give way");
        assert!(ys[29].is_some(), "the newest turn stays");
        let shown: Vec<f32> = ys.iter().flatten().copied().collect();
        assert!(shown.last().unwrap() <= &60.0);
    }

    #[test]
    fn the_current_turn_is_the_last_one_above_the_reading_line() {
        let tops = vec![Some(0.0), Some(400.0), Some(900.0)];
        assert_eq!(current_turn(&tops, 0.0, 600.0), Some(0));
        assert_eq!(current_turn(&tops, 300.0, 600.0), Some(1));
        assert_eq!(current_turn(&tops, 800.0, 600.0), Some(2));
        // Turns not laid out (paged away) are skipped.
        assert_eq!(current_turn(&[None, Some(500.0)], 0.0, 600.0), Some(1));
    }

    #[test]
    fn previews_are_one_line() {
        assert_eq!(preview("a\n  b   c", 10), "a b c");
        assert_eq!(preview("abcdefghij", 4), "abcd…");
        let now = Utc::now();
        assert!(sent_label(now, now).starts_with("Today "));
    }
}
