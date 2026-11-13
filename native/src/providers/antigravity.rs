//! Antigravity dialect (`POST {base}/v1internal:streamGenerateContent?alt=sse`).
//!
//! Antigravity's backend is Code Assist's sibling: the stream is the same Gemini JSON, but it is
//! wrapped in an envelope and served from a different host with a different request shape. So this
//! file does four things and inherits everything else:
//!
//! * it wraps our `GenerateContentRequest` in `{model, userAgent, requestType, project, requestId,
//!   request}` — the fields the backend reads off the envelope rather than the body;
//! * it accepts **either** response envelope. A Gemini-family model streams a bare
//!   `{"candidates": …}` while a Claude-family model is re-emitted as `{"response": {"candidates":
//!   …}}`; our own `CaGenerateResponse` deserializer reads the second shape as an empty default,
//!   so the unwrap happens here;
//! * it hands the unwrapped object to [`gemini::decode_gemini_value_with`], which is where the
//!   part walking, function-call assembly, usage attachment and `google.rpc` error mapping live;
//! * it sends a tool round back the way the backend checks it ([`Wire::project_contents`]): every
//!   result names the call it answers, and a Claude model gets back the signed thinking that came
//!   before its calls, kept while it streamed ([`Wire::keep_thinking`]). Without them the request
//!   carrying the tool results is refused (HTTP 400) and the turn ends as soon as the tools ran.
//!
//! What it does **not** do is imitate the reference client. The User-Agent says what we are
//! (`anticraft-native/… (antigravity-oauth)`), not `antigravity/hub/…`; see the contract's §8.
//! The credential is an OAuth bearer from a first-party sign-in, never a key.

use std::sync::Mutex;

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::agent::history::SKIP_THOUGHT_SIGNATURE;
use crate::api::{CaError, CaGenerateResponse, GenerateContentRequest, Part, ProviderBlock};
use crate::providers::common::{Decoder, Dialect, WireAuth, MAX_EVENT_BYTES};
use crate::providers::gemini;
use crate::providers::{ProviderKind, ProviderProfile};

/// Largest single event this dialect will parse. The shared parser already enforces
/// [`MAX_EVENT_BYTES`] per event; checking again keeps `decode` safe when a caller hands it a whole
/// response, as the tests do.
const MAX_PAYLOAD_BYTES: usize = MAX_EVENT_BYTES;

/// Sent as `userAgent` in the envelope. The backend only logs it; it is not a client fingerprint.
const ENVELOPE_USER_AGENT: &str = "anticraft-native";

/// Request kind the backend routes agent turns under.
const REQUEST_TYPE: &str = "agent";

/// One profile rendered as the Antigravity backend.
pub struct Wire {
    base_url: String,
    /// Normalized model id: no `models/` prefix, no `:method` suffix.
    model: String,
    /// Applied only when the request carries no cap of its own.
    max_output_tokens: Option<u32>,
    /// The account's discovered Cloud project. `None` when sign-in could not find one; the request
    /// is still sent, because the backend can resolve it from the token for some accounts.
    project: Option<String>,
    /// Input to the synthesized tool-call ids, stable across a retry of the same turn.
    seed: String,
    /// The model is one of the backend's Claude models, which needs its thinking back.
    claude: bool,
    /// What the thinking this request keeps is bound to, see [`binding`].
    binding: String,
    /// Text of the thinking block still streaming, until its signature closes it.
    thinking: Mutex<String>,
}

impl Wire {
    pub fn new(profile: &ProviderProfile, project: Option<&str>) -> Self {
        let raw = profile.model.trim();
        let model = raw
            .split_once(':')
            .map_or(raw, |(head, _)| head)
            .strip_prefix("models/")
            .unwrap_or_else(|| raw.split_once(':').map_or(raw, |(head, _)| head))
            .trim()
            .to_owned();
        let base_url = profile.resolved_base_url();
        Self {
            seed: format!("{}|{}", profile.kind.as_str(), profile.model.trim()),
            claude: is_claude(&model),
            binding: binding(&profile.id, &base_url, &model),
            thinking: Mutex::new(String::new()),
            base_url,
            model,
            max_output_tokens: profile.max_output_tokens,
            project: project.map(str::trim).filter(|p| !p.is_empty()).map(str::to_owned),
        }
    }

    /// Follow a Claude model's thinking through the stream and hand each block over once signed.
    ///
    /// Thinking arrives as `thought` text in pieces, and its signature closes it: on the last
    /// piece, on an empty part after it, or on the first visible text (whose part then no longer
    /// carries it). A signature on a function call is that call's own and stays on it. Thinking
    /// still unsigned when visible text or a call follows is let go, as is a signed block with no
    /// text: the backend refuses both when they come back, and CLIProxyAPI drops them too.
    ///
    /// The block is the part the backend streamed, whole: `{thought, text, thoughtSignature}`.
    /// The thought pieces themselves still reach the loop, which shows them and stores none.
    fn keep_thinking(&self, part: &mut Part) -> Option<Part> {
        let mut pending = self.thinking.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let signed = part.thought_signature.as_deref().is_some_and(|s| !s.trim().is_empty());
        let text = part.text.as_deref().unwrap_or_default();
        if part.is_thought() {
            pending.push_str(text);
            if !signed {
                return None;
            }
        } else if part.function_call.is_some() {
            pending.clear();
            return None;
        } else if pending.is_empty() || !signed {
            if !text.is_empty() {
                pending.clear();
            }
            return None;
        }
        let signature = if part.is_thought() { part.thought_signature.clone() } else { part.thought_signature.take() }?;
        let text = std::mem::take(&mut *pending);
        if text.trim().is_empty() {
            return None;
        }
        Some(Part {
            provider_block: Some(ProviderBlock {
                provider: ProviderKind::Antigravity.as_str().to_owned(),
                binding: Some(self.binding.clone()),
                block: json!({"thought": true, "text": text, "thoughtSignature": signature}),
            }),
            ..Default::default()
        })
    }

