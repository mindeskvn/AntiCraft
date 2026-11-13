//! Application menu (SPEC §8.1) and renderer context menus (SPEC §5.2).
//!
//! Menu items carry no accelerators: the webview owns keyboard focus, so the
//! §8.3 keybindings are intercepted in `bridge.js` instead (avoids double fire).

use muda::{CheckMenuItem, IsMenuItem, Menu, MenuId, MenuItem, PredefinedMenuItem, Submenu};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;

pub const DOCS_URL: &str = "https://antigravity.google/docs";

/// Stable menu ids.
pub mod ids {
    pub const NEW_WINDOW: &str = "new-window";
    pub const QUIT: &str = "quit";
    pub const CLOSE_WINDOW: &str = "close-window";
    pub const RELOAD: &str = "reload";
    pub const RESET_ZOOM: &str = "reset-zoom";
    pub const ZOOM_IN: &str = "zoom-in";
    pub const ZOOM_OUT: &str = "zoom-out";
    pub const FULLSCREEN: &str = "toggle-fullscreen";
    pub const MINIMIZE: &str = "minimize";
    pub const DOCS: &str = "docs";
    pub const CHECK_FOR_UPDATES: &str = "check-for-updates";
    pub const RUNNING_AGENTS: &str = "running-agents";
    pub const TRAY_OPEN: &str = "tray-open";
    pub const TRAY_QUIT: &str = "tray-quit";
    /// Prefix for renderer context-menu items.
    pub const CONTEXT_PREFIX: &str = "ctx:";
}

/// What a clicked menu id means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuAction {
    NewWindow,
    Quit,
    CloseWindow,
    Reload,
    ResetZoom,
    ZoomIn,
    ZoomOut,
    ToggleFullscreen,
    Minimize,
    Docs,
    CheckForUpdates,
    TrayOpen,
    /// Renderer context-menu item (full id including prefix).
    Context(String),
    Unknown,
}

pub fn action_for(id: &MenuId) -> MenuAction {
    let s = id.as_ref();
    match s {
        ids::NEW_WINDOW => MenuAction::NewWindow,
        ids::QUIT | ids::TRAY_QUIT => MenuAction::Quit,
        ids::CLOSE_WINDOW => MenuAction::CloseWindow,
        ids::RELOAD => MenuAction::Reload,
        ids::RESET_ZOOM => MenuAction::ResetZoom,
        ids::ZOOM_IN => MenuAction::ZoomIn,
        ids::ZOOM_OUT => MenuAction::ZoomOut,
        ids::FULLSCREEN => MenuAction::ToggleFullscreen,
        ids::MINIMIZE => MenuAction::Minimize,
        ids::DOCS => MenuAction::Docs,
        ids::CHECK_FOR_UPDATES => MenuAction::CheckForUpdates,
        ids::TRAY_OPEN => MenuAction::TrayOpen,
        s if s.starts_with(ids::CONTEXT_PREFIX) => MenuAction::Context(s.to_string()),
        _ => MenuAction::Unknown,
    }
}

/// The window menu bar: File / Edit / View / Window / Help, modelled on the
/// default menu with the §8.1 insertions (File[0] "New Window", Help[0] "Docs";
/// DevTools hidden as in a packaged build).
pub struct AppMenu {
    pub menu: Menu,
}

impl AppMenu {
    pub fn build() -> muda::Result<Self> {
        let file = Submenu::with_items(
            "File",
            true,
            &[
                &MenuItem::with_id(ids::NEW_WINDOW, "New Window", true, None),
                &PredefinedMenuItem::separator(),
                &MenuItem::with_id(ids::CLOSE_WINDOW, "Close Window", true, None),
                &MenuItem::with_id(ids::QUIT, "Exit", true, None),
            ],
        )?;
        let edit = Submenu::with_items(
            "Edit",
            true,
            &[
                &PredefinedMenuItem::undo(None),
                &PredefinedMenuItem::redo(None),
                &PredefinedMenuItem::separator(),
                &PredefinedMenuItem::cut(None),
                &PredefinedMenuItem::copy(None),
                &PredefinedMenuItem::paste(None),
                &PredefinedMenuItem::select_all(None),
            ],
        )?;
        let view = Submenu::with_items(
            "View",
            true,
            &[
                &MenuItem::with_id(ids::RELOAD, "Reload", true, None),
                &PredefinedMenuItem::separator(),
                &MenuItem::with_id(ids::RESET_ZOOM, "Actual Size", true, None),
                &MenuItem::with_id(ids::ZOOM_IN, "Zoom In", true, None),
                &MenuItem::with_id(ids::ZOOM_OUT, "Zoom Out", true, None),
                &PredefinedMenuItem::separator(),
                &MenuItem::with_id(ids::FULLSCREEN, "Toggle Full Screen", true, None),
            ],
        )?;
        let window = Submenu::with_items(
            "Window",
            true,
            &[
                &MenuItem::with_id(ids::MINIMIZE, "Minimize", true, None),
                &MenuItem::with_id(ids::CLOSE_WINDOW, "Close", true, None),
            ],
        )?;
        let help = Submenu::with_items(
            "Help",
            true,
            &[
                &MenuItem::with_id(ids::DOCS, "Docs", true, None),
                &MenuItem::with_id(ids::CHECK_FOR_UPDATES, "Check for Updates", true, None),
            ],
        )?;
        let menu = Menu::with_items(&[&file, &edit, &view, &window, &help])?;
        Ok(Self { menu })
    }

