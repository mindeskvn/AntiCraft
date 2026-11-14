//! Small painted widgets shared by the views: nav rows, icon buttons, pill chips, toggles, cards.

use egui::{Align2, Color32, CornerRadius, Rect, Response, Sense, Stroke, StrokeKind, Ui, Vec2};

use super::theme;

pub const COMING_SOON: &str = "Coming soon";

/// Hover fade factor (0..1) for a response, about 120 ms.
fn hover_t(ui: &Ui, r: &Response) -> f32 {
    ui.ctx().animate_bool_with_time(r.id.with("hover"), r.hovered() && r.enabled(), 0.12)
}

fn lerp(a: Color32, b: Color32, t: f32) -> Color32 {
    a.lerp_to_gamma(b, t)
}

/// Sidebar navigation row (full width x 30): icon at x+10, label at x+34.
pub fn nav_row(ui: &mut Ui, icon: &str, label: &str, selected: bool, enabled: bool) -> Response {
    nav_row_impl(ui, icon, label, selected, enabled, false)
}

/// Like [`nav_row`], but drawn as a boxed button (`#1c1c1f`, 1px `#29292c`), as the reference
/// draws "New Conversation". Its text is bright whether or not it is selected.
pub fn nav_row_boxed(ui: &mut Ui, icon: &str, label: &str, selected: bool, enabled: bool) -> Response {
    nav_row_impl(ui, icon, label, selected, enabled, true)
}

fn nav_row_impl(ui: &mut Ui, icon: &str, label: &str, selected: bool, enabled: bool, boxed: bool) -> Response {
    let w = ui.available_width();
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, r) = ui.allocate_exact_size(Vec2::new(w, 30.0), sense);
    let t = hover_t(ui, &r);
    let p = ui.painter();
    let mut text = if selected || boxed { theme::FG } else { lerp(theme::FG_SECONDARY, theme::FG, t) };
    if boxed {
        let fill = if selected { theme::ROW_SELECTED } else { lerp(theme::BOX_FILL, theme::SECONDARY, t) };
        p.rect(rect, 8, fill, Stroke::new(1.0, theme::FIELD_BORDER), StrokeKind::Inside);
    } else if selected {
        p.rect(rect, 6, theme::ROW_SELECTED, Stroke::new(1.0, theme::ROW_SELECTED_EDGE), StrokeKind::Inside);
    } else if enabled && t > 0.0 {
        p.rect_filled(rect, 8, theme::ROW_HOVER.gamma_multiply(t));
    }
    if !enabled {
        text = text.gamma_multiply(0.4);
    }
    p.text(rect.left_center() + Vec2::new(10.0, 0.0), Align2::LEFT_CENTER, icon, theme::icon(16.0), text);
    p.text(rect.left_center() + Vec2::new(34.0, 0.0), Align2::LEFT_CENTER, label, theme::font(13.0), text);
    if enabled {
        r.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        r.on_hover_text(COMING_SOON)
    }
}

/// Square icon button (`size` 20/24/28). Disabled buttons show `disabled_tip` on hover.
pub fn icon_button(ui: &mut Ui, icon: &str, size: f32, tip: &str, enabled: bool) -> Response {
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, r) = ui.allocate_exact_size(Vec2::splat(size), sense);
    paint_icon_button(ui, rect, &r, icon, enabled, theme::FG_SECONDARY);
    if tip.is_empty() {
        r
    } else {
        r.on_hover_text(tip)
    }
}

/// Paint an icon button into an existing rect (used for overlay row actions).
pub fn paint_icon_button(ui: &Ui, rect: Rect, r: &Response, icon: &str, enabled: bool, base: Color32) {
    let t = hover_t(ui, r);
    let radius = if rect.width() >= 28.0 { 8 } else { 6 };
    let p = ui.painter();
    if enabled && t > 0.0 {
        p.rect_filled(rect, radius, if r.is_pointer_button_down_on() { theme::PRESSED } else { theme::SECONDARY.gamma_multiply(t) });
    }
    let mut c = lerp(base, theme::FG, t);
    if !enabled {
        c = c.gamma_multiply(0.4);
    }
    let size = (rect.width() * 0.6).clamp(12.0, 16.0);
    p.text(rect.center(), Align2::CENTER_CENTER, icon, theme::icon(size), c);
}

