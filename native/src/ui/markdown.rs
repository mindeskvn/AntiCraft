//! Markdown rendering for assistant messages (DESIGN.md 2.5).
//!
//! Prose goes through egui_commonmark; fenced code blocks are split out and drawn as
//! `card` blocks with a 28px header strip (language + Copy button) and a monospace body.
//! Unified diffs get per-line tinted backgrounds (15% alpha) and a line-number gutter.

use egui::{Align2, Color32, Rect, Sense, Stroke, Ui, Vec2};
use egui_commonmark::{CommonMarkCache, CommonMarkViewer};

use super::{icons, theme, widgets};

/// One piece of an assistant message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment<'a> {
    Prose(&'a str),
    /// `lang` may be empty; `closed` is false while the fence is still streaming.
    Code { lang: &'a str, body: &'a str, closed: bool },
}

/// Split markdown into prose and fenced code blocks (``` or ~~~, up to 3 spaces indent).
pub fn split_fences(text: &str) -> Vec<Segment<'_>> {
    let mut out = Vec::new();
    let mut prose_start = 0;
    let mut pos = 0;
    // (fence char, fence len, lang, body start)
    let mut open: Option<(char, usize, &str, usize)> = None;
    while pos < text.len() {
        let end = text[pos..].find('\n').map_or(text.len(), |i| pos + i + 1);
        let line = text[pos..end].trim_end_matches(['\n', '\r']);
        let indent = line.len() - line.trim_start_matches(' ').len();
        let t = &line[indent..];
        match open {
            None if indent <= 3 && (t.starts_with("```") || t.starts_with("~~~")) => {
                let ch = t.chars().next().unwrap_or('`');
                let n = t.chars().take_while(|c| *c == ch).count();
                let lang = t[n..].split_whitespace().next().unwrap_or("");
                if pos > prose_start {
                    out.push(Segment::Prose(&text[prose_start..pos]));
                }
                open = Some((ch, n, lang, end));
            }
            Some((ch, n, lang, body))
                if indent <= 3 && t.chars().take_while(|c| *c == ch).count() >= n && t.trim_start_matches(ch).trim().is_empty() =>
            {
                out.push(Segment::Code { lang, body: text[body..pos].trim_end_matches(['\n', '\r']), closed: true });
                open = None;
                prose_start = end;
            }
            _ => {}
        }
        pos = end;
    }
    match open {
        Some((_, _, lang, body)) => {
            out.push(Segment::Code { lang, body: text[body.min(text.len())..].trim_end_matches(['\n', '\r']), closed: false });
        }
        None if prose_start < text.len() => out.push(Segment::Prose(&text[prose_start..])),
        None => {}
    }
    out
}

/// Render `text` as CommonMark with custom code blocks, with an id that is stable per message (keeps copy state and scroll).
pub fn render_with_id(ui: &mut Ui, cache: &mut CommonMarkCache, id: egui::Id, text: &str) {
    ui.scope(|ui| {
        let s = ui.style_mut();
        // Reference body: 14px FG, ~22px leading, 8px between paragraphs.
        s.spacing.item_spacing.y = 8.0;
        s.text_styles.insert(egui::TextStyle::Heading, theme::medium(15.0));
        s.visuals.hyperlink_color = theme::LINK;
        s.visuals.override_text_color = Some(theme::FG);
        s.override_text_style = Some(egui::TextStyle::Body);
        for (i, seg) in split_fences(text).into_iter().enumerate() {
            match seg {
                Segment::Prose(p) if p.trim().is_empty() => {}
                Segment::Prose(p) => {
                    CommonMarkViewer::new().show_with_id(id.with(("md", i)), ui, cache, p);
                }
                Segment::Code { lang, body, .. } => {
                    ui.add_space(4.0);
                    code_block(ui, id.with(("code", i)), lang, body);
                    ui.add_space(6.0);
                }
            }
        }
    });
}

/// Code block card: header strip (language, Copy) and monospace body.
pub fn code_block(ui: &mut Ui, id: egui::Id, lang: &str, body: &str) {
    widgets::card(theme::RADIUS_CODE, 0).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 0.0;
        let w = ui.available_width();
        let (head, _) = ui.allocate_exact_size(Vec2::new(w, 28.0), Sense::hover());
        let label = if lang.is_empty() { "text" } else { lang };
        ui.painter().text(head.left_center() + Vec2::new(12.0, 0.0), Align2::LEFT_CENTER, label, theme::font(12.0), theme::FG_MUTED);
        let copied_at = id.with("copied");
        let now = ui.input(|i| i.time);
        let copied = ui.data(|d| d.get_temp::<f64>(copied_at)).is_some_and(|t| now - t < 1.5);
        let btn = Rect::from_center_size(egui::pos2(head.right() - 16.0, head.center().y), Vec2::splat(22.0));
        let icon = if copied { icons::CHECK } else { icons::COPY };
        if widgets::overlay_icon(ui, btn, id.with("copy"), icon, if copied { "Copied" } else { "Copy" }, true).clicked() {
            ui.ctx().copy_text(body.to_owned());
            ui.data_mut(|d| d.insert_temp(copied_at, now));
        }
        if copied {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(300));
        }
        ui.painter().line_segment([head.left_bottom(), head.right_bottom()], Stroke::new(1.0, theme::CARD_BORDER));
        egui::ScrollArea::horizontal().id_salt(id.with("scroll")).auto_shrink([false, true]).show(ui, |ui| {
            egui::Frame::NONE.inner_margin(egui::Margin { left: 12, right: 12, top: 10, bottom: 10 }).show(ui, |ui| {
                ui.add(egui::Label::new(egui::RichText::new(body).font(theme::mono(12.5)).color(theme::FG)).extend());
            });
        });
    });
}

