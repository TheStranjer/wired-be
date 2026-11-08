//! The one error type handlers return, rendered as a JSON response.
//!
//! Every failure becomes a response whose status comes from the variant, whose
//! `content-type` is `application/json` and whose body is
//! `{"error": "<message>"}`, following `docs/ARCHITECTURE.md` decision 8. The
//! [`AppError::ForbiddenWithIdentifier`] variant adds Pleroma's
//! machine-readable reason code to that body
//! (`{"error": "<message>", "identifier": "<code>"}`), the shape the OAuth
//! endpoints use for account-status refusals like `account_is_disabled`;
//! [`AppError::forbidden_with_identifier`] builds one. The
//! messages mirror Pleroma's fallback controllers
//! (`lib/pleroma/web/mastodon_api/controllers/fallback_controller.ex` and
//! `lib/pleroma/web/fallback/redirect_controller.ex`): "Record not found"
//! for a missing row, "Not implemented" for an unrouted path under `/api`
//! (Pleroma's `api_not_implemented`), "Not found" for any other unrouted
//! path, "Throttled" for a rate limit (the wording of its rate limiter),
//! "Gone" for a resource that used to exist and no longer does (Mastodon's
//! wording for a deactivated account), "Something went wrong" for
//! server-side failures, whose real cause is only written to the log, and
//! 424/502 variants for upstreams this instance proxies to (see
//! [`crate::services::media_proxy`]) whose detail is sent as-is, and a 503
//! variant for a feature that is switched off or whose backing service does
//! not answer (see [`crate::services::translate`]), likewise sent as-is.

use std::{error::Error, fmt};

use axum::{
    Json,
    http::{HeaderValue, StatusCode, header::WWW_AUTHENTICATE},
    response::{IntoResponse, Response},
};
use serde::Serialize;
use sqlx::Error as SqlxError;
use tracing::error;

/// Body message for [`AppError::NotFound`], Pleroma's wording.
const NOT_FOUND_MESSAGE: &str = "Record not found";
/// Body message for [`AppError::NotImplemented`], Pleroma's
/// `api_not_implemented` wording.
const NOT_IMPLEMENTED_MESSAGE: &str = "Not implemented";
/// Body message for [`AppError::RouteNotFound`], the wording of Pleroma's
/// router-level 404.
const ROUTE_NOT_FOUND_MESSAGE: &str = "Not found";
/// Body message for [`AppError::Gone`], the wording Mastodon uses for an
/// account that is deactivated (and so no longer served).
const GONE_MESSAGE: &str = "Gone";
/// Body message for [`AppError::RateLimited`], Pleroma's rate limiter wording.
const RATE_LIMITED_MESSAGE: &str = "Throttled";
/// Body message for failures whose real cause must stay in the log.
const INTERNAL_MESSAGE: &str = "Something went wrong";
/// `WWW-Authenticate` value sent with every 401: the API speaks bearer tokens.
const BEARER_CHALLENGE: &str = "Bearer";

