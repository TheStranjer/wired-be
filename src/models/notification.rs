//! The `notifications` table: one account being told something happened.
//!
//! Reference: Pleroma's `Pleroma.Notification` (`lib/pleroma/notification.ex`)
//! and the grouped reads of `Pleroma.Web.MastodonAPI.MastodonAPI`
//! (`lib/pleroma/web/mastodon_api/mastodon_api.ex`, the
//! `get_grouped_notification_page` / `notification_group_*` family the v2
//! API calls). What that side calls the *group key* is [`grouped_key`]
//! here, computed once at insert and stored in the `group_key` column:
//! `<type>-<status_id>` for the groupable types — `<type>-none` when the
//! notification names no status — with the emoji appended for a
//! `pleroma:emoji_reaction` so two reactions to one status are two groups.
//! Every other type is ungrouped and reads as the unique key
//! `ungrouped-<notification id>`, spelled by [`UNGROUNDED_PREFIX`] and
//! derived from the row rather than stored.
//!
//! The grouped reads are the ones [`crate::api::notifications_v2`] answers
//! with: [`group_page`] the grouped listing (one page of groups, newest
//! group first, each entry carrying up to [`SAMPLE_SIZE`] of its newest
//! members' actors), [`group_info`] one group's metadata,
//! [`group_actor_ids`] every actor of one group, [`dismiss_group`] the
//! group dismissal, and [`unread_count`] the capped count of unseen
//! undismissed notifications past the caller's marker. The flat reads and
//! writes are the ones [`crate::api::notifications`] answers with:
//! [`create`] the one raw insert, [`maybe_notify`] the insert that honours
//! the recipient's `block_from_strangers` setting,
//! [`get`] one row by id, [`get_visible`] the same row as the listings
//! show it, [`page_for_user`] the flat listing under the client filters
//! and the mute exclusions, [`dismiss`] / [`dismiss_all`] / [`dismiss_ids`]
//! the three dismissal shapes, [`mark_read_through_id`] the read marking
//! Pleroma's `set_read_up_to` performs, [`delete_for_status`] the
//! cleanup that takes a deleted status's notifications with it (and
//! [`delete_for_chat_message`] its twin for a withdrawn chat message),
//! [`already_notified`] the deduplication read [`crate::services::notify`]
//! asks before telling a recipient the same fact twice, and
//! [`delete_matching`] the withdrawal write that takes one telling back —
//! a favourited post un-favourited, a boost withdrawn, a follow request
//! granted. [`already_told`] and [`delete_telling`] are the same pair over
//! the full five-part key — status *and* reaction emoji named — for the
//! tellings that deduplicate per ballot and per emoji.
//!
//! Dismissal is soft: the writes stamp [`dismissed_at`](Notification::dismissed_at)
//! and every read hides the stamped rows rather than deleting them. Every
//! read and write is scoped to the recipient (`user_id`), skips dismissed
//! rows, and — as Pleroma's `for_user_query` does — skips rows whose
//! actor's account is deactivated, so an invisible account silently leaves
//! its group.

use chrono::{DateTime, Utc};
use uuid::Uuid;

mod create;
mod delete;
mod dismiss;
mod flat_page;
mod group_page;
mod grouping;
mod telling;
mod unread;

#[cfg(test)]
mod test_support;

pub use create::{already_notified, create, maybe_notify};
pub use delete::{
    delete_all_for_user, delete_for_chat_message, delete_for_status, delete_matching,
};
pub use dismiss::{dismiss, dismiss_all, dismiss_group, dismiss_ids, get, mark_read_through_id};
pub use flat_page::{FlatPageParams, NotificationFilters, get_visible, page_for_user};
pub use group_page::{
    GroupEntry, GroupInfo, GroupPageParams, group_actor_ids, group_info, group_page,
};
pub use grouping::{
    GroupSelector, SAMPLE_SIZE, UNGROUNDED_PREFIX, grouped_key, is_groupable, select_group,
};
pub use telling::{Telling, already_told, delete_telling};
pub use unread::{UnreadParams, unread_count};

