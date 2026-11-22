//! The mock translation provider, the fixtures and the request helpers the
//! translation tests share. The test modules are its children, so they reach
//! every helper through `use super::*`.

mod languages;
mod translate_refusals;
mod translate_route;

use std::sync::Arc;

use axum::{
    Router,
    body::Body,
    extract::State,
    http::{HeaderMap, Request, StatusCode, header},
    response::Response,
    routing::{get, post},
};
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::{net::TcpListener, sync::Mutex, task::JoinHandle};
use wired_be::{
    config::TEST_BASE_URL,
    models::{
        status::{self, NewStatus, Status, Visibility},
        user::{NewUser, User},
    },
    test_db::TestDb,
};

use crate::common;

/// The 503 body of an instance with no translation provider.
const NOT_ENABLED: &str = "Translation is not enabled";
/// The 503 body of a provider that failed the call.
const UNAVAILABLE: &str = "Translation service unavailable";
/// The 404 every unreadable status is answered with.
const NOT_FOUND: &str = "Record not found";
/// The 401 `AuthUser` renders without any credentials.
const NO_TOKEN: &str = "Invalid credentials";
/// The 403 of a user token that lacks the endpoint's scope.
const NO_SCOPE: &str = "Insufficient permissions: read:statuses.";
/// The scope the route asks a user token for.
const SCOPES: &str = "read:statuses";
/// The id of a status that is not stored here.
const UNKNOWN_ID: &str = "01934567-89ab-7cde-f012-3456789abcde";
/// The key [`common::test_translation_config`] gives a `deepl` provider.
const TEST_KEY: &str = common::TRANSLATION_TEST_KEY;

/// One request the mock provider answered, as its peer saw it.
#[derive(Clone, Debug)]
struct Seen {
    /// The request method, e.g. `POST`.
    method: String,
    /// The path, without query.
    path: String,
    /// The path with its query, e.g. `/v2/languages?type=source`.
    target: String,
    /// The `authorization` header, or the empty string.
    authorization: String,
    /// The request body as text.
    body: String,
}

/// The mock provider: the routes both providers document, the answers a test
/// scripts for them, and every request it had to serve.
#[derive(Debug)]
struct Mock {
    /// Every request served, in arrival order.
    seen: Mutex<Vec<Seen>>,
    /// Status every route answers with.
    status: StatusCode,
    /// Body of `POST /translate`.
    libre_answer: String,
    /// Body of `GET /languages`.
    libre_languages: String,
    /// Body of `POST /v2/translate`.
    deepl_answer: String,
    /// Body of `POST /v2/languages`.
    deepl_languages: String,
}

impl Mock {
    /// A provider answering `LibreTranslate`'s `POST /translate` with
    /// `translated` and its `GET /languages` with English and German.
    fn libretranslate(translated: &str) -> Self {
        Self {
            seen: Mutex::new(Vec::new()),
            status: StatusCode::OK,
            libre_answer: json!({ "translatedText": translated }).to_string(),
            libre_languages: json!([{"code": "en"}, {"code": "de"}]).to_string(),
            deepl_answer: String::new(),
            deepl_languages: String::new(),
        }
    }

    /// A provider answering `DeepL`'s `POST /v2/translate` with `translated`
    /// (its source detected as English) and both language lists with English
    /// and Brazilian Portuguese.
    fn deepl(translated: &str) -> Self {
        Self {
            seen: Mutex::new(Vec::new()),
            status: StatusCode::OK,
            libre_answer: String::new(),
            libre_languages: String::new(),
            deepl_answer: json!({
                "translations": [{ "text": translated, "detected_source_language": "EN" }]
            })
            .to_string(),
            deepl_languages: json!([{"language": "EN"}, {"language": "PT-BR"}]).to_string(),
        }
    }

    /// The requests served so far.
    async fn requests(&self) -> Vec<Seen> {
        self.seen.lock().await.clone()
    }
}

/// Binds and serves `mock` on loopback, returning its origin URL and the task
/// that keeps it alive: a test detaches the handle and lets the provider live
/// as long as its own runtime does.
async fn spawn(mock: Arc<Mock>) -> (String, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new()
        .route("/translate", post(answer))
        .route("/languages", get(answer))
        .route("/v2/translate", post(answer))
        .route("/v2/languages", post(answer))
        .with_state(mock);
    let server = tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, app).await {
            tracing::debug!("mock provider stopped: {err}");
        }
    });
    (format!("http://{addr}"), server)
}

