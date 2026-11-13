//! Anthropic Messages dialect (`{base}/messages`, `stream: true`).
//!
//! The Messages API differs from our internal `GenerateContentRequest` in ways a field rename
//! cannot bridge, so this file is the whole translation in both directions:
//!
//! * Roles are `user`/`assistant` and the API rejects two consecutive turns of the same role, so
//!   the history is coalesced while it is projected rather than sent verbatim.
//! * Tool results are not their own message: they are `tool_result` blocks inside a `user`
//!   message and must precede any text in that message.
//! * `max_tokens` is mandatory (the API answers 400 without it), and `system` is a top-level
//!   string rather than a content part.
//! * The stream is a sequence of named, indexed content blocks. A `tool_use` block's arguments
//!   arrive as JSON text fragments, one `content_block_delta` at a time, so decoding needs the
//!   per-block assembly the shared [`Decoder`] provides.
//! * Thinking arrives as signed `thinking` and `redacted_thinking` blocks, and a turn that called
//!   a tool must send them back complete, unmodified and in order with the tool result. They are
//!   kept whole in [`Part::provider_block`] and replayed only to a request like the one that
//!   produced them (see [`Wire::binding`]); no other backend ever receives them.
//!
//! The driver in [`super`] owns HTTP, retry, cancellation and the SSE framing; nothing here
//! performs I/O and every path below is exercised by literal-payload unit tests.

use std::sync::Mutex;

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::api::{
    CaError, CaGenerateResponse, GenerateContentRequest, FunctionDeclaration, Part, ProviderBlock, UsageMetadata,
};
use crate::providers::common::{
    call_chunk, part_chunk, synthesize_call_id, thought_chunk, Dialect, Decoder, PartialCall, WireAuth,
};
use crate::providers::{ProviderKind, ProviderProfile};

/// Value used when neither the request nor the profile sets a token cap.
///
/// `max_tokens` has no default on this API; omitting it is a request error, so a provider profile
/// that leaves `max_output_tokens` empty still has to send something. Thinking counts toward it
/// (and is always on for Claude Opus 5.5), so the cap leaves room for thinking plus a long reply,
/// such as a whole file in one `write_file` call. 64K is the output maximum of the smallest
/// current model (Claude Haiku 4.5), and an unused cap costs nothing: output rate limits count
/// the tokens produced, not `max_tokens`.
const DEFAULT_MAX_TOKENS: u32 = 64_000;

/// Longest accumulated tool-argument body accepted for one call.
///
/// The driver already caps the whole stream, but a single runaway block would otherwise grow an
/// unbounded `String` before that cap is reached. Arguments beyond this are dropped and parsing
/// falls back to `{}`, which is recoverable; the alternative is exhausting memory.
const MAX_ARGS_BYTES: usize = 1024 * 1024;

/// Value sent as `anthropic-version`. Pinned: the header is a date, and a new one can change the
/// response shape of everything decoded here. The model list ([`super::models`]) sends it too.
pub(super) const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Beta header an OAuth (subscription) request must carry. The Messages API refuses a token minted
/// by the Claude Code sign-in without it.
const OAUTH_BETA: &str = "oauth-2025-04-20";

/// System block Anthropic's OAuth endpoint requires on every inference request.
///
/// This is a wire requirement, not a cosmetic one: a token from the Claude Code sign-in is refused
/// without it. It is the only thing reproduced from the official client, and it is added **only** on
/// the bearer path; the API-key path sends exactly what the user configured and no more.
const IDENTITY_BLOCK: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

/// Separator the agent loop puts between a tool name and its id (`{name}__{id}`).
///
/// The loop normalizes call ids that way so a resumed history cannot confuse two tools, but the
/// provider must see the id it issued. Both directions cross this separator.
const ID_SEP: &str = "__";

// ---------------------------------------------------------------------------
// Per-turn decode state
// ---------------------------------------------------------------------------

/// State that does not fit the shared block table.
///
/// Input tokens are reported in `message_start`, long before the output count that accompanies
/// them in `message_delta`, so one of the two numbers must be carried across events. The loop's
/// usage event replaces the previous snapshot instead of summing, so a lone output count would
/// show an input of zero; the two halves are therefore joined here.
///
/// Tool calls are held here from their `content_block_stop` until the stop reason is known: a
/// reply cut off at `max_tokens` inside a `tool_use` block still closes the block, and its half
/// argument object must not run as `{}`. The loop runs calls only after the stream ends, so
/// holding them costs nothing.
///
/// Reasoning blocks are assembled here as they stream and handed over when they close, so the
/// loop stores them in block order: a block that closes while a call is held is held behind it.
#[derive(Debug, Default)]
struct TurnState {
    input_tokens: u64,
    /// Stop reason recorded from `message_delta`, kept for logging and diagnostics.
    stop_reason: Option<String>,
    /// Model reported by `message_start`, echoed on the first chunk.
    model: Option<String>,
    /// `max_tokens` the request was sent with, for the message when a reply is cut off.
    max_tokens: u32,
    /// The block opened last is a `tool_use` block.
    last_block_tool: bool,
    /// Closed blocks, in block order, not handed to the loop yet.
    held: Vec<Held>,
    /// [`Wire::binding`] of the request this turn answers, set by `build_body`.
    binding: Option<String>,
    /// Reasoning blocks still streaming, by block index.
    reasoning: Vec<(usize, Value)>,
}

/// A closed block held back until the stop reason is known.
#[derive(Debug)]
enum Held {
    Call(PartialCall),
    /// Text or a reasoning block that arrived after a held call, held so the order stays the
    /// wire's: Anthropic checks a returned thinking block against every block before it.
    Part(Box<Part>),
}

/// One Messages API wire format for one profile.
pub struct Wire {
    profile: ProviderProfile,
    /// The request is authenticated with an OAuth bearer token rather than `x-api-key`.
    ///
    /// Set at construction because the two paths differ in more than a header: the bearer path also
    /// prepends the identity system block (see [`IDENTITY_BLOCK`]), and that block is part of the
    /// body. `build_body` has no access to the credential, so the mode has to be known here.
    bearer: bool,
    /// Interior mutability because [`Dialect::decode`] receives `&self` but the usage split above
    /// forces state across events.
    turn: Mutex<TurnState>,
}

impl Wire {
    pub fn new(profile: &ProviderProfile) -> Self {
        Self { profile: profile.clone(), bearer: false, turn: Mutex::new(TurnState::default()) }
    }

    /// The same dialect for a subscription account's token.
    pub fn with_bearer(profile: &ProviderProfile) -> Self {
        Self { profile: profile.clone(), bearer: true, turn: Mutex::new(TurnState::default()) }
    }

    /// Lock without the possibility of a panic.
    ///
    /// A panic elsewhere could poison the lock; the carried state is two counters and a string, so
    /// recovering it is strictly better than aborting the turn.
    fn turn(&self) -> std::sync::MutexGuard<'_, TurnState> {
        self.turn.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Effective cap on the answer: request, then profile, then [`DEFAULT_MAX_TOKENS`].
    fn max_tokens(&self, req: &GenerateContentRequest) -> u32 {
        req.generation_config
            .as_ref()
            .and_then(|g| g.max_output_tokens)
            .or(self.profile.max_output_tokens)
            .unwrap_or(DEFAULT_MAX_TOKENS)
            .max(1)
    }

    /// Start of the fingerprint a reasoning block is bound to; [`build_messages`] adds the messages.
    ///
    /// Anthropic checks a thinking block sent back against everything before it: the system
    /// prompt, the tools and every earlier message, including the reasoning those messages
    /// carried. Current models reject a block whose prefix changed with a 400, and the same
    /// history then fails on every later request. Our system prompt does change (a mode, an
    /// artifact list, a new day or folder listing when the agent is rebuilt), and compaction
    /// replaces older turns. So a block is sent back only while this fingerprint still matches;
    /// leaving reasoning out is always accepted, and since the fingerprint covers the earlier
    /// reasoning too, a later block is left out with any earlier one, which is the API's own rule
    /// for a gap. The endpoint and model are part of it because a signature is only good where it
    /// was issued: a proxy's signatures are not Anthropic's, and a model reads only some others'.
    /// `max_tokens` and the other request knobs are not part of the check, so they are not part
    /// of the fingerprint. Nor is the key: nothing derived from a secret is stored.
    ///
    /// `profile.id` is the first feed. A subscription account's profile carries
    /// `acct:<account id>` (see `app::account_profile`), so account A's signed blocks do not match
    /// account B's request and are dropped before they leave; a key-based profile carries its own
    /// id, which is stable, so its blocks keep matching across turns. Blocks stored before this
    /// feed existed carry a digest computed without it, so each is dropped once on replay and
    /// re-issued under the new one — dropping reasoning is always the safe direction.
    fn binding(&self, system: Option<&str>, tools: &[Value]) -> Sha256 {
        let mut prefix = Sha256::new();
        feed(&mut prefix, self.profile.id.as_bytes());
        // The auth mode is part of it: a key profile and an account profile that happen to share an
        // id send different system blocks, so reasoning issued under one must not travel under the
        // other.
        feed(&mut prefix, if self.bearer { b"oauth" as &[u8] } else { b"key" as &[u8] });
        feed(&mut prefix, self.endpoint().as_bytes());
        feed(&mut prefix, self.profile.model.as_bytes());
        feed(&mut prefix, system.unwrap_or_default().as_bytes());
        feed(&mut prefix, &serde_json::to_vec(tools).unwrap_or_default());
        prefix
    }
}