/// Any way a request can fail, each variant carrying its HTTP status.
#[derive(Debug)]
pub enum AppError {
    /// The request was malformed (400).
    BadRequest(String),
    /// The credentials are missing or invalid (401).
    Unauthorized(String),
    /// The caller is known but not allowed to do this (403).
    Forbidden(String),
    /// A [`Self::Forbidden`] whose body additionally carries Pleroma's
    /// machine-readable reason code: `{"error": message, "identifier": id}`
    /// (the OAuth account-status refusals, e.g. `account_is_disabled`).
    /// Build one with [`AppError::forbidden_with_identifier`].
    ForbiddenWithIdentifier(String, &'static str),
    /// No record matches the request (404).
    NotFound,
    /// No record matches the request, with the endpoint-specific message
    /// the client is owed (404): Pleroma's emoji pack endpoints answer an
    /// unknown pack with `{"error": "Pack not found"}` rather than the
    /// generic "Record not found".
    NotFoundMessage(String),
    /// The request path is under `/api` but no endpoint is routed there
    /// (404), Pleroma's `api_not_implemented`.
    NotImplemented,
    /// No route matches the request path at all (404). Unlike
    /// [`Self::NotFound`], which reports a missing record to an endpoint
    /// that exists, this is the router-level fallback.
    RouteNotFound,
    /// The resource is permanently gone: a deactivated local account's
    /// `ActivityPub` actor (410), so federation stops retrying it.
    Gone,
    /// The request conflicts with existing state, e.g. a duplicate (409).
    Conflict(String),
    /// The request body, or one part of it, is larger than the endpoint
    /// accepts (413); see [`crate::extract::multipart`] for the upload
    /// limits.
    PayloadTooLarge(String),
    /// The request body's content type is not one the endpoint parses
    /// (415); see [`crate::extract`] for the accepted types.
    UnsupportedMediaType(String),
    /// The upstream this endpoint fronts refused or failed the request,
    /// or its body is not of a kind this endpoint serves (424 Failed
    /// Dependency, Pleroma's answer when a media proxy fetch cannot be
    /// served, `lib/pleroma/web/media_proxy/media_proxy_controller.ex`).
    FailedDependency(String),
    /// The upstream server this instance proxies to could not be reached
    /// or answered unhelpfully (502), e.g. a media proxy fetch whose
    /// transport failed (see [`crate::services::media_proxy`]). Unlike
    /// [`Self::BadGateway`], the federation-gateway error, its detail is
    /// a fixed client-safe message, sent as-is.
    UpstreamFailure(String),
    /// A feature the endpoint would need is switched off, or the service
    /// behind it is not answering (503 Service Unavailable): translation
    /// with no provider configured, or a provider whose call failed (see
    /// [`crate::services::translate`]). Like [`Self::UpstreamFailure`], its
    /// detail is a fixed client-safe message, sent as-is.
    ServiceUnavailable(String),
    /// The request is well formed but semantically invalid (422).
    Unprocessable(String),
    /// The caller exceeded its rate limit (429).
    RateLimited,
    /// An instance this one talked to upstream failed or misbehaved (502),
    /// the federation-gateway error: its detail is logged, never sent.
    BadGateway(String),
    /// A server-side failure; its detail is logged, never sent (500).
    Internal(String),
    /// A PostgreSQL failure; its detail is logged, never sent (500).
    Database(SqlxError),
}

impl AppError {
    /// The machine-readable reason code this error adds to its body, if
    /// any. Only [`Self::ForbiddenWithIdentifier`] carries one.
    #[must_use]
    const fn identifier(&self) -> Option<&'static str> {
        let Self::ForbiddenWithIdentifier(_, identifier) = self else {
            return None;
        };
        Some(identifier)
    }

    /// Build a 403 whose body is Pleroma's
    /// `{"error": message, "identifier": id}`: a forbidden response that
    /// also names a stable reason code (e.g. `account_is_disabled`).
    /// Later tickets use this for the other OAuth account-status refusals.
    #[must_use]
    pub fn forbidden_with_identifier(message: &str, identifier: &'static str) -> Self {
        Self::ForbiddenWithIdentifier(message.to_owned(), identifier)
    }

