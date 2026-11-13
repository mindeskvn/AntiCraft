//! MCP Streamable HTTP transport (2025-03-26).
//!
//! One POST per JSON-RPC message to the configured URL. The response may be either a single
//! JSON body or a Server-Sent Events stream; the client branches on `Content-Type`. A
//! dedicated `reqwest::Client` is built per server because the shared app client has a fixed
//! 180 s read timeout that kills any `tools/call` that runs longer, and because the MCP
//! `User-Agent` must differ from the Code Assist identity.
//!
//! Security notes that shaped the implementation:
//! - Redirects are refused outright (`Policy::none()`). The default policy follows 10 hops
//!   and rewrites POST to GET on 301/302/303, which would silently turn a `tools/call` into
//!   a GET and could replay `MCP-Session-Id` and `Authorization` to another origin.
//! - HTTPS is required unless the host is loopback. A workspace-supplied `mcp_config.json`
//!   is attacker-controlled content in a cloned repo; plain HTTP lets a LAN adversary read
//!   every argument and response.
//! - The server-initiated request path answers with -32601 so the server does not block
//!   waiting for a reply that never arrives.

use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, CONTENT_TYPE};
use reqwest::redirect;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use super::config::Timeouts;
use super::jsonrpc::{
    error_reply, notification, request, split_payload, IdGen, Message, METHOD_NOT_FOUND,
};
use super::{McpError, PROTOCOL_VERSION};

/// Maximum body size accepted from the server, in bytes.
pub const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;
/// Largest description body kept when the server returns a non-2xx status, in bytes.
const MAX_ERROR_BODY_BYTES: usize = 16 * 1024;

/// One live HTTP MCP connection. The `reqwest::Client` is held here because it carries the
/// session id and the server-specific headers.
pub struct HttpTransport {
    client: reqwest::Client,
    url: String,
    ids: IdGen,
    timeouts: Timeouts,
    /// `None` before the first successful `initialize`.
    session_id: tokio::sync::Mutex<Option<String>>,
}

impl std::fmt::Debug for HttpTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpTransport").field("url", &self.url).finish()
    }
}

impl HttpTransport {
    /// Build a client for `url`, complete the handshake, and return the transport with the
    /// server's tool list.
    pub async fn connect(
        url: &str,
        headers: &[(String, String)],
        timeouts: Timeouts,
        cancel: &CancellationToken,
    ) -> Result<(Self, Vec<Value>), McpError> {
        validate_url(url)?;
        let client = build_client(headers)?;
        let transport = Self {
            client,
            url: url.to_owned(),
            ids: IdGen::new(),
            timeouts,
            session_id: tokio::sync::Mutex::new(None),
        };
        let tools = transport.handshake(cancel).await?;
        Ok((transport, tools))
    }

