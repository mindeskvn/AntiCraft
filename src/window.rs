//! Main window: tao window + WebView2 via wry (SPEC §4).
//!
//! Each window hosts two child webviews sharing one WebView2 environment
//! (`<userData>\WebView2`): the app webview and a loading overlay that is
//! dropped on the first `Finished` page load (§4.3).

use crate::app::{AppContext, UserEvent};
use crate::paths::APP_NAME;
use crate::plugin_scheme;
use crate::theme::{hex_rgb, ThemeMode};
use serde_json::Value;
use tao::dpi::LogicalSize;
use tao::event_loop::EventLoopWindowTarget;
use tao::window::{Window, WindowBuilder};
use wry::{NewWindowResponse, PageLoadEvent, Rect, WebContext, WebView, WebViewBuilder, WebViewBuilderExtWindows};

pub const WIDTH: f64 = 1400.0;
pub const HEIGHT: f64 = 900.0;
pub const MIN_WIDTH: f64 = 500.0;
pub const MIN_HEIGHT: f64 = 400.0;
pub const TITLE_MAX: usize = 25;
pub const ZOOM_STEP: f64 = 0.5;

/// WebView2 browser args. All webviews sharing the data dir must use the same
/// args; wry's defaults are repeated because overriding replaces them. There
/// is no global certificate ignore: `--allow-insecure-localhost` is scoped to
/// loopback, and [`install_loopback_cert_bypass`] scopes the rest.
///
/// `--remote-debugging-port` is the user's `--remote-debugging-port` switch,
/// or `0` when absent (§14). The CDP endpoint is loopback-only but
/// unauthenticated; this is accepted residual risk (see README).
pub fn browser_args(remote_debugging_port: u16) -> String {
    format!(
        "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection \
--allow-insecure-localhost --remote-debugging-port={remote_debugging_port} \
--disable-blink-features=AutomationControlled"
    )
}

/// The preload replacement injected into the main frame of every page.
pub const BRIDGE_JS: &str = include_str!("bridge.js");

/// LS UI URL (`https://127.0.0.1:<port>/`, trailing slash included).
pub fn ls_url(port: u16) -> String {
    format!("https://127.0.0.1:{port}/")
}

/// §4.2 title rule: more than 25 chars → first 25 + `...`; `None` = unchanged.
pub fn truncate_title(t: &str) -> Option<String> {
    (t.chars().count() > TITLE_MAX).then(|| format!("{}...", t.chars().take(TITLE_MAX).collect::<String>()))
}

/// Chromium zoom level → scale factor (each level step is 20%).
pub fn zoom_factor(level: f64) -> f64 {
    1.2f64.powf(level)
}

/// §4.3 loading overlay: background page with a centered 64 px pulsing glyph
/// (opacity 0.25↔0.5 over 2.2 s); the whole page is a drag region.
pub fn loading_overlay_html(bg: &str, fg: &str) -> String {
    format!(
        r#"<!doctype html><html><head><meta charset="utf-8"><style>
html,body{{margin:0;height:100%;background:{bg};overflow:hidden}}
body{{display:flex;align-items:center;justify-content:center;-webkit-app-region:drag;app-region:drag}}
.g{{width:64px;height:64px;border-radius:50%;border:12px solid {fg};box-sizing:border-box;animation:p 2.2s ease-in-out infinite}}
@keyframes p{{0%,100%{{opacity:.25}}50%{{opacity:.5}}}}
</style></head><body><div class="g" role="img" aria-label="Loading"></div></body></html>"#
    )
}

/// JS that delivers an event to `bridge.js` listeners.
pub fn emit_script(channel: &str, payload: &Value) -> String {
    let ch = serde_json::to_string(channel).unwrap_or_else(|_| "\"\"".into());
    format!("window.__anticraftIpc&&window.__anticraftIpc.emit({ch},{payload})")
}

/// JS that settles an invoke promise. `Ok(None)` resolves with `undefined`.
pub fn reply_script(id: u64, result: &Result<Option<Value>, String>) -> String {
    match result {
        Ok(None) => format!("window.__anticraftIpc&&window.__anticraftIpc.resolve({id},true)"),
        Ok(Some(v)) => format!("window.__anticraftIpc&&window.__anticraftIpc.resolve({id},true,{v})"),
        Err(e) => format!(
            "window.__anticraftIpc&&window.__anticraftIpc.resolve({id},false,{})",
            Value::String(e.clone())
        ),
    }
}

/// Options for [`create_window`].
pub struct WindowOptions<'a> {
    pub url: &'a str,
    /// Wire zoom restore/persist (only the initial window and Ctrl+Shift+N).
    pub persist_zoom: bool,
}

/// One shell window.
pub struct AppWindow {
    pub window: Window,
    pub webview: WebView,
    overlay: Option<WebView>,
    pub zoom_level: f64,
    pub persist_zoom: bool,
}

fn full_bounds(window: &Window) -> Rect {
    let s = window.inner_size();
    Rect {
        position: wry::dpi::PhysicalPosition::new(0, 0).into(),
        size: wry::dpi::PhysicalSize::new(s.width, s.height).into(),
    }
}

