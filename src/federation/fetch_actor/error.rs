//! How fetching a remote actor can fail: the [`FetchError`] vocabulary
//! with its rendering, its error sources and its conversions, plus
//! [`server_error`]'s fold of a server-side [`AppError`] into a
//! client-safe [`FetchError::Invalid`] (decision 8).

use std::{error::Error, fmt};

use tracing::error;

use crate::{
    error::AppError,
    federation::{remote_actor::ActorError, webfinger_client::WebfingerError},
    http_client::HttpError,
};

/// How fetching a remote actor can fail.
#[derive(Debug)]
pub enum FetchError {
    /// The request itself failed or the peer answered a status that is not
    /// one of the actor-specific ones: the transport and status failures of
    /// [`crate::http_client`].
    Http(HttpError),
    /// The peer answered 410: the actor is permanently gone. A stale
    /// mirror must not mask this one.
    Gone,
    /// No actor behind the address: the peer answered 404, or a *local*
    /// actor id names a row this database does not have.
    NotFound,
    /// The answer cannot become a mirror: a non-JSON content type, an
    /// unparsable or non-self-consistent document, a database failure
    /// while storing it, or a fetch that could not be signed. The string
    /// states which.
    Invalid(String),
    /// A handle lookup fetched an actor that does not name the requested
    /// `acct` (the Mastodon round trip of this module's docs).
    AcctMismatch,
    /// The `WebFinger` step of an `acct` lookup failed.
    Webfinger(WebfingerError),
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Http(err) => write!(f, "actor fetch failed: {err}"),
            Self::Gone => f.write_str("the actor is gone"),
            Self::NotFound => f.write_str("the actor was not found"),
            Self::Invalid(detail) => f.write_str(detail),
            Self::AcctMismatch => {
                f.write_str("the fetched actor does not belong to the requested account")
            }
            Self::Webfinger(err) => write!(f, "could not resolve the account: {err}"),
        }
    }
}

impl Error for FetchError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Http(err) => Some(err),
            Self::Webfinger(err) => Some(err),
            Self::Gone | Self::NotFound | Self::Invalid(_) | Self::AcctMismatch => None,
        }
    }
}

/// Any transport or status failure of the shared client is an
/// [`FetchError::Http`], so `?` works throughout the fetch.
impl From<HttpError> for FetchError {
    fn from(err: HttpError) -> Self {
        Self::Http(err)
    }
}

/// Any `WebFinger` failure of an `acct` lookup is a
/// [`FetchError::Webfinger`].
impl From<WebfingerError> for FetchError {
    fn from(err: WebfingerError) -> Self {
        Self::Webfinger(err)
    }
}

/// A rejected or unstashable actor document is a [`FetchError::Invalid`]
/// carrying the parser's field detail; the database calls of
/// [`upsert_remote_user`](crate::federation::remote_actor::upsert_remote_user)
/// fold into the same variant through `server_error`.
impl From<ActorError> for FetchError {
    fn from(err: ActorError) -> Self {
        match err {
            ActorError::Failed(server) => server_error(&server),
            ActorError::Invalid(detail) => Self::Invalid(format!("invalid remote actor: {detail}")),
            ActorError::NicknameConflict => Self::Invalid(
                "invalid remote actor: the nickname is owned by a different actor id".to_owned(),
            ),
        }
    }
}

impl From<FetchError> for AppError {
    fn from(err: FetchError) -> Self {
        match err {
            FetchError::Gone | FetchError::NotFound => Self::NotFound,
            FetchError::Invalid(detail) => Self::Unprocessable(detail),
            FetchError::AcctMismatch => {
                Self::Unprocessable("the fetched actor is not the requested account".to_owned())
            }
            FetchError::Http(err) => Self::BadGateway(format!("actor dereference failed: {err}")),
            FetchError::Webfinger(WebfingerError::InvalidAcct) => {
                Self::Unprocessable("not a valid acct:nick@domain handle".to_owned())
            }
            FetchError::Webfinger(err) => {
                Self::BadGateway(format!("webfinger lookup failed: {err}"))
            }
        }
    }
}

/// Folds a server-side [`AppError`] (the database calls of the lookups)
/// into [`FetchError::Invalid`]: its real cause is logged, and the detail
/// callers may surface stays a fixed message — server-side causes never
/// ride into a client body, per decision 8.
pub(super) fn server_error(err: &AppError) -> FetchError {
    if matches!(err, AppError::NotFound) {
        return FetchError::NotFound;
    }
    error!(%err, "remote actor lookup failed server-side");
    FetchError::Invalid("could not store or look up the actor".to_owned())
}

#[cfg(test)]
mod tests {
    use axum::{http::StatusCode, response::IntoResponse as _};

    use super::*;

    #[test]
    fn fetch_errors_render_their_causes() {
        assert_eq!(FetchError::Gone.to_string(), "the actor is gone", "Gone");
        assert_eq!(
            FetchError::NotFound.to_string(),
            "the actor was not found",
            "NotFound"
        );
        assert_eq!(
            FetchError::Invalid("bad doc".to_owned()).to_string(),
            "bad doc",
            "Invalid passes its detail through"
        );
        assert_eq!(
            FetchError::AcctMismatch.to_string(),
            "the fetched actor does not belong to the requested account",
            "AcctMismatch"
        );
        assert_eq!(
            FetchError::Http(HttpError::Timeout).to_string(),
            "actor fetch failed: HTTP request timed out",
            "Http keeps the transport message"
        );
        assert_eq!(
            FetchError::Webfinger(WebfingerError::Malformed).to_string(),
            "could not resolve the account: webfinger document is malformed",
            "Webfinger keeps the resolver's message"
        );
        assert!(
            Error::source(&FetchError::Http(HttpError::Timeout)).is_some(),
            "Http keeps its source"
        );
        assert!(
            Error::source(&FetchError::Webfinger(WebfingerError::Malformed)).is_some(),
            "and so does Webfinger"
        );
        assert!(
            Error::source(&FetchError::Gone).is_none(),
            "a plain Gone has no source"
        );
    }

    #[tokio::test]
    async fn fetch_errors_map_to_response_statuses() {
        for (error, status) in [
            (FetchError::Gone, StatusCode::NOT_FOUND),
            (FetchError::NotFound, StatusCode::NOT_FOUND),
            (
                FetchError::Invalid("bad".to_owned()),
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
            (FetchError::AcctMismatch, StatusCode::UNPROCESSABLE_ENTITY),
            (
                FetchError::Http(HttpError::Timeout),
                StatusCode::BAD_GATEWAY,
            ),
            (
                FetchError::Webfinger(WebfingerError::InvalidAcct),
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
            (
                FetchError::Webfinger(WebfingerError::Http(HttpError::Status(404))),
                StatusCode::BAD_GATEWAY,
            ),
        ] {
            let shown = format!("{error:?}");
            let response = AppError::from(error).into_response();
            assert_eq!(response.status(), status, "status of {shown}");
        }
    }
}
