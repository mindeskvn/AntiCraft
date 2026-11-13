//! `plugin://` custom scheme proxy (SPEC §4.4).
//!
//! WebView2 cannot register arbitrary schemes through wry, so wry maps
//! `plugin` to `https://plugin.<authority>/...` (documented deviation: the
//! page origin differs from `plugin://<authority>`). The handler accepts both
//! forms and [`PLUGIN_URL_SHIM_JS`] rewrites literal `plugin://` URLs.
//!
//! Hardening over §4.4: authorities must map to loopback origins, targets may
//! not leave the origin, redirects are not followed, and credentials /
//! hop-by-hop / Set-Cookie headers are stripped.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use wry::http::{Request, Response, StatusCode};

/// Authority → original origin (e.g. `http://localhost:1234`).
#[derive(Clone, Default)]
pub struct AuthorityMap(Arc<RwLock<HashMap<String, String>>>);

impl AuthorityMap {
    /// Replace the whole map (`extensions:send-authorities`).
    pub fn replace(&self, entries: HashMap<String, String>) {
        let mut g = self.0.write().unwrap();
        g.clear();
        g.extend(entries);
    }

    pub fn get(&self, authority: &str) -> Option<String> {
        self.0.read().unwrap().get(authority).cloned()
    }
}

/// Authority from a request URI host (`<authority>` or wry's `plugin.<authority>`).
pub fn authority_of(uri: &wry::http::Uri) -> Option<String> {
    let host = uri.host()?;
    if uri.scheme_str() == Some("plugin") {
        return Some(host.to_string());
    }
    host.strip_prefix("plugin.").map(str::to_owned)
}

/// Accept only `http(s)://<loopback>:<port>` with no path, query, fragment or
/// credentials (AntiCraft hardening, see README). Returns the normalized
/// origin string.
pub fn validate_origin(raw: &str) -> Option<String> {
    let u = url::Url::parse(raw).ok()?;
    if !matches!(u.scheme(), "http" | "https") || !u.username().is_empty() || u.password().is_some() {
        return None;
    }
    let loopback = match u.host()? {
        url::Host::Domain(d) => d.eq_ignore_ascii_case("localhost"),
        url::Host::Ipv4(ip) => ip == std::net::Ipv4Addr::LOCALHOST,
        url::Host::Ipv6(ip) => ip == std::net::Ipv6Addr::LOCALHOST,
    };
    let port = u.port()?; // explicit port required
    if !loopback || !(u.path().is_empty() || u.path() == "/") || u.query().is_some() || u.fragment().is_some() {
        return None;
    }
    Some(format!("{}://{}:{port}", u.scheme(), u.host_str()?))
}

/// Resolve `path+query` against the stored origin; the path replaces any path
/// in the origin and the fragment is dropped. Returns `None` if resolution
/// would leave the origin's host/port (e.g. a `//evil` path). This origin
/// check is an AntiCraft hardening over §4.4 (documented in the README).
pub fn resolve_target(origin: &str, path_and_query: &str) -> Option<url::Url> {
    let base = url::Url::parse(origin).ok()?;
    let mut t = base.join(path_and_query).ok()?;
    t.set_fragment(None);
    (t.origin() == base.origin()).then_some(t)
}

/// Request headers never forwarded upstream (credentials and hop-by-hop).
const DROP_REQUEST_HEADERS: &[&str] = &[
    "host",
    "cookie",
    "authorization",
    "proxy-authorization",
    "proxy-connection",
    "connection",
    "keep-alive",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Response headers never copied back (hop-by-hop, cookies, length that the
/// re-buffered body would contradict).
const DROP_RESPONSE_HEADERS: &[&str] = &[
    "set-cookie",
    "set-cookie2",
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "content-length",
];

fn dropped(list: &[&str], name: &str) -> bool {
    list.iter().any(|h| h.eq_ignore_ascii_case(name)) || name.to_ascii_lowercase().starts_with("proxy-")
}

/// HTTP client for the proxy: never follows redirects (a 3xx is returned to
/// the page as-is), no cookie store, bounded timeouts.
pub fn proxy_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(10))
        .read_timeout(std::time::Duration::from_secs(120))
        .build()
        .unwrap_or_default()
}

/// Script that makes literal `plugin://<authority>/...` URLs built by the web
/// UI reach wry's `https://plugin.<authority>/...` handler (fetch, XHR,
/// iframe/img/script `src`, `window.open`).
pub const PLUGIN_URL_SHIM_JS: &str = include_str!("plugin_shim.js");

