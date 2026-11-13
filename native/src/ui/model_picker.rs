//! Model picker (DESIGN.md 2.6, `_ref/ui/04-model-menu.png`): text chip in the composer that
//! opens a compact model list just below it (flips upward when there is no room).
//!
//! The list is grouped by backend. Every signed-in subscription account gets a group of its
//! models (headed by the account, "Current" on the one in use), and so does every provider profile
//! with models loaded (Customizations > Providers > Edit > Load models). The Code Assist models
//! follow while Code Assist sends turns, or can (a Google sign-in). Picking a model of another
//! backend also puts that backend in use: that is how the composer switches between them.

use egui::{Align2, Sense, Ui, Vec2};

use super::{format, icons, theme, UiAction, UiState};
use crate::app::AppCommand;
use crate::auth::external::{catalog, valid_model_id};
use crate::auth::AuthStatus;
use crate::settings::resolve_model;

const MENU_WIDTH: f32 = 242.0;
/// The menu once it lists accounts: a group header holds an account name and "Current".
const ACCOUNT_MENU_WIDTH: f32 = 300.0;
/// Tallest the list grows before it scrolls. Three accounts' catalogs are taller than a window.
const MENU_MAX_H: f32 = 440.0;
const ROW_H: f32 = 23.0;
const HEADER_H: f32 = 20.0;
const HEADER_COLOR: egui::Color32 = egui::Color32::from_rgb(0x6e, 0x6e, 0x6e);
const ROW_HOVER_FILL: egui::Color32 = egui::Color32::from_rgb(0x2d, 0x2d, 0x2d);

/// Widest provider model id the chip shows in full; a longer one is cut with an ellipsis (the
/// tooltip has all of it), so the send button stays on the composer row.
const PROVIDER_CHIP_MAX_W: f32 = 240.0;

/// One block of the picker.
pub struct MenuGroup {
    pub header: Option<String>,
    pub rows: Vec<MenuChoice>,
    /// Whose models these are.
    pub source: Source,
    /// Drawn as "Current" in the header row.
    pub current: bool,
}

/// The backend a [`MenuGroup`] lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A signed-in account, by id.
    Account(String),
    /// A provider profile, by id.
    Profile(String),
    /// Google Code Assist, drawn by [`model_menu`].
    CodeAssist,
}

/// One row of an account group: the model id, its label and the hover note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuChoice {
    pub id: String,
    pub label: String,
    pub note: String,
}

/// What a click in the picker chose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pick {
    CodeAssist(String),
    Account { account: String, model: String },
    Profile { profile: String, model: String },
    /// "Load models…" in the group of a provider that has none: open its Edit dialog.
    EditProfile(String),
}

/// Model chip: names the model new turns use, with a menu to change it.
///
/// With an account in use the chip is `{provider} · {model}` ("Antigravity · Gemini 3 Flash"):
/// which sign-in sends the turn is the first thing it says, and the account itself is in the
/// tooltip and the sidebar. A provider profile in use is named the same way, by its label; one
/// whose model was never chosen says "Choose model". Every backend's models are in the menu.
pub fn show(ui: &mut Ui, state: &mut UiState, commands: &mut Vec<UiAction>) {
    if let Some(entry) = state.settings.active_account_entry() {
        let provider = entry.provider.label();
        let name = if entry.label.trim().is_empty() { provider } else { entry.label.trim() };
        let model = entry.model.trim();
        let label = format!("{provider} · {}", format::model_label(model));
        let tip = if stranded(state) {
            format!("The {provider} account {name} needs signing in again. Pick a Code Assist model to use Code Assist meanwhile.")
        } else {
            format!("{model} on the {provider} account {name}. Choose another model or account.")
        };
        chip_with_menu(ui, state, commands, &label, &tip);
        return;
    }
    if let Some((label, tip)) = state.settings.active_provider_profile().map(profile_chip) {
        chip_with_menu(ui, state, commands, &label, &tip);
        return;
    }
    let label = format::model_label(&state.settings.model);
    chip_with_menu(ui, state, commands, &label, "Choose model");
}

/// The chip's text and tooltip while `profile` sends turns.
fn profile_chip(profile: &crate::providers::ProviderProfile) -> (String, String) {
    let name = profile.label.trim();
    match profile.model.trim() {
        "" => (format!("{name} · Choose model"), format!("{name} has no model chosen. Pick one of its models.")),
        model => (format!("{name} · {}", format::model_label(model)), format!("{model} on the provider {name}. Choose another model or backend.")),
    }
}

