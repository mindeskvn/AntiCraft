//! AntiCraft native: desktop coding agent for Google Code Assist (egui + tokio).
//!
//! Protocol behavior is derived from google-gemini/gemini-cli (Apache-2.0); see NOTICE.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

// Backend modules keep API surface (quota, token counting, revoke) not yet used by the UI.
#[allow(dead_code, unused_imports)]
mod agent;
#[allow(dead_code, unused_imports)]
mod api;
mod app;
#[allow(dead_code, unused_imports)]
mod auth;
mod log;
#[allow(dead_code, unused_imports)]
mod providers;
mod settings;
#[allow(dead_code, unused_imports)]
mod store;
mod ui;

#[cfg(test)]
mod e2e_tests;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use tokio_util::sync::CancellationToken;

/// Parsed command-line flags.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Cli {
    pub data_dir: Option<PathBuf>,
    pub workspace: Option<PathBuf>,
    pub model: Option<String>,
    pub logout: bool,
    /// `--mode default|accept-edits|auto|plan`: execution mode for this run.
    pub mode: Option<ExecutionMode>,
    /// `--dangerously-skip-permissions`: auto-approve every tool call except denylisted commands.
    pub dangerously_skip_permissions: bool,
    /// `--sandbox`: run terminal commands in a sandbox (not available on this platform; stored
    /// so the UI can report why `auto` mode falls back).
    pub sandbox: bool,
    /// Offline demo screen (no backend); see `ui::demo::NAMES`. Alias: `--ui-fixture`.
    pub ui_fixture: Option<String>,
    /// With `--demo`: save a PNG screenshot here and exit. Alias: `--snapshot`.
    pub snapshot: Option<PathBuf>,
    /// With `--demo`: inner window size as `WxH` (default 1400x900).
    pub window_size: Option<(u32, u32)>,
}

/// Antigravity's public execution modes (`--mode`), mapped onto AntiCraft's policy knobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExecutionMode {
    /// Standard interactive behaviour: prompt for edits and commands.
    #[default]
    Default,
    /// Auto-approve workspace file edits.
    AcceptEdits,
    /// Auto-approve edits and sandbox-safe commands.
    Auto,
    /// Research and plan before writing any code.
    Plan,
}

impl ExecutionMode {
    fn parse(v: &str) -> Result<Self, String> {
        match v.trim().to_ascii_lowercase().as_str() {
            "default" => Ok(Self::Default),
            "accept-edits" | "accept_edits" => Ok(Self::AcceptEdits),
            "auto" => Ok(Self::Auto),
            "plan" | "planning" => Ok(Self::Plan),
            other => Err(format!("unknown --mode {other:?}, expected default|accept-edits|auto|plan")),
        }
    }

    /// Apply this mode to `settings` (the policy snapshot is rebuilt from them).
    pub fn apply(self, s: &mut crate::settings::Settings) {
        use crate::agent::policy::{AgentMode, TerminalPolicy};
        match self {
            // `default` keeps the user's own safety choices from settings.json.
            ExecutionMode::Default => {}
            ExecutionMode::AcceptEdits => {
                s.agent_mode = AgentMode::Fast;
                s.auto_approve_edits = true;
            }
            ExecutionMode::Auto => {
                s.agent_mode = AgentMode::Fast;
                s.auto_approve_edits = true;
                s.terminal_policy = TerminalPolicy::Auto;
                s.auto_approve_read_tools = true;
            }
            ExecutionMode::Plan => {
                s.agent_mode = AgentMode::Planning;
            }
        }
    }
}

/// Parse `WxH` into a window size (each side 200..=8192).
fn parse_size(v: &str) -> Result<(u32, u32), String> {
    let (w, h) = v.split_once(['x', 'X']).ok_or_else(|| format!("bad --window-size {v:?}, expected WxH"))?;
    let w: u32 = w.trim().parse().map_err(|_| format!("bad width in {v:?}"))?;
    let h: u32 = h.trim().parse().map_err(|_| format!("bad height in {v:?}"))?;
    if !(200..=8192).contains(&w) || !(200..=8192).contains(&h) {
        return Err(format!("--window-size {v:?} out of range"));
    }
    Ok((w, h))
}

