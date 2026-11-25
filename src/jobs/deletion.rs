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
//! 2. its follows go through
//!    [`delete_all_for_user_with_counts`](crate::models::follow::delete_all_for_user_with_counts),
//!    which also steps the counters of the live peers, and its favourites,
//!    emoji reactions, bookmarks, notifications, lists and media rows go
//!    through the per-table `delete_all_for_user` writers, each taking
//!    exactly what a hard delete of the user row would have cascaded; the
//!    statuses, follows, favourites, reactions, bookmarks and notifications
//!    are the one sweep both deletion kinds share, `clear_shared`;
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
//! personal data to purge, no list to own and no upload to unlink; the
//! follow half is the same counted sweep, and so is the reaction half
//! ([`delete_all_for_user`](crate::models::emoji_reaction::delete_all_for_user)).
//! Neither kind deletes the user row, so no cascade ever fires: a kept row
//! keeps its reactions alive until `clear_shared` sweeps them.
//!
//! Every write is idempotent — tombstoning a tombstone clears nothing,
//! deleting a missing row deletes nothing — so a retried pass converges on
//! the same state, which is why a database failure is a
//! [`JobError::Retry`] and only unusable arguments or a vanished user row
//! are a discard.
//!
//! The sweep's first act is to fire
//! [`AccountEvent::Deleted`](crate::protocols::events::AccountEvent::Deleted),
//! between loading the account and clearing its rows, because `ActivityPub`'s
//! fan-out of the actor's `Delete` to the remote followers and followees reads
//! the relationships the clear step deletes. A protocol failing to handle the
//! event, such as a failure to read those recipients or queue the `Delete`,
//! retries the job before anything is cleared.

use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

mod local;
mod remote;
#[cfg(test)]
mod test_support;

pub use local::handle_delete;
pub use remote::handle_delete_remote;

use crate::{
    jobs::{JobError, retry},
    models::{bookmark, chat, emoji_reaction, favourite, follow, notification, status, user},
    state::AppState,
};

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

/// The job name in the discard text of unusable [`DeleteArgs`].
const DELETE_JOB: &str = "account delete";

/// The job name in the retry text of a failed pass; every step is
/// idempotent, so another pass costs nothing but the wait.
const PASS_JOB: &str = "account delete pass";

/// How many rows each step of [`clear_shared`] took, for the debug line of
/// the job that ran it.
#[derive(Debug, Clone, Copy)]
struct SweepCounts {
    /// Statuses tombstoned.
    statuses: u64,
    /// Chat messages withdrawn.
    chat_messages: u64,
    /// Follow relationships removed, in either direction.
    follows: u64,
    /// Favourites removed.
    favourites: u64,
    /// Emoji reactions removed.
    reactions: u64,
    /// Bookmarks removed.
    bookmarks: u64,
    /// Notifications removed, in either role.
    notifications: u64,
}

/// The sweep both deletion kinds share: the account's statuses tombstone, its
/// chat messages are withdrawn, its follows go with the live peers' counters stepped, and its favourites,
/// emoji reactions, bookmarks and notifications go. Neither kind deletes the
/// user row, so no cascade does any of this for them.
///
/// # Errors
///
/// [`JobError::Retry`] on any database failure (see [`retry`]).
async fn clear_shared(state: &AppState, account: &user::User) -> Result<SweepCounts, JobError> {
    let db = &state.db;
    let id = account.id;
    let statuses = status::soft_delete_all_for_user(db, id)
        .await
        .map_err(|err| retry(PASS_JOB, &err))?;
    let chat_messages = chat::withdraw_all_from_sender(db, id)
        .await
        .map_err(|err| retry(PASS_JOB, &err))?;
    let follows = follow::delete_all_for_user_with_counts(db, id)
        .await
        .map_err(|err| retry(PASS_JOB, &err))?;
    let favourites = favourite::delete_all_for_user(db, id)
        .await
        .map_err(|err| retry(PASS_JOB, &err))?;
    let reactions = emoji_reaction::delete_all_for_user(db, id)
        .await
        .map_err(|err| retry(PASS_JOB, &err))?;
    let bookmarks = bookmark::delete_all_for_user(db, id)
        .await
        .map_err(|err| retry(PASS_JOB, &err))?;
    let notifications = notification::delete_all_for_user(db, id)
        .await
        .map_err(|err| retry(PASS_JOB, &err))?;
    Ok(SweepCounts {
        statuses,
        chat_messages,
        follows,
        favourites,
        reactions,
        bookmarks,
        notifications,
    })
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