// ---------------------------------------------------------------------------
// Request translation
// ---------------------------------------------------------------------------

/// Strip the loop's `{name}__{id}` decoration so a `tool_result` points at the id the model was
/// given. A bare id (a history replayed from another provider) passes through unchanged.
fn wire_call_id(name: &str, id: Option<&str>) -> Option<String> {
    let id = id?;
    if id.is_empty() {
        return None;
    }
    let prefix = format!("{name}{ID_SEP}");
    Some(id.strip_prefix(&prefix).unwrap_or(id).to_owned())
}

/// Render one `functionResponse` body as the string a `tool_result` block carries.
///
/// The loop stores `{"output": ...}` on success and `{"error": "..."}` on failure. The flag is
/// returned separately because Anthropic has no in-band error marker: without `is_error` the model
/// reads a failure as ordinary tool output and may act on it as if it succeeded.
fn tool_result_text(response: &Value) -> (String, bool) {
    match response {
        Value::Object(map) => {
            if let Some(output) = map.get("output") {
                (stringify(output), false)
            } else if let Some(error) = map.get("error") {
                (stringify(error), true)
            } else {
                (stringify(response), false)
            }
        }
        other => (stringify(other), false),
    }
}

/// Plain strings stay verbatim; anything else is compact JSON so a structured result keeps its
/// shape instead of being flattened into Rust's `Debug` rendering.
fn stringify(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Text blocks of one content, with thought parts removed.
fn text_blocks(parts: &[Part]) -> Vec<Value> {
    parts
        .iter()
        .filter(|p| !p.is_thought())
        .filter_map(|p| p.text.as_deref().filter(|t| !t.is_empty()))
        .map(|t| json!({"type": "text", "text": t}))
        .collect()
}

/// One projected block of a content.
enum Item<'a> {
    /// Sent as is.
    Wire(Value),
    /// A reasoning block Anthropic issued, sent only while its binding still holds.
    Reasoning(&'a ProviderBlock),
}

/// Project one content onto its wire blocks.
///
/// Our [`Part::inline_data`] is dropped: it is only ever populated from a Code Assist response
/// and an unpriceable blob fails the whole turn. Gemini thought parts and their signatures are
/// dropped as well. Anthropic only accepts a thinking block that carries a signature *it* issued;
/// a signature from another provider, or the placeholder the history inserts for replayed calls,
/// is rejected outright, and text thinking without a block would be attributed to the assistant's
/// visible answer. Anthropic's own blocks come back from [`Part::provider_block`], in their place
/// among the text and calls; any other provider's are skipped.
fn blocks_for(parts: &[Part], is_model: bool) -> Vec<Item<'_>> {
    let mut out = Vec::with_capacity(parts.len());
    // Positions for synthesized ids count wire blocks only, so whether reasoning is sent never
    // changes an id.
    let mut sent = 0usize;
    for part in parts {
        if let Some(block) = &part.provider_block {
            if is_model && block.provider == ProviderKind::Anthropic.as_str() {
                out.push(Item::Reasoning(block));
            }
            continue;
        }
        if let Some(call) = &part.function_call {
            if !is_model {
                continue;
            }
            let input = match &call.args {
                Value::Object(map) => Value::Object(map.clone()),
                Value::Null => json!({}),
                // The API requires `input` to be an object; a scalar args value can only come
                // from a provider we did not produce, so it is wrapped rather than dropped.
                other => json!({"value": other}),
            };
            let id = wire_call_id(&call.name, call.id.as_deref())
                .unwrap_or_else(|| synthesize_call_id("anthropic-history", sent));
            out.push(Item::Wire(json!({
                "type": "tool_use",
                "id": id,
                "name": call.name,
                "input": input,
            })));
            sent += 1;
            continue;
        }
        if let Some(response) = &part.function_response {
            if is_model {
                continue;
            }
            let (text, is_error) = tool_result_text(&response.response);
            let id = wire_call_id(&response.name, response.id.as_deref())
                .unwrap_or_else(|| synthesize_call_id("anthropic-result", sent));
            let mut block = json!({
                "type": "tool_result",
                "tool_use_id": id,
                "content": text,
            });
            if is_error {
                block["is_error"] = Value::Bool(true);
            }
            out.push(Item::Wire(block));
            sent += 1;
            continue;
        }
        if let Some(text) = part.text.as_deref() {
            if !text.is_empty() && !part.is_thought() {
                out.push(Item::Wire(json!({"type": "text", "text": text})));
                sent += 1;
            }
        }
    }
    out
}

/// Whether a block is a tool result, which must precede every other block in its message.
fn is_tool_result(block: &Value) -> bool {
    block.get("type").and_then(Value::as_str) == Some("tool_result")
}

