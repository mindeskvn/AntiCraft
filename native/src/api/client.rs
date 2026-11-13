//! HTTP client for Code Assist (PROTOCOL.md 2.1-2.14): setup, generate, stream, countTokens,
//! quota, retry with backoff and Google RPC error classification.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::stream::BoxStream;
use futures::StreamExt;
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use super::types::*;
use super::{sse, CaError};
use crate::auth::TokenProvider;

pub const DEFAULT_ENDPOINT: &str = "https://cloudcode-pa.googleapis.com";
pub const DEFAULT_API_VERSION: &str = "v1internal";
/// LRO poll interval for `onboardUser` (PROTOCOL 2.6).
pub const LRO_POLL_INTERVAL: Duration = Duration::from_secs(5);
/// Overall onboarding timeout (AntiCraft addition).
pub const LRO_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// Setup cache lifetime (PROTOCOL 2.7 step 5).
pub const SETUP_CACHE_TTL: Duration = Duration::from_secs(30);
/// Longest server-requested retry delay we honour (PROTOCOL 2.13).
pub const MAX_RETRY_DELAY: Duration = Duration::from_secs(300);
/// Largest error body read from the server.
pub const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;
/// Largest unary JSON response accepted.
pub const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;
/// Max chars of any server message kept in a [`CaError`].
pub const MAX_ERROR_MESSAGE_CHARS: usize = 500;
/// Unary setup calls retry this many times on 429/499/5xx/transport (PROTOCOL 2.13).
pub const SETUP_RETRY_ATTEMPTS: u32 = 3;
pub const SETUP_RETRY_INITIAL: Duration = Duration::from_millis(100);
/// No SSE bytes for this long ends the stream with a transport error.
pub const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(120);

/// Endpoint configuration; overridable for mock servers.
#[derive(Debug, Clone)]
pub struct ApiConfig {
    pub endpoint: String,
    pub api_version: String,
    pub user_agent: String,
    /// User-selected GCP project (`GOOGLE_CLOUD_PROJECT`), if any.
    pub user_project: Option<String>,
    /// Latest Flash GA enabled: changes the backend model remapping (PROTOCOL 3.11).
    pub latest_flash_ga: bool,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            endpoint: std::env::var("CODE_ASSIST_ENDPOINT").unwrap_or_else(|_| DEFAULT_ENDPOINT.into()),
            api_version: std::env::var("CODE_ASSIST_API_VERSION")
                .unwrap_or_else(|_| DEFAULT_API_VERSION.into()),
            user_agent: format!(
                "AntiCraft/{} ({}; {})",
                env!("CARGO_PKG_VERSION"),
                std::env::consts::OS,
                std::env::consts::ARCH
            ),
            user_project: None,
            latest_flash_ga: false,
        }
    }
}

impl ApiConfig {
    /// `{endpoint}/{version}`.
    pub fn base(&self) -> String {
        format!("{}/{}", self.endpoint.trim_end_matches('/'), self.api_version)
    }
    /// `{base}:{method}`.
    pub fn method_url(&self, method: &str) -> String {
        format!("{}:{method}", self.base())
    }
    /// `{base}/{operation_name}`.
    pub fn operation_url(&self, name: &str) -> String {
        format!("{}/{name}", self.base())
    }
}

/// Exponential backoff with jitter (PROTOCOL 2.13, application level).
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub initial_delay: Duration,
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self { max_attempts: 10, initial_delay: Duration::from_secs(5), max_delay: Duration::from_secs(30) }
    }
}

impl RetryPolicy {
    /// Delay before attempt `attempt` (1-based retry index), +-30% jitter, capped.
    pub fn delay_for(&self, attempt: u32) -> Duration {
        let base = self.initial_delay.saturating_mul(1u32 << attempt.saturating_sub(1).min(16));
        let base = base.min(self.max_delay);
        let jitter = 0.7 + rand::random::<f64>() * 0.6;
        base.mul_f64(jitter).min(self.max_delay)
    }
}

/// Abstraction over "send a turn to a model" so the agent loop can be tested with a fake.
pub trait ModelBackend: Send + Sync {
    /// Stream one turn. `cancel` aborts the request.
    fn stream_turn(
        &self,
        model: String,
        user_prompt_id: String,
        request: GenerateContentRequest,
        cancel: CancellationToken,
    ) -> BoxStream<'static, Result<CaGenerateResponse, CaError>>;
}

/// Code Assist API client. Cheap to clone.
#[derive(Clone)]
pub struct CodeAssistClient {
    http: reqwest::Client,
    tokens: Arc<dyn TokenProvider>,
    config: Arc<ApiConfig>,
    retry: RetryPolicy,
    lro_poll: Duration,
    setup_cache: Arc<Mutex<Option<(Instant, UserSetup)>>>,
    setup_retry: RetryPolicy,
    stream_idle: Duration,
}

