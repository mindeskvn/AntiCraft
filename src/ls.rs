//! language_server launcher (SPEC §3).
//!
//! Discovers the binary, builds argv/env, spawns with piped stdio, merges
//! stdout+stderr chunk by chunk into one line stream mirrored to
//! `language_server.log` (truncated per spawn), waits for the port line, and
//! supervises restarts (3 in 60 s, 2 s delay). On Windows every LS process is
//! placed in a kill-on-close Job Object so it dies with the shell.

use crate::paths::Paths;
use regex::Regex;
use std::collections::VecDeque;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{ChildStdin, Command};
use tokio::sync::{mpsc, oneshot};

pub const ENV_LS_BIN: &str = "CODEIUM_LANGUAGE_SERVER_BIN";
pub const ENV_LS_PORT: &str = "JETSKI_LS_PORT";
pub const ENV_BROWSER_PORT_FILE: &str = "AGY_BROWSER_ACTIVE_PORT_FILE";
pub const ENV_DEVTOOLS_MCP_JS: &str = "CHROME_DEVTOOLS_MCP_JS";
pub const API_SERVER_URL: &str = "https://generativelanguage.googleapis.com";
pub const CLOUD_CODE_ENDPOINT: &str = "https://daily-cloudcode-pa.googleapis.com";
pub const READY_TIMEOUT: Duration = Duration::from_secs(60);
pub const RESTART_DELAY: Duration = Duration::from_secs(2);
pub const RESTART_WINDOW: Duration = Duration::from_secs(60);
pub const MAX_RESTARTS: u32 = 3;
pub const KILL_GRACE: Duration = Duration::from_secs(5);
pub const STDERR_RING_CHARS: usize = 100_000;
pub const NODE_WRAPPER_NAME: &str = "agy-node.cmd";

/// Markers that start a crash trace in stderr (§3.5).
pub const CRASH_MARKERS: &[&str] = &[
    "panic:",
    "fatal error:",
    "unexpected fault address",
    "runtime:",
    "running GoogleExitFunction",
    "panic serving",
];

#[derive(Debug, thiserror::Error)]
pub enum LsError {
    #[error("failed to spawn language server: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("Timeout: language server did not report its port within {0}s")]
    Timeout(u64),
    #[error("Language server exited unexpectedly (code={code}, signal={signal})")]
    EarlyExit { code: String, signal: String },
    #[error("language server start cancelled by shutdown")]
    Cancelled,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

// ------------------------------------------------------------- discovery

/// Resolve the LS binary: `--ls-path`, then `CODEIUM_LANGUAGE_SERVER_BIN`,
/// then `<installed>\resources\bin\language_server.exe`.
pub fn discover_binary(cli: Option<&Path>, paths: &Paths) -> PathBuf {
    if let Some(p) = cli {
        return p.to_path_buf();
    }
    if let Some(v) = std::env::var_os(ENV_LS_BIN).filter(|v| !v.is_empty()) {
        return PathBuf::from(v);
    }
    paths.installed_resources().join("bin").join("language_server.exe")
}

/// `JETSKI_LS_PORT` coercion (§2 step 14): the value is interpreted with
/// JavaScript `Number()` string rules. If that yields `0` or NaN the port is
/// `"0"`; otherwise the trimmed literal is passed through as written
/// (SPEC: exponent/fractional forms reach the LS as-is).
pub fn target_port_from_env(raw: Option<&str>) -> String {
    match raw.map(js_number) {
        Some(Some(n)) if n != 0.0 => raw.unwrap_or_default().trim().to_string(),
        _ => "0".into(),
    }
}

/// JS `StringToNumber` subset: `None` means NaN. Surrounding whitespace is
/// ignored, empty is 0, `0x`/`0o`/`0b` integers (unsigned), case-sensitive
/// `Infinity` with an optional sign, and plain decimal literals.
fn js_number(s: &str) -> Option<f64> {
    let s = s.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}');
    if s.is_empty() {
        return Some(0.0);
    }
    let b = s.as_bytes();
    if b.len() > 2 && b[0] == b'0' {
        let radix = match b[1] {
            b'x' | b'X' => Some(16),
            b'o' | b'O' => Some(8),
            b'b' | b'B' => Some(2),
            _ => None,
        };
        if let Some(r) = radix {
            let mut v = 0f64;
            for c in s[2..].chars() {
                v = v * f64::from(r) + f64::from(c.to_digit(r)?);
            }
            return Some(v);
        }
    }
    let (sign, body) = match b[0] {
        b'+' => (1.0, &s[1..]),
        b'-' => (-1.0, &s[1..]),
        _ => (1.0, s),
    };
    if body == "Infinity" {
        return Some(sign * f64::INFINITY);
    }
    // Decimal literal: digits [. digits] [e[+-]digits], at least one digit
    // in the mantissa. Anything else (inf, nan, underscores...) is NaN.
    let (mantissa, exponent) = match body.find(['e', 'E']) {
        Some(i) => (&body[..i], Some(&body[i + 1..])),
        None => (body, None),
    };
    let mut parts = mantissa.splitn(2, '.');
    let int = parts.next().unwrap_or_default();
    let frac = parts.next().unwrap_or_default();
    let all_digits = |t: &str| t.bytes().all(|c| c.is_ascii_digit());
    if int.is_empty() && frac.is_empty() || !all_digits(int) || !all_digits(frac) {
        return None;
    }
    if let Some(e) = exponent {
        let digits = e.strip_prefix(['+', '-']).unwrap_or(e);
        if digits.is_empty() || !all_digits(digits) {
            return None;
        }
    }
    body.parse::<f64>().ok().map(|v| sign * v)
}

