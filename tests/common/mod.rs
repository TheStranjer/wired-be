//! Reusable helpers for the HTTP integration tests (decision 13).
//!
//! Every endpoint test follows the same shape: `#[sqlx::test]` hands the
//! test a fresh, fully migrated database and its `PgPool`; [`test_app`]
//! builds the real router on top of that pool; [`TestRequest`] drives the
//! router in-process through `tower::ServiceExt::oneshot` — no sockets, no
//! ports — and returns a [`TestResponse`] to assert against.
//!
//! The `#![cfg(test)]` gate is always active for crates under `tests/`
//! (Cargo compiles them with `--test`). It is what lets Clippy's
//! `allow-unwrap-in-tests` family apply below: a test helper may abort on
//! programmer error (invalid header, unparsable body) exactly like a
//! `#[test]` function does.
//!
//! Later tickets add database fixture helpers here (`create_user`,
//! `create_token`, ...); keep them in this module instead of duplicating
//! them per test file.

#![cfg(test)]

use std::{path::Path, sync::Arc};

use axum::{
    Router,
    body::{Body, Bytes},
    http::{HeaderMap, HeaderName, HeaderValue, Method, Request, StatusCode, header},
    response::Response,
};
use http_body_util::BodyExt as _;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use sqlx::PgPool;
use tower::ServiceExt as _;
use uuid::Uuid;
use wired_be::{
    config::Config,
    http_client::HttpClient,
    models::{
        oauth_app::{self, NewOauthApp},
        oauth_token::{self, NewToken},
        status::{self, NewStatus, Status, Visibility},
        user::{Flag, NewUser, User, followers, following, inbox, insert, local_ap_id, outbox},
    },
    state::AppState,
    uploads::UploaderBackend,
    util::password::hash_password,
};

/// The configuration [`test_app`] runs with: `BASE_URL` `https://test.example`,
/// defaults everywhere else, no environment access.
#[must_use]
pub fn test_config() -> Config {
    Config::for_tests()
}

/// [`test_config`] with its uploads moved to `uploads_dir`, for tests that
/// store files and must not write into the repository.
#[must_use]
pub fn test_config_with_uploads_dir(uploads_dir: &Path) -> Config {
    let mut config = test_config();
    config.uploads_dir = uploads_dir.to_path_buf();
    config
}

/// [`test_config`] with the emoji packs root moved to `emoji_dir`, for
/// tests that read packs from disk and must not touch the repository.
#[must_use]
pub fn test_config_with_emoji_dir(emoji_dir: &Path) -> Config {
    let mut config = test_config();
    config.emoji_dir = emoji_dir.to_path_buf();
    config
}

/// The local-file [`UploaderBackend`] storing in `uploads_dir`: the same
/// storage [`test_app_with_uploads_dir`] serves from.
///
/// Use it to put or delete a file before requesting it over HTTP.
#[must_use]
pub fn test_uploader(uploads_dir: &Path) -> UploaderBackend {
    UploaderBackend::from(&test_config_with_uploads_dir(uploads_dir))
}

/// The real application router from [`wired_be::app`], serving over `pool`
/// and the [`test_config`] settings.
///
/// Uploads go to the default `UPLOADS_DIR`, which is inside the working
/// directory: tests that store a file want [`test_app_with_uploads_dir`]
/// instead.
pub fn test_app(pool: PgPool) -> Router {
    test_app_with_config(pool, test_config())
}

/// The real application router, serving uploads out of `uploads_dir`.
pub fn test_app_with_uploads_dir(pool: PgPool, uploads_dir: &Path) -> Router {
    test_app_with_config(pool, test_config_with_uploads_dir(uploads_dir))
}

/// The real application router, reading emoji packs from `emoji_dir`.
pub fn test_app_with_emoji_dir(pool: PgPool, emoji_dir: &Path) -> Router {
    test_app_with_config(pool, test_config_with_emoji_dir(emoji_dir))
}

/// The real application router, storing uploads in `uploads_dir` and
/// capping one uploaded file at `max_file_bytes`.
///
/// The upload endpoints answer 413 at `UPLOAD_LIMIT_BYTES`; tests that want
/// to see that without sending 16 MB of bytes set a small limit here. The
/// other upload setting keeps its test default.
pub fn test_app_with_upload_limits(
    pool: PgPool,
    uploads_dir: &Path,
    max_file_bytes: usize,
) -> Router {
    let mut config = test_config_with_uploads_dir(uploads_dir);
    config.upload_limits.max_file_bytes = max_file_bytes;
    test_app_with_config(pool, config)
}