fn empty(status: StatusCode) -> Response<Vec<u8>> {
    let mut r = Response::new(Vec::new());
    *r.status_mut() = status;
    r
}

/// Proxy one request. Unknown authority → 404, fetch error → 500.
pub async fn proxy(client: &reqwest::Client, map: &AuthorityMap, req: Request<Vec<u8>>) -> Response<Vec<u8>> {
    let Some(authority) = authority_of(req.uri()) else { return empty(StatusCode::NOT_FOUND) };
    let Some(origin) = map.get(&authority) else { return empty(StatusCode::NOT_FOUND) };
    let pq = req.uri().path_and_query().map(|p| p.as_str()).unwrap_or("/");
    let Some(target) = resolve_target(&origin, pq) else { return empty(StatusCode::NOT_FOUND) };
    let method = reqwest::Method::from_bytes(req.method().as_str().as_bytes()).unwrap_or(reqwest::Method::GET);
    let mut rb = client.request(method, target);
    for (k, v) in req.headers() {
        if dropped(DROP_REQUEST_HEADERS, k.as_str()) {
            continue;
        }
        rb = rb.header(k.as_str(), v.as_bytes());
    }
    let body = req.into_body();
    if !body.is_empty() {
        rb = rb.body(body);
    }
    match rb.send().await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let headers: Vec<(String, Vec<u8>)> = resp
                .headers()
                .iter()
                .filter(|(k, _)| !dropped(DROP_RESPONSE_HEADERS, k.as_str()))
                .map(|(k, v)| (k.as_str().to_owned(), v.as_bytes().to_vec()))
                .collect();
            match resp.bytes().await {
                Ok(bytes) => {
                    let mut b = Response::builder().status(status);
                    for (k, v) in headers {
                        b = b.header(k, v);
                    }
                    b.body(bytes.to_vec()).unwrap_or_else(|_| empty(StatusCode::INTERNAL_SERVER_ERROR))
                }
                Err(_) => empty(StatusCode::INTERNAL_SERVER_ERROR),
            }
        }
        Err(e) => {
            tracing::warn!("plugin:// fetch failed: {e}");
            empty(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolution() {
        let t = resolve_target("http://localhost:9/base/", "/a/b?x=1").unwrap();
        assert_eq!(t.as_str(), "http://localhost:9/a/b?x=1");
        assert!(resolve_target("http://localhost:9", "//evil.com/x").is_none());
        let u: wry::http::Uri = "https://plugin.abc/x".parse().unwrap();
        assert_eq!(authority_of(&u).as_deref(), Some("abc"));
        let u: wry::http::Uri = "plugin://abc/x".parse().unwrap();
        assert_eq!(authority_of(&u).as_deref(), Some("abc"));
    }

    #[test]
    fn origin_validation_and_headers() {
        assert_eq!(validate_origin("http://localhost:1234").as_deref(), Some("http://localhost:1234"));
        assert_eq!(validate_origin("https://127.0.0.1:9/").as_deref(), Some("https://127.0.0.1:9"));
        assert_eq!(validate_origin("http://[::1]:5").as_deref(), Some("http://[::1]:5"));
        assert!(validate_origin("http://169.254.169.254:80").is_none());
        assert!(validate_origin("http://localhost").is_none());
        assert!(validate_origin("http://localhost:1/x").is_none());
        assert!(validate_origin("http://u:p@localhost:1").is_none());
        assert!(validate_origin("file:///c:/x").is_none());
        assert!(dropped(DROP_REQUEST_HEADERS, "Cookie"));
        assert!(dropped(DROP_REQUEST_HEADERS, "Proxy-Foo"));
        assert!(!dropped(DROP_REQUEST_HEADERS, "accept"));
        assert!(dropped(DROP_RESPONSE_HEADERS, "Set-Cookie"));
        assert!(PLUGIN_URL_SHIM_JS.contains("https://plugin."));
    }

    #[test]
    fn map_replace() {
        let m = AuthorityMap::default();
        m.replace([("a".to_string(), "http://x".to_string())].into());
        m.replace([("b".to_string(), "http://y".to_string())].into());
        assert!(m.get("a").is_none());
        assert_eq!(m.get("b").as_deref(), Some("http://y"));
    }
}