// ------------------------------------------------------------------ argv

/// Inputs to the §3.1 argv.
#[derive(Debug, Clone)]
pub struct LsArgs {
    pub app_version: String,
    /// `0` or the `JETSKI_LS_PORT` string.
    pub https_server_port: String,
    /// UUID v4, once per shell run.
    pub csrf_token: String,
    /// Directory NAME (`antigravity`).
    pub app_data_dir: String,
    /// `(url, token)` when the host bridge started.
    pub host_bridge: Option<(String, String)>,
    pub headless: bool,
    /// WSL mode adds `--exit_on_stdin_close` (WSL is unsupported; always false here).
    pub wsl: bool,
}

/// Build argv in exact §3.1 order.
pub fn build_args(a: &LsArgs) -> Vec<String> {
    let mut v: Vec<String> = [
        "--standalone",
        "--override_ide_name",
        "antigravity",
        "--subclient_type",
        "hub",
        "--override_ide_version",
        &a.app_version,
        "--override_user_agent_name",
        "antigravity",
        "--https_server_port",
        &a.https_server_port,
        "--csrf_token",
        &a.csrf_token,
        "--app_data_dir",
        &a.app_data_dir,
        "--api_server_url",
        API_SERVER_URL,
        "--cloud_code_endpoint",
        CLOUD_CODE_ENDPOINT,
        "--enable_sidecars",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    if let Some((url, token)) = &a.host_bridge {
        v.push(format!("--host_bridge_url={url}"));
        v.push(format!("--host_bridge_token={token}"));
    }
    if a.headless {
        v.push("--headless".into());
    }
    if a.wsl {
        v.push("--exit_on_stdin_close".into());
    }
    v
}

// ------------------------------------------------------------------- env

/// Exact `agy-node.cmd` content: three LF-terminated lines.
pub fn node_wrapper_content(electron_exe: &Path) -> String {
    format!("@echo off\nset ELECTRON_RUN_AS_NODE=1\n\"{}\" %*\n", electron_exe.display())
}

/// (Re)write `<dir>\agy-node.cmd` only if the content differs. Returns whether
/// the file was written. Creates `dir` recursively.
pub fn write_node_wrapper(dir: &Path, electron_exe: &Path) -> std::io::Result<bool> {
    let content = node_wrapper_content(electron_exe);
    let file = dir.join(NODE_WRAPPER_NAME);
    if std::fs::read(&file).ok().as_deref() == Some(content.as_bytes()) {
        return Ok(false);
    }
    std::fs::create_dir_all(dir)?;
    std::fs::write(&file, content)?;
    Ok(true)
}

/// Build the LS environment from `base` (normally `std::env::vars_os()`):
/// PATH (case-preserving key) prefixed with `<userData>\bin`,
/// `AGY_BROWSER_ACTIVE_PORT_FILE` and `CHROME_DEVTOOLS_MCP_JS`.
pub fn build_env(
    base: impl IntoIterator<Item = (OsString, OsString)>,
    paths: &Paths,
) -> Vec<(OsString, OsString)> {
    let mut env: Vec<(OsString, OsString)> = base.into_iter().collect();
    let bin = paths.node_wrapper_dir.as_os_str().to_os_string();
    match env.iter_mut().find(|(k, _)| k.to_string_lossy().eq_ignore_ascii_case("PATH")) {
        Some((_, v)) => {
            let mut nv = bin;
            nv.push(";");
            nv.push(&*v);
            *v = nv;
        }
        None => env.push(("PATH".into(), bin)),
    }
    let mut set = |k: &str, v: OsString| {
        env.retain(|(ek, _)| !ek.to_string_lossy().eq_ignore_ascii_case(k));
        env.push((k.into(), v));
    };
    set(ENV_BROWSER_PORT_FILE, paths.devtools_active_port.clone().into_os_string());
    set(ENV_DEVTOOLS_MCP_JS, paths.chrome_devtools_mcp_js().into_os_string());
    env
}

/// Write the node wrapper (non-fatal; only when the installed exe exists so a
/// valid existing file is never clobbered) and return the full LS environment.
pub fn prepare_environment(paths: &Paths) -> Vec<(OsString, OsString)> {
    let exe = paths.installed_electron_exe();
    if exe.exists() {
        match write_node_wrapper(&paths.node_wrapper_dir, &exe) {
            Ok(true) => tracing::info!("wrote {}", paths.node_wrapper_dir.join(NODE_WRAPPER_NAME).display()),
            Ok(false) => {}
            Err(e) => tracing::warn!("failed to write node wrapper: {e}"),
        }
    } else {
        tracing::warn!(
            "installed Antigravity.exe not found at {}; agy-node.cmd not written",
            exe.display()
        );
    }
    build_env(std::env::vars_os(), paths)
}

// ------------------------------------------------------------ line rules

/// Classification of one merged output line (§3.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineEvent {
    Port(u16),
    OAuthUrl(String),
    OpenUrl(String),
}

