//! Shared vocabulary of the provider dialects.
//!
//! Everything that is not specific to one wire format lives here: the limits the shared driver
//! enforces, constructors for the chunk shape the agent loop already consumes, the [`Dialect`]
//! contract, and the decode state dialects use to reassemble tool calls whose arguments arrive
//! one fragment at a time.

use std::time::Duration;

use serde_json::{Map, Value};

use crate::api::{
    CaError, CaGenerateResponse, Candidate, Content, FunctionCall, GenerateContentRequest,
    GenerateContentResponse, Part, UsageMetadata,
};

// ---------------------------------------------------------------------------
// Limits
// ---------------------------------------------------------------------------

/// A provider that stays silent for this long is treated as dead. Longer than the Code Assist
/// client's 120 s because external endpoints have no equivalent heartbeat and reasoning models
/// routinely sit idle while they think.
pub const IDLE: Duration = Duration::from_secs(180);

/// Largest streamed body accepted for one turn. Guards against a runaway or hostile endpoint
/// that never ends its stream.
pub const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;

/// Largest single SSE event accepted. The shared parser applies it per line and per event.
pub const MAX_EVENT_BYTES: usize = 4 * 1024 * 1024;

/// Argument text accumulated per tool-call slot.
///
/// The driver already caps the whole body, but one slot holding megabytes of arguments would still
/// be parsed as JSON at the end of the turn. Arguments this large are not a tool call; the prefix is
/// kept so the parse failure degrades to an empty object instead of an aborted turn.
pub const MAX_ARG_BYTES: usize = 1024 * 1024;

// ---------------------------------------------------------------------------
// Credentials
// ---------------------------------------------------------------------------

/// The credential the driver resolved for one attempt.
///
/// A dialect never stores a secret of its own: it is handed one per request, so a backend that
/// swaps the account under it cannot send the previous account's token. `Key` is what a configured
/// profile carries (and what a key-shaped token is); `Bearer` is an OAuth access token, which is
/// refreshed from outside the dialect and must never be logged.
#[derive(Clone, Copy)]
pub enum WireAuth<'a> {
    /// A configured key (or a key-shaped token).
    Key(&'a str),
    /// An OAuth bearer token.
    Bearer(&'a str),
}

impl<'a> WireAuth<'a> {
    /// The credential itself, for the dialects that put it in a header.
    pub fn value(&self) -> &'a str {
        match self {
            WireAuth::Key(credential) | WireAuth::Bearer(credential) => credential,
        }
    }
}

// ---------------------------------------------------------------------------
// Dialect contract
// ---------------------------------------------------------------------------

/// One provider wire format.
///
/// The driver in [`super`] is generic over this trait, so the agent loop never learns which
/// provider it is talking to: it still asks for a `GenerateContentRequest` and still reads
/// `CaGenerateResponse` chunks. A dialect owns exactly two translations plus auth, nothing else.
///
/// `Sync` is part of the bound because the driver holds one across `await` points (resolving a
/// token, refreshing after a 401) and the resulting stream must still be `Send`.
pub trait Dialect: Send + Sync {
    /// Absolute URL to POST to. Providers that carry credentials in the query string (Gemini
    /// Developer API) append them here rather than in a header.
    fn endpoint(&self) -> String;

    /// Our request rendered as the provider's JSON body.
    fn build_body(&self, req: &GenerateContentRequest) -> Value;