    /// `initialize`, `notifications/initialized`, then paged `tools/list`.
    async fn handshake(&self, cancel: &CancellationToken) -> Result<Vec<Value>, McpError> {
        let timeout = self.timeouts.init();
        let id = self.ids.next();
        let msg = request(
            id,
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": { "roots": { "listChanged": false } },
                "clientInfo": { "name": "anticraft-native", "version": env!("CARGO_PKG_VERSION") }
            }),
        );
        let (result, session) = self.post_and_wait(id, msg, timeout, cancel).await?;
        let caps = result.get("capabilities");
        if caps.and_then(|c| c.get("tools")).is_none() {
            let version = result.get("protocolVersion").and_then(Value::as_str).unwrap_or("unknown");
            return Err(McpError::Protocol(format!(
                "the server answers version {version} without declaring the tools capability"
            )));
        }
        if let Some(sid) = session {
            *self.session_id.lock().await = Some(sid);
        }
        // `notifications/initialized` is a fire-and-forget notification; 202 is the expected
        // response, but a failure here does not abort the session.
        let _ = self
            .post_raw(notification("notifications/initialized", json!({})), timeout, cancel)
            .await;
        self.list_tools(cancel).await
    }

    /// Page through `tools/list`.
    async fn list_tools(&self, cancel: &CancellationToken) -> Result<Vec<Value>, McpError> {
        let timeout = self.timeouts.init();
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..32 {
            let id = self.ids.next();
            let params = match &cursor {
                Some(c) => json!({ "cursor": c }),
                None => json!({}),
            };
            let msg = request(id, "tools/list", params);
            let (page, _) = self.post_and_wait(id, msg, timeout, cancel).await?;
            if let Some(items) = page.get("tools").and_then(Value::as_array) {
                out.extend(items.iter().cloned());
            }
            if out.len() >= super::MAX_TOOLS_PER_SERVER {
                out.truncate(super::MAX_TOOLS_PER_SERVER);
                break;
            }
            cursor = page
                .get("nextCursor")
                .and_then(Value::as_str)
                .filter(|c| !c.is_empty())
                .map(str::to_owned);
            if cursor.is_none() {
                break;
            }
        }
        Ok(out)
    }

    /// Execute one tool call and return the raw `result` object.
    pub async fn call(&self, tool: &str, args: Value, cancel: &CancellationToken) -> Result<Value, McpError> {
        let id = self.ids.next();
        let msg = request(id, "tools/call", json!({ "name": tool, "arguments": args }));
        let (result, _) = self
            .post_and_wait(id, msg, self.timeouts.call(), cancel)
            .await?;
        Ok(result)
    }

    /// POST `message`, drive the response body until our `id` is matched, and return the
    /// `result` object plus any `MCP-Session-Id` header.
    async fn post_and_wait(
        &self,
        id: u64,
        message: Value,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<(Value, Option<String>), McpError> {
        // The deadline covers the whole exchange, body included. Bounding only the header
        // send would let a server that answers 200 and then dribbles its body hold a caller
        // forever: reqwest's response future resolves as soon as the headers arrive.
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(McpError::Cancelled),
            result = tokio::time::timeout(timeout, self.post_and_wait_inner(id, message, timeout)) => {
                result.map_err(|_| McpError::Timeout)?
            }
        }
    }

    /// The exchange itself, under the caller's overall deadline.
    async fn post_and_wait_inner(
        &self,
        id: u64,
        message: Value,
        timeout: Duration,
    ) -> Result<(Value, Option<String>), McpError> {
        let (response, session_id) = self.send_request(&message).await?;
        let ct = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned();
        let status = response.status().as_u16();
        // 202 means the server accepted a notification; a request must not get 202.
        if status == 202 {
            return Err(McpError::Protocol(format!(
                "the server answered a request (id {id}) with 202 Accepted; expected a result"
            )));
        }
        if status >= 400 {
            let body = read_limited_body(response, MAX_ERROR_BODY_BYTES).await?;
            let text = String::from_utf8_lossy(&body).chars().take(500).collect::<String>();
            return Err(McpError::Protocol(format!("HTTP {status}: {text}")));
        }
        if ct.contains("text/event-stream") {
            let result = self.read_sse_until_id(response, id, timeout).await?;
            return Ok((result, session_id));
        }
        // application/json or anything else: treat as a single message.
        let body = read_limited_body(response, MAX_BODY_BYTES).await?;
        let value: Value = serde_json::from_slice(&body).map_err(|e| McpError::Decode(e.to_string()))?;
        for msg in split_payload(&value) {
            if let Message::Response { id: rid, result } = msg {
                if rid.as_request_id() == Some(id) {
                    return match result {
                        Ok(v) => Ok((v, session_id)),
                        Err(err) => Err(McpError::Protocol(format!("{}: {}", err.code, err.message))),
                    };
                }
            }
        }
        Err(McpError::Protocol(format!("the server response for id {id} did not contain a matching result")))
    }

    /// Drive an SSE body until we see a response for `expected_id`, then stop. The server may
    /// keep the stream open for unsolicited messages; we return as soon as our id is matched.
    async fn read_sse_until_id(
        &self,
        response: reqwest::Response,
        expected_id: u64,
        outer_timeout: Duration,
    ) -> Result<Value, McpError> {
        use futures::StreamExt;
        let mut stream = response.bytes_stream();
        let mut parser = crate::api::sse::SseParser::new();
        let idle_timeout = self.timeouts.idle().min(outer_timeout);
        // The same body cap as a unary response. `SseParser` bounds a *single* event at 16 MiB,
        // which is four times what this client is willing to hold, so the running total is
        // counted here: a stream of small events must not be able to grow without bound either.
        let mut seen: usize = 0;
        loop {
            let chunk = {
                let sleep = tokio::time::sleep(idle_timeout);
                tokio::pin!(sleep);
                tokio::select! {
                    biased;
                    _ = &mut sleep => return Err(McpError::Timeout),
                    chunk = stream.next() => chunk,
                }
            };
            let bytes = match chunk {
                Some(Ok(b)) => b,
                Some(Err(e)) => return Err(McpError::Dead(format!("stream error: {e}"))),
                None => {
                    // EOF before we saw our id: the server closed the stream early.
                    if let Some(last) = parser.finish() {
                        if let Ok(v) = serde_json::from_str::<Value>(&last) {
                            if let Some(result) = extract_id_result(&v, expected_id) {
                                return result;
                            }
                        }
                    }
                    return Err(McpError::Protocol(format!("the SSE stream closed before answering id {expected_id}")));
                }
            };
            let events = parser.push(&bytes).map_err(|e| McpError::Decode(e.to_string()))?;
            seen = seen.saturating_add(bytes.len());
            if seen > MAX_BODY_BYTES {
                return Err(McpError::Protocol(format!(
                    "the server stream exceeded the {MAX_BODY_BYTES} byte limit"
                )));
            }
            for payload in events {
                let value: Value = match serde_json::from_str(&payload) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                for msg in split_payload(&value) {
                    match msg {
                        Message::Response { id, result } if id.as_request_id() == Some(expected_id) => {
                            return match result {
                                Ok(v) => Ok(v),
                                Err(err) => Err(McpError::Protocol(format!("{}: {}", err.code, err.message))),
                            };
                        }
                        Message::ServerRequest { id, method, .. } => {
                            // Conforming servers declare no capability we should serve, so every
                            // server request is unexpected. We answer rather than ignoring it so
                            // the server does not block waiting for a reply.
                            tracing::debug!("mcp/http: refusing server request `{method}`");
                            let reply = error_reply(&id, METHOD_NOT_FOUND, "method not supported by this client");
                            let _ = self.post_raw(reply, Duration::from_secs(5), &CancellationToken::new()).await;
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    /// POST one message frame and return the raw response. Does not parse the body.
    async fn send_request(
        &self,
        message: &Value,
    ) -> Result<(reqwest::Response, Option<String>), McpError> {
        let body = serde_json::to_vec(message).map_err(|e| McpError::Protocol(e.to_string()))?;
        let mut req = self
            .client
            .post(&self.url)
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json, text/event-stream")
            .body(body);
        let sid = self.session_id.lock().await.clone();
        if let Some(id) = sid {
            req = req.header("mcp-session-id", &id).header("mcp-protocol-version", PROTOCOL_VERSION);
        }
        let response = req.send().await.map_err(|e| McpError::Dead(format!("HTTP request failed: {e}")))?;
        let status = response.status().as_u16();
        if (300..400).contains(&status) {
            // Any redirect means the server is not where it was configured. Never follow:
            // a redirect can replay Authorization and MCP-Session-Id to another origin.
            return Err(McpError::Protocol(format!("the server returned a redirect (HTTP {status}); refusing to follow")));
        }
        let session_id = response
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        Ok((response, session_id))
    }

    /// POST a notification (no response id to wait for). Errors are intentionally ignored by
    /// most callers — a notification that fails does not abort a session.
    async fn post_raw(
        &self,
        message: Value,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<(), McpError> {
        let body = serde_json::to_vec(&message).map_err(|e| McpError::Protocol(e.to_string()))?;
        let sid = self.session_id.lock().await.clone();
        let mut req = self
            .client
            .post(&self.url)
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json, text/event-stream")
            .body(body);
        if let Some(id) = sid {
            req = req.header("mcp-session-id", &id).header("mcp-protocol-version", PROTOCOL_VERSION);
        }
        let send_fut = req.send();
        let response = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(McpError::Cancelled),
            _ = tokio::time::sleep(timeout) => return Err(McpError::Timeout),
            res = send_fut => res.map_err(|e| McpError::Dead(e.to_string()))?,
        };
        let status = response.status().as_u16();
        if status >= 400 {
            return Err(McpError::Protocol(format!("HTTP {status}")));
        }
        Ok(())
    }
}

/// Parse the URL and reject non-HTTPS except for loopback.
///
/// HTTPS is required so a workspace-sourced config in a cloned repo cannot redirect the agent
/// to a server the repo author controls over plain HTTP.
fn validate_url(raw: &str) -> Result<(), McpError> {
    let parsed = url::Url::parse(raw).map_err(|e| McpError::Protocol(format!("invalid URL: {e}")))?;
    match parsed.scheme() {
        "https" => {}
        "http" => {
            let host = parsed.host_str().unwrap_or("");
            let loopback = host == "localhost"
                || host == "127.0.0.1"
                || host == "[::1]"
                || host == "::1";
            if !loopback {
                return Err(McpError::Protocol(
                    "plain HTTP is only allowed for loopback addresses; use HTTPS for remote servers".to_owned(),
                ));
            }
        }
        other => return Err(McpError::Protocol(format!("unsupported URL scheme `{other}`"))),
    }
    // Reject userinfo: credentials in the URL travel in cleartext and would land in logs.
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(McpError::Protocol(
            "URL must not contain userinfo; use headers for credentials".to_owned(),
        ));
    }
    Ok(())
}

/// A dedicated HTTP client for one MCP server. Must never be the app's shared client:
/// the shared client has a 180 s read timeout and Code Assist `User-Agent`.
fn build_client(extra_headers: &[(String, String)]) -> Result<reqwest::Client, McpError> {
    let mut default_headers = HeaderMap::new();
    for (key, value) in extra_headers {
        let name = HeaderName::from_bytes(key.as_bytes())
            .map_err(|e| McpError::Protocol(format!("invalid header name `{key}`: {e}")))?;
        let val = HeaderValue::from_str(value)
            .map_err(|_| McpError::Protocol(format!("invalid header value for `{key}`")))?;
        default_headers.insert(name, val);
    }
    reqwest::Client::builder()
        .redirect(redirect::Policy::none())
        .default_headers(default_headers)
        .user_agent(concat!("anticraft-native/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| McpError::Protocol(format!("could not build HTTP client: {e}")))
}

/// Read up to `cap` bytes from the response body, fail on overflow.
async fn read_limited_body(response: reqwest::Response, cap: usize) -> Result<Vec<u8>, McpError> {
    use futures::StreamExt;
    let mut out = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| McpError::Dead(e.to_string()))?;
        if out.len().saturating_add(chunk.len()) > cap {
            return Err(McpError::Protocol(format!("the server response exceeds the {cap} byte limit")));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// If `v` contains a response matching `expected_id`, return its result.
fn extract_id_result(v: &Value, expected_id: u64) -> Option<Result<Value, McpError>> {
    for msg in split_payload(v) {
        if let Message::Response { id, result } = msg {
            if id.as_request_id() == Some(expected_id) {
                return Some(match result {
                    Ok(v) => Ok(v),
                    Err(err) => Err(McpError::Protocol(format!("{}: {}", err.code, err.message))),
                });
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::post;
    use axum::{http::HeaderMap, Router};
    use serde_json::json;
    use std::sync::{Arc, Mutex};
    use tokio::net::TcpListener;

    /// Bind an ephemeral port and return both the router handle and the base URL.
    async fn serve(router: Router) -> (tokio::task::JoinHandle<()>, String) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        (handle, format!("http://127.0.0.1:{port}"))
    }

    /// A minimal scripted server: answers each POST with the next queued body, verbatim.
    ///
    /// Used where the test is about the HTTP layer rather than the protocol sequence, so the
    /// queued body can be a redirect, an error, or junk.
    #[derive(Clone)]
    struct Mock {
        queue: Arc<Mutex<Vec<(u16, Value)>>>,
    }

    impl Mock {
        fn new(responses: Vec<(u16, Value)>) -> Self {
            Self { queue: Arc::new(Mutex::new(responses)) }
        }

        fn router(self) -> Router {
            let m = self.clone();
            Router::new().route(
                "/",
                post(move |_headers: HeaderMap, _body: axum::body::Bytes| {
                    let m = m.clone();
                    async move {
                        let next = m.queue.lock().unwrap().remove(0);
                        axum::response::Response::builder()
                            .status(next.0)
                            .header("content-type", "application/json")
                            .body(axum::body::Body::from(serde_json::to_vec(&next.1).unwrap()))
                            .unwrap()
                    }
                }),
            )
        }
    }

    #[tokio::test]
    async fn https_url_is_required_for_non_loopback() {
        assert!(validate_url("http://example.com/mcp").is_err());
        assert!(validate_url("http://localhost/mcp").is_ok());
        assert!(validate_url("http://127.0.0.1/mcp").is_ok());
        assert!(validate_url("https://example.com/mcp").is_ok());
        assert!(validate_url("ws://example.com/mcp").is_err());
        assert!(validate_url("http://user:pass@localhost/mcp").is_err());
    }

    /// A server that answers each request with the result for its method, echoing the id.
    ///
    /// Hand-building canned bodies is brittle: the client's ids are its own business, and a
    /// test that hard-codes them is testing the sequence, not the transport.
    fn echo_router(tools: Vec<Value>) -> Router {
        Router::new().route(
            "/",
            post(move |_headers: HeaderMap, body: axum::body::Bytes| {
                let tools = tools.clone();
                async move {
                    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
                    let Some(id) = req.get("id").cloned() else {
                        // A notification: acknowledge it the way a real server does.
                        return axum::response::Response::builder()
                            .status(202)
                            .body(axum::body::Body::empty())
                            .unwrap();
                    };
                    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
                    let result = match method {
                        "initialize" => json!({
                            "protocolVersion": PROTOCOL_VERSION,
                            "capabilities": { "tools": {} },
                            "serverInfo": { "name": "mock", "version": "0" }
                        }),
                        "tools/list" => json!({ "tools": tools.clone() }),
                        other => json!({ "echo": other }),
                    };
                    let body = serde_json::to_vec(&json!({ "jsonrpc": "2.0", "id": id, "result": result })).unwrap();
                    axum::response::Response::builder()
                        .header("content-type", "application/json")
                        .body(axum::body::Body::from(body))
                        .unwrap()
                }
            }),
        )
    }

    #[tokio::test]
    async fn a_json_body_response_is_accepted() {
        let (_h, url) = serve(echo_router(vec![json!({
            "name": "ping",
            "description": "d",
            "inputSchema": { "type": "object" }
        })]))
        .await;
        let cancel = CancellationToken::new();
        let (transport, tools) = HttpTransport::connect(&url, &[], Timeouts::default(), &cancel).await.expect("connect");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "ping");
        let _ = transport;
    }

    #[tokio::test]
    async fn a_non_tools_capability_is_a_protocol_error() {
        let mock = Mock::new(vec![(
            200,
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": {
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "serverInfo": { "name": "mock", "version": "0" }
                }
            }),
        )]);
        let (_h, url) = serve(mock.router()).await;
        let err = HttpTransport::connect(&url, &[], Timeouts::default(), &CancellationToken::new()).await.expect_err("must fail");
        assert!(matches!(err, McpError::Protocol(_)), "{err:?}");
    }

    #[tokio::test]
    async fn a_redirect_is_refused() {
        let router = Router::new().route(
            "/",
            post(|| async {
                axum::response::Response::builder()
                    .status(302)
                    .header("Location", "http://evil.test/")
                    .body(axum::body::Body::empty())
                    .unwrap()
            }),
        );
        let (_h, url) = serve(router).await;
        let err = HttpTransport::connect(&url, &[], Timeouts::default(), &CancellationToken::new()).await.expect_err("redirect refused");
        let text = err.to_string();
        assert!(text.contains("redirect") || text.contains("302"), "{text}");
    }

    #[tokio::test]
    async fn a_202_to_a_request_is_a_protocol_error() {
        let router = Router::new().route(
            "/",
            post(|| async {
                axum::response::Response::builder()
                    .status(202)
                    .body(axum::body::Body::empty())
                    .unwrap()
            }),
        );
        let (_h, url) = serve(router).await;
        let err = HttpTransport::connect(&url, &[], Timeouts::default(), &CancellationToken::new()).await.expect_err("202 is an error");
        assert!(matches!(err, McpError::Protocol(_)), "{err:?}");
    }

    #[tokio::test]
    async fn a_4xx_error_is_surfaced_with_body_text() {
        let router = Router::new().route(
            "/",
            post(|| async {
                axum::response::Response::builder()
                    .status(401)
                    .body(axum::body::Body::from("Unauthorized"))
                    .unwrap()
            }),
        );
        let (_h, url) = serve(router).await;
        let err = HttpTransport::connect(&url, &[], Timeouts::default(), &CancellationToken::new()).await.expect_err("401 is an error");
        let text = err.to_string();
        assert!(text.contains("401"), "{text}");
    }

    #[tokio::test]
    async fn cancellation_returns_cancelled() {
        // A server that takes a long time to respond.
        let router = Router::new().route(
            "/",
            post(|| async {
                tokio::time::sleep(Duration::from_secs(60)).await;
                axum::response::Response::builder().body(axum::body::Body::empty()).unwrap()
            }),
        );
        let (_h, url) = serve(router).await;
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = HttpTransport::connect(&url, &[], Timeouts::default(), &cancel).await.expect_err("cancelled");
        assert!(matches!(err, McpError::Cancelled), "{err:?}");
    }

    #[tokio::test]
    async fn body_size_cap_is_enforced() {
        let big = "x".repeat(MAX_BODY_BYTES + 1);
        let router = Router::new().route(
            "/",
            post(move || {
                let big = big.clone();
                async move {
                    axum::response::Response::builder()
                        .header("content-type", "application/json")
                        .body(axum::body::Body::from(big))
                        .unwrap()
                }
            }),
        );
        let (_h, url) = serve(router).await;
        let err = HttpTransport::connect(&url, &[], Timeouts::default(), &CancellationToken::new()).await.expect_err("too large");
        assert!(matches!(err, McpError::Protocol(_)), "{err:?}");
    }

    #[tokio::test]
    async fn sse_response_drives_stream_until_id_matches() {
        // Every request is answered as SSE, with an unrelated notification first so the skip
        // path is exercised on the same stream as the match path.
        let router = Router::new().route(
            "/",
            post(move |_headers: HeaderMap, body: axum::body::Bytes| async move {
                let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
                let Some(id) = req.get("id").cloned() else {
                    return axum::response::Response::builder()
                        .status(202)
                        .body(axum::body::Body::empty())
                        .unwrap();
                };
                let result = match req.get("method").and_then(Value::as_str) {
                    Some("initialize") => json!({
                        "protocolVersion": PROTOCOL_VERSION,
                        "capabilities": { "tools": {} },
                        "serverInfo": { "name": "mock", "version": "0" }
                    }),
                    _ => json!({ "tools": [] }),
                };
                let note = json!({ "jsonrpc": "2.0", "method": "notifications/progress", "params": {} });
                let answer = json!({ "jsonrpc": "2.0", "id": id, "result": result });
                let body = format!("data: {note}\n\ndata: {answer}\n\n");
                axum::response::Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(axum::body::Body::from(body))
                    .unwrap()
            }),
        );
        let (_h, url) = serve(router).await;
        let (transport, tools) = HttpTransport::connect(&url, &[], Timeouts::default(), &CancellationToken::new())
            .await
            .expect("connect via SSE");
        assert!(tools.is_empty());
        let _ = transport;
    }

    #[tokio::test]
    async fn extra_headers_are_sent() {
        let received = Arc::new(Mutex::new(HeaderMap::new()));
        let captured = received.clone();
        let router = Router::new().route(
            "/",
            post(move |headers: HeaderMap, body: axum::body::Bytes| {
                let captured = captured.clone();
                async move {
                    *captured.lock().unwrap() = headers;
                    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
                    let Some(id) = req.get("id").cloned() else {
                        return axum::response::Response::builder()
                            .status(202)
                            .body(axum::body::Body::empty())
                            .unwrap();
                    };
                    let result = match req.get("method").and_then(Value::as_str) {
                        Some("initialize") => json!({
                            "protocolVersion": PROTOCOL_VERSION,
                            "capabilities": { "tools": {} },
                            "serverInfo": { "name": "mock", "version": "0" }
                        }),
                        _ => json!({ "tools": [] }),
                    };
                    let body = serde_json::to_vec(&json!({ "jsonrpc": "2.0", "id": id, "result": result })).unwrap();
                    axum::response::Response::builder()
                        .header("content-type", "application/json")
                        .body(axum::body::Body::from(body))
                        .unwrap()
                }
            }),
        );
        let (_h, url) = serve(router).await;
        let extra = vec![("X-Api-Key".to_owned(), "secret".to_owned())];
        let _ = HttpTransport::connect(&url, &extra, Timeouts::default(), &CancellationToken::new()).await;
        let h = received.lock().unwrap();
        let val = h.get("x-api-key").and_then(|v| v.to_str().ok()).unwrap_or("");
        assert_eq!(val, "secret");
    }
}
