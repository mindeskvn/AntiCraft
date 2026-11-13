//! MCP (Model Context Protocol) client: stdio and Streamable HTTP transports.
//!
//! MCP servers expose tools the model can call. This module owns the connection set ([`McpHub`])
//! and the two transports, plus the small pure pieces that both need: config parsing
//! ([`config`]), exposed naming ([`name`]), the shared wire types ([`jsonrpc`]) and schema
//! sanitizing ([`schema`]).
//!
//! **Why a hub rather than a `ToolRegistry` extension.** The registry in `agent::tools` is a
//! closed `match` over built-in names; MCP tools arrive at runtime from a process or a socket,
//! so they are dispatched before the registry, like skills and orchestration. The hub keeps an
//! immutable snapshot of everything discovered at connect time, which is what lets
//! [`McpHub::declarations`] and [`McpHub::lookup`] stay synchronous — the request builder in
//! `Agent` is a sync function and cannot await.
//!
//! **Trust boundary.** Everything a server sends is untrusted: names, descriptions, schemas and
//! results all pass through a sanitizer or a cap before they reach the model. A server that
//! fails to connect, hangs, or floods its pipes is disconnected or bounded; it never takes the
//! others down and never stops the app from starting.
//!
//! The protocol revision spoken here is [`PROTOCOL_VERSION`]; the handshake deliberately claims
//! only `roots` (and not even `listChanged`) so no server can ask this client to run a model,
//! read the filesystem, or prompt the user on its behalf.

pub mod config;
pub mod http;
pub mod jsonrpc;
pub mod name;
pub mod schema;
pub mod stdio;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::api::FunctionDeclaration;

pub use config::{parse_servers, McpServerConfig, McpTransport};
pub use name::{exposed_name, MAX_EXPOSED_BYTES};

/// MCP revision this client speaks. Sent verbatim in `initialize` and in the
/// `MCP-Protocol-Version` header after the handshake.
pub const PROTOCOL_VERSION: &str = "2025-03-26";
/// Most tools taken from one server. A server is not allowed to flood the request with
/// declarations; each one costs context on every turn.
pub const MAX_TOOLS_PER_SERVER: usize = 256;
/// Longest tool description exposed to the model, in bytes. MCP descriptions can be pages of
/// prose and are attacker-controlled text, so they are flattened and capped.
pub const MAX_DESCRIPTION_BYTES: usize = 1024;
/// Longest serialized argument object sent to one server, in bytes. Arguments are echoed into
/// the confirmation card and into the server's own log, so they are bounded like a tool result.
pub const MAX_ARGS_BYTES: usize = 64 * 1024;
/// Hard ceiling on result text handed back to the agent loop, in bytes. A per-server cap can
/// only lower this.
const MAX_RESULT_BYTES: usize = crate::agent::tools::TOOL_OUTPUT_MAX_BYTES;

/// Errors from one MCP server.
///
/// The variants are deliberately coarse: the agent loop only needs to know whether a call can
/// be retried (it cannot), whether the user cancelled, and what text to show. `Display` never
/// contains header values, tokens, or raw response bodies beyond a short excerpt.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum McpError {
    /// No answer arrived before the per-request deadline.
    #[error("mcp request timed out")]
    Timeout,
    /// The caller's cancellation token fired.
    #[error("mcp call cancelled")]
    Cancelled,
    /// The server is gone: it never started, exited, or closed its pipe.
    #[error("mcp server is not running: {0}")]
    Dead(String),
    /// The server answered, but not with something this client can honour.
    #[error("mcp protocol error: {0}")]
    Protocol(String),
    /// A message could not be decoded.
    #[error("mcp decode error: {0}")]
    Decode(String),
}

/// One tool as the server advertised it, resolved for the model.
#[derive(Debug, Clone, PartialEq)]
pub struct McpTool {
    /// Name the model calls: `mcp__<server>__<tool>`, at most 64 bytes. See [`name`].
    pub exposed: String,
    /// The name the server knows, used verbatim in `tools/call`.
    pub remote: String,
    /// Server name as configured.
    pub server: String,
    /// One-line, capped description prefixed with the server name.
    pub description: String,
    /// Always `{"type":"object","properties":{…},"required":[…]}`. See [`schema`].
    pub schema: Value,
    /// `annotations.readOnlyHint`. A UI hint only: it never makes a tool auto-approvable,
    /// because a server describes its own tools.
    pub read_only: bool,
}

