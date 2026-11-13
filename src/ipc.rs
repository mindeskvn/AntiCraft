//! Renderer ↔ main IPC handlers (SPEC §5.2).
//!
//! `bridge.js` posts `{"id":n,"channel":"...","args":[...]}` through
//! `window.ipc.postMessage`; `id == 0` means fire-and-forget. Fast handlers
//! reply immediately; slow ones run on the tokio runtime and reply through
//! `UserEvent::IpcReply`. Messages from untrusted origins are dropped.

use crate::app::{App, PendingContext, UserEvent};
use crate::menu::{build_context_menu, ContextMenuItem};
use crate::window::ZOOM_STEP;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::path::PathBuf;
use std::time::Duration;
use tao::event_loop::EventLoopWindowTarget;
use tao::window::WindowId;

/// `Ok(None)` resolves the promise with `undefined`; `Err` rejects it.
pub type IpcResult = Result<Option<Value>, String>;

/// Internal channel: Ctrl+Shift+N from `bridge.js` (§8.3).
pub const CH_NEW_WINDOW: &str = "__anticraft:new-window";
/// Internal channel: Ctrl+Q from `bridge.js` (§8.3).
pub const CH_QUIT: &str = "__anticraft:quit";
/// Internal channel: Ctrl+wheel zoom from `bridge.js`; arg `true` = zoom in.
pub const CH_WHEEL_ZOOM: &str = "__anticraft:wheel-zoom";

/// Prefixes allowed by `shell:open-external`.
pub const EXTERNAL_ALLOWLIST: &[&str] = &["https://", "http://", "antigravity-ide://"];

/// One renderer request.
#[derive(Debug, Clone, Deserialize)]
pub struct IpcRequest {
    #[serde(default)]
    pub id: u64,
    pub channel: String,
    #[serde(default)]
    pub args: Vec<Value>,
}

pub fn parse_request(body: &str) -> Result<IpcRequest, String> {
    serde_json::from_str(body).map_err(|e| format!("malformed ipc message: {e}"))
}

/// IPC is accepted only from the LS UI origin (or the dev URL origin).
pub fn is_trusted_origin(page_url: &str, ls_port: Option<u16>, dev_url: Option<&str>) -> bool {
    let Ok(u) = url::Url::parse(page_url) else { return false };
    if let Some(port) = ls_port {
        if u.scheme() == "https" && u.host_str() == Some("127.0.0.1") && u.port() == Some(port) {
            return true;
        }
    }
    dev_url
        .and_then(|d| url::Url::parse(d).ok())
        .is_some_and(|d| d.origin() == u.origin())
}

/// `shell:open-external` allowlist.
pub fn is_allowed_external(url: &str) -> bool {
    EXTERNAL_ALLOWLIST.iter().any(|p| url.starts_with(p))
}

/// Open a URL/URI with the shell (`ShellExecuteW "open"`).
pub fn shell_open(target: &str) {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::UI::Shell::ShellExecuteW;
        use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
        let op: Vec<u16> = "open\0".encode_utf16().collect();
        let t: Vec<u16> = target.encode_utf16().chain(Some(0)).collect();
        let r = ShellExecuteW(
            std::ptr::null_mut(),
            op.as_ptr(),
            t.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        );
        if (r as isize) <= 32 {
            tracing::warn!("ShellExecuteW failed for {target}: {}", r as isize);
        }
    }
}

/// Reveal an existing local file in Explorer via `SHOpenFolderAndSelectItems`
/// (no command line is built). Runs on a short-lived thread because the shell
/// call needs COM.
pub fn reveal_in_explorer(path: PathBuf) {
    std::thread::spawn(move || {
        #[cfg(windows)]
        unsafe {
            use windows_sys::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
            use windows_sys::Win32::UI::Shell::{ILFree, SHOpenFolderAndSelectItems, SHParseDisplayName};
            let wide: Vec<u16> = path.as_os_str().to_string_lossy().encode_utf16().chain(Some(0)).collect();
            let init = CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32);
            let mut pidl = std::ptr::null_mut();
            let hr = SHParseDisplayName(wide.as_ptr(), std::ptr::null_mut(), &mut pidl, 0, std::ptr::null_mut());
            if hr >= 0 && !pidl.is_null() {
                let hr = SHOpenFolderAndSelectItems(pidl, 0, std::ptr::null(), 0);
                if hr < 0 {
                    tracing::warn!("SHOpenFolderAndSelectItems failed for {}: {hr:#x}", path.display());
                }
                ILFree(pidl);
            } else {
                tracing::warn!("SHParseDisplayName failed for {}: {hr:#x}", path.display());
            }
            if init >= 0 {
                CoUninitialize();
            }
        }
        #[cfg(not(windows))]
        let _ = path;
    });
}

