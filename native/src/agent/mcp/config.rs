//! Parsing of `mcp_config.json` documents into [`McpServerConfig`].
//!
//! The document is user- and workspace-controlled: a cloned repo can ship one under
//! `.agents/plugins/`, so parsing never panics, never fails as a whole, and bounds everything
//! it keeps. An entry that cannot be used is skipped with a `tracing::warn` rather than
//! dropped silently or turned into an error, because one broken server must not take the
//! other configured servers down with it.
//!
//! Two extensions beyond the documented shape are accepted on purpose. `"enabled": false`
//! (or `"disabled": true`) lets a user park a server without deleting it, and a header value
//! may be `${env:NAME}`, so a checked-in config never has to hold a secret. Unknown keys are
//! ignored, which keeps forward compatibility with newer MCP documents.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::Value;

/// Most servers one document may contribute. Startup spawns a process per stdio server, so a
/// hostile config must not be able to spawn dozens of them just by listing more entries.
pub const MAX_SERVERS: usize = 32;
/// Most headers kept per HTTP server.
pub const MAX_HEADERS: usize = 64;
/// Longest server name accepted, in bytes. The name is embedded in every exposed tool name.
pub const MAX_NAME_BYTES: usize = 64;

/// Per-server deadlines, in seconds.
///
/// They are configurable because the right budget is a property of the server, not of this
/// client: a local `node` server answers in milliseconds, while a remote one behind a gateway
/// may legitimately take minutes for one `tools/call`. Every accessor clamps, so a hostile or
/// mistaken config cannot park a caller for a day or make it give up before the handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    /// Budget for `initialize` and `tools/list`.
    pub init_secs: u64,
    /// Budget for one `tools/call`.
    pub call_secs: u64,
    /// Longest silence tolerated between SSE chunks.
    pub idle_secs: u64,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self { init_secs: 20, call_secs: 120, idle_secs: 60 }
    }
}

impl Timeouts {
    /// The handshake budget, clamped to `1..=600 s`.
    pub fn init(self) -> Duration {
        Duration::from_secs(self.init_secs.clamp(1, 600))
    }

    /// The `tools/call` budget, clamped to `1..=3600 s`.
    pub fn call(self) -> Duration {
        Duration::from_secs(self.call_secs.clamp(1, 3600))
    }

    /// The stream-idle budget, clamped to `1..=600 s`.
    pub fn idle(self) -> Duration {
        Duration::from_secs(self.idle_secs.clamp(1, 600))
    }
}

/// Most arguments kept per stdio server.
const MAX_ARGS: usize = 64;
/// Most environment entries kept per stdio server.
const MAX_ENV: usize = 64;
/// Default cap on the text one tool result contributes, in bytes.
const DEFAULT_RESULT_CAP: usize = crate::agent::tools::TOOL_OUTPUT_MAX_BYTES;
/// Largest result cap a config may ask for, in bytes. A server cannot buy itself more context
/// than the built-in tools get.
pub const MAX_RESULT_CAP: usize = crate::agent::tools::TOOL_OUTPUT_MAX_BYTES;

/// One entry of the `mcpServers` object, resolved to a transport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerConfig {
    /// Server name as written in the config. Unique within one hub.
    pub name: String,
    /// How to reach the server.
    pub transport: McpTransport,
    /// Per-server deadlines.
    pub timeouts: Timeouts,
    /// Bytes of one tool result kept for the model, clamped to [`MAX_RESULT_CAP`].
    pub result_cap: usize,
}

/// The two MCP transports AntiCraft speaks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpTransport {
    /// Spawn a child process and exchange newline-delimited JSON-RPC over its stdio.
    Stdio {
        /// Executable to run. Resolved the same way a shell would resolve it.
        command: String,
        /// Arguments, verbatim.
        args: Vec<String>,
        /// Environment entries layered on top of the inherited environment.
        env: Vec<(String, String)>,
        /// Working directory; `None` inherits the app's.
        cwd: Option<PathBuf>,
    },
    /// Streamable HTTP: one POST per message to `url`.
    Http {
        /// Endpoint of the server.
        url: String,
        /// Static headers, `${env:NAME}` references already expanded.
        headers: Vec<(String, String)>,
    },
}