    /// The history as this backend reads it.
    ///
    /// For every model, a call and its result carry the same id: the one the backend issued,
    /// without the loop's `{name}__` decoration (`TurnAccumulator`). A result is paired with its
    /// call by that id; on a Claude model it becomes a `tool_result` naming its `tool_use`, and a
    /// result without one is a 400.
    ///
    /// For a Claude model, as CLIProxyAPI (the reference for this backend) sends it:
    /// * each model turn gets back the thinking it was given ([`Wire::keep_thinking`]), ahead of
    ///   its text and calls. A tool round whose thinking is missing is refused. A block goes back
    ///   only to the account, endpoint and model that signed it ([`binding`]); every other one is
    ///   left out. It is not bound to the request it answered, unlike the Anthropic dialect's:
    ///   this backend checks that a round's thinking is there, and a round whose system prompt
    ///   moved on (an artifact written, a hook) still has to carry it;
    /// * no `skip_thought_signature_validator` placeholder: it is a Gemini convention, not a
    ///   Claude signature. A call keeps the signature the backend gave it; text keeps none;
    /// * tool results travel in a content of their own, ahead of text sent with them (steering).
    ///
    /// Any other model keeps the placeholder on a call it did not sign (one from another model's
    /// turn, see `History::for_request`): it is how such a call is accepted, as on Code Assist.
    fn project_contents(&self, contents: Vec<Value>) -> Vec<Value> {
        let mut out = Vec::with_capacity(contents.len());
        for mut content in contents {
            let Some(object) = content.as_object_mut() else {
                out.push(content);
                continue;
            };
            let model_turn = object.get("role").and_then(Value::as_str) == Some("model");
            let Some(Value::Array(parts)) = object.remove("parts") else {
                out.push(content);
                continue;
            };
            let mut kept = Vec::with_capacity(parts.len());
            for mut part in parts {
                let Some(fields) = part.as_object_mut() else {
                    kept.push(part);
                    continue;
                };
                if let Some(block) = fields.remove("providerBlock") {
                    kept.extend(self.replayed(&block, model_turn));
                    continue;
                }
                for key in ["functionCall", "functionResponse"] {
                    if let Some(Value::Object(call)) = fields.get_mut(key) {
                        undecorate_id(call);
                    }
                }
                if self.claude {
                    let placeholder = fields.get("thoughtSignature").and_then(Value::as_str) == Some(SKIP_THOUGHT_SIGNATURE);
                    if placeholder || !fields.contains_key("functionCall") {
                        fields.remove("thoughtSignature");
                    }
                }
                kept.push(part);
            }
            // A content of nothing but another model's thinking: nothing is left to send.
            if kept.is_empty() {
                continue;
            }
            if self.claude && model_turn {
                // Thinking first, calls last. The sort is stable, so each group keeps the order it
                // streamed in.
                kept.sort_by_key(|p| {
                    if p.get("thought") == Some(&Value::Bool(true)) {
                        0
                    } else if p.get("functionCall").is_some() {
                        2
                    } else {
                        1
                    }
                });
            } else if self.claude {
                let (results, rest): (Vec<Value>, Vec<Value>) =
                    kept.into_iter().partition(|p| p.get("functionResponse").is_some());
                if !results.is_empty() && !rest.is_empty() {
                    let mut first = object.clone();
                    first.insert("parts".to_owned(), Value::Array(results));
                    out.push(Value::Object(first));
                    kept = rest;
                } else {
                    kept = if results.is_empty() { rest } else { results };
                }
            }
            object.insert("parts".to_owned(), Value::Array(kept));
            out.push(content);
        }
        out
    }

    /// A kept thinking block as the part it was streamed as, when this request may carry it: a
    /// model turn, sent by the account, to the endpoint and model that signed it.
    fn replayed(&self, block: &Value, model_turn: bool) -> Option<Value> {
        let ours = block.get("provider").and_then(Value::as_str) == Some(ProviderKind::Antigravity.as_str())
            && block.get("binding").and_then(Value::as_str) == Some(self.binding.as_str());
        if !(ours && model_turn && self.claude) {
            return None;
        }
        let part = block.get("block")?;
        let filled = |key: &str| part.get(key).and_then(Value::as_str).is_some_and(|s| !s.trim().is_empty());
        (part.get("thought") == Some(&Value::Bool(true)) && filled("text") && filled("thoughtSignature")).then(|| part.clone())
    }
}

impl Dialect for Wire {
    fn endpoint(&self) -> String {
        format!("{}/v1internal:streamGenerateContent?alt=sse", self.base_url.trim_end_matches('/'))
    }

