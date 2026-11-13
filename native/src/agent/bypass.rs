//! Bypass Mode: the first requests of each message go to a model of the user's choosing.
//!
//! A message may begin on a different provider from the one the conversation normally uses —
//! the reason a person wants it is their own, and the app only routes: the tools, history,
//! approvals and everything else are the ones a turn always has. After the configured number of
//! requests the turn continues on the normal model, which is what makes the mode "the first N
//! requests" rather than a second backend for the whole conversation.
//!
//! The instruction files are read once when the agent is built, so a turn cannot change them
//! mid-flight. Their text rides in the system instruction of the bypass requests only, never in
//! the history: a model reading them is how the mode is steered, not something the transcript
//! should carry into later turns on the normal model.

use std::path::PathBuf;
use std::sync::Arc;

use crate::api::ModelBackend;
use crate::settings::{MAX_BYPASS_FILES, MAX_BYPASS_FILE_BYTES};

/// One route the turn can take for its first requests.
#[derive(Clone)]
pub struct BypassRoute {
    /// The provider that answers those requests.
    pub backend: Arc<dyn ModelBackend>,
    /// The model id sent to it, as typed (a provider profile's model is never alias-resolved).
    pub model: String,
    /// What the notice calls the provider.
    pub label: String,
    /// Requests of a message that go here, counted from the message's first request.
    pub requests: u32,
    /// Text of the `.md` files, added to those requests' system instruction.
    pub instructions: String,
    /// Input window of the route's model, in tokens. The turn's history travels in its first
    /// request, so compaction must keep the history under the smaller of the two windows: the
    /// normal model's and this one.
    pub context_window: u64,
}

impl std::fmt::Debug for BypassRoute {
    /// `dyn ModelBackend` has no `Debug`, and nothing useful would come of one: the backend
    /// carries a key.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BypassRoute")
            .field("model", &self.model)
            .field("label", &self.label)
            .field("requests", &self.requests)
            .field("instructions_bytes", &self.instructions.len())
            .finish()
    }
}

impl BypassRoute {
    /// True for the `turn`-th request of a message (the first is 1).
    pub fn covers(&self, turn: u32) -> bool {
        turn <= self.requests
    }
}

/// Read the instruction files, in list order, each under a heading naming it.
///
/// A file that cannot be read, or is not UTF-8 text, is skipped rather than failing the turn: the
/// list is the user's, and a moved file must not make every message unsendable. The size cap
/// applies per file, so one huge file cannot fill every request of the mode.
pub fn load_instructions(files: &[PathBuf]) -> String {
    let mut out = String::new();
    for path in files.iter().take(MAX_BYPASS_FILES) {
        let Ok(bytes) = read_capped(path, MAX_BYPASS_FILE_BYTES) else { continue };
        let Ok(text) = String::from_utf8(bytes) else {
            tracing::debug!("bypass instruction file is not UTF-8, skipped");
            continue;
        };
        if text.trim().is_empty() {
            continue;
        }
        // The name only: the path can name a user folder the request has no use for.
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str("--- ");
        out.push_str(if name.is_empty() { "instruction" } else { &name });
        out.push_str(" ---\n");
        out.push_str(text.trim_end());
    }
    out
}

/// At most `cap` bytes, without splitting a character.
fn read_capped(path: &std::path::Path, cap: usize) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut buf = Vec::new();
    std::fs::File::open(path)?.take(cap as u64).read_to_end(&mut buf)?;
    // A byte cap can land inside a UTF-8 sequence; the text is truncated to the last whole
    // character instead of being read as invalid.
    if let Err(e) = std::str::from_utf8(&buf) {
        if e.error_len().is_none() {
            buf.truncate(e.valid_up_to());
        }
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_are_read_with_their_names_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("rules.md");
        let b = dir.path().join("tone.md");
        std::fs::write(&a, "# Be brief\n").unwrap();
        std::fs::write(&b, "Answer in Vietnamese.").unwrap();
        let text = load_instructions(&[a, b]);
        assert_eq!(text, "--- rules.md ---\n# Be brief\n\n--- tone.md ---\nAnswer in Vietnamese.");
    }

    #[test]
    fn unreadable_empty_and_binary_files_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let ok = dir.path().join("ok.md");
        std::fs::write(&ok, "kept").unwrap();
        let empty = dir.path().join("empty.md");
        std::fs::write(&empty, "   \n").unwrap();
        let binary = dir.path().join("binary.md");
        std::fs::write(&binary, [0xff, 0xfe, 0x00]).unwrap();
        let text = load_instructions(&[dir.path().join("missing.md"), empty, binary, ok]);
        assert_eq!(text, "--- ok.md ---\nkept");
        assert!(load_instructions(&[]).is_empty());
    }

    /// A file past the cap is cut at a character boundary, not read as invalid text.
    #[test]
    fn a_large_file_is_cut_to_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("big.md");
        // Two-byte characters, so a cut at the cap lands mid-character.
        let body = "ầ".repeat(MAX_BYPASS_FILE_BYTES);
        std::fs::write(&p, &body).unwrap();
        let text = load_instructions(&[p]);
        assert!(text.len() < MAX_BYPASS_FILE_BYTES + 64, "{}", text.len());
        assert!(text.starts_with("--- big.md ---\nầ"), "{}", &text[..40.min(text.len())]);
    }

    #[test]
    fn the_route_covers_the_first_requests_only() {
        let route = BypassRoute { backend: Arc::new(Silent), model: "m".into(), label: "Gateway".into(), requests: 2, instructions: String::new(), context_window: 128_000 };
        assert!(route.covers(1) && route.covers(2));
        assert!(!route.covers(3));
        let shown = format!("{route:?}");
        assert!(shown.contains("requests: 2") && !shown.contains("backend"));
    }

    struct Silent;
    impl crate::api::ModelBackend for Silent {
        fn stream_turn(
            &self,
            _m: String,
            _p: String,
            _r: crate::api::GenerateContentRequest,
            _c: tokio_util::sync::CancellationToken,
        ) -> futures::stream::BoxStream<'static, Result<crate::api::CaGenerateResponse, crate::api::CaError>> {
            use futures::StreamExt;
            futures::stream::empty().boxed()
        }
    }
}