/// `(+additions, -removals)` of a unified diff.
pub fn diff_stats(diff: &str) -> (usize, usize) {
    let mut a = 0;
    let mut r = 0;
    for l in diff.lines() {
        if l.starts_with('+') && !l.starts_with("+++") {
            a += 1;
        } else if l.starts_with('-') && !l.starts_with("---") {
            r += 1;
        }
    }
    (a, r)
}

/// Parse `@@ -a,b +c,d @@` into (old start, new start).
fn hunk_starts(line: &str) -> Option<(usize, usize)> {
    let mut it = line.split_whitespace().skip(1);
    let num = |s: &str| -> Option<usize> { s.get(1..)?.split(',').next()?.parse().ok() };
    Some((num(it.next()?)?, num(it.next()?)?))
}

/// One displayable diff row: hunk header, or a line with its gutter number.
enum DiffRow<'a> {
    Hunk(&'a str),
    Line { sign: u8, num: usize, body: &'a str },
}

fn diff_rows(diff: &str) -> Vec<DiffRow<'_>> {
    let (mut old, mut new) = (0usize, 0usize);
    let mut rows = Vec::new();
    for line in diff.lines() {
        if line.starts_with("+++") || line.starts_with("---") {
            continue;
        }
        if line.starts_with("@@") {
            if let Some((o, n)) = hunk_starts(line) {
                (old, new) = (o, n);
            }
            rows.push(DiffRow::Hunk(line));
            continue;
        }
        let sign = line.as_bytes().first().copied().unwrap_or(b' ');
        let num = match sign {
            b'+' => {
                new += 1;
                new - 1
            }
            b'-' => {
                old += 1;
                old - 1
            }
            _ => {
                old += 1;
                new += 1;
                new - 1
            }
        };
        rows.push(DiffRow::Line { sign, num, body: line.get(1..).unwrap_or("") });
    }
    rows
}

/// Render a unified diff: tinted rows, line-number gutter, hunk headers in muted text.
/// Only the visible rows are laid out (`show_rows`); `salt` (the call id) keeps scroll state
/// separate per diff.
pub fn render_diff(ui: &mut Ui, salt: &str, diff: &str) {
    let font = theme::mono(12.5);
    let row_h = 19.0;
    let rows = diff_rows(diff);
    egui::ScrollArea::vertical()
        .max_height(320.0)
        .id_salt(("diff", salt))
        .auto_shrink([false, true])
        .show_rows(ui, row_h, rows.len(), |ui, range| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for row in &rows[range] {
                let w = ui.available_width();
                let (rect, _) = ui.allocate_exact_size(Vec2::new(w, row_h), Sense::hover());
                let y = rect.center().y;
                let (sign, num, body) = match row {
                    DiffRow::Hunk(line) => {
                        ui.painter().text(egui::pos2(rect.left() + 8.0, y), Align2::LEFT_CENTER, *line, font.clone(), theme::FG_MUTED);
                        continue;
                    }
                    DiffRow::Line { sign, num, body } => (*sign, *num, *body),
                };
                let (bg, fg, sc) = match sign {
                    b'+' => (theme::SUCCESS.gamma_multiply(0.15), theme::FG, theme::SUCCESS),
                    b'-' => (theme::ERROR.gamma_multiply(0.15), theme::FG, theme::ERROR),
                    _ => (Color32::TRANSPARENT, theme::FG_SECONDARY, theme::FG_MUTED),
                };
                let p = ui.painter();
                if bg != Color32::TRANSPARENT {
                    p.rect_filled(rect, 0, bg);
                }
                p.text(egui::pos2(rect.left() + 36.0, y), Align2::RIGHT_CENTER, num.to_string(), font.clone(), theme::FG_MUTED);
                let s = match sign {
                    b'+' => "+",
                    b'-' => "-",
                    _ => " ",
                };
                p.text(egui::pos2(rect.left() + 46.0, y), Align2::LEFT_CENTER, s, font.clone(), sc);
                let tr = Rect::from_min_max(egui::pos2(rect.left() + 60.0, rect.top()), rect.max);
                widgets::paint_truncated(ui, tr, body, font.clone(), fg);
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_fences() {
        let s = split_fences("hi\n```rust\nfn a() {}\n```\nbye\n");
        assert_eq!(s, vec![Segment::Prose("hi\n"), Segment::Code { lang: "rust", body: "fn a() {}", closed: true }, Segment::Prose("bye\n")]);
    }

    #[test]
    fn unclosed_fence_while_streaming() {
        let s = split_fences("x\n~~~\nlet a");
        assert_eq!(s, vec![Segment::Prose("x\n"), Segment::Code { lang: "", body: "let a", closed: false }]);
    }

    #[test]
    fn longer_fence_needs_longer_close() {
        let s = split_fences("````md\n```\ninner\n```\n````\n");
        assert_eq!(s, vec![Segment::Code { lang: "md", body: "```\ninner\n```", closed: true }]);
    }

    #[test]
    fn stats_and_hunks() {
        assert_eq!(diff_stats("--- a\n+++ b\n@@ -1,2 +1,2 @@\n-x\n+y\n+z\n"), (2, 1));
        assert_eq!(hunk_starts("@@ -12,3 +14,4 @@"), Some((12, 14)));
    }
}
