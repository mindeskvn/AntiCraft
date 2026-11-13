//! Design tokens (DESIGN.md section 1), font setup and egui style overrides.

use egui::{Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin, Stroke, Style, TextStyle, Visuals};

pub const BG: Color32 = Color32::from_rgb(0x10, 0x10, 0x10);
pub const FG: Color32 = Color32::from_rgb(0xcc, 0xcc, 0xcc);
pub const FG_SECONDARY: Color32 = Color32::from_rgb(0x9d, 0x9d, 0x9d);
pub const FG_MUTED: Color32 = Color32::from_rgb(0x6e, 0x6e, 0x6e);
pub const PLACEHOLDER: Color32 = Color32::from_rgb(0x47, 0x47, 0x47);
pub const PRIMARY: Color32 = Color32::from_rgb(0x00, 0x7a, 0xcc);
pub const LINK: Color32 = Color32::from_rgb(0x6a, 0x9f, 0xcb);
pub const SUCCESS: Color32 = Color32::from_rgb(0x7a, 0xae, 0x66);
pub const WARNING: Color32 = Color32::from_rgb(0xf3, 0xc9, 0x49);
pub const ERROR: Color32 = Color32::from_rgb(0xde, 0x55, 0x55);
pub const SIDEBAR: Color32 = Color32::from_rgb(0x16, 0x16, 0x16);
/// Sidebar right edge / title band separator.
pub const EDGE: Color32 = Color32::from_rgb(0x1c, 0x1c, 0x1c);
/// Recessed fill for text fields and segmented tracks.
pub const FIELD_BG: Color32 = Color32::from_rgb(0x16, 0x16, 0x16);
/// Icon accents used by settings rows.
pub const ACCENT_BLUE: Color32 = Color32::from_rgb(0x34, 0x87, 0xff);
pub const ACCENT_AMBER: Color32 = Color32::from_rgb(0xf5, 0xbf, 0x0a);
pub const ACCENT_RED: Color32 = Color32::from_rgb(0xe0, 0x4a, 0x2a);
pub const CARD: Color32 = Color32::from_rgb(0x1c, 0x1c, 0x1c);
pub const CARD_BORDER: Color32 = Color32::from_rgb(0x25, 0x25, 0x25);
/// User bubble, popup frame and separator border (`#282828`).
pub const POPUP_BORDER: Color32 = Color32::from_rgb(0x28, 0x28, 0x28);
/// Thin border used by the customizations search field and tab track.
pub const TRACK_BORDER: Color32 = Color32::from_rgb(0x22, 0x22, 0x22);
/// Toggle "on" / accent blue measured on the reference.
pub const TOGGLE_ON: Color32 = Color32::from_rgb(0x19, 0xb2, 0x6b);
pub const BORDER: Color32 = Color32::from_rgb(0x1b, 0x1b, 0x1b);
pub const SECONDARY: Color32 = Color32::from_rgb(0x2f, 0x2f, 0x2f);
pub const ROW_SELECTED: Color32 = Color32::from_rgb(0x32, 0x32, 0x32);
/// 1px edge of the selected nav row (slightly lighter than the fill, as measured).
pub const ROW_SELECTED_EDGE: Color32 = Color32::from_rgb(0x36, 0x36, 0x36);
/// Border of the boxed "New Conversation" field when it is not the current view.
pub const FIELD_BORDER: Color32 = Color32::from_rgb(0x28, 0x28, 0x28);
pub const ROW_HOVER: Color32 = Color32::from_rgba_premultiplied(10, 10, 10, 10);
pub const SEND_BG: Color32 = Color32::from_rgb(0x2d, 0x2d, 0x2d);
pub const PRESSED: Color32 = Color32::from_rgb(0x3a, 0x3a, 0x3a);

pub const SIDEBAR_WIDTH: f32 = 235.0;
pub const COLUMN_WIDTH: f32 = 768.0;
pub const TOP_BAR_HEIGHT: f32 = 32.0;
pub const TOOLBAR_HEIGHT: f32 = 40.0;
pub const MESSAGE_GAP: f32 = 16.0;
pub const SECTION_GAP: f32 = 24.0;
pub const RADIUS_CARD: u8 = 10;
pub const RADIUS_CODE: u8 = 8;
pub const RADIUS_POPUP: u8 = 10;
pub const RADIUS_BUBBLE: u8 = 12;

/// Name of the icon font family (Phosphor Regular).
pub const ICON_FAMILY: &str = "phosphor";
static PHOSPHOR: &[u8] = include_bytes!("../../assets/Phosphor-Regular.ttf");

pub fn font(size: f32) -> FontId {
    FontId::proportional(size)
}

pub fn medium(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("medium".into()))
}

pub fn mono(size: f32) -> FontId {
    FontId::monospace(size)
}

pub fn icon(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(ICON_FAMILY.into()))
}

/// Read a system font (never bundled). Missing files fall back to egui defaults.
fn system_font(name: &str) -> Option<Vec<u8>> {
    let dir = std::env::var_os("WINDIR").map(std::path::PathBuf::from).unwrap_or_else(|| r"C:\Windows".into());
    std::fs::read(dir.join("Fonts").join(name)).ok()
}