    /// The HTTP status code this error is answered with.
    const fn status(&self) -> StatusCode {
        match self {
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            Self::Forbidden(_) | Self::ForbiddenWithIdentifier(_, _) => StatusCode::FORBIDDEN,
            Self::NotFound
            | Self::NotFoundMessage(_)
            | Self::NotImplemented
            | Self::RouteNotFound => StatusCode::NOT_FOUND,
            Self::Gone => StatusCode::GONE,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::PayloadTooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
            Self::UnsupportedMediaType(_) => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Self::FailedDependency(_) => StatusCode::FAILED_DEPENDENCY,
            Self::ServiceUnavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            Self::Unprocessable(_) => StatusCode::UNPROCESSABLE_ENTITY,
            Self::RateLimited => StatusCode::TOO_MANY_REQUESTS,
            Self::UpstreamFailure(_) | Self::BadGateway(_) => StatusCode::BAD_GATEWAY,
            Self::Internal(_) | Self::Database(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// The message sent to the client: the variant's own text, except for
    /// server-side failures, which only reveal [`INTERNAL_MESSAGE`].
    fn message(&self) -> &str {
        match self {
            Self::BadRequest(detail)
            | Self::Unauthorized(detail)
            | Self::Forbidden(detail)
            | Self::ForbiddenWithIdentifier(detail, _)
            | Self::Conflict(detail)
            | Self::PayloadTooLarge(detail)
            | Self::UnsupportedMediaType(detail)
            | Self::FailedDependency(detail)
            | Self::UpstreamFailure(detail)
            | Self::ServiceUnavailable(detail)
            | Self::Unprocessable(detail)
            | Self::NotFoundMessage(detail) => detail,
            Self::NotFound => NOT_FOUND_MESSAGE,
            Self::NotImplemented => NOT_IMPLEMENTED_MESSAGE,
            Self::RouteNotFound => ROUTE_NOT_FOUND_MESSAGE,
            Self::Gone => GONE_MESSAGE,
            Self::RateLimited => RATE_LIMITED_MESSAGE,
            Self::BadGateway(_) | Self::Internal(_) | Self::Database(_) => INTERNAL_MESSAGE,
        }
    }

    /// Logs the detail of a server-side failure, which the client never sees.
    fn log_hidden_detail(&self) {
        let status = self.status();
        if let Self::Internal(detail) = self {
            error!(%detail, %status, "internal error");
        }
        if let Self::Database(detail) = self {
            error!(%detail, %status, "database error");
        }
        if let Self::BadGateway(detail) = self {
            error!(%detail, %status, "bad gateway");
        }
    }
}

/// `Display` states the real cause, so it is fit for logs; the response body
/// sends the client-safe message from `AppError::message` instead.
impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadRequest(detail)
            | Self::Unauthorized(detail)
            | Self::Forbidden(detail)
            | Self::ForbiddenWithIdentifier(detail, _)
            | Self::Conflict(detail)
            | Self::PayloadTooLarge(detail)
            | Self::UnsupportedMediaType(detail)
            | Self::FailedDependency(detail)
            | Self::UpstreamFailure(detail)
            | Self::ServiceUnavailable(detail)
            | Self::Unprocessable(detail)
            | Self::Internal(detail)
            | Self::BadGateway(detail)
            | Self::NotFoundMessage(detail) => f.write_str(detail),
            Self::NotFound => f.write_str(NOT_FOUND_MESSAGE),
            Self::NotImplemented => f.write_str(NOT_IMPLEMENTED_MESSAGE),
            Self::RouteNotFound => f.write_str(ROUTE_NOT_FOUND_MESSAGE),
            Self::Gone => f.write_str(GONE_MESSAGE),
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
            | Self::ForbiddenWithIdentifier(_, _)
            | Self::NotFound
            | Self::NotFoundMessage(_)
            | Self::NotImplemented
            | Self::RouteNotFound
            | Self::Gone
            | Self::Conflict(_)
            | Self::PayloadTooLarge(_)
            | Self::UnsupportedMediaType(_)
            | Self::FailedDependency(_)
            | Self::UpstreamFailure(_)
            | Self::ServiceUnavailable(_)
            | Self::Unprocessable(_)
            | Self::RateLimited
            | Self::BadGateway(_)
            | Self::Internal(_) => None,
        }
    }
}

