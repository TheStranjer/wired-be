//! The `blocks` table: which account refuses which, and what writing that
//! fact does to the follows between the pair.
//!
//! Per `docs/ARCHITECTURE.md` decision 6 a block is one of the relational
//! facts that federate, so a row carries the `ActivityPub` id of the `Block`
//! activity behind it: `{BASE_URL}/activities/{uuid}` for a blocker this
//! instance owns and `None` for a remote one until the federation ticket that
//! mirrors an incoming activity records the id it sent. The reference
//! behaviour is Pleroma's `User.block` / `User.unblock`
//! (`lib/pleroma/user.ex`) over the blocking edge of
//! `Pleroma.UserRelationship` (`lib/pleroma/user_relationship.ex`), reached
//! from the Mastodon `AccountController`'s `block`/`unblock` actions
//! (`lib/pleroma/web/mastodon_api/controllers/account_controller.ex`).
//!
//! Blocking severs the pair (`User.block`'s "sever any follow relationships to
//! prevent leaks per activitypub"): the two accounts' accepted follows of each
//! other and both directions' pending follow requests are removed, with the
//! denormalised counters stepped down for every accepted relationship that
//! goes. [`block`] does that in one transaction, so the block row, the follow
//! rows it deletes and the counters those rows fed are written or rolled back
//! together; Pleroma performs the same set as a sequence of separate writes.
//! The follow-side half of that write belongs to the table it owns and lives
//! in [`crate::models::follow::sever_between`].
//!
//! Both writes are idempotent on the ticket's `(blocker_id, blocked_id)` key:
//! a pair that is already blocked keeps its row — and its id, so the listing's
//! cursor does not move, and its `ap_id`, so the activity already federated is
//! not replaced — and a repeat call writes nothing, while [`unblock`] deletes
//! whatever pair it is given and is content with none. Deleting either account
//! takes its blocks through the foreign key's cascade.
//!
//! The federation mirror of an inbound `Block` activity adds the two writes
//! that fact needs: [`set_ap_id`] records the remote activity id on the row
//! [`block`] left without one (a remote blocker's id is the one its own
//! instance minted), and [`get_by_ap_id`] reads the block back by that id —
//! what an `Undo` that names only the activity resolves through.
//!
//! The three reads are the ones the endpoints and the guards need:
//! [`is_blocking`] the one directional fact, [`blocked_between`] the same
//! question in either direction (what [`follow`](crate::models::follow) refuses on), and
//! [`blocked_ids`] / [`blocker_ids`] the flat sets a timeline or an inbox
//! filters by. [`blocking_page`] gives `GET /api/v1/blocks` its page, each
//! entry led by the [`Block`] row next to the [`User`] behind it — the row id
//! is what the cursor and the `Link` header key on (decision 4).
//!
//! Signatures name the two ends of a block `blocker` and `target` (the word
//! [`follow`](crate::models::follow) borrows for the account it is about), because `blocker` next to
//! `blocked` is one Clippy `similar_names` error; the row keeps the table's
//! own `blocker_id` and `blocked_id` names.

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::{models::user::User, util::id::new_id};

mod activity;
mod read;
#[cfg(test)]
mod test_support;
mod write;

pub use activity::{get_by_ap_id, get_by_blocker_and_ap_id, set_ap_id};
pub use read::{blocked_between, blocked_ids, blocker_ids, blocking_page, is_blocking};
pub use write::{block, unblock};

/// 422 message [`block`] answers when an account asks to block itself, the
/// wording this ticket pins for the endpoint (Pleroma's changeset refuses the
/// self relationship for every edge type the same way).
const SELF_BLOCK: &str = "Can not block yourself";

/// One row of `blocks`: the `blocker_id` account blocks the `blocked_id`
/// one, plus the `ActivityPub` id of the `Block` activity behind it.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct Block {
    /// `UUIDv7` primary key, minted by [`block`] (decision 4); the id
    /// `GET /api/v1/blocks` pages by.
    pub id: Uuid,
    /// The blocking account; every read and write of a block is scoped to it.
    pub blocker_id: Uuid,
    /// The blocked account.
    pub blocked_id: Uuid,
    /// `ActivityPub` id of the `Block` activity behind this row (decision 6):
    /// a `{BASE_URL}/activities/{uuid}` id for a local blocker, the remote id
    /// for a mirrored one, `None` until an activity exists to record.
    pub ap_id: Option<String>,
    /// When the row was inserted; a block is a plain fact, so it carries no
    /// `updated_at` and [`block`] keeps the stored row of an already blocked
    /// pair exactly as it is.
    pub inserted_at: DateTime<Utc>,
}

/// One fetched row of [`blocking_page`]: the [`Block`] columns the page keys on
/// next to the [`User`] they point at, read off [`PAGE_SQL`]'s join and folded
/// into that pair by [`into_entry`].
#[derive(Debug, sqlx::FromRow)]
struct BlockUserRow {
    /// Primary key of the `blocks` row behind this entry, aliased because the
    /// joined account row carries its own `id`.
    block_id: Uuid,
    /// The blocker: `blocks.blocker_id`, the account the page is scoped to.
    blocker_id: Uuid,
    /// The blocked account: `blocks.blocked_id`, the user this entry is about.
    blocked_id: Uuid,
    /// The block's `ActivityPub` id, aliased because [`User`] has its own
    /// `ap_id` (and needs the unaliased column to decode itself).
    block_ap_id: Option<String>,
    /// The block's `inserted_at`, aliased because the user row carries its own
    /// creation timestamp under that name.
    block_inserted_at: DateTime<Utc>,
    /// The blocked account, decoded from the join's unaliased `u.*`.
    #[sqlx(flatten)]
    user: User,
}

/// The `ap_id` to store for a new block: a freshly minted
/// `{base_url}/activities/{uuid}` (decision 6) for a blocker this instance
/// owns, `None` for a remote blocker, whose own activity id the federation
/// layer records from the `Block` activity it processed.
fn local_activity_id(base_url: &str, blocker: &User) -> Option<String> {
    blocker
        .local
        .get()
        .then(|| format!("{base_url}/activities/{}", new_id()))
}
