# AntiCraft module interfaces

Each module can be implemented on its own against these contracts. Section numbers refer to `SPEC.md`.

## Threading model

- The **UI thread** owns the tao `EventLoop<UserEvent>`, every `AppWindow` and webview, the muda menus and the tray. Only `App` (app.rs) touches them.
- Background work runs on a **multi-thread tokio runtime**: the LS, the host bridge, the updater, the second-instance pipe and async IPC. It reaches the UI only through `EventLoopProxy<UserEvent>::send_event`.
- `StorageManager`, `SettingsService` and `Updater` are `Send + Sync` and shared via `Arc` (or a cheap `Clone`).

## core

### paths.rs
- `const APP_NAME = "Antigravity"`, `APP_DATA_DIR_NAME = "antigravity"`, `APP_VERSION = "2.26.0"`, `ENV_USER_DATA = "ANTICRAFT_USER_DATA"`.
- `struct Paths { user_data, logs, app_storage, wsl_state, devtools_active_port, node_wrapper_dir, webview2_data, ls_log, main_log, home, gemini_config, installed_root }`
- `Paths::resolve(cli_user_data: Option<&Path>) -> anyhow::Result<Paths>`: precedence is the CLI arg, then the env var, then `%APPDATA%\AntiCraft`.
- `Paths::from_parts(user_data, home, installed_root)` (for tests)
- Helpers:
  - `installed_resources()`
  - `installed_electron_exe()`
  - `ide_exe()`
  - `chrome_devtools_mcp_js()`
  - `ensure_dirs()`

### storage.rs (§6.1)
- `type Changes = serde_json::Map<String, Value>`: a `null` value deletes the key.
- `type ChangeListener = Arc<dyn Fn(&Changes) + Send + Sync>`
- `StorageManager::new(path, defaults: BTreeMap<String,String>)`
- `get_items() -> Map`: defaults are merged in and never written to disk.
- `get_string(key)`
- `default_for(key)`
- `update_items(&Changes) -> Result<(), StorageError>`: writes the full file, then notifies listeners. Only mkdir and write errors are returned.
- `subscribe(listener) -> SubscriptionId`
- `unsubscribe(id)`

### settings.rs
- Key constants:
  - `KEY_RUN_IN_BACKGROUND`
  - `KEY_KEEP_COMPUTER_AWAKE`
  - `KEY_AUTO_CHECK_FOR_UPDATES`
  - `KEY_ZOOM_LEVEL`
  - `KEY_IDE_WIZARD_SHOWN`
- `default_settings()`
- `enum SettingChange { RunInBackground(Option<bool>), KeepComputerAwake(Option<bool>), AutoCheckForUpdates(Option<bool>) }`: `None` means null, i.e. revert to the default.
- `PowerBlocker::set(bool)`: wraps `SetThreadExecutionState`.
- `SettingsService::new(Arc<StorageManager>) -> Arc<Self>`: applies keep-awake right away.
- Getters:
  - `run_in_background()`
  - `keep_computer_awake()`
  - `auto_check_for_updates()`
  - `zoom_level() -> Option<f64>`
  - `set_zoom_level(f64)`
  - `on_change(Arc<dyn Fn(&SettingChange)>)`

### theme.rs (§4.2)
- `enum ThemeMode { Dark, Light }`
- `.colors() -> ThemeColors { bg, fg }`
- `mode_from_raw(Option<&str>, os_is_dark)`
- `load_theme(&Path, os_is_dark)`
- `os_prefers_dark()`
- `hex_rgb("#RRGGBB")`

