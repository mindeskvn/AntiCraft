//! Auto-updater for the Windows generic provider (SPEC §13).
//!
//! Feed: `<feed>latest-x64-win.yml` with header `x-app-version`. The installer
//! is downloaded into `%LOCALAPPDATA%\anticraft-updater`, verified against the
//! base64 SHA-512 in the yml, and run after the normal quit teardown.

use crate::host_bridge::UpdateStatusProvider;
use crate::proto;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha512};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const DEFAULT_FEED_URL: &str =
    "https://antigravity-hub-auto-updater-974169037036.us-central1.run.app/manifest/";
pub const CHANNEL_FILE: &str = "latest-x64-win.yml";
pub const INITIAL_CHECK_DELAY: Duration = Duration::from_secs(10);
pub const CHECK_INTERVAL: Duration = Duration::from_millis(3_600_000);

/// Wire state type strings (§5.1 UpdateStateObj.type).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UpdateStateType {
    Idle,
    Disabled,
    Checking,
    AvailableForDownload,
    Downloading,
    Downloaded,
    Updating,
    Ready,
}

impl UpdateStateType {
    pub fn wire(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Disabled => "disabled",
            Self::Checking => "checking for updates",
            Self::AvailableForDownload => "available for download",
            Self::Downloading => "downloading",
            Self::Downloaded => "downloaded",
            Self::Updating => "updating",
            Self::Ready => "ready",
        }
    }
    /// Unknown strings map to idle (renderer behavior).
    pub fn from_wire(s: &str) -> Self {
        match s {
            "disabled" => Self::Disabled,
            "checking for updates" => Self::Checking,
            "available for download" => Self::AvailableForDownload,
            "downloading" => Self::Downloading,
            "downloaded" => Self::Downloaded,
            "updating" => Self::Updating,
            "ready" => Self::Ready,
            _ => Self::Idle,
        }
    }
}

/// `{type, update?:{version}}` as broadcast on `updater:state-changed`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateStateObj {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub update: Option<UpdateInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateInfo {
    pub version: String,
}

impl UpdateStateObj {
    pub fn new(kind: UpdateStateType, version: Option<String>) -> Self {
        Self { kind: kind.wire().to_string(), update: version.map(|version| UpdateInfo { version }) }
    }
    pub fn idle() -> Self {
        Self::new(UpdateStateType::Idle, None)
    }
    pub fn state_type(&self) -> UpdateStateType {
        UpdateStateType::from_wire(&self.kind)
    }
    pub fn version(&self) -> Option<&str> {
        self.update.as_ref().map(|u| u.version.as_str())
    }
}

/// Menu step label (§8.1 "Check for Updates" item, macOS only in the original;
/// exposed for the Windows tray/menu if desired).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum MenuStep {
    Check,
    Checking,
    Downloading,
    RestartToUpdate,
}

#[allow(dead_code)]
impl MenuStep {
    pub fn label(self) -> &'static str {
        match self {
            Self::Check => "Check for Updates",
            Self::Checking => "Checking for Updates...",
            Self::Downloading => "Downloading Update...",
            Self::RestartToUpdate => "Restart to Update",
        }
    }
    pub fn from_state(t: UpdateStateType) -> Self {
        match t {
            UpdateStateType::Checking => Self::Checking,
            UpdateStateType::AvailableForDownload | UpdateStateType::Downloading => Self::Downloading,
            UpdateStateType::Ready => Self::RestartToUpdate,
            _ => Self::Check,
        }
    }
}

/// Parsed `latest-x64-win.yml` (fields used by the shell).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct UpdateManifest {
    pub version: String,
    pub path: String,
    pub sha512: String,
}

#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("feed returned HTTP {0}")]
    Status(u16),
    #[error("invalid update manifest: {0}")]
    Manifest(String),
    #[error("invalid version: {0}")]
    Version(#[from] semver::Error),
    #[error("sha512 mismatch")]
    Checksum,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("installer exceeds the size limit")]
    TooLarge,
    #[error("no downloaded update")]
    NothingToInstall,
}