impl CodeAssistClient {
    pub fn new(http: reqwest::Client, tokens: Arc<dyn TokenProvider>, config: ApiConfig) -> Self {
        Self {
            http,
            tokens,
            config: Arc::new(config),
            retry: RetryPolicy::default(),
            lro_poll: LRO_POLL_INTERVAL,
            setup_cache: Arc::new(Mutex::new(None)),
            setup_retry: RetryPolicy {
                max_attempts: SETUP_RETRY_ATTEMPTS,
                initial_delay: SETUP_RETRY_INITIAL,
                max_delay: Duration::from_secs(2),
            },
            stream_idle: STREAM_IDLE_TIMEOUT,
        }
    }

    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// Override the onboarding LRO poll interval (tests use a few milliseconds).
    pub fn with_lro_poll_interval(mut self, interval: Duration) -> Self {
        self.lro_poll = interval;
        self
    }

    /// Override the SSE idle timeout (tests).
    pub fn with_stream_idle_timeout(mut self, idle: Duration) -> Self {
        self.stream_idle = idle;
        self
    }

    pub fn config(&self) -> &ApiConfig {
        &self.config
    }

    /// Forget the cached project/tier (call on login or logout).
    pub async fn clear_setup_cache(&self) {
        *self.setup_cache.lock().await = None;
    }

    // ----- setup ---------------------------------------------------------

    /// `POST :loadCodeAssist` (PROTOCOL 2.5).
    ///
    /// A VPC-SC `SECURITY_POLICY_VIOLATED` error is treated as a standard-tier account, so
    /// setup continues with the user's project (or asks for one).
    pub async fn load_code_assist(&self, req: &LoadCodeAssistRequest) -> Result<LoadCodeAssistResponse, CaError> {
        match self.with_setup_retries(|| self.post_json("loadCodeAssist", req)).await {
            Err(CaError::SecurityPolicyViolated) => Ok(LoadCodeAssistResponse {
                current_tier: Some(GeminiUserTier { id: Some("standard-tier".into()), ..Default::default() }),
                ..Default::default()
            }),
            other => other,
        }
    }

    /// `POST :onboardUser` (PROTOCOL 2.6).
    pub async fn onboard_user(&self, req: &OnboardUserRequest) -> Result<LongRunningOperation, CaError> {
        self.with_setup_retries(|| self.post_json("onboardUser", req)).await
    }

    /// `GET {base}/{name}`.
    pub async fn get_operation(&self, name: &str) -> Result<LongRunningOperation, CaError> {
        let url = self.config.operation_url(name);
        let resp = self.send_authed(|| self.http.get(&url)).await?;
        decode(resp).await
    }

    /// Poll the LRO every [`LRO_POLL_INTERVAL`] until `done`, [`LRO_TIMEOUT`] or `cancel`.
    pub async fn wait_operation(
        &self,
        mut op: LongRunningOperation,
        cancel: &CancellationToken,
    ) -> Result<LongRunningOperation, CaError> {
        let started = Instant::now();
        while op.done != Some(true) {
            let Some(name) = op.name.clone() else { break };
            if started.elapsed() > LRO_TIMEOUT {
                return Err(CaError::Transport("onboarding timed out".into()));
            }
            tokio::select! {
                _ = cancel.cancelled() => return Err(CaError::Cancelled),
                _ = tokio::time::sleep(self.lro_poll) => {}
            }
            op = self.get_operation(&name).await?;
        }
        Ok(op)
    }

    /// Resolve project and tier (PROTOCOL 2.7), cached for [`SETUP_CACHE_TTL`].
    pub async fn setup_user(&self, cancel: &CancellationToken) -> Result<UserSetup, CaError> {
        if let Some((at, s)) = &*self.setup_cache.lock().await {
            if at.elapsed() < SETUP_CACHE_TTL {
                return Ok(s.clone());
            }
        }
        let user_project = self
            .config
            .user_project
            .clone()
            .filter(|p| !p.is_empty() && !p.chars().all(|c| c.is_ascii_digit()));
        let load = self
            .load_code_assist(&LoadCodeAssistRequest {
                cloudaicompanion_project: user_project.clone(),
                metadata: ClientMetadata::setup(user_project.as_deref()),
                mode: None,
            })
            .await?;
        let setup = resolve_setup(self, load, user_project, cancel).await?;
        *self.setup_cache.lock().await = Some((Instant::now(), setup.clone()));
        Ok(setup)
    }

    // ----- generate ------------------------------------------------------