/// The shared [`AppState`] every test router is built on, and every job
/// handler is handed: `pool` plus `config`, with the hub, HTTP client,
/// uploader and stats cache the app needs.
///
/// A test that runs a background job rather than a request takes this
/// directly — handlers receive an [`AppState`], not a router — while
/// [`test_app_with_config`] wraps the very same state into the router.
#[must_use]
pub fn test_app_state(pool: PgPool, config: Config) -> AppState {
    let config = Arc::new(config);
    let hub = wired_be::streaming::hub::Hub::new(&pool);
    AppState {
        db: pool,
        hub,
        config: Arc::clone(&config),
        http_client: HttpClient::from(config.as_ref()),
        uploader: UploaderBackend::from(config.as_ref()),
        instance_stats_cache: Arc::default(),
    }
}

/// The real application router from [`wired_be::app`] over `pool` and
/// `config`.
///
/// Tickets whose endpoints read a config setting test the non-default
/// values through here: build a [`test_config`], set the fields, and
/// mount the router.
pub fn test_app_with_config(pool: PgPool, config: Config) -> Router {
    wired_be::app(test_app_state(pool, config))
}

/// Inserts a local [`User`] named `nickname` straight through
/// [`wired_be::models::user`], with no password, and returns its row.
///
/// The actor id is [`local_ap_id`] on the [`test_config`] instance, so
/// fixtures and federation-shaped code agree on the URL.
///
/// # Panics
///
/// If the insert fails, e.g. because the nickname is already taken in the
/// test database.
pub async fn create_local_user(pool: &PgPool, nickname: &str) -> User {
    let config = test_config();
    let ap_id = local_ap_id(&config.base_url, nickname);
    insert(pool, &NewUser::local(nickname, &ap_id))
        .await
        .expect("local user fixture inserts")
}

/// Inserts a local [`User`] named `nickname` that can log in with
/// `password`.
///
/// The row carries an Argon2id hash made with
/// [`hash_password`](wired_be::util::password::hash_password) and the
/// derived email `<nickname>@example.org`, so both lookup paths of
/// [`authenticate`] (nickname and email) find it, and only `password`
/// does.
///
/// This deliberately bypasses
/// [`create_local_user`](wired_be::models::user::register::create_local_user)
/// and its validation: fixtures need a login-capable row cheaply, so no
/// RSA key pair is generated and no registration rules are re-checked.
/// Use the register path itself when a test is about registration.
///
/// [`authenticate`]: wired_be::models::user::register::authenticate
///
/// # Panics
///
/// If hashing or the insert fails, e.g. because the nickname or derived
/// email is already taken in the test database.
pub async fn create_user_with_password(pool: &PgPool, nickname: &str, password: &str) -> User {
    let config = test_config();
    let new = NewUser {
        email: Some(format!("{nickname}@example.org")),
        password_hash: Some(
            hash_password(password).expect("password hashing succeeds for fixtures"),
        ),
        ..NewUser::local(nickname, &local_ap_id(&config.base_url, nickname))
    };
    insert(pool, &new)
        .await
        .expect("local user fixture with password inserts")
}

/// Inserts a local [`User`] named `nickname` whose columns `customize`
/// adjusts on top of [`NewUser::local`], and returns its row.
///
/// The profile and federation tests need accounts that differ from the
/// bare [`create_local_user`] fixture — locked, a bot, with an avatar —
/// without restating the actor id every time.
///
/// # Panics
///
/// If the insert fails, e.g. because the nickname is already taken in the
/// test database.
pub async fn create_local_user_with<F>(pool: &PgPool, nickname: &str, customize: F) -> User
where
    F: FnOnce(NewUser) -> NewUser,
{
    let config = test_config();
    let ap_id = local_ap_id(&config.base_url, nickname);
    let new = customize(NewUser::local(nickname, &ap_id));
    insert(pool, &new)
        .await
        .expect("local user fixture inserts")
}