/// `file://` URL → existing local drive-letter path. UNC (`file://host/...`),
/// device (`\\?\`, `\\.\`) and missing paths are rejected so revealing can
/// never trigger an outbound SMB connection.
pub fn file_url_to_path(u: &str) -> Option<PathBuf> {
    let url = url::Url::parse(u).ok().filter(|u| u.scheme() == "file")?;
    if url.host_str().is_some_and(|h| !h.is_empty() && !h.eq_ignore_ascii_case("localhost")) {
        return None;
    }
    let p = url.to_file_path().ok()?;
    if !is_drive_letter_path(&p.to_string_lossy()) {
        return None;
    }
    let canon = std::fs::canonicalize(&p).ok()?;
    let s = canon.to_string_lossy();
    let plain = s.strip_prefix(r"\\?\").unwrap_or(&s).to_string();
    is_drive_letter_path(&plain).then(|| PathBuf::from(plain))
}

/// `X:\...` and nothing else.
fn is_drive_letter_path(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/')
}

// ------------------------------------------------------------------- WSL

/// One `wsl --list --verbose` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WslDistro {
    pub name: String,
    pub is_default: bool,
    pub version: u32,
}

/// Text encodings wsl.exe may emit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WslEncoding {
    Utf16Le,
    Utf8,
}

/// §10: any NUL byte means UTF-16LE, otherwise UTF-8.
fn sniff_wsl_encoding(bytes: &[u8]) -> WslEncoding {
    if bytes.contains(&0) {
        WslEncoding::Utf16Le
    } else {
        WslEncoding::Utf8
    }
}

/// Decode wsl.exe output per §10. A leading U+FEFF is removed either way
/// (harmless on the UTF-8 path, where it would otherwise glue onto a token).
pub fn decode_wsl_output(bytes: &[u8]) -> String {
    let text = match sniff_wsl_encoding(bytes) {
        WslEncoding::Utf16Le => {
            let units: Vec<u16> = bytes.chunks_exact(2).map(|p| u16::from_le_bytes([p[0], p[1]])).collect();
            String::from_utf16_lossy(&units)
        }
        WslEncoding::Utf8 => String::from_utf8_lossy(bytes).into_owned(),
    };
    match text.strip_prefix('\u{feff}') {
        Some(rest) => rest.to_owned(),
        None => text,
    }
}

/// Split one listing line into `(default_marker, tokens)`. The marker is a
/// leading `*`, either standalone or attached to the name.
fn tokenize_wsl_row(line: &str) -> (bool, Vec<&str>) {
    let trimmed = line.trim_start();
    let (marked, rest) = match trimmed.strip_prefix('*') {
        Some(r) => (true, r),
        None => (false, trimmed),
    };
    (marked, rest.split_whitespace().collect())
}

/// One row as described in §10: exactly NAME, STATE, VERSION (decimal).
/// Rows with any other shape (header, names with spaces) are rejected.
fn parse_wsl_row(line: &str) -> Option<WslDistro> {
    let (is_default, tokens) = tokenize_wsl_row(line);
    let [name, _state, version] = tokens.as_slice() else { return None };
    if version.is_empty() || !version.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(WslDistro { name: (*name).to_owned(), is_default, version: version.parse().ok()? })
}

/// Distros owned by container runtimes, hidden from the user (§10).
const HIDDEN_DISTRO_PREFIXES: [&str; 3] = ["docker-desktop", "rancher-", "podman-"];