    /// Auth, version and content-type headers. `auth` is the credential the driver resolved for
    /// this attempt; a dialect must not keep one of its own (see [`WireAuth`]).
    fn apply_headers(&self, rb: reqwest::RequestBuilder, auth: WireAuth<'_>) -> reqwest::RequestBuilder;

    /// Translate one SSE payload into zero or more chunks. Stateful formats (OpenAI tool calls,
    /// Anthropic content blocks) accumulate partial arguments in `state` and return them later.
    fn decode(
        &self,
        state: &mut Decoder,
        payload: &str,
    ) -> Result<Vec<CaGenerateResponse>, CaError>;

    /// End of stream: emit whatever `decode` was still holding back, typically buffered calls.
    fn finish(&self, state: &mut Decoder) -> Result<Vec<CaGenerateResponse>, CaError>;
}

// ---------------------------------------------------------------------------
// Function-call ids
// ---------------------------------------------------------------------------

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

fn fnv1a(bytes: impl IntoIterator<Item = u8>) -> u64 {
    let mut hash = FNV_OFFSET;
    for byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// Stable id for a tool call whose wire format has none.
///
/// The agent loop matches a function response back to its call by id, and a retracted or resumed
/// turn must produce the same ids it produced before, so the id is derived from the turn seed and
/// the call's position instead of a counter or a random value. Eight hex characters keep it short
/// enough to read in a log line while making collisions inside one turn implausible.
pub fn synthesize_call_id(seed: &str, index: usize) -> String {
    let hash = fnv1a(seed.as_bytes().iter().copied().chain(index.to_le_bytes()));
    format!("call_{:08x}", (hash >> 32) as u32)
}

// ---------------------------------------------------------------------------
// Chunk constructors
// ---------------------------------------------------------------------------

/// Wrap one model content in a response with a single candidate, which is the shape the loop reads.
fn response_with(content: Content) -> CaGenerateResponse {
    CaGenerateResponse {
        response: Some(GenerateContentResponse {
            candidates: vec![Candidate {
                content: Some(content),
                finish_reason: None,
                index: Some(0),
            }],
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// One visible text delta.
pub fn text_chunk(text: &str) -> CaGenerateResponse {
    response_with(Content::model(vec![Part::text(text)]))
}

/// One reasoning delta. The loop drops thought parts from history but the UI may show them.
pub fn thought_chunk(text: &str) -> CaGenerateResponse {
    response_with(Content::model(vec![Part {
        text: Some(text.to_owned()),
        thought: Some(true),
        ..Default::default()
    }]))
}

/// One completed tool call.
pub fn call_chunk(call: FunctionCall) -> CaGenerateResponse {
    response_with(Content::model(vec![Part {
        function_call: Some(call),
        ..Default::default()
    }]))
}

/// One completed tool call that carries more than the call (a thought signature).
pub fn part_chunk(part: Part) -> CaGenerateResponse {
    response_with(Content::model(vec![part]))
}

// ---------------------------------------------------------------------------
// Secrets in URLs
// ---------------------------------------------------------------------------

/// Percent-encode `raw` for use as a URL path segment or query-string value.
///
/// RFC 3986 unreserved characters (ALPHA / DIGIT / `-` / `.` / `_` / `~`) are left alone;
/// everything else is encoded as `%XX`. The `url` crate ships `form_urlencoded::byte_serialize`
/// but that encodes spaces as `+`, which is valid only in `application/x-www-form-urlencoded`
/// bodies, not in URL path segments or query-string values. We need `%20` for the API key.
///
/// Shared with the error scrubber: a gateway that echoes the request line quotes the key in
/// this form, not as typed.
pub(crate) fn percent_encode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char);
            }
            _ => {
                out.push('%');
                out.push(char::from_digit((byte >> 4) as u32, 16).unwrap_or('0').to_ascii_uppercase());
                out.push(char::from_digit((byte & 0xF) as u32, 16).unwrap_or('0').to_ascii_uppercase());
            }
        }
    }
    out
}

/// Token accounting on its own, without a candidate. Providers report usage in a final event
/// that carries no text; the loop reads `usage_metadata` and ignores the empty candidate list.
pub fn usage_chunk(prompt: u64, output: u64) -> CaGenerateResponse {
    let usage = UsageMetadata {
        prompt_token_count: prompt,
        candidates_token_count: output,
        total_token_count: prompt.saturating_add(output),
        ..Default::default()
    };
    CaGenerateResponse {
        response: Some(GenerateContentResponse {
            usage_metadata: Some(usage),
            ..Default::default()
        }),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// History projection
// ---------------------------------------------------------------------------

/// Project our parts onto what an external provider can accept.
///
/// Inline media is dropped: our internal `Blob` is only ever populated from a Code Assist
/// response, and forwarding base64 blobs to a provider that cannot price or accept them would
/// fail the whole turn. Empty text parts are dropped because some providers reject `""` content.
/// Thought flags and signatures are kept only on model turns: a user turn carrying them makes
/// OpenAI-compatible servers echo a reasoning field the model never produced.
pub fn expand_content(parts: &[Part], role_is_model: bool) -> Vec<Part> {
    let mut out = Vec::with_capacity(parts.len());
    for part in parts {
        let mut keep = Part::default();
        if let Some(text) = part.text.as_deref() {
            if !text.is_empty() {
                keep.text = Some(text.to_owned());
                if role_is_model && part.is_thought() {
                    keep.thought = Some(true);
                }
            }
        }
        keep.function_call = part.function_call.clone();
        keep.function_response = part.function_response.clone();
        if role_is_model {
            keep.thought_signature = part.thought_signature.clone();
        }
        if keep.text.is_some() || keep.function_call.is_some() || keep.function_response.is_some() {
            out.push(keep);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Decode state
// ---------------------------------------------------------------------------

/// A tool call whose arguments are still arriving as text fragments.
#[derive(Debug, Clone, Default)]
pub struct PartialCall {
    /// Provider-assigned id, when the wire format has one.
    pub id: Option<String>,
    pub name: String,
    /// Raw JSON fragments concatenated in arrival order; parsed once the stream completes.
    pub args: String,
    /// Gemini `thoughtSignature` of the call, from the first fragment that carried one. It must
    /// go back to the model with the call on the next request.
    pub thought_signature: Option<String>,
}

impl PartialCall {
    /// Nothing has been received for this slot yet.
    pub fn is_empty(&self) -> bool {
        self.id.is_none() && self.name.is_empty() && self.args.is_empty()
    }

    /// The call as a model part, keeping its thought signature.
    pub fn into_part(mut self, fallback_id: String) -> Part {
        let thought_signature = self.thought_signature.take();
        Part { function_call: Some(self.into_call(fallback_id)), thought_signature, ..Default::default() }
    }

    /// Turn the accumulated fragments into a call the loop can execute.
    ///
    /// `fallback_id` is used when the provider supplies none. Arguments that do not parse become
    /// an empty object rather than an error: a truncated tool call is recoverable (the tool reports
    /// the missing field) while aborting the turn loses the text that preceded it.
    pub fn into_call(self, fallback_id: String) -> FunctionCall {
        let args = if self.args.trim().is_empty() {
            Value::Object(Map::new())
        } else {
            match serde_json::from_str(&self.args) {
                Ok(args) => args,
                Err(e) => {
                    tracing::warn!(tool = %self.name, error = %e, "provider sent tool arguments that are not JSON");
                    Value::Object(Map::new())
                }
            }
        };
        FunctionCall { id: Some(self.id.unwrap_or(fallback_id)), name: self.name, args }
    }
}

/// Per-stream decode state handed to every [`Dialect::decode`] call and finally to
/// [`Dialect::finish`].
///
/// Only the slot table is shared; everything a single event needs stays local to that event. The
/// slot index is the position the wire format itself uses (OpenAI `tool_calls[].index`, Anthropic
/// content-block index), which is why gaps are allowed.
#[derive(Debug, Default)]
pub struct Decoder {
    slots: Vec<Option<PartialCall>>,
}

impl Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Slot `index`, created on first use.
    pub fn slot(&mut self, index: usize) -> &mut PartialCall {
        if self.slots.len() <= index {
            self.slots.resize_with(index + 1, || None);
        }
        self.slots[index].get_or_insert_with(PartialCall::default)
    }

    /// Remove and return slot `index`.
    pub fn take(&mut self, index: usize) -> Option<PartialCall> {
        self.slots.get_mut(index).and_then(Option::take)
    }

    /// Remove and return every non-empty slot, in wire order.
    pub fn take_all(&mut self) -> Vec<PartialCall> {
        let slots = std::mem::take(&mut self.slots);
        slots.into_iter().flatten().filter(|call| !call.is_empty()).collect()
    }

    /// No slot holds anything.
    pub fn is_empty(&self) -> bool {
        self.slots.iter().all(Option::is_none)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Blob, FunctionResponse};
    use serde_json::json;

    #[test]
    fn synthesize_call_id_is_stable_and_distinct() {
        let a = synthesize_call_id("turn-1", 0);
        assert_eq!(a, synthesize_call_id("turn-1", 0));
        assert_ne!(a, synthesize_call_id("turn-1", 1));
        assert_ne!(a, synthesize_call_id("turn-2", 0));
        assert!(a.starts_with("call_"));
        assert_eq!(a.len(), 13);
        assert!(a[5..].chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn chunk_constructors_produce_one_part() {
        let text = text_chunk("hello");
        assert_eq!(text.first_parts().len(), 1);
        assert_eq!(text.first_parts()[0].text.as_deref(), Some("hello"));
        assert!(!text.first_parts()[0].is_thought());

        let thought = thought_chunk("why");
        assert_eq!(thought.first_parts().len(), 1);
        assert!(thought.first_parts()[0].is_thought());

        let call = call_chunk(FunctionCall {
            id: Some("call_1".into()),
            name: "read_file".into(),
            args: json!({"file_path": "x"}),
        });
        assert_eq!(call.first_parts().len(), 1);
        assert_eq!(call.first_parts()[0].function_call.as_ref().map(|f| f.name.as_str()), Some("read_file"));
    }

    #[test]
    fn usage_chunk_has_no_candidate() {
        let usage = usage_chunk(10, 5);
        assert!(usage.first_parts().is_empty());
        let meta = usage.inner().usage_metadata.expect("usage metadata");
        assert_eq!(meta.prompt_token_count, 10);
        assert_eq!(meta.candidates_token_count, 5);
        assert_eq!(meta.total_token_count, 15);
    }

    #[test]
    fn expand_content_drops_media_and_empty_text() {
        let thought = Part { text: Some("thinking".into()), thought: Some(true), ..Default::default() };
        let parts = vec![
            Part::text(""),
            Part::text("hi"),
            thought,
            Part { inline_data: Some(Blob { mime_type: "image/png".into(), data: "AA==".into() }), ..Default::default() },
            Part::function_response(FunctionResponse { id: None, name: "t".into(), response: json!({"output": 1}) }),
        ];

        let model = expand_content(&parts, true);
        assert_eq!(model.len(), 3);
        assert_eq!(model[0].text.as_deref(), Some("hi"));
        assert!(model[1].is_thought());
        assert!(model[2].function_response.is_some());
        assert!(model.iter().all(|p| p.inline_data.is_none()));

        let user = expand_content(&parts, false);
        assert_eq!(user.len(), 3);
        assert!(user.iter().all(|p| !p.is_thought() && p.thought_signature.is_none()));
        assert_eq!(user[1].text.as_deref(), Some("thinking"));
    }

    #[test]
    fn partial_call_parses_arguments() {
        let call = PartialCall { id: None, name: "list_dir".into(), args: r#"{"path":"."}"#.into(), ..Default::default() };
        let parsed = call.into_call("call_fallback".into());
        assert_eq!(parsed.id.as_deref(), Some("call_fallback"));
        assert_eq!(parsed.args["path"], json!("."));

        let blank = PartialCall { id: Some("call_x".into()), name: "f".into(), args: String::new(), ..Default::default() };
        let parsed = blank.into_call("unused".into());
        assert_eq!(parsed.id.as_deref(), Some("call_x"));
        assert_eq!(parsed.args, json!({}));

        let broken = PartialCall { id: None, name: "f".into(), args: "{\"a\":".into(), ..Default::default() };
        assert_eq!(broken.into_call("c".into()).args, json!({}));
    }

    #[test]
    fn decoder_keeps_slots_across_events() {
        let mut state = Decoder::new();
        state.slot(0).name.push_str("read_");
        state.slot(0).name.push_str("file");
        state.slot(1).id = Some("call_b".into());
        state.slot(1).args.push_str(r#"{"a":1}"#);
        assert!(!state.is_empty());

        let calls = state.take_all();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "read_file");
        assert!(state.is_empty());

        state.slot(3).name = "taken".into();
        assert_eq!(state.take(3).map(|c| c.name), Some("taken".into()));
        assert!(state.take(3).is_none());
    }
}
