//! Status translation over HTTP (`POST /api/v1/statuses/{id}/translate`,
//! `GET /api/v1/instance/translation_languages` and the v2 instance's
//! `configuration.translation`), driven against a mock provider on loopback
//! (`docs/ARCHITECTURE.md` decision 13).
//!
//! What these tests own is the endpoint half of the feature: the visibility
//! gate (a post the caller may not read is the shared 404, and the provider is
//! never woken for it), the target language (`lang`, else the caller's own
//! `language`, else `en`), the exact answer document, the request each provider
//! is sent and the answer it returns — both shapes come from Pleroma's
//! `Libretranslate` and `Deepl` modules — the sanitizer pass over whatever the
//! provider said, the two 503s ("not enabled" with no provider, "service
//! unavailable" when the configured one fails), the language matrix with its
//! one-hour cache (a second request must not cost another provider call), the
//! v2 `translation.enabled` flag, and the auth gate.
//!
//! The provider protocol itself — URL assembly, error mapping, the matrix
//! arithmetic — is unit-tested where it lives, in `src/services/translate.rs`.

#![cfg(test)]

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

/// `POST …/translate` on a `LibreTranslate` instance: the provider is sent the
/// request Pleroma's provider sends and its answer comes back as the entity,
/// with the status's own language reported as the detected one.
#[tokio::test]
async fn libretranslate_translates_a_status() {
    let db = common::test_db().await;
    let (app, mock, status, token) = libre_scenario(&db, "<p>Hallo</p>").await;

    let response = translate(&app, &token, &status.id.to_string(), json!({"lang": "de"})).await;

    assert_eq!(response.status, StatusCode::OK, "{}", response.text());
    assert_eq!(
        response.json_value(),
        json!({
            "content": "<p>Hallo</p>",
            "spoiler_text": "",
            "poll": null,
            "media_attachments": [],
            "detected_source_language": "en",
            "provider": "LibreTranslate",
        }),
        "the ticket's document, with exactly the ticket's keys"
    );
    let seen = mock.requests().await;
    assert_eq!(seen.len(), 1, "one call per translation");
    assert_eq!(seen[0].method, "POST", "the translate call is a POST");
    assert_eq!(seen[0].path, "/translate", "at the documented path");
    assert_eq!(
        sent(&seen[0]),
        json!({
            "q": status.content,
            "source": "en",
            "target": "de",
            "format": "html",
            "api_key": "",
        }),
        "the status's own HTML content, with a keyless server sent an empty key"
    );
}

/// `POST …/translate` on a `DeepL` instance: `DeepL`'s own request shape — the
/// text in an array, `tag_handling=html`, the key in `Authorization` — and the
/// language `DeepL` detected is what the answer reports.
#[tokio::test]
async fn deepl_translates_a_status() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let mock = Arc::new(Mock::deepl("<p>Hallo</p>"));
    let (url, _server) = spawn(Arc::clone(&mock)).await;
    let author = common::create_local_user(&pool, "alice").await;
    let status = post_status(&pool, &author, &Fixture::new("Hello")).await;
    let token = reader(&pool, "bob", SCOPES).await;
    let app = common::test_translation_app(pool, "deepl", &url);

    let response = translate(&app, &token, &status.id.to_string(), json!({"lang": "de"})).await;

    assert_eq!(response.status, StatusCode::OK, "{}", response.text());
    assert_eq!(
        response.json_value(),
        json!({
            "content": "<p>Hallo</p>",
            "spoiler_text": "",
            "poll": null,
            "media_attachments": [],
            "detected_source_language": "EN",
            "provider": "DeepL",
        }),
        "the provider's own name and detection"
    );
    let seen = mock.requests().await;
    assert_eq!(seen[0].path, "/v2/translate", "the v2 endpoint");
    assert_eq!(
        seen[0].authorization,
        format!("DeepL-Auth-Key {TEST_KEY}"),
        "the configured key authenticates the call"
    );
    assert_eq!(
        sent(&seen[0]),
        json!({
            "text": [status.content],
            "source_lang": "en",
            "target_lang": "de",
            "tag_handling": "html",
        }),
        "the request Pleroma's DeepL provider sends"
    );
}