/// The chip with a caret and the grouped menu under it.
fn chip_with_menu(ui: &mut Ui, state: &mut UiState, commands: &mut Vec<UiAction>, label: &str, tip: &str) {
    let enabled = !state.busy;
    let font = theme::font(12.5);
    let full_w = ui.painter().layout_no_wrap(label.to_owned(), font.clone(), theme::FG_SECONDARY).size().x;
    // An account name can be long; the chip keeps the send button on the composer row.
    let gw = full_w.min(PROVIDER_CHIP_MAX_W);
    let (rect, r) = ui.allocate_exact_size(Vec2::new(gw + 30.0, 28.0), if enabled { Sense::click() } else { Sense::hover() });
    let popup_id = egui::Popup::default_response_id(&r);
    let asked = std::mem::take(&mut state.open_model_menu) && enabled;
    if asked {
        egui::Popup::open_id(ui.ctx(), popup_id);
    }
    let open = egui::Popup::is_id_open(ui.ctx(), popup_id);
    let opening = scroll_on_open(ui, popup_id, asked || (r.clicked() && !open));
    let hot = enabled && (r.hovered() || open);
    let p = ui.painter();
    if hot {
        p.rect_filled(rect, 6, theme::SECONDARY.gamma_multiply(if open { 0.8 } else { 0.6 }));
    }
    let mut c = if hot { theme::FG } else { theme::FG_SECONDARY };
    if !enabled {
        c = c.gamma_multiply(0.5);
    }
    let text = egui::Rect::from_min_size(egui::pos2(rect.left() + 8.0, rect.top()), Vec2::new(gw, rect.height()));
    if full_w > gw {
        super::widgets::paint_truncated(ui, text, label, font, c);
    } else {
        ui.painter().text(text.left_center(), Align2::LEFT_CENTER, label, font, c);
    }
    ui.painter().text(egui::pos2(rect.left() + 12.0 + gw, rect.center().y + 1.0), Align2::LEFT_CENTER, icons::CARET_DOWN, theme::icon(10.0), c);
    let r = if !enabled {
        r.on_hover_text("The model cannot change during a turn")
    } else if open {
        r
    } else {
        r.on_hover_text(tip)
    };
    // Wider once a group header names an account or a provider.
    let width = if menu_groups(state).iter().any(|g| g.source != Source::CodeAssist) { ACCOUNT_MENU_WIDTH } else { MENU_WIDTH };
    // The reference popup's left edge sits 32px inside the composer (11px left of our chip).
    egui::Popup::menu(&r)
        .anchor(r.rect.translate(Vec2::new(-11.0, 0.0)))
        .align(egui::RectAlign::BOTTOM_START)
        .align_alternatives(&[egui::RectAlign::TOP_START])
        .gap(4.0)
        .width(width)
        .frame(popup_frame())
        .show(|ui| {
            ui.set_width(width - 2.0);
            let picked = egui::ScrollArea::vertical().id_salt("model_menu_list").max_height(MENU_MAX_H).show(ui, |ui| menu_groups_click(ui, state, opening)).inner;
            if let Some(pick) = picked {
                match pick {
                    Pick::CodeAssist(m) => {
                        // Offered beside an account or a profile only when Code Assist can send,
                        // or the account cannot (see `menu_groups`): the pick leaves them, or
                        // turns would still go to them.
                        if state.settings.select_code_assist() {
                            commands.push(AppCommand::SetActiveProvider(None));
                        }
                        state.settings.model = m.clone();
                        commands.push(AppCommand::SetModel(m));
                    }
                    Pick::Account { account, model } => pick_account_model(state, commands, account, model),
                    Pick::Profile { profile, model } => pick_profile_model(state, commands, profile, model),
                    Pick::EditProfile(id) => super::customizations::edit_provider(state, commands, &id),
                }
                ui.close();
            }
        });
}

/// Whether the menu `popup_id` should scroll to the model in use this frame: from the frame it is
/// opened (`opened`) until it has been drawn once.
///
/// One frame is not enough: egui lays a popup out first in an invisible sizing pass, where its
/// scroll area has no height yet and a scroll request is dropped. A request kept past that
/// lands; one kept for good would hold the list on the checked row whatever the wheel does.
pub fn scroll_on_open(ui: &Ui, popup_id: egui::Id, opened: bool) -> bool {
    let key = popup_id.with("scroll_on_open");
    // Drawn for real: the popup's area answered a frame ago, so its size is known.
    let drawn = ui.ctx().read_response(popup_id).is_some();
    let pending = opened || ui.data(|d| d.get_temp::<bool>(key)).unwrap_or(false);
    ui.data_mut(|d| d.insert_temp(key, pending && !drawn));
    pending
}

/// Put `account` in use with `model`. Shown at once; the backend's settings echo confirms it.
pub fn pick_account_model(state: &mut UiState, commands: &mut Vec<UiAction>, account: String, model: String) {
    if let Some(e) = state.settings.accounts.iter_mut().find(|e| e.id == account) {
        e.model = model.clone();
    }
    state.settings.select_account(&account);
    commands.push(AppCommand::UseAccountModel { account, model });
}

