//! The extractor's rejection type: [`InboxAuthError`] and its renderings —
//! the statused 401/403/400 family and the one non-failure 202 that drops
//! a gone actor's self-`Delete`.

use std::{error::Error, fmt};

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};

use super::{
    ACTOR_MISMATCH, INVALID_DIGEST, INVALID_JSON, INVALID_SIGNATURE, MISSING_SIGNATURE,
    PAYLOAD_TOO_LARGE, REJECTED, UNKNOWN_KEY, UNSUPPORTED_TYPE,
};
use crate::error::AppError;

/// A request rejected or dropped by the [`SignedActivity`] extractor.
///
/// Every authentication failure is an [`AppError`] under
/// [`Rejected`](Self::Rejected), rendered with its own status and
/// `{"error": …}` body (`docs/ARCHITECTURE.md` decision 8);
/// [`Discarded`](Self::Discarded) is the one non-failure: the activity is
/// answered `202 Accepted` and dropped without processing.
///
/// [`SignedActivity`]: super::SignedActivity
#[derive(Debug)]
pub enum InboxAuthError {
    /// The request failed one authentication step; its status and JSON
    /// error body come from the wrapped [`AppError`].
    Rejected(AppError),
    /// A `Delete` about an actor that answers `410 Gone` to the key
    /// resolution: accepted so the remote stops retrying, then dropped.
    Discarded,
}

impl InboxAuthError {
    /// The 413 [`Rejected`](Self::Rejected) for a body over the cap.
    pub(super) fn too_large() -> Self {
        Self::Rejected(AppError::PayloadTooLarge(PAYLOAD_TOO_LARGE.to_owned()))
    }

    /// The 415 [`Rejected`](Self::Rejected) for a non-JSON `Content-Type`.
    pub(super) fn wrong_type() -> Self {
        Self::Rejected(AppError::UnsupportedMediaType(UNSUPPORTED_TYPE.to_owned()))
    }

    /// The 400 [`Rejected`](Self::Rejected) for a body that is not JSON.
    pub(super) fn invalid_json() -> Self {
        Self::Rejected(AppError::BadRequest(INVALID_JSON.to_owned()))
    }

    /// The 401 [`Rejected`](Self::Rejected) for a missing or unparsable
    /// `Signature` header.
    pub(super) fn missing_signature() -> Self {
        Self::Rejected(AppError::Unauthorized(MISSING_SIGNATURE.to_owned()))
    }

    /// The 401 [`Rejected`](Self::Rejected) for a `Digest` that does not
    /// describe the received body.
    pub(super) fn invalid_digest() -> Self {
        Self::Rejected(AppError::Unauthorized(INVALID_DIGEST.to_owned()))
    }

    /// The 401 [`Rejected`](Self::Rejected) for a `keyId` that resolves to
    /// no readable actor key.
    pub(super) fn unknown_key() -> Self {
        Self::Rejected(AppError::Unauthorized(UNKNOWN_KEY.to_owned()))
    }

    /// The 401 [`Rejected`](Self::Rejected) for a signature that does not
    /// verify against the stored or the refetched key.
    pub(super) fn invalid_signature() -> Self {
        Self::Rejected(AppError::Unauthorized(INVALID_SIGNATURE.to_owned()))
    }

    /// The 401 [`Rejected`](Self::Rejected) for an activity whose `actor`
    /// is not the authenticated signer.
    pub(super) fn actor_mismatch() -> Self {
        Self::Rejected(AppError::Unauthorized(ACTOR_MISMATCH.to_owned()))
    }

    /// The 403 [`Rejected`](Self::Rejected) for a signer from a domain the
    /// instance policy rejects.
    pub(super) fn blocked() -> Self {
        Self::Rejected(AppError::Forbidden(REJECTED.to_owned()))
    }
}

/// Any [`AppError`] is an authentication rejection, so `?` works through
/// the extractor's database and fetch steps.
impl From<AppError> for InboxAuthError {
    fn from(err: AppError) -> Self {
        Self::Rejected(err)
    }
}

/// `Display` states the outcome; every payload is log-safe — no body, no
/// key material.
impl fmt::Display for InboxAuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rejected(err) => write!(f, "inbox authentication failed: {err}"),
            Self::Discarded => f.write_str("the Delete of a gone actor was accepted and dropped"),
        }
    }
}

impl Error for InboxAuthError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Rejected(err) => Some(err),
            Self::Discarded => None,
        }
    }
}

impl IntoResponse for InboxAuthError {
    fn into_response(self) -> Response {
        match self {
            Self::Rejected(err) => err.into_response(),
            Self::Discarded => StatusCode::ACCEPTED.into_response(),
        }
    }
}
