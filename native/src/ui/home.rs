//! Home screen (DESIGN.md 2.4, `_ref/ui/01-home.png`): workspace chip, centered composer card
//! (with its "Local" tray) and, when nothing can send a turn yet, a sign-in card under it.

use egui::{Rect, Sense, Stroke, Ui, Vec2};

use super::{composer, conversation, icons, login, theme, widgets, UiAction, UiState};
use crate::auth::AuthStatus;

/// Outer composer card width on Home (reference x 467..1168 at 1400x900).
const HOME_WIDTH: f32 = 701.0;
/// Approximate height of chip + composer + notice, used to center the group.
const GROUP_HEIGHT: f32 = 210.0;
/// Outer card width minus its content width.
const CARD_EXTRA: f32 = 4.0;

pub fn show(ui: &mut Ui, state: &mut UiState, commands: &mut Vec<UiAction>) {
    let avail = ui.available_rect_before_wrap();
    // The card frame paints outside its content width (stroke and rounding): content width
    // HOME_WIDTH - 4, offset 3.5px from the outer left edge, gives the reference x 467..1167.
    let w = (avail.width() - 48.0).clamp(200.0, HOME_WIDTH.min(theme::COLUMN_WIDTH)) - CARD_EXTRA;
    // Center on the true main area (sidebar edge to window edge); the panel's available rect
    // stops short of the window's right edge.
    let center_x = (avail.left() + ui.ctx().content_rect().right().max(avail.right())) / 2.0;
    // Slightly above the optical center, as in the reference.
    let top = avail.top() + ((avail.height() - GROUP_HEIGHT) / 2.0 - 39.0).max(16.0);
    let col = Rect::from_min_size(egui::pos2(center_x - (w + CARD_EXTRA) / 2.0 + 3.5, top), Vec2::new(w, (avail.bottom() - top).max(0.0)));
    ui.scope_builder(egui::UiBuilder::new().max_rect(col), |ui| {
        ui.set_width(w);
        ui.spacing_mut().item_spacing.y = 8.0;
        if let Some(b) = state.banner.clone() {
            conversation::banner(ui, state, &b, commands);
        }
        if super::bypass::band(ui, state) {
            ui.add_space(6.0);
        }
        ui.horizontal(|ui| {
            ui.add_space(2.0);
            composer::workspace_chip(ui, state, commands);
        });
        ui.add_space(5.0);
        composer::show(ui, state, commands, true);
        ui.add_space(4.0);
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
            info_card(ui);
        }
    });
}

/// Notice card under the composer: `#1c1c1c`, 1px `#282828`, radius 12.
fn notice_card() -> egui::Frame {
    widgets::card(12, 10).stroke(Stroke::new(1.0, theme::POPUP_BORDER))
}

/// Signed-in info card (reference y 490..558): how tools are confirmed. Dismissable for the
/// session (UI-only flag in egui memory).
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
                ui.spacing_mut().item_spacing.y = 10.0;
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
    use crate::ui::test_util::{context, painted};

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
}
