//! JSON-RPC 2.0 wire types shared by both MCP transports.
//!
//! Both transports carry the same messages; only framing differs (newline-delimited JSON over
//! stdio, one POST body per message over HTTP). Keeping classification here means the two
//! transports cannot drift on the traps: a request the server sends *us* must be answered or
//! the server blocks, a response we did not ask for is dropped rather than treated as a
//! decode failure, and a batch array introduced by a non-conforming server is tolerated even
//! though this client never sends one.

use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{json, Value};

/// JSON-RPC "method not found". Used both to reject a server-initiated request we do not
/// implement and to recognise a server rejecting one of ours.
pub const METHOD_NOT_FOUND: i64 = -32601;
/// JSON-RPC "invalid params". A server uses it when the model built arguments the tool
/// rejects; the model can act on it, so it is reported verbatim.
pub const INVALID_PARAMS: i64 = -32602;

/// A JSON-RPC id. Numeric in every message this client sends, but a server may answer with a
/// string, so both forms are accepted on the way in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RpcId {
    /// The common case.
    Num(u64),
    /// A server that echoes ids as strings.
    Str(String),
}

impl RpcId {
    /// The id as a number, accepting the decimal-string form. Anything else is not one of ours.
    pub fn as_request_id(&self) -> Option<u64> {
        match self {
            RpcId::Num(n) => Some(*n),
            RpcId::Str(s) => s.parse().ok(),
        }
    }

    /// The id as a JSON value, for building a reply.
    pub fn to_value(&self) -> Value {
        match self {
            RpcId::Num(n) => json!(n),
            RpcId::Str(s) => json!(s),
        }
    }
}

impl std::fmt::Display for RpcId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RpcId::Num(n) => write!(f, "{n}"),
            RpcId::Str(s) => write!(f, "{s}"),
        }
    }
}

/// The `error` member of a failed response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcError {
    /// JSON-RPC or application-defined code.
    pub code: i64,
    /// Human-readable message, already one line and length-capped by [`RpcError::parse`].
    pub message: String,
}

impl RpcError {
    /// Parse the `error` member. A malformed one still yields an error rather than being
    /// ignored, because the caller is waiting and silence would only show up as a timeout.
    pub fn parse(v: &Value) -> Self {
        let code = v.get("code").and_then(Value::as_i64).unwrap_or(0);
        let message = v
            .get("message")
            .and_then(Value::as_str)
            .map(crate::agent::mcp::one_line)
            .unwrap_or_else(|| "the server reported an error with no message".to_owned());
        Self { code, message }
    }
}

/// One inbound message, classified by shape rather than by arrival order.
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    /// An answer to a request we sent.
    Response { id: RpcId, result: Result<Value, RpcError> },
    /// A one-way message from the server.
    Notification { method: String, params: Value },
    /// A request from the server. We declare no capabilities that could produce one, so it is
    /// answered with [`METHOD_NOT_FOUND`] rather than left hanging.
    ServerRequest { id: RpcId, method: String, params: Value },
    /// `id: null`, both `result` and `error`, neither, or not an object at all. Ignored with a
    /// log line; never allowed to fail the transport.
    Unusable(&'static str),
}

/// Classify one decoded JSON value.
pub fn classify(v: &Value) -> Message {
    let Some(obj) = v.as_object() else {
        return Message::Unusable("not a JSON object");
    };
    let id = obj.get("id").and_then(parse_id);
    let params = obj.get("params").cloned().unwrap_or(Value::Null);
    if let Some(method) = obj.get("method").and_then(Value::as_str) {
        return match id {
            Some(id) => Message::ServerRequest { id, method: method.to_owned(), params },
            None => Message::Notification { method: method.to_owned(), params },
        };
    }
    match (id, obj.get("result"), obj.get("error")) {
        (Some(id), Some(result), None) => Message::Response { id, result: Ok(result.clone()) },
        (Some(id), None, Some(error)) => Message::Response { id, result: Err(RpcError::parse(error)) },
        (_, Some(_), Some(_)) => Message::Unusable("both result and error"),
        (None, _, _) => Message::Unusable("response without an id"),
        (Some(_), None, None) => Message::Unusable("neither result nor error"),
    }
}

/// Decode `id` when it is a number or a string; `null` and objects are not ids.
fn parse_id(v: &Value) -> Option<RpcId> {
    match v {
        Value::Number(n) => n.as_u64().map(RpcId::Num),
        Value::String(s) if !s.is_empty() && s.len() <= 64 => Some(RpcId::Str(s.clone())),
        _ => None,
    }
}