/// Put `profile` in use with `model`. Shown at once; the backend's settings echo confirms it.
pub fn pick_profile_model(state: &mut UiState, commands: &mut Vec<UiAction>, profile: String, model: String) {
    if let Some(p) = state.settings.providers.iter_mut().find(|p| p.id == profile) {
        p.model = model.clone();
    }
    state.settings.select_profile(Some(profile.clone()));
    commands.push(AppCommand::UseProviderModel { profile, model });
}

/// Popup frame measured on the reference: `#1c1c1c` fill, 1px `#282828`, radius 8, soft shadow.
fn popup_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(theme::CARD)
        .stroke(egui::Stroke::new(1.0, theme::POPUP_BORDER))
        .corner_radius(8)
        .inner_margin(egui::Margin::symmetric(1, 0))
        .shadow(egui::epaint::Shadow { offset: [0, 2], blur: 8, spread: 0, color: egui::Color32::from_black_alpha(90) })
}

/// Whether the account in use has no live session (it expired, or its file could not be read):
/// its models cannot be sent, so it has no group in the menu.
fn stranded(state: &UiState) -> bool {
    state.settings.active_account_entry().is_some_and(|e| !state.signed_in_accounts().any(|v| v.id == e.id))
}

/// Account groups (one per signed-in account, headed by the account), provider groups, then the
/// Code Assist group when Code Assist is in use, when it can send (a Google sign-in), or when the
/// account in use cannot send. Without it, an only account that expired would open an empty menu,
/// with no way back to Code Assist from the composer.
pub fn menu_groups(state: &UiState) -> Vec<MenuGroup> {
    let mut groups = account_groups(state);
    groups.extend(profile_groups(state));
    let s = &state.settings;
    let current = s.active_account_entry().is_none() && s.active_provider_profile().is_none();
    if current || stranded(state) || matches!(state.auth, AuthStatus::LoggedIn { .. }) {
        groups.push(code_assist_group(current));
    }
    groups
}

/// One group per signed-in account, in the Account tab's order.
///
/// Its rows are the account's fetched list, or the provider's catalog until a fetch landed, plus
/// the model chosen for it when neither lists that one (so the check mark always has a row).
fn account_groups(state: &UiState) -> Vec<MenuGroup> {
    let active = state.settings.active_account_entry().map(|e| e.id.clone());
    state
        .signed_in_accounts()
        .map(|v| {
            let entry = state.settings.accounts.iter().find(|e| e.id == v.id);
            let chosen = entry.map(|e| e.model.trim()).filter(|m| !m.is_empty()).unwrap_or(v.model.trim());
            let mut ids: Vec<String> = if v.models.is_empty() { catalog::models(v.provider) } else { v.models.clone() };
            ids.retain(|m| valid_model_id(m));
            if valid_model_id(chosen) && !ids.iter().any(|m| m.eq_ignore_ascii_case(chosen)) {
                ids.insert(0, chosen.to_owned());
            }
            let name = super::account_models::account_name(state, v);
            let header = if name == v.provider.label() { name } else { format!("{} · {name}", v.provider.label()) };
            MenuGroup { header: Some(header), rows: choices(ids), source: Source::Account(v.id.clone()), current: active.as_deref() == Some(v.id.as_str()) }
        })
        .collect()
}

/// One group per provider profile with models to pick: its loaded list, plus its chosen model
/// when the list lacks it. The profile in use has a group even with none, which offers
/// "Load models…" instead.
fn profile_groups(state: &UiState) -> Vec<MenuGroup> {
    let active = state.settings.active_provider_profile().map(|p| p.id.as_str());
    state
        .settings
        .providers
        .iter()
        .filter_map(|p| {
            let mut ids = p.models.clone();
            let chosen = p.model.trim();
            if !chosen.is_empty() && !ids.iter().any(|m| m == chosen) {
                ids.insert(0, chosen.to_owned());
            }
            let current = active == Some(p.id.as_str());
            (current || !ids.is_empty()).then(|| MenuGroup { header: Some(p.label.trim().to_owned()), rows: choices(ids), source: Source::Profile(p.id.clone()), current })
        })
        .collect()
}

/// Rows for model ids: the readable name, the id itself on hover.
fn choices(ids: Vec<String>) -> Vec<MenuChoice> {
    ids.into_iter().map(|id| MenuChoice { label: format::model_label(&id), note: id.clone(), id }).collect()
}

/// The Code Assist models (aliases, then concrete ids). Drawn by [`model_menu`].
fn code_assist_group(current: bool) -> MenuGroup {
    let rows = format::model_choices().into_iter().map(|c| MenuChoice { id: c.id, label: c.label, note: c.note.to_owned() }).collect();
    MenuGroup { header: Some("Model".into()), rows, source: Source::CodeAssist, current }
}

/// Renders the groups of [`menu_groups`]. `model_menu`-compatible: returns the first click.
/// With `opening`, the menu scrolls to the model in use.
pub fn menu_groups_click(ui: &mut Ui, state: &UiState, opening: bool) -> Option<Pick> {
    draw_groups(ui, state, &menu_groups(state), opening, false)
}