/// Parse the channel yml. Accepts top-level `path`/`sha512`, falling back to
/// the first `files[]` entry (`url`/`sha512`) as electron-builder writes both.
pub fn parse_manifest(yml: &str) -> Result<UpdateManifest, UpdateError> {
    let v = yaml_value(yml)?;
    let s = |val: &serde_yaml::Value, k: &str| val.get(k).and_then(|x| x.as_str()).map(str::to_owned);
    let version = version_of(&v)?;
    let first = v.get("files").and_then(|f| f.as_sequence()).and_then(|f| f.first()).cloned();
    let path = s(&v, "path")
        .or_else(|| first.as_ref().and_then(|f| s(f, "url")))
        .ok_or_else(|| UpdateError::Manifest("missing path".into()))?;
    let sha512 = s(&v, "sha512")
        .or_else(|| first.as_ref().and_then(|f| s(f, "sha512")))
        .ok_or_else(|| UpdateError::Manifest("missing sha512".into()))?;
    Ok(UpdateManifest { version, path, sha512 })
}

/// Only the `version` field of the channel yml.
pub fn manifest_version(yml: &str) -> Result<String, UpdateError> {
    version_of(&yaml_value(yml)?)
}

fn yaml_value(yml: &str) -> Result<serde_yaml::Value, UpdateError> {
    serde_yaml::from_str(yml).map_err(|e| UpdateError::Manifest(e.to_string()))
}

fn version_of(v: &serde_yaml::Value) -> Result<String, UpdateError> {
    match v.get("version") {
        Some(serde_yaml::Value::String(s)) => Ok(s.clone()),
        Some(serde_yaml::Value::Number(n)) => Ok(n.to_string()),
        _ => Err(UpdateError::Manifest("missing version".into())),
    }
}

/// `true` iff `latest` > `current` (semver; a leading `v` is tolerated).
pub fn is_newer(current: &str, latest: &str) -> Result<bool, UpdateError> {
    let p = |s: &str| semver::Version::parse(s.trim().trim_start_matches('v'));
    Ok(p(latest)? > p(current)?)
}

/// Verify `data` against a base64 SHA-512 digest.
#[cfg(test)]
pub fn verify_sha512(data: &[u8], expected_b64: &str) -> bool {
    let digest = Sha512::digest(data);
    let ours = base64::engine::general_purpose::STANDARD.encode(digest);
    ours == expected_b64.trim()
}

/// Updater configuration.
#[derive(Debug, Clone)]
pub struct UpdaterConfig {
    /// Feed base URL ending with `/`.
    pub feed_url: String,
    /// Version compared against the feed (default `2.26.0`).
    pub current_version: String,
    /// Download dir, default `%LOCALAPPDATA%\anticraft-updater`.
    pub cache_dir: PathBuf,
    /// Headless mode: suppress dialogs (and the ready broadcast, §13).
    pub headless: bool,
}

impl UpdaterConfig {
    pub fn default_cache_dir() -> PathBuf {
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join("anticraft-updater")
    }
}

impl Default for UpdaterConfig {
    fn default() -> Self {
        Self {
            feed_url: DEFAULT_FEED_URL.into(),
            current_version: crate::paths::APP_VERSION.into(),
            cache_dir: Self::default_cache_dir(),
            headless: false,
        }
    }
}

/// Subscriber for state broadcasts.
pub type StateListener = Arc<dyn Fn(&UpdateStateObj) + Send + Sync>;
/// Called after a manual check finds no update ("No updates available" dialog).
pub type UpToDateCallback = Arc<dyn Fn() + Send + Sync>;
/// Called by `quit_and_install`; must start the normal quit/teardown. The
/// installer itself runs from [`Updater::run_pending_install`] once teardown
/// has finished.
pub type ExitRequest = Arc<dyn Fn() + Send + Sync>;

/// Upper bound for a downloaded installer.
pub const MAX_INSTALLER_BYTES: u64 = 1024 * 1024 * 1024;

/// A verified installer on disk plus the digest it must still match.
#[derive(Debug, Clone)]
struct DownloadedInstaller {
    file: PathBuf,
    sha512_b64: String,
}

struct Inner {
    state: UpdateStateObj,
    downloaded: Option<DownloadedInstaller>,
    /// Set by `quit_and_install`; consumed after shutdown.
    install_on_exit: bool,
    listeners: Vec<StateListener>,
    schedule: Option<tokio::task::JoinHandle<()>>,
    initialized: bool,
}

/// Updater state machine. Cheap to clone (`Arc` inside).
#[derive(Clone)]
pub struct Updater {
    cfg: Arc<UpdaterConfig>,
    http: reqwest::Client,
    inner: Arc<Mutex<Inner>>,
    /// True while a check/download runs; extra checks are skipped.
    busy: Arc<AtomicBool>,
    up_to_date: Arc<Mutex<Option<UpToDateCallback>>>,
    exit: Arc<Mutex<Option<ExitRequest>>>,
    rt: tokio::runtime::Handle,
}

