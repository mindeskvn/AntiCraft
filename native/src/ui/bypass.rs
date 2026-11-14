//! Bypass Mode's warning band, drawn wherever a message can be typed.
//!
//! The mode routes the first requests of each message to a provider the user configured, so it
//! is never silent: while it is on, every composer carries this band, and the band says which
//! provider answers, how many requests go there, and which model takes over after. It is not
//! dismissible — a mode that changes where a prompt is sent should not be hideable by a click
//! that a later user of the screen cannot undo — but it carries the switch that turns it off.

use egui::{Stroke, Ui};

use super::{icons, theme, widgets, UiState};

/// The band, whenever the mode is switched on. Returns whether it drew.
///
/// It draws for "on but pointing nowhere" too: the mode reading as on while every message quietly
/// took the normal route is the one state where this band matters most.
pub fn band(ui: &mut Ui, state: &mut UiState) -> bool {
    if !state.settings.bypass.enabled {
        return false;
    }
    let text = match summary(state) {
        Some((label, model, normal)) => {
            let n = state.settings.bypass.requests;
            let requests = if n == 1 { "The first request".to_owned() } else { format!("The first {n} requests") };
            format!(
                "Bypass Mode is on. {requests} of each message go to {label} ({model}), then the conversation returns to {normal}. \
                 That provider sees those requests and the conversation history sent with them, under its own terms."
            )
        }
        // Switched on with nothing to route to: every message takes the normal route. Said
        // plainly, so nobody believes their prompts are going anywhere else.
        None if state.settings.bypass.provider.is_none() => {
            "Bypass Mode is on but no provider is chosen, so every message still goes to the normal model. \
             Choose a provider in Settings > Tools and approvals > Bypass Mode."
                .to_owned()
        }
        None => "Bypass Mode is on but its provider no longer exists, so every message still goes to the normal model. \
                 Choose a provider in Settings > Tools and approvals > Bypass Mode."
            .to_owned(),
    };
    let mut turn_off = false;
    egui::Frame::new()
        .fill(theme::ERROR.gamma_multiply(0.12))
        .stroke(Stroke::new(1.0, theme::ERROR.gamma_multiply(0.4)))
        .corner_radius(8)
        .inner_margin(egui::Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(icons::WARNING).font(theme::icon(15.0)).color(theme::ERROR));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if widgets::text_link(ui, "Turn off", 12.5).clicked() {
                        turn_off = true;
                    }
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        ui.add(egui::Label::new(egui::RichText::new(text).font(theme::font(12.5)).color(theme::FG)).wrap());
                    });
                });
            });
        });
    if turn_off {
        state.settings.bypass.enabled = false;
        state.settings_dirty = true;
    }
    true
}

