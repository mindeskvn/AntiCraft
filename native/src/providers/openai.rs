//! OpenAI-compatible chat-completions dialect (`{base}/chat/completions`, SSE `data:` frames).
//!
//! The same wire format serves OpenAI itself and the compatible servers users point us at (Groq,
//! LM Studio, vLLM, OpenRouter, most gateways). This dialect therefore sticks to the intersection
//! of the format: `stream`, `stream_options.include_usage`, `tools`, and the four generation
//! knobs every server accepts. A field only one vendor understands is a rejected request and a
//! lost turn, so nothing is sent speculatively.
//!
//! Two shapes differ from our own model in ways that drive the code below:
//!
//! * A tool result is a first-class message (`role: "tool"`), not a part of a user turn, so one of
//!   our user contents can become several wire messages.
//! * Tool arguments arrive as string fragments keyed by `index`, so a call does not exist until
//!   the stream says it is finished. [`Wire::decode`] accumulates the fragments in the shared
//!   [`Decoder`] and releases the calls at the finish reason, at `[DONE]`, or at end of stream.

use std::collections::HashSet;
use std::fmt;

use serde_json::{json, Map, Value};

use crate::agent::mcp::schema::sanitize_schema;
use crate::api::{
    CaError, CaGenerateResponse, Candidate, Content, FunctionCall, FunctionResponse,
    GenerateContentRequest, GenerateContentResponse, GenerationConfig, Role,
};
use crate::providers::common::{
    call_chunk, expand_content, synthesize_call_id, text_chunk, thought_chunk, usage_chunk, Dialect,
    Decoder, WireAuth,
};
use crate::providers::ProviderProfile;

/// The terminator OpenAI sends after the last chunk. It is not JSON and says nothing about
/// completion, but it is the last point at which buffered calls can still be released.
const DONE: &str = "[DONE]";

/// Characters of a provider error kept in [`CaError`]. Long enough for a real explanation, short
/// enough that a stack trace or an HTML fragment cannot dominate a log line or the UI.
const MAX_ERROR_CHARS: usize = 512;

/// Highest `tool_calls[].index` honoured in one stream.
///
/// [`Decoder::slot`] sizes itself to the index it is asked for, so an index of a billion would
/// turn a single hostile frame into gigabytes of allocation. Tools in one turn are counted in
/// single digits; the cap is generous and still bounded.
const MAX_TOOL_SLOTS: usize = 128;

/// Argument text accumulated per tool-call slot.
///
/// The driver already caps the whole body, but one slot holding megabytes of arguments would
/// still be parsed as JSON at the end of the turn. Arguments this large are not a tool call; the
/// prefix is kept so the parse failure degrades to an empty object instead of an aborted turn.
const MAX_ARG_BYTES: usize = 1024 * 1024;

// ---------------------------------------------------------------------------
// Wire
// ---------------------------------------------------------------------------

/// One configured OpenAI-compatible endpoint.
///
/// Only the fields a request and a stream need are kept, so the API key cannot be reached through
/// any path other than [`Dialect::apply_headers`].
pub struct Wire {
    /// Base URL with its trailing slash already removed by `ProviderProfile::resolved_base_url`.
    base_url: String,
    model: String,
    api_key: String,
    /// Profile-level cap, applied only when the request names none.
    max_tokens: Option<u32>,
}

impl Wire {
    pub fn new(profile: &ProviderProfile) -> Self {
        Self {
            base_url: profile.resolved_base_url(),
            model: profile.model.trim().to_owned(),
            api_key: profile.api_key.trim().to_owned(),
            max_tokens: profile.max_output_tokens,
        }
    }

    /// Request body: system prompt, full history, tools and generation knobs.
    fn body(&self, req: &GenerateContentRequest) -> Value {
        let mut body = Map::new();
        body.insert("model".to_owned(), json!(self.model));
        body.insert("messages".to_owned(), Value::Array(messages(req, &self.model)));
        body.insert("stream".to_owned(), json!(true));
        // Without this OpenAI leaves `usage` out of a streamed answer entirely and the loop's
        // token accounting reads zero for the whole turn. Servers that predate the option ignore
        // it; servers that reject unknown fields are not worth supporting because they reject
        // `tools` in the same breath.
        body.insert("stream_options".to_owned(), json!({ "include_usage": true }));
        if let Some(tools) = tools(req) {
            body.insert("tools".to_owned(), tools);
        }
        self.apply_config(&mut body, req.generation_config.as_ref());
        Value::Object(body)
    }

    /// Copy the generation config onto the body, dropping what has no equivalent.
    ///
    /// `top_k` and `thinking_config` are Code Assist concepts: no OpenAI-compatible server takes
    /// them, and inventing a mapping (a sampling knob for a token budget) would silently change
    /// how the model answers. `response_mime_type` is dropped for the same reason: it exists to
    /// force JSON for tool-bound turns, and OpenAI's nearest field would forbid a normal answer.
    fn apply_config(&self, body: &mut Map<String, Value>, config: Option<&GenerationConfig>) {
        let Some(config) = config else {
            if let Some(max) = self.max_tokens {
                body.insert("max_tokens".to_owned(), json!(max));
            }
            return;
        };
        if let Some(temperature) = config.temperature {
            body.insert("temperature".to_owned(), json!(temperature));
        }
        if let Some(top_p) = config.top_p {
            body.insert("top_p".to_owned(), json!(top_p));
        }
        // The profile cap is a fallback, not an override: a request that names its own budget
        // wins, because the loop sets one when the answer must fit a known space.
        if let Some(max) = config.max_output_tokens.or(self.max_tokens) {
            body.insert("max_tokens".to_owned(), json!(max));
        }
        if let Some(stop) = config.stop_sequences.as_ref().filter(|stop| !stop.is_empty()) {
            body.insert("stop".to_owned(), json!(stop));
        }
    }

