//! Command-line interface.

use clap::Parser;
use std::path::PathBuf;

/// AntiCraft: a Rust desktop shell for the Antigravity language server.
#[derive(Debug, Clone, Parser, Default)]
#[command(name = "anticraft", version, about)]
pub struct Cli {
    /// userData directory (default %APPDATA%\AntiCraft; env ANTICRAFT_USER_DATA).
    #[arg(long = "user-data-dir", value_name = "DIR")]
    pub user_data_dir: Option<PathBuf>,

    /// language_server.exe path (else CODEIUM_LANGUAGE_SERVER_BIN, else the installed app).
    #[arg(long = "ls-path", value_name = "PATH")]
    pub ls_path: Option<PathBuf>,

    /// WSL distro. Accepted for compatibility; WSL is unsupported and runs locally.
    #[arg(long = "wsl-distro", value_name = "NAME", num_args = 0..=1, default_missing_value = "")]
    pub wsl_distro: Option<String>,

    /// No UI: forward stdin lines to the LS (OAuth code paste).
    /// Also enabled by ELECTRON_OZONE_PLATFORM_HINT=headless.
    #[arg(long)]
    pub headless: bool,

    /// Do not register the antigravity:// protocol at startup.
    #[arg(long = "no-register-protocol")]
    pub no_register_protocol: bool,

    /// Remove the antigravity:// registration if it points at this exe, then exit.
    #[arg(long = "unregister-protocol")]
    pub unregister_protocol: bool,

    /// Skip the LS and load this URL (also env DEV_URL).
    #[arg(long = "dev-url", value_name = "URL")]
    pub dev_url: Option<String>,

    /// WebView2 CDP port (§14). Absent means `0` (random port, written to
    /// DevToolsActivePort).
    #[arg(long = "remote-debugging-port", value_name = "PORT")]
    pub remote_debugging_port: Option<u16>,

    /// Positional args (deep links such as antigravity://...).
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub rest: Vec<String>,
}

/// Long switches this CLI understands; `true` = takes a value (may be given
/// as `--x=v` or `--x v`).
const KNOWN_SWITCHES: &[(&str, bool)] = &[
    ("--user-data-dir", true),
    ("--ls-path", true),
    ("--wsl-distro", false), // optional value; only `--wsl-distro=v` or the next non-switch arg
    ("--headless", false),
    ("--no-register-protocol", false),
    ("--unregister-protocol", false),
    ("--dev-url", true),
    ("--remote-debugging-port", true),
    ("--help", false),
    ("-h", false),
    ("--version", false),
    ("-V", false),
];

/// Drop switches clap does not know (Chromium/Electron-style flags that the
/// original shell passed through), so they never abort startup. Positional
/// arguments such as deep links are kept. Returns `(kept, dropped)`.
pub fn filter_unknown_switches(argv: Vec<String>) -> (Vec<String>, Vec<String>) {
    let mut kept = Vec::with_capacity(argv.len());
    let mut dropped = Vec::new();
    let mut it = argv.into_iter();
    if let Some(program) = it.next() {
        kept.push(program);
    }
    let mut take_value = false;
    let mut after_separator = false;
    for a in it {
        if take_value || after_separator || !a.starts_with('-') || a == "-" {
            take_value = false;
            kept.push(a);
            continue;
        }
        if a == "--" {
            after_separator = true;
            kept.push(a);
            continue;
        }
        let name = a.split_once('=').map_or(a.as_str(), |(n, _)| n);
        match KNOWN_SWITCHES.iter().find(|(n, _)| *n == name) {
            Some((_, needs_value)) => {
                take_value = *needs_value && !a.contains('=');
                kept.push(a);
            }
            None => dropped.push(a),
        }
    }
    (kept, dropped)
}

impl Cli {
    /// Parse the process argv, ignoring unknown switches (logged later).
    pub fn parse_lenient() -> (Self, Vec<String>) {
        let (kept, dropped) = filter_unknown_switches(std::env::args().collect());
        (Self::parse_from(kept), dropped)
    }

    /// Headless from flag or `ELECTRON_OZONE_PLATFORM_HINT=headless`.
    pub fn is_headless(&self) -> bool {
        headless_from(self.headless, std::env::var("ELECTRON_OZONE_PLATFORM_HINT").ok().as_deref())
    }

    /// `--dev-url`, then env `DEV_URL` (non-empty).
    pub fn effective_dev_url(&self) -> Option<String> {
        dev_url_from(self.dev_url.as_deref(), std::env::var("DEV_URL").ok().as_deref())
    }

    /// WSL distro requested via flag or `ANTIGRAVITY_WSL_DISTRO` (empty flag forces local).
    #[allow(dead_code)]
    pub fn requested_wsl_distro(&self) -> Option<String> {
        wsl_distro_from(self.wsl_distro.as_deref(), std::env::var("ANTIGRAVITY_WSL_DISTRO").ok().as_deref(), None)
    }