/// Icon button placed at an absolute rect (does not advance the layout).
pub fn overlay_icon(ui: &mut Ui, rect: Rect, id: egui::Id, icon: &str, tip: &str, enabled: bool) -> Response {
    let r = ui.interact(rect, id, if enabled { Sense::click() } else { Sense::hover() });
    paint_icon_button(ui, rect, &r, icon, enabled, theme::FG_MUTED);
    r.on_hover_text(tip)
}

/// Pill chip with optional leading icon and trailing caret. Height 28 (or 24 when `small`).
pub fn pill_chip(ui: &mut Ui, icon: Option<&str>, label: &str, caret: bool, enabled: bool, small: bool) -> Response {
    let h = if small { 24.0 } else { 28.0 };
    let font = theme::font(12.0);
    let galley = ui.painter().layout_no_wrap(label.to_owned(), font, theme::FG_SECONDARY);
    let mut w = galley.size().x + 20.0;
    if icon.is_some() {
        w += 18.0;
    }
    if caret {
        w += 16.0;
    }
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, r) = ui.allocate_exact_size(Vec2::new(w, h), sense);
    let t = hover_t(ui, &r);
    let p = ui.painter();
    if enabled && t > 0.0 {
        p.rect_filled(rect, CornerRadius::same((h / 2.0) as u8), theme::SECONDARY.gamma_multiply(t));
    }
    let mut c = if small { theme::FG_MUTED } else { lerp(theme::FG_SECONDARY, theme::FG, t) };
    if !enabled {
        c = c.gamma_multiply(0.4);
    }
    let mut x = rect.left() + 10.0;
    if let Some(i) = icon {
        p.text(egui::pos2(x, rect.center().y), Align2::LEFT_CENTER, i, theme::icon(14.0), c);
        x += 18.0;
    }
    p.galley(egui::pos2(x, rect.center().y - galley.size().y / 2.0), galley.clone(), c);
    x += galley.size().x + 4.0;
    if caret {
        p.text(egui::pos2(x, rect.center().y), Align2::LEFT_CENTER, super::icons::CARET_DOWN, theme::icon(11.0), c);
    }
    r
}

/// Chip of the message box's controls row (28 h): optional icon, label, and an upward caret,
/// since its menu opens above the box.
pub fn pill_chip_up(ui: &mut Ui, icon: Option<&str>, label: &str, enabled: bool) -> Response {
    let font = theme::font(12.5);
    let galley = ui.painter().layout_no_wrap(label.to_owned(), font, theme::FG_SECONDARY);
    let w = galley.size().x + 16.0 + if icon.is_some() { 19.0 } else { 0.0 } + 15.0;
    let (rect, r) = ui.allocate_exact_size(Vec2::new(w, 28.0), if enabled { Sense::click() } else { Sense::hover() });
    let open = egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(&r));
    let t = hover_t(ui, &r).max(if open { 1.0 } else { 0.0 });
    let p = ui.painter();
    if enabled && t > 0.0 {
        p.rect_filled(rect, 8, theme::SECONDARY.gamma_multiply(t));
    }
    let mut c = lerp(theme::FG_SECONDARY, theme::FG, t);
    if !enabled {
        c = c.gamma_multiply(0.4);
    }
    let mut x = rect.left() + 8.0;
    if let Some(i) = icon {
        p.text(egui::pos2(x, rect.center().y), Align2::LEFT_CENTER, i, theme::icon(14.0), c);
        x += 19.0;
    }
    p.galley(egui::pos2(x, rect.center().y - galley.size().y / 2.0), galley.clone(), c);
    x += galley.size().x + 5.0;
    p.text(egui::pos2(x, rect.center().y), Align2::LEFT_CENTER, super::icons::CARET_UP, theme::icon(10.0), c);
    if enabled {
        r.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        r
    }
}