fn is_user_distro(d: &WslDistro) -> bool {
    let lower = d.name.to_ascii_lowercase();
    d.version == 2 && HIDDEN_DISTRO_PREFIXES.iter().all(|p| !lower.starts_with(p))
}

/// Parsed `wsl --list --verbose` rows that the shell offers.
pub fn parse_wsl_list(text: &str) -> Vec<WslDistro> {
    let mut out = Vec::new();
    for line in text.lines() {
        if let Some(d) = parse_wsl_row(line) {
            if is_user_distro(&d) {
                out.push(d);
            }
        }
    }
    out
}

fn wsl_exe() -> PathBuf {
    let root = std::env::var_os("SystemRoot").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    root.join("System32").join("wsl.exe")
}

/// `wsl:get-state`. AntiCraft always runs locally, so `activeDistro` is `""`.
pub async fn wsl_state() -> Value {
    let unavailable = json!({"available": false, "activeDistro": null, "distros": []});
    let exe = wsl_exe();
    if !exe.exists() {
        return unavailable;
    }
    let mut cmd = tokio::process::Command::new(exe);
    cmd.args(["--list", "--verbose"]).kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000);
    match tokio::time::timeout(Duration::from_secs(30), cmd.output()).await {
        Ok(Ok(out)) if out.status.success() => {
            let distros: Vec<Value> = parse_wsl_list(&decode_wsl_output(&out.stdout))
                .into_iter()
                .map(|d| json!({"name": d.name, "isDefault": d.is_default}))
                .collect();
            json!({"available": true, "activeDistro": "", "distros": distros})
        }
        _ => unavailable,
    }
}

// --------------------------------------------------------------- dispatch

fn arg_str(args: &[Value], i: usize) -> Result<&str, String> {
    args.get(i).and_then(Value::as_str).ok_or_else(|| format!("argument {i} must be a string"))
}

impl App {
    /// Spawn an async handler whose result is delivered back as an IpcReply.
    fn reply_async<F>(&self, window: WindowId, id: u64, fut: F)
    where
        F: std::future::Future<Output = IpcResult> + Send + 'static,
    {
        let proxy = self.ctx.proxy.clone();
        self.ctx.rt.spawn(async move {
            let result = fut.await;
            let _ = proxy.send_event(UserEvent::IpcReply { window, id, result });
        });
    }

    /// Entry point for every `window.ipc.postMessage`.
    pub(crate) fn handle_ipc(
        &mut self,
        target: &EventLoopWindowTarget<UserEvent>,
        window: WindowId,
        origin: &str,
        body: &str,
    ) {
        if !is_trusted_origin(origin, self.ctx.ls_port, self.ctx.dev_url.as_deref()) {
            tracing::warn!("dropping ipc from untrusted origin {origin}");
            return;
        }
        let req = match parse_request(body) {
            Ok(r) => r,
            Err(e) => return tracing::warn!("{e}"),
        };
        let id = req.id;
        if let Some(result) = self.dispatch(target, window, req) {
            self.reply(window, id, result);
        }
    }

    pub(crate) fn reply(&self, window: WindowId, id: u64, result: IpcResult) {
        if let Some(w) = self.window(window) {
            w.reply(id, &result);
        }
    }

