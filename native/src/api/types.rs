//! Serde types for the Code Assist `v1internal` API (PROTOCOL.md 2.3-2.10).
//!
//! Casing is mixed on the wire: the envelope fields `user_prompt_id`, `enabled_credit_types`
//! and the inner `session_id` are snake_case, everything else is camelCase.

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Content / Part
// ---------------------------------------------------------------------------

/// Conversation role. Tool responses use [`Role::User`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Model,
}

/// One message in the history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Content {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<Role>,
    #[serde(default)]
    pub parts: Vec<Part>,
}

impl Content {
    pub fn user_text(text: impl Into<String>) -> Self {
        Self { role: Some(Role::User), parts: vec![Part::text(text)] }
    }
    pub fn model(parts: Vec<Part>) -> Self {
        Self { role: Some(Role::Model), parts }
    }
    /// Concatenated non-thought text of all parts.
    pub fn text(&self) -> String {
        self.parts
            .iter()
            .filter(|p| !p.is_thought())
            .filter_map(|p| p.text.as_deref())
            .collect()
    }
    /// All function calls in this content, in order.
    pub fn function_calls(&self) -> impl Iterator<Item = &FunctionCall> {
        self.parts.iter().filter_map(|p| p.function_call.as_ref())
    }
}

/// A content part. Modeled as a flat struct of optional fields so unknown combinations
/// (e.g. `functionCall` + `thoughtSignature`) round-trip unchanged.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Part {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// `true` marks a thinking part. Never stored in history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thought: Option<bool>,
    /// Opaque signature; kept, never shown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thought_signature: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub function_call: Option<FunctionCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub function_response: Option<FunctionResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inline_data: Option<Blob>,
    /// A reasoning block one external provider issued and needs back verbatim. Carries no text,
    /// so the transcript, summaries and token estimates never see it; see [`ProviderBlock`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_block: Option<ProviderBlock>,
}

/// A reasoning block kept exactly as an external provider streamed it, with its provenance.
///
/// Anthropic signs every `thinking` block, and when a tool result is sent the assistant turn that
/// called the tool must carry its blocks back complete, unmodified and in order, or the request is
/// rejected. The block cannot live in `text`, `thought` or `thoughtSignature`: those are the
/// Gemini fields Code Assist and the Gemini API read, each vendor rejects the other's signatures,
/// and stored text is shown in the transcript. So it rides in its own field, opaque to everything
/// but the dialect named by `provider`, and [`retain_provider_blocks`] removes it before a
/// request goes to any other backend. The field is optional and omitted when empty:
/// conversations saved before it existed still load, and a history without such blocks
/// serializes exactly as before.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderBlock {
    /// Wire format that issued the block, spelled as `ProviderKind::as_str` (`"anthropic"`). A
    /// plain string, so a conversation saved by a build that knows more providers still loads.
    pub provider: String,
    /// Fingerprint of what the issuing provider binds the block to, set by its dialect. A block is
    /// sent back only while the request still matches it; see the Anthropic dialect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding: Option<String>,
    /// The block as the provider sent it, e.g. `{"type":"thinking","thinking":..,"signature":..}`.
    pub block: Value,
}

/// Keep only the [`Part::provider_block`]s that `provider` issued (`None`: remove them all), so a
/// backend never receives another backend's reasoning or signatures.
///
/// A content left without parts is removed too, since an empty `parts` list is a request error,
/// but only when it held such a block: every other content passes through unchanged.
pub fn retain_provider_blocks(contents: &mut Vec<Content>, provider: Option<&str>) {
    contents.retain_mut(|c| {
        let before = c.parts.len();
        c.parts.retain(|p| p.provider_block.as_ref().is_none_or(|b| Some(b.provider.as_str()) == provider));
        c.parts.len() == before || !c.parts.is_empty()
    });
}

