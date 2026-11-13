//! AntiCraft entry point: startup orchestration (SPEC §2).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod cli;
mod deeplink;
mod host_bridge;
mod ipc;
mod log;
mod ls;
mod menu;
mod paths;
mod plugin_scheme;
mod protocol;
mod proto;
mod settings;
mod single_instance;
mod storage;
mod theme;
mod tray;
mod updater;
mod window;

use anyhow::Context as _;
use std::sync::Arc;

fn main() {
    let code = match run() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("fatal: {e:#}");
            eprintln!("anticraft: {e:#}");
            1
        }
    };
    std::process::exit(code);
}

fn run() -> anyhow::Result<i32> {
    let (cli, ignored_switches) = cli::Cli::parse_lenient();
    let paths = Arc::new(paths::Paths::resolve(cli.user_data_dir.as_deref())?);
    paths.ensure_dirs().context("creating userData directories")?;
    let _log = log::init(&paths.main_log);
    if !ignored_switches.is_empty() {
        tracing::info!("ignoring unsupported switches: {}", ignored_switches.join(" "));
    }
    let scheme = deeplink::scheme();
    let exe = std::env::current_exe().context("resolving current exe")?;

    if cli.unregister_protocol {
        match protocol::unregister_if_ours(&scheme, &exe)? {
            protocol::UnregisterOutcome::Removed => println!("Removed {scheme}:// registration."),
            protocol::UnregisterOutcome::NotRegistered => println!("{scheme}:// is not registered."),
            protocol::UnregisterOutcome::OwnedByOther(cmd) => {
                println!("{scheme}:// points at another program ({cmd}); left unchanged.")
            }
        }
        return Ok(0);
    }

    // §2 step 1: single instance (per userData).
    let argv: Vec<String> = std::env::args().collect();
    let lock = match single_instance::acquire(&paths.user_data)? {
        single_instance::Instance::Primary(l) => l,
        single_instance::Instance::Secondary { pipe_name } => {
            if let Err(e) = single_instance::forward_argv(&pipe_name, &argv) {
                tracing::warn!("could not reach the running instance: {e}");
            }
            return Ok(0);
        }
    };

    let headless = cli.is_headless();
    let dev_url = cli.effective_dev_url();
    if let Some(d) = cli.resolved_wsl_distro(&paths.wsl_state) {
        tracing::warn!("WSL distro '{d}' requested, but WSL is unsupported by AntiCraft; running locally");
    }

    // §2 step 6: protocol registration (default on).
    if !cli.no_register_protocol {
        if let Err(e) = protocol::register(&scheme, &exe) {
            tracing::warn!("failed to register {scheme}://: {e}");
        }
    }

    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;

    // Second-instance pipe: served before the (slow) stamp query and LS start
    // so a forwarded deep link (e.g. the OAuth callback) is never refused.
    // Messages are buffered until the UI or headless loop consumes them.
    let (second_tx, second_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<String>>();
    {
        let name = lock.pipe_name.clone();
        rt.spawn(async move {
            #[cfg(windows)]
            if let Err(e) = single_instance::serve(name, move |a| {
                let _ = second_tx.send(a);
            })
            .await
            {
                tracing::warn!("second-instance pipe stopped: {e}");
            }
            #[cfg(not(windows))]
            let _ = (name, second_tx);
        });
    }

    // whenReady step 2: storage + settings.
    let storage = Arc::new(storage::StorageManager::new(&paths.app_storage, settings::default_settings()));
    let settings = settings::SettingsService::new(storage.clone());
    let pending_link = deeplink::PendingDeepLink::default();
    if let Some(u) = deeplink::find_in_argv(&argv, &scheme) {
        pending_link.store(u);
    }

    let ls_bin = ls::discover_binary(cli.ls_path.as_deref(), &paths);
    if let Some(cl) = rt.block_on(ls::query_stamp(&ls_bin)) {
        tracing::info!("language server build CL {cl}");
    }

    let updater = updater::Updater::new(
        updater::UpdaterConfig { headless, ..Default::default() },
        rt.handle().clone(),
    );

    if headless {
        if dev_url.is_some() {
            tracing::info!("DEV_URL is ignored in headless mode (no window)");
        }
        let code = rt.block_on(run_headless(paths, ls_bin, updater, settings, second_rx, scheme));
        // The stdin reader sits on a blocking-pool thread; do not wait for it.
        rt.shutdown_timeout(std::time::Duration::from_secs(1));
        drop(lock);
        return code;
    }

    // UI path.
    let event_loop = tao::event_loop::EventLoopBuilder::<app::UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    app::App::install_event_forwarders(&proxy);
    wire_broadcasts(&proxy, &storage, &updater);

    let mut backend = app::Backend::default();
    let mut ls_port = None;
    if dev_url.is_none() {
        if !ls_bin.exists() {
            app::message_box(
                None,
                rfd::MessageLevel::Error,
                "Binary not found",
                &format!("The language server was not found at:\n{}", ls_bin.display()),
                rfd::MessageButtons::Ok,
            );
            return Ok(1);
        }
        let p = proxy.clone();
        let started = rt.block_on(start_backend(&paths, &ls_bin, &updater, false, move |port| {
            let _ = p.send_event(app::UserEvent::PortChanged(port));
        }));
        match started {
            Ok((b, port)) => {
                backend = b;
                ls_port = Some(port);
            }
            Err((e, b)) => {
                rt.block_on(b.shutdown());
                app::message_box(None, rfd::MessageLevel::Error, "Startup failed", &format!("{e:#}"), rfd::MessageButtons::Ok);
                return Ok(1);
            }
        }
        tracing::info!(
            "Local: {}  LS log: {}  Main log: {}",
            window::ls_url(ls_port.unwrap_or(0)),
            paths.ls_log.display(),
            paths.main_log.display()
        );
    }

    // Deliver buffered and future second-instance argv to the UI loop.
    {
        let p = proxy.clone();
        let mut rx = second_rx;
        rt.spawn(async move {
            while let Some(a) = rx.recv().await {
                let _ = p.send_event(app::UserEvent::SecondInstance(a));
            }
        });
    }

    // §13 auto-updater.
    {
        let p = proxy.clone();
        updater.set_up_to_date_callback(Arc::new(move || {
            let _ = p.send_event(app::UserEvent::UpToDate);
        }));
        let p = proxy.clone();
        updater.set_exit_request(Arc::new(move || {
            let _ = p.send_event(app::UserEvent::Quit { confirm: false });
        }));
        wire_auto_check(&updater, &settings);
    }

    let ctx = app::AppContext {
        paths,
        storage,
        settings,
        updater,
        authorities: plugin_scheme::AuthorityMap::default(),
        http: plugin_scheme::proxy_client(),
        rt: rt.handle().clone(),
        proxy,
        pending_link,
        ls_port,
        dev_url,
        browser_args: window::browser_args(cli.remote_debugging_port.unwrap_or(0)),
    };
    let app = app::App::new(ctx, backend);
    // Keep the runtime and instance lock alive for the process lifetime.
    std::mem::forget(rt);
    std::mem::forget(lock);
    app.run(event_loop)
}

/// Storage and updater broadcasts → UI events.
fn wire_broadcasts(
    proxy: &tao::event_loop::EventLoopProxy<app::UserEvent>,
    storage: &Arc<storage::StorageManager>,
    updater: &updater::Updater,
) {
    let p = proxy.clone();
    storage.subscribe(Arc::new(move |c| {
        let _ = p.send_event(app::UserEvent::StorageChanged(ipc::changes_value(c)));
    }));
    let p = proxy.clone();
    updater.subscribe(Arc::new(move |s| {
        let _ = p.send_event(app::UserEvent::UpdaterState(s.clone()));
    }));
}

/// `autoCheckForUpdates` scheduling (10 s delay, hourly; toggles apply immediately).
fn wire_auto_check(updater: &updater::Updater, settings: &Arc<settings::SettingsService>) {
    let s = settings.clone();
    updater.init(move || s.auto_check_for_updates());
    let u = updater.clone();
    settings.on_change(Arc::new(move |c| {
        if let settings::SettingChange::AutoCheckForUpdates(v) = c {
            u.set_auto_check(v.unwrap_or(true));
        }
    }));
}

/// §2 steps 12-14: CSRF token, host bridge (non-fatal), LS start.
async fn start_backend(
    paths: &paths::Paths,
    ls_bin: &std::path::Path,
    updater: &updater::Updater,
    headless: bool,
    on_port: impl Fn(u16) + Send + Sync + 'static,
) -> Result<(app::Backend, u16), (anyhow::Error, app::Backend)> {
    let csrf = uuid::Uuid::new_v4().to_string();
    let bridge = match host_bridge::HostBridge::start(Arc::new(updater.clone())).await {
        Ok(b) => Some(b),
        Err(e) => {
            tracing::error!("host bridge failed to start: {e}");
            None
        }
    };
    let args = ls::build_args(&ls::LsArgs {
        app_version: paths::APP_VERSION.into(),
        https_server_port: ls::target_port_from_env(std::env::var(ls::ENV_LS_PORT).ok().as_deref()),
        csrf_token: csrf,
        app_data_dir: paths::APP_DATA_DIR_NAME.into(),
        host_bridge: bridge.as_ref().map(|b| (b.url(), b.token().to_string())),
        headless,
        wsl: false,
    });
    let cfg = ls::LsConfig {
        program: ls_bin.to_path_buf(),
        args,
        env: Some(ls::prepare_environment(paths)),
        keep_stdin_open: headless,
        log_path: paths.ls_log.clone(),
        ready_timeout: ls::READY_TIMEOUT,
        restart_delay: ls::RESTART_DELAY,
    };
    let cbs = ls::LsCallbacks {
        on_oauth_url: Some(Arc::new(move |u: &str| {
            if headless {
                println!("\n==== Sign in ====\nVisit this URL, then paste the code here:\n{u}\n");
            }
        })),
        on_open_url: Some(Arc::new(|u: &str| ipc::shell_open(u))),
        on_port_changed: Some(Arc::new(move |p| {
            tracing::info!("[Auto-Restart] Port changed! {p}");
            on_port(p);
        })),
    };
    match ls::LanguageServer::start(cfg, cbs).await {
        Ok((ls, port)) => Ok((app::Backend { ls: Some(ls), bridge }, port)),
        Err((e, ls)) => Err((e.into(), app::Backend { ls: Some(ls), bridge })),
    }
}

/// Headless mode: no UI, stdin forwarded to the LS, Ctrl+C quits.
async fn run_headless(
    paths: Arc<paths::Paths>,
    ls_bin: std::path::PathBuf,
    updater: updater::Updater,
    settings: Arc<settings::SettingsService>,
    mut second_rx: tokio::sync::mpsc::UnboundedReceiver<Vec<String>>,
    scheme: String,
) -> anyhow::Result<i32> {
    if !ls_bin.exists() {
        tracing::error!("Binary not found: {}", ls_bin.display());
        return Ok(1);
    }
    let (backend, port) = match start_backend(&paths, &ls_bin, &updater, true, |_| {}).await {
        Ok(v) => v,
        Err((e, b)) => {
            tracing::error!("Startup failed: {e:#}");
            b.shutdown().await;
            return Ok(1);
        }
    };
    println!(
        "Local: {}\nLS log: {}\nMain log: {}",
        window::ls_url(port),
        paths.ls_log.display(),
        paths.main_log.display()
    );
    // quit-and-install requests a normal quit; teardown happens below.
    let quit = Arc::new(tokio::sync::Notify::new());
    {
        let q = quit.clone();
        updater.set_exit_request(Arc::new(move || q.notify_one()));
    }
    wire_auto_check(&updater, &settings);

    if let Some(ls) = backend.ls.clone() {
        tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Err(e) = ls.write_stdin_line(&line).await {
                    tracing::warn!("stdin forward failed: {e}");
                }
            }
        });
    }

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = quit.notified() => break,
            Some(argv) = second_rx.recv() => {
                // No window to route a deep link to: surface it on the console.
                match deeplink::find_in_argv(&argv, &scheme) {
                    Some(u) => {
                        tracing::info!("second instance forwarded deep link {u}");
                        println!("Received deep link (headless, not dispatched): {u}");
                    }
                    None => tracing::info!("second instance started while headless; ignored"),
                }
            }
        }
    }
    backend.shutdown().await;
    updater.run_pending_install();
    Ok(0)
}
