//! Home screen (DESIGN.md 2.4): a greeting at the top with starter suggestions, and the
//! message box pinned to the bottom with the project picker (and its Git branch) above it.
//! When nothing can send a turn yet, the sign-in card sits under the greeting instead.

use chrono::Timelike;
use egui::{Align2, Rect, Sense, Stroke, Ui, Vec2};

use super::{composer, conversation, git, icons, login, theme, widgets, UiAction, UiState};
use crate::auth::AuthStatus;

/// Widest the greeting's cards get.
const NOTICE_WIDTH: f32 = 560.0;
/// Starter suggestions: a label and the prompt it puts in the box.
pub const SUGGESTIONS: [(&str, &str); 4] = [
    ("Explain this project", "Explain how this project is organized: its main parts, how they fit together, and where to start reading."),
    ("Find bugs", "Look through the code for likely bugs and list them with the file and line, most serious first. Do not change anything yet."),
    ("Write tests", "Find an important part of this project that has no tests and write tests for it."),
    ("Plan a feature", "Help me plan a new feature: ask me what it should do, then write an implementation plan before changing any code."),
];

/// "Good morning" / "Good afternoon" / "Good evening" for a local hour.
pub fn greeting(hour: u32) -> &'static str {
    match hour {
        5..=11 => "Good morning",
        12..=17 => "Good afternoon",
        _ => "Good evening",
    }
}

/// The line under the greeting.
fn subtitle(state: &UiState) -> String {
    match state.workspace.as_deref() {
        Some(ws) => format!("What should we work on in {}?", super::format::project_label(ws)),
        None => "Open a project folder to get started.".to_owned(),
    }
}

/// Width of the message box: the conversation's column, so the first prompt does not move it.
fn column(avail: f32) -> (f32, f32) {
    let max = theme::column_max().unwrap_or(f32::INFINITY);
    let w = (avail - 64.0).min(max).max(200.0_f32.min(avail));
    (((avail - w) / 2.0).max(0.0), w)
}

pub fn show(ui: &mut Ui, state: &mut UiState, commands: &mut Vec<UiAction>) {
    let (pad, w) = column(ui.available_width());

    // The message box, pinned to the bottom like a conversation's. No fill, so the card's
    // rounded corners show.
    let dock = egui::Panel::bottom("home_composer_dock")
        .show_separator_line(false)
        .frame(egui::Frame::NONE.inner_margin(egui::Margin { left: 0, right: 0, top: 8, bottom: 14 }))
        .show(ui, |ui| {
            let top = ui.cursor().top();
            let rect = Rect::from_min_size(egui::pos2(ui.max_rect().left() + pad, top), Vec2::new(w, 400.0));
            ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
                ui.set_width(rect.width());
                ui.spacing_mut().item_spacing.y = 6.0;
                if let Some(b) = state.banner.clone() {
                    conversation::banner(ui, state, &b, commands);
                }
                if super::bypass::band(ui, state) {
                    ui.add_space(2.0);
                }
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    composer::workspace_chip(ui, state, commands);
                    branch_label(ui, state);
                });
                composer::show(ui, state, commands, true);
            });
        });
    // Same guard as the conversation's dock: a box that grew a line is laid out again at once.
    let dock_h = dock.response.rect.height();
    let dock_h_id = ui.make_persistent_id("home_dock_height");
    if ui.data(|d| d.get_temp::<f32>(dock_h_id)).is_none_or(|h| (h - dock_h).abs() > 0.5) {
        ui.data_mut(|d| d.insert_temp(dock_h_id, dock_h));
        ui.ctx().request_discard("the home composer changed height");
    }

    // The greeting area above it.
    let area = ui.available_rect_before_wrap();
    let top = area.top() + (area.height() * 0.2).clamp(24.0, 160.0);
    let cw = w.min(NOTICE_WIDTH);
    let col = Rect::from_min_max(egui::pos2(area.center().x - cw / 2.0, top), egui::pos2(area.center().x + cw / 2.0, area.bottom()));
    ui.scope_builder(egui::UiBuilder::new().max_rect(col), |ui| {
        ui.set_width(cw);
        ui.spacing_mut().item_spacing.y = 6.0;
        let hour = chrono::Local::now().hour();
        ui.vertical_centered(|ui| {
            ui.label(egui::RichText::new(greeting(hour)).font(theme::medium(28.0)).color(theme::FG));
            ui.add_space(2.0);
            ui.label(egui::RichText::new(subtitle(state)).font(theme::font(15.0)).color(theme::FG_MUTED));
        });
        ui.add_space(22.0);
        // Turns on an account or a provider profile need no Google account, and a Google session
        // that is signed in (or signing in) serves Code Assist: no sign-in card then. Unless the
        // account in use has expired: every turn is refused until it is signed in again.
        let signed_out = !matches!(state.auth, AuthStatus::LoggedIn { .. } | AuthStatus::LoggingIn);
        if (signed_out && !state.any_backend_ready()) || state.lapsed_active_account().is_some() {
            notice_card().show(ui, |ui| {
                ui.set_width(ui.available_width());
                login::home_notice(ui, state, commands);
            });
        } else {
            if state.workspace.is_some() {
                suggestions(ui, state);
                ui.add_space(14.0);
            }
            info_card(ui);
        }
    });
}

