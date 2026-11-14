//! Design tokens (DESIGN.md section 1), font setup and egui style overrides.
//!
//! The palette follows the 2.30 visual refresh: the window chrome (menu bar, toolbar row and
//! sidebar) shares one background, and the screens sit on it as a darker rounded card.

use std::cell::Cell;

use egui::{Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin, Stroke, Style, TextStyle, Visuals};

use crate::settings::{Appearance, FontSize, UiFont};

/// Content card: conversation, side panels and the settings pages.
pub const BG: Color32 = Color32::from_rgb(0x07, 0x07, 0x08);
pub const FG: Color32 = Color32::from_rgb(0xe3, 0xe3, 0xe5);
pub const FG_SECONDARY: Color32 = Color32::from_rgb(0xbc, 0xbd, 0xc6);
pub const FG_MUTED: Color32 = Color32::from_rgb(0x85, 0x86, 0x8d);
/// Dimmest text: "See all", disabled labels.
pub const FG_DIM: Color32 = Color32::from_rgb(0x67, 0x68, 0x6e);
pub const PLACEHOLDER: Color32 = Color32::from_rgb(0x75, 0x76, 0x7c);
pub const PRIMARY: Color32 = Color32::from_rgb(0x2f, 0x70, 0xe3);
pub const LINK: Color32 = Color32::from_rgb(0x8a, 0xb0, 0xe8);
pub const SUCCESS: Color32 = Color32::from_rgb(0x7a, 0xae, 0x66);
pub const WARNING: Color32 = Color32::from_rgb(0xf3, 0xc9, 0x49);
pub const ERROR: Color32 = Color32::from_rgb(0xde, 0x55, 0x55);
/// Window chrome: menu bar, toolbar row, sidebar and the gutter around the content card.
pub const SIDEBAR: Color32 = Color32::from_rgb(0x11, 0x11, 0x14);
/// Outline of the content card and the divider between its panes.
pub const EDGE: Color32 = Color32::from_rgb(0x19, 0x19, 0x1d);
/// Recessed fill for text fields and segmented tracks.
pub const FIELD_BG: Color32 = Color32::from_rgb(0x10, 0x10, 0x13);
/// Icon accents used by settings rows.
pub const ACCENT_BLUE: Color32 = Color32::from_rgb(0x34, 0x87, 0xff);
pub const ACCENT_AMBER: Color32 = Color32::from_rgb(0xf5, 0xbf, 0x0a);
pub const ACCENT_RED: Color32 = Color32::from_rgb(0xe0, 0x4a, 0x2a);
/// Raised surfaces on the card: composer, popups, code blocks, tool cards.
pub const CARD: Color32 = Color32::from_rgb(0x15, 0x15, 0x1a);
pub const CARD_BORDER: Color32 = Color32::from_rgb(0x23, 0x23, 0x28);
/// Popup frame and separator border.
pub const POPUP_BORDER: Color32 = Color32::from_rgb(0x2a, 0x2a, 0x30);
/// Thin border used by the customizations search field and tab track.
pub const TRACK_BORDER: Color32 = Color32::from_rgb(0x1f, 0x1f, 0x24);
/// Toggle "on".
pub const TOGGLE_ON: Color32 = Color32::from_rgb(0x19, 0xb2, 0x6b);
pub const BORDER: Color32 = Color32::from_rgb(0x19, 0x19, 0x1d);
/// Hover fill of chips, buttons and segmented options.
pub const SECONDARY: Color32 = Color32::from_rgb(0x25, 0x25, 0x2b);
pub const ROW_SELECTED: Color32 = Color32::from_rgb(0x1f, 0x1f, 0x24);
/// 1px edge of the selected nav row (slightly lighter than the fill, as measured).
pub const ROW_SELECTED_EDGE: Color32 = Color32::from_rgb(0x2a, 0x2a, 0x2f);
/// Fill and border of the boxed "New Conversation" button.
pub const BOX_FILL: Color32 = Color32::from_rgb(0x1c, 0x1c, 0x1f);
pub const FIELD_BORDER: Color32 = Color32::from_rgb(0x29, 0x29, 0x2c);
pub const ROW_HOVER: Color32 = Color32::from_rgba_premultiplied(10, 10, 10, 10);
pub const SEND_BG: Color32 = Color32::from_rgb(0x25, 0x25, 0x2b);
pub const PRESSED: Color32 = Color32::from_rgb(0x30, 0x30, 0x37);
/// Your messages: right-aligned chat bubbles.
pub const BUBBLE: Color32 = Color32::from_rgb(0x1a, 0x1a, 0x1f);
pub const BUBBLE_EDGE: Color32 = Color32::from_rgb(0x24, 0x24, 0x2a);
/// Unread conversation dot in the sidebar.
pub const UNREAD: Color32 = Color32::from_rgb(0x2f, 0x70, 0xe3);