/// A missing row is a 404 like any other absent record; every other database
/// failure becomes a 500 whose detail stays server-side.
impl From<SqlxError> for AppError {
    fn from(err: SqlxError) -> Self {
        if matches!(err, SqlxError::RowNotFound) {
            Self::NotFound
        } else {
            Self::Database(err)
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        self.log_hidden_detail();
        let status = self.status();
        let message = self.message();
        let body = self.identifier().map_or_else(
            || Json(ErrorBody { error: message }).into_response(),
            |identifier| {
                Json(IdentifiedErrorBody {
                    error: message,
                    identifier,
                })
                .into_response()
            },
        );
        if matches!(self, Self::Unauthorized(_)) {
            (
                status,
                [(WWW_AUTHENTICATE, HeaderValue::from_static(BEARER_CHALLENGE))],
                body,
            )
                .into_response()
        } else {
            (status, body).into_response()
        }
    }
}

/// The JSON shape of every error response.
#[derive(Debug, Serialize)]
struct ErrorBody<'a> {
    /// The client-safe description of what went wrong.
    error: &'a str,
}

/// The JSON shape of an error response that carries Pleroma's
/// machine-readable reason code beside the message
/// (`{"error": msg, "identifier": id}`), built by
/// [`AppError::ForbiddenWithIdentifier`].
#[derive(Debug, Serialize)]
struct IdentifiedErrorBody<'a> {
    /// The client-safe description of what went wrong.
    error: &'a str,
    /// Pleroma's stable reason code, e.g. `account_is_disabled`.
    identifier: &'a str,
}

