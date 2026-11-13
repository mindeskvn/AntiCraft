//! A provider profile's model list ("Load models" on the Providers tab).
//!
//! Each wire lists its models its own way:
//! - OpenAI-compatible: `GET {base}/models` with the key as a bearer, answering
//!   `{"data":[{"id":…}]}`. Gateways that answer `{"models":[…]}` instead (Ollama-style, or
//!   CLIProxyAPI's Codex shape) are read too.
//! - Anthropic: `GET {base}/models?limit=1000` with `x-api-key` and `anthropic-version`, paged
//!   with `has_more` and `last_id`.
//! - Gemini Developer API: `GET {base}/models?pageSize=1000&key=…`, paged with `nextPageToken`.
//!   Only models that generate content are kept, without their `models/` prefix.
//!
//! A base URL pasted without its version (`http://localhost:8317/`) is common, so a 404 there is
//! asked again once under the version (`/v1`, `/v1beta` for Gemini). Every message is user-facing
//! and never carries the key.

use std::time::Duration;

use serde_json::Value;

use super::{provider_message, read_limited, scrub, ProviderKind, ProviderProfile};

/// How long one page may take. A list is small; a server slower than this is not answering.
const TIMEOUT: Duration = Duration::from_secs(30);

/// Bytes of one page kept. OpenRouter's list, the largest known, is about 1 MiB with descriptions.
const MAX_PAGE_BYTES: usize = 8 * 1024 * 1024;

/// Pages followed. Each asks for the most the API allows, so no real list comes near this.
const MAX_PAGES: usize = 10;

/// Most ids kept, here and by `Settings::sanitized` for a hand-edited file.
pub const MAX_MODELS: usize = 1000;

/// Longest id kept. Ids go into request bodies (and Gemini's URL path).
const MAX_ID_LEN: usize = 200;

/// What a load found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedModels {
    /// Sorted, deduplicated, at most [`MAX_MODELS`].
    pub models: Vec<String>,
    /// The list's URL (never with the key), shown under the list: it tells where a base URL
    /// without its version was answered.
    pub from: String,
}

/// Why one attempt failed.
enum ListError {
    /// 404 at this URL: worth asking again under the version.
    NotFound(String),
    Other(String),
}

/// List the models `profile`'s endpoint offers, with its key.
pub async fn list_models(http: &reqwest::Client, profile: &ProviderProfile) -> Result<LoadedModels, String> {
    let key = profile.api_key.trim();
    if key.is_empty() {
        return Err("Enter the API key first.".into());
    }
    let base = list_base(profile);
    let first = fetch(http, profile.kind, &base, key).await;
    let result = match first {
        Err(ListError::NotFound(url)) if !ends_with_version(&base) => {
            let versioned = format!("{base}/{}", version_segment(profile.kind));
            match fetch(http, profile.kind, &versioned, key).await {
                // Both missing: the first is the URL the base names.
                Err(ListError::NotFound(_)) => Err(ListError::NotFound(url)),
                other => other,
            }
        }
        other => other,
    };
    result.map_err(|e| match e {
        ListError::NotFound(url) => format!("{url} was not found (HTTP 404). Check the base URL, or type the model id."),
        ListError::Other(message) => message,
    })
}

/// Normalise a list of ids: trimmed, only ids a request can carry (printable ASCII, no spaces,
/// at most [`MAX_ID_LEN`]), the first of each duplicate, at most [`MAX_MODELS`].
///
/// Printable ASCII also keeps out what could make one row look like another (bidirectional
/// overrides, zero-width characters): the ids come from a server the user only half trusts.
pub fn normalize(ids: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for id in ids {
        let id = id.trim();
        if valid_model_id(id) && !out.iter().any(|m| m == id) {
            out.push(id.to_owned());
            if out.len() == MAX_MODELS {
                break;
            }
        }
    }
    out
}

/// A model id a provider profile can send: printable ASCII without spaces, at most
/// [`MAX_ID_LEN`]. Looser than an account's (`auth::external::valid_model_id`): gateways name
/// models with more characters than the subscriptions do.
pub fn valid_model_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_ID_LEN && id.chars().all(|c| c.is_ascii_graphic())
}

/// The base the list hangs off: the profile's endpoint without a pasted chat route.
fn list_base(profile: &ProviderProfile) -> String {
    let base = profile.resolved_base_url();
    // The OpenAI dialect accepts a base that names the whole chat route (`openai::Wire::endpoint`).
    match base.strip_suffix("/chat/completions") {
        Some(root) => root.trim_end_matches('/').to_owned(),
        None => base,
    }
}

