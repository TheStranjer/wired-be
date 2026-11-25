//! The `conversations` tables: one direct-message thread, each account's
//! side of it, and the accounts that side addresses.
//!
//! Reference: Pleroma's `Pleroma.Conversation`
//! (`lib/pleroma/conversation.ex`), `Pleroma.Conversation.Participation`
//! (`lib/pleroma/conversation/participation.ex`) and
//! `Pleroma.Conversation.Participation.RecipientShip`
//! (`lib/pleroma/conversation/participation/recipient_ship.ex`). Pleroma
//! keys a conversation by the AP `context` of the activities in it and
//! hands every recipient — local or remote — a participation row; here a
//! participation belongs to a **local** account alone and the remote
//! addressees ride the participation's recipient list instead, so the
//! thread's audience is stored without a remote account ever listing a
//! conversation it cannot read.
//!
//! [`participate`] is the write every consumer of this ticket hangs off:
//! the create service's `after_status_created` hook runs it (through
//! [`participate_for_status`]) for a `direct` status — statuses of any
//! other visibility do nothing. It finds-or-creates the thread's
//! `conversations` row — the `in_reply_to_conversation_id` create already
//! names it on the row, a reply inherits the parent's
//! [`direct_conversation_id`](crate::models::status::Status::direct_conversation_id), a new
//! root keys a fresh one on its thread id — writes it back onto the status, and
//! for every local participant (author and local accounts mentioned by the
//! status) upserts the participation: its `last_status_id` and
//! `updated_at` move to this status, `read` becomes whether the
//! participant authored it (the recipients land unread), and its recipient
//! list is refreshed to every other participant, remote accounts included.
//!
//! The reads serve the surfaces the conversation endpoints build on:
//! [`get_participation`] one owned row, [`page_for_user`] the account's
//! list newest-activity-first (Pleroma's `recipients` restriction and all),
//! [`mark_read`] and [`mark_all_read`] the read flags,
//! [`delete_participation`] a withdrawal, [`unread_count`] the badge,
//! [`recipients`] the audience an `in_reply_to_conversation_id` post
//! addresses, [`recipients_map`] the same lists for a whole page in one
//! read, [`set_recipients`] the wholesale rewrite Pleroma's `PATCH`
//! performs, [`page_direct_statuses`] one thread's status listing, and
//! [`participation_ids`] the one batched read
//! [`crate::services::render_status`] runs per page to fill
//! `pleroma.direct_conversation_id`, and [`participations_for_status`] the
//! local roster one freshly threaded status is announced to, off the create
//! hook's [`crate::services::streaming::conversation_updated`] push.
//!
//! [`repoint_after_delete`] is the delete side of `last_status_id`: a soft
//! delete never fires the foreign key, so when the status one
//! participation last saw is tombstoned, the participation is repointed to
//! the conversation's newest remaining direct status — or deleted, with
//! its recipient rows cascading, when none remain.

use chrono::{DateTime, Utc};
use uuid::Uuid;

mod pages;
mod participation;
mod recipients;
mod repoint;
mod threads;
mod write;

#[cfg(test)]
mod test_support;

pub use pages::{page_direct_statuses, page_for_user};
pub use participation::{
    delete_participation, get_participation, mark_all_read, mark_read, participation_ids,
    participations_for_status, unread_count,
};
pub use recipients::{recipients, recipients_map, set_recipients, thread_members};
pub use repoint::repoint_after_delete;
pub use write::{participate, participate_for_status};

/// One row of `conversation_participations`: one local account's side of
/// one direct-message thread.
///
/// Its `id` is what `pleroma.direct_conversation_id` renders and what
/// `POST /api/v1/statuses` takes back as `in_reply_to_conversation_id`.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct Participation {
    /// `UUIDv7` primary key, minted by [`participate`] (decision 4).
    pub id: Uuid,
    /// Whose side of the thread this is.
    pub user_id: Uuid,
    /// The thread's row in `conversations`.
    pub conversation_id: Uuid,
    /// Whether the account has seen [`last_status_id`](Self::last_status_id);
    /// the author's own post lands read, every other participant's lands
    /// unread until [`mark_read`] or [`mark_all_read`] says otherwise.
    pub read: bool,
    /// The conversation's newest direct status as of this row's last bump;
    /// moved on by [`participate`], re-pointed (or the row deleted) by
    /// [`repoint_after_delete`].
    pub last_status_id: Option<Uuid>,
    /// When the account joined the thread.
    pub inserted_at: DateTime<Utc>,
    /// When this row was last bumped by a new direct status.
    pub updated_at: DateTime<Utc>,
}
