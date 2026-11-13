//! Tray icon (SPEC §8.2).

use crate::menu::ids;
use crate::paths::APP_NAME;
use tray_icon::menu::{Menu, MenuItem, PredefinedMenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder};

/// `No agents running` / `1 agent running` / `N agents running`.
pub fn agent_label(n: u32) -> String {
    match n {
        0 => "No agents running".to_string(),
        1 => "1 agent running".to_string(),
        n => format!("{n} agents running"),
    }
}

/// Procedural app glyph (RGBA, `size`×`size`): a filled ring on transparent.
/// Used for both the window and tray icon because the shell ships no image
/// decoder; see README.
pub fn app_icon_rgba(size: u32) -> Vec<u8> {
    let mut v = vec![0u8; (size * size * 4) as usize];
    let c = (size as f32 - 1.0) / 2.0;
    let (r_out, r_in) = (size as f32 * 0.48, size as f32 * 0.22);
    for y in 0..size {
        for x in 0..size {
            let d = ((x as f32 - c).powi(2) + (y as f32 - c).powi(2)).sqrt();
            if d <= r_out && d >= r_in {
                let i = ((y * size + x) * 4) as usize;
                v[i..i + 4].copy_from_slice(&[0x4f, 0x8c, 0xff, 0xff]);
            }
        }
    }
    v
}

/// Tray icon with the §8.2 menu.
pub struct AppTray {
    _tray: TrayIcon,
    agents: MenuItem,
}

impl AppTray {
    pub fn create() -> anyhow::Result<Self> {
        let agents = MenuItem::with_id(ids::RUNNING_AGENTS, agent_label(0), false, None);
        let open = MenuItem::with_id(ids::TRAY_OPEN, format!("Open {APP_NAME}"), true, None);
        let quit = MenuItem::with_id(ids::TRAY_QUIT, "Quit", true, None);
        let menu = Menu::with_items(&[&agents, &PredefinedMenuItem::separator(), &open, &quit])?;
        let icon = tray_icon::Icon::from_rgba(app_icon_rgba(32), 32, 32)?;
        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip(APP_NAME)
            .with_icon(icon)
            .with_menu_on_left_click(false)
            .build()?;
        Ok(Self { _tray: tray, agents })
    }

    /// Update the disabled `running-agents` label.
    pub fn set_agent_count(&self, n: u32) {
        self.agents.set_text(agent_label(n));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels() {
        assert_eq!(agent_label(0), "No agents running");
        assert_eq!(agent_label(1), "1 agent running");
        assert_eq!(agent_label(5), "5 agents running");
        assert_eq!(app_icon_rgba(16).len(), 16 * 16 * 4);
    }
}