/// The Customizations "Default model" row: every account and provider group, then always the
/// Code Assist list. The row sets the Code Assist model even while another backend sends turns,
/// so that model keeps its check there, and a model of another backend can be picked too.
pub fn default_model_click(ui: &mut Ui, state: &UiState, opening: bool) -> Option<Pick> {
    let mut groups = account_groups(state);
    groups.extend(profile_groups(state));
    let s = &state.settings;
    groups.push(code_assist_group(s.active_account_entry().is_none() && s.active_provider_profile().is_none()));
    draw_groups(ui, state, &groups, opening, true)
}

/// Draw `groups`; `scroll` brings the checked row into view (the frame a menu opens on).
///
/// The Code Assist model is checked while Code Assist sends turns, or always with
/// `check_code_assist` (the "Default model" row, which names it). The composer's menu lists Code
/// Assist beside the backend in use as the way back to it: a check there would read as two
/// models in use.
fn draw_groups(ui: &mut Ui, state: &UiState, groups: &[MenuGroup], scroll: bool, check_code_assist: bool) -> Option<Pick> {
    let mut picked = None;
    ui.spacing_mut().item_spacing.y = 0.0;
    for (i, g) in groups.iter().enumerate() {
        if i > 0 {
            separator(ui);
        }
        match &g.source {
            Source::CodeAssist => {
                if let Some(m) = model_menu(ui, state, g.current || check_code_assist, scroll && g.current) {
                    picked = Some(Pick::CodeAssist(m));
                }
                continue;
            }
            Source::Account(account) => {
                group_header(ui, g.header.as_deref().unwrap_or_default(), g.current);
                let chosen = state.settings.accounts.iter().find(|e| e.id == *account).map(|e| e.model.trim()).unwrap_or_default();
                for c in &g.rows {
                    if choice_row(ui, c, c.id.eq_ignore_ascii_case(chosen) && g.current, scroll) {
                        picked = Some(Pick::Account { account: account.clone(), model: c.id.clone() });
                    }
                }
            }
            Source::Profile(profile) => {
                group_header(ui, g.header.as_deref().unwrap_or_default(), g.current);
                // Gateway ids can differ in case alone, so the chosen one is matched exactly.
                let chosen = state.settings.providers.iter().find(|p| p.id == *profile).map(|p| p.model.trim()).unwrap_or_default();
                if g.rows.is_empty() && action_row(ui, "Load models…", "Open this provider and load the models it offers") {
                    picked = Some(Pick::EditProfile(profile.clone()));
                }
                for c in &g.rows {
                    if choice_row(ui, c, c.id == chosen && g.current, scroll) {
                        picked = Some(Pick::Profile { profile: profile.clone(), model: c.id.clone() });
                    }
                }
            }
        }
        ui.add_space(4.0);
    }
    picked
}

fn separator(ui: &mut Ui) {
    let (sr, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 9.0), Sense::hover());
    ui.painter().hline(sr.x_range(), sr.center().y, egui::Stroke::new(1.0, theme::POPUP_BORDER));
}

/// An account group's header: the account (cut to fit) and, for the one in use, "Current" in
/// green at the right edge. Same words and colour as the Account tab's pill; that pill is drawn
/// by its own page, so this is plain text.
fn group_header(ui: &mut Ui, text: &str, current: bool) {
    let (hr, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), HEADER_H), Sense::hover());
    let mut right = hr.right() - 10.0;
    if current {
        let r = ui.painter().text(egui::pos2(right, hr.center().y), Align2::RIGHT_CENTER, "Current", theme::font(11.0), theme::SUCCESS);
        right = r.left() - 8.0;
    }
    let text_rect = egui::Rect::from_min_max(egui::pos2(hr.left() + 10.0, hr.top()), egui::pos2(right.max(hr.left() + 10.0), hr.bottom()));
    super::widgets::paint_truncated(ui, text_rect, text, theme::font(11.0), HEADER_COLOR);
}

/// Center the menu's list on the checked row `r`. At once rather than animated: the list is just
/// opening, and it should open there, not scroll there in front of the user.
fn scroll_to(r: &egui::Response) {
    r.scroll_to_me_animation(Some(egui::Align::Center), egui::style::ScrollAnimation::none());
}

/// A muted row that does something instead of picking a model ("Load models…"). Returns the click.
fn action_row(ui: &mut Ui, label: &str, tip: &str) -> bool {
    let (rect, r) = ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_H), Sense::click());
    if r.hovered() {
        ui.painter().rect_filled(rect.shrink2(Vec2::new(5.0, 1.0)), 4, ROW_HOVER_FILL);
    }
    let color = if r.hovered() { theme::FG } else { theme::FG_MUTED };
    ui.painter().text(egui::pos2(rect.left() + 10.0, rect.center().y), Align2::LEFT_CENTER, label, theme::font(13.0), color);
    r.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(tip).clicked()
}

