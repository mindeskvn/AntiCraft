//! Gemini Developer API dialect (`POST {base}/models/{model}:streamGenerateContent?alt=sse`).
//!
//! The translation here is mostly deletion, which is why the file can be small and still correct.
//! Our `GenerateContentRequest`/`GenerateContentResponse` types were copied from Code Assist, and
//! Code Assist is itself Gemini JSON: the same `contents`, `systemInstruction`, `tools` and
//! `usageMetadata`, wrapped in an envelope. Reusing them means the public API accepts our body
//! nearly as-is, and the only work is removing what Code Assist tolerates and the public API does
//! not:
//!
//! - the `session_id` request field, which belongs to the Code Assist envelope;
//! - a `thinkingConfig` on a model that does not think (`chat_defaults` attaches one to every
//!   model, and the public API answers 400 rather than ignoring it);
//! - the `skip_thought_signature_validator` placeholder that `History::for_request` inserts for
//!   Code Assist, and the `id` on a `functionResponse`, neither of which this API knows.
//!
//! On the way back, each SSE event is a bare `GenerateContentResponse` rather than
//! `{"response": ...}`, so decoding is one unwrap plus a per-part pass that holds back the rare
//! split function call and flushes it at end of stream.

use serde_json::{Map, Value};

use crate::agent::history::SKIP_THOUGHT_SIGNATURE;
use crate::api::{
    CaError, CaGenerateResponse, Candidate, Content, GenerateContentRequest, GenerateContentResponse, Part, Role,
};
use crate::providers::common::{
    part_chunk, percent_encode, synthesize_call_id, Decoder, Dialect, PartialCall, WireAuth, MAX_ARG_BYTES,
    MAX_EVENT_BYTES,
};
use crate::providers::ProviderProfile;

/// Model families whose `thinkingConfig` the public API understands. Read off
/// [`crate::api::GenerationConfig::chat_defaults`]: it attaches `thinkingLevel` for `gemini-3*`
/// and an 8192-token `thinkingBudget` for everything else, but only these two families accept
/// either field. Any other model needs the block removed, not adjusted.
const THINKING_MODEL_PREFIXES: [&str; 2] = ["gemini-3", "gemini-2.5"];

/// Slot spacing used to key a streamed part by (candidate, position).
///
/// `Decoder` indexes slots by one number, and a function call is identified on the wire by its
/// candidate and its position inside that candidate. Multiplying the candidate by this stride
/// keeps the two apart without adding a second table; no real reply carries a thousand parts.
const SLOT_STRIDE: usize = 1024;

/// Largest single event this dialect will parse. The shared parser already enforces
/// [`MAX_EVENT_BYTES`] per event; checking again costs nothing and keeps `decode` safe when a
/// caller hands it a whole response, as the tests do.
const MAX_PAYLOAD_BYTES: usize = MAX_EVENT_BYTES;

/// One profile rendered as the Gemini Developer API.
pub struct Wire {
    base_url: String,
    /// Normalized model id: no `models/` prefix, no `:method` suffix.
    model: String,
    api_key: String,
    /// Applied only when the request carries no cap of its own.
    max_output_tokens: Option<u32>,
    /// Input to the synthesized tool-call ids; stable across a retry of the same turn.
    seed: String,
}

impl Wire {
    /// Copy what the dialect needs out of the profile, normalized once.
    ///
    /// Users paste model ids in every shape they have seen them (`models/gemini-2.5-pro`,
    /// `gemini-2.5-pro:generateContent`). The endpoint adds the `models/` prefix and the method
    /// suffix itself, so those are stripped here rather than producing a 404 later.
    pub fn new(profile: &ProviderProfile) -> Self {
        let raw = profile.model.trim();
        let model = raw
            .split_once(':')
            .map_or(raw, |(head, _)| head)
            .strip_prefix("models/")
            .unwrap_or_else(|| raw.split_once(':').map_or(raw, |(head, _)| head))
            .trim()
            .to_owned();
        Self {
            base_url: profile.resolved_base_url(),
            model,
            api_key: profile.api_key.trim().to_owned(),
            max_output_tokens: profile.max_output_tokens,
            seed: format!("{}|{}", profile.kind.as_str(), profile.model.trim()),
        }
    }

    /// Whether this model accepts a thinking config.
    fn accepts_thinking(&self) -> bool {
        accepts_thinking(&self.model)
    }
}

/// Whether a model family understands `generationConfig.thinkingConfig`.
///
/// Read off [`crate::api::GenerationConfig::chat_defaults`]: it attaches `thinkingLevel` for
/// `gemini-3*` and an 8192-token `thinkingBudget` for everything else, but only these two families
/// accept either field. Any other model needs the block removed, not adjusted — the API answers
/// 400 rather than ignoring it.
///
/// Shared with the Antigravity dialect, whose backend is the same Gemini JSON and has the same rule.
pub(crate) fn accepts_thinking(model: &str) -> bool {
    THINKING_MODEL_PREFIXES.iter().any(|p| model.starts_with(p))
}

