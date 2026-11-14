//! Settings (DESIGN.md 2.8): an overlay over the current screen, as the 2.31 reference shows it.
//!
//! A modal card with a title row (settings search and a close button), a rail of sections down
//! its left edge and the section's flat rows on the right (colored icon, label + description,
//! control on the right). The screen underneath keeps its state and is painted behind the card.
//! Edits mark `settings_dirty`; `AntiCraftApp::ui` flushes them through `SaveSettings`.

use egui::{Align2, Color32, Rect, Sense, Stroke, StrokeKind, Ui, Vec2};

use super::model_picker::{self, Pick};
use super::{account_models, format, icons, login, theme, widgets, CustomizeTab, ProviderDialog, UiAction, UiState};
use crate::app::{AccountLoginInfo, AppCommand};
use crate::auth::antigravity::ImportCheck;
use crate::auth::external::{AccountProvider, AccountStatus, AccountView};
use crate::auth::{AuthStatus, ClientSource, CLIENT_SECRET_ENV};
use crate::agent::policy::{validate_rule, MAX_RULES};
use crate::agent::plugins::PluginChoice;
use crate::settings::{AgentMode, ArtifactReviewPolicy, ConversationWidth, FontSize, PermissionMode, QueueMode, Settings, Theme, UiFont, MAX_PROVIDER_PROFILES};
use crate::providers::{ProviderKind, ProviderProfile};

/// Widest the section's rows get inside the card.
const COLUMN: f32 = 758.0;
/// Row pitch on the reference is ~73px: 6 + 40 + 12 margin plus 15 spacing.
const ROW_GAP: f32 = 15.0;
/// Width of the section rail on the card's left edge.
const RAIL_W: f32 = 200.0;
/// The card's preferred size; a smaller window shrinks it.
const CARD_W: f32 = 1040.0;
const CARD_H: f32 = 720.0;
/// Room the card leaves at the window's edges.
const CARD_MARGIN: f32 = 40.0;
/// Height of the card's title row.
const HEADER_H: f32 = 52.0;
/// Width of the settings search field.
const SEARCH_W: f32 = 200.0;
/// Rows of the section rail, in order.
const SECTIONS: [(CustomizeTab, &str, &str); 5] = [
    (CustomizeTab::General, icons::BRAIN, "Models"),
    (CustomizeTab::Agent, icons::WRENCH, "Tools and approvals"),
    (CustomizeTab::Workspaces, icons::FOLDER, "Workspaces"),
    (CustomizeTab::Providers, icons::DESKTOP, "Providers"),
    (CustomizeTab::Account, icons::USER_CIRCLE, "Account"),
];

/// Size of the card in a window whose content area is `avail`.
fn card_size(avail: Vec2) -> Vec2 {
    Vec2::new(CARD_W.min(avail.x - 2.0 * CARD_MARGIN).max(560.0), CARD_H.min(avail.y - 2.0 * CARD_MARGIN).max(360.0))
}

/// The Settings overlay over the current screen, while `state.customize_open`. It closes with
/// its close button or Esc, never with a click outside: that would throw away a typed key.
pub fn overlay(ctx: &egui::Context, state: &mut UiState, commands: &mut Vec<UiAction>) {
    if !state.customize_open {
        return;
    }
    // The provider dialog is a modal of its own, shown after this one: Esc is its first.
    let dialog_open = state.provider_dialog.is_some();
    let size = card_size(ctx.content_rect().size());
    let mut close = false;
    let frame = widgets::card(theme::RADIUS_POPUP + 2, 0).stroke(Stroke::new(1.0, theme::POPUP_BORDER)).shadow(ctx.global_style().visuals.popup_shadow);
    let modal = egui::Modal::new(egui::Id::unique("anticraft_settings")).frame(frame).backdrop_color(Color32::from_black_alpha(140)).show(ctx, |ui| {
        let (card, _) = ui.allocate_exact_size(size, Sense::hover());
        let header = Rect::from_min_size(card.min, Vec2::new(card.width(), HEADER_H));
        close |= title_row(ui, header);
        let rail = Rect::from_min_max(egui::pos2(card.left(), header.bottom()), egui::pos2(card.left() + RAIL_W, card.bottom()));
        let content = Rect::from_min_max(egui::pos2(rail.right() + 1.0, header.bottom() + 1.0), card.max);
        ui.painter().vline(rail.right(), rail.y_range(), Stroke::new(1.0, theme::EDGE));
        ui.scope_builder(egui::UiBuilder::new().max_rect(rail.shrink2(Vec2::new(10.0, 12.0))), |ui| section_rail(ui, state));
        ui.scope_builder(egui::UiBuilder::new().max_rect(content), |ui| {
            ui.set_clip_rect(content.intersect(ui.clip_rect()));
            sections(ui, state, commands);
        });
    });
    let esc = || ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
    if close || (!dialog_open && modal.is_top_modal && !modal.any_popup_open && esc()) {
        state.close_customize();
    }
}

/// The title row: "Settings", the search field and the close button. True when closed.
fn title_row(ui: &mut Ui, row: Rect) -> bool {
    ui.painter().text(egui::pos2(row.left() + 22.0, row.center().y), Align2::LEFT_CENTER, "Settings", theme::medium(15.5), theme::FG);
    ui.painter().hline(row.x_range(), row.bottom(), Stroke::new(1.0, theme::EDGE));
    let close = Rect::from_center_size(egui::pos2(row.right() - 26.0, row.center().y), Vec2::splat(26.0));
    let r = ui.interact(close, ui.scope_id().with("settings_close"), Sense::click());
    widgets::paint_icon_button(ui, close, &r, icons::X, true, theme::FG_MUTED);
    let field = Rect::from_min_size(egui::pos2(close.left() - 10.0 - SEARCH_W, row.center().y - 14.0), Vec2::new(SEARCH_W, 28.0));
    search_field(ui, field);
    r.on_hover_text("Close (Esc)").clicked()
}

/// The current section's rows, filtered by the search field, without the card around them.
/// (The overlay clips them to its own rect; tests that read a row far down the page draw them
/// straight into a tall canvas.)
pub(crate) fn sections(ui: &mut Ui, state: &mut UiState, commands: &mut Vec<UiAction>) {
    let query: String = ui.data(|d| d.get_temp(search_id())).unwrap_or_default();
    egui::ScrollArea::vertical().id_salt(("customize_page", state.customize_tab as u8)).auto_shrink([false, false]).show(ui, |ui| {
        ui.add_space(22.0);
        widgets::centered_column(ui, COLUMN.min(ui.available_width() - 48.0), |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            let mut cx = Ctx { query: query.trim().to_lowercase(), shown: 0 };
            match state.customize_tab {
                CustomizeTab::General => models(ui, state, commands, &mut cx),
                CustomizeTab::Agent => tools(ui, state, commands, &mut cx),
                CustomizeTab::Workspaces => workspaces(ui, state, commands, &mut cx),
                CustomizeTab::Providers => providers(ui, state, commands, &mut cx),
                CustomizeTab::Account => account(ui, state, commands, &mut cx),
            }
            if cx.shown == 0 && !cx.query.is_empty() {
                ui.add_space(24.0);
                ui.vertical_centered(|ui| {
                    ui.label(egui::RichText::new("No settings match your search").font(theme::font(13.0)).color(theme::FG_MUTED));
                });
            }
        });
        ui.add_space(32.0);
    });
}

/// The section rail: one row per section, the open one filled, and the version at the foot.
fn section_rail(ui: &mut Ui, state: &mut UiState) {
    ui.spacing_mut().item_spacing.y = 2.0;
    for (tab, icon, label) in SECTIONS {
        let selected = state.customize_tab == tab;
        if widgets::nav_row(ui, icon, label, selected, true).clicked() && !selected {
            state.open_customize(tab);
        }
    }
    let foot = ui.max_rect();
    ui.painter().text(
        egui::pos2(foot.left() + 10.0, foot.bottom() - 6.0),
        Align2::LEFT_BOTTOM,
        format!("AntiCraft {}", env!("CARGO_PKG_VERSION")),
        theme::font(11.5),
        theme::FG_DIM,
    );
}

/// Per-frame row filter state.
struct Ctx {
    query: String,
    shown: usize,
}

impl Ctx {
    fn visible(&self, label: &str, desc: &str) -> bool {
        self.query.is_empty() || label.to_lowercase().contains(&self.query) || desc.to_lowercase().contains(&self.query)
    }
}

/// egui memory key of the settings search field (UI-only).
fn search_id() -> egui::Id {
    egui::Id::unique("anticraft_settings_search")
}

/// The settings search field. What it holds filters the section's rows.
fn search_field(ui: &mut Ui, field: Rect) {
    let id = search_id();
    let mut q: String = ui.data(|d| d.get_temp(id)).unwrap_or_default();
    let focused = ui.ctx().memory(|m| m.has_focus(id.with("edit")));
    let p = ui.painter();
    p.rect_filled(field, 6, theme::FIELD_BG);
    p.rect_stroke(field, 6, Stroke::new(1.0, if focused { theme::PRIMARY } else { theme::TRACK_BORDER }), StrokeKind::Inside);
    p.text(field.left_center() + Vec2::new(11.0, 0.0), Align2::LEFT_CENTER, icons::MAGNIFYING_GLASS, theme::icon(13.0), theme::FG_MUTED);
    let edit = Rect::from_min_max(field.min + Vec2::new(28.0, 0.0), field.max - Vec2::new(8.0, 0.0));
    ui.put(
        edit,
        egui::TextEdit::singleline(&mut q)
            .id(id.with("edit"))
            .frame(egui::Frame::NONE)
            .font(theme::font(12.5))
            .align(egui::Align2::LEFT_CENTER)
            .hint_text(egui::RichText::new("Search settings").color(theme::PLACEHOLDER))
            .desired_width(edit.width()),
    );
    ui.data_mut(|d| d.insert_temp(id, q));
}

/// Distance from the label column to a left-placed toggle (reference toggle x 834 for text x 528; includes item spacing).
const TOGGLE_COLUMN: f32 = 297.0;
/// Border of numeric / choice fields.
const FIELD_STROKE: Color32 = Color32::from_rgb(0x4b, 0x4b, 0x4b);

/// Section heading (13/medium) with a muted description line.
fn section(ui: &mut Ui, title: &str, desc: &str) {
    ui.horizontal(|ui| {
        ui.add_space(10.0);
        ui.label(egui::RichText::new(title).font(theme::medium(17.5)).color(theme::FG));
    });
    ui.add_space(1.0);
    ui.horizontal(|ui| {
        ui.add_space(10.0);
        ui.add(egui::Label::new(egui::RichText::new(desc).font(theme::font(11.5)).color(theme::FG_MUTED)).wrap());
    });
    ui.add_space(9.0);
}

/// Rounded (radius 8) tile filled with a top-to-bottom gradient from `accent` to a lighter tint.
fn gradient_tile(ui: &Ui, rect: Rect, accent: Color32) {
    let bottom = accent.lerp_to_gamma(Color32::WHITE, 0.25);
    let shape = egui::epaint::RectShape::filled(rect, 8, accent);
    // Tessellate the rounded rect, then recolor each vertex by its height.
    let mut mesh = egui::Mesh::default();
    let mut tess = egui::epaint::Tessellator::new(ui.ctx().pixels_per_point(), Default::default(), [1, 1], Vec::new());
    tess.tessellate_rect(&shape, &mut mesh);
    for v in &mut mesh.vertices {
        let t = ((v.pos.y - rect.top()) / rect.height()).clamp(0.0, 1.0);
        let c = accent.lerp_to_gamma(bottom, t);
        let a = v.color.a() as f32 / 255.0;
        v.color = c.gamma_multiply(a);
    }
    ui.painter().add(mesh);
}

/// One flat settings row: colored icon, label + description, control right-aligned.
fn row(ui: &mut Ui, cx: &mut Ctx, icon: &str, accent: Color32, label: &str, desc: &str, control: impl FnOnce(&mut Ui)) {
    row_impl(ui, cx, icon, accent, label, theme::FG, desc, false, control, None);
}

/// [`row`] with its label in `theme::WARNING`: something the user has to act on (an account
/// whose session ended).
fn warning_row(ui: &mut Ui, cx: &mut Ctx, icon: &str, label: &str, desc: &str, control: impl FnOnce(&mut Ui)) {
    row_impl(ui, cx, icon, theme::WARNING, label, theme::WARNING, desc, false, control, None);
}

/// [`row`] with extra content (`body`) under the header, aligned with the label column and
/// inside the same hover background. Neither closure runs when the search hides the row.
#[allow(clippy::too_many_arguments)]
fn row_with_body(ui: &mut Ui, cx: &mut Ctx, icon: &str, accent: Color32, label: &str, desc: &str, control: impl FnOnce(&mut Ui), body: impl FnOnce(&mut Ui)) {
    let mut body = Some(body);
    row_impl(ui, cx, icon, accent, label, theme::FG, desc, false, control, Some(&mut |ui: &mut Ui| {
        if let Some(b) = body.take() {
            b(ui);
        }
    }));
}

/// Boolean settings row: the 40x22 toggle sits in a column ~306px right of the label start.
fn toggle_row(ui: &mut Ui, cx: &mut Ctx, icon: &str, accent: Color32, label: &str, desc: &str, on: &mut bool) -> bool {
    let mut changed = false;
    row_impl(ui, cx, icon, accent, label, theme::FG, desc, true, |ui| changed = widgets::toggle(ui, on).changed(), None);
    changed
}

/// Left offset of a row's label column inside the row frame: 30px tile, 8px item spacing and
/// the 35px gap of [`row_impl`].
const BODY_INDENT: f32 = 73.0;

#[allow(clippy::too_many_arguments)]
fn row_impl(
    ui: &mut Ui,
    cx: &mut Ctx,
    icon: &str,
    accent: Color32,
    label: &str,
    label_color: Color32,
    desc: &str,
    toggle_col: bool,
    control: impl FnOnce(&mut Ui),
    body: Option<&mut dyn FnMut(&mut Ui)>,
) {
    if !cx.visible(label, desc) {
        return;
    }
    cx.shown += 1;
    let w = ui.available_width();
    // Reserve the hover background before the content so it paints underneath.
    let bg = ui.painter().add(egui::Shape::Noop);
    let r = ui.scope(|ui| {
        ui.set_width(w);
        egui::Frame::NONE.inner_margin(egui::Margin { left: 16, right: 16, top: 6, bottom: 12 }).show(ui, |ui| {
            ui.set_width(w - 32.0);
            ui.horizontal(|ui| {
                ui.set_min_height(40.0);
                // 30x40 accent tile (vertical gradient, lighter at the bottom) with a small glyph.
                let (ir, _) = ui.allocate_exact_size(Vec2::new(30.0, 40.0), Sense::hover());
                gradient_tile(ui, ir, accent);
                ui.painter().text(ir.center(), Align2::CENTER_CENTER, icon, theme::icon(14.0), Color32::WHITE);
                // 44px gap tile-to-label (43 minus the 8px item spacing, plus glyph bearing).
                ui.add_space(35.0);
                let text_w = if toggle_col { TOGGLE_COLUMN - 10.0 } else { (ui.available_width() - 260.0).max(180.0) };
                ui.vertical(|ui| {
                    ui.set_width(text_w);
                    ui.spacing_mut().item_spacing.y = 3.0;
                    ui.label(egui::RichText::new(label).font(theme::font(13.5)).color(label_color));
                    if !desc.is_empty() {
                        let l = egui::Label::new(egui::RichText::new(desc).font(theme::font(12.0)).color(theme::FG_MUTED));
                        // Toggle rows keep a single line so the 73px pitch holds; full text on hover.
                        if toggle_col {
                            ui.add(l.truncate());
                        } else {
                            ui.add(l.wrap());
                        }
                    }
                });
                if toggle_col {
                    ui.add_space(10.0);
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), control);
                } else {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 8.0;
                        control(ui);
                    });
                }
            });
            if let Some(body) = body {
                ui.add_space(6.0);
                ui.horizontal_top(|ui| {
                    ui.add_space(BODY_INDENT);
                    ui.vertical(|ui| body(ui));
                });
            }
        });
    });
    let rect = r.response.rect;
    if ui.rect_contains_pointer(rect) {
        ui.painter().set(bg, egui::Shape::rect_filled(rect, 8, theme::ROW_HOVER));
    }
    ui.add_space(ROW_GAP);
}

/// Number field: `[-] value [+]`-free drag value styled like the other fields.
fn number(ui: &mut Ui, value: &mut u64, range: std::ops::RangeInclusive<u64>, suffix: &str) -> bool {
    // Compact control: 26 h, radius 4, `#1c1c1c` fill, 1px `#4b4b4b` border.
    ui.scope(|ui| {
        let v = ui.visuals_mut();
        for w in [&mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active] {
            w.bg_fill = theme::CARD;
            w.weak_bg_fill = theme::CARD;
            w.bg_stroke = Stroke::new(1.0, FIELD_STROKE);
            w.corner_radius = egui::CornerRadius::same(4);
        }
        ui.add_sized(Vec2::new(96.0, 26.0), egui::DragValue::new(value).range(range).suffix(suffix).speed(1.0)).changed()
    })
    .inner
}

fn models(ui: &mut Ui, state: &mut UiState, commands: &mut Vec<UiAction>, cx: &mut Ctx) {
    let mut dirty = false;
    section(ui, "Models", "Choose the model AntiCraft uses for new turns. Aliases resolve when a request is sent.");
    let busy = state.busy;
    let mut picked = None;
    {
        let label = format::model_label(&state.settings.model);
        let desc = default_model_desc(&state.settings);
        let st: &UiState = state;
        row(ui, cx, icons::BRAIN, theme::ACCENT_BLUE, "Default model", &desc, |ui| {
            let r = widgets::pill_chip(ui, None, &label, true, !busy, false);
            ui.painter().rect_stroke(r.rect, 14, Stroke::new(1.0, FIELD_STROKE), StrokeKind::Inside);
            // The click that opens the list: it scrolls to the model in use.
            let opening = r.clicked();
            widgets::popup_menu(&r, 300.0, |ui| {
                // The signed-in accounts' and the providers' models, then the Code Assist list:
                // three catalogs are taller than the window, so the list scrolls.
                let pick = egui::ScrollArea::vertical().id_salt("default_model_list").max_height(440.0).show(ui, |ui| model_picker::default_model_click(ui, st, opening)).inner;
                if let Some(p) = pick {
                    picked = Some(p);
                    ui.close();
                }
            });
        });
    }
    match picked {
        Some(Pick::CodeAssist(m)) => {
            state.settings.model = m;
            dirty = true;
        }
        // An account's or a provider's model is set by the backend, which puts that backend in
        // use as well.
        Some(Pick::Account { account, model }) => model_picker::pick_account_model(state, commands, account, model),
        Some(Pick::Profile { profile, model }) => model_picker::pick_profile_model(state, commands, profile, model),
        Some(Pick::EditProfile(id)) => edit_provider(state, commands, &id),
        None => {}
    }
    dirty |= toggle_row(ui, cx, icons::LIGHTNING, theme::ACCENT_AMBER, "Preview model access", "Turn on if your account can use preview models. AntiCraft does not detect this.", &mut state.settings.preview_access);
    dirty |= toggle_row(ui, cx, icons::LIGHTNING, theme::ACCENT_BLUE, "Latest Flash GA", "Send Flash requests as the newest generally available Flash model.", &mut state.settings.latest_flash_ga);
    ui.add_space(22.0);
    section(ui, "Appearance", "Changes apply at once. Only the dark theme is fully styled.");
    row(ui, cx, icons::DESKTOP, theme::FG_SECONDARY, "Theme", "Applies to the whole window.", |ui| {
        dirty |= widgets::segmented(ui, &mut state.settings.theme, &[(Theme::Light, "Light"), (Theme::Dark, "Dark")]);
    });
    let a = &mut state.settings.appearance;
    row(ui, cx, icons::ARROWS_LEFT_RIGHT, theme::ACCENT_BLUE, "Conversation width", "Default is 768 px, Wide 960 px. Fill stretches conversations across the whole center pane. The agent works in both.", |ui| {
        dirty |= widgets::segmented(ui, &mut a.width, &[(ConversationWidth::Default, "Default"), (ConversationWidth::Wide, "Wide"), (ConversationWidth::Fill, "Fill")]);
    });
    let fonts = [(UiFont::System, "System UI"), (UiFont::Inter, "Inter"), (UiFont::GoogleSans, "Google Sans")];
    let missing: Vec<&str> = fonts.iter().filter(|(f, _)| !theme::font_available(*f)).map(|(_, l)| *l).collect();
    let font_desc = if missing.is_empty() {
        "The font of the interface text. Code keeps its monospaced font.".to_owned()
    } else {
        format!("The font of the interface text. {} {} not installed on this computer; AntiCraft uses fonts installed in Windows and does not bundle them.", missing.join(" and "), if missing.len() == 1 { "is" } else { "are" })
    };
    row(ui, cx, icons::TEXT_T, theme::ACCENT_AMBER, "Font", &font_desc, |ui| {
        let options: Vec<(UiFont, &str, bool)> = fonts.iter().map(|(f, l)| (*f, *l, theme::font_available(*f))).collect();
        dirty |= widgets::segmented_some(ui, &mut a.font, &options, "Not installed on this computer");
    });
    row(ui, cx, icons::TEXT_T, theme::FG_SECONDARY, "Font size", "Interface text size in pixels. Icons keep their size.", |ui| {
        dirty |= widgets::segmented(ui, &mut a.font_size, &[(FontSize::Small, "12"), (FontSize::Medium, "14"), (FontSize::Large, "16")]);
    });
    state.settings_dirty |= dirty;
}