    /// Returns `Some(result)` for an immediate reply, `None` when the reply is
    /// deferred (async task or context menu).
    fn dispatch(
        &mut self,
        target: &EventLoopWindowTarget<UserEvent>,
        window: WindowId,
        req: IpcRequest,
    ) -> Option<IpcResult> {
        let a = &req.args;
        let r: IpcResult = match req.channel.as_str() {
            // ---- updater
            "updater:get-state" => Ok(Some(serde_json::to_value(self.ctx.updater.state()).unwrap_or(Value::Null))),
            "updater:apply" => {
                self.ctx.updater.apply();
                Ok(None)
            }
            "updater:quit-and-install" => {
                if let Err(e) = self.ctx.updater.quit_and_install() {
                    tracing::info!("quit-and-install ignored: {e}");
                }
                Ok(None)
            }
            "updater:check-for-updates" => {
                self.ctx.updater.spawn_check(true);
                Ok(None)
            }
            // ---- dialogs
            "dialog:open-workspace" => {
                let mut d = rfd::FileDialog::new().set_title("Open workspace").set_can_create_directories(true);
                if let Some(w) = self.window(window) {
                    d = d.set_parent(&w.window);
                }
                Ok(d.pick_folder().map(|p| Value::String(p.to_string_lossy().into_owned())))
            }
            "dialog:open-workspaces" => {
                let mut d = rfd::FileDialog::new().set_title("Open workspaces").set_can_create_directories(true);
                if let Some(w) = self.window(window) {
                    d = d.set_parent(&w.window);
                }
                let picked = d.pick_folders().unwrap_or_default();
                Ok(Some(Value::Array(
                    picked.into_iter().map(|p| Value::String(p.to_string_lossy().into_owned())).collect(),
                )))
            }
            // ---- notifications
            "notification:send" => {
                let o = a.first().cloned().unwrap_or(Value::Null);
                let title = o.get("title").and_then(Value::as_str).unwrap_or("").to_string();
                let body = o.get("body").and_then(Value::as_str).unwrap_or("").to_string();
                let silent = o.get("silent").and_then(Value::as_bool).unwrap_or(false);
                self.ctx.rt.spawn_blocking(move || {
                    let mut n = notify_rust::Notification::new();
                    n.summary(&title).body(&body);
                    if silent {
                        n.sound_name("");
                    }
                    if let Err(e) = n.show() {
                        tracing::warn!("notification failed: {e}");
                    }
                });
                Ok(None)
            }
            "notification:open-system-preferences" => {
                shell_open("ms-settings:notifications");
                Ok(None)
            }
            // ---- storage
            "storage:get-items" => Ok(Some(Value::Object(self.ctx.storage.get_items()))),
            "storage:update-items" => match a.first() {
                Some(Value::Object(changes)) => self.ctx.storage.update_items(changes).map(|_| None).map_err(|e| e.to_string()),
                _ => Err("changes must be an object".into()),
            },
            // ---- logs / extensions / deep link / agents
            "logs:electron" => Ok(Some(Value::String(match std::fs::read_to_string(&self.ctx.paths.main_log) {
                Ok(t) => t,
                Err(e) => format!("Failed to read logs: {e}"),
            }))),
            "extensions:send-authorities" => match a.first() {
                Some(Value::Object(m)) => {
                    let mut map = std::collections::HashMap::new();
                    for (k, v) in m {
                        match v.as_str().and_then(crate::plugin_scheme::validate_origin) {
                            Some(origin) => {
                                map.insert(k.clone(), origin);
                            }
                            None => tracing::warn!("send-authorities: dropped '{k}' -> {v} (not a loopback http(s) origin)"),
                        }
                    }
                    self.ctx.authorities.replace(map);
                    Ok(None)
                }
                _ => Err("authorities must be an object".into()),
            },
            "deep-link:get-stored" => Ok(Some(self.ctx.pending_link.take().map(Value::String).unwrap_or(Value::Null))),
            "agent:update-active-count" => {
                let n = a.first().and_then(Value::as_f64).unwrap_or(0.0).max(0.0) as u32;
                if let Some(t) = &self.tray {
                    t.set_agent_count(n);
                }
                Ok(None)
            }
            // ---- window controls
            "window:set-title-bar-overlay" => Ok(None), // native frame; see README
            "window:minimize" => self.with_target(window, |w| w.window.set_minimized(true)),
            "window:maximize" => self.with_target(window, |w| w.window.set_maximized(true)),
            "window:unmaximize" => self.with_target(window, |w| w.window.set_maximized(false)),
            "window:close" => {
                let t = self.target_id(window);
                if let Some(t) = t {
                    self.close_window(t);
                }
                Ok(None)
            }
            "window:is-maximized" => {
                Ok(Some(Value::Bool(self.target_window(window).is_some_and(|w| w.window.is_maximized()))))
            }
            "window:toggle-devtools" => self.with_target(window, |w| w.toggle_devtools()),
            "window:zoom-in" | "window:zoom-out" | "window:reset-zoom" => {
                let ch = req.channel.clone();
                if let Some(w) = self.window_mut(window) {
                    let level = match ch.as_str() {
                        "window:zoom-in" => w.zoom_level + ZOOM_STEP,
                        "window:zoom-out" => w.zoom_level - ZOOM_STEP,
                        _ => 0.0,
                    };
                    w.set_zoom_level(level);
                    // §5.2: the explicit zoom IPC always persists.
                    self.ctx.settings.set_zoom_level(level);
                }
                Ok(None)
            }
            // Ctrl+wheel (Electron's zoom-changed): persist only for windows
            // created with the storage manager (§4.2).
            CH_WHEEL_ZOOM => {
                let zoom_in = a.first().and_then(Value::as_bool).unwrap_or(false);
                if let Some(w) = self.window_mut(window) {
                    let level = w.zoom_level + if zoom_in { ZOOM_STEP } else { -ZOOM_STEP };
                    w.set_zoom_level(level);
                    if w.persist_zoom {
                        self.ctx.settings.set_zoom_level(level);
                    }
                }
                Ok(None)
            }
            "window:show-context-menu" => return self.show_context_menu(window, req.id, a.first()),
            "window:close-context-menu" => {
                #[cfg(windows)]
                unsafe {
                    windows_sys::Win32::UI::WindowsAndMessaging::EndMenu();
                }
                Ok(None)
            }
            // ---- shell
            "shell:open-external" => match arg_str(a, 0) {
                Ok(u) => {
                    if is_allowed_external(u) {
                        shell_open(u);
                    }
                    Ok(None)
                }
                Err(e) => Err(e),
            },
            "shell:reveal-in-file-picker" => match arg_str(a, 0) {
                Ok(u) => match file_url_to_path(u) {
                    Some(p) => {
                        reveal_in_explorer(p);
                        Ok(None)
                    }
                    None => Err(format!("not an existing local file URL: {u}")),
                },
                Err(e) => Err(e),
            },
            "webview:execute-js-isolated-world" => {
                Err("executeJavaScriptInIsolatedWorld is not supported by AntiCraft (no <webview> guests)".into())
            }
            // ---- IDE / WSL
            "ide:is-installed" => Ok(Some(Value::Bool(self.ctx.paths.ide_exe().exists()))),
            "wsl:get-state" => {
                self.reply_async(window, req.id, async { Ok(Some(wsl_state().await)) });
                return None;
            }
            "wsl:connect" => match arg_str(a, 0) {
                Ok("") => Ok(None), // already local
                Ok(d) => {
                    self.relaunch_into_wsl(d.to_string());
                    Ok(None)
                }
                Err(e) => Err(e),
            },
            // ---- internal keybindings
            CH_NEW_WINDOW => {
                let url = self.main_url();
                self.open_window(target, &url, true);
                Ok(None)
            }
            CH_QUIT => {
                let _ = self.ctx.proxy.send_event(UserEvent::Quit { confirm: true });
                Ok(None)
            }
            other => Err(format!("No handler registered for '{other}'")),
        };
        Some(r)
    }

