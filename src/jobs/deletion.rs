//! The account-deletion jobs: clearing a departed account's data.
//!
//! `POST /api/pleroma/delete_account` (WBE-136, `delete_account` in
//! [`crate::api::pleroma_account_settings`]) marks the account deleted and
//! queues [`users.delete`](KIND_DELETE) on
//! [`QUEUE_BACKGROUND`](super::QUEUE_BACKGROUND) — the counterpart of
//! Pleroma's `User.delete` enqueueing its `DeleteWorker`
//! (`lib/pleroma/user.ex`). The endpoint answers immediately; everything
//! slow happens here.
//!
//! The job walks the account's data and takes it with it:
//!
//! 1. every status the account authored becomes a tombstone,
//!    [`soft_delete_all_for_user`](crate::models::status::soft_delete_all_for_user)
//!    batching 100 rows per statement and looping until a pass clears none
//!    (the ticket's soft delete in batches of 100);
//! 2. its follows, favourites, bookmarks, notifications, lists and media
//!    rows go through the per-table `delete_all_for_user` writers, each
//!    taking exactly what a hard delete of the user row would have
//!    cascaded;
//! 3. its scheduled statuses are deleted and each one's pending
//!    `scheduled_statuses.publish` job is cancelled, so nothing fires as the
//!    deleted actor;
//! 4. its backups lose their stored archives first (an already-missing file
//!    counts as gone, any other failure retries the pass) and then their
//!    rows, so the archived direct messages stop being served;
//! 5. its Web Push subscriptions go, the user row being kept and no cascade
//!    firing;
//! 6. the user row itself is kept for referential integrity — decision 7
//!    hangs mirrors, activities and relationships off it — but its
//!    personal data (email, password hash, display name) is replaced with
//!    empty values, Pleroma's `purge` step.
//!
//! [`users.delete_remote`](KIND_DELETE_REMOTE) is the same sweep for an
//! account that was never ours: the inbound `Delete` of a peer's own actor
//! ([`crate::federation::handlers::actor`]) marks the mirror inactive and
//! queues it. The differences are the ones the mirror forces — there is no
//! personal data to purge, no list to own and no upload to unlink, but the
//! peers whose follow rows go are live accounts whose denormalised counters
//! have to be stepped, so the follow half runs through
//! [`delete_all_for_user_with_counts`](crate::models::follow::delete_all_for_user_with_counts)
//! and the reaction half through
//! [`delete_all_for_user`](crate::models::emoji_reaction::delete_all_for_user)
//! (a local account's reactions leave with its row's cascade; a kept mirror
//! row keeps them alive until this sweeps them).
//!
//! Every write is idempotent — tombstoning a tombstone clears nothing,
//! deleting a missing row deletes nothing — so a retried pass converges on
//! the same state, which is why a database failure is a
//! [`JobError::Retry`] and only unusable arguments or a vanished user row
//! are a discard.
//!
//! Federation of the actor's `Delete` is the sweep's own first act:
//! [`crate::services::federation_out::after_account_deleted`] runs between
//! loading the account and clearing its rows, because the fan-out to the
//! remote followers and followees reads the relationships the clear step
//! deletes.

use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

mod local;
mod remote;
#[cfg(test)]
mod test_support;

pub use local::handle_delete;
pub use remote::handle_delete_remote;

use crate::{error::AppError, jobs::JobError};

/// The job kind of an account deletion, handled by [`handle_delete`] on
/// [`QUEUE_BACKGROUND`](super::QUEUE_BACKGROUND).
pub const KIND_DELETE: &str = "users.delete";

/// The job kind of a *remote* account's deletion — a peer's `Delete` of its
/// own actor — handled by [`handle_delete_remote`] on
/// [`QUEUE_BACKGROUND`](super::QUEUE_BACKGROUND).
pub const KIND_DELETE_REMOTE: &str = "users.delete_remote";

/// The `{user_id}` arguments of a delete job, as [`job_args`] writes and
/// both [`handle_delete`] and [`handle_delete_remote`] read them.
#[derive(Debug, Deserialize)]
struct DeleteArgs {
    /// The account whose data the job clears.
    user_id: Uuid,
}

/// The `{user_id}` job-arguments document for account `user_id`.
///
/// Both delete jobs take this one shape:
/// [`crate::api::pleroma_account_settings`] enqueues it for [`KIND_DELETE`],
/// [`crate::federation::handlers::actor`] for [`KIND_DELETE_REMOTE`], and
/// [`handle_delete`] and [`handle_delete_remote`] both deserialise it.
#[must_use]
pub fn job_args(user_id: Uuid) -> Value {
    json!({ "user_id": user_id })
}

/// Reads [`DeleteArgs`] out of a claimed job's payload; arguments this
/// shape cannot deserialize are permanent — the job is discarded, never
/// retried.
fn parse_args(args: &Value) -> Result<Uuid, JobError> {
    serde_json::from_value::<DeleteArgs>(args.clone())
        .map(|parsed| parsed.user_id)
        .map_err(|err| {
            JobError::Discard(format!("account delete job arguments are unusable: {err}"))
        })
}

/// Lifts a database failure into the queue's retry arm; every step is
/// idempotent, so another pass costs nothing but the wait.
fn retry(err: &AppError) -> JobError {
    JobError::Retry(format!("account delete pass failed: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::QUEUE_BACKGROUND;

    /// The job-arguments document is the `{user_id}` shape the endpoint
    /// enqueues with, on the `background` queue the ticket names.
    #[test]
    fn the_job_args_carry_the_user_id_alone() {
        let id = Uuid::now_v7();
        assert_eq!(job_args(id), json!({ "user_id": id }));
        assert_eq!(QUEUE_BACKGROUND, "background", "the ticket's queue");
    }
}
