//! Codex dialect (ChatGPT's Responses API: `POST {base}/responses`, SSE events).
//!
//! The Responses API is not chat-completions with another name. Three of its properties decide the
//! shape of this file:
//!
//! * **The history is a flat item list, not a message list.** Text, tool calls and tool results are
//!   separate items in one `input` array, and their order is the conversation. So the projection
//!   walks our [`Part`]s directly instead of routing through [`common::expand_content`], which drops
//!   [`Part::provider_block`] — and that block is the second property.
//! * **Reasoning must be replayed.** With `store: false`, the model's own reasoning item (with its
//!   `encrypted_content`) has to come back on the next request, in place, or a tool turn that a
//!   thinking model made is refused. The item is kept whole in `Part::provider_block` tagged
//!   `"codex"` and replayed verbatim; [`crate::api::retain_provider_blocks`] has already removed any
//!   other provider's reasoning before this dialect sees the history.
//! * **Arguments stream as fragments on a named item.** A `function_call` item is announced with an
//!   `output_index`, its arguments arrive one `response.function_call_arguments.delta` at a time,
//!   and the item is only real at `response.output_item.done`. The shared [`Decoder`] holds them.
//!
//! Nothing here imitates the official client: the `User-Agent` and `originator` say
//! `anticraft-native`, not `codex-tui/…`, and no turn-metadata or cloaking header is sent.

use std::collections::HashSet;
use std::fmt;

use serde_json::{json, Map, Value};

use crate::agent::mcp::schema::sanitize_schema;
use crate::api::{
    CaError, CaGenerateResponse, Candidate, Content, FunctionDeclaration, GenerateContentRequest,
    GenerateContentResponse, Part, Role,
};
use crate::providers::common::{
    call_chunk, part_chunk, synthesize_call_id, text_chunk, thought_chunk, usage_chunk, Decoder,
    Dialect, PartialCall, WireAuth, MAX_ARG_BYTES,
};
use crate::providers::ProviderProfile;

/// Provider tag written into [`crate::api::ProviderBlock::provider`] for a replayed reasoning item.
/// It is [`crate::providers::ProviderKind::as_str`] so the tag and the retain call cannot drift
/// apart.
const CODEX: &str = "codex";

/// Highest `output_index` honoured in one stream.
///
/// [`Decoder::slot`] sizes itself to the index it is asked for, so an index of a billion would turn
/// a single hostile frame into gigabytes of allocation. Items in one turn are counted in single
/// digits.
const MAX_SLOTS: usize = 128;

/// One profile rendered as the Codex backend.
pub struct Wire {
    base_url: String,
    model: String,
    /// `Chatgpt-Account-Id`; required by the backend when the token is a ChatGPT subscription's.
    account_id: Option<String>,
    max_output_tokens: Option<u32>,
    /// Seed for ids a call is missing, so a retried turn produces the same ids it produced before.
    seed: String,
    /// What a reasoning item this wire receives is bound to: the profile id, which for a
    /// subscription account is `acct:<account id>`. `encrypted_content` is sealed for the
    /// workspace that produced it, so another account's item is a 400 ("could not be verified")
    /// rather than reasoning — it is left out instead (see `input_items`).
    binding: String,
}

impl Wire {
    pub fn new(profile: &ProviderProfile, account_id: Option<&str>) -> Self {
        Self {
            base_url: profile.resolved_base_url(),
            model: profile.model.trim().to_owned(),
            account_id: account_id.map(str::trim).filter(|a| !a.is_empty()).map(str::to_owned),
            max_output_tokens: profile.max_output_tokens,
            seed: format!("{}|{}", profile.kind.as_str(), profile.model.trim()),
            binding: profile.id.clone(),
        }
    }

    /// Map a streamed error event onto the loop's vocabulary.
    ///
    /// A 401/403 mid-stream is an authentication failure, not a decode fault: the loop must stop
    /// and show the sign-in banner rather than retry. Everything else is reported as the provider
    /// wrote it; the driver scrubs any credential from the message.
    fn stream_error(error: &Value) -> CaError {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .filter(|m| !m.trim().is_empty())
            .unwrap_or("the provider reported an error");
        let word = |key: &str| error.get(key).and_then(Value::as_str).map(|s| s.trim().to_ascii_lowercase()).unwrap_or_default();
        let kind = word("type");
        // `code` is a status number on some frames (`401`) and a word on others
        // (`rate_limit_exceeded`); both spellings are read.
        let status = error.get("code").and_then(Value::as_u64).map(|c| c as u16);
        let code = word("code");
        // The plan's quota is spent: retrying cannot help until it resets, and the message says
        // when. Checked first, since the frame may also carry a 429.
        if kind == "usage_limit_reached" || code == "usage_limit_reached" {
            return CaError::Decode(message.to_owned());
        }
        if matches!(status, Some(401 | 403))
            || kind == "authentication_error"
            || matches!(code.as_str(), "invalid_api_key" | "token_expired" | "unauthorized")
        {
            return CaError::Auth(message.to_owned());
        }
        // Rate limits and an overloaded or failing server are worth another attempt; a rejected
        // request is not, and retrying it only spends the attempt budget.
        if status == Some(429)
            || matches!(kind.as_str(), "rate_limit_error" | "server_error" | "service_unavailable_error")
            || matches!(code.as_str(), "rate_limit_exceeded" | "server_error" | "server_is_overloaded")
        {
            return CaError::Retryable { delay: None };
        }
        CaError::Decode(message.to_owned())
    }
}

