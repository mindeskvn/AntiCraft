//! Logging: `tracing` to `<logs>\main.log` plus stderr (filter via RUST_LOG).

use std::path::Path;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

/// Keeps the non-blocking file writer alive; drop to flush.
pub struct LogGuard(#[allow(dead_code)] Option<tracing_appender::non_blocking::WorkerGuard>);

/// Initialise global logging. Falls back to stderr only if the file cannot
/// be opened.
pub fn init(main_log: &Path) -> LogGuard {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let file = main_log.parent().and_then(|d| {
        std::fs::create_dir_all(d).ok()?;
        std::fs::OpenOptions::new().create(true).append(true).open(main_log).ok()
    });
    let stderr_layer = fmt::layer().with_writer(std::io::stderr).with_target(false);
    match file {
        Some(f) => {
            let (nb, guard) = tracing_appender::non_blocking(f);
            let file_layer = fmt::layer().with_writer(nb).with_ansi(false).with_target(false);
            let _ = tracing_subscriber::registry().with(filter).with(stderr_layer).with(file_layer).try_init();
            LogGuard(Some(guard))
        }
        None => {
            let _ = tracing_subscriber::registry().with(filter).with(stderr_layer).try_init();
            LogGuard(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_main_log() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs").join("main.log");
        let guard = init(&path);
        assert!(guard.0.is_some());
        tracing::info!("anticraft-log-test-marker");
        drop(guard); // flushes the non-blocking writer
        assert!(path.exists());
        // Another test may have installed the global subscriber first; only
        // assert the content when ours won.
        let body = std::fs::read_to_string(&path).unwrap();
        if !body.is_empty() {
            assert!(body.contains("anticraft-log-test-marker"));
        }
    }
}
