//! The `mutes` table: which account hides another from its timelines, with
//! which options, and until when.
//!
//! The reference behaviour is Pleroma's `User.mute` / `User.unmute`
//! (`lib/pleroma/user.ex`) over the mute edge of `Pleroma.UserRelationship`
//! (`lib/pleroma/user_relationship.ex`), reached from the Mastodon
//! `AccountController`'s `mute`/`unmute`/`mutes` actions
//! (`lib/pleroma/web/mastodon_api/controllers/account_controller.ex`). Where
//! Pleroma splits one mute across two edges (the timeline mute and the
//! notification mute), this table keeps the fact as one row per pair with
//! Mastodon's two parameters as its columns: `hide_notifications` is the
//! `notifications` parameter (`true` hides the muted account's notifications
//! as well as its posts, `false` mutes the timeline only) and `expires_at` is
//! its `duration`, counted from the write, `NULL` standing for an indefinite
//! mute.
//!
//! [`mute`] is an upsert on the ticket's `(muter_id, muted_id)` key: a pair
//! that is already muted keeps its row — and its id, so the listing's cursor
//! does not move, and its `inserted_at` — while the two options are rewritten
//! from the request, so re-muting is how a client changes or clears a
//! duration, or revives a mute that lapsed but has not been swept yet.
//! [`unmute`] deletes whatever pair it is given and is content with none.
//! Deleting either account takes its mutes through the foreign key's cascade.
//! Unlike a block, a mute severs nothing: the pair's follows stay exactly as
//! they were, the way Pleroma's `User.mute` writes its edges and nothing else.
//!
//! A mute at or past its `expires_at` is treated as absent by *every* read
//! here — each statement carries `expires_at IS NULL OR expires_at > now()`,
//! asked against the database clock so the instant a mute stops applying is
//! the same one every connection sees — and [`delete_expired`] removes those
//! rows for good; the `mutes.expire` periodic job runs it. Until it does, the
//! filters answer as though the mute had been lifted, which is what the
//! timeline and notification filters need.
//!
//! The reads are the ones the endpoints, the [`Relationship`] loader and the
//! filters need: [`is_muting`] the one directional fact, [`muted_ids`] and
//! [`notification_muted_ids`] the flat sets a timeline or an inbox filters by,
//! and [`muting_page`] the `GET /api/v1/mutes` listing, each entry led by the
//! [`Mute`] row next to the [`User`] behind it — the row id is what the cursor
//! and the `Link` header key on (decision 4), and the row's
//! [`expires_at`](Mute::expires_at) is what the listing renders as
//! `mute_expires_at`.
//!
//! [`Relationship`]: crate::entities::relationship::Relationship

use std::time::Duration;

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::models::user::User;

#[cfg(test)]
mod lifecycle_tests;
mod read;
#[cfg(test)]
mod test_support;
mod write;

pub use self::{
    read::{is_muting, muted_ids, muting_page, notification_muted_ids},
    write::{delete_expired, mute, unmute},
};

/// 422 message [`mute`] answers when an account asks to mute itself, the
/// wording this ticket pins for the endpoint (Pleroma's changeset refuses the
/// self relationship for every edge type the same way).
const SELF_MUTE: &str = "Can not mute yourself";

/// One row of `mutes`: the `muter_id` account hides the `muted_id` one, with
/// the two Mastodon options the write was given.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct Mute {
    /// `UUIDv7` primary key, minted by [`mute`] (decision 4); the id
    /// `GET /api/v1/mutes` pages by.
    pub id: Uuid,
    /// The muting account; every read and write of a mute is scoped to it.
    pub muter_id: Uuid,
    /// The muted account.
    pub muted_id: Uuid,
    /// Mastodon's `notifications` parameter as stored: `true` hides the muted
    /// account's notifications along with its posts, `false` mutes only the
    /// timeline. The [`muting_notifications`] flag of the [`Relationship`]
    /// entity reads it.
    ///
    /// [`muting_notifications`]: crate::entities::relationship::Relationship::muting_notifications
    /// [`Relationship`]: crate::entities::relationship::Relationship
    pub hide_notifications: bool,
    /// When the mute stops applying, `None` for an indefinite one. A row at or
    /// past this instant answers as absent to every read here until
    /// [`delete_expired`] removes it.
    pub expires_at: Option<DateTime<Utc>>,
    /// When the row was first inserted. A re-mute rewrites the options of the
    /// same row and leaves this — and the row's [`id`](Self::id) — alone.
    pub inserted_at: DateTime<Utc>,
}

/// The options a mute is written with: the two parameters
/// `POST /api/v1/accounts/{id}/mute` takes, already resolved to what they mean
/// for the row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MuteOptions {
    /// Mastodon's `notifications`: whether the mute also hides the muted
    /// account's notifications. The parameter's own default is `true`.
    pub notifications: bool,
    /// Mastodon's `duration`, in seconds counted from the write: `Some(secs)`
    /// with `secs` positive mutes for that long; `None` — and any non-positive
    /// value — mutes indefinitely.
    pub expires_in: Option<i64>,
}

impl Default for MuteOptions {
    /// Mastodon's defaults for both parameters: notifications hidden, no
    /// duration, i.e. an ordinary indefinite mute.
    fn default() -> Self {
        Self {
            notifications: true,
            expires_in: None,
        }
    }
}

/// One fetched row of [`muting_page`]: the [`Mute`] columns the page keys on
/// next to the [`User`] they point at, read off [`PAGE_SQL`]'s join and folded
/// into that pair by [`into_entry`].
#[derive(Debug, sqlx::FromRow)]
struct MuteUserRow {
    /// Primary key of the `mutes` row behind this entry, aliased because the
    /// joined account row carries its own `id`.
    mute_id: Uuid,
    /// The muter: `mutes.muter_id`, the account the page is scoped to.
    muter_id: Uuid,
    /// The muted account: `mutes.muted_id`, the user this entry is about.
    muted_id: Uuid,
    /// Stored `hide_notifications` column of the mute row, unaliased because
    /// [`User`] has no column of that name.
    hide_notifications: bool,
    /// Stored `expires_at` of the mute row, likewise unaliased; the value the
    /// listing renders as `mute_expires_at`.
    expires_at: Option<DateTime<Utc>>,
    /// The mute's `inserted_at`, aliased because the user row carries its own
    /// creation timestamp under that name.
    mute_inserted_at: DateTime<Utc>,
    /// The muted account, decoded from the join's unaliased `u.*`.
    #[sqlx(flatten)]
    user: User,
}

/// The interval [`UPSERT_SQL`] adds to the database's `now()`: the requested
/// duration as a [`Duration`], or `None` — which the statement turns into a
/// `NULL` `expires_at` — for an indefinite mute. No duration at all counts, and
/// so does one that is not positive: Mastodon's `duration=0` is "no expiry",
/// and a nonsense negative value is treated the same way rather than as a mute
/// that is already over.
fn expiry_of(options: MuteOptions) -> Option<Duration> {
    options
        .expires_in
        .filter(|secs| *secs > 0)
        .and_then(|secs| u64::try_from(secs).ok())
        .map(Duration::from_secs)
}

/// Splits [`MuteUserRow`] back into the pair [`muting_page`] answers: the
/// [`Mute`] of the aliased columns, next to the [`User`] the join flattened.
fn into_entry(row: MuteUserRow) -> (Mute, User) {
    let mute = Mute {
        id: row.mute_id,
        muter_id: row.muter_id,
        muted_id: row.muted_id,
        hide_notifications: row.hide_notifications,
        expires_at: row.expires_at,
        inserted_at: row.mute_inserted_at,
    };
    (mute, row.user)
}
