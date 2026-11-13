//! Account row pinned at the bottom of the sidebar, plus the sign-in card used on Home.
//!
//! Both offer the Antigravity sign-in. It signs in with Antigravity's own OAuth client and needs
//! nothing set up, while the Gemini CLI sign-in (Customizations > Account) uses gemini-cli's
//! client and needs its secret. Offering the one that always works keeps "Sign in" meaning one
//! thing; the Gemini CLI sign-in stays on the Account tab for whoever wants it.

use egui::{Align2, Rect, Sense, Ui, Vec2};

use super::{icons, theme, widgets, CustomizeTab, UiAction, UiState, View};
use crate::app::AppCommand;
use crate::auth::external::{AccountProvider, AccountStatus};
use crate::auth::AuthStatus;

/// The glyph that stands for a subscription account's provider.
pub fn provider_icon(p: AccountProvider) -> &'static str {
    match p {
        AccountProvider::Antigravity => icons::BRAIN,
        AccountProvider::Codex => icons::CHATS_CIRCLE,
        AccountProvider::ClaudeCode => icons::SIGN_IN,
    }
}

/// Bottom-of-sidebar account row: who is signed in, and a gear that opens Customizations >
/// Account. A signed-in subscription account is named first (the one in use, else the first),
/// then the Gemini CLI sign-in's Google account. Signed out it is a "Sign in" pill that starts
/// the Antigravity sign-in.
pub fn account_row(ui: &mut Ui, state: &mut UiState, commands: &mut Vec<UiAction>) {
    let w = ui.available_width();
    ui.add_space(4.0);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(w, 36.0), Sense::hover());
    let gear = Rect::from_min_size(egui::pos2(rect.right() - 30.0, rect.center().y - 14.0), Vec2::splat(28.0));
    if widgets::overlay_icon(ui, gear, ui.scope_id().with("gear"), icons::GEAR_SIX, "Customizations", true).clicked() {
        state.customize_tab = CustomizeTab::Account;
        state.navigate(View::Customizations);
    }
    let body = Rect::from_min_max(rect.min, egui::pos2(gear.left() - 6.0, rect.max.y));
    if let Some(view) = state.shown_account() {
        let name = view.email.clone().unwrap_or_else(|| view.provider.label().to_owned());
        let p = ui.painter();
        p.text(body.left_center() + Vec2::new(10.0, 0.0), Align2::LEFT_CENTER, provider_icon(view.provider), theme::icon(18.0), theme::FG_SECONDARY);
        let text_rect = Rect::from_min_max(body.min + Vec2::new(36.0, 0.0), body.max);
        widgets::paint_truncated(ui, text_rect, &name, theme::font(13.0), theme::FG_SECONDARY);
        ui.interact(body, ui.scope_id().with("account"), Sense::hover()).on_hover_text(format!("{} account {name}", view.provider.label()));
        return;
    }
    match &state.auth {
        AuthStatus::LoggedIn { email } => {
            let p = ui.painter();
            p.text(body.left_center() + Vec2::new(10.0, 0.0), Align2::LEFT_CENTER, icons::USER_CIRCLE, theme::icon(18.0), theme::FG_SECONDARY);
            let text_rect = Rect::from_min_max(body.min + Vec2::new(36.0, 0.0), body.max);
            widgets::paint_truncated(ui, text_rect, email.as_deref().unwrap_or("Google account"), theme::font(13.0), theme::FG_SECONDARY);
        }
        AuthStatus::LoggingIn => {
            if signing_in(ui, body, "cancel_login") {
                commands.push(AppCommand::CancelLogin);
            }
        }
        AuthStatus::LoggedOut | AuthStatus::Error(_) => {
            // An account sign-in in flight; Cancel goes by its attempt id (its row has no
            // account id yet).
            if let Some(attempt) = state.running_account_login().map(|l| l.id) {
                if signing_in(ui, body, "cancel_account_login") {
                    commands.push(AppCommand::CancelAccountLogin(attempt));
                }
                return;
            }
            // No client secret is needed, so the pill works whatever the Gemini CLI sign-in's
            // state. Only a running turn stops it (the backend would refuse the sign-in).
            let enabled = !state.busy;
            let pill = Rect::from_min_size(body.min + Vec2::new(4.0, 4.0), Vec2::new(body.width().min(120.0), 28.0));
            let r = ui.interact(pill, ui.scope_id().with("signin"), if enabled { Sense::click() } else { Sense::hover() });
            let fill = if !enabled {
                theme::SECONDARY
            } else if r.hovered() {
                theme::PRIMARY.gamma_multiply(1.2)
            } else {
                theme::PRIMARY
            };
            ui.painter().rect_filled(pill, 14, fill);
            let label = format!("{}  Sign in", icons::SIGN_IN);
            ui.painter().text(pill.center(), Align2::CENTER_CENTER, label, theme::font(13.0), theme::FG);
            if !enabled {
                r.on_hover_text("Stop the running turn first");
            } else if r.on_hover_text("Sign in with your Antigravity account").clicked() {
                state.account_errors.remove(&AccountProvider::Antigravity);
                commands.push(AppCommand::SignInAccount(AccountProvider::Antigravity));
            }
        }
    }
}

