//! The one error type handlers return, and how it reaches the client.
//!
//! Every [`AppError`] is answered with `content-type: application/json` and the
//! body `{"error": "<message>"}`, where `<message>` is [`AppError::message`].
//! Causes that must not reach a client ([`AppError::Internal`],
//! [`AppError::Database`]) are written to the log with `tracing` instead.

use std::{error::Error, fmt};

use axum::{
    Json,
    http::{HeaderName, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Serialize;

/// Message for [`AppError::NotFound`], and for a query that found no row.
const NOT_FOUND_MESSAGE: &str = "Record not found";
/// Message for [`AppError::RateLimited`], as Pleroma's rate limiter words it.
const RATE_LIMITED_MESSAGE: &str = "Throttled";
/// Message for server-side failures, whose cause stays in the log.
const INTERNAL_MESSAGE: &str = "Something went wrong";
/// `WWW-Authenticate` value sent with every [`AppError::Unauthorized`].
const BEARER_CHALLENGE: &str = "Bearer";

/// Any failure an API handler can report.
///
/// Handlers return [`AppResult`]. The status code and the message each variant
/// produces are in [`AppError::status_code`] and [`AppError::message`].
#[derive(Debug)]
pub enum AppError {
    /// The request was malformed or held an invalid value: 400.
    BadRequest(String),
    /// No usable bearer token came with the request: 401.
    Unauthorized(String),
    /// The caller is authenticated but not allowed here: 403.
    Forbidden(String),
    /// The addressed record does not exist: 404.
    NotFound,
    /// The request clashes with the record as it stands: 409.
    Conflict(String),
    /// The request body's content type is not one the endpoint parses
    /// (415); see [`crate::extract`] for the accepted types.
    UnsupportedMediaType(String),
    /// The request was understood but cannot be carried out: 422.
    Unprocessable(String),
    /// The caller has been throttled: 429.
    RateLimited,
    /// A server-side failure: 500. The cause is logged, never sent.
    Internal(String),
    /// A database failure: 500, or 404 for [`sqlx::Error::RowNotFound`]. The
    /// cause is logged, never sent.
    Database(sqlx::Error),
}

impl AppError {
    /// The HTTP status code this error is answered with.
    #[must_use]
    pub const fn status_code(&self) -> StatusCode {
        match self {
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            Self::Forbidden(_) => StatusCode::FORBIDDEN,
            Self::NotFound | Self::Database(sqlx::Error::RowNotFound) => StatusCode::NOT_FOUND,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::UnsupportedMediaType(_) => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Self::Unprocessable(_) => StatusCode::UNPROCESSABLE_ENTITY,
            Self::RateLimited => StatusCode::TOO_MANY_REQUESTS,
            Self::Internal(_) | Self::Database(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// The message put in the JSON body: the caller's own words for a request
    /// error, a fixed one for a missing record, and a fixed one hiding the
    /// cause of a server-side failure.
    #[must_use]
    pub fn message(&self) -> &str {
        match self {
            Self::BadRequest(message)
            | Self::Unauthorized(message)
            | Self::Forbidden(message)
            | Self::Conflict(message)
            | Self::UnsupportedMediaType(message)
            | Self::Unprocessable(message) => message,
            Self::NotFound | Self::Database(sqlx::Error::RowNotFound) => NOT_FOUND_MESSAGE,
            Self::RateLimited => RATE_LIMITED_MESSAGE,
            Self::Internal(_) | Self::Database(_) => INTERNAL_MESSAGE,
        }
    }

    /// Writes the cause of a server-side failure to the log. A query that found
    /// no row is an ordinary 404, so it stays as quiet as
    /// [`AppError::NotFound`] does.
    fn log_cause(&self) {
        match self {
            Self::Internal(message) => tracing::error!(%message, "internal server error"),
            Self::Database(err) => {
                if !matches!(err, sqlx::Error::RowNotFound) {
                    tracing::error!(error = %err, "database error");
                }
            }
            Self::BadRequest(_)
            | Self::Unauthorized(_)
            | Self::Forbidden(_)
            | Self::NotFound
            | Self::Conflict(_)
            | Self::UnsupportedMediaType(_)
            | Self::Unprocessable(_)
            | Self::RateLimited => {}
        }
    }

    /// The `WWW-Authenticate` challenge a 401 carries; no extra headers for
    /// anything else.
    const fn auth_challenge(&self) -> Option<[(HeaderName, HeaderValue); 1]> {
        match self {
            Self::Unauthorized(_) => Some([(
                header::WWW_AUTHENTICATE,
                HeaderValue::from_static(BEARER_CHALLENGE),
            )]),
            Self::BadRequest(_)
            | Self::Forbidden(_)
            | Self::NotFound
            | Self::Conflict(_)
            | Self::UnsupportedMediaType(_)
            | Self::Unprocessable(_)
            | Self::RateLimited
            | Self::Internal(_)
            | Self::Database(_) => None,
        }
    }

    /// The JSON body this error is answered with.
    fn json_body(&self) -> Json<ErrorBody<'_>> {
        Json(ErrorBody {
            error: self.message(),
        })
    }
}

/// The shape of every error response.
#[derive(Debug, Serialize)]
struct ErrorBody<'a> {
    /// The message that is safe to show the caller.
    error: &'a str,
}

/// Describes the error by its cause, including the one hidden from a 500 body.
impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadRequest(message)
            | Self::Unauthorized(message)
            | Self::Forbidden(message)
            | Self::Conflict(message)
            | Self::UnsupportedMediaType(message)
            | Self::Unprocessable(message)
            | Self::Internal(message) => f.write_str(message),
            Self::NotFound => f.write_str(NOT_FOUND_MESSAGE),
            Self::RateLimited => f.write_str(RATE_LIMITED_MESSAGE),
            Self::Database(err) => write!(f, "database error: {err}"),
        }
    }
}