/// Description of the "Default model" row. While an account or a provider profile is in use,
/// turns go to its own model, and the row says so instead of implying the picked model is used.
fn default_model_desc(settings: &Settings) -> String {
    if let Some(a) = settings.active_account_entry() {
        let name = if a.label.trim().is_empty() { a.provider.label() } else { a.label.trim() };
        return format!(
            "Code Assist model. New turns now use {} ({name}); change it from the composer or the Account tab.",
            a.model.trim()
        );
    }
    match settings.active_provider_profile() {
        Some(p) => format!(
            "Code Assist model. New turns now use {} ({}); switch back to Code Assist from the model menu in the composer.",
            p.label.trim(),
            match p.model.trim() {
                "" => "no model chosen",
                m => m,
            }
        ),
        None => "Used for every new turn. You can still switch per message from the composer.".to_owned(),
    }
}

fn tools(ui: &mut Ui, state: &mut UiState, commands: &mut Vec<UiAction>, cx: &mut Ctx) {
    let mut dirty = false;
    let st = &mut state.settings;
    section(ui, "Agent", "How the agent plans its work and when it stops to check with you.");
    row(ui, cx, icons::BRAIN, theme::ACCENT_BLUE, "Default mode", "Planning researches first, writes a plan and works in task groups. Fast acts right away. The composer chip overrides it per conversation.", |ui| {
        dirty |= widgets::segmented(ui, &mut st.agent_mode, &[(AgentMode::Planning, "Planning"), (AgentMode::Fast, "Fast")]);
    });
    row(ui, cx, icons::EYE, theme::ACCENT_AMBER, "Plan review", "Planning mode only. Request review pauses on the implementation plan; Agent decides lets the agent judge; Always proceed never pauses.", |ui| {
        dirty |= widgets::segmented(
            ui,
            &mut st.artifact_review,
            &[(ArtifactReviewPolicy::RequestReview, "Request review"), (ArtifactReviewPolicy::AgentDecides, "Agent decides"), (ArtifactReviewPolicy::AlwaysProceed, "Always proceed")],
        );
    });
    row(ui, cx, icons::CHATS_CIRCLE, theme::FG_SECONDARY, "Messages sent while busy", "Queue waits for the current turn to end. Send now adds the message to the running turn. Alt+Enter does the other one.", |ui| {
        dirty |= widgets::segmented(ui, &mut st.queued_messages, &[(QueueMode::Queue, "Queue"), (QueueMode::SendImmediately, "Send now")]);
    });
    ui.add_space(22.0);
    section(ui, "Tools and approvals", "Decide which agent tools run on their own. Denylisted commands never run, whatever else is set.");
    dirty |= toggle_row(ui, cx, icons::MAGNIFYING_GLASS, theme::ACCENT_BLUE, "Auto-approve read and search tools", "read_file, list_directory, glob and grep_search run without asking.", &mut st.auto_approve_read_tools);
    row(ui, cx, icons::HAND, theme::ACCENT_AMBER, "Mode", "The mode every new conversation starts from. Manual asks for commands and edits, with no auto-run. Edit Auto applies file edits and still asks for commands. Plan researches and writes a plan first. Auto runs read-only and build commands and applies edits. Bypass runs every call without asking (deletions, denylisted commands and MCP tools still ask) and is not saved: it lasts for this run only, and a restart comes back on Auto, which is the mode its save stores.", |ui| {
        let mut mode = st.permission_mode();
        if widgets::segmented(
            ui,
            &mut mode,
            &[(PermissionMode::Manual, "Manual"), (PermissionMode::EditAuto, "Edit Auto"), (PermissionMode::Plan, "Plan"), (PermissionMode::Auto, "Auto"), (PermissionMode::Bypass, "Bypass")],
        ) {
            st.set_permission_mode(mode);
            dirty = true;
        }
    });
    row(ui, cx, icons::CHECK_CIRCLE, theme::SUCCESS, "\"Always allow\" choices", "Choices made in the confirmation prompt last until the conversation or app is closed.", |ui| {
        ui.label(egui::RichText::new("Session only").font(theme::font(12.0)).color(theme::FG_MUTED));
    });
    ui.add_space(22.0);
    section(ui, "Terminal", "run_shell_command runs PowerShell inside the workspace. Commands run on their own according to the Mode above.");
    row(ui, cx, icons::CLOCK_COUNTER_CLOCKWISE, theme::FG_SECONDARY, "Shell timeout", "Seconds before a running command and its child processes are stopped (5-3600).", |ui| {
        dirty |= number(ui, &mut st.shell_timeout_secs, 5..=3600, " s");
    });
    row(ui, cx, icons::FILE_TEXT, theme::FG_SECONDARY, "Shell output cap", "Maximum command output returned to the model.", |ui| {
        let mut kb = (st.shell_output_cap_bytes as u64 / 1024).max(1);
        if number(ui, &mut kb, 1..=4096, " KB") {
            st.shell_output_cap_bytes = (kb * 1024) as _;
            dirty = true;
        }
    });
    ui.add_space(12.0);
    if cx.visible("Allowlist", "commands that run without asking") {
        cx.shown += 1;
        dirty |= rule_list(ui, "allow", "Allowlist", "Commands starting with these words run without asking. Prefix a rule with regex: for a pattern.", "e.g. npm run lint", &mut st.terminal_allowlist);
        ui.add_space(ROW_GAP);
    }
    if cx.visible("Denylist", "commands that never run") {
        cx.shown += 1;
        dirty |= rule_list(ui, "deny", "Denylist", "Commands matching these rules are refused, also inside pipes, chains and subexpressions.", "e.g. git push --force", &mut st.terminal_denylist);
    }
    ui.add_space(22.0);
    bypass_mode(ui, state, commands, cx);
    ui.add_space(22.0);
    plugins(ui, state, commands, cx);
    state.settings_dirty |= dirty;
}

/// Bypass Mode: switch, the provider its first requests go to, how many, and the instruction
/// files added to those requests.
///
/// The section warns in `theme::ERROR` the whole time it is on, matching the band the composer
/// carries: a mode that sends a prompt somewhere else is never quiet about it. It sits under the
/// tool settings because that is what it changes — where the same tools' requests are sent.
fn bypass_mode(ui: &mut Ui, state: &mut UiState, commands: &mut Vec<UiAction>, cx: &mut Ctx) {
    let mut dirty = false;
    let has_provider = !state.settings.providers.is_empty();
    if cx.visible("Bypass Mode", "the first requests of each message go to another provider") {
        cx.shown += 1;
        section(
            ui,
            "Bypass Mode",
            "The first requests of each message go to a provider you choose, then the conversation returns to the model in use. \
             Tools, history and approvals are the same; the provider you name sees those requests and their content.",
        );
        if !has_provider {
            egui::Frame::NONE.inner_margin(egui::Margin { left: 16, right: 16, top: 2, bottom: 8 }).show(ui, |ui| {
                ui.add(
                    egui::Label::new(
                        egui::RichText::new("Bypass Mode needs a provider profile. Add one here, or on the Providers tab first.")
                            .font(theme::font(12.0))
                            .color(theme::WARNING),
                    )
                    .wrap(),
                );
            });
        }
    }

    // A settings edit is allowed mid-turn here: the turn reads its route when the agent is
    // built, so switching the mode on or off never changes a message already in flight.
    // Both controls are per-row edits of the local settings copy, like the Providers tab, so one
    // save carries the mode and the profile dialog's result together.
    dirty |= toggle_row(
        ui,
        cx,
        icons::WARNING,
        theme::ACCENT_RED,
        "Use Bypass Mode",
        "On: a red band sits over the composer and the first requests of each message take the route below. Off: every request uses the model in use.",
        &mut state.settings.bypass.enabled,
    );

    let selected = state.settings.bypass.provider.clone();
    let label = selected
        .as_deref()
        .and_then(|id| state.settings.providers.iter().find(|p| p.id == id))
        .map(profile_label)
        .unwrap_or_else(|| "Choose a provider".to_owned());
    let mut pick = None;
    let mut add = false;
    {
        let profiles: Vec<(String, String, String)> =
            state.settings.providers.iter().map(|p| (p.id.clone(), profile_label(p), profile_desc(p))).collect();
        row(ui, cx, icons::BRAIN, theme::ACCENT_RED, "Bypass provider", "The profile that answers those requests. Its own model is sent; a model you pick in the composer does not replace it.", |ui| {
            let r = widgets::pill_chip(ui, None, &label, true, true, false);
            ui.painter().rect_stroke(r.rect, 14, Stroke::new(1.0, FIELD_STROKE), StrokeKind::Inside);
            widgets::popup_menu(&r, 340.0, |ui| {
                if profiles.is_empty() {
                    ui.label(egui::RichText::new("No provider profiles yet.").font(theme::font(12.0)).color(theme::FG_MUTED));
                    ui.add_space(4.0);
                }
                for (id, name, desc) in &profiles {
                    let current = selected.as_deref() == Some(id.as_str());
                    if profile_row(ui, name, desc, current) {
                        pick = Some(id.clone());
                        ui.close();
                    }
                }
                if !profiles.is_empty() {
                    ui.separator();
                }
                if widgets::ghost_button(ui, Some(icons::PLUS), "Add provider…", theme::FG, true).clicked() {
                    add = true;
                    ui.close();
                }
            });
        });
    }
    if let Some(id) = pick {
        state.settings.bypass.provider = Some(id);
        dirty = true;
    }
    if add && state.settings.providers.len() < MAX_PROVIDER_PROFILES {
        let kind = state.settings.providers.last().map_or(ProviderKind::GeminiApi, |p| p.kind);
        state.provider_dialog = Some(ProviderDialog::add(kind));
        state.bypass_dialog = true;
    }

    row(ui, cx, icons::CLOCK_COUNTER_CLOCKWISE, theme::ACCENT_RED, "Requests before switching back", "Counted from the first request of each message. One message may take several requests (one per tool round).", |ui| {
        let mut n = state.settings.bypass.requests as u64;
        if number(ui, &mut n, 1..=crate::settings::MAX_BYPASS_REQUESTS as u64, "") {
            state.settings.bypass.requests = n as u32;
            dirty = true;
        }
    });

    bypass_files(ui, state, commands, cx, &mut dirty);

    // The dialog opened here is the Providers tab's own, drawn on this tab while it belongs here.
    if state.bypass_dialog && state.provider_dialog.is_some() {
        let actions = provider_dialog(ui, state, commands);
        if state.provider_dialog.is_none() {
            // Closed: Cancel, Esc, the close button, a click outside, or a Save.
            state.bypass_dialog = false;
        }
        for action in &actions {
            let id = match action {
                ProviderAction::Add(p) | ProviderAction::Replace(p) => Some(p.id.clone()),
                _ => None,
            };
            if let Some(id) = id {
                // A profile added from this section is the one the mode names: ending up with a
                // mode pointed at some other profile would be a surprise.
                state.settings.bypass.provider = Some(id);
            }
        }
        dirty |= apply_actions(&mut state.settings, &actions);
    }
    state.settings_dirty |= dirty;
}

/// Label of a profile as the picker shows it.
fn profile_label(p: &ProviderProfile) -> String {
    match p.model.trim() {
        "" => p.label.trim().to_owned(),
        m => format!("{} · {m}", p.label.trim()),
    }
}

/// A profile in a menu: name, muted detail, a check when current. True when clicked.
fn profile_row(ui: &mut Ui, label: &str, desc: &str, current: bool) -> bool {
    let (rect, r) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 34.0), Sense::click());
    if current || r.hovered() {
        ui.painter().rect_filled(rect, 4, theme::SECONDARY);
    }
    let text = Rect::from_min_max(egui::pos2(rect.left() + 8.0, rect.top()), egui::pos2(rect.right() - 26.0, rect.bottom()));
    widgets::paint_truncated(ui, Rect::from_min_max(text.min, egui::pos2(text.right(), rect.center().y + 1.0)), label, theme::font(12.5), theme::FG);
    widgets::paint_truncated(ui, Rect::from_min_max(egui::pos2(text.left(), rect.center().y + 1.0), text.max), desc, theme::font(11.0), theme::FG_MUTED);
    if current {
        ui.painter().text(egui::pos2(rect.right() - 9.0, rect.center().y), Align2::RIGHT_CENTER, icons::CHECK, theme::icon(12.0), theme::FG);
    }
    r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
}

/// The instruction files list: one row per file with Remove, then "Add .md file…".
fn bypass_files(ui: &mut Ui, state: &mut UiState, commands: &mut Vec<UiAction>, cx: &mut Ctx, dirty: &mut bool) {
    let desc = "Markdown read into the bypass requests' instructions, never into the transcript. Useful for a house style or a rule set meant for that provider only.";
    if !cx.visible("Instruction files", desc) {
        return;
    }
    cx.shown += 1;
    egui::Frame::NONE.inner_margin(egui::Margin { left: 16, right: 16, top: 6, bottom: 6 }).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 4.0;
        ui.label(egui::RichText::new("Instruction files").font(theme::font(13.5)).color(theme::FG));
        ui.add(egui::Label::new(egui::RichText::new(desc).font(theme::font(12.0)).color(theme::FG_MUTED)).wrap());
        ui.add_space(4.0);
        let mut remove = None;
        if state.settings.bypass.files.is_empty() {
            ui.label(egui::RichText::new("No files").font(theme::font(12.0)).color(theme::PLACEHOLDER));
        }
        for (i, path) in state.settings.bypass.files.iter().enumerate() {
            ui.horizontal(|ui| {
                let w = (ui.available_width() - 32.0).max(120.0);
                let (rect, r) = ui.allocate_exact_size(Vec2::new(w, 26.0), Sense::hover());
                ui.painter().rect_filled(rect, 4, theme::FIELD_BG);
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string());
                widgets::paint_truncated(ui, rect.shrink2(Vec2::new(9.0, 0.0)), &name, theme::font(12.0), theme::FG);
                r.on_hover_text(path.display().to_string());
                if widgets::icon_button(ui, icons::X, 24.0, "Remove file", true).clicked() {
                    remove = Some(i);
                }
            });
        }
        if let Some(i) = remove {
            state.settings.bypass.files.remove(i);
            *dirty = true;
        }
        let full = state.settings.bypass.files.len() >= crate::settings::MAX_BYPASS_FILES;
        ui.add_space(2.0);
        let r = widgets::ghost_button(ui, Some(icons::PLUS), "Add .md file…", theme::FG, !full);
        if full {
            r.on_hover_text(format!("The list holds at most {} files.", crate::settings::MAX_BYPASS_FILES));
        } else if r.clicked() {
            commands.push(AppCommand::PickBypassFiles);
        }
    });
    ui.add_space(ROW_GAP);
}

/// Plugins section: one enable toggle per discovered plugin, then the MCP server status line
/// and a reconnect button.
///
/// Both controls push backend commands rather than editing settings here: discovery, hub
/// connection and the `settings.json` write all happen on the backend, and a toggle that only
/// changed the local copy would show a plugin as enabled while its hooks stayed unregistered.
fn plugins(ui: &mut Ui, state: &mut UiState, commands: &mut Vec<UiAction>, cx: &mut Ctx) {
    section(
        ui,
        "Plugins",
        "Skills, rules, hooks and MCP servers bundled in a plugin folder under .agents/plugins in the workspace or in the data folder. A plugin's hooks run only after you switch it on yourself.",
    );
    if state.plugins.is_empty() {
        egui::Frame::NONE.inner_margin(egui::Margin { left: 16, right: 16, top: 6, bottom: 6 }).show(ui, |ui| {
            ui.label(egui::RichText::new("No plugins found").font(theme::font(12.0)).color(theme::PLACEHOLDER));
        });
        ui.add_space(ROW_GAP);
    }
    let list = state.plugins.clone();
    for p in &list {
        let mut on = p.enabled;
        // Version first (if any), then the manifest description, then where it came from: the
        // source is what tells two same-named plugins apart.
        let mut parts: Vec<String> = Vec::new();
        if let Some(v) = p.version.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
            parts.push(format!("v{v}"));
        }
        if let Some(d) = p.description.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
            parts.push(d.to_owned());
        }
        parts.push(format!("from {}", p.source));
        if p.skill_count > 0 {
            parts.push(format!("{} skill{}", p.skill_count, if p.skill_count == 1 { "" } else { "s" }));
        }
        let desc = parts.join(" · ");
        let id = p.id.clone();
        row(ui, cx, icons::WRENCH, theme::ACCENT_BLUE, &p.name, &desc, |ui| {
            if widgets::toggle(ui, &mut on).changed() {
                let choice = if on { PluginChoice::Enabled } else { PluginChoice::Disabled };
                commands.push(AppCommand::SetPluginChoice(id.clone(), choice));
            }
        });
    }

    // Connected servers are listed one per line: the status is per server, and a single line
    // of many names is unreadable once a workspace has three or four.
    let status = state.mcp_status.clone();
    let summary = if status.is_empty() {
        "No MCP servers configured. Add mcp_config.json under .agents/ in the workspace.".to_owned()
    } else {
        status
            .iter()
            .map(|(name, connected, tools)| {
                if *connected {
                    format!("{name} ({tools} tool{})", if *tools == 1 { "" } else { "s" })
                } else {
                    format!("{name} (disconnected)")
                }
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    if cx.visible("MCP servers", &summary) {
        cx.shown += 1;
        egui::Frame::NONE.inner_margin(egui::Margin { left: 16, right: 16, top: 2, bottom: 6 }).show(ui, |ui| {
            ui.add(egui::Label::new(egui::RichText::new(summary).font(theme::font(12.0)).color(theme::FG_MUTED)).wrap());
            ui.add_space(4.0);
            if widgets::ghost_button(ui, Some(icons::ARROW_CLOCKWISE), "Reconnect MCP", theme::FG, true).clicked() {
                commands.push(AppCommand::ReloadMcp);
            }
        });
    }
}

/// Editable list of command rules: one chip row per rule with a remove button, then an input
/// that adds a rule on Enter or "Add". Invalid rules show the reason under the input.
fn rule_list(ui: &mut Ui, salt: &str, title: &str, desc: &str, hint: &str, rules: &mut Vec<String>) -> bool {
    let mut changed = false;
    let id = ui.make_persistent_id(("rule_input", salt));
    let (mut input, mut error): (String, Option<String>) = ui.data(|d| d.get_temp(id)).unwrap_or_default();
    egui::Frame::NONE.inner_margin(egui::Margin { left: 16, right: 16, top: 6, bottom: 6 }).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 4.0;
        ui.label(egui::RichText::new(title).font(theme::font(13.5)).color(theme::FG));
        ui.add(egui::Label::new(egui::RichText::new(desc).font(theme::font(12.0)).color(theme::FG_MUTED)).wrap());
        ui.add_space(4.0);
        let mut remove = None;
        if rules.is_empty() {
            ui.label(egui::RichText::new("No rules").font(theme::font(12.0)).color(theme::PLACEHOLDER));
        }
        for (i, r) in rules.iter().enumerate() {
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(Vec2::new((ui.available_width() - 32.0).max(120.0), 26.0), Sense::hover());
                ui.painter().rect_filled(rect, 4, theme::FIELD_BG);
                widgets::paint_truncated(ui, rect.shrink2(Vec2::new(9.0, 0.0)), r, theme::mono(12.0), theme::FG);
                if widgets::icon_button(ui, icons::X, 24.0, "Remove rule", true).clicked() {
                    remove = Some(i);
                }
            });
        }
        if let Some(i) = remove {
            rules.remove(i);
            changed = true;
        }
        ui.add_space(2.0);
        let full = rules.len() >= MAX_RULES;
        ui.horizontal(|ui| {
            let w = (ui.available_width() - 70.0).max(120.0);
            let r = widgets::text_field(ui, &mut input, w, hint, false);
            if r.changed() {
                error = None;
            }
            let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let add = widgets::ghost_button(ui, Some(icons::PLUS), "Add", theme::FG, !full && !input.trim().is_empty()).clicked();
            if (enter || add) && !full && !input.trim().is_empty() {
                let rule = input.trim().to_owned();
                match validate_rule(&rule) {
                    Err(e) => error = Some(e),
                    Ok(()) if rules.iter().any(|x| x.eq_ignore_ascii_case(&rule)) => error = Some("That rule is already in the list.".into()),
                    Ok(()) => {
                        rules.push(rule);
                        input.clear();
                        changed = true;
                    }
                }
            }
        });
        if let Some(e) = &error {
            ui.label(egui::RichText::new(e).font(theme::font(11.5)).color(theme::ERROR));
        } else if full {
            ui.label(egui::RichText::new(format!("The list holds at most {MAX_RULES} rules.")).font(theme::font(11.5)).color(theme::FG_MUTED));
        }
    });
    ui.data_mut(|d| d.insert_temp(id, (input, error)));
    changed
}