/// Pill toggle 36x20 bound to `on`. Returns true when changed.
pub fn toggle(ui: &mut Ui, on: &mut bool) -> Response {
    // 40x22 pill: green when on, `#3c3c3c` off, 16px white knob.
    let (rect, mut r) = ui.allocate_exact_size(Vec2::new(40.0, 22.0), Sense::click());
    if r.clicked() {
        *on = !*on;
        r.mark_changed();
    }
    let t = ui.ctx().animate_bool_with_time(r.id, *on, 0.12);
    let p = ui.painter();
    p.rect_filled(rect, 11, lerp(Color32::from_rgb(0x3c, 0x3c, 0x3c), theme::TOGGLE_ON, t));
    let x = egui::lerp(rect.left() + 11.0..=rect.right() - 11.0, t);
    p.circle_filled(egui::pos2(x, rect.center().y), 8.0, Color32::WHITE);
    r.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Card frame (`card` fill, 1px `card-border`).
pub fn card(radius: u8, pad: i8) -> egui::Frame {
    egui::Frame::new()
        .fill(theme::CARD)
        .stroke(Stroke::new(1.0, theme::CARD_BORDER))
        .corner_radius(radius)
        .inner_margin(pad)
}

/// Shared frame of the review surfaces (composer banner and the artifacts panel's review
/// bar): faint amber fill, amber border and a 3px amber accent bar on the left edge.
pub fn review_frame<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> egui::InnerResponse<R> {
    let r = egui::Frame::new()
        .fill(theme::WARNING.gamma_multiply(0.08))
        .stroke(Stroke::new(1.0, theme::WARNING.gamma_multiply(0.45)))
        .corner_radius(theme::RADIUS_CARD)
        .inner_margin(egui::Margin { left: 15, right: 12, top: 8, bottom: 8 })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        });
    let rect = r.response.rect;
    let bar = Rect::from_min_max(rect.left_top() + Vec2::new(1.0, 1.0), egui::pos2(rect.left() + 4.0, rect.bottom() - 1.0));
    let cr = CornerRadius { nw: theme::RADIUS_CARD - 1, sw: theme::RADIUS_CARD - 1, ne: 0, se: 0 };
    ui.painter().rect_filled(bar, cr, theme::WARNING);
    r
}

/// Plain text-only button ("See all (N)", "Load older messages").
pub fn text_link(ui: &mut Ui, label: &str, size: f32) -> Response {
    let r = ui.add(egui::Label::new(egui::RichText::new(label).font(theme::font(size)).color(theme::FG_MUTED)).sense(Sense::click()));
    if r.hovered() {
        ui.painter().text(r.rect.left_center(), Align2::LEFT_CENTER, label, theme::font(size), theme::FG);
    }
    r.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Section label (12/medium, muted).
pub fn section_label(ui: &mut Ui, text: &str) -> Response {
    ui.label(egui::RichText::new(text).font(theme::medium(12.0)).color(theme::FG_MUTED))
}

/// Paint `text` into `rect` truncated with an ellipsis; left aligned, vertically centered.
pub fn paint_truncated(ui: &Ui, rect: Rect, text: &str, font: egui::FontId, color: Color32) {
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_owned(), font, color);
    job.wrap = egui::text::TextWrapping::truncate_at_width(rect.width().max(1.0));
    let g = ui.painter().layout_job(job);
    ui.painter().galley(egui::pos2(rect.left(), rect.center().y - g.size().y / 2.0), g, color);
}

/// Centered column of at most `max` width inside the current ui.
pub fn centered_column<R>(ui: &mut Ui, max: f32, add: impl FnOnce(&mut Ui) -> R) -> R {
    let avail = ui.available_width();
    let w = avail.min(max);
    let pad = ((avail - w) / 2.0).max(0.0);
    ui.horizontal_top(|ui| {
        ui.add_space(pad);
        ui.vertical(|ui| {
            ui.set_width(w);
            add(ui)
        })
        .inner
    })
    .inner
}

/// Attach a menu popup to `anchor` (opens on click, closes on outside click / Esc).
pub fn popup_menu<R>(anchor: &Response, width: f32, add: impl FnOnce(&mut Ui) -> R) -> Option<R> {
    let ui_frame = card(theme::RADIUS_POPUP, 6).shadow(anchor.ctx.global_style().visuals.popup_shadow);
    egui::Popup::menu(anchor)
        .gap(4.0)
        .frame(ui_frame)
        .width(width)
        .show(|ui| {
            ui.set_min_width(width - 12.0);
            add(ui)
        })
        .map(|r| r.inner)
}


