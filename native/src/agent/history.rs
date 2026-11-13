//! Chat history and streamed-turn accumulation (PROTOCOL.md 3.4, 3.5).

use crate::api::{Content, FunctionCall, Part, Role};

/// Placeholder signature for function calls restored without one (PROTOCOL 3.5).
pub const SKIP_THOUGHT_SIGNATURE: &str = "skip_thought_signature_validator";

/// Alternating user/model history. Thought parts are never stored.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct History {
    contents: Vec<Content>,
}

impl History {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_contents(contents: Vec<Content>) -> Self {
        Self { contents }
    }

    pub fn contents(&self) -> &[Content] {
        &self.contents
    }

    pub fn into_contents(self) -> Vec<Content> {
        self.contents
    }

    pub fn push(&mut self, c: Content) {
        self.contents.push(c);
    }

    /// Copy for a request: ensures the first `functionCall` of every model content has a
    /// `thoughtSignature`, inserting [`SKIP_THOUGHT_SIGNATURE`] when missing.
    pub fn for_request(&self) -> Vec<Content> {
        let mut out = self.contents.clone();
        for c in out.iter_mut().filter(|c| c.role == Some(Role::Model)) {
            if let Some(p) = c.parts.iter_mut().find(|p| p.function_call.is_some()) {
                if p.thought_signature.is_none() {
                    p.thought_signature = Some(SKIP_THOUGHT_SIGNATURE.into());
                }
            }
        }
        out
    }
}

/// Builds one model turn from streamed chunks.
#[derive(Debug, Default)]
pub struct TurnAccumulator {
    parts: Vec<Part>,
    call_counter: u32,
}

impl TurnAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add one chunk's parts. Thought parts are skipped; adjacent text parts are merged;
    /// function calls get normalized IDs (`{name}__{id}`).
    pub fn push_parts(&mut self, parts: &[Part]) {
        for p in parts {
            if p.is_thought() {
                continue;
            }
            if let Some(fc) = &p.function_call {
                let mut p = p.clone();
                p.function_call = Some(self.normalize_call(fc.clone()));
                self.parts.push(p);
                continue;
            }
            match (self.parts.last_mut(), &p.text) {
                (Some(last), Some(t)) if last.text.is_some() && last.function_call.is_none() && p.thought_signature.is_none() => {
                    last.text.get_or_insert_with(String::new).push_str(t);
                }
                _ => self.parts.push(p.clone()),
            }
        }
    }

    fn normalize_call(&mut self, mut fc: FunctionCall) -> FunctionCall {
        if fc.name.is_empty() {
            fc.name = "generic_tool".into();
        }
        self.call_counter += 1;
        let id = fc.id.take().unwrap_or_else(|| {
            format!("{}_{}_{}", fc.name, chrono::Utc::now().timestamp_millis(), self.call_counter)
        });
        let prefix = format!("{}__", fc.name);
        fc.id = Some(if id.starts_with(&prefix) { id } else { format!("{prefix}{id}") });
        fc
    }

    pub fn function_calls(&self) -> Vec<FunctionCall> {
        self.parts.iter().filter_map(|p| p.function_call.clone()).collect()
    }

    /// True when nothing usable arrived (`invalid_stream`).
    pub fn is_empty(&self) -> bool {
        self.parts.iter().all(|p| p.function_call.is_none() && p.text.as_deref().is_none_or(str::is_empty))
    }

    /// Final model content for history.
    pub fn into_content(self) -> Content {
        Content::model(self.parts)
    }
}

/// Split a thought text into (`**Subject**`, description) (PROTOCOL 3.6).
pub fn parse_thought(text: &str) -> (Option<String>, String) {
    let t = text.trim();
    if let Some(rest) = t.strip_prefix("**") {
        if let Some(end) = rest.find("**") {
            return (Some(rest[..end].trim().to_owned()), rest[end + 2..].trim().to_owned());
        }
    }
    (None, t.to_owned())
}

