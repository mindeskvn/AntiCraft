//! Single instance (SPEC §2 step 1, §2 step 7).
//!
//! A named mutex keyed by the userData path decides primary vs secondary. The
//! primary serves a local named pipe; a secondary writes its argv as a JSON
//! array to the pipe and exits. Pipes reject remote clients.

use std::io;
use std::path::Path;

/// Result of [`acquire`].
pub enum Instance {
    Primary(InstanceLock),
    Secondary { pipe_name: String },
}

/// Holds the mutex for the process lifetime.
pub struct InstanceLock {
    #[cfg_attr(not(windows), allow(dead_code))]
    handle: usize,
    pub pipe_name: String,
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        #[cfg(windows)]
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.handle as _);
        }
    }
}

/// Canonical text form of the userData path used for the instance key:
/// resolved (`canonicalize` when it exists), `\\?\` prefix removed, `/`
/// turned into `\`, trailing separators trimmed, lowercased.
pub fn normalize_user_data(user_data: &Path) -> String {
    let resolved = std::fs::canonicalize(user_data)
        .or_else(|_| std::path::absolute(user_data))
        .unwrap_or_else(|_| user_data.to_path_buf());
    let mut s = resolved.to_string_lossy().replace('/', "\\");
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        s = format!(r"\\{rest}");
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        s = rest.to_string();
    }
    while s.len() > 3 && s.ends_with('\\') {
        s.pop();
    }
    s.to_lowercase()
}

/// Stable FNV-1a hash of the normalized userData path, as 16 hex chars.
pub fn instance_key(user_data: &Path) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in normalize_user_data(user_data).bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

pub fn pipe_name_for(key: &str) -> String {
    format!(r"\\.\pipe\anticraft-{key}")
}

/// Acquire the per-userData lock.
#[cfg(windows)]
pub fn acquire(user_data: &Path) -> io::Result<Instance> {
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows_sys::Win32::System::Threading::CreateMutexW;
    let key = instance_key(user_data);
    let name: Vec<u16> = format!("Local\\AntiCraft-{key}").encode_utf16().chain(Some(0)).collect();
    let pipe_name = pipe_name_for(&key);
    unsafe {
        let h = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
        if h.is_null() {
            return Err(io::Error::last_os_error());
        }
        if GetLastError() == ERROR_ALREADY_EXISTS {
            windows_sys::Win32::Foundation::CloseHandle(h);
            return Ok(Instance::Secondary { pipe_name });
        }
        Ok(Instance::Primary(InstanceLock { handle: h as usize, pipe_name }))
    }
}

#[cfg(not(windows))]
pub fn acquire(user_data: &Path) -> io::Result<Instance> {
    let key = instance_key(user_data);
    Ok(Instance::Primary(InstanceLock { handle: 0, pipe_name: pipe_name_for(&key) }))
}

/// Secondary side: send argv to the primary (retries while the pipe is busy).
pub fn forward_argv(pipe_name: &str, argv: &[String]) -> io::Result<()> {
    use std::io::Write;
    let payload = serde_json::to_vec(argv).map_err(io::Error::other)?;
    let mut last = None;
    for _ in 0..20 {
        match std::fs::OpenOptions::new().write(true).open(pipe_name) {
            Ok(mut f) => {
                f.write_all(&payload)?;
                return f.flush();
            }
            Err(e) => {
                last = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }
    Err(last.unwrap_or_else(|| io::Error::other("pipe unavailable")))
}

/// Primary side: accept connections forever, calling `on_argv` per message.
#[cfg(windows)]
pub async fn serve<F>(pipe_name: String, on_argv: F) -> io::Result<()>
where
    F: Fn(Vec<String>) + Send + Sync + 'static,
{
    use std::sync::Arc;
    use tokio::io::AsyncReadExt;
    use tokio::net::windows::named_pipe::ServerOptions;
    let on_argv = Arc::new(on_argv);
    let mut server = ServerOptions::new()
        .first_pipe_instance(true)
        .reject_remote_clients(true)
        .create(&pipe_name)?;
    let mut create_failures = 0u32;
    loop {
        if let Err(e) = server.connect().await {
            // One bad client (e.g. connect-then-drop) must not end the loop.
            tracing::warn!("second-instance pipe connect failed: {e}");
        } else {
            let mut conn = server;
            let cb = on_argv.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut limited = (&mut conn).take(MAX_MESSAGE_BYTES);
                let read = limited.read_to_end(&mut buf);
                match tokio::time::timeout(READ_TIMEOUT, read).await {
                    Ok(Ok(_)) => match serde_json::from_slice::<Vec<String>>(&buf) {
                        Ok(argv) => cb(argv),
                        Err(e) => tracing::warn!("ignoring malformed second-instance message: {e}"),
                    },
                    Ok(Err(e)) => tracing::warn!("second-instance read failed: {e}"),
                    Err(_) => tracing::warn!("second-instance client timed out"),
                }
            });
        }
        server = loop {
            match ServerOptions::new().reject_remote_clients(true).create(&pipe_name) {
                Ok(s) => {
                    create_failures = 0;
                    break s;
                }
                Err(e) => {
                    create_failures += 1;
                    if create_failures >= MAX_CREATE_FAILURES {
                        return Err(e);
                    }
                    tracing::warn!("recreating second-instance pipe failed ({create_failures}): {e}");
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
            }
        };
    }
}

/// Per-message cap and read deadline for second-instance clients.
#[cfg(windows)]
const MAX_MESSAGE_BYTES: u64 = 64 * 1024;
#[cfg(windows)]
const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
#[cfg(windows)]
const MAX_CREATE_FAILURES: u32 = 10;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn key_is_case_insensitive() {
        assert_eq!(instance_key(Path::new(r"C:\A")), instance_key(Path::new(r"c:\a")));
        assert_ne!(instance_key(Path::new(r"C:\A")), instance_key(Path::new(r"C:\B")));
    }

    #[test]
    fn key_ignores_separator_forms() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().to_path_buf();
        let slash = std::path::PathBuf::from(p.to_string_lossy().replace('\\', "/"));
        let trailing = std::path::PathBuf::from(format!("{}\\", p.display()));
        assert_eq!(instance_key(&p), instance_key(&slash));
        assert_eq!(instance_key(&p), instance_key(&trailing));
        assert!(!normalize_user_data(&p).starts_with(r"\\?\"));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn primary_secondary_and_forward() {
        let d = tempfile::tempdir().unwrap();
        let Instance::Primary(lock) = acquire(d.path()).unwrap() else { panic!("expected primary") };
        let Instance::Secondary { pipe_name } = acquire(d.path()).unwrap() else { panic!("expected secondary") };
        assert_eq!(pipe_name, lock.pipe_name);
        let got = Arc::new(Mutex::new(None));
        let g2 = got.clone();
        let name = lock.pipe_name.clone();
        tokio::spawn(serve(name, move |a| *g2.lock().unwrap() = Some(a)));
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let pn = pipe_name.clone();
        tokio::task::spawn_blocking(move || forward_argv(&pn, &["x".into(), "antigravity://y".into()]))
            .await
            .unwrap()
            .unwrap();
        for _ in 0..50 {
            if got.lock().unwrap().is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(got.lock().unwrap().clone().unwrap()[1], "antigravity://y");
        drop(lock);
    }
}