impl Part {
    pub fn text(t: impl Into<String>) -> Self {
        Self { text: Some(t.into()), ..Default::default() }
    }
    pub fn function_response(r: FunctionResponse) -> Self {
        Self { function_response: Some(r), ..Default::default() }
    }
    pub fn is_thought(&self) -> bool {
        self.thought == Some(true)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Blob {
    pub mime_type: String,
    /// Base64 data.
    pub data: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionCall {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub args: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub name: String,
    /// `{"output": ...}` on success or `{"error": "..."}` on failure.
    pub response: Value,
}

// ---------------------------------------------------------------------------
// Tools / generation config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tool {
    pub function_declarations: Vec<FunctionDeclaration>,
}

/// `{name, description, parametersJsonSchema}` (PROTOCOL 3.3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FunctionDeclaration {
    pub name: String,
    pub description: String,
    pub parameters_json_schema: Value,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_thoughts: Option<bool>,
    /// Gemini 2.5: token budget (8192 for chat).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_budget: Option<i32>,
    /// Gemini 3.x: `"HIGH"` etc.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerationConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_sequences: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_mime_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_config: Option<ThinkingConfig>,
}

impl GenerationConfig {
    /// Chat defaults for `model` (PROTOCOL 3.6).
    pub fn chat_defaults(model: &str) -> Self {
        let thinking = if model.starts_with("gemini-3") {
            ThinkingConfig { include_thoughts: Some(true), thinking_level: Some("HIGH".into()), ..Default::default() }
        } else {
            ThinkingConfig { include_thoughts: Some(true), thinking_budget: Some(8192), ..Default::default() }
        };
        Self {
            temperature: Some(1.0),
            top_p: Some(0.95),
            top_k: Some(64),
            thinking_config: Some(thinking),
            ..Default::default()
        }
    }
}

// ---------------------------------------------------------------------------
// Generate envelope
// ---------------------------------------------------------------------------

/// Inner `request` object.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerateContentRequest {
    pub contents: Vec<Content>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_instruction: Option<Content>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Tool>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_config: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub safety_settings: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation_config: Option<GenerationConfig>,
    #[serde(rename = "session_id", default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

/// Outer Code Assist envelope for `generateContent` / `streamGenerateContent` (PROTOCOL 2.8).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaGenerateRequest {
    /// Bare model ID, no `models/` prefix.
    pub model: String,
    pub project: String,
    #[serde(rename = "user_prompt_id")]
    pub user_prompt_id: String,
    pub request: GenerateContentRequest,
    #[serde(rename = "enabled_credit_types", default, skip_serializing_if = "Option::is_none")]
    pub enabled_credit_types: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Candidate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<Content>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<u32>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageMetadata {
    #[serde(default)]
    pub prompt_token_count: u64,
    #[serde(default)]
    pub candidates_token_count: u64,
    #[serde(default)]
    pub tool_use_prompt_token_count: u64,
    #[serde(default)]
    pub thoughts_token_count: u64,
    #[serde(default)]
    pub cached_content_token_count: u64,
    #[serde(default)]
    pub total_token_count: u64,
}

impl UsageMetadata {
    /// `max(prompt + candidates + toolUsePrompt + thoughts, total)` (PROTOCOL 2.8).
    pub fn effective_total(&self) -> u64 {
        (self.prompt_token_count
            + self.candidates_token_count
            + self.tool_use_prompt_token_count
            + self.thoughts_token_count)
            .max(self.total_token_count)
    }
}

/// Inner `response` object.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerateContentResponse {
    #[serde(default)]
    pub candidates: Vec<Candidate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_metadata: Option<UsageMetadata>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_feedback: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Credit {
    pub credit_type: String,
    /// int64 sent as a JSON string.
    pub credit_amount: String,
}

/// Outer response (unary, and each SSE chunk).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaGenerateResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<GenerateContentResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consumed_credits: Option<Vec<Credit>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining_credits: Option<Vec<Credit>>,
}

impl CaGenerateResponse {
    /// Unwrapped inner response; missing `response` means no candidates.
    pub fn inner(&self) -> GenerateContentResponse {
        self.response.clone().unwrap_or_default()
    }
    /// Parts of the first candidate.
    pub fn first_parts(&self) -> &[Part] {
        self.response
            .as_ref()
            .and_then(|r| r.candidates.first())
            .and_then(|c| c.content.as_ref())
            .map(|c| c.parts.as_slice())
            .unwrap_or(&[])
    }
}

// ---------------------------------------------------------------------------
// Setup: loadCodeAssist / onboardUser
// ---------------------------------------------------------------------------