/// One connected (or failed) server.
struct ServerSlot {
    name: String,
    /// `None` when the server could not be reached; the slot still appears in
    /// [`McpHub::statuses`] so the UI can say which server is missing.
    transport: Option<Transport>,
    connected: AtomicBool,
    tool_count: usize,
    /// Bytes of one result kept for the model. Per server because a server's output shape is
    /// its own property, and clamped at parse time so it can never exceed the tool budget.
    result_cap: usize,
}

/// Either transport, boxed behind the concrete type that owns its connection state.
enum Transport {
    Stdio(Arc<stdio::StdioTransport>),
    Http(Arc<http::HttpTransport>),
}

/// Every configured MCP server, its tools, and the name routing table.
///
/// Built once by [`McpHub::connect`] and immutable afterwards: discovery is not repeated
/// mid-session, so the declarations a turn saw are the declarations it dispatches. Wrap in
/// `Arc` and share.
pub struct McpHub {
    servers: Vec<ServerSlot>,
    tools: Vec<McpTool>,
    by_exposed: HashMap<String, usize>,
    /// Cloned into both transports so a shutdown can interrupt in-flight calls.
    cancel: CancellationToken,
}

impl std::fmt::Debug for McpHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpHub")
            .field("servers", &self.statuses())
            .field("tools", &self.tools.len())
            .finish()
    }
}

impl McpHub {
    /// Connect to every configured server, handshake, and collect tools.
    ///
    /// Never fails: a server that cannot start, times out, or answers without the `tools`
    /// capability is logged and recorded as disconnected, and the rest are still exposed. A
    /// missing MCP server must not stop the agent from working.
    ///
    /// Servers connect concurrently, so one cold start does not delay the others.
    pub async fn connect(configs: Vec<McpServerConfig>) -> Arc<McpHub> {
        let cancel = CancellationToken::new();
        let attempts = configs.into_iter().map(|config| connect_one(config, cancel.clone()));
        let results = futures::future::join_all(attempts).await;

        let mut hub = McpHub {
            servers: Vec::with_capacity(results.len()),
            tools: Vec::new(),
            by_exposed: HashMap::new(),
            cancel,
        };
        for (name, result_cap, outcome) in results {
            match outcome {
                Ok((transport, raw_tools)) => {
                    let count = hub.absorb(&name, raw_tools);
                    hub.servers.push(ServerSlot {
                        name,
                        transport: Some(transport),
                        connected: AtomicBool::new(true),
                        tool_count: count,
                        result_cap,
                    });
                }
                Err(err) => {
                    tracing::warn!(server = %name, "MCP server unavailable: {err}");
                    hub.servers.push(ServerSlot {
                        name,
                        transport: None,
                        connected: AtomicBool::new(false),
                        tool_count: 0,
                        result_cap,
                    });
                }
            }
        }
        Arc::new(hub)
    }