fn re(cell: &'static OnceLock<Regex>, pat: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pat).expect("valid regex"))
}

/// All events found on a line (a line may carry several).
pub fn classify_line(line: &str) -> Vec<LineEvent> {
    static PORT: OnceLock<Regex> = OnceLock::new();
    static OAUTH: OnceLock<Regex> = OnceLock::new();
    static OPEN: OnceLock<Regex> = OnceLock::new();
    let mut out = Vec::new();
    if let Some(c) = re(&PORT, r"(?i)listening on \w+ port at (\d+) for HTTP(S)?\b").captures(line) {
        if let Ok(p) = c[1].parse::<u16>() {
            out.push(LineEvent::Port(p));
        }
    }
    if let Some(m) = re(&OAUTH, r"https://accounts\.google\.com/o/oauth2/auth\S+").find(line) {
        // `\S+` also swallows a trailing ANSI escape (e.g. ESC[0m) from colored LS
        // output; cut at the first control character so the URL pastes cleanly.
        let url = m.as_str();
        let end = url.find(|c: char| c.is_control()).unwrap_or(url.len());
        out.push(LineEvent::OAuthUrl(url[..end].to_string()));
    }
    if let Some(c) = re(&OPEN, r"ANTIGRAVITY_OPEN_URL: (\S+)").captures(line) {
        if let Ok(u) = url::Url::parse(&c[1]) {
            if u.scheme() == "http" || u.scheme() == "https" {
                out.push(LineEvent::OpenUrl(c[1].to_string()));
            }
        }
    }
    out
}

/// Parse `Built at CL: (\d+)` from `--stamp` output.
pub fn parse_stamp(stdout: &str) -> Option<String> {
    static CL: OnceLock<Regex> = OnceLock::new();
    re(&CL, r"Built at CL: (\d+)").captures(stdout).map(|c| c[1].to_string())
}

/// Run `<binary> --stamp`. Deviation from the original: bounded by a 30 s
/// timeout so a hung stamp cannot block startup forever.
pub async fn query_stamp(binary: &Path) -> Option<String> {
    let mut cmd = Command::new(binary);
    cmd.arg("--stamp").stdin(Stdio::null()).kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    let out = tokio::time::timeout(Duration::from_secs(30), cmd.output()).await.ok()?.ok()?;
    parse_stamp(&String::from_utf8_lossy(&out.stdout))
}

// ---------------------------------------------------------- stderr / crash

/// Bounded stderr buffer (keeps the last `cap` chars).
#[derive(Debug, Clone)]
pub struct StderrRing {
    buf: VecDeque<char>,
    cap: usize,
}

impl StderrRing {
    pub fn new(cap: usize) -> Self {
        Self { buf: VecDeque::new(), cap }
    }
    pub fn push(&mut self, s: &str) {
        self.buf.extend(s.chars());
        while self.buf.len() > self.cap {
            self.buf.pop_front();
        }
    }
    pub fn contents(&self) -> String {
        self.buf.iter().collect()
    }
}

/// All stderr lines from the first one containing a crash marker.
pub fn extract_crash_trace(stderr: &str) -> Option<String> {
    let lines: Vec<&str> = stderr.lines().collect();
    let i = lines.iter().position(|l| CRASH_MARKERS.iter().any(|m| l.contains(m)))?;
    Some(lines[i..].join("\n"))
}

// ---------------------------------------------------------- restart policy

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartDecision {
    /// Restart attempt `n` of `MAX_RESTARTS`.
    Restart(u32),
    GiveUp,
}

/// §3.5 counting: reset after 60 s since the last restart; give up at 3.
#[derive(Debug, Clone)]
pub struct RestartPolicy {
    count: u32,
    last: Option<Instant>,
    window: Duration,
    max: u32,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self { count: 0, last: None, window: RESTART_WINDOW, max: MAX_RESTARTS }
    }
}

impl RestartPolicy {
    pub fn on_crash(&mut self, now: Instant) -> RestartDecision {
        if self.last.is_some_and(|l| now.duration_since(l) > self.window) {
            self.count = 0;
        }
        self.last = Some(now);
        if self.count >= self.max {
            RestartDecision::GiveUp
        } else {
            self.count += 1;
            RestartDecision::Restart(self.count)
        }
    }
}

// --------------------------------------------------------------- Job Object

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
#[cfg(windows)]
const CREATE_SUSPENDED: u32 = 0x0000_0004;

/// Process-wide kill-on-close job (stored as usize since HANDLE is !Send).
#[cfg(windows)]
fn job_handle() -> Option<usize> {
    static JOB: OnceLock<Option<usize>> = OnceLock::new();
    *JOB.get_or_init(|| unsafe {
        use windows_sys::Win32::System::JobObjects::*;
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return None;
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let ok = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        if ok == 0 {
            return None;
        }
        Some(job as usize)
    })
}