    /// The request wrapped in Antigravity's envelope.
    fn build_body(&self, req: &GenerateContentRequest) -> Value {
        let mut request = serde_json::to_value(req).unwrap_or_else(|_| Value::Object(Map::new()));
        let Value::Object(map) = &mut request else {
            return Value::Object(Map::new());
        };
        // Envelope-only fields: the inner request must not carry them.
        map.remove("session_id");
        map.remove("sessionId");
        // Code Assist tolerates safety settings; this backend rejects them.
        map.remove("safetySettings");

        if let Some(Value::Array(contents)) = map.get_mut("contents") {
            let history = std::mem::take(contents);
            *contents = self.project_contents(history);
        }

        // The same family gate as the Gemini dialect: a `thinkingConfig` on a model that does not
        // think is a 400, not an ignored field.
        if let Some(Value::Object(config)) = map.get_mut("generationConfig") {
            if !gemini::accepts_thinking(&self.model) {
                config.remove("thinkingConfig");
            }
            // The profile cap is a fallback. A request that already chose a value keeps it.
            if let Some(cap) = self.max_output_tokens {
                if !config.contains_key("maxOutputTokens") {
                    config.insert("maxOutputTokens".to_owned(), Value::from(cap));
                }
            }
        } else if let Some(cap) = self.max_output_tokens {
            map.insert(
                "generationConfig".to_owned(),
                Value::Object(Map::from_iter([("maxOutputTokens".to_owned(), Value::from(cap))])),
            );
        }

        // A stable session id, so a replayed history is one session to the provider and its caches
        // keep working. Derived from the first user text, exactly as the backend's own client does
        // it: the digest's first eight bytes as a big-endian i64, printed with a leading `-`.
        map.insert("sessionId".to_owned(), Value::String(session_id(req)));

        let mut envelope = Map::new();
        envelope.insert("model".to_owned(), Value::String(self.model.clone()));
        envelope.insert("userAgent".to_owned(), Value::String(ENVELOPE_USER_AGENT.to_owned()));
        envelope.insert("requestType".to_owned(), Value::String(REQUEST_TYPE.to_owned()));
        if let Some(project) = &self.project {
            envelope.insert("project".to_owned(), Value::String(project.clone()));
        }
        // Unique per request: the backend uses it to join logs, and reusing one would join two
        // unrelated turns.
        envelope.insert("requestId".to_owned(), Value::String(format!("agent-{}", uuid::Uuid::new_v4())));
        envelope.insert("request".to_owned(), Value::Object(map.clone()));
        Value::Object(envelope)
    }

    fn apply_headers(&self, rb: reqwest::RequestBuilder, auth: WireAuth<'_>) -> reqwest::RequestBuilder {
        // This backend has no key form: the credential is always the account's bearer token. A key
        // can only reach here through a hand-built backend, and sending it as a bearer is the
        // least surprising thing to do with a "credential-shaped string".
        let token = match auth {
            WireAuth::Bearer(token) => token,
            WireAuth::Key(key) => key,
        };
        rb.bearer_auth(token)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .header(reqwest::header::USER_AGENT, ANTIGRAVITY_UA)
    }

    fn decode(&self, state: &mut Decoder, payload: &str) -> Result<Vec<CaGenerateResponse>, CaError> {
        let trimmed = payload.trim();
        if trimmed.is_empty() || trimmed == "[DONE]" {
            return Ok(Vec::new());
        }
        if trimmed.len() > MAX_PAYLOAD_BYTES {
            return Err(CaError::Decode("Antigravity event exceeded the size limit".into()));
        }

        let mut object = match serde_json::from_str::<Value>(trimmed) {
            Ok(Value::Object(map)) => map,
            Ok(_) => return Err(CaError::Decode("Antigravity event is not a JSON object".into())),
            Err(e) => {
                // Never echo the payload: it may be a fragment of the user's own prompt.
                return Err(CaError::Decode(format!(
                    "invalid Antigravity event ({} bytes): {e}",
                    trimmed.len()
                )))
            }
        };

        // The two envelope shapes. `response` is an object on a Claude-family reply; a Gemini-family
        // reply is the response itself. Both then follow one path.
        if let Some(Value::Object(inner)) = object.get("response").cloned() {
            object = inner;
        }
        if !self.claude {
            // Gemini needs none of its thinking back; its signatures ride on the parts it signed.
            return gemini::decode_gemini_value(state, object, &self.seed);
        }
        gemini::decode_gemini_value_with(state, object, &self.seed, &mut |part| self.keep_thinking(part))
    }

    fn finish(&self, state: &mut Decoder) -> Result<Vec<CaGenerateResponse>, CaError> {
        gemini::finish_gemini(state, &self.seed)
    }
}

/// `User-Agent` sent with every request.
///
/// Deliberately names us: no `antigravity/hub/…`, no `X-Goog-Api-Client`, no Node client string.
/// The provider can therefore tell a third-party client apart, which is the honest arrangement and
/// the one the Account tab warns about.
const ANTIGRAVITY_UA: &str = concat!("anticraft-native/", env!("CARGO_PKG_VERSION"), " (antigravity-oauth)");