/// The "Signing in… (cancel)" line with its spinner; returns whether it was clicked.
fn signing_in(ui: &mut Ui, body: Rect, salt: &str) -> bool {
    let s = Rect::from_center_size(body.left_center() + Vec2::new(18.0, 0.0), Vec2::splat(14.0));
    egui::Spinner::new().size(14.0).color(theme::FG_MUTED).paint_at(ui, s);
    let cancel = Rect::from_min_max(body.min + Vec2::new(36.0, 4.0), body.max - Vec2::new(0.0, 4.0));
    let r = ui.interact(cancel, ui.scope_id().with(salt), Sense::click());
    let c = if r.hovered() { theme::FG } else { theme::FG_SECONDARY };
    widgets::paint_truncated(ui, cancel, "Signing in… (cancel)", theme::font(13.0), c);
    ui.ctx().request_repaint();
    r.clicked()
}

/// What the Home card's button does.
enum Action {
    /// Start this provider's sign-in.
    SignIn(AccountProvider),
    /// Stop the account sign-in with this attempt id.
    Cancel(u64),
    /// Send turns to this signed-in account.
    Use(String),
}

/// Sign-in prompt on the Home view when the user cannot send yet: a compact card under the
/// composer with an icon, a short explanation and one action. Home shows it while turns go to
/// Code Assist and no Google session is signed in, and while the account in use has expired.
pub fn home_notice(ui: &mut Ui, state: &mut UiState, commands: &mut Vec<UiAction>) {
    let lapsed = state.lapsed_active_account().map(|v| {
        let reason = match &v.status {
            AccountStatus::Expired(r) | AccountStatus::Error(r) => Some(r.clone()),
            _ => None,
        };
        (v.provider, reason)
    });
    let has_lapsed = lapsed.is_some();
    if !has_lapsed && matches!(state.auth, AuthStatus::LoggedIn { .. } | AuthStatus::LoggingIn) {
        return;
    }
    let antigravity = AccountProvider::Antigravity;
    let (icon, accent, title, mut text, detail, label, action) = if let Some(l) = state.running_account_login() {
        let provider = l.provider.label();
        (icons::SIGN_IN, theme::FG_SECONDARY, format!("Signing in with {provider}"), "Finish the sign-in in your browser.".to_owned(), None, "Cancel".to_owned(), Action::Cancel(l.id))
    } else if let Some((p, reason)) = lapsed {
        // Turns go to this account, and it can no longer send them.
        let provider = p.label();
        (
            icons::WARNING,
            theme::WARNING,
            format!("The {provider} session ended"),
            format!("Sign in with your {provider} account again to keep using it."),
            reason,
            "Sign in again".to_owned(),
            Action::SignIn(p),
        )
    } else if let Some(v) = state.shown_account() {
        // Signed in, but Code Assist was chosen (it needs a Google sign-in): the account is
        // one click away, no browser round trip needed.
        let provider = v.provider.label();
        (
            provider_icon(v.provider),
            theme::FG_SECONDARY,
            "Google Code Assist needs a Google sign-in".to_owned(),
            format!("Use your {provider} account to start a conversation."),
            None,
            format!("Use {provider}"),
            Action::Use(v.id.clone()),
        )
    } else {
        let error = state.account_errors.get(&antigravity).cloned();
        let (icon, accent, title) = match &error {
            Some(_) => (icons::WARNING, theme::ERROR, "Sign-in failed"),
            None => (icons::USER_CIRCLE, theme::FG_SECONDARY, "You are signed out"),
        };
        let text = "Sign in with your Antigravity account to start a conversation.".to_owned();
        (icon, accent, title.to_owned(), text, error, "Sign in with Antigravity".to_owned(), Action::SignIn(antigravity))
    };
    // A usable session another Google tool saved works without any sign-in.
    if matches!(action, Action::SignIn(_)) && !has_lapsed {
        if let Some(s) = state.import_check.as_ref().and_then(|c| c.usable()) {
            text.push_str(&format!(" Or use the existing {} from Customizations > Account.", s.issuer.session_noun()));
        }
    }
    let primary = !matches!(action, Action::Cancel(_));
    widgets::card(12, 0).inner_margin(egui::Margin { left: 14, right: 12, top: 10, bottom: 10 }).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 10.0;
            if matches!(action, Action::Cancel(_)) {
                ui.add(egui::Spinner::new().size(16.0).color(theme::FG_MUTED));
            } else {
                ui.label(egui::RichText::new(icon).font(theme::icon(18.0)).color(accent));
            }
            // The button keeps its full label: the text column takes what is left.
            let button_w = action_width(ui, &label);
            let text_w = (ui.available_width() - button_w - 10.0).max(120.0);
            ui.vertical(|ui| {
                ui.set_width(text_w);
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.label(egui::RichText::new(&title).font(theme::medium(13.0)).color(theme::FG));
                ui.add(egui::Label::new(egui::RichText::new(text.as_str()).font(theme::font(12.0)).color(theme::FG_MUTED)).wrap());
                if let Some(d) = &detail {
                    ui.add(egui::Label::new(egui::RichText::new(d).font(theme::font(11.5)).color(theme::ERROR.gamma_multiply(0.8))).truncate());
                }
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if action_button(ui, &label, primary).clicked() {
                    match &action {
                        Action::SignIn(p) => {
                            state.account_errors.remove(p);
                            commands.push(AppCommand::SignInAccount(*p));
                        }
                        Action::Cancel(id) => commands.push(AppCommand::CancelAccountLogin(*id)),
                        Action::Use(id) => commands.push(AppCommand::UseAccount(id.clone())),
                    }
                }
            });
        });
    });
}

