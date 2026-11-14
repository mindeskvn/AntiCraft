//! The supported-models popup of a subscription account (Settings > Account, "Models").
//!
//! Modeled on the user's KiroTool popup: a title naming the account, a count line ("Supports
//! N/M models · updated HH:MM") beside a "Check again" button, and one pill per model, green when
//! the account serves it and muted when it does not.
//!
//! Both numbers of the count line come from one list: the provider's static catalog joined with
//! what the last fetch returned. KiroTool counted the fetched list (which held `auto`) over the
//! rendered one (which dropped it) and showed "8/7"; here `N` counts members of the very list
//! `M` measures, so `N <= M` holds whatever the provider sends.
//!
//! The popup never polls. "Check again" is a command; its result arrives as an
//! [`crate::app::AppEvent::AccountModels`] carrying the whole state, which replaces the old one,
//! so a lost event cannot leave the button stuck on "Checking…".

use std::collections::BTreeSet;

use egui::{CornerRadius, RichText, Sense, Stroke, StrokeKind, Ui, Vec2};

use super::{format, icons, theme, widgets, UiAction, UiState};
use crate::app::AppCommand;
use crate::auth::external::{catalog, valid_model_id, AccountView};

/// Popup width: KiroTool's dialog is 400 px wide plus its padding.
const POPUP_WIDTH: f32 = 420.0;
/// Chip height; the pill's corners are half of it.
const CHIP_H: f32 = 22.0;
/// Space left and right of a chip's content.
const CHIP_PAD: f32 = 10.0;
/// Diameter of the status dot.
const CHIP_DOT: f32 = 8.0;
/// Space between the dot and the label.
const CHIP_DOT_GAP: f32 = 6.0;
/// Space between chips, across and down.
const CHIP_GAP: f32 = 8.0;

/// The "Models" button on an account row. Opens the popup; consumes `open_account_models`.
///
/// Drawn in a scope keyed by the account id. The popup is tied to the button's id, and an
/// automatic id follows the row's position: with one, signing out of the account above would hand
/// an open popup — and its "Check again" — to whichever account moved into its place.
pub fn models_button(ui: &mut Ui, state: &mut UiState, commands: &mut Vec<UiAction>, view: &AccountView) -> egui::Response {
    let scope = egui::UiBuilder::new().scope_id(egui::Id::unique("anticraft_account_models").with(view.id.as_str()));
    ui.scope_builder(scope, |ui| {
        let r = widgets::ghost_button(ui, None, "Models", theme::FG, true);
        // A demo screen asks for the popup to be open on its first frame. Taken, like
        // `open_model_menu`, so it opens once; and only by the row it names.
        if state.open_account_models.take_if(|id| *id == view.id).is_some() {
            egui::Popup::open_id(ui.ctx(), egui::Popup::default_response_id(&r));
        }
        popup(&r, POPUP_WIDTH, |ui| show(ui, state, commands, view));
        r
    })
    .inner
}

/// The popup body (public so the demo screen can draw it, and so tests can drive it).
pub fn show(ui: &mut Ui, state: &mut UiState, commands: &mut Vec<UiAction>, view: &AccountView) {
    let popup = state.accounts_models.get(&view.id).cloned().unwrap_or_default();
    // Never checked: nothing to count yet. "0/3" would read as "serves nothing".
    let never_checked = view.models.is_empty() && view.models_fetched_at.is_none();
    ui.spacing_mut().item_spacing.y = 8.0;
    ui.label(RichText::new(format!("Supported models · {}", account_name(state, view))).font(theme::medium(13.5)).color(theme::FG));
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        if popup.checking {
            ui.add(egui::Spinner::new().size(14.0).color(theme::FG_MUTED));
            ui.label(RichText::new("Checking…").font(theme::font(12.0)).color(theme::FG_MUTED));
        } else if !never_checked {
            ui.label(RichText::new(count_line(view)).font(theme::font(12.0)).color(theme::FG_MUTED));
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            // Disabled while a check runs: the backend runs one per account at a time anyway,
            // and the label says why nothing happens.
            let r = if popup.checking {
                widgets::ghost_button(ui, None, "Checking…", theme::FG, false)
            } else {
                widgets::ghost_button(ui, Some(icons::ARROW_CLOCKWISE), "Check again", theme::FG, true)
            };
            if r.clicked() && !popup.checking {
                commands.push(AppCommand::CheckAccountModels(view.id.clone()));
            }
        });
    });
    // A failed check says why; the chips of the last good list stay below it.
    if let Some(e) = &popup.error {
        ui.add(egui::Label::new(RichText::new(e).font(theme::font(12.0)).color(theme::ERROR)).wrap());
    }
    if never_checked {
        if !popup.checking {
            ui.label(RichText::new("No model list yet. Press Check again.").font(theme::font(12.0)).color(theme::FG_MUTED));
        }
        return;
    }
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = Vec2::splat(CHIP_GAP);
        // One list for the chips and the count line, alphabetical (as KiroTool's), not split
        // into a served and an unserved group.
        for m in candidates(view) {
            let id = ui.scope_id().with(("model_chip", m.as_str()));
            chip(ui, id, &m, serves(view, &m));
        }
    });
}