fn workspaces(ui: &mut Ui, state: &mut UiState, commands: &mut Vec<UiAction>, cx: &mut Ctx) {
    section(ui, "Workspaces", "Folders the agent has worked in. Every file path the agent uses stays inside the current workspace.");
    let list = state.settings.workspace_history.clone();
    let mut remove = None;
    if list.is_empty() {
        row(ui, cx, icons::FOLDER, theme::FG_MUTED, "No workspaces yet", "Open a folder to let the agent work in it.", |_| {});
    }
    for (i, ws) in list.iter().enumerate() {
        let current = state.workspace.as_deref() == Some(ws.as_path());
        let label = format::project_label(ws);
        let busy = state.busy;
        row(ui, cx, if current { icons::FOLDER_OPEN } else { icons::FOLDER }, if current { theme::ACCENT_BLUE } else { theme::FG_SECONDARY }, &label, &ws.display().to_string(), |ui| {
            if widgets::icon_button(ui, icons::X, 24.0, "Remove from list", !current).clicked() {
                remove = Some(i);
            }
            if widgets::icon_button(ui, icons::ARROW_SQUARE_OUT, 24.0, "Open in Explorer", true).clicked() {
                let _ = open::that_detached(ws);
            }
            if current {
                current_pill(ui);
            } else if widgets::ghost_button(ui, None, "Use", theme::FG, !busy).clicked() {
                commands.push(AppCommand::SetWorkspace(ws.clone()));
            }
        });
    }
    if let Some(i) = remove {
        state.settings.workspace_history.remove(i);
        state.settings_dirty = true;
    }
    ui.add_space(10.0);
    ui.horizontal(|ui| {
        ui.add_space(10.0);
        if widgets::ghost_button(ui, Some(icons::FOLDER_OPEN), "Open folder...", theme::FG, !state.busy).clicked() {
            commands.push(AppCommand::PickWorkspace);
        }
    });
}

/// The Account tab: the subscription accounts first, then the Gemini CLI sign-in.
///
/// The two Google sign-ins are easy to mix up, so the page keeps them apart. An Antigravity
/// account signs in with Antigravity's own OAuth client: it needs nothing set up, renews itself,
/// and is the sign-in the sidebar and Home offer. The Gemini CLI sign-in uses gemini-cli's client
/// and its secret, and stays here for whoever has that set up.
fn account(ui: &mut Ui, state: &mut UiState, commands: &mut Vec<UiAction>, cx: &mut Ctx) {
    accounts(ui, state, commands, cx);
    ui.add_space(22.0);
    gemini_cli_sign_in(ui, state, commands, cx);
}

/// What the Account tab says about using a consumer subscription from a third-party client.
const ACCOUNT_TERMS: &str = "These are consumer subscription sign-ins. The providers may restrict third-party clients: a refused request appears on the row, and an API key on the Providers tab is the alternative. Using a subscription this way may violate that provider's terms and put the account at risk.";

/// The order the Accounts section lists providers in. Antigravity leads: it is the sign-in the
/// rest of the app offers.
const ACCOUNT_PROVIDERS: [AccountProvider; 3] = [AccountProvider::Antigravity, AccountProvider::Codex, AccountProvider::ClaudeCode];

/// Tile colour of a provider's rows.
fn provider_accent(p: AccountProvider) -> Color32 {
    match p {
        AccountProvider::Antigravity => theme::ACCENT_BLUE,
        AccountProvider::Codex => theme::SUCCESS,
        AccountProvider::ClaudeCode => theme::ACCENT_RED,
    }
}

/// What may start or stop now, read once per frame before any row is drawn.
struct SignInGate {
    /// A turn runs: sign-ins, sign-outs and switches wait for it.
    busy: bool,
    /// The provider whose account sign-in runs.
    running: Option<AccountProvider>,
    /// That sign-in's attempt id, the only thing its Cancel can name.
    attempt: Option<u64>,
    /// The Gemini CLI sign-in runs. One sign-in at a time, whichever kind.
    google: bool,
}

impl SignInGate {
    fn of(state: &UiState) -> Self {
        Self {
            busy: state.busy,
            running: state.account_signing_in(),
            attempt: state.running_account_login().map(|l| l.id),
            google: matches!(state.auth, AuthStatus::LoggingIn),
        }
    }

    /// Why a sign-in cannot start now; `None` when it can. The backend refuses the same cases
    /// with the same words.
    fn blocked(&self) -> Option<&'static str> {
        if self.busy {
            Some("Stop the running turn first")
        } else if self.running.is_some() || self.google {
            Some("Finish or cancel the current sign-in first.")
        } else {
            None
        }
    }
}

/// The Accounts section: per provider (Antigravity, ChatGPT Codex, Claude Code), its accounts and
/// then its sign-in row, and the terms note under them.
///
/// Every button pushes a command and the backend answers with whole rows, so nothing here
/// guesses at a sign-in's outcome. Only the account in use is set locally at once (the "Current"
/// pill), as the composer's picker does; the backend's settings echo confirms it.
fn accounts(ui: &mut Ui, state: &mut UiState, commands: &mut Vec<UiAction>, cx: &mut Ctx) {
    section(ui, "Accounts", "Sign in with a subscription and use its models (Antigravity, ChatGPT Codex, Claude Code).");
    let gate = SignInGate::of(state);
    let views = state.accounts.clone();
    for p in ACCOUNT_PROVIDERS {
        // The sign-in placeholder (empty id) is drawn as the provider's own row, below.
        for v in views.iter().filter(|v| v.provider == p && !v.id.is_empty()) {
            match &v.status {
                AccountStatus::SignedIn => signed_in_row(ui, state, commands, cx, v, &gate),
                AccountStatus::Expired(_) | AccountStatus::Error(_) | AccountStatus::SignedOut => lapsed_row(ui, state, commands, cx, v, &gate),
                AccountStatus::SigningIn => {}
            }
        }
        sign_in_row(ui, state, commands, cx, p, &gate);
    }
    // No attempt is left for a pasted link to finish: the field's undo history goes with the text
    // (`UiState::apply` empties the text itself). Read out first, then forgotten.
    if state.account_login.is_none() {
        let key = paste_field_key();
        if let Some(field) = ui.data(|d| d.get_temp::<egui::Id>(key)) {
            forget_field(ui, field);
            ui.data_mut(|d| d.remove::<egui::Id>(key));
        }
    }
    if cx.visible("Subscription terms", ACCOUNT_TERMS) {
        cx.shown += 1;
        egui::Frame::NONE.inner_margin(egui::Margin { left: 16, right: 16, top: 0, bottom: 6 }).show(ui, |ui| {
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                ui.label(egui::RichText::new(icons::WARNING).font(theme::icon(13.0)).color(theme::WARNING));
                ui.add(egui::Label::new(egui::RichText::new(ACCOUNT_TERMS).font(theme::font(12.0)).color(theme::WARNING)).wrap());
            });
        });
    }
}

/// The "Sign in" button of a provider. Pressing it clears the provider's last error at once;
/// the backend's report of the new attempt follows.
fn sign_in_button(ui: &mut Ui, state: &mut UiState, commands: &mut Vec<UiAction>, p: AccountProvider, gate: &SignInGate) {
    let blocked = gate.blocked();
    let r = widgets::primary_button(ui, "Sign in", blocked.is_none());
    if let Some(why) = blocked {
        r.on_hover_text(why);
    } else if r.clicked() {
        state.account_errors.remove(&p);
        // A link pasted for an older attempt cannot finish this one.
        state.account_link_input.clear();
        commands.push(AppCommand::SignInAccount(p));
    }
}

/// A provider's own row: "Sign in", or the sign-in that runs with its Cancel. The last failure
/// shows under it until the next attempt, and an attempt that stopped can still be finished from
/// the address the browser landed on ("Paste redirect link").
fn sign_in_row(ui: &mut Ui, state: &mut UiState, commands: &mut Vec<UiAction>, cx: &mut Ctx, p: AccountProvider, gate: &SignInGate) {
    let label = p.label();
    let accent = provider_accent(p);
    if gate.running == Some(p) {
        let attempt = gate.attempt;
        row(ui, cx, icons::SIGN_IN, accent, label, "Finish the sign-in in your browser.", |ui| {
            // By the attempt's id: the row has no account id yet, and an id that no longer
            // matches cancels nothing.
            let r = widgets::ghost_button(ui, None, "Cancel", theme::FG, attempt.is_some());
            if let (true, Some(id)) = (r.clicked(), attempt) {
                commands.push(AppCommand::CancelAccountLogin(id));
            }
            ui.add(egui::Spinner::new().size(14.0).color(theme::FG_MUTED));
        });
        return;
    }
    let desc = format!("Sign in with your {label} account. Tokens are stored owner-only in the data folder.");
    let error = state.account_errors.get(&p).cloned();
    // Kept only while no sign-in runs: a new attempt lets the old one go.
    let kept = state.account_login.clone().filter(|l| l.provider == p && gate.running.is_none());
    let control = |ui: &mut Ui| {
        // Right to left: Sign in at the edge, the paste link beside it.
        sign_in_button(ui, state, commands, p, gate);
        if let Some(info) = &kept {
            paste_link_button(ui, state, commands, info);
        }
    };
    match error {
        None => row(ui, cx, icons::SIGN_IN, accent, label, &desc, control),
        Some(e) => row_with_body(ui, cx, icons::SIGN_IN, accent, label, &desc, control, |ui| {
            ui.add(egui::Label::new(egui::RichText::new(e).font(theme::font(12.0)).color(theme::ERROR)).wrap());
        }),
    }
}

/// "Paste redirect link" and its popup: the attempt's authorization page to open or copy, a
/// field for the address the browser landed on, Submit and Cancel.
///
/// The pasted text holds an authorization code. It goes to the backend and nowhere else, and the
/// field is emptied (undo history included) once it is sent or dismissed.
fn paste_link_button(ui: &mut Ui, state: &mut UiState, commands: &mut Vec<UiAction>, info: &AccountLoginInfo) {
    let r = widgets::ghost_button(ui, Some(icons::ARROW_CLOCKWISE), "Paste redirect link", theme::FG, true);
    account_models::popup(&r, 360.0, |ui| paste_popup(ui, state, commands, info));
}

/// egui memory key holding the paste field's id, so its state can be dropped once its attempt is
/// gone (the field's own id depends on where its popup was opened).
fn paste_field_key() -> egui::Id {
    egui::Id::unique("anticraft_account_paste_field")
}

fn paste_popup(ui: &mut Ui, state: &mut UiState, commands: &mut Vec<UiAction>, info: &AccountLoginInfo) {
    // Whether the text sent last could not be read, kept in egui memory. Read before drawing:
    // nothing is drawn inside a memory accessor.
    let bad_id = ui.make_persistent_id(("paste_bad", info.id));
    let mut bad: bool = ui.data(|d| d.get_temp(bad_id)).unwrap_or(false);
    ui.spacing_mut().item_spacing.y = 6.0;
    ui.add(egui::Label::new(egui::RichText::new("Open this link in the browser, then paste the address it landed on.").font(theme::font(12.0)).color(theme::FG)).wrap());
    ui.horizontal(|ui| {
        let w = (ui.available_width() - 90.0).max(120.0);
        let (rect, r) = ui.allocate_exact_size(Vec2::new(w, 28.0), Sense::hover());
        widgets::paint_truncated(ui, rect, &info.auth_url, theme::font(11.5), theme::FG_MUTED);
        r.on_hover_text(&info.auth_url);
        if widgets::ghost_button(ui, Some(icons::COPY), "Copy", theme::FG, true).clicked() {
            ui.ctx().copy_text(info.auth_url.clone());
        }
    });
    let field = widgets::text_field(ui, &mut state.account_link_input, 320.0, "Paste the redirect URL", false);
    let key = paste_field_key();
    ui.data_mut(|d| d.insert_temp(key, field.id));
    if field.changed() {
        bad = false;
    }
    let enter = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
    if bad {
        ui.label(egui::RichText::new(crate::auth::oauth::BAD_PASTE_MESSAGE).font(theme::font(11.5)).color(theme::ERROR));
    }
    let ready = !state.account_link_input.trim().is_empty();
    let (submit, cancel) = ui
        .horizontal(|ui| {
            let submit = widgets::primary_button(ui, "Submit", ready).clicked();
            let cancel = widgets::ghost_button(ui, None, "Cancel", theme::FG, true).clicked();
            (submit, cancel)
        })
        .inner;
    if ready && (submit || enter) {
        // Read here first, with the backend's own parser: text that is no redirect link would
        // only come back as the same message, after using up the attempt's turn.
        if crate::auth::oauth::parse_pasted_link(&state.account_link_input) == crate::auth::oauth::PastedLink::Unreadable {
            bad = true;
        } else {
            let link = std::mem::take(&mut state.account_link_input);
            commands.push(AppCommand::PasteAccountLink { login: info.id, link: link.into() });
            forget_field(ui, field.id);
            bad = false;
            ui.close();
        }
    } else if cancel {
        state.account_link_input.clear();
        commands.push(AppCommand::ForgetAccountLogin(info.id));
        forget_field(ui, field.id);
        bad = false;
        ui.close();
    }
    ui.data_mut(|d| d.insert_temp(bad_id, bad));
}

/// A signed-in account: who it is, what it sends, and Models / Use / Sign out.
fn signed_in_row(ui: &mut Ui, state: &mut UiState, commands: &mut Vec<UiAction>, cx: &mut Ctx, v: &AccountView, gate: &SignInGate) {
    let name = account_models::account_name(state, v);
    let label = match v.plan.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
        Some(plan) => format!("{name} · {plan}"),
        None => name,
    };
    // The model settings hold for it: what a pick just changed, before the backend's rows say so.
    let model = state.settings.accounts.iter().find(|e| e.id == v.id).map(|e| e.model.trim()).filter(|m| !m.is_empty()).unwrap_or(v.model.trim()).to_owned();
    let desc = format!("{} · {model}", v.provider.label());
    let active = state.settings.active_account_entry().is_some_and(|e| e.id == v.id);
    let file = format!("File: accounts/{}.json", v.id);
    let checked = v.models_fetched_at.map(|t| format!("Model list last checked {}", format::clock(t)));
    // A sign-in of the same provider can write this very account's file (signing in again renews
    // it in place); the backend refuses a sign-out meanwhile, so the button says so first.
    let renewing = gate.running == Some(v.provider);
    let busy = gate.busy;
    row_with_body(
        ui,
        cx,
        login::provider_icon(v.provider),
        provider_accent(v.provider),
        &label,
        &desc,
        |ui| {
            // Right to left: Sign out at the edge, then Use / Current, then Models.
            let r = widgets::ghost_button(ui, Some(icons::SIGN_OUT), "Sign out", theme::ERROR, !busy && !renewing);
            if busy {
                r.on_hover_text("Stop the running turn first");
            } else if renewing {
                r.on_hover_text("Finish or cancel the current sign-in first.");
            } else if r.clicked() {
                commands.push(AppCommand::SignOutAccount(v.id.clone()));
            }
            if active {
                current_pill(ui);
            } else {
                let r = widgets::ghost_button(ui, None, "Use", theme::FG, !busy);
                if busy {
                    r.on_hover_text("Stop the running turn first");
                } else if r.clicked() {
                    state.settings.select_account(&v.id);
                    commands.push(AppCommand::UseAccount(v.id.clone()));
                }
            }
            account_models::models_button(ui, state, commands, v);
        },
        |ui| {
            ui.label(egui::RichText::new(&file).font(theme::font(11.5)).color(theme::FG_MUTED));
            if let Some(c) = &checked {
                ui.label(egui::RichText::new(c).font(theme::font(11.5)).color(theme::FG_MUTED));
            }
        },
    );
}

/// An account settings still list whose session ended: a refresh found it dead (`Expired`), or
/// it has no token source. Signing in again is the way back.
fn lapsed_row(ui: &mut Ui, state: &mut UiState, commands: &mut Vec<UiAction>, cx: &mut Ctx, v: &AccountView, gate: &SignInGate) {
    let name = account_models::account_name(state, v);
    let (label, desc) = match &v.status {
        AccountStatus::Expired(reason) => (format!("{name} · Session expired"), reason.clone()),
        AccountStatus::Error(e) => (format!("{name} · Signed out"), e.clone()),
        _ => (format!("{name} · Signed out"), "Sign in again to use this account.".to_owned()),
    };
    let p = v.provider;
    warning_row(ui, cx, icons::WARNING, &label, &desc, |ui| sign_in_button(ui, state, commands, p, gate));
}