/// A result that fails with [`AppError`], the return type of every handler.
pub type AppResult<T> = Result<T, AppError>;

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, header::CONTENT_TYPE};
    use http_body_util::BodyExt as _;

    /// Text that must never reach a client.
    const SECRET: &str = "password=hunter2";
    /// A Pleroma-style machine-readable reason code, as the identifier
    /// variant puts it in the body.
    const ACCOUNT_ID: &str = "account_is_disabled";

    /// The exact body an [`AppError`] is answered with for `message`.
    fn json_body(message: &str) -> String {
        format!(r#"{{"error":"{message}"}}"#)
    }

    /// Renders `error` into a response, reporting status, headers and body.
    async fn render(error: AppError) -> (StatusCode, HeaderMap, String) {
        let response = error.into_response();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        (status, headers, body)
    }

    /// Every variant, with the status and body message it must produce.
    fn cases() -> Vec<(AppError, StatusCode, String)> {
        [
            (AppError::BadRequest("bad".to_owned()), "bad"),
            (AppError::Unauthorized("no token".to_owned()), "no token"),
            (AppError::Forbidden("denied".to_owned()), "denied"),
            (AppError::NotFound, NOT_FOUND_MESSAGE),
            (
                AppError::NotFoundMessage("Pack not found".to_owned()),
                "Pack not found",
            ),
            (AppError::NotImplemented, NOT_IMPLEMENTED_MESSAGE),
            (AppError::RouteNotFound, ROUTE_NOT_FOUND_MESSAGE),
            (AppError::Gone, GONE_MESSAGE),
            (AppError::Conflict("taken".to_owned()), "taken"),
            (AppError::PayloadTooLarge("too big".to_owned()), "too big"),
            (
                AppError::UnsupportedMediaType("Unsupported media type".to_owned()),
                "Unsupported media type",
            ),
            (
                AppError::FailedDependency("Unsupported media type".to_owned()),
                "Unsupported media type",
            ),
            (AppError::UpstreamFailure("no route".to_owned()), "no route"),
            (
                AppError::ServiceUnavailable("Translation is not enabled".to_owned()),
                "Translation is not enabled",
            ),
            (AppError::Unprocessable("too long".to_owned()), "too long"),
            (AppError::RateLimited, RATE_LIMITED_MESSAGE),
            (AppError::BadGateway(SECRET.to_owned()), INTERNAL_MESSAGE),
            (AppError::Internal(SECRET.to_owned()), INTERNAL_MESSAGE),
            (
                AppError::Database(SqlxError::Protocol(SECRET.to_owned())),
                INTERNAL_MESSAGE,
            ),
        ]
        .into_iter()
        .map(|(error, message)| {
            let status = error.status();
            (error, status, json_body(message))
        })
        .collect()
    }

    #[tokio::test]
    async fn every_variant_has_its_status_and_json_body() {
        for (error, status, body) in cases() {
            let (got_status, headers, got_body) = render(error).await;
            assert_eq!(got_status, status, "status of body {body}");
            assert_eq!(got_body, body, "body of {status}");
            assert_eq!(
                headers.get(CONTENT_TYPE),
                Some(&HeaderValue::from_static("application/json")),
                "content-type of {status}"
            );
        }
    }

    #[tokio::test]
    async fn unauthorized_challenges_with_bearer() {
        let (status, headers, body) = render(AppError::Unauthorized("no token".to_owned())).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "401 status");
        assert_eq!(
            headers.get(WWW_AUTHENTICATE),
            Some(&HeaderValue::from_static(BEARER_CHALLENGE)),
            "a 401 carries the bearer challenge"
        );
        assert_eq!(body, json_body("no token"), "401 body");
    }

    #[tokio::test]
    async fn forbidden_with_identifier_adds_the_reason_code_to_the_body() {
        let error =
            AppError::forbidden_with_identifier("Your account is currently disabled", ACCOUNT_ID);
        let (status, headers, body) = render(error).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "the identifier 403 status");
        assert_eq!(
            headers.get(CONTENT_TYPE),
            Some(&HeaderValue::from_static("application/json")),
            "the identifier 403 is JSON"
        );
        assert!(
            !headers.contains_key(WWW_AUTHENTICATE),
            "a 403 does not challenge"
        );
        assert_eq!(
            body,
            format!(
                r#"{{"error":"Your account is currently disabled","identifier":"{ACCOUNT_ID}"}}"#
            ),
            "the body carries error first and identifier second"
        );
    }

    #[tokio::test]
    async fn plain_errors_do_not_leak_the_identifier_key() {
        for (error, _, body) in cases() {
            let (_, _, got_body) = render(error).await;
            assert_eq!(got_body, body, "no extra key in {body}");
        }
    }

    #[tokio::test]
    async fn no_other_status_challenges_with_bearer() {
        for (error, status, _) in cases() {
            let (_, headers, _) = render(error).await;
            assert_eq!(
                headers.contains_key(WWW_AUTHENTICATE),
                status == StatusCode::UNAUTHORIZED,
                "WWW-Authenticate belongs to 401 only, not to {status}"
            );
        }
    }

    #[tokio::test]
    async fn internal_error_hides_its_detail() {
        let (status, _, body) = render(AppError::Internal(SECRET.to_owned())).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "500 status");
        assert_eq!(body, json_body(INTERNAL_MESSAGE), "generic 500 body");
        assert!(!body.contains(SECRET), "internal detail must not leak");
    }

    #[tokio::test]
    async fn bad_gateway_hides_its_upstream_detail() {
        let (status, _, body) = render(AppError::BadGateway(SECRET.to_owned())).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "502 status");
        assert_eq!(body, json_body(INTERNAL_MESSAGE), "generic 502 body");
        assert!(!body.contains(SECRET), "upstream detail must not leak");
    }

    #[tokio::test]
    async fn upstream_failure_sends_its_client_safe_detail() {
        let (status, _, body) = render(AppError::UpstreamFailure(SECRET.to_owned())).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "502 status");
        assert_eq!(body, json_body(SECRET), "the proxy 502 body is its own");
    }

    #[tokio::test]
    async fn service_unavailable_sends_its_client_safe_detail() {
        let (status, headers, body) =
            render(AppError::ServiceUnavailable("no provider".to_owned())).await;
        assert_eq!(
            status,
            StatusCode::SERVICE_UNAVAILABLE,
            "503 status for a switched-off or failing service"
        );
        assert_eq!(
            headers.get(CONTENT_TYPE),
            Some(&HeaderValue::from_static("application/json")),
            "the 503 is JSON like every other error"
        );
        assert_eq!(
            body,
            json_body("no provider"),
            "the 503 body is its own message, sent as-is"
        );
    }

    #[tokio::test]
    async fn database_error_hides_its_detail() {
        let error = AppError::from(SqlxError::Protocol(SECRET.to_owned()));
        assert!(
            matches!(error, AppError::Database(_)),
            "a database failure is the Database variant"
        );
        let (status, _, body) = render(error).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "500 status");
        assert_eq!(body, json_body(INTERNAL_MESSAGE), "generic 500 body");
        assert!(!body.contains(SECRET), "database detail must not leak");
    }

    #[tokio::test]
    async fn row_not_found_maps_to_not_found() {
        let error = AppError::from(SqlxError::RowNotFound);
        assert!(matches!(error, AppError::NotFound), "a lost row is a 404");
        let (status, _, body) = render(error).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "404 status");
        assert_eq!(body, json_body(NOT_FOUND_MESSAGE), "404 body");
    }

    #[test]
    fn display_states_the_real_cause() {
        assert_eq!(
            AppError::BadRequest("bad".to_owned()).to_string(),
            "bad",
            "display of a client error"
        );
        assert_eq!(
            AppError::NotFound.to_string(),
            NOT_FOUND_MESSAGE,
            "display of not found"
        );
        assert_eq!(
            AppError::RouteNotFound.to_string(),
            ROUTE_NOT_FOUND_MESSAGE,
            "display of the router-level 404"
        );
        assert_eq!(
            AppError::Gone.to_string(),
            GONE_MESSAGE,
            "display of a gone resource"
        );
        assert_eq!(
            AppError::RateLimited.to_string(),
            RATE_LIMITED_MESSAGE,
            "display of a rate limit"
        );
        assert_eq!(
            AppError::Internal(SECRET.to_owned()).to_string(),
            SECRET,
            "display keeps the detail a 500 hides"
        );
        assert_eq!(
            AppError::BadGateway(SECRET.to_owned()).to_string(),
            SECRET,
            "display keeps the detail a 502 hides"
        );
        assert_eq!(
            AppError::UpstreamFailure("upstream failed".to_owned()).to_string(),
            "upstream failed",
            "display of the proxy 502 states its client-safe detail"
        );
        assert_eq!(
            AppError::ServiceUnavailable("Translation is not enabled".to_owned()).to_string(),
            "Translation is not enabled",
            "display of the 503 states its client-safe detail"
        );
        assert_eq!(
            AppError::forbidden_with_identifier("disabled", ACCOUNT_ID).to_string(),
            "disabled",
            "display keeps the message; the identifier is only in the body"
        );
        assert_eq!(
            AppError::from(SqlxError::ColumnNotFound("nope".to_owned())).to_string(),
            "database error: no column found for name: nope",
            "display of a database error"
        );
    }

    #[test]
    fn only_database_errors_have_a_source() {
        let error = AppError::from(SqlxError::PoolClosed);
        let source = error.source().expect("a database error keeps its source");
        assert_eq!(
            source.to_string(),
            "attempted to acquire a connection on a closed pool",
            "the source is the sqlx error"
        );
        assert!(
            AppError::RateLimited.source().is_none(),
            "a plain error has no source"
        );
        assert!(
            AppError::forbidden_with_identifier("disabled", ACCOUNT_ID)
                .source()
                .is_none(),
            "the identifier variant is a plain error too"
        );
    }

    #[test]
    fn app_result_carries_errors_converted_by_question_mark() {
        /// A handler shape: `?` turns a [`SqlxError`] into an [`AppError`].
        fn handler(found: bool) -> AppResult<&'static str> {
            if found {
                Ok("row")
            } else {
                Err(SqlxError::RowNotFound.into())
            }
        }

        assert_eq!(handler(true).unwrap(), "row", "the success side");
        assert_eq!(
            handler(false).unwrap_err().to_string(),
            NOT_FOUND_MESSAGE,
            "? on a sqlx call converts into AppError"
        );
    }
}