/// True for a user content that starts a turn (typed text, not a `functionResponse` batch).
pub fn is_user_text_turn(c: &Content) -> bool {
    c.role == Some(Role::User)
        && c.parts.iter().all(|p| p.function_response.is_none())
        && c.parts.iter().any(|p| p.text.as_deref().is_some_and(|t| !t.is_empty()))
}

/// Indices of every user text turn, oldest first.
pub fn user_turn_starts(contents: &[Content]) -> Vec<usize> {
    contents.iter().enumerate().filter(|(_, c)| is_user_text_turn(c)).map(|(i, _)| i).collect()
}

/// Rough token estimate (about 4 bytes per token) of everything a request would carry.
/// Used before the server reports a `promptTokenCount`, and for compaction notices.
pub fn estimate_tokens(contents: &[Content]) -> u64 {
    let bytes: usize = contents
        .iter()
        .flat_map(|c| c.parts.iter())
        .filter(|p| !p.is_thought())
        .map(|p| {
            p.text.as_ref().map_or(0, String::len)
                + p.function_call.as_ref().map_or(0, |f| f.name.len() + f.args.to_string().len())
                + p.function_response.as_ref().map_or(0, |r| r.name.len() + r.response.to_string().len())
        })
        .sum();
    (bytes as u64).div_ceil(4)
}

impl History {
    /// Number of stored contents.
    pub fn len(&self) -> usize {
        self.contents.len()
    }

    pub fn is_empty(&self) -> bool {
        self.contents.is_empty()
    }

    /// Replace everything (used after compaction).
    pub fn replace(&mut self, contents: Vec<Content>) {
        self.contents = contents;
    }

    /// See [`estimate_tokens`].
    pub fn estimate_tokens(&self) -> u64 {
        estimate_tokens(&self.contents)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn merges_text_and_drops_thoughts() {
        let mut acc = TurnAccumulator::new();
        acc.push_parts(&[Part::text("Hel")]);
        acc.push_parts(&[Part { text: Some("thinking".into()), thought: Some(true), ..Default::default() }]);
        acc.push_parts(&[Part::text("lo")]);
        acc.push_parts(&[Part {
            function_call: Some(FunctionCall { id: None, name: "read_file".into(), args: json!({}) }),
            ..Default::default()
        }]);
        let calls = acc.function_calls();
        assert!(calls[0].id.as_deref().unwrap().starts_with("read_file__read_file_"));
        let c = acc.into_content();
        assert_eq!(c.parts.len(), 2);
        assert_eq!(c.parts[0].text.as_deref(), Some("Hello"));
    }

    #[test]
    fn request_copy_adds_signature() {
        let mut h = History::new();
        h.push(Content::model(vec![Part {
            function_call: Some(FunctionCall { id: Some("x".into()), name: "glob".into(), args: json!({}) }),
            ..Default::default()
        }]));
        assert_eq!(h.for_request()[0].parts[0].thought_signature.as_deref(), Some(SKIP_THOUGHT_SIGNATURE));
        assert!(h.contents()[0].parts[0].thought_signature.is_none());
    }

    #[test]
    fn thought_subject() {
        assert_eq!(parse_thought("**Plan** read files"), (Some("Plan".into()), "read files".into()));
    }

    #[test]
    fn user_turns_and_estimate() {
        use crate::api::FunctionResponse;
        let fr = Content { role: Some(Role::User), parts: vec![Part::function_response(FunctionResponse { id: None, name: "x".into(), response: json!({"output":"ok"}) })] };
        let c = vec![Content::user_text("hi"), Content::model(vec![Part::text("yo")]), fr, Content::model(vec![Part::text("done")]), Content::user_text("next")];
        assert_eq!(user_turn_starts(&c), vec![0, 4]);
        assert!(!is_user_text_turn(&c[2]));
        assert_eq!(estimate_tokens(&[Content::user_text("abcdefgh")]), 2);
        let mut h = History::from_contents(c);
        assert_eq!(h.len(), 5);
        h.replace(vec![]);
        assert!(h.is_empty());
    }
}