/// Inserts a mirror of the remote actor `nickname` on `host`: a non-local
/// [`User`], the shape a fetch of `https://{host}/users/{nickname}` stores.
///
/// The nickname carries `@host` (decision 7) and the actor id, domain and
/// endpoints are the ones the fetched document would have named.
///
/// # Panics
///
/// If the insert fails, e.g. because the handle is already taken in the
/// test database.
pub async fn create_remote_user(pool: &PgPool, nickname: &str, host: &str) -> User {
    let ap_id = format!("https://{host}/users/{nickname}");
    let new = NewUser {
        nickname: format!("{nickname}@{host}"),
        ap_id: ap_id.clone(),
        domain: Some(host.to_owned()),
        name: nickname.to_owned(),
        inbox: Some(inbox(&ap_id)),
        shared_inbox: Some(format!("https://{host}/inbox")),
        outbox: Some(outbox(&ap_id)),
        followers_url: Some(followers(&ap_id)),
        following_url: Some(following(&ap_id)),
        public_key: Some(
            "-----BEGIN PUBLIC KEY-----\nREMOTE-DATA\n-----END PUBLIC KEY-----\n".to_owned(),
        ),
        ..NewUser::default()
    };
    insert(pool, &new)
        .await
        .expect("remote user fixture inserts")
}

/// Inserts a local [`Status`] posted by `user` — `text` as a plain-text
/// status of `visibility` — straight through
/// [`wired_be::models::status`], and returns its row.
///
/// The model derives everything a local post owns from its minted id on the
/// [`test_config`] instance: the `{base_url}/objects/{uuid}` uri, the
/// `{base_url}/notice/{uuid}` url, the conversation (its own id, since this
/// replies to nothing) and the author's `statuses_count`, so a fixture made
/// here looks exactly like a post the API would have written. A status that
/// replies, reblogs or quotes needs more of [`NewStatus`] than this passes:
/// build it and call the model directly.
///
/// # Panics
///
/// If the insert fails, e.g. because `user` has no row in the test database.
pub async fn create_status(
    pool: &PgPool,
    user: &User,
    text: &str,
    visibility: Visibility,
) -> Status {
    let new = NewStatus {
        visibility,
        ..NewStatus::local(user, text)
    };
    status::insert(pool, &test_config().base_url, &new)
        .await
        .expect("status fixture inserts")
}

/// The redirect URI every app fixture registers.
const TEST_REDIRECT_URIS: &str = "https://client.example/callback";
/// Registers a test app named "Test Client" granting `scopes` (one
/// space-separated string) and returns its row: the `/oauth/token` tests
/// need the credential pair, not a minted token.
///
/// # Panics
///
/// If the app insert fails.
pub async fn create_app(pool: &PgPool, scopes: &str) -> oauth_app::OauthApp {
    oauth_app::create(pool, &test_app_registration(None, scopes))
        .await
        .expect("app fixture inserts")
}

/// Inserts a local [`User`] named `nickname` that can log in with
/// `password` but is deactivated (`is_active = false`): the shape a
/// suspended account has.
///
/// # Panics
///
/// If hashing or the insert fails.
pub async fn create_deactivated_user_with_password(
    pool: &PgPool,
    nickname: &str,
    password: &str,
) -> User {
    let config = test_config();
    let new = NewUser {
        email: Some(format!("{nickname}@example.org")),
        password_hash: Some(
            hash_password(password).expect("password hashing succeeds for fixtures"),
        ),
        is_active: Flag::new(false),
        ..NewUser::local(nickname, &local_ap_id(&config.base_url, nickname))
    };
    insert(pool, &new)
        .await
        .expect("deactivated local user fixture inserts")
}

/// Registers a test app and mints a user-bound bearer token, both
/// granting `scopes` (one space-separated string,
/// [`Scopes::parse`](wired_be::oauth::scopes::Scopes::parse) form);
/// returns the access token string to send as `Authorization: Bearer`.
///
/// The token carries the default lifetime; tests that need another
/// state (expired, deactivated owner) mint through
/// [`wired_be::models::oauth_token`] themselves and adjust the row.
///
/// # Panics
///
/// If the app or token insert fails.
pub async fn create_app_and_token(pool: &PgPool, user: &User, scopes: &str) -> String {
    let app = oauth_app::create(pool, &test_app_registration(Some(user.id), scopes))
        .await
        .expect("app fixture inserts");
    create_token_for(pool, Some(user.id), app.id, scopes)
        .await
        .token
}