    /// Translate one streamed frame, updating the tool-call accumulator in `state`.
    fn decode_frame(
        &self,
        state: &mut Decoder,
        payload: &str,
    ) -> Result<Vec<CaGenerateResponse>, CaError> {
        let payload = payload.trim();
        if payload.is_empty() {
            return Ok(Vec::new());
        }
        if payload == DONE {
            return Ok(flush_calls(state, &self.model));
        }
        let frame: Value = match serde_json::from_str(payload) {
            Ok(frame) => frame,
            Err(error) => {
                // One unreadable frame is not worth the turn: the text before it has already been
                // delivered, and providers routinely close with a truncated or comment-like
                // frame. Dropping it keeps the answer the user can already see.
                tracing::warn!(error = %error, len = payload.len(), "openai stream frame is not JSON");
                return Ok(Vec::new());
            }
        };
        if let Some(error) = frame.get("error") {
            return Err(self.stream_error(error, payload));
        }

        let mut chunks = Vec::new();
        let finish = match frame
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
        {
            Some(choice) => decode_choice(state, choice, &mut chunks),
            None => None,
        };
        if let Some(usage) = frame.get("usage").filter(|usage| usage.is_object()) {
            push_usage(usage, &mut chunks);
        }
        if finish.is_some() {
            // Buffered calls are complete the moment the stream stops, whatever reason it gives:
            // a server that reports `stop` after streaming a call still owes us that call, and
            // holding it back would leave the model's turn without a result.
            chunks.extend(flush_calls(state, &self.model));
        }
        if let Some(reason) = finish {
            chunks.push(finish_chunk(&reason));
        }
        Ok(chunks)
    }

    /// Map a streamed error frame onto the loop's error vocabulary.
    ///
    /// Rate limits and server faults are worth another attempt, so they become `Retryable` with no
    /// hint (the frame carries no `Retry-After`; the loop's own backoff applies). Everything else
    /// is reported as the provider wrote it, truncated and with our key removed: a gateway that
    /// echoes the request line would otherwise put a live key in a log or a screenshot.
    fn stream_error(&self, error: &Value, payload: &str) -> CaError {
        let kind = error.get("type").and_then(Value::as_str).unwrap_or_default();
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .or_else(|| error.as_str())
            .unwrap_or(payload);
        if kind.starts_with("rate_limit") || kind.starts_with("server_error") {
            tracing::warn!(kind, message = %self.scrub(message), "provider reported a retryable stream error");
            return CaError::Retryable { delay: None };
        }
        CaError::Decode(self.scrub(message))
    }

    /// Truncate a provider message and remove the API key from it.
    fn scrub(&self, message: &str) -> String {
        let without_key = if self.api_key.len() >= 4 && message.contains(&self.api_key) {
            message.replace(&self.api_key, "<redacted>")
        } else {
            message.to_owned()
        };
        let trimmed = without_key.trim();
        if trimmed.chars().count() <= MAX_ERROR_CHARS {
            return trimmed.to_owned();
        }
        let mut out: String = trimmed.chars().take(MAX_ERROR_CHARS).collect();
        out.push_str("...");
        out
    }
}

/// Hand-written so a `dbg!` or a panic dump can never carry the key; the profile's own `Debug`
/// already redacts, and this type must not be the hole in that chain.
impl fmt::Debug for Wire {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("openai::Wire")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("api_key", &"<redacted>")
            .field("max_tokens", &self.max_tokens)
            .finish()
    }
}

impl Dialect for Wire {
    fn endpoint(&self) -> String {
        // A base URL that already names the whole route is a common paste: users copy the
        // documented endpoint instead of the API root. Appending again would turn that into a
        // 404 that reads like a bad model name.
        if self.base_url.ends_with("/chat/completions") {
            self.base_url.clone()
        } else {
            format!("{}/chat/completions", self.base_url)
        }
    }

    fn build_body(&self, req: &GenerateContentRequest) -> Value {
        self.body(req)
    }

    fn apply_headers(&self, rb: reqwest::RequestBuilder, auth: WireAuth<'_>) -> reqwest::RequestBuilder {
        // The credential is whatever the driver resolved: this wire carries it in the header for
        // both a configured key and an OAuth token, so the dialect does not need to tell them apart.
        rb.bearer_auth(auth.value())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            // Several gateways choose the streaming response by negotiation rather than by the
            // body; the header costs nothing and avoids a buffered answer we cannot read.
            .header(reqwest::header::ACCEPT, "text/event-stream")
    }

    fn decode(
        &self,
        state: &mut Decoder,
        payload: &str,
    ) -> Result<Vec<CaGenerateResponse>, CaError> {
        self.decode_frame(state, payload)
    }

    fn finish(&self, state: &mut Decoder) -> Result<Vec<CaGenerateResponse>, CaError> {
        // A server that closes without `finish_reason` or `[DONE]` still streamed its calls.
        Ok(flush_calls(state, &self.model))
    }
}

// ---------------------------------------------------------------------------
// Request: history
// ---------------------------------------------------------------------------

/// Wire messages under construction, plus the tool calls not yet answered.
///
/// A [`FunctionResponse`] normally carries the id of its call, which is all that is needed. A
/// restored or hand-built history can lose it, and OpenAI rejects a tool message whose
/// `tool_call_id` matches nothing, so the calls the assistant just made are remembered here and
/// matched by name as a fallback.
struct Messages {
    out: Vec<Value>,
    /// `(name, id)` of calls still waiting for a result, in call order.
    open_calls: Vec<(String, String)>,
    /// Seed for ids a call is missing, see [`synthesize_call_id`].
    seed: String,
    synthesized: usize,
}