/// The Gemini CLI sign-in (Google, through gemini-cli's OAuth client), the session another Google
/// tool saved, the Cloud project, and that client's secret.
fn gemini_cli_sign_in(ui: &mut Ui, state: &mut UiState, commands: &mut Vec<UiAction>, cx: &mut Ctx) {
    let mut dirty = false;
    let not_configured = state.oauth_not_configured();
    section(
        ui,
        "Gemini CLI sign-in (Google)",
        "Google Code Assist through gemini-cli's OAuth client, which is not Antigravity's, so it needs that client's secret (OAuth client, below).",
    );
    // A reused session shows as signed in as well; this row is about AntiCraft's own sign-in.
    let reused = state.imported_session.is_some();
    let auth = match &state.auth {
        AuthStatus::LoggedIn { .. } if reused => AuthStatus::LoggedOut,
        other => other.clone(),
    };
    // Turns go to the account or the provider profile in use, which needs no Google account.
    let optional = match state.settings.active_account_entry() {
        Some(a) => Some(format!("Not needed while the {} account {} is in use.", a.provider.label(), a.label.trim())),
        None => state.settings.active_provider_profile().map(|p| format!("Not needed while {} is in use.", p.label.trim())),
    };
    // One sign-in at a time: the backend lets this one start beside an account's, but two
    // browser pages at once is a question nobody should have to answer.
    let account_running = state.account_signing_in().is_some();
    let (label, desc) = match &auth {
        AuthStatus::LoggedIn { email } => (email.clone().unwrap_or_else(|| "Google account".into()), "Signed in".to_owned()),
        AuthStatus::LoggingIn => ("Signing in...".into(), "Finish the sign-in in your browser.".into()),
        AuthStatus::LoggedOut if reused => (
            "Not signed in with AntiCraft".into(),
            "Code Assist uses the session below. Sign in to use AntiCraft's own Google sign-in, which renews itself.".into(),
        ),
        AuthStatus::LoggedOut => ("Not signed in".into(), optional.unwrap_or_else(|| "Sign in to start conversations.".into())),
        AuthStatus::Error(_) if not_configured => {
            ("Not signed in".into(), optional.unwrap_or_else(|| "Sign-in needs the OAuth client secret described below.".into()))
        }
        AuthStatus::Error(e) => ("Not signed in".into(), e.clone()),
    };
    row(ui, cx, icons::USER_CIRCLE, theme::ACCENT_BLUE, &label, &desc, |ui| match auth {
        AuthStatus::LoggedIn { .. } => {
            if widgets::ghost_button(ui, Some(icons::SIGN_OUT), "Sign out", theme::ERROR, true).clicked() {
                commands.push(AppCommand::Logout);
            }
        }
        AuthStatus::LoggingIn => {
            if widgets::ghost_button(ui, None, "Cancel", theme::FG, true).clicked() {
                commands.push(AppCommand::CancelLogin);
            }
            ui.add(egui::Spinner::new().size(14.0).color(theme::FG_MUTED));
        }
        _ => {
            let r = widgets::primary_button(ui, "Sign in", !not_configured && !account_running);
            if not_configured {
                r.on_hover_text("Set the OAuth client secret below first.");
            } else if account_running {
                r.on_hover_text("Finish or cancel the current sign-in first.");
            } else if r.clicked() {
                commands.push(AppCommand::Login);
            }
        }
    });
    // An Antigravity account signed in here renews itself; a session imported from the
    // Antigravity app cannot. One Antigravity row is enough, so the import row goes, unless that
    // session is in use: its "Stop using" must stay reachable.
    let antigravity = state.signed_in_accounts().any(|v| v.provider == AccountProvider::Antigravity);
    if !antigravity || state.imported_session.is_some() {
        existing_session(ui, state, commands, cx);
    }
    row(ui, cx, icons::DESKTOP, theme::FG_SECONDARY, "Google Cloud project", "Optional. Leave empty to use the project Code Assist assigns.", |ui| {
        let mut p = state.settings.gcp_project.clone().unwrap_or_default();
        if widgets::text_field(ui, &mut p, 200.0, "project-id", false).changed() {
            state.settings.gcp_project = Some(p.trim().to_owned()).filter(|p| !p.is_empty());
            dirty = true;
        }
    });
    ui.add_space(22.0);
    section(
        ui,
        "OAuth client",
        "The secret of gemini-cli's installed-app OAuth client, used by the Gemini CLI sign-in only: the Antigravity sign-in needs none. The value is never shown or written to settings.json or logs.",
    );
    let desc = match state.oauth_source {
        Some(ClientSource::File) => "Saved in oauth_client.json in the data folder (owner-only).".to_owned(),
        Some(ClientSource::Env) => format!("Set via {CLIENT_SECRET_ENV}."),
        Some(ClientSource::BuiltIn) => "Set at build time.".to_owned(),
        None if not_configured => format!(
            "Missing, so the Gemini CLI sign-in is off. It needs the public client secret of gemini-cli's installed-app OAuth \
             client: the OAUTH_CLIENT_SECRET value in gemini-cli's source (packages/core/src/code_assist/oauth2.ts). Paste it \
             below, or set {CLIENT_SECRET_ENV} when building or starting AntiCraft."
        ),
        None => "Configured.".to_owned(),
    };
    let (icon, accent, status) = if not_configured { (icons::WARNING, theme::ERROR, "Missing") } else { (icons::CHECK_CIRCLE, theme::SUCCESS, "Configured") };
    row(ui, cx, icon, accent, "Client secret status", &desc, |ui| {
        ui.label(egui::RichText::new(status).font(theme::font(12.0)).color(accent));
    });
    let has_file = state.oauth_source == Some(ClientSource::File);
    let busy = state.busy;
    row(ui, cx, icons::SIGN_IN, theme::ACCENT_AMBER, "Enter client secret", "Stored only in this Windows account's data folder. Press Enter or Save.", |ui| {
        if has_file && widgets::ghost_button(ui, None, "Clear", theme::ERROR, !busy).clicked() {
            commands.push(AppCommand::ClearOAuthClientSecret);
        }
        let ready = !state.secret_input.trim().is_empty() && !busy;
        let save_clicked = widgets::primary_button(ui, "Save", ready).clicked();
        // Narrower beside "Clear", so the controls stay out of the row's description.
        let r = widgets::text_field(ui, &mut state.secret_input, if has_file { 130.0 } else { 200.0 }, "Client secret", true);
        let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if ready && (save_clicked || enter) {
            commands.push(AppCommand::SetOAuthClientSecret(std::mem::take(&mut state.secret_input)));
            // Drop the field's TextEditState (its undo history holds the typed secret) and focus.
            ui.ctx().data_mut(|d| d.remove::<egui::text_edit::TextEditState>(r.id));
            ui.memory_mut(|m| m.surrender_focus(r.id));
        }
    });
    state.settings_dirty |= dirty;
}

/// The row under the Google sign-in row: the session another Google tool saved on this computer
/// (what the backend's last look found), or the one in use. Only token-free summaries reach the
/// UI; the backend reads the files.
fn existing_session(ui: &mut Ui, state: &mut UiState, commands: &mut Vec<UiAction>, cx: &mut Ctx) {
    let now = chrono::Utc::now();
    if let Some(info) = &state.imported_session {
        let (title, desc) = (info.title(), info.describe(now));
        row(ui, cx, icons::CHECK_CIRCLE, theme::SUCCESS, &title, &desc, |ui| {
            if widgets::ghost_button(ui, Some(icons::SIGN_OUT), "Stop using", theme::ERROR, true).clicked() {
                commands.push(AppCommand::StopImportedSession);
            }
        });
        return;
    }
    let found = match &state.import_check {
        Some(ImportCheck::Found(s)) => Some(s.clone()),
        _ => None,
    };
    let (icon, accent, label, mut desc) = match &state.import_check {
        None => (
            icons::CLOCK_COUNTER_CLOCKWISE,
            theme::FG_MUTED,
            "Existing session".to_owned(),
            "Looking for a Google session another tool saved on this computer...".to_owned(),
        ),
        Some(ImportCheck::NotFound) => (
            icons::INFO,
            theme::FG_SECONDARY,
            "No existing session found".to_owned(),
            "AntiCraft can reuse the Google sign-in that the Gemini CLI or Antigravity saved in the .gemini folder of your \
             user folder, but there is none. Sign in with Google above instead."
                .to_owned(),
        ),
        Some(ImportCheck::Unreadable(files)) => (
            icons::WARNING,
            theme::WARNING,
            "Existing session unreadable".to_owned(),
            format!(
                "No access token AntiCraft can read in {}. Sign in with Google above instead.",
                files.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
            ),
        ),
        Some(ImportCheck::Found(s)) if s.usable => (icons::CHECK_CIRCLE, theme::SUCCESS, s.title(), s.explain(now)),
        Some(ImportCheck::Found(s)) => (icons::WARNING, theme::WARNING, s.title(), s.explain(now)),
    };
    let usable = found.as_ref().is_some_and(|s| s.usable);
    let own = state.own_sign_in();
    if usable && own {
        desc.push_str(" AntiCraft's own sign-in is in use and takes precedence.");
    }
    let signing_in = matches!(state.auth, AuthStatus::LoggingIn);
    let ended = state.import_ended.clone();
    let mut check = false;
    let control = |ui: &mut Ui| {
        // Right to left: "Check again" at the edge, "Use this session" beside it.
        if widgets::ghost_button(ui, Some(icons::ARROW_CLOCKWISE), "Check again", theme::FG, true).clicked() {
            check = true;
        }
        if usable && !own {
            let r = widgets::primary_button(ui, "Use this session", !signing_in);
            if signing_in {
                r.on_hover_text("Finish or cancel the Google sign-in first.");
            } else if r.clicked() {
                commands.push(AppCommand::UseImportedSession);
            }
        }
    };
    if ended.is_none() && found.is_none() {
        row(ui, cx, icon, accent, &label, &desc, control);
    } else {
        row_with_body(ui, cx, icon, accent, &label, &desc, control, |ui| {
            if let Some(reason) = &ended {
                ui.add(egui::Label::new(egui::RichText::new(reason).font(theme::font(12.0)).color(theme::WARNING)).wrap());
            }
            if let Some(s) = &found {
                ui.label(egui::RichText::new(format!("File: {}", s.source.display())).font(theme::font(11.5)).color(theme::FG_MUTED));
            }
        });
    }
    if check {
        state.import_ended = None;
        commands.push(AppCommand::CheckImportedSession);
    }
}

/// "Current" pill shown in place of the "Use" button of the workspace or backend in use.
fn current_pill(ui: &mut Ui) {
    let g = ui.painter().layout_no_wrap("Current".into(), theme::font(11.5), theme::FG_SECONDARY);
    let (rect, _) = ui.allocate_exact_size(g.size() + Vec2::new(14.0, 6.0), Sense::hover());
    ui.painter().rect_filled(rect, rect.height() / 2.0, theme::SECONDARY);
    ui.painter().galley(rect.center() - g.size() / 2.0, g, theme::FG_SECONDARY);
}

/// Width of the label column of the provider dialog's fields.
const FIELD_LABEL_W: f32 = 64.0;
/// Width of the provider dialog's content.
const DIALOG_W: f32 = 470.0;
/// Distance from the window's top to the provider dialog's.
const DIALOG_TOP: f32 = 90.0;
/// Tallest the dialog's model list grows before it scrolls.
const DIALOG_LIST_H: f32 = 180.0;
/// Pitch of a row in the dialog's model list.
const DIALOG_ROW_H: f32 = 24.0;

/// The providers tab: the configured API-key profiles, each with Edit, Delete and Use, then
/// "Add provider". Adding and editing happen in a dialog ([`provider_dialog`]), which also loads
/// the models a provider offers; one is picked there or later from the composer's model menu.
///
/// Google Code Assist is not listed: it is the Google sign-in's backend (Account tab), and the
/// composer's model menu switches to it while that sign-in works.
///
/// Profiles live in `settings.providers` and the one in use in `settings.active_provider`.
/// Every change, the selection included, edits the local copy and marks `settings_dirty`, so
/// a single `SaveSettings` carries the list and the selection together and the backend's
/// sanitize pass sees what the UI shows. (Sending `SetActiveProvider` beside it raced: the
/// backend refused a profile it had not received yet, and a stale `SaveSettings` could undo
/// the choice.) Clicks are collected as [`ProviderAction`]s and applied by id after the rows
/// are drawn. The only command sent is the dialog's "Load models".
fn providers(ui: &mut Ui, state: &mut UiState, commands: &mut Vec<UiAction>, cx: &mut Ctx) {
    section(
        ui,
        "Model providers",
        "Gemini Developer API, OpenAI-compatible and Anthropic endpoints with an API key. Load a provider's models in its dialog, then pick one there or from the model menu in the composer. API keys are saved in settings.json in the data folder.",
    );
    let busy = state.busy;
    let any_click = ui.ctx().input(|i| i.pointer.any_click());
    let mut armed_now = state.provider_delete_armed.take();
    let mut actions: Vec<ProviderAction> = Vec::new();
    let mut delete_clicked = false;
    let mut edit: Option<ProviderDialog> = None;

    for profile in &state.settings.providers {
        let active = state.settings.active_provider.as_deref() == Some(profile.id.as_str());
        let armed = armed_now.as_deref() == Some(profile.id.as_str());
        let (mut pick, mut del, mut open) = (false, false, false);
        row(ui, cx, icons::BRAIN, if active { theme::ACCENT_BLUE } else { theme::FG_SECONDARY }, &profile.label, &profile_desc(profile), |ui| {
            // Right to left: "Use"/"Current" at the edge, so the selection lines up in one
            // column whatever the Delete label says, then Delete, then Edit.
            if active {
                current_pill(ui);
            } else {
                let r = widgets::ghost_button(ui, None, "Use", theme::FG, !busy);
                pick = if busy { r.on_hover_text("Stop the running turn first").clicked() } else { r.clicked() };
            }
            let label = if armed { "Click again to delete" } else { "Delete" };
            let r = widgets::ghost_button(ui, None, label, theme::ERROR, !busy);
            del = if busy { r.on_hover_text("Stop the running turn first").clicked() } else { r.clicked() };
            open = widgets::ghost_button(ui, Some(icons::PENCIL_SIMPLE), "Edit", theme::FG, true).clicked();
        });
        if pick {
            actions.push(ProviderAction::Select(Some(profile.id.clone())));
        }
        if del {
            delete_clicked = true;
            if armed {
                actions.push(ProviderAction::Delete(profile.id.clone()));
                armed_now = None;
            } else {
                armed_now = Some(profile.id.clone());
            }
        }
        if open {
            edit = Some(ProviderDialog::edit(profile));
        }
    }

    if cx.visible("Add provider", "Gemini Developer API, OpenAI-compatible or Anthropic endpoint with an API key") {
        cx.shown += 1;
        if state.settings.providers.is_empty() {
            ui.horizontal(|ui| {
                ui.add_space(16.0);
                ui.label(egui::RichText::new("No providers yet.").font(theme::font(12.0)).color(theme::FG_MUTED));
            });
            ui.add_space(8.0);
        }
        let full = state.settings.providers.len() >= MAX_PROVIDER_PROFILES;
        ui.horizontal(|ui| {
            ui.add_space(16.0);
            let r = widgets::ghost_button(ui, Some(icons::PLUS), "Add provider", theme::FG, !full);
            if full {
                r.on_hover_text(format!("The list holds at most {MAX_PROVIDER_PROFILES} profiles."));
            } else if r.clicked() {
                // A second profile of the same kind is common: start on the last one's.
                let kind = state.settings.providers.last().map_or(ProviderKind::GeminiApi, |p| p.kind);
                edit = Some(ProviderDialog::add(kind));
            }
        });
    }
    if let Some(d) = edit {
        state.provider_dialog = Some(d);
    }

    actions.extend(provider_dialog(ui, state, commands));

    let changed = apply_actions(&mut state.settings, &actions);
    // Any other click disarms a pending delete, as closing the menu does in the title bar.
    if (any_click && !delete_clicked) || armed_now.as_deref().is_some_and(|id| !state.settings.providers.iter().any(|p| p.id == id)) {
        armed_now = None;
    }
    state.provider_delete_armed = armed_now;
    state.settings_dirty |= changed;
}

/// Row description of a profile: kind, model (or that none is chosen), and the base URL when
/// one is set.
fn profile_desc(profile: &ProviderProfile) -> String {
    let model = match profile.model.trim() {
        "" if profile.models.is_empty() => "no model chosen".to_owned(),
        "" => format!("no model chosen ({} loaded)", profile.models.len()),
        m => m.to_owned(),
    };
    let mut desc = format!("{} · {model}", profile.kind.label());
    if let Some(url) = profile.base_url.as_deref().map(str::trim).filter(|u| !u.is_empty()) {
        desc.push_str(" · ");
        desc.push_str(url);
    }
    desc
}

/// Open `id`'s dialog from outside the tab (the composer's "Load models…"): the Providers tab is
/// shown and its models start loading.
pub fn edit_provider(state: &mut UiState, commands: &mut Vec<UiAction>, id: &str) {
    let Some(profile) = state.settings.providers.iter().find(|p| p.id == id).cloned() else { return };
    state.open_customize(CustomizeTab::Providers);
    let mut d = ProviderDialog::edit(&profile);
    start_load(&mut state.provider_models_seq, commands, &mut d, profile);
    state.provider_dialog = Some(d);
}

/// Ask the backend for `target`'s models; only the answer numbered as this request is taken.
fn start_load(seq: &mut u64, commands: &mut Vec<UiAction>, d: &mut ProviderDialog, target: ProviderProfile) {
    *seq += 1;
    d.loading = Some(*seq);
    d.load_error = None;
    commands.push(AppCommand::LoadProviderModels { request: *seq, profile: target });
}

/// The saved key of the profile `d` edits, if it has one.
fn saved_key<'a>(d: &ProviderDialog, existing: &'a [ProviderProfile]) -> Option<&'a str> {
    let id = d.editing.as_deref()?;
    existing.iter().find(|p| p.id == id).map(|p| p.api_key.as_str()).filter(|k| !k.trim().is_empty())
}

/// What "Load models" lists from: the dialog's endpoint, with the typed key or else the saved one.
fn load_target(d: &ProviderDialog, existing: &[ProviderProfile]) -> ProviderProfile {
    let key = match d.api_key.trim() {
        "" => saved_key(d, existing).unwrap_or_default().trim().to_owned(),
        k => k.to_owned(),
    };
    ProviderProfile {
        id: d.editing.clone().unwrap_or_else(|| "new".into()),
        label: d.kind.label().to_owned(),
        kind: d.kind,
        model: String::new(),
        base_url: Some(d.base_url.trim().to_owned()).filter(|u| !u.is_empty()),
        api_key: key,
        max_output_tokens: None,
        context_window: None,
        models: Vec::new(),
    }
}

/// The Add / Edit provider dialog: type, label, base URL and key, then the models the provider
/// lists ("Load models") and, optionally, the one new turns start on. Save applies it to the
/// list; Cancel, the close button and Esc leave the list as it was. A click outside does not
/// close it: a typed key would go with it.
fn provider_dialog(ui: &mut Ui, state: &mut UiState, commands: &mut Vec<UiAction>) -> Vec<ProviderAction> {
    let mut out = Vec::new();
    let Some(mut d) = state.provider_dialog.take() else { return out };
    let existing = &state.settings.providers;
    // The edited profile is gone (the backend dropped it): nothing is left to edit.
    if d.editing.as_ref().is_some_and(|id| !existing.iter().any(|p| p.id == *id)) {
        return out;
    }
    let has_saved_key = saved_key(&d, existing).is_some();
    let (mut close, mut save, mut load) = (false, false, false);
    let mut key_field = None;
    let frame = widgets::card(theme::RADIUS_POPUP, 18).shadow(ui.ctx().global_style().visuals.popup_shadow);
    // Hung from the top, not centered: a centered dialog moves up whenever it grows (a list that
    // loads, a message), a frame after it grew, under the pointer of a click aimed at a row.
    let id = egui::Id::unique("anticraft_provider_dialog");
    let area = egui::Modal::default_area(id).anchor(Align2::CENTER_TOP, Vec2::new(0.0, DIALOG_TOP));
    let modal = egui::Modal::new(id).area(area).frame(frame).show(ui.ctx(), |ui| {
        ui.set_width(DIALOG_W);
        ui.spacing_mut().item_spacing.y = 6.0;
        ui.horizontal(|ui| {
            let title = if d.editing.is_some() { "Edit provider" } else { "Add provider" };
            ui.label(egui::RichText::new(title).font(theme::medium(15.0)).color(theme::FG));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                close |= widgets::icon_button(ui, icons::X, 14.0, "Close", true).clicked();
            });
        });
        ui.add_space(4.0);

        let mut edited = false;
        let mut endpoint = false;
        if field_row(ui, "Type", "kind", |ui| widgets::segmented(ui, &mut d.kind, &KIND_LABELS)) {
            // The previous kind's endpoint, and the list it gave, rarely fit the new one.
            d.base_url.clear();
            d.models.clear();
            d.models_from = None;
            d.load_error = None;
            endpoint = true;
        }
        let kind = d.kind;
        let fw = DIALOG_W - field_offset(ui);
        edited |= field_row(ui, "Label", "label", |ui| widgets::text_field(ui, &mut d.label, fw, kind.label(), false)).changed();
        endpoint |= field_row(ui, "Base URL", "base_url", |ui| widgets::text_field(ui, &mut d.base_url, fw, kind.default_base_url(), false)).changed();
        let hint = if has_saved_key { "Leave empty to keep the saved key" } else { "API key" };
        let key = field_row(ui, "API key", "api_key", |ui| widgets::text_field(ui, &mut d.api_key, fw, hint, true));
        endpoint |= key.changed();
        key_field = Some(key.id);
        if endpoint {
            // An answer about the endpoint as it was is not this one's.
            d.loading = None;
        }

        ui.add_space(8.0);
        let can_load = has_saved_key || !d.api_key.trim().is_empty();
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Models").font(theme::medium(13.0)).color(theme::FG));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if d.loading.is_some() {
                    widgets::ghost_button(ui, None, "Loading…", theme::FG, false);
                    ui.add(egui::Spinner::new().size(14.0).color(theme::FG_MUTED));
                } else {
                    let r = widgets::ghost_button(ui, Some(icons::ARROW_CLOCKWISE), "Load models", theme::FG, can_load);
                    if !can_load {
                        r.on_hover_text("Enter the API key first");
                    } else if r.clicked() {
                        load = true;
                    }
                }
            });
        });
        if let Some(e) = &d.load_error {
            ui.add(egui::Label::new(egui::RichText::new(e).font(theme::font(12.0)).color(theme::ERROR)).wrap());
        } else if d.loading.is_none() && !d.models.is_empty() {
            let line = match &d.models_from {
                Some(from) => format!("{} models · {from}", d.models.len()),
                None => format!("{} models, loaded earlier", d.models.len()),
            };
            ui.add(egui::Label::new(egui::RichText::new(line).font(theme::font(12.0)).color(theme::FG_MUTED)).wrap());
        }
        if field_row(ui, "Model", "model", |ui| widgets::text_field(ui, &mut d.model, fw, "Optional: pick below or type an id", false)).changed() {
            d.filtering = true;
            edited = true;
        }
        if let Some(m) = model_list(ui, &d) {
            d.model = m;
            d.filtering = false;
            edited = true;
        }

        if edited || endpoint {
            d.error = None;
        }
        if let Some(e) = &d.error {
            ui.add(egui::Label::new(egui::RichText::new(e).font(theme::font(12.0)).color(theme::ERROR)).wrap());
        }
        ui.add_space(6.0);
        // A new profile needs its key; an edited one keeps the saved key when the field is empty.
        let ready = d.editing.is_some() || !d.api_key.trim().is_empty();
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let r = widgets::primary_button(ui, "Save", ready);
                if !ready {
                    r.on_hover_text("Enter the API key first");
                } else if r.clicked() {
                    save = true;
                }
                close |= widgets::ghost_button(ui, None, "Cancel", theme::FG, true).clicked();
            });
        });
    });
    // Esc closes as Cancel does, unless a popup inside the dialog has it.
    if modal.is_top_modal && !modal.any_popup_open && ui.ctx().input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
        close = true;
    }

    if load && !close {
        let target = load_target(&d, existing);
        start_load(&mut state.provider_models_seq, commands, &mut d, target);
    }
    if save && !close {
        match dialog_profile(&d, existing) {
            Ok(profile) => {
                out.push(if d.editing.is_some() { ProviderAction::Replace(profile) } else { ProviderAction::Add(profile) });
                close = true;
            }
            Err(e) => d.error = Some(e),
        }
    }
    if close {
        // The key field's undo history holds the typed key.
        if let Some(id) = key_field {
            forget_field(ui, id);
        }
    } else {
        state.provider_dialog = Some(d);
    }
    out
}