/// The checked-out branch of the project folder, beside the project picker.
fn branch_label(ui: &mut Ui, state: &UiState) {
    let Some(ws) = state.workspace.as_deref() else { return };
    let Some(head) = git::head_cached(ui.ctx(), ws) else { return };
    let text = head.label().to_owned();
    let g = ui.painter().layout_no_wrap(text.clone(), theme::font(12.5), theme::FG_MUTED);
    let (rect, r) = ui.allocate_exact_size(Vec2::new(g.size().x.min(180.0) + 26.0, 24.0), Sense::hover());
    let p = ui.painter();
    p.text(egui::pos2(rect.left() + 6.0, rect.center().y), Align2::LEFT_CENTER, icons::GIT_BRANCH, theme::icon(13.0), theme::FG_MUTED);
    let tr = Rect::from_min_max(egui::pos2(rect.left() + 23.0, rect.top()), rect.max);
    widgets::paint_truncated(ui, tr, &text, theme::font(12.5), theme::FG_MUTED);
    let tip = match head {
        git::Head::Branch(b) => format!("On branch {b}"),
        git::Head::Detached(c) => format!("Detached HEAD at {c}"),
    };
    r.on_hover_text(tip);
}

/// Starter suggestions: pills that put a prompt in the box (nothing is sent).
fn suggestions(ui: &mut Ui, state: &mut UiState) {
    let font = theme::font(12.5);
    let widths: Vec<f32> = SUGGESTIONS.iter().map(|(l, _)| ui.painter().layout_no_wrap((*l).to_owned(), font.clone(), theme::FG).size().x + 26.0).collect();
    let gap = 8.0;
    let total: f32 = widths.iter().sum::<f32>() + gap * (widths.len() as f32 - 1.0);
    let avail = ui.available_width();
    // One centered row when it fits, else two.
    let split = if total <= avail { widths.len() } else { 2 };
    for range in [0..split, split..widths.len()].into_iter().filter(|r| !r.is_empty()) {
        let row_w: f32 = widths[range.clone()].iter().sum::<f32>() + gap * (range.len() as f32 - 1.0);
        let (row, _) = ui.allocate_exact_size(Vec2::new(avail, 30.0), Sense::hover());
        let mut x = row.center().x - row_w / 2.0;
        for i in range {
            let rect = Rect::from_min_size(egui::pos2(x, row.top()), Vec2::new(widths[i], 30.0));
            x += widths[i] + gap;
            let r = ui.interact(rect, ui.scope_id().with(("suggestion", i)), Sense::click());
            let hot = r.hovered();
            ui.painter().rect(rect, 15, if hot { theme::SECONDARY } else { theme::BG }, Stroke::new(1.0, theme::CARD_BORDER), egui::StrokeKind::Inside);
            let c = if hot { theme::FG } else { theme::FG_SECONDARY };
            ui.painter().text(rect.center(), Align2::CENTER_CENTER, SUGGESTIONS[i].0, font.clone(), c);
            if r.on_hover_text(SUGGESTIONS[i].1).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                state.input = SUGGESTIONS[i].1.to_owned();
                composer::request_focus(ui.ctx());
            }
        }
        ui.add_space(2.0);
    }
}

/// Notice card under the greeting: raised fill, 1px border, radius 12.
fn notice_card() -> egui::Frame {
    widgets::card(12, 10).stroke(Stroke::new(1.0, theme::POPUP_BORDER))
}

