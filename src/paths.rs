//! Filesystem layout (SPEC §1).
//!
//! AntiCraft keeps its own userData (`%APPDATA%\AntiCraft`) so it never
//! collides with the original shell's Chromium profile, but it uses the
//! *installed* Antigravity for the LS binary, the node wrapper target and the
//! unpacked chrome-devtools-mcp script.

use std::path::{Path, PathBuf};

/// Product name used for titles and tray labels.
pub const APP_NAME: &str = "Antigravity";
/// Name passed to the LS as `--app_data_dir` (a directory NAME, not a path).
pub const APP_DATA_DIR_NAME: &str = "antigravity";
/// Version reported to the LS and the update feed (the installed Antigravity version).
pub const APP_VERSION: &str = "2.26.0";
/// Env var overriding the userData directory.
pub const ENV_USER_DATA: &str = "ANTICRAFT_USER_DATA";

/// All resolved paths used by the shell. Construct once at startup and share.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    /// `<userData>`; default `%APPDATA%\AntiCraft`.
    pub user_data: PathBuf,
    /// `<userData>\logs`.
    pub logs: PathBuf,
    /// `<userData>\app_storage.json` (§6.1).
    pub app_storage: PathBuf,
    /// `<userData>\wsl_state.json` (§6.2). Kept for parity; WSL is unsupported.
    pub wsl_state: PathBuf,
    /// `<userData>\WebView2\EBWebView\DevToolsActivePort`: the WebView2 browser
    /// process uses `EBWebView` inside the WebView2 data folder as its Chromium
    /// user-data dir and writes the file there. Exported as
    /// `AGY_BROWSER_ACTIVE_PORT_FILE`.
    pub devtools_active_port: PathBuf,
    /// `<userData>\bin`, prefixed to the LS PATH; holds `agy-node.cmd`.
    pub node_wrapper_dir: PathBuf,
    /// `<userData>\WebView2`, the WebView2 user data folder.
    pub webview2_data: PathBuf,
    /// `<logs>\language_server.log`.
    pub ls_log: PathBuf,
    /// `<logs>\main.log`.
    pub main_log: PathBuf,
    /// User home directory.
    pub home: PathBuf,
    /// `~/.gemini/config/config.json` (theme, read-only).
    pub gemini_config: PathBuf,
    /// Installed Antigravity root, `%LOCALAPPDATA%\Programs\Antigravity`.
    pub installed_root: PathBuf,
}

impl Paths {
    /// Resolve paths. Precedence for userData: `cli_user_data` (`--user-data-dir`),
    /// then env `ANTICRAFT_USER_DATA`, then `%APPDATA%\AntiCraft`.
    pub fn resolve(cli_user_data: Option<&Path>) -> anyhow::Result<Self> {
        let user_data = match cli_user_data {
            Some(p) => p.to_path_buf(),
            None => match env_dir(ENV_USER_DATA) {
                Some(v) => v,
                None => env_dir("APPDATA")
                    .ok_or_else(|| anyhow::anyhow!("APPDATA is not set"))?
                    .join("AntiCraft"),
            },
        };
        let home = env_dir("USERPROFILE")
            .or_else(|| env_dir("HOME"))
            .ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?;
        let installed_root = env_dir("LOCALAPPDATA")
            .unwrap_or_else(|| home.join("AppData").join("Local"))
            .join("Programs")
            .join("Antigravity");
        Ok(Self::from_parts(user_data, home, installed_root))
    }

    /// Build every derived path from the three roots (used by tests).
    pub fn from_parts(user_data: PathBuf, home: PathBuf, installed_root: PathBuf) -> Self {
        let logs = user_data.join("logs");
        Self {
            app_storage: user_data.join("app_storage.json"),
            wsl_state: user_data.join("wsl_state.json"),
            devtools_active_port: user_data.join("WebView2").join("EBWebView").join("DevToolsActivePort"),
            node_wrapper_dir: user_data.join("bin"),
            webview2_data: user_data.join("WebView2"),
            ls_log: logs.join("language_server.log"),
            main_log: logs.join("main.log"),
            gemini_config: home.join(".gemini").join("config").join("config.json"),
            logs,
            user_data,
            home,
            installed_root,
        }
    }

    /// `<installed_root>\resources`.
    pub fn installed_resources(&self) -> PathBuf {
        self.installed_root.join("resources")
    }

    /// `<installed_root>\Antigravity.exe`, target of the `agy-node.cmd` wrapper.
    pub fn installed_electron_exe(&self) -> PathBuf {
        self.installed_root.join("Antigravity.exe")
    }

    /// `%LOCALAPPDATA%\Programs\Antigravity IDE\Antigravity IDE.exe` (`ide:is-installed`).
    pub fn ide_exe(&self) -> PathBuf {
        self.installed_root
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default()
            .join("Antigravity IDE")
            .join("Antigravity IDE.exe")
    }

    /// Value for `CHROME_DEVTOOLS_MCP_JS` (§3.2) inside the installed app.asar.unpacked.
    pub fn chrome_devtools_mcp_js(&self) -> PathBuf {
        self.installed_resources()
            .join("app.asar.unpacked")
            .join("node_modules")
            .join("chrome-devtools-mcp")
            .join("build")
            .join("src")
            .join("bin")
            .join("chrome-devtools-mcp.js")
    }

    /// Create userData, logs and bin directories.
    pub fn ensure_dirs(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.user_data)?;
        std::fs::create_dir_all(&self.logs)?;
        std::fs::create_dir_all(&self.node_wrapper_dir)?;
        Ok(())
    }
}

fn env_dir(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_paths() {
        let p = Paths::from_parts(r"C:\ud".into(), r"C:\home".into(), r"C:\P\Antigravity".into());
        assert_eq!(p.ls_log, PathBuf::from(r"C:\ud\logs\language_server.log"));
        assert_eq!(p.app_storage, PathBuf::from(r"C:\ud\app_storage.json"));
        assert_eq!(p.devtools_active_port, PathBuf::from(r"C:\ud\WebView2\EBWebView\DevToolsActivePort"));
        assert_eq!(p.ide_exe(), PathBuf::from(r"C:\P\Antigravity IDE\Antigravity IDE.exe"));
        assert!(p.chrome_devtools_mcp_js().ends_with("chrome-devtools-mcp.js"));
    }

    #[test]
    fn cli_override_wins() {
        let dir = tempfile::tempdir().unwrap();
        let p = Paths::resolve(Some(dir.path())).unwrap();
        assert_eq!(p.user_data, dir.path());
    }

    #[test]
    fn ensure_dirs_creates_layout() {
        let d = tempfile::tempdir().unwrap();
        let p = Paths::from_parts(d.path().join("ud"), d.path().into(), d.path().join("inst"));
        p.ensure_dirs().unwrap();
        assert!(p.logs.is_dir());
        assert!(p.node_wrapper_dir.is_dir());
        assert_eq!(p.main_log, d.path().join("ud").join("logs").join("main.log"));
        assert_eq!(p.gemini_config, d.path().join(".gemini").join("config").join("config.json"));
        assert_eq!(p.installed_electron_exe(), d.path().join("inst").join("Antigravity.exe"));
    }
}