/// Build a window with its webview and loading overlay.
pub fn create_window(
    target: &EventLoopWindowTarget<UserEvent>,
    web_context: &mut WebContext,
    ctx: &AppContext,
    opts: WindowOptions<'_>,
) -> anyhow::Result<AppWindow> {
    let theme = crate::theme::load_theme(&ctx.paths.gemini_config, crate::theme::os_prefers_dark());
    let colors = theme.colors();
    let (r, g, b) = hex_rgb(colors.bg);
    let icon = tao::window::Icon::from_rgba(crate::tray::app_icon_rgba(32), 32, 32).ok();
    let window = WindowBuilder::new()
        .with_title(APP_NAME)
        .with_inner_size(LogicalSize::new(WIDTH, HEIGHT))
        .with_min_inner_size(LogicalSize::new(MIN_WIDTH, MIN_HEIGHT))
        .with_window_icon(icon)
        .with_theme(Some(match theme {
            ThemeMode::Dark => tao::window::Theme::Dark,
            ThemeMode::Light => tao::window::Theme::Light,
        }))
        .build(target)?;

    let wid = window.id();
    let p_ipc = ctx.proxy.clone();
    let p_load = ctx.proxy.clone();
    let p_title = ctx.proxy.clone();
    let p_new = ctx.proxy.clone();
    let (map, client, rt) = (ctx.authorities.clone(), ctx.http.clone(), ctx.rt.clone());

    let webview = WebViewBuilder::new_with_web_context(web_context)
        .with_url(opts.url)
        .with_bounds(full_bounds(&window))
        .with_background_color((r, g, b, 255))
        .with_initialization_script_for_main_only(BRIDGE_JS, true)
        .with_initialization_script_for_main_only(plugin_scheme::PLUGIN_URL_SHIM_JS, false)
        .with_additional_browser_args(ctx.browser_args.clone())
        .with_hotkeys_zoom(false)
        .with_devtools(cfg!(debug_assertions))
        // No `with_ipc_handler`: wry would define a frozen `window.ipc`, and the
        // LS web UI assigns its own `window.ipc`, which then throws and the app
        // never mounts. The bridge posts through `chrome.webview` instead and the
        // message handler is attached below.
        .with_on_page_load_handler(move |ev, _url| {
            if let PageLoadEvent::Finished = ev {
                let _ = p_load.send_event(UserEvent::PageLoaded(wid));
            }
        })
        .with_document_title_changed_handler(move |t| {
            let _ = p_title.send_event(UserEvent::TitleChanged(wid, t));
        })
        .with_new_window_req_handler(move |url, _features| {
            let _ = p_new.send_event(UserEvent::OpenExternal(url));
            NewWindowResponse::Deny
        })
        .with_asynchronous_custom_protocol("plugin".into(), move |_id, req, responder| {
            let (map, client) = (map.clone(), client.clone());
            rt.spawn(async move {
                responder.respond(plugin_scheme::proxy(&client, &map, req).await);
            });
        })
        .build_as_child(&window)?;
    #[cfg(windows)]
    install_loopback_cert_bypass(&webview);
    #[cfg(windows)]
    install_message_handler(&webview, move |origin, body| {
        let _ = p_ipc.send_event(UserEvent::Ipc { window: wid, origin, body });
    });

    let overlay = WebViewBuilder::new_with_web_context(web_context)
        .with_html(loading_overlay_html(colors.bg, colors.fg))
        .with_bounds(full_bounds(&window))
        .with_background_color((r, g, b, 255))
        .with_additional_browser_args(ctx.browser_args.clone())
        .build_as_child(&window)
        .map_err(|e| tracing::warn!("loading overlay unavailable: {e}"))
        .ok();

    Ok(AppWindow { window, webview, overlay, zoom_level: 0.0, persist_zoom: opts.persist_zoom })
}

impl AppWindow {
    /// Keep both child webviews sized to the content area.
    pub fn resize(&self) {
        let b = full_bounds(&self.window);
        let _ = self.webview.set_bounds(b);
        if let Some(o) = &self.overlay {
            let _ = o.set_bounds(full_bounds(&self.window));
        }
    }

    /// Drop the overlay (first `Finished` load). Returns whether it was present.
    pub fn remove_overlay(&mut self) -> bool {
        self.overlay.take().is_some()
    }

    pub fn emit(&self, channel: &str, payload: &Value) {
        let _ = self.webview.evaluate_script(&emit_script(channel, payload));
    }

    pub fn reply(&self, id: u64, result: &Result<Option<Value>, String>) {
        if id != 0 {
            let _ = self.webview.evaluate_script(&reply_script(id, result));
        }
    }

    /// Apply a Chromium zoom level and tell `bridge.js` the factor.
    pub fn set_zoom_level(&mut self, level: f64) {
        self.zoom_level = level;
        let f = zoom_factor(level);
        let _ = self.webview.zoom(f);
        let _ = self.webview.evaluate_script(&format!(
            "window.__anticraftSetZoomFactor&&window.__anticraftSetZoomFactor({f})"
        ));
    }