    fn with_target(&self, window: WindowId, f: impl FnOnce(&crate::window::AppWindow)) -> IpcResult {
        if let Some(w) = self.target_window(window) {
            f(w);
        }
        Ok(None)
    }

    fn show_context_menu(&mut self, window: WindowId, id: u64, items: Option<&Value>) -> Option<IpcResult> {
        let items: Vec<ContextMenuItem> = match items.map(|v| serde_json::from_value(v.clone())) {
            Some(Ok(v)) => v,
            _ => return Some(Ok(Some(Value::Null))),
        };
        let built = match build_context_menu(&items) {
            Ok(Some(b)) => b,
            Ok(None) => return Some(Ok(Some(Value::Null))),
            Err(e) => return Some(Err(e.to_string())),
        };
        let Some(tw) = self.target_id(window) else { return Some(Ok(Some(Value::Null))) };
        // Close any open popup first.
        if let Some(prev) = self.pending_context.take() {
            self.reply(prev.window, prev.id, Ok(Some(Value::Null)));
        }
        #[cfg(windows)]
        {
            use muda::ContextMenu;
            use tao::platform::windows::WindowExtWindows;
            let hwnd = self.window(tw).map(|w| w.window.hwnd()).unwrap_or(0);
            let menu = built.menu;
            self.pending_context = Some(PendingContext { window, id, ids: built.ids, menu: Some(menu) });
            let shown = {
                let pc = self.pending_context.as_ref().and_then(|p| p.menu.as_ref());
                pc.map(|m| unsafe { m.show_context_menu_for_hwnd(hwnd, None) }).unwrap_or(false)
            };
            if !shown {
                // Dismissed: the click (if any) arrives as a MenuEvent first.
                let proxy = self.ctx.proxy.clone();
                let _ = proxy.send_event(UserEvent::ContextMenuDismissed { window, id });
            }
            None
        }
        #[cfg(not(windows))]
        {
            let _ = (tw, built);
            Some(Ok(Some(Value::Null)))
        }
    }