/// The dialog's models, narrowed by the Model field while it is typed in, the chosen one
/// checked. Returns the one clicked.
///
/// The filter changes only with typing and with a pick, never with a click's press: the rows
/// must stay where they were between the press and the release of the click that picks one.
fn model_list(ui: &mut Ui, d: &ProviderDialog) -> Option<String> {
    let typed = d.model.trim();
    let filter = if d.filtering { typed.to_ascii_lowercase() } else { String::new() };
    let shown: Vec<&String> = d.models.iter().filter(|m| filter.is_empty() || m.to_ascii_lowercase().contains(&filter)).collect();
    let note = |ui: &mut Ui, text: &str| {
        ui.horizontal(|ui| {
            ui.add_space(field_offset(ui));
            ui.add(egui::Label::new(egui::RichText::new(text).font(theme::font(12.0)).color(theme::FG_MUTED)).wrap());
        });
    };
    if d.models.is_empty() {
        note(ui, "Load the provider's models to pick one here. Without a model, pick one later from the model menu in the composer.");
        return None;
    }
    if shown.is_empty() {
        note(ui, "No loaded model matches. Save to use the id as typed.");
        return None;
    }
    let mut picked = None;
    let indent = field_offset(ui);
    // Indented by a margin, not by a horizontal row: inside one, the rows would inherit its
    // left-to-right layout (side by side, each after the first squeezed to nothing), and the
    // list would get the row's height to scroll in instead of its own.
    let margin = egui::Margin { left: indent.round() as i8, ..egui::Margin::ZERO };
    egui::Frame::NONE.inner_margin(margin).show(ui, |ui| {
        egui::Frame::new().fill(theme::FIELD_BG).stroke(Stroke::new(1.0, theme::CARD_BORDER)).corner_radius(6).inner_margin(3).show(ui, |ui| {
            ui.set_width(DIALOG_W - indent - 8.0);
            egui::ScrollArea::vertical().id_salt("provider_models").max_height(DIALOG_LIST_H).auto_shrink([false, true]).show_rows(ui, DIALOG_ROW_H, shown.len(), |ui, range| {
                ui.spacing_mut().item_spacing.y = 0.0;
                for m in &shown[range] {
                    let selected = m.as_str() == typed;
                    let (rect, r) = ui.allocate_exact_size(Vec2::new(ui.available_width(), DIALOG_ROW_H), Sense::click());
                    if selected || r.hovered() {
                        ui.painter().rect_filled(rect, 4, theme::SECONDARY);
                    }
                    let text = Rect::from_min_max(egui::pos2(rect.left() + 8.0, rect.top()), egui::pos2(rect.right() - 26.0, rect.bottom()));
                    widgets::paint_truncated(ui, text, m, theme::font(12.5), if selected || r.hovered() { theme::FG } else { theme::FG_SECONDARY });
                    if selected {
                        ui.painter().text(egui::pos2(rect.right() - 9.0, rect.center().y), Align2::RIGHT_CENTER, icons::CHECK, theme::icon(12.0), theme::FG);
                    }
                    if r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        picked = Some((*m).clone());
                    }
                }
            });
        });
    });
    picked
}

/// One field row: a fixed-width muted label, then the field.
///
/// The field is scoped by `salt`: `widgets::text_field` derives its id from the hint, and a
/// hint that follows the provider kind must not reset the field's state.
fn field_row<R>(ui: &mut Ui, label: &str, salt: &str, add: impl FnOnce(&mut Ui) -> R) -> R {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::new(FIELD_LABEL_W, 28.0), Sense::hover());
        ui.painter().text(rect.left_center(), Align2::LEFT_CENTER, label, theme::font(11.5), theme::FG_MUTED);
        ui.push_id(salt, add).inner
    })
    .inner
}

/// Distance from a field row's left edge to its field.
fn field_offset(ui: &Ui) -> f32 {
    FIELD_LABEL_W + ui.spacing().item_spacing.x
}

/// Forget a text field: its stored state (cursor and undo history) and its focus.
fn forget_field(ui: &Ui, id: egui::Id) {
    ui.ctx().data_mut(|d| d.remove::<egui::text_edit::TextEditState>(id));
    ui.memory_mut(|m| m.surrender_focus(id));
}

/// One change to the profile list or the selection, collected while the tab is drawn and
/// applied by id afterwards ([`apply_actions`]), so a deleted row never shifts another
/// row's index mid-frame.
///
/// No `Debug`: a profile carries a key.
enum ProviderAction {
    /// A new profile from [`dialog_profile`].
    Add(ProviderProfile),
    /// An edited profile from [`dialog_profile`], replacing the one with its id.
    Replace(ProviderProfile),
    Delete(String),
    /// Backend for new turns: a profile id, or `None` for Google Code Assist.
    Select(Option<String>),
}

/// Apply `actions` in order and return whether `settings` changed.
///
/// Profiles are validated again here, so every profile in the list is one
/// `Settings::sanitized` keeps: the backend never drops a profile the UI shows. Actions
/// naming a profile that no longer exists are ignored.
fn apply_actions(settings: &mut Settings, actions: &[ProviderAction]) -> bool {
    let mut changed = false;
    for action in actions {
        match action {
            ProviderAction::Add(profile) => {
                if profile.validate().is_ok() && !settings.providers.iter().any(|p| p.id == profile.id) {
                    settings.providers.push(profile.clone());
                    changed = true;
                }
            }
            ProviderAction::Replace(profile) => {
                if profile.validate().is_ok() {
                    if let Some(p) = settings.providers.iter_mut().find(|p| p.id == profile.id) {
                        if *p != *profile {
                            *p = profile.clone();
                            changed = true;
                        }
                    }
                }
            }
            ProviderAction::Delete(id) => {
                let before = settings.providers.len();
                settings.providers.retain(|p| p.id != *id);
                if settings.providers.len() != before {
                    if settings.active_provider.as_deref() == Some(id.as_str()) {
                        settings.active_provider = None;
                    }
                    changed = true;
                }
            }
            // Exclusive with an account: a profile, or Code Assist, takes over from one in use.
            // Setting `active_provider` alone would leave the account in front of it. An unknown
            // id is refused.
            ProviderAction::Select(id) => changed |= settings.select_profile(id.clone()),
        }
    }
    changed
}

/// Kinds in the order the dialog offers them. The account-only kinds are not among them.
const KIND_LABELS: [(ProviderKind, &str); 3] = [
    (ProviderKind::GeminiApi, ProviderKind::GeminiApi.label()),
    (ProviderKind::OpenAi, ProviderKind::OpenAi.label()),
    (ProviderKind::Anthropic, ProviderKind::Anthropic.label()),
];

/// Settings key derived from a label: lowercase ASCII letters and digits, other runs collapsed
/// to one `-`, at most 32 characters. Empty when the label has no ASCII letter or digit.
fn slug(label: &str) -> String {
    let mut out = String::new();
    for c in label.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    out.truncate(32);
    out.trim_matches('-').to_owned()
}

/// `base`, or `base{sep}2`, `base{sep}3`, ... : the first that `taken` refuses.
fn first_free(base: &str, sep: &str, taken: impl Fn(&str) -> bool) -> String {
    if !taken(base) {
        return base.to_owned();
    }
    (2..).map(|n| format!("{base}{sep}{n}")).find(|c| !taken(c)).unwrap_or_default()
}

/// Turn the dialog into a profile that can be applied as is, or say why not.
///
/// A new profile: an empty label becomes the kind's name (numbered when another profile already
/// uses it), and the id is a slug of the label, numbered when taken. An edited one keeps its id,
/// its caps and, when the key field is empty, its key. The model may be empty. Errors are
/// user-facing and never quote the key.
fn dialog_profile(d: &ProviderDialog, existing: &[ProviderProfile]) -> Result<ProviderProfile, String> {
    let model = d.model.trim().to_owned();
    if !model.is_empty() && !crate::providers::models::valid_model_id(&model) {
        return Err("The model id has a space or a character a request cannot carry.".into());
    }
    let base_url = Some(d.base_url.trim().to_owned()).filter(|u| !u.is_empty());
    let profile = match &d.editing {
        Some(id) => {
            let Some(old) = existing.iter().find(|p| p.id == *id) else {
                return Err("This provider no longer exists.".into());
            };
            let label = match d.label.trim() {
                "" => d.kind.label().to_owned(),
                l => l.to_owned(),
            };
            let api_key = match d.api_key.trim() {
                "" => old.api_key.clone(),
                k => k.to_owned(),
            };
            ProviderProfile { label, kind: d.kind, model, base_url, api_key, models: d.models.clone(), ..old.clone() }
        }
        None => {
            if existing.len() >= MAX_PROVIDER_PROFILES {
                return Err(format!("The list holds at most {MAX_PROVIDER_PROFILES} profiles."));
            }
            let label = match d.label.trim() {
                "" => first_free(d.kind.label(), " ", |l| existing.iter().any(|p| p.label.trim() == l)),
                l => l.to_owned(),
            };
            let base = match slug(&label) {
                s if s.is_empty() => d.kind.as_str().to_owned(),
                s => s,
            };
            ProviderProfile {
                id: first_free(&base, "-", |id| existing.iter().any(|p| p.id == id)),
                label,
                kind: d.kind,
                model,
                base_url,
                api_key: d.api_key.trim().to_owned(),
                max_output_tokens: None,
                context_window: None,
                models: d.models.clone(),
            }
        }
    };
    profile.validate().map_err(|e| sentence(&e))?;
    Ok(profile)
}