impl fmt::Debug for Wire {
    /// Hand-written: the account id is not a secret but the type must not become the hole in a
    /// redaction chain if a credential is ever added to it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("codex::Wire")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("has_account_id", &self.account_id.is_some())
            .finish()
    }
}

impl Dialect for Wire {
    fn endpoint(&self) -> String {
        // A paste of the documented endpoint is common; appending again would be a 404 that reads
        // like a bad model name.
        if self.base_url.ends_with("/responses") {
            self.base_url.clone()
        } else {
            format!("{}/responses", self.base_url.trim_end_matches('/'))
        }
    }

    fn build_body(&self, req: &GenerateContentRequest) -> Value {
        let mut body = Map::new();
        body.insert("model".to_owned(), json!(self.model));
        // `instructions` is present and empty, always. The ChatGPT backend checks that field
        // against its own client's prompt and refuses a request whose instructions it does not
        // recognise, so our system prompt travels as the first input item instead — a `developer`
        // message, which is how the Responses API carries a system role (CLIProxyAPI's
        // translators do the same).
        body.insert("instructions".to_owned(), json!(""));
        let mut input = Vec::new();
        if let Some(system) = req.system_instruction.as_ref().map(Content::text) {
            flush_text(&mut input, "developer", "input_text", &mut Some(system));
        }
        input.extend(input_items(req, &self.seed, &self.binding));
        body.insert("input".to_owned(), Value::Array(input));
        if let Some(tools) = tools(req) {
            body.insert("tools".to_owned(), tools);
        }
        body.insert("stream".to_owned(), json!(true));
        // Nothing is stored server-side, so state that must survive a turn comes back through the
        // input array — which is exactly why `include` asks for the encrypted reasoning content.
        body.insert("store".to_owned(), json!(false));
        body.insert("include".to_owned(), json!(["reasoning.encrypted_content"]));
        body.insert("parallel_tool_calls".to_owned(), json!(true));

        if let Some(cap) = req
            .generation_config
            .as_ref()
            .and_then(|g| g.max_output_tokens)
            .or(self.max_output_tokens)
        {
            body.insert("max_output_tokens".to_owned(), json!(cap));
        }
        // Sampling knobs (temperature, top_p, top_k) and the thinking config are not sent: this
        // backend chooses reasoning behaviour from the model and rejects `thinkingConfig`, and a
        // Code Assist default is not a user choice worth exporting.
        Value::Object(body)
    }