/// Signed-in info card: how tools are confirmed. Dismissable for the session (UI-only flag in
/// egui memory).
fn info_card(ui: &mut Ui) {
    let id = ui.make_persistent_id("home_info_dismissed");
    if ui.data(|d| d.get_temp::<bool>(id)).unwrap_or(false) {
        return;
    }
    let mut close = false;
    notice_card().inner_margin(egui::Margin { left: 13, right: 10, top: 9, bottom: 9 }).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 10.0;
            ui.label(egui::RichText::new(icons::INFO).font(theme::icon(16.0)).color(theme::FG_SECONDARY));
            ui.vertical(|ui| {
                ui.set_width(ui.available_width() - 30.0);
                ui.spacing_mut().item_spacing.y = 6.0;
                ui.label(egui::RichText::new("The agent works inside the selected folder").font(theme::font(12.0)).color(theme::FG_SECONDARY));
                ui.add(
                    egui::Label::new(
                        egui::RichText::new("Reads run automatically. File edits show a diff and shell commands ask before they run.")
                            .font(theme::font(12.5))
                            .color(theme::FG),
                    )
                    .wrap(),
                );
            });
            let (rect, r) = ui.allocate_exact_size(Vec2::splat(20.0), Sense::click());
            widgets::paint_icon_button(ui, rect, &r, icons::X, true, theme::FG_MUTED);
            close = r.on_hover_text("Dismiss").clicked();
        });
    });
    if close {
        ui.data_mut(|d| d.insert_temp(id, true));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::test_util::{click, context, painted};

    fn shows(texts: &[(String, egui::Rect)], part: &str) -> bool {
        texts.iter().any(|(t, _)| t.contains(part))
    }

    /// Turns go to the provider profile, which needs no Google account.
    #[test]
    fn no_sign_in_notice_while_a_provider_is_in_use() {
        let ctx = context();
        let mut state = UiState::default();
        state.settings.providers = vec![crate::providers::ProviderProfile {
            id: "p".into(),
            label: "P".into(),
            kind: crate::providers::ProviderKind::OpenAi,
            model: "gpt-4o-mini".into(),
            base_url: None,
            api_key: "k".into(),
            max_output_tokens: None,
            context_window: None,
            models: Vec::new(),
        }];
        state.settings.active_provider = Some("p".into());
        let mut commands = Vec::new();
        for auth in [AuthStatus::LoggedOut, AuthStatus::Error("OAuth client is not configured".into())] {
            state.auth = auth;
            let texts = painted(&ctx, Vec::new(), |ui| show(ui, &mut state, &mut commands));
            assert!(!shows(&texts, "signed out") && !shows(&texts, "Set up sign-in") && !shows(&texts, "Sign in"), "{texts:?}");
        }
        // On Code Assist the notice stays.
        state.settings.active_provider = None;
        state.auth = AuthStatus::LoggedOut;
        let texts = painted(&ctx, Vec::new(), |ui| show(ui, &mut state, &mut commands));
        assert!(shows(&texts, "You are signed out"));
        assert!(commands.is_empty());
    }

    /// An account in use sends turns with no Google sign-in; without one, and without a client
    /// secret, the card offers the Antigravity sign-in rather than the secret set-up.
    #[test]
    fn the_sign_in_card_follows_the_backend_in_use() {
        let ctx = context();
        let mut state = UiState::default();
        state.apply(crate::app::AppEvent::OAuthClientSource(None));
        state.apply(crate::app::AppEvent::Auth(AuthStatus::Error("OAuth client is not configured".into())));
        let mut commands = Vec::new();
        let texts = painted(&ctx, Vec::new(), |ui| show(ui, &mut state, &mut commands));
        assert!(shows(&texts, "You are signed out") && shows(&texts, "Sign in with Antigravity"), "{texts:?}");
        assert!(!shows(&texts, "Set up sign-in"), "{texts:?}");

        state.settings.accounts = vec![crate::settings::AccountEntry {
            id: "antigravity-0123abcd".into(),
            provider: crate::auth::external::AccountProvider::Antigravity,
            label: "me@example.com".into(),
            model: "gemini-3-flash".into(),
        }];
        state.settings.active_account = Some("antigravity-0123abcd".into());
        let texts = painted(&ctx, Vec::new(), |ui| show(ui, &mut state, &mut commands));
        assert!(!shows(&texts, "signed out") && !shows(&texts, "Sign in"), "{texts:?}");
        // A Google session signed in serves Code Assist: no card either.
        state.settings.active_account = None;
        state.auth = AuthStatus::LoggedIn { email: Some("me@example.com".into()) };
        let texts = painted(&ctx, Vec::new(), |ui| show(ui, &mut state, &mut commands));
        assert!(!shows(&texts, "signed out") && !shows(&texts, "Sign in"), "{texts:?}");
        assert!(commands.is_empty());
    }

    #[test]
    fn the_greeting_follows_the_hour() {
        assert_eq!(greeting(6), "Good morning");
        assert_eq!(greeting(13), "Good afternoon");
        assert_eq!(greeting(21), "Good evening");
        assert_eq!(greeting(2), "Good evening");
    }

    /// The greeting is above the message box, and a suggestion fills the box without sending.
    #[test]
    fn greeting_on_top_box_at_the_bottom_and_suggestions_fill_it() {
        let ctx = context();
        let mut state = UiState { auth: AuthStatus::LoggedIn { email: None }, workspace: Some(r"C:\work\demo-app".into()), ..UiState::default() };
        let mut commands = Vec::new();
        painted(&ctx, Vec::new(), |ui| show(ui, &mut state, &mut commands));
        let texts = painted(&ctx, Vec::new(), |ui| show(ui, &mut state, &mut commands));
        let hello = texts.iter().find(|(t, _)| t.starts_with("Good ")).expect("greeting").1;
        let placeholder = texts.iter().find(|(t, _)| t.starts_with("Ask anything")).expect("message box").1;
        assert!(hello.bottom() < placeholder.top(), "{hello:?} {placeholder:?}");
        assert!(placeholder.top() > 1300.0, "pinned near the bottom of the 1600px frame: {placeholder:?}");
        assert!(shows(&texts, "What should we work on in demo-app?"));
        let at = texts.iter().find(|(t, _)| t == "Find bugs").expect("suggestion").1.center();
        painted(&ctx, vec![egui::Event::PointerMoved(at), click(at, true)], |ui| show(ui, &mut state, &mut commands));
        painted(&ctx, vec![click(at, false)], |ui| show(ui, &mut state, &mut commands));
        assert!(state.input.starts_with("Look through the code"), "{:?}", state.input);
        assert!(commands.is_empty(), "nothing is sent");
    }
}