    /// `POST :generateContent` (unary, with transport retries).
    ///
    /// Another provider's reasoning blocks are removed first, as in [`Self::stream_generate_content`].
    pub async fn generate_content(&self, req: &CaGenerateRequest) -> Result<CaGenerateResponse, CaError> {
        let mut req = req.clone();
        retain_provider_blocks(&mut req.request.contents, None);
        self.with_retries(|| self.post_json("generateContent", &req)).await
    }

    /// `POST :streamGenerateContent?alt=sse`. No retry here; the agent layer retries.
    ///
    /// `req.model` is remapped to the backend ID (PROTOCOL 3.11). A history that ran on an
    /// external provider can hold that provider's signed reasoning blocks ([`Part::provider_block`]);
    /// they are removed here, so the Code Assist wire format is the same whatever ran before. A
    /// 401 refreshes the token once and resends. Each body chunk must arrive within the idle
    /// timeout.
    pub async fn stream_generate_content(
        &self,
        req: &CaGenerateRequest,
    ) -> Result<BoxStream<'static, Result<CaGenerateResponse, CaError>>, CaError> {
        let url = format!("{}?alt=sse", self.config.method_url("streamGenerateContent"));
        let mut req = req.clone();
        retain_provider_blocks(&mut req.request.contents, None);
        req.model = crate::settings::backend_model_id(&req.model, self.config.latest_flash_ga);
        let resp = self.send_authed(|| self.http.post(&url).json(&req)).await?;
        let status = resp.status();
        if !status.is_success() {
            let body = read_limited(resp, MAX_ERROR_BODY_BYTES).await;
            return Err(classify_error(status.as_u16(), &String::from_utf8_lossy(&body)));
        }
        let idle = self.stream_idle;
        let bytes = resp.bytes_stream();
        let timed = async_stream::stream! {
            let mut bytes = Box::pin(bytes);
            loop {
                match tokio::time::timeout(idle, bytes.next()).await {
                    Ok(Some(item)) => yield item.map_err(|e| e.without_url().to_string()),
                    Ok(None) => return,
                    Err(_) => { yield Err(format!("no data for {idle:?}")); return; }
                }
            }
        };
        Ok(sse::response_stream(timed).boxed())
    }

    /// `POST :countTokens` (PROTOCOL 2.9). `model` is the bare ID; the prefix is added here.
    /// Another provider's reasoning blocks are removed, as for a generate call.
    pub async fn count_tokens(&self, model: &str, mut contents: Vec<Content>) -> Result<u64, CaError> {
        retain_provider_blocks(&mut contents, None);
        let req = CountTokensRequest { request: CountTokensInner { model: format!("models/{model}"), contents } };
        let r: CountTokensResponse = self.with_setup_retries(|| self.post_json("countTokens", &req)).await?;
        Ok(r.total_tokens)
    }

    /// `POST :retrieveUserQuota` (PROTOCOL 2.10).
    pub async fn retrieve_user_quota(&self, project: &str) -> Result<RetrieveUserQuotaResponse, CaError> {
        let body = serde_json::json!({ "project": project });
        self.with_setup_retries(|| self.post_json("retrieveUserQuota", &body)).await
    }

    // ----- plumbing ------------------------------------------------------

    fn with_headers(&self, rb: reqwest::RequestBuilder, token: &str) -> reqwest::RequestBuilder {
        rb.bearer_auth(token)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::USER_AGENT, &self.config.user_agent)
    }

    /// Send with a bearer token; on 401 force a refresh and resend once (PROTOCOL 1.5).
    async fn send_authed<F>(&self, build: F) -> Result<reqwest::Response, CaError>
    where
        F: Fn() -> reqwest::RequestBuilder,
    {
        let token = self.tokens.access_token().await?;
        let resp = self.with_headers(build(), &token).send().await?;
        if resp.status() != reqwest::StatusCode::UNAUTHORIZED {
            return Ok(resp);
        }
        drop(resp);
        // The token the server just refused: a provider whose refresh token is single-use must not
        // spend it twice for two parallel 401s on the same token.
        let token = self.tokens.force_refresh(&token).await?;
        Ok(self.with_headers(build(), &token).send().await?)
    }

    async fn post_json<Req: Serialize + ?Sized, Resp: DeserializeOwned>(
        &self,
        method: &str,
        body: &Req,
    ) -> Result<Resp, CaError> {
        let url = self.config.method_url(method);
        let resp = self.send_authed(|| self.http.post(&url).json(body)).await?;
        decode(resp).await
    }

    /// Short retry for unary setup calls (3 attempts, 100 ms exponential).
    async fn with_setup_retries<T, F, Fut>(&self, mut f: F) -> Result<T, CaError>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<T, CaError>>,
    {
        let mut attempt = 0;
        loop {
            attempt += 1;
            match f().await {
                Err(e) if e.is_retryable() && attempt < self.setup_retry.max_attempts => {
                    let delay = match &e {
                        CaError::Retryable { delay: Some(d) } => (*d).min(self.setup_retry.max_delay),
                        _ => self.setup_retry.delay_for(attempt),
                    };
                    tokio::time::sleep(delay).await;
                }
                other => return other,
            }
        }
    }

    async fn with_retries<T, F, Fut>(&self, mut f: F) -> Result<T, CaError>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<T, CaError>>,
    {
        let mut attempt = 0;
        loop {
            attempt += 1;
            match f().await {
                Err(e) if e.is_retryable() && attempt < self.retry.max_attempts => {
                    let delay = match &e {
                        CaError::Retryable { delay: Some(d) } => *d,
                        _ => self.retry.delay_for(attempt),
                    };
                    tracing::info!("retrying Code Assist call in {delay:?} (attempt {attempt})");
                    tokio::time::sleep(delay).await;
                }
                other => return other,
            }
        }
    }
}