    fn apply_headers(&self, rb: reqwest::RequestBuilder, auth: WireAuth<'_>) -> reqwest::RequestBuilder {
        let token = match auth {
            // The backend takes a ChatGPT token as a bearer; an API key would go to a different
            // host entirely, and a profile of this kind cannot be created.
            WireAuth::Bearer(token) => token,
            WireAuth::Key(key) => key,
        };
        let mut rb = rb
            .bearer_auth(token)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::ACCEPT, "text/event-stream")
            // Ours, not `codex-tui`: the provider can tell a third-party client apart, which is the
            // honest arrangement and what the Account tab warns about.
            .header("originator", "anticraft-native")
            .header(reqwest::header::USER_AGENT, CODEX_UA);
        if let Some(account) = &self.account_id {
            rb = rb.header("Chatgpt-Account-Id", account);
        }
        rb
    }

    fn decode(&self, state: &mut Decoder, payload: &str) -> Result<Vec<CaGenerateResponse>, CaError> {
        let payload = payload.trim();
        if payload.is_empty() {
            return Ok(Vec::new());
        }
        let event: Value = match serde_json::from_str(payload) {
            Ok(event) => event,
            Err(error) => {
                // One unreadable frame is not worth the turn: the text before it has already been
                // delivered, and an `event:` line may arrive with an empty data field.
                tracing::warn!(error = %error, len = payload.len(), "codex stream frame is not JSON");
                return Ok(Vec::new());
            }
        };
        let kind = event.get("type").and_then(Value::as_str).unwrap_or_default();
        let mut out = Vec::new();

        match kind {
            "response.output_text.delta" => {
                if let Some(text) = event.get("delta").and_then(Value::as_str).filter(|t| !t.is_empty()) {
                    out.push(text_chunk(text));
                }
            }
            // Summary and full reasoning text both reach the UI; the loop drops thought parts
            // before history, so showing them costs nothing.
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                if let Some(text) = event.get("delta").and_then(Value::as_str).filter(|t| !t.is_empty()) {
                    out.push(thought_chunk(text));
                }
            }

            "response.output_item.added" => {
                let item = event.get("item").cloned().unwrap_or(Value::Null);
                if item.get("type").and_then(Value::as_str) == Some("function_call") {
                    if let Some(slot) = item_slot(&event, state) {
                        let pending = state.slot(slot);
                        if let Some(id) = item.get("call_id").and_then(Value::as_str) {
                            pending.id = Some(id.to_owned());
                        }
                        if let Some(name) = item.get("name").and_then(Value::as_str) {
                            pending.name = name.to_owned();
                        }
                        // Some servers send the complete argument object here and no deltas.
                        if let Some(arguments) = item.get("arguments").and_then(Value::as_str).filter(|a| !a.is_empty()) {
                            append_args(pending, arguments);
                        }
                    }
                }
            }

            "response.function_call_arguments.delta" => {
                let fragment = event.get("delta").and_then(Value::as_str).unwrap_or_default();
                if !fragment.is_empty() {
                    if let Some(slot) = item_slot(&event, state) {
                        append_args(state.slot(slot), fragment);
                    }
                }
            }

            "response.output_item.done" => {
                let item = event.get("item").cloned().unwrap_or(Value::Null);
                match item.get("type").and_then(Value::as_str) {
                    Some("function_call") => {
                        let index = item_index(&event);
                        // Take whatever the slot accumulated, then fill in from the finished item:
                        // the item carries the authoritative name and arguments.
                        let mut call = index.and_then(|i| state.take(i)).unwrap_or_default();
                        if call.name.is_empty() {
                            call.name = item.get("name").and_then(Value::as_str).unwrap_or_default().to_owned();
                        }
                        if call.id.is_none() {
                            call.id = item.get("call_id").and_then(Value::as_str).map(str::to_owned);
                        }
                        if call.args.trim().is_empty() {
                            if let Some(arguments) = item.get("arguments").and_then(Value::as_str) {
                                call.args = arguments.to_owned();
                            }
                        }
                        if call.name.is_empty() {
                            tracing::warn!("codex sent a function_call item with no name; dropped");
                            return Ok(out);
                        }
                        let fallback = synthesize_call_id(&self.seed, index.unwrap_or(0));
                        out.push(call_chunk(call.into_call(fallback)));
                    }
                    // A completed reasoning item is the only thing that makes the next turn legal
                    // for a thinking model: replay it verbatim, tagged as ours and as this
                    // account's.
                    Some("reasoning") => {
                        out.push(part_chunk(Part {
                            provider_block: Some(crate::api::ProviderBlock {
                                provider: CODEX.to_owned(),
                                binding: Some(self.binding.clone()),
                                block: item,
                            }),
                            ..Default::default()
                        }));
                    }
                    _ => {}
                }
            }

            "response.completed" | "response.incomplete" => {
                let response = event.get("response").cloned().unwrap_or(Value::Null);
                if let Some(usage) = response.get("usage").filter(|u| u.is_object()) {
                    let prompt = usage.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
                    let output = usage.get("output_tokens").and_then(Value::as_u64).unwrap_or(0);
                    if prompt > 0 || output > 0 {
                        out.push(usage_chunk(prompt, output));
                    }
                }
                let reason = response.get("incomplete_details").and_then(|d| d.get("reason")).and_then(Value::as_str);
                if reason == Some("content_filter") {
                    // Stopped by the provider's filter. Usually little or nothing was written, and
                    // an empty reply is something the loop retries — into the same filter. So the
                    // turn ends here, with the reason, and no call it started is run.
                    state.take_all();
                    return Err(CaError::Decode(
                        "ChatGPT stopped this reply at its content filter. Rephrase the request, or pick another model for this turn.".into(),
                    ));
                }
                // Anything the stream announced but never closed is still a call the model made,
                // and the loop would otherwise leave it unanswered.
                out.extend(flush_calls(state, &self.seed));
                // `max_output_tokens` is what the Responses API says; `max_tokens` is accepted
                // too, as older frames spelled it.
                let cut = matches!(reason, Some("max_output_tokens" | "max_tokens"));
                out.push(finish_chunk(if cut { "MAX_TOKENS" } else { "STOP" }));
            }

            "response.failed" => {
                let error = event.get("response").and_then(|r| r.get("error")).cloned().unwrap_or(Value::Null);
                return Err(Self::stream_error(&error));
            }
            "error" => {
                let error = event.get("error").filter(|e| e.is_object()).cloned().unwrap_or_else(|| event.clone());
                return Err(Self::stream_error(&error));
            }

            // `response.created`, `response.in_progress`, `response.content_part.*`, the
            // `…done` variants and anything newer are ignored: an unknown event must not abort a
            // turn that is otherwise delivering text.
            _ => {}
        }

        Ok(out)
    }

    fn finish(&self, state: &mut Decoder) -> Result<Vec<CaGenerateResponse>, CaError> {
        // A stream that closes without `response.completed` still streamed its calls.
        Ok(flush_calls(state, &self.seed))
    }
}

/// `User-Agent` sent with every request. Ours, deliberately: no `codex-tui/…`.
const CODEX_UA: &str = concat!("anticraft-native/", env!("CARGO_PKG_VERSION"));

/// Append a fragment to a slot's argument text, under the shared cap.
fn append_args(slot: &mut PartialCall, fragment: &str) {
    let room = MAX_ARG_BYTES.saturating_sub(slot.args.len());
    if fragment.len() > room {
        tracing::warn!(tool = %slot.name, "tool arguments exceeded the size cap; keeping the prefix");
        let mut end = room;
        while end > 0 && !fragment.is_char_boundary(end) {
            end -= 1;
        }
        slot.args.push_str(&fragment[..end]);
    } else {
        slot.args.push_str(fragment);
    }
}

