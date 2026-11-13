//! Google Code Assist (`cloudcode-pa` `v1internal`) client (PROTOCOL.md section 2).

pub mod client;
pub mod sse;
pub mod types;

use std::time::Duration;

pub use client::{ApiConfig, CodeAssistClient, ModelBackend, RetryPolicy};
pub use types::*;

/// Errors from the Code Assist API. `Display` never includes headers or tokens.
#[derive(Debug, Clone, thiserror::Error)]
pub enum CaError {
    /// Rate limit or transient failure; retry after `delay` if given.
    #[error("rate limited or temporarily unavailable")]
    Retryable { delay: Option<Duration> },
    /// Daily quota, capacity or credits exhausted. Do not retry.
    #[error("quota exhausted: {reason}")]
    TerminalQuota { reason: String },
    /// Account needs verification at `url` (PROTOCOL 2.7 step 2, 2.13).
    #[error("account validation required: {description}")]
    ValidationRequired { url: String, description: String },
    #[error("account is not eligible: {}", .0.iter().filter_map(|t| t.reason_message.clone()).collect::<Vec<_>>().join(", "))]
    IneligibleTier(Vec<IneligibleTier>),
    #[error("a Google Cloud project ID is required for this account")]
    ProjectIdRequired,
    #[error("model not found")]
    NotFound,
    /// VPC-SC `SECURITY_POLICY_VIOLATED` (PROTOCOL 2.5); setup treats it as standard tier.
    #[error("request blocked by a security policy (VPC Service Controls)")]
    SecurityPolicyViolated,
    /// Authentication failed (401/403 without a validation reason, or no login).
    #[error("authentication failed: {0}")]
    Auth(String),
    #[error("HTTP {status}: {body_redacted}")]
    Http { status: u16, body_redacted: String },
    #[error("network error: {0}")]
    Transport(String),
    #[error("invalid response: {0}")]
    Decode(String),
    #[error("request cancelled")]
    Cancelled,
}

impl CaError {
    /// Whether the application-level retry loop should try again.
    pub fn is_retryable(&self) -> bool {
        matches!(self, CaError::Retryable { .. } | CaError::Transport(_))
    }

    /// Whether the UI should show the quota banner.
    pub fn is_quota(&self) -> bool {
        matches!(self, CaError::TerminalQuota { .. } | CaError::Retryable { .. })
    }
}

impl From<crate::auth::AuthError> for CaError {
    fn from(e: crate::auth::AuthError) -> Self {
        CaError::Auth(e.to_string())
    }
}

impl From<reqwest::Error> for CaError {
    fn from(e: reqwest::Error) -> Self {
        CaError::Transport(e.without_url().to_string())
    }
}