/// Clears the busy flag when a check ends (any path).
struct BusyGuard(Arc<AtomicBool>);
impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// What `ApplyUpdate` (§7.2) should do for a given last state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyAction {
    Install,
    AlreadyInProgress,
    StartCheck,
}

pub fn apply_action(t: UpdateStateType) -> ApplyAction {
    use UpdateStateType as T;
    if t == T::Ready {
        ApplyAction::Install
    } else if [T::AvailableForDownload, T::Downloading, T::Checking].contains(&t) {
        ApplyAction::AlreadyInProgress
    } else {
        ApplyAction::StartCheck
    }
}

const RESERVED_NAME_CHARS: &str = "<>:\"/\\|?*";

/// Windows-safe file name from the last path segment of the installer URL
/// (the query string is never part of it).
pub fn installer_file_name(u: &url::Url) -> String {
    let segment = u.path_segments().and_then(|mut s| s.next_back()).unwrap_or_default();
    let clean: String = segment
        .chars()
        .map(|c| if c.is_control() || RESERVED_NAME_CHARS.contains(c) { '_' } else { c })
        .collect();
    let clean = clean.trim_matches(|c| c == ' ' || c == '.').to_string();
    if clean.is_empty() {
        "installer.exe".into()
    } else {
        clean
    }
}

/// Base64 SHA-512 of a file on disk.
fn sha512_file(path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha512::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(base64::engine::general_purpose::STANDARD.encode(h.finalize()))
}

/// Authenticode check (WinVerifyTrust, generic verify v2, no UI).
#[cfg(windows)]
fn authenticode_valid(path: &Path) -> bool {
    use windows_sys::Win32::Security::WinTrust::*;
    let wide: Vec<u16> = path.as_os_str().to_string_lossy().encode_utf16().chain(Some(0)).collect();
    unsafe {
        let mut file = WINTRUST_FILE_INFO {
            cbStruct: std::mem::size_of::<WINTRUST_FILE_INFO>() as u32,
            pcwszFilePath: wide.as_ptr(),
            ..Default::default()
        };
        let mut data = WINTRUST_DATA {
            cbStruct: std::mem::size_of::<WINTRUST_DATA>() as u32,
            dwUIChoice: WTD_UI_NONE,
            fdwRevocationChecks: WTD_REVOKE_NONE,
            dwUnionChoice: WTD_CHOICE_FILE,
            dwStateAction: WTD_STATEACTION_VERIFY,
            ..Default::default()
        };
        data.Anonymous.pFile = &mut file;
        let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;
        let r = WinVerifyTrust(std::ptr::null_mut(), &mut action, &mut data as *mut _ as *mut core::ffi::c_void);
        data.dwStateAction = WTD_STATEACTION_CLOSE;
        WinVerifyTrust(std::ptr::null_mut(), &mut action, &mut data as *mut _ as *mut core::ffi::c_void);
        r == 0
    }
}

#[cfg(not(windows))]
fn authenticode_valid(_path: &Path) -> bool {
    true
}