pub const SIDEBAR_WIDTH: f32 = 235.0;
/// The reading column's default width (Settings > Appearance can narrow it to 768, widen it to
/// 960, or fill the card; see [`column_max`]).
pub const COLUMN_WIDTH: f32 = 960.0;
pub const TOP_BAR_HEIGHT: f32 = 32.0;
pub const TOOLBAR_HEIGHT: f32 = 36.0;
pub const MESSAGE_GAP: f32 = 16.0;
pub const SECTION_GAP: f32 = 24.0;
pub const RADIUS_CARD: u8 = 10;
pub const RADIUS_CODE: u8 = 8;
pub const RADIUS_POPUP: u8 = 10;
pub const RADIUS_BUBBLE: u8 = 14;
/// Corner radius of the content card the screens sit on.
pub const RADIUS_SHELL: u8 = 8;
/// Gap between the content card and the window's right and bottom edges.
pub const SHELL_GUTTER: i8 = 7;
/// Below this window width the sidebar and a side panel are not shown side by side.
pub const NARROW_WINDOW: f32 = 1024.0;

/// Name of the icon font family (Phosphor Regular).
pub const ICON_FAMILY: &str = "phosphor";
static PHOSPHOR: &[u8] = include_bytes!("../../assets/Phosphor-Regular.ttf");

thread_local! {
    /// Text scale from Settings > Appearance > Font size, for this thread's frames. A thread
    /// local, not a process global: UI tests run on many threads at once and measure text.
    static TEXT_SCALE: Cell<f32> = const { Cell::new(1.0) };
    /// Conversation width from Settings > Appearance; `None` fills the card.
    static COLUMN_MAX: Cell<Option<f32>> = const { Cell::new(Some(COLUMN_WIDTH)) };
}

fn scaled(size: f32) -> f32 {
    size * TEXT_SCALE.with(Cell::get)
}

pub fn font(size: f32) -> FontId {
    FontId::proportional(scaled(size))
}

pub fn medium(size: f32) -> FontId {
    FontId::new(scaled(size), FontFamily::Name("medium".into()))
}

pub fn mono(size: f32) -> FontId {
    FontId::monospace(scaled(size))
}

/// Icons keep their size: the font size setting is about text.
pub fn icon(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(ICON_FAMILY.into()))
}

/// Multiplier of the chosen font size against the 14px default.
pub fn text_scale(size: FontSize) -> f32 {
    match size {
        FontSize::Small => 12.0 / 14.0,
        FontSize::Medium => 1.0,
        FontSize::Large => 16.0 / 14.0,
    }
}

/// Widest reading column, or `None` when the conversation fills the card.
pub fn column_max() -> Option<f32> {
    COLUMN_MAX.with(Cell::get)
}

/// Read a system font (never bundled). Missing files fall back to egui defaults.
fn system_font(name: &str) -> Option<Vec<u8>> {
    let dir = std::env::var_os("WINDIR").map(std::path::PathBuf::from).unwrap_or_else(|| r"C:\Windows".into());
    std::fs::read(dir.join("Fonts").join(name)).ok()
}

/// A font installed for all users or only for this one (`%LOCALAPPDATA%\Microsoft\Windows\Fonts`).
fn installed_font(name: &str) -> Option<Vec<u8>> {
    system_font(name).or_else(|| {
        let dir = std::env::var_os("LOCALAPPDATA").map(std::path::PathBuf::from)?;
        std::fs::read(dir.join("Microsoft").join("Windows").join("Fonts").join(name)).ok()
    })
}

/// File names a font choice may be installed under: (regular, semibold).
fn font_files(font: UiFont) -> (&'static [&'static str], &'static [&'static str]) {
    match font {
        UiFont::System => (&["segoeui.ttf"], &["seguisb.ttf", "segoeuib.ttf"]),
        UiFont::Inter => (
            &["Inter-Regular.ttf", "Inter-Regular.otf", "InterVariable.ttf", "Inter.ttf", "Inter-VariableFont_opsz,wght.ttf"],
            &["Inter-SemiBold.ttf", "Inter-SemiBold.otf", "Inter-Medium.ttf", "Inter-Medium.otf"],
        ),
        UiFont::GoogleSans => (
            &["GoogleSans-Regular.ttf", "GoogleSansText-Regular.ttf", "GoogleSansFlex-Regular.ttf", "ProductSans-Regular.ttf"],
            &["GoogleSans-Medium.ttf", "GoogleSansText-Medium.ttf", "GoogleSans-Bold.ttf", "ProductSans-Medium.ttf"],
        ),
    }
}