/// `ProviderProfile::validate` messages are lowercase fragments; show them as sentences.
fn sentence(message: &str) -> String {
    let mut chars = message.chars();
    let mut out: String = chars.next().map(|c| c.to_uppercase().collect()).unwrap_or_default();
    out.push_str(chars.as_str());
    if !out.ends_with('.') {
        out.push('.');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::AppEvent;
    use crate::providers::models::LoadedModels;

    fn profile(id: &str) -> ProviderProfile {
        ProviderProfile {
            id: id.into(),
            label: format!("Profile {id}"),
            kind: ProviderKind::OpenAi,
            model: format!("m-{id}"),
            base_url: None,
            api_key: format!("k-{id}"),
            max_output_tokens: None,
            context_window: None,
            models: Vec::new(),
        }
    }

    /// The Add dialog with these fields typed.
    fn dialog(kind: ProviderKind, label: &str, model: &str, key: &str) -> ProviderDialog {
        ProviderDialog { label: label.into(), model: model.into(), api_key: key.into(), ..ProviderDialog::add(kind) }
    }

    fn settings(ids: &[&str], active: Option<&str>) -> Settings {
        Settings { providers: ids.iter().map(|id| profile(id)).collect(), active_provider: active.map(Into::into), ..Settings::default() }
    }

    fn ids(s: &Settings) -> Vec<&str> {
        s.providers.iter().map(|p| p.id.as_str()).collect()
    }

    #[test]
    fn slug_is_a_short_ascii_key() {
        assert_eq!(slug("My Gemini profile"), "my-gemini-profile");
        assert_eq!(slug("  --Work / Personal--  "), "work-personal");
        assert_eq!(slug("Cấu hình"), "c-u-h-nh");
        assert_eq!(slug("日本"), "");
        let long = slug(&"ab ".repeat(40));
        assert!(long.len() <= 32 && !long.ends_with('-'), "{long}");
    }

    #[test]
    fn new_profile_gets_a_label_and_a_unique_id() {
        let p = dialog_profile(&dialog(ProviderKind::OpenAi, "", " gpt-4o-mini ", " k-new "), &[]).unwrap();
        assert_eq!((p.id.as_str(), p.label.as_str(), p.model.as_str()), ("openai-compatible", "OpenAI-compatible", "gpt-4o-mini"));
        assert!(p.api_key == "k-new", "the key is trimmed");
        assert_eq!(p.base_url, None, "an empty base URL means the kind's default");

        // The same empty label again is numbered, and so is the id.
        let second = dialog_profile(&dialog(ProviderKind::OpenAi, "", "m", "k"), std::slice::from_ref(&p)).unwrap();
        assert_eq!((second.id.as_str(), second.label.as_str()), ("openai-compatible-2", "OpenAI-compatible 2"));

        // A typed label is kept even when another profile uses it; only the id is numbered.
        let mut work = p.clone();
        work.id = "work".into();
        work.label = "Work".into();
        let again = dialog_profile(&dialog(ProviderKind::Anthropic, " Work ", "m", "k"), &[work]).unwrap();
        assert_eq!((again.id.as_str(), again.label.as_str()), ("work-2", "Work"));

        // A label with no ASCII letters falls back to the kind for the id.
        let jp = dialog_profile(&dialog(ProviderKind::GeminiApi, "日本", "m", "k"), &[]).unwrap();
        assert_eq!((jp.id.as_str(), jp.label.as_str()), ("gemini-api", "日本"));
    }

    #[test]
    fn a_new_profile_needs_a_key_but_no_model() {
        // No model: saved as is, picked later from the composer's menu.
        let mut d = dialog(ProviderKind::Anthropic, "pix4k", "  ", "k-secret");
        d.models = vec!["claude-opus-5-5".into()];
        let p = dialog_profile(&d, &[]).unwrap();
        assert_eq!((p.model.as_str(), p.models.as_slice()), ("", ["claude-opus-5-5".to_owned()].as_slice()), "the loaded list goes with it");

        let err = dialog_profile(&dialog(ProviderKind::OpenAi, "x", "m", "  "), &[]).unwrap_err();
        assert_eq!(err, "API key is empty.");
        let err = dialog_profile(&dialog(ProviderKind::OpenAi, "x", "has space", "k"), &[]).unwrap_err();
        assert!(err.starts_with("The model id has a space"), "{err}");

        let mut d = dialog(ProviderKind::OpenAi, "x", "m", "k-secret");
        d.base_url = "ftp://example.com".into();
        let err = dialog_profile(&d, &[]).unwrap_err();
        assert!(err.starts_with("Base URL must be http or https"), "{err}");
        assert!(!err.contains("k-secret"));
        d.base_url = "not a url".into();
        assert!(dialog_profile(&d, &[]).unwrap_err().starts_with("Base URL is not a valid URL"));
        d.base_url = " http://localhost:8080/v1 ".into();
        assert_eq!(dialog_profile(&d, &[]).unwrap().base_url.as_deref(), Some("http://localhost:8080/v1"));

        let full: Vec<ProviderProfile> = (0..MAX_PROVIDER_PROFILES).map(|i| profile(&format!("p{i}"))).collect();
        let err = dialog_profile(&dialog(ProviderKind::OpenAi, "x", "m", "k"), &full).unwrap_err();
        assert!(err.contains("at most"), "{err}");
    }

    #[test]
    fn an_edited_profile_keeps_its_id_caps_and_saved_key() {
        let mut old = profile("c");
        old.kind = ProviderKind::Anthropic;
        old.max_output_tokens = Some(8192);
        let existing = [profile("a"), old.clone()];
        let mut d = ProviderDialog::edit(&old);
        assert!(d.api_key.is_empty(), "the saved key is never put in the field");
        d.label = "  ".into();
        d.model = "claude-opus-5-5".into();
        let p = dialog_profile(&d, &existing).unwrap();
        assert_eq!((p.id.as_str(), p.api_key.as_str(), p.max_output_tokens), ("c", "k-c", Some(8192)), "an empty key field keeps the key");
        assert_eq!((p.label.as_str(), p.model.as_str()), ("Anthropic", "claude-opus-5-5"), "an emptied label falls back to the kind's name");
        d.api_key = " k-replaced ".into();
        assert_eq!(dialog_profile(&d, &existing).unwrap().api_key, "k-replaced");
        // The profile went away while its dialog was open.
        assert_eq!(dialog_profile(&d, &existing[..1]).unwrap_err(), "This provider no longer exists.");
    }

    #[test]
    fn built_profiles_survive_sanitize() {
        let mut s = settings(&["a"], None);
        let p = dialog_profile(&dialog(ProviderKind::Anthropic, "", "", "k"), &s.providers).unwrap();
        let id = p.id.clone();
        assert!(apply_actions(&mut s, &[ProviderAction::Add(p), ProviderAction::Select(Some(id.clone()))]));
        let saved = s.clone().sanitized();
        assert_eq!(ids(&saved), ids(&s), "no profile is dropped on save, a model-less one included");
        assert_eq!(saved.active_provider.as_deref(), Some(id.as_str()));
    }

    #[test]
    fn delete_goes_by_id_and_clears_the_selection() {
        // Deleting the last profile used to index past the end.
        let mut s = settings(&["a", "b", "c"], Some("c"));
        assert!(apply_actions(&mut s, &[ProviderAction::Delete("c".into())]));
        assert_eq!(ids(&s), ["a", "b"]);
        assert_eq!(s.active_provider, None, "the deleted profile was in use");

        // A middle profile: the others keep their fields and the selection stays.
        let mut s = settings(&["a", "b", "c"], Some("c"));
        assert!(apply_actions(&mut s, &[ProviderAction::Delete("b".into())]));
        assert_eq!(ids(&s), ["a", "c"]);
        assert_eq!(s.providers[1].model, "m-c");
        assert_eq!(s.active_provider.as_deref(), Some("c"));

        // Two deletes in one batch, then a stale one: ids, not positions.
        let mut s = settings(&["a", "b", "c"], None);
        assert!(apply_actions(&mut s, &[ProviderAction::Delete("a".into()), ProviderAction::Delete("c".into()), ProviderAction::Delete("a".into())]));
        assert_eq!(ids(&s), ["b"]);
        assert!(!apply_actions(&mut s, &[ProviderAction::Delete("gone".into())]));
    }

    #[test]
    fn select_changes_the_local_copy() {
        let mut s = settings(&["a", "b"], None);
        assert!(apply_actions(&mut s, &[ProviderAction::Select(Some("b".into()))]));
        assert_eq!(s.active_provider.as_deref(), Some("b"));
        assert!(!apply_actions(&mut s, &[ProviderAction::Select(Some("b".into()))]), "already selected");
        assert!(!apply_actions(&mut s, &[ProviderAction::Select(Some("nope".into()))]), "unknown ids are ignored");
        assert_eq!(s.active_provider.as_deref(), Some("b"));
        assert!(apply_actions(&mut s, &[ProviderAction::Select(None)]));
        assert_eq!(s.active_provider, None, "back to Code Assist");
    }

    #[test]
    fn replace_goes_by_id_and_refuses_what_would_not_validate() {
        let mut s = settings(&["a", "b"], Some("a"));
        let mut b = profile("b");
        b.model = "gpt-4o".into();
        b.models = vec!["gpt-4o".into(), "o3".into()];
        assert!(apply_actions(&mut s, &[ProviderAction::Replace(b.clone())]));
        assert_eq!(s.providers[1], b, "in place: the list keeps its order");
        assert!(!apply_actions(&mut s, &[ProviderAction::Replace(b.clone())]), "the same profile again");

        let mut keyless = b.clone();
        keyless.api_key = " ".into();
        let mut gone = profile("gone");
        gone.model = "x".into();
        assert!(!apply_actions(&mut s, &[ProviderAction::Replace(keyless), ProviderAction::Replace(gone)]));
        assert_eq!(s.providers[1], b);
        // An Add that would not validate, or would reuse an id, is not appended either.
        let mut broken = profile("c");
        broken.label.clear();
        assert!(!apply_actions(&mut s, &[ProviderAction::Add(broken), ProviderAction::Add(profile("a"))]));
        assert_eq!(ids(&s), ["a", "b"]);
        let saved = s.clone().sanitized();
        assert_eq!(ids(&saved), ["a", "b"]);
        assert_eq!(saved.active_provider.as_deref(), Some("a"));
    }

    #[test]
    fn debug_never_shows_typed_keys() {
        let d = dialog(ProviderKind::OpenAi, "x", "m", "k-typed-secret");
        let shown = format!("{d:?}");
        assert!(!shown.contains("typed-secret"));
        assert!(shown.contains("<redacted>"));
    }

    // --- Rendering -------------------------------------------------------------------------

    use crate::ui::test_util::{self, key};

    fn context() -> egui::Context {
        test_util::context()
    }

    fn providers_state() -> UiState {
        let mut state = UiState { customize_tab: CustomizeTab::Providers, ..Default::default() };
        state.settings.providers = vec![profile("a"), profile("b"), profile("c")];
        state.settings.providers[2].kind = ProviderKind::Anthropic;
        state.settings.providers[2].base_url = Some("http://localhost:8080/v1".into());
        state.settings.active_provider = Some("b".into());
        state
    }

    fn click(pos: egui::Pos2, pressed: bool) -> egui::Event {
        test_util::click(pos, pressed)
    }

    /// One frame of the sections, on the full canvas: the painted texts and the commands it
    /// sent. Tests read the rows without the overlay's scroll area narrowing them.
    fn draw(ctx: &egui::Context, state: &mut UiState, events: Vec<egui::Event>) -> (Vec<(String, Rect)>, Vec<UiAction>) {
        let mut commands = Vec::new();
        let texts = test_util::painted(ctx, events, |ui| {
            egui::CentralPanel::default().show(ui, |ui| super::sections(ui, state, &mut commands));
        });
        (texts, commands)
    }

    /// One frame of the page that sends nothing; returns the painted texts.
    fn page(ctx: &egui::Context, state: &mut UiState, events: Vec<egui::Event>) -> Vec<(String, Rect)> {
        let (texts, commands) = draw(ctx, state, events);
        assert!(commands.is_empty(), "nothing is sent: {commands:?}");
        texts
    }

    /// One frame of the whole Settings overlay (title row, rail and sections). A modal area
    /// settles over two frames, so this plays `events` and then an empty frame, and returns
    /// the texts of the second.
    fn shell(ctx: &egui::Context, state: &mut UiState, events: Vec<egui::Event>) -> Vec<(String, Rect)> {
        state.customize_open = true;
        let mut commands = Vec::new();
        test_util::painted(ctx, events, |_ui| overlay(ctx, state, &mut commands));
        let texts = test_util::painted(ctx, Vec::new(), |_ui| overlay(ctx, state, &mut commands));
        assert!(commands.is_empty(), "nothing is sent: {commands:?}");
        texts
    }

    /// Click `text` in the overlay (the search field sits in its title row).
    fn shell_click_text(ctx: &egui::Context, state: &mut UiState, text: &str) {
        let texts = shell(ctx, state, Vec::new());
        let pos = texts.iter().find(|(t, _)| t == text).map(|(_, r)| r.center()).unwrap_or_else(|| panic!("no {text:?} in the overlay: {texts:?}"));
        shell(ctx, state, vec![egui::Event::PointerMoved(pos), click(pos, true)]);
        shell(ctx, state, vec![click(pos, false)]);
    }

    /// Rects of the painted texts equal to `text`, top to bottom.
    fn rects_of(texts: &[(String, Rect)], text: &str) -> Vec<Rect> {
        let mut rects: Vec<Rect> = texts.iter().filter(|(t, _)| t == text).map(|(_, r)| *r).collect();
        rects.sort_by(|a, b| a.top().total_cmp(&b.top()));
        rects
    }

    fn shows(texts: &[(String, Rect)], part: &str) -> bool {
        texts.iter().any(|(t, _)| t.contains(part))
    }

    /// Press and release at `pos`; the commands of both frames.
    fn click_at(ctx: &egui::Context, state: &mut UiState, pos: egui::Pos2) -> Vec<UiAction> {
        let (_, mut sent) = draw(ctx, state, vec![egui::Event::PointerMoved(pos), click(pos, true)]);
        sent.extend(draw(ctx, state, vec![click(pos, false)]).1);
        sent
    }

    /// Click the lowest painted text equal to `text` (a field's value or hint, a list row).
    fn click_text(ctx: &egui::Context, state: &mut UiState, text: &str) -> Vec<UiAction> {
        let texts = page(ctx, state, Vec::new());
        let rect = *rects_of(&texts, text).last().unwrap_or_else(|| panic!("no {text:?} in {texts:?}"));
        click_at(ctx, state, rect.center())
    }

    /// Click the button whose text ends with `label` (it may start with an icon), the lowest one
    /// when several do: the dialog is drawn under the rows it covers.
    fn click_button(ctx: &egui::Context, state: &mut UiState, label: &str) -> Vec<UiAction> {
        let texts = page(ctx, state, Vec::new());
        let pos = texts
            .iter()
            .filter(|(t, _)| t.ends_with(label))
            .map(|(_, r)| r.center())
            .max_by(|a, b| a.y.total_cmp(&b.y))
            .unwrap_or_else(|| panic!("no {label:?} button in {texts:?}"));
        click_at(ctx, state, pos)
    }

    /// Where the button ending with `button` is on the row labelled `row_label`.
    fn row_button(texts: &[(String, Rect)], row_label: &str, button: &str) -> egui::Pos2 {
        let row = rects_of(texts, row_label)[0];
        texts
            .iter()
            .filter(|(t, r)| t.ends_with(button) && r.left() > row.right() && (r.center().y - row.center().y).abs() < 30.0)
            .map(|(_, r)| r.center())
            .next()
            .unwrap_or_else(|| panic!("no {button:?} on the row {row_label:?}"))
    }

    fn type_text(ctx: &egui::Context, state: &mut UiState, text: &str) {
        for c in text.chars() {
            page(ctx, state, vec![egui::Event::Text(c.to_string())]);
        }
    }

    fn press(ctx: &egui::Context, state: &mut UiState, k: egui::Key, modifiers: egui::Modifiers) -> Vec<(String, Rect)> {
        page(ctx, state, vec![key(k, modifiers)])
    }

    /// What the UI would save must be what the backend keeps: every profile validates, the
    /// sanitize pass drops none, and the selection survives it.
    fn check_saved(state: &UiState) {
        for p in &state.settings.providers {
            assert!(p.validate().is_ok(), "profile {} would be dropped on save", p.id);
        }
        let saved = state.settings.clone().sanitized();
        assert_eq!(ids(&saved), ids(&state.settings));
        assert_eq!(saved.active_provider, state.settings.active_provider);
    }

    #[test]
    fn the_tab_lists_each_profile_with_edit_delete_and_use() {
        let ctx = context();
        let mut state = providers_state();
        state.settings.providers[0].model.clear();
        state.settings.providers[0].models = vec!["x".into(), "y".into()];
        let texts = page(&ctx, &mut state, Vec::new());
        for part in ["Profile a", "OpenAI-compatible · no model chosen (2 loaded)", "OpenAI-compatible · m-b", "Anthropic · m-c · http://localhost:8080/v1"] {
            assert!(shows(&texts, part), "{part:?} missing from {texts:?}");
        }
        assert_eq!(rects_of(&texts, "Current").len(), 1);
        for (row, button) in [("Profile a", "Use"), ("Profile a", "Delete"), ("Profile a", "Edit"), ("Profile b", "Current"), ("Profile c", "Edit")] {
            row_button(&texts, row, button);
        }
        assert!(texts.iter().any(|(t, _)| t.ends_with("Add provider")));
        // Code Assist is the Google sign-in's backend, not a provider; and nothing is edited in place.
        assert!(!shows(&texts, "Google Code Assist"), "{texts:?}");
        assert!(!shows(&texts, "New API key") && !shows(&texts, "model-id") && !shows(&texts, "Base URL"));
        assert!(!state.settings_dirty, "drawing alone changes nothing");

        // Without profiles the tab says so, and still offers to add one.
        state.settings.providers.clear();
        state.settings.active_provider = None;
        let texts = page(&ctx, &mut state, Vec::new());
        assert!(shows(&texts, "No providers yet.") && texts.iter().any(|(t, _)| t.ends_with("Add provider")));
    }

    #[test]
    fn use_and_delete_work_from_the_rows() {
        let ctx = context();
        let mut state = providers_state();
        let texts = page(&ctx, &mut state, Vec::new());
        assert!(click_at(&ctx, &mut state, row_button(&texts, "Profile a", "Use")).is_empty());
        assert_eq!(state.settings.active_provider.as_deref(), Some("a"));
        // Delete arms first, then deletes; another click disarms.
        let texts = page(&ctx, &mut state, Vec::new());
        click_at(&ctx, &mut state, row_button(&texts, "Profile c", "Delete"));
        assert_eq!(state.provider_delete_armed.as_deref(), Some("c"));
        let texts = page(&ctx, &mut state, Vec::new());
        click_at(&ctx, &mut state, row_button(&texts, "Profile c", "Click again to delete"));
        assert_eq!(ids(&state.settings), ["a", "b"]);
        assert!(state.settings_dirty);
        check_saved(&state);

        // Mid-turn the backend in use stays as it is.
        state.busy = true;
        let texts = page(&ctx, &mut state, Vec::new());
        click_at(&ctx, &mut state, row_button(&texts, "Profile b", "Use"));
        click_at(&ctx, &mut state, row_button(&texts, "Profile b", "Delete"));
        assert_eq!((state.settings.active_provider.as_deref(), state.provider_delete_armed.as_deref()), (Some("a"), None));
    }

    #[test]
    fn add_provider_saves_a_profile_without_a_model() {
        let ctx = context();
        let mut state = providers_state();
        assert!(click_button(&ctx, &mut state, "Add provider").is_empty());
        let d = state.provider_dialog.as_ref().expect("the dialog opened");
        assert_eq!((d.editing.as_deref(), d.kind), (None, ProviderKind::Anthropic), "a new profile starts on the last one's kind");
        let texts = page(&ctx, &mut state, Vec::new());
        assert_eq!(rects_of(&texts, "Add provider").len(), 1, "the dialog's title: {texts:?}");
        assert!(shows(&texts, "Load the provider's models to pick one here."));

        // Without a key nothing can be loaded or saved.
        assert!(click_button(&ctx, &mut state, "Load models").is_empty());
        assert!(click_button(&ctx, &mut state, "Save").is_empty());
        assert!(state.provider_dialog.is_some() && !state.settings_dirty);

        click_text(&ctx, &mut state, "API key");
        type_text(&ctx, &mut state, "k-new");
        // The Label field's hint is the kind's name, below the Type control's "Anthropic".
        click_text(&ctx, &mut state, "Anthropic");
        type_text(&ctx, &mut state, "Work");
        assert!(click_button(&ctx, &mut state, "Save").is_empty());

        assert!(state.provider_dialog.is_none(), "saved, the dialog closed");
        let p = state.settings.providers.last().unwrap();
        assert_eq!((p.id.as_str(), p.label.as_str(), p.kind, p.model.as_str(), p.api_key.as_str()), ("work", "Work", ProviderKind::Anthropic, "", "k-new"));
        assert!(state.settings_dirty);
        check_saved(&state);
        assert!(ctx.memory(|m| m.focused()).is_none(), "the key field let go of the focus");
    }

    /// Open `row_label`'s dialog with its Edit button.
    fn open_edit(ctx: &egui::Context, state: &mut UiState, row_label: &str) {
        let texts = page(ctx, state, Vec::new());
        assert!(click_at(ctx, state, row_button(&texts, row_label, "Edit")).is_empty());
        assert!(state.provider_dialog.as_ref().is_some_and(|d| d.editing.is_some()), "the dialog opened");
    }

    #[test]
    fn edit_fills_the_dialog_and_save_replaces_the_profile_keeping_its_key() {
        let ctx = context();
        let mut state = providers_state();
        open_edit(&ctx, &mut state, "Profile c");
        let texts = page(&ctx, &mut state, Vec::new());
        assert!(shows(&texts, "Edit provider") && shows(&texts, "Leave empty to keep the saved key"), "{texts:?}");
        assert!(!shows(&texts, "k-c"), "the saved key is never shown");
        let d = state.provider_dialog.as_ref().unwrap();
        assert_eq!((d.label.as_str(), d.base_url.as_str(), d.model.as_str()), ("Profile c", "http://localhost:8080/v1", "m-c"));

        click_text(&ctx, &mut state, "m-c");
        press(&ctx, &mut state, egui::Key::A, egui::Modifiers::COMMAND);
        type_text(&ctx, &mut state, "claude-opus-5-5");
        click_button(&ctx, &mut state, "Save");
        let c = &state.settings.providers[2];
        assert_eq!((c.id.as_str(), c.model.as_str(), c.api_key.as_str(), c.base_url.as_deref()), ("c", "claude-opus-5-5", "k-c", Some("http://localhost:8080/v1")));
        assert!(state.provider_dialog.is_none() && state.settings_dirty);
        check_saved(&state);
    }

    #[test]
    fn load_models_asks_with_the_saved_key_and_takes_only_its_answer() {
        let ctx = context();
        let mut state = providers_state();
        open_edit(&ctx, &mut state, "Profile c");
        let sent = click_button(&ctx, &mut state, "Load models");
        let request = match sent.as_slice() {
            [AppCommand::LoadProviderModels { request, profile }] => {
                assert_eq!((profile.kind, profile.base_url.as_deref(), profile.api_key.as_str()), (ProviderKind::Anthropic, Some("http://localhost:8080/v1"), "k-c"));
                *request
            }
            other => panic!("expected one load: {other:?}"),
        };
        assert!(shows(&page(&ctx, &mut state, Vec::new()), "Loading…"));
        // A second click cannot ask again while it runs.
        assert!(click_button(&ctx, &mut state, "Loading…").is_empty());

        // An answer to another request is someone else's.
        let other = LoadedModels { models: vec!["stale".into()], from: "http://elsewhere/v1/models".into() };
        state.apply(AppEvent::ProviderModels { request: request + 7, result: Ok(other) });
        assert!(state.provider_dialog.as_ref().unwrap().loading.is_some());
        let loaded = LoadedModels { models: vec!["claude-haiku-5-5".into(), "claude-opus-5-5".into()], from: "http://localhost:8080/v1/models".into() };
        state.apply(AppEvent::ProviderModels { request, result: Ok(loaded) });
        let texts = page(&ctx, &mut state, Vec::new());
        assert!(shows(&texts, "2 models · http://localhost:8080/v1/models"), "{texts:?}");
        // One row under the other, each id drawn whole. Laid out left to right, the second row
        // was a sliver at the list's right edge that no click could reach.
        let (haiku, opus) = (rects_of(&texts, "claude-haiku-5-5")[0], rects_of(&texts, "claude-opus-5-5")[0]);
        assert!((haiku.left() - opus.left()).abs() < 1.0 && opus.top() > haiku.bottom(), "{haiku:?} {opus:?}");
        assert!(opus.width() > 60.0, "{opus:?}");

        click_text(&ctx, &mut state, "claude-opus-5-5");
        assert_eq!(state.provider_dialog.as_ref().unwrap().model, "claude-opus-5-5");
        click_button(&ctx, &mut state, "Save");
        let c = &state.settings.providers[2];
        assert_eq!(c.model, "claude-opus-5-5");
        assert_eq!(c.models, ["claude-haiku-5-5", "claude-opus-5-5"], "the list is saved with the profile");
        check_saved(&state);

        // Reopened, the list is there, as loaded earlier.
        open_edit(&ctx, &mut state, "Profile c");
        assert!(shows(&page(&ctx, &mut state, Vec::new()), "2 models, loaded earlier"));
    }

    #[test]
    fn editing_the_endpoint_drops_the_answer_about_the_old_one() {
        let ctx = context();
        let mut state = providers_state();
        open_edit(&ctx, &mut state, "Profile c");
        let request = match click_button(&ctx, &mut state, "Load models").as_slice() {
            [AppCommand::LoadProviderModels { request, .. }] => *request,
            other => panic!("{other:?}"),
        };
        click_text(&ctx, &mut state, "http://localhost:8080/v1");
        type_text(&ctx, &mut state, "x");
        assert!(state.provider_dialog.as_ref().unwrap().loading.is_none());
        state.apply(AppEvent::ProviderModels { request, result: Ok(LoadedModels { models: vec!["old".into()], from: "http://localhost:8080/v1/models".into() }) });
        assert!(state.provider_dialog.as_ref().unwrap().models.is_empty(), "the answer about the old URL is dropped");

        // A new kind starts over: its own default endpoint and no list.
        state.provider_dialog.as_mut().unwrap().models = vec!["m1".into()];
        click_text(&ctx, &mut state, "Gemini Developer API");
        let d = state.provider_dialog.as_ref().unwrap();
        assert_eq!((d.kind, d.base_url.as_str(), d.models.len()), (ProviderKind::GeminiApi, "", 0));
    }

    #[test]
    fn a_failed_load_says_why_and_keeps_the_list() {
        let ctx = context();
        let mut state = providers_state();
        state.settings.providers[2].models = vec!["claude-haiku-5-5".into()];
        open_edit(&ctx, &mut state, "Profile c");
        let request = match click_button(&ctx, &mut state, "Load models").as_slice() {
            [AppCommand::LoadProviderModels { request, .. }] => *request,
            other => panic!("{other:?}"),
        };
        state.apply(AppEvent::ProviderModels { request, result: Err("The provider refused the API key (HTTP 401)".into()) });
        let texts = page(&ctx, &mut state, Vec::new());
        assert!(shows(&texts, "The provider refused the API key (HTTP 401)"));
        assert!(!rects_of(&texts, "claude-haiku-5-5").is_empty(), "the list from before stays: {texts:?}");
        assert!(texts.iter().any(|(t, _)| t.ends_with("Load models")), "and it can be tried again");
    }

    #[test]
    fn the_model_field_narrows_the_list_while_it_is_typed_in() {
        let ctx = context();
        let mut state = providers_state();
        state.settings.providers[2].models = vec!["claude-haiku-5-5".into(), "claude-opus-5-5".into(), "gpt-4o".into()];
        open_edit(&ctx, &mut state, "Profile c");
        let rows = |texts: &[(String, Rect)]| ["claude-haiku-5-5", "claude-opus-5-5", "gpt-4o"].iter().filter(|m| !rects_of(texts, m).is_empty()).count();
        // c's model ("m-c") is in no list, and the list is whole all the same.
        assert_eq!(rows(&page(&ctx, &mut state, Vec::new())), 3);
        click_text(&ctx, &mut state, "m-c");
        press(&ctx, &mut state, egui::Key::A, egui::Modifiers::COMMAND);
        type_text(&ctx, &mut state, "OPUS");
        assert_eq!(rows(&page(&ctx, &mut state, Vec::new())), 1, "case does not matter");
        type_text(&ctx, &mut state, "zz");
        assert!(shows(&page(&ctx, &mut state, Vec::new()), "No loaded model matches."));
        press(&ctx, &mut state, egui::Key::A, egui::Modifiers::COMMAND);
        type_text(&ctx, &mut state, "opus");
        click_text(&ctx, &mut state, "claude-opus-5-5");
        // Picked: the whole list is back to pick another from, the pick checked.
        let texts = page(&ctx, &mut state, Vec::new());
        assert_eq!(rows(&texts), 3);
        assert_eq!(rects_of(&texts, icons::CHECK).len(), 1);
        assert_eq!(state.provider_dialog.as_ref().unwrap().model, "claude-opus-5-5");
    }

    /// The list scrolls in its own height: about seven rows show (only those are laid out), the
    /// rest scroll. Inside a horizontal row it had the row's height and showed two and a half.
    #[test]
    fn the_dialog_list_shows_seven_rows_and_scrolls_the_rest() {
        let ctx = context();
        let mut state = providers_state();
        let ids: Vec<String> = (0..30).map(|i| format!("model-{i:02}")).collect();
        state.settings.providers[2].models = ids.clone();
        open_edit(&ctx, &mut state, "Profile c");
        page(&ctx, &mut state, Vec::new());
        let texts = page(&ctx, &mut state, Vec::new());
        let shown = ids.iter().filter(|m| !rects_of(&texts, m).is_empty()).count();
        assert!((7..=9).contains(&shown), "{shown} rows laid out");
    }

    #[test]
    fn cancel_close_and_escape_leave_the_list_as_it_was() {
        let ctx = context();
        let mut state = providers_state();
        let before = state.settings.clone();
        for way in ["Cancel", "close", "Escape"] {
            open_edit(&ctx, &mut state, "Profile a");
            click_text(&ctx, &mut state, "Profile a");
            type_text(&ctx, &mut state, " edited");
            click_text(&ctx, &mut state, "Leave empty to keep the saved key");
            type_text(&ctx, &mut state, "k-typed");
            let key_field = ctx.memory(|m| m.focused()).expect("the key field has focus");
            match way {
                "Cancel" => {
                    click_button(&ctx, &mut state, "Cancel");
                }
                "close" => {
                    let texts = page(&ctx, &mut state, Vec::new());
                    let x = texts.iter().find(|(t, _)| t == icons::X).map(|(_, r)| r.center()).expect("the close button");
                    click_at(&ctx, &mut state, x);
                }
                _ => {
                    press(&ctx, &mut state, egui::Key::Escape, egui::Modifiers::NONE);
                }
            }
            assert!(state.provider_dialog.is_none(), "{way} closed it");
            assert_eq!(state.settings, before, "{way} changed nothing");
            assert!(!state.settings_dirty);
            assert!(egui::text_edit::TextEditState::load(&ctx, key_field).is_none(), "{way}: the typed key's undo history is gone");
        }
        // A click outside does not close it.
        open_edit(&ctx, &mut state, "Profile a");
        click_at(&ctx, &mut state, egui::pos2(5.0, 5.0));
        assert!(state.provider_dialog.is_some());
    }

    #[test]
    fn the_search_finds_rows_by_their_model_and_url() {
        let ctx = context();
        let mut state = providers_state();
        // The field is in the overlay's title row; the sections read what it holds.
        shell_click_text(&ctx, &mut state, "Search settings");
        for c in "localhost".chars() {
            shell(&ctx, &mut state, vec![egui::Event::Text(c.to_string())]);
        }
        let texts = page(&ctx, &mut state, Vec::new());
        assert!(shows(&texts, "Profile c") && !shows(&texts, "Profile a") && !shows(&texts, "Profile b"));
        assert!(!texts.iter().any(|(t, _)| t.ends_with("Add provider")));
    }

    #[test]
    fn the_account_tab_says_sign_in_is_optional_while_a_provider_is_in_use() {
        let ctx = context();
        let mut state = UiState { customize_tab: CustomizeTab::Account, ..providers_state() };
        let texts = page(&ctx, &mut state, Vec::new());
        assert!(shows(&texts, "Not needed while Profile b is in use"), "{texts:?}");
        assert!(!shows(&texts, "Sign in to start conversations"));
        state.settings.active_provider = None;
        let texts = page(&ctx, &mut state, Vec::new());
        assert!(shows(&texts, "Sign in to start conversations."));
    }

    #[test]
    fn default_model_row_names_the_provider_in_use() {
        let mut s = settings(&["a", "b"], None);
        assert!(default_model_desc(&s).starts_with("Used for every new turn"));
        s.active_provider = Some("b".into());
        let desc = default_model_desc(&s);
        assert!(desc.contains("Profile b (m-b)") && desc.contains("model menu in the composer"), "{desc}");
        s.providers[1].model.clear();
        assert!(default_model_desc(&s).contains("Profile b (no model chosen)"));
    }

    fn account_entry(id: &str, provider: AccountProvider, label: &str, model: &str) -> crate::settings::AccountEntry {
        crate::settings::AccountEntry { id: id.into(), provider, label: label.into(), model: model.into() }
    }

    #[test]
    fn default_model_row_names_the_account_in_use() {
        let mut s = settings(&["a"], None);
        s.accounts = vec![account_entry("codex-0123abcd", AccountProvider::Codex, "dev@example.com", "gpt-5.1-codex")];
        s.select_account("codex-0123abcd");
        assert_eq!(
            default_model_desc(&s),
            "Code Assist model. New turns now use gpt-5.1-codex (dev@example.com); change it from the composer or the Account tab."
        );
    }

    #[test]
    fn a_profile_picked_while_an_account_is_in_use_replaces_it() {
        let ctx = context();
        let mut state = providers_state();
        state.settings.accounts = vec![account_entry("antigravity-0123abcd", AccountProvider::Antigravity, "me@example.com", "gemini-3-flash")];
        assert!(state.settings.select_account("antigravity-0123abcd"));
        let texts = page(&ctx, &mut state, Vec::new());
        assert!(rects_of(&texts, "Current").is_empty(), "no profile is in use");
        click_at(&ctx, &mut state, row_button(&texts, "Profile b", "Use"));
        assert_eq!((state.settings.active_account.as_deref(), state.settings.active_provider.as_deref()), (None, Some("b")));
        let saved = state.settings.clone().sanitized();
        assert_eq!((saved.active_account, saved.active_provider.as_deref()), (None, Some("b")));
    }

    #[test]
    fn forgotten_field_loses_its_state_and_focus() {
        let ctx = context();
        let id = egui::Id::unique("forget_me");
        egui::text_edit::TextEditState::default().store(&ctx, id);
        ctx.memory_mut(|m| m.request_focus(id));
        ctx.run_ui(egui::RawInput::default(), |ui| forget_field(ui, id)).drop_without_applying_deltas();
        assert!(egui::text_edit::TextEditState::load(&ctx, id).is_none(), "the undo history is gone");
        assert!(!ctx.memory(|m| m.has_focus(id)));
    }

    // --- Bypass Mode -----------------------------------------------------------------------

    /// The Tools tab with Bypass Mode's section, on a state whose providers are known.
    fn bypass_state() -> UiState {
        let mut s = providers_state();
        s.customize_tab = CustomizeTab::Agent;
        s
    }

    /// A canvas tall enough for the Tools tab, whose Bypass section sits past 1600 px. The page
    /// is not clipped there, so reading it is the same as reading the top of any other tab.
    const TALL: egui::Vec2 = egui::vec2(1400.0, 2600.0);

    /// One frame of the page on a tall canvas; returns the painted texts.
    fn tall_page(ctx: &egui::Context, state: &mut UiState, events: Vec<egui::Event>) -> Vec<(String, Rect)> {
        let mut commands = Vec::new();
        let texts = crate::ui::test_util::painted_in(ctx, TALL, events, |ui| {
            egui::CentralPanel::default().show(ui, |ui| super::sections(ui, state, &mut commands));
        });
        assert!(commands.is_empty(), "nothing is sent: {commands:?}");
        texts
    }

    /// Press and release at `pos` on the tall canvas.
    fn tall_click(ctx: &egui::Context, state: &mut UiState, pos: egui::Pos2) {
        let mut commands = Vec::new();
        let mut frame = |events| {
            crate::ui::test_util::painted_in(ctx, TALL, events, |ui| {
                egui::CentralPanel::default().show(ui, |ui| super::sections(ui, state, &mut commands));
            })
        };
        frame(vec![egui::Event::PointerMoved(pos), click(pos, true)]);
        frame(vec![click(pos, false)]);
    }

    /// Click the button whose text ends with `label` on the tall canvas.
    fn tall_click_button(ctx: &egui::Context, state: &mut UiState, label: &str) {
        let texts = tall_page(ctx, state, Vec::new());
        let pos = texts
            .iter()
            .filter(|(t, _)| t.ends_with(label))
            .map(|(_, r)| r.center())
            .max_by(|a, b| a.y.total_cmp(&b.y))
            .unwrap_or_else(|| panic!("no {label:?} button in {texts:?}"));
        tall_click(ctx, state, pos);
    }

    /// Click the lowest painted text equal to `text` on the tall canvas.
    fn tall_click_text(ctx: &egui::Context, state: &mut UiState, text: &str) {
        let texts = tall_page(ctx, state, Vec::new());
        let rect = *rects_of(&texts, text).last().unwrap_or_else(|| panic!("no {text:?} in {texts:?}"));
        tall_click(ctx, state, rect.center());
    }

    /// Type `text` into whatever field has the focus.
    fn tall_type(ctx: &egui::Context, state: &mut UiState, text: &str) {
        for c in text.chars() {
            tall_page(ctx, state, vec![egui::Event::Text(c.to_string())]);
        }
    }

    /// Where the picker pill of the row labelled `row_label` is.
    fn bypass_picker_pos(texts: &[(String, Rect)], row_label: &str) -> egui::Pos2 {
        let row = rects_of(texts, row_label)[0];
        texts
            .iter()
            .filter(|(t, _)| t == "Choose a provider" || t.contains(" · "))
            .filter(|(_, r)| r.left() > row.left() && (r.center().y - row.center().y).abs() < 40.0)
            .map(|(_, r)| r.center())
            .next()
            .unwrap_or_else(|| panic!("no picker on {row_label:?}"))
    }

    /// Where the toggle of the row labelled `row_label` is: the 40x22 switch in the toggle
    /// column, which starts about 297 px right of the label.
    fn bypass_toggle_pos(texts: &[(String, Rect)], row_label: &str) -> egui::Pos2 {
        let row = rects_of(texts, row_label)[0];
        egui::pos2(row.left() + TOGGLE_COLUMN + 20.0, row.center().y)
    }

    /// The Tools tab offers the five permission modes as one row, and a click writes all four
    /// policy fields through `set_permission_mode` and marks the settings dirty. The old
    /// separate "apply file edits" toggle and "Command auto-run" control are gone.
    #[test]
    fn the_mode_row_offers_the_five_modes() {
        use crate::settings::PermissionMode;
        let ctx = context();
        let mut state = bypass_state();
        state.settings.set_permission_mode(PermissionMode::Manual);
        let texts = tall_page(&ctx, &mut state, Vec::new());
        let row = rects_of(&texts, "Mode")[0];
        // The segmented control sits in the row's own vertical band (the description can make
        // the row tall), to the right of the label.
        let on_row = |r: &Rect| r.center().y > row.top() && r.center().y < row.bottom() + 80.0 && r.center().x > row.right();
        for label in ["Manual", "Edit Auto", "Plan", "Auto", "Bypass"] {
            assert!(texts.iter().any(|(t, r)| t == label && on_row(r)), "{label:?} is not on the Mode row: {texts:?}");
        }
        assert!(!shows(&texts, "Apply file edits without asking"));
        assert!(!shows(&texts, "Command auto-run"));
        // The description keeps the words the settings search looks for.
        assert!(shows(&texts, "auto-run") && shows(&texts, "Manual asks"));

        for m in [PermissionMode::Plan, PermissionMode::Auto, PermissionMode::Bypass, PermissionMode::EditAuto] {
            let texts = tall_page(&ctx, &mut state, Vec::new());
            let row = rects_of(&texts, "Mode")[0];
            let label = super::super::composer::permission_label(m).1;
            let pos = texts
                .iter()
                .find(|(t, r)| t == label && r.center().y > row.top() && r.center().y < row.bottom() + 80.0 && r.center().x > row.right())
                .map(|(_, r)| r.center())
                .unwrap_or_else(|| panic!("no {label:?} on the Mode row: {texts:?}"));
            tall_click(&ctx, &mut state, pos);
            assert_eq!(state.settings.permission_mode(), m, "{m:?}");
            assert_eq!(state.settings.skip_permissions, m.skip_permissions(), "{m:?}");
            assert!(state.settings_dirty, "{m:?}");
        }
    }

    /// The tools tab carries the Bypass section.
    #[test]
    fn the_tools_tab_carries_the_bypass_section() {
        let ctx = context();
        let mut state = bypass_state();
        let texts = tall_page(&ctx, &mut state, Vec::new());
        for part in ["Bypass Mode", "Use Bypass Mode", "Bypass provider", "Requests before switching back", "Instruction files", "No files"] {
            assert!(shows(&texts, part), "{part:?} missing from {texts:?}");
        }
        assert!(shows(&texts, "The first requests of each message go to a provider you choose"));
        // Off by default, nothing is picked, and drawing changes nothing.
        assert!(!state.settings.bypass.enabled);
        assert!(!state.settings_dirty);
        assert!(state.settings.bypass.provider.is_none());
    }

    /// The switch and the count are edits of the local copy, saved like every other row.
    #[test]
    fn the_switch_and_the_count_are_saved() {
        let ctx = context();
        let mut state = bypass_state();
        let texts = tall_page(&ctx, &mut state, Vec::new());
        tall_click(&ctx, &mut state, bypass_toggle_pos(&texts, "Use Bypass Mode"));
        assert!(state.settings.bypass.enabled, "the switch turned on");
        assert!(state.settings_dirty);
        let saved = state.settings.clone().sanitized();
        assert!(saved.bypass.enabled && saved.bypass.requests >= 1);
    }

    /// The picker lists every profile, picks one, and offers "Add provider…".
    #[test]
    fn the_picker_chooses_the_bypass_provider() {
        let ctx = context();
        let mut state = bypass_state();
        let texts = tall_page(&ctx, &mut state, Vec::new());
        tall_click(&ctx, &mut state, bypass_picker_pos(&texts, "Bypass provider"));
        let texts = tall_page(&ctx, &mut state, Vec::new());
        assert!(shows(&texts, "Profile a · m-a") && shows(&texts, "Profile c · m-c"), "{texts:?}");
        let pick = *rects_of(&texts, "Profile c · m-c").last().expect("the entry");
        tall_click(&ctx, &mut state, pick.center());
        assert_eq!(state.settings.bypass.provider.as_deref(), Some("c"));
        assert!(state.settings_dirty);
        // The picker's label follows it.
        assert!(shows(&tall_page(&ctx, &mut state, Vec::new()), "Profile c · m-c"));
    }

    /// "Add provider…" opens the Providers tab's dialog on this tab, and what it saves becomes
    /// the mode's provider.
    #[test]
    fn a_provider_added_here_becomes_the_bypass_provider() {
        let ctx = context();
        let mut state = bypass_state();
        let texts = tall_page(&ctx, &mut state, Vec::new());
        tall_click(&ctx, &mut state, bypass_picker_pos(&texts, "Bypass provider"));
        tall_click_button(&ctx, &mut state, "Add provider…");
        assert!(state.provider_dialog.is_some() && state.bypass_dialog, "the dialog opened here");
        assert!(shows(&tall_page(&ctx, &mut state, Vec::new()), "Add provider"), "and is drawn on this tab");

        tall_click_text(&ctx, &mut state, "API key");
        tall_type(&ctx, &mut state, "k-bypass");
        tall_click_button(&ctx, &mut state, "Save");
        let saved = state.settings.providers.last().expect("the new profile");
        assert_eq!(state.settings.bypass.provider.as_deref(), Some(saved.id.as_str()), "the mode names it");
        assert!(!state.bypass_dialog, "the dialog is done");
        assert!(state.settings_dirty);
        check_saved(&state);
    }

    /// A saved instruction file is listed with its name, and its Remove takes that row.
    #[test]
    fn instruction_files_are_listed_and_removed() {
        let ctx = context();
        let mut state = bypass_state();
        state.apply(AppEvent::BypassFiles(vec![std::path::PathBuf::from(r"C:\rules\house.md"), std::path::PathBuf::from(r"C:\rules\tone.md")]));
        let texts = tall_page(&ctx, &mut state, Vec::new());
        assert!(shows(&texts, "house.md") && shows(&texts, "tone.md"), "{texts:?}");
        assert!(state.settings_dirty);
        // The same file again is not added twice.
        state.apply(AppEvent::BypassFiles(vec![std::path::PathBuf::from(r"C:\rules\house.md")]));
        assert_eq!(state.settings.bypass.files.len(), 2);
        // The Remove beside the first row.
        let row = rects_of(&texts, "house.md")[0];
        let x = texts
            .iter()
            .filter(|(t, r)| t == icons::X && (r.center().y - row.center().y).abs() < 20.0 && r.left() > row.left())
            .map(|(_, r)| r.center())
            .next()
            .expect("the first row's Remove");
        tall_click(&ctx, &mut state, x);
        assert_eq!(state.settings.bypass.files.len(), 1, "the first row's Remove took that file");
        assert!(shows(&tall_page(&ctx, &mut state, Vec::new()), "tone.md"));
    }

    /// "Add .md file…" asks the backend for a picker; nothing is read here.
    #[test]
    fn the_add_file_button_asks_for_a_picker() {
        let ctx = context();
        let mut state = bypass_state();
        let mut commands = Vec::new();
        let texts = crate::ui::test_util::painted_in(&ctx, TALL, Vec::new(), |ui| {
            egui::CentralPanel::default().show(ui, |ui| super::sections(ui, &mut state, &mut commands));
        });
        let pos = texts.iter().find(|(t, _)| t.ends_with("Add .md file…")).map(|(_, r)| r.center()).expect("the button");
        let mut sent = Vec::new();
        for events in [vec![egui::Event::PointerMoved(pos), click(pos, true)], vec![click(pos, false)]] {
            commands.clear();
            crate::ui::test_util::painted_in(&ctx, TALL, events, |ui| {
                egui::CentralPanel::default().show(ui, |ui| super::sections(ui, &mut state, &mut commands));
            });
            sent.append(&mut commands);
        }
        assert!(matches!(sent.as_slice(), [AppCommand::PickBypassFiles]), "{sent:?}");
    }

    /// Opening the dialog from the Bypass section and switching tabs hands it to the Providers
    /// tab, which draws the same dialog; a profile it saves there is not a bypass pick.
    #[test]
    fn the_bypass_dialog_belongs_to_its_tab() {
        let ctx = context();
        let mut state = bypass_state();
        state.provider_dialog = Some(ProviderDialog::add(ProviderKind::OpenAi));
        state.bypass_dialog = true;
        // Two frames: a modal fades in, and the frame that opens it paints nothing yet.
        let _ = tall_page(&ctx, &mut state, Vec::new());
        assert!(shows(&tall_page(&ctx, &mut state, Vec::new()), "Add provider"), "drawn on Tools");
        // The Providers tab draws it too (its section calls the same function).
        state.customize_tab = CustomizeTab::Providers;
        state.bypass_dialog = false;
        assert!(shows(&page(&ctx, &mut state, Vec::new()), "Add provider"), "drawn on Providers");
        // Back on Tools with the flag off, the Tools tab does not draw it: that is what the flag
        // means, and it is cleared whenever the tab changes.
        state.customize_tab = CustomizeTab::Agent;
        assert!(!shows(&tall_page(&ctx, &mut state, Vec::new()), "Add provider"), "not drawn twice");
    }

    /// With no profiles at all the section says what it needs instead of offering an empty menu.
    #[test]
    fn the_section_explains_a_missing_provider() {
        let ctx = context();
        let mut state = bypass_state();
        state.settings.providers.clear();
        let texts = tall_page(&ctx, &mut state, Vec::new());
        assert!(shows(&texts, "Bypass Mode needs a provider profile."), "{texts:?}");
        assert!(shows(&texts, "Choose a provider"));
    }
}

/// Customizations > Account: the existing-session row and what sign-in needs.
#[cfg(test)]
mod account_tests {
    use super::*;
    use crate::app::AppEvent;
    use crate::auth::antigravity::{ImportSummary, Issuer, SessionInfo};
    use crate::ui::test_util::{click, context, painted};

    /// One frame of the sections (no overlay): the painted texts and the commands it sent.
    fn draw(ctx: &egui::Context, state: &mut UiState, events: Vec<egui::Event>) -> (Vec<(String, Rect)>, Vec<UiAction>) {
        let mut commands = Vec::new();
        let texts = painted(ctx, events, |ui| {
            egui::CentralPanel::default().show(ui, |ui| super::sections(ui, state, &mut commands));
        });
        (texts, commands)
    }

    fn shows(texts: &[(String, Rect)], part: &str) -> bool {
        texts.iter().any(|(t, _)| t.contains(part))
    }

    /// Click the button labelled `label` (its text may start with an icon); the commands sent.
    fn click_button(ctx: &egui::Context, state: &mut UiState, label: &str) -> Vec<UiAction> {
        let (texts, _) = draw(ctx, state, Vec::new());
        let pos = texts.iter().find(|(t, _)| t.ends_with(label)).map(|(_, r)| r.center()).unwrap_or_else(|| panic!("no {label:?} button in {texts:?}"));
        let (_, mut sent) = draw(ctx, state, vec![egui::Event::PointerMoved(pos), click(pos, true)]);
        sent.extend(draw(ctx, state, vec![click(pos, false)]).1);
        sent
    }

    fn summary(usable: bool) -> ImportSummary {
        let expiry = if usable { chrono::Utc::now() + chrono::Duration::minutes(40) } else { "2026-10-01T12:00:00Z".parse().unwrap() };
        ImportSummary {
            source: std::path::PathBuf::from(r"C:\Users\me\.gemini\oauth_creds.json"),
            issuer: Issuer::Antigravity,
            account: Some("me@example.com".into()),
            expiry: Some(expiry),
            has_refresh_token: true,
            code_assist_scope: Some(true),
            client_configured: true,
            can_refresh: false,
            usable,
        }
    }

    fn account_with(check: ImportCheck) -> UiState {
        UiState { customize_tab: CustomizeTab::Account, import_check: Some(check), ..UiState::default() }
    }

    #[test]
    fn an_expired_session_says_why_and_what_to_do_instead() {
        let ctx = context();
        let mut state = account_with(ImportCheck::Found(summary(false)));
        let (texts, _) = draw(&ctx, &mut state, Vec::new());
        assert!(shows(&texts, "Antigravity session · me@example.com"), "{texts:?}");
        assert!(shows(&texts, "issued to Antigravity's own sign-in client and expired on 2026-10-01. AntiCraft cannot renew it, because only Antigravity"));
        assert!(shows(&texts, "Sign in with Google using the same account: it gives the same Code Assist access."));
        assert!(shows(&texts, r"File: C:\Users\me\.gemini\oauth_creds.json"));
        assert!(!shows(&texts, "Use this session"), "nothing usable, nothing offered");
        let sent = click_button(&ctx, &mut state, "Check again");
        assert!(matches!(sent.as_slice(), [AppCommand::CheckImportedSession]), "{sent:?}");
    }

    #[test]
    fn a_usable_session_is_offered_only_without_anticrafts_own_sign_in() {
        let ctx = context();
        let mut state = account_with(ImportCheck::Found(summary(true)));
        let sent = click_button(&ctx, &mut state, "Use this session");
        assert!(matches!(sent.as_slice(), [AppCommand::UseImportedSession]), "{sent:?}");

        state.auth = AuthStatus::LoggedIn { email: Some("own@example.com".into()) };
        let (texts, _) = draw(&ctx, &mut state, Vec::new());
        assert!(!shows(&texts, "Use this session"));
        assert!(shows(&texts, "AntiCraft's own sign-in is in use and takes precedence."));

        // In use: how long it lasts and a way to stop; the sign-in row still offers AntiCraft's own.
        let s = summary(true);
        state.imported_session = Some(SessionInfo { source: s.source, issuer: s.issuer, account: s.account, expiry: s.expiry, can_refresh: false });
        state.auth = AuthStatus::LoggedIn { email: Some("me@example.com".into()) };
        let (texts, _) = draw(&ctx, &mut state, Vec::new());
        assert!(shows(&texts, "Not signed in with AntiCraft"));
        assert!(shows(&texts, "AntiCraft cannot renew it, so it re-reads the file before then"));
        let sent = click_button(&ctx, &mut state, "Stop using");
        assert!(matches!(sent.as_slice(), [AppCommand::StopImportedSession]), "{sent:?}");
    }

    // --- Accounts -------------------------------------------------------------------------

    const AG: &str = "antigravity-0123abcd";
    const CODEX: &str = "codex-0123abcd";

    fn signed_in(id: &str, provider: AccountProvider, email: &str, plan: Option<&str>, model: &str) -> AccountView {
        AccountView {
            id: id.into(),
            provider,
            email: Some(email.into()),
            plan: plan.map(Into::into),
            model: model.into(),
            models: vec![model.into()],
            models_fetched_at: Some(chrono::Utc::now()),
            status: AccountStatus::SignedIn,
        }
    }

    /// The Account tab with an Antigravity account in use and a Codex account beside it.
    fn with_accounts() -> UiState {
        let mut state = UiState { customize_tab: CustomizeTab::Account, ..UiState::default() };
        let views = vec![
            signed_in(AG, AccountProvider::Antigravity, "me@example.com", None, "gemini-3-flash"),
            signed_in(CODEX, AccountProvider::Codex, "dev@example.com", Some("plus"), "gpt-5.1-codex"),
        ];
        state.settings.accounts = views
            .iter()
            .map(|v| crate::settings::AccountEntry { id: v.id.clone(), provider: v.provider, label: v.email.clone().unwrap(), model: v.model.clone() })
            .collect();
        state.settings.active_account = Some(AG.into());
        state.apply(AppEvent::Accounts(views));
        state
    }

    /// The backend's report of a sign-in that started: the attempt, then the rows with its
    /// placeholder.
    fn start_sign_in(state: &mut UiState, provider: AccountProvider, attempt: u64) {
        state.apply(AppEvent::AccountLogin { login: Some(AccountLoginInfo { id: attempt, provider, auth_url: "https://auth.example/authorize?state=s".into(), error: None }) });
        let mut rows: Vec<AccountView> = state.accounts.iter().filter(|v| !v.id.is_empty()).cloned().collect();
        rows.push(AccountView {
            id: String::new(),
            provider,
            email: None,
            plan: None,
            model: String::new(),
            models: Vec::new(),
            models_fetched_at: None,
            status: AccountStatus::SigningIn,
        });
        state.apply(AppEvent::Accounts(rows));
    }

    /// The backend's report of a sign-in that stopped and is kept for a pasted link.
    fn stop_sign_in(state: &mut UiState, provider: AccountProvider, attempt: u64, error: &str) {
        state.apply(AppEvent::AccountLogin {
            login: Some(AccountLoginInfo { id: attempt, provider, auth_url: "https://auth.example/authorize?state=s".into(), error: Some(error.into()) }),
        });
        let rows: Vec<AccountView> = state.accounts.iter().filter(|v| !v.id.is_empty()).cloned().collect();
        state.apply(AppEvent::Accounts(rows));
    }

    /// Where the button ending with `button` is on the row whose label is `row_label`.
    fn button_in_row(texts: &[(String, Rect)], row_label: &str, button: &str) -> egui::Pos2 {
        let row = texts.iter().find(|(t, _)| t == row_label).map(|(_, r)| *r).unwrap_or_else(|| panic!("no row {row_label:?} in {texts:?}"));
        texts
            .iter()
            .filter(|(t, r)| t.ends_with(button) && r.left() > row.right())
            .min_by(|a, b| (a.1.center().y - row.center().y).abs().total_cmp(&(b.1.center().y - row.center().y).abs()))
            .filter(|(_, r)| (r.center().y - row.center().y).abs() < 40.0)
            .map(|(_, r)| r.center())
            .unwrap_or_else(|| panic!("no {button:?} on the row {row_label:?}"))
    }

    /// Press and release at `pos`; the commands of both frames.
    fn click_at(ctx: &egui::Context, state: &mut UiState, pos: egui::Pos2) -> Vec<UiAction> {
        let (_, mut sent) = draw(ctx, state, vec![egui::Event::PointerMoved(pos), click(pos, true)]);
        sent.extend(draw(ctx, state, vec![click(pos, false)]).1);
        sent
    }

    fn click_in_row(ctx: &egui::Context, state: &mut UiState, row_label: &str, button: &str) -> Vec<UiAction> {
        let (texts, _) = draw(ctx, state, Vec::new());
        let pos = button_in_row(&texts, row_label, button);
        click_at(ctx, state, pos)
    }

    fn top_of(texts: &[(String, Rect)], text: &str) -> f32 {
        texts.iter().find(|(t, _)| t == text).map(|(_, r)| r.top()).unwrap_or_else(|| panic!("no {text:?}"))
    }

    #[test]
    fn the_accounts_come_first_with_antigravity_leading() {
        let ctx = context();
        let mut state = UiState { customize_tab: CustomizeTab::Account, ..UiState::default() };
        let (texts, sent) = draw(&ctx, &mut state, Vec::new());
        assert!(sent.is_empty());
        let order = ["Accounts", "Antigravity", "ChatGPT (Codex)", "Claude Code", "Gemini CLI sign-in (Google)", "OAuth client"];
        let tops: Vec<f32> = order.iter().map(|t| top_of(&texts, t)).collect();
        assert!(tops.windows(2).all(|w| w[0] < w[1]), "{order:?} at {tops:?}");
        assert!(shows(&texts, "Sign in with a subscription and use its models (Antigravity, ChatGPT Codex, Claude Code)."));
        assert!(shows(&texts, "Sign in with your Antigravity account. Tokens are stored owner-only in the data folder."));
        let terms = texts.iter().find(|(t, _)| t.starts_with("These are consumer subscription sign-ins.")).map(|(_, r)| r.top()).expect("the terms note");
        assert!(tops[3] < terms && terms < tops[4], "the note sits under the Accounts section");
        assert!(shows(&texts, "gemini-cli's OAuth client, which is not Antigravity's"), "{texts:?}");
        // Signing in with an account needs no client secret, unlike the Gemini CLI sign-in.
        state.apply(AppEvent::OAuthClientSource(None));
        let sent = click_in_row(&ctx, &mut state, "Antigravity", "Sign in");
        assert!(matches!(sent.as_slice(), [AppCommand::SignInAccount(AccountProvider::Antigravity)]), "{sent:?}");
    }

    #[test]
    fn login_error_is_shown_on_its_row_and_cleared_by_the_next_attempt() {
        let ctx = context();
        let mut state = with_accounts();
        start_sign_in(&mut state, AccountProvider::Codex, 5);
        let (texts, _) = draw(&ctx, &mut state, Vec::new());
        assert!(shows(&texts, "Finish the sign-in in your browser."));
        // One sign-in at a time: the other providers wait, and so does the Gemini CLI sign-in.
        let sent = click_in_row(&ctx, &mut state, "Claude Code", "Sign in");
        assert!(sent.is_empty(), "{sent:?}");
        let sent = click_in_row(&ctx, &mut state, "ChatGPT (Codex)", "Cancel");
        assert!(matches!(sent.as_slice(), [AppCommand::CancelAccountLogin(5)]), "by the attempt's id: {sent:?}");

        stop_sign_in(&mut state, AccountProvider::Codex, 5, "login timed out");
        let (texts, _) = draw(&ctx, &mut state, Vec::new());
        assert!(shows(&texts, "login timed out"), "the reason stays on the row");
        assert!(shows(&texts, "Paste redirect link"));
        // A report about another provider leaves it where it is.
        state.apply(AppEvent::AccountLogin { login: Some(AccountLoginInfo { id: 6, provider: AccountProvider::ClaudeCode, auth_url: String::new(), error: None }) });
        state.apply(AppEvent::AccountLogin { login: None });
        assert!(shows(&draw(&ctx, &mut state, Vec::new()).0, "login timed out"));

        // The next attempt clears it at once, before the backend answers.
        let sent = click_in_row(&ctx, &mut state, "ChatGPT (Codex)", "Sign in");
        assert!(matches!(sent.as_slice(), [AppCommand::SignInAccount(AccountProvider::Codex)]), "{sent:?}");
        assert!(!state.account_errors.contains_key(&AccountProvider::Codex));
        assert!(!shows(&draw(&ctx, &mut state, Vec::new()).0, "login timed out"));
    }

    #[test]
    fn account_rows_emit_the_right_commands() {
        let ctx = context();
        let mut state = with_accounts();
        let (texts, _) = draw(&ctx, &mut state, Vec::new());
        for part in ["me@example.com", "dev@example.com · plus", "Antigravity · gemini-3-flash", "ChatGPT (Codex) · gpt-5.1-codex", "File: accounts/codex-0123abcd.json", "Model list last checked "] {
            assert!(shows(&texts, part), "{part:?} missing from {texts:?}");
        }
        assert!(button_in_row(&texts, "me@example.com", "Current").x > 0.0, "the account in use");

        let sent = click_in_row(&ctx, &mut state, "dev@example.com · plus", "Sign out");
        assert!(matches!(sent.as_slice(), [AppCommand::SignOutAccount(id)] if id == CODEX), "{sent:?}");
        let sent = click_in_row(&ctx, &mut state, "dev@example.com · plus", "Use");
        assert!(matches!(sent.as_slice(), [AppCommand::UseAccount(id)] if id == CODEX), "{sent:?}");
        assert_eq!(state.settings.active_account.as_deref(), Some(CODEX), "the pill moves at once");
        let (texts, _) = draw(&ctx, &mut state, Vec::new());
        button_in_row(&texts, "dev@example.com · plus", "Current");
        button_in_row(&texts, "me@example.com", "Use");

        let sent = click_in_row(&ctx, &mut state, "me@example.com", "Models");
        assert!(sent.is_empty(), "opening the popup sends nothing");
        let (texts, _) = draw(&ctx, &mut state, Vec::new());
        assert!(shows(&texts, "Supported models · me@example.com"), "{texts:?}");
        let sent = click_in_row(&ctx, &mut state, "Claude Code", "Sign in");
        assert!(matches!(sent.as_slice(), [AppCommand::SignInAccount(AccountProvider::ClaudeCode)]), "{sent:?}");

        // Mid-turn nothing that changes the backend in use can be pressed.
        state.busy = true;
        for (row, button) in [("me@example.com", "Sign out"), ("me@example.com", "Use"), ("Antigravity", "Sign in")] {
            assert!(click_in_row(&ctx, &mut state, row, button).is_empty(), "{row} / {button}");
        }
    }

    #[test]
    fn a_sign_in_blocks_signing_out_of_that_provider_only() {
        let ctx = context();
        let mut state = with_accounts();
        start_sign_in(&mut state, AccountProvider::Codex, 9);
        assert!(click_in_row(&ctx, &mut state, "dev@example.com · plus", "Sign out").is_empty(), "the sign-in may renew this very account");
        let sent = click_in_row(&ctx, &mut state, "me@example.com", "Sign out");
        assert!(matches!(sent.as_slice(), [AppCommand::SignOutAccount(id)] if id == AG), "{sent:?}");
        // The Gemini CLI sign-in waits as well.
        state.oauth_source = Some(ClientSource::File);
        let (texts, _) = draw(&ctx, &mut state, Vec::new());
        let google = texts.iter().filter(|(t, _)| t == "Sign in").map(|(_, r)| r.center()).max_by(|a, b| a.y.total_cmp(&b.y)).expect("the Gemini CLI Sign in");
        assert!(click_at(&ctx, &mut state, google).is_empty());
    }

    #[test]
    fn a_pasted_link_finishes_the_kept_attempt() {
        let ctx = context();
        let mut state = with_accounts();
        start_sign_in(&mut state, AccountProvider::Codex, 11);
        stop_sign_in(&mut state, AccountProvider::Codex, 11, "port 1455 on localhost is already in use");
        assert!(click_in_row(&ctx, &mut state, "ChatGPT (Codex)", "Paste redirect link").is_empty());
        let (texts, _) = draw(&ctx, &mut state, Vec::new());
        assert!(shows(&texts, "Open this link in the browser, then paste the address it landed on."), "{texts:?}");
        assert!(shows(&texts, "https://auth.example/authorize?state=s"));

        // Clicking into the field keeps the popup open, and the text goes to the field.
        let field = texts.iter().find(|(t, _)| t == "Paste the redirect URL").map(|(_, r)| r.center()).expect("the field");
        click_at(&ctx, &mut state, field);
        draw(&ctx, &mut state, vec![egui::Event::Text("not a link".into())]);
        assert_eq!(state.account_link_input, "not a link");
        let (texts, _) = draw(&ctx, &mut state, Vec::new());
        let submit = texts.iter().find(|(t, _)| t == "Submit").map(|(_, r)| r.center()).expect("Submit");
        // Text that is no redirect link is answered here, and nothing is sent.
        assert!(click_at(&ctx, &mut state, submit).is_empty());
        let (texts, _) = draw(&ctx, &mut state, Vec::new());
        assert!(shows(&texts, "That does not look like a redirect URL."), "{texts:?}");
        assert!(!texts.iter().any(|(t, _)| t.contains("That does not look like a redirect URL.") && t.contains("not a link")), "the input is never echoed");

        // The message line moved the button down.
        let submit = texts.iter().find(|(t, _)| t == "Submit").map(|(_, r)| r.center()).expect("Submit");
        let link = "http://localhost:1455/auth/callback?code=c0de&state=s";
        state.account_link_input = link.into();
        let sent = click_at(&ctx, &mut state, submit);
        assert!(matches!(sent.as_slice(), [AppCommand::PasteAccountLink { login: 11, link: l }] if l.as_str() == link), "{sent:?}");
        assert!(state.account_link_input.is_empty(), "the code does not stay in the field");
        let (texts, _) = draw(&ctx, &mut state, Vec::new());
        assert!(!shows(&texts, "Submit"), "the popup closed");

        // Cancel lets the attempt go.
        assert!(click_in_row(&ctx, &mut state, "ChatGPT (Codex)", "Paste redirect link").is_empty());
        let (texts, _) = draw(&ctx, &mut state, Vec::new());
        let cancel = texts.iter().find(|(t, _)| t == "Cancel").map(|(_, r)| r.center()).expect("Cancel");
        let sent = click_at(&ctx, &mut state, cancel);
        assert!(matches!(sent.as_slice(), [AppCommand::ForgetAccountLogin(11)]), "{sent:?}");

        // Text typed and left behind is dropped, field state included, once the attempt is gone.
        assert!(click_in_row(&ctx, &mut state, "ChatGPT (Codex)", "Paste redirect link").is_empty());
        let (texts, _) = draw(&ctx, &mut state, Vec::new());
        let field = texts.iter().find(|(t, _)| t == "Paste the redirect URL").map(|(_, r)| r.center()).expect("the field");
        click_at(&ctx, &mut state, field);
        draw(&ctx, &mut state, vec![egui::Event::Text("code=left-behind".into())]);
        let key = paste_field_key();
        let field_id: egui::Id = ctx.data(|d| d.get_temp(key)).expect("the field is known");
        assert!(egui::text_edit::TextEditState::load(&ctx, field_id).is_some());
        state.apply(AppEvent::AccountLogin { login: None });
        assert!(state.account_link_input.is_empty());
        draw(&ctx, &mut state, Vec::new());
        assert!(egui::text_edit::TextEditState::load(&ctx, field_id).is_none(), "the undo history is gone");
    }

    #[test]
    fn an_expired_account_says_so_and_offers_sign_in() {
        let ctx = context();
        let mut state = with_accounts();
        let mut rows = state.accounts.clone();
        rows[1].status = AccountStatus::Expired("session expired, please log in again".into());
        state.apply(AppEvent::Accounts(rows));
        let (texts, _) = draw(&ctx, &mut state, Vec::new());
        assert!(shows(&texts, "dev@example.com · Session expired"), "{texts:?}");
        assert!(shows(&texts, "session expired, please log in again"));
        let sent = click_in_row(&ctx, &mut state, "dev@example.com · Session expired", "Sign in");
        assert!(matches!(sent.as_slice(), [AppCommand::SignInAccount(AccountProvider::Codex)]), "{sent:?}");
    }

    #[test]
    fn the_import_row_gives_way_to_an_antigravity_account() {
        let ctx = context();
        let mut state = account_with(ImportCheck::Found(summary(true)));
        assert!(shows(&draw(&ctx, &mut state, Vec::new()).0, "Use this session"));
        let views = with_accounts().accounts;
        state.apply(AppEvent::Accounts(views));
        let (texts, _) = draw(&ctx, &mut state, Vec::new());
        assert!(!shows(&texts, "Use this session") && !shows(&texts, "Antigravity session ·"), "{texts:?}");
        // In use, it stays: its "Stop using" is the way out.
        let s = summary(true);
        state.imported_session = Some(SessionInfo { source: s.source, issuer: s.issuer, account: s.account, expiry: s.expiry, can_refresh: false });
        assert!(shows(&draw(&ctx, &mut state, Vec::new()).0, "Stop using"));
    }

    #[test]
    fn nothing_found_and_a_missing_client_secret_are_explained() {
        let ctx = context();
        let mut state = account_with(ImportCheck::NotFound);
        state.apply(AppEvent::OAuthClientSource(None));
        state.apply(AppEvent::Auth(AuthStatus::Error("OAuth client is not configured".into())));
        let (texts, _) = draw(&ctx, &mut state, Vec::new());
        assert!(shows(&texts, "No existing session found"));
        assert!(shows(&texts, "public client secret of gemini-cli's installed-app OAuth client"));
        assert!(shows(&texts, "packages/core/src/code_assist/oauth2.ts"));
        assert!(shows(&texts, CLIENT_SECRET_ENV));
        assert!(!shows(&texts, "GOCSPX"), "no secret is printed");
    }
}
