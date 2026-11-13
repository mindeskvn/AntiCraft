//! MCP stdio transport: a child process speaking newline-delimited JSON-RPC 2.0.
//!
//! The shape is forced by three failure modes seen with real servers:
//!
//! * **Framing.** One message per line. The 4 MiB frame cap is enforced *while* reading via
//!   `fill_buf`/`consume`, because `read_until` allocates the whole line before any check, so a
//!   server that never sends a newline would grow memory without bound.
//! * **Deadlock on shutdown.** A child's pipes must be drained before waiting on it, and stderr
//!   must be drained all along — an unread pipe fills at roughly 64 KiB, after which the server
//!   blocks inside `write` and every call looks like a hang until its timeout. stderr is kept as
//!   a bounded tail so a dying server's last words can still be reported.
//! * **One stuck call must not take the server down.** A timeout removes that caller's entry,
//!   tells the server that the request is cancelled, and kills nothing.
//!
//! The protocol machinery is generic over the two pipe halves ([`Session`]) so it can be driven
//! by an in-memory duplex in tests; [`StdioTransport`] adds process ownership on top.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::oneshot;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use super::config::Timeouts;
use super::jsonrpc::{error_reply, notification, request, split_payload, IdGen, Message, METHOD_NOT_FOUND};
use super::{McpError, PROTOCOL_VERSION};

/// Largest single stdout line accepted. A server that exceeds it is flooding, not answering.
pub const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;
/// Most requests that may be waiting on one server at once.
pub const MAX_PENDING: usize = 32;
/// Tail of stderr kept for the error reported when the process dies.
pub const STDERR_TAIL_BYTES: usize = 16 * 1024;
/// How long a server gets to exit on its own after stdin closes.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
/// Deadline for writing one frame to the child's stdin.
///
/// Writes are the one part of a request that has no natural deadline of its own: the per-call
/// timeout starts after the frame is out, so a server that stops reading its pipe would park
/// every caller here indefinitely rather than under their own deadlines.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// Read buffer in front of the child's stdout.
const READ_BUFFER_BYTES: usize = 64 * 1024;
/// Chunk size for the stderr drainer.
const STDERR_CHUNK_BYTES: usize = 4096;

/// A waiter for one response.
type Waiter = oneshot::Sender<Result<Value, McpError>>;
/// The map of request id to waiter, shared between the caller and the read loop.
type Pending = Arc<Mutex<HashMap<u64, Waiter>>>;

/// Protocol state shared with the read loop, independent of where the bytes come from.
struct Session<W: AsyncWrite + Unpin + Send> {
    /// Write half. Held for the whole of one frame so two concurrent calls cannot interleave
    /// bytes inside a line. `None` once shutdown has closed it: for a stdio server, EOF on
    /// stdin *is* the shutdown request, and a pipe that stays open is a server that can only
    /// be killed.
    out: Mutex<Option<W>>,
    pending: Pending,
    ids: IdGen,
    /// Set by the read loop on EOF or a frame overflow, and by `close`.
    dead: AtomicBool,
    /// Last [`STDERR_TAIL_BYTES`] of the child's stderr.
    stderr: Arc<Mutex<String>>,
    /// Whether writing `notifications/cancelled` is meaningful.
    cancel_notifications: bool,
}

impl<W: AsyncWrite + Unpin + Send> Session<W> {
    /// Write one message as a single newline-terminated line.
    ///
    /// The write itself is bounded. A server that stopped reading its stdin fills the pipe,
    /// and without this deadline every caller would block here forever — past its own request
    /// deadline, because the deadline only starts once the frame is out.
    async fn write(&self, message: &Value) -> Result<(), McpError> {
        let mut line = serde_json::to_vec(message).map_err(|e| McpError::Protocol(e.to_string()))?;
        // Newline-delimited framing: a literal newline inside the JSON would split the frame.
        // `serde_json` escapes control characters inside strings so this cannot be produced,
        // but the check is a refusal rather than an assertion: a debug-only panic here would
        // be an availability bug in a path that must always answer, and silently splitting the
        // frame would be worse.
        if line.contains(&b'\n') {
            return Err(McpError::Protocol("the message serialized with an embedded newline".to_owned()));
        }
        line.push(b'\n');
        let mut guard = self.out.lock().await;
        let Some(out) = guard.as_mut() else {
            return Err(McpError::Dead("the server's input is closed".to_owned()));
        };
        let written = tokio::time::timeout(WRITE_TIMEOUT, async {
            out.write_all(&line).await?;
            out.flush().await
        })
        .await;
        match written {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => {
                // A torn pipe means the process is gone; releasing the writer makes that
                // visible to every other caller instead of failing them one at a time.
                *guard = None;
                Err(McpError::Dead(format!("stdin write failed: {e}")))
            }
            Err(_) => {
                // The pipe is full and nobody is draining it. Dropping the writer is the only
                // way to unblock: closing stdin kills the server, which the caller then sees.
                *guard = None;
                Err(McpError::Dead(format!(
                    "the server did not read its input within {}s",
                    WRITE_TIMEOUT.as_secs()
                )))
            }
        }
    }