/// Put a (still suspended) child into the kill-on-close job.
#[cfg(windows)]
fn assign_to_job(child: &tokio::process::Child) -> std::io::Result<()> {
    let job = job_handle().ok_or_else(|| std::io::Error::other("job object unavailable"))?;
    let h = child.raw_handle().ok_or_else(|| std::io::Error::other("child already reaped"))?;
    unsafe {
        use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;
        if AssignProcessToJobObject(job as _, h as _) == 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Resume every thread of a process created with `CREATE_SUSPENDED`.
#[cfg(windows)]
fn resume_process(pid: u32) -> std::io::Result<()> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::*;
    use windows_sys::Win32::System::Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME};
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snap == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error());
        }
        let mut entry: THREADENTRY32 = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
        let mut resumed = 0;
        let mut more = Thread32First(snap, &mut entry) != 0;
        while more {
            if entry.th32OwnerProcessID == pid {
                let t = OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID);
                if !t.is_null() {
                    if ResumeThread(t) != u32::MAX {
                        resumed += 1;
                    }
                    CloseHandle(t);
                }
            }
            more = Thread32Next(snap, &mut entry) != 0;
        }
        CloseHandle(snap);
        if resumed == 0 {
            return Err(std::io::Error::other("no thread of the language server could be resumed"));
        }
    }
    Ok(())
}

// ------------------------------------------------------------- spawn/supervise

/// Callback invoked with a URL.
pub type UrlCallback = Arc<dyn Fn(&str) + Send + Sync>;
/// Callback invoked with a port.
pub type PortCallback = Arc<dyn Fn(u16) + Send + Sync>;

/// Hooks fired by the launcher.
#[derive(Clone, Default)]
pub struct LsCallbacks {
    /// Headless OAuth banner (§3.3).
    pub on_oauth_url: Option<UrlCallback>,
    /// `ANTIGRAVITY_OPEN_URL:` with an http/https URL (already validated).
    pub on_open_url: Option<UrlCallback>,
    /// After the first start and every successful restart.
    pub on_port_changed: Option<PortCallback>,
}

/// Everything needed to (re)spawn the LS with identical args.
#[derive(Debug, Clone)]
pub struct LsConfig {
    pub program: PathBuf,
    pub args: Vec<String>,
    /// Full environment; `None` inherits the shell's.
    pub env: Option<Vec<(OsString, OsString)>>,
    /// Headless keeps stdin open for forwarding; otherwise it is closed at once.
    pub keep_stdin_open: bool,
    pub log_path: PathBuf,
    pub ready_timeout: Duration,
    pub restart_delay: Duration,
}

#[derive(Debug, Clone)]
struct ExitInfo {
    code: Option<i32>,
}

/// Lines queued for the LS stdin; the writer task owns the pipe so a full
/// pipe can never block anyone holding `proc_`.
const STDIN_QUEUE: usize = 64;

struct LsProcess {
    pid: Option<u32>,
    stdin_tx: Option<mpsc::Sender<String>>,
    stdin_writer: Option<tokio::task::JoinHandle<()>>,
    kill_tx: Option<oneshot::Sender<()>>,
    monitor: Option<tokio::task::JoinHandle<()>>,
    exit_rx: Option<oneshot::Receiver<ExitInfo>>,
}

impl LsProcess {
    /// Close stdin, terminate (5 s grace, then force) and wait for the monitor.
    async fn stop(mut self) {
        self.stdin_tx = None;
        if let Some(w) = self.stdin_writer.take() {
            w.abort();
        }
        if let Some(tx) = self.kill_tx.take() {
            let _ = tx.send(());
        }
        if let Some(m) = self.monitor.take() {
            let _ = tokio::time::timeout(KILL_GRACE + Duration::from_secs(1), m).await;
        }
        tracing::info!("language server stopped (pid {:?})", self.pid);
    }
}

struct Shared {
    cfg: LsConfig,
    cbs: LsCallbacks,
    intentional: AtomicBool,
    /// Flipped to `true` by `shutdown`; wakes any in-flight respawn.
    cancel: tokio::sync::watch::Sender<bool>,
    /// Held by the supervisor for the whole crash → respawn → store sequence
    /// and by `shutdown` before it collects the process, so a respawn can
    /// never slip past a shutdown.
    respawn: tokio::sync::Mutex<()>,
    port: Mutex<Option<u16>>,
    proc_: tokio::sync::Mutex<Option<LsProcess>>,
    ring: Arc<Mutex<StderrRing>>,
}

impl Shared {
    async fn cancelled(&self) {
        let mut rx = self.cancel.subscribe();
        let _ = rx.wait_for(|c| *c).await;
    }
}

/// Supervised language server. Cheap to clone.
#[derive(Clone)]
pub struct LanguageServer {
    shared: Arc<Shared>,
}

enum Chunk {
    Out(Vec<u8>),
    Err(Vec<u8>),
}

type SpawnResult = Result<(LsProcess, u16), (LsError, Option<LsProcess>)>;

fn pump<R: tokio::io::AsyncRead + Unpin + Send + 'static>(
    mut r: R,
    tx: mpsc::UnboundedSender<Chunk>,
    is_err: bool,
) {
    tokio::spawn(async move {
        let mut b = vec![0u8; 8192];
        loop {
            match r.read(&mut b).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let d = b[..n].to_vec();
                    let c = if is_err { Chunk::Err(d) } else { Chunk::Out(d) };
                    if tx.send(c).is_err() {
                        break;
                    }
                }
            }
        }
    });
}