/// Split one payload into messages, tolerating the legacy batch array.
///
/// The 2025-03-26 revision removed batching and this client never sends arrays, but a server
/// that answers with one would otherwise stall every call until its timeout. Element-wise
/// decoding keeps the stream usable without adopting the feature.
pub fn split_payload(v: &Value) -> Vec<Message> {
    match v {
        Value::Array(items) => items.iter().map(classify).collect(),
        other => vec![classify(other)],
    }
}

/// A `method` request carrying `id`.
pub fn request(id: u64, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

/// A `method` notification: no id, no response expected.
pub fn notification(method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "method": method, "params": params })
}

/// A successful response to a server-initiated request.
pub fn success_reply(id: &RpcId, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id.to_value(), "result": result })
}

/// An error response to a server-initiated request.
pub fn error_reply(id: &RpcId, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id.to_value(), "error": { "code": code, "message": message } })
}

/// Monotonic request ids. Starts at 1 so a `0` in a server reply is never mistaken for ours;
/// ids are per hub, so a restarted server sees a sequence it has not already answered.
#[derive(Debug, Default)]
pub struct IdGen {
    next: AtomicU64,
}

impl IdGen {
    /// A generator whose first id is 1.
    pub fn new() -> Self {
        Self { next: AtomicU64::new(1) }
    }

    /// The next id, and the value to put in the frame.
    pub fn next(&self) -> u64 {
        self.next.fetch_add(1, Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_a_response_with_a_result() {
        let v = json!({ "jsonrpc": "2.0", "id": 7, "result": { "tools": [] } });
        match classify(&v) {
            Message::Response { id, result } => {
                assert_eq!(id.as_request_id(), Some(7));
                assert_eq!(result.ok().and_then(|r| r.get("tools").cloned()), Some(json!([])));
            }
            other => panic!("expected a response, got {other:?}"),
        }
    }

    #[test]
    fn classifies_an_error_response_with_a_capped_message() {
        let long = "x".repeat(2000);
        let v = json!({ "jsonrpc": "2.0", "id": "9", "error": { "code": -32602, "message": long } });
        match classify(&v) {
            Message::Response { id, result } => {
                assert_eq!(id.as_request_id(), Some(9), "string ids must parse");
                let err = result.expect_err("an error response");
                assert_eq!(err.code, INVALID_PARAMS);
                assert!(err.message.len() <= 400, "{} bytes", err.message.len());
            }
            other => panic!("expected a response, got {other:?}"),
        }
    }

    #[test]
    fn distinguishes_notifications_from_server_requests() {
        let note = classify(&json!({ "jsonrpc": "2.0", "method": "notifications/message" }));
        assert!(matches!(note, Message::Notification { ref method, .. } if method == "notifications/message"));
        let req = classify(&json!({ "jsonrpc": "2.0", "id": 3, "method": "sampling/createMessage" }));
        assert!(matches!(req, Message::ServerRequest { id, .. } if id.as_request_id() == Some(3)));
    }

    #[test]
    fn unusable_shapes_are_ignored_not_fatal() {
        assert!(matches!(classify(&json!([])), Message::Unusable(_)));
        assert!(matches!(classify(&json!({ "id": 1 })), Message::Unusable(_)));
        assert!(matches!(
            classify(&json!({ "id": 1, "result": {}, "error": { "code": 1 } })),
            Message::Unusable(_)
        ));
        // `id: null` is what a server sends when it could not attribute a parse error.
        assert!(matches!(
            classify(&json!({ "id": null, "error": { "code": -32700 } })),
            Message::Unusable(_)
        ));
    }

    #[test]
    fn a_batch_array_is_split_element_wise() {
        let v = json!([
            { "jsonrpc": "2.0", "id": 1, "result": {} },
            { "jsonrpc": "2.0", "method": "notifications/progress" }
        ]);
        let msgs = split_payload(&v);
        assert_eq!(msgs.len(), 2);
        assert!(matches!(msgs[0], Message::Response { .. }));
        assert!(matches!(msgs[1], Message::Notification { .. }));
    }

    #[test]
    fn builds_frames_with_the_expected_shape() {
        let req = request(12, "tools/call", json!({ "name": "x" }));
        assert_eq!(req["jsonrpc"], "2.0");
        assert_eq!(req["id"], 12);
        assert_eq!(req["method"], "tools/call");
        let note = notification("notifications/initialized", json!({}));
        assert!(note.get("id").is_none(), "a notification must not carry an id");
        let reply = error_reply(&RpcId::Num(4), METHOD_NOT_FOUND, "nope");
        assert_eq!(reply["id"], 4);
        assert_eq!(reply["error"]["code"], -32601);
    }

    #[test]
    fn ids_are_monotonic_and_start_at_one() {
        let gen = IdGen::new();
        assert_eq!(gen.next(), 1);
        assert_eq!(gen.next(), 2);
        assert_eq!(gen.next(), 3);
    }
}