    /// Send a request and wait for its answer. See the module docs for the timeout contract.
    async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Value, McpError> {
        if self.dead.load(Ordering::SeqCst) {
            return Err(self.death_error());
        }
        let id = self.ids.next();
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().await;
            if pending.len() >= MAX_PENDING {
                return Err(McpError::Protocol(format!(
                    "more than {MAX_PENDING} requests are already in flight to the server"
                )));
            }
            // The waiter is registered before the frame is written so a very fast server
            // cannot answer before anybody is listening.
            pending.insert(id, tx);
        }
        if let Err(e) = self.write(&request(id, method, params)).await {
            self.pending.lock().await.remove(&id);
            return Err(e);
        }
        let outcome = {
            let deadline = tokio::time::sleep(timeout);
            tokio::pin!(deadline);
            tokio::select! {
                biased;
                _ = cancel.cancelled() => Err(McpError::Cancelled),
                received = rx => match received {
                    Ok(result) => result,
                    // The read loop drops every sender on EOF or overflow.
                    Err(_) => Err(self.death_error()),
                },
                _ = &mut deadline => Err(McpError::Timeout),
            }
        };
        match outcome {
            Ok(result) => Ok(result),
            Err(err) => {
                self.pending.lock().await.remove(&id);
                if matches!(err, McpError::Timeout | McpError::Cancelled) && self.cancel_notifications {
                    // Fire and forget, but still bounded: a server that stopped reading stdin
                    // would otherwise block here for as long as its pipe stays full.
                    let note = notification("notifications/cancelled", json!({ "requestId": id, "reason": "client timeout" }));
                    let _ = tokio::time::timeout(SHUTDOWN_GRACE, self.write(&note)).await;
                }
                Err(err)
            }
        }
    }

    /// A notification, fire and forget.
    async fn notify(&self, method: &str, params: Value) -> Result<(), McpError> {
        self.write(&notification(method, params)).await
    }

    /// The error to report once the process is gone, with whatever it printed last.
    fn death_error(&self) -> McpError {
        let tail = self.stderr.try_lock().map(|t| t.trim().to_owned()).unwrap_or_default();
        if tail.is_empty() {
            McpError::Dead("the server stopped responding".to_owned())
        } else {
            McpError::Dead(format!("the server stopped: {tail}"))
        }
    }

    /// Fail every waiter so callers return immediately instead of waiting out their timeouts.
    async fn fail_all(&self) {
        self.dead.store(true, Ordering::SeqCst);
        self.pending.lock().await.clear();
    }
}

/// A live stdio server: the child, its pipes, and the two background tasks.
pub struct StdioTransport {
    session: Arc<Session<tokio::process::ChildStdin>>,
    child: Mutex<Option<tokio::process::Child>>,
    /// Kills grandchildren: a Job Object on Windows, a process group on unix. Dropped only
    /// after the child is reaped — dropping it earlier kills the server.
    tree: Mutex<Option<crate::agent::mcp::stdio::TreeGuard>>,
    reader: Mutex<Option<tokio::task::JoinHandle<()>>>,
    stderr_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    timeouts: Timeouts,
    name: String,
}

impl std::fmt::Debug for StdioTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StdioTransport").field("name", &self.name).finish()
    }
}