impl McpServerConfig {
    /// A stdio server with no arguments, extra environment or working directory.
    pub fn stdio(name: impl Into<String>, command: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            transport: McpTransport::Stdio {
                command: command.into(),
                args: Vec::new(),
                env: Vec::new(),
                cwd: None,
            },
            timeouts: Timeouts::default(),
            result_cap: DEFAULT_RESULT_CAP,
        }
    }

    /// A Streamable HTTP server with no extra headers.
    pub fn http(name: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            transport: McpTransport::Http { url: url.into(), headers: Vec::new() },
            timeouts: Timeouts::default(),
            result_cap: DEFAULT_RESULT_CAP,
        }
    }
}

/// Parse a `{"mcpServers": {...}}` document.
///
/// `source` only labels the warnings (`"<data-dir>/mcp_config.json"` and so on). The result
/// keeps the order of the document, capped at [`MAX_SERVERS`], with duplicate names removed
/// because the hub keys servers by name.
pub fn parse_servers(v: &Value, source: &str) -> Vec<McpServerConfig> {
    let Some(map) = v.get("mcpServers").and_then(Value::as_object) else {
        if !v.is_null() {
            tracing::warn!("mcp: {source} has no \"mcpServers\" object");
        }
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for (raw_name, entry) in map {
        if out.len() >= MAX_SERVERS {
            tracing::warn!("mcp: {source} configures more than {MAX_SERVERS} servers; the rest are ignored");
            break;
        }
        let Some(config) = parse_entry(raw_name, entry, source) else { continue };
        if !seen.insert(config.name.clone()) {
            tracing::warn!("mcp: {source} configures `{}` twice; the later entry is ignored", config.name);
            continue;
        }
        out.push(config);
    }
    out
}

/// One `mcpServers` member. `None` means "skip this entry", never "fail the document".
fn parse_entry(raw_name: &str, entry: &Value, source: &str) -> Option<McpServerConfig> {
    let name = raw_name.trim();
    if name.is_empty() {
        tracing::warn!("mcp: {source} has an entry with an empty name; skipped");
        return None;
    }
    if name.len() > MAX_NAME_BYTES {
        tracing::warn!("mcp: {source} names a server longer than {MAX_NAME_BYTES} bytes; skipped");
        return None;
    }
    let Some(obj) = entry.as_object() else {
        tracing::warn!("mcp: {source} entry `{name}` is not an object; skipped");
        return None;
    };
    // Present-but-false is an explicit opt-out; an absent key leaves the server enabled.
    if obj.get("enabled").and_then(Value::as_bool) == Some(false) {
        return None;
    }
    if obj.get("disabled").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let command = obj.get("command").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty());
    let url = obj
        .get("url")
        .or_else(|| obj.get("serverUrl"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let timeouts = parse_timeouts(obj, name, source);
    let result_cap = parse_result_cap(obj);
    match (command, url) {
        (Some(_), Some(_)) => {
            tracing::warn!("mcp: {source} entry `{name}` sets both `command` and `url`; skipped");
            None
        }
        (Some(command), None) => Some(McpServerConfig {
            name: name.to_owned(),
            transport: McpTransport::Stdio {
                command: command.to_owned(),
                args: string_list(obj.get("args"), MAX_ARGS),
                env: string_pairs(obj.get("env"), MAX_ENV, false),
                cwd: obj.get("cwd").and_then(Value::as_str).map(PathBuf::from),
            },
            timeouts,
            result_cap,
        }),
        (None, Some(url)) => Some(McpServerConfig {
            name: name.to_owned(),
            transport: McpTransport::Http {
                url: url.to_owned(),
                headers: allowed_headers(obj.get("headers"), MAX_HEADERS),
            },
            timeouts,
            result_cap,
        }),
        (None, None) => {
            tracing::warn!("mcp: {source} entry `{name}` has neither `command` nor `url`; skipped");
            None
        }
    }
}

/// The optional `"timeouts"` object. Absent or malformed members keep their defaults rather
/// than failing the entry: a typo in a tuning knob must not hide a working server.
fn parse_timeouts(obj: &serde_json::Map<String, Value>, name: &str, source: &str) -> Timeouts {
    let Some(node) = obj.get("timeouts").and_then(Value::as_object) else {
        return Timeouts::default();
    };
    let mut out = Timeouts::default();
    if let Some(v) = node.get("initSecs").and_then(Value::as_u64) {
        out.init_secs = v;
    }
    if let Some(v) = node.get("callSecs").and_then(Value::as_u64) {
        out.call_secs = v;
    }
    if let Some(v) = node.get("idleSecs").and_then(Value::as_u64) {
        out.idle_secs = v;
    }
    // `requestSecs` is the HTTP design document's name for the whole-exchange budget, which is
    // the `tools/call` budget here; accepted so a document written to that design still tunes
    // something rather than being ignored.
    if node.get("callSecs").is_none() {
        if let Some(v) = node.get("requestSecs").and_then(Value::as_u64) {
            out.call_secs = v;
        }
    }
    if out.init_secs == 0 || out.call_secs == 0 || out.idle_secs == 0 {
        tracing::warn!("mcp: {source} entry `{name}` sets a zero timeout; it is clamped on use");
    }
    out
}

/// The optional `"resultCapBytes"` cap on one tool result, clamped to [`MAX_RESULT_CAP`].
fn parse_result_cap(obj: &serde_json::Map<String, Value>) -> usize {
    obj.get("resultCapBytes")
        .and_then(Value::as_u64)
        .and_then(|v| usize::try_from(v).ok())
        .map(|v| v.clamp(1, MAX_RESULT_CAP))
        .unwrap_or(DEFAULT_RESULT_CAP)
}

/// String members of an array, capped. Non-string elements are dropped rather than failing.
fn string_list(value: Option<&Value>, cap: usize) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).take(cap).map(str::to_owned).collect())
        .unwrap_or_default()
}