/// Records one request and answers it from the script of `mock`.
async fn answer(
    State(mock): State<Arc<Mock>>,
    headers: HeaderMap,
    request: Request<Body>,
) -> Response {
    let (parts, body) = request.into_parts();
    let bytes = http_body_util::BodyExt::collect(body)
        .await
        .unwrap()
        .to_bytes();
    let path = parts.uri.path().to_owned();
    let seen = Seen {
        method: parts.method.as_str().to_owned(),
        target: parts.uri.to_string(),
        authorization: header_text(&headers, header::AUTHORIZATION),
        body: String::from_utf8_lossy(&bytes).into_owned(),
        path: path.clone(),
    };
    mock.seen.lock().await.push(seen);
    let text = match path.as_str() {
        "/translate" => mock.libre_answer.clone(),
        "/languages" => mock.libre_languages.clone(),
        "/v2/translate" => mock.deepl_answer.clone(),
        _ => mock.deepl_languages.clone(),
    };
    Response::builder()
        .status(mock.status)
        .header(header::CONTENT_TYPE, "application/json")
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

/// The status a fixture posts: [`post_status`] inserts it, and the builder
/// methods change one part of it at a time.
#[derive(Clone, Debug)]
struct Fixture<'a> {
    /// Plain text of the post; [`NewStatus::local`] turns it into HTML.
    text: &'a str,
    /// Who may see it.
    visibility: Visibility,
    /// The language it claims; `None` stores no language at all.
    language: Option<&'a str>,
    /// Content-warning subject line.
    spoiler: &'a str,
}

impl<'a> Fixture<'a> {
    /// A public post of `text`, written in English, with no warning.
    const fn new(text: &'a str) -> Self {
        Fixture {
            text,
            visibility: Visibility::Public,
            language: Some("en"),
            spoiler: "",
        }
    }

    /// The same post, seen only as `visibility` allows.
    #[must_use]
    const fn seen_by(mut self, visibility: Visibility) -> Self {
        self.visibility = visibility;
        self
    }

    /// The same post, with `spoiler` as its content warning.
    #[must_use]
    fn warning(mut self, spoiler: &'a str) -> Self {
        self.spoiler = spoiler;
        self
    }

    /// The same post, claiming no language at all.
    #[must_use]
    fn untranslated(mut self) -> Self {
        self.language = None;
        self
    }
}

/// Inserts the status `fixture` describes, authored by `author`.
async fn post_status(pool: &PgPool, author: &User, fixture: &Fixture<'_>) -> Status {
    let new = NewStatus {
        visibility: fixture.visibility,
        language: fixture.language.map(str::to_owned),
        spoiler_text: fixture.spoiler.to_owned(),
        ..NewStatus::local(author, fixture.text)
    };
    status::insert(pool, TEST_BASE_URL, &new)
        .await
        .expect("status fixture inserts")
}

/// A local user named `nickname` plus a token granting `scopes`.
async fn reader(pool: &PgPool, nickname: &str, scopes: &str) -> String {
    let user = common::create_local_user(pool, nickname).await;
    common::create_app_and_token(pool, &user, scopes).await
}

/// A reader whose account's own default language is `language`.
async fn reader_with_language(pool: &PgPool, nickname: &str, language: &str) -> String {
    let user = common::create_local_user_with(pool, nickname, |new| NewUser {
        language: Some(language.to_owned()),
        ..new
    })
    .await;
    common::create_app_and_token(pool, &user, SCOPES).await
}

/// `POST /api/v1/statuses/{id}/translate` with `body` as `token`.
async fn translate(app: &Router, token: &str, id: &str, body: Value) -> common::TestResponse {
    common::TestRequest::post(&format!("/api/v1/statuses/{id}/translate"))
        .bearer(token)
        .json(&body)
        .send(app)
        .await
}

/// `GET /api/v1/instance/translation_languages`, anonymously.
async fn languages(app: &Router) -> common::TestResponse {
    common::TestRequest::get("/api/v1/instance/translation_languages")
        .send(app)
        .await
}

/// The JSON a mock request body parsed into.
fn sent(seen: &Seen) -> Value {
    serde_json::from_str(&seen.body).expect("the provider is sent JSON")
}

/// A `LibreTranslate` instance whose provider answers `translated`, with
/// `alice`'s public "Hello" post and `bob`'s reader token.
async fn libre_scenario(db: &TestDb, translated: &str) -> (Router, Arc<Mock>, Status, String) {
    let pool = db.pool().clone();
    let mock = Arc::new(Mock::libretranslate(translated));
    let (url, _server) = spawn(Arc::clone(&mock)).await;
    let author = common::create_local_user(&pool, "alice").await;
    let status = post_status(&pool, &author, &Fixture::new("Hello")).await;
    let token = reader(&pool, "bob", SCOPES).await;
    let app = common::test_translation_app(pool, "libretranslate", &url);
    (app, mock, status, token)
}