impl ModelBackend for CodeAssistClient {
    fn stream_turn(
        &self,
        model: String,
        user_prompt_id: String,
        request: GenerateContentRequest,
        cancel: CancellationToken,
    ) -> BoxStream<'static, Result<CaGenerateResponse, CaError>> {
        let this = self.clone();
        async_stream::stream! {
            let setup = tokio::select! {
                _ = cancel.cancelled() => { yield Err(CaError::Cancelled); return; }
                r = this.setup_user(&cancel) => match r {
                    Ok(s) => s,
                    Err(e) => { yield Err(e); return; }
                },
            };
            let req = CaGenerateRequest { model, project: setup.project_id, user_prompt_id, request, enabled_credit_types: None };
            let mut inner = tokio::select! {
                _ = cancel.cancelled() => { yield Err(CaError::Cancelled); return; }
                r = this.stream_generate_content(&req) => match r {
                    Ok(s) => s,
                    Err(e) => { yield Err(e); return; }
                },
            };
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => { yield Err(CaError::Cancelled); return; }
                    next = inner.next() => match next {
                        Some(item) => yield item,
                        None => return,
                    }
                }
            }
        }
        .boxed()
    }
}

async fn decode<T: DeserializeOwned>(resp: reqwest::Response) -> Result<T, CaError> {
    let status = resp.status();
    if !status.is_success() {
        let bytes = read_limited(resp, MAX_ERROR_BODY_BYTES).await;
        return Err(classify_error(status.as_u16(), &String::from_utf8_lossy(&bytes)));
    }
    let mut resp = resp;
    let mut bytes = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        if bytes.len() + chunk.len() > MAX_RESPONSE_BYTES {
            return Err(CaError::Decode("response too large".into()));
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|e| CaError::Decode(e.to_string()))
}

/// Read at most `limit` bytes of a body; errors end the read early.
async fn read_limited(mut resp: reqwest::Response, limit: usize) -> Vec<u8> {
    let mut out = Vec::new();
    while let Ok(Some(chunk)) = resp.chunk().await {
        let room = limit.saturating_sub(out.len());
        out.extend_from_slice(&chunk[..chunk.len().min(room)]);
        if out.len() >= limit {
            break;
        }
    }
    out
}

fn truncate_message(m: &str) -> String {
    if m.chars().count() > MAX_ERROR_MESSAGE_CHARS {
        m.chars().take(MAX_ERROR_MESSAGE_CHARS).collect::<String>() + "..."
    } else {
        m.to_owned()
    }
}