impl StdioTransport {
    /// Spawn the child, complete the handshake, and return the transport with its tools.
    ///
    /// The process is torn down on every failure path, including a handshake that answers
    /// without the `tools` capability: nothing here leaves a server running behind an error.
    pub async fn connect(
        name: &str,
        command: &str,
        args: &[String],
        env: &[(String, String)],
        cwd: Option<&std::path::Path>,
        timeouts: Timeouts,
        cancel: &CancellationToken,
    ) -> Result<(Arc<Self>, Vec<Value>), McpError> {
        let mut cmd = Command::new(command);
        cmd.args(args)
            .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            // Never null: stderr is the only diagnostic a failed server gives. Never unread
            // either — see the module docs.
            .stderr(std::process::Stdio::piped())
            // Last-resort net only; `shutdown` is the graceful path.
            .kill_on_drop(true);
        #[cfg(windows)]
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        #[cfg(unix)]
        cmd.process_group(0);
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        let mut child = cmd.spawn().map_err(|e| McpError::Dead(format!("could not start `{command}`: {e}")))?;
        // Attached immediately, before the server can fork grandchildren.
        let tree = TreeGuard::attach(&child);
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let (Some(stdin), Some(stdout)) = (stdin, stdout) else {
            let _ = child.start_kill();
            return Err(McpError::Protocol("the server was started without piped stdio".to_owned()));
        };

        let stderr_tail = Arc::new(Mutex::new(String::new()));
        let stderr_task = stderr.map(|pipe| tokio::spawn(drain_stderr(pipe, stderr_tail.clone())));

        let session = Arc::new(Session {
            out: Mutex::new(Some(stdin)),
            pending: Arc::new(Mutex::new(HashMap::new())),
            ids: IdGen::new(),
            dead: AtomicBool::new(false),
            stderr: stderr_tail,
            cancel_notifications: true,
        });
        let reader = tokio::spawn(read_loop(BufReader::with_capacity(READ_BUFFER_BYTES, stdout), session.clone()));

        let transport = Arc::new(Self {
            session,
            child: Mutex::new(Some(child)),
            tree: Mutex::new(tree),
            reader: Mutex::new(Some(reader)),
            stderr_task: Mutex::new(stderr_task),
            timeouts,
            name: name.to_owned(),
        });

        match transport.handshake(cancel).await {
            Ok(tools) => Ok((transport, tools)),
            Err(err) => {
                transport.shutdown().await;
                Err(err)
            }
        }
    }

    /// `initialize`, `notifications/initialized`, then `tools/list` following `nextCursor`.
    async fn handshake(&self, cancel: &CancellationToken) -> Result<Vec<Value>, McpError> {
        let reply = self
            .session
            .request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    // Claim only what this client implements. Declaring `sampling` would oblige
                    // us to run a model for the server, which we never do.
                    "capabilities": { "roots": { "listChanged": false } },
                    "clientInfo": { "name": "anticraft-native", "version": env!("CARGO_PKG_VERSION") }
                }),
                self.timeouts.init(),
                cancel,
            )
            .await?;
        let capabilities = reply.get("capabilities");
        let has_tools = capabilities.and_then(|c| c.get("tools")).is_some();
        if !has_tools {
            let version = reply.get("protocolVersion").and_then(Value::as_str).unwrap_or("unknown");
            return Err(McpError::Protocol(format!(
                "the server answers version {version} without declaring the tools capability"
            )));
        }
        self.session.notify("notifications/initialized", json!({})).await?;
        self.list_tools(cancel).await
    }

    /// Page through `tools/list`. Entries are returned raw; the hub validates and names them.
    async fn list_tools(&self, cancel: &CancellationToken) -> Result<Vec<Value>, McpError> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        // A server that keeps handing back a cursor would loop forever; 32 pages is generous.
        for _ in 0..32 {
            let params = match &cursor {
                Some(c) => json!({ "cursor": c }),
                None => json!({}),
            };
            let page = self.session.request("tools/list", params, self.timeouts.init(), cancel).await?;
            if let Some(items) = page.get("tools").and_then(Value::as_array) {
                out.extend(items.iter().cloned());
            }
            if out.len() >= super::MAX_TOOLS_PER_SERVER {
                out.truncate(super::MAX_TOOLS_PER_SERVER);
                break;
            }
            cursor = page
                .get("nextCursor")
                .and_then(Value::as_str)
                .filter(|c| !c.is_empty())
                .map(str::to_owned);
            if cursor.is_none() {
                break;
            }
        }
        Ok(out)
    }

    /// Run one tool and return its uncapped `content` plus the `isError` flag.
    pub async fn call(&self, tool: &str, args: Value, cancel: &CancellationToken) -> Result<Value, McpError> {
        self.session.request("tools/call", json!({ "name": tool, "arguments": args }), self.timeouts.call(), cancel).await
    }

    /// Whether the server is known to be gone.
    pub fn is_dead(&self) -> bool {
        self.session.dead.load(Ordering::SeqCst)
    }

    /// Close stdin, give the server a moment to exit, then kill the whole tree.
    ///
    /// Reaping runs to completion even when `shutdown` is called twice: the tasks are aborted
    /// and the guard dropped on the second pass too, which is what makes it idempotent.
    pub async fn shutdown(&self) {
        self.session.dead.store(true, Ordering::SeqCst);
        self.session.pending.lock().await.clear();
        // Closing stdin is the protocol's shutdown signal, and it has to happen *before* the
        // wait: the child's `stdin` handle lives in the session (the reader task still holds the
        // session), so a well-behaved server would otherwise never see EOF and every shutdown
        // would spend the whole grace period waiting for an exit that cannot come.
        let out = self.session.out.lock().await.take();
        drop(out);
        let mut proc = self.child.lock().await.take();
        if let Some(child) = proc.as_mut() {
            // A well-behaved server exits on EOF, which is what the grace period is for.
            // `kill_on_drop` would otherwise race that wait, so the kill is explicit.
            if tokio::time::timeout(SHUTDOWN_GRACE, child.wait()).await.is_err() {
                let _ = child.start_kill();
                let _ = child.wait().await;
            }
        }
        drop(proc);
        if let Some(task) = self.reader.lock().await.take() {
            task.abort();
        }
        if let Some(task) = self.stderr_task.lock().await.take() {
            task.abort();
        }
        // Dropped last: closing the job object kills anything the server left behind.
        let _tree = self.tree.lock().await.take();
    }
}