/// The favourite type: groupable, keyed by the favourited status.
pub const TYPE_FAVOURITE: &str = "favourite";
/// The reblog type: groupable, keyed by the reblogged status.
pub const TYPE_REBLOG: &str = "reblog";
/// The Pleroma emoji-reaction type: groupable, keyed by the status *and*
/// the emoji, so the key carries the distinction Pleroma's view renders as
/// the group's `emoji` key.
pub const TYPE_EMOJI_REACTION: &str = "pleroma:emoji_reaction";
/// The follow-request type: the one type [`maybe_notify`] never withholds
/// from an account that blocks strangers.
///
/// Pleroma applies `block_from_strangers` to every other type — a
/// stranger's *follow* needs the recipient to follow them back first,
/// which is precisely what an incoming request is not.
pub const TYPE_FOLLOW_REQUEST: &str = "follow_request";
/// The follow type: an accepted follow of the recipient. Ungrouped.
pub const TYPE_FOLLOW: &str = "follow";
/// The mention type: a status that names its recipient. Ungrouped.
pub const TYPE_MENTION: &str = "mention";
/// The poll type: a ballot cast on a poll the recipient authored, and the
/// closing of a poll they authored or voted on. Ungrouped.
pub const TYPE_POLL: &str = "poll";
/// The status type: a new post by an account the recipient subscribed to
/// (Mastodon's subscription alert). Ungrouped.
pub const TYPE_STATUS: &str = "status";
/// The update type: a post the recipient boosted was edited. Ungrouped.
pub const TYPE_UPDATE: &str = "update";
/// The Pleroma chat-mention type: a chat message delivered to its recipient.
///
/// Ungrouped, and it names no status — its subject is the
/// [`chat_message_id`](Notification::chat_message_id) row, which the entity
/// renders under `chat_message` (Pleroma's `put_chat_message`).
pub const TYPE_CHAT_MENTION: &str = "pleroma:chat_mention";
/// The move type: an account the recipient follows moved to a new actor
/// (Mastodon's `move`). Ungrouped.
pub const TYPE_MOVE: &str = "move";

/// The types the grouped API merges across notifications.
///
/// Every other type keeps the unique [`UNGROUNDED_PREFIX`] key. Pleroma's
/// `@groupable_notification_types` with the follow dropped (this project's
/// grouping keeps only the status-keyed types: a status-less follow would
/// merge every follow on earth into `follow-none`) and the emoji reaction
/// added.
pub const GROUPABLE_TYPES: [&str; 3] = [TYPE_FAVOURITE, TYPE_REBLOG, TYPE_EMOJI_REACTION];

/// One row of `notifications`: one account's notification of one event.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct Notification {
    /// `UUIDv7` primary key, minted by [`create`] (decision 4); its text
    /// order is the notification's place in the account's timeline.
    pub id: Uuid,
    /// The recipient account.
    pub user_id: Uuid,
    /// The account that caused the notification (Mastodon's
    /// `from_account_id`).
    pub from_account_id: Uuid,
    /// Mastodon's type name as stored. Called `event_type` to keep it from
    /// reading as `Notification::notification_type`; the column is `type`.
    #[sqlx(rename = "type")]
    pub event_type: String,
    /// The status this notification is about; `None` for the status-less
    /// types.
    pub status_id: Option<Uuid>,
    /// The emoji of a reaction; `None` for every other type.
    pub emoji: Option<String>,
    /// The report of a `pleroma:report` notification; `None` otherwise.
    pub report_id: Option<Uuid>,
    /// The chat message of a `pleroma:chat_mention` notification; `None`
    /// for every other type. A chat mention names no status: the message
    /// this key holds is the notification's whole subject.
    pub chat_message_id: Option<Uuid>,
    /// The stored group key ([`grouped_key`]); `None` exactly when the type
    /// is not groupable.
    pub group_key: Option<String>,
    /// Whether the recipient has read past this notification.
    pub seen: bool,
    /// When the recipient dismissed it (and with it its group); `None`
    /// while it is live. Every read here hides the stamped rows.
    pub dismissed_at: Option<DateTime<Utc>>,
    /// When the notification was created, to the database clock.
    pub inserted_at: DateTime<Utc>,
}

/// The columns [`create`] writes; the rest are this module's own doing
/// (the id and the computed group key) or column defaults (`seen`,
/// `dismissed_at`, `inserted_at`).
#[derive(Clone, Debug)]
pub struct NewNotification<'a> {
    /// The recipient account.
    pub user_id: Uuid,
    /// The account that caused the notification.
    pub from_account_id: Uuid,
    /// Mastodon's type name.
    pub notification_type: &'a str,
    /// The status the notification is about, when it names one.
    pub status_id: Option<Uuid>,
    /// The reaction emoji, for a `pleroma:emoji_reaction`.
    pub emoji: Option<&'a str>,
    /// The report, for a `pleroma:report`.
    pub report_id: Option<Uuid>,
    /// The chat message, for a [`TYPE_CHAT_MENTION`].
    pub chat_message_id: Option<Uuid>,
}
