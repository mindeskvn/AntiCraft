//! UI event loop (SPEC §2 not-headless steps, §2.1 lifecycle, §8, §9).
//!
//! Everything that touches windows, webviews, menus or the tray lives on the
//! main thread inside [`App`]. Background work (LS, bridge, updater, pipes)
//! talks to it only through [`UserEvent`] via the event-loop proxy.

use crate::deeplink::{self, PendingDeepLink};
use crate::host_bridge::HostBridge;
use crate::ls::LanguageServer;
use crate::menu::{self, AppMenu, MenuAction};
use crate::paths::Paths;
use crate::plugin_scheme::AuthorityMap;
use crate::settings::SettingsService;
use crate::storage::StorageManager;
use crate::tray::AppTray;
use crate::updater::{UpdateStateObj, Updater};
use crate::window::{self, AppWindow, WindowOptions};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tao::event::{Event, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoop, EventLoopProxy, EventLoopWindowTarget};
use tao::window::{Fullscreen, Window, WindowId};
use wry::WebContext;

/// Events delivered to the UI thread.
#[derive(Debug)]
pub enum UserEvent {
    /// `window.ipc.postMessage` from a page (`origin` is the page URL).
    Ipc { window: WindowId, origin: String, body: String },
    /// Deferred reply for an async IPC handler.
    IpcReply { window: WindowId, id: u64, result: crate::ipc::IpcResult },
    /// Main-frame navigation finished (`did-finish-load`).
    PageLoaded(WindowId),
    /// `document.title` changed.
    TitleChanged(WindowId, String),
    /// `window.open` / target=_blank → system browser.
    OpenExternal(String),
    /// Quit request; `confirm` shows the §2.1 "Confirm Quit" dialog.
    Quit { confirm: bool },
    /// Native context menu closed without a selection.
    ContextMenuDismissed { window: WindowId, id: u64 },
    /// Click on any muda menu item (app menu, tray menu, context menu).
    Menu(muda::MenuEvent),
    /// Tray icon interaction.
    Tray(tray_icon::TrayIconEvent),
    /// LS (re)started on a port; reload all windows.
    PortChanged(u16),
    /// Deep link to dispatch (§9).
    #[allow(dead_code)]
    DeepLink(String),
    /// argv forwarded by a second instance.
    SecondInstance(Vec<String>),
    /// Updater broadcast (`updater:state-changed`).
    UpdaterState(UpdateStateObj),
    /// Storage broadcast (`storage:changed`).
    StorageChanged(Value),
    /// Manual check found nothing ("No updates available").
    UpToDate,
    /// Background shutdown finished; exit the loop.
    ShutdownComplete,
}

/// Background services torn down on quit (LS first, bridge last).
#[derive(Default)]
pub struct Backend {
    pub ls: Option<LanguageServer>,
    pub bridge: Option<HostBridge>,
}

impl Backend {
    /// §2.1 steps 3-4: kill the LS, then close the host bridge.
    pub async fn shutdown(self) {
        if let Some(ls) = self.ls {
            ls.shutdown().await;
        }
        if let Some(b) = self.bridge {
            b.shutdown().await;
        }
    }
}

/// Shared, mostly-immutable state used by windows and IPC handlers.
pub struct AppContext {
    pub paths: Arc<Paths>,
    pub storage: Arc<StorageManager>,
    pub settings: Arc<SettingsService>,
    pub updater: Updater,
    pub authorities: AuthorityMap,
    /// Client for the plugin:// proxy only (no redirects, see plugin_scheme).
    pub http: reqwest::Client,
    pub rt: tokio::runtime::Handle,
    pub proxy: EventLoopProxy<UserEvent>,
    pub pending_link: PendingDeepLink,
    /// Current LS port (stale after a crash, as in the original).
    pub ls_port: Option<u16>,
    /// `--dev-url` / `DEV_URL`: LS skipped, windows load this.
    pub dev_url: Option<String>,
    /// WebView2 browser args (identical for every webview of the environment).
    pub browser_args: String,
}

/// An open renderer context menu awaiting a click or dismiss.
pub struct PendingContext {
    pub window: WindowId,
    pub id: u64,
    /// muda id → renderer item `id`.
    pub ids: HashMap<String, Value>,
    /// Kept alive while the popup is tracked.
    pub menu: Option<muda::Menu>,
}