/// Font definitions: Segoe UI (runtime-loaded) for text, Segoe UI Semibold for "medium",
/// Cascadia Mono / Consolas for code, Phosphor as icon font and proportional fallback.
pub fn fonts() -> FontDefinitions {
    let mut f = FontDefinitions::default();
    let add = |f: &mut FontDefinitions, key: &str, bytes: Option<Vec<u8>>| -> bool {
        match bytes {
            Some(b) => {
                f.font_data.insert(key.into(), FontData::from_owned(b).into());
                true
            }
            None => false,
        }
    };
    let have_ui = add(&mut f, "segoe", system_font("segoeui.ttf"));
    let have_sb = add(&mut f, "segoe-sb", system_font("seguisb.ttf").or_else(|| system_font("segoeuib.ttf")));
    let have_mono = add(&mut f, "mono", system_font("CascadiaMono.ttf").or_else(|| system_font("consola.ttf")));
    let have_sym = add(&mut f, "segoe-sym", system_font("seguisym.ttf"));
    f.font_data.insert(ICON_FAMILY.into(), FontData::from_static(PHOSPHOR).into());

    let prop = f.families.entry(FontFamily::Proportional).or_default();
    if have_ui {
        prop.insert(0, "segoe".into());
    }
    prop.insert(prop.len().min(1), ICON_FAMILY.into());
    if have_sym {
        prop.push("segoe-sym".into());
    }
    let prop = prop.clone();
    if have_mono {
        f.families.entry(FontFamily::Monospace).or_default().insert(0, "mono".into());
    }
    let mut med = prop.clone();
    if have_sb {
        med.insert(0, "segoe-sb".into());
    }
    f.families.insert(FontFamily::Name("medium".into()), med);
    // Icon family: Phosphor first (icons only use private-use codepoints).
    let mut icons = vec![ICON_FAMILY.to_owned()];
    icons.extend(prop);
    f.families.insert(FontFamily::Name(ICON_FAMILY.into()), icons);
    f
}

fn widget(bg: Color32, fg: Color32, stroke: Stroke, radius: u8) -> egui::style::WidgetVisuals {
    egui::style::WidgetVisuals {
        bg_fill: bg,
        weak_bg_fill: bg,
        bg_stroke: stroke,
        corner_radius: CornerRadius::same(radius),
        fg_stroke: Stroke::new(1.0, fg),
        expansion: 0.0,
    }
}

/// Dark visuals matching the reference tokens.
pub fn visuals() -> Visuals {
    let mut v = Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = CARD;
    v.window_stroke = Stroke::new(1.0, CARD_BORDER);
    v.window_corner_radius = CornerRadius::same(10);
    v.menu_corner_radius = CornerRadius::same(10);
    v.extreme_bg_color = CARD;
    v.faint_bg_color = Color32::from_rgb(0x16, 0x16, 0x16);
    v.code_bg_color = CARD;
    v.hyperlink_color = LINK;
    v.warn_fg_color = WARNING;
    v.error_fg_color = ERROR;
    v.override_text_color = None;
    v.selection.bg_fill = PRIMARY.gamma_multiply(0.5);
    v.selection.stroke = Stroke::new(1.0, FG);
    v.popup_shadow = egui::epaint::Shadow { offset: [0, 8], blur: 24, spread: 0, color: Color32::from_black_alpha(128) };
    v.window_shadow = v.popup_shadow;
    v.widgets.noninteractive = widget(BG, FG, Stroke::new(1.0, BORDER), 6);
    v.widgets.inactive = widget(Color32::TRANSPARENT, FG_SECONDARY, Stroke::NONE, 6);
    v.widgets.inactive.bg_fill = SECONDARY;
    v.widgets.hovered = widget(SECONDARY, FG, Stroke::NONE, 6);
    v.widgets.active = widget(PRESSED, FG, Stroke::NONE, 6);
    v.widgets.open = widget(SECONDARY, FG, Stroke::NONE, 6);
    v.striped = false;
    v
}

pub fn style() -> Style {
    let mut s = Style { visuals: visuals(), ..Style::default() };
    s.text_styles = [
        (TextStyle::Small, font(12.0)),
        (TextStyle::Body, font(14.0)),
        (TextStyle::Button, font(13.0)),
        (TextStyle::Heading, medium(20.0)),
        (TextStyle::Monospace, mono(12.5)),
    ]
    .into();
    s.spacing.item_spacing = egui::vec2(8.0, 6.0);
    s.spacing.button_padding = egui::vec2(10.0, 5.0);
    s.spacing.menu_margin = Margin::same(6);
    s.spacing.interact_size.y = 24.0;
    s.spacing.scroll = egui::style::ScrollStyle::thin();
    s.interaction.tooltip_delay = 0.15;
    s
}

/// Install fonts and the dark style. The Light theme setting is persisted but not styled yet
/// (DESIGN.md 1): the custom-painted widgets use the dark tokens, so the app always renders dark.
pub fn apply(ctx: &egui::Context) {
    install(ctx);
}

/// Same as [`apply`] (kept for existing callers).
pub fn install(ctx: &egui::Context) {
    ctx.set_fonts(fonts());
    ctx.set_style_of(egui::Theme::Dark, style());
    ctx.set_style_of(egui::Theme::Light, style());
    ctx.set_theme(egui::ThemePreference::Dark);
}