/// Slot an item event belongs to: its `output_index`, else a running counter.
///
/// The Responses API names items by `output_index`, and every delta repeats the index of the item
/// it belongs to. A server that omits it is handled by falling back to the item id's hash, which is
/// stable for one item across its events, rather than a counter that would open a new slot per
/// delta. An index past the cap is refused rather than sized for.
fn item_index(event: &Value) -> Option<usize> {
    if let Some(index) = event.get("output_index").and_then(Value::as_u64) {
        let index = index as usize;
        if index >= MAX_SLOTS {
            tracing::warn!(index, "codex output index beyond the supported range");
            return None;
        }
        return Some(index);
    }
    let id = event
        .get("item_id")
        .and_then(Value::as_str)
        .or_else(|| event.get("item").and_then(|i| i.get("id")).and_then(Value::as_str))?;
    Some(stable_slot(id))
}

/// Slot for an item event carrying no `output_index`, from the item id.
fn item_slot(event: &Value, state: &mut Decoder) -> Option<usize> {
    let index = item_index(event)?;
    // Touch the slot so the id's events land in one place even before any delta arrives.
    state.slot(index);
    Some(index)
}

/// A slot number derived from an item id, so every event for one item lands in the same slot.
fn stable_slot(id: &str) -> usize {
    let hash = id.bytes().fold(0u64, |acc, b| acc.wrapping_mul(31).wrapping_add(u64::from(b)));
    (hash as usize) % MAX_SLOTS
}

/// Release every call the stream has completed.
fn flush_calls(state: &mut Decoder, seed: &str) -> Vec<CaGenerateResponse> {
    state
        .take_all()
        .into_iter()
        .enumerate()
        .map(|(position, call)| call_chunk(call.into_call(synthesize_call_id(seed, position))))
        .collect()
}

/// Final chunk carrying the finish reason.
fn finish_chunk(reason: &str) -> CaGenerateResponse {
    CaGenerateResponse {
        response: Some(GenerateContentResponse {
            candidates: vec![Candidate { content: None, finish_reason: Some(reason.to_owned()), index: Some(0) }],
            ..Default::default()
        }),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Request: history projection
// ---------------------------------------------------------------------------

/// Project the whole history as the flat `input` item list.
///
/// Read off the parts directly rather than through `common::expand_content`: that helper drops
/// `provider_block`, and a replayed reasoning item is exactly what a thinking model's tool turn
/// needs. The order of items is the conversation, so text of one turn is collected into a single
/// message and flushed before anything that must not be folded into it.
///
/// A reasoning item is replayed only to the account that produced it (`binding`): its
/// `encrypted_content` is sealed for that workspace. One from another account, or from before
/// items were bound, is left out — dropping reasoning costs the model some context, sending it
/// costs the turn.
fn input_items(req: &GenerateContentRequest, seed: &str, binding: &str) -> Vec<Value> {
    let mut out = Vec::new();
    let mut synthesized = 0usize;
    for content in &req.contents {
        let is_model = content.role == Some(Role::Model);
        let role = if is_model { "assistant" } else { "user" };
        let text_type = if is_model { "output_text" } else { "input_text" };
        // Text of this content, collected until a part that must stand on its own in the list.
        let mut text: Option<String> = None;
        for part in &content.parts {
            if let Some(block) = &part.provider_block {
                // Only our own blocks survive the driver's retain pass; a block from another
                // provider would be a bug, and replaying it would send a foreign shape.
                if block.provider == CODEX && block.binding.as_deref() == Some(binding) {
                    flush_text(&mut out, role, text_type, &mut text);
                    out.push(block.block.clone());
                }
                continue;
            }
            if let Some(call) = &part.function_call {
                if !is_model {
                    continue;
                }
                flush_text(&mut out, role, text_type, &mut text);
                let call_id = match call.id.as_deref().map(str::trim).filter(|id| !id.is_empty()) {
                    Some(id) => wire_call_id(&call.name, id),
                    None => {
                        synthesized += 1;
                        synthesize_call_id(seed, synthesized)
                    }
                };
                out.push(json!({
                    "type": "function_call",
                    "call_id": call_id,
                    "name": call.name,
                    // Arguments are a JSON *string* on this wire, not an object.
                    "arguments": arguments_text(&call.args),
                }));
                continue;
            }
            if let Some(response) = &part.function_response {
                if is_model {
                    continue;
                }
                flush_text(&mut out, role, text_type, &mut text);
                let call_id = response
                    .id
                    .as_deref()
                    .map(str::trim)
                    .filter(|id| !id.is_empty())
                    .map(|id| wire_call_id(&response.name, id))
                    .unwrap_or_else(|| {
                        synthesized += 1;
                        synthesize_call_id(seed, synthesized)
                    });
                out.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": tool_result_text(&response.response),
                }));
                continue;
            }
            // Thought parts are not replayed as text: the reasoning item above is the model's own
            // record, and folding its readable form into the answer would teach the model that it
            // had spoken its thinking aloud.
            if let Some(chunk) = part.text.as_deref().filter(|t| !t.is_empty() && !part.is_thought()) {
                text.get_or_insert_with(String::new).push_str(chunk);
            }
        }
        flush_text(&mut out, role, text_type, &mut text);
    }
    out
}

/// Emit the collected text as one message item, if there is any.
fn flush_text(out: &mut Vec<Value>, role: &str, text_type: &str, text: &mut Option<String>) {
    let Some(text) = text.take() else { return };
    if text.trim().is_empty() {
        return;
    }
    out.push(json!({
        "type": "message",
        "role": role,
        "content": [{ "type": text_type, "text": text }],
    }));
}

