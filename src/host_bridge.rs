//! Host bridge server: LS → shell RPC over Connect unary (SPEC §7).
//!
//! Plain HTTP/1.1 on `127.0.0.1:<ephemeral>`. A security gate (loopback peer,
//! exact `Host`, constant-time Bearer token) runs before any routing, so an
//! unauthenticated request to an unknown path gets 403/401, never 404.

use crate::proto::{self, Message};
use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::Response;
use axum::Router;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use subtle::ConstantTimeEq;
use tokio::sync::oneshot;

/// Source of update state for the two RPCs. Implemented by
/// [`crate::updater::Updater`]; tests use a fake.
pub trait UpdateStatusProvider: Send + Sync + 'static {
    /// §7.2 GetUpdateStatus: built from cached state only, never hits the network.
    fn get_update_status(&self) -> proto::UpdateStatus;
    /// §7.2 ApplyUpdate: returns `accepted`. May start a non-manual check or
    /// schedule quit-and-install; must not block.
    fn apply_update(&self) -> bool;
}

/// Max request body accepted (the request messages are empty).
const MAX_BODY: usize = 64 * 1024;

/// A running bridge. Drop or [`HostBridge::shutdown`] to stop.
pub struct HostBridge {
    addr: SocketAddr,
    token: String,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl HostBridge {
    /// Bind `127.0.0.1:0`, generate a token and start serving on the current runtime.
    pub async fn start(provider: Arc<dyn UpdateStatusProvider>) -> std::io::Result<Self> {
        Self::start_with_token(provider, generate_token()).await
    }

    /// Same as [`HostBridge::start`] with a caller-provided token (tests).
    pub async fn start_with_token(
        provider: Arc<dyn UpdateStatusProvider>,
        token: String,
    ) -> std::io::Result<Self> {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
        let addr = listener.local_addr()?;
        let state = Arc::new(BridgeState {
            provider,
            token: token.clone(),
            expected_host: format!("127.0.0.1:{}", addr.port()),
        });
        let app = Router::new().fallback(handle).with_state(state);
        let (tx, rx) = oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            let svc = app.into_make_service_with_connect_info::<SocketAddr>();
            let res = axum::serve(listener, svc)
                .with_graceful_shutdown(async move {
                    let _ = rx.await;
                })
                .await;
            if let Err(e) = res {
                tracing::error!("host bridge server error: {e}");
            }
        });
        tracing::info!("host bridge listening on http://{addr}");
        Ok(Self { addr, token, shutdown: Some(tx), task: Some(task) })
    }

    /// `http://127.0.0.1:<port>`, passed as `--host_bridge_url=`.
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.addr.port())
    }

    /// 64 lowercase hex chars, passed as `--host_bridge_token=`.
    pub fn token(&self) -> &str {
        &self.token
    }

    #[allow(dead_code)]
    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// Graceful shutdown; waits for in-flight requests to finish.
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(t) = self.task.take() {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), t).await;
        }
    }
}

impl Drop for HostBridge {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

/// 32 random bytes as lowercase hex.
pub fn generate_token() -> String {
    let bytes: [u8; 32] = rand::random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

struct BridgeState {
    provider: Arc<dyn UpdateStatusProvider>,
    token: String,
    expected_host: String,
}

/// Why the gate rejected a request (logged; body stays generic).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateRejection {
    NonLoopbackPeer,
    BadHost,
    MissingBearer,
    BadToken,
}

impl GateRejection {
    fn status(self) -> StatusCode {
        match self {
            GateRejection::NonLoopbackPeer | GateRejection::BadHost => StatusCode::FORBIDDEN,
            _ => StatusCode::UNAUTHORIZED,
        }
    }
    fn reason(self) -> &'static str {
        match self {
            GateRejection::NonLoopbackPeer => "non-loopback peer",
            GateRejection::BadHost => "host mismatch",
            GateRejection::MissingBearer => "missing bearer token",
            GateRejection::BadToken => "invalid bearer token",
        }
    }
}