/// A status with no language of its own is sent to the provider without one:
/// `auto` for `LibreTranslate`, and no `source_lang` at all for `DeepL`.
#[tokio::test]
async fn a_status_that_names_no_language_is_left_to_the_provider() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let mock = Arc::new(Mock::libretranslate("<p>Hallo</p>"));
    let (url, _server) = spawn(Arc::clone(&mock)).await;
    let author = common::create_local_user(&pool, "alice").await;
    let status = post_status(&pool, &author, &Fixture::new("Hello").untranslated()).await;
    let token = reader(&pool, "bob", SCOPES).await;
    let app = common::test_translation_app(pool, "libretranslate", &url);

    let response = translate(&app, &token, &status.id.to_string(), json!({"lang": "de"})).await;

    assert_eq!(response.status, StatusCode::OK, "{}", response.text());
    assert_eq!(
        sent(&mock.requests().await[0])["source"],
        "auto",
        "the provider is asked to detect it"
    );
    assert_eq!(
        response.json_value()["detected_source_language"],
        Value::Null,
        "and a server that says nothing back detects nothing we can report"
    );
}

/// With no `lang` in the body the endpoint translates into the caller's own
/// default language, and into English when their account never named one.
#[tokio::test]
async fn the_target_defaults_to_the_callers_language_then_english() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let mock = Arc::new(Mock::libretranslate("<p>Saluton</p>"));
    let (url, _server) = spawn(Arc::clone(&mock)).await;
    let author = common::create_local_user(&pool, "alice").await;
    let status = post_status(&pool, &author, &Fixture::new("Hello")).await;
    let esperanto = reader_with_language(&pool, "carol", "eo").await;
    let unspoken = reader(&pool, "dave", SCOPES).await;
    let app = common::test_translation_app(pool, "libretranslate", &url);

    let first = translate(&app, &esperanto, &status.id.to_string(), json!({})).await;
    let second = translate(
        &app,
        &unspoken,
        &status.id.to_string(),
        json!({"lang": " fr "}),
    )
    .await;
    let third = translate(&app, &unspoken, &status.id.to_string(), json!({})).await;

    for response in [&first, &second, &third] {
        assert_eq!(
            response.status,
            StatusCode::OK,
            "a body without a usable `lang` still translates: {}",
            response.text()
        );
    }
    let targets: Vec<String> = mock
        .requests()
        .await
        .iter()
        .map(|entry| {
            sent(entry)["target"]
                .as_str()
                .unwrap_or_default()
                .to_owned()
        })
        .collect();
    assert_eq!(
        targets,
        vec!["eo".to_owned(), "fr".to_owned(), "en".to_owned()],
        "the account default, then the padded spelling trimmed, then English"
    );
}

/// Whatever markup the provider answers with is sanitized before it is served:
/// an injected script and a `javascript:` link do not survive the trip.
#[tokio::test]
async fn the_providers_html_is_sanitized() {
    let db = common::test_db().await;
    let (app, _mock, status, token) = libre_scenario(
        &db,
        "<p>Hallo</p><script>alert(1)</script><a href=\"javascript:x\">link</a><em>Welt</em>",
    )
    .await;

    let response = translate(&app, &token, &status.id.to_string(), json!({"lang": "de"})).await;

    assert_eq!(response.status, StatusCode::OK, "{}", response.text());
    let content = response.json_value()["content"]
        .as_str()
        .expect("content is a string")
        .to_owned();
    assert!(
        content.starts_with("<p>Hallo</p>") && content.contains("<em>Welt</em>"),
        "the markup this instance serves anyway survives: {content}"
    );
    assert!(
        !content.contains("script") && !content.contains("javascript:"),
        "the script element is gone with its contents and the bad link \
         scheme keeps no target: {content}"
    );
}