impl Dialect for Wire {
    /// `{base}/models/{model}:streamGenerateContent?alt=sse&key={api_key}`.
    ///
    /// The key goes in the query string because that is the Developer API's own convention; the
    /// same form is used for a custom `base_url` (a proxy or a local server), since a compatible
    /// gateway is more likely to accept `?key=` than a header it never documented. The shared
    /// driver keeps the URL out of error messages, which is what makes a secret in the URL safe
    /// to carry at all.
    fn endpoint(&self) -> String {
        format!(
            "{}/models/{}:streamGenerateContent?alt=sse&key={}",
            self.base_url.trim_end_matches('/'),
            percent_encode(&self.model),
            percent_encode(&self.api_key)
        )
    }

    /// JSON in, JSON out; the credential is already in the URL.
    ///
    /// An OAuth account never reaches this dialect: [`super::ProviderBackend::dialect`] only builds
    /// it for a key-based profile, because the Gemini Developer API takes a key in the query string
    /// and has nothing to do with a subscription token. The assertion records that.
    fn apply_headers(&self, rb: reqwest::RequestBuilder, auth: WireAuth<'_>) -> reqwest::RequestBuilder {
        debug_assert!(matches!(auth, WireAuth::Key(_)), "the Gemini dialect carries its key in the URL");
        let _ = auth;
        rb.header(reqwest::header::CONTENT_TYPE, "application/json")
    }

    /// Our request as the public API's body. See the module docs for every field removed.
    fn build_body(&self, req: &GenerateContentRequest) -> Value {
        let mut body = serde_json::to_value(req).unwrap_or_else(|_| Value::Object(Map::new()));
        let Value::Object(map) = &mut body else {
            return Value::Object(Map::new());
        };
        // Code Assist only. Removed under both spellings so a hand-edited request cannot leak it.
        map.remove("session_id");
        map.remove("sessionId");

        if let Some(Value::Array(contents)) = map.get_mut("contents") {
            for content in contents.iter_mut() {
                project_content(content);
            }
        }

        match map.get_mut("generationConfig") {
            Some(Value::Object(config)) => {
                if !self.accepts_thinking() {
                    config.remove("thinkingConfig");
                }
                // The profile cap is a fallback. A request that already chose a value keeps it.
                if let Some(cap) = self.max_output_tokens {
                    if !config.contains_key("maxOutputTokens") {
                        config.insert("maxOutputTokens".to_owned(), Value::from(cap));
                    }
                }
            }
            _ if self.max_output_tokens.is_some() => {
                let cap = self.max_output_tokens.unwrap_or_default();
                map.insert(
                    "generationConfig".to_owned(),
                    Value::Object(Map::from_iter([("maxOutputTokens".to_owned(), Value::from(cap))])),
                );
            }
            _ => {}
        }

        // `tools`, `toolConfig` and `safetySettings` are already in the public shape:
        // `[{functionDeclarations: [{name, description, parametersJsonSchema}]}]`. Every built-in
        // tool, orchestration tool, skill, rule and MCP tool reaches us in that one list, and
        // `mcp__server__tool` names are accepted unchanged, so nothing is filtered here.
        body
    }

    /// One SSE payload is one complete `GenerateContentResponse`, or a status error.
    fn decode(&self, state: &mut Decoder, payload: &str) -> Result<Vec<CaGenerateResponse>, CaError> {
        let trimmed = payload.trim();
        // Gemini does not send the OpenAI terminator, but a proxy in front of it might.
        if trimmed.is_empty() || trimmed == "[DONE]" {
            return Ok(Vec::new());
        }
        if trimmed.len() > MAX_PAYLOAD_BYTES {
            return Err(CaError::Decode("Gemini event exceeded the size limit".into()));
        }

        let object = match serde_json::from_str::<Value>(trimmed) {
            Ok(Value::Object(map)) => map,
            Ok(_) => return Err(CaError::Decode("Gemini event is not a JSON object".into())),
            Err(e) => {
                // Never echo the payload: it may be a fragment of the user's own prompt.
                return Err(CaError::Decode(format!(
                    "invalid Gemini event ({} bytes): {e}",
                    trimmed.len()
                )))
            }
        };
        decode_gemini_value(state, object, &self.seed)
    }

    /// End of stream: hand over calls whose name or arguments never completed.
    ///
    /// `PartialCall::into_call` degrades unparseable arguments to `{}`. That is the recoverable
    /// outcome: the tool reports the missing field and the model retries, while dropping the call
    /// would leave a function call in history with no response.
    fn finish(&self, state: &mut Decoder) -> Result<Vec<CaGenerateResponse>, CaError> {
        finish_gemini(state, &self.seed)
    }
}