/// Headers a configured server may set.
///
/// The list is closed on purpose. `mcp_config.json` can arrive from a cloned repository, and a
/// header map that accepts anything lets that file set `Host`, `Cookie` or a hop-by-hop header
/// on every request — rewriting where the request goes or replaying a session this app owns.
/// Credentials are the only legitimate reason to set a header here, and `${env:NAME}` is how a
/// checked-in document refers to one without containing it.
pub const ALLOWED_HEADERS: &[&str] = &["authorization", "x-api-key", "x-auth-token", "x-goog-api-key"];

/// Filter a header map down to [`ALLOWED_HEADERS`], warning once per rejected name.
fn allowed_headers(value: Option<&Value>, cap: usize) -> Vec<(String, String)> {
    let Some(map) = value.and_then(Value::as_object) else { return Vec::new() };
    let mut out: Vec<(String, String)> = Vec::with_capacity(map.len().min(cap));
    for (key, raw) in map {
        let lower = key.trim().to_ascii_lowercase();
        if !ALLOWED_HEADERS.contains(&lower.as_str()) {
            tracing::warn!("mcp: header `{key}` is not allowed; drop it or use one of {ALLOWED_HEADERS:?}");
            continue;
        }
        if out.len() >= cap {
            tracing::warn!("mcp: more than {cap} headers configured; the rest are ignored");
            break;
        }
        // Per-entry failures are reported by the same rules as any other config map: an unset
        // variable or a non-string value drops that one header, not the server.
        match string_pairs(Some(&Value::Object([(key.clone(), raw.clone())].into_iter().collect())), 1, true)
            .into_iter()
            .next()
        {
            Some(entry) => out.push(entry),
            None => continue,
        }
    }
    out
}

/// String members of an object, capped, optionally expanding `${env:NAME}` values.
///
/// An entry whose variable is unset is dropped, not sent empty: an empty `Authorization`
/// header is worse than no header, because it turns a missing secret into a 401.
fn string_pairs(value: Option<&Value>, cap: usize, expand: bool) -> Vec<(String, String)> {
    let Some(map) = value.and_then(Value::as_object) else { return Vec::new() };
    let mut out = Vec::with_capacity(map.len().min(cap));
    for (key, raw) in map {
        if out.len() >= cap {
            tracing::warn!("mcp: more than {cap} entries in a config map; the rest are ignored");
            break;
        }
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        let Some(raw) = raw.as_str() else {
            tracing::warn!("mcp: value for `{key}` is not a string; ignored");
            continue;
        };
        let resolved = if expand { expand_env(raw) } else { Some(raw.to_owned()) };
        match resolved {
            Some(value) => out.push((key.to_owned(), value)),
            None => tracing::warn!("mcp: value for `{key}` refers to an unset environment variable; ignored"),
        }
    }
    out
}

