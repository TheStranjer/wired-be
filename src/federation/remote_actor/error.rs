//! [`ActorError`]: every way a remote actor document can fail to be mirrored.

use std::{error::Error, fmt};

use sqlx::Error as SqlxError;

use crate::error::AppError;

/// Every way a remote actor document can fail to become a stored user.
#[derive(Debug)]
pub enum ActorError {
    /// The document is not a usable actor: a required field is missing or
    /// malformed, or the `publicKey` names a different actor. The string
    /// names the offending field.
    Invalid(String),
    /// The nickname the document implies (`preferredUsername@host`) is held
    /// by a different `ap_id` that is *not* a stale mirror of the same
    /// origin: a local row, or a remote row of another domain. A remote row
    /// of the same domain is the account moved or recreated, and the upsert
    /// renames it to `<its id>.<nickname>` and proceeds, as Pleroma does
    /// (`ActivityPub.maybe_handle_clashing_nickname`). Since the nickname's
    /// host is the actor id's host, only a local holder can reach this in
    /// practice; it stays as a guard so such a row is never renamed or
    /// overwritten.
    NicknameConflict,
    /// A server-side failure while storing the row: the database calls of
    /// [`upsert_remote_user`](super::upsert_remote_user).
    Failed(AppError),
}

impl fmt::Display for ActorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(detail) => write!(f, "invalid remote actor: {detail}"),
            Self::NicknameConflict => {
                write!(f, "the nickname is owned by a different actor id")
            }
            Self::Failed(err) => write!(f, "could not store the remote actor: {err}"),
        }
    }
}

impl Error for ActorError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Invalid(_) | Self::NicknameConflict => None,
            Self::Failed(err) => Some(err),
        }
    }
}

/// Any [`AppError`] is a server-side [`ActorError::Failed`], so `?` works
/// inside [`upsert_remote_user`](super::upsert_remote_user).
impl From<AppError> for ActorError {
    fn from(err: AppError) -> Self {
        Self::Failed(err)
    }
}

/// A database failure converts through [`AppError`], keeping the
/// row-not-found mapping of `impl From<SqlxError> for AppError` intact.
impl From<SqlxError> for ActorError {
    fn from(err: SqlxError) -> Self {
        Self::Failed(AppError::from(err))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actor_errors_render_their_causes() {
        assert_eq!(
            ActorError::Invalid("`id` is missing".to_owned()).to_string(),
            "invalid remote actor: `id` is missing",
            "Invalid shows its detail"
        );
        assert_eq!(
            ActorError::NicknameConflict.to_string(),
            "the nickname is owned by a different actor id"
        );
        let failed = ActorError::from(SqlxError::PoolClosed);
        let message = failed.to_string();
        assert!(
            message.starts_with("could not store the remote actor: "),
            "Failed prefixes its cause, got {message}"
        );
        assert!(
            message.contains("closed pool"),
            "the Display of the wrapped AppError shows through, got {message}"
        );
        assert!(Error::source(&failed).is_some(), "Failed keeps the source");
        assert!(
            Error::source(&ActorError::NicknameConflict).is_none(),
            "a plain conflict has no source"
        );
    }
}
