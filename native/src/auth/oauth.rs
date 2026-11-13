//! OAuth 2.0 installed-app mechanics: PKCE, state, loopback callback server, code exchange,
//! refresh and userinfo (PROTOCOL.md 1.2-1.5, 1.7).

use base64::Engine as _;
use serde::Deserialize;
use sha2::Digest as _;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use super::external::ProviderSpec;
use super::{AuthConfig, AuthError, LOGIN_TIMEOUT, REFRESH_TIMEOUT, SCOPES};

pub const SUCCESS_URL: &str = "https://developers.google.com/gemini-code-assist/auth_success_gemini";
pub const FAILURE_URL: &str = "https://developers.google.com/gemini-code-assist/auth_failure_gemini";
pub const CALLBACK_PATH: &str = "/oauth2callback";
/// How long one loopback connection may take to send its request line.
pub const CALLBACK_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// OAuth endpoint URLs; all overridable so tests can point at a mock server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthEndpoints {
    pub auth_url: String,
    pub token_url: String,
    pub userinfo_url: String,
    pub tokeninfo_url: String,
    pub revoke_url: String,
}

impl OAuthEndpoints {
    /// Google production endpoints (google-auth-library defaults).
    pub fn google() -> Self {
        Self {
            auth_url: "https://accounts.google.com/o/oauth2/v2/auth".into(),
            token_url: "https://oauth2.googleapis.com/token".into(),
            userinfo_url: "https://www.googleapis.com/oauth2/v2/userinfo".into(),
            tokeninfo_url: "https://oauth2.googleapis.com/tokeninfo".into(),
            revoke_url: "https://oauth2.googleapis.com/revoke".into(),
        }
    }

    /// All endpoints under one base URL (mock servers): `{base}/auth`, `{base}/token`, ...
    pub fn with_base(base: &str) -> Self {
        let b = base.trim_end_matches('/');
        Self {
            auth_url: format!("{b}/auth"),
            token_url: format!("{b}/token"),
            userinfo_url: format!("{b}/userinfo"),
            tokeninfo_url: format!("{b}/tokeninfo"),
            revoke_url: format!("{b}/revoke"),
        }
    }
}

/// PKCE S256 verifier/challenge pair.
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Pkce {
    /// 43-char base64url verifier from 32 random bytes, and its S256 challenge.
    pub fn generate() -> Self {
        Self::generate_bytes(32)
    }

    /// The same pair from `n` random bytes. Codex and Claude both ask for a **96-byte** verifier,
    /// which base64url spells as exactly 128 characters — the length their authorize endpoints
    /// expect. A verifier is a bearer credential for the code exchange, so it is generated here
    /// and never formatted into a log line.
    pub fn generate_bytes(n: usize) -> Self {
        use rand::RngCore as _;
        let mut bytes = vec![0u8; n];
        rand::rng().fill_bytes(&mut bytes);
        let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&bytes);
        let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(sha2::Sha256::digest(verifier.as_bytes()));
        Self { verifier, challenge }
    }
}

/// 32 random bytes as 64 lowercase hex chars.
pub fn generate_state() -> String {
    let bytes: [u8; 32] = rand::random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Constant-time string equality for the `state` check.
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Build the authorization URL (PROTOCOL 1.2 step 5, plus PKCE and optional `prompt=consent`).
pub fn build_auth_url(
    endpoints: &OAuthEndpoints,
    client_id: &str,
    redirect_uri: &str,
    state: &str,
    pkce: &Pkce,
    force_consent: bool,
) -> Result<url::Url, AuthError> {
    let mut u = url::Url::parse(&endpoints.auth_url).map_err(|e| AuthError::Denied(format!("bad auth url: {e}")))?;
    {
        let mut q = u.query_pairs_mut();
        q.append_pair("client_id", client_id)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("response_type", "code")
            .append_pair("access_type", "offline")
            .append_pair("scope", &SCOPES.join(" "))
            .append_pair("state", state)
            .append_pair("code_challenge", &pkce.challenge)
            .append_pair("code_challenge_method", "S256");
        if force_consent {
            q.append_pair("prompt", "consent");
        }
    }
    Ok(u)
}

/// Authorization URL from a [`ProviderSpec`] plus per-flow extras.
///
/// One builder for all three external providers instead of three copies: the only parts that
/// differ (client, scopes, extra parameters, whether PKCE is used at all) are in the spec. `extra`
/// is appended after the spec's own parameters so a caller can override one — query pairs are
/// collected into a map first, and the last write for a key wins, which is also why the result
/// has a stable order rather than duplicating a key.
///
/// `pkce` is `Option`: Antigravity's installed-app flow has none, and adding a challenge the
/// provider will not verify would still make Google echo a `code_verifier`-less flow confusingly.
pub fn build_auth_url_for(
    spec: &ProviderSpec,
    redirect_uri: &str,
    state: &str,
    pkce: Option<&Pkce>,
    extra: &[(&str, &str)],
) -> Result<url::Url, AuthError> {
    let mut u = url::Url::parse(spec.auth_url).map_err(|e| AuthError::Denied(format!("bad auth url: {e}")))?;
    {
        let mut q = u.query_pairs_mut();
        q.append_pair("client_id", spec.client_id)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("response_type", "code")
            .append_pair("scope", &spec.scopes.join(" "))
            .append_pair("state", state);
        if let Some(p) = pkce {
            q.append_pair("code_challenge", &p.challenge)
                .append_pair("code_challenge_method", "S256");
        }
        for (k, v) in spec.extra_auth_params.iter().chain(extra.iter()) {
            q.append_pair(k, v);
        }
    }
    Ok(u)
}

/// Token endpoint response. `Debug` is redacted.
#[derive(Clone, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub expires_in: Option<i64>,
    pub refresh_token: Option<String>,
    pub scope: Option<String>,
    pub token_type: Option<String>,
    pub id_token: Option<String>,
}

impl std::fmt::Debug for TokenResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenResponse")
            .field("expires_in", &self.expires_in)
            .field("has_refresh_token", &self.refresh_token.is_some())
            .finish_non_exhaustive()
    }
}