/// `${env:NAME}` -> the variable's value. `None` when it is unset or empty.
fn expand_env(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if let Some(name) = trimmed.strip_prefix("${env:").and_then(|rest| rest.strip_suffix('}')) {
        return std::env::var(name).ok().filter(|s| !s.is_empty());
    }
    Some(trimmed.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_a_stdio_server() {
        let doc = json!({
            "mcpServers": {
                "fs": {
                    "command": "node",
                    "args": ["server.js", "--root", "."],
                    "env": { "TOKEN": "abc", "COUNT": 3 },
                    "cwd": "C:/work"
                }
            }
        });
        let servers = parse_servers(&doc, "test");
        assert_eq!(servers.len(), 1);
        let McpTransport::Stdio { command, args, env, cwd } = &servers[0].transport else {
            panic!("expected stdio");
        };
        assert_eq!(command, "node");
        assert_eq!(args.as_slice(), ["server.js", "--root", "."].as_slice());
        assert_eq!(env, &vec![("TOKEN".to_string(), "abc".to_string())]);
        assert_eq!(cwd.as_deref(), Some(std::path::Path::new("C:/work")));
    }

    #[test]
    fn parses_an_http_server_and_the_legacy_url_key() {
        let doc = json!({
            "mcpServers": {
                "remote": { "serverUrl": "https://example.test/mcp", "headers": { "Authorization": "Bearer t" } }
            }
        });
        let servers = parse_servers(&doc, "test");
        let McpTransport::Http { url, headers } = &servers[0].transport else { panic!("expected http") };
        assert_eq!(url, "https://example.test/mcp");
        assert_eq!(headers, &vec![("Authorization".to_string(), "Bearer t".to_string())]);
    }

    #[test]
    fn skips_entries_that_cannot_be_used() {
        let doc = json!({
            "mcpServers": {
                "both": { "command": "node", "url": "https://x.test" },
                "neither": { "args": [] },
                "not-an-object": 3,
                "": { "command": "node" }
            }
        });
        assert!(parse_servers(&doc, "test").is_empty());
    }

    #[test]
    fn honours_enabled_and_disabled_flags() {
        let doc = json!({
            "mcpServers": {
                "off": { "command": "node", "enabled": false },
                "parked": { "command": "node", "disabled": true },
                "on": { "command": "node", "enabled": true }
            }
        });
        let servers = parse_servers(&doc, "test");
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "on");
    }

    #[test]
    fn only_credential_headers_are_kept() {
        let doc = json!({
            "mcpServers": {
                "remote": {
                    "url": "https://x.test",
                    "headers": {
                        "Authorization": "Bearer t",
                        "X-Api-Key": "k",
                        "Host": "evil.test",
                        "Cookie": "session=1"
                    }
                }
            }
        });
        let servers = parse_servers(&doc, "test");
        let McpTransport::Http { headers, .. } = &servers[0].transport else { panic!("expected http") };
        let names: Vec<&str> = headers.iter().map(|(k, _)| k.as_str()).collect();
        assert!(names.contains(&"X-Api-Key"), "{names:?}");
        assert!(!names.contains(&"Host"), "a config must not rewrite the target host");
        assert!(!names.contains(&"Cookie"), "a config must not inject a session cookie");
    }

    #[test]
    fn caps_the_number_of_servers() {
        let mut servers = serde_json::Map::new();
        for i in 0..(MAX_SERVERS + 5) {
            servers.insert(format!("s{i:02}"), json!({ "command": "node" }));
        }
        let doc = json!({ "mcpServers": servers });
        assert_eq!(parse_servers(&doc, "test").len(), MAX_SERVERS);
    }

    #[test]
    fn caps_the_number_of_headers() {
        // Every allowed name, in a different casing, is kept; the allowlist is what bounds the
        // set, and the cap is a second, independent bound for a longer future list.
        let mut headers = serde_json::Map::new();
        for (i, name) in ["Authorization", "x-api-key", "X-Auth-Token", "X-Goog-Api-Key"].iter().enumerate() {
            headers.insert((*name).to_owned(), json!(format!("v{i}")));
        }
        let doc = json!({ "mcpServers": { "h": { "url": "https://x.test", "headers": headers } } });
        let servers = parse_servers(&doc, "test");
        let McpTransport::Http { headers, .. } = &servers[0].transport else { panic!("expected http") };
        assert_eq!(headers.len(), 4);
        assert!(headers.len() <= MAX_HEADERS);

        // Names that merely start with an allowed one are not allowed: the list is exact.
        let mut many = serde_json::Map::new();
        for i in 0..(MAX_HEADERS + 5) {
            many.insert(format!("X-Api-Key{i}"), json!("v"));
        }
        let doc = json!({ "mcpServers": { "h": { "url": "https://x.test", "headers": many } } });
        let McpTransport::Http { headers, .. } = &parse_servers(&doc, "test")[0].transport else {
            panic!("expected http");
        };
        assert!(headers.is_empty(), "a prefix match must not pass the allowlist: {headers:?}");
    }

    #[test]
    fn expands_environment_references_in_headers() {
        // A name no other test uses: the process environment is global to the test binary.
        std::env::set_var("ANTICRAFT_MCP_TEST_TOKEN", "secret-value");
        let doc = json!({
            "mcpServers": {
                "remote": { "url": "https://x.test", "headers": { "Authorization": "${env:ANTICRAFT_MCP_TEST_TOKEN}" } }
            }
        });
        let servers = parse_servers(&doc, "test");
        let McpTransport::Http { headers, .. } = &servers[0].transport else { panic!("expected http") };
        assert_eq!(headers[0].1, "secret-value");
        std::env::remove_var("ANTICRAFT_MCP_TEST_TOKEN");
    }

    #[test]
    fn per_server_timeouts_and_result_caps_are_parsed_and_clamped() {
        let doc = json!({
            "mcpServers": {
                "tuned": {
                    "command": "node",
                    "timeouts": { "initSecs": 3, "callSecs": 900, "idleSecs": 10 },
                    "resultCapBytes": 4096
                },
                "absurd": { "command": "node", "timeouts": { "callSecs": 99_999 }, "resultCapBytes": 999_999_999 }
            }
        });
        let servers = parse_servers(&doc, "test");
        assert_eq!(servers.len(), 2);
        // The document is a JSON object, so its order is the parser's (sorted here).
        let by_name = |n: &str| servers.iter().find(|s| s.name == n).expect("configured server");
        let tuned = by_name("tuned");
        assert_eq!(tuned.timeouts.init_secs, 3);
        assert_eq!(tuned.timeouts.call(), std::time::Duration::from_secs(900));
        assert_eq!(tuned.timeouts.idle_secs, 10);
        assert_eq!(tuned.result_cap, 4096);
        // A hostile value cannot buy more context than the built-in tools get.
        let absurd = by_name("absurd");
        assert_eq!(absurd.result_cap, MAX_RESULT_CAP);
        assert!(absurd.timeouts.call() <= std::time::Duration::from_secs(3600));
    }

    #[test]
    fn timeouts_default_when_absent_or_malformed() {
        let doc = json!({
            "mcpServers": {
                "plain": { "command": "node" },
                "broken": { "command": "node", "timeouts": "nonsense", "resultCapBytes": -5 }
            }
        });
        for server in parse_servers(&doc, "test") {
            assert_eq!(server.timeouts, Timeouts::default());
            assert_eq!(server.result_cap, crate::agent::tools::TOOL_OUTPUT_MAX_BYTES);
        }
    }

    #[test]
    fn a_missing_document_yields_no_servers() {
        assert!(parse_servers(&json!({}), "test").is_empty());
        assert!(parse_servers(&Value::Null, "test").is_empty());
        assert!(parse_servers(&json!({ "mcpServers": [] }), "test").is_empty());
    }
}