fn handle_line(raw: &[u8], out: &mut Vec<u8>, port_tx: &mut Option<oneshot::Sender<u16>>, cbs: &LsCallbacks) {
    let l = raw.strip_suffix(b"\r").unwrap_or(raw);
    let line = String::from_utf8_lossy(l).into_owned();
    out.extend_from_slice(line.as_bytes());
    out.push(b'\n');
    for ev in classify_line(&line) {
        match ev {
            LineEvent::Port(p) => {
                if let Some(tx) = port_tx.take() {
                    let _ = tx.send(p);
                }
            }
            LineEvent::OAuthUrl(u) => {
                if let Some(cb) = &cbs.on_oauth_url {
                    cb(&u)
                }
            }
            LineEvent::OpenUrl(u) => {
                if let Some(cb) = &cbs.on_open_url {
                    cb(&u)
                }
            }
        }
    }
}

fn spawn_stdin_writer(mut stdin: ChildStdin) -> (mpsc::Sender<String>, tokio::task::JoinHandle<()>) {
    let (tx, mut rx) = mpsc::channel::<String>(STDIN_QUEUE);
    let task = tokio::spawn(async move {
        while let Some(line) = rx.recv().await {
            let mut data = line.into_bytes();
            data.push(b'\n');
            if stdin.write_all(&data).await.is_err() || stdin.flush().await.is_err() {
                break;
            }
        }
    });
    (tx, task)
}

/// Spawn the process suspended, put it in the job, then let it run, so no
/// grandchild can be created outside the job.
fn spawn_child(cfg: &LsConfig) -> Result<tokio::process::Child, LsError> {
    let mut cmd = Command::new(&cfg.program);
    cmd.args(&cfg.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(env) = &cfg.env {
        cmd.env_clear().envs(env.iter().map(|(k, v)| (k, v)));
    }
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED);
    let mut child = cmd.spawn().map_err(LsError::Spawn)?;
    #[cfg(windows)]
    {
        if let Err(e) = assign_to_job(&child) {
            // kill_on_drop still reaps it if the shell exits normally.
            tracing::warn!("could not assign language server to job object: {e}");
        }
        let resumed = child.id().ok_or_else(|| std::io::Error::other("no pid")).and_then(resume_process);
        if let Err(e) = resumed {
            let _ = child.start_kill();
            return Err(LsError::Spawn(e));
        }
    }
    Ok(child)
}

async fn spawn_once(sh: &Arc<Shared>) -> SpawnResult {
    let cfg = &sh.cfg;
    let mut child = spawn_child(cfg).map_err(|e| (e, None))?;

    let stdin = child.stdin.take().filter(|_| cfg.keep_stdin_open); // dropped → EOF
    let (stdin_tx, stdin_writer) = match stdin.map(spawn_stdin_writer) {
        Some((tx, task)) => (Some(tx), Some(task)),
        None => (None, None),
    };
    let (ctx, mut crx) = mpsc::unbounded_channel::<Chunk>();
    if let Some(out) = child.stdout.take() {
        pump(out, ctx.clone(), false);
    }
    if let Some(err) = child.stderr.take() {
        pump(err, ctx.clone(), true);
    }
    drop(ctx);

    // Merger: one line stream, mirrored to the log (truncated per spawn).
    let (port_tx, mut port_rx) = oneshot::channel::<u16>();
    let log_path = cfg.log_path.clone();
    let ring = sh.ring.clone();
    let cbs = sh.cbs.clone();
    let merger = tokio::spawn(async move {
        if let Some(d) = log_path.parent() {
            let _ = tokio::fs::create_dir_all(d).await;
        }
        let mut log = tokio::fs::File::create(&log_path).await.ok();
        let mut port_tx = Some(port_tx);
        let mut buf: Vec<u8> = Vec::new();
        while let Some(chunk) = crx.recv().await {
            let data = match chunk {
                Chunk::Out(d) => d,
                Chunk::Err(d) => {
                    ring.lock().unwrap().push(&String::from_utf8_lossy(&d));
                    d
                }
            };
            buf.extend_from_slice(&data);
            let mut out = Vec::new();
            while let Some(i) = buf.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = buf.drain(..=i).collect();
                handle_line(&line[..line.len() - 1], &mut out, &mut port_tx, &cbs);
            }
            if let (Some(f), false) = (log.as_mut(), out.is_empty()) {
                let _ = f.write_all(&out).await;
            }
        }
        if !buf.is_empty() {
            let mut out = Vec::new();
            handle_line(&buf, &mut out, &mut port_tx, &cbs);
            if let Some(f) = log.as_mut() {
                let _ = f.write_all(&out).await;
            }
        }
        if let Some(f) = log.as_mut() {
            let _ = f.flush().await;
        }
    });

    // Monitor: owns the Child; on request terminate, wait 5 s, force kill.
    let pid = child.id();
    let (kill_tx, kill_rx) = oneshot::channel::<()>();
    let (exit_tx, mut exit_rx) = oneshot::channel::<ExitInfo>();
    let monitor = tokio::spawn(async move {
        let status = tokio::select! {
            s = child.wait() => s.ok(),
            _ = kill_rx => {
                // On Windows both steps are TerminateProcess; kept for parity.
                let _ = child.start_kill();
                match tokio::time::timeout(KILL_GRACE, child.wait()).await {
                    Ok(s) => s.ok(),
                    Err(_) => {
                        let _ = child.kill().await;
                        child.try_wait().ok().flatten()
                    }
                }
            }
        };
        let _ = exit_tx.send(ExitInfo { code: status.and_then(|s| s.code()) });
    });

    let mut p = LsProcess {
        pid,
        stdin_tx,
        stdin_writer,
        kill_tx: Some(kill_tx),
        monitor: Some(monitor),
        exit_rx: None,
    };
    let deadline = tokio::time::sleep(cfg.ready_timeout);
    tokio::pin!(deadline);
    let mut output_closed = false;
    loop {
        tokio::select! {
            port = &mut port_rx, if !output_closed => match port {
                Ok(port) => {
                    p.exit_rx = Some(exit_rx);
                    return Ok((p, port));
                }
                // Output closed without a port line: keep waiting for the
                // exit, the deadline or a cancel.
                Err(_) => output_closed = true,
            },
            info = &mut exit_rx => {
                let info = info.unwrap_or(ExitInfo { code: None });
                if !output_closed {
                    // Let the merger drain; the port line may still be in flight.
                    let _ = tokio::time::timeout(Duration::from_secs(1), merger).await;
                    if let Ok(port) = port_rx.try_recv() {
                        // Started but already exited: hand a fired exit to the supervisor.
                        let (tx, rx) = oneshot::channel();
                        let _ = tx.send(info);
                        p.exit_rx = Some(rx);
                        return Ok((p, port));
                    }
                }
                return Err((early_exit(&info), None));
            },
            _ = &mut deadline => {
                p.exit_rx = Some(exit_rx);
                return Err((LsError::Timeout(cfg.ready_timeout.as_secs()), Some(p)));
            },
            _ = sh.cancelled() => {
                p.exit_rx = Some(exit_rx);
                return Err((LsError::Cancelled, Some(p)));
            },
        }
    }
}