impl Updater {
    /// Create in state `idle`. `rt` is used for background checks.
    pub fn new(cfg: UpdaterConfig, rt: tokio::runtime::Handle) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .read_timeout(Duration::from_secs(60))
            .build()
            .unwrap_or_default();
        Self {
            cfg: Arc::new(cfg),
            http,
            inner: Arc::new(Mutex::new(Inner {
                state: UpdateStateObj::idle(),
                downloaded: None,
                install_on_exit: false,
                listeners: Vec::new(),
                schedule: None,
                initialized: false,
            })),
            busy: Arc::new(AtomicBool::new(false)),
            up_to_date: Arc::new(Mutex::new(None)),
            exit: Arc::new(Mutex::new(None)),
            rt,
        }
    }

    #[allow(dead_code)]
    pub fn config(&self) -> &UpdaterConfig {
        &self.cfg
    }

    /// Last broadcast state (`updater:get-state`).
    pub fn state(&self) -> UpdateStateObj {
        self.inner.lock().unwrap().state.clone()
    }

    pub fn subscribe(&self, l: StateListener) {
        self.inner.lock().unwrap().listeners.push(l);
    }

    pub fn set_up_to_date_callback(&self, cb: UpToDateCallback) {
        *self.up_to_date.lock().unwrap() = Some(cb);
    }

    pub fn set_exit_request(&self, cb: ExitRequest) {
        *self.exit.lock().unwrap() = Some(cb);
    }

    /// Set and broadcast a state.
    pub fn set_state(&self, s: UpdateStateObj) {
        let ls = {
            let mut i = self.inner.lock().unwrap();
            i.state = s.clone();
            i.listeners.clone()
        };
        for l in ls {
            l(&s);
        }
    }

    /// `updater:apply`: broadcasts exactly `{type:'ready'}`, does not install.
    pub fn apply(&self) {
        self.set_state(UpdateStateObj::new(UpdateStateType::Ready, None));
    }

    /// Init scheduling: after 10 s, if `auto_enabled()`, check now and hourly.
    pub fn init(&self, auto_enabled: impl Fn() -> bool + Send + 'static) {
        self.inner.lock().unwrap().initialized = true;
        let me = self.clone();
        self.rt.spawn(async move {
            tokio::time::sleep(INITIAL_CHECK_DELAY).await;
            if auto_enabled() {
                me.set_auto_check(true);
            }
        });
    }

    /// React to `autoCheckForUpdates` toggles (`None` = default `true`).
    pub fn set_auto_check(&self, enabled: bool) {
        let mut i = self.inner.lock().unwrap();
        if !i.initialized {
            return;
        }
        if enabled {
            if i.schedule.is_some() {
                return;
            }
            let me = self.clone();
            i.schedule = Some(self.rt.spawn(async move {
                loop {
                    me.check_logged(false).await;
                    tokio::time::sleep(CHECK_INTERVAL).await;
                }
            }));
        } else if let Some(h) = i.schedule.take() {
            h.abort();
        }
    }

    /// Spawn a check in the background (`manual` shows the up-to-date callback).
    pub fn spawn_check(&self, manual: bool) {
        let me = self.clone();
        self.rt.spawn(async move { me.check_logged(manual).await });
    }

    async fn check_logged(&self, manual: bool) {
        if let Err(e) = self.check(manual).await {
            tracing::warn!("update check failed: {e}");
            self.set_state(UpdateStateObj::idle());
        }
    }

    /// Full check + auto-download. Returns the manifest if an update was
    /// downloaded; `Ok(None)` when up to date or another check is running.
    pub async fn check(&self, manual: bool) -> Result<Option<UpdateManifest>, UpdateError> {
        if self.busy.swap(true, Ordering::SeqCst) {
            tracing::info!("update check already in progress; skipped");
            return Ok(None);
        }
        let _busy = BusyGuard(self.busy.clone());
        self.set_state(UpdateStateObj::new(UpdateStateType::Checking, None));
        let url = format!("{}{}", self.cfg.feed_url, CHANNEL_FILE);
        let resp = self
            .http
            .get(&url)
            .header("x-app-version", &self.cfg.current_version)
            .timeout(Duration::from_secs(60))
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(UpdateError::Status(resp.status().as_u16()));
        }
        let yml = resp.text().await?;
        // An up-to-date feed may carry only `version`; compare before requiring
        // the download fields.
        if !is_newer(&self.cfg.current_version, &manifest_version(&yml)?)? {
            self.set_state(UpdateStateObj::idle());
            if manual && !self.cfg.headless {
                if let Some(cb) = self.up_to_date.lock().unwrap().clone() {
                    cb();
                }
            }
            return Ok(None);
        }
        let manifest = parse_manifest(&yml)?;
        self.set_state(UpdateStateObj::new(UpdateStateType::AvailableForDownload, Some(manifest.version.clone())));
        let installer = self.download(&manifest).await?;
        self.inner.lock().unwrap().downloaded = Some(installer);
        if !self.cfg.headless {
            self.set_state(UpdateStateObj::new(UpdateStateType::Ready, Some(manifest.version.clone())));
        } else {
            // §13 headless: no ready broadcast; install via the normal quit.
            let _ = self.quit_and_install();
        }
        Ok(Some(manifest))
    }

    /// Stream the installer to `<name>.partial`, hashing as it goes, then
    /// rename into place once the SHA-512 matches.
    async fn download(&self, m: &UpdateManifest) -> Result<DownloadedInstaller, UpdateError> {
        use tokio::io::AsyncWriteExt;
        let url = url::Url::parse(&self.cfg.feed_url)
            .and_then(|b| b.join(&m.path))
            .map_err(|e| UpdateError::Manifest(e.to_string()))?;
        let name = installer_file_name(&url);
        let mut resp = self.http.get(url).header("x-app-version", &self.cfg.current_version).send().await?;
        if !resp.status().is_success() {
            return Err(UpdateError::Status(resp.status().as_u16()));
        }
        if resp.content_length().is_some_and(|n| n > MAX_INSTALLER_BYTES) {
            return Err(UpdateError::TooLarge);
        }
        tokio::fs::create_dir_all(&self.cfg.cache_dir).await?;
        let dest = self.cfg.cache_dir.join(&name);
        let partial = self.cfg.cache_dir.join(format!("{name}.partial"));
        let result: Result<(), UpdateError> = async {
            let mut out = tokio::fs::File::create(&partial).await?;
            let mut hasher = Sha512::new();
            let mut total: u64 = 0;
            let mut announced = false;
            while let Some(chunk) = resp.chunk().await? {
                if !announced {
                    // §5.1: `downloading` carries no version.
                    self.set_state(UpdateStateObj::new(UpdateStateType::Downloading, None));
                    announced = true;
                }
                total += chunk.len() as u64;
                if total > MAX_INSTALLER_BYTES {
                    return Err(UpdateError::TooLarge);
                }
                hasher.update(&chunk);
                out.write_all(&chunk).await?;
            }
            out.flush().await?;
            drop(out);
            let got = base64::engine::general_purpose::STANDARD.encode(hasher.finalize());
            if got != m.sha512.trim() {
                return Err(UpdateError::Checksum);
            }
            tokio::fs::rename(&partial, &dest).await?;
            Ok(())
        }
        .await;
        if let Err(e) = result {
            let _ = tokio::fs::remove_file(&partial).await;
            return Err(e);
        }
        Ok(DownloadedInstaller { file: dest, sha512_b64: m.sha512.trim().to_string() })
    }

    /// Mark the downloaded installer for running after teardown, then request
    /// a normal quit. Errors if nothing was downloaded.
    pub fn quit_and_install(&self) -> Result<(), UpdateError> {
        {
            let mut i = self.inner.lock().unwrap();
            if i.downloaded.is_none() {
                return Err(UpdateError::NothingToInstall);
            }
            i.install_on_exit = true;
        }
        if let Some(cb) = self.exit.lock().unwrap().clone() {
            cb();
        }
        Ok(())
    }

    /// Called after the LS and host bridge are down. If an install was
    /// requested, re-verify the file (digest + Authenticode) and launch it
    /// detached with the NSIS update args.
    pub fn run_pending_install(&self) {
        let pending = {
            let mut i = self.inner.lock().unwrap();
            if !std::mem::take(&mut i.install_on_exit) {
                return;
            }
            i.downloaded.clone()
        };
        let Some(d) = pending else { return };
        match sha512_file(&d.file) {
            Ok(h) if h == d.sha512_b64 => {}
            Ok(_) => {
                tracing::error!("installer {} changed after download; not running it", d.file.display());
                return;
            }
            Err(e) => {
                tracing::error!("cannot re-read installer {}: {e}", d.file.display());
                return;
            }
        }
        if !authenticode_valid(&d.file) {
            tracing::error!("installer {} has no valid Authenticode signature; not running it", d.file.display());
            return;
        }
        let mut cmd = std::process::Command::new(&d.file);
        cmd.arg("--updated");
        if self.cfg.headless {
            cmd.arg("/S");
        }
        match cmd.spawn() {
            Ok(_) => tracing::info!("launched installer {}", d.file.display()),
            Err(e) => tracing::error!("failed to launch installer {}: {e}", d.file.display()),
        }
    }
}