/// One chip. `supported` picks the green or the muted style.
fn chip(ui: &mut Ui, id: egui::Id, text: &str, supported: bool) -> egui::Response {
    let (fg, fill, border) = if supported {
        (theme::SUCCESS, theme::SUCCESS.gamma_multiply(0.12), theme::SUCCESS.gamma_multiply(0.40))
    } else {
        (theme::FG_MUTED, theme::SECONDARY, theme::CARD_BORDER)
    };
    let galley = ui.painter().layout_no_wrap(text.to_owned(), theme::font(12.0), fg);
    let size = Vec2::new(CHIP_PAD + CHIP_DOT + CHIP_DOT_GAP + galley.size().x + CHIP_PAD, CHIP_H);
    let (_, rect) = ui.allocate_space(size);
    let r = ui.interact(rect, id, Sense::hover());
    let p = ui.painter();
    p.rect(rect, CornerRadius::same((CHIP_H / 2.0) as u8), fill, Stroke::new(1.0, border), StrokeKind::Inside);
    p.circle_filled(egui::pos2(rect.left() + CHIP_PAD + CHIP_DOT / 2.0, rect.center().y), CHIP_DOT / 2.0, fg);
    let text_x = rect.left() + CHIP_PAD + CHIP_DOT + CHIP_DOT_GAP;
    p.galley(egui::pos2(text_x, rect.center().y - galley.size().y / 2.0), galley, fg);
    r.on_hover_text(if supported { "This account serves this model" } else { "This account does not serve this model" })
}

/// "Supports N/M models", plus " · updated HH:MM" (local time) once a list was fetched.
fn count_line(view: &AccountView) -> String {
    let (n, m) = counts(view);
    let mut line = format!("Supports {n}/{m} models");
    if let Some(t) = view.models_fetched_at {
        line.push_str(&format!(" · updated {}", format::clock(t)));
    }
    line
}

/// The one list the chips and both numbers of the count line come from: the provider's catalog
/// joined with the fetched list, lower-cased, deduplicated and sorted.
///
/// An id that could not be sent in a request is left out: it would also be the one string here
/// that came from the network unchecked.
pub fn candidates(view: &AccountView) -> Vec<String> {
    catalog::models(view.provider)
        .into_iter()
        .chain(view.models.iter().map(|m| m.trim().to_ascii_lowercase()))
        .filter(|m| valid_model_id(m))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// `(N, M)` of "Supports N/M models". `N` counts members of the list `M` measures, so it can
/// never exceed it.
pub fn counts(view: &AccountView) -> (usize, usize) {
    let list = candidates(view);
    (list.iter().filter(|m| serves(view, m)).count(), list.len())
}

/// The account's last fetch listed `model`.
fn serves(view: &AccountView, model: &str) -> bool {
    view.models.iter().any(|m| m.trim().eq_ignore_ascii_case(model))
}

/// How a row and the popup title name an account: its email, else the label settings keep for
/// it, else its provider.
pub fn account_name(state: &UiState, view: &AccountView) -> String {
    if let Some(email) = view.email.as_deref().map(str::trim).filter(|e| !e.is_empty()) {
        return email.to_owned();
    }
    state
        .settings
        .accounts
        .iter()
        .find(|e| e.id == view.id)
        .map(|e| e.label.trim())
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| view.provider.label().to_owned())
}