/// Walk one already-parsed Gemini JSON object and produce the loop's chunks.
///
/// Extracted from [`Wire::decode`] so the Antigravity dialect — whose stream is the same Gemini
/// JSON inside a different envelope — reuses the part walking, the function-call assembly and the
/// usage attachment instead of keeping a second copy that would drift. `seed` is the per-turn id
/// seed, so calls a dialect has to synthesize stay stable across a retry.
pub(crate) fn decode_gemini_value(
    state: &mut Decoder,
    object: Map<String, Value>,
    seed: &str,
) -> Result<Vec<CaGenerateResponse>, CaError> {
    decode_gemini_value_with(state, object, seed, &mut |_| None)
}

/// [`decode_gemini_value`] that first shows `keep` every wire part, in order. A part `keep`
/// returns goes to the loop ahead of that wire part's own output, and `keep` may edit the wire
/// part before it is read. The Antigravity dialect keeps a Claude model's signed thinking this
/// way, as a [`Part::provider_block`] the loop stores where the thinking was.
pub(crate) fn decode_gemini_value_with(
    state: &mut Decoder,
    mut object: Map<String, Value>,
    seed: &str,
    keep: &mut dyn FnMut(&mut Part) -> Option<Part>,
) -> Result<Vec<CaGenerateResponse>, CaError> {
    if let Some(error) = object.remove("error") {
        return Err(map_rpc_error(&error));
    }
    let mut response: GenerateContentResponse = serde_json::from_value(Value::Object(object))
        .map_err(|e| CaError::Decode(format!("invalid Gemini response: {e}")))?;

    if let Some(reason) = blocked_reason(&response) {
        return Err(CaError::Decode(reason));
    }

    let mut out = Vec::new();
    for (candidate_index, candidate) in response.candidates.iter_mut().enumerate() {
        let mut visible: Vec<Part> = Vec::new();
        let parts = candidate.content.as_mut().map(|c| c.parts.as_mut_slice()).unwrap_or_default();
        for (position, part) in parts.iter_mut().enumerate() {
            visible.extend(keep(part));
            if part.function_call.is_some() {
                // A whole call normally arrives at once, so it is emitted without waiting for
                // the stream to end; the loop executes tools sooner and the turn finishes
                // sooner. Only a call that is still missing its name or arguments waits.
                let slot = candidate_index.saturating_mul(SLOT_STRIDE).saturating_add(position);
                if let Some(ready) = absorb_call(state, slot, part, seed) {
                    visible.push(ready);
                }
            } else if part.text.as_deref().is_some_and(|t| !t.is_empty()) {
                // A provider block is made by a dialect, never taken from the wire: one would be
                // stored and sent back as a field this API does not know.
                visible.push(Part { provider_block: None, ..part.clone() });
            }
        }
        if visible.is_empty() {
            continue;
        }
        out.push(CaGenerateResponse {
            response: Some(GenerateContentResponse {
                candidates: vec![Candidate {
                    content: Some(Content { role: Some(Role::Model), parts: visible }),
                    finish_reason: candidate.finish_reason.clone(),
                    index: Some(candidate_index as u32),
                }],
                ..Default::default()
            }),
            ..Default::default()
        });
    }

    // Usage arrives on the last event, often with no candidate beside it. Attaching it to the
    // last chunk keeps one usage report per turn rather than one per text delta.
    if let Some(usage) = response.usage_metadata {
        match out.last_mut().and_then(|c| c.response.as_mut()) {
            Some(last) => last.usage_metadata = Some(usage),
            None => out.push(CaGenerateResponse {
                response: Some(GenerateContentResponse {
                    usage_metadata: Some(usage),
                    ..Default::default()
                }),
                ..Default::default()
            }),
        }
    }

    // An event with no visible part and no usage (a lone `finishReason`, an empty heartbeat)
    // is not an error; there is simply nothing to show.
    Ok(out)
}

/// End of stream for a Gemini-shaped stream: hand over calls whose arguments never completed.
///
/// See [`Wire::finish`]; shared with the Antigravity dialect for the same reason as
/// [`decode_gemini_value`].
pub(crate) fn finish_gemini(state: &mut Decoder, seed: &str) -> Result<Vec<CaGenerateResponse>, CaError> {
    let pending = state.take_all();
    Ok(pending
        .into_iter()
        .enumerate()
        .map(|(i, call)| part_chunk(call.into_part(synthesize_call_id(seed, i))))
        .collect())
}

// ---------------------------------------------------------------------------
// Request projection
// ---------------------------------------------------------------------------