/// The id the provider issued, without the loop's `{name}__` decoration.
///
/// The loop normalizes call ids that way so a resumed history cannot confuse two tools; the backend
/// must see the id it issued, because it pairs a result to its call by that id alone.
fn wire_call_id(name: &str, id: &str) -> String {
    const SEP: &str = "__";
    let prefix = format!("{name}{SEP}");
    id.strip_prefix(&prefix).unwrap_or(id).to_owned()
}

/// Our arguments are a JSON value; the wire wants that value as a string.
fn arguments_text(args: &Value) -> String {
    match args {
        Value::Null => "{}".to_owned(),
        other => serde_json::to_string(other).unwrap_or_else(|_| "{}".to_owned()),
    }
}

/// Text of a tool result: `{"output": …}` or `{"error": …}` as plain text, anything else as JSON.
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

/// Every tool declaration, flattened, in the Responses API's flat shape.
///
/// Unlike chat-completions, a function tool is `{type, name, description, parameters, strict}` with
/// no nested `function` object. `strict` is `false` because several of our tools take free-form
/// schemas, and a strict one must describe every field.
fn tools(req: &GenerateContentRequest) -> Option<Value> {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut out = Vec::new();
    for declaration in req.tools.iter().flatten().flat_map(|t| &t.function_declarations) {
        let name = declaration.name.trim();
        if name.is_empty() {
            tracing::warn!("skipping a tool declaration with no name");
            continue;
        }
        if !seen.insert(name) {
            tracing::warn!(tool = name, "skipping a duplicate tool declaration");
            continue;
        }
        out.push(tool_json(name, declaration));
    }
    if out.is_empty() {
        None
    } else {
        Some(Value::Array(out))
    }
}