/// The content warning is the status's own, untranslated: the endpoint sends
/// the provider only the status content.
#[tokio::test]
async fn the_spoiler_rides_along_as_written() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let mock = Arc::new(Mock::libretranslate("<p>Hallo</p>"));
    let (url, _server) = spawn(Arc::clone(&mock)).await;
    let author = common::create_local_user(&pool, "alice").await;
    let status = post_status(
        &pool,
        &author,
        &Fixture::new("Hello").warning("spoilers ahead"),
    )
    .await;
    let token = reader(&pool, "bob", SCOPES).await;
    let app = common::test_translation_app(pool, "libretranslate", &url);

    let response = translate(&app, &token, &status.id.to_string(), json!({"lang": "de"})).await;

    assert_eq!(response.status, StatusCode::OK, "{}", response.text());
    assert_eq!(
        response.json_value()["spoiler_text"],
        "spoilers ahead",
        "the warning is served as its author wrote it"
    );
    let seen = mock.requests().await;
    assert_eq!(seen.len(), 1, "one call, not one per translatable field");
    assert_eq!(
        sent(&seen[0])["q"],
        status.content,
        "and it carries only the status content"
    );
}

/// A provider that refuses, cannot be reached, or answers unreadably is the
/// 503 the ticket pins, with the real reason kept out of the body.
#[tokio::test]
async fn a_failing_provider_is_a_service_unavailable() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let author = common::create_local_user(&pool, "alice").await;
    let status = post_status(&pool, &author, &Fixture::new("Hello")).await;
    let token = reader(&pool, "bob", SCOPES).await;

    let refused_app = {
        let mock = Arc::new(Mock {
            status: StatusCode::TOO_MANY_REQUESTS,
            ..Mock::libretranslate("<p>Hallo</p>")
        });
        let (url, _server) = spawn(mock).await;
        common::test_translation_app(pool.clone(), "libretranslate", &url)
    };
    let unreadable_app = {
        let mock = Arc::new(Mock {
            libre_answer: "<h1>not json</h1>".to_owned(),
            ..Mock::libretranslate("<p>Hallo</p>")
        });
        let (url, _server) = spawn(mock).await;
        common::test_translation_app(pool.clone(), "libretranslate", &url)
    };
    // Port 9 (discard) answers nothing at all.
    let dead_app =
        common::test_translation_app(pool, "libretranslate", "http://127.0.0.1:9/nowhere");

    let refused = translate(
        &refused_app,
        &token,
        &status.id.to_string(),
        json!({"lang": "de"}),
    )
    .await;
    let unreadable = translate(
        &unreadable_app,
        &token,
        &status.id.to_string(),
        json!({"lang": "de"}),
    )
    .await;
    let unreachable = translate(
        &dead_app,
        &token,
        &status.id.to_string(),
        json!({"lang": "de"}),
    )
    .await;

    for response in [&refused, &unreadable, &unreachable] {
        assert_eq!(
            response.status,
            StatusCode::SERVICE_UNAVAILABLE,
            "the provider did not deliver: {}",
            response.text()
        );
        common::assert_error(response, UNAVAILABLE, "a failing provider");
    }
    assert!(
        !refused.text().contains("429") && !unreachable.text().contains("127.0.0.1"),
        "the reason stays server-side: {} / {}",
        refused.text(),
        unreachable.text()
    );
}

/// With `TRANSLATION_PROVIDER` at its `none` default the route answers the 503
/// the ticket pins, and the status gate still comes first.
#[tokio::test]
async fn no_provider_is_a_service_unavailable() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let author = common::create_local_user(&pool, "alice").await;
    let status = post_status(&pool, &author, &Fixture::new("Hello")).await;
    let secret = post_status(
        &pool,
        &author,
        &Fixture::new("Secret").seen_by(Visibility::Private),
    )
    .await;
    let token = reader(&pool, "bob", SCOPES).await;
    let app = common::test_app(pool);

    let refused = translate(&app, &token, &status.id.to_string(), json!({"lang": "de"})).await;
    let invisible = translate(&app, &token, &secret.id.to_string(), json!({"lang": "de"})).await;

    assert_eq!(
        refused.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "503 status"
    );
    common::assert_error(&refused, NOT_ENABLED, "no provider configured");
    assert_eq!(
        refused.header("content-type"),
        Some("application/json"),
        "the refusal is JSON like every error"
    );
    assert_eq!(
        invisible.status,
        StatusCode::NOT_FOUND,
        "an unreadable post is still the shared 404"
    );
    common::assert_error(&invisible, NOT_FOUND, "someone else's private post");
}

