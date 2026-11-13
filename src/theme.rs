//! Window theme from `~/.gemini/config/config.json` (SPEC §4.2).

use std::path::Path;

/// Resolved theme mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeMode {
    Dark,
    Light,
}

/// Background / foreground colors as `#RRGGBB`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThemeColors {
    pub bg: &'static str,
    pub fg: &'static str,
}

impl ThemeMode {
    pub fn colors(self) -> ThemeColors {
        match self {
            ThemeMode::Dark => ThemeColors { bg: "#131313", fg: "#FAFAFA" },
            ThemeMode::Light => ThemeColors { bg: "#FAFAFA", fg: "#383A42" },
        }
    }
}

/// Parse `#RRGGBB` into (r, g, b).
pub fn hex_rgb(hex: &str) -> (u8, u8, u8) {
    let h = hex.trim_start_matches('#');
    let p = |i: usize| u8::from_str_radix(h.get(i..i + 2).unwrap_or("00"), 16).unwrap_or(0);
    (p(0), p(2), p(4))
}

/// Pure mapping of a raw `userSettings.themeMode` string (case-sensitive):
/// contains `INHERIT` → follow OS; contains `LIGHT` → light; else dark.
pub fn mode_from_raw(raw: Option<&str>, os_is_dark: bool) -> ThemeMode {
    match raw {
        Some(s) if s.contains("INHERIT") => {
            if os_is_dark {
                ThemeMode::Dark
            } else {
                ThemeMode::Light
            }
        }
        Some(s) if s.contains("LIGHT") => ThemeMode::Light,
        _ => ThemeMode::Dark,
    }
}

/// Read the config file; any error or non-string value → dark.
pub fn load_theme(config_path: &Path, os_is_dark: bool) -> ThemeMode {
    let raw = std::fs::read_to_string(config_path)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v.get("userSettings")?.get("themeMode")?.as_str().map(str::to_owned));
    mode_from_raw(raw.as_deref(), os_is_dark)
}

/// OS dark mode (`HKCU\...\Themes\Personalize\AppsUseLightTheme == 0`).
/// Returns `true` (dark) when unreadable.
pub fn os_prefers_dark() -> bool {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
        let key: Vec<u16> = "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize\0"
            .encode_utf16()
            .collect();
        let val: Vec<u16> = "AppsUseLightTheme\0".encode_utf16().collect();
        let mut data: u32 = 1;
        let mut size: u32 = 4;
        let rc = RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            val.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            &mut data as *mut u32 as *mut _,
            &mut size,
        );
        if rc == 0 {
            return data == 0;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mapping() {
        assert_eq!(mode_from_raw(Some("THEME_MODE_LIGHT"), true), ThemeMode::Light);
        assert_eq!(mode_from_raw(Some("THEME_MODE_INHERIT"), false), ThemeMode::Light);
        assert_eq!(mode_from_raw(Some("theme_light"), false), ThemeMode::Dark);
        assert_eq!(mode_from_raw(None, false), ThemeMode::Dark);
    }

    #[test]
    fn file_parsing() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("config.json");
        assert_eq!(load_theme(&p, false), ThemeMode::Dark);
        std::fs::write(&p, r#"{"userSettings":{"themeMode":"X_LIGHT"}}"#).unwrap();
        assert_eq!(load_theme(&p, true), ThemeMode::Light);
        std::fs::write(&p, r#"{"userSettings":{"themeMode":5}}"#).unwrap();
        assert_eq!(load_theme(&p, false), ThemeMode::Dark);
        assert_eq!(hex_rgb("#131313"), (0x13, 0x13, 0x13));
    }

    #[test]
    fn inherit_follows_os_and_colors() {
        assert_eq!(mode_from_raw(Some("THEME_MODE_INHERIT"), true), ThemeMode::Dark);
        assert_eq!(mode_from_raw(Some("THEME_MODE_DARK"), false), ThemeMode::Dark);
        assert_eq!(ThemeMode::Dark.colors().bg, "#131313");
        assert_eq!(ThemeMode::Light.colors().fg, "#383A42");
        assert_eq!(hex_rgb("#FAFAFA"), (0xFA, 0xFA, 0xFA));
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("config.json");
        std::fs::write(&p, "not json").unwrap();
        assert_eq!(load_theme(&p, false), ThemeMode::Dark);
    }
}