## language-server (ls.rs, §3)
- `discover_binary(cli: Option<&Path>, &Paths) -> PathBuf`
- `target_port_from_env(Option<&str>) -> String`
- `struct LsArgs { app_version, https_server_port, csrf_token, app_data_dir, host_bridge: Option<(url, token)>, headless, wsl }`
- `build_args(&LsArgs) -> Vec<String>`: exact §3.1 order.
- `node_wrapper_content(exe)`
- `write_node_wrapper(dir, exe) -> io::Result<bool>`: writes only when the content changed.
- `build_env(base, &Paths)`
- `prepare_environment(&Paths)`: writes the wrapper only when the installed `Antigravity.exe` exists.
- `enum LineEvent { Port(u16), OAuthUrl(String), OpenUrl(String) }`
- `classify_line(&str) -> Vec<LineEvent>`
- `parse_stamp(&str)`
- `async query_stamp(&Path) -> Option<String>`
- `StderrRing`, `extract_crash_trace(&str)`
- `RestartPolicy::on_crash(Instant) -> RestartDecision::{Restart(n), GiveUp}`
- `struct LsCallbacks { on_oauth_url, on_open_url: Option<Arc<dyn Fn(&str)>>, on_port_changed: Option<Arc<dyn Fn(u16)>> }`
- `struct LsConfig { program, args, env, keep_stdin_open, log_path, ready_timeout, restart_delay }`
- `LanguageServer::start(LsConfig, LsCallbacks) -> Result<(LanguageServer, u16), (LsError, LanguageServer)>`
  - On error the handle still owns a timed-out process, so call `shutdown()` on it.
  - `port()`
  - `is_running()`
  - `stderr_tail()`
  - `write_stdin_line(&str)`
  - `shutdown()`: terminate, wait 5 s, force kill.
- `LsError::{Spawn, Timeout(secs), EarlyExit{code,signal}, Io}`
- Every child is assigned to a kill-on-close Job Object.

## host-bridge (host_bridge.rs, proto.rs, §7)
- `trait UpdateStatusProvider: Send + Sync { fn get_update_status(&self) -> proto::UpdateStatus; fn apply_update(&self) -> bool; }`
- `HostBridge::start(Arc<dyn UpdateStatusProvider>) -> io::Result<HostBridge>`
- `HostBridge::start_with_token(...)` (for tests)
- `url()`, `token()`, `port()`, `async shutdown(self)`
- `generate_token()`: 64 lowercase hex characters.
- `check_gate(peer, &HeaderMap, expected_host, token) -> Result<(), GateRejection>`
- `parse_bearer(&str)`
- `is_allowed_peer(IpAddr)`
- proto.rs:
  - `trait Message { encode, decode, encode_to_vec }`
  - Messages: `GetUpdateStatusRequest`, `GetUpdateStatusResponse { status: Option<UpdateStatus> }`, `UpdateStatus { current_version, latest_version, update_available }`, `ApplyUpdateRequest`, `ApplyUpdateResponse { accepted }`. All are also serde types with proto3 JSON names.
  - `PATH_GET_UPDATE_STATUS`, `PATH_APPLY_UPDATE`

## updater (updater.rs, §13)
- `enum UpdateStateType`: `.wire()` and `from_wire()` use the exact wire strings.
- `struct UpdateStateObj { type, update?: { version } }` (serde)
- `UpdateStateObj::new(t, Option<String>)`, `idle()`
- `parse_manifest(&str) -> UpdateManifest { version, path, sha512 }`
- `is_newer(current, latest)`
- `verify_sha512(data, b64)`
- `status_from_state(current_version, &UpdateStateObj) -> proto::UpdateStatus`
- `UpdaterConfig { feed_url, current_version, cache_dir, headless }`
  - The default cache dir is `%LOCALAPPDATA%\anticraft-updater`.
- `Updater::new(cfg, rt::Handle)` (`Clone`)
  - `state()`
  - `subscribe(Arc<dyn Fn(&UpdateStateObj)>)`
  - `set_up_to_date_callback`
  - `set_exit_request`
  - `set_state`
  - `apply()`: broadcasts `ready` only.
  - `init(auto_enabled: impl Fn() -> bool)`: 10 s delay, then hourly.
  - `set_auto_check(bool)`
  - `spawn_check(manual)`
  - `async check(manual)`
  - `quit_and_install()`
- `impl UpdateStatusProvider for Updater`

## ui
- **app.rs**
  - `enum UserEvent { Ipc, IpcReply, PageLoaded, TitleChanged, OpenExternal, Quit{confirm}, ContextMenuDismissed, Menu, Tray, PortChanged, DeepLink, SecondInstance, UpdaterState, StorageChanged, UpToDate, ShutdownComplete }`
  - `struct AppContext { paths, storage, settings, updater, authorities, http, rt, proxy, pending_link, ls_port, dev_url }`
  - `struct Backend { ls, bridge }`, with `async shutdown()` that stops the LS first and the bridge last.
  - `App::new(ctx, backend)`
  - `App::install_event_forwarders(&proxy)`
  - `App::run(event_loop) -> !`
  - Window helpers:
    - `open_window`
    - `show_or_create`
    - `target_id` / `target_window`
    - `close_window`
    - `request_quit(confirm)`
  - `message_box(parent, level, title, desc, buttons)`