impl Messages {
    fn new(seed: &str) -> Self {
        Self { out: Vec::new(), open_calls: Vec::new(), seed: seed.to_owned(), synthesized: 0 }
    }

    fn push_system(&mut self, text: String) {
        self.out.push(json!({ "role": "system", "content": text }));
    }

    /// One user content: its text becomes a user message, its tool results follow as tool
    /// messages. A content with neither (for example an image-only turn after media is dropped)
    /// contributes nothing, which keeps the request free of empty messages.
    fn push_user(&mut self, content: &Content) {
        let parts = expand_content(&content.parts, false);
        let text: String = parts.iter().filter_map(|part| part.text.as_deref()).collect();
        if !text.trim().is_empty() {
            self.out.push(json!({ "role": "user", "content": text }));
        }
        // A tool result is never folded into the user text: OpenAI matches it to its call by
        // `tool_call_id` alone, and a result inside a user message would leave the call
        // unanswered, which some servers reject outright.
        for part in &parts {
            let Some(response) = part.function_response.as_ref() else {
                continue;
            };
            let Some(id) = self.answer(response) else {
                continue;
            };
            self.out.push(json!({
                "role": "tool",
                "tool_call_id": id,
                "content": tool_result_text(&response.response),
            }));
        }
    }

    /// One model content: text becomes the assistant message, calls become its `tool_calls`.
    fn push_model(&mut self, content: &Content) {
        let parts = expand_content(&content.parts, true);
        // Reasoning is not replayed. No portable field carries it back, and merging it into
        // `content` would teach the model that its own thinking had been spoken aloud.
        let text: String =
            parts.iter().filter(|part| !part.is_thought()).filter_map(|part| part.text.as_deref()).collect();
        let calls: Vec<&FunctionCall> = parts.iter().filter_map(|part| part.function_call.as_ref()).collect();
        if text.trim().is_empty() && calls.is_empty() {
            // An assistant message with no content and no calls is rejected by several servers,
            // and it carries no information, so the turn is skipped entirely.
            return;
        }

        let mut message = Map::new();
        message.insert("role".to_owned(), json!("assistant"));
        // `null` rather than `""`: that is what OpenAI itself sends for a call-only turn, and
        // some servers treat an empty string as a real, empty answer.
        message.insert(
            "content".to_owned(),
            if text.is_empty() { Value::Null } else { json!(text) },
        );
        if !calls.is_empty() {
            let mut wire_calls = Vec::with_capacity(calls.len());
            let mut opened = Vec::with_capacity(calls.len());
            for call in calls {
                let id = match call.id.as_deref().map(str::trim).filter(|id| !id.is_empty()) {
                    Some(id) => id.to_owned(),
                    None => {
                        let id = synthesize_call_id(&self.seed, self.synthesized);
                        self.synthesized += 1;
                        id
                    }
                };
                wire_calls.push(json!({
                    "id": id,
                    "type": "function",
                    "function": {
                        "name": call.name,
                        // The arguments are a JSON *string* on this wire, not an object.
                        "arguments": arguments_text(&call.args),
                    },
                }));
                opened.push((call.name.clone(), id));
            }
            message.insert("tool_calls".to_owned(), Value::Array(wire_calls));
            self.open_calls.extend(opened);
        }
        self.out.push(Value::Object(message));
    }

    /// Id to attach to a tool result, or `None` when there is nothing to attach it to.
    ///
    /// Our loop copies the call id into every response, so the exact path is the normal one. When
    /// the id is missing, the oldest unanswered call with the same name takes the result; an
    /// unmatched result is dropped instead of being sent with an empty id, because one orphan
    /// fails the whole request and takes every other message down with it.
    fn answer(&mut self, response: &FunctionResponse) -> Option<String> {
        if let Some(id) = response.id.as_deref().map(str::trim).filter(|id| !id.is_empty()) {
            self.open_calls.retain(|(_, open)| open != id);
            return Some(id.to_owned());
        }
        let position = self
            .open_calls
            .iter()
            .position(|(name, _)| name == &response.name)
            .or_else(|| (!self.open_calls.is_empty()).then_some(0))?;
        Some(self.open_calls.remove(position).1)
    }
}

/// Render our history as wire messages, system instruction first.
fn messages(req: &GenerateContentRequest, seed: &str) -> Vec<Value> {
    let mut messages = Messages::new(seed);
    if let Some(system) = req
        .system_instruction
        .as_ref()
        .map(Content::text)
        .filter(|text| !text.trim().is_empty())
    {
        messages.push_system(system);
    }
    for content in &req.contents {
        match content.role {
            Some(Role::Model) => messages.push_model(content),
            // A missing role is malformed history; treating it as user text keeps the turn
            // sendable, while inventing an assistant turn would replay text the model never said.
            _ => messages.push_user(content),
        }
    }
    messages.out
}

/// Our arguments are a JSON value; the wire wants that value as a string.
fn arguments_text(args: &Value) -> String {
    match args {
        Value::Null => "{}".to_owned(),
        // Serializing a `Value` cannot fail for anything a parse produced; the fallback exists so
        // a hand-built value cannot abort the turn over formatting.
        other => serde_json::to_string(other).unwrap_or_else(|_| "{}".to_owned()),
    }
}

