//! The in-process request builder and its multipart parts.

use std::net::SocketAddr;

use axum::{
    Router,
    body::{Body, Bytes},
    extract::ConnectInfo,
    http::{HeaderMap, HeaderName, HeaderValue, Method, Request, header},
};
use serde::Serialize;
use tower::ServiceExt as _;

use super::TestResponse;

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
    /// The socket peer to carry as `ConnectInfo`; `None` sends none.
    peer: Option<SocketAddr>,
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
            peer: None,
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

    /// Carries `addr` as the request's `ConnectInfo` socket peer, the way
    /// `axum::serve` does for a real connection. The rate limiter reads it
    /// when no proxy header names the client (see
    /// [`wired_be::rate_limit`]).
    #[must_use]
    pub fn peer(self, addr: SocketAddr) -> Self {
        Self {
            peer: Some(addr),
            ..self
        }
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
            peer: self.peer,
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
        if let Some(peer) = self.peer {
            builder = builder.extension(ConnectInfo(peer));
        }
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
