//! Server-sent-events parser for `streamGenerateContent?alt=sse` (PROTOCOL.md 2.12).
//!
//! Byte-level incremental parser: feed arbitrary chunks, get complete `data:` payloads.
//! Unlike gemini-cli, a leftover buffer is flushed at EOF.

use futures::{Stream, StreamExt};

use super::types::CaGenerateResponse;
use super::CaError;

/// Largest single line or event accepted (bytes).
pub const MAX_EVENT_BYTES: usize = 16 * 1024 * 1024;

/// Incremental SSE event assembler.
#[derive(Debug)]
pub struct SseParser {
    /// Bytes of the current, not yet terminated line.
    line: Vec<u8>,
    /// `data:` payloads of the current event.
    data: Vec<String>,
    data_bytes: usize,
    limit: usize,
}

impl Default for SseParser {
    fn default() -> Self {
        Self::with_limit(MAX_EVENT_BYTES)
    }
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_limit(limit: usize) -> Self {
        Self { line: Vec::new(), data: Vec::new(), data_bytes: 0, limit }
    }

    /// Feed bytes; returns every event payload completed by them (data lines joined with `\n`).
    /// Fails when a line or an event exceeds the size limit.
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<String>, CaError> {
        let mut out = Vec::new();
        for &b in bytes {
            if b == b'\n' {
                let mut line = std::mem::take(&mut self.line);
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                self.handle_line(&String::from_utf8_lossy(&line), &mut out);
                if self.data_bytes > self.limit {
                    return Err(CaError::Decode("SSE event too large".into()));
                }
            } else {
                if self.line.len() >= self.limit {
                    return Err(CaError::Decode("SSE line too long".into()));
                }
                self.line.push(b);
            }
        }
        Ok(out)
    }

    /// Flush at EOF: a pending line and any buffered data become a final event.
    pub fn finish(&mut self) -> Option<String> {
        let mut out = Vec::new();
        if !self.line.is_empty() {
            let line = std::mem::take(&mut self.line);
            let s = String::from_utf8_lossy(&line).trim_end_matches('\r').to_owned();
            self.handle_line(&s, &mut out);
        }
        self.handle_line("", &mut out);
        out.pop()
    }

    fn handle_line(&mut self, line: &str, out: &mut Vec<String>) {
        if line.is_empty() {
            if !self.data.is_empty() {
                out.push(std::mem::take(&mut self.data).join("\n"));
            }
            self.data_bytes = 0;
        } else if let Some(rest) = line.strip_prefix("data:") {
            let d = rest.trim().to_owned();
            self.data_bytes += d.len() + 1;
            self.data.push(d);
        }
        // `:` comments, `id:`, `event:`, `retry:` are ignored.
    }
}

/// Parse one event payload. Malformed JSON yields `None` and a content-free log line.
pub fn parse_event(payload: &str) -> Option<CaGenerateResponse> {
    match serde_json::from_str(payload) {
        Ok(v) => Some(v),
        Err(_) => {
            tracing::warn!("invalid chunk in SSE stream (len {})", payload.len());
            None
        }
    }
}

/// Turn a byte stream into a stream of parsed response chunks.
pub fn response_stream<S, B, E>(bytes: S) -> impl Stream<Item = Result<CaGenerateResponse, CaError>>
where
    S: Stream<Item = Result<B, E>> + Send + 'static,
    B: AsRef<[u8]>,
    E: std::fmt::Display,
{
    async_stream::stream! {
        let mut parser = SseParser::new();
        let mut bytes = Box::pin(bytes);
        while let Some(chunk) = bytes.next().await {
            match chunk {
                Ok(b) => match parser.push(b.as_ref()) {
                    Ok(evs) => for ev in evs {
                        if let Some(r) = parse_event(&ev) { yield Ok(r); }
                    },
                    Err(e) => { yield Err(e); return; }
                },
                Err(e) => { yield Err(CaError::Transport(e.to_string())); return; }
            }
        }
        if let Some(ev) = parser.finish() {
            if let Some(r) = parse_event(&ev) { yield Ok(r); }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crlf_split_and_comments() {
        let mut p = SseParser::new();
        let mut ev = p.push(b": keepalive\r\nid: 1\r\ndata: {\"a\":\r\n").unwrap();
        assert!(ev.is_empty());
        ev = p.push(b"data: 1}\r\n\r\n").unwrap();
        assert_eq!(ev, vec!["{\"a\":\n1}".to_string()]);
    }

    #[test]
    fn split_across_chunks_and_eof_flush() {
        let mut p = SseParser::new();
        assert!(p.push(b"da").unwrap().is_empty());
        assert!(p.push(b"ta: {\"traceId\":\"x\"}").unwrap().is_empty());
        assert_eq!(p.finish().as_deref(), Some("{\"traceId\":\"x\"}"));
    }

    #[test]
    fn blank_lines_without_data_emit_nothing() {
        let mut p = SseParser::new();
        assert!(p.push(b"\n\n\n").unwrap().is_empty());
        assert!(p.finish().is_none());
    }

    #[tokio::test]
    async fn malformed_chunk_is_skipped() {
        let chunks: Vec<Result<&'static [u8], std::io::Error>> =
            vec![Ok(b"data: {bad\n\n"), Ok(b"data: {\"traceId\":\"ok\"}\n\n")];
        let out: Vec<_> = response_stream(futures::stream::iter(chunks)).collect().await;
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].as_ref().unwrap().trace_id.as_deref(), Some("ok"));
    }

    #[test]
    fn oversized_line_and_event_are_rejected() {
        let mut p = SseParser::with_limit(16);
        assert!(matches!(p.push(&[b'x'; 32]), Err(CaError::Decode(_))));
        let mut p = SseParser::with_limit(16);
        assert!(p.push(b"data: 0123456789\n").is_ok());
        assert!(matches!(p.push(b"data: 0123456789\n"), Err(CaError::Decode(_))));
    }
}