/// One account or provider model: label, check at the right when it is the model in use, which
/// `scroll` brings into view. Returns the click.
fn choice_row(ui: &mut Ui, c: &MenuChoice, selected: bool, scroll: bool) -> bool {
    let (rect, r) = ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_H), Sense::click());
    if selected && scroll {
        scroll_to(&r);
    }
    let p = ui.painter();
    if selected || r.hovered() {
        p.rect_filled(rect.shrink2(Vec2::new(5.0, 1.0)), 4, ROW_HOVER_FILL);
    }
    let label_color = if selected || r.hovered() { theme::FG } else { theme::FG_SECONDARY };
    let text = egui::Rect::from_min_max(egui::pos2(rect.left() + 10.0, rect.top()), egui::pos2(rect.right() - 30.0, rect.bottom()));
    super::widgets::paint_truncated(ui, text, &c.label, theme::font(13.0), label_color);
    if selected {
        ui.painter().text(egui::pos2(rect.right() - 12.0, rect.center().y), Align2::RIGHT_CENTER, icons::CHECK, theme::icon(13.0), theme::FG);
    }
    r.on_hover_text(&c.note).clicked()
}

/// Model list (header, aliases, separator, concrete models); returns the clicked id. With `check`
/// the Code Assist model is checked, and `scroll` brings it into view.
pub fn model_menu(ui: &mut Ui, state: &UiState, check: bool, scroll: bool) -> Option<String> {
    let mut picked = None;
    ui.spacing_mut().item_spacing.y = 0.0;
    let (hr, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), HEADER_H), Sense::hover());
    ui.painter().text(egui::pos2(hr.left() + 10.0, hr.center().y), Align2::LEFT_CENTER, "Model", theme::font(11.0), HEADER_COLOR);
    let choices = format::model_choices();
    let (aliases, concrete): (Vec<_>, Vec<_>) = choices.iter().partition(|c| c.alias);
    for c in aliases {
        if let Some(m) = row(ui, state, c, check, scroll) {
            picked = Some(m);
        }
    }
    if !concrete.is_empty() {
        separator(ui);
        for c in concrete {
            if let Some(m) = row(ui, state, c, check, scroll) {
                picked = Some(m);
            }
        }
    }
    ui.add_space(4.0);
    picked
}

