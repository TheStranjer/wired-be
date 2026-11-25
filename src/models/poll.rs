//! The `polls`, `poll_options` and `poll_votes` tables: the question a status
//! asks and the votes cast on it.
//!
//! Reference: Pleroma keeps a poll inside the status's object — a `Question`
//! whose `oneOf`/`anyOf` list its options, built by `make_poll_data` of
//! `lib/pleroma/web/common_api/utils.ex` and read back by
//! `lib/pleroma/web/mastodon_api/views/poll_view.ex`, with each vote an
//! `Answer` activity in the object's collection. This module lays the same
//! thing out relationally (`docs/ARCHITECTURE.md` decisions 3 and 6): one poll
//! per status, one row per option, one row per mark, so ticking a box counts
//! a row instead of rewriting the post.
//!
//! Every id is a `UUIDv7` made in Rust (decision 4), which is why
//! [`create`] takes `&mut PgConnection` rather than a pool: a poll is born
//! inside the transaction that inserts its status, and a borrow of a pooled
//! connection or of an open transaction both reach it the same way.
//!
//! The numbers the Mastodon `Poll` entity reports — the poll's
//! [`voters_count`](Poll::voters_count) and [`votes_count`](Poll::votes_count)
//! and each option's [`votes_count`](PollOption::votes_count) — are columns,
//! not computed reads: the vote endpoint's transaction moves them. This layer
//! stores what it is given, including
//! [`expires_at`](PollParams::expires_at): the API's `poll[expires_in]`
//! seconds-to-instant conversion belongs to the status-creation ticket, not
//! here.
//!
//! Alongside the writes, the reads the entity needs: [`get_for_status`] for
//! one status, [`for_statuses`] for a batch of them (the timeline's single
//! query, mirroring [`crate::models::media_attachment::for_statuses`]),
//! [`options`] for a poll's choices in order, and
//! [`voted_option_positions`] — added by this ticket too — for the positions
//! the viewer marked, which is the entity's `voted` flag and `own_votes`
//! array. The render batch's page-shaped mates, [`options_for_statuses`] and
//! [`votes_by_status`], answer the whole page with the same reads in one
//! statement each, keyed by status id like [`for_statuses`].
//!
//! The vote endpoint's ticket (WBE-106) lands the reads and the write the
//! `POST /api/v1/polls/{id}/votes` transaction is made of: [`get_by_id`]
//! resolves the poll the route is keyed on, [`has_voted`] answers the
//! endpoint's already-voted refusal, and [`cast_vote`] is the one
//! transaction that inserts the voter's marks and moves every counter the
//! entity reports — locking the poll row first, so two concurrent ballots
//! from one account serialize and the second is refused rather than
//! double-counted. The inbound poll-vote ticket (WBE-174) reuses that same
//! write for a federated `Create` of one answer, which is why
//! [`cast_vote`]'s refusal is stated per mark: a poll that is
//! [`multiple`](Poll::multiple) takes a voter's later options as they arrive
//! (one `Create` each) while [`voters_count`](Poll::voters_count) moves for
//! the first one only, and a poll that is not refuses the second ballot
//! whole.
//!
//! The poll-end notification ticket (WBE-113) adds the sweep the
//! [`polls.end_notify`](crate::jobs::polls::KIND_END_NOTIFY) job runs:
//! [`fetch_ended_unnotified`] finds the polls that closed inside its window
//! and were never told about, [`voter_ids`] answers whose ballots the
//! closing should tell, and [`mark_ended_notified`] retires the row so the
//! next pass walks past it (Pleroma needs no such flag — its
//! `PollWorker` Oban job is scheduled once per poll and simply runs,
//! `lib/pleroma/workers/poll_worker.ex`).

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::models::user;

mod cast_vote;
mod create;
mod ending;
mod reads;
mod replace;
mod votes;

#[cfg(test)]
mod test_support;

pub use cast_vote::cast_vote;
pub use create::{RemoteTotals, create, record_remote_totals};
pub use ending::{fetch_ended_unnotified, mark_ended_notified, voter_ids};
pub use reads::{for_statuses, get_by_id, get_for_status, options, options_for_statuses};
pub use replace::{delete_for_status, replace_remote_poll};
pub use votes::{has_voted, voted_option_positions, votes_by_status};

