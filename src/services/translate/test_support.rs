//! Fixtures the translation service's unit tests share: the mock provider
//! server and the translators that talk to it.

use std::sync::Arc;

use axum::{
    Router,
    body::Body,
    extract::State,
    http::{HeaderMap, Method, Request, StatusCode, header},
    response::Response,
    routing::{any, get},
};
use serde_json::json;
use tokio::{net::TcpListener, sync::Mutex, task::JoinHandle};

use super::{JSON_MIME, Translator};
use crate::{
    config::{Config, TRANSLATION_API_KEY_VAR, TRANSLATION_API_URL_VAR, TRANSLATION_PROVIDER_VAR},
    http_client::HttpClient,
};

/// The `User-Agent` these clients send.
pub(super) const TEST_UA: &str = "Wired (https://wired.test)";
/// The key the tests configure a provider with.
pub(super) const TEST_KEY: &str = "test-provider-key";

/// One provider request, as the mock recorded it.
#[derive(Clone, Debug)]
pub(super) struct Seen {
    /// Method and path with query, e.g. `POST /v2/translate`.
    pub(super) target: String,
    /// The `content-type` header, or the empty string.
    pub(super) content_type: String,
    /// The `authorization` header, or the empty string.
    pub(super) authorization: String,
    /// The request body as text.
    pub(super) body: String,
}

/// The mock provider: what it recorded, and what it has been told to
/// answer with.
#[derive(Debug, Default)]
pub(super) struct Mock {
    /// Every request the mini-server has answered.
    pub(super) seen: Mutex<Vec<Seen>>,
    /// Status to answer every call with.
    pub(super) status: StatusCode,
    /// Body to answer a translate call with.
    pub(super) translate_body: String,
    /// Body to answer `GET /languages` with.
    pub(super) languages_body: String,
    /// Body to answer `POST /v2/languages` with.
    pub(super) deepl_languages_body: String,
}

impl Mock {
    /// A mock answering success with `translate_body`, plus the fixed
    /// two-language lists.
    pub(super) fn new(translate_body: String) -> Self {
        Self {
            status: StatusCode::OK,
            translate_body,
            languages_body: json!([{"code": "en"}, {"code": "de"}]).to_string(),
            deepl_languages_body: json!([{"language": "EN"}, {"language": "DE"}]).to_string(),
            ..Self::default()
        }
    }

    /// The requests this mock has answered so far.
    pub(super) async fn requests(&self) -> Vec<Seen> {
        self.seen.lock().await.clone()
    }
}

/// Serves `mock` on a loopback port, returning its origin URL and the task
/// that keeps it alive.
pub(super) async fn spawn(mock: Arc<Mock>) -> (String, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new()
        .route("/languages", get(record))
        .route("/{*rest}", any(record))
        .with_state(mock);
    let server = tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, app).await {
            tracing::debug!("mock provider stopped: {err}");
        }
    });
    (format!("http://{addr}"), server)
}

/// Records one request and answers it per the mock's script.
async fn record(State(mock): State<Arc<Mock>>, method: Method, request: Request<Body>) -> Response {
    let (parts, body) = request.into_parts();
    let bytes = http_body_util::BodyExt::collect(body)
        .await
        .unwrap()
        .to_bytes();
    let path = parts.uri.path().to_owned();
    let seen = Seen {
        target: format!("{method} {}", parts.uri),
        content_type: header_text(&parts.headers, header::CONTENT_TYPE),
        authorization: header_text(&parts.headers, header::AUTHORIZATION),
        body: String::from_utf8_lossy(&bytes).into_owned(),
    };
    mock.seen.lock().await.push(seen);
    let text = if method == Method::GET {
        mock.languages_body.clone()
    } else if path.ends_with("/v2/languages") {
        mock.deepl_languages_body.clone()
    } else {
        mock.translate_body.clone()
    };
    Response::builder()
        .status(mock.status)
        .header(header::CONTENT_TYPE, JSON_MIME)
        .body(Body::from(text))
        .unwrap()
}

/// The visible text of header `name`, empty when absent or unreadable.
fn header_text(headers: &HeaderMap, name: header::HeaderName) -> String {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned()
}

/// The config of one provider, reaching it at `api_url`
/// (`None` leaves that setting unset).
pub(super) fn provider_config(
    provider: &str,
    api_url: Option<&str>,
    api_key: Option<&str>,
) -> Config {
    Config::from_lookup(|key| match key {
        crate::config::DATABASE_URL_VAR => Some("postgres://localhost/wired".to_owned()),
        crate::config::BASE_URL_VAR => Some("https://test.example".to_owned()),
        TRANSLATION_PROVIDER_VAR => Some(provider.to_owned()),
        TRANSLATION_API_URL_VAR => api_url.map(str::to_owned),
        TRANSLATION_API_KEY_VAR => api_key.map(str::to_owned),
        _ => None,
    })
    .unwrap()
}

/// The translator of one provider configuration, talking to `api_url`.
pub(super) fn provider_translator(
    provider: &str,
    api_url: &str,
    api_key: Option<&str>,
) -> Translator {
    let config = provider_config(provider, Some(api_url), api_key);
    Translator::from_config(&config, &HttpClient::new_for_tests(TEST_UA))
}

/// The JSON body of a `LibreTranslate` answer.
pub(super) fn libre_body(text: &str) -> String {
    json!({ "translatedText": text }).to_string()
}

/// The JSON body of a `DeepL` answer.
pub(super) fn deepl_body(text: &str, detected: &str) -> String {
    json!({ "translations": [{ "text": text, "detected_source_language": detected }] }).to_string()
}