/// Apply PROTOCOL 2.7 to a `loadCodeAssist` response, onboarding when needed.
async fn resolve_setup(
    client: &CodeAssistClient,
    load: LoadCodeAssistResponse,
    user_project: Option<String>,
    cancel: &CancellationToken,
) -> Result<UserSetup, CaError> {
    let ineligible = load.ineligible_tiers.clone().unwrap_or_default();
    let no_project = |inel: Vec<IneligibleTier>| {
        if inel.is_empty() { CaError::ProjectIdRequired } else { CaError::IneligibleTier(inel) }
    };
    if let Some(current) = &load.current_tier {
        let project = load.cloudaicompanion_project.clone().or(user_project).ok_or_else(|| no_project(ineligible))?;
        return Ok(UserSetup {
            project_id: project,
            user_tier: load.paid_tier.as_ref().and_then(|t| t.id.clone()).or(current.id.clone()).unwrap_or_else(|| "standard-tier".into()),
            user_tier_name: load.paid_tier.as_ref().and_then(|t| t.name.clone()).or(current.name.clone()),
            paid_tier: load.paid_tier.clone(),
            has_onboarded_previously: current.has_onboarded_previously.unwrap_or(true),
        });
    }
    if let Some(v) = ineligible.iter().find(|t| t.reason_code.as_deref() == Some("VALIDATION_REQUIRED")) {
        if let Some(url) = &v.validation_url {
            return Err(CaError::ValidationRequired {
                url: url.clone(),
                description: v.reason_message.clone().unwrap_or_default(),
            });
        }
    }
    let tier = load
        .allowed_tiers
        .unwrap_or_default()
        .into_iter()
        .find(|t| t.is_default == Some(true))
        .unwrap_or(GeminiUserTier {
            id: Some("legacy-tier".into()),
            user_defined_cloudaicompanion_project: Some(true),
            ..Default::default()
        });
    let tier_id = tier.id.clone().unwrap_or_else(|| "legacy-tier".into());
    // The free tier must not send a project (server answers Precondition Failed).
    let send_project = if tier_id == "free-tier" { None } else { user_project.clone() };
    let op = client
        .onboard_user(&OnboardUserRequest {
            tier_id: tier_id.clone(),
            cloudaicompanion_project: send_project.clone(),
            metadata: ClientMetadata::setup(send_project.as_deref()),
        })
        .await?;
    let op = client.wait_operation(op, cancel).await?;
    let project = op
        .response
        .and_then(|r| r.cloudaicompanion_project)
        .and_then(|p| p.id)
        .or(user_project)
        .ok_or_else(|| no_project(ineligible))?;
    Ok(UserSetup {
        project_id: project,
        user_tier: tier_id,
        user_tier_name: tier.name,
        paid_tier: None,
        has_onboarded_previously: tier.has_onboarded_previously.unwrap_or(false),
    })
}

/// Parse a proto Duration string (`"12s"`, `"0.5s"`, `"900ms"`).
pub fn parse_proto_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    if let Some(ms) = s.strip_suffix("ms") {
        return ms.parse::<f64>().ok().filter(|v| *v >= 0.0).map(|v| Duration::from_secs_f64(v / 1000.0));
    }
    s.strip_suffix('s')?.parse::<f64>().ok().filter(|v| *v >= 0.0).map(Duration::from_secs_f64)
}

fn is_cloudcode_domain(d: &str) -> bool {
    matches!(
        d,
        "cloudcode-pa.googleapis.com" | "staging-cloudcode-pa.googleapis.com" | "autopush-cloudcode-pa.googleapis.com"
    )
}