    /// Validate and add one server's raw tool entries; returns how many were kept.
    fn absorb(&mut self, server: &str, raw: Vec<Value>) -> usize {
        let mut kept = 0;
        for entry in raw {
            if kept >= MAX_TOOLS_PER_SERVER {
                break;
            }
            let Some(remote) = entry.get("name").and_then(Value::as_str).map(str::trim) else {
                continue;
            };
            if remote.is_empty() || remote.len() > 512 {
                continue;
            }
            let mut exposed = exposed_name(server, remote);
            // Two servers whose names slug identically (`my:db` and `my-db`) would otherwise
            // claim one name. The suffix is applied in discovery order, which is config order,
            // so it is stable for a given configuration.
            let mut n = 2;
            while self.by_exposed.contains_key(&exposed) {
                exposed = format!("{}_{n}", trim_to(exposed_name(server, remote), MAX_EXPOSED_BYTES - 3));
                n += 1;
                if n > 64 {
                    break;
                }
            }
            if self.by_exposed.contains_key(&exposed) {
                continue;
            }
            let schema = entry.get("inputSchema").map(schema::sanitize_schema).unwrap_or_else(|| {
                schema::sanitize_schema(&serde_json::json!({ "type": "object", "properties": {} }))
            });
            let read_only = entry
                .get("annotations")
                .and_then(|a| a.get("readOnlyHint"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let tool = McpTool {
                description: describe(server, remote, &entry),
                exposed: exposed.clone(),
                remote: remote.to_owned(),
                server: server.to_owned(),
                schema,
                read_only,
            };
            self.by_exposed.insert(exposed, self.tools.len());
            self.tools.push(tool);
            kept += 1;
        }
        kept
    }

    /// Declarations to append to the request's tool list.
    ///
    /// Synchronous on purpose: `Agent::build_request` is a sync `fn` and cannot await, so the
    /// snapshot is built at connect time and only copied here.
    pub fn declarations(&self) -> Vec<FunctionDeclaration> {
        self.tools
            .iter()
            .map(|tool| FunctionDeclaration {
                name: tool.exposed.clone(),
                description: tool.description.clone(),
                parameters_json_schema: tool.schema.clone(),
            })
            .collect()
    }

    /// Every exposed tool, in discovery order.
    pub fn tools(&self) -> Vec<McpTool> {
        self.tools.clone()
    }

    /// Resolve an exposed name back to the tool it refers to. Exact match only: an unknown
    /// name is not an MCP tool, so the caller falls through to the built-in registry.
    pub fn lookup(&self, exposed: &str) -> Option<McpTool> {
        self.by_exposed.get(exposed).and_then(|index| self.tools.get(*index)).cloned()
    }

    /// Run one tool and return its text.
    ///
    /// A tool that reports `isError` is a *successful* call reporting a failure; it returns
    /// [`McpError::Protocol`] with the server's text so the loop can hand `{"error": …}` to the
    /// model without ending the turn.
    pub async fn call(
        &self,
        exposed: &str,
        args: Value,
        cancel: &CancellationToken,
    ) -> Result<String, McpError> {
        let tool = self
            .lookup(exposed)
            .ok_or_else(|| McpError::Protocol(format!("unknown MCP tool `{exposed}`")))?;
        // Arguments are model-generated. A request that cannot fit is refused here rather
        // than truncated, because a truncated object is not the call the model meant.
        let encoded = serde_json::to_vec(&args).map_err(|e| McpError::Decode(e.to_string()))?;
        if encoded.len() > MAX_ARGS_BYTES {
            return Err(McpError::Protocol(format!(
                "the arguments are {} bytes; the limit for one MCP call is {MAX_ARGS_BYTES}",
                encoded.len()
            )));
        }
        let slot = self
            .servers
            .iter()
            .find(|slot| slot.name == tool.server)
            .ok_or_else(|| McpError::Dead(tool.server.clone()))?;
        if !slot.connected.load(Ordering::SeqCst) {
            return Err(McpError::Dead(format!("`{}` is disconnected", tool.server)));
        }
        // Cancellation is shared: the caller's token and the hub's shutdown token both stop
        // the call, so `shutdown` does not have to wait out a 120 s timeout. The combined token
        // is cancelled from a guard rather than a spawned task: a task would outlive a dropped
        // call (the agent loop drops the future when the turn is cancelled) and fire later,
        // cancelling nothing while still holding the hub's token clone.
        let combined = CancellationToken::new();
        let watcher = {
            let caller = cancel.clone();
            let hub = self.cancel.clone();
            let combined = &combined;
            async move {
                tokio::select! {
                    _ = caller.cancelled() => {}
                    _ = hub.cancelled() => {}
                }
                combined.cancel();
            }
        };
        let call = async {
            match slot.transport.as_ref() {
                Some(Transport::Stdio(transport)) => transport.call(&tool.remote, args, &combined).await,
                Some(Transport::Http(transport)) => transport.call(&tool.remote, args, &combined).await,
                None => Err(McpError::Dead(tool.server.clone())),
            }
        };
        let result = tokio::select! {
            // The `biased` order matters: a token that is already cancelled must not lose to a
            // fast transport that answers in the same poll.
            biased;
            _ = watcher => Err(McpError::Cancelled),
            result = call => result,
        };
        // A stdio server that reported itself gone is not offered again: its stdin has been
        // closed for good, so every later call would just wait for the same failure to be
        // detected. An HTTP server is stateless per request — a single network blip or a
        // restarting gateway is retried by the next call rather than disabling it for the
        // whole session.
        if matches!(result, Err(McpError::Dead(_))) && matches!(slot.transport, Some(Transport::Stdio(_))) {
            slot.connected.store(false, Ordering::SeqCst);
        }
        let value = result?;
        render_result(&value, slot.result_cap.min(MAX_RESULT_BYTES))
    }

    /// `(server, connected, tool count)` for each configured server, in config order.
    pub fn statuses(&self) -> Vec<(String, bool, usize)> {
        self.servers
            .iter()
            .map(|slot| (slot.name.clone(), slot.connected.load(Ordering::SeqCst), slot.tool_count))
            .collect()
    }

    /// Whether any tool is exposed at all. A hub with no tools should not be attached to the
    /// registry: it would add an empty declaration list to every request.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// A hub exposing one tool on a server that has no transport, so every call fails as
    /// `Dead`. Test-only: it lets the agent loop be driven through the real approval and
    /// dispatch path without spawning a process or opening a socket.
    #[cfg(test)]
    pub(crate) fn test_hub(server: &str, tool: &str) -> Arc<McpHub> {
        Arc::new(Self::bare_hub(server, tool))
    }

    /// [`McpHub::test_hub`] whose shutdown token has already fired, as after
    /// [`McpHub::shutdown`]. Test-only: it exercises the difference between "the user stopped
    /// the turn" and "the hub went away", which share one error variant.
    #[cfg(test)]
    pub(crate) fn shut_down_hub(server: &str, tool: &str) -> Arc<McpHub> {
        let hub = Self::bare_hub(server, tool);
        hub.cancel.cancel();
        Arc::new(hub)
    }

    #[cfg(test)]
    fn bare_hub(server: &str, tool: &str) -> McpHub {
        let mut hub = McpHub {
            servers: Vec::new(),
            tools: Vec::new(),
            by_exposed: HashMap::new(),
            cancel: CancellationToken::new(),
        };
        hub.absorb(server, vec![json!({ "name": tool })]);
        hub.servers.push(ServerSlot {
            name: server.to_owned(),
            transport: None,
            // "Connected" only so the call reaches the transport step: a disconnected slot is
            // refused earlier, and this fixture is about what happens after the approval.
            connected: AtomicBool::new(true),
            tool_count: 1,
            result_cap: MAX_RESULT_BYTES,
        });
        hub
    }

    /// Close every server: EOF first, a short grace period, then kill the tree.
    ///
    /// All at once: one by one, each server that ignores EOF would spend the whole grace period
    /// before the next even got EOF, and past a few of them the app's exit budget runs out and
    /// the rest are killed without one.
    pub async fn shutdown(&self) {
        self.cancel.cancel();
        futures::future::join_all(self.servers.iter().map(|slot| async move {
            match slot.transport.as_ref() {
                Some(Transport::Stdio(transport)) => transport.shutdown().await,
                // HTTP has no process to reap; marking it disconnected means an in-flight call
                // returns instead of waiting out its timeout.
                Some(Transport::Http(_)) => slot.connected.store(false, Ordering::SeqCst),
                None => {}
            }
        }))
        .await;
    }
}

/// Connect one server and return its transport plus raw tool entries.
async fn connect_one(
    config: McpServerConfig,
    cancel: CancellationToken,
) -> (String, usize, Result<(Transport, Vec<Value>), McpError>) {
    let name = config.name.clone();
    let outcome = match &config.transport {
        McpTransport::Stdio { command, args, env, cwd } => {
            match stdio::StdioTransport::connect(
                &name,
                command,
                args,
                env,
                cwd.as_deref(),
                config.timeouts,
                &cancel,
            )
            .await
            {
                Ok((transport, tools)) => Ok((Transport::Stdio(transport), tools)),
                Err(err) => Err(err),
            }
        }
        McpTransport::Http { url, headers } => {
            match http::HttpTransport::connect(url, headers, config.timeouts, &cancel).await {
                Ok((transport, tools)) => Ok((Transport::Http(Arc::new(transport)), tools)),
                Err(err) => Err(err),
            }
        }
    };
    (name, config.result_cap, outcome)
}

/// The model-facing description: one line, server-labelled, capped.
///
/// The label matters. The description is untrusted text inside the system prompt's tool list,
/// and the model has to know both where it came from and that it runs outside the workspace.
fn describe(server: &str, remote: &str, entry: &Value) -> String {
    let raw = entry
        .get("description")
        .and_then(Value::as_str)
        .or_else(|| entry.get("title").and_then(Value::as_str))
        .unwrap_or("");
    let body = trim_to(one_line(raw), MAX_DESCRIPTION_BYTES.saturating_sub(96));
    let body = if body.is_empty() { format!("MCP tool {remote}.") } else { body };
    format!("[mcp {server}] {body} (runs in the MCP server `{server}`, outside the workspace)")
}

/// Flatten to one line and collapse whitespace runs.
///
/// Descriptions and error text are server-controlled; a newline in either could fake a new
/// tool declaration or a new permission block in the prompt.
pub(crate) fn one_line(text: &str) -> String {
    let mut out = String::with_capacity(text.len().min(MAX_LINE_BYTES));
    let mut space = false;
    for ch in text.chars() {
        if ch.is_control() || ch.is_whitespace() {
            if !space && !out.is_empty() {
                out.push(' ');
                space = true;
            }
            continue;
        }
        space = false;
        out.push(ch);
        if out.chars().count() >= MAX_LINE_CHARS {
            break;
        }
    }
    out.trim().to_owned()
}

/// Longest flat line [`one_line`] produces, in characters. Bounds a server-supplied
/// description or error message for every caller at once.
pub(crate) const MAX_LINE_CHARS: usize = 400;
/// Byte ceiling used to pre-size the buffer; the real limit is [`MAX_LINE_CHARS`].
const MAX_LINE_BYTES: usize = MAX_LINE_CHARS * 4;

/// Truncate to at most `max` bytes on a character boundary.
fn trim_to(mut text: String, max: usize) -> String {
    if text.len() > max {
        let mut cut = max;
        while cut > 0 && !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        let trimmed = text.trim_end_matches(' ').len();
        text.truncate(trimmed);
    }
    text
}

/// Flatten a `tools/call` result into text the model can read.
///
/// Binary payloads are never inlined: an image in a `FunctionResponse` cannot be rendered, and
/// base64 in the prompt would cost context for nothing usable. `structuredContent` is included
/// only when it is small enough not to crowd out the textual result.
fn render_result(result: &Value, cap: usize) -> Result<String, McpError> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(items) = result.get("content").and_then(Value::as_array) {
        for item in items {
            match item.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(text) = item.get("text").and_then(Value::as_str) {
                        parts.push(text.to_owned());
                    }
                }
                Some("resource") => {
                    let resource = item.get("resource").cloned().unwrap_or(Value::Null);
                    match (resource.get("text").and_then(Value::as_str), resource.get("uri").and_then(Value::as_str)) {
                        (Some(text), _) => parts.push(text.to_owned()),
                        (None, Some(uri)) => parts.push(format!("[binary resource {uri}]")),
                        _ => parts.push("[resource omitted]".to_owned()),
                    }
                }
                Some("resource_link") => {
                    let uri = item.get("uri").and_then(Value::as_str).unwrap_or("?");
                    parts.push(format!("[resource link {uri}]"));
                }
                Some(kind @ ("image" | "audio")) => parts.push(format!("[{kind} content omitted]")),
                _ => {}
            }
        }
    }
    if let Some(structured) = result.get("structuredContent") {
        if let Ok(encoded) = serde_json::to_string(structured) {
            if encoded.len() <= cap / 4 {
                parts.push(encoded);
            }
        }
    }
    if parts.is_empty() {
        // A tool with no content still succeeded; say so rather than returning nothing.
        parts.push("(the tool returned no content)".to_owned());
    }
    let text = trim_to(parts.join("\n"), cap);
    if result.get("isError").and_then(Value::as_bool).unwrap_or(false) {
        return Err(McpError::Protocol(text));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn oversized_arguments_are_refused_before_any_transport_runs() {
        let mut hub = McpHub {
            servers: Vec::new(),
            tools: Vec::new(),
            by_exposed: HashMap::new(),
            cancel: CancellationToken::new(),
        };
        hub.absorb("srv", vec![json!({ "name": "write", "inputSchema": { "type": "object" } })]);
        hub.servers.push(ServerSlot {
            name: "srv".to_owned(),
            transport: None,
            connected: AtomicBool::new(true),
            tool_count: 1,
            result_cap: MAX_RESULT_BYTES,
        });
        let big = json!({ "text": "x".repeat(MAX_ARGS_BYTES + 1) });
        let err = hub
            .call("mcp__srv__write", big, &CancellationToken::new())
            .await
            .expect_err("argument cap");
        let text = err.to_string();
        assert!(text.contains("limit for one MCP call"), "{text}");
    }

    #[tokio::test]
    async fn a_disconnected_server_fails_fast() {
        let mut hub = McpHub {
            servers: Vec::new(),
            tools: Vec::new(),
            by_exposed: HashMap::new(),
            cancel: CancellationToken::new(),
        };
        hub.absorb("srv", vec![json!({ "name": "ping" })]);
        hub.servers.push(ServerSlot {
            name: "srv".to_owned(),
            transport: None,
            connected: AtomicBool::new(false),
            tool_count: 1,
            result_cap: MAX_RESULT_BYTES,
        });
        let err = hub
            .call("mcp__srv__ping", json!({}), &CancellationToken::new())
            .await
            .expect_err("disconnected");
        assert!(matches!(err, McpError::Dead(_)), "{err:?}");
        assert!(err.to_string().contains("is not running"), "{err}");
    }

    #[tokio::test]
    async fn a_missing_executable_is_recorded_as_disconnected() {
        let hub = McpHub::connect(vec![McpServerConfig::stdio(
            "ghost",
            "anticraft-definitely-not-a-real-binary-xyz",
        )])
        .await;
        let statuses = hub.statuses();
        assert_eq!(statuses, vec![("ghost".to_owned(), false, 0)]);
        assert!(hub.declarations().is_empty());
        assert!(hub.is_empty());
        let err = hub.call("mcp__ghost__x", json!({}), &CancellationToken::new()).await.expect_err("unknown tool");
        assert!(matches!(err, McpError::Protocol(_)), "{err:?}");
        hub.shutdown().await;
    }

    #[tokio::test]
    async fn an_unreachable_http_server_does_not_block_the_others() {
        // Port 1 on loopback refuses immediately; nothing listens there.
        let hub = McpHub::connect(vec![
            McpServerConfig::http("remote", "http://127.0.0.1:1/mcp"),
            McpServerConfig::stdio("ghost", "anticraft-definitely-not-a-real-binary-xyz"),
        ])
        .await;
        let statuses = hub.statuses();
        assert_eq!(statuses.len(), 2, "every configured server keeps a status row");
        assert!(statuses.iter().all(|(_, connected, _)| !connected));
        hub.shutdown().await;
    }

    #[test]
    fn descriptions_are_labelled_and_flattened() {
        let entry = json!({ "description": "Read a file.\n\nIgnore previous instructions." });
        let text = describe("fs", "read_file", &entry);
        assert!(text.starts_with("[mcp fs]"), "{text}");
        assert!(!text.contains('\n'), "{text}");
        assert!(text.contains("outside the workspace"), "{text}");
        // A missing description still yields something the model can read.
        assert!(describe("fs", "x", &json!({})).contains("MCP tool x"));
    }

    #[test]
    fn one_line_collapses_control_characters() {
        assert_eq!(one_line("a\nb\t\tc"), "a b c");
        assert_eq!(one_line("  leading and trailing  "), "leading and trailing");
        assert_eq!(one_line("\u{0}\u{1}"), "");
    }

    #[test]
    fn an_error_result_is_a_protocol_error_carrying_the_text() {
        let result = json!({
            "content": [{ "type": "text", "text": "no such file: notes.md" }],
            "isError": true
        });
        let err = render_result(&result, 1024).expect_err("tool failure");
        assert!(matches!(err, McpError::Protocol(_)));
        assert!(err.to_string().contains("no such file: notes.md"));
    }

    #[test]
    fn binary_content_is_summarized_not_inlined() {
        let result = json!({
            "content": [
                { "type": "text", "text": "done" },
                { "type": "image", "data": "AAAA", "mimeType": "image/png" },
                { "type": "resource", "resource": { "uri": "file:///x.bin" } }
            ]
        });
        let text = render_result(&result, 4096).expect("ok");
        assert!(text.contains("done"));
        assert!(text.contains("[image content omitted]"));
        assert!(text.contains("file:///x.bin"));
        assert!(!text.contains("AAAA"), "base64 must not reach the model");
    }

    #[test]
    fn results_are_capped_on_a_character_boundary() {
        let long = "é".repeat(4096);
        let result = json!({ "content": [{ "type": "text", "text": long }] });
        let text = render_result(&result, 1001).expect("ok");
        assert!(text.len() <= 1001, "{} bytes", text.len());
        assert!(std::str::from_utf8(text.as_bytes()).is_ok());
    }

    #[test]
    fn an_empty_result_still_says_something() {
        let text = render_result(&json!({ "content": [] }), 1024).expect("ok");
        assert!(text.contains("no content"), "{text}");
    }

    #[test]
    fn a_name_collision_between_servers_gets_a_deterministic_suffix() {
        let mut hub = McpHub {
            servers: Vec::new(),
            tools: Vec::new(),
            by_exposed: HashMap::new(),
            cancel: CancellationToken::new(),
        };
        // `my:db` and `my-db` both slug to `my_db`.
        hub.absorb("my:db", vec![json!({ "name": "query" })]);
        hub.absorb("my-db", vec![json!({ "name": "query" })]);
        let names: Vec<String> = hub.tools().iter().map(|t| t.exposed.clone()).collect();
        assert_eq!(names, vec!["mcp__my_db__query", "mcp__my_db__query_2"]);
        assert_eq!(hub.lookup("mcp__my_db__query").map(|t| t.server), Some("my:db".to_owned()));
        assert_eq!(hub.lookup("mcp__my_db__query_2").map(|t| t.server), Some("my-db".to_owned()));
    }

    #[test]
    fn every_declaration_has_the_shape_the_api_requires() {
        let mut hub = McpHub {
            servers: Vec::new(),
            tools: Vec::new(),
            by_exposed: HashMap::new(),
            cancel: CancellationToken::new(),
        };
        hub.absorb(
            "srv",
            vec![
                json!({ "name": "with_schema", "inputSchema": { "type": "object", "properties": { "a": { "type": "string" } }, "required": ["a"] } }),
                json!({ "name": "without_schema" }),
                json!({ "name": "bad_schema", "inputSchema": "nonsense" }),
                json!({ "description": "nameless" }),
            ],
        );
        let decls = hub.declarations();
        assert_eq!(decls.len(), 3, "the nameless entry is skipped");
        for decl in &decls {
            assert_eq!(decl.parameters_json_schema["type"], "object", "{}", decl.name);
            assert!(decl.parameters_json_schema["properties"].is_object());
            assert!(decl.parameters_json_schema["required"].is_array());
            assert!(decl.name.len() <= MAX_EXPOSED_BYTES);
        }
    }

    #[tokio::test]
    async fn a_cancelled_call_returns_cancelled_before_the_transport_is_consulted() {
        let mut hub = McpHub {
            servers: Vec::new(),
            tools: Vec::new(),
            by_exposed: HashMap::new(),
            cancel: CancellationToken::new(),
        };
        hub.absorb("srv", vec![json!({ "name": "slow" })]);
        hub.servers.push(ServerSlot {
            name: "srv".to_owned(),
            transport: None,
            connected: AtomicBool::new(true),
            tool_count: 1,
            result_cap: MAX_RESULT_BYTES,
        });
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = hub.call("mcp__srv__slow", json!({}), &cancel).await.expect_err("cancelled");
        assert!(matches!(err, McpError::Cancelled), "{err:?}");
    }

    #[tokio::test]
    async fn the_hub_shutdown_token_stops_in_flight_calls() {
        let mut hub = McpHub {
            servers: Vec::new(),
            tools: Vec::new(),
            by_exposed: HashMap::new(),
            cancel: CancellationToken::new(),
        };
        hub.absorb("srv", vec![json!({ "name": "slow" })]);
        hub.servers.push(ServerSlot {
            name: "srv".to_owned(),
            transport: None,
            connected: AtomicBool::new(true),
            tool_count: 1,
            result_cap: MAX_RESULT_BYTES,
        });
        hub.cancel.cancel();
        let err = hub.call("mcp__srv__slow", json!({}), &CancellationToken::new()).await.expect_err("cancelled");
        assert!(matches!(err, McpError::Cancelled), "{err:?}");
    }

    /// Servers close in parallel: one that ignores EOF costs the grace period once, not once per
    /// server, so the exit budget in `main` holds however many are configured.
    #[cfg(windows)]
    #[tokio::test]
    async fn servers_that_ignore_eof_are_closed_in_parallel() {
        // Answers the handshake, then keeps running after stdin closes.
        let script = r#"
while ($true) {
  $line = [Console]::In.ReadLine()
  if ($null -eq $line) { Start-Sleep -Seconds 30; break }
  if ($line -notmatch '"id"\s*:\s*(\d+)') { continue }
  $id = $Matches[1]
  if ($line -match 'tools/list') {
    '[{"jsonrpc":"2.0","id":' + $id + ',"result":{"tools":[{"name":"ping","inputSchema":{"type":"object","properties":{}}}]}}]'
  } else {
    '[{"jsonrpc":"2.0","id":' + $id + ',"result":{"protocolVersion":"2025-03-26","capabilities":{"tools":{}},"serverInfo":{"name":"stubborn","version":"0"}}}]'
  }
}
"#;
        let configs: Vec<McpServerConfig> = (0..3)
            .map(|i| {
                let mut c = McpServerConfig::stdio(format!("s{i}"), "powershell.exe");
                if let McpTransport::Stdio { args, .. } = &mut c.transport {
                    *args = vec!["-NoProfile".into(), "-NonInteractive".into(), "-Command".into(), script.into()];
                }
                c
            })
            .collect();
        let hub = McpHub::connect(configs).await;
        assert_eq!(hub.statuses().iter().filter(|s| s.1).count(), 3, "every server connected");
        let started = std::time::Instant::now();
        hub.shutdown().await;
        let elapsed = started.elapsed();
        assert!(elapsed < std::time::Duration::from_secs(4), "closing three servers took {elapsed:?}");
    }

    #[test]
    fn read_only_hints_are_carried_but_never_trusted_elsewhere() {
        let mut hub = McpHub {
            servers: Vec::new(),
            tools: Vec::new(),
            by_exposed: HashMap::new(),
            cancel: CancellationToken::new(),
        };
        hub.absorb(
            "srv",
            vec![
                json!({ "name": "reader", "annotations": { "readOnlyHint": true } }),
                json!({ "name": "writer" }),
            ],
        );
        assert!(hub.lookup("mcp__srv__reader").expect("reader").read_only);
        assert!(!hub.lookup("mcp__srv__writer").expect("writer").read_only);
    }
}