fn first_installed(names: &[&str]) -> Option<Vec<u8>> {
    names.iter().find_map(|n| installed_font(n))
}

/// The font choice can be used on this computer. System UI always can (it falls back to
/// egui's own font when Segoe UI is missing); Inter and Google Sans need to be installed.
pub fn font_available(font: UiFont) -> bool {
    static FOUND: std::sync::OnceLock<[bool; 2]> = std::sync::OnceLock::new();
    let found = FOUND.get_or_init(|| {
        let has = |f| {
            let (regular, _) = font_files(f);
            regular.iter().any(|n| {
                let windir = std::env::var_os("WINDIR").map(std::path::PathBuf::from).unwrap_or_else(|| r"C:\Windows".into());
                windir.join("Fonts").join(n).is_file()
                    || std::env::var_os("LOCALAPPDATA")
                        .map(std::path::PathBuf::from)
                        .is_some_and(|d| d.join("Microsoft").join("Windows").join("Fonts").join(n).is_file())
            })
        };
        [has(UiFont::Inter), has(UiFont::GoogleSans)]
    });
    match font {
        UiFont::System => true,
        UiFont::Inter => found[0],
        UiFont::GoogleSans => found[1],
    }
}

/// Font definitions: the chosen UI font (runtime-loaded, never bundled) for text and its
/// semibold for "medium", Segoe UI behind it, Cascadia Mono / Consolas for code, Phosphor as
/// icon font and proportional fallback.
pub fn fonts() -> FontDefinitions {
    fonts_for(UiFont::System)
}

