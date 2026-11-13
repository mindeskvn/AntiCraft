//! Tracing setup: daily file at `{data_dir}/logs/native.log`, with token redaction applied to
//! every formatted line before it reaches disk.

use std::io::Write;
use std::path::Path;
use std::sync::OnceLock;

use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::EnvFilter;

/// Patterns for secrets that must never reach the log: Google OAuth tokens and secrets, the
/// provider API keys (`sk-...` and `sk-ant-...` keys, Google `AIza...` keys, an `x-api-key`
/// header, a `key=` query parameter), and any bare JWT — a ChatGPT access or ID token is one, and
/// it carries no prefix the other arms would catch. A backstop only: provider code keeps keys out
/// of its messages in the first place.
fn secret_patterns() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(
            r#"(?i)(\beyJ[A-Za-z0-9_\-]{8,}\.[A-Za-z0-9_\-]{8,}\.[A-Za-z0-9_\-]*|ya29\.[A-Za-z0-9._\-]+|1//[A-Za-z0-9._\-]+|GOCSPX-[A-Za-z0-9_\-]+|bearer\s+[A-Za-z0-9._\-]+|\bsk-[A-Za-z0-9_\-]{8,}|\bAIza[A-Za-z0-9_\-]{8,}|x-api-key["']?\s*[:=]\s*["']?[^\s"'&,;}]+|[?&]key=[^\s"'&,;}]+|(access_token|refresh_token|id_token|client_secret|code)["']?\s*[:=]\s*["']?[^\s"'&,}]+)"#,
        )
        .expect("valid redaction regex")
    })
}

/// Replace every secret-looking substring with `<redacted>`.
pub fn redact(input: &str) -> String {
    secret_patterns().replace_all(input, "<redacted>").into_owned()
}

/// `MakeWriter` wrapper that redacts each write.
#[derive(Clone)]
pub struct RedactingMakeWriter<M> {
    inner: M,
}

impl<M> RedactingMakeWriter<M> {
    pub fn new(inner: M) -> Self {
        Self { inner }
    }
}

pub struct RedactingWriter<W: Write> {
    inner: W,
}

impl<W: Write> Write for RedactingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let text = String::from_utf8_lossy(buf);
        self.inner.write_all(redact(&text).as_bytes())?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

impl<'a, M: MakeWriter<'a>> MakeWriter<'a> for RedactingMakeWriter<M> {
    type Writer = RedactingWriter<M::Writer>;
    fn make_writer(&'a self) -> Self::Writer {
        RedactingWriter { inner: self.inner.make_writer() }
    }
}

/// Install the global subscriber. Keep the returned guard alive until exit to flush logs.
/// Level comes from `ANTICRAFT_LOG` (default `info`).
pub fn init(data_dir: &Path) -> anyhow::Result<tracing_appender::non_blocking::WorkerGuard> {
    let dir = data_dir.join("logs");
    std::fs::create_dir_all(&dir)?;
    let appender = tracing_appender::rolling::never(&dir, "native.log");
    let (nb, guard) = tracing_appender::non_blocking(appender);
    let filter = EnvFilter::try_from_env("ANTICRAFT_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_writer(RedactingMakeWriter::new(nb))
        .try_init()
        .map_err(|e| anyhow::anyhow!("logging init failed: {e}"))?;
    Ok(guard)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_tokens() {
        let s = redact("auth Bearer ya29.abc refresh_token=1//xyz secret GOCSPX-q_1 \"access_token\":\"zzz\"");
        for leaked in ["ya29.abc", "1//xyz", "GOCSPX-q_1", "zzz"] {
            assert!(!s.contains(leaked), "{leaked} leaked in {s}");
        }
        assert_eq!(redact("plain message"), "plain message");
    }

    #[test]
    fn redacts_provider_api_keys() {
        let s = redact("relay: upstream overloaded for key sk-ant-api03-AbC_12-xyz; x-api-key: hdr-secret-1; GET /v1beta/models/m?alt=sse&key=AIzaSyQ-secret_2 sk-proj-Q1w2E3r4T5y6U7i8");
        for leaked in ["sk-ant-api03-AbC_12-xyz", "hdr-secret-1", "AIzaSyQ-secret_2", "sk-proj-Q1w2E3r4T5y6U7i8"] {
            assert!(!s.contains(leaked));
        }
        // Ordinary words that merely contain the prefixes are left alone.
        assert_eq!(redact("task-abcdefghijklmnopqrstuvwxyz keyboard monkey=1"), "task-abcdefghijklmnopqrstuvwxyz keyboard monkey=1");
    }

    #[test]
    fn redacts_a_bare_jwt() {
        // A ChatGPT access token is a JWT with no `Bearer` in front of it once it is out of a
        // header — in a debug dump of a record, say. Unsigned ones (empty third segment) too.
        let jwt = "eyJhbGciOiJSUzI1NiJ9.eyJzdWIiOiJ1c2VyLTEiLCJleHAiOjF9.c2lnbmF0dXJlLWJ5dGVz";
        let s = redact(&format!("refresh failed for token {jwt} (401)"));
        assert!(!s.contains("eyJzdWIiOiJ1c2VyLTEiLCJleHAiOjF9"), "{s}");
        assert!(s.contains("<redacted>") && s.ends_with("(401)"), "{s}");
        let unsigned = "eyJhbGciOiJub25lIn0.eyJlbWFpbCI6Im1lQGV4YW1wbGUuY29tIn0.";
        assert!(!redact(unsigned).contains("eyJlbWFpbCI6Im1lQGV4YW1wbGUuY29tIn0"));
        // A dotted name that merely starts like one is left alone.
        assert_eq!(redact("see eyJump.config for details"), "see eyJump.config for details");
    }

    #[test]
    fn writer_redacts_before_disk() {
        let mut w = RedactingWriter { inner: Vec::new() };
        let line = b"token refresh_token=1//secretvalue done
";
        assert_eq!(w.write(line).unwrap(), line.len());
        let out = String::from_utf8(w.inner).unwrap();
        assert!(!out.contains("secretvalue"));
        assert!(out.contains("<redacted>") && out.ends_with("done
"));
    }
}