/// Registers a test app and mints an app-only (`client_credentials`)
/// bearer token granting `scopes`; returns the access token string.
///
/// The user-unbound counterpart of [`create_app_and_token`], for the
/// app-only behaviour of the extractors.
///
/// # Panics
///
/// If the app or token insert fails.
pub async fn create_app_only_token(pool: &PgPool, scopes: &str) -> String {
    let app = oauth_app::create(pool, &test_app_registration(None, scopes))
        .await
        .expect("app fixture inserts");
    create_token_for(pool, None, app.id, scopes).await.token
}

/// Registers an app named `name` owned by `owner` (`None` for a
/// user-unbound registration) and returns its row.
///
/// The named counterpart of [`create_app`] and
/// [`create_app_and_token`], for tests that key on the client name or
/// the owner column and not on the fixed `"Test Client"` fixture.
///
/// # Panics
///
/// If the app insert fails.
pub async fn create_app_named(
    pool: &PgPool,
    name: &str,
    owner: Option<&User>,
) -> oauth_app::OauthApp {
    oauth_app::create(
        pool,
        &NewOauthApp {
            client_name: name.to_owned(),
            redirect_uris: TEST_REDIRECT_URIS.to_owned(),
            scopes: "read".to_owned(),
            website: None,
            user_id: owner.map(|found| found.id),
        },
    )
    .await
    .expect("named app fixture inserts")
}

/// Mints a token acting as `user` issued by `app` and granting `scopes`,
/// returning the whole stored row (id, access token and expiry included)
/// for tests that revoke it over HTTP or expire it first.
///
/// # Panics
///
/// If the token insert fails.
pub async fn create_token(
    pool: &PgPool,
    user: &User,
    app: &oauth_app::OauthApp,
    scopes: &str,
) -> oauth_token::OauthToken {
    create_token_for(pool, Some(user.id), app.id, scopes).await
}

/// Moves the token row `id`'s `valid_until` an hour into the past: the
/// state a lapsed token has, which the validity checks refuse.
///
/// # Panics
///
/// If the update fails or hits no row.
pub async fn expire_token(pool: &PgPool, id: Uuid) {
    let outcome = sqlx::query(
        "UPDATE oauth_tokens SET valid_until = now() - interval '1 hour' WHERE id = $1",
    )
    .bind(id)
    .execute(pool)
    .await
    .expect("the expiry update runs");
    assert_eq!(outcome.rows_affected(), 1, "the update hit the token row");
}

/// The [`NewOauthApp`] both token fixtures register: a named test
/// client granted `scopes` on behalf of `user_id`.
fn test_app_registration(user_id: Option<Uuid>, scopes: &str) -> NewOauthApp {
    NewOauthApp {
        client_name: "Test Client".to_owned(),
        redirect_uris: TEST_REDIRECT_URIS.to_owned(),
        scopes: scopes.to_owned(),
        website: None,
        user_id,
    }
}

/// Mints a token for `user_id` (`None` app-only) issued by `app_id`
/// with `scopes` and returns the stored row.
async fn create_token_for(
    pool: &PgPool,
    user_id: Option<Uuid>,
    app_id: Uuid,
    scopes: &str,
) -> oauth_token::OauthToken {
    oauth_token::create_token(
        pool,
        &NewToken {
            user_id,
            app_id,
            scopes: scopes.to_owned(),
            ttl: None,
        },
    )
    .await
    .expect("token fixture inserts")
}

/// A request under construction: the verb methods start a builder, the
/// body and header methods refine it, and [`send`](Self::send) runs it to
/// completion.
#[derive(Debug)]
pub struct TestRequest {
    /// HTTP method to use.
    method: Method,
    /// Request target, e.g. `/api/v1/accounts/1`.
    path: String,
    /// Headers to send.
    headers: HeaderMap,
    /// The request body; empty until a body method sets it.
    body: Bytes,
}

impl TestRequest {
    /// A `GET` request for `path`.
    #[must_use]
    pub fn get(path: &str) -> Self {
        Self::new(Method::GET, path)
    }

    /// A `POST` request for `path`.
    #[must_use]
    pub fn post(path: &str) -> Self {
        Self::new(Method::POST, path)
    }