pub fn fonts_for(choice: UiFont) -> FontDefinitions {
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
    let (regular, semibold) = font_files(choice);
    let have_choice = choice != UiFont::System && add(&mut f, "choice", first_installed(regular));
    let have_choice_sb = have_choice && add(&mut f, "choice-sb", first_installed(semibold));
    let have_ui = add(&mut f, "segoe", system_font("segoeui.ttf"));
    let have_sb = add(&mut f, "segoe-sb", system_font("seguisb.ttf").or_else(|| system_font("segoeuib.ttf")));
    let have_mono = add(&mut f, "mono", system_font("CascadiaMono.ttf").or_else(|| system_font("consola.ttf")));
    let have_sym = add(&mut f, "segoe-sym", system_font("seguisym.ttf"));
    f.font_data.insert(ICON_FAMILY.into(), FontData::from_static(PHOSPHOR).into());

    let prop = f.families.entry(FontFamily::Proportional).or_default();
    if have_ui {
        prop.insert(0, "segoe".into());
    }
    if have_choice {
        prop.insert(0, "choice".into());
    }
    let after_text = prop.len().min(if have_choice && have_ui { 2 } else { 1 });
    prop.insert(after_text, ICON_FAMILY.into());
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
    if have_choice_sb {
        med.insert(0, "choice-sb".into());
    } else if have_choice {
        // No semibold file: the regular face keeps "medium" text in the chosen font.
        med.insert(0, "choice".into());
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
    v.window_stroke = Stroke::new(1.0, POPUP_BORDER);
    v.window_corner_radius = CornerRadius::same(10);
    v.menu_corner_radius = CornerRadius::same(10);
    v.extreme_bg_color = FIELD_BG;
    v.faint_bg_color = FIELD_BG;
    v.code_bg_color = CARD;
    v.hyperlink_color = LINK;
    v.warn_fg_color = WARNING;
    v.error_fg_color = ERROR;
    v.override_text_color = None;
    v.selection.bg_fill = PRIMARY.gamma_multiply(0.5);
    v.selection.stroke = Stroke::new(1.0, FG);
    v.popup_shadow = egui::epaint::Shadow { offset: [0, 8], blur: 24, spread: 0, color: Color32::from_black_alpha(160) };
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
    s.text_styles = text_styles();
    s.spacing.item_spacing = egui::vec2(8.0, 6.0);
    s.spacing.button_padding = egui::vec2(10.0, 5.0);
    s.spacing.menu_margin = Margin::same(6);
    s.spacing.interact_size.y = 24.0;
    s.spacing.scroll = egui::style::ScrollStyle::thin();
    s.interaction.tooltip_delay = 0.15;
    s
}

/// egui's own text styles, at the current text scale.
fn text_styles() -> std::collections::BTreeMap<TextStyle, FontId> {
    [
        (TextStyle::Small, font(12.0)),
        (TextStyle::Body, font(14.0)),
        (TextStyle::Button, font(13.0)),
        (TextStyle::Heading, medium(20.0)),
        (TextStyle::Monospace, mono(12.5)),
    ]
    .into()
}

/// Install fonts and the dark style at the default appearance. The Light theme setting is
/// persisted but not styled yet (DESIGN.md 1): the custom-painted widgets use the dark tokens,
/// so the app always renders dark.
pub fn apply(ctx: &egui::Context) {
    install(ctx);
}

/// Same as [`apply`] (kept for existing callers).
pub fn install(ctx: &egui::Context) {
    TEXT_SCALE.with(|s| s.set(1.0));
    COLUMN_MAX.with(|c| c.set(Some(COLUMN_WIDTH)));
    ctx.set_fonts(fonts());
    ctx.set_style_of(egui::Theme::Dark, style());
    ctx.set_style_of(egui::Theme::Light, style());
    ctx.set_theme(egui::ThemePreference::Dark);
    ctx.data_mut(|d| d.insert_temp(applied_id(), Appearance::default()));
}

fn applied_id() -> egui::Id {
    egui::Id::unique("anticraft_applied_appearance")
}

/// Apply Settings > Appearance for this frame: the text scale and column width every frame
/// (they are thread locals), the fonts and egui's text styles only when they changed.
pub fn set_appearance(ctx: &egui::Context, appearance: &Appearance) {
    TEXT_SCALE.with(|s| s.set(text_scale(appearance.font_size)));
    COLUMN_MAX.with(|c| c.set(appearance.width.max()));
    let last = ctx.data(|d| d.get_temp::<Appearance>(applied_id()));
    if last.as_ref() == Some(appearance) {
        return;
    }
    if last.map(|l| l.font) != Some(appearance.font) {
        ctx.set_fonts(fonts_for(appearance.font));
    }
    for theme in [egui::Theme::Dark, egui::Theme::Light] {
        ctx.style_mut_of(theme, |s| s.text_styles = text_styles());
    }
    ctx.data_mut(|d| d.insert_temp(applied_id(), *appearance));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::ConversationWidth;

    /// The font size scales text from this thread only, and `install` puts it back.
    #[test]
    fn font_size_scales_text_but_not_icons() {
        let ctx = egui::Context::default();
        install(&ctx);
        assert_eq!(font(14.0).size, 14.0);
        set_appearance(&ctx, &Appearance { font_size: FontSize::Large, ..Appearance::default() });
        assert_eq!(font(14.0).size, 16.0);
        assert_eq!(medium(14.0).size, 16.0);
        assert_eq!(icon(14.0).size, 14.0);
        assert_eq!(ctx.global_style().text_styles[&TextStyle::Body].size, 16.0);
        set_appearance(&ctx, &Appearance { font_size: FontSize::Small, ..Appearance::default() });
        assert_eq!(font(14.0).size, 12.0);
        let other = std::thread::spawn(|| font(14.0).size).join().unwrap();
        assert_eq!(other, 14.0, "another thread keeps the default");
        install(&ctx);
        assert_eq!(font(14.0).size, 14.0);
    }

    #[test]
    fn conversation_width_sets_the_column() {
        let ctx = egui::Context::default();
        install(&ctx);
        assert_eq!(column_max(), Some(960.0));
        set_appearance(&ctx, &Appearance { width: ConversationWidth::Default, ..Appearance::default() });
        assert_eq!(column_max(), Some(768.0));
        set_appearance(&ctx, &Appearance { width: ConversationWidth::Wide, ..Appearance::default() });
        assert_eq!(column_max(), Some(960.0));
        set_appearance(&ctx, &Appearance { width: ConversationWidth::Fill, ..Appearance::default() });
        assert_eq!(column_max(), None);
        install(&ctx);
    }

    /// A font that is not installed keeps the window on Segoe UI instead of failing.
    #[test]
    fn a_missing_font_falls_back() {
        for choice in [UiFont::System, UiFont::Inter, UiFont::GoogleSans] {
            let f = fonts_for(choice);
            let prop = &f.families[&FontFamily::Proportional];
            assert!(prop.contains(&ICON_FAMILY.to_owned()));
            if !font_available(choice) {
                assert!(!prop.contains(&"choice".to_owned()), "{choice:?}");
            }
        }
        assert!(font_available(UiFont::System));
    }
}