const USAGE: &str = "Usage: anticraft-native [--data-dir DIR] [--workspace DIR] [--model ID] [--mode default|accept-edits|auto|plan] [--sandbox] [--dangerously-skip-permissions] [--logout] [--demo home|conversation|turn-start|turn-error|progress|planning|review|model-menu|history|customizations|login|accounts|accounts-models|providers|provider-add|provider-dialog|provider-menu|bypass|bypass-chat|thinking|confirm-delete [--screenshot OUT.png] [--window-size WxH]]\n  --logout deletes the Gemini CLI sign-in (oauth_creds.json). Use the Account tab to sign out of a subscription account.";

impl Cli {
    /// Parse `args` (without the program name).
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut cli = Cli::default();
        let mut it = args.into_iter();
        while let Some(a) = it.next() {
            let mut value = |flag: &str| it.next().ok_or_else(|| format!("{flag} needs a value\n{USAGE}"));
            match a.as_str() {
                "--data-dir" => cli.data_dir = Some(value("--data-dir")?.into()),
                "--workspace" => cli.workspace = Some(value("--workspace")?.into()),
                "--model" => cli.model = Some(value("--model")?),
                "--mode" => cli.mode = Some(ExecutionMode::parse(&value("--mode")?)?),
                "--sandbox" => cli.sandbox = true,
                "--dangerously-skip-permissions" => cli.dangerously_skip_permissions = true,
                "--logout" => cli.logout = true,
                "--demo" | "--ui-fixture" => cli.ui_fixture = Some(value("--demo")?),
                "--screenshot" | "--snapshot" => cli.snapshot = Some(value("--screenshot")?.into()),
                "--window-size" => cli.window_size = Some(parse_size(&value("--window-size")?)?),
                "-h" | "--help" => return Err(USAGE.into()),
                other => return Err(format!("unknown argument: {other}\n{USAGE}")),
            }
        }
        Ok(cli)
    }
}

fn main() -> anyhow::Result<()> {
    let cli = match Cli::parse(std::env::args().skip(1)) {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("{msg}");
            std::process::exit(2);
        }
    };
    if let Some(name) = cli.ui_fixture.clone() {
        return run_fixture(&name, cli.snapshot.clone(), cli.window_size);
    }
    let data_dir = cli.data_dir.clone().unwrap_or_else(settings::default_data_dir);
    auth::store::create_private_dir(&data_dir).with_context(|| format!("creating {}", data_dir.display()))?;
    let _log_guard = log::init(&data_dir).ok();
    tracing::info!("AntiCraft native {} starting", env!("CARGO_PKG_VERSION"));

    if cli.logout {
        // Only the Gemini CLI sign-in: subscription accounts are listed, and signed out of one by
        // one, on the Account tab.
        auth::CredentialStore::in_dir(&data_dir).delete()?;
        println!("Logged out. Use the Account tab to sign out of a subscription account.");
        return Ok(());
    }

    // One window per data folder: two would overwrite each other's settings.json. Held until
    // the process exits, after the backend's last save.
    let _data_lock = match settings::lock_data_dir(&data_dir) {
        Ok(lock) => Some(lock),
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
            tracing::warn!("data folder {} is in use by another instance", data_dir.display());
            rfd::MessageDialog::new()
                .set_level(rfd::MessageLevel::Warning)
                .set_title("AntiCraft is already running")
                .set_description(format!(
                    "Another AntiCraft window is already using the data folder {}.\n\nSwitch to that window, or start this one with --data-dir and another folder.",
                    data_dir.display()
                ))
                .set_buttons(rfd::MessageButtons::Ok)
                .show();
            anyhow::bail!("another AntiCraft window is using the data folder {}", data_dir.display());
        }
        Err(e) => {
            // A folder that cannot be locked at all must not stop the app.
            tracing::warn!("could not lock the data folder: {e}");
            None
        }
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("anticraft-rt")
        .build()?;
    let shutdown = CancellationToken::new();
    let opts = app::AppOptions {
        data_dir,
        workspace: cli.workspace,
        model: cli.model,
        mode: cli.mode,
        skip_permissions: cli.dangerously_skip_permissions,
    };

    let native = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_title("AntiCraft").with_inner_size([1400.0, 900.0]).with_min_inner_size([900.0, 560.0]),
        // Flat dark fills: dithering adds visible 1-level noise to the titlebar.
        dithering: false,
        ..Default::default()
    };
    let rt_handle = runtime.handle().clone();
    let sd = shutdown.clone();
    // The backend task's handle, so exit can wait for its queued commands (see below).
    let backend: Arc<std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>> = Arc::default();
    let slot = backend.clone();
    let result = eframe::run_native(
        "AntiCraft",
        native,
        Box::new(move |cc| {
            let ctx = cc.egui_ctx.clone();
            let repaint: Arc<dyn Fn() + Send + Sync> = Arc::new(move || ctx.request_repaint());
            let (channels, task) = app::spawn_backend(&rt_handle, opts, sd.clone(), repaint);
            if let Ok(mut s) = slot.lock() {
                *s = Some(task);
            }
            Ok(Box::new(ui::AntiCraftApp::new(cc, channels, sd)))
        }),
    );

    // Graceful shutdown. `on_exit` queued the last `SaveSettings` and `Shutdown` and cancelled
    // the token; cancelling here also stops the backend when `on_exit` never ran (its loop
    // drains queued commands before it looks at the token). Then let the backend finish that
    // queue and close MCP servers before the runtime stops: dropping it mid-command would lose
    // the save. The wait is capped so a stuck command cannot keep the process alive.
    shutdown.cancel();
    let task = backend.lock().ok().and_then(|mut s| s.take());
    if let Some(task) = task {
        // Build the timer inside `block_on`: created on this thread, outside the runtime,
        // `timeout` panics with "no reactor running".
        let waited = runtime.block_on(async move { tokio::time::timeout(std::time::Duration::from_secs(5), task).await });
        if waited.is_err() {
            tracing::warn!("backend still busy 5 s after the window closed; stopping it");
        }
    }
    runtime.shutdown_timeout(std::time::Duration::from_secs(2));
    result.map_err(|e| anyhow::anyhow!("UI error: {e}"))
}