/// The base's last path segment is an API version (`v1`, `v1beta`, `v2`).
fn ends_with_version(base: &str) -> bool {
    let path = url::Url::parse(base).map(|u| u.path().to_owned()).unwrap_or_default();
    let last = path.trim_end_matches('/').rsplit('/').next().unwrap_or_default();
    let mut chars = last.chars();
    chars.next() == Some('v') && chars.next().is_some_and(|c| c.is_ascii_digit())
}

/// Where a wire keeps its version: the Gemini API's models are under `v1beta`.
fn version_segment(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::GeminiApi => "v1beta",
        _ => "v1",
    }
}

/// Every page of the list under `base`, normalised.
async fn fetch(http: &reqwest::Client, kind: ProviderKind, base: &str, key: &str) -> Result<LoadedModels, ListError> {
    let from = format!("{base}/models");
    let mut ids: Vec<String> = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let mut url = url::Url::parse(&from).map_err(|e| ListError::Other(format!("The base URL is not a valid URL: {e}")))?;
        {
            let mut q = url.query_pairs_mut();
            match kind {
                ProviderKind::Anthropic => {
                    q.append_pair("limit", "1000");
                    if let Some(after) = &cursor {
                        q.append_pair("after_id", after);
                    }
                }
                ProviderKind::GeminiApi => {
                    q.append_pair("pageSize", "1000");
                    if let Some(token) = &cursor {
                        q.append_pair("pageToken", token);
                    }
                    // In the query, as the Gemini dialect sends it: a gateway that works for turns
                    // accepts it there.
                    q.append_pair("key", key);
                }
                _ => {}
            }
        }
        let page = get_json(http, kind, url, &from, key).await?;
        let parsed = match kind {
            ProviderKind::Anthropic => anthropic_page(&page),
            ProviderKind::GeminiApi => gemini_page(&page),
            _ => openai_ids(&page).map(|ids| (ids, None)),
        };
        let Some((page_ids, next)) = parsed else {
            return Err(ListError::Other(format!("{from} did not answer with a model list.")));
        };
        ids.extend(page_ids);
        match next {
            // A cursor that does not move would ask for the same page again.
            Some(next) if cursor.as_deref() != Some(next.as_str()) => cursor = Some(next),
            _ => break,
        }
    }
    ids.sort_by(|a, b| a.to_ascii_lowercase().cmp(&b.to_ascii_lowercase()).then_with(|| a.cmp(b)));
    Ok(LoadedModels { models: normalize(ids), from })
}

/// One GET, as JSON. `shown` is the URL a message may name (never with the key in its query).
async fn get_json(http: &reqwest::Client, kind: ProviderKind, url: url::Url, shown: &str, key: &str) -> Result<Value, ListError> {
    let rb = http.get(url).timeout(TIMEOUT).header(reqwest::header::ACCEPT, "application/json");
    let rb = match kind {
        ProviderKind::Anthropic => rb.header("x-api-key", key).header("anthropic-version", super::anthropic::ANTHROPIC_VERSION),
        // The key is in the query.
        ProviderKind::GeminiApi => rb,
        _ => rb.bearer_auth(key),
    };
    let resp = rb.send().await.map_err(|e| {
        // `without_url` keeps the query, and with it a Gemini key, out of the message.
        ListError::Other(format!("Could not reach {shown}: {}", scrub(&e.without_url().to_string(), &[key])))
    })?;
    let status = resp.status().as_u16();
    let bytes = read_limited(resp, MAX_PAGE_BYTES).await;
    if status == 404 {
        return Err(ListError::NotFound(shown.to_owned()));
    }
    if !(200..300).contains(&status) {
        let message = scrub(&provider_message(&String::from_utf8_lossy(&bytes)), &[key]);
        let message = if message.is_empty() { String::new() } else { format!(": {message}") };
        return Err(ListError::Other(match status {
            401 | 403 => format!("The provider refused the API key (HTTP {status}){message}"),
            _ => format!("{shown} answered HTTP {status}{message}"),
        }));
    }
    serde_json::from_slice(&bytes).map_err(|_| ListError::Other(format!("{shown} did not answer with a model list.")))
}

/// An OpenAI-style list: `data[]` or `models[]` (or a bare array), each entry an id string or an
/// object with `id` (else `name`). `None` when the shape is none of these.
fn openai_ids(v: &Value) -> Option<Vec<String>> {
    let list = v.get("data").or_else(|| v.get("models")).unwrap_or(v).as_array()?;
    Some(
        list.iter()
            .filter_map(|m| m.as_str().or_else(|| m.get("id").and_then(Value::as_str)).or_else(|| m.get("name").and_then(Value::as_str)))
            .map(|id| id.strip_prefix("models/").unwrap_or(id).to_owned())
            .collect(),
    )
}