/// Result of parsing one loopback callback request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallbackOutcome {
    Code(String),
    /// Empty/slow connection (browser preconnect) or wrong Host; ignored.
    Ignored,
    WrongPath,
    Denied(String),
    StateMismatch,
    NoCode,
}

/// Classify a callback request target (path + query) against the expected state (PROTOCOL 1.2 step 7).
pub fn parse_callback(target: &str, expected_state: &str) -> CallbackOutcome {
    parse_callback_at(target, CALLBACK_PATH, expected_state)
}

/// [`parse_callback`] with a configurable path.
///
/// The external providers each register their own callback path (`/oauth-callback`,
/// `/auth/callback`, `/callback`), and the path is part of what the provider echoes back, so it
/// is compared exactly rather than ignored. Everything else — state before anything else, then an
/// `error` grant, then the code — is the order the Google flow already relies on.
pub fn parse_callback_at(target: &str, path: &str, expected_state: &str) -> CallbackOutcome {
    let Ok(u) = url::Url::parse(&format!("http://127.0.0.1{target}")) else {
        return CallbackOutcome::WrongPath;
    };
    if u.path() != path {
        return CallbackOutcome::WrongPath;
    }
    let get = |k: &str| u.query_pairs().find(|(n, _)| n == k).map(|(_, v)| v.into_owned());
    // State first, so only the real redirect (which echoes our state) can end the flow.
    match get("state") {
        Some(s) if constant_time_eq(&s, expected_state) => {}
        _ => return CallbackOutcome::StateMismatch,
    }
    if let Some(err) = get("error") {
        let desc = get("error_description").unwrap_or_default();
        return CallbackOutcome::Denied(format!("{err} {desc}").trim().to_owned());
    }
    get("code").map(CallbackOutcome::Code).unwrap_or(CallbackOutcome::NoCode)
}

/// What the user pasted into a "paste the redirect link" box.
///
/// Separate from [`CallbackOutcome`] because the two do not describe the same thing: a callback
/// is a request the server received (so a wrong path is meaningful), while a paste is a string a
/// person copied. It can be the full URL the browser landed on, the URL with Claude's
/// `code#state` fragment appended, or just the query part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PastedLink {
    /// The authorization code, and the state that came with it when the link carried one. The
    /// caller must still compare the state: a fragment's state **overrides** a query state, which
    /// is how Claude returns it, and the result is checked exactly like a loopback callback.
    Code { code: String, state: Option<String> },
    /// The provider refused the request and said why.
    Denied(String),
    /// Not a redirect URL this can read. **Never** carries the input: the pasted string is a
    /// credential and must not be echoed into a row or a log.
    Unreadable,
}

/// Parse a pasted redirect link.
///
/// The two shapes that matter:
/// * a normal redirect, `http://localhost:54545/callback?code=…&state=…`;
/// * Claude's, which puts the state behind a `#` **inside the code parameter**
///   (`?code=<code>#<state>`), so the browser's fragment separator is part of the value the
///   provider sent. Splitting on the first `#` and letting the trailing part win the state is
///   the only reading that satisfies both: the exchange body needs the state from there.
///
/// A bare query (`code=…&state=…`) is accepted too, because that is what some provider pages let
/// the user copy at the end of a failed loopback attempt.
pub fn parse_pasted_link(link: &str) -> PastedLink {
    let raw = link.trim();
    if raw.is_empty() {
        return PastedLink::Unreadable;
    }
    // `?` before `#`: a URL's fragment comes after its query, so splitting there keeps the
    // query intact and moves Claude's inline `#state` into the fragment slot.
    let (head, fragment) = match raw.split_once('#') {
        Some((h, f)) => (h, Some(f)),
        None => (raw, None),
    };
    let query = match head.split_once('?') {
        Some((_, q)) => q,
        // No `?`: treat it as a bare query, but only if it looks like one. A random sentence
        // with an `=` in it would otherwise parse into an empty pair set.
        None if head.contains('=') => head,
        None => return PastedLink::Unreadable,
    };
    let mut code = None;
    let mut state = None;
    let mut error = None;
    let mut description = None;
    for (k, v) in url::form_urlencoded::parse(query.as_bytes()) {
        match k.as_ref() {
            "code" => code = Some(v.into_owned()),
            "state" => state = Some(v.into_owned()),
            "error" => error = Some(v.into_owned()),
            "error_description" => description = Some(v.into_owned()),
            _ => {}
        }
    }
    // The fragment wins over the query state; it is where Claude puts the real one.
    if let Some(f) = fragment {
        let f = f.trim();
        if !f.is_empty() && !f.contains('=') && !f.contains('&') {
            state = Some(f.to_owned());
        }
    }
    if let Some(err) = error {
        let text = format!("{} {}", err, description.unwrap_or_default());
        return PastedLink::Denied(text.trim().to_owned());
    }
    match code {
        // An empty `code=` is a provider bug, not a code: treating it as one would send an empty
        // exchange and blame the user for the resulting 400.
        Some(c) if !c.trim().is_empty() => PastedLink::Code { code: c, state },
        _ => PastedLink::Unreadable,
    }
}

/// The message shown when a pasted link cannot be parsed. One string, so the caller cannot
/// accidentally format the input into it.
pub const BAD_PASTE_MESSAGE: &str = "That does not look like a redirect URL.";

/// Loopback listener bound once and kept open (avoids the probe-then-rebind race).
///
/// The Google flow advertises `127.0.0.1` and needs one listener. The external providers are
/// registered with `http://localhost:…`, and the browser may resolve `localhost` to `::1` — on
/// Windows it usually does. Binding only IPv4 is how a callback lands on a closed port and the
/// sign-in appears to hang, so the server holds whichever stacks it could bind and accepts on
/// either.
pub struct LoopbackServer {
    /// The path the provider was told to redirect to; also what [`Self::parse`] compares.
    path: &'static str,
    v4: TcpListener,
    /// `None` when the host has no usable IPv6 loopback. Not fatal: IPv4 still works, and the
    /// paste path covers a browser that insisted on `::1`.
    v6: Option<TcpListener>,
    port: u16,
}