    /// A `PUT` request for `path`.
    #[must_use]
    pub fn put(path: &str) -> Self {
        Self::new(Method::PUT, path)
    }

    /// A `PATCH` request for `path`.
    #[must_use]
    pub fn patch(path: &str) -> Self {
        Self::new(Method::PATCH, path)
    }

    /// A `DELETE` request for `path`.
    #[must_use]
    pub fn delete(path: &str) -> Self {
        Self::new(Method::DELETE, path)
    }

    /// Shared constructor for the verb methods.
    fn new(method: Method, path: &str) -> Self {
        Self {
            method,
            path: path.to_owned(),
            headers: HeaderMap::new(),
            body: Bytes::new(),
        }
    }

    /// Sets the body to `value` as JSON, with an `application/json`
    /// `content-type`.
    ///
    /// # Panics
    ///
    /// If `value` fails to serialize (e.g. an `f64::NAN`); plain test
    /// fixtures never do.
    #[must_use]
    pub fn json<T: Serialize>(self, value: &T) -> Self {
        // Test fixtures are static serializable values; serialization
        // cannot fail for them.
        let body = Bytes::from(serde_json::to_vec(value).expect("test fixture serializes as JSON"));
        self.content_type("application/json").raw_body(body)
    }

    /// Sets the body to `fields`, percent-encoded as an
    /// `application/x-www-form-urlencoded` body.
    #[must_use]
    pub fn form(self, fields: &[(&str, &str)]) -> Self {
        let encoded = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(fields.iter().copied())
            .finish();
        self.content_type("application/x-www-form-urlencoded")
            .raw_body(Bytes::from(encoded))
    }

    /// Adds an `authorization: Bearer <token>` header.
    ///
    /// # Panics
    ///
    /// If `token` contains characters not allowed in a header value.
    #[must_use]
    pub fn bearer(self, token: &str) -> Self {
        let value =
            HeaderValue::from_str(&format!("Bearer {token}")).expect("bearer token is header-safe");
        self.push_header(header::AUTHORIZATION, value)
    }

    /// Adds the header `name: value`, replacing an earlier value for the
    /// same name.
    ///
    /// # Panics
    ///
    /// If `name` or `value` is not a valid HTTP header.
    #[must_use]
    pub fn header(self, name: &str, value: &str) -> Self {
        let name = HeaderName::try_from(name).expect("test header name is valid");
        let value = HeaderValue::from_str(value).expect("test header value is valid");
        self.push_header(name, value)
    }

    /// Sets the body to `parts` framed as `multipart/form-data` with
    /// `boundary`, and the `content-type` that announces it, so an upload
    /// endpoint sees exactly what a browser's `FormData` would send.
    ///
    /// A file part is one with a `filename`; the endpoint under test decides
    /// which fields hold files.
    ///
    /// # Panics
    ///
    /// If the resulting `content-type` is not a valid header value; test
    /// boundaries are plain ASCII.
    #[must_use]
    pub fn multipart(self, boundary: &str, parts: &[MultipartPart]) -> Self {
        let mut body: Vec<u8> = Vec::new();
        for part in parts {
            write_multipart_part(&mut body, boundary, part);
        }
        body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        self.header(
            "content-type",
            &format!("multipart/form-data; boundary={boundary}"),
        )
        .raw_body(Bytes::from(body))
    }

    /// Sets the `content-type` header, for the body methods.
    fn content_type(self, mime: &'static str) -> Self {
        self.push_header(header::CONTENT_TYPE, HeaderValue::from_static(mime))
    }

    /// Adds a pre-validated header, replacing an earlier value for `name`.
    fn push_header(mut self, name: HeaderName, value: HeaderValue) -> Self {
        let replaced = self.headers.insert(name, value);
        drop(replaced);
        self
    }

    /// Replaces the body verbatim, for the body methods.
    fn raw_body(self, body: Bytes) -> Self {
        Self {
            method: self.method,
            path: self.path,
            headers: self.headers,
            body,
        }
    }

    /// Runs this request through `app` in-process and buffers the whole
    /// response.
    ///
    /// # Panics
    ///
    /// If the request cannot be constructed (invalid path).
    #[must_use]
    pub async fn send(self, app: &Router) -> TestResponse {
        let mut builder = Request::builder().method(self.method).uri(self.path);
        for (name, value) in &self.headers {
            builder = builder.header(name.clone(), value.clone());
        }
        let request = builder
            .body(Body::from(self.body))
            .expect("test request is a valid HTTP request");
        // A Router's service error type is `Infallible`; the empty arm
        // proves it.
        let response = match app.clone().oneshot(request).await {
            Ok(response) => response,
            Err(err) => match err {},
        };
        TestResponse::from_response(response).await
    }
}