/// The `polls` columns [`Poll`] decodes, in the one spelling every statement
/// that returns a poll row interpolates: a new column is added here (and to
/// the struct) and nowhere else.
pub(super) const POLL_COLUMNS: &str =
    "id, status_id, expires_at, multiple, hide_totals, voters_count, votes_count, ended_notified";

/// One row of `polls`: the poll a status carries, without its options or its
/// voters.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct Poll {
    /// `UUIDv7` primary key, minted by [`create`] (decision 4).
    pub id: Uuid,
    /// The status asking the question; unique, so a status has one poll at
    /// most and deleting it takes the poll with it.
    pub status_id: Uuid,
    /// When voting closes; `None` for a poll that never expires.
    pub expires_at: Option<DateTime<Utc>>,
    /// Whether a voter may mark more than one option.
    pub multiple: bool,
    /// Whether per-option counts stay hidden while the poll runs.
    pub hide_totals: bool,
    /// Distinct accounts that have voted; moved by the vote transaction.
    pub voters_count: i32,
    /// Votes cast in total, which passes [`voters_count`](Self::voters_count)
    /// as soon as [`multiple`](Self::multiple) lets a voter tick twice.
    pub votes_count: i32,
    /// Whether the [`polls.end_notify`](crate::jobs::polls::KIND_END_NOTIFY)
    /// sweep has already told about this poll closing (WBE-113); the column
    /// starts `false` and [`mark_ended_notified`] is the only write that
    /// raises it. A [`user::Flag`], not a third bare `bool`, for the reason
    /// every wide table's booleans are (Clippy's `struct_excessive_bools`,
    /// which the repo models rather than loosens).
    ///
    /// [`user::Flag`]: crate::models::user::Flag
    pub ended_notified: user::Flag,
}

/// One row of `poll_options`: one choice of a poll.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct PollOption {
    /// `UUIDv7` primary key, minted by [`create`] (decision 4); a vote names
    /// this id.
    pub id: Uuid,
    /// The poll this option belongs to; deleting the poll deletes its options.
    pub poll_id: Uuid,
    /// 0-based order within the poll: the index a client votes by and the
    /// order [`options`] answers in.
    pub position: i32,
    /// The option's text as its author wrote it.
    pub title: String,
    /// Votes this option has; moved by the vote transaction, `0` when the
    /// poll was created.
    pub votes_count: i32,
}

/// What to create a poll from: every column of `polls` except the id
/// [`create`] mints, the `status_id` it is handed and the timestamps the
/// database stamps.
///
/// The counters default to zero — what a locally created poll starts with —
/// and the ingest path (see [`crate::services::ingest`]) fills them from a
/// remote `Question`'s reported numbers, because a mirrored poll has
/// already been voted on somewhere and its counters arrive with it.
#[derive(Clone, Debug, Default)]
pub struct PollParams {
    /// The choices, in the order voters see them — [`create`] writes one
    /// `poll_options` row each, at position `0`, `1`, … Empty is allowed
    /// here (the endpoint's minimum-options check is the API's, not this
    /// layer's).
    pub options: Vec<String>,
    /// When voting closes; `None` for a poll that never expires. The seconds
    /// a client sends are converted before this struct is built.
    pub expires_at: Option<DateTime<Utc>>,
    /// Whether a voter may mark more than one option; `false` by default.
    pub multiple: bool,
    /// Whether per-option counts stay hidden until the poll closes; `false`
    /// by default.
    pub hide_totals: bool,
    /// Per-option vote counts, index-aligned with
    /// [`options`](Self::options); positions past the end of this list
    /// start at `0`, which is what every local create leaves it.
    pub option_votes: Vec<i32>,
    /// Distinct accounts that have voted; `0` for a fresh poll, the
    /// remote's `votersCount` for a mirrored one.
    pub voters_count: i32,
    /// Votes cast in total; `0` for a fresh poll, the remote's reported
    /// sum (or the option counts' total) for a mirrored one.
    pub votes_count: i32,
}