/// Pure §7.2 GetUpdateStatus mapping: a version counts only for the three
/// in-flight states and only when the state actually carries one.
pub fn status_from_state(current_version: &str, s: &UpdateStateObj) -> proto::UpdateStatus {
    let carries_version = match s.state_type() {
        UpdateStateType::AvailableForDownload | UpdateStateType::Downloading | UpdateStateType::Ready => s.version(),
        _ => None,
    };
    proto::UpdateStatus {
        current_version: current_version.to_string(),
        update_available: carries_version.is_some(),
        latest_version: carries_version.unwrap_or(current_version).to_string(),
    }
}

impl UpdateStatusProvider for Updater {
    fn get_update_status(&self) -> proto::UpdateStatus {
        status_from_state(&self.cfg.current_version, &self.state())
    }

    fn apply_update(&self) -> bool {
        match apply_action(self.state().state_type()) {
            ApplyAction::Install => self.quit_and_install().is_ok(),
            ApplyAction::AlreadyInProgress => true,
            ApplyAction::StartCheck => {
                self.spawn_check(false);
                true
            }
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yml_parse() {
        let y = "version: 2.27.0\nfiles:\n  - url: Antigravity-Setup-2.27.0.exe\n    sha512: abc==\n    size: 1\npath: Antigravity-Setup-2.27.0.exe\nsha512: abc==\nreleaseDate: '2026-01-01'\n";
        let m = parse_manifest(y).unwrap();
        assert_eq!(m.version, "2.27.0");
        assert_eq!(m.path, "Antigravity-Setup-2.27.0.exe");
        let y2 = "version: 3.0.0\nfiles:\n  - url: a.exe\n    sha512: zz\n";
        assert_eq!(parse_manifest(y2).unwrap().path, "a.exe");
        assert!(parse_manifest("path: x").is_err());
        // Up-to-date feeds may return only the version.
        assert_eq!(manifest_version("version: 2.26.0\n").unwrap(), "2.26.0");
        assert!(manifest_version("path: x").is_err());
    }

    #[test]
    fn version_compare() {
        assert!(is_newer("2.26.0", "2.27.0").unwrap());
        assert!(!is_newer("2.26.0", "2.26.0").unwrap());
        assert!(!is_newer("2.26.0", "v2.25.9").unwrap());
        assert!(is_newer("2.26.0", "2.26.1-beta.1").unwrap());
    }

    #[test]
    fn checksum() {
        let b64 = base64::engine::general_purpose::STANDARD.encode(Sha512::digest(b"hi"));
        assert!(verify_sha512(b"hi", &b64));
        assert!(!verify_sha512(b"ho", &b64));
    }

    #[test]
    fn state_mapping() {
        for t in [UpdateStateType::Idle, UpdateStateType::Checking, UpdateStateType::Ready, UpdateStateType::Downloading] {
            assert_eq!(UpdateStateType::from_wire(t.wire()), t);
        }
        assert_eq!(UpdateStateType::from_wire("bogus"), UpdateStateType::Idle);
        let s = status_from_state("2.26.0", &UpdateStateObj::new(UpdateStateType::Ready, Some("2.27.0".into())));
        assert!(s.update_available);
        assert_eq!(s.latest_version, "2.27.0");
        let s = status_from_state("2.26.0", &UpdateStateObj::new(UpdateStateType::Downloading, None));
        assert!(!s.update_available);
        assert_eq!(s.latest_version, "2.26.0");
        let j = serde_json::to_string(&UpdateStateObj::idle()).unwrap();
        assert_eq!(j, r#"{"type":"idle"}"#);
        assert_eq!(MenuStep::from_state(UpdateStateType::Ready).label(), "Restart to Update");
    }

    #[test]
    fn installer_names() {
        let u = url::Url::parse("https://h/dl/Setup-2.27.0.exe?sig=a%3Fb&x=1").unwrap();
        assert_eq!(installer_file_name(&u), "Setup-2.27.0.exe");
        let u = url::Url::parse("https://h/dl/").unwrap();
        assert_eq!(installer_file_name(&u), "installer.exe");
        let u = url::Url::parse("https://h/a%3Ab.exe").unwrap();
        assert_eq!(installer_file_name(&u), "a%3Ab.exe");
    }

    #[test]
    fn apply_actions_and_payloads() {
        assert_eq!(apply_action(UpdateStateType::Ready), ApplyAction::Install);
        assert_eq!(apply_action(UpdateStateType::Downloading), ApplyAction::AlreadyInProgress);
        assert_eq!(apply_action(UpdateStateType::Checking), ApplyAction::AlreadyInProgress);
        assert_eq!(apply_action(UpdateStateType::Idle), ApplyAction::StartCheck);
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let u = Updater::new(UpdaterConfig::default(), rt.handle().clone());
        u.apply();
        assert_eq!(serde_json::to_string(&u.state()).unwrap(), r#"{"type":"ready"}"#);
        assert!(u.quit_and_install().is_err());
        u.run_pending_install();
    }
}
