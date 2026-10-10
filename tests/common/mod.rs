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

use std::sync::Arc;

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
use wired_be::{
    config::Config,
    http_client::HttpClient,
    models::user::{NewUser, User, insert, local_ap_id},
    state::AppState,
};

/// The configuration [`test_app`] runs with: `BASE_URL` `https://test.example`,
/// defaults everywhere else, no environment access.
#[must_use]
pub fn test_config() -> Config {
    Config::for_tests()
}

/// The real application router from [`wired_be::app`], serving over `pool`
/// and the [`test_config`] settings.
pub fn test_app(pool: PgPool) -> Router {
    let config = Arc::new(test_config());
    wired_be::app(AppState {
        db: pool,
        http_client: HttpClient::from(config.as_ref()),
        config,
    })
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