/// One 23px row: label (10px inset), muted "(Preview)" suffix, check at the right when selected.
/// The selected row keeps a `#2d2d2d` pill; other rows fill on hover.
fn row(ui: &mut Ui, state: &UiState, c: &format::ModelChoice, check: bool, scroll: bool) -> Option<String> {
    let s = &state.settings;
    let enabled = !c.preview || s.preview_access;
    let selected = check && s.model == c.id;
    let (rect, r) = ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_H), if enabled { Sense::click() } else { Sense::hover() });
    if selected && scroll {
        scroll_to(&r);
    }
    let p = ui.painter();
    if selected || (enabled && r.hovered()) {
        p.rect_filled(rect.shrink2(Vec2::new(5.0, 1.0)), 4, ROW_HOVER_FILL);
    }
    let alpha = if enabled { 1.0 } else { 0.4 };
    let label_color = if selected || r.hovered() { theme::FG } else { theme::FG_SECONDARY };
    let l = p.text(egui::pos2(rect.left() + 10.0, rect.center().y), Align2::LEFT_CENTER, &c.label, theme::font(13.0), label_color.gamma_multiply(alpha));
    if c.preview {
        p.text(egui::pos2(l.right() + 4.0, rect.center().y), Align2::LEFT_CENTER, "(Preview)", theme::font(11.5), theme::FG_MUTED.gamma_multiply(alpha));
    }
    if selected {
        p.text(egui::pos2(rect.right() - 12.0, rect.center().y), Align2::RIGHT_CENTER, icons::CHECK, theme::icon(13.0), theme::FG);
    }
    if !enabled {
        r.on_hover_text("Enable preview access in Customizations");
        return None;
    }
    let id = if c.alias { resolve_model(&c.id, s.preview_access, s.latest_flash_ga) } else { c.id.clone() };
    let tip = if c.alias && !c.note.is_empty() { format!("{} ({id})", c.note) } else { id };
    r.on_hover_text(tip).clicked().then(|| c.id.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::external::{AccountProvider, AccountStatus, AccountView};
    use crate::providers::{ProviderKind, ProviderProfile};
    use crate::settings::AccountEntry;
    use crate::ui::test_util::{click, context, painted};

    fn with_provider(model: &str) -> UiState {
        let mut state = UiState::default();
        state.settings.model = "gemini-2.5-pro".into();
        state.settings.providers = vec![ProviderProfile {
            id: "p1".into(),
            label: "Work".into(),
            kind: ProviderKind::OpenAi,
            model: model.into(),
            base_url: None,
            api_key: "k-test".into(),
            max_output_tokens: None,
            context_window: None,
            models: Vec::new(),
        }];
        state.settings.active_provider = Some("p1".into());
        state
    }

    /// Draw the chip, click the text `target` and draw two more frames (a popup opens on the
    /// frame after the click). Returns the texts of the last frame and the commands sent.
    fn click_chip(state: &mut UiState, target: &str) -> (Vec<(String, egui::Rect)>, Vec<UiAction>) {
        let ctx = context();
        let mut commands = Vec::new();
        let frame = |state: &mut UiState, commands: &mut Vec<UiAction>, events: Vec<egui::Event>| {
            painted(&ctx, events, |ui| {
                ui.horizontal(|ui| show(ui, state, commands));
            })
        };
        let texts = frame(state, &mut commands, Vec::new());
        let at = texts.iter().find(|(t, _)| t == target).map(|(_, r)| r.center()).unwrap_or_else(|| panic!("no {target:?} on the chip"));
        frame(state, &mut commands, vec![egui::Event::PointerMoved(at), click(at, true)]);
        frame(state, &mut commands, vec![click(at, false)]);
        frame(state, &mut commands, Vec::new());
        let texts = frame(state, &mut commands, Vec::new());
        (texts, commands)
    }

    #[test]
    fn the_chip_names_the_provider_and_lists_its_models() {
        let mut state = with_provider("gpt-4o-mini");
        state.settings.providers[0].models = vec!["gpt-4o".into(), "gpt-4o-mini".into()];
        let (texts, commands) = click_chip(&mut state, "Work · GPT 4o Mini");
        assert!(commands.is_empty(), "opening the menu sends nothing");
        assert!(texts.iter().any(|(t, _)| t == "Work") && texts.iter().any(|(t, _)| t == "GPT 4o"), "the provider's group: {texts:?}");
        assert!(texts.iter().any(|(t, _)| t == "Current"));
        assert!(!texts.iter().any(|(t, _)| t == "Flash"), "Code Assist cannot send without a Google sign-in");
        assert_eq!(state.settings.model, "gemini-2.5-pro");

        // A model id too long for the composer is cut on the chip (the text stays the full id).
        let long = format!("accounts/fireworks/models/{}", "x".repeat(80));
        let mut state = with_provider(&long);
        let (texts, _) = click_chip(&mut state, &format!("Work · {long}"));
        let chip = texts.iter().find(|(t, _)| *t == format!("Work · {long}")).map(|(_, r)| *r).unwrap();
        assert!(chip.width() <= PROVIDER_CHIP_MAX_W + 1.0, "chip text is {} wide", chip.width());
    }

    #[test]
    fn picking_a_provider_model_puts_the_provider_in_use() {
        let mut state = with_account();
        state.settings.providers = with_provider("").settings.providers;
        state.settings.providers[0].models = vec!["gpt-4o".into(), "o3".into()];
        let groups = menu_groups(&state);
        assert_eq!(groups.iter().map(|g| g.source.clone()).collect::<Vec<_>>(), [Source::Account("codex-0123abcd".into()), Source::Profile("p1".into()), Source::CodeAssist]);
        assert_eq!(groups[1].rows.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), ["gpt-4o", "o3"], "no model chosen: just the list");

        let (_, commands) = click_chip_menu(&mut state, "Gemini 2.5 Pro", "o3");
        assert!(matches!(commands.as_slice(), [AppCommand::UseProviderModel { profile, model }] if profile == "p1" && model == "o3"), "{commands:?}");
        // Shown at once, before the backend's echo.
        assert_eq!((state.settings.active_provider.as_deref(), state.settings.providers[0].model.as_str()), (Some("p1"), "o3"));
        let (texts, _) = click_chip(&mut state, "Work · o3");
        assert!(texts.iter().any(|(t, _)| t == "Current"));

        // Back to the account from the same menu: the profile stops sending.
        let (_, commands) = click_chip_menu(&mut state, "Work · o3", "GPT 5.1");
        assert!(matches!(commands.as_slice(), [AppCommand::UseAccountModel { .. }]), "{commands:?}");
        assert_eq!((state.settings.active_account.as_deref(), state.settings.active_provider.as_deref()), (Some("codex-0123abcd"), None));
    }

    /// The menu opens on the model in use, even when it sits below three accounts' lists.
    #[test]
    fn the_menu_opens_scrolled_to_the_model_in_use() {
        let mut state = with_account();
        state.accounts[0].models.clear();
        for (id, provider) in [("antigravity-0123abcd", AccountProvider::Antigravity), ("claude-0123abcd", AccountProvider::ClaudeCode)] {
            let model = catalog::models(provider)[0].clone();
            state.settings.accounts.insert(0, AccountEntry { id: id.into(), provider, label: format!("{id}@example.com"), model: model.clone() });
            state.accounts.insert(0, AccountView { id: id.into(), provider, email: Some(format!("{id}@example.com")), plan: None, model, models: Vec::new(), models_fetched_at: None, status: AccountStatus::SignedIn });
        }
        state.settings.providers = with_provider("o3").settings.providers;
        state.settings.providers[0].models = (0..12).map(|i| format!("m{i:02}")).chain(["o3".to_owned()]).collect();
        state.settings.select_profile(Some("p1".into()));
        let (texts, _) = click_chip(&mut state, "Work · o3");
        let chip = texts.iter().find(|(t, _)| t == "Work · o3").map(|(_, r)| *r).unwrap();
        let check = texts.iter().find(|(t, _)| t == icons::CHECK).map(|(_, r)| *r).expect("the model in use is checked");
        assert!(check.top() > chip.bottom() && check.bottom() < chip.bottom() + MENU_MAX_H, "the check at {check:?} is out of the menu under {chip:?}");
    }

    #[test]
    fn a_provider_without_models_offers_to_load_them() {
        let mut state = with_provider("");
        let groups = menu_groups(&state);
        assert_eq!(groups.len(), 1, "the provider in use, even without a list");
        assert!(groups[0].rows.is_empty() && groups[0].current);
        // A provider not in use and without models is left out.
        state.settings.active_provider = None;
        assert!(menu_groups(&state).iter().all(|g| g.source == Source::CodeAssist));
        state.settings.active_provider = Some("p1".into());

        let (_, commands) = click_chip_menu(&mut state, "Work · Choose model", "Load models…");
        assert!(matches!(commands.as_slice(), [AppCommand::LoadProviderModels { profile, .. }] if profile.api_key == "k-test"), "{commands:?}");
        assert_eq!((state.view, state.customize_tab), (crate::ui::View::Customizations, crate::ui::CustomizeTab::Providers));
        assert!(state.provider_dialog.as_ref().is_some_and(|d| d.editing.as_deref() == Some("p1") && d.loading.is_some()), "its dialog is open, loading");
    }

    #[test]
    fn with_a_google_sign_in_code_assist_is_offered_beside_a_provider() {
        let mut state = with_provider("gpt-4o");
        state.auth = AuthStatus::LoggedIn { email: Some("me@example.com".into()) };
        let groups = menu_groups(&state);
        assert_eq!(groups.last().map(|g| (g.source.clone(), g.current)), Some((Source::CodeAssist, false)));
        // One model is in use, and only it is checked: not also Code Assist's.
        state.settings.providers[0].models = vec!["gpt-4o".into()];
        let (texts, _) = click_chip(&mut state, "Work · GPT 4o");
        assert!(texts.iter().any(|(t, _)| t == "Flash"), "Code Assist is listed: {texts:?}");
        assert_eq!(texts.iter().filter(|(t, _)| t == icons::CHECK).count(), 1, "{texts:?}");
        // Picking from it leaves the provider, so the turn really goes to Code Assist.
        let (_, commands) = click_chip_menu(&mut state, "Work · GPT 4o", "Flash");
        assert!(matches!(commands.as_slice(), [AppCommand::SetActiveProvider(None), AppCommand::SetModel(_)]), "{commands:?}");
        assert_eq!(state.settings.active_provider, None);
    }

    #[test]
    fn without_a_provider_the_chip_opens_the_code_assist_picker() {
        let mut state = with_provider("gpt-4o-mini");
        state.settings.active_provider = None;
        let (texts, commands) = click_chip(&mut state, "Gemini 2.5 Pro");
        assert!(texts.iter().any(|(t, _)| t == "Model") && texts.iter().any(|(t, _)| t == "Flash"), "the picker opened");
        assert!(commands.is_empty(), "opening the picker sends nothing");
    }

    /// Code Assist in use, with a Codex account signed in but not in use.
    fn with_account() -> UiState {
        let mut state = UiState::default();
        state.settings.model = "gemini-2.5-pro".into();
        state.settings.accounts = vec![AccountEntry { id: "codex-0123abcd".into(), provider: AccountProvider::Codex, label: "dev@example.com".into(), model: "gpt-5.1-codex".into() }];
        state.accounts = vec![AccountView {
            id: "codex-0123abcd".into(),
            provider: AccountProvider::Codex,
            email: Some("dev@example.com".into()),
            plan: Some("plus".into()),
            model: "gpt-5.1-codex".into(),
            models: vec!["gpt-5.1".into(), "gpt-5.1-codex".into()],
            models_fetched_at: None,
            status: AccountStatus::SignedIn,
        }];
        state
    }

    #[test]
    fn the_picker_lists_account_models_and_activates_the_account() {
        let mut state = with_account();
        let groups = menu_groups(&state);
        assert_eq!(groups.len(), 2, "the account, then Code Assist (in use)");
        assert_eq!(groups[0].source, Source::Account("codex-0123abcd".into()));
        assert_eq!(groups[0].header.as_deref(), Some("ChatGPT (Codex) · dev@example.com"));
        assert!(!groups[0].current && groups[1].current && groups[1].source == Source::CodeAssist);
        assert_eq!(groups[0].rows.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), ["gpt-5.1", "gpt-5.1-codex"]);

        // Open the menu from the Code Assist chip and pick the account's other model.
        let (texts, commands) = click_chip(&mut state, "Gemini 2.5 Pro");
        assert!(commands.is_empty());
        assert!(texts.iter().any(|(t, _)| t == "ChatGPT (Codex) · dev@example.com"), "{texts:?}");
        assert!(texts.iter().any(|(t, _)| t == "Flash"), "Code Assist is listed while it is in use");
        let (_, commands) = click_chip_menu(&mut state, "Gemini 2.5 Pro", "GPT 5.1");
        assert!(
            matches!(commands.as_slice(), [AppCommand::UseAccountModel { account, model }] if account == "codex-0123abcd" && model == "gpt-5.1"),
            "{commands:?}"
        );
        // Shown at once: the account is in use with that model, and the chip names it.
        assert_eq!(state.settings.active_account.as_deref(), Some("codex-0123abcd"));
        assert_eq!(state.settings.accounts[0].model, "gpt-5.1");
        let (texts, _) = click_chip(&mut state, "ChatGPT (Codex) · GPT 5.1");
        assert!(texts.iter().any(|(t, _)| t == "Current"), "the group in use is marked");
        assert!(!texts.iter().any(|(t, _)| t == "Flash"), "Code Assist is not offered while an account sends turns");
        assert_eq!(menu_groups(&state).len(), 1);
    }

    /// Open the menu from the chip text `chip`, then click the menu row `row`.
    fn click_chip_menu(state: &mut UiState, chip: &str, row: &str) -> (Vec<(String, egui::Rect)>, Vec<UiAction>) {
        let ctx = context();
        let mut commands = Vec::new();
        let frame = |state: &mut UiState, commands: &mut Vec<UiAction>, events: Vec<egui::Event>| {
            painted(&ctx, events, |ui| {
                ui.horizontal(|ui| show(ui, state, commands));
            })
        };
        let texts = frame(state, &mut commands, Vec::new());
        let at = texts.iter().find(|(t, _)| t == chip).map(|(_, r)| r.center()).unwrap_or_else(|| panic!("no {chip:?} chip"));
        frame(state, &mut commands, vec![egui::Event::PointerMoved(at), click(at, true)]);
        frame(state, &mut commands, vec![click(at, false)]);
        frame(state, &mut commands, Vec::new());
        let texts = frame(state, &mut commands, Vec::new());
        let at = texts.iter().find(|(t, _)| t == row).map(|(_, r)| r.center()).unwrap_or_else(|| panic!("no {row:?} row in {texts:?}"));
        frame(state, &mut commands, vec![egui::Event::PointerMoved(at), click(at, true)]);
        frame(state, &mut commands, vec![click(at, false)]);
        let texts = frame(state, &mut commands, Vec::new());
        (texts, commands)
    }

    #[test]
    fn an_account_without_a_fetched_list_offers_its_catalog() {
        let mut state = with_account();
        state.accounts[0].models.clear();
        state.settings.accounts[0].model = "gpt-6.1-terra".into();
        let rows: Vec<String> = menu_groups(&state)[0].rows.iter().map(|c| c.id.clone()).collect();
        assert_eq!(rows.len(), catalog::models(AccountProvider::Codex).len());
        assert!(rows.contains(&"gpt-6.1-terra".to_owned()));
        // A model chosen by hand that no list knows still gets its row, first.
        state.settings.accounts[0].model = "gpt-6-preview".into();
        assert_eq!(menu_groups(&state)[0].rows[0].id, "gpt-6-preview");
        // An expired account has no group: its models cannot be sent.
        state.accounts[0].status = AccountStatus::Expired("session expired".into());
        assert_eq!(menu_groups(&state).len(), 1);
    }

    #[test]
    fn an_account_in_use_that_expired_leaves_code_assist_to_pick() {
        let mut state = with_account();
        state.settings.select_account("codex-0123abcd");
        state.settings.accounts[0].model = "gpt-6.1-sol".into();
        state.accounts[0].status = AccountStatus::Expired("session expired".into());
        // Not an empty menu: Code Assist is there, and not marked as the one in use.
        let groups = menu_groups(&state);
        assert_eq!(groups.len(), 1);
        assert!(groups[0].source == Source::CodeAssist && !groups[0].current);

        // Picking from it leaves the account, so the turn really goes to Code Assist.
        let (_, commands) = click_chip_menu(&mut state, "ChatGPT (Codex) · GPT 6.1 Sol", "Flash");
        assert!(
            matches!(commands.as_slice(), [AppCommand::SetActiveProvider(None), AppCommand::SetModel(_)]),
            "{commands:?}"
        );
        assert!(state.settings.active_account.is_none());
    }
}