impl Error for AppError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(err) => Some(err),
            Self::BadRequest(_)
            | Self::Unauthorized(_)
            | Self::Forbidden(_)
            | Self::NotFound
            | Self::Conflict(_)
            | Self::UnsupportedMediaType(_)
            | Self::Unprocessable(_)
            | Self::RateLimited
            | Self::Internal(_) => None,
        }
    }
}

/// Turns a database error into [`AppError::Database`], except a missing row,
/// which becomes an ordinary [`AppError::NotFound`] so that `?` in a handler
/// answers 404.
impl From<sqlx::Error> for AppError {
    fn from(err: sqlx::Error) -> Self {
        if matches!(err, sqlx::Error::RowNotFound) {
            Self::NotFound
        } else {
            Self::Database(err)
        }
    }
}

/// Logs the cause of a server-side failure, then renders the JSON response.
impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        self.log_cause();
        let body = self.json_body();
        (self.status_code(), self.auth_challenge(), body).into_response()
    }
}

/// What an API handler returns: a value, or an [`AppError`].
pub type AppResult<T> = Result<T, AppError>;

#[cfg(test)]
mod tests {
    use super::*;

    use std::io;

    use axum::http::HeaderMap;
    use http_body_util::BodyExt as _;

    /// A cause that must never reach a response body.
    const SECRET: &str = "host=private password=s3cret-in-dsn";

    /// The body a server-side failure is answered with.
    const INTERNAL_BODY: &str = r#"{"error":"Something went wrong"}"#;

    /// The body an error carrying this message is answered with.
    fn body_of(message: &str) -> String {
        format!(r#"{{"error":"{message}"}}"#)
    }

    /// Renders an error the way axum renders a handler's returned `AppError`.
    async fn render(error: AppError) -> (StatusCode, HeaderMap, String) {
        let (parts, body) = error.into_response().into_parts();
        let bytes = body.collect().await.unwrap().to_bytes();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        (parts.status, parts.headers, text)
    }

    #[tokio::test]
    async fn every_variant_answers_with_its_status_and_exact_body() {
        let cases = [
            (
                AppError::BadRequest("bad id".to_owned()),
                StatusCode::BAD_REQUEST,
                body_of("bad id"),
            ),
            (
                AppError::Unauthorized("no token".to_owned()),
                StatusCode::UNAUTHORIZED,
                body_of("no token"),
            ),
            (
                AppError::Forbidden("not yours".to_owned()),
                StatusCode::FORBIDDEN,
                body_of("not yours"),
            ),
            (
                AppError::NotFound,
                StatusCode::NOT_FOUND,
                body_of(NOT_FOUND_MESSAGE),
            ),
            (
                AppError::Conflict("already taken".to_owned()),
                StatusCode::CONFLICT,
                body_of("already taken"),
            ),
            (
                AppError::UnsupportedMediaType("nope".to_owned()),
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                body_of("nope"),
            ),
            (
                AppError::Unprocessable("too long".to_owned()),
                StatusCode::UNPROCESSABLE_ENTITY,
                body_of("too long"),
            ),
            (
                AppError::RateLimited,
                StatusCode::TOO_MANY_REQUESTS,
                body_of(RATE_LIMITED_MESSAGE),
            ),
            (
                AppError::Internal(SECRET.to_owned()),
                StatusCode::INTERNAL_SERVER_ERROR,
                INTERNAL_BODY.to_owned(),
            ),
            (
                AppError::Database(sqlx::Error::Io(io::Error::other(SECRET))),
                StatusCode::INTERNAL_SERVER_ERROR,
                INTERNAL_BODY.to_owned(),
            ),
            (
                AppError::Database(sqlx::Error::RowNotFound),
                StatusCode::NOT_FOUND,
                body_of(NOT_FOUND_MESSAGE),
            ),
        ];
        for (error, want_status, want_body) in cases {
            let (status, headers, body) = render(error).await;
            assert_eq!(status, want_status, "status for the {want_body} body");
            assert_eq!(
                headers
                    .get(header::CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok()),
                Some("application/json"),
                "content type for the {want_body} body"
            );
            assert_eq!(body, want_body, "body for status {status}");
        }
    }

    #[tokio::test]
    async fn unauthorized_challenges_with_bearer() {
        let (status, headers, _) = render(AppError::Unauthorized("expired".to_owned())).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "status");
        assert_eq!(
            headers
                .get_all(header::WWW_AUTHENTICATE)
                .iter()
                .collect::<Vec<_>>(),
            vec![HeaderValue::from_static(BEARER_CHALLENGE)],
            "exactly one bearer challenge"
        );
    }