- **window.rs**
  - `create_window(target, &mut WebContext, &AppContext, WindowOptions{url, persist_zoom}) -> AppWindow`
  - `AppWindow` methods:
    - `resize`
    - `remove_overlay`
    - `emit(channel, &Value)`
    - `reply(id, &IpcResult)`
    - `set_zoom_level`
    - `set_page_title`
    - `load_url`
    - `show_and_focus`
    - `toggle_devtools`
  - Free functions:
    - `ls_url(port)`
    - `truncate_title`
    - `zoom_factor`
    - `loading_overlay_html`
    - `emit_script` / `reply_script`
    - `install_loopback_cert_bypass`
    - `is_loopback_url`
  - `browser_args(port)`, constant `BRIDGE_JS`.
- **bridge.js**
  - Renderer to main: `window.ipc.postMessage(JSON {id, channel, args})`. `id == 0` means fire-and-forget.
  - Main to renderer: `__anticraftIpc.resolve(id, ok, value)`, `__anticraftIpc.emit(channel, payload)`, `__anticraftSetZoomFactor(f)`.
  - Internal channels: `__anticraft:new-window`, `__anticraft:quit`.
- **ipc.rs**
  - `type IpcResult = Result<Option<Value>, String>`
  - `parse_request`
  - `is_trusted_origin(page_url, ls_port, dev_url)`
  - `is_allowed_external`
  - `shell_open`
  - `reveal_in_explorer`
  - `file_url_to_path`
  - `decode_wsl_output`, `parse_wsl_list`, `async wsl_state()`
  - `changes_value`
  - `App::handle_ipc` dispatches every §5.2 channel.
- **menu.rs**
  - `ids::*`
  - `MenuAction`, `action_for(&MenuId)`
  - `AppMenu::build()`, `.attach(hwnd)`
  - `ContextMenuItem` (serde, §5.1)
  - `build_context_menu(&[ContextMenuItem]) -> Option<BuiltContextMenu{menu, ids}>`
  - `DOCS_URL`
- **tray.rs**
  - `agent_label(n)`
  - `app_icon_rgba(size)`
  - `AppTray::create()`, `.set_agent_count(n)`
- **deeplink.rs**
  - `scheme_for(app_name)`, `scheme()`
  - `find_in_argv(&[S], scheme)`
  - `PendingDeepLink::{store, take}`
- **protocol.rs**
  - `register(scheme, exe)`
  - `registered_command(scheme)`
  - `unregister_if_ours(scheme, exe) -> UnregisterOutcome::{Removed, NotRegistered, OwnedByOther(cmd)}`
  - `open_command_for`, `command_points_to`
- **plugin_scheme.rs**
  - `AuthorityMap::{replace, get}`
  - `authority_of(&Uri)`
  - `resolve_target(origin, path_and_query)`
  - `async proxy(&reqwest::Client, &AuthorityMap, Request<Vec<u8>>) -> Response<Vec<u8>>`
- **single_instance.rs**
  - `acquire(user_data) -> Instance::{Primary(InstanceLock), Secondary{pipe_name}}`
  - `forward_argv(pipe, &[String])`
  - `async serve(pipe, Fn(Vec<String>))`

## main
- **cli.rs**
  - `Cli { user_data_dir, ls_path, wsl_distro, headless, no_register_protocol, unregister_protocol, dev_url, rest }`
  - `is_headless()`
  - `effective_dev_url()`
  - `requested_wsl_distro()`
- **log.rs**
  - `init(main_log) -> LogGuard`: writes to the file and stderr; `RUST_LOG` controls the filter.
- **main.rs**
  - Startup order:
    1. `--unregister-protocol` (handle and exit)
    2. Single instance
    3. Protocol registration
    4. Storage and settings
    5. Pending deep link
    6. `--stamp`
    7. Updater
    8. Headless or UI
    9. Binary check
    10. Bridge and LS (`start_backend`)
    11. Pipe server
    12. Auto-check
    13. `App::run`
