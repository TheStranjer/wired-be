//! The `statuses` table: posts, replies, reblogs and quotes.
//!
//! Per `docs/ARCHITECTURE.md` decision 6 statuses are relational
//! Mastodon-style rows rather than Pleroma's JSONB activities: Pleroma keeps
//! the same facts in `Pleroma.Activity` wrapping an `Object`
//! (`lib/pleroma/activity.ex`, `lib/pleroma/object.ex`), where a `Create`
//! becomes one row here, its actor becomes [`user_id`](Status::user_id), its
//! `published` timestamp becomes [`inserted_at`](Status::inserted_at), and an
//! `Announce` becomes a row with [`reblog_of_id`](Status::reblog_of_id) and no
//! text of its own.
//!
//! A local status owns the two URLs clients and peers know it by, derived
//! from its id: `{BASE_URL}/objects/{uuid}` in [`uri`](Status::uri) and
//! `{BASE_URL}/notice/{uuid}` in [`url`](Status::url). [`insert`] mints the id
//! (decision 4) and so is the only place those can be computed; it also
//! resolves [`conversation_id`](Status::conversation_id) — a reply inherits
//! its parent's conversation, a root status is its own conversation — and
//! moves the denormalised counters in the same transaction, so a status and
//! the counts that describe it are always written together. A `direct`
//! status also belongs to a DM thread: the
//! [`direct_conversation_id`](Status::direct_conversation_id) column names
//! its `conversations` row, landed by
//! [`crate::models::conversation::participate`] once the row is stored. It
//! also links the accounts in [`NewStatus::mentioned_user_ids`] in that transaction
//! (see [`crate::models::mention`]), so a status and the mentions it
//! carries appear together or not at all, and the same for the tags in
//! [`NewStatus::hashtags`] (see [`crate::models::hashtag`]).
//!
//! Deletion is soft: [`soft_delete`] keeps the row as a tombstone with its
//! text cleared and its `uri` reserved (so a deleted remote post is never
//! re-imported) and steps the counters back down. Deleting through the API
//! takes the boosts with the post: [`soft_delete_with_reblogs`] tombstones the
//! status and every live reblog of it in one transaction. Everything that
//! reads by id hides tombstones; [`get_by_uri`] and
//! [`get_by_id_including_deleted`] do not, because federation and moderation
//! need to see them. A status created with `expires_in` carries that moment
//! in [`expires_at`](Status::expires_at), and the expiration is just the
//! delete arriving on a schedule: [`fetch_expired`] hands the
//! [`statuses.expire_sweep`](crate::jobs::statuses::KIND_SWEEP) safety job
//! the rows whose time has run out.
//!
//! Reblogs are the same table wearing the `Announce` role (decision 6):
//! [`create_reblog`] writes a live reblog row with no text of its own, the
//! original's conversation, and the activity uri `{BASE_URL}/activities/{id}`
//! rather than the plain status's `/objects/{id}`; the partial unique index
//! on `(user_id, reblog_of_id)` keeps one live reblog per pair, so a repeat
//! restates the row instead of writing a second one. [`remove_reblog`]
//! withdraws it — the same tombstone [`soft_delete`] writes, with the same
//! counter steps — and [`reblogged_status_ids`] and [`reblogger_page`] serve
//! the two reads: the batched `reblogged` flag of the status renderer and
//! the `reblogged_by` account listing.
//!
//! Counts move by whole steps only, through [`insert`], [`soft_delete`] and
//! [`bump_counter`], and are clamped at zero in SQL, so they can neither go
//! negative nor be lost to a concurrent write. The linked counters —
//! [`replies_count`](Status::replies_count), [`reblogs_count`](Status::reblogs_count)
//! and [`quotes_count`](Status::quotes_count) — move with the row that links:
//! writing a status that answers, reblogs or quotes another steps its
//! target's counter up in the same transaction, and tombstoning that row
//! steps it back down, so [`bump_counter`] is left to the counters no status
//! row links to (favourites).

mod counters;
mod delete;
mod new_status;
mod pages;
mod quote_links;
mod quote_policy;
mod quoting;
mod reads;
mod reblogs;
mod threads;
mod types;

#[cfg(test)]
mod test_support;

pub mod admin;

pub use counters::bump_counter;
pub use delete::{
    fetch_expired, insert_tombstone, soft_delete, soft_delete_all_for_user,
    soft_delete_with_reblogs,
};
pub use new_status::{NewStatus, insert, insert_conn};
pub use pages::{
    backup_page, outbox_page, public_replies_count, public_replies_page, status_visibility_counts,
};
pub use quote_links::{
    link_backfilled_quote, link_quotes_of, link_waiting_quotes, pending_quote_uri,
};
pub use quote_policy::{QuotePolicy, set_quote_policy};
pub use quoting::quoting_page;
pub use reads::{
    conversation_id_for_context, get_by_id, get_by_id_including_deleted, get_by_uri, get_many,
};
pub use reblogs::{
    create_reblog, local_reblogger_ids, reblogged_status_ids, reblogger_page, remove_reblog,
};
pub use threads::{ancestor_chain, descendant_tree, link_backfilled_reply};
pub use types::{
    Counter, DEFAULT_CONTENT_TYPE, ReblogPageEntry, Status, StatusVisibilityCounts, Visibility,
};
