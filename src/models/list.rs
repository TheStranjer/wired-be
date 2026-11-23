//! The `lists` table (and its `list_members` join table): per-user named
//! collections of accounts with Mastodon v1 semantics.
//!
//! Reference: Pleroma's `Pleroma.List` (`lib/pleroma/list.ex`). Pleroma
//! stores membership as a `following` array of follower addresses on the
//! list row and derives an `ap_id` per list; neither is needed here: the
//! row's `UUIDv7` id (decision 4) is what the API names, and membership is
//! the `list_members` table, keyed by the member's `users` row (decision 7)
//! — which is also what makes [`lists_containing`] a plain join. The
//! `replies_policy` column has no Pleroma 2.10 counterpart; it is
//! Mastodon's, and the API validates its value while the migration's check
//! constraint keeps the column honest.
//!
//! Lists are strictly per-user: [`get_owned`], [`update`] and [`delete`]
//! all carry the owner in their `WHERE` clause, so a foreign id is
//! indistinguishable from a missing one (both answer `None`/`false`), which
//! is exactly the 404 the API owes. Deleting a user takes their lists —
//! and, through the members' cascade, their memberships everywhere — with
//! it; deleting a list takes its own memberships with it.
//!
//! [`update`] is the partial write the API's `PUT` performs: a `NULL`
//! (absent) parameter keeps the stored column, so only what the request
//! names changes.
//!
//! A list's members are written by [`add_members`] and [`remove_members`] and
//! read by [`members_page`] and [`member_ids`]. A list's own order is the order
//! its members were added in, which no `UUIDv7` column of this table can state
//! (the member's id orders the accounts, not the memberships), so
//! `list_members.position` — a `bigserial` ordinal the database fills on
//! insert — is what [`members_page`] keys its `max_id`/`min_id`/`since_id`
//! cursors on, the same column Mastodon's list-accounts page keys on.

mod containing;
mod crud;
mod members;
mod membership_pages;

#[cfg(test)]
mod test_support;

pub use containing::{lists_containing, lists_following_member};
pub use crud::{create, delete, delete_all_for_user, for_user, get_owned, update};
pub use members::{add_members, remove_between, remove_members};
pub use membership_pages::{member_ids, members_page};

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::models::user::User;

/// One row of `lists`: a named collection of accounts one user keeps.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct List {
    /// `UUIDv7` primary key, minted by [`create`] (decision 4); also the
    /// creation order lists are read in.
    pub id: Uuid,
    /// The owning account; every read and write of a list is scoped to it.
    pub user_id: Uuid,
    /// The user-defined title, verbatim as its owner wrote it.
    pub title: String,
    /// Whose replies the list shows: one of `none`, `list` or `followed`
    /// (the API validates; the column stores what passed).
    pub replies_policy: String,
    /// Whether members are removed from the owner's Home feed
    /// (Mastodon's and Pleroma's `exclusive`).
    pub exclusive: bool,
    /// When the row was created.
    pub inserted_at: DateTime<Utc>,
    /// When the row last changed; refreshed by every [`update`].
    pub updated_at: DateTime<Utc>,
}

/// What to create: every `lists` column except the `id` [`create`] mints,
/// the `user_id` the handler takes from its caller and the timestamps the
/// database stamps.
///
/// [`Default::default`] mirrors the column defaults, so a caller sets only
/// what it decides on; the usual minimum is the title.
#[derive(Clone, Debug)]
pub struct NewList {
    /// The title; empty in the default.
    pub title: String,
    /// Whose replies the list shows; `"list"` by default, like the column.
    pub replies_policy: String,
    /// Whether members leave the Home feed; `false` by default, like the
    /// column.
    pub exclusive: bool,
}

impl Default for NewList {
    /// The column defaults: empty title, the `list` replies policy and
    /// exclusiveness off.
    fn default() -> Self {
        Self {
            title: String::new(),
            replies_policy: "list".to_owned(),
            exclusive: false,
        }
    }
}

/// What to change: the present-only parameters of the API's `PUT`. A `None`
/// field keeps the stored column; [`update`] applies exactly the named
/// ones.
#[derive(Clone, Debug, Default)]
pub struct ListChanges {
    /// The new title; `None` keeps the stored one.
    pub title: Option<String>,
    /// The new replies policy; `None` keeps the stored one.
    pub replies_policy: Option<String>,
    /// The new exclusiveness; `None` keeps the stored one.
    pub exclusive: Option<bool>,
}

/// One entry of a paged member list: the member's [`User`] row plus the
/// `position` of the `list_members` row that put them on the page.
///
/// The membership ordinal — not the member's id — is what [`members_page`]
/// keys on (WBE-104: the order members were added in is the list's own order),
/// so the endpoint's `max_id`/`min_id`/`since_id` cursors and its `Link`
/// header carry [`MemberEntry::position`] values, as
/// [`crate::models::follow::FollowPageEntry`] carries follow row ids.
#[derive(Debug, sqlx::FromRow)]
pub struct MemberEntry {
    /// `list_members.position`: the sequence ordinal the membership was
    /// inserted at, and so the member's place in the list's own order.
    pub position: i64,
    /// The member's account row.
    #[sqlx(flatten)]
    pub user: User,
}
