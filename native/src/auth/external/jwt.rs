//! Reading the claims of a JWT without verifying it.
//!
//! The only caller is the Codex sign-in: the ID token it gets back from OpenAI's token endpoint
//! carries the account's email, its ChatGPT account id and its plan, and reading them is how the
//! login names the account and fills the row. There is nothing to verify *against* — we are the
//! client that just received the token over TLS from the issuer, and the signature is checked by
//! whoever consumes the token, not by us.
//!
//! So this deliberately does **not** check the signature, `exp` or `aud`, and nothing may treat
//! its output as proof of identity. It is a parse, not an authentication.

use serde::Deserialize;

/// The namespaced claim OpenAI puts its account fields in. Only `email` sits at the top level of
/// its ID token; the workspace id and the plan live inside this object (CLIProxyAPI
/// `internal/auth/codex/jwt_parser.go`, the `CodexAuthInfo` field).
pub const OPENAI_AUTH_CLAIM: &str = "https://api.openai.com/auth";

/// The claims this app reads. Every field is optional: providers add, move and drop them, and a
/// missing one must mean "unknown", never a failed login.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Claims {
    #[serde(default)]
    pub email: Option<String>,
    /// Codex: the `chatgpt_account_id` claim, sent back as `Chatgpt-Account-Id`.
    #[serde(default)]
    pub chatgpt_account_id: Option<String>,
    /// Codex subscription tier (`free` / `plus` / `pro` / `team` …). Hand-edited and older tokens
    /// spell it `plan_type`.
    #[serde(default, alias = "plan_type")]
    pub chatgpt_plan_type: Option<String>,
    /// Expiry, in seconds since the epoch, when the token carries one.
    #[serde(default)]
    pub exp: Option<i64>,
}

/// The fields of [`OPENAI_AUTH_CLAIM`] this app reads.
#[derive(Debug, Default, Deserialize)]
struct OpenAiAuth {
    #[serde(default)]
    chatgpt_account_id: Option<String>,
    #[serde(default)]
    chatgpt_plan_type: Option<String>,
}

impl Claims {
    /// Read the claims out of `token`, `None` when it is not a JWT this can make sense of.
    ///
    /// The account id and the plan are read from [`OPENAI_AUTH_CLAIM`] first, where OpenAI's real
    /// tokens carry them; the top-level spellings are only a fallback for older or hand-made
    /// tokens. Reading the top level alone is how a real sign-in loses the workspace id, and with
    /// it the `Chatgpt-Account-Id` header every Codex request needs.
    ///
    /// Never returns an error string that quotes the token: the caller only ever needs to know
    /// whether it worked.
    pub fn read(token: &str) -> Option<Self> {
        let payload = payload_json(token)?;
        let nested = payload
            .get(OPENAI_AUTH_CLAIM)
            .cloned()
            .and_then(|v| serde_json::from_value::<OpenAiAuth>(v).ok())
            .unwrap_or_default();
        let mut claims: Self = serde_json::from_value(payload).ok()?;
        let filled = |v: Option<String>| v.filter(|s| !s.trim().is_empty());
        if let Some(id) = filled(nested.chatgpt_account_id) {
            claims.chatgpt_account_id = Some(id);
        }
        if let Some(plan) = filled(nested.chatgpt_plan_type) {
            claims.chatgpt_plan_type = Some(plan);
        }
        Some(claims)
    }

    /// The name to file the account under: the email when there is one, else the account id.
    ///
    /// Codex org-scoped tokens sometimes omit `email` while keeping the account id, and an account
    /// with neither cannot be named at all — the login refuses rather than writing a record with
    /// an empty identity.
    pub fn identity(&self) -> Option<&str> {
        self.email
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .or_else(|| self.chatgpt_account_id.as_deref().map(str::trim).filter(|s| !s.is_empty()))
    }
}

/// The decoded payload of `token` as JSON, without reading a single claim out of it.
///
/// Split out so a caller that needs a claim this struct does not model can still get at it, and
/// so the parsing rules live in one place.
pub fn payload_json(token: &str) -> Option<serde_json::Value> {
    let mut parts = token.trim().split('.');
    let _header = parts.next()?;
    let payload = parts.next()?;
    if payload.is_empty() {
        return None;
    }
    let bytes = decode_base64url(payload)?;
    serde_json::from_slice(&bytes).ok()
}