/// Width [`action_button`] takes for `label`.
fn action_width(ui: &Ui, label: &str) -> f32 {
    ui.painter().layout_no_wrap(label.to_owned(), theme::medium(12.5), theme::FG).size().x + 28.0
}

/// Pill button: primary fill (`primary`) or a neutral `secondary` fill.
fn action_button(ui: &mut Ui, label: &str, primary: bool) -> egui::Response {
    let g = ui.painter().layout_no_wrap(label.to_owned(), theme::medium(12.5), theme::FG);
    let (rect, r) = ui.allocate_exact_size(Vec2::new(g.size().x + 28.0, 28.0), Sense::click());
    let t = ui.ctx().animate_bool_with_time(r.id, r.hovered(), 0.12);
    let (base, hover, tc) = if primary {
        (theme::PRIMARY, theme::PRIMARY.lerp_to_gamma(egui::Color32::WHITE, 0.12), egui::Color32::WHITE)
    } else {
        (theme::SECONDARY, theme::PRESSED, theme::FG)
    };
    ui.painter().rect_filled(rect, 14, base.lerp_to_gamma(hover, t));
    ui.painter().galley(rect.center() - g.size() / 2.0, g, tc);
    r.on_hover_cursor(egui::CursorIcon::PointingHand)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{AccountLoginInfo, AppEvent};
    use crate::auth::antigravity::{ImportCheck, ImportSummary, Issuer};
    use crate::auth::external::{AccountStatus, AccountView};
    use crate::ui::test_util::{click, context, painted};

    #[test]
    fn signed_out_home_points_to_a_usable_existing_session() {
        let ctx = context();
        let mut state = UiState { auth: AuthStatus::LoggedOut, oauth_source: Some(crate::auth::ClientSource::File), ..UiState::default() };
        let mut commands = Vec::new();
        let hint = "Or use the existing Antigravity session from Customizations > Account.";
        let texts = painted(&ctx, Vec::new(), |ui| home_notice(ui, &mut state, &mut commands));
        assert!(!texts.iter().any(|(t, _)| t.contains(hint)), "nothing found, nothing pointed to");
        let usable = ImportSummary {
            source: std::path::PathBuf::from("oauth_creds.json"),
            issuer: Issuer::Antigravity,
            account: None,
            expiry: None,
            has_refresh_token: false,
            code_assist_scope: None,
            client_configured: true,
            can_refresh: false,
            usable: true,
        };
        state.import_check = Some(ImportCheck::Found(usable.clone()));
        let texts = painted(&ctx, Vec::new(), |ui| home_notice(ui, &mut state, &mut commands));
        assert!(texts.iter().any(|(t, _)| t.contains(hint)), "{texts:?}");
        // An expired one is not offered.
        state.import_check = Some(ImportCheck::Found(ImportSummary { usable: false, ..usable }));
        let texts = painted(&ctx, Vec::new(), |ui| home_notice(ui, &mut state, &mut commands));
        assert!(!texts.iter().any(|(t, _)| t.contains(hint)));
        assert!(commands.is_empty());
    }

    /// Signed out, and no client secret for the Gemini CLI sign-in anywhere.
    fn without_a_client_secret() -> UiState {
        let mut state = UiState::default();
        state.apply(AppEvent::OAuthClientSource(None));
        state.apply(AppEvent::Auth(AuthStatus::Error("OAuth client is not configured".into())));
        assert!(state.oauth_not_configured());
        state
    }

    /// Press and release on the text ending with `label`, drawing with `draw`; the commands sent.
    fn click_label(ctx: &egui::Context, state: &mut UiState, label: &str, draw: fn(&mut Ui, &mut UiState, &mut Vec<UiAction>)) -> Vec<UiAction> {
        let mut commands = Vec::new();
        let texts = painted(ctx, Vec::new(), |ui| draw(ui, state, &mut commands));
        let at = texts.iter().find(|(t, _)| t.ends_with(label)).map(|(_, r)| r.center()).unwrap_or_else(|| panic!("no {label:?} in {texts:?}"));
        painted(ctx, vec![egui::Event::PointerMoved(at), click(at, true)], |ui| draw(ui, state, &mut commands));
        painted(ctx, vec![click(at, false)], |ui| draw(ui, state, &mut commands));
        commands
    }

    #[test]
    fn home_signs_in_with_antigravity_without_a_client_secret() {
        let ctx = context();
        let mut state = without_a_client_secret();
        let mut commands = Vec::new();
        let texts = painted(&ctx, Vec::new(), |ui| home_notice(ui, &mut state, &mut commands));
        let shows = |part: &str| texts.iter().any(|(t, _)| t.contains(part));
        assert!(shows("You are signed out"), "{texts:?}");
        assert!(shows("Sign in with your Antigravity account to start a conversation."));
        assert!(!shows("Set up sign-in") && !shows("client secret"), "the secret is not needed for this sign-in");
        let sent = click_label(&ctx, &mut state, "Sign in with Antigravity", home_notice);
        assert!(matches!(sent.as_slice(), [AppCommand::SignInAccount(AccountProvider::Antigravity)]), "{sent:?}");

        // It failed: the card says so, and the same button tries again.
        state.account_errors.insert(AccountProvider::Antigravity, "port 51121 on localhost is already in use".into());
        let texts = painted(&ctx, Vec::new(), |ui| home_notice(ui, &mut state, &mut commands));
        assert!(texts.iter().any(|(t, _)| t == "Sign-in failed") && texts.iter().any(|(t, _)| t.contains("51121")));
        let sent = click_label(&ctx, &mut state, "Sign in with Antigravity", home_notice);
        assert!(matches!(sent.as_slice(), [AppCommand::SignInAccount(AccountProvider::Antigravity)]));
        assert!(!state.account_errors.contains_key(&AccountProvider::Antigravity), "cleared as the attempt starts");
    }

    fn start_sign_in(state: &mut UiState, provider: AccountProvider, attempt: u64) {
        state.apply(AppEvent::AccountLogin { login: Some(AccountLoginInfo { id: attempt, provider, auth_url: "https://accounts.example/".into(), error: None }) });
        state.apply(AppEvent::Accounts(vec![AccountView {
            id: String::new(),
            provider,
            email: None,
            plan: None,
            model: String::new(),
            models: Vec::new(),
            models_fetched_at: None,
            status: AccountStatus::SigningIn,
        }]));
    }

    #[test]
    fn home_cancels_a_running_sign_in_by_its_attempt() {
        let ctx = context();
        let mut state = without_a_client_secret();
        start_sign_in(&mut state, AccountProvider::Antigravity, 41);
        let sent = click_label(&ctx, &mut state, "Cancel", home_notice);
        assert!(matches!(sent.as_slice(), [AppCommand::CancelAccountLogin(41)]), "{sent:?}");
    }

    #[test]
    fn the_sidebar_pill_signs_in_with_antigravity_without_a_client_secret() {
        let ctx = context();
        let mut state = without_a_client_secret();
        let sent = click_label(&ctx, &mut state, "Sign in", account_row);
        assert!(matches!(sent.as_slice(), [AppCommand::SignInAccount(AccountProvider::Antigravity)]), "{sent:?}");

        // While that attempt runs the row cancels it, by the attempt's id.
        start_sign_in(&mut state, AccountProvider::Antigravity, 42);
        let mut commands = Vec::new();
        let texts = painted(&ctx, Vec::new(), |ui| account_row(ui, &mut state, &mut commands));
        assert!(!texts.iter().any(|(t, _)| t.ends_with("Sign in")), "no second sign-in meanwhile");
        let sent = click_label(&ctx, &mut state, "Signing in… (cancel)", account_row);
        assert!(matches!(sent.as_slice(), [AppCommand::CancelAccountLogin(42)]), "{sent:?}");

        // Signed in: the row names the account, and the gear opens the Account tab.
        state.apply(AppEvent::Accounts(vec![AccountView {
            id: "antigravity-0123abcd".into(),
            provider: AccountProvider::Antigravity,
            email: Some("me@example.com".into()),
            plan: None,
            model: "gemini-3-flash".into(),
            models: Vec::new(),
            models_fetched_at: None,
            status: AccountStatus::SignedIn,
        }]));
        let texts = painted(&ctx, Vec::new(), |ui| account_row(ui, &mut state, &mut commands));
        assert!(texts.iter().any(|(t, _)| t == "me@example.com"), "{texts:?}");
        assert!(texts.iter().any(|(t, _)| t == icons::BRAIN), "with Antigravity's glyph");
        assert!(!texts.iter().any(|(t, _)| t.ends_with("Sign in")));
        let gear = texts.iter().find(|(t, _)| t == icons::GEAR_SIX).map(|(_, r)| r.center()).expect("the gear");
        painted(&ctx, vec![egui::Event::PointerMoved(gear), click(gear, true)], |ui| account_row(ui, &mut state, &mut commands));
        painted(&ctx, vec![click(gear, false)], |ui| account_row(ui, &mut state, &mut commands));
        assert_eq!((state.view, state.customize_tab), (View::Customizations, CustomizeTab::Account));
        assert!(commands.is_empty());
    }

    #[test]
    fn home_asks_to_sign_in_again_when_the_account_in_use_expired() {
        let ctx = context();
        // Google is signed in too: it does not send turns while an account is in use.
        let mut state = UiState { auth: AuthStatus::LoggedIn { email: Some("me@example.com".into()) }, ..UiState::default() };
        state.settings.accounts = vec![crate::settings::AccountEntry {
            id: "codex-0123abcd".into(),
            provider: AccountProvider::Codex,
            label: "dev@example.com".into(),
            model: "gpt-5.1-codex".into(),
        }];
        state.settings.active_account = Some("codex-0123abcd".into());
        state.apply(AppEvent::Accounts(vec![AccountView {
            id: "codex-0123abcd".into(),
            provider: AccountProvider::Codex,
            email: Some("dev@example.com".into()),
            plan: None,
            model: "gpt-5.1-codex".into(),
            models: Vec::new(),
            models_fetched_at: None,
            status: AccountStatus::Expired("session expired, please log in again".into()),
        }]));
        assert!(state.any_backend_ready(), "still selected");
        let mut commands = Vec::new();
        let texts = painted(&ctx, Vec::new(), |ui| crate::ui::home::show(ui, &mut state, &mut commands));
        assert!(texts.iter().any(|(t, _)| t == "The ChatGPT (Codex) session ended"), "{texts:?}");
        assert!(texts.iter().any(|(t, _)| t == "session expired, please log in again"));
        let sent = click_label(&ctx, &mut state, "Sign in again", home_notice);
        assert!(matches!(sent.as_slice(), [AppCommand::SignInAccount(AccountProvider::Codex)]), "{sent:?}");
        // Signed in again: the card goes.
        let mut rows = state.accounts.clone();
        rows[0].status = AccountStatus::SignedIn;
        state.apply(AppEvent::Accounts(rows));
        let texts = painted(&ctx, Vec::new(), |ui| crate::ui::home::show(ui, &mut state, &mut commands));
        assert!(!texts.iter().any(|(t, _)| t.contains("session ended") || t.contains("Sign in")), "{texts:?}");
        assert!(commands.is_empty());
    }

    #[test]
    fn home_offers_a_signed_in_account_that_is_not_in_use() {
        let ctx = context();
        let mut state = without_a_client_secret();
        state.apply(AppEvent::Accounts(vec![AccountView {
            id: "codex-0123abcd".into(),
            provider: AccountProvider::Codex,
            email: Some("dev@example.com".into()),
            plan: None,
            model: "gpt-5.1-codex".into(),
            models: Vec::new(),
            models_fetched_at: None,
            status: AccountStatus::SignedIn,
        }]));
        let sent = click_label(&ctx, &mut state, "Use ChatGPT (Codex)", home_notice);
        assert!(matches!(sent.as_slice(), [AppCommand::UseAccount(id)] if id == "codex-0123abcd"), "{sent:?}");
    }
}