/// Coalesce the history into the role-alternating message list the API requires.
///
/// Two consecutive contents of the same role are merged into one message rather than sent as two,
/// because the API answers 400 for a repeated role. Tool results are lifted to the front of the
/// merged user content: the API requires a `tool_result` to be the first block answering a
/// preceding `tool_use`, and our history puts a text part before carried tool results in some
/// resumed turns. A conversation that opens with an assistant turn (a history seeded mid-stream)
/// loses that turn instead of failing the request, since a bad first message rejects everything.
///
/// `prefix` arrives holding the fingerprint of what precedes the messages (see [`Wire::binding`])
/// and gains each message as it is built. An assistant message carries its reasoning only when the
/// fingerprint at its start is the one its reasoning was bound to, that is, when everything before
/// it is what the request that produced it sent. Only the first content of a message can qualify:
/// a reply always followed a user message.
fn build_messages(req: &GenerateContentRequest, prefix: &mut Sha256) -> Vec<Value> {
    let mut messages: Vec<(bool, Vec<Vec<Item<'_>>>)> = Vec::new();

    for content in &req.contents {
        let is_model = content.role == Some(crate::api::Role::Model);
        let items = blocks_for(&content.parts, is_model);
        // Reasoning alone gives the model nothing to act on, and a turn always has more.
        if !items.iter().any(|item| matches!(item, Item::Wire(_))) {
            continue;
        }
        match messages.last_mut() {
            Some((last_model, contents)) if *last_model == is_model => contents.push(items),
            _ => messages.push((is_model, vec![items])),
        }
    }

    let mut out = Vec::with_capacity(messages.len());
    let mut dropped_leading = 0usize;
    for (is_model, contents) in messages {
        if is_model && out.is_empty() {
            dropped_leading += 1;
            continue;
        }
        let at = digest(prefix);
        let mut blocks = Vec::new();
        for (position, items) in contents.into_iter().enumerate() {
            for item in items {
                match item {
                    Item::Wire(block) => blocks.push(block),
                    Item::Reasoning(r) if position == 0 && r.binding.as_deref() == Some(at.as_str()) => {
                        blocks.push(r.block.clone());
                    }
                    Item::Reasoning(_) => {}
                }
            }
        }
        let (mut results, others): (Vec<Value>, Vec<Value>) = blocks.into_iter().partition(is_tool_result);
        results.extend(others);
        let message = json!({
            "role": if is_model { "assistant" } else { "user" },
            "content": results,
        });
        feed(prefix, message.to_string().as_bytes());
        out.push(message);
    }
    if dropped_leading > 0 {
        tracing::warn!(dropped_leading, "anthropic history began with an assistant turn; it was dropped");
    }
    out
}

/// Add one length-prefixed field to a binding fingerprint, so two fields never run together.
fn feed(prefix: &mut Sha256, bytes: &[u8]) {
    prefix.update((bytes.len() as u64).to_le_bytes());
    prefix.update(bytes);
}

/// The fingerprint so far, as stored in [`ProviderBlock::binding`].
fn digest(prefix: &Sha256) -> String {
    prefix.clone().finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// System instruction as one top-level string.
///
/// The field is a string (or an array of text blocks) and never a message; joining the parts
/// here keeps the caller's single content shape while satisfying the API.
fn system_text(req: &GenerateContentRequest) -> Option<String> {
    let content = req.system_instruction.as_ref()?;
    let joined: String = content.text();
    if joined.trim().is_empty() {
        None
    } else {
        Some(joined)
    }
}

/// Every tool declaration, flattened across tool groups.
///
/// The loop sends exactly one group, but flattening here means a second group (for example a
/// per-mode set) cannot silently lose its declarations.
fn tool_declarations(req: &GenerateContentRequest) -> Vec<Value> {
    let decls: Vec<&FunctionDeclaration> =
        req.tools.iter().flatten().flat_map(|t| t.function_declarations.iter()).collect();
    decls
        .into_iter()
        .map(|d| {
            json!({
                "name": d.name,
                "description": d.description,
                "input_schema": object_schema(&d.parameters_json_schema),
            })
        })
        .collect()
}

/// The declared schema with an object root guaranteed.
///
/// `input_schema` must be an object schema; a missing or non-object value would reject the whole
/// request, so it degrades to an empty object schema, which simply accepts no named arguments.
fn object_schema(schema: &Value) -> Value {
    match schema {
        Value::Object(map) if !map.is_empty() => Value::Object(map.clone()),
        _ => json!({"type": "object", "properties": {}}),
    }
}

impl Dialect for Wire {
    fn endpoint(&self) -> String {
        format!("{}/messages", self.profile.resolved_base_url())
    }

    fn build_body(&self, req: &GenerateContentRequest) -> Value {
        let mut body = Map::new();
        let max_tokens = self.max_tokens(req);
        let system = system_text(req);
        let tools = tool_declarations(req);
        let mut prefix = self.binding(system.as_deref(), &tools);
        let messages = build_messages(req, &mut prefix);
        {
            let mut turn = self.turn();
            turn.max_tokens = max_tokens;
            // The reply is bound to this whole request; its reasoning blocks carry the binding.
            turn.binding = Some(digest(&prefix));
        }
        body.insert("model".into(), Value::String(self.profile.model.clone()));
        body.insert("max_tokens".into(), json!(max_tokens));
        body.insert("stream".into(), Value::Bool(true));
        body.insert("messages".into(), Value::Array(messages));

        if let Some(system) = system {
            // On the bearer path the identity block is the first system block; a request without
            // it is refused by Anthropic's OAuth endpoint. The API-key path keeps the plain string
            // it always sent.
            if self.bearer {
                let mut blocks = vec![json!({"type": "text", "text": IDENTITY_BLOCK})];
                blocks.push(json!({"type": "text", "text": system}));
                body.insert("system".into(), Value::Array(blocks));
            } else {
                body.insert("system".into(), Value::String(system));
            }
        } else if self.bearer {
            body.insert(
                "system".into(),
                Value::Array(vec![json!({"type": "text", "text": IDENTITY_BLOCK})]),
            );
        }

        if !tools.is_empty() {
            body.insert("tools".into(), Value::Array(tools));
        }

        if let Some(config) = &req.generation_config {
            if let Some(stops) = config.stop_sequences.as_ref().filter(|s| !s.is_empty()) {
                body.insert("stop_sequences".into(), json!(stops));
            }
            // No sampling parameters. The temperature, top_p and top_k the loop sets are Code
            // Assist defaults, not a user choice, and Claude rejects them with a 400: Opus 4.7
            // and later take none of them, current Sonnet and Haiku models refuse non-default
            // values, and every Claude 4 model refuses temperature and top_p together.
            // `thinking_config` is dropped because thinking is a per-model capability here
            // (`thinking` blocks) rather than a request field, and a budget sent to a model
            // without it is an error.
        }

        Value::Object(body)
    }

    fn apply_headers(&self, rb: reqwest::RequestBuilder, auth: WireAuth<'_>) -> reqwest::RequestBuilder {
        let rb = rb.header("anthropic-version", ANTHROPIC_VERSION).header(reqwest::header::CONTENT_TYPE, "application/json");
        match auth {
            // A configured key keeps the header it always used.
            WireAuth::Key(key) => rb.header("x-api-key", key.trim()),
            // A subscription token travels as a bearer and needs the OAuth beta header. `x-api-key`
            // is deliberately not sent: a request carrying both is refused.
            WireAuth::Bearer(token) => rb.bearer_auth(token).header("anthropic-beta", OAUTH_BETA),
        }
    }

    fn decode(&self, state: &mut Decoder, payload: &str) -> Result<Vec<CaGenerateResponse>, CaError> {
        let event: Value = serde_json::from_str(payload)
            .map_err(|e| CaError::Decode(format!("anthropic event is not JSON: {e}")))?;
        let kind = event.get("type").and_then(Value::as_str).unwrap_or_default();
        let mut out = Vec::new();

        match kind {
            "message_start" => {
                let message = event.get("message");
                let usage = message.and_then(|m| m.get("usage"));
                let input = usage.and_then(|u| u.get("input_tokens")).and_then(Value::as_u64).unwrap_or(0);
                let model = message
                    .and_then(|m| m.get("model"))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let mut turn = self.turn();
                turn.input_tokens = input;
                turn.model = model.clone();
                // The output count is unknown until `message_delta`; emitting nothing here keeps
                // the single usage event in this dialect complete instead of half-filled.
                if let Some(model) = model {
                    out.push(model_chunk(&model));
                }
            }

            // `ping` and anything newer than this table is ignored deliberately: an unknown event
            // must not abort a turn that is otherwise delivering text.
            "content_block_start" => {
                let index = block_index(&event)?;
                let block = event.get("content_block").cloned().unwrap_or(Value::Null);
                self.turn().last_block_tool = block.get("type").and_then(Value::as_str) == Some("tool_use");
                match block.get("type").and_then(Value::as_str) {
                    Some("tool_use") => {
                        let id = block.get("id").and_then(Value::as_str).map(str::to_owned);
                        let name = block
                            .get("name")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                            .unwrap_or_default();
                        let slot = state.slot(index);
                        slot.id = id;
                        slot.name = name;
                        // Some servers send the complete input object here and no deltas.
                        if let Some(input) = block.get("input").filter(|v| is_non_empty(v)) {
                            slot.args.push_str(&input.to_string());
                        }
                    }
                    Some("text") => {
                        if let Some(text) = block.get("text").and_then(Value::as_str) {
                            if !text.is_empty() {
                                self.emit(&mut out, Part::text(text));
                            }
                        }
                    }
                    // Reasoning is kept whole, as the API wants it back: a `thinking` block is
                    // announced empty and filled by `thinking_delta` and `signature_delta` below,
                    // a `redacted_thinking` block arrives complete. Its text also streams to the
                    // UI as thoughts. No other block kind carries anything the loop reads.
                    Some("thinking" | "redacted_thinking") => {
                        if let Some(text) = block.get("thinking").and_then(Value::as_str).filter(|t| !t.is_empty()) {
                            out.push(thought_chunk(text));
                        }
                        self.turn().reasoning.push((index, block));
                    }
                    _ => {}
                }
            }

            "content_block_delta" => {
                let index = block_index(&event)?;
                let delta = event.get("delta").cloned().unwrap_or(Value::Null);
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        if let Some(text) = delta.get("text").and_then(Value::as_str) {
                            if !text.is_empty() {
                                self.emit(&mut out, Part::text(text));
                            }
                        }
                    }
                    Some("thinking_delta") => {
                        if let Some(text) = delta.get("thinking").and_then(Value::as_str) {
                            if !text.is_empty() {
                                out.push(thought_chunk(text));
                                self.fill_reasoning(index, "thinking", text);
                            }
                        }
                    }
                    Some("signature_delta") => {
                        if let Some(signature) = delta.get("signature").and_then(Value::as_str) {
                            self.fill_reasoning(index, "signature", signature);
                        }
                    }
                    Some("input_json_delta") => {
                        if let Some(fragment) = delta.get("partial_json").and_then(Value::as_str) {
                            let room = MAX_ARGS_BYTES.saturating_sub(state.slot(index).args.len());
                            if fragment.len() > room {
                                // Keep what still fits so the truncated body is at worst invalid
                                // JSON (which `into_call` turns into `{}`), never a lost call.
                                state.slot(index).args.push_str(&fragment[..room]);
                            } else {
                                state.slot(index).args.push_str(fragment);
                            }
                        }
                    }
                    // `citations_delta` carries nothing the loop consumes.
                    _ => {}
                }
            }

            "content_block_stop" => {
                let index = block_index(&event)?;
                if let Some(call) = state.take(index) {
                    if call.name.is_empty() {
                        tracing::warn!(index, "anthropic tool_use block had no name; dropped");
                    } else {
                        // Held until the stop reason says whether the block was cut off.
                        self.turn().held.push(Held::Call(call));
                    }
                } else if let Some(part) = self.close_reasoning(index) {
                    self.emit(&mut out, part);
                }
            }

            "message_delta" => {
                let delta = event.get("delta");
                let stop_reason =
                    delta.and_then(|d| d.get("stop_reason")).and_then(Value::as_str).map(str::to_owned);
                let output = event
                    .get("usage")
                    .and_then(|u| u.get("output_tokens"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                // The input count was recorded at `message_start`. Clearing it here means a
                // later `finish` cannot double-report: a cut stream reports once, a complete
                // stream reports once.
                let (input, model, cut_call, max_tokens) = {
                    let mut turn = self.turn();
                    turn.stop_reason = stop_reason.clone();
                    let input = turn.input_tokens;
                    turn.input_tokens = 0;
                    let cut = stop_reason.as_deref() == Some("max_tokens")
                        && (turn.last_block_tool
                            || !state.is_empty()
                            || turn.held.iter().any(|h| matches!(h, Held::Call(c) if !args_complete(&c.args))));
                    (input, turn.model.clone(), cut, turn.max_tokens)
                };
                tracing::debug!(
                    stop_reason = stop_reason.as_deref().unwrap_or(""),
                    input_tokens = input,
                    output_tokens = output,
                    "anthropic message finished"
                );
                if cut_call {
                    // The loop ends the turn on this error and drops the calls of the reply, so
                    // the half-written call never runs; the text before it is kept.
                    self.turn().held.clear();
                    state.take_all();
                    return Err(CaError::Decode(format!(
                        "the reply reached the {max_tokens}-token output limit in the middle of a tool call, so the call was not run. Ask for smaller steps, or raise maxOutputTokens for this provider in settings.json."
                    )));
                }
                if stop_reason.as_deref() == Some("refusal") {
                    // A safety classifier declined the request. Often nothing was written, and an
                    // empty reply is something the loop retries — the same request, declined
                    // again, on the user's quota. So it ends the turn here, saying what happened,
                    // and nothing the reply started is run.
                    self.turn().held.clear();
                    state.take_all();
                    let category = delta
                        .and_then(|d| d.get("stop_details"))
                        .and_then(|s| s.get("category"))
                        .and_then(Value::as_str)
                        .filter(|c| !c.is_empty() && c.len() <= 40 && c.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_'));
                    let why = category.map(|c| format!(" ({c})")).unwrap_or_default();
                    return Err(CaError::Decode(format!(
                        "Claude declined this request{why}. Rephrase it, or pick another model for this turn."
                    )));
                }
                out.push(usage_chunk_in(&model, input, output));
                if stop_reason.as_deref() == Some("max_tokens") {
                    out.push(max_tokens_chunk());
                }
            }

            "message_stop" => {
                // A well-formed stream closes every block first. Draining here as well means a
                // server that omits the final `content_block_stop` still yields its calls, and
                // `finish` stays the single place the driver's end-of-stream path relies on.
                out.extend(self.flush_calls(state));
            }

            "error" => return Err(self.error_from_event(&event)),

            _ => {}
        }

        Ok(out)
    }

    fn finish(&self, state: &mut Decoder) -> Result<Vec<CaGenerateResponse>, CaError> {
        let mut out = Vec::new();
        // A stream cut before `message_delta` still knows its input count; reporting it is better
        // than leaving the UI at zero.
        let (input, model) = {
            let mut turn = self.turn();
            let input = turn.input_tokens;
            turn.input_tokens = 0;
            (input, turn.model.clone())
        };
        if input > 0 {
            out.push(usage_chunk_in(&model, input, 0));
        }
        out.extend(self.flush_calls(state));
        Ok(out)
    }
}

impl Wire {
    /// Emit every block still held or buffered, in block order.
    ///
    /// Used both at `message_stop` and at end of stream; taking the blocks makes a second call a
    /// no-op, so an early stop cannot double-report a call the loop would then execute twice.
    fn flush_calls(&self, state: &mut Decoder) -> Vec<CaGenerateResponse> {
        let held = std::mem::take(&mut self.turn().held);
        let mut out = Vec::with_capacity(held.len());
        let mut calls = 0usize;
        for item in held.into_iter().chain(state.take_all().into_iter().map(Held::Call)) {
            match item {
                Held::Call(call) => {
                    let position = calls;
                    calls += 1;
                    if !call.name.is_empty() {
                        out.push(call_chunk(call.into_call(synthesize_call_id("anthropic-flush", position))));
                    }
                }
                Held::Part(part) => out.push(part_chunk(*part)),
            }
        }
        out
    }

    /// Hand `part` to the loop now, or hold it behind a held call so the stored order is the
    /// wire's (see [`Held::Part`]).
    fn emit(&self, out: &mut Vec<CaGenerateResponse>, part: Part) {
        let mut turn = self.turn();
        if turn.held.is_empty() {
            out.push(part_chunk(part));
        } else {
            turn.held.push(Held::Part(Box::new(part)));
        }
    }

    /// Add a streamed delta to the `thinking` block at `index`: `thinking` text is appended, the
    /// `signature` replaced, as the SDKs' own accumulators do. A delta for any other block kind is
    /// ignored rather than written into a block that must go back unmodified.
    fn fill_reasoning(&self, index: usize, field: &str, text: &str) {
        let mut turn = self.turn();
        let Some((_, Value::Object(block))) = turn.reasoning.iter_mut().rev().find(|(i, _)| *i == index) else { return };
        if block.get("type").and_then(Value::as_str) != Some("thinking") {
            return;
        }
        match (field, block.get_mut(field)) {
            ("thinking", Some(Value::String(existing))) => existing.push_str(text),
            _ => {
                block.insert(field.to_owned(), Value::String(text.to_owned()));
            }
        }
    }

    /// The closed reasoning block at `index`, as a part the loop stores, when it is complete.
    ///
    /// A block without its signature (or a redacted block without its data) is dropped: the API
    /// rejects one sent back like that, and leaving reasoning out is always accepted. The part
    /// carries the binding of the request it answers, which decides whether it is sent back.
    fn close_reasoning(&self, index: usize) -> Option<Part> {
        let mut turn = self.turn();
        let position = turn.reasoning.iter().rposition(|(i, _)| *i == index)?;
        let (_, block) = turn.reasoning.remove(position);
        let sealed = |field: &str| block.get(field).and_then(Value::as_str).is_some_and(|s| !s.is_empty());
        let complete = match block.get("type").and_then(Value::as_str) {
            Some("thinking") => sealed("signature"),
            Some("redacted_thinking") => sealed("data"),
            _ => false,
        };
        if !complete {
            tracing::debug!(index, "anthropic reasoning block closed without its signature; not kept");
            return None;
        }
        Some(Part {
            provider_block: Some(ProviderBlock {
                provider: ProviderKind::Anthropic.as_str().to_owned(),
                binding: turn.binding.clone(),
                block,
            }),
            ..Default::default()
        })
    }

    /// Map a streamed `error` event onto the loop's retry vocabulary.
    ///
    /// The HTTP status is already 200 by the time one of these arrives, so the body is the only
    /// signal. Overload, timeouts and transient server faults are exactly what the loop's backoff
    /// is for; a malformed request or a rejected key is not, and retrying it would burn the
    /// attempt budget. A transient kind whose message says the request itself has to change is
    /// not transient either (see [`needs_a_different_request`]).
    ///
    /// The provider's message never reaches the log, and reaches the error with the profile's key
    /// removed: a relay that forwards the user's key may quote it back. The driver runs a second,
    /// wider pass over everything a dialect returns — it is the only place that knows the resolved
    /// OAuth token — so this is the cheap half, not the whole guarantee.
    fn error_from_event(&self, event: &Value) -> CaError {
        let error = event.get("error").cloned().unwrap_or(Value::Null);
        let kind = error.get("type").and_then(Value::as_str).unwrap_or_default();
        let key = self.profile.api_key.trim();
        let secrets: &[&str] = if key.is_empty() { &[] } else { &[key] };
        let message = super::scrub(error.get("message").and_then(Value::as_str).unwrap_or("unknown error"), secrets);
        let transient = matches!(kind, "overloaded_error" | "api_error" | "rate_limit_error" | "timeout_error");
        match kind {
            _ if transient && !needs_a_different_request(&message) => {
                tracing::warn!(kind, len = message.len(), "anthropic stream reported a retryable error");
                CaError::Retryable { delay: None }
            }
            // The provider's own words say what to change, so they are shown as written.
            _ if transient => {
                tracing::warn!(kind, "anthropic stream reported an error that retrying cannot clear");
                CaError::Decode(message)
            }
            "invalid_request_error" => CaError::Decode(message),
            other => CaError::Decode(format!("anthropic error event {other}: {message}")),
        }
    }
}

/// Phrases with which a provider says the request is the problem, not the moment.
///
/// Anthropic documents `api_error` as an unexpected internal error to retry with backoff, and its
/// own message for one is a generic "Internal server error". Proxies in front of other backends
/// use the same kind for failures no retry can clear: a conversation the model refuses to
/// continue, or reasoning blocks it cannot accept. Retrying one of those spends the whole attempt
/// budget (minutes) and then shows a generic rate-limit banner in place of the explanation.
const NOT_TRANSIENT: [&str; 7] = [
    "cannot continue this conversation",
    "can't continue this conversation",
    "select a different model",
    "start a new conversation",
    "rewind the current conversation",
    "cannot be modified",
    "bound to a different conversation",
];

/// Whether a transient-kind error message says the request has to change before it can succeed.
fn needs_a_different_request(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    NOT_TRANSIENT.iter().any(|phrase| message.contains(phrase))
}

/// Tool arguments that are a whole JSON value, or none at all (a call with no arguments).
fn args_complete(args: &str) -> bool {
    args.trim().is_empty() || serde_json::from_str::<Value>(args).is_ok()
}

/// Chunk telling the loop the reply stopped at its output limit.
fn max_tokens_chunk() -> CaGenerateResponse {
    CaGenerateResponse {
        response: Some(crate::api::GenerateContentResponse {
            candidates: vec![crate::api::Candidate { content: None, finish_reason: Some("MAX_TOKENS".into()), index: Some(0) }],
            ..Default::default()
        }),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Decoding helpers
// ---------------------------------------------------------------------------

/// `index` of a content-block event. Missing or non-numeric indexes are a decode fault; the event
/// cannot be attributed to a block and guessing index 0 would corrupt an unrelated call.
fn block_index(event: &Value) -> Result<usize, CaError> {
    event
        .get("index")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .ok_or_else(|| CaError::Decode("anthropic content block event without an index".into()))
}

/// True for an object or array that actually holds entries; `{}` is what the API sends for a
/// tool with no arguments and must not be treated as pre-filled input to parse.
fn is_non_empty(value: &Value) -> bool {
    match value {
        Value::Object(map) => !map.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Null => false,
        _ => true,
    }
}

/// One chunk carrying the reported model, so a caller that logs `modelVersion` sees the provider's
/// actual snapshot rather than the alias it asked for.
fn model_chunk(model: &str) -> CaGenerateResponse {
    CaGenerateResponse {
        response: Some(crate::api::GenerateContentResponse {
            model_version: Some(model.to_owned()),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Usage with the model version attached, so the single usage event also carries it.
fn usage_chunk_in(model: &Option<String>, prompt: u64, output: u64) -> CaGenerateResponse {
    let usage = UsageMetadata {
        prompt_token_count: prompt,
        candidates_token_count: output,
        total_token_count: prompt.saturating_add(output),
        ..Default::default()
    };
    CaGenerateResponse {
        response: Some(crate::api::GenerateContentResponse {
            usage_metadata: Some(usage),
            model_version: model.clone(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Content, FunctionCall, FunctionResponse, GenerationConfig, Role, Tool};
    use serde_json::json;

    fn profile() -> ProviderProfile {
        ProviderProfile {
            id: "a1".into(),
            label: "Claude".into(),
            kind: crate::providers::ProviderKind::Anthropic,
            model: "claude-sonnet-4-5".into(),
            base_url: None,
            api_key: "sk-ant-secret".into(),
            max_output_tokens: None,
            context_window: None,
            models: Vec::new(),
        }
    }

    fn wire() -> Wire {
        Wire::new(&profile())
    }

    /// Decode a whole turn and flatten every chunk into (text or call) parts.
    fn run(wire: &Wire, payloads: &[&str]) -> Vec<CaGenerateResponse> {
        let mut state = Decoder::new();
        let mut out = Vec::new();
        for p in payloads {
            out.extend(wire.decode(&mut state, p).expect("decode"));
        }
        out.extend(wire.finish(&mut state).expect("finish"));
        out
    }

    fn texts(chunks: &[CaGenerateResponse]) -> String {
        chunks.iter().filter_map(|c| c.first_parts().first()).filter_map(|p| p.text.clone()).collect()
    }

    fn calls(chunks: &[CaGenerateResponse]) -> Vec<FunctionCall> {
        chunks.iter().flat_map(|c| c.first_parts().to_vec()).filter_map(|p| p.function_call).collect()
    }

    #[test]
    fn text_streams_as_separate_chunks() {
        let w = wire();
        let chunks = run(
            &w,
            &[
                r#"{"type":"message_start","message":{"model":"claude-sonnet-4-5","usage":{"input_tokens":11}}}"#,
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lo"}}"#,
                r#"{"type":"content_block_stop","index":0}"#,
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}"#,
                r#"{"type":"message_stop"}"#,
            ],
        );
        assert_eq!(texts(&chunks), "Hello");
        assert!(calls(&chunks).is_empty());
    }

    #[test]
    fn thinking_delta_becomes_a_thought_part() {
        let w = wire();
        let chunks = run(
            &w,
            &[
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"weigh options"}}"#,
            ],
        );
        let part = chunks[0].first_parts()[0].clone();
        assert!(part.is_thought());
        assert_eq!(part.text.as_deref(), Some("weigh options"));
    }

    #[test]
    fn tool_call_arguments_are_assembled_from_fragments() {
        let w = wire();
        let chunks = run(
            &w,
            &[
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_01","name":"read_file","input":{}}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":""}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"file_"}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"path\":\"src/main.rs\"}"}}"#,
                r#"{"type":"content_block_stop","index":0}"#,
                r#"{"type":"message_stop"}"#,
            ],
        );
        let calls = calls(&chunks);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id.as_deref(), Some("toolu_01"));
        assert_eq!(calls[0].name, "read_file");
        assert_eq!(calls[0].args, json!({"file_path": "src/main.rs"}));
    }

    #[test]
    fn interleaved_text_and_tool_calls_keep_their_order() {
        let w = wire();
        let chunks = run(
            &w,
            &[
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Let me look."}}"#,
                r#"{"type":"content_block_stop","index":0}"#,
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_a","name":"glob","input":{}}}"#,
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"pattern\":\"*.rs\"}"}}"#,
                r#"{"type":"content_block_stop","index":1}"#,
                r#"{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_b","name":"read_file","input":{}}}"#,
                r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"file_path\":\"a.rs\"}"}}"#,
                r#"{"type":"content_block_stop","index":2}"#,
                r#"{"type":"message_stop"}"#,
            ],
        );
        assert_eq!(texts(&chunks), "Let me look.");
        let calls = calls(&chunks);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "glob");
        assert_eq!(calls[1].id.as_deref(), Some("toolu_b"));

        // Arrival order is part of the contract: the text must precede both calls.
        let summary: Vec<&str> = chunks
            .iter()
            .filter_map(|c| c.first_parts().first())
            .map(|p| if p.function_call.is_some() { "call" } else { "text" })
            .collect();
        assert_eq!(summary, vec!["text", "call", "call"]);
    }

    #[test]
    fn usage_joins_message_start_and_message_delta() {
        let w = wire();
        let chunks = run(
            &w,
            &[
                r#"{"type":"message_start","message":{"model":"claude-sonnet-4-5","usage":{"input_tokens":42,"output_tokens":1}}}"#,
                r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":7}}"#,
                r#"{"type":"message_stop"}"#,
            ],
        );
        let usages: Vec<UsageMetadata> =
            chunks.iter().filter_map(|c| c.response.as_ref().and_then(|r| r.usage_metadata)).collect();
        assert_eq!(usages.len(), 1);
        assert_eq!(usages[0].prompt_token_count, 42);
        assert_eq!(usages[0].candidates_token_count, 7);
        assert_eq!(usages[0].total_token_count, 49);
        // The model snapshot travels with the first chunk.
        assert!(chunks.iter().any(|c| c.response.as_ref().and_then(|r| r.model_version.as_deref()) == Some("claude-sonnet-4-5")));
    }

    #[test]
    fn error_events_map_to_the_retry_vocabulary() {
        let w = wire();
        let mut state = Decoder::new();
        let overloaded = r#"{"type":"error","error":{"type":"overloaded_error","message":"busy"}}"#;
        assert!(matches!(w.decode(&mut state, overloaded), Err(CaError::Retryable { delay: None })));

        let rate = r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow"}}"#;
        assert!(matches!(w.decode(&mut state, rate), Err(CaError::Retryable { .. })));

        let api = r#"{"type":"error","error":{"type":"api_error","message":"Internal server error"}}"#;
        assert!(matches!(w.decode(&mut state, api), Err(CaError::Retryable { .. })));

        let invalid = r#"{"type":"error","error":{"type":"invalid_request_error","message":"bad beta header"}}"#;
        match w.decode(&mut state, invalid) {
            Err(CaError::Decode(m)) => assert_eq!(m, "bad beta header"),
            other => panic!("expected decode error, got {other:?}"),
        }
    }

    #[test]
    fn unknown_and_malformed_payloads_do_not_panic() {
        let w = wire();
        let mut state = Decoder::new();
        // Unknown event kinds are skipped, not fatal: a newer API may add some.
        assert!(w.decode(&mut state, r#"{"type":"future_thing","x":1}"#).expect("unknown").is_empty());
        assert!(w.decode(&mut state, r#"{"type":"ping"}"#).expect("ping").is_empty());
        assert!(w.decode(&mut state, r#"{}"#).expect("empty object").is_empty());

        // Not JSON at all, and a truncated object, both become a decode error rather than a panic.
        assert!(matches!(w.decode(&mut state, "not json"), Err(CaError::Decode(_))));
        assert!(matches!(w.decode(&mut state, r#"{"type":"content_block_delta""#), Err(CaError::Decode(_))));

        // A block event without an index cannot be attributed and is rejected.
        assert!(matches!(
            w.decode(&mut state, r#"{"type":"content_block_stop"}"#),
            Err(CaError::Decode(_))
        ));
    }

    #[test]
    fn broken_tool_argument_json_becomes_empty_args() {
        let w = wire();
        let chunks = run(
            &w,
            &[
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_x","name":"grep_search","input":{}}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"pattern\":"}}"#,
                r#"{"type":"content_block_stop","index":0}"#,
            ],
        );
        let calls = calls(&chunks);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "grep_search");
        assert_eq!(calls[0].id.as_deref(), Some("toolu_x"));
        assert_eq!(calls[0].args, json!({}));
    }

    #[test]
    fn end_of_stream_flushes_a_call_that_never_stopped() {
        let w = wire();
        let mut state = Decoder::new();
        w.decode(
            &mut state,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_z","name":"finish","input":{}}}"#,
        )
        .expect("start");
        w.decode(
            &mut state,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"summary\":\"done\"}"}}"#,
        )
        .expect("delta");
        assert!(!state.is_empty());

        let flushed = w.finish(&mut state).expect("finish");
        let calls = calls(&flushed);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "finish");
        assert_eq!(calls[0].args, json!({"summary": "done"}));

        // A second finish is a no-op, so a call is never executed twice.
        assert!(w.finish(&mut state).expect("second finish").is_empty());
    }

    #[test]
    fn message_stop_drains_blocks_the_server_forgot_to_stop() {
        let w = wire();
        let chunks = run(
            &w,
            &[
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_q","name":"list_directory","input":{}}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"dir_path\":\".\"}"}}"#,
                r#"{"type":"message_stop"}"#,
            ],
        );
        let calls = calls(&chunks);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].args, json!({"dir_path": "."}));
    }

    #[test]
    fn server_supplied_complete_input_is_kept() {
        let w = wire();
        let chunks = run(
            &w,
            &[
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_k","name":"run_shell_command","input":{"command":"ls"}}}"#,
                r#"{"type":"content_block_stop","index":0}"#,
            ],
        );
        let calls = calls(&chunks);
        assert_eq!(calls[0].args, json!({"command": "ls"}));
    }

    #[test]
    fn body_carries_system_messages_tools_and_a_token_cap() {
        let w = wire();
        let req = GenerateContentRequest {
            contents: vec![
                Content::user_text("read it"),
                Content::model(vec![
                    Part::text("On it."),
                    Part { thought: Some(true), text: Some("hidden".into()), ..Default::default() },
                    Part {
                        function_call: Some(FunctionCall {
                            id: Some("read_file__toolu_01".into()),
                            name: "read_file".into(),
                            args: json!({"file_path": "a.txt"}),
                        }),
                        ..Default::default()
                    },
                ]),
                Content {
                    role: Some(Role::User),
                    parts: vec![
                        Part::text("here is the file"),
                        Part::function_response(FunctionResponse {
                            id: Some("read_file__toolu_01".into()),
                            name: "read_file".into(),
                            response: json!({"output": "contents"}),
                        }),
                    ],
                },
                Content {
                    role: Some(Role::User),
                    parts: vec![Part::function_response(FunctionResponse {
                        id: Some("glob__toolu_02".into()),
                        name: "glob".into(),
                        response: json!({"error": "denied"}),
                    })],
                },
            ],
            system_instruction: Some(Content::user_text("you are a coding agent")),
            tools: Some(vec![Tool {
                function_declarations: vec![
                    FunctionDeclaration {
                        name: "read_file".into(),
                        description: "Reads a file.".into(),
                        parameters_json_schema: json!({"type":"object","properties":{"file_path":{"type":"string"}},"required":["file_path"]}),
                    },
                    FunctionDeclaration {
                        name: "mcp__github__list_issues".into(),
                        description: "Lists issues.".into(),
                        parameters_json_schema: json!({"type":"object","properties":{}}),
                    },
                    FunctionDeclaration {
                        name: "broken".into(),
                        description: "No schema.".into(),
                        parameters_json_schema: Value::Null,
                    },
                ],
            }]),
            generation_config: Some(GenerationConfig {
                temperature: Some(0.3),
                top_p: Some(0.9),
                top_k: Some(64),
                max_output_tokens: Some(1234),
                stop_sequences: Some(vec!["END".into()]),
                thinking_config: Some(Default::default()),
                ..Default::default()
            }),
            ..Default::default()
        };

        let body = w.build_body(&req);
        assert_eq!(body["model"], json!("claude-sonnet-4-5"));
        assert_eq!(body["max_tokens"], json!(1234));
        assert_eq!(body["stream"], json!(true));
        assert_eq!(body["system"], json!("you are a coding agent"));
        // Claude Opus 4.7 and later reject sampling parameters with a 400, and every Claude 4
        // model rejects temperature and top_p together.
        assert!(body.get("temperature").is_none());
        assert!(body.get("top_p").is_none());
        assert_eq!(body["stop_sequences"], json!(["END"]));
        assert!(body.get("top_k").is_none());
        assert!(body.get("thinking_config").is_none());
        assert!(body.get("thinking").is_none());

        // The two consecutive user contents merged, and both tool results were lifted to the
        // front of the merged content in arrival order.
        let messages = body["messages"].as_array().expect("messages");
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["role"], json!("user"));
        assert_eq!(messages[1]["role"], json!("assistant"));
        assert_eq!(messages[2]["role"], json!("user"));

        let assistant = messages[1]["content"].as_array().expect("assistant blocks");
        let kinds: Vec<&str> = assistant.iter().filter_map(|b| b["type"].as_str()).collect();
        assert_eq!(kinds, vec!["text", "tool_use"]);
        assert_eq!(assistant[1]["id"], json!("toolu_01"));
        assert_eq!(assistant[1]["input"], json!({"file_path": "a.txt"}));

        let user = messages.last().expect("user turn");
        let blocks = user["content"].as_array().expect("user blocks");
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0]["type"], json!("tool_result"));
        assert_eq!(blocks[0]["tool_use_id"], json!("toolu_01"));
        assert_eq!(blocks[0]["content"], json!("contents"));
        assert!(blocks[0].get("is_error").is_none());
        assert_eq!(blocks[1]["type"], json!("tool_result"));
        assert_eq!(blocks[1]["tool_use_id"], json!("toolu_02"));
        assert_eq!(blocks[1]["is_error"], json!(true));
        assert_eq!(blocks[2]["type"], json!("text"));
        assert_eq!(blocks[2]["text"], json!("here is the file"));

        // Tools: schema forwarded, object root enforced, MCP-style names kept verbatim.
        let tools = body["tools"].as_array().expect("tools");
        assert_eq!(tools.len(), 3);
        assert_eq!(tools[0]["name"], json!("read_file"));
        assert_eq!(tools[0]["input_schema"]["required"], json!(["file_path"]));
        assert_eq!(tools[1]["name"], json!("mcp__github__list_issues"));
        assert_eq!(tools[2]["input_schema"], json!({"type":"object","properties":{}}));
    }

    #[test]
    fn tool_error_results_are_flagged_and_defaults_apply() {
        let w = wire();
        let req = GenerateContentRequest {
            contents: vec![
                Content::model(vec![Part {
                    function_call: Some(FunctionCall { id: Some("toolu_9".into()), name: "glob".into(), args: json!({}) }),
                    ..Default::default()
                }]),
                Content {
                    role: Some(Role::User),
                    parts: vec![Part::function_response(FunctionResponse {
                        id: Some("glob__toolu_9".into()),
                        name: "glob".into(),
                        response: json!({"error": "outside workspace"}),
                    })],
                },
            ],
            ..Default::default()
        };
        let body = w.build_body(&req);
        let messages = body["messages"].as_array().expect("messages");
        // A leading assistant turn is dropped so the API does not reject the whole request.
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], json!("user"));
        assert_eq!(messages[0]["content"][0]["is_error"], json!(true));
        assert_eq!(messages[0]["content"][0]["content"], json!("outside workspace"));
        assert_eq!(body["max_tokens"], json!(DEFAULT_MAX_TOKENS));
        assert!(body.get("system").is_none());
        assert!(body.get("tools").is_none());

        // The profile cap is the fallback when the request sets none.
        let capped = Wire::new(&ProviderProfile { max_output_tokens: Some(4096), ..profile() });
        assert_eq!(capped.build_body(&req)["max_tokens"], json!(4096));
        let mut requested = req.clone();
        requested.generation_config = Some(GenerationConfig { max_output_tokens: Some(512), ..Default::default() });
        assert_eq!(capped.build_body(&requested)["max_tokens"], json!(512));
    }

    #[test]
    fn the_default_cap_leaves_room_for_thinking_and_a_long_reply() {
        // Thinking counts toward max_tokens, and on Claude Opus 5.5 it is always on.
        let body = wire().build_body(&GenerateContentRequest { contents: vec![Content::user_text("hi")], ..Default::default() });
        assert!(body["max_tokens"].as_u64().unwrap() >= 32_000, "{}", body["max_tokens"]);
    }

    #[test]
    fn a_refusal_ends_the_turn_with_a_reason_instead_of_an_empty_reply() {
        let w = wire();
        let mut state = Decoder::new();
        let payloads = [
            r#"{"type":"message_start","message":{"model":"claude-opus-5-5","usage":{"input_tokens":10}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_r","name":"run_shell_command","input":{}}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"command\":\"ls\"}"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"refusal","stop_details":{"type":"refusal","category":"cyber","explanation":"x"}},"usage":{"output_tokens":3}}"#,
            r#"{"type":"message_stop"}"#,
        ];
        let mut chunks = Vec::new();
        let mut error = None;
        for p in payloads {
            match w.decode(&mut state, p) {
                Ok(c) => chunks.extend(c),
                Err(e) => {
                    error = Some(e);
                    break;
                }
            }
        }
        assert!(calls(&chunks).is_empty(), "nothing a refused reply started is run");
        let error = error.expect("the refusal is reported");
        assert!(matches!(error, CaError::Decode(_)), "not retried: {error:?}");
        assert!(error.to_string().contains("declined this request (cyber)"), "{error}");

        // No category, or one that is not a plain word: the message still reads, without it.
        let mut state = Decoder::new();
        let _ = w.decode(&mut state, r#"{"type":"message_start","message":{"model":"m","usage":{"input_tokens":1}}}"#);
        let err = w
            .decode(&mut state, r#"{"type":"message_delta","delta":{"stop_reason":"refusal","stop_details":{"category":"a b\u202e"}}}"#)
            .unwrap_err();
        assert!(err.to_string().contains("Claude declined this request. Rephrase"), "{err}");
    }

    #[test]
    fn a_tool_call_cut_off_at_max_tokens_is_reported_not_run() {
        let w = wire();
        let mut state = Decoder::new();
        let payloads = [
            r#"{"type":"message_start","message":{"model":"claude-sonnet-4-5","usage":{"input_tokens":10}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Writing it."}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_w","name":"write_file","input":{}}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"file_path\":\"big.rs\",\"content\":\"fn main() {"}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":8192}}"#,
            r#"{"type":"message_stop"}"#,
        ];
        let mut chunks = Vec::new();
        let mut error = None;
        for p in payloads {
            match w.decode(&mut state, p) {
                Ok(c) => chunks.extend(c),
                Err(e) => {
                    error = Some(e);
                    break;
                }
            }
        }
        assert!(calls(&chunks).is_empty(), "a call with half its arguments is not run");
        assert_eq!(texts(&chunks), "Writing it.", "the text before it still arrives");
        let error = error.expect("the cut-off is reported");
        assert!(error.to_string().contains("output limit"), "{error}");
    }

    #[test]
    fn a_stream_error_never_carries_the_key() {
        let w = wire();
        let mut state = Decoder::new();
        // A relay that forwards the user's key quotes it back.
        let event = r#"{"type":"error","error":{"type":"authentication_error","message":"upstream rejected key sk-ant-secret"}}"#;
        let err = w.decode(&mut state, event).unwrap_err();
        assert!(!err.to_string().contains("sk-ant-secret"));
    }

    #[test]
    fn endpoint_and_headers_target_the_messages_api() {
        let w = wire();
        assert_eq!(w.endpoint(), "https://api.anthropic.com/v1/messages");
        let proxied = Wire::new(&ProviderProfile {
            base_url: Some("http://localhost:9000/anthropic/".into()),
            ..profile()
        });
        assert_eq!(proxied.endpoint(), "http://localhost:9000/anthropic/messages");

        let request = w
            .apply_headers(reqwest::Client::new().post("http://localhost/"), WireAuth::Key("sk-ant-secret"))
            .build()
            .expect("request builds");
        assert_eq!(request.headers().get("x-api-key").and_then(|v| v.to_str().ok()), Some("sk-ant-secret"));
        assert_eq!(
            request.headers().get("anthropic-version").and_then(|v| v.to_str().ok()),
            Some(ANTHROPIC_VERSION)
        );
        assert!(request.headers().get("anthropic-beta").is_none(), "a key request carries no OAuth beta");
    }

    #[test]
    fn the_identity_block_is_sent_only_with_a_bearer() {
        // A subscription token is refused without the identity block, and the API-key path must not
        // grow one: a key profile sends exactly what the user configured.
        let request = GenerateContentRequest {
            contents: vec![Content::user_text("hi")],
            system_instruction: Some(Content::user_text("be terse")),
            ..Default::default()
        };
        let key_body = wire().build_body(&request);
        assert_eq!(key_body["system"], json!("be terse"));

        let bearer_body = Wire::with_bearer(&profile()).build_body(&request);
        assert_eq!(
            bearer_body["system"],
            json!([
                {"type": "text", "text": IDENTITY_BLOCK},
                {"type": "text", "text": "be terse"},
            ])
        );

        // With no system prompt the bearer request still carries the identity block.
        let bare = GenerateContentRequest { contents: vec![Content::user_text("hi")], ..Default::default() };
        let bearer_body = Wire::with_bearer(&profile()).build_body(&bare);
        assert_eq!(bearer_body["system"], json!([{"type": "text", "text": IDENTITY_BLOCK}]));
        assert!(wire().build_body(&bare).get("system").is_none());

        // Headers: bearer plus the beta header, and no x-api-key, which the API refuses together.
        let built = Wire::with_bearer(&profile())
            .apply_headers(reqwest::Client::new().post("http://localhost/"), WireAuth::Bearer("oauth-token"))
            .build()
            .expect("request builds");
        assert_eq!(
            built.headers().get("authorization").and_then(|v| v.to_str().ok()),
            Some("Bearer oauth-token")
        );
        assert_eq!(built.headers().get("anthropic-beta").and_then(|v| v.to_str().ok()), Some(OAUTH_BETA));
        assert!(built.headers().get("x-api-key").is_none());
    }

    #[test]
    fn two_accounts_never_replay_each_others_thinking() {
        // Two accounts of the same provider differ only in the profile id the app assigns
        // (`acct:<account id>`), so that id must be part of the binding digest.
        let account_a = ProviderProfile { id: "acct:claude-0123abcd".into(), api_key: String::new(), ..profile() };
        let account_b = ProviderProfile { id: "acct:claude-89abcdef".into(), api_key: String::new(), ..profile() };

        let content = produce(&account_a, "sys", "sig-a");
        let mut contents = vec![Content::user_text("list it")];
        contents.extend(answered(content));

        let a = Wire::with_bearer(&account_a).build_body(&bound_request("sys", contents.clone())).to_string();
        assert!(a.contains("sig-a"), "the account that issued it keeps its reasoning");

        let b = Wire::with_bearer(&account_b).build_body(&bound_request("sys", contents)).to_string();
        assert!(!b.contains("sig-a"), "another account must not receive it: {b}");
        assert!(!b.contains("providerBlock"), "{b}");

        // The auth mode is part of the digest too: a key profile with the same id is a different
        // request shape and does not inherit an account's reasoning.
        let same_id_key = ProviderProfile { id: account_a.id.clone(), ..profile() };
        let key_body = Wire::new(&same_id_key).build_body(&bound_request("sys", vec![Content::user_text("list it")])).to_string();
        assert!(!key_body.contains("sig-a"), "{key_body}");
    }

    // -----------------------------------------------------------------------
    // Thinking round trip
    // -----------------------------------------------------------------------

    /// A request with the system prompt and tools a turn is bound to.
    fn bound_request(system: &str, contents: Vec<Content>) -> GenerateContentRequest {
        GenerateContentRequest {
            contents,
            system_instruction: Some(Content::user_text(system)),
            tools: Some(vec![Tool {
                function_declarations: vec![FunctionDeclaration {
                    name: "list_directory".into(),
                    description: "Lists a directory.".into(),
                    parameters_json_schema: json!({"type":"object","properties":{"dir_path":{"type":"string"}}}),
                }],
            }]),
            ..Default::default()
        }
    }

    /// A tool-use reply of a thinking model: signed thinking, a redacted block, text, then a call.
    fn thinking_turn(signature: &str) -> Vec<String> {
        [
            r#"{"type":"message_start","message":{"model":"claude-opus-5-5","usage":{"input_tokens":20}}}"#.to_owned(),
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}"#.to_owned(),
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Look at the "}}"#.to_owned(),
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"tree first."}}"#.to_owned(),
            format!(r#"{{"type":"content_block_delta","index":0,"delta":{{"type":"signature_delta","signature":"{signature}"}}}}"#),
            r#"{"type":"content_block_stop","index":0}"#.to_owned(),
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"redacted_thinking","data":"EmwKAhgB"}}"#.to_owned(),
            r#"{"type":"content_block_stop","index":1}"#.to_owned(),
            r#"{"type":"content_block_start","index":2,"content_block":{"type":"text","text":""}}"#.to_owned(),
            r#"{"type":"content_block_delta","index":2,"delta":{"type":"text_delta","text":"Listing it."}}"#.to_owned(),
            r#"{"type":"content_block_stop","index":2}"#.to_owned(),
            r#"{"type":"content_block_start","index":3,"content_block":{"type":"tool_use","id":"toolu_01","name":"list_directory","input":{}}}"#.to_owned(),
            r#"{"type":"content_block_delta","index":3,"delta":{"type":"input_json_delta","partial_json":"{\"dir_path\":\".\"}"}}"#.to_owned(),
            r#"{"type":"content_block_stop","index":3}"#.to_owned(),
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":40}}"#.to_owned(),
            r#"{"type":"message_stop"}"#.to_owned(),
        ]
        .into()
    }

    /// The model content the agent loop stores for one streamed reply.
    fn stored(chunks: &[CaGenerateResponse]) -> Content {
        let mut acc = crate::agent::TurnAccumulator::new();
        for chunk in chunks {
            acc.push_parts(chunk.first_parts());
        }
        acc.into_content()
    }

    /// One reply produced by `profile` for a request with `system`, as stored.
    fn produce(profile: &ProviderProfile, system: &str, signature: &str) -> Content {
        // The account path is the bearer one; a key profile must be built the same way it sends,
        // because the auth mode is part of the binding digest.
        let w = if profile.api_key.trim().is_empty() {
            Wire::with_bearer(profile)
        } else {
            Wire::new(profile)
        };
        w.build_body(&bound_request(system, vec![Content::user_text("list it")]));
        let payloads = thinking_turn(signature);
        let payloads: Vec<&str> = payloads.iter().map(String::as_str).collect();
        stored(&run(&w, &payloads))
    }

    /// `model` followed by the results of its calls, as the loop appends them.
    fn answered(model: Content) -> [Content; 2] {
        let results = model
            .function_calls()
            .map(|c| Part::function_response(FunctionResponse { id: c.id.clone(), name: c.name.clone(), response: json!({"output": "a.rs"}) }))
            .collect();
        [model, Content { role: Some(Role::User), parts: results }]
    }

    fn kind_of(part: &Part) -> &str {
        match (&part.provider_block, &part.function_call) {
            (Some(b), _) => b.block["type"].as_str().unwrap_or("?"),
            (None, Some(_)) => "call",
            (None, None) => "text",
        }
    }

    #[test]
    fn thinking_blocks_are_kept_whole_with_their_signatures() {
        let content = produce(&profile(), "sys", "EqQBsig");
        let kinds: Vec<&str> = content.parts.iter().map(kind_of).collect();
        assert_eq!(kinds, ["thinking", "redacted_thinking", "text", "call"]);
        let thinking = content.parts[0].provider_block.as_ref().expect("block");
        assert_eq!(thinking.provider, "anthropic");
        assert_eq!(thinking.block, json!({"type":"thinking","thinking":"Look at the tree first.","signature":"EqQBsig"}));
        assert!(thinking.binding.is_some());
        assert_eq!(content.parts[1].provider_block.as_ref().expect("block").block, json!({"type":"redacted_thinking","data":"EmwKAhgB"}));
        // Nothing of the reasoning becomes stored text or a Gemini signature.
        assert_eq!(content.text(), "Listing it.");
        assert!(content.parts.iter().all(|p| p.thought_signature.is_none() && !p.is_thought()));
    }

    #[test]
    fn a_tool_turn_sends_its_reasoning_back_unchanged_and_in_order() {
        let model = produce(&profile(), "sys", "EqQBsig");
        let mut contents = vec![Content::user_text("list it")];
        contents.extend(answered(model));
        // As the loop sends it, with the placeholder signature Code Assist wants on the call.
        let contents = crate::agent::History::from_contents(contents).for_request();
        let body = Wire::new(&profile()).build_body(&bound_request("sys", contents));
        assert_eq!(
            body["messages"][1]["content"],
            json!([
                {"type":"thinking","thinking":"Look at the tree first.","signature":"EqQBsig"},
                {"type":"redacted_thinking","data":"EmwKAhgB"},
                {"type":"text","text":"Listing it."},
                {"type":"tool_use","id":"toolu_01","name":"list_directory","input":{"dir_path":"."}},
            ])
        );
        assert!(!body.to_string().contains(crate::agent::history::SKIP_THOUGHT_SIGNATURE));
    }

    #[test]
    fn no_other_providers_reasoning_or_signature_reaches_anthropic() {
        let mut model = produce(&profile(), "sys", "EqQBsig");
        let binding = model.parts[0].provider_block.as_ref().and_then(|b| b.binding.clone());
        // What a Gemini turn of the same conversation can leave behind.
        model.parts.insert(0, Part { text: Some("gemini thought".into()), thought: Some(true), thought_signature: Some("gemini-text-sig".into()), ..Default::default() });
        for part in model.parts.iter_mut().filter(|p| p.function_call.is_some()) {
            part.thought_signature = Some("gemini-call-sig".into());
        }
        model.parts.push(Part {
            provider_block: Some(ProviderBlock { provider: "gemini-api".into(), binding, block: json!({"type":"thinking","thinking":"x","signature":"foreign-sig"}) }),
            ..Default::default()
        });
        let mut contents = vec![Content::user_text("list it")];
        contents.extend(answered(model));
        let wire = Wire::new(&profile()).build_body(&bound_request("sys", contents)).to_string();
        for leaked in ["gemini thought", "gemini-text-sig", "gemini-call-sig", "foreign-sig", "providerBlock"] {
            assert!(!wire.contains(leaked), "{leaked} reached the body: {wire}");
        }
        assert!(wire.contains("EqQBsig"));
    }

    /// One tool turn answered under `system` and appended as the loop appends it, then a prompt.
    fn take_turn(history: &mut Vec<Content>, profile: &ProviderProfile, system: &str, signature: &str) {
        let w = Wire::new(profile);
        w.build_body(&bound_request(system, history.clone()));
        let payloads = thinking_turn(signature);
        let payloads: Vec<&str> = payloads.iter().map(String::as_str).collect();
        history.extend(answered(stored(&run(&w, &payloads))));
        history.push(Content::user_text("and again"));
    }

    /// Which of the test signatures a request under `system` carries.
    fn signatures_sent(profile: &ProviderProfile, system: &str, history: &[Content]) -> Vec<&'static str> {
        let body = Wire::new(profile).build_body(&bound_request(system, history.to_vec())).to_string();
        ["sig-a", "sig-b", "sig-c"].into_iter().filter(|s| body.contains(s)).collect()
    }

    #[test]
    fn reasoning_goes_back_only_to_the_request_shape_that_produced_it() {
        // Anthropic checks a returned block against the system prompt, tools and messages before
        // it, and current models answer a mismatch with a 400 on every later request.
        let p = profile();
        let mut history = vec![Content::user_text("list it")];
        for signature in ["sig-a", "sig-b", "sig-c"] {
            take_turn(&mut history, &p, "prompt A", signature);
        }
        assert_eq!(signatures_sent(&p, "prompt A", &history), ["sig-a", "sig-b", "sig-c"]);

        // A changed system prompt (another mode, a rebuilt file tree), model or endpoint.
        assert!(signatures_sent(&p, "prompt B", &history).is_empty());
        let other_model = ProviderProfile { model: "claude-sonnet-5-5".into(), ..profile() };
        assert!(signatures_sent(&other_model, "prompt A", &history).is_empty());
        let other_host = ProviderProfile { base_url: Some("http://localhost:9/v1".into()), ..profile() };
        assert!(signatures_sent(&other_host, "prompt A", &history).is_empty());

        // Compaction replaces the older turns with a summary, and what was kept was bound to them.
        let mut compacted = vec![Content::user_text("[Conversation summary] listed it twice"), Content::model(vec![Part::text("Understood.")])];
        compacted.extend_from_slice(&history[6..]);
        assert!(signatures_sent(&p, "prompt A", &compacted).is_empty());
    }

    #[test]
    fn reasoning_produced_without_an_earlier_block_never_travels_with_it() {
        // A turn under another system prompt is bound to a history without the first turn's
        // reasoning. When the first prompt returns, that reasoning is valid again and the second
        // turn's is not: putting a block back invalidates what was produced while it was gone.
        let p = profile();
        let mut history = vec![Content::user_text("list it")];
        take_turn(&mut history, &p, "prompt A", "sig-a");
        take_turn(&mut history, &p, "prompt B", "sig-b");
        assert_eq!(signatures_sent(&p, "prompt B", &history), ["sig-b"]);
        take_turn(&mut history, &p, "prompt A", "sig-c");
        assert_eq!(signatures_sent(&p, "prompt A", &history), ["sig-a", "sig-c"]);
        assert_eq!(signatures_sent(&p, "prompt B", &history), ["sig-b"]);
    }

    #[test]
    fn reasoning_after_a_held_call_keeps_its_place() {
        let w = wire();
        w.build_body(&bound_request("sys", vec![Content::user_text("go")]));
        let content = stored(&run(
            &w,
            &[
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"s0"}}"#,
                r#"{"type":"content_block_stop","index":0}"#,
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_a","name":"glob","input":{}}}"#,
                r#"{"type":"content_block_stop","index":1}"#,
                r#"{"type":"content_block_start","index":2,"content_block":{"type":"thinking","thinking":"","signature":""}}"#,
                r#"{"type":"content_block_delta","index":2,"delta":{"type":"signature_delta","signature":"s2"}}"#,
                r#"{"type":"content_block_stop","index":2}"#,
                r#"{"type":"content_block_start","index":3,"content_block":{"type":"tool_use","id":"toolu_b","name":"glob","input":{}}}"#,
                r#"{"type":"content_block_stop","index":3}"#,
                r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":9}}"#,
                r#"{"type":"message_stop"}"#,
            ],
        ));
        let order: Vec<String> = content
            .parts
            .iter()
            .map(|p| match (&p.provider_block, &p.function_call) {
                (Some(b), _) => b.block["signature"].as_str().unwrap_or("").to_owned(),
                (None, Some(c)) => c.id.clone().unwrap_or_default(),
                _ => "text".to_owned(),
            })
            .collect();
        assert_eq!(order, ["s0", "glob__toolu_a", "s2", "glob__toolu_b"]);
    }

    #[test]
    fn a_reasoning_block_cut_before_its_signature_is_not_kept() {
        let w = wire();
        w.build_body(&bound_request("sys", vec![Content::user_text("go")]));
        let content = stored(&run(
            &w,
            &[
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"half a tho"}}"#,
            ],
        ));
        assert!(content.parts.iter().all(|p| p.provider_block.is_none()), "{content:?}");
    }

    #[test]
    fn an_api_error_that_says_what_to_change_is_shown_not_retried() {
        let w = wire();
        let mut state = Decoder::new();
        // What a proxy in front of a thinking model sent for a tool turn that lost its thinking.
        let stuck = r#"{"type":"error","error":{"type":"api_error","message":"The selected model cannot continue this conversation. Please select a different model, or start a new conversation, or rewind the current conversation to an earlier step and try again."}}"#;
        let err = w.decode(&mut state, stuck).unwrap_err();
        assert!(!err.is_retryable(), "{err:?}");
        assert!(err.to_string().contains("cannot continue this conversation"), "{err}");
        // Anthropic's own server faults are still retried, with or without a message.
        for transient in [
            r#"{"type":"error","error":{"type":"api_error","message":"Internal server error"}}"#,
            r#"{"type":"error","error":{"type":"api_error"}}"#,
            r#"{"type":"error","error":{"type":"timeout_error","message":"Request timed out"}}"#,
        ] {
            assert!(w.decode(&mut state, transient).unwrap_err().is_retryable(), "{transient}");
        }
    }
}