/// Route responses to their waiters and answer server-initiated requests.
///
/// Owns the read half for the life of the process. Exits on EOF, on an I/O error, or on a frame
/// over the cap, and in every case fails all pending waiters first.
async fn read_loop<R, W>(mut reader: BufReader<R>, session: Arc<Session<W>>)
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send,
{
    let mut frame = Vec::with_capacity(8 * 1024);
    loop {
        frame.clear();
        match read_frame(&mut reader, &mut frame).await {
            Ok(0) | Err(()) => {
                // EOF, a torn pipe, or a hostile megaframe: all mean "no more answers", so
                // every waiter is failed rather than left to sit out its own deadline.
                session.fail_all().await;
                return;
            }
            Ok(_) => {}
        }
        let Ok(message) = serde_json::from_slice::<Value>(&frame) else {
            tracing::debug!("mcp: ignoring a stdout line that is not JSON ({} bytes)", frame.len());
            continue;
        };
        for msg in split_payload(&message) {
            match msg {
                Message::Response { id, result } => {
                    let Some(id) = id.as_request_id() else { continue };
                    // Unknown or duplicate ids are simply not ours; a duplicate never resolves
                    // twice because the entry is removed on the first match.
                    let Some(waiter) = session.pending.lock().await.remove(&id) else { continue };
                    let _ = match result {
                        Ok(value) => waiter.send(Ok(value)),
                        Err(err) => waiter.send(Err(McpError::Protocol(format!(
                            "the server rejected a request ({}): {}",
                            err.code, err.message
                        )))),
                    };
                }
                Message::ServerRequest { id, method, .. } => {
                    // We declared no capability that could produce one, so a conforming server
                    // sends none. Answering is still mandatory: a server that asked blocks
                    // until somebody replies, and refusing loudly beats deadlocking.
                    tracing::debug!("mcp: refusing server request `{method}`");
                    let reply = error_reply(&id, METHOD_NOT_FOUND, "method not supported by this client");
                    let _ = tokio::time::timeout(Duration::from_secs(2), session.write(&reply)).await;
                }
                // Notifications: `tools/list_changed` and friends are read at the next discovery
                // pass, not mid-call. Ignoring them here keeps the tool list stable per turn.
                Message::Notification { .. } => {}
                Message::Unusable(why) => tracing::debug!("mcp: ignoring an unusable message ({why})"),
            }
        }
    }
}

/// Read one newline-terminated frame into `buf`; `Ok(0)` means EOF.
///
/// The cap is checked before every `extend`, so the buffer never exceeds [`MAX_FRAME_BYTES`].
/// The returned value is the number of bytes in the frame, including its newline.
async fn read_frame<R>(reader: &mut R, buf: &mut Vec<u8>) -> Result<usize, ()>
where
    R: AsyncBufReadExt + Unpin,
{
    read_frame_capped(reader, buf, MAX_FRAME_BYTES).await
}