/// UI-thread application state.
pub struct App {
    pub ctx: AppContext,
    /// Ordered by creation; index 0 is "the first window".
    pub(crate) windows: Vec<AppWindow>,
    pub(crate) focused: Option<WindowId>,
    pub(crate) tray: Option<AppTray>,
    pub(crate) app_menu: Option<AppMenu>,
    pub(crate) pending_context: Option<PendingContext>,
    backend: Option<Backend>,
    quitting: bool,
    // Declared last so it is dropped after every webview.
    web_context: WebContext,
}

/// Show a message box parented to `parent` when given.
pub fn message_box(
    parent: Option<&Window>,
    level: rfd::MessageLevel,
    title: &str,
    description: &str,
    buttons: rfd::MessageButtons,
) -> rfd::MessageDialogResult {
    let mut d = rfd::MessageDialog::new()
        .set_level(level)
        .set_title(title)
        .set_description(description)
        .set_buttons(buttons);
    if let Some(p) = parent {
        d = d.set_parent(p);
    }
    d.show()
}

impl App {
    pub fn new(ctx: AppContext, backend: Backend) -> Self {
        let web_context = WebContext::new(Some(ctx.paths.webview2_data.clone()));
        Self {
            ctx,
            windows: Vec::new(),
            focused: None,
            tray: None,
            app_menu: None,
            pending_context: None,
            backend: Some(backend),
            quitting: false,
            web_context,
        }
    }

    /// Forward muda and tray events into the loop.
    pub fn install_event_forwarders(proxy: &EventLoopProxy<UserEvent>) {
        let p = proxy.clone();
        muda::MenuEvent::set_event_handler(Some(move |e| {
            let _ = p.send_event(UserEvent::Menu(e));
        }));
        let p = proxy.clone();
        tray_icon::TrayIconEvent::set_event_handler(Some(move |e| {
            let _ = p.send_event(UserEvent::Tray(e));
        }));
    }

    /// Run the loop forever (never returns).
    pub fn run(mut self, event_loop: EventLoop<UserEvent>) -> ! {
        event_loop.run(move |event, target, control_flow| {
            *control_flow = ControlFlow::Wait;
            self.handle_event(event, target, control_flow);
        })
    }

    fn handle_event(
        &mut self,
        event: Event<'_, UserEvent>,
        target: &EventLoopWindowTarget<UserEvent>,
        control_flow: &mut ControlFlow,
    ) {
        match event {
            Event::NewEvents(StartCause::Init) => self.on_init(target),
            Event::WindowEvent { window_id, event, .. } => match event {
                WindowEvent::CloseRequested => self.close_window(window_id),
                WindowEvent::Resized(_) => {
                    if let Some(w) = self.window(window_id) {
                        w.resize();
                    }
                }
                WindowEvent::Focused(true) => self.focused = Some(window_id),
                _ => {}
            },
            Event::UserEvent(ev) => self.on_user_event(ev, target, control_flow),
            _ => {}
        }
    }

    fn on_init(&mut self, target: &EventLoopWindowTarget<UserEvent>) {
        match AppMenu::build() {
            Ok(m) => self.app_menu = Some(m),
            Err(e) => tracing::warn!("failed to build menu: {e}"),
        }
        let url = self.main_url();
        self.open_window(target, &url, true);
        match AppTray::create() {
            Ok(t) => self.tray = Some(t),
            Err(e) => tracing::warn!("failed to create tray icon: {e}"),
        }
    }

    fn on_user_event(
        &mut self,
        ev: UserEvent,
        target: &EventLoopWindowTarget<UserEvent>,
        control_flow: &mut ControlFlow,
    ) {
        match ev {
            UserEvent::Ipc { window, origin, body } => self.handle_ipc(target, window, &origin, &body),
            UserEvent::IpcReply { window, id, result } => self.reply(window, id, result),
            UserEvent::PageLoaded(id) => self.on_page_loaded(id),
            UserEvent::TitleChanged(id, t) => {
                if let Some(w) = self.window(id) {
                    w.set_page_title(&t);
                }
            }
            UserEvent::OpenExternal(u) => {
                // §4.2 has no allowlist here; AntiCraft applies the same one
                // as shell:open-external (documented deviation).
                if crate::ipc::is_allowed_external(&u) {
                    crate::ipc::shell_open(&u);
                } else {
                    tracing::warn!("blocked window.open to {u}");
                }
            }
            UserEvent::Quit { confirm } => self.request_quit(confirm),
            UserEvent::ContextMenuDismissed { window, id } => self.on_context_dismissed(window, id),
            UserEvent::Menu(e) => self.on_menu(target, e),
            UserEvent::Tray(e) => {
                if let tray_icon::TrayIconEvent::Click {
                    button: tray_icon::MouseButton::Left,
                    button_state: tray_icon::MouseButtonState::Up,
                    ..
                } = e
                {
                    self.show_or_create(target);
                }
            }
            UserEvent::PortChanged(port) => {
                self.ctx.ls_port = Some(port);
                if self.ctx.dev_url.is_none() {
                    let url = window::ls_url(port);
                    for w in &self.windows {
                        w.load_url(&url);
                    }
                }
            }
            UserEvent::DeepLink(u) => self.dispatch_deep_link(u),
            UserEvent::SecondInstance(argv) => {
                self.show_or_create(target);
                if let Some(u) = deeplink::find_in_argv(&argv, &deeplink::scheme()) {
                    self.dispatch_deep_link(u);
                }
            }
            UserEvent::UpdaterState(s) => {
                let v = serde_json::to_value(&s).unwrap_or(Value::Null);
                self.broadcast("updater:state-changed", &v);
            }
            UserEvent::StorageChanged(v) => self.broadcast("storage:changed", &v),
            UserEvent::UpToDate => {
                message_box(
                    self.focused.and_then(|f| self.window(f)).map(|w| &w.window),
                    rfd::MessageLevel::Info,
                    "Check for Updates",
                    "No updates available",
                    rfd::MessageButtons::Ok,
                );
            }
            UserEvent::ShutdownComplete => *control_flow = ControlFlow::Exit,
        }
    }