/// Peer must be 127.0.0.1, ::1 or ::ffff:127.0.0.1.
pub fn is_allowed_peer(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4 == std::net::Ipv4Addr::LOCALHOST,
        IpAddr::V6(v6) => {
            v6 == std::net::Ipv6Addr::LOCALHOST
                || v6.to_ipv4_mapped() == Some(std::net::Ipv4Addr::LOCALHOST)
        }
    }
}

/// Parse `Authorization`: trimmed, `Bearer` (any case), whitespace, one
/// non-whitespace run to the end.
pub fn parse_bearer(value: &str) -> Option<&str> {
    let v = value.trim();
    let (scheme, rest) = v.split_at(v.find(char::is_whitespace)?);
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let tok = rest.trim_start();
    if tok.is_empty() || tok.contains(char::is_whitespace) {
        return None;
    }
    Some(tok)
}

/// Run the three gate checks in order.
pub fn check_gate(
    peer: IpAddr,
    headers: &HeaderMap,
    expected_host: &str,
    token: &str,
) -> Result<(), GateRejection> {
    if !is_allowed_peer(peer) {
        return Err(GateRejection::NonLoopbackPeer);
    }
    let host = headers.get(header::HOST).and_then(|h| h.to_str().ok());
    if host != Some(expected_host) {
        return Err(GateRejection::BadHost);
    }
    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(parse_bearer)
        .ok_or(GateRejection::MissingBearer)?;
    if bool::from(presented.as_bytes().ct_eq(token.as_bytes())) {
        Ok(())
    } else {
        Err(GateRejection::BadToken)
    }
}

fn json_error(status: StatusCode, error: &str) -> Response {
    let body = serde_json::json!({ "error": error }).to_string();
    let len = body.len();
    let mut r = Response::new(Body::from(body));
    *r.status_mut() = status;
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    r
}

/// Connect protocol error body (`{"code","message"}`) with matching HTTP status.
fn connect_error(status: StatusCode, code: &str, message: &str) -> Response {
    let body = serde_json::json!({ "code": code, "message": message }).to_string();
    let mut r = Response::new(Body::from(body));
    *r.status_mut() = status;
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    r
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Codec {
    Json,
    Proto,
}

fn ok_response(codec: Codec, body: Vec<u8>) -> Response {
    let mut r = Response::new(Body::from(body));
    let ct = match codec {
        Codec::Json => "application/json",
        Codec::Proto => "application/proto",
    };
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static(ct));
    r
}

fn decode_req<T: Message + serde::de::DeserializeOwned>(codec: Codec, body: &Bytes) -> Result<T, String> {
    match codec {
        Codec::Proto => T::decode(body).map_err(|e| e.to_string()),
        Codec::Json => {
            if body.iter().all(|b| b.is_ascii_whitespace()) {
                Ok(T::default())
            } else {
                serde_json::from_slice(body).map_err(|e| e.to_string())
            }
        }
    }
}

fn encode_resp<T: Message + serde::Serialize>(codec: Codec, m: &T) -> Vec<u8> {
    match codec {
        Codec::Proto => m.encode_to_vec(),
        Codec::Json => serde_json::to_vec(m).unwrap_or_default(),
    }
}

