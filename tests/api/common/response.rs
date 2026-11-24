//! The captured response and the shared gate assertions.

use std::fmt::Display;

use axum::{
    body::Bytes,
    http::{HeaderMap, HeaderName, StatusCode},
    response::Response,
};
use http_body_util::BodyExt as _;
use serde::de::DeserializeOwned;
use serde_json::Value;

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
    pub(super) async fn from_response(response: Response) -> Self {
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

    /// The `error` string of a JSON error response, or `None` when the body
    /// is not an object with a string `error` field.
    #[must_use]
    pub fn error_field(&self) -> Option<String> {
        let body: Value = serde_json::from_slice(&self.body).ok()?;
        Some(body.get("error")?.as_str()?.to_owned())
    }

    /// The `error` string of a JSON error response.
    ///
    /// # Panics
    ///
    /// If the body is not an object with a string `error` field.
    #[must_use]
    pub fn error_text(&self) -> String {
        let Some(error) = self.error_field() else {
            panic!("errors carry a string error field, got {}", self.text());
        };
        error
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

/// The status a gated route answers, naming the case `who` and showing the
/// body when the assertion fails.
///
/// # Panics
///
/// If the status differs.
pub fn assert_answer(response: &TestResponse, status: StatusCode, who: &str) {
    assert_eq!(response.status, status, "{who}: {}", response.text());
}

/// The `{"error": …}` body a gated route answers with, for `who`.
///
/// # Panics
///
/// If the rendered error differs.
pub fn assert_error(response: &TestResponse, message: &str, who: &str) {
    assert_eq!(
        response.json_value().get("error").and_then(Value::as_str),
        Some(message),
        "the refusal wording for {who}: {}",
        response.text()
    );
}

/// The status every response in `refused` answers, for `who`.
///
/// # Panics
///
/// If any status differs.
pub fn assert_all_answered(refused: &[TestResponse], status: StatusCode, who: &str) {
    for response in refused {
        assert_answer(response, status, who);
    }
}

/// The `{"error": …}` body every response in `refused` carries, for `who`.
///
/// # Panics
///
/// If any rendered error differs.
pub fn assert_all_errors(refused: &[TestResponse], message: &str, who: &str) {
    for response in refused {
        assert_error(response, message, who);
    }
}

/// The refusal a gated route answers for every response in `refused`:
/// `status`, with `message` as its `{"error": …}` body, for `who`.
///
/// # Panics
///
/// If any status or wording differs.
pub fn assert_all_refused(refused: &[TestResponse], status: StatusCode, message: &str, who: &str) {
    for response in refused {
        assert_answer(response, status, who);
        assert_error(response, message, who);
    }
}

/// The `Link` header Pleroma's `add_link_headers` writes for a page of the
/// listing at `path`: `next` walks to the `oldest` row's cursor, `prev` back
/// to the `newest`'s.
#[must_use]
pub fn expected_link(path: &str, newest: &dyn Display, oldest: &dyn Display) -> String {
    let base = wired_be::config::TEST_BASE_URL;
    format!(
        "<{base}{path}?max_id={oldest}>; rel=\"next\", \
         <{base}{path}?min_id={newest}>; rel=\"prev\""
    )
}

/// A mock peer's federation answer: `document` served as
/// `application/activity+json`, the media type the fetchers require
/// (axum's own `Json` answers `application/json`, which they refuse).
#[must_use]
pub fn activity_json(document: &Value) -> Response {
    use axum::{http::header::CONTENT_TYPE, response::IntoResponse as _};

    (
        [(CONTENT_TYPE, "application/activity+json")],
        document.to_string(),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A response with status 400 and `body` as its bytes.
    fn answer(body: &str) -> TestResponse {
        TestResponse {
            status: StatusCode::BAD_REQUEST,
            headers: HeaderMap::new(),
            body: Bytes::from(body.to_owned()),
        }
    }

    /// A string `error` field is read back by both accessors.
    #[test]
    fn error_field_and_text_read_the_error_string() {
        let response = answer(r#"{"error": "Invalid state"}"#);
        assert_eq!(response.error_field().as_deref(), Some("Invalid state"));
        assert_eq!(response.error_text(), "Invalid state");
    }

    /// A body without a string `error` is `None`, whatever its shape.
    #[test]
    fn error_field_is_none_without_a_string_error() {
        assert_eq!(answer(r#"{"error": 7}"#).error_field(), None);
        assert_eq!(answer(r#"{"other": "x"}"#).error_field(), None);
        assert_eq!(answer("not json").error_field(), None);
        assert_eq!(answer("[]").error_field(), None);
    }

    /// `error_text` panics when there is no string `error`.
    #[test]
    #[should_panic(expected = "string error field")]
    fn error_text_panics_without_a_string_error() {
        drop(answer(r#"{"other": "x"}"#).error_text());
    }

    /// The `next` link carries the oldest cursor, `prev` the newest.
    #[test]
    fn expected_link_puts_oldest_in_next_and_newest_in_prev() {
        assert_eq!(
            expected_link("/api/v1/blocks", &9, &"3"),
            format!(
                "<{base}/api/v1/blocks?max_id=3>; rel=\"next\", \
                 <{base}/api/v1/blocks?min_id=9>; rel=\"prev\"",
                base = wired_be::config::TEST_BASE_URL
            )
        );
    }
}