impl LoopbackServer {
    /// Bind on `127.0.0.1:{port}` (plus `::1` when the host has it); `None` picks a random free
    /// port. The path defaults to [`CALLBACK_PATH`], exactly as before.
    ///
    /// This is the Google flow's server, and that flow advertises the literal `127.0.0.1`: no
    /// browser is sent to `::1`, so a `::1` held by someone else is only worth a warning here.
    pub async fn bind(port: Option<u16>) -> std::io::Result<Self> {
        Self::bind_stacks(port, CALLBACK_PATH, false).await
    }

    /// [`Self::bind`] with the callback path spelled out, for a redirect that names `localhost`.
    ///
    /// IPv4 is the primary stack: if it cannot be bound the sign-in cannot work at all, so that
    /// error is returned with a message naming the **advertised** host and port rather than the
    /// bind address — "127.0.0.1:51121 is already in use" is the wrong thing to show a user who
    /// is looking at a `localhost` link.
    ///
    /// `::1` must then be ours as well, or not exist. The browser resolves `localhost` itself and
    /// may well pick `::1` — on Windows it usually does — so a `::1` on this port held by another
    /// program would be handed the authorization code instead of us. That is refused, not
    /// warned about. A host with no IPv6 loopback at all is fine: no browser can choose it.
    pub async fn bind_pair(port: Option<u16>, path: &'static str) -> std::io::Result<Self> {
        Self::bind_stacks(port, path, true).await
    }

    async fn bind_stacks(port: Option<u16>, path: &'static str, own_v6: bool) -> std::io::Result<Self> {
        // A random port that happens to be taken on `::1` only is not the user's problem: pick
        // another. A fixed one is, and is reported.
        let tries = if port.is_none() { 5 } else { 1 };
        let mut refused = None;
        for _ in 0..tries {
            let v4 = TcpListener::bind(("127.0.0.1", port.unwrap_or(0))).await.map_err(|e| {
                if e.kind() == std::io::ErrorKind::AddrInUse {
                    let p = port.unwrap_or(0);
                    std::io::Error::new(
                        e.kind(),
                        format!(
                            "port {p} on localhost is already in use by another program. Close it and try again, or paste the redirect link instead"
                        ),
                    )
                } else {
                    e
                }
            })?;
            let bound = v4.local_addr()?.port();
            match TcpListener::bind(("::1", bound)).await {
                Ok(l) => return Ok(Self { path, v4, v6: Some(l), port: bound }),
                Err(e) if held_by_another(&e) && own_v6 => {
                    // Reported as `AddrInUse` whatever the OS said (Windows answers an exclusive
                    // holder with "access denied"), so the caller shows it like any busy port.
                    refused = Some(std::io::Error::new(
                        std::io::ErrorKind::AddrInUse,
                        format!(
                            "port {bound} on localhost is held on IPv6 (::1) by another program, which could receive the sign-in instead of AntiCraft. Close it and try again, or paste the redirect link instead"
                        ),
                    ));
                }
                Err(e) => {
                    if held_by_another(&e) {
                        tracing::warn!("port {bound} is free on 127.0.0.1 but busy on ::1");
                    }
                    return Ok(Self { path, v4, v6: None, port: bound });
                }
            }
        }
        Err(refused.unwrap_or_else(|| std::io::Error::new(std::io::ErrorKind::AddrInUse, "no free loopback port")))
    }