async fn handle(
    State(st): State<Arc<BridgeState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request,
) -> Response {
    if let Err(rej) = check_gate(peer.ip(), req.headers(), &st.expected_host, &st.token) {
        tracing::warn!("host bridge rejected request from {peer}: {}", rej.reason());
        let word = if rej.status() == StatusCode::FORBIDDEN { "forbidden" } else { "unauthorized" };
        return json_error(rej.status(), word);
    }
    let path = req.uri().path().to_string();
    if path != proto::PATH_GET_UPDATE_STATUS && path != proto::PATH_APPLY_UPDATE {
        return connect_error(StatusCode::NOT_FOUND, "unimplemented", "unknown procedure");
    }
    if req.method() != Method::POST {
        return connect_error(StatusCode::METHOD_NOT_ALLOWED, "unimplemented", "POST required");
    }
    let ct = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(';').next().unwrap_or("").trim().to_ascii_lowercase())
        .unwrap_or_default();
    let codec = match ct.as_str() {
        "application/json" => Codec::Json,
        "application/proto" => Codec::Proto,
        _ => return connect_error(StatusCode::UNSUPPORTED_MEDIA_TYPE, "unknown", "unsupported content-type"),
    };
    let body = match axum::body::to_bytes(req.into_body(), MAX_BODY).await {
        Ok(b) => b,
        Err(_) => return connect_error(StatusCode::BAD_REQUEST, "invalid_argument", "body too large"),
    };
    if path == proto::PATH_GET_UPDATE_STATUS {
        if let Err(e) = decode_req::<proto::GetUpdateStatusRequest>(codec, &body) {
            return connect_error(StatusCode::BAD_REQUEST, "invalid_argument", &e);
        }
        let resp = proto::GetUpdateStatusResponse { status: Some(st.provider.get_update_status()) };
        ok_response(codec, encode_resp(codec, &resp))
    } else {
        if let Err(e) = decode_req::<proto::ApplyUpdateRequest>(codec, &body) {
            return connect_error(StatusCode::BAD_REQUEST, "invalid_argument", &e);
        }
        let resp = proto::ApplyUpdateResponse { accepted: st.provider.apply_update() };
        ok_response(codec, encode_resp(codec, &resp))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct Fake;
    impl UpdateStatusProvider for Fake {
        fn get_update_status(&self) -> proto::UpdateStatus {
            proto::UpdateStatus {
                current_version: "2.26.0".into(),
                latest_version: "2.27.0".into(),
                update_available: true,
            }
        }
        fn apply_update(&self) -> bool {
            true
        }
    }

    async fn raw(port: u16, req: String, body: &[u8]) -> (u16, Vec<u8>) {
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s.write_all(req.as_bytes()).await.unwrap();
        s.write_all(body).await.unwrap();
        let mut out = Vec::new();
        s.read_to_end(&mut out).await.unwrap();
        let text = String::from_utf8_lossy(&out);
        let status: u16 = text[9..12].parse().unwrap();
        let idx = out.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        (status, out[idx..].to_vec())
    }

    fn req(path: &str, host: &str, auth: Option<&str>, ct: &str, len: usize) -> String {
        let mut r = format!("POST {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nContent-Type: {ct}\r\nContent-Length: {len}\r\n");
        if let Some(a) = auth {
            r.push_str(&format!("Authorization: {a}\r\n"));
        }
        r.push_str("\r\n");
        r
    }

    #[test]
    fn bearer_parsing() {
        assert_eq!(parse_bearer("  bearer abc "), Some("abc"));
        assert_eq!(parse_bearer("Bearer a b"), None);
        assert_eq!(parse_bearer("Basic abc"), None);
        assert_eq!(parse_bearer("Bearer"), None);
        assert_eq!(generate_token().len(), 64);
    }

    #[tokio::test]
    async fn gate_and_rpcs() {
        let b = HostBridge::start_with_token(Arc::new(Fake), "t0k".into()).await.unwrap();
        let p = b.port();
        let host = format!("127.0.0.1:{p}");

        let (s, body) = raw(p, req("/nope", "localhost:1", Some("Bearer t0k"), "application/json", 0), b"").await;
        assert_eq!(s, 403);
        assert_eq!(body, br#"{"error":"forbidden"}"#);

        let (s, _) = raw(p, req("/nope", &host, None, "application/json", 0), b"").await;
        assert_eq!(s, 401);
        let (s, _) = raw(p, req("/nope", &host, Some("Bearer wrong"), "application/json", 0), b"").await;
        assert_eq!(s, 401);

        let (s, body) = raw(p, req(proto::PATH_GET_UPDATE_STATUS, &host, Some("Bearer t0k"), "application/json", 2), b"{}").await;
        assert_eq!(s, 200);
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["status"]["latestVersion"], "2.27.0");
        assert_eq!(v["status"]["updateAvailable"], true);

        let (s, body) = raw(p, req(proto::PATH_APPLY_UPDATE, &host, Some("Bearer t0k"), "application/proto", 0), b"").await;
        assert_eq!(s, 200);
        assert!(proto::ApplyUpdateResponse::decode(&body).unwrap().accepted);

        // Missing Host header -> 403.
        let r = String::from(
            "POST /x HTTP/1.1\r\nConnection: close\r\nAuthorization: Bearer t0k\r\nContent-Length: 0\r\n\r\n",
        );
        let (s, _) = raw(p, r, b"").await;
        assert!(s == 403 || s == 400, "got {s}");
        // localhost:<port> is not the exact host -> 403.
        let (s, _) = raw(p, req("/x", &format!("localhost:{p}"), Some("Bearer t0k"), "application/json", 0), b"").await;
        assert_eq!(s, 403);
        // Token with trailing garbage counts as missing -> 401.
        let (s, body) = raw(p, req("/x", &host, Some("Bearer t0k extra"), "application/json", 0), b"").await;
        assert_eq!(s, 401);
        assert_eq!(body, br#"{"error":"unauthorized"}"#);
        // Authenticated but unknown path -> 404 only after the gate.
        let (s, _) = raw(p, req("/x", &host, Some("bearer t0k"), "application/json", 0), b"").await;
        assert_eq!(s, 404);

        // proto happy path for GetUpdateStatus.
        let reqb = proto::GetUpdateStatusRequest {}.encode_to_vec();
        let (s, body) = raw(p, req(proto::PATH_GET_UPDATE_STATUS, &host, Some("Bearer t0k"), "application/proto", reqb.len()), &reqb).await;
        assert_eq!(s, 200);
        let st = proto::GetUpdateStatusResponse::decode(&body).unwrap().status.unwrap();
        assert_eq!(st.current_version, "2.26.0");
        assert!(st.update_available);

        // json happy path for ApplyUpdate.
        let (s, body) = raw(p, req(proto::PATH_APPLY_UPDATE, &host, Some("Bearer t0k"), "application/json; charset=utf-8", 2), b"{}").await;
        assert_eq!(s, 200);
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["accepted"], true);

        b.shutdown().await;
    }

    #[tokio::test]
    async fn error_headers() {
        let b = HostBridge::start_with_token(Arc::new(Fake), generate_token()).await.unwrap();
        let p = b.port();
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", p)).await.unwrap();
        s.write_all(req("/", &format!("127.0.0.1:{p}"), None, "application/json", 0).as_bytes()).await.unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).await.unwrap();
        let lower = out.to_ascii_lowercase();
        assert!(lower.starts_with("http/1.1 401"));
        assert!(lower.contains("content-type: application/json"));
        assert!(lower.contains("cache-control: no-store"));
        assert!(lower.contains("content-length: 24"));
        b.shutdown().await;
    }

    #[test]
    fn gate_unit() {
        use std::net::{Ipv4Addr, Ipv6Addr};
        assert!(is_allowed_peer(IpAddr::V4(Ipv4Addr::LOCALHOST)));
        assert!(is_allowed_peer(IpAddr::V6(Ipv6Addr::LOCALHOST)));
        assert!(is_allowed_peer("::ffff:127.0.0.1".parse().unwrap()));
        assert!(!is_allowed_peer("127.0.0.2".parse().unwrap()));
        assert!(!is_allowed_peer("10.0.0.1".parse().unwrap()));
        let mut h = HeaderMap::new();
        h.insert(header::HOST, HeaderValue::from_static("127.0.0.1:5"));
        h.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer tok"));
        assert_eq!(check_gate("10.0.0.1".parse().unwrap(), &h, "127.0.0.1:5", "tok"), Err(GateRejection::NonLoopbackPeer));
        assert_eq!(check_gate(IpAddr::V4(Ipv4Addr::LOCALHOST), &h, "127.0.0.1:6", "tok"), Err(GateRejection::BadHost));
        assert_eq!(check_gate(IpAddr::V4(Ipv4Addr::LOCALHOST), &h, "127.0.0.1:5", "tok2"), Err(GateRejection::BadToken));
        assert_eq!(check_gate(IpAddr::V4(Ipv4Addr::LOCALHOST), &h, "127.0.0.1:5", "tok"), Ok(()));
        let t = generate_token();
        assert!(t.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')));
    }
}