/// One page of Anthropic's list and the cursor of the next one.
fn anthropic_page(v: &Value) -> Option<(Vec<String>, Option<String>)> {
    let ids = v.get("data")?.as_array()?.iter().filter_map(|m| m.get("id").and_then(Value::as_str)).map(str::to_owned).collect();
    let more = v.get("has_more").and_then(Value::as_bool).unwrap_or(false);
    let next = v.get("last_id").and_then(Value::as_str).filter(|_| more).map(str::to_owned);
    Some((ids, next))
}

/// One page of the Gemini API's list (models that generate content) and the next page's token.
///
/// An entry without `supportedGenerationMethods` is kept: a gateway that leaves the field out
/// serves the model for chat.
fn gemini_page(v: &Value) -> Option<(Vec<String>, Option<String>)> {
    let ids = v
        .get("models")?
        .as_array()?
        .iter()
        .filter(|m| {
            m.get("supportedGenerationMethods").and_then(Value::as_array).is_none_or(|methods| {
                methods.iter().any(|x| matches!(x.as_str(), Some("generateContent" | "streamGenerateContent")))
            })
        })
        .filter_map(|m| m.get("name").and_then(Value::as_str))
        .map(|name| name.strip_prefix("models/").unwrap_or(name).to_owned())
        .collect();
    let next = v.get("nextPageToken").and_then(Value::as_str).filter(|t| !t.is_empty()).map(str::to_owned);
    Some((ids, next))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::{Query, State};
    use axum::http::{HeaderMap, StatusCode, Uri};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    /// One request the mock saw: its path, query and the auth headers.
    #[derive(Debug, Clone, Default)]
    struct Seen {
        path: String,
        query: HashMap<String, String>,
        bearer: Option<String>,
        api_key: Option<String>,
        version: Option<String>,
    }

    type Reply = Arc<dyn Fn(&Seen) -> (StatusCode, String) + Send + Sync>;

    /// A server that records every request and answers with `reply`.
    async fn server(reply: impl Fn(&Seen) -> (StatusCode, String) + Send + Sync + 'static) -> (String, Arc<Mutex<Vec<Seen>>>) {
        let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
        let state: (Arc<Mutex<Vec<Seen>>>, Reply) = (seen.clone(), Arc::new(reply));
        let app = axum::Router::new()
            .fallback(|State((log, reply)): State<(Arc<Mutex<Vec<Seen>>>, Reply)>, uri: Uri, Query(query): Query<HashMap<String, String>>, headers: HeaderMap| async move {
                let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).map(str::to_owned);
                let s = Seen {
                    path: uri.path().to_owned(),
                    query,
                    bearer: header("authorization"),
                    api_key: header("x-api-key"),
                    version: header("anthropic-version"),
                };
                log.lock().unwrap().push(s.clone());
                reply(&s)
            })
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), seen)
    }

    fn profile(kind: ProviderKind, base: &str) -> ProviderProfile {
        ProviderProfile {
            id: "p".into(),
            label: "P".into(),
            kind,
            model: String::new(),
            base_url: Some(base.to_owned()),
            api_key: "sk-list-secret".into(),
            max_output_tokens: None,
            context_window: None,
            models: Vec::new(),
        }
    }

    async fn load(p: &ProviderProfile) -> Result<LoadedModels, String> {
        tokio::time::timeout(Duration::from_secs(20), list_models(&reqwest::Client::new(), p)).await.expect("the load finished in time")
    }

    fn ok(body: serde_json::Value) -> (StatusCode, String) {
        (StatusCode::OK, body.to_string())
    }

    #[tokio::test]
    async fn an_openai_list_is_read_with_the_bearer_key_sorted_and_deduplicated() {
        let (base, seen) = server(|_| ok(serde_json::json!({"object": "list", "data": [{"id": "gpt-4o"}, {"id": "GPT-3.5"}, {"id": "gpt-4o"}, {"id": "bad id"}, {"id": "o3-mini"}]}))).await;
        let got = load(&profile(ProviderKind::OpenAi, &format!("{base}/v1"))).await.unwrap();
        assert_eq!(got.models, ["GPT-3.5", "gpt-4o", "o3-mini"], "sorted without regard to case, one of each, no id with a space");
        assert_eq!(got.from, format!("{base}/v1/models"));
        let s = seen.lock().unwrap()[0].clone();
        assert_eq!((s.path.as_str(), s.bearer.as_deref()), ("/v1/models", Some("Bearer sk-list-secret")));

        // A base that names the whole chat route lists from its root.
        let got = load(&profile(ProviderKind::OpenAi, &format!("{base}/v1/chat/completions"))).await.unwrap();
        assert_eq!(got.from, format!("{base}/v1/models"));
    }

    #[tokio::test]
    async fn gateway_shapes_are_read_too() {
        for body in [
            serde_json::json!({"models": [{"name": "models/llama3"}, {"id": "qwen2.5:7b"}]}),
            serde_json::json!({"models": ["llama3", "qwen2.5:7b"]}),
            serde_json::json!([{"id": "llama3"}, {"id": "qwen2.5:7b"}]),
        ] {
            let (base, _) = server(move |_| ok(body.clone())).await;
            let got = load(&profile(ProviderKind::OpenAi, &format!("{base}/v1"))).await.unwrap();
            assert_eq!(got.models, ["llama3", "qwen2.5:7b"]);
        }
        let (base, _) = server(|_| ok(serde_json::json!({"object": "list"}))).await;
        let err = load(&profile(ProviderKind::OpenAi, &format!("{base}/v1"))).await.unwrap_err();
        assert_eq!(err, format!("{base}/v1/models did not answer with a model list."));
        // An empty list is an answer; the dialog says so.
        let (base, _) = server(|_| ok(serde_json::json!({"data": []}))).await;
        assert!(load(&profile(ProviderKind::OpenAi, &format!("{base}/v1"))).await.unwrap().models.is_empty());
    }

    #[tokio::test]
    async fn anthropic_pages_are_followed_with_the_key_header() {
        let (base, seen) = server(|s| match s.query.get("after_id").map(String::as_str) {
            None => ok(serde_json::json!({"data": [{"type": "model", "id": "claude-opus-5-5"}], "has_more": true, "last_id": "claude-opus-5-5"})),
            Some("claude-opus-5-5") => ok(serde_json::json!({"data": [{"type": "model", "id": "claude-haiku-5-5"}], "has_more": false, "last_id": "claude-haiku-5-5"})),
            Some(_) => (StatusCode::BAD_REQUEST, "{}".into()),
        })
        .await;
        let got = load(&profile(ProviderKind::Anthropic, &format!("{base}/v1"))).await.unwrap();
        assert_eq!(got.models, ["claude-haiku-5-5", "claude-opus-5-5"]);
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 2);
        for s in &seen {
            assert_eq!((s.path.as_str(), s.api_key.as_deref(), s.version.as_deref()), ("/v1/models", Some("sk-list-secret"), Some("2023-06-01")));
            assert_eq!(s.bearer, None, "a key profile sends no bearer");
            assert_eq!(s.query.get("limit").map(String::as_str), Some("1000"));
        }
    }

    #[tokio::test]
    async fn a_cursor_that_does_not_move_stops_the_paging() {
        let (base, seen) = server(|_| ok(serde_json::json!({"data": [{"id": "claude-x"}], "has_more": true, "last_id": "claude-x"}))).await;
        let got = load(&profile(ProviderKind::Anthropic, &format!("{base}/v1"))).await.unwrap();
        assert_eq!(got.models, ["claude-x"]);
        assert_eq!(seen.lock().unwrap().len(), 2, "the second page named the same cursor");
    }

    #[tokio::test]
    async fn gemini_keeps_chat_models_without_their_prefix() {
        let (base, seen) = server(|s| match s.query.get("pageToken").map(String::as_str) {
            None => ok(serde_json::json!({
                "models": [
                    {"name": "models/gemini-2.5-pro", "supportedGenerationMethods": ["generateContent", "countTokens"]},
                    {"name": "models/text-embedding-004", "supportedGenerationMethods": ["embedContent"]},
                ],
                "nextPageToken": "t2",
            })),
            Some(_) => ok(serde_json::json!({"models": [{"name": "models/gemini-3-flash"}]})),
        })
        .await;
        let got = load(&profile(ProviderKind::GeminiApi, &format!("{base}/v1beta"))).await.unwrap();
        assert_eq!(got.models, ["gemini-2.5-pro", "gemini-3-flash"], "no embedding model");
        assert_eq!(got.from, format!("{base}/v1beta/models"), "the key is not in the URL shown");
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen[0].query.get("key").map(String::as_str), Some("sk-list-secret"));
        assert_eq!(seen[1].query.get("pageToken").map(String::as_str), Some("t2"));
    }

    #[tokio::test]
    async fn a_base_without_its_version_is_asked_again_under_it() {
        let (base, seen) = server(|s| match s.path.as_str() {
            "/v1/models" => ok(serde_json::json!({"data": [{"id": "claude-sonnet-5-5"}]})),
            _ => (StatusCode::NOT_FOUND, "404 page not found".into()),
        })
        .await;
        let got = load(&profile(ProviderKind::Anthropic, &format!("{base}/"))).await.unwrap();
        assert_eq!(got.models, ["claude-sonnet-5-5"]);
        assert_eq!(got.from, format!("{base}/v1/models"), "the list says where it was found");
        assert_eq!(seen.lock().unwrap().iter().map(|s| s.path.clone()).collect::<Vec<_>>(), ["/models", "/v1/models"]);

        // Gemini's models are under v1beta.
        let (base, seen) = server(|s| match s.path.as_str() {
            "/v1beta/models" => ok(serde_json::json!({"models": [{"name": "models/gemini-3-flash"}]})),
            _ => (StatusCode::NOT_FOUND, String::new()),
        })
        .await;
        assert_eq!(load(&profile(ProviderKind::GeminiApi, &base)).await.unwrap().models, ["gemini-3-flash"]);
        assert_eq!(seen.lock().unwrap().last().unwrap().path, "/v1beta/models");

        // Missing under both, or already versioned: one message, naming the URL the base gives.
        let (base, seen) = server(|_| (StatusCode::NOT_FOUND, String::new())).await;
        let err = load(&profile(ProviderKind::OpenAi, &base)).await.unwrap_err();
        assert_eq!(err, format!("{base}/models was not found (HTTP 404). Check the base URL, or type the model id."));
        assert_eq!(seen.lock().unwrap().len(), 2);
        let (base, seen) = server(|_| (StatusCode::NOT_FOUND, String::new())).await;
        assert!(load(&profile(ProviderKind::OpenAi, &format!("{base}/v1"))).await.is_err());
        assert_eq!(seen.lock().unwrap().len(), 1, "a versioned base is not asked again");
    }

    #[tokio::test]
    async fn errors_never_carry_the_key() {
        // A gateway that echoes the key back in its message.
        let (base, _) = server(|_| (StatusCode::UNAUTHORIZED, r#"{"error":{"message":"bad key sk-list-secret"}}"#.into())).await;
        let err = load(&profile(ProviderKind::OpenAi, &format!("{base}/v1"))).await.unwrap_err();
        assert_eq!(err, "The provider refused the API key (HTTP 401): bad key <redacted>");
        let (base, _) = server(|_| (StatusCode::INTERNAL_SERVER_ERROR, "upstream down".into())).await;
        let err = load(&profile(ProviderKind::GeminiApi, &format!("{base}/v1beta"))).await.unwrap_err();
        assert_eq!(err, format!("{base}/v1beta/models answered HTTP 500: upstream down"));
        assert!(!err.contains("sk-list-secret"), "the key in Gemini's query is not in the message");

        // Nothing listens: the message names the list's URL, not the one with the key.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let closed = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let err = load(&profile(ProviderKind::GeminiApi, &format!("{closed}/v1beta"))).await.unwrap_err();
        assert!(err.starts_with(&format!("Could not reach {closed}/v1beta/models: ")), "{err}");
        assert!(!err.contains("sk-list-secret"), "{err}");

        // No key, no request.
        let mut p = profile(ProviderKind::OpenAi, &closed);
        p.api_key = "  ".into();
        assert_eq!(load(&p).await.unwrap_err(), "Enter the API key first.");
    }

    #[test]
    fn normalize_keeps_ids_a_request_can_carry() {
        let ids = ["  gpt-4o ", "gpt-4o", "", "has space", "tab\tid", "\u{202e}gpt", "accounts/fireworks/models/llama-v3p1:405b@1+x"].map(String::from);
        assert_eq!(normalize(ids), ["gpt-4o", "accounts/fireworks/models/llama-v3p1:405b@1+x"]);
        assert_eq!(normalize(std::iter::once("x".repeat(MAX_ID_LEN + 1))), Vec::<String>::new());
        let many = normalize((0..MAX_MODELS + 50).map(|i| format!("m{i}")));
        assert_eq!(many.len(), MAX_MODELS);
        assert!(ends_with_version("http://h/v1") && ends_with_version("http://h/api/v1beta/") && ends_with_version("http://h/v2"));
        assert!(!ends_with_version("http://h/") && !ends_with_version("http://h/openai") && !ends_with_version("http://h/video"));
    }
}