    /// Resolve a context-menu click (`ctx:*` id).
    pub(crate) fn on_context_click(&mut self, menu_id: &str) {
        if let Some(pc) = self.pending_context.take() {
            let v = pc.ids.get(menu_id).cloned().unwrap_or(Value::Null);
            self.reply(pc.window, pc.id, Ok(Some(v)));
        }
    }

    /// Deferred dismiss: resolves `null` only if no click won.
    pub(crate) fn on_context_dismissed(&mut self, window: WindowId, id: u64) {
        if self.pending_context.as_ref().is_some_and(|p| p.window == window && p.id == id) {
            self.pending_context = None;
            self.reply(window, id, Ok(Some(Value::Null)));
        }
    }
}

/// Changes object as broadcast on `storage:changed`.
pub fn changes_value(c: &Map<String, Value>) -> Value {
    Value::Object(c.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins() {
        assert!(is_trusted_origin("https://127.0.0.1:5/x", Some(5), None));
        assert!(!is_trusted_origin("https://127.0.0.1:6/x", Some(5), None));
        assert!(!is_trusted_origin("http://127.0.0.1:5/x", Some(5), None));
        assert!(is_trusted_origin("http://localhost:3000/a", None, Some("http://localhost:3000/")));
    }

    #[test]
    fn external_allowlist() {
        assert!(is_allowed_external("https://a"));
        assert!(is_allowed_external("antigravity-ide://x"));
        assert!(!is_allowed_external("file:///c:/x"));
        assert!(!is_allowed_external("javascript:alert(1)"));
    }

    #[test]
    fn wsl_parsing() {
        let text = "  NAME                   STATE           VERSION\n* Ubuntu                 Running         2\n  docker-desktop-data    Stopped         2\n  Legacy                 Stopped         1\n  Debian                 Stopped         2\n";
        let mut utf16 = vec![0xff, 0xfe];
        for u in text.encode_utf16() {
            utf16.extend_from_slice(&u.to_le_bytes());
        }
        let d = parse_wsl_list(&decode_wsl_output(&utf16));
        assert_eq!(d.len(), 2);
        assert_eq!(d[0], WslDistro { name: "Ubuntu".into(), is_default: true, version: 2 });
        assert_eq!(d[1].name, "Debian");
    }

    #[test]
    fn requests() {
        let r = parse_request(r#"{"id":4,"channel":"storage:get-items","args":[]}"#).unwrap();
        assert_eq!((r.id, r.channel.as_str()), (4, "storage:get-items"));
        assert!(parse_request("nope").is_err());
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("b.txt");
        std::fs::write(&f, "x").unwrap();
        let u = url::Url::from_file_path(&f).unwrap();
        assert!(file_url_to_path(u.as_str()).is_some());
        assert!(file_url_to_path("file:///C:/definitely/missing/x.txt").is_none());
        assert!(file_url_to_path("file://attacker-host/share/x").is_none());
        assert!(file_url_to_path("https://x").is_none());
    }
}
