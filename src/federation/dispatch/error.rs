//! How an activity fails on the way in: the [`FederationError`] the handlers
//! and the dispatcher's own stages report, and its mapping onto the job's
//! treatment ([`JobError`]).

use std::{error::Error, fmt};

use tracing::warn;

use crate::jobs::JobError;

/// Every way an activity can fail on the way in, as the handlers and the
/// dispatcher's own stages report it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FederationError {
    /// A database or network failure: the work may succeed on a later
    /// attempt, so the dispatcher turns it into a [`JobError::Retry`] and
    /// releases the dedupe row for the retry to find.
    Transient(String),
    /// A failure no retry can fix — a malformed document, an operation
    /// the peer got wrong: the dispatcher logs it and discards the job.
    Permanent(String),
}

impl fmt::Display for FederationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transient(detail) => write!(f, "transient federation failure: {detail}"),
            Self::Permanent(detail) => write!(f, "permanent federation failure: {detail}"),
        }
    }
}

impl Error for FederationError {}

/// Whether the worker will run the job again, which is exactly when the
/// ledger row must not stand in its way.
pub(super) const fn is_retryable(err: &JobError) -> bool {
    matches!(err, JobError::Retry(_) | JobError::RetryAfter(_, _))
}

/// Maps a handler's failure onto the job's treatment, per the ticket's
/// step 7: transient is a retry, everything else a logged discard.
pub(super) fn job_error_from(err: FederationError) -> JobError {
    match err {
        FederationError::Transient(detail) => JobError::Retry(detail),
        FederationError::Permanent(detail) => {
            warn!(%detail, "discarding an activity its handler refused");
            JobError::Discard(detail)
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::federation::{
        dispatch::{
            dispatch_activity,
            test_support::{app_state, incoming_args, ledger_rows, seen, signer, test_handlers},
        },
        mrf,
    };

    /// A permanent failure discards the job and keeps the ledger row: a
    /// redelivery of a refused activity must not run it again.
    #[tokio::test]
    async fn a_permanent_failure_discards_and_keeps_the_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = signer(&pool).await;
        let activity = json!({
            "id": "https://peer.example/activities/discard-1",
            "type": "Block",
            "actor": "https://peer.example/users/alice",
            "object": "https://test.example/users/carol",
        });

        let outcome = dispatch_activity(
            &state,
            incoming_args(&activity, &alice),
            &test_handlers(),
            mrf::filter_incoming_boxed,
        )
        .await;

        assert!(
            matches!(&outcome, Err(JobError::Discard(detail)) if detail == "injected"),
            "the permanent failure is a discard: {outcome:?}"
        );
        assert_eq!(
            seen("permanent", "https://peer.example/activities/discard-1"),
            1,
            "the handler ran once, then nothing more"
        );
        assert_eq!(
            ledger_rows(&pool, "https://peer.example/activities/discard-1").await,
            1,
            "the refused activity stays claimed"
        );
    }
}