/// Text of a tool result.
///
/// The loop stores `{"output": ...}` on success and `{"error": "..."}` on failure; either becomes
/// plain text so the model reads the result rather than JSON punctuation. Anything else is passed
/// through as JSON, because an unfamiliar tool's payload is still more useful than nothing.
fn tool_result_text(response: &Value) -> String {
    for key in ["output", "error"] {
        if let Some(value) = response.get(key) {
            return match value {
                Value::String(text) => text.clone(),
                Value::Null => String::new(),
                other => serde_json::to_string(other).unwrap_or_default(),
            };
        }
    }
    match response {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

// ---------------------------------------------------------------------------
// Request: tools
// ---------------------------------------------------------------------------

/// Every tool declaration the loop built, or `None` when it built none.
///
/// The dialect keeps no list of its own: built-ins, orchestration, skills, rule lookup, finish and
/// MCP tools all arrive in `req.tools`, and a second list here would silently drift the day a tool
/// is added. MCP names already carry `mcp__server__tool`, which is legal on this wire.
fn tools(req: &GenerateContentRequest) -> Option<Value> {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut out = Vec::new();
    for declaration in req.tools.iter().flatten().flat_map(|tool| &tool.function_declarations) {
        let name = declaration.name.trim();
        if name.is_empty() {
            // A nameless declaration is rejected by the provider and cannot be called.
            tracing::warn!("skipping a tool declaration with no name");
            continue;
        }
        if !seen.insert(name) {
            // Duplicate names fail the whole request; the first declaration keeps the name so the
            // remaining tools stay usable.
            tracing::warn!(tool = name, "skipping a duplicate tool declaration");
            continue;
        }
        out.push(json!({
            "type": "function",
            "function": {
                "name": name,
                "description": declaration.description,
                // The provider requires an object root with `properties`; a server-supplied schema
                // that is a union, a `$ref` or a bare string would fail the request, so it goes
                // through the same pass the Code Assist path uses.
                "parameters": sanitize_schema(&declaration.parameters_json_schema),
            },
        }));
    }
    if out.is_empty() {
        None
    } else {
        Some(Value::Array(out))
    }
}

// ---------------------------------------------------------------------------
// Response: state machine
// ---------------------------------------------------------------------------

/// Read the delta of one choice into chunks and return the finish reason it carries, mapped.
fn decode_choice(
    state: &mut Decoder,
    choice: &Value,
    chunks: &mut Vec<CaGenerateResponse>,
) -> Option<String> {
    // A gateway that ignores `stream` answers with a whole `message` instead of a `delta`. The
    // field shapes are identical, so one accumulation path handles both.
    if let Some(delta) = choice.get("delta").or_else(|| choice.get("message")) {
        // Reasoning is not part of every server's vocabulary. Where it exists it is streamed the
        // way text is, and keeping it visible in the UI costs nothing: the loop drops thought
        // parts before they reach history.
        let thought = ["reasoning_content", "reasoning"]
            .iter()
            .find_map(|key| delta.get(*key).and_then(Value::as_str))
            .filter(|text| !text.is_empty());
        if let Some(thought) = thought {
            chunks.push(thought_chunk(thought));
        }
        if let Some(text) = delta.get("content").and_then(Value::as_str).filter(|text| !text.is_empty()) {
            chunks.push(text_chunk(text));
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for (position, call) in calls.iter().enumerate() {
                absorb_call(state, position, call);
            }
        }
    }
    choice
        .get("finish_reason")
        .and_then(Value::as_str)
        .filter(|reason| !reason.is_empty())
        .map(map_finish_reason)
}

/// Add one streamed tool-call fragment to its slot.
fn absorb_call(state: &mut Decoder, position: usize, call: &Value) {
    let Some(index) = tool_index(call, position) else {
        return;
    };
    let slot = state.slot(index);
    if let Some(id) = call.get("id").and_then(Value::as_str).filter(|id| !id.is_empty()) {
        // Servers repeat the id on later fragments; the first value wins so a stray fragment
        // cannot detach the call from the result that will answer it.
        slot.id.get_or_insert_with(|| id.to_owned());
    }
    if let Some(name) = call.get("function").and_then(|function| function.get("name")).and_then(Value::as_str) {
        // The name arrives whole on the first fragment. Appending whatever a later fragment
        // repeats would produce `read_fileread_file`, so only an empty slot is filled.
        if slot.name.is_empty() {
            slot.name.push_str(name);
        }
    }
    if let Some(fragment) = call
        .get("function")
        .and_then(|function| function.get("arguments"))
        .and_then(Value::as_str)
    {
        let room = MAX_ARG_BYTES.saturating_sub(slot.args.len());
        if fragment.len() > room {
            tracing::warn!(tool = %slot.name, "tool arguments exceeded the size cap; keeping the prefix");
            slot.args.push_str(truncate_bytes(fragment, room));
        } else {
            slot.args.push_str(fragment);
        }
    }
}

/// Slot a fragment belongs to.
///
/// `index` is the wire's own tool-call position and may have gaps. Some servers omit it and send
/// one call per frame, so the frame position is used then. An index past the cap is refused
/// rather than sized for; see [`MAX_TOOL_SLOTS`].
fn tool_index(call: &Value, position: usize) -> Option<usize> {
    let index = call
        .get("index")
        .and_then(Value::as_u64)
        .map(|value| value as usize)
        .unwrap_or(position);
    if index >= MAX_TOOL_SLOTS {
        tracing::warn!(index, "tool call index beyond the supported range");
        return None;
    }
    Some(index)
}

/// Release every call the stream has completed.
///
/// Ids come out in slot order; a call the server never gave an id gets the stable synthesized one,
/// so a retried turn produces the same ids it produced before.
fn flush_calls(state: &mut Decoder, seed: &str) -> Vec<CaGenerateResponse> {
    state
        .take_all()
        .into_iter()
        .enumerate()
        .map(|(position, call)| {
            let fallback = synthesize_call_id(seed, position);
            call_chunk(call.into_call(fallback))
        })
        .collect()
}

/// Our finish reason for the wire's.
fn map_finish_reason(reason: &str) -> String {
    match reason {
        "length" => "MAX_TOKENS".to_owned(),
        // A turn that ends to run tools ends normally as far as history is concerned.
        "stop" | "tool_calls" => "STOP".to_owned(),
        "content_filter" => "SAFETY".to_owned(),
        // An unknown reason (some servers add their own) is passed through upper-cased: the loop
        // ignores it, but the UI can still show why the stream ended.
        other => other.trim().to_ascii_uppercase(),
    }
}

/// Final chunk carrying the finish reason.
fn finish_chunk(reason: &str) -> CaGenerateResponse {
    CaGenerateResponse {
        response: Some(GenerateContentResponse {
            candidates: vec![Candidate {
                content: None,
                finish_reason: Some(reason.to_owned()),
                index: Some(0),
            }],
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Token accounting from a `usage` object, when it carries anything.
fn push_usage(usage: &Value, chunks: &mut Vec<CaGenerateResponse>) {
    let prompt = usage.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(0);
    let completion = usage.get("completion_tokens").and_then(Value::as_u64).unwrap_or(0);
    if prompt == 0 && completion == 0 {
        // Some servers repeat an all-zero usage on every frame; forwarding it would only spam the
        // Usage event stream with nothing to report.
        return;
    }
    chunks.push(usage_chunk(prompt, completion));
}

/// `max` bytes of `text`, cut on a character boundary.
fn truncate_bytes(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

// ---------------------------------------------------------------------------
// Tests: literal payloads, no network
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Part, Tool};

    fn profile() -> ProviderProfile {
        ProviderProfile {
            id: "p1".into(),
            label: "OpenAI".into(),
            kind: crate::providers::ProviderKind::OpenAi,
            model: "gpt-4o-mini".into(),
            base_url: None,
            api_key: "sk-test-key".into(),
            max_output_tokens: None,
            context_window: None,
            models: Vec::new(),
        }
    }

    fn wire() -> Wire {
        Wire::new(&profile())
    }

    /// Take every chunk from a list of literal frames.
    fn decode_all(wire: &Wire, state: &mut Decoder, frames: &[&str]) -> Vec<CaGenerateResponse> {
        frames
            .iter()
            .flat_map(|frame| wire.decode(state, frame).expect("frame decodes"))
            .collect()
    }

    /// One-line rendering of a chunk, so an ordering assertion reads like the stream did.
    fn describe(chunk: &CaGenerateResponse) -> String {
        let inner = chunk.inner();
        if let Some(usage) = inner.usage_metadata {
            return format!("usage:{}:{}", usage.prompt_token_count, usage.candidates_token_count);
        }
        let Some(candidate) = inner.candidates.first() else {
            return "empty".to_owned();
        };
        if let Some(reason) = candidate.finish_reason.as_deref() {
            return format!("finish:{reason}");
        }
        let Some(part) = candidate.content.as_ref().and_then(|content| content.parts.first()) else {
            return "empty".to_owned();
        };
        if let Some(call) = part.function_call.as_ref() {
            return format!(
                "call:{}:{}:{}",
                call.name,
                call.id.as_deref().unwrap_or(""),
                call.args
            );
        }
        if part.is_thought() {
            return format!("thought:{}", part.text.as_deref().unwrap_or(""));
        }
        format!("text:{}", part.text.as_deref().unwrap_or(""))
    }

    fn descriptions(chunks: &[CaGenerateResponse]) -> Vec<String> {
        chunks.iter().map(describe).collect()
    }

    fn declarations(names: &[(&str, Value)]) -> Vec<Tool> {
        vec![Tool {
            function_declarations: names
                .iter()
                .map(|(name, schema)| crate::api::FunctionDeclaration {
                    name: (*name).into(),
                    description: format!("{name} description"),
                    parameters_json_schema: schema.clone(),
                })
                .collect(),
        }]
    }

    #[test]
    fn endpoint_appends_the_route_once_and_headers_carry_the_bearer_token() {
        assert_eq!(wire().endpoint(), "https://api.openai.com/v1/chat/completions");

        let mut local = profile();
        local.base_url = Some("http://localhost:1234/v1/".into());
        assert_eq!(Wire::new(&local).endpoint(), "http://localhost:1234/v1/chat/completions");

        local.base_url = Some("http://localhost:1234/v1/chat/completions".into());
        assert_eq!(Wire::new(&local).endpoint(), "http://localhost:1234/v1/chat/completions");

        let request = wire()
            .apply_headers(
                reqwest::Client::new().post(wire().endpoint()),
                crate::providers::common::WireAuth::Key("sk-test-key"),
            )
            .build()
            .expect("request builds");
        assert_eq!(
            request.headers().get("authorization").and_then(|v| v.to_str().ok()),
            Some("Bearer sk-test-key")
        );
        assert_eq!(
            request.headers().get("content-type").and_then(|v| v.to_str().ok()),
            Some("application/json")
        );
    }

    #[test]
    fn body_maps_system_history_calls_and_config() {
        let mut config = GenerationConfig::chat_defaults("gemini-2.5-pro");
        config.temperature = Some(0.2);
        config.top_p = Some(0.9);
        config.max_output_tokens = Some(128);
        config.stop_sequences = Some(vec!["END".into()]);

        let request = GenerateContentRequest {
            contents: vec![
                Content::user_text("hello"),
                Content::model(vec![
                    Part::text("sure"),
                    Part {
                        function_call: Some(FunctionCall {
                            id: Some("call_abc".into()),
                            name: "read_file".into(),
                            args: json!({ "file_path": "a.rs" }),
                        }),
                        ..Default::default()
                    },
                ]),
                Content {
                    role: Some(Role::User),
                    parts: vec![Part::function_response(FunctionResponse {
                        id: Some("call_abc".into()),
                        name: "read_file".into(),
                        response: json!({ "output": "fn main() {}" }),
                    })],
                },
            ],
            system_instruction: Some(Content::user_text("be brief")),
            generation_config: Some(config),
            ..Default::default()
        };

        let body = wire().build_body(&request);
        assert_eq!(body["model"], "gpt-4o-mini");
        assert_eq!(body["stream"], json!(true));
        assert_eq!(body["stream_options"]["include_usage"], json!(true));

        let messages = body["messages"].as_array().expect("messages");
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "be brief");
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[1]["content"], "hello");

        assert_eq!(messages[2]["role"], "assistant");
        assert_eq!(messages[2]["content"], "sure");
        assert_eq!(messages[2]["tool_calls"][0]["id"], "call_abc");
        assert_eq!(messages[2]["tool_calls"][0]["type"], "function");
        assert_eq!(messages[2]["tool_calls"][0]["function"]["name"], "read_file");
        // Arguments are a JSON string on this wire, not an object.
        assert_eq!(
            messages[2]["tool_calls"][0]["function"]["arguments"],
            serde_json::to_string(&json!({ "file_path": "a.rs" })).expect("args")
        );

        assert_eq!(messages[3]["role"], "tool");
        assert_eq!(messages[3]["tool_call_id"], "call_abc");
        assert_eq!(messages[3]["content"], "fn main() {}");

        assert!((body["temperature"].as_f64().expect("temperature") - 0.2).abs() < 1e-6);
        assert!((body["top_p"].as_f64().expect("top_p") - 0.9).abs() < 1e-6);
        assert_eq!(body["max_tokens"], 128);
        assert_eq!(body["stop"], json!(["END"]));
        // Code Assist concepts with no equivalent here.
        assert!(body.get("top_k").is_none());
        assert!(body.get("thinkingConfig").is_none());
        assert!(body.get("responseMimeType").is_none());
        assert!(body.get("response_format").is_none());
    }

    #[test]
    fn tool_results_never_merge_into_the_user_text() {
        let request = GenerateContentRequest {
            contents: vec![Content {
                role: Some(Role::User),
                parts: vec![
                    Part::text("here is the result"),
                    Part::function_response(FunctionResponse {
                        id: Some("call_1".into()),
                        name: "read_file".into(),
                        response: json!({ "error": "file not found" }),
                    }),
                ],
            }],
            ..Default::default()
        };

        let body = wire().build_body(&request);
        let messages = body["messages"].as_array().expect("messages");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"], "here is the result");
        assert_eq!(messages[1]["role"], "tool");
        assert_eq!(messages[1]["tool_call_id"], "call_1");
        assert_eq!(messages[1]["content"], "file not found");
    }

    #[test]
    fn a_call_only_turn_sends_null_content_and_empty_turns_are_dropped() {
        let request = GenerateContentRequest {
            contents: vec![
                Content::model(vec![]),
                Content::model(vec![Part {
                    function_call: Some(FunctionCall {
                        id: Some("call_x".into()),
                        name: "list_directory".into(),
                        args: json!({ "dir_path": "." }),
                    }),
                    ..Default::default()
                }]),
            ],
            ..Default::default()
        };

        let body = wire().build_body(&request);
        let messages = body["messages"].as_array().expect("messages");
        // The empty model turn is not sent; the call-only one is, with a null content.
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "assistant");
        assert_eq!(messages[0]["content"], Value::Null);
        assert_eq!(messages[0]["tool_calls"][0]["id"], "call_x");
    }

    #[test]
    fn reasoning_in_history_is_not_replayed_as_assistant_text() {
        let request = GenerateContentRequest {
            contents: vec![Content::model(vec![
                Part { text: Some("thinking".into()), thought: Some(true), ..Default::default() },
                Part::text("answer"),
            ])],
            ..Default::default()
        };
        let body = wire().build_body(&request);
        let messages = body["messages"].as_array().expect("messages");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["content"], "answer");
    }

    #[test]
    fn tool_schemas_are_sanitized_to_object_roots() {
        let request = GenerateContentRequest {
            contents: vec![Content::user_text("hi")],
            tools: Some(declarations(&[
                (
                    "mcp__files__read",
                    json!({ "type": "object", "properties": { "path": { "type": "string" } }, "required": ["path"] }),
                ),
                // A root that cannot describe arguments at all.
                ("broken", json!({ "type": "string" })),
                // A union root with `null`, which the provider would reject.
                ("nullable", json!({ "type": ["object", "null"], "properties": { "a": { "type": "integer" } } })),
            ])),
            ..Default::default()
        };

        let body = wire().build_body(&request);
        let tools = body["tools"].as_array().expect("tools");
        assert_eq!(tools.len(), 3);
        for tool in tools {
            assert_eq!(tool["type"], "function");
            assert_eq!(tool["function"]["parameters"]["type"], "object");
            assert!(tool["function"]["parameters"]["properties"].is_object());
        }
        assert_eq!(tools[0]["function"]["name"], "mcp__files__read");
        assert_eq!(tools[0]["function"]["parameters"]["required"], json!(["path"]));
        assert_eq!(tools[0]["function"]["description"], "mcp__files__read description");
    }

    #[test]
    fn nameless_and_duplicate_declarations_are_skipped() {
        let request = GenerateContentRequest {
            contents: vec![Content::user_text("hi")],
            tools: Some(declarations(&[
                ("read_file", json!({ "type": "object", "properties": {} })),
                ("read_file", json!({ "type": "object", "properties": {} })),
                ("   ", json!({ "type": "object", "properties": {} })),
            ])),
            ..Default::default()
        };
        let body = wire().build_body(&request);
        assert_eq!(body["tools"].as_array().expect("tools").len(), 1);
    }

    #[test]
    fn tools_are_omitted_when_there_are_none() {
        let body = wire().build_body(&GenerateContentRequest {
            contents: vec![Content::user_text("hi")],
            ..Default::default()
        });
        assert!(body.get("tools").is_none());

        let body = wire().build_body(&GenerateContentRequest {
            contents: vec![Content::user_text("hi")],
            tools: Some(vec![Tool { function_declarations: vec![] }]),
            ..Default::default()
        });
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn profile_cap_applies_only_when_the_request_names_none() {
        let mut capped = profile();
        capped.max_output_tokens = Some(512);
        let capped = Wire::new(&capped);

        let body = capped.build_body(&GenerateContentRequest::default());
        assert_eq!(body["max_tokens"], 512);

        let request = GenerateContentRequest {
            generation_config: Some(GenerationConfig { max_output_tokens: Some(64), ..Default::default() }),
            ..Default::default()
        };
        assert_eq!(capped.build_body(&request)["max_tokens"], 64);
    }

    #[test]
    fn text_deltas_stream_in_order_and_stop_maps_to_stop() {
        let mut state = Decoder::new();
        let chunks = decode_all(
            &wire(),
            &mut state,
            &[
                r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":""}}]}"#,
                r#"{"choices":[{"index":0,"delta":{"content":"Hel"}}]}"#,
                r#"{"choices":[{"index":0,"delta":{"content":"lo"},"finish_reason":null}]}"#,
                r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            ],
        );
        assert_eq!(descriptions(&chunks), vec!["text:Hel", "text:lo", "finish:STOP"]);
    }

    #[test]
    fn tool_arguments_assemble_from_fragments_and_flush_at_the_finish_reason() {
        let mut state = Decoder::new();
        let chunks = decode_all(
            &wire(),
            &mut state,
            &[
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"read_file","arguments":""}}]}}]}"#,
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"file_"}}]}}]}"#,
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"path\":\"a.rs\"}"}}]}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            ],
        );
        assert_eq!(
            descriptions(&chunks),
            vec![r#"call:read_file:call_1:{"file_path":"a.rs"}"#, "finish:STOP"]
        );
        // Nothing is left buffered, so end of stream adds nothing.
        assert!(wire().finish(&mut state).expect("finish").is_empty());
    }

    #[test]
    fn indexed_calls_with_gaps_come_out_in_wire_order() {
        let mut state = Decoder::new();
        let chunks = decode_all(
            &wire(),
            &mut state,
            &[
                r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"id":"b","function":{"name":"glob","arguments":"{}"}},{"index":0,"id":"a","function":{"name":"grep_search","arguments":"{\"pattern\":\"x\"}"}}]}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            ],
        );
        assert_eq!(
            descriptions(&chunks),
            vec![
                r#"call:grep_search:a:{"pattern":"x"}"#.to_owned(),
                r#"call:glob:b:{}"#.to_owned(),
                "finish:STOP".to_owned(),
            ]
        );
    }

    #[test]
    fn interleaved_text_and_tool_calls_keep_their_order() {
        let mut state = Decoder::new();
        let chunks = decode_all(
            &wire(),
            &mut state,
            &[
                r#"{"choices":[{"delta":{"content":"Let me look.","tool_calls":[{"index":0,"id":"call_z","function":{"name":"read_file","arguments":"{\"file_path\":\"a\"}"}}]}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            ],
        );
        assert_eq!(
            descriptions(&chunks),
            vec![
                "text:Let me look.".to_owned(),
                r#"call:read_file:call_z:{"file_path":"a"}"#.to_owned(),
                "finish:STOP".to_owned(),
            ]
        );
    }

    #[test]
    fn usage_maps_to_prompt_and_completion_tokens() {
        let mut state = Decoder::new();
        let chunks = decode_all(
            &wire(),
            &mut state,
            &[
                r#"{"choices":[{"delta":{"content":"x"}}]}"#,
                r#"{"choices":[],"usage":{"prompt_tokens":11,"completion_tokens":7,"total_tokens":18}}"#,
            ],
        );
        assert_eq!(descriptions(&chunks), vec!["text:x", "usage:11:7"]);
        let usage = chunks[1].inner().usage_metadata.expect("usage");
        assert_eq!(usage.total_token_count, 18);

        // An all-zero usage frame carries nothing and is dropped.
        let zero = decode_all(&wire(), &mut state, &[r#"{"choices":[],"usage":{"prompt_tokens":0,"completion_tokens":0}}"#]);
        assert!(zero.is_empty());
    }

    #[test]
    fn every_known_finish_reason_maps_to_our_vocabulary() {
        for (wire_reason, ours) in [
            ("length", "MAX_TOKENS"),
            ("stop", "STOP"),
            ("tool_calls", "STOP"),
            ("content_filter", "SAFETY"),
            ("vendor_specific", "VENDOR_SPECIFIC"),
        ] {
            let mut state = Decoder::new();
            let frame = format!(r#"{{"choices":[{{"delta":{{}},"finish_reason":"{wire_reason}"}}]}}"#);
            let chunks = decode_all(&wire(), &mut state, &[frame.as_str()]);
            assert_eq!(descriptions(&chunks), vec![format!("finish:{ours}")]);
        }
    }

    #[test]
    fn error_frames_become_retryable_or_decode_errors() {
        let mut state = Decoder::new();
        let rate = wire()
            .decode(&mut state, r#"{"error":{"message":"Rate limit reached","type":"rate_limit_exceeded"}}"#);
        match rate {
            Err(CaError::Retryable { delay: None }) => {}
            other => panic!("expected retryable, got {other:?}"),
        }
        let server = wire()
            .decode(&mut state, r#"{"error":{"message":"overloaded","type":"server_error"}}"#);
        assert!(matches!(server, Err(CaError::Retryable { .. })));

        let invalid = wire()
            .decode(&mut state, r#"{"error":{"message":"bad model","type":"invalid_request_error"}}"#);
        match invalid {
            Err(CaError::Decode(message)) => assert_eq!(message, "bad model"),
            other => panic!("expected decode error, got {other:?}"),
        }

        // A long message is truncated, and a key echoed back is removed.
        let long = format!("{} sk-test-key", "x".repeat(2000));
        let frame = json!({ "error": { "message": long, "type": "invalid_request_error" } }).to_string();
        match wire().decode(&mut state, &frame) {
            Err(CaError::Decode(message)) => {
                assert!(!message.contains("sk-test-key"));
                assert!(message.chars().count() <= MAX_ERROR_CHARS + 3);
                assert!(message.ends_with("..."));
            }
            other => panic!("expected decode error, got {other:?}"),
        }
    }

    #[test]
    fn malformed_and_unknown_frames_are_skipped_without_panicking() {
        let mut state = Decoder::new();
        for frame in [
            "",
            "   ",
            "[not json",
            "{}",
            r#"{"id":"abc","object":"chat.completion.chunk","choices":[]}"#,
            r#"{"choices":[{"delta":{"content":5}}]}"#,
            r#"{"choices":"none"}"#,
            r#"{"choices":[{"delta":{"tool_calls":"nope"}}]}"#,
        ] {
            let chunks = wire().decode(&mut state, frame).expect("frame is tolerated");
            assert!(chunks.is_empty(), "frame {frame:?} produced {chunks:?}");
        }
        assert!(state.is_empty());
    }

    #[test]
    fn a_whole_message_object_is_accepted_in_place_of_a_delta() {
        let mut state = Decoder::new();
        let chunks = decode_all(
            &wire(),
            &mut state,
            &[r#"{"choices":[{"message":{"role":"assistant","content":"full"},"finish_reason":"stop"}]}"#],
        );
        assert_eq!(descriptions(&chunks), vec!["text:full", "finish:STOP"]);
    }

    #[test]
    fn finish_flushes_buffered_calls_and_is_idempotent() {
        let mut state = Decoder::new();
        let chunks = decode_all(
            &wire(),
            &mut state,
            &[
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"write_file"}}]}}]}"#,
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"file_path\":\"a\",\"content\":\"b\"}"}}]}}]}"#,
            ],
        );
        assert!(chunks.is_empty(), "nothing is released before the stream ends");

        let flushed = wire().finish(&mut state).expect("finish");
        assert_eq!(flushed.len(), 1);
        let call = flushed[0].first_parts()[0].function_call.clone().expect("call");
        assert_eq!(call.name, "write_file");
        let id = call.id.expect("synthesized id");
        assert!(id.starts_with("call_"), "unexpected id {id}");
        assert_eq!(call.args["file_path"], json!("a"));

        assert!(wire().finish(&mut state).expect("second finish").is_empty());
    }

    #[test]
    fn the_done_sentinel_flushes_buffered_calls() {
        let mut state = Decoder::new();
        let chunks = decode_all(
            &wire(),
            &mut state,
            &[
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_d","function":{"name":"glob","arguments":"{\"pattern\":\"*\"}"}}]}}]}"#,
                "[DONE]",
            ],
        );
        assert_eq!(descriptions(&chunks), vec![r#"call:glob:call_d:{"pattern":"*"}"#]);
        assert!(wire().finish(&mut state).expect("finish").is_empty());
    }

    #[test]
    fn an_absurd_tool_index_is_refused_instead_of_allocated() {
        let mut state = Decoder::new();
        let frame = r#"{"choices":[{"delta":{"tool_calls":[{"index":1000000000,"function":{"name":"x","arguments":"{}"}}]}}]}"#;
        assert!(wire().decode(&mut state, frame).expect("tolerated").is_empty());
        assert!(wire().finish(&mut state).expect("finish").is_empty());
    }

    #[test]
    fn a_repeated_name_fragment_is_not_concatenated() {
        let mut state = Decoder::new();
        let chunks = decode_all(
            &wire(),
            &mut state,
            &[
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_r","function":{"name":"read_file","arguments":"{}"}}]}}]}"#,
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_r","function":{"name":"read_file","arguments":""}}]}}]}"#,
            ],
        );
        assert!(chunks.is_empty());
        let flushed = wire().finish(&mut state).expect("finish");
        let call = flushed[0].first_parts()[0].function_call.clone().expect("call");
        assert_eq!(call.name, "read_file");
    }

    #[test]
    fn an_orphan_tool_result_is_dropped_rather_than_sent_with_an_empty_id() {
        let request = GenerateContentRequest {
            contents: vec![Content {
                role: Some(Role::User),
                parts: vec![Part::function_response(FunctionResponse {
                    id: None,
                    name: "read_file".into(),
                    response: json!({ "output": "x" }),
                })],
            }],
            ..Default::default()
        };
        let body = wire().build_body(&request);
        assert!(body["messages"].as_array().expect("messages").is_empty());
    }

    #[test]
    fn a_response_without_an_id_attaches_to_the_call_of_the_same_name() {
        let request = GenerateContentRequest {
            contents: vec![
                Content::model(vec![Part {
                    function_call: Some(FunctionCall {
                        id: Some("call_kept".into()),
                        name: "read_file".into(),
                        args: json!({ "file_path": "a" }),
                    }),
                    ..Default::default()
                }]),
                Content {
                    role: Some(Role::User),
                    parts: vec![Part::function_response(FunctionResponse {
                        id: None,
                        name: "read_file".into(),
                        response: json!({ "output": "ok" }),
                    })],
                },
            ],
            ..Default::default()
        };
        let body = wire().build_body(&request);
        let messages = body["messages"].as_array().expect("messages");
        assert_eq!(messages[1]["tool_call_id"], "call_kept");
    }
}