/// Send / stop button: a 28px circle, white with a dark arrow when there is something to send,
/// dim when the box is empty, and a stop square while a turn runs.
pub fn send_button(ui: &mut Ui, busy: bool, ready: bool) -> Response {
    let (rect, r) = ui.allocate_exact_size(Vec2::splat(28.0), Sense::click());
    let active = busy || ready;
    let (mut bg, fg, icon) = if busy {
        (theme::FG, theme::BG, super::icons::STOP)
    } else if ready {
        (theme::FG, theme::BG, super::icons::ARROW_UP)
    } else {
        (theme::SEND_BG, theme::FG_MUTED, super::icons::ARROW_UP)
    };
    if active && r.hovered() {
        bg = Color32::WHITE;
    }
    let p = ui.painter();
    p.circle_filled(rect.center(), 13.0, bg);
    p.text(rect.center(), Align2::CENTER_CENTER, icon, theme::icon(if busy { 12.0 } else { 14.0 }), fg);
    if active {
        r.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(if busy { "Stop" } else { "Send (Enter)" })
    } else {
        r.on_hover_text("Type a message to send")
    }
}

/// Style of a 28px text [`button`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ButtonKind {
    Primary,
    Secondary,
    Ghost,
}

/// Filled primary button (Sign in, Save); grey when disabled.
pub fn primary_button(ui: &mut Ui, label: &str, enabled: bool) -> Response {
    let g = ui.painter().layout_no_wrap(label.to_owned(), theme::medium(12.5), Color32::WHITE);
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, r) = ui.allocate_exact_size(Vec2::new(g.size().x + 24.0, 28.0), sense);
    let fill = if !enabled {
        theme::SECONDARY
    } else if r.hovered() {
        theme::PRIMARY.lerp_to_gamma(Color32::WHITE, 0.12)
    } else {
        theme::PRIMARY
    };
    let tc = if enabled { Color32::WHITE } else { theme::FG_MUTED };
    ui.painter().rect_filled(rect, 6, fill);
    ui.painter().galley(rect.center() - g.size() / 2.0, g, tc);
    if enabled {
        r.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        r
    }
}

/// Outlined ghost button with an optional icon (Clear, Sign out, Open folder).
pub fn ghost_button(ui: &mut Ui, icon: Option<&str>, label: &str, color: Color32, enabled: bool) -> Response {
    let mut job = egui::text::LayoutJob::default();
    if let Some(i) = icon {
        job.append(i, 0.0, egui::TextFormat::simple(theme::icon(13.0), color));
        job.append(label, 6.0, egui::TextFormat::simple(theme::font(12.5), color));
    } else {
        job.append(label, 0.0, egui::TextFormat::simple(theme::font(12.5), color));
    }
    let g = ui.painter().layout_job(job);
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, r) = ui.allocate_exact_size(Vec2::new(g.size().x + 22.0, 28.0), sense);
    let t = ui.ctx().animate_bool_with_time(r.id, r.hovered() && enabled, 0.12);
    let p = ui.painter();
    if t > 0.0 {
        p.rect_filled(rect, 6, theme::SECONDARY.gamma_multiply(t));
    }
    p.rect_stroke(rect, 6, Stroke::new(1.0, theme::CARD_BORDER), StrokeKind::Inside);
    p.galley_with_override_text_color(rect.center() - g.size() / 2.0, g, if enabled { color } else { color.gamma_multiply(0.4) });
    if enabled {
        r.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        r
    }
}