    /// Bind one explicit address and path, on a single stack. Tests use this to hold a port
    /// (proving the paste fallback survives an `AddrInUse`); nothing in the app needs it.
    ///
    /// The socket is filed under `v4` whatever its family: `accept_one` treats one listener as
    /// one listener, and the Host check already accepts every loopback spelling.
    pub async fn bind_on(host: &str, port: Option<u16>, path: &'static str) -> std::io::Result<Self> {
        let listener = TcpListener::bind((host, port.unwrap_or(0))).await?;
        let port = listener.local_addr()?.port();
        Ok(Self { path, v4: listener, v6: None, port })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// `http://127.0.0.1:{port}{path}` — unchanged spelling, kept for the Google flow and its
    /// tests.
    pub fn redirect_uri(&self) -> String {
        format!("http://127.0.0.1:{}{}", self.port, self.path)
    }

    /// `http://localhost:{port}{path}` — what the external providers are registered with.
    ///
    /// Deliberately not `redirect_uri()` with a string swap at the call site: the string has to
    /// match the provider's registration byte for byte, and this is the one place that knows the
    /// bound port.
    pub fn redirect_uri_host(&self) -> String {
        format!("http://localhost:{}{}", self.port, self.path)
    }

    /// Accept one connection, answer it, and classify it. Connections that send nothing
    /// within [`CALLBACK_READ_TIMEOUT`], or name another Host, are [`CallbackOutcome::Ignored`].
    pub async fn accept_one(&self, expected_state: &str) -> std::io::Result<CallbackOutcome> {
        let (mut sock, _) = match self.v6.as_ref() {
            Some(v6) => {
                // `select!` over two accepts: whichever stack the browser reached wins, and the
                // other future is dropped without a half-accepted socket.
                let v4 = self.v4.accept();
                let v6 = v6.accept();
                tokio::pin!(v4, v6);
                tokio::select! {
                    r = &mut v4 => r?,
                    r = &mut v6 => r?,
                }
            }
            None => self.v4.accept().await?,
        };
        let head = match tokio::time::timeout(CALLBACK_READ_TIMEOUT, read_head(&mut sock)).await {
            Ok(Ok(h)) if !h.is_empty() => h,
            _ => return Ok(CallbackOutcome::Ignored),
        };
        let mut lines = head.lines();
        let target = lines.next().and_then(|l| l.split_whitespace().nth(1)).unwrap_or("/").to_owned();
        // Three spellings reach a loopback listener; all three are this server.
        let host_ok = lines
            .take_while(|l| !l.is_empty())
            .find_map(|l| l.split_once(':').filter(|(k, _)| k.trim().eq_ignore_ascii_case("host")).map(|(_, v)| v.trim().to_owned()))
            .is_none_or(|h| {
                h == format!("127.0.0.1:{}", self.port)
                    || h == format!("localhost:{}", self.port)
                    || h == format!("[::1]:{}", self.port)
            });
        let outcome = if host_ok { parse_callback_at(&target, self.path, expected_state) } else { CallbackOutcome::Ignored };
        let resp = match &outcome {
            CallbackOutcome::Code(_) => redirect(SUCCESS_URL),
            CallbackOutcome::Denied(_) | CallbackOutcome::NoCode => redirect(FAILURE_URL),
            CallbackOutcome::StateMismatch => plain("400 Bad Request", "State mismatch. Possible CSRF attack"),
            CallbackOutcome::WrongPath | CallbackOutcome::Ignored => plain("404 Not Found", "Not found"),
        };
        let _ = sock.write_all(resp.as_bytes()).await;
        let _ = sock.shutdown().await;
        Ok(outcome)
    }

    /// Accept connections until the real callback (exact path, matching state) arrives.
    /// Junk requests, preconnects and forged states are answered and ignored.
    pub async fn wait_for_callback(&self, expected_state: &str) -> std::io::Result<CallbackOutcome> {
        loop {
            match self.accept_one(expected_state).await? {
                CallbackOutcome::WrongPath | CallbackOutcome::Ignored => continue,
                CallbackOutcome::StateMismatch => {
                    tracing::warn!("ignored a loopback callback with a mismatched state");
                    continue;
                }
                other => return Ok(other),
            }
        }
    }
}

/// Whether a failed bind means another socket owns the address, as opposed to the host having no
/// such address at all. Windows answers a port held with `SO_EXCLUSIVEADDRUSE` with "access
/// denied" rather than "in use", so both count.
fn held_by_another(e: &std::io::Error) -> bool {
    matches!(e.kind(), std::io::ErrorKind::AddrInUse | std::io::ErrorKind::PermissionDenied)
}

/// Read until the end of the HTTP header block (or 8 KiB / EOF).
async fn read_head(sock: &mut tokio::net::TcpStream) -> std::io::Result<String> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    while buf.len() < 8192 {
        let n = sock.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn plain(status: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn redirect(location: &str) -> String {
    format!("HTTP/1.1 301 Moved Permanently\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
}

/// Full browser flow: bind, open the system browser, await the callback, exchange the code.
pub async fn browser_login(
    http: &reqwest::Client,
    config: &AuthConfig,
    force_consent: bool,
    cancel: CancellationToken,
) -> Result<TokenResponse, AuthError> {
    browser_login_with(http, config, force_consent, cancel, |url| {
        if let Err(e) = open::that(url.as_str()) {
            tracing::warn!("could not open browser: {e}");
        }
    })
    .await
}

/// [`browser_login`] with an injectable "open browser" step (tests drive the callback).
pub async fn browser_login_with(
    http: &reqwest::Client,
    config: &AuthConfig,
    force_consent: bool,
    cancel: CancellationToken,
    open_browser: impl FnOnce(url::Url),
) -> Result<TokenResponse, AuthError> {
    let server = LoopbackServer::bind(config.callback_port).await?;
    let state = generate_state();
    let pkce = Pkce::generate();
    let redirect_uri = server.redirect_uri();
    let url = build_auth_url(&config.endpoints, &config.client_id, &redirect_uri, &state, &pkce, force_consent)?;
    open_browser(url);
    let outcome = tokio::select! {
        r = server.wait_for_callback(&state) => r?,
        _ = cancel.cancelled() => return Err(AuthError::Cancelled),
        _ = tokio::time::sleep(LOGIN_TIMEOUT) => return Err(AuthError::Timeout),
    };
    match outcome {
        CallbackOutcome::Code(code) => tokio::select! {
            r = exchange_code(http, config, &code, &redirect_uri, Some(&pkce.verifier)) => r,
            _ = cancel.cancelled() => Err(AuthError::Cancelled),
        },
        CallbackOutcome::Denied(msg) => Err(AuthError::Denied(msg)),
        CallbackOutcome::StateMismatch => Err(AuthError::StateMismatch),
        CallbackOutcome::WrongPath | CallbackOutcome::NoCode | CallbackOutcome::Ignored => {
            Err(AuthError::Denied("No authorization code received".into()))
        }
    }
}

/// Exchange an authorization code (PROTOCOL 1.4).
pub async fn exchange_code(
    http: &reqwest::Client,
    config: &AuthConfig,
    code: &str,
    redirect_uri: &str,
    code_verifier: Option<&str>,
) -> Result<TokenResponse, AuthError> {
    let mut form = vec![
        ("code", code),
        ("client_id", config.client_id.as_str()),
        ("client_secret", config.client_secret.as_str()),
        ("redirect_uri", redirect_uri),
        ("grant_type", "authorization_code"),
    ];
    if let Some(v) = code_verifier {
        form.push(("code_verifier", v));
    }
    post_token(http, &config.endpoints.token_url, &form).await
}

/// Refresh grant (PROTOCOL 1.5). `invalid_grant` maps to [`AuthError::ReauthRequired`].
pub async fn refresh_token(
    http: &reqwest::Client,
    config: &AuthConfig,
    refresh_token: &str,
) -> Result<TokenResponse, AuthError> {
    let form = [
        ("refresh_token", refresh_token),
        ("client_id", config.client_id.as_str()),
        ("client_secret", config.client_secret.as_str()),
        ("grant_type", "refresh_token"),
    ];
    post_token(http, &config.endpoints.token_url, &form).await
}

/// Form-encoded token POST (exchange or refresh), with a 30-second deadline.
///
/// A token request that hangs must not hold the per-account gate forever: every caller waits
/// behind one of these, so a stalled connection would look like a frozen app. [`REFRESH_TIMEOUT`]
/// is the deadline for exchange, refresh and the external providers alike.
pub async fn post_token_form(
    http: &reqwest::Client,
    url: &str,
    form: &[(&str, &str)],
) -> Result<TokenResponse, AuthError> {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(form)
        .finish();
    token_call(http, url, "application/x-www-form-urlencoded", body).await
}

/// JSON token POST. `body` is a struct, not a map, where the caller needs a fixed field order:
/// Anthropic's endpoint is mirrored from a client that serialises a struct, and serde_json keeps
/// declaration order while a `Map` would sort the keys.
pub async fn post_token_json<B: serde::Serialize + ?Sized>(
    http: &reqwest::Client,
    url: &str,
    body: &B,
) -> Result<TokenResponse, AuthError> {
    let text = serde_json::to_string(body)?;
    token_call(http, url, "application/json", text).await
}

/// One token request: deadline, then either the body or a parsed OAuth error.
async fn token_call(
    http: &reqwest::Client,
    url: &str,
    content_type: &str,
    body: String,
) -> Result<TokenResponse, AuthError> {
    let (status, _headers, bytes) = send_token_request(http, url, content_type, body).await?;
    if status.is_success() {
        return Ok(serde_json::from_slice(&bytes)?);
    }
    Err(token_error(status, &bytes))
}

/// The raw half of [`token_call`]: deadline, status, headers, body. Nothing is parsed here.
///
/// A refresh has to see a 429 and its `Retry-After` before deciding to give up, and a login has
/// to see the extra keys a token response carries per provider (`account`, `organization`).
/// Parsing in one shared place would mean throwing both away, so the decision is left to callers
/// that know more than this function does.
pub(crate) async fn send_token_request(
    http: &reqwest::Client,
    url: &str,
    content_type: &str,
    body: String,
) -> Result<(reqwest::StatusCode, reqwest::header::HeaderMap, Vec<u8>), AuthError> {
    let send = http
        .post(url)
        .header(reqwest::header::CONTENT_TYPE, content_type)
        .body(body)
        .send();
    let resp = match tokio::time::timeout(REFRESH_TIMEOUT, send).await {
        Ok(r) => r?,
        Err(_) => return Err(AuthError::Timeout),
    };
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp.bytes().await?;
    Ok((status, headers, bytes.to_vec()))
}

/// `Retry-After` clamped to [5 s, 5 min]. Both spellings are read: delta-seconds, which every
/// provider here uses, and an HTTP date, which the RFC also allows. A missing or unreadable
/// header yields the maximum wait — waiting longer is always safe when retrying a request that
/// might otherwise spend a single-use token.
pub(crate) fn retry_after(headers: &reqwest::header::HeaderMap) -> std::time::Duration {
    let max = std::time::Duration::from_secs(5 * 60);
    let raw = headers.get(reqwest::header::RETRY_AFTER).and_then(|v| v.to_str().ok());
    let parsed = raw.and_then(|s| {
        let s = s.trim();
        if let Ok(secs) = s.parse::<u64>() {
            return Some(std::time::Duration::from_secs(secs));
        }
        // HTTP-date: `Retry-After` in that form is a wall-clock time.
        chrono::DateTime::parse_from_rfc2822(s)
            .ok()
            .and_then(|when| (when.with_timezone(&chrono::Utc) - chrono::Utc::now()).to_std().ok())
    });
    parsed.unwrap_or(max).clamp(std::time::Duration::from_secs(5), max)
}

/// A successful token body, parsed twice: into [`TokenResponse`] for the common fields and into
/// the whole [`serde_json::Value`] for the provider-specific ones.
///
/// A `Value` that fails to parse, or one missing `access_token`, is the JSON error rather than a
/// half-filled response — the caller must never store a record with no token in it.
pub(crate) fn split_token_body(
    status: reqwest::StatusCode,
    bytes: &[u8],
) -> Result<(TokenResponse, serde_json::Value), AuthError> {
    if !status.is_success() {
        return Err(token_error(status, bytes));
    }
    let value: serde_json::Value = serde_json::from_slice(bytes)?;
    let resp = serde_json::from_value(value.clone())?;
    Ok((resp, value))
}

/// Error codes that mean the refresh token itself is dead, so only a new sign-in helps.
///
/// `invalid_grant` is the RFC's word for it. OpenAI says the same thing in its own codes: a
/// rotated token presented again, an expired or invalidated one, a revoked family. Deliberately
/// absent: `invalid_client` and a bare 401, which say the *client* was refused — a registration
/// or proxy problem that a new sign-in would not fix, and `ReauthRequired` deletes the account.
const DEAD_REFRESH_CODES: &[&str] = &[
    "invalid_grant",
    "refresh_token_reused",
    "refresh_token_expired",
    "refresh_token_invalidated",
    "refresh_token_revoked",
    "token_revoked",
];

/// Map a non-success token response to the error the UI shows. Shared by [`token_call`] and
/// [`split_token_body`] so both paths agree on what a dead refresh token looks like.
pub(crate) fn token_error(status: reqwest::StatusCode, bytes: &[u8]) -> AuthError {
    match parse_token_error(bytes) {
        Some((code, _)) if DEAD_REFRESH_CODES.contains(&code.as_str()) => AuthError::ReauthRequired,
        Some((code, desc)) => AuthError::TokenEndpoint(describe_token_error(&code, desc.as_deref())),
        None => AuthError::TokenEndpoint(format!("HTTP {status}")),
    }
}

/// OAuth error bodies come in three shapes: the RFC 6749 one,
/// `{"error":"invalid_grant","error_description":…}`, Anthropic's nested
/// `{"error":{"type":"invalid_grant","message":…}}`, and OpenAI's nested one, whose `type` is a
/// broad class (`invalid_request_error`) and whose `code` says what actually happened
/// (`refresh_token_reused`). Reading only the first is why a dead Claude refresh token would
/// surface as "HTTP 400"; reading `type` before `code` is why a dead Codex one would.
///
/// `None` when the body is none of them: the caller then reports the HTTP status, which is all
/// the body actually told us.
fn parse_token_error(bytes: &[u8]) -> Option<(String, Option<String>)> {
    let v: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let err = v.get("error")?;
    if let Some(s) = err.as_str() {
        let desc = v.get("error_description").and_then(|d| d.as_str());
        return Some((s.to_owned(), desc.map(str::to_owned)));
    }
    let named = |key: &str| err.get(key).and_then(|t| t.as_str()).map(str::trim).filter(|s| !s.is_empty());
    let code = named("code").or_else(|| named("type"))?;
    let desc = err
        .get("message")
        .and_then(|m| m.as_str())
        .or_else(|| v.get("error_description").and_then(|d| d.as_str()));
    Some((code.to_owned(), desc.map(str::to_owned)))
}

/// `"{code} {description}"`, trimmed and cut to a size a row can show.
///
/// The description is provider-controlled text that lands in the Account tab, so control and
/// bidi characters are dropped here rather than trusted to the UI, and the length is bounded so a
/// gateway's HTML error page cannot become the row.
fn describe_token_error(code: &str, desc: Option<&str>) -> String {
    let scrubbed: String = desc
        .unwrap_or_default()
        .chars()
        .filter(|c| {
            let u = *c as u32;
            !(u < 0x20 || (0x7f..=0x9f).contains(&u) || (0x202a..=0x202e).contains(&u) || (0x2066..=0x2069).contains(&u))
        })
        .collect();
    let text = format!("{code} {}", scrubbed.trim());
    let text = text.trim();
    match text.char_indices().nth(200) {
        Some((i, _)) => format!("{}…", &text[..i]),
        None => text.to_owned(),
    }
}

async fn post_token(
    http: &reqwest::Client,
    url: &str,
    form: &[(&str, &str)],
) -> Result<TokenResponse, AuthError> {
    post_token_form(http, url, form).await
}

/// Fetch the account email (PROTOCOL 1.7). Failures are logged without the token and ignored.
pub async fn fetch_user_email(
    http: &reqwest::Client,
    endpoints: &OAuthEndpoints,
    access_token: &str,
) -> Option<String> {
    #[derive(Deserialize)]
    struct UserInfo {
        email: Option<String>,
    }
    match http.get(&endpoints.userinfo_url).bearer_auth(access_token).send().await {
        Ok(r) if r.status().is_success() => r.json::<UserInfo>().await.ok().and_then(|u| u.email),
        Ok(r) => {
            tracing::warn!("userinfo returned HTTP {}", r.status());
            None
        }
        Err(e) => {
            tracing::warn!("userinfo request failed: {}", e.without_url());
            None
        }
    }
}

/// `GET {tokeninfo}?access_token=..` (PROTOCOL 1.5 startup check). `Ok(false)` on a 4xx
/// (revoked or invalid), `Err` on transport or 5xx errors. The URL is never logged.
pub async fn token_is_valid(
    http: &reqwest::Client,
    endpoints: &OAuthEndpoints,
    access_token: &str,
) -> Result<bool, AuthError> {
    let resp = http
        .get(&endpoints.tokeninfo_url)
        .query(&[("access_token", access_token)])
        .send()
        .await?;
    let status = resp.status();
    if status.is_success() {
        Ok(true)
    } else if status.is_client_error() {
        Ok(false)
    } else {
        Err(AuthError::Http(format!("tokeninfo returned HTTP {status}")))
    }
}

/// Best-effort `POST {revoke}` with form `token=..` (PROTOCOL 1.8). Failures are only logged.
pub async fn revoke_token(http: &reqwest::Client, endpoints: &OAuthEndpoints, token: &str) {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("token", token)
        .finish();
    let r = http
        .post(&endpoints.revoke_url)
        .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await;
    match r {
        Ok(r) if r.status().is_success() => {}
        Ok(r) => tracing::warn!("token revoke returned HTTP {}", r.status()),
        Err(e) => tracing::warn!("token revoke failed: {}", e.without_url()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_url_has_pkce_and_params() {
        let p = Pkce::generate();
        let u = build_auth_url(&OAuthEndpoints::google(), "cid", "http://127.0.0.1:1/oauth2callback", "st", &p, false)
            .unwrap();
        let q: std::collections::HashMap<_, _> = u.query_pairs().into_owned().collect();
        assert_eq!(q["client_id"], "cid");
        assert_eq!(q["response_type"], "code");
        assert_eq!(q["access_type"], "offline");
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["code_challenge"], p.challenge);
        assert_eq!(q["state"], "st");
        assert_eq!(q["scope"], SCOPES.join(" "));
        assert!(!q.contains_key("prompt"));
        let u2 = build_auth_url(&OAuthEndpoints::google(), "cid", "r", "st", &p, true).unwrap();
        assert!(u2.query_pairs().any(|(k, v)| k == "prompt" && v == "consent"));
    }

    #[test]
    fn pkce_challenge_is_s256_of_verifier() {
        let p = Pkce::generate();
        let expect = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(p.verifier.as_bytes()));
        assert_eq!(p.challenge, expect);
    }

    #[test]
    fn constant_time_eq_works() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "ab"));
    }

    #[test]
    fn token_response_debug_is_redacted() {
        let t: TokenResponse =
            serde_json::from_str(r#"{"access_token":"AT-SECRET","refresh_token":"RT-SECRET","expires_in":10}"#).unwrap();
        let d = format!("{t:?}");
        assert!(!d.contains("AT-SECRET") && !d.contains("RT-SECRET"));
    }

    async fn raw_get(port: u16, target: &str) -> String {
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s.write_all(format!("GET {target} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n").as_bytes()).await.unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).await.unwrap();
        out
    }

    /// Every wait for a callback or a mock server is bounded: a listener that never sees its
    /// request must fail the test in seconds, not hang the suite.
    async fn bounded<T>(f: impl std::future::Future<Output = T>) -> T {
        tokio::time::timeout(std::time::Duration::from_secs(20), f).await.expect("finished in time")
    }

    #[tokio::test]
    async fn loopback_server_handles_callbacks() {
        let server = LoopbackServer::bind(None).await.unwrap();
        assert_ne!(server.port(), 0);
        assert!(server.redirect_uri().starts_with("http://127.0.0.1:"));
        assert!(server.redirect_uri().ends_with("/oauth2callback"));
        let port = server.port();

        let c = tokio::spawn(async move { raw_get(port, "/oauth2callback?state=s1&code=c1").await });
        assert_eq!(bounded(server.accept_one("s1")).await.unwrap(), CallbackOutcome::Code("c1".into()));
        let resp = bounded(c).await.unwrap();
        assert!(resp.starts_with("HTTP/1.1 301"));
        assert!(resp.contains(SUCCESS_URL));

        let c = tokio::spawn(async move { raw_get(port, "/oauth2callback?state=zz&code=c1").await });
        assert_eq!(bounded(server.accept_one("s1")).await.unwrap(), CallbackOutcome::StateMismatch);
        assert!(bounded(c).await.unwrap().contains("State mismatch. Possible CSRF attack"));

        let c = tokio::spawn(async move { raw_get(port, "/favicon.ico").await });
        assert_eq!(bounded(server.accept_one("s1")).await.unwrap(), CallbackOutcome::WrongPath);
        assert!(bounded(c).await.unwrap().starts_with("HTTP/1.1 404"));
    }

    #[tokio::test]
    async fn junk_connections_do_not_end_the_flow() {
        let server = LoopbackServer::bind(None).await.unwrap();
        let port = server.port();
        let client = tokio::spawn(async move {
            // Preconnect that sends nothing and closes.
            drop(tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap());
            raw_get(port, "/favicon.ico").await;
            raw_get(port, "/oauth2callback?state=forged&code=evil").await;
            raw_get(port, "/oauth2callbackX?state=s1&code=c0").await;
            raw_get(port, "/oauth2callback?state=s1&code=real").await
        });
        let out = tokio::time::timeout(std::time::Duration::from_secs(10), server.wait_for_callback("s1")).await.unwrap().unwrap();
        assert_eq!(out, CallbackOutcome::Code("real".into()));
        assert!(bounded(client).await.unwrap().contains(SUCCESS_URL));
    }

    #[tokio::test]
    async fn token_errors_are_mapped() {
        use axum::{http::StatusCode, routing::post, Router};
        let app = Router::new().route(
            "/token",
            post(|body: String| async move {
                let err = if body.contains("refresh_token=revoked") {
                    serde_json::json!({"error":"invalid_grant","error_description":"Token has been expired or revoked."})
                } else {
                    serde_json::json!({"error":"invalid_client"})
                };
                (StatusCode::BAD_REQUEST, axum::Json(err))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let config = AuthConfig {
            client_id: "test-client".into(),
            client_secret: "test-secret".into(),
            endpoints: OAuthEndpoints::with_base(&format!("http://{addr}")),
            callback_port: None,
        };
        let http = reqwest::Client::new();
        assert!(matches!(refresh_token(&http, &config, "revoked").await, Err(AuthError::ReauthRequired)));
        match refresh_token(&http, &config, "other").await {
            Err(AuthError::TokenEndpoint(m)) => assert_eq!(m, "invalid_client"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn a_dead_refresh_token_is_recognised_in_every_providers_spelling() {
        let status = reqwest::StatusCode::BAD_REQUEST;
        let dead = [
            // RFC 6749 (Google).
            serde_json::json!({"error": "invalid_grant", "error_description": "Token has been expired or revoked."}),
            // Anthropic.
            serde_json::json!({"error": {"type": "invalid_grant", "message": "Refresh token not found or invalid"}}),
            // OpenAI: the class in `type`, the reason in `code`.
            serde_json::json!({"error": {"message": "already used", "type": "invalid_request_error", "param": null, "code": "refresh_token_reused"}}),
            serde_json::json!({"error": {"type": "invalid_request_error", "code": "refresh_token_expired"}}),
            serde_json::json!({"error": {"type": "invalid_request_error", "code": "refresh_token_invalidated"}}),
            serde_json::json!({"error": {"code": "token_revoked"}}),
        ];
        for body in dead {
            let bytes = serde_json::to_vec(&body).unwrap();
            assert!(matches!(token_error(status, &bytes), AuthError::ReauthRequired), "{body}");
        }
        // A refused client is not a dead token: the account must survive it.
        for body in [
            serde_json::json!({"error": "invalid_client"}),
            serde_json::json!({"error": {"type": "invalid_request_error", "code": "invalid_client", "message": "Unknown client"}}),
            serde_json::json!({"error": {"type": "invalid_request_error", "code": "", "message": "x"}}),
        ] {
            let bytes = serde_json::to_vec(&body).unwrap();
            assert!(matches!(token_error(status, &bytes), AuthError::TokenEndpoint(_)), "{body}");
        }
        assert!(matches!(token_error(reqwest::StatusCode::UNAUTHORIZED, b"<html>"), AuthError::TokenEndpoint(m) if m.contains("401")));
    }

    #[test]
    fn retry_after_is_clamped_to_five_seconds_or_five_minutes() {
        use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};
        let with = |v: &str| {
            let mut h = HeaderMap::new();
            h.insert(RETRY_AFTER, HeaderValue::from_str(v).unwrap());
            retry_after(&h)
        };
        assert_eq!(with("1"), std::time::Duration::from_secs(5), "below the floor");
        assert_eq!(with("600"), std::time::Duration::from_secs(300), "above the ceiling");
        assert_eq!(with("30"), std::time::Duration::from_secs(30));
        // A missing or unreadable header waits the maximum: waiting longer is always safe when
        // the retry might otherwise spend a single-use token.
        assert_eq!(retry_after(&HeaderMap::new()), std::time::Duration::from_secs(300));
        assert_eq!(with("soon"), std::time::Duration::from_secs(300));
    }

    #[tokio::test]
    async fn a_json_token_error_body_is_read_like_a_form_one() {
        // Anthropic's nested shape: without this, a dead refresh token reads as "HTTP 400" and
        // the account is never cleaned up.
        use axum::{http::StatusCode, routing::post, Router};
        let app = Router::new().route(
            "/token",
            post(|| async {
                (
                    StatusCode::BAD_REQUEST,
                    axum::Json(serde_json::json!({"error": {"type": "invalid_grant", "message": "expired"}})),
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let err = bounded(post_token_json(&reqwest::Client::new(), &format!("http://{addr}/token"), &serde_json::json!({})))
            .await
            .unwrap_err();
        assert!(matches!(err, AuthError::ReauthRequired), "{err:?}");
    }

    #[test]
    fn a_pasted_link_is_read_in_both_spellings() {
        // The full URL the browser landed on.
        assert_eq!(
            parse_pasted_link("http://localhost:54545/callback?code=abc&state=xyz"),
            PastedLink::Code { code: "abc".into(), state: Some("xyz".into()) }
        );
        // Claude's `code#state`: the fragment is inside the code parameter.
        assert_eq!(
            parse_pasted_link("http://localhost:54545/callback?code=abc#xyz"),
            PastedLink::Code { code: "abc".into(), state: Some("xyz".into()) }
        );
        // A fragment wins over a query state: that is where the provider put the real one.
        assert_eq!(
            parse_pasted_link("http://localhost:54545/callback?code=abc&state=query#frag"),
            PastedLink::Code { code: "abc".into(), state: Some("frag".into()) }
        );
        // The bare query, which some failure pages leave the user with.
        assert_eq!(
            parse_pasted_link("code=abc&state=xyz"),
            PastedLink::Code { code: "abc".into(), state: Some("xyz".into()) }
        );
        assert_eq!(
            parse_pasted_link("http://localhost/cb?error=access_denied&error_description=nope"),
            PastedLink::Denied("access_denied nope".into())
        );
        for bad in ["", "   ", "hello", "code=", "?state=only"] {
            assert_eq!(parse_pasted_link(bad), PastedLink::Unreadable, "{bad:?}");
        }
    }

    #[test]
    fn pkce_shape() {
        let p = Pkce::generate();
        assert_eq!(p.verifier.len(), 43);
        assert_eq!(p.challenge.len(), 43);
    }

    #[tokio::test]
    async fn the_advertised_redirect_uses_localhost_and_the_spec_path() {
        // What the external providers are registered with: `localhost`, not the bind address, and
        // exactly the path the spec names. `redirect_uri()` keeps its 127.0.0.1 spelling for the
        // Google flow.
        let server = LoopbackServer::bind_pair(None, "/callback").await.unwrap();
        assert!(server.redirect_uri_host().starts_with("http://localhost:"), "{}", server.redirect_uri_host());
        assert!(server.redirect_uri_host().ends_with("/callback"));
        assert!(server.redirect_uri().starts_with("http://127.0.0.1:"));
        assert!(server.redirect_uri().ends_with("/callback"));

        // The callback on that path is accepted, and a sibling path is not.
        let port = server.port();
        let c = tokio::spawn(async move { raw_get(port, "/callback?state=s1&code=c1").await });
        assert_eq!(bounded(server.accept_one("s1")).await.unwrap(), CallbackOutcome::Code("c1".into()));
        assert!(bounded(c).await.unwrap().contains(SUCCESS_URL));
        let c = tokio::spawn(async move { raw_get(port, "/oauth2callback?state=s1&code=c1").await });
        assert_eq!(bounded(server.accept_one("s1")).await.unwrap(), CallbackOutcome::WrongPath);
        drop(c);
    }

    #[tokio::test]
    async fn a_localhost_redirect_is_refused_when_another_program_holds_ipv6_loopback() {
        // Someone else's listener on `[::1]:port`. Skipped on a host with no IPv6 loopback, where
        // no browser can pick `::1` and there is nothing to refuse.
        let Ok(squatter) = TcpListener::bind(("::1", 0)).await else { return };
        let port = squatter.local_addr().unwrap().port();
        if TcpListener::bind(("127.0.0.1", port)).await.is_err() {
            return; // the IPv4 side is taken too; that path has its own test
        }

        // The external providers advertise `localhost`: the browser could hand the code to the
        // squatter, so the server must not start.
        let err = LoopbackServer::bind_pair(Some(port), "/callback").await.err().expect("refused");
        assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse);
        assert!(err.to_string().contains("paste the redirect link"), "{err}");
        assert!(err.to_string().contains(&port.to_string()), "{err}");

        // The Google flow advertises the literal 127.0.0.1, which no other program holds.
        let google = LoopbackServer::bind(Some(port)).await.expect("IPv4 is enough for 127.0.0.1");
        assert_eq!(google.port(), port);
        assert!(google.v6.is_none());
        drop(squatter);
    }

    #[test]
    fn state_is_64_hex() {
        let s = generate_state();
        assert_eq!(s.len(), 64);
        assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn callback_classification() {
        assert_eq!(
            parse_callback("/oauth2callback?state=abc&code=xyz", "abc"),
            CallbackOutcome::Code("xyz".into())
        );
        assert_eq!(parse_callback("/oauth2callback?state=bad&code=xyz", "abc"), CallbackOutcome::StateMismatch);
        assert_eq!(parse_callback("/other", "abc"), CallbackOutcome::WrongPath);
        assert!(matches!(parse_callback("/oauth2callback?state=abc&error=access_denied", "abc"), CallbackOutcome::Denied(_)));
        assert_eq!(parse_callback("/oauth2callback?error=access_denied", "abc"), CallbackOutcome::StateMismatch);
        assert_eq!(parse_callback("/x/oauth2callback?state=abc&code=1", "abc"), CallbackOutcome::WrongPath);
        assert_eq!(parse_callback("/oauth2callback?state=abc", "abc"), CallbackOutcome::NoCode);
    }

    #[tokio::test]
    async fn exchange_against_mock_token_endpoint() {
        use axum::{routing::post, Router};
        let app = Router::new().route(
            "/token",
            post(|body: String| async move {
                assert!(body.contains("grant_type=authorization_code"));
                assert!(body.contains("code_verifier=v"));
                axum::Json(serde_json::json!({"access_token":"mock-at","expires_in":3599,"refresh_token":"mock-rt"}))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let config = AuthConfig {
            client_id: "test-client".into(),
            client_secret: "test-secret".into(),
            endpoints: OAuthEndpoints::with_base(&format!("http://{addr}")),
            callback_port: None,
        };
        let t = exchange_code(&reqwest::Client::new(), &config, "c", "http://127.0.0.1/cb", Some("v"))
            .await
            .unwrap();
        assert_eq!(t.expires_in, Some(3599));
        assert!(t.refresh_token.is_some());
    }
}