    /// §4.2 page-title update.
    pub fn set_page_title(&self, title: &str) {
        self.window.set_title(&truncate_title(title).unwrap_or_else(|| title.to_string()));
    }

    pub fn load_url(&self, url: &str) {
        let _ = self.webview.load_url(url);
    }

    /// Restore if minimized, show and focus.
    pub fn show_and_focus(&self) {
        if self.window.is_minimized() {
            self.window.set_minimized(false);
        }
        self.window.set_visible(true);
        self.window.set_focus();
    }

    pub fn toggle_devtools(&self) {
        #[cfg(debug_assertions)]
        {
            if self.webview.is_devtools_open() {
                self.webview.close_devtools();
            } else {
                self.webview.open_devtools();
            }
        }
    }
}

/// §3.6: accept certificate errors only for `127.0.0.1` / `localhost`;
/// everything else uses default verification.
#[cfg(windows)]
pub fn install_loopback_cert_bypass(wv: &WebView) {
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        ICoreWebView2_14, COREWEBVIEW2_SERVER_CERTIFICATE_ERROR_ACTION_ALWAYS_ALLOW,
        COREWEBVIEW2_SERVER_CERTIFICATE_ERROR_ACTION_DEFAULT,
    };
    use webview2_com::ServerCertificateErrorDetectedEventHandler;
    use windows_core::{Interface, PWSTR};
    use wry::WebViewExtWindows;

    let Ok(core) = wv.webview().cast::<ICoreWebView2_14>() else {
        tracing::warn!("WebView2 runtime lacks ICoreWebView2_14; relying on --allow-insecure-localhost");
        return;
    };
    let handler = ServerCertificateErrorDetectedEventHandler::create(Box::new(|_, args| {
        let Some(args) = args else { return Ok(()) };
        let mut uri = PWSTR::null();
        unsafe { args.RequestUri(&mut uri)? };
        let uri = webview2_com::take_pwstr(uri);
        let action = if is_loopback_url(&uri) {
            COREWEBVIEW2_SERVER_CERTIFICATE_ERROR_ACTION_ALWAYS_ALLOW
        } else {
            COREWEBVIEW2_SERVER_CERTIFICATE_ERROR_ACTION_DEFAULT
        };
        unsafe { args.SetAction(action) }
    }));
    let mut token = 0i64;
    if let Err(e) = unsafe { core.add_ServerCertificateErrorDetected(&handler, &mut token) } {
        tracing::warn!("failed to install certificate handler: {e}");
    }
}

/// Deliver main-frame `chrome.webview.postMessage` strings as `(source_url, body)`.
/// Registered without touching any page global, so the web UI owns `window.ipc`.
#[cfg(windows)]
pub fn install_message_handler(wv: &WebView, on_message: impl Fn(String, String) + 'static) {
    use webview2_com::{take_pwstr, WebMessageReceivedEventHandler};
    use windows_core::PWSTR;
    use wry::WebViewExtWindows;

    let handler = WebMessageReceivedEventHandler::create(Box::new(move |_, args| {
        let Some(args) = args else { return Ok(()) };
        let mut src = PWSTR::null();
        unsafe { args.Source(&mut src)? };
        let mut msg = PWSTR::null();
        // Non-string messages (postMessage of an object) are ignored.
        if unsafe { args.TryGetWebMessageAsString(&mut msg) }.is_ok() {
            on_message(take_pwstr(src), take_pwstr(msg));
        }
        Ok(())
    }));
    let mut token = 0i64;
    if let Err(e) = unsafe { wv.webview().add_WebMessageReceived(&handler, &mut token) } {
        tracing::warn!("failed to install IPC message handler: {e}");
    }
}

/// Host is exactly `127.0.0.1` or `localhost`.
pub fn is_loopback_url(u: &str) -> bool {
    url::Url::parse(u)
        .ok()
        .and_then(|u| u.host_str().map(|h| h == "127.0.0.1" || h == "localhost"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_and_urls() {
        assert_eq!(truncate_title("short"), None);
        assert_eq!(truncate_title(&"a".repeat(30)).unwrap(), format!("{}...", "a".repeat(25)));
        assert_eq!(ls_url(5), "https://127.0.0.1:5/");
        assert!(is_loopback_url("https://127.0.0.1:9/x"));
        assert!(!is_loopback_url("https://127.0.0.1.evil.com/"));
        assert!((zoom_factor(0.0) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn scripts() {
        assert_eq!(
            reply_script(3, &Ok(Some(Value::Null))),
            "window.__anticraftIpc&&window.__anticraftIpc.resolve(3,true,null)"
        );
        assert!(reply_script(3, &Err("x\"y".into())).ends_with(r#"false,"x\"y")"#));
        assert!(emit_script("deep-link", &Value::from("u")).contains(r#"emit("deep-link","u")"#));
    }
}