    /// Attach the menu bar to a window.
    #[cfg(windows)]
    pub fn attach(&self, hwnd: isize) {
        if let Err(e) = unsafe { self.menu.init_for_hwnd(hwnd) } {
            tracing::warn!("failed to attach menu: {e}");
        }
    }
}

/// Renderer context MenuItem (§5.1).
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct ContextMenuItem {
    pub id: Option<Value>,
    pub label: Option<String>,
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub checked: Option<bool>,
    pub disabled: Option<bool>,
    pub accelerator: Option<String>,
    pub submenu: Option<Vec<ContextMenuItem>>,
}

/// A built popup menu plus the mapping from muda id to the renderer's `id`.
pub struct BuiltContextMenu {
    pub menu: Menu,
    pub ids: HashMap<String, Value>,
}

fn build_into(
    append: &dyn Fn(&dyn IsMenuItem) -> muda::Result<()>,
    items: &[ContextMenuItem],
    ids_out: &mut HashMap<String, Value>,
    counter: &mut u32,
) -> muda::Result<()> {
    for it in items {
        let kind = it.kind.as_deref().unwrap_or("normal");
        let label = it.label.clone().unwrap_or_default();
        let enabled = !it.disabled.unwrap_or(false);
        let accel = it.accelerator.as_deref().and_then(|a| a.parse().ok());
        if kind == "separator" {
            append(&PredefinedMenuItem::separator())?;
            continue;
        }
        let sub_items = it.submenu.as_deref().filter(|s| !s.is_empty());
        if kind == "submenu" || sub_items.is_some() {
            let sub = Submenu::new(&label, enabled);
            build_into(&|i| sub.append(i), sub_items.unwrap_or(&[]), ids_out, counter)?;
            append(&sub)?;
            continue;
        }
        *counter += 1;
        let mid = format!("{}{}", ids::CONTEXT_PREFIX, counter);
        ids_out.insert(mid.clone(), it.id.clone().unwrap_or(Value::Null));
        if kind == "checkbox" || kind == "radio" {
            append(&CheckMenuItem::with_id(mid, &label, enabled, it.checked.unwrap_or(false), accel))?;
        } else {
            append(&MenuItem::with_id(mid, &label, enabled, accel))?;
        }
    }
    Ok(())
}

/// Build a popup from renderer items. Returns `None` for empty input.
pub fn build_context_menu(items: &[ContextMenuItem]) -> muda::Result<Option<BuiltContextMenu>> {
    if items.is_empty() {
        return Ok(None);
    }
    let menu = Menu::new();
    let mut ids_out = HashMap::new();
    let mut counter = 0;
    build_into(&|i| menu.append(i), items, &mut ids_out, &mut counter)?;
    Ok(Some(BuiltContextMenu { menu, ids: ids_out }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actions() {
        assert_eq!(action_for(&MenuId::new(ids::NEW_WINDOW)), MenuAction::NewWindow);
        assert_eq!(action_for(&MenuId::new("ctx:3")), MenuAction::Context("ctx:3".into()));
        assert_eq!(action_for(&MenuId::new("zzz")), MenuAction::Unknown);
    }

    #[test]
    fn context_items_deserialize() {
        let v: Vec<ContextMenuItem> = serde_json::from_str(
            r#"[{"id":"a","label":"A"},{"type":"separator","id":"x"},{"label":"S","submenu":[{"id":7,"label":"B"}]}]"#,
        )
        .unwrap();
        assert_eq!(v.len(), 3);
        assert_eq!(v[2].submenu.as_ref().unwrap()[0].id, Some(Value::from(7)));
    }
}