/// [`widgets::popup_menu`]'s frame and anchoring, except that a click inside keeps it open.
///
/// `popup_menu` closes on any click, which suits a list of choices. These popups hold a button
/// whose result is shown in place ("Checking…") and, for a pasted redirect link, a text field:
/// closing on the click that focuses the field would make it unusable. A click outside and Esc
/// still close them.
pub fn popup<R>(anchor: &egui::Response, width: f32, add: impl FnOnce(&mut Ui) -> R) -> Option<R> {
    let frame = widgets::card(theme::RADIUS_POPUP, 6).shadow(anchor.ctx.global_style().visuals.popup_shadow);
    egui::Popup::menu(anchor)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .gap(4.0)
        .frame(frame)
        .width(width)
        .show(|ui| {
            ui.set_min_width(width - 12.0);
            add(ui)
        })
        .map(|r| r.inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::AccountModelsState;
    use crate::auth::external::{AccountProvider, AccountStatus};
    use crate::ui::test_util::{click, context, painted};
    use egui::Rect;

    const ID: &str = "antigravity-0123abcd";

    /// An Antigravity account. Its catalog is the spec's three defaults: claude-sonnet-4-6,
    /// gemini-3-flash and gemini-3.5-flash-lite.
    fn view(models: &[&str], fetched: bool) -> AccountView {
        AccountView {
            id: ID.into(),
            provider: AccountProvider::Antigravity,
            email: Some("me@example.com".into()),
            plan: None,
            model: "gemini-3-flash".into(),
            models: models.iter().map(|m| (*m).to_owned()).collect(),
            models_fetched_at: fetched.then(chrono::Utc::now),
            status: AccountStatus::SignedIn,
        }
    }

    fn draw(ctx: &egui::Context, state: &mut UiState, v: &AccountView, events: Vec<egui::Event>) -> (Vec<(String, Rect)>, Vec<UiAction>) {
        let mut commands = Vec::new();
        let texts = painted(ctx, events, |ui| show(ui, state, &mut commands, v));
        (texts, commands)
    }

    fn shows(texts: &[(String, Rect)], part: &str) -> bool {
        texts.iter().any(|(t, _)| t.contains(part))
    }

    /// Press and release on the rightmost text ending with `label` (the button, when a line says
    /// the same thing); the commands of both frames.
    fn click_text(ctx: &egui::Context, state: &mut UiState, v: &AccountView, label: &str) -> Vec<UiAction> {
        let (texts, _) = draw(ctx, state, v, Vec::new());
        let pos = texts
            .iter()
            .filter(|(t, _)| t.ends_with(label))
            .map(|(_, r)| r.center())
            .max_by(|a, b| a.x.total_cmp(&b.x))
            .unwrap_or_else(|| panic!("no {label:?} in {texts:?}"));
        let (_, mut sent) = draw(ctx, state, v, vec![egui::Event::PointerMoved(pos), click(pos, true)]);
        sent.extend(draw(ctx, state, v, vec![click(pos, false)]).1);
        sent
    }

    #[test]
    fn the_models_popup_counts_against_the_catalog() {
        let ctx = context();
        let mut state = UiState::default();
        // Fetched: one catalog model and two the catalog does not know. Five candidates, three served.
        let v = view(&["gemini-3-flash", "gemini-3-pro-high", "gpt-oss-120b-medium"], true);
        assert_eq!(counts(&v), (3, 5));
        let (texts, commands) = draw(&ctx, &mut state, &v, Vec::new());
        assert!(shows(&texts, "Supported models · me@example.com"), "{texts:?}");
        assert!(shows(&texts, "Supports 3/5 models · updated "), "{texts:?}");
        // One chip per candidate, the served and the unserved alike.
        for m in candidates(&v) {
            assert!(texts.iter().any(|(t, _)| *t == m), "no chip for {m}");
        }
        assert!(commands.is_empty(), "drawing sends nothing");

        // N never exceeds M, whatever the provider sends: `auto`, duplicates, other cases, junk.
        for models in [
            vec!["auto", "gemini-3-flash"],
            vec!["Gemini-3-Flash", "gemini-3-flash", "GEMINI-3-FLASH"],
            vec!["has space", "gemini-3-flash", ""],
            vec!["claude-sonnet-4-6", "gemini-3-flash", "gemini-3.5-flash-lite"],
            vec![],
        ] {
            let (n, m) = counts(&view(&models, true));
            assert!(n <= m, "{models:?}: {n}/{m}");
        }
        assert_eq!(counts(&view(&["Gemini-3-Flash", "gemini-3-flash"], true)), (1, 3), "one id in two spellings is one model");
        assert_eq!(counts(&view(&["auto"], true)), (1, 4), "`auto` is counted on both sides or not at all");
    }

    #[test]
    fn the_models_popup_rechecks() {
        let ctx = context();
        let mut state = UiState::default();
        let v = view(&["gemini-3-flash"], true);
        let sent = click_text(&ctx, &mut state, &v, "Check again");
        assert!(matches!(sent.as_slice(), [AppCommand::CheckAccountModels(id)] if id == ID), "{sent:?}");

        // The backend reports the check running: the line and the button say so, and the button
        // does nothing.
        state.accounts_models.insert(ID.into(), AccountModelsState { checking: true, error: None });
        let (texts, _) = draw(&ctx, &mut state, &v, Vec::new());
        assert_eq!(texts.iter().filter(|(t, _)| t.ends_with("Checking…")).count(), 2, "{texts:?}");
        assert!(!shows(&texts, "Check again") && !shows(&texts, "Supports "));
        assert!(click_text(&ctx, &mut state, &v, "Checking…").is_empty(), "no second check while one runs");

        // It failed: the message shows inside the popup and the cached chips stay.
        state.accounts_models.insert(ID.into(), AccountModelsState { checking: false, error: Some("this Antigravity account has no Cloud project yet; sign in again".into()) });
        let (texts, _) = draw(&ctx, &mut state, &v, Vec::new());
        assert!(shows(&texts, "has no Cloud project yet"));
        assert!(shows(&texts, "Supports 1/3 models") && texts.iter().any(|(t, _)| t == "gemini-3-flash"));
        assert!(shows(&texts, "Check again"));
    }

    #[test]
    fn an_account_never_checked_says_so_instead_of_counting() {
        let ctx = context();
        let mut state = UiState::default();
        let v = view(&[], false);
        let (texts, _) = draw(&ctx, &mut state, &v, Vec::new());
        assert!(shows(&texts, "No model list yet. Press Check again."), "{texts:?}");
        assert!(!shows(&texts, "Supports "), "no 0/3 for a list that was never fetched");
        assert!(!texts.iter().any(|(t, _)| t == "gemini-3-flash"), "no chips either");
        // An account without an email is named by its settings label.
        let v = AccountView { email: None, ..v };
        state.settings.accounts = vec![crate::settings::AccountEntry { id: ID.into(), provider: AccountProvider::Antigravity, label: "Work".into(), model: "gemini-3-flash".into() }];
        let (texts, _) = draw(&ctx, &mut state, &v, Vec::new());
        assert!(shows(&texts, "Supported models · Work"), "{texts:?}");
    }

    #[test]
    fn the_button_opens_the_popup_a_demo_asks_for_once() {
        let ctx = context();
        let mut state = UiState { open_account_models: Some("codex-0123abcd".into()), ..UiState::default() };
        let v = view(&["gemini-3-flash"], true);
        let mut commands = Vec::new();
        let mut frame = |state: &mut UiState| painted(&ctx, Vec::new(), |ui| {
            models_button(ui, state, &mut commands, &v);
        });
        let texts = frame(&mut state);
        assert!(!shows(&texts, "Supported models"), "another account's popup is not this one");
        assert_eq!(state.open_account_models.as_deref(), Some("codex-0123abcd"), "and its request is left for it");

        state.open_account_models = Some(ID.into());
        frame(&mut state);
        assert!(state.open_account_models.is_none(), "consumed");
        // The popup is laid out on the frame it opens and drawn from the next one.
        let texts = frame(&mut state);
        assert!(shows(&texts, "Supported models · me@example.com"), "{texts:?}");
        assert!(texts.iter().any(|(t, _)| t == "gemini-3-flash"), "a chip is drawn");
        assert!(commands.is_empty());
    }

    #[test]
    fn an_open_popup_stays_with_its_account_when_the_rows_move() {
        let ctx = context();
        let other = AccountView { id: "antigravity-89abcdef".into(), email: Some("other@example.com".into()), ..view(&["gemini-3-flash"], true) };
        let mine = view(&["gemini-3-flash"], true);
        let mut state = UiState { open_account_models: Some(ID.into()), ..UiState::default() };
        let mut commands = Vec::new();
        let mut frame = |state: &mut UiState, rows: &[&AccountView]| {
            painted(&ctx, Vec::new(), |ui| {
                for v in rows {
                    models_button(ui, state, &mut commands, v);
                }
            })
        };
        // Opened under another row…
        frame(&mut state, &[&other, &mine]);
        assert!(shows(&frame(&mut state, &[&other, &mine]), "Supported models · me@example.com"));
        // …the row above goes (signed out), and this one moves up: the popup is still this
        // account's, not the one that would now sit in its place.
        let texts = frame(&mut state, &[&mine]);
        assert!(shows(&texts, "Supported models · me@example.com"), "{texts:?}");
        let texts = frame(&mut state, &[&mine, &other]);
        assert!(!shows(&texts, "Supported models · other@example.com"), "{texts:?}");
    }
}