    /// Full §2 step 4 chain: flag, env, then the `distro` field of `wsl_state.json`.
    pub fn resolved_wsl_distro(&self, wsl_state: &std::path::Path) -> Option<String> {
        let state = std::fs::read_to_string(wsl_state).ok();
        wsl_distro_from(
            self.wsl_distro.as_deref(),
            std::env::var("ANTIGRAVITY_WSL_DISTRO").ok().as_deref(),
            state.as_deref(),
        )
    }
}

/// Headless iff the flag is set or the ozone hint equals `headless`.
pub fn headless_from(flag: bool, ozone_hint: Option<&str>) -> bool {
    flag || ozone_hint == Some("headless")
}

/// The CLI value wins over the env value; empty strings count as unset.
pub fn dev_url_from(flag: Option<&str>, env: Option<&str>) -> Option<String> {
    flag.or(env).filter(|s| !s.is_empty()).map(str::to_owned)
}

/// §2 step 4 precedence. A present-but-empty switch forces local; an empty env
/// value falls through; a malformed state file or non-string `distro` is empty.
pub fn wsl_distro_from(flag: Option<&str>, env: Option<&str>, state_json: Option<&str>) -> Option<String> {
    if let Some(f) = flag {
        return Some(f.to_owned()).filter(|d| !d.is_empty());
    }
    if let Some(e) = env.filter(|e| !e.is_empty()) {
        return Some(e.to_owned());
    }
    state_json
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
        .and_then(|v| v.get("distro").and_then(|d| d.as_str()).map(str::to_owned))
        .filter(|d| !d.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse() {
        let c = Cli::parse_from(["anticraft", "--user-data-dir=C:\\x", "--wsl-distro=", "--headless", "antigravity://a"]);
        assert_eq!(c.user_data_dir, Some(PathBuf::from("C:\\x")));
        assert_eq!(c.wsl_distro.as_deref(), Some(""));
        assert_eq!(c.requested_wsl_distro(), None);
        assert!(c.headless);
        assert_eq!(c.rest, vec!["antigravity://a".to_string()]);
        let c = Cli::parse_from(["anticraft", "--unregister-protocol"]);
        assert!(c.unregister_protocol && !c.no_register_protocol);
    }

    #[test]
    fn all_flags() {
        let c = Cli::parse_from([
            "anticraft", "--ls-path", "C:/ls.exe", "--wsl-distro", "Ubuntu", "--no-register-protocol",
            "--dev-url", "http://localhost:5173",
        ]);
        assert_eq!(c.ls_path, Some(PathBuf::from("C:/ls.exe")));
        assert_eq!(c.wsl_distro.as_deref(), Some("Ubuntu"));
        assert!(c.no_register_protocol && !c.headless && !c.unregister_protocol);
        assert_eq!(c.dev_url.as_deref(), Some("http://localhost:5173"));
        assert!(c.rest.is_empty());
    }

    #[test]
    fn unknown_switches_and_debug_port() {
        let argv: Vec<String> = [
            "anticraft", "--remote-debugging-port=9222", "--disable-gpu", "--ls-path", "C:/ls.exe",
            "--enable-features=X", "antigravity://cb?code=1",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let (kept, dropped) = filter_unknown_switches(argv);
        assert_eq!(dropped, vec!["--disable-gpu".to_string(), "--enable-features=X".to_string()]);
        let c = Cli::parse_from(kept);
        assert_eq!(c.remote_debugging_port, Some(9222));
        assert_eq!(c.ls_path, Some(PathBuf::from("C:/ls.exe")));
        assert_eq!(c.rest, vec!["antigravity://cb?code=1".to_string()]);
        let c = Cli::parse_from(filter_unknown_switches(vec!["a".into(), "--remote-debugging-port".into(), "1".into()]).0);
        assert_eq!(c.remote_debugging_port, Some(1));
    }

    #[test]
    fn headless_rules() {
        assert!(headless_from(true, None));
        assert!(headless_from(false, Some("headless")));
        assert!(!headless_from(false, Some("wayland")));
        assert!(!headless_from(false, None));
    }

    #[test]
    fn dev_url_rules() {
        assert_eq!(dev_url_from(Some("a"), Some("b")).as_deref(), Some("a"));
        assert_eq!(dev_url_from(None, Some("b")).as_deref(), Some("b"));
        assert_eq!(dev_url_from(None, Some("")), None);
        assert_eq!(dev_url_from(Some(""), Some("b")), None);
        assert_eq!(dev_url_from(None, None), None);
    }

    #[test]
    fn wsl_precedence() {
        let st = Some(r#"{"distro":"Debian"}"#);
        assert_eq!(wsl_distro_from(Some("U"), Some("E"), st).as_deref(), Some("U"));
        assert_eq!(wsl_distro_from(Some(""), Some("E"), st), None);
        assert_eq!(wsl_distro_from(None, Some("E"), st).as_deref(), Some("E"));
        assert_eq!(wsl_distro_from(None, Some(""), st).as_deref(), Some("Debian"));
        assert_eq!(wsl_distro_from(None, None, Some("{bad")), None);
        assert_eq!(wsl_distro_from(None, None, Some(r#"{"distro":5}"#)), None);
        assert_eq!(wsl_distro_from(None, None, None), None);
    }
}