/// A status this caller may not see — a private or direct post of someone
/// else's, a tombstone, an unknown id, a malformed id — is the shared 404, and
/// none of them wakes the provider.
#[tokio::test]
async fn an_invisible_status_is_not_found_without_calling_the_provider() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let mock = Arc::new(Mock::libretranslate("<p>Hallo</p>"));
    let (url, _server) = spawn(Arc::clone(&mock)).await;
    let author = common::create_local_user(&pool, "alice").await;
    let secret = post_status(
        &pool,
        &author,
        &Fixture::new("Secret").seen_by(Visibility::Private),
    )
    .await;
    let direct = post_status(
        &pool,
        &author,
        &Fixture::new("Just us").seen_by(Visibility::Direct),
    )
    .await;
    let deleted = post_status(&pool, &author, &Fixture::new("Gone")).await;
    let token = reader(&pool, "bob", SCOPES).await;
    let _tombstone = status::soft_delete(&pool, deleted.id)
        .await
        .expect("the tombstone write runs");
    let app = common::test_translation_app(pool, "libretranslate", &url);

    let probes = [
        translate(&app, &token, &secret.id.to_string(), json!({"lang": "de"})).await,
        translate(&app, &token, &direct.id.to_string(), json!({"lang": "de"})).await,
        translate(&app, &token, &deleted.id.to_string(), json!({"lang": "de"})).await,
        translate(&app, &token, UNKNOWN_ID, json!({"lang": "de"})).await,
        translate(&app, &token, "not-an-id", json!({"lang": "de"})).await,
    ];

    common::assert_all_refused(
        &probes,
        StatusCode::NOT_FOUND,
        NOT_FOUND,
        "an unreadable id",
    );
    assert!(
        mock.requests().await.is_empty(),
        "nothing unreadable is ever sent to a provider"
    );
}

/// The route needs a user token with `read:statuses`: no credentials are the
/// 401, a token without the scope the 403, an app-only token the 403 that says
/// a user is required — and none of them reaches the provider.
#[tokio::test]
async fn the_route_needs_a_reader_token() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let mock = Arc::new(Mock::libretranslate("<p>Hallo</p>"));
    let (url, _server) = spawn(Arc::clone(&mock)).await;
    let author = common::create_local_user(&pool, "alice").await;
    let status = post_status(&pool, &author, &Fixture::new("Hello")).await;
    let wrong_scope = reader(&pool, "mallory", "read:accounts").await;
    let app_only = common::create_app_only_token(&pool, SCOPES).await;
    let path = format!("/api/v1/statuses/{}/translate", status.id);
    let app = common::test_translation_app(pool, "libretranslate", &url);

    let anonymous = common::TestRequest::post(&path)
        .json(&json!({"lang": "de"}))
        .send(&app)
        .await;
    let refused = translate(
        &app,
        &wrong_scope,
        &status.id.to_string(),
        json!({"lang": "de"}),
    )
    .await;
    let unbound = translate(
        &app,
        &app_only,
        &status.id.to_string(),
        json!({"lang": "de"}),
    )
    .await;

    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED, "no token");
    common::assert_error(&anonymous, NO_TOKEN, "an anonymous caller");
    assert_eq!(
        anonymous.header("www-authenticate"),
        Some("Bearer"),
        "a 401 challenges for the scheme"
    );
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "the wrong scope");
    common::assert_error(&refused, NO_SCOPE, "a token without read:statuses");
    assert_eq!(
        unbound.status,
        StatusCode::FORBIDDEN,
        "an app-only token is no user"
    );
    assert!(
        mock.requests().await.is_empty(),
        "a refused request never reaches the provider"
    );
}