/// `ClientMetadata` (PROTOCOL 2.3). Enum-like fields are open strings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientMetadata {
    pub ide_type: String,
    pub platform: String,
    pub plugin_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duet_project: Option<String>,
}

impl ClientMetadata {
    /// Minimal form used by setup calls.
    pub fn setup(project: Option<&str>) -> Self {
        Self {
            ide_type: "IDE_UNSPECIFIED".into(),
            platform: "PLATFORM_UNSPECIFIED".into(),
            plugin_type: "GEMINI".into(),
            duet_project: project.map(str::to_owned),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadCodeAssistRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloudaicompanion_project: Option<String>,
    pub metadata: ClientMetadata,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrivacyNotice {
    #[serde(default)]
    pub show_notice: Option<bool>,
    #[serde(default)]
    pub notice_text: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeminiUserTier {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub is_default: Option<bool>,
    #[serde(default)]
    pub user_defined_cloudaicompanion_project: Option<bool>,
    #[serde(default)]
    pub has_onboarded_previously: Option<bool>,
    #[serde(default)]
    pub has_accepted_tos: Option<bool>,
    #[serde(default)]
    pub privacy_notice: Option<PrivacyNotice>,
    #[serde(default)]
    pub available_credits: Option<Vec<Credit>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IneligibleTier {
    #[serde(default)]
    pub reason_code: Option<String>,
    #[serde(default)]
    pub reason_message: Option<String>,
    #[serde(default)]
    pub tier_id: Option<String>,
    #[serde(default)]
    pub tier_name: Option<String>,
    #[serde(default)]
    pub validation_url: Option<String>,
    #[serde(default)]
    pub validation_url_link_text: Option<String>,
    #[serde(default)]
    pub validation_error_message: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadCodeAssistResponse {
    #[serde(default)]
    pub current_tier: Option<GeminiUserTier>,
    #[serde(default)]
    pub allowed_tiers: Option<Vec<GeminiUserTier>>,
    #[serde(default)]
    pub ineligible_tiers: Option<Vec<IneligibleTier>>,
    #[serde(default)]
    pub cloudaicompanion_project: Option<String>,
    #[serde(default)]
    pub paid_tier: Option<GeminiUserTier>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OnboardUserRequest {
    pub tier_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloudaicompanion_project: Option<String>,
    pub metadata: ClientMetadata,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectRef {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OnboardUserResponse {
    #[serde(default)]
    pub cloudaicompanion_project: Option<ProjectRef>,
}

/// Long-running operation returned by `onboardUser` and by polling `GET {base}/{name}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LongRunningOperation {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub done: Option<bool>,
    #[serde(default)]
    pub response: Option<OnboardUserResponse>,
}

/// Resolved setup (PROTOCOL 2.7). `project_id` is sent as `project` in generate calls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserSetup {
    pub project_id: String,
    pub user_tier: String,
    pub user_tier_name: Option<String>,
    pub paid_tier: Option<GeminiUserTier>,
    pub has_onboarded_previously: bool,
}

// ---------------------------------------------------------------------------
// countTokens / retrieveUserQuota
// ---------------------------------------------------------------------------

/// Inner countTokens request; `model` has the `models/` prefix here (PROTOCOL 2.9).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CountTokensInner {
    pub model: String,
    pub contents: Vec<Content>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CountTokensRequest {
    pub request: CountTokensInner,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CountTokensResponse {
    #[serde(default)]
    pub total_tokens: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaBucket {
    #[serde(default)]
    pub remaining_amount: Option<String>,
    #[serde(default)]
    pub remaining_fraction: Option<f64>,
    #[serde(default)]
    pub reset_time: Option<String>,
    #[serde(default)]
    pub token_type: Option<String>,
    #[serde(default)]
    pub model_id: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RetrieveUserQuotaResponse {
    #[serde(default)]
    pub buckets: Vec<QuotaBucket>,
}

// ---------------------------------------------------------------------------
// Google RPC error body
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct GoogleRpcErrorBody {
    pub error: GoogleRpcError,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct GoogleRpcError {
    #[serde(default)]
    pub code: u16,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub status: Option<String>,
    /// Raw `details[]` (ErrorInfo, RetryInfo, QuotaFailure, Help), keyed by `@type`.
    #[serde(default)]
    pub details: Vec<Value>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn envelope_casing() {
        let req = CaGenerateRequest {
            model: "gemini-2.5-pro".into(),
            project: "p".into(),
            user_prompt_id: "u1".into(),
            request: GenerateContentRequest {
                contents: vec![Content::user_text("hi")],
                session_id: Some("s1".into()),
                generation_config: Some(GenerationConfig::chat_defaults("gemini-2.5-pro")),
                ..Default::default()
            },
            enabled_credit_types: None,
        };
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["user_prompt_id"], "u1");
        assert_eq!(v["request"]["session_id"], "s1");
        assert_eq!(v["request"]["generationConfig"]["topP"], json!(0.95f32));
        assert_eq!(v["request"]["generationConfig"]["thinkingConfig"]["thinkingBudget"], 8192);
        assert!(v.get("enabled_credit_types").is_none());
        assert!(v["request"].get("toolConfig").is_none());
    }

    #[test]
    fn part_round_trip_keeps_signature() {
        let raw = json!({"functionCall":{"id":"a","name":"read_file","args":{"file_path":"x"}},"thoughtSignature":"sig"});
        let p: Part = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(p.thought_signature.as_deref(), Some("sig"));
        assert_eq!(serde_json::to_value(&p).unwrap(), raw);
    }

    #[test]
    fn provider_blocks_load_round_trip_and_never_change_other_parts() {
        // A conversation saved before the field existed loads, and writes back unchanged.
        let old = json!({"role":"model","parts":[{"text":"hi"},{"functionCall":{"id":"a","name":"glob","args":{}}}]});
        let c: Content = serde_json::from_value(old.clone()).unwrap();
        assert!(c.parts.iter().all(|p| p.provider_block.is_none()));
        assert_eq!(serde_json::to_value(&c).unwrap(), old);

        // A stored block survives the conversation store's own serialization.
        let block = json!({"type":"thinking","thinking":"","signature":"EqQB"});
        let p = Part {
            provider_block: Some(ProviderBlock { provider: "anthropic".into(), binding: Some("b1".into()), block: block.clone() }),
            ..Default::default()
        };
        let stored = serde_json::to_value(&p).unwrap();
        assert_eq!(stored, json!({"providerBlock":{"provider":"anthropic","binding":"b1","block":block}}));
        assert_eq!(serde_json::from_value::<Part>(stored).unwrap(), p);
    }

    #[test]
    fn retaining_provider_blocks_keeps_the_issuers_and_drops_only_what_it_emptied() {
        let block = |provider: &str| Part {
            provider_block: Some(ProviderBlock {
                provider: provider.into(),
                binding: None,
                block: json!({"type":"redacted_thinking","data":"x"}),
            }),
            ..Default::default()
        };
        let call = Part {
            function_call: Some(FunctionCall { id: Some("c".into()), name: "glob".into(), args: json!({}) }),
            ..Default::default()
        };
        let contents = vec![
            Content::user_text("go"),
            Content::model(vec![block("anthropic"), call.clone()]),
            Content::model(vec![block("anthropic")]),
            Content::model(vec![block("other"), Part::text("hi")]),
            Content::model(vec![]),
        ];

        let mut kept = contents.clone();
        retain_provider_blocks(&mut kept, Some("anthropic"));
        assert_eq!(kept.len(), 5);
        assert_eq!(kept[1], contents[1]);
        assert_eq!(kept[3].parts, vec![Part::text("hi")]);

        let mut none = contents;
        retain_provider_blocks(&mut none, None);
        // The call keeps its content; a content that held only a block is gone; an empty one
        // that never held a block is not this function's business.
        assert_eq!(none.len(), 4);
        assert_eq!(none[1].parts, vec![call]);
        assert_eq!(none[2].parts, vec![Part::text("hi")]);
        assert!(none[3].parts.is_empty());
        assert!(!serde_json::to_string(&none).unwrap().contains("providerBlock"));
    }

    #[test]
    fn response_without_inner_has_no_candidates() {
        let r: CaGenerateResponse = serde_json::from_value(json!({"traceId":"t"})).unwrap();
        assert!(r.inner().candidates.is_empty());
        assert!(r.first_parts().is_empty());
    }
}