/// Single-line text field (28 h, recessed fill, focus ring).
pub fn text_field(ui: &mut Ui, text: &mut String, width: f32, hint: &str, password: bool) -> Response {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 28.0), Sense::hover());
    let id = ui.scope_id().with(("field", hint));
    let focused = ui.ctx().memory(|m| m.has_focus(id));
    ui.painter().rect_filled(rect, 6, theme::FIELD_BG);
    ui.painter().rect_stroke(rect, 6, Stroke::new(1.0, if focused { theme::PRIMARY } else { theme::CARD_BORDER }), StrokeKind::Inside);
    let inner = rect.shrink2(Vec2::new(9.0, 0.0));
    ui.put(
        inner,
        egui::TextEdit::singleline(text)
            .id(id)
            .password(password)
            .frame(egui::Frame::NONE)
            .font(theme::font(12.5))
            .align(egui::Align2::LEFT_CENTER)
            .hint_text(egui::RichText::new(hint).color(theme::PLACEHOLDER))
            .desired_width(inner.width()),
    )
}

/// Small segmented control. Returns true when the value changed.
pub fn segmented<T: PartialEq + Copy>(ui: &mut Ui, value: &mut T, options: &[(T, &str)]) -> bool {
    let mut changed = false;
    let widths: Vec<f32> = options.iter().map(|(_, l)| ui.painter().layout_no_wrap((*l).to_owned(), theme::font(12.0), theme::FG).size().x + 20.0).collect();
    let total: f32 = widths.iter().sum::<f32>() + 4.0;
    let (track, _) = ui.allocate_exact_size(Vec2::new(total, 26.0), Sense::hover());
    ui.painter().rect_filled(track, 6, theme::FIELD_BG);
    ui.painter().rect_stroke(track, 6, Stroke::new(1.0, theme::CARD_BORDER), StrokeKind::Inside);
    let mut x = track.left() + 2.0;
    for (i, (v, l)) in options.iter().enumerate() {
        let rect = Rect::from_min_size(egui::pos2(x, track.top() + 2.0), Vec2::new(widths[i], 22.0));
        x += widths[i];
        let r = ui.interact(rect, ui.scope_id().with(("seg", i, *l)), Sense::click());
        let active = *value == *v;
        if active {
            ui.painter().rect_filled(rect, 4, theme::SECONDARY);
        } else if r.hovered() {
            ui.painter().rect_filled(rect, 4, theme::ROW_HOVER);
        }
        let c = if active { theme::FG } else { theme::FG_MUTED };
        ui.painter().text(rect.center(), Align2::CENTER_CENTER, *l, theme::font(12.0), c);
        if r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() && !active {
            *value = *v;
            changed = true;
        }
    }
    changed
}

/// [`segmented`] whose options can be unavailable: drawn dim, not clickable, with `why` on hover.
pub fn segmented_some<T: PartialEq + Copy>(ui: &mut Ui, value: &mut T, options: &[(T, &str, bool)], why: &str) -> bool {
    let mut changed = false;
    let widths: Vec<f32> = options.iter().map(|(_, l, _)| ui.painter().layout_no_wrap((*l).to_owned(), theme::font(12.0), theme::FG).size().x + 20.0).collect();
    let total: f32 = widths.iter().sum::<f32>() + 4.0;
    let (track, _) = ui.allocate_exact_size(Vec2::new(total, 26.0), Sense::hover());
    ui.painter().rect_filled(track, 6, theme::FIELD_BG);
    ui.painter().rect_stroke(track, 6, Stroke::new(1.0, theme::CARD_BORDER), StrokeKind::Inside);
    let mut x = track.left() + 2.0;
    for (i, (v, l, available)) in options.iter().enumerate() {
        let rect = Rect::from_min_size(egui::pos2(x, track.top() + 2.0), Vec2::new(widths[i], 22.0));
        x += widths[i];
        let r = ui.interact(rect, ui.scope_id().with(("seg", i, *l)), if *available { Sense::click() } else { Sense::hover() });
        let active = *value == *v;
        if active {
            ui.painter().rect_filled(rect, 4, theme::SECONDARY);
        } else if *available && r.hovered() {
            ui.painter().rect_filled(rect, 4, theme::ROW_HOVER);
        }
        let c = if active {
            theme::FG
        } else if *available {
            theme::FG_MUTED
        } else {
            theme::FG_MUTED.gamma_multiply(0.45)
        };
        ui.painter().text(rect.center(), Align2::CENTER_CENTER, *l, theme::font(12.0), c);
        if !*available {
            r.on_hover_text(why);
            continue;
        }
        if r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() && !active {
            *value = *v;
            changed = true;
        }
    }
    changed
}