/// A `lang` of the wrong type is the body extractor's 422, before the provider
/// is ever asked.
#[tokio::test]
async fn a_non_string_lang_is_unprocessable() {
    let db = common::test_db().await;
    let (app, mock, status, token) = libre_scenario(&db, "<p>Hallo</p>").await;

    let response = translate(&app, &token, &status.id.to_string(), json!({"lang": 7})).await;

    assert_eq!(
        response.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        response.text()
    );
    assert!(
        response.json_value()["error"]
            .as_str()
            .is_some_and(|message| message.starts_with("Invalid parameters:")),
        "the extractor names its own failure: {}",
        response.text()
    );
    assert!(mock.requests().await.is_empty(), "and nothing was asked");
}

/// The translated content round-trips as form parameters too (decision 10):
/// `lang=de` in an urlencoded body is the same request.
#[tokio::test]
async fn the_body_may_be_a_form() {
    let db = common::test_db().await;
    let (app, _mock, status, token) = libre_scenario(&db, "<p>Hallo</p>").await;

    let response = common::TestRequest::post(&format!("/api/v1/statuses/{}/translate", status.id))
        .bearer(&token)
        .form(&[("lang", "de")])
        .send(&app)
        .await;

    assert_eq!(response.status, StatusCode::OK, "{}", response.text());
    assert_eq!(
        response.json_value()["content"],
        "<p>Hallo</p>",
        "the form's `lang` won"
    );
}

/// `GET /api/v1/instance/translation_languages` answers the provider's matrix —
/// and answers it from the cache, so the second request costs no provider call.
#[tokio::test]
async fn the_language_matrix_is_served_and_cached() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let mock = Arc::new(Mock::libretranslate("<p>Hallo</p>"));
    let (url, _server) = spawn(Arc::clone(&mock)).await;
    let app = common::test_translation_app(pool, "libretranslate", &url);

    let first = languages(&app).await;
    let second = languages(&app).await;

    assert_eq!(first.status, StatusCode::OK, "the document is served");
    assert_eq!(
        first.json_value(),
        json!({"de": ["en"], "en": ["de"]}),
        "every source with every other language as its target"
    );
    assert_eq!(
        second.json_value(),
        first.json_value(),
        "the cached document is the same one"
    );
    let seen = mock.requests().await;
    assert_eq!(
        seen.iter()
            .filter(|entry| entry.path == "/languages")
            .count(),
        1,
        "one fetch per cache window, not one per request"
    );
    assert_eq!(seen[0].method, "GET", "the list is a `GET` call");
}

/// `DeepL` builds its matrix from the two lists its API documents, and
/// authenticates those calls exactly as it authenticates a translation.
#[tokio::test]
async fn deepl_lists_its_languages_from_both_queries() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let mock = Arc::new(Mock::deepl("<p>Hallo</p>"));
    let (url, _server) = spawn(Arc::clone(&mock)).await;
    let app = common::test_translation_app(pool, "deepl", &url);

    let response = languages(&app).await;

    assert_eq!(response.status, StatusCode::OK, "{}", response.text());
    assert_eq!(
        response.json_value(),
        json!({
            "en": ["pt", "pt-br"],
            "pt": ["en", "pt-br"],
            "pt-br": ["en", "pt"],
        }),
        "a regional spelling is listed beside its base tag"
    );
    let seen = mock.requests().await;
    assert_eq!(
        seen.iter()
            .map(|entry| entry.target.as_str())
            .collect::<Vec<&str>>(),
        vec!["/v2/languages?type=source", "/v2/languages?type=target"],
        "two queries, sources first"
    );
    assert!(
        seen.iter()
            .all(|entry| entry.authorization == format!("DeepL-Auth-Key {TEST_KEY}")),
        "the list is as authenticated as the translation"
    );
}

