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
//! Later tickets add more database fixture helpers here (`create_status`,
//! ...); keep them in this module instead of duplicating them per test
//! file.

#![cfg(test)]

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
    models::{
        oauth_app::{self, NewOauthApp},
        oauth_token::{self, NewToken},
    },
    state::AppState,
};

/// The real application router from [`wired_be::app`], serving over
/// `pool`.
pub fn test_app(pool: PgPool) -> Router {
    wired_be::app(AppState { db: pool })
}

/// The redirect URI every app fixture registers.
const TEST_REDIRECT_URIS: &str = "https://client.example/callback";

/// Inserts a bare local user row named `nickname` and returns its id.
///
/// Straight in SQL: the registration model arrives with its own ticket,
/// and bearer-auth fixtures only need the row the `oauth_tokens.user_id`
/// foreign key points at.
///
/// # Panics
///
/// If the insert fails, e.g. because the nickname is already taken in the
/// test database.
pub async fn create_local_user(pool: &PgPool, nickname: &str) -> Uuid {
    let id = Uuid::now_v7();
    let outcome =
        sqlx::query("INSERT INTO users (id, nickname, ap_id, local) VALUES ($1, $2, $3, true)")
            .bind(id)
            .bind(nickname)
            .bind(format!("https://test.example/users/{nickname}"))
            .execute(pool)
            .await
            .expect("local user fixture inserts");
    assert_eq!(
        outcome.rows_affected(),
        1,
        "the fixture insert wrote the row"
    );
    id
}

/// Registers a test app named "Test Client" granting `scopes` (one
/// space-separated string) and returns its row.
///
/// # Panics
///
/// If the app insert fails.
pub async fn create_app(pool: &PgPool, scopes: &str) -> oauth_app::OauthApp {
    oauth_app::create(pool, &test_app_registration(None, scopes))
        .await
        .expect("app fixture inserts")
}

/// Registers a test app and mints a user-bound bearer token, both
/// granting `scopes`; returns the access token string to send as
/// `Authorization: Bearer`.
///
/// # Panics
///
/// If the app or token insert fails.
pub async fn create_app_and_token(pool: &PgPool, user_id: Uuid, scopes: &str) -> String {
    let app = oauth_app::create(pool, &test_app_registration(Some(user_id), scopes))
        .await
        .expect("app fixture inserts");
    create_token_for(pool, Some(user_id), app.id, scopes)
        .await
        .token
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

/// The [`NewOauthApp`] both fixtures register: a named test client
/// granted `scopes` on behalf of `user_id`.
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
    /// Request target, e.g. `/api/v1/apps`.
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