/// Map an HTTP error status + body to [`CaError`] (PROTOCOL 2.13). The body is truncated and
/// never contains request headers.
pub fn classify_error(status: u16, body: &str) -> CaError {
    let parsed: Option<GoogleRpcErrorBody> = serde_json::from_str(body).ok();
    let err = parsed.map(|b| b.error).unwrap_or_default();
    let ty = |d: &serde_json::Value| d.get("@type").and_then(|t| t.as_str()).unwrap_or("").to_owned();
    let info = err.details.iter().find(|d| ty(d).ends_with("google.rpc.ErrorInfo"));
    let reason = info.and_then(|d| d.get("reason")).and_then(|v| v.as_str()).unwrap_or("");
    let domain = info.and_then(|d| d.get("domain")).and_then(|v| v.as_str()).unwrap_or("");
    let retry_delay = err
        .details
        .iter()
        .find(|d| ty(d).ends_with("google.rpc.RetryInfo"))
        .and_then(|d| d.get("retryDelay"))
        .and_then(|v| v.as_str())
        .and_then(parse_proto_duration);
    let message = truncate_message(if err.message.is_empty() { body } else { &err.message });
    let security_violation =
        err.details.iter().any(|d| d.get("reason").and_then(|v| v.as_str()) == Some("SECURITY_POLICY_VIOLATED"));

    match status {
        _ if security_violation => CaError::SecurityPolicyViolated,
        404 => CaError::NotFound,
        401 => CaError::Auth(message),
        403 if reason == "VALIDATION_REQUIRED" && is_cloudcode_domain(domain) => {
            let link = err
                .details
                .iter()
                .find(|d| ty(d).ends_with("google.rpc.Help"))
                .and_then(|d| d.get("links"))
                .and_then(|l| l.get(0));
            CaError::ValidationRequired {
                url: link.and_then(|l| l.get("url")).and_then(|v| v.as_str()).unwrap_or_default().to_owned(),
                description: link
                    .and_then(|l| l.get("description"))
                    .and_then(|v| v.as_str())
                    .unwrap_or(&message)
                    .to_owned(),
            }
        }
        403 => CaError::Auth(message),
        429 | 499 | 503 => {
            if message.contains("limit: 0") {
                return CaError::TerminalQuota { reason: message };
            }
            let daily = err.details.iter().filter(|d| ty(d).ends_with("google.rpc.QuotaFailure")).any(|d| {
                d.get("violations").and_then(|v| v.as_array()).is_some_and(|vs| {
                    vs.iter().any(|v| {
                        v.get("quotaId").and_then(|q| q.as_str()).is_some_and(|q| q.contains("PerDay") || q.contains("Daily"))
                    })
                })
            });
            if daily {
                return CaError::TerminalQuota { reason: "daily quota exhausted".into() };
            }
            match reason {
                "MODEL_CAPACITY_EXHAUSTED" | "MODEL_CAPACITY_EXCEEDED" | "INSUFFICIENT_G1_CREDITS_BALANCE" => {
                    return CaError::TerminalQuota { reason: reason.to_owned() }
                }
                "QUOTA_EXHAUSTED" if is_cloudcode_domain(domain) => {
                    return CaError::TerminalQuota { reason: message }
                }
                "RATE_LIMIT_EXCEEDED" if is_cloudcode_domain(domain) => {
                    return match retry_delay {
                        Some(d) if d <= MAX_RETRY_DELAY => CaError::Retryable { delay: Some(d) },
                        _ => CaError::TerminalQuota { reason: message },
                    }
                }
                _ => {}
            }
            if let Some(d) = retry_delay {
                return if d <= MAX_RETRY_DELAY {
                    CaError::Retryable { delay: Some(d) }
                } else {
                    CaError::TerminalQuota { reason: message }
                };
            }
            if let Some(d) = regex::Regex::new(r"Please retry in ([0-9.]+(?:ms|s))")
                .ok()
                .and_then(|re| re.captures(&message).and_then(|c| parse_proto_duration(&c[1])))
            {
                return CaError::Retryable { delay: Some(d) };
            }
            let capacity = regex::Regex::new(r"exhausted your capacity|capacity exceeded|MODEL_CAPACITY_EXHAUSTED")
                .map(|re| re.is_match(&message))
                .unwrap_or(false);
            if capacity {
                CaError::TerminalQuota { reason: message }
            } else {
                CaError::Retryable { delay: None }
            }
        }
        500..=599 => CaError::Retryable { delay: None },
        _ => CaError::Http { status, body_redacted: message.chars().take(500).collect() },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::StaticToken;
    use axum::{http::HeaderMap, routing::post, Json, Router};

    #[test]
    fn durations() {
        assert_eq!(parse_proto_duration("12s"), Some(Duration::from_secs(12)));
        assert_eq!(parse_proto_duration("0.5s"), Some(Duration::from_millis(500)));
        assert_eq!(parse_proto_duration("900ms"), Some(Duration::from_millis(900)));
        assert_eq!(parse_proto_duration("x"), None);
    }

    #[test]
    fn classify_rate_limit_and_daily() {
        let rl = r#"{"error":{"code":429,"message":"m","details":[
            {"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"RATE_LIMIT_EXCEEDED","domain":"cloudcode-pa.googleapis.com"},
            {"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"12s"}]}}"#;
        assert!(matches!(classify_error(429, rl), CaError::Retryable { delay: Some(d) } if d == Duration::from_secs(12)));
        let daily = r#"{"error":{"code":429,"message":"m","details":[
            {"@type":"type.googleapis.com/google.rpc.QuotaFailure","violations":[{"quotaId":"RequestsPerDay"}]}]}}"#;
        assert!(matches!(classify_error(429, daily), CaError::TerminalQuota { .. }));
        assert!(matches!(classify_error(404, ""), CaError::NotFound));
        assert!(matches!(classify_error(400, "bad"), CaError::Http { status: 400, .. }));
    }

    #[tokio::test]
    async fn stream_against_mock_server() {
        let app = Router::new()
            .route(
                "/v1internal:loadCodeAssist",
                post(|| async {
                    Json(serde_json::json!({"currentTier":{"id":"free-tier"},"cloudaicompanionProject":"proj-1"}))
                }),
            )
            .route(
                "/v1internal:streamGenerateContent",
                post(|headers: HeaderMap, q: axum::extract::RawQuery, Json(body): Json<serde_json::Value>| async move {
                    assert!(headers.contains_key("authorization"));
                    assert_eq!(q.0.as_deref(), Some("alt=sse"));
                    assert_eq!(body["project"], "proj-1");
                    assert!(body.get("user_prompt_id").is_some());
                    "data: {\"response\":{\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"Hi\"}]}}]}}\r\n\r\n"
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let cfg = ApiConfig { endpoint: format!("http://{addr}"), ..ApiConfig::default() };
        let client = CodeAssistClient::new(reqwest::Client::new(), Arc::new(StaticToken("test-token".into())), cfg);
        let req = GenerateContentRequest { contents: vec![Content::user_text("hi")], ..Default::default() };
        let chunks: Vec<_> = client
            .stream_turn("gemini-2.5-pro".into(), "p1".into(), req, CancellationToken::new())
            .collect()
            .await;
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].as_ref().unwrap().first_parts()[0].text.as_deref(), Some("Hi"));
    }

    #[tokio::test]
    async fn another_providers_reasoning_never_reaches_code_assist() {
        // A conversation that ran on Anthropic before the user switched back to Code Assist.
        let app = Router::new()
            .route(
                "/v1internal:loadCodeAssist",
                post(|| async { Json(serde_json::json!({"currentTier":{"id":"free-tier"},"cloudaicompanionProject":"proj-1"})) }),
            )
            .route(
                "/v1internal:streamGenerateContent",
                post(|Json(body): Json<serde_json::Value>| async move {
                    let contents = body["request"]["contents"].as_array().cloned().unwrap_or_default();
                    let wire = body.to_string();
                    // Nothing of the block arrives, the call it preceded does, and the model
                    // content that held only a block is gone rather than sent empty.
                    let ok = !wire.contains("providerBlock")
                        && !wire.contains("EqQB")
                        && contents.len() == 3
                        && contents[1]["parts"][0]["functionCall"]["name"] == "glob";
                    let text = if ok { "ok" } else { "leaked" };
                    format!("data: {{\"response\":{{\"candidates\":[{{\"content\":{{\"role\":\"model\",\"parts\":[{{\"text\":\"{text}\"}}]}}}}]}}}}\r\n\r\n")
                }),
            );
        let client = client_for(serve(app).await, None);
        let block = Part {
            provider_block: Some(ProviderBlock {
                provider: "anthropic".into(),
                binding: Some("b1".into()),
                block: serde_json::json!({"type":"thinking","thinking":"plan","signature":"EqQB"}),
            }),
            ..Default::default()
        };
        let call = Part {
            function_call: Some(FunctionCall { id: Some("glob__toolu_1".into()), name: "glob".into(), args: serde_json::json!({}) }),
            ..Default::default()
        };
        let req = GenerateContentRequest {
            contents: vec![
                Content::user_text("list"),
                Content::model(vec![block.clone(), call]),
                Content::model(vec![block]),
                Content::user_text("again"),
            ],
            ..Default::default()
        };
        let chunks: Vec<_> = client
            .stream_turn("gemini-2.5-pro".into(), "p1".into(), req, CancellationToken::new())
            .collect()
            .await;
        assert_eq!(chunks[0].as_ref().unwrap().first_parts()[0].text.as_deref(), Some("ok"));
    }

    use std::sync::atomic::{AtomicUsize, Ordering};

    async fn serve(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    fn client_for(endpoint: String, project: Option<&str>) -> CodeAssistClient {
        let cfg = ApiConfig { endpoint, user_project: project.map(Into::into), ..ApiConfig::default() };
        CodeAssistClient::new(reqwest::Client::new(), Arc::new(StaticToken("test-token".into())), cfg)
            .with_retry(RetryPolicy {
                max_attempts: 3,
                initial_delay: Duration::from_millis(5),
                max_delay: Duration::from_millis(20),
            })
            .with_lro_poll_interval(Duration::from_millis(5))
    }

    fn sample_request() -> CaGenerateRequest {
        CaGenerateRequest {
            model: "m".into(),
            project: "p".into(),
            user_prompt_id: "u".into(),
            request: GenerateContentRequest::default(),
            enabled_credit_types: None,
        }
    }

    #[tokio::test]
    async fn onboarding_polls_lro_and_caches_setup() {
        let loads = Arc::new(AtomicUsize::new(0));
        let polls = Arc::new(AtomicUsize::new(0));
        let (l2, p2) = (loads.clone(), polls.clone());
        let app = Router::new()
            .route(
                "/v1internal:loadCodeAssist",
                post(move || {
                    l2.fetch_add(1, Ordering::SeqCst);
                    async {
                        Json(serde_json::json!({
                            "allowedTiers": [{"id": "free-tier", "name": "Free", "isDefault": true}]
                        }))
                    }
                }),
            )
            .route(
                "/v1internal:onboardUser",
                post(|Json(body): Json<serde_json::Value>| async move {
                    assert_eq!(body["tierId"], "free-tier");
                    // The free tier must not send a project.
                    assert!(body.get("cloudaicompanionProject").is_none_or(|v| v.is_null()));
                    Json(serde_json::json!({"name": "operations/op1", "done": false}))
                }),
            )
            .route(
                "/v1internal/operations/op1",
                axum::routing::get(move || {
                    let n = p2.fetch_add(1, Ordering::SeqCst);
                    async move {
                        if n == 0 {
                            Json(serde_json::json!({"name": "operations/op1", "done": false}))
                        } else {
                            Json(serde_json::json!({"name": "operations/op1", "done": true,
                                "response": {"cloudaicompanionProject": {"id": "onboarded-proj"}}}))
                        }
                    }
                }),
            );
        let client = client_for(serve(app).await, Some("user-proj"));
        let cancel = CancellationToken::new();
        let s = client.setup_user(&cancel).await.unwrap();
        assert_eq!(s.project_id, "onboarded-proj");
        assert_eq!(s.user_tier, "free-tier");
        assert!(!s.has_onboarded_previously);
        assert_eq!(polls.load(Ordering::SeqCst), 2);
        // The second call is served from the cache.
        let s2 = client.setup_user(&cancel).await.unwrap();
        assert_eq!(s2.project_id, s.project_id);
        assert_eq!(loads.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn setup_errors_validation_and_project_required() {
        let app = Router::new().route(
            "/v1internal:loadCodeAssist",
            post(|| async {
                Json(serde_json::json!({"ineligibleTiers": [{"reasonCode": "VALIDATION_REQUIRED",
                    "reasonMessage": "verify", "validationUrl": "https://example.test/verify"}]}))
            }),
        );
        let client = client_for(serve(app).await, None);
        let err = client.setup_user(&CancellationToken::new()).await.unwrap_err();
        assert!(matches!(err, CaError::ValidationRequired { ref url, .. } if url == "https://example.test/verify"));

        let app = Router::new().route(
            "/v1internal:loadCodeAssist",
            post(|| async { Json(serde_json::json!({"currentTier": {"id": "standard-tier"}})) }),
        );
        let client = client_for(serve(app).await, None);
        let err = client.setup_user(&CancellationToken::new()).await.unwrap_err();
        assert!(matches!(err, CaError::ProjectIdRequired));
    }

    #[tokio::test]
    async fn generate_content_retries_5xx_then_succeeds() {
        let hits = Arc::new(AtomicUsize::new(0));
        let h2 = hits.clone();
        let app = Router::new().route(
            "/v1internal:generateContent",
            post(move || {
                let n = h2.fetch_add(1, Ordering::SeqCst);
                async move {
                    if n < 2 {
                        (axum::http::StatusCode::SERVICE_UNAVAILABLE, "{}".to_string())
                    } else {
                        (axum::http::StatusCode::OK, r#"{"traceId":"t","response":{"candidates":[]}}"#.to_string())
                    }
                }
            }),
        );
        let client = client_for(serve(app).await, None);
        let r = client.generate_content(&sample_request()).await.unwrap();
        assert_eq!(r.trace_id.as_deref(), Some("t"));
        assert_eq!(hits.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn quota_is_terminal_and_not_retried() {
        let hits = Arc::new(AtomicUsize::new(0));
        let h2 = hits.clone();
        let app = Router::new().route(
            "/v1internal:generateContent",
            post(move || {
                h2.fetch_add(1, Ordering::SeqCst);
                async {
                    (
                        axum::http::StatusCode::TOO_MANY_REQUESTS,
                        r#"{"error":{"code":429,"message":"q","details":[{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"QUOTA_EXHAUSTED","domain":"cloudcode-pa.googleapis.com"}]}}"#,
                    )
                }
            }),
        );
        let client = client_for(serve(app).await, None);
        let err = client.generate_content(&sample_request()).await.unwrap_err();
        assert!(matches!(err, CaError::TerminalQuota { .. }));
        assert!(err.is_quota());
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn count_tokens_and_401_is_auth_error() {
        let app = Router::new()
            .route(
                "/v1internal:countTokens",
                post(|Json(body): Json<serde_json::Value>| async move {
                    assert_eq!(body["request"]["model"], "models/gemini-2.5-pro");
                    Json(serde_json::json!({"totalTokens": 42}))
                }),
            )
            .route(
                "/v1internal:streamGenerateContent",
                post(|| async {
                    (axum::http::StatusCode::UNAUTHORIZED, r#"{"error":{"code":401,"message":"bad creds"}}"#)
                }),
            );
        let client = client_for(serve(app).await, None);
        let n = client.count_tokens("gemini-2.5-pro", vec![Content::user_text("hi")]).await.unwrap();
        assert_eq!(n, 42);
        let err = client.stream_generate_content(&sample_request()).await.err().unwrap();
        assert!(matches!(err, CaError::Auth(_)));
        // Error text must never contain the bearer token.
        assert!(!err.to_string().contains("test-token"));
    }
}