    fn on_page_loaded(&mut self, id: WindowId) {
        let zoom = self.ctx.settings.zoom_level();
        if let Some(w) = self.window_mut(id) {
            w.remove_overlay();
            if w.persist_zoom {
                if let Some(level) = zoom {
                    w.set_zoom_level(level);
                }
            }
        }
    }

    fn on_menu(&mut self, target: &EventLoopWindowTarget<UserEvent>, e: muda::MenuEvent) {
        let focused = self.focused.unwrap_or_else(|| self.windows.first().map(|w| w.window.id()).unwrap_or(unsafe_dummy_id()));
        match menu::action_for(e.id()) {
            MenuAction::NewWindow => {
                let url = self.main_url();
                self.open_window(target, &url, false);
            }
            MenuAction::Quit => self.request_quit(false),
            MenuAction::CloseWindow => {
                if let Some(t) = self.target_id(focused) {
                    self.close_window(t);
                }
            }
            MenuAction::Reload => {
                if let Some(w) = self.target_window(focused) {
                    let _ = w.webview.reload();
                }
            }
            MenuAction::ZoomIn | MenuAction::ZoomOut | MenuAction::ResetZoom => {
                let a = menu::action_for(e.id());
                if let Some(t) = self.target_id(focused) {
                    if let Some(w) = self.window_mut(t) {
                        let level = match a {
                            MenuAction::ZoomIn => w.zoom_level + window::ZOOM_STEP,
                            MenuAction::ZoomOut => w.zoom_level - window::ZOOM_STEP,
                            _ => 0.0,
                        };
                        w.set_zoom_level(level);
                        self.ctx.settings.set_zoom_level(level);
                    }
                }
            }
            MenuAction::ToggleFullscreen => {
                if let Some(w) = self.target_window(focused) {
                    let fs = w.window.fullscreen().is_some();
                    w.window.set_fullscreen(if fs { None } else { Some(Fullscreen::Borderless(None)) });
                }
            }
            MenuAction::Minimize => {
                if let Some(w) = self.target_window(focused) {
                    w.window.set_minimized(true);
                }
            }
            MenuAction::Docs => crate::ipc::shell_open(menu::DOCS_URL),
            MenuAction::CheckForUpdates => self.ctx.updater.spawn_check(true),
            MenuAction::TrayOpen => self.show_or_create(target),
            MenuAction::Context(id) => self.on_context_click(&id),
            MenuAction::Unknown => {}
        }
    }

    // ------------------------------------------------------------ windows

    /// URL for new windows: dev URL or `https://127.0.0.1:<port>/`.
    pub fn main_url(&self) -> String {
        match (&self.ctx.dev_url, self.ctx.ls_port) {
            (Some(d), _) => d.clone(),
            (None, Some(p)) => window::ls_url(p),
            (None, None) => "about:blank".into(),
        }
    }

    /// Create a window; `persist_zoom` wires zoom restore/persist (§4.2).
    pub fn open_window(&mut self, target: &EventLoopWindowTarget<UserEvent>, url: &str, persist_zoom: bool) {
        match window::create_window(target, &mut self.web_context, &self.ctx, WindowOptions { url, persist_zoom }) {
            Ok(w) => {
                #[cfg(windows)]
                if let Some(m) = &self.app_menu {
                    use tao::platform::windows::WindowExtWindows;
                    m.attach(w.window.hwnd());
                }
                self.focused = Some(w.window.id());
                self.windows.push(w);
            }
            Err(e) => tracing::error!("failed to create window: {e}"),
        }
    }