/// Stable session id for the request: `-<i64>`, from the first user turn's first text.
///
/// The provider uses it to keep one conversation's turns together, so it must be **the same** for a
/// replayed history and different for a different conversation. A digest of the first user text is
/// both. Without any user text (an empty or image-only history) a random i63 is used, which is what
/// the backend's own client falls back to; it is only ever read for logging and cache grouping.
fn session_id(req: &GenerateContentRequest) -> String {
    let seed = req
        .contents
        .iter()
        .filter(|c| c.role != Some(crate::api::Role::Model))
        .flat_map(|c| c.parts.iter())
        .find_map(|p| p.text.as_deref().filter(|t| !t.is_empty()));
    let value = match seed {
        Some(text) => {
            let digest = Sha256::digest(text.as_bytes());
            let mut first = [0u8; 8];
            first.copy_from_slice(&digest[..8]);
            i64::from_be_bytes(first)
        }
        None => (rand::random::<u64>() & i64::MAX as u64) as i64,
    };
    format!("-{value}")
}

/// Antigravity's Claude models (`claude-opus-5-5-high`, `claude-sonnet-4-6`, …), told apart from
/// its Gemini ones by name, as CLIProxyAPI does for this backend.
fn is_claude(model: &str) -> bool {
    model.to_ascii_lowercase().contains("claude")
}