/// The language document is the empty object when the configured provider
/// refuses its list, and a refusal is not cached: the provider is asked again
/// rather than buying an hour of silence. An instance with no provider at all
/// answers the same `{}` — `instance::translation_languages_is_the_empty_object`
/// is that test.
#[tokio::test]
async fn a_provider_without_a_list_answers_no_pairs() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let mock = Arc::new(Mock {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        ..Mock::libretranslate("<p>Hallo</p>")
    });
    let (url, _server) = spawn(Arc::clone(&mock)).await;
    let app = common::test_translation_app(pool, "libretranslate", &url);

    let refused = languages(&app).await;
    let again = languages(&app).await;

    assert_eq!(
        refused.status,
        StatusCode::OK,
        "the endpoint answers even a failing provider"
    );
    assert_eq!(
        refused.json_value(),
        json!({}),
        "a provider that refuses its list has no language pairs: {}",
        refused.text()
    );
    assert_eq!(again.json_value(), json!({}), "and the second answer too");
    assert_eq!(
        mock.requests().await.len(),
        2,
        "a refusal is not cached, so the provider gets asked again"
    );
}

/// `GET /api/v2/instance` reports `configuration.translation.enabled` from the
/// same settings the endpoints act on: true for a usable provider, false while
/// none is configured and false for one missing what it needs.
#[tokio::test]
async fn the_v2_document_reports_whether_translation_is_enabled() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let configured = common::test_app_with_config(
        pool.clone(),
        common::test_translation_config("deepl", Some("http://127.0.0.1:9")),
    );
    let urlless = common::test_app_with_config(
        pool.clone(),
        common::test_translation_config("libretranslate", None),
    );
    let unconfigured = common::test_app(pool);

    for (app, enabled) in [
        (&configured, true),
        (&urlless, false),
        (&unconfigured, false),
    ] {
        let response = common::TestRequest::get("/api/v2/instance").send(app).await;
        assert_eq!(response.status, StatusCode::OK, "the document is served");
        assert_eq!(
            response.json_value()["configuration"]["translation"],
            json!({ "enabled": enabled }),
            "the translation group of a provider that is usable: {}",
            response.text()
        );
    }
}

/// A reblog row is translated as itself — the row the client named, whose own
/// content is empty — exactly as the other status routes answer it.
#[tokio::test]
async fn a_reblog_is_translated_as_its_own_row() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let mock = Arc::new(Mock::libretranslate("<p>Hallo</p>"));
    let (url, _server) = spawn(Arc::clone(&mock)).await;
    let author = common::create_local_user(&pool, "alice").await;
    let original = post_status(&pool, &author, &Fixture::new("Hello")).await;
    let booster = common::create_local_user(&pool, "carol").await;
    let boost = status::insert(
        &pool,
        TEST_BASE_URL,
        &NewStatus {
            user_id: booster.id,
            reblog_of_id: Some(original.id),
            language: Some("en".to_owned()),
            local: true,
            ..NewStatus::default()
        },
    )
    .await
    .expect("reblog fixture inserts");
    let token = reader(&pool, "bob", SCOPES).await;
    let app = common::test_translation_app(pool, "libretranslate", &url);

    let response = translate(&app, &token, &boost.id.to_string(), json!({"lang": "de"})).await;

    assert_eq!(response.status, StatusCode::OK, "{}", response.text());
    assert_eq!(
        response.json_value()["content"],
        "<p>Hallo</p>",
        "the answer is what the provider said"
    );
    assert_eq!(
        sent(&mock.requests().await[0])["q"],
        boost.content,
        "and the provider was sent the reblog row's own (empty) content"
    );
}

/// The route is mounted at the status id and nowhere else: the trailing-slash
/// spelling is the app-wide fallback, not a second mount.
#[tokio::test]
async fn the_route_is_keyed_on_the_status_id() {
    let db = common::test_db().await;
    let (app, _mock, status, token) = libre_scenario(&db, "<p>Hallo</p>").await;

    let slashed = common::TestRequest::post(&format!("/api/v1/statuses/{}/translate/", status.id))
        .bearer(&token)
        .json(&json!({"lang": "de"}))
        .send(&app)
        .await;

    assert_eq!(
        slashed.status,
        StatusCode::NOT_FOUND,
        "the slash spelling is not a second mount"
    );
    assert_eq!(
        slashed.json_value(),
        json!({"error": "Not implemented"}),
        "Pleroma's api_not_implemented fallback"
    );
}