/// Remove the two fields a Code Assist request carries and this API rejects.
fn project_content(content: &mut Value) {
    let Some(content) = content.as_object_mut() else { return };
    let Some(Value::Array(parts)) = content.get_mut("parts") else { return };
    for part in parts.iter_mut() {
        let Some(part) = part.as_object_mut() else { continue };
        // The placeholder tells Code Assist to skip signature validation. The public API treats a
        // signature as real, so leaving the placeholder in fails a multi-turn tool exchange.
        if part.get("thoughtSignature").and_then(Value::as_str) == Some(SKIP_THOUGHT_SIGNATURE) {
            part.remove("thoughtSignature");
        }
        if let Some(Value::Object(response)) = part.get_mut("functionResponse") {
            // The public API pairs a response to its call by name and order, not by id.
            response.remove("id");
        }
    }
}

// ---------------------------------------------------------------------------
// Response decoding
// ---------------------------------------------------------------------------

/// Fold one wire part into its slot and return the part when the call is now complete.
///
/// A complete first fragment is the common case and returns immediately. A fragment that lacks a
/// name or carries arguments as text stays buffered; the next fragment for the same slot merges
/// into it, and [`finish_gemini`] flushes whatever is left.
///
/// The part's `thoughtSignature` goes with the call: Gemini 3 signs the first call of a turn and
/// expects the signature back on it, and a call stored without one is sent unsigned next time.
fn absorb_call(state: &mut Decoder, slot: usize, part: &Part, seed: &str) -> Option<Part> {
    let call = part.function_call.as_ref()?;
    let complete = |c: &PartialCall| {
        !c.name.is_empty()
            && (c.args.trim().is_empty()
                || serde_json::from_str::<Value>(&c.args).is_ok_and(|v| v.is_object()))
    };
    {
        let pending = state.slot(slot);
        if let Some(id) = &call.id {
            pending.id = Some(id.clone());
        }
        if pending.thought_signature.is_none() {
            pending.thought_signature.clone_from(&part.thought_signature);
        }
        if !call.name.is_empty() {
            pending.name.clone_from(&call.name);
        }
        match &call.args {
            Value::String(fragment) => {
                // Same cap as the other dialects: one runaway slot must not become an unbounded
                // allocation, and a truncated body degrades to `{}` rather than aborting the turn.
                let room = MAX_ARG_BYTES.saturating_sub(pending.args.len());
                if fragment.len() > room {
                    tracing::warn!(tool = %pending.name, "tool arguments exceeded the size cap; keeping the prefix");
                    pending.args.push_str(truncate_bytes(fragment, room));
                } else {
                    pending.args.push_str(fragment);
                }
            }
            // A missing `args` is an empty argument object, not a fragment.
            Value::Null => {}
            Value::Object(map) if map.is_empty() => {}
            other => pending.args = other.to_string(),
        }
        if !complete(pending) {
            return None;
        }
    }
    let done = state.take(slot).unwrap_or_default();
    Some(done.into_part(synthesize_call_id(seed, slot)))
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

/// Render an in-stream `{"error": {...}}` as the error the agent loop already understands.
///
/// A status can arrive after a 200 because generation failed mid-stream (a safety policy, a quota
/// hit, an argument the server only validates later). A 429 must stay retryable and a 403 must
/// stay an auth problem, or the loop retries something it should not, or gives up on something it
/// should retry. A 429 for a quota that retrying cannot clear (a daily limit, `limit: 0`, a long
/// `RetryInfo`) carries the same `google.rpc` details as Code Assist, so it is read the same way.
///
/// The driver scrubs the key from whatever message this returns.
fn map_rpc_error(error: &Value) -> CaError {
    let code = error.get("code").and_then(Value::as_u64).unwrap_or(0) as u16;
    let status = error.get("status").and_then(Value::as_str).unwrap_or("");
    let message = error.get("message").and_then(Value::as_str).unwrap_or(status);
    match code {
        401 | 403 => CaError::Auth(message.to_owned()),
        404 => CaError::NotFound,
        429 => match crate::api::client::classify_error(429, &serde_json::json!({ "error": error }).to_string()) {
            terminal @ CaError::TerminalQuota { .. } => terminal,
            CaError::Retryable { delay } => {
                CaError::Retryable { delay: delay.filter(|d| *d <= crate::api::client::MAX_RETRY_DELAY) }
            }
            _ => CaError::Retryable { delay: None },
        },
        500..=599 => CaError::Retryable { delay: None },
        _ if message.is_empty() => CaError::Decode(format!("Gemini error {code} {status}")),
        _ => CaError::Decode(message.to_owned()),
    }
}

/// `promptFeedback.blockReason` as a message the user can act on, when the prompt was refused.
fn blocked_reason(response: &GenerateContentResponse) -> Option<String> {
    let feedback = response.prompt_feedback.as_ref()?;
    let reason = feedback.get("blockReason").and_then(Value::as_str)?;
    if reason.is_empty() || reason == "BLOCK_REASON_UNSPECIFIED" {
        return None;
    }
    match feedback.get("blockReasonMessage").and_then(Value::as_str) {
        Some(detail) if !detail.is_empty() => Some(format!("Gemini blocked the prompt: {reason}: {detail}")),
        _ => Some(format!("Gemini blocked the prompt: {reason}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{FunctionCall, FunctionDeclaration, FunctionResponse, GenerationConfig, Tool};
    use crate::providers::ProviderKind;
    use serde_json::json;

    fn wire(model: &str) -> Wire {
        Wire::new(&ProviderProfile {
            id: "g".into(),
            label: "Gemini".into(),
            kind: ProviderKind::GeminiApi,
            model: model.into(),
            base_url: None,
            api_key: "AIza test-key".into(),
            max_output_tokens: None,
            context_window: None,
            models: Vec::new(),
        })
    }

    fn full_request() -> GenerateContentRequest {
        GenerateContentRequest {
            contents: vec![
                Content::user_text("hello"),
                Content::model(vec![Part {
                    function_call: Some(FunctionCall {
                        id: Some("c1".into()),
                        name: "read_file".into(),
                        args: json!({"file_path": "a.rs"}),
                    }),
                    ..Default::default()
                }]),
                Content {
                    role: Some(Role::User),
                    parts: vec![Part::function_response(FunctionResponse {
                        id: Some("c1".into()),
                        name: "read_file".into(),
                        response: json!({"output": "fn main() {}"}),
                    })],
                },
            ],
            system_instruction: Some(Content::user_text("be terse")),
            tools: Some(vec![Tool {
                function_declarations: vec![FunctionDeclaration {
                    name: "read_file".into(),
                    description: "Reads a file.".into(),
                    parameters_json_schema: json!({"type":"object","properties":{"file_path":{"type":"string"}},"required":["file_path"]}),
                }],
            }]),
            generation_config: Some(GenerationConfig::chat_defaults("gemini-2.5-pro")),
            session_id: Some("session-1".into()),
            ..Default::default()
        }
    }

    fn decode(w: &Wire, state: &mut Decoder, payload: &str) -> Result<Vec<CaGenerateResponse>, CaError> {
        w.decode(state, payload)
    }

    #[test]
    fn body_sends_system_history_and_tools_without_the_session() {
        let body = wire("gemini-2.5-pro").build_body(&full_request());
        assert!(body.get("session_id").is_none());
        assert!(body.get("sessionId").is_none());
        assert_eq!(body["systemInstruction"]["parts"][0]["text"], json!("be terse"));
        assert_eq!(body["contents"][0]["role"], json!("user"));
        assert_eq!(body["contents"][1]["role"], json!("model"));
        assert_eq!(body["contents"][1]["parts"][0]["functionCall"]["name"], json!("read_file"));
        assert_eq!(
            body["contents"][2]["parts"][0]["functionResponse"]["response"]["output"],
            json!("fn main() {}")
        );
        assert_eq!(
            body["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"]["required"],
            json!(["file_path"])
        );
        assert_eq!(body["generationConfig"]["thinkingConfig"]["thinkingBudget"], json!(8192));
    }

    #[test]
    fn strip_removes_the_signature_placeholder_and_response_ids() {
        let mut req = full_request();
        req.contents[1].parts[0].thought_signature = Some(SKIP_THOUGHT_SIGNATURE.into());
        req.contents[1].parts[1 - 1].text = None;
        let body = wire("gemini-2.5-pro").build_body(&req);
        assert!(body["contents"][1]["parts"][0].get("thoughtSignature").is_none());
        assert!(body["contents"][2]["parts"][0]["functionResponse"].get("id").is_none());

        // A real signature is kept: Gemini 3 needs it echoed back on a function call.
        let mut req = full_request();
        req.contents[1].parts[0].thought_signature = Some("real-signature".into());
        let body = wire("gemini-3-pro-preview").build_body(&req);
        assert_eq!(body["contents"][1]["parts"][0]["thoughtSignature"], json!("real-signature"));
    }

    #[test]
    fn thinking_is_kept_only_for_families_that_understand_it() {
        // gemini-3: thinkingLevel=HIGH (via chat_defaults for that model prefix).
        let req3 = GenerateContentRequest {
            generation_config: Some(GenerationConfig::chat_defaults("gemini-3-pro-preview")),
            ..full_request()
        };
        let kept = wire("gemini-3-pro-preview").build_body(&req3);
        assert_eq!(kept["generationConfig"]["thinkingConfig"]["thinkingLevel"], json!("HIGH"));

        // gemini-2.5: thinkingBudget=8192.
        let req25 = GenerateContentRequest {
            generation_config: Some(GenerationConfig::chat_defaults("gemini-2.5-flash")),
            ..full_request()
        };
        let kept = wire("gemini-2.5-flash").build_body(&req25);
        assert_eq!(kept["generationConfig"]["thinkingConfig"]["thinkingBudget"], json!(8192));

        // gemini-2.0: thinkingConfig must be dropped.
        let dropped = wire("gemini-2.0-flash").build_body(&full_request());
        assert!(dropped["generationConfig"].get("thinkingConfig").is_none());
        assert_eq!(dropped["generationConfig"]["temperature"], json!(1.0));
    }

    #[test]
    fn profile_token_cap_is_only_a_fallback() {
        let mut w = wire("gemini-2.5-pro");
        w.max_output_tokens = Some(4096);
        let mut req = full_request();
        let body = w.build_body(&req);
        assert_eq!(body["generationConfig"]["maxOutputTokens"], json!(4096));

        // A request that chose its own cap keeps it.
        if let Some(config) = req.generation_config.as_mut() {
            config.max_output_tokens = Some(17);
        }
        let body = w.build_body(&req);
        assert_eq!(body["generationConfig"]["maxOutputTokens"], json!(17));

        // No generation config at all: one is created rather than the cap being dropped.
        let bare = GenerateContentRequest { contents: vec![Content::user_text("x")], ..Default::default() };
        let body = w.build_body(&bare);
        assert_eq!(body["generationConfig"]["maxOutputTokens"], json!(4096));
    }

    #[test]
    fn endpoint_carries_the_model_and_a_percent_encoded_key() {
        let w = wire("models/gemini-2.5-pro:generateContent");
        assert_eq!(
            w.endpoint(),
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-pro:streamGenerateContent?alt=sse&key=AIza%20test-key"
        );

        let mut custom = wire("gemini-2.5-pro");
        custom.base_url = "http://localhost:8080/v1beta/".into();
        // Same query-param style even off the Google host; a compatible gateway accepts it.
        assert!(custom.endpoint().starts_with("http://localhost:8080/v1beta/models/gemini-2.5-pro:"));
    }

    #[test]
    fn headers_send_only_a_content_type() {
        let w = wire("gemini-2.5-pro");
        let built = w
            .apply_headers(reqwest::Client::new().post("http://example.test"), WireAuth::Key("AIza test-key"))
            .build()
            .expect("request builds");
        assert_eq!(built.headers()[reqwest::header::CONTENT_TYPE], "application/json");
        assert!(built.headers().get(reqwest::header::AUTHORIZATION).is_none());
    }

    #[test]
    fn streams_text_parts_and_keeps_thoughts() {
        let w = wire("gemini-2.5-pro");
        let mut state = Decoder::new();
        let chunks = decode(
            &w,
            &mut state,
            r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"Hel"},{"text":"lo"}]},"index":0}]}"#,
        )
        .expect("decodes");
        assert_eq!(chunks.len(), 1);
        let parts = chunks[0].first_parts();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].text.as_deref(), Some("Hel"));
        assert_eq!(chunks[0].response.as_ref().and_then(|r| r.candidates[0].index), Some(0));

        let chunks = decode(
            &w,
            &mut state,
            r#"{"candidates":[{"content":{"parts":[{"text":"plan","thought":true}]}}]}"#,
        )
        .expect("decodes");
        assert!(chunks[0].first_parts()[0].is_thought());

        // Empty text is not a chunk.
        let chunks = decode(&w, &mut state, r#"{"candidates":[{"content":{"parts":[{"text":""}]}}]}"#)
            .expect("decodes");
        assert!(chunks.is_empty());
    }

    #[test]
    fn a_complete_tool_call_is_emitted_without_waiting_for_the_end() {
        let w = wire("gemini-2.5-pro");
        let mut state = Decoder::new();
        let chunks = decode(
            &w,
            &mut state,
            r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"read_file","args":{"file_path":"a.rs"}}}]}}]}"#,
        )
        .expect("decodes");
        assert_eq!(chunks.len(), 1);
        let call = chunks[0].first_parts()[0].function_call.clone().expect("call");
        assert_eq!(call.name, "read_file");
        assert_eq!(call.args["file_path"], json!("a.rs"));
        assert!(call.id.is_some(), "an id is synthesized for the loop");
        assert!(state.is_empty(), "nothing is left buffered");

        // A call with no arguments is complete too.
        let chunks = decode(
            &w,
            &mut state,
            r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"list_directory"}}]}}]}"#,
        )
        .expect("decodes");
        let call = chunks[0].first_parts()[0].function_call.clone().expect("call");
        assert_eq!(call.args, json!({}));
    }

    #[test]
    fn text_and_calls_interleave_in_order() {
        let w = wire("gemini-2.5-pro");
        let mut state = Decoder::new();
        let chunks = decode(
            &w,
            &mut state,
            r#"{"candidates":[{"content":{"parts":[
                {"text":"Reading now."},
                {"functionCall":{"name":"read_file","args":{"file_path":"a.rs"}}},
                {"text":" and one more."}
            ]}}]}"#,
        )
        .expect("decodes");
        let parts = chunks[0].first_parts();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0].text.as_deref(), Some("Reading now."));
        assert_eq!(parts[1].function_call.as_ref().map(|c| c.name.as_str()), Some("read_file"));
        assert_eq!(parts[2].text.as_deref(), Some(" and one more."));
    }

    #[test]
    fn usage_maps_onto_the_last_chunk_or_stands_alone() {
        let w = wire("gemini-2.5-pro");
        let mut state = Decoder::new();
        let chunks = decode(
            &w,
            &mut state,
            r#"{"candidates":[{"content":{"parts":[{"text":"hi"}]}}],"usageMetadata":{"promptTokenCount":11,"candidatesTokenCount":4,"totalTokenCount":15}}"#,
        )
        .expect("decodes");
        let usage = chunks[0].response.as_ref().and_then(|r| r.usage_metadata).expect("usage");
        assert_eq!(usage.prompt_token_count, 11);
        assert_eq!(usage.total_token_count, 15);

        let chunks = decode(
            &w,
            &mut state,
            r#"{"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":1,"totalTokenCount":4}}"#,
        )
        .expect("decodes");
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].first_parts().is_empty());
        assert_eq!(chunks[0].response.as_ref().and_then(|r| r.usage_metadata).map(|u| u.total_token_count), Some(4));
    }

    #[test]
    fn in_stream_status_maps_onto_the_retry_vocabulary() {
        let w = wire("gemini-2.5-pro");
        let mut state = Decoder::new();
        let err = decode(&w, &mut state, r#"{"error":{"code":429,"message":"slow down"}}"#).unwrap_err();
        assert!(matches!(err, CaError::Retryable { delay: None }) && err.is_retryable());

        let err = decode(&w, &mut state, r#"{"error":{"code":403,"message":"bad key"}}"#).unwrap_err();
        assert!(matches!(err, CaError::Auth(ref m) if m == "bad key"));

        let err = decode(&w, &mut state, r#"{"error":{"code":400,"message":"bad field"}}"#).unwrap_err();
        assert!(matches!(err, CaError::Decode(ref m) if m == "bad field"));
    }

    #[test]
    fn a_blocked_prompt_becomes_a_readable_decode_error() {
        let w = wire("gemini-2.5-pro");
        let mut state = Decoder::new();
        let err = decode(
            &w,
            &mut state,
            r#"{"promptFeedback":{"blockReason":"SAFETY","blockReasonMessage":"violates policy"},"candidates":[]}"#,
        )
        .unwrap_err();
        match err {
            CaError::Decode(m) => {
                assert!(m.contains("SAFETY"));
                assert!(m.contains("violates policy"));
            }
            other => panic!("expected a decode error, got {other:?}"),
        }

        // The unspecified reason is not a block.
        let chunks = decode(
            &w,
            &mut state,
            r#"{"promptFeedback":{"blockReason":"BLOCK_REASON_UNSPECIFIED"},"candidates":[{"content":{"parts":[{"text":"ok"}]}}]}"#,
        )
        .expect("decodes");
        assert_eq!(chunks[0].first_parts()[0].text.as_deref(), Some("ok"));
    }

    #[test]
    fn malformed_payloads_fail_without_panicking() {
        let w = wire("gemini-2.5-pro");
        let mut state = Decoder::new();
        for payload in ["not json", "{\"candidates\": 5}", "[1,2,3]", "{\"candidates\":[{\"content\":\"x\"}]}"] {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                decode(&w, &mut state, payload)
            }));
            assert!(result.is_ok(), "decode panicked on {payload}");
            assert!(result.expect("no panic").is_err(), "malformed payload must be an error: {payload}");
        }
    }

    #[test]
    fn unknown_and_empty_events_are_ignored() {
        let w = wire("gemini-2.5-pro");
        let mut state = Decoder::new();
        for payload in ["", "   ", "[DONE]", "{}", r#"{"foo":1}"#, r#"{"candidates":[]}"#] {
            let chunks = decode(&w, &mut state, payload).expect("ignored");
            assert!(chunks.is_empty(), "{payload} should produce no chunk");
        }
    }

    #[test]
    fn finish_flushes_a_call_split_across_events() {
        let w = wire("gemini-2.5-pro");
        let mut state = Decoder::new();
        // First fragment names the tool but its arguments are still text.
        let first = decode(
            &w,
            &mut state,
            r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"grep_search","args":"{\"pat"}}]}}]}"#,
        )
        .expect("decodes");
        assert!(first.is_empty(), "an incomplete call is held back");
        assert!(!state.is_empty());

        // The rest of the arguments arrives for the same candidate and position.
        let second = decode(
            &w,
            &mut state,
            r#"{"candidates":[{"content":{"parts":[{"functionCall":{"args":"tern\":\"TODO\",\"dir_path\":\".\"}"}}]}}]}"#,
        )
        .expect("decodes");
        assert_eq!(second.len(), 1, "the completed call is emitted as soon as it parses");
        let call = second[0].first_parts()[0].function_call.clone().expect("call");
        assert_eq!(call.name, "grep_search");
        assert_eq!(call.args["pattern"], json!("TODO"));
        assert!(state.is_empty());

        // A fragment that never completes is still handed over at end of stream.
        let held = decode(
            &w,
            &mut state,
            r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"glob","args":"{\"patt"}}]}}]}"#,
        )
        .expect("decodes");
        assert!(held.is_empty());
        let flushed = w.finish(&mut state).expect("flushes");
        assert_eq!(flushed.len(), 1);
        let call = flushed[0].first_parts()[0].function_call.clone().expect("call");
        assert_eq!(call.name, "glob");
        assert_eq!(call.args, json!({}), "unparseable arguments degrade to an empty object");
        assert!(call.id.is_some());

        // A second finish has nothing left.
        assert!(w.finish(&mut state).expect("flushes").is_empty());
    }

    #[test]
    fn a_signed_function_call_keeps_its_signature() {
        // Gemini 3 signs the first call of a turn and wants the signature back with it.
        let w = wire("gemini-3-pro-preview");
        let mut state = Decoder::new();
        let chunks = decode(
            &w,
            &mut state,
            r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"read_file","args":{"file_path":"a.rs"}},"thoughtSignature":"sig-1"}]}}]}"#,
        )
        .expect("decodes");
        let part = &chunks[0].first_parts()[0];
        assert!(part.function_call.is_some());
        assert_eq!(part.thought_signature.as_deref(), Some("sig-1"));

        // A call split across events keeps the signature its first fragment carried.
        let held = decode(
            &w,
            &mut state,
            r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"grep_search","args":"{\"pat"},"thoughtSignature":"sig-2"}]}}]}"#,
        )
        .expect("decodes");
        assert!(held.is_empty());
        let rest = decode(
            &w,
            &mut state,
            r#"{"candidates":[{"content":{"parts":[{"functionCall":{"args":"tern\":\"TODO\"}"}}]}}]}"#,
        )
        .expect("decodes");
        assert_eq!(rest[0].first_parts()[0].thought_signature.as_deref(), Some("sig-2"));
        let held = decode(
            &w,
            &mut state,
            r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"glob","args":"{\"pa"},"thoughtSignature":"sig-3"}]}}]}"#,
        )
        .expect("decodes");
        assert!(held.is_empty());
        let flushed = w.finish(&mut state).expect("flushes");
        assert_eq!(flushed[0].first_parts()[0].thought_signature.as_deref(), Some("sig-3"));
    }

    #[test]
    fn an_exhausted_daily_quota_in_the_stream_is_not_retried() {
        let w = wire("gemini-2.5-pro");
        let mut state = Decoder::new();
        let err = decode(
            &w,
            &mut state,
            r#"{"error":{"code":429,"message":"Quota exceeded for metric: generate_content_free_tier_requests","status":"RESOURCE_EXHAUSTED","details":[{"@type":"type.googleapis.com/google.rpc.QuotaFailure","violations":[{"quotaId":"GenerateRequestsPerDayPerProjectPerModel-FreeTier"}]}]}}"#,
        )
        .unwrap_err();
        assert!(matches!(err, CaError::TerminalQuota { .. }), "{err:?}");
        // A plain rate limit still retries.
        let err = decode(&w, &mut state, r#"{"error":{"code":429,"message":"slow down"}}"#).unwrap_err();
        assert!(err.is_retryable());
    }

    #[test]
    fn a_lone_finish_reason_is_not_dropped_as_an_error() {
        let w = wire("gemini-2.5-pro");
        let mut state = Decoder::new();
        let chunks = decode(
            &w,
            &mut state,
            r#"{"candidates":[{"content":{"parts":[{"text":"partial"}]},"finishReason":"MAX_TOKENS"}]}"#,
        )
        .expect("decodes");
        assert_eq!(chunks.len(), 1);
        assert_eq!(
            chunks[0].response.as_ref().and_then(|r| r.candidates[0].finish_reason.as_deref()),
            Some("MAX_TOKENS")
        );
    }
}