/// What a kept thinking block is bound to: the account (its profile id, `acct:<account id>`), the
/// endpoint and the model. A signature is only good where it was issued, and two accounts never
/// get each other's thinking. Each field is length-prefixed so two never run together; nothing
/// in it is secret.
fn binding(profile_id: &str, base_url: &str, model: &str) -> String {
    let mut digest = Sha256::new();
    for field in [profile_id, base_url, model] {
        digest.update((field.len() as u64).to_le_bytes());
        digest.update(field.as_bytes());
    }
    digest.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Strip the loop's `{name}__` decoration from a `functionCall` or `functionResponse` id, so the
/// backend sees the id it issued. An id without it (a history from another client) is kept.
fn undecorate_id(call: &mut Map<String, Value>) {
    let prefix = format!("{}__", call.get("name").and_then(Value::as_str).unwrap_or_default());
    if let Some(Value::String(id)) = call.get_mut("id") {
        if let Some(issued) = id.strip_prefix(&prefix).filter(|issued| !issued.is_empty()) {
            *id = issued.to_owned();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{
        Content, FunctionCall, FunctionDeclaration, FunctionResponse, GenerationConfig, Part, Role, Tool,
    };
    use crate::providers::ProviderKind;
    use serde_json::json;

    fn profile(model: &str) -> ProviderProfile {
        ProviderProfile {
            id: "acct:antigravity-0123abcd".into(),
            label: "Antigravity".into(),
            kind: ProviderKind::Antigravity,
            model: model.into(),
            base_url: None,
            api_key: String::new(),
            max_output_tokens: None,
            context_window: None,
            models: Vec::new(),
        }
    }

    fn wire(model: &str) -> Wire {
        Wire::new(&profile(model), Some("projects/123456"))
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
            generation_config: Some(GenerationConfig::chat_defaults("gemini-3-flash")),
            session_id: Some("session-1".into()),
            safety_settings: Some(json!([{"category": "HARM_CATEGORY_X", "threshold": "BLOCK_NONE"}])),
            ..Default::default()
        }
    }

    #[test]
    fn endpoint_targets_the_internal_stream_route() {
        assert_eq!(
            wire("gemini-3-flash").endpoint(),
            "https://daily-cloudcode-pa.googleapis.com/v1internal:streamGenerateContent?alt=sse"
        );
        let custom = Wire::new(
            &ProviderProfile { base_url: Some("http://localhost:8080/".into()), ..profile("gemini-3-flash") },
            None,
        );
        assert_eq!(custom.endpoint(), "http://localhost:8080/v1internal:streamGenerateContent?alt=sse");
    }

    #[test]
    fn body_wraps_the_request_in_the_envelope() {
        let body = wire("gemini-3-flash").build_body(&full_request());

        // The envelope.
        assert_eq!(body["model"], json!("gemini-3-flash"));
        assert_eq!(body["userAgent"], json!(ENVELOPE_USER_AGENT));
        assert_eq!(body["requestType"], json!("agent"));
        assert_eq!(body["project"], json!("projects/123456"));
        assert!(body["requestId"].as_str().is_some_and(|id| id.starts_with("agent-")), "{body}");

        // The inner request.
        let request = &body["request"];
        assert!(request.get("session_id").is_none(), "the snake_case Code Assist spelling is not sent");
        assert!(request["sessionId"].as_str().is_some_and(|id| id.starts_with('-')), "{request}");
        assert!(request.get("safetySettings").is_none(), "this backend rejects safety settings");
        assert_eq!(request["systemInstruction"]["parts"][0]["text"], json!("be terse"));
        assert_eq!(request["contents"][1]["parts"][0]["functionCall"]["name"], json!("read_file"));
        assert_eq!(request["contents"][2]["parts"][0]["functionResponse"]["response"]["output"], json!("fn main() {}"));
        assert_eq!(request["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"]["required"], json!(["file_path"]));
        assert_eq!(request["generationConfig"]["thinkingConfig"]["thinkingLevel"], json!("HIGH"));

        // No envelope field leaked into the inner request and no inner field into the envelope.
        for key in ["model", "userAgent", "requestType", "requestId"] {
            assert!(request.get(key).is_none(), "{key} belongs to the envelope: {request}");
        }
    }

    #[test]
    fn the_session_id_is_stable_for_a_history_and_differs_between_them() {
        let w = wire("gemini-3-flash");
        let first = w.build_body(&full_request());
        let second = w.build_body(&full_request());
        let id = first["request"]["sessionId"].as_str().expect("session id").to_owned();
        assert!(id.starts_with('-'), "{id}");
        assert!(id[1..].parse::<i64>().is_ok(), "{id}");
        assert_eq!(second["request"]["sessionId"], json!(id), "a replayed history is one session");

        // A different conversation is a different session.
        let other = w.build_body(&GenerateContentRequest {
            contents: vec![Content::user_text("something else entirely")],
            ..Default::default()
        });
        assert_ne!(other["request"]["sessionId"], json!(id));

        // A history with no user text still gets one.
        let empty = w.build_body(&GenerateContentRequest::default());
        let empty_id = empty["request"]["sessionId"].as_str().expect("session id");
        assert!(empty_id[1..].parse::<i64>().is_ok(), "{empty_id}");
    }

    #[test]
    fn the_project_is_sent_only_when_the_account_has_one() {
        let body = Wire::new(&profile("gemini-3-flash"), None).build_body(&full_request());
        assert!(body.get("project").is_none(), "{body}");
        let body = Wire::new(&profile("gemini-3-flash"), Some("   ")).build_body(&full_request());
        assert!(body.get("project").is_none(), "a blank project is the same as none");

        let body = wire("gemini-3-flash").build_body(&full_request());
        assert_eq!(body["project"], json!("projects/123456"));
    }

    #[test]
    fn antigravity_drops_thinking_config_for_models_that_do_not_think() {
        let kept = wire("gemini-3-flash").build_body(&full_request());
        assert!(kept["request"]["generationConfig"]["thinkingConfig"].is_object());

        let req25 = GenerateContentRequest {
            generation_config: Some(GenerationConfig::chat_defaults("gemini-2.5-flash")),
            ..full_request()
        };
        let kept = wire("gemini-2.5-flash").build_body(&req25);
        assert_eq!(kept["request"]["generationConfig"]["thinkingConfig"]["thinkingBudget"], json!(8192));

        // And dropped for a family that has no thinking at all.
        let dropped = wire("gpt-oss-120b-medium").build_body(&full_request());
        assert!(dropped["request"]["generationConfig"].get("thinkingConfig").is_none(), "{dropped}");
        // The rest of the generation config survives.
        assert_eq!(dropped["request"]["generationConfig"]["temperature"], json!(1.0));
    }

    #[test]
    fn profile_token_cap_is_only_a_fallback() {
        let capped = Wire::new(
            &ProviderProfile { max_output_tokens: Some(4096), ..profile("gemini-3-flash") },
            None,
        );
        let body = capped.build_body(&GenerateContentRequest::default());
        assert_eq!(body["request"]["generationConfig"]["maxOutputTokens"], json!(4096));

        let mut req = full_request();
        if let Some(config) = req.generation_config.as_mut() {
            config.max_output_tokens = Some(17);
        }
        assert_eq!(capped.build_body(&req)["request"]["generationConfig"]["maxOutputTokens"], json!(17));
    }

    #[test]
    fn antigravity_body_wraps_the_request_and_accepts_both_envelope_shapes() {
        // A Gemini-family reply is the response itself; a Claude-family reply arrives wrapped.
        let bare = r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"Hel"},{"text":"lo"}]},"finishReason":"STOP"}]}"#;
        let wrapped = format!(r#"{{"response":{bare},"modelVersion":"x"}}"#);

        let w = wire("gemini-3-flash");
        let mut a = Decoder::new();
        let mut b = Decoder::new();
        let from_bare = w.decode(&mut a, bare).expect("bare envelope decodes");
        let from_wrapped = w.decode(&mut b, &wrapped).expect("wrapped envelope decodes");

        assert_eq!(from_bare.len(), 1);
        assert_eq!(from_wrapped.len(), 1);
        assert_eq!(from_bare[0], from_wrapped[0], "the two shapes must be indistinguishable downstream");
        assert_eq!(from_bare[0].first_parts().len(), 2);
        assert_eq!(from_bare[0].first_parts()[0].text.as_deref(), Some("Hel"));
        assert_eq!(
            from_bare[0].response.as_ref().and_then(|r| r.candidates[0].finish_reason.as_deref()),
            Some("STOP")
        );
    }

    #[test]
    fn a_tool_call_and_usage_survive_both_envelopes() {
        let w = wire("claude-sonnet-4-6");
        for payload in [
            r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"read_file","args":{"file_path":"a.rs"}}}]}}],"usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":2,"totalTokenCount":7}}"#.to_owned(),
            r#"{"response":{"candidates":[{"content":{"parts":[{"functionCall":{"name":"read_file","args":{"file_path":"a.rs"}}}]}}],"usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":2,"totalTokenCount":7}}}"#.to_owned(),
        ] {
            let mut state = Decoder::new();
            let chunks = w.decode(&mut state, &payload).expect("decodes");
            let call = chunks[0].first_parts()[0].function_call.clone().expect("call");
            assert_eq!(call.name, "read_file");
            assert_eq!(call.args["file_path"], json!("a.rs"));
            assert!(call.id.is_some(), "an id is synthesized for the loop");
            let usage = chunks[0].response.as_ref().and_then(|r| r.usage_metadata).expect("usage");
            assert_eq!(usage.total_token_count, 7);
        }
    }

    #[test]
    fn a_thought_part_streams_as_a_thought() {
        let w = wire("gemini-3-flash");
        let mut state = Decoder::new();
        let chunks = w
            .decode(
                &mut state,
                r#"{"response":{"candidates":[{"content":{"parts":[{"text":"plan","thought":true}]}}]}}"#,
            )
            .expect("decodes");
        assert!(chunks[0].first_parts()[0].is_thought());
    }

    #[test]
    fn in_stream_errors_keep_the_retry_vocabulary() {
        let w = wire("gemini-3-flash");
        let mut state = Decoder::new();
        let err = w.decode(&mut state, r#"{"error":{"code":429,"message":"slow down"}}"#).unwrap_err();
        assert!(matches!(err, CaError::Retryable { .. }) && err.is_retryable());

        let err = w.decode(&mut state, r#"{"error":{"code":403,"message":"bad token"}}"#).unwrap_err();
        assert!(matches!(err, CaError::Auth(ref m) if m == "bad token"));

        // The daily quota of a subscription must not be retried for minutes.
        let err = w
            .decode(
                &mut state,
                r#"{"error":{"code":429,"message":"Quota exceeded","status":"RESOURCE_EXHAUSTED","details":[{"@type":"type.googleapis.com/google.rpc.QuotaFailure","violations":[{"quotaId":"GenerateRequestsPerDayPerProjectPerModel-FreeTier"}]}]}}"#,
            )
            .unwrap_err();
        assert!(matches!(err, CaError::TerminalQuota { .. }), "{err:?}");
    }

    #[test]
    fn unknown_and_malformed_events_do_not_panic() {
        let w = wire("gemini-3-flash");
        let mut state = Decoder::new();
        for payload in ["", "   ", "[DONE]", "{}", r#"{"foo":1}"#, r#"{"candidates":[]}"#, r#"{"response":{}}"#] {
            let chunks = w.decode(&mut state, payload).expect("tolerated");
            assert!(chunks.is_empty(), "{payload} produced {chunks:?}");
        }
        for payload in ["not json", "[1,2,3]", r#"{"candidates": 5}"#] {
            assert!(w.decode(&mut state, payload).is_err(), "{payload}");
        }
    }

    #[test]
    fn finish_flushes_a_call_split_across_events() {
        let w = wire("gemini-3-flash");
        let mut state = Decoder::new();
        let held = w
            .decode(
                &mut state,
                r#"{"response":{"candidates":[{"content":{"parts":[{"functionCall":{"name":"glob","args":"{\"patt"}}]}}]}}"#,
            )
            .expect("decodes");
        assert!(held.is_empty(), "an incomplete call is held back");
        let flushed = w.finish(&mut state).expect("flushes");
        assert_eq!(flushed.len(), 1);
        let call = flushed[0].first_parts()[0].function_call.clone().expect("call");
        assert_eq!(call.name, "glob");
        assert_eq!(call.args, json!({}), "unparseable arguments degrade to an empty object");
        assert!(w.finish(&mut state).expect("second finish").is_empty());
    }

    #[test]
    fn headers_carry_the_bearer_and_our_own_user_agent() {
        let w = wire("gemini-3-flash");
        let built = w
            .apply_headers(reqwest::Client::new().post("http://example.test"), WireAuth::Bearer("ya29.token"))
            .build()
            .expect("request builds");
        assert_eq!(built.headers().get("authorization").and_then(|v| v.to_str().ok()), Some("Bearer ya29.token"));
        assert_eq!(built.headers().get("content-type").and_then(|v| v.to_str().ok()), Some("application/json"));
        assert_eq!(
            built.headers().get("accept").and_then(|v| v.to_str().ok()),
            Some("text/event-stream")
        );
        let ua = built.headers().get("user-agent").and_then(|v| v.to_str().ok()).unwrap_or_default();
        assert!(ua.starts_with("anticraft-native/"), "{ua}");
        assert!(ua.contains("antigravity-oauth"), "{ua}");
        // Nothing that imitates the first-party client.
        assert!(!ua.contains("antigravity/hub"), "{ua}");
        assert!(built.headers().get("x-goog-api-client").is_none());
    }

    /// Every part a stream of `events` hands the loop, in order, the end of the stream included.
    fn streamed(w: &Wire, events: &[&str]) -> Vec<Part> {
        let mut state = Decoder::new();
        let mut parts = Vec::new();
        for event in events {
            for chunk in w.decode(&mut state, event).expect("decodes") {
                parts.extend(chunk.first_parts().iter().cloned());
            }
        }
        for chunk in w.finish(&mut state).expect("finishes") {
            parts.extend(chunk.first_parts().iter().cloned());
        }
        parts
    }

    /// The request after the tool round `parts` streamed: the history as the loop keeps it (the
    /// model turn through `TurnAccumulator`, a result for every call), as `History::for_request`
    /// hands it to the backend.
    fn after_tools(parts: &[Part]) -> GenerateContentRequest {
        let mut turn = crate::agent::history::TurnAccumulator::new();
        turn.push_parts(parts);
        let results = turn
            .function_calls()
            .into_iter()
            .map(|c| Part::function_response(FunctionResponse { id: c.id, name: c.name, response: json!({"output": "ok"}) }))
            .collect();
        let history = crate::agent::History::from_contents(vec![
            Content::user_text("read both"),
            turn.into_content(),
            Content { role: Some(Role::User), parts: results },
        ]);
        GenerateContentRequest { contents: history.for_request(), ..Default::default() }
    }

    /// A Claude model's tool round as the backend streams it: thinking in pieces, signed by an
    /// empty part, then the calls with the ids Vertex issued.
    const CLAUDE_ROUND: &[&str] = &[
        r#"{"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"Read ","thought":true}]}}]}}"#,
        r#"{"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"both.","thought":true}]}}]}}"#,
        r#"{"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"","thought":true,"thoughtSignature":"sig-1"}]}}]}}"#,
        r#"{"response":{"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"read_file","args":{"file_path":"a.rs"},"id":"toolu_vrtx_01"}},{"functionCall":{"name":"read_file","args":{"file_path":"b.rs"},"id":"toolu_vrtx_02"}}]},"finishReason":"STOP"}]}}"#,
    ];

    /// The turn that ended as soon as its tools ran: the request carrying the results has to give
    /// the model its signed thinking back, ahead of the calls, and pair every result with its call
    /// by the id the backend issued. Without either, the backend answers 400.
    #[test]
    fn a_claude_tool_round_goes_back_with_its_thinking_and_paired_ids() {
        let w = wire("claude-opus-5-5-high");
        let parts = streamed(&w, CLAUDE_ROUND);
        // The pieces still stream as thoughts; the kept block has no text the transcript shows.
        assert_eq!(parts.iter().filter(|p| p.is_thought()).count(), 2, "{parts:?}");
        let kept: Vec<&ProviderBlock> = parts.iter().filter_map(|p| p.provider_block.as_ref()).collect();
        assert_eq!(kept.len(), 1, "{parts:?}");
        let thinking = json!({"thought": true, "text": "Read both.", "thoughtSignature": "sig-1"});
        assert_eq!(kept[0].block, thinking);
        assert_eq!(kept[0].provider, "antigravity");

        let body = w.build_body(&after_tools(&parts));
        let contents = &body["request"]["contents"];
        assert_eq!(contents[1]["parts"][0], thinking, "{body}");
        assert_eq!(contents[1]["parts"][1]["functionCall"]["id"], json!("toolu_vrtx_01"));
        assert_eq!(contents[1]["parts"][2]["functionCall"]["id"], json!("toolu_vrtx_02"));
        assert_eq!(contents[2]["parts"][0]["functionResponse"]["id"], json!("toolu_vrtx_01"));
        assert_eq!(contents[2]["parts"][1]["functionResponse"]["id"], json!("toolu_vrtx_02"));
        // The Gemini placeholder is no Claude signature, and the `{name}__` decoration is the
        // loop's own.
        let sent = body.to_string();
        assert!(!sent.contains(SKIP_THOUGHT_SIGNATURE), "{body}");
        assert!(!sent.contains("providerBlock") && !sent.contains("read_file__"), "{body}");
    }

    /// A signature is only good where it was issued: another account, endpoint or model gets the
    /// round without the block. A Gemini model needs none of it, and keeps the placeholder on the
    /// call it did not sign.
    #[test]
    fn kept_thinking_goes_back_only_where_it_was_signed() {
        let request = after_tools(&streamed(&wire("claude-opus-5-5-high"), CLAUDE_ROUND));
        let elsewhere = [
            Wire::new(&ProviderProfile { id: "acct:antigravity-ffff0000".into(), ..profile("claude-opus-5-5-high") }, Some("projects/123456")),
            Wire::new(&ProviderProfile { base_url: Some("http://localhost:8080".into()), ..profile("claude-opus-5-5-high") }, Some("projects/123456")),
            wire("claude-sonnet-4-6"),
        ];
        for w in &elsewhere {
            let body = w.build_body(&request);
            assert!(!body.to_string().contains("sig-1"), "{body}");
            assert_eq!(body["request"]["contents"][1]["parts"][0]["functionCall"]["id"], json!("toolu_vrtx_01"), "{body}");
        }

        let body = wire("gemini-3-flash").build_body(&request);
        assert!(!body.to_string().contains("sig-1"), "{body}");
        let contents = &body["request"]["contents"];
        assert_eq!(contents[1]["parts"][0]["thoughtSignature"], json!(SKIP_THOUGHT_SIGNATURE), "{body}");
        assert!(contents[1]["parts"][1].get("thoughtSignature").is_none(), "{body}");
        assert_eq!(contents[1]["parts"][0]["functionCall"]["id"], json!("toolu_vrtx_01"));
        assert_eq!(contents[2]["parts"][1]["functionResponse"]["id"], json!("toolu_vrtx_02"));
    }

    /// Thinking is kept only when signed and not blank: the backend refuses either kind back. A
    /// call keeps the signature it came with, and text sends none.
    #[test]
    fn only_signed_thinking_is_kept_and_a_call_keeps_its_own_signature() {
        let w = wire("claude-opus-5-5-high");
        let parts = streamed(
            &w,
            &[
                // Unsigned: let go when the text after it arrives.
                r#"{"response":{"candidates":[{"content":{"parts":[{"text":"unsigned plan","thought":true}]}}]}}"#,
                r#"{"response":{"candidates":[{"content":{"parts":[{"text":"Looking."}]}}]}}"#,
                // Signed, but blank.
                r#"{"response":{"candidates":[{"content":{"parts":[{"text":" ","thought":true,"thoughtSignature":"sig-blank"}]}}]}}"#,
                r#"{"response":{"candidates":[{"content":{"parts":[{"functionCall":{"name":"read_file","args":{"file_path":"a.rs"},"id":"toolu_1"},"thoughtSignature":"sig-call"}]}}]}}"#,
            ],
        );
        assert!(parts.iter().all(|p| p.provider_block.is_none()), "{parts:?}");
        let call = parts.iter().find(|p| p.function_call.is_some()).expect("the call");
        assert_eq!(call.thought_signature.as_deref(), Some("sig-call"));

        let body = w.build_body(&after_tools(&parts));
        let model = &body["request"]["contents"][1]["parts"];
        assert_eq!(model[0], json!({"text": "Looking."}), "{body}");
        assert_eq!(model[1]["thoughtSignature"], json!("sig-call"), "{body}");
        assert!(!body.to_string().contains("sig-blank"), "{body}");
    }

    /// A signature on the first visible text closes the thinking before it and goes with that
    /// thinking: text sends no signature back.
    #[test]
    fn a_signature_on_the_first_text_closes_the_thinking() {
        let parts = streamed(
            &wire("claude-opus-5-5-high"),
            &[
                r#"{"response":{"candidates":[{"content":{"parts":[{"text":"Think.","thought":true}]}}]}}"#,
                r#"{"response":{"candidates":[{"content":{"parts":[{"text":"Answer","thoughtSignature":"sig-t"}]}}]}}"#,
            ],
        );
        let kept: Vec<Value> = parts.iter().filter_map(|p| p.provider_block.as_ref()).map(|b| b.block.clone()).collect();
        assert_eq!(kept, vec![json!({"thought": true, "text": "Think.", "thoughtSignature": "sig-t"})]);
        let text = parts.iter().find(|p| p.text.as_deref() == Some("Answer")).expect("the text");
        assert!(text.thought_signature.is_none(), "{parts:?}");
    }

    /// A Claude model turn goes back thinking first and calls last, and a round's results travel
    /// ahead of the text sent with them (steering), in a content of their own. Another model gets
    /// both as they were.
    #[test]
    fn claude_rounds_are_ordered_and_split_as_the_backend_reads_them() {
        let w = wire("claude-opus-5-5-high");
        let block = Part {
            provider_block: Some(ProviderBlock {
                provider: "antigravity".into(),
                binding: Some(w.binding.clone()),
                block: json!({"thought": true, "text": "plan", "thoughtSignature": "sig-p"}),
            }),
            ..Default::default()
        };
        let call = Part {
            function_call: Some(FunctionCall { id: Some("read_file__toolu_1".into()), name: "read_file".into(), args: json!({}) }),
            ..Default::default()
        };
        let result = FunctionResponse { id: Some("read_file__toolu_1".into()), name: "read_file".into(), response: json!({"output": "x"}) };
        let request = GenerateContentRequest {
            contents: vec![
                Content::user_text("go"),
                Content::model(vec![Part::text("Let me look."), block, call]),
                Content { role: Some(Role::User), parts: vec![Part::function_response(result), Part::text("also check b.rs")] },
            ],
            ..Default::default()
        };

        let body = w.build_body(&request);
        let contents = &body["request"]["contents"];
        assert_eq!(contents.as_array().map(Vec::len), Some(4), "{body}");
        assert_eq!(contents[1]["parts"][0]["thoughtSignature"], json!("sig-p"), "{body}");
        assert_eq!(contents[1]["parts"][1]["text"], json!("Let me look."));
        assert_eq!(contents[1]["parts"][2]["functionCall"]["id"], json!("toolu_1"));
        assert_eq!(
            contents[2],
            json!({"role": "user", "parts": [{"functionResponse": {"id": "toolu_1", "name": "read_file", "response": {"output": "x"}}}]})
        );
        assert_eq!(contents[3], json!({"role": "user", "parts": [{"text": "also check b.rs"}]}));

        let body = wire("gemini-3-flash").build_body(&request);
        let contents = &body["request"]["contents"];
        assert_eq!(contents.as_array().map(Vec::len), Some(3), "{body}");
        assert_eq!(contents[1]["parts"][0]["text"], json!("Let me look."), "{body}");
        assert_eq!(contents[1]["parts"][1]["functionCall"]["id"], json!("toolu_1"));
        assert_eq!(contents[2]["parts"][0]["functionResponse"]["id"], json!("toolu_1"));
        assert_eq!(contents[2]["parts"][1]["text"], json!("also check b.rs"));
    }

    /// Only the loop's decoration comes off an id. One without it (a history from another
    /// client), or with nothing after it, is sent as it is.
    #[test]
    fn only_the_loops_decoration_comes_off_an_id() {
        for (id, sent) in [("read_file__toolu_1", "toolu_1"), ("toolu_2", "toolu_2"), ("read_file__", "read_file__"), ("glob__x", "glob__x")] {
            let mut call = Map::from_iter([("name".to_owned(), json!("read_file")), ("id".to_owned(), json!(id))]);
            undecorate_id(&mut call);
            assert_eq!(call["id"], json!(sent), "{id}");
        }
    }
}