/// `(provider label, its model, the model turns return to)`, when the mode routes.
fn summary(state: &UiState) -> Option<(String, String, String)> {
    let cfg = &state.settings.bypass;
    if !cfg.routes() {
        return None;
    }
    let id = cfg.provider.as_deref()?;
    let p = state.settings.providers.iter().find(|p| p.id == id)?;
    let model = match p.model.trim() {
        "" => "no model chosen".to_owned(),
        m => m.to_owned(),
    };
    let normal = match state.settings.active_account_entry() {
        Some(a) => a.model.trim().to_owned(),
        None => match state.settings.active_provider_profile() {
            Some(p) => match p.model.trim() {
                "" => "its provider's model".to_owned(),
                m => m.to_owned(),
            },
            None => super::format::model_label(&state.settings.model),
        },
    };
    Some((p.label.trim().to_owned(), model, normal))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{ProviderKind, ProviderProfile};
    use crate::settings::Settings;
    use crate::ui::test_util::{context, painted};

    fn profile(id: &str, label: &str, model: &str) -> ProviderProfile {
        ProviderProfile {
            id: id.into(),
            label: label.into(),
            kind: ProviderKind::OpenAi,
            model: model.into(),
            base_url: None,
            api_key: "k".into(),
            max_output_tokens: None,
            context_window: None,
            models: Vec::new(),
        }
    }

    fn state(enabled: bool, provider: Option<&str>, model: &str) -> UiState {
        let mut s = UiState { settings: Settings { providers: vec![profile("gw", "Gateway", model)], ..Settings::default() }, ..UiState::default() };
        s.settings.bypass.enabled = enabled;
        s.settings.bypass.provider = provider.map(Into::into);
        s.settings.bypass.requests = 3;
        s
    }

    fn drawn(state: &mut UiState, events: Vec<egui::Event>) -> Vec<(String, egui::Rect)> {
        let ctx = context();
        painted(&ctx, events, |ui| {
            band(ui, state);
        })
    }

    fn has(texts: &[(String, egui::Rect)], part: &str) -> bool {
        texts.iter().any(|(t, _)| t.contains(part))
    }

    /// The band names the provider, its model, the count and the model that follows; it names
    /// the count in the singular for one request.
    #[test]
    fn the_band_says_where_the_requests_go_and_what_follows() {
        let mut s = state(true, Some("gw"), "gpt-4o");
        let texts = drawn(&mut s, Vec::new());
        assert!(has(&texts, "Bypass Mode is on."), "{texts:?}");
        assert!(has(&texts, "The first 3 requests of each message go to Gateway (gpt-4o)"), "{texts:?}");
        assert!(has(&texts, "the conversation returns to Gemini 2.5 Pro"), "{texts:?}");
        s.settings.bypass.requests = 1;
        assert!(has(&drawn(&mut s, Vec::new()), "The first request of each message"), "singular");
        s.settings.bypass.provider = None;
        // A profile without a model says so rather than showing an empty pair of brackets.
        let mut s = state(true, Some("gw"), "");
        assert!(has(&drawn(&mut s, Vec::new()), "(no model chosen)"));
    }

    /// Off, the band is absent. On with nothing to route to, it is present and says so: a mode
    /// that reads as on while every message takes the normal route is the state it must not hide.
    #[test]
    fn the_band_speaks_for_a_mode_that_routes_nothing() {
        let mut off = state(false, Some("gw"), "m");
        assert!(drawn(&mut off, Vec::new()).is_empty());
        assert!(!drawn(&mut off, Vec::new()).iter().any(|(t, _)| t.contains("Bypass Mode")));

        let mut nowhere = state(true, None, "m");
        let texts = drawn(&mut nowhere, Vec::new());
        assert!(has(&texts, "no provider is chosen, so every message still goes to the normal model"), "{texts:?}");
        assert!(texts.iter().any(|(t, _)| t == "Turn off"), "and it can be turned off from there");

        let mut gone = state(true, Some("nope"), "m");
        let texts = drawn(&mut gone, Vec::new());
        assert!(has(&texts, "its provider no longer exists"), "{texts:?}");
    }

    /// The band says the history travels with those requests, not only the prompt: a long
    /// conversation is sent to that provider as part of the first request.
    #[test]
    fn the_band_says_the_history_goes_too() {
        let mut s = state(true, Some("gw"), "m");
        let texts = drawn(&mut s, Vec::new());
        assert!(has(&texts, "and the conversation history sent with them"), "{texts:?}");
    }

    /// Turning it off from the band switches the mode off and marks the settings for saving.
    #[test]
    fn the_band_turns_the_mode_off() {
        // One context for every frame: a click's press and release are only one click when the
        // widget keeps its state between them.
        let ctx = context();
        let mut s = state(true, Some("gw"), "m");
        let frame = |s: &mut UiState, events| {
            painted(&ctx, events, |ui| {
                band(ui, s);
            })
        };
        let texts = frame(&mut s, Vec::new());
        let pos = texts.iter().find(|(t, _)| t == "Turn off").map(|(_, r)| r.center()).expect("the link");
        frame(&mut s, vec![egui::Event::PointerMoved(pos), crate::ui::test_util::click(pos, true)]);
        frame(&mut s, vec![crate::ui::test_util::click(pos, false)]);
        assert!(!s.settings.bypass.enabled);
        assert!(s.settings_dirty, "the change reaches settings.json");
    }
}