/// base64url, padded or not: providers disagree about the padding, and a pad left in by hand must
/// not be the difference between a named account and an anonymous one.
fn decode_base64url(segment: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    if let Ok(bytes) = engine.decode(segment.trim_end_matches('=')) {
        return Some(bytes);
    }
    // Some issuers use the standard alphabet for the same three segments.
    base64::engine::general_purpose::STANDARD_NO_PAD.decode(segment.trim_end_matches('=')).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jwt(claims: serde_json::Value) -> String {
        use base64::Engine as _;
        let enc = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        // No signature: nothing here verifies one, and the third segment is not read.
        format!(
            "{}.{}.{}",
            enc.encode(b"{\"alg\":\"none\"}"),
            enc.encode(serde_json::to_vec(&claims).unwrap()),
            enc.encode(b"not-a-signature")
        )
    }

    /// The shape of a real OpenAI ID token: `email` at the top, the workspace id and the plan
    /// inside the namespaced claim.
    #[test]
    fn reads_the_account_id_and_plan_from_the_namespaced_claim() {
        let token = jwt(serde_json::json!({
            "email": "me@example.com",
            OPENAI_AUTH_CLAIM: {"chatgpt_account_id": "acct-42", "chatgpt_plan_type": "plus", "user_id": "user-1"},
            "exp": 1_700_000_000i64,
        }));
        let c = Claims::read(&token).unwrap();
        assert_eq!(c.email.as_deref(), Some("me@example.com"));
        assert_eq!(c.chatgpt_account_id.as_deref(), Some("acct-42"));
        assert_eq!(c.chatgpt_plan_type.as_deref(), Some("plus"));
        assert_eq!(c.exp, Some(1_700_000_000));
        assert_eq!(c.identity(), Some("me@example.com"));
    }

    #[test]
    fn the_top_level_spellings_are_a_fallback_only() {
        // Older and hand-made tokens carry the fields at the top level.
        let c = Claims::read(&jwt(serde_json::json!({"chatgpt_account_id": "a", "plan_type": "pro"}))).unwrap();
        assert_eq!(c.chatgpt_account_id.as_deref(), Some("a"));
        assert_eq!(c.chatgpt_plan_type.as_deref(), Some("pro"));
        // When both are present the namespaced claim wins, and a blank one does not erase a value.
        let c = Claims::read(&jwt(serde_json::json!({
            "chatgpt_account_id": "top",
            "chatgpt_plan_type": "free",
            OPENAI_AUTH_CLAIM: {"chatgpt_account_id": "nested", "chatgpt_plan_type": "  "},
        })))
        .unwrap();
        assert_eq!(c.chatgpt_account_id.as_deref(), Some("nested"));
        assert_eq!(c.chatgpt_plan_type.as_deref(), Some("free"));
        // A namespaced claim of the wrong type is ignored, not a failed read.
        let c = Claims::read(&jwt(serde_json::json!({"email": "x@example.com", OPENAI_AUTH_CLAIM: "oops"}))).unwrap();
        assert_eq!(c.email.as_deref(), Some("x@example.com"));
        assert_eq!(c.chatgpt_account_id, None);
    }

    #[test]
    fn identity_falls_back_to_the_account_id_and_ignores_blanks() {
        let c = Claims { email: Some("  ".into()), chatgpt_account_id: Some("acct-1".into()), ..Default::default() };
        assert_eq!(c.identity(), Some("acct-1"));
        assert_eq!(Claims::default().identity(), None);
        let c = Claims { email: Some(" me@example.com ".into()), ..Default::default() };
        assert_eq!(c.identity(), Some("me@example.com"));
    }

    #[test]
    fn nonsense_is_none_not_a_panic() {
        for bad in ["", "not-a-jwt", "a.b", "a..c", "a.!!!.c", "a.eyJub3QganNvbn0.c"] {
            assert!(Claims::read(bad).is_none(), "{bad:?}");
        }
        // A payload that is valid JSON but not an object is still a failed read.
        assert!(Claims::read(&jwt(serde_json::json!([1, 2]))).is_none());
        // Padding is tolerated: `e30=` is `{}` with a pad the issuer should not have left on.
        assert!(Claims::read("h.e30=.s").is_some());
        assert_eq!(Claims::read("h.e30=.s").unwrap(), Claims::default());
    }
}