/// Show a fixture UI state with detached channels (no backend, no network, no credentials).
fn run_fixture(name: &str, snapshot: Option<PathBuf>, size: Option<(u32, u32)>) -> anyhow::Result<()> {
    let (w, h) = size.unwrap_or((1400, 900));
    let Some(state) = ui::demo::state(name) else {
        anyhow::bail!("unknown demo {name:?}; choose one of {:?}", ui::demo::NAMES);
    };
    // Demo mode never touches the real data dir: no backend runs, and any eframe storage is
    // pointed at a throwaway temp folder.
    let tmp = std::env::temp_dir().join(format!("anticraft-demo-{}", uuid::Uuid::new_v4().simple()));
    let native = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_title("AntiCraft (demo)").with_inner_size([w as f32, h as f32]).with_resizable(false),
        persistence_path: Some(tmp.clone()),
        persist_window: false,
        dithering: false,
        ..Default::default()
    };
    let shutdown = CancellationToken::new();
    let res = eframe::run_native(
        "AntiCraft demo",
        native,
        Box::new(move |cc| {
            let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel();
            let (_ev_tx, ev_rx) = tokio::sync::mpsc::unbounded_channel();
            // Keep the receiver/sender halves alive for the app lifetime.
            std::mem::forget(_cmd_rx);
            std::mem::forget(_ev_tx);
            let channels = app::UiChannels { commands: cmd_tx, events: ev_rx };
            Ok(Box::new(ui::AntiCraftApp::with_state(cc, channels, shutdown, state, snapshot)))
        }),
    )
    .map_err(|e| anyhow::anyhow!("UI error: {e}"));
    let _ = std::fs::remove_dir_all(&tmp);
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_parsing() {
        let c = Cli::parse(["--workspace", "C:/w", "--model", "m", "--logout"].map(String::from)).unwrap();
        assert_eq!(c.workspace, Some(PathBuf::from("C:/w")));
        assert_eq!(c.model.as_deref(), Some("m"));
        assert!(c.logout);
        assert!(Cli::parse(["--bogus".to_string()]).is_err());
        assert!(Cli::parse(["--model".to_string()]).is_err());
        let d = Cli::parse(["--data-dir", "D:/data"].map(String::from)).unwrap();
        assert_eq!(d.data_dir, Some(PathBuf::from("D:/data")));
        assert!(!d.logout);
        assert_eq!(Cli::parse(Vec::<String>::new()).unwrap(), Cli::default());
        let f = Cli::parse(["--ui-fixture", "home", "--snapshot", "o.png"].map(String::from)).unwrap();
        assert_eq!(f.ui_fixture.as_deref(), Some("home"));
        assert_eq!(f.snapshot, Some(PathBuf::from("o.png")));
        let d = Cli::parse(["--demo", "model-menu", "--screenshot", "s.png"].map(String::from)).unwrap();
        assert_eq!(d.ui_fixture.as_deref(), Some("model-menu"));
        assert_eq!(d.snapshot, Some(PathBuf::from("s.png")));
        let z = Cli::parse(["--demo", "conversation", "--window-size", "1172x900"].map(String::from)).unwrap();
        assert_eq!(z.window_size, Some((1172, 900)));
        assert!(Cli::parse(["--window-size", "abc"].map(String::from)).is_err());
        assert!(Cli::parse(["--window-size", "10x10"].map(String::from)).is_err());
    }

    #[test]
    fn usage_lists_every_demo_screen() {
        let demos = USAGE.split("--demo ").nth(1).and_then(|s| s.split(' ').next()).unwrap();
        let listed: Vec<&str> = demos.split('|').collect();
        for name in ui::demo::NAMES.iter().chain(ui::demo::ORCHESTRATION_NAMES) {
            assert!(listed.contains(name), "--demo {name} is missing from USAGE");
        }
        assert!(USAGE.contains("Use the Account tab to sign out of a subscription account."));
    }

    #[test]
    fn cli_mode_and_permission_flags() {
        let c = Cli::parse(["--mode", "accept-edits", "--sandbox", "--dangerously-skip-permissions"].map(String::from)).unwrap();
        assert_eq!(c.mode, Some(ExecutionMode::AcceptEdits));
        assert!(c.sandbox);
        assert!(c.dangerously_skip_permissions);
        assert_eq!(Cli::parse(["--mode", "plan"].map(String::from)).unwrap().mode, Some(ExecutionMode::Plan));
        assert_eq!(Cli::parse(["--mode", "auto"].map(String::from)).unwrap().mode, Some(ExecutionMode::Auto));
        assert_eq!(Cli::parse(["--mode", "default"].map(String::from)).unwrap().mode, Some(ExecutionMode::Default));
        assert!(Cli::parse(["--mode", "yolo"].map(String::from)).is_err(), "an unknown mode is an error, not a silent default");
        assert!(Cli::parse(["--mode"].map(String::from)).is_err());
    }

    #[test]
    fn modes_map_onto_policy_settings() {
        use crate::agent::policy::{AgentMode, TerminalPolicy};
        let mut s = crate::settings::Settings::default();
        // `default` leaves the user's own safety choices alone.
        let before = s.clone();
        ExecutionMode::Default.apply(&mut s);
        assert_eq!(s, before);

        // accept-edits: edits without prompts, commands unchanged.
        let mut s = crate::settings::Settings { terminal_policy: TerminalPolicy::Off, ..Default::default() };
        ExecutionMode::AcceptEdits.apply(&mut s);
        assert!(s.auto_approve_edits);
        assert_eq!(s.terminal_policy, TerminalPolicy::Off);

        // auto: the most permissive interactive mode.
        let mut s = crate::settings::Settings::default();
        ExecutionMode::Auto.apply(&mut s);
        assert!(s.auto_approve_edits && s.auto_approve_read_tools);
        assert_eq!(s.terminal_policy, TerminalPolicy::Auto);
        assert_eq!(s.agent_mode, AgentMode::Fast);

        // plan: planning mode, nothing auto-approved.
        let mut s = crate::settings::Settings::default();
        ExecutionMode::Plan.apply(&mut s);
        assert_eq!(s.agent_mode, AgentMode::Planning);
        assert!(!s.auto_approve_edits);
    }
}