    /// Tray / second-instance: focus the first window or create one (no zoom persistence).
    pub fn show_or_create(&mut self, target: &EventLoopWindowTarget<UserEvent>) {
        if self.quitting {
            return;
        }
        if let Some(w) = self.windows.first() {
            w.show_and_focus();
        } else {
            let url = self.main_url();
            self.open_window(target, &url, false);
        }
    }

    pub fn window(&self, id: WindowId) -> Option<&AppWindow> {
        self.windows.iter().find(|w| w.window.id() == id)
    }

    pub fn window_mut(&mut self, id: WindowId) -> Option<&mut AppWindow> {
        self.windows.iter_mut().find(|w| w.window.id() == id)
    }

    /// "Target window": the sender if it exists, else focused, else first.
    pub fn target_id(&self, sender: WindowId) -> Option<WindowId> {
        [Some(sender), self.focused]
            .into_iter()
            .flatten()
            .find(|id| self.window(*id).is_some())
            .or_else(|| self.windows.first().map(|w| w.window.id()))
    }

    pub fn target_window(&self, sender: WindowId) -> Option<&AppWindow> {
        self.target_id(sender).and_then(|id| self.window(id))
    }

    /// Close one window; quit if it was the last and runInBackground is off.
    pub fn close_window(&mut self, id: WindowId) {
        self.windows.retain(|w| w.window.id() != id);
        if self.focused == Some(id) {
            self.focused = None;
        }
        if self.windows.is_empty() && !self.quitting && !self.ctx.settings.run_in_background() {
            self.request_quit(false);
        }
    }

    fn broadcast(&self, channel: &str, payload: &Value) {
        for w in &self.windows {
            w.emit(channel, payload);
        }
    }

    fn dispatch_deep_link(&mut self, url: String) {
        if let Some(w) = self.windows.first() {
            w.show_and_focus();
            w.emit("deep-link", &Value::String(url));
        } else {
            self.ctx.pending_link.store(url);
        }
    }

    /// WSL is out of scope for AntiCraft.
    pub fn relaunch_into_wsl(&mut self, distro: String) {
        tracing::warn!("wsl:connect({distro}) requested but WSL is unsupported by AntiCraft; staying local");
    }

    // --------------------------------------------------------------- quit

    /// §2.1 before-quit. `confirm` = Ctrl+Q path.
    pub fn request_quit(&mut self, confirm: bool) {
        if self.quitting {
            return;
        }
        if confirm {
            let Some(parent) = self.focused.and_then(|f| self.window(f)).or_else(|| self.windows.first()) else {
                return; // no window → silently dropped
            };
            // §2.1: buttons [Cancel, Quit], Quit is the default. rfd's
            // MessageBox backend has no separate detail line, so the detail
            // follows the message after a blank line.
            let r = message_box(
                Some(&parent.window),
                rfd::MessageLevel::Warning,
                CONFIRM_QUIT_TITLE,
                &format!("{CONFIRM_QUIT_MESSAGE}\n\n{CONFIRM_QUIT_DETAIL}"),
                rfd::MessageButtons::OkCancelCustom(QUIT_LABEL.into(), "Cancel".into()),
            );
            if r != rfd::MessageDialogResult::Custom(QUIT_LABEL.into()) {
                return;
            }
        }
        self.quitting = true;
        self.pending_context = None;
        self.windows.clear();
        self.tray = None;
        let backend = self.backend.take().unwrap_or_default();
        let proxy = self.ctx.proxy.clone();
        let updater = self.ctx.updater.clone();
        self.ctx.rt.spawn(async move {
            backend.shutdown().await;
            // §13: the installer runs only once the LS and bridge are down.
            updater.run_pending_install();
            let _ = proxy.send_event(UserEvent::ShutdownComplete);
        });
    }
}

pub const CONFIRM_QUIT_TITLE: &str = "Confirm Quit";
pub const CONFIRM_QUIT_MESSAGE: &str = "Are you sure you want to quit?";
pub const CONFIRM_QUIT_DETAIL: &str = "There may be agents or background tasks running.";
pub const QUIT_LABEL: &str = "Quit";

/// Fallback id when no window exists (only used for lookups that then fail).
fn unsafe_dummy_id() -> WindowId {
    unsafe { WindowId::dummy() }
}