fn early_exit(info: &ExitInfo) -> LsError {
    LsError::EarlyExit {
        code: info.code.map(|c| c.to_string()).unwrap_or_else(|| "null".into()),
        signal: "null".into(),
    }
}

impl LanguageServer {
    /// Spawn, wait for the port, call `on_port_changed`, arm crash monitoring.
    /// On failure the returned handle still owns any live process (timeout
    /// case), so the caller can `shutdown()` it on quit.
    pub async fn start(cfg: LsConfig, cbs: LsCallbacks) -> Result<(Self, u16), (LsError, Self)> {
        let (cancel, _) = tokio::sync::watch::channel(false);
        let shared = Arc::new(Shared {
            cfg,
            cbs,
            intentional: AtomicBool::new(false),
            cancel,
            respawn: tokio::sync::Mutex::new(()),
            port: Mutex::new(None),
            proc_: tokio::sync::Mutex::new(None),
            ring: Arc::new(Mutex::new(StderrRing::new(STDERR_RING_CHARS))),
        });
        let me = Self { shared };
        match spawn_once(&me.shared).await {
            Ok((mut p, port)) => {
                let exit_rx = p.exit_rx.take();
                *me.shared.port.lock().unwrap() = Some(port);
                *me.shared.proc_.lock().await = Some(p);
                if let Some(cb) = &me.shared.cbs.on_port_changed {
                    cb(port);
                }
                if let Some(rx) = exit_rx {
                    tokio::spawn(supervise(me.shared.clone(), rx));
                }
                Ok((me, port))
            }
            Err((e, p)) => {
                *me.shared.proc_.lock().await = p;
                Err((e, me))
            }
        }
    }

    /// Last known port (NOT reset after an exit, as in the original).
    #[allow(dead_code)]
    pub fn port(&self) -> Option<u16> {
        *self.shared.port.lock().unwrap()
    }

    /// Whether a process is currently tracked.
    #[allow(dead_code)]
    pub async fn is_running(&self) -> bool {
        self.shared.proc_.lock().await.is_some()
    }

    /// Current stderr ring contents.
    #[allow(dead_code)]
    pub fn stderr_tail(&self) -> String {
        self.shared.ring.lock().unwrap().contents()
    }

    /// Headless stdin forwarding: queue `line` for the LS if it is alive.
    /// The lock is released before waiting for queue space.
    pub async fn write_stdin_line(&self, line: &str) -> std::io::Result<()> {
        let tx = self.shared.proc_.lock().await.as_ref().and_then(|p| p.stdin_tx.clone());
        if let Some(tx) = tx {
            tx.send(line.to_string()).await.map_err(|_| std::io::Error::from(std::io::ErrorKind::BrokenPipe))?;
        }
        Ok(())
    }

    /// §3.7: mark intentional, cancel any in-flight respawn, wait for the
    /// supervisor to settle, then terminate (≤5 s grace, force kill) and clear.
    pub async fn shutdown(&self) {
        let sh = &self.shared;
        sh.intentional.store(true, Ordering::SeqCst);
        sh.cancel.send_replace(true);
        let _settled = sh.respawn.lock().await;
        let p = sh.proc_.lock().await.take();
        if let Some(p) = p {
            p.stop().await;
        }
    }
}

