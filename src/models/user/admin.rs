//! The admin API's reads and writes over `users`.
//!
//! The reads are the filtered, counted page behind
//! `GET /api/v1/pleroma/admin/users` and the nickname-or-id lookup behind
//! `GET /api/v1/pleroma/admin/users/{nickname}` (WBE-195), and beside them
//! [`mastodon_list`]: the same table keyed the Mastodon admin API's way —
//! [`Criterion`]-weighed booleans, the four text criteria of
//! `GET /api/v{1,2}/admin/accounts` and the `id` keyset that listing pages by
//! instead of the admin API's offset window (WBE-210).
//!
//! [`staff_with_email`] is a third read: the active local admins and
//! moderators that hold an address, which is who the report letter goes to
//! (WBE-212). [`resolve_nicknames`] is the fourth: the batch nickname
//! lookup the list-taking admin account endpoints act on (WBE-198).
//!
//! The writers here belong to the maintenance the admin API does to one
//! account rather than to the profile the account edits itself, so they
//! sit beside the lookups that name the row: [`set_tags`] replaces the
//! profile-tag array (WBE-197), [`set_role`] moves one of the two staff
//! flags [`Role`] names, [`set_suggested`] the suggestion-pool flag the
//! `GET /api/v{1,2}/suggestions` listings read back, [`set_active`] the
//! activation flag the admin lifecycle pair flips (WBE-196),
//! [`set_approved`] the staff gate on a pending registration and
//! [`delete_user`] the pending account an admin rejects (WBE-213). Like
//! [`set_also_known_as`](super::set_also_known_as) and
//! [`set_settings_store`](super::set_settings_store) above them, each
//! writes exactly the whole value the endpoint computed — the merge,
//! dedup and skip rules of Pleroma's `User.tag`, `User.untag` and
//! `User.set_suggestion` live in `src/api/admin/users.rs`, where the
//! requests are.
//!
//! The reference is `Pleroma.Web.AdminAPI.Search.user`
//! (`lib/pleroma/web/admin_api/search.ex`) over `Pleroma.User.Query.build`
//! (`lib/pleroma/user/query.ex`): one `WHERE` clause assembled from the
//! request's criteria, counted without the page window and then windowed by
//! `LIMIT`/`OFFSET` (`User.Query.paginate`). Pleroma orders that listing by
//! `desc: id`; the ticket pins the nickname order instead, which the unique
//! citext column makes stable without a tie-break.
//!
//! Four of Pleroma's criteria are re-worded for this schema:
//!
//! - `internal: false` (its `nickname NOT LIKE 'internal.%'`) is this
//!   repo's [`NOT_INSTANCE_ACTOR`] — the one account a listing hides. The
//!   direct lookup does not apply it: naming an account is a lookup, not a
//!   listing (see the note on [`NOT_INSTANCE_ACTOR`]).
//! - `is_approved` and `is_confirmed` are not columns here;
//!   [`approval_pending`] and [`email_confirmed`] are their opposites, so
//!   `active`, `deactivated`, `need_approval` and `unconfirmed` read those
//!   two columns inverted.
//! - `query` is Pleroma's nickname `ILIKE` arm widened to the three text
//!   identities an admin searches by: nickname, display name and email.
//! - `invisible` (`info->>'invisible'`) has no column here, so it is not a
//!   criterion: nothing hides a row beyond the instance actor.
//!
//! The citext columns are cast to `text` wherever they meet `ILIKE` (the
//! same cast [`user_search`](crate::models::user_search) applies) and to
//! `citext` wherever they meet `=`, because a bare text parameter would
//! fold to the case-sensitive `text = text` operator — the reason
//! [`get_by_email`] casts too.
//!
//! [`approval_pending`]: super::User::approval_pending
//! [`email_confirmed`]: super::User::email_confirmed
//! [`NOT_INSTANCE_ACTOR`]: super::NOT_INSTANCE_ACTOR
//! [`get_by_email`]: super::get_by_email

/// The three-state boolean the Mastodon listings weigh and its `WHERE`-arm
/// writer.
mod criterion;
/// The data move of the `disabled_at` migration.
#[cfg(test)]
mod disabled_migration_tests;
/// The direct nickname-or-id reads of one admin account.
mod lookups;
/// The Mastodon admin account listing.
mod mastodon;
/// The `WHERE` arms and the page window of the Pleroma admin listing.
mod pleroma_filters;
/// The Pleroma admin user listing and its counted page.
mod pleroma_list;
/// The admin writes that answer with the row they moved.
mod returns;
/// Fixtures more than one child's tests build on.
#[cfg(test)]
mod test_support;
/// The staff read and the one-account writes of staff state and tags.
mod writes;

pub use criterion::Criterion;
pub use lookups::{get_by_nickname_or_id, resolve_nicknames};
pub use mastodon::{MastodonListParams, mastodon_list};
pub use pleroma_list::{ListFilters, ListPage, ListParams, list};
pub use returns::{Standing, delete_user, set_active, set_approved, set_standing, set_suggested};
pub use writes::{Role, set_role, set_tags, staff_with_email};