/// One part of a `multipart/form-data` body built by
/// [`TestRequest::multipart`].
///
/// The part's `content-type` is what the *client* claims for it: upload
/// endpoints read the type from the bytes instead, so a test that wants to
/// prove that sends a mismatching one.
#[derive(Clone, Debug)]
pub struct MultipartPart {
    /// Name of the form field the part arrives under.
    pub name: &'static str,
    /// Client file name, which is what makes a part a file part.
    pub filename: Option<&'static str>,
    /// The part's `content-type`, if it carries one.
    pub content_type: Option<&'static str>,
    /// The part's bytes: a text field's value or a file's contents.
    pub bytes: Bytes,
}

impl MultipartPart {
    /// A text field part: `name` with `value` and no file name, so the
    /// extractor structures it into the parameters like a form key.
    #[must_use]
    pub fn text(name: &'static str, value: &'static str) -> Self {
        Self {
            name,
            filename: None,
            content_type: None,
            bytes: Bytes::from_static(value.as_bytes()),
        }
    }

    /// A file part: `name` holding `bytes`, uploaded as `filename` and
    /// announced by the client as `content_type`.
    #[must_use]
    pub fn file(
        name: &'static str,
        filename: &'static str,
        content_type: &'static str,
        bytes: Bytes,
    ) -> Self {
        Self {
            name,
            filename: Some(filename),
            content_type: Some(content_type),
            bytes,
        }
    }
}

/// Appends one `part` of a multipart body to `body`, framed with `boundary`:
/// the `Content-Disposition` line, an optional `Content-Type`, a blank line,
/// the bytes and the crlf that closes the part.
fn write_multipart_part(body: &mut Vec<u8>, boundary: &str, part: &MultipartPart) {
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    match part.filename {
        Some(filename) => body.extend_from_slice(
            format!(
                "Content-Disposition: form-data; name=\"{}\"; filename=\"{filename}\"\r\n",
                part.name
            )
            .as_bytes(),
        ),
        None => body.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{}\"\r\n", part.name).as_bytes(),
        ),
    }
    if let Some(content_type) = part.content_type {
        body.extend_from_slice(format!("Content-Type: {content_type}\r\n").as_bytes());
    }
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(&part.bytes);
    body.extend_from_slice(b"\r\n");
}

/// The response captured by [`TestRequest::send`]: status, headers and the
/// fully buffered body, ready for assertions.
#[derive(Debug)]
pub struct TestResponse {
    /// The response status code.
    pub status: StatusCode,
    /// The response headers.
    pub headers: HeaderMap,
    /// The buffered response body.
    pub body: Bytes,
}

impl TestResponse {
    /// Captures `response` completely.
    ///
    /// # Panics
    ///
    /// If the body stream fails, which in-process responses cannot.
    async fn from_response(response: Response) -> Self {
        let status = response.status();
        let headers = response.headers().clone();
        let body = response
            .into_body()
            .collect()
            .await
            .expect("in-process response body collects")
            .to_bytes();
        Self {
            status,
            headers,
            body,
        }
    }

    /// The body parsed as JSON of shape `T`.
    ///
    /// # Panics
    ///
    /// If the body is not valid JSON shaped like `T`; call
    /// [`text`](Self::text) to see what came back instead.
    #[must_use]
    pub fn json<T: DeserializeOwned>(&self) -> T {
        serde_json::from_slice(&self.body).expect("response body is JSON")
    }

    /// The body parsed as a generic JSON value.
    ///
    /// # Panics
    ///
    /// If the body is not valid JSON.
    #[must_use]
    pub fn json_value(&self) -> Value {
        self.json()
    }

    /// The body decoded as UTF-8 text (lossy for invalid bytes).
    #[must_use]
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// The value of response header `name`, or `None` when the header is
    /// absent or holds non-visible bytes.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        let name = HeaderName::try_from(name).ok()?;
        self.headers.get(name)?.to_str().ok()
    }
}