async fn supervise(sh: Arc<Shared>, mut exit_rx: oneshot::Receiver<ExitInfo>) {
    let mut policy = RestartPolicy::default();
    loop {
        let info = (&mut exit_rx).await.unwrap_or(ExitInfo { code: None });
        let _respawning = sh.respawn.lock().await;
        if sh.intentional.load(Ordering::SeqCst) {
            return;
        }
        *sh.proc_.lock().await = None;
        match info.code {
            Some(c) => tracing::error!("Language server crashed: exited with code {c}"),
            // Windows has no signals; the original's text is kept for log scrapers.
            None => tracing::error!("Language server crashed: killed by signal unknown"),
        }
        let trace = extract_crash_trace(&sh.ring.lock().unwrap().contents());
        if let Some(trace) = trace {
            tracing::error!("Language server crash trace:\n{trace}");
        }
        let n = match policy.on_crash(Instant::now()) {
            RestartDecision::GiveUp => {
                tracing::error!("Language server crashed {MAX_RESTARTS} times in a row. Giving up.");
                return;
            }
            RestartDecision::Restart(n) => n,
        };
        tracing::warn!("Attempting restart {n}/{MAX_RESTARTS} in {}s...", sh.cfg.restart_delay.as_secs_f32());
        tokio::select! {
            _ = tokio::time::sleep(sh.cfg.restart_delay) => {}
            _ = sh.cancelled() => return,
        }
        if sh.intentional.load(Ordering::SeqCst) {
            return;
        }
        match spawn_once(&sh).await {
            Ok((mut p, port)) => {
                if sh.intentional.load(Ordering::SeqCst) {
                    p.stop().await;
                    return;
                }
                let Some(rx) = p.exit_rx.take() else { return };
                exit_rx = rx;
                *sh.port.lock().unwrap() = Some(port);
                *sh.proc_.lock().await = Some(p);
                if let Some(cb) = &sh.cbs.on_port_changed {
                    cb(port);
                }
            }
            Err((e, p)) => {
                if !matches!(e, LsError::Cancelled) {
                    tracing::error!("Language server restart failed: {e}");
                }
                // A timed-out process stays tracked so shutdown can stop it;
                // after a shutdown it is stopped right here.
                match p {
                    Some(p) if sh.intentional.load(Ordering::SeqCst) => p.stop().await,
                    other => *sh.proc_.lock().await = other,
                }
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn args() -> LsArgs {
        LsArgs {
            app_version: "2.26.0".into(),
            https_server_port: "0".into(),
            csrf_token: "c".into(),
            app_data_dir: "antigravity".into(),
            host_bridge: Some(("http://127.0.0.1:9".into(), "ab".into())),
            headless: true,
            wsl: false,
        }
    }

    #[test]
    fn argv_order() {
        let v = build_args(&args());
        assert_eq!(&v[..3], ["--standalone", "--override_ide_name", "antigravity"]);
        assert_eq!(v[19], "--enable_sidecars");
        assert_eq!(v[20], "--host_bridge_url=http://127.0.0.1:9");
        assert_eq!(v[21], "--host_bridge_token=ab");
        assert_eq!(v.last().unwrap(), "--headless");
    }

    #[test]
    fn port_env() {
        assert_eq!(target_port_from_env(None), "0");
        assert_eq!(target_port_from_env(Some("abc")), "0");
        assert_eq!(target_port_from_env(Some("0")), "0");
        assert_eq!(target_port_from_env(Some("1e3")), "1e3");
        assert_eq!(target_port_from_env(Some("4321")), "4321");
        assert_eq!(target_port_from_env(Some("0x1F90")), "0x1F90");
        assert_eq!(target_port_from_env(Some("0x0")), "0");
        assert_eq!(target_port_from_env(Some("inf")), "0");
        assert_eq!(target_port_from_env(Some("Infinity")), "Infinity");
        assert_eq!(target_port_from_env(Some("-5.5")), "-5.5");
        assert_eq!(target_port_from_env(Some("1_0")), "0");
        assert_eq!(target_port_from_env(Some(" ")), "0");
    }

    #[test]
    fn lines() {
        assert_eq!(classify_line("I listening on random port at 54321 for HTTPS"), vec![LineEvent::Port(54321)]);
        assert_eq!(classify_line("LISTENING ON fixed PORT AT 1 for http"), vec![LineEvent::Port(1)]);
        assert!(classify_line("listening on random port at 5 for HTTPX").is_empty());
        assert_eq!(
            classify_line("ANTIGRAVITY_OPEN_URL: https://x.y/z"),
            vec![LineEvent::OpenUrl("https://x.y/z".into())]
        );
        assert!(classify_line("ANTIGRAVITY_OPEN_URL: file:///c").is_empty());
        assert!(matches!(
            classify_line("go https://accounts.google.com/o/oauth2/auth?x=1 now")[0],
            LineEvent::OAuthUrl(_)
        ));
        assert_eq!(
            classify_line("visit https://accounts.google.com/o/oauth2/auth?x=1\x1b[0m"),
            vec![LineEvent::OAuthUrl("https://accounts.google.com/o/oauth2/auth?x=1".into())]
        );
        assert_eq!(parse_stamp("a\nBuilt at CL: 12345\n").as_deref(), Some("12345"));
    }

    #[test]
    fn crash_trace_and_ring() {
        let t = extract_crash_trace("ok\nmore\npanic: boom\ngoroutine 1\n").unwrap();
        assert_eq!(t, "panic: boom\ngoroutine 1");
        let mut r = StderrRing::new(4);
        r.push("abcdef");
        assert_eq!(r.contents(), "cdef");
    }

    #[test]
    fn restart_policy() {
        let mut p = RestartPolicy::default();
        let t = Instant::now();
        assert_eq!(p.on_crash(t), RestartDecision::Restart(1));
        assert_eq!(p.on_crash(t), RestartDecision::Restart(2));
        assert_eq!(p.on_crash(t), RestartDecision::Restart(3));
        assert_eq!(p.on_crash(t), RestartDecision::GiveUp);
        assert_eq!(p.on_crash(t + Duration::from_secs(61)), RestartDecision::Restart(1));
    }

    #[test]
    fn wrapper_written_once() {
        let d = tempfile::tempdir().unwrap();
        let exe = Path::new("C:\\X\\Antigravity.exe");
        assert!(write_node_wrapper(&d.path().join("bin"), exe).unwrap());
        assert!(!write_node_wrapper(&d.path().join("bin"), exe).unwrap());
        let c = std::fs::read_to_string(d.path().join("bin").join(NODE_WRAPPER_NAME)).unwrap();
        assert_eq!(c, "@echo off\nset ELECTRON_RUN_AS_NODE=1\n\"C:\\X\\Antigravity.exe\" %*\n");
    }

    #[test]
    fn env_prefix() {
        let p = Paths::from_parts("C:\\ud".into(), "C:\\h".into(), "C:\\i".into());
        let env = build_env(vec![("Path".into(), "C:\\Windows".into())], &p);
        let path = env.iter().find(|(k, _)| k == "Path").unwrap();
        assert_eq!(path.1, OsString::from("C:\\ud\\bin;C:\\Windows"));
        assert!(env.iter().any(|(k, _)| k == ENV_DEVTOOLS_MCP_JS));
    }

    fn fake(script: &str, log: &Path) -> LsConfig {
        LsConfig {
            program: "cmd.exe".into(),
            args: vec!["/c".into(), script.into()],
            env: None,
            keep_stdin_open: false,
            log_path: log.to_path_buf(),
            ready_timeout: Duration::from_secs(10),
            restart_delay: Duration::from_millis(50),
        }
    }

    fn cbs(counter: Arc<AtomicUsize>) -> LsCallbacks {
        LsCallbacks {
            on_oauth_url: None,
            on_open_url: None,
            on_port_changed: Some(Arc::new(move |_| {
                counter.fetch_add(1, Ordering::SeqCst);
            })),
        }
    }

    #[tokio::test]
    async fn fake_ls_ready_and_shutdown() {
        let d = tempfile::tempdir().unwrap();
        let log = d.path().join("logs").join("language_server.log");
        let c = Arc::new(AtomicUsize::new(0));
        let cfg = fake(
            "echo hello&& echo listening on random port at 54321 for HTTPS&& ping -n 30 127.0.0.1 >nul",
            &log,
        );
        let (ls, port) = LanguageServer::start(cfg, cbs(c.clone())).await.map_err(|(e, _)| e).unwrap();
        assert_eq!(port, 54321);
        assert_eq!(c.load(Ordering::SeqCst), 1);
        assert!(ls.is_running().await);
        ls.shutdown().await;
        assert!(!ls.is_running().await);
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(text.contains("hello\n"));
    }

    #[tokio::test]
    async fn fake_ls_early_exit() {
        let d = tempfile::tempdir().unwrap();
        let c = Arc::new(AtomicUsize::new(0));
        let r = LanguageServer::start(fake("exit 3", &d.path().join("l.log")), cbs(c)).await;
        match r {
            Err((LsError::EarlyExit { code, .. }, _)) => assert_eq!(code, "3"),
            _ => panic!("expected early exit"),
        }
    }

    #[tokio::test]
    async fn fake_ls_timeout_keeps_process() {
        let d = tempfile::tempdir().unwrap();
        let mut cfg = fake("ping -n 30 127.0.0.1 >nul", &d.path().join("l.log"));
        cfg.ready_timeout = Duration::from_millis(500);
        let c = Arc::new(AtomicUsize::new(0));
        let Err((LsError::Timeout(_), ls)) = LanguageServer::start(cfg, cbs(c)).await else {
            panic!("expected timeout")
        };
        assert!(ls.is_running().await);
        ls.shutdown().await;
        assert!(!ls.is_running().await);
    }

    #[tokio::test]
    async fn fake_ls_restarts_three_times() {
        let d = tempfile::tempdir().unwrap();
        let c = Arc::new(AtomicUsize::new(0));
        let cfg = fake("echo listening on random port at 1234 for HTTP", &d.path().join("l.log"));
        let (_ls, _) = LanguageServer::start(cfg, cbs(c.clone())).await.map_err(|(e, _)| e).unwrap();
        for _ in 0..100 {
            if c.load(Ordering::SeqCst) >= 4 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(c.load(Ordering::SeqCst), 4);
    }
}