/// One function tool.
fn tool_json(name: &str, declaration: &FunctionDeclaration) -> Value {
    json!({
        "type": "function",
        "name": name,
        "description": declaration.description,
        // The backend requires an object root with `properties`; a server-supplied schema that is a
        // union, a `$ref` or a bare string rejects the whole request, so it goes through the same
        // pass the Code Assist path uses.
        "parameters": sanitize_schema(&declaration.parameters_json_schema),
        "strict": false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{FunctionCall, FunctionResponse, GenerationConfig, Tool};
    use crate::providers::ProviderKind;
    use serde_json::json;

    fn profile() -> ProviderProfile {
        ProviderProfile {
            id: "acct:codex-0123abcd".into(),
            label: "ChatGPT (Codex)".into(),
            kind: ProviderKind::Codex,
            model: "gpt-5.1-codex".into(),
            base_url: None,
            api_key: String::new(),
            max_output_tokens: None,
            context_window: None,
            models: Vec::new(),
        }
    }

    fn wire() -> Wire {
        Wire::new(&profile(), Some("acct_123"))
    }

    /// Content a codex turn leaves behind: a reasoning item, a visible answer and a call.
    fn codex_model_turn() -> Content {
        Content::model(vec![
            Part {
                provider_block: Some(crate::api::ProviderBlock {
                    provider: CODEX.into(),
                    binding: Some(profile().id),
                    block: json!({"type": "reasoning", "id": "rs_1", "encrypted_content": "gAAAA-sealed"}),
                }),
                ..Default::default()
            },
            Part::text("Listing it."),
            Part {
                function_call: Some(FunctionCall {
                    id: Some("list_directory__call_abc".into()),
                    name: "list_directory".into(),
                    args: json!({ "dir_path": "." }),
                }),
                ..Default::default()
            },
        ])
    }

    fn history() -> GenerateContentRequest {
        GenerateContentRequest {
            contents: vec![
                Content::user_text("list the folder"),
                codex_model_turn(),
                Content {
                    role: Some(Role::User),
                    parts: vec![Part::function_response(FunctionResponse {
                        id: Some("list_directory__call_abc".into()),
                        name: "list_directory".into(),
                        response: json!({ "output": "a.rs\nb.rs" }),
                    })],
                },
                Content::user_text("now summarize"),
            ],
            system_instruction: Some(Content::user_text("be terse")),
            tools: Some(vec![Tool {
                function_declarations: vec![FunctionDeclaration {
                    name: "list_directory".into(),
                    description: "Lists a directory.".into(),
                    parameters_json_schema: json!({"type":"object","properties":{"dir_path":{"type":"string"}},"required":["dir_path"]}),
                }],
            }]),
            generation_config: Some(GenerationConfig {
                temperature: Some(0.4),
                max_output_tokens: Some(512),
                thinking_config: Some(Default::default()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// Decode a whole turn, flattened.
    fn run(wire: &Wire, frames: &[&str]) -> Result<Vec<CaGenerateResponse>, CaError> {
        let mut state = Decoder::new();
        let mut out = Vec::new();
        for frame in frames {
            out.extend(wire.decode(&mut state, frame)?);
        }
        out.extend(wire.finish(&mut state)?);
        Ok(out)
    }

    #[test]
    fn endpoint_appends_the_route_once() {
        assert_eq!(wire().endpoint(), "https://chatgpt.com/backend-api/codex/responses");
        let mut pasted = profile();
        pasted.base_url = Some("https://chatgpt.com/backend-api/codex/responses".into());
        assert_eq!(Wire::new(&pasted, None).endpoint(), "https://chatgpt.com/backend-api/codex/responses");
        pasted.base_url = Some("http://localhost:9999/v1/".into());
        assert_eq!(Wire::new(&pasted, None).endpoint(), "http://localhost:9999/v1/responses");
    }

    #[test]
    fn headers_carry_the_bearer_account_and_our_own_identity() {
        let built = wire()
            .apply_headers(reqwest::Client::new().post("http://example.test"), WireAuth::Bearer("eyJ.token"))
            .build()
            .expect("request builds");
        assert_eq!(built.headers().get("authorization").and_then(|v| v.to_str().ok()), Some("Bearer eyJ.token"));
        assert_eq!(built.headers().get("chatgpt-account-id").and_then(|v| v.to_str().ok()), Some("acct_123"));
        assert_eq!(built.headers().get("accept").and_then(|v| v.to_str().ok()), Some("text/event-stream"));
        assert_eq!(built.headers().get("originator").and_then(|v| v.to_str().ok()), Some("anticraft-native"));
        let ua = built.headers().get("user-agent").and_then(|v| v.to_str().ok()).unwrap_or_default();
        assert!(ua.starts_with("anticraft-native/"), "{ua}");
        // Nothing that imitates the first-party client.
        assert!(!ua.contains("codex-tui"), "{ua}");
        assert!(built.headers().get("x-codex-turn-metadata").is_none());

        // Without an account id the header is absent rather than empty.
        let built = Wire::new(&profile(), None)
            .apply_headers(reqwest::Client::new().post("http://example.test"), WireAuth::Bearer("t"))
            .build()
            .expect("request builds");
        assert!(built.headers().get("chatgpt-account-id").is_none());
    }

    #[test]
    fn codex_body_maps_history_tools_and_reasoning() {
        let body = wire().build_body(&history());

        assert_eq!(body["model"], json!("gpt-5.1-codex"));
        // The system prompt is the first input item, not `instructions`, which the backend
        // checks against its own client's prompt.
        assert_eq!(body["instructions"], json!(""));
        assert_eq!(body["stream"], json!(true));
        assert_eq!(body["store"], json!(false));
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(body["parallel_tool_calls"], json!(true));
        assert_eq!(body["max_output_tokens"], json!(512));
        // A Code Assist sampling default is not exported, and this API rejects `thinkingConfig`.
        assert!(body.get("temperature").is_none());
        assert!(body.get("thinkingConfig").is_none());
        assert!(body.get("thinking_config").is_none());

        let input = body["input"].as_array().expect("input");
        assert_eq!(
            input,
            &vec![
                json!({"type":"message","role":"developer","content":[{"type":"input_text","text":"be terse"}]}),
                json!({"type":"message","role":"user","content":[{"type":"input_text","text":"list the folder"}]}),
                // Replayed verbatim, in place, before the text and call of its own turn.
                json!({"type": "reasoning", "id": "rs_1", "encrypted_content": "gAAAA-sealed"}),
                json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"Listing it."}]}),
                json!({"type":"function_call","call_id":"call_abc","name":"list_directory","arguments":"{\"dir_path\":\".\"}"}),
                json!({"type":"function_call_output","call_id":"call_abc","output":"a.rs\nb.rs"}),
                json!({"type":"message","role":"user","content":[{"type":"input_text","text":"now summarize"}]}),
            ]
        );
        // The loop's `{name}__` decoration is not sent: the backend pairs by the id it issued.
        assert!(!body.to_string().contains("list_directory__"));

        // Tools are flat, with an object root and `strict: false`.
        let tools = body["tools"].as_array().expect("tools");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], json!("function"));
        assert_eq!(tools[0]["name"], json!("list_directory"));
        assert_eq!(tools[0]["strict"], json!(false));
        assert_eq!(tools[0]["parameters"]["required"], json!(["dir_path"]));
        assert!(tools[0].get("function").is_none(), "the Responses API has no nested function object");
    }

    #[test]
    fn another_providers_reasoning_never_reaches_the_body() {
        // The driver's retain pass removes it; this asserts the projection does not resurrect it if
        // one slips through (a hand-built request in a test or a future caller).
        let mut model = codex_model_turn();
        model.parts.push(Part {
            provider_block: Some(crate::api::ProviderBlock {
                provider: "anthropic".into(),
                binding: Some("b".into()),
                block: json!({"type":"thinking","thinking":"secret plan","signature":"sig"}),
            }),
            ..Default::default()
        });
        let body = wire().build_body(&GenerateContentRequest {
            contents: vec![Content::user_text("go"), model],
            ..Default::default()
        });
        let text = body.to_string();
        assert!(!text.contains("secret plan"), "{text}");
        assert!(!text.contains("\"sig\""), "{text}");
        assert!(text.contains("gAAAA-sealed"), "our own block is still replayed");
    }

    #[test]
    fn reasoning_is_replayed_only_to_the_account_that_produced_it() {
        // Two ChatGPT accounts differ only in the profile id the app gives them. Account B cannot
        // decrypt account A's reasoning, so sending it would fail the turn.
        let history = || GenerateContentRequest {
            contents: vec![Content::user_text("go"), codex_model_turn(), Content::user_text("again")],
            ..Default::default()
        };
        let a = wire().build_body(&history()).to_string();
        assert!(a.contains("gAAAA-sealed"), "the account that produced it gets it back");

        let other = ProviderProfile { id: "acct:codex-89abcdef".into(), ..profile() };
        let b = Wire::new(&other, Some("acct_456")).build_body(&history());
        let text = b.to_string();
        assert!(!text.contains("gAAAA-sealed"), "{text}");
        // The rest of that turn still goes: its text and its call are history, not reasoning.
        assert!(text.contains("Listing it.") && text.contains("call_abc"), "{text}");

        // A block stored before items were bound is dropped as well, once.
        let mut turn = codex_model_turn();
        turn.parts[0].provider_block.as_mut().unwrap().binding = None;
        let unbound = wire().build_body(&GenerateContentRequest {
            contents: vec![Content::user_text("go"), turn],
            ..Default::default()
        });
        assert!(!unbound.to_string().contains("gAAAA-sealed"));
    }

    #[test]
    fn empty_and_call_only_turns_produce_no_empty_messages() {
        let body = wire().build_body(&GenerateContentRequest {
            contents: vec![
                Content::model(vec![]),
                Content::model(vec![Part { text: Some("   ".into()), ..Default::default() }]),
                Content::model(vec![Part {
                    function_call: Some(FunctionCall {
                        id: Some("call_x".into()),
                        name: "glob".into(),
                        args: Value::Null,
                    }),
                    ..Default::default()
                }]),
            ],
            ..Default::default()
        });
        let input = body["input"].as_array().expect("input");
        assert_eq!(input.len(), 1, "{input:?}");
        assert_eq!(input[0]["type"], json!("function_call"));
        assert_eq!(input[0]["arguments"], json!("{}"), "a null argument object is sent as an empty object");
        assert_eq!(body["instructions"], json!(""), "the field is always present");
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn codex_decodes_text_tool_calls_reasoning_and_completion() {
        let chunks = run(
            &wire(),
            &[
                r#"{"type":"response.created","response":{"id":"resp_1"}}"#,
                r#"{"type":"response.output_text.delta","delta":"Hel"}"#,
                r#"{"type":"response.output_text.delta","delta":"lo"}"#,
                r#"{"type":"response.completed","response":{"usage":{"input_tokens":11,"output_tokens":2,"total_tokens":13}}}"#,
            ],
        )
        .expect("decodes");
        let texts: String = chunks
            .iter()
            .filter_map(|c| c.first_parts().first())
            .filter_map(|p| p.text.clone())
            .collect();
        assert_eq!(texts, "Hello");
        let usage = chunks
            .iter()
            .filter_map(|c| c.response.as_ref().and_then(|r| r.usage_metadata))
            .next()
            .expect("usage");
        assert_eq!(usage.prompt_token_count, 11);
        assert_eq!(usage.candidates_token_count, 2);
        assert_eq!(
            chunks
                .last()
                .and_then(|c| c.response.as_ref())
                .and_then(|r| r.candidates.first())
                .and_then(|c| c.finish_reason.clone()),
            Some("STOP".to_owned())
        );
    }

    #[test]
    fn reasoning_deltas_become_thoughts_and_the_item_is_kept_whole() {
        let chunks = run(
            &wire(),
            &[
                r#"{"type":"response.reasoning_summary_text.delta","delta":"weigh "}"#,
                r#"{"type":"response.reasoning_text.delta","delta":"options"}"#,
                r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_2","encrypted_content":"sealed-2","summary":[]}}"#,
            ],
        )
        .expect("decodes");

        let thoughts: Vec<&str> = chunks
            .iter()
            .filter_map(|c| c.first_parts().first())
            .filter(|p| p.is_thought())
            .filter_map(|p| p.text.as_deref())
            .collect();
        assert_eq!(thoughts, vec!["weigh ", "options"]);

        let block = chunks
            .iter()
            .filter_map(|c| c.first_parts().first())
            .find_map(|p| p.provider_block.as_ref())
            .expect("the reasoning item is kept");
        assert_eq!(block.provider, "codex");
        assert_eq!(block.block["encrypted_content"], json!("sealed-2"));
        assert_eq!(block.block["id"], json!("rs_2"));
    }

    #[test]
    fn tool_calls_assemble_from_fragments_and_close_on_output_item_done() {
        let chunks = run(
            &wire(),
            &[
                r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","id":"fc_1","call_id":"call_abc","name":"read_file","arguments":""}}"#,
                r#"{"type":"response.function_call_arguments.delta","item_id":"fc_1","output_index":1,"delta":"{\"file_"}"#,
                r#"{"type":"response.function_call_arguments.delta","item_id":"fc_1","output_index":1,"delta":"path\":\"a.rs\"}"}"#,
                r#"{"type":"response.output_item.done","output_index":1,"item":{"type":"function_call","id":"fc_1","call_id":"call_abc","name":"read_file","arguments":"{\"file_path\":\"a.rs\"}"}}"#,
                r#"{"type":"response.completed","response":{"usage":{"input_tokens":1,"output_tokens":1}}}"#,
            ],
        )
        .expect("decodes");

        let calls: Vec<FunctionCall> = chunks
            .iter()
            .filter_map(|c| c.first_parts().first())
            .filter_map(|p| p.function_call.clone())
            .collect();
        assert_eq!(calls.len(), 1, "{chunks:?}");
        assert_eq!(calls[0].id.as_deref(), Some("call_abc"));
        assert_eq!(calls[0].name, "read_file");
        assert_eq!(calls[0].args, json!({"file_path": "a.rs"}));
    }

    #[test]
    fn a_call_is_still_released_when_the_item_done_event_never_arrives() {
        let chunks = run(
            &wire(),
            &[
                r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"call_z","name":"glob","arguments":"{\"pattern\":\"*.rs\"}"}}"#,
            ],
        )
        .expect("decodes");
        // The added item carried complete arguments, but only `finish` releases it.
        let calls: Vec<FunctionCall> = chunks
            .iter()
            .filter_map(|c| c.first_parts().first())
            .filter_map(|p| p.function_call.clone())
            .collect();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].args, json!({"pattern": "*.rs"}));
    }

    #[test]
    fn interruption_maps_to_max_tokens_and_failure_to_an_error() {
        for reason in ["max_output_tokens", "max_tokens"] {
            let frame = format!(
                r#"{{"type":"response.incomplete","response":{{"incomplete_details":{{"reason":"{reason}"}},"usage":{{"input_tokens":1,"output_tokens":9}}}}}}"#
            );
            let chunks = run(&wire(), &[frame.as_str()]).expect("decodes");
            assert_eq!(
                chunks
                    .last()
                    .and_then(|c| c.response.as_ref())
                    .and_then(|r| r.candidates.first())
                    .and_then(|c| c.finish_reason.clone()),
                Some("MAX_TOKENS".to_owned()),
                "{reason}"
            );
        }

        // The content filter ends the turn with its reason instead of an empty reply to retry.
        let mut state = Decoder::new();
        let filtered = r#"{"type":"response.incomplete","response":{"incomplete_details":{"reason":"content_filter"}}}"#;
        assert!(matches!(wire().decode(&mut state, filtered), Err(CaError::Decode(m)) if m.contains("content filter")));

        let mut state = Decoder::new();
        let failed = r#"{"type":"response.failed","response":{"error":{"code":401,"message":"bad token"}}}"#;
        assert!(matches!(wire().decode(&mut state, failed), Err(CaError::Auth(ref m)) if m == "bad token"));

        let error = r#"{"type":"error","error":{"code":500,"type":"server_error","message":"overloaded"}}"#;
        assert!(matches!(wire().decode(&mut state, error), Err(CaError::Retryable { .. })));

        let rejected = r#"{"type":"error","error":{"code":400,"message":"bad model"}}"#;
        match wire().decode(&mut state, rejected) {
            Err(CaError::Decode(m)) => assert_eq!(m, "bad model"),
            other => panic!("expected a decode error, got {other:?}"),
        }
    }

    #[test]
    fn error_codes_spelled_as_words_are_classified_too() {
        let classify = |frame: &str| wire().decode(&mut Decoder::new(), frame).unwrap_err();
        for frame in [
            r#"{"type":"error","error":{"type":"invalid_request_error","code":"rate_limit_exceeded","message":"slow down"}}"#,
            r#"{"type":"error","error":{"code":"server_is_overloaded","message":"busy"}}"#,
            r#"{"type":"response.failed","response":{"error":{"code":"server_error","message":"oops"}}}"#,
        ] {
            assert!(matches!(classify(frame), CaError::Retryable { .. }), "{frame}");
        }
        for frame in [
            r#"{"type":"error","error":{"code":"token_expired","message":"expired"}}"#,
            r#"{"type":"error","error":{"type":"authentication_error","message":"who?"}}"#,
            r#"{"type":"error","error":{"code":"invalid_api_key","message":"no"}}"#,
        ] {
            assert!(matches!(classify(frame), CaError::Auth(_)), "{frame}");
        }
        // A spent plan quota is not retried, even when the frame also says 429: the message says
        // when it resets.
        let quota = r#"{"type":"error","error":{"type":"usage_limit_reached","code":429,"message":"You hit your usage limit. Try again in 3 hours."}}"#;
        assert!(matches!(classify(quota), CaError::Decode(m) if m.contains("3 hours")));
    }

    #[test]
    fn malformed_and_unknown_frames_are_skipped_without_panicking() {
        let mut state = Decoder::new();
        for frame in [
            "",
            "   ",
            "[not json",
            "{}",
            r#"{"type":"response.created","response":{}}"#,
            r#"{"type":"response.output_text.delta"}"#,
            r#"{"type":"response.output_item.added","item":{}}"#,
            r#"{"type":"response.output_item.done","item":{"type":"function_call"}}"#,
        ] {
            let chunks = wire().decode(&mut state, frame).expect("frame is tolerated");
            assert!(chunks.is_empty() || !frame.is_empty(), "{frame}");
        }
    }

    #[test]
    fn an_absurd_output_index_is_refused_instead_of_allocated() {
        let mut state = Decoder::new();
        let frame = r#"{"type":"response.function_call_arguments.delta","output_index":1000000000,"delta":"{}"}"#;
        assert!(wire().decode(&mut state, frame).expect("tolerated").is_empty());
        assert!(wire().finish(&mut state).expect("finish").is_empty());
    }
}