    #[tokio::test]
    async fn other_errors_send_no_auth_challenge() {
        for error in [
            AppError::BadRequest("nope".to_owned()),
            AppError::Forbidden("nope".to_owned()),
            AppError::NotFound,
            AppError::RateLimited,
            AppError::Internal("nope".to_owned()),
            AppError::Database(sqlx::Error::WorkerCrashed),
        ] {
            let want_status = error.status_code();
            let (_, headers, _) = render(error).await;
            assert!(
                headers.get(header::WWW_AUTHENTICATE).is_none(),
                "no challenge for {want_status}"
            );
        }
    }

    #[tokio::test]
    async fn a_missing_row_becomes_a_plain_404() {
        let converted: AppError = sqlx::Error::RowNotFound.into();
        assert!(
            matches!(converted, AppError::NotFound),
            "RowNotFound maps to NotFound"
        );
        let (status, _, body) = render(converted).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "converted status");
        assert_eq!(body, body_of(NOT_FOUND_MESSAGE), "converted body");
    }

    #[tokio::test]
    async fn other_database_errors_stay_generic_and_do_not_leak() {
        for err in [
            sqlx::Error::Io(io::Error::other(SECRET)),
            sqlx::Error::Protocol(SECRET.to_owned()),
            sqlx::Error::ColumnNotFound(SECRET.to_owned()),
        ] {
            let converted: AppError = err.into();
            assert!(
                matches!(converted, AppError::Database(_)),
                "a database error stays in AppError::Database"
            );
            let want_status = converted.status_code();
            let (status, _, body) = render(converted).await;
            assert_eq!(status, want_status, "status stays {want_status}");
            assert_eq!(body, INTERNAL_BODY, "generic body for {want_status}");
            assert!(!body.contains(SECRET), "cause must not leak: {body}");
        }
    }

    #[test]
    fn display_keeps_the_real_cause_a_500_hides() {
        let internal = AppError::Internal(SECRET.to_owned());
        assert_eq!(internal.to_string(), SECRET, "display shows the cause");
        assert_eq!(
            internal.message(),
            INTERNAL_MESSAGE,
            "the body stays generic"
        );

        let cause = sqlx::Error::Io(io::Error::other(SECRET));
        let cause_text = cause.to_string();
        let database = AppError::Database(cause);
        assert_eq!(
            database.to_string(),
            format!("database error: {cause_text}"),
            "display shows the sqlx error"
        );
        assert_eq!(
            database.message(),
            INTERNAL_MESSAGE,
            "the body stays generic"
        );
    }

    #[test]
    fn database_errors_expose_their_source() {
        let error = AppError::Database(sqlx::Error::PoolClosed);
        assert_eq!(
            error.status_code(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "status"
        );
        let source = error.source().expect("the sqlx error is the source");
        assert_eq!(
            source.to_string(),
            sqlx::Error::PoolClosed.to_string(),
            "the source is the sqlx error"
        );
    }

    #[test]
    fn every_other_variant_has_no_source() {
        for error in [
            AppError::BadRequest("x".to_owned()),
            AppError::Unauthorized("x".to_owned()),
            AppError::Forbidden("x".to_owned()),
            AppError::NotFound,
            AppError::Conflict("x".to_owned()),
            AppError::UnsupportedMediaType("x".to_owned()),
            AppError::Unprocessable("x".to_owned()),
            AppError::RateLimited,
            AppError::Internal("x".to_owned()),
        ] {
            assert!(error.source().is_none(), "no source for {error}");
        }
    }

    #[test]
    fn app_error_can_be_used_as_a_std_error() {
        let boxed: Box<dyn Error> = Box::new(AppError::NotFound);
        assert_eq!(boxed.to_string(), NOT_FOUND_MESSAGE, "via dyn Error");
    }

    /// Handler-shaped: `?` on a query converts a `sqlx::Error` into `AppResult`.
    fn first_nickname(nicknames: &[&'static str]) -> AppResult<&'static str> {
        let first = nicknames.first().ok_or(sqlx::Error::RowNotFound)?;
        Ok(first)
    }

    #[test]
    fn app_result_carries_errors_converted_with_the_question_mark() {
        let missing = first_nickname(&[]).unwrap_err();
        assert_eq!(
            missing.status_code(),
            StatusCode::NOT_FOUND,
            "the question mark converted RowNotFound"
        );
        assert_eq!(
            first_nickname(&["alice"]).unwrap(),
            "alice",
            "the happy path passes through"
        );
    }
}