/// [`read_frame`] with an explicit cap, so the overflow path is testable without building a
/// four-megabyte pipe.
async fn read_frame_capped<R>(reader: &mut R, buf: &mut Vec<u8>, cap: usize) -> Result<usize, ()>
where
    R: AsyncBufReadExt + Unpin,
{
    loop {
        let available = reader.fill_buf().await.map_err(|_| ())?;
        if available.is_empty() {
            return Ok(buf.len());
        }
        let take = match available.iter().position(|b| *b == b'\n') {
            Some(index) => index + 1,
            None => available.len(),
        };
        if buf.len().saturating_add(take) > cap {
            return Err(());
        }
        let terminated = available[take - 1] == b'\n';
        buf.extend_from_slice(&available[..take]);
        reader.consume(take);
        if terminated {
            return Ok(buf.len());
        }
    }
}

/// Keep the last [`STDERR_TAIL_BYTES`] of stderr, discarding the rest.
///
/// Draining is mandatory and endless: the pipe must never fill, and the server's complaints are
/// only worth keeping as a tail for the error shown when it dies.
async fn drain_stderr<R: AsyncRead + Unpin>(mut pipe: R, tail: Arc<Mutex<String>>) {
    let mut chunk = vec![0u8; STDERR_CHUNK_BYTES];
    loop {
        match pipe.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                let text = String::from_utf8_lossy(&chunk[..n]).into_owned();
                let mut guard = tail.lock().await;
                guard.push_str(&text);
                if guard.len() > STDERR_TAIL_BYTES {
                    let excess = guard.len() - STDERR_TAIL_BYTES;
                    // `excess` is a byte count, so it must be advanced to a character boundary
                    // rather than handed to a character-indexed iterator: the previous form
                    // counted characters, found no such index in a multibyte tail, and fell
                    // through to clearing the whole buffer — losing the dying server's last
                    // words in exactly the case they matter.
                    let mut cut = excess.min(guard.len());
                    while cut < guard.len() && !guard.is_char_boundary(cut) {
                        cut += 1;
                    }
                    guard.drain(..cut);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Process-tree guard
// ---------------------------------------------------------------------------
//
// The shell tool has an equivalent private to `tools::proc_tree`; it is not reachable from
// here and this module owns its files only, so the guard is duplicated. Same contract: the
// guard lives as long as the child, and dropping it kills anything the server spawned.

/// Keeps a child's whole process tree killable on shutdown.
#[cfg(windows)]
pub struct TreeGuard(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
// SAFETY: a job object handle is a kernel handle that any thread may use.
unsafe impl Send for TreeGuard {}
#[cfg(windows)]
// SAFETY: see the `Send` note above.
unsafe impl Sync for TreeGuard {}

#[cfg(windows)]
impl TreeGuard {
    /// Put `child` and everything it spawns afterwards into a job that dies with the guard.
    ///
    /// Returns `None` when the job cannot be created or assigned — the process is still
    /// killable directly, grandchildren just are not covered. Never fatal: a server that
    /// cannot be job-wrapped is much better than no server.
    pub fn attach(child: &tokio::process::Child) -> Option<Self> {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
            SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };
        let raw = child.raw_handle()?;
        // SAFETY: plain Win32 calls on handles this process owns; every failure closes the job
        // before returning `None`.
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return None;
            }
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let configured = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const core::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if configured == 0 || AssignProcessToJobObject(job, raw as _) == 0 {
                tracing::debug!("mcp: could not put the server in a job object; grandchildren may outlive it");
                CloseHandle(job);
                return None;
            }
            Some(Self(job))
        }
    }
}

#[cfg(windows)]
impl Drop for TreeGuard {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;
        // SAFETY: `self.0` is a valid job handle until this call, which runs once.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

/// Process-group id, so the whole tree is addressable on shutdown.
///
/// The server is started with `process_group(0)`, which makes its pid the group id. Dropping
/// the guard sends `SIGKILL` to that group, which is the only unix path that reaches
/// grandchildren without a libc binding in this crate's dependency set — `tokio`'s
/// `start_kill` reaches only the direct child.
#[cfg(unix)]
pub struct TreeGuard(u32);

#[cfg(unix)]
impl TreeGuard {
    /// Remember the child's pid, which doubles as the process-group id.
    pub fn attach(child: &tokio::process::Child) -> Option<Self> {
        child.id()
    }
}

#[cfg(unix)]
impl Drop for TreeGuard {
    fn drop(&mut self) {
        // `kill` is a shell builtin as well as `/bin/kill`, so it resolves without a PATH
        // search for the tool itself; a group kill of a group that already exited is a no-op
        // error, which is why the status is ignored.
        let _ = std::process::Command::new("kill")
            .arg("-KILL")
            .arg(format!("-{}", self.0))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

/// No-op guard on platforms with neither job objects nor process groups.
#[cfg(not(any(windows, unix)))]
pub struct TreeGuard;

#[cfg(not(any(windows, unix)))]
impl TreeGuard {
    /// Nothing to attach.
    pub fn attach(_child: &tokio::process::Child) -> Option<Self> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::DuplexStream;

    /// The write half of the in-memory duplex, which is what a real session holds too.
    type TestSession = Session<tokio::io::WriteHalf<DuplexStream>>;

    /// A session wired to an in-memory duplex: whatever the test writes to `server` is what
    /// the client reads, and vice versa. No process, no network.
    struct Harness {
        session: Arc<TestSession>,
        server: DuplexStream,
        reader: tokio::task::JoinHandle<()>,
    }

    fn harness(capacity: usize) -> Harness {
        let (client_side, server_side) = tokio::io::duplex(capacity);
        let (read_half, write_half) = tokio::io::split(client_side);
        let session = Arc::new(Session {
            out: Mutex::new(Some(write_half)),
            pending: Arc::new(Mutex::new(HashMap::new())),
            ids: IdGen::new(),
            dead: AtomicBool::new(false),
            stderr: Arc::new(Mutex::new(String::new())),
            cancel_notifications: true,
        });
        let reader = tokio::spawn(read_loop(BufReader::new(read_half), session.clone()));
        Harness { session, server: server_side, reader }
    }

    impl Harness {
        /// Read one line the client wrote, as JSON.
        ///
        /// The reader is rebuilt per call because the harness only owns one duplex end. That
        /// end is written by the client and read here, so there is nothing to lose by it: no
        /// other consumer reads this half.
        async fn next_request(&mut self) -> Value {
            let mut line = Vec::new();
            loop {
                let mut byte = [0u8; 1];
                let n = self.server.read(&mut byte).await.expect("read");
                if n == 0 {
                    panic!("the client closed the pipe instead of writing a frame");
                }
                if byte[0] == b'\n' {
                    break;
                }
                line.push(byte[0]);
            }
            serde_json::from_slice(&line).expect("json")
        }

        /// Answer the client's most recent request id with `result`.
        async fn answer(&mut self, id: &Value, result: Value) {
            let frame = json!({ "jsonrpc": "2.0", "id": id, "result": result });
            self.send(&frame).await;
        }

        async fn send(&mut self, frame: &Value) {
            let mut bytes = serde_json::to_vec(frame).expect("serialize");
            bytes.push(b'\n');
            self.server.write_all(&bytes).await.expect("write");
            self.server.flush().await.expect("flush");
        }
    }

    #[tokio::test]
    async fn a_response_resolves_the_matching_request() {
        let mut h = harness(8 * 1024);
        let cancel = CancellationToken::new();
        let caller = {
            let session = h.session.clone();
            tokio::spawn(async move { session.request("ping", json!({}), Duration::from_secs(5), &cancel).await })
        };
        let frame = h.next_request().await;
        assert_eq!(frame["method"], "ping");
        h.answer(&frame["id"], json!({ "ok": true })).await;
        let result = caller.await.expect("task").expect("response");
        assert_eq!(result["ok"], true);
    }

    #[tokio::test]
    async fn an_unmatched_id_is_ignored_and_the_real_answer_still_arrives() {
        let mut h = harness(8 * 1024);
        let cancel = CancellationToken::new();
        let caller = {
            let session = h.session.clone();
            tokio::spawn(async move { session.request("ping", json!({}), Duration::from_secs(5), &cancel).await })
        };
        let frame = h.next_request().await;
        // A late reply to a request we already gave up on, then a batch containing ours.
        h.answer(&json!(9999), json!({ "stray": true })).await;
        h.send(&json!([
            { "jsonrpc": "2.0", "method": "notifications/message" },
            { "jsonrpc": "2.0", "id": frame["id"], "result": { "ok": 1 } }
        ]))
        .await;
        assert_eq!(caller.await.expect("task").expect("response")["ok"], 1);
    }

    #[tokio::test]
    async fn a_server_request_is_answered_with_method_not_found() {
        let mut h = harness(8 * 1024);
        h.send(&json!({
            "jsonrpc": "2.0",
            "id": 42,
            "method": "sampling/createMessage",
            "params": { "messages": [] }
        }))
        .await;
        let reply = h.next_request().await;
        assert_eq!(reply["id"], 42);
        assert_eq!(reply["error"]["code"], METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn eof_fails_every_waiter() {
        let mut h = harness(8 * 1024);
        let cancel = CancellationToken::new();
        let caller = {
            let session = h.session.clone();
            tokio::spawn(async move { session.request("ping", json!({}), Duration::from_secs(30), &cancel).await })
        };
        let _frame = h.next_request().await;
        drop(h.server);
        let err = caller.await.expect("task").expect_err("must fail fast, not time out");
        assert!(matches!(err, McpError::Dead(_)), "{err:?}");
        assert!(h.session.dead.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn a_frame_over_the_cap_kills_the_stream_without_allocating_it() {
        // A four-megabyte pipe would make the test an exercise in patience, so the cap is
        // exercised directly: the same code path, a small limit, a line that never ends.
        let mut reader = BufReader::new(std::io::Cursor::new(vec![b'x'; 4096]));
        let mut buf = Vec::new();
        assert_eq!(read_frame_capped(&mut reader, &mut buf, 1024).await, Err(()));
        assert_eq!(buf.len(), 0, "an over-cap frame must not be buffered first");
        // A line that fits still reads normally under a cap.
        let mut reader = BufReader::new(std::io::Cursor::new(b"{\"a\":1}\n".to_vec()));
        assert_eq!(read_frame_capped(&mut reader, &mut buf, 1024).await, Ok(8));
    }

    #[tokio::test]
    async fn frames_split_on_newlines_and_keep_partial_lines() {
        let (mut tx, rx) = tokio::io::duplex(1024);
        let mut reader = BufReader::new(rx);
        tx.write_all(b"{\"a\":1}\n{\"b\":").await.expect("write");
        let mut buf = Vec::new();
        assert_eq!(read_frame(&mut reader, &mut buf).await, Ok(8));
        assert_eq!(buf, b"{\"a\":1}\n");
        buf.clear();
        // Unterminated remainder: the read returns only at EOF, as a final frame.
        drop(tx);
        assert_eq!(read_frame(&mut reader, &mut buf).await, Ok(5));
        assert_eq!(buf, b"{\"b\":");
    }

    #[tokio::test]
    async fn a_full_pending_map_refuses_new_requests_instead_of_growing() {
        let h = harness(8 * 1024);
        {
            let mut pending = h.session.pending.lock().await;
            for id in 0..MAX_PENDING as u64 {
                let (tx, _rx) = oneshot::channel();
                pending.insert(id, tx);
            }
        }
        let err = h
            .session
            .request("ping", json!({}), Duration::from_secs(5), &CancellationToken::new())
            .await
            .expect_err("the map is full");
        assert!(matches!(err, McpError::Protocol(_)), "{err:?}");
        h.reader.abort();
    }

    #[tokio::test]
    async fn a_cancelled_call_returns_cancelled_and_tells_the_server() {
        let mut h = harness(8 * 1024);
        let cancel = CancellationToken::new();
        let caller = {
            let session = h.session.clone();
            let token = cancel.clone();
            tokio::spawn(async move { session.request("slow", json!({}), Duration::from_secs(30), &token).await })
        };
        let frame = h.next_request().await;
        cancel.cancel();
        let err = caller.await.expect("task").expect_err("cancelled");
        assert!(matches!(err, McpError::Cancelled), "{err:?}");
        // The follow-up notification must reach the server with the right id.
        let note = h.next_request().await;
        assert_eq!(note["method"], "notifications/cancelled");
        assert_eq!(note["params"]["requestId"], frame["id"]);
    }

    #[tokio::test]
    async fn a_timeout_returns_timeout_and_drops_the_waiter() {
        let mut h = harness(8 * 1024);
        let err = h
            .session
            .request("slow", json!({}), Duration::from_millis(20), &CancellationToken::new())
            .await
            .expect_err("timed out");
        assert!(matches!(err, McpError::Timeout), "{err:?}");
        let _request = h.next_request().await;
        let _cancel = h.next_request().await;
        assert!(h.session.pending.lock().await.is_empty(), "the entry must not leak");
        h.reader.abort();
    }

    #[tokio::test]
    async fn the_stderr_tail_keeps_only_the_end() {
        let tail = Arc::new(Mutex::new(String::new()));
        let noise = vec![b'.'; STDERR_TAIL_BYTES + 4096];
        let mut cursor = std::io::Cursor::new(noise);
        drain_stderr(&mut cursor, tail.clone()).await;
        let kept = tail.lock().await.clone();
        assert!(kept.len() <= STDERR_TAIL_BYTES, "{} bytes", kept.len());
        assert!(kept.len() >= STDERR_TAIL_BYTES - STDERR_CHUNK_BYTES);
    }

    #[tokio::test]
    async fn a_death_error_carries_the_stderr_tail() {
        let h = harness(8 * 1024);
        assert!(h.session.death_error().to_string().contains("stopped responding"));
        *h.session.stderr.lock().await = "boom: missing dependency\n".to_owned();
        let text = h.session.death_error().to_string();
        assert!(text.contains("boom: missing dependency"), "{text}");
        h.reader.abort();
    }

    #[tokio::test]
    async fn an_error_response_surfaces_as_a_protocol_error() {
        let mut h = harness(8 * 1024);
        let cancel = CancellationToken::new();
        let caller = {
            let session = h.session.clone();
            tokio::spawn(async move { session.request("tools/call", json!({}), Duration::from_secs(5), &cancel).await })
        };
        let frame = h.next_request().await;
        h.send(&json!({
            "jsonrpc": "2.0",
            "id": frame["id"],
            "error": { "code": -32602, "message": "missing argument: path" }
        }))
        .await;
        let err = caller.await.expect("task").expect_err("error response");
        let text = err.to_string();
        assert!(text.contains("missing argument: path"), "{text}");
        assert!(text.contains("-32602"), "{text}");
    }

    #[tokio::test]
    async fn the_stderr_tail_survives_multibyte_output() {
        // Byte count and character count diverge by 3x here, which is what made the old
        // character-indexed cut fall through to clearing the whole tail.
        let tail = Arc::new(Mutex::new(String::new()));
        let noise = "ß".repeat(STDERR_TAIL_BYTES);
        let mut cursor = std::io::Cursor::new(noise.into_bytes());
        drain_stderr(&mut cursor, tail.clone()).await;
        let kept = tail.lock().await.clone();
        assert!(!kept.is_empty(), "a multibyte tail must not be discarded wholesale");
        assert!(kept.len() <= STDERR_TAIL_BYTES, "{} bytes", kept.len());
        assert!(kept.len() >= STDERR_TAIL_BYTES - STDERR_CHUNK_BYTES * 2, "{} bytes", kept.len());
    }

    #[tokio::test]
    async fn a_write_after_close_reports_the_input_as_closed() {
        let h = harness(8 * 1024);
        *h.session.out.lock().await = None;
        let err = h.session.notify("notifications/initialized", json!({})).await.expect_err("closed");
        assert!(matches!(err, McpError::Dead(_)), "{err:?}");
        assert!(err.to_string().contains("input is closed"), "{err}");
        h.reader.abort();
    }

    /// A real child process, so the spawn flags, the pipe wiring, the EOF shutdown and the
    /// handshake are all exercised together rather than only through the duplex harness.
    ///
    /// Windows-only and PowerShell-based because the rules forbid an external process on the
    /// success path by default and forbid assuming a portable shell; the answers are built by
    /// string concatenation so the escaping stays readable.
    #[cfg(windows)]
    #[tokio::test]
    async fn a_real_child_completes_the_handshake_and_exits_on_close() {
        let script = r#"
$ErrorActionPreference = 'Stop'
while ($true) {
  $line = [Console]::In.ReadLine()
  if ($null -eq $line) { break }
  if ($line -notmatch '"id"\s*:\s*(\d+)') { continue }
  $id = $Matches[1]
  if ($line -match 'tools/list') {
    '[{"jsonrpc":"2.0","id":' + $id + ',"result":{"tools":[{"name":"ping","description":"Ping","inputSchema":{"type":"object","properties":{}}}]}}]'
  } else {
    '[{"jsonrpc":"2.0","id":' + $id + ',"result":{"protocolVersion":"2025-03-26","capabilities":{"tools":{}},"serverInfo":{"name":"echo","version":"0"}}}]'
  }
}
"#;
        let args = vec![
            "-NoProfile".to_owned(),
            "-NonInteractive".to_owned(),
            "-Command".to_owned(),
            script.to_owned(),
        ];
        let cancel = CancellationToken::new();
        let (transport, tools) = StdioTransport::connect("echo", "powershell.exe", &args, &[], None, Timeouts::default(), &cancel)
            .await
            .expect("a real child should connect");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "ping");
        // The child exits on stdin EOF, so shutdown must finish well inside the grace period.
        // The bug this guards against is a stdin handle that is never closed: the child then
        // keeps waiting for input and every shutdown pays the full two seconds before a kill.
        let started = std::time::Instant::now();
        transport.shutdown().await;
        let elapsed = started.elapsed();
        assert!(elapsed < Duration::from_millis(1500), "shutdown took {elapsed:?}; stdin was not closed");
        assert!(transport.is_dead());
    }
}
