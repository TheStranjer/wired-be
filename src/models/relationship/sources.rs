//! The flag sources: the fixed query set behind `Relationship::load_many`
//! and the rows each query answers.

use std::collections::{HashMap, HashSet};

use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    error::AppResult,
    models::{account_note, endorsement, follow::FollowState, mute},
};

/// [`load_many`](Relationship::load_many)'s outgoing side: the viewer's own
/// follow rows for every requested target, with the two per-follow toggles
/// the entity carries. One statement, one fetch, any number of targets.
const OUTGOING_SQL: &str = "SELECT followee_id AS target_id, state, show_reblogs, notify
      FROM follows
     WHERE follower_id = $1 AND followee_id = ANY($2)";

/// [`load_many`](Relationship::load_many)'s incoming side: which of the
/// requested targets have an accepted follow of the viewer — the whole
/// answer behind `followed_by`.
const INCOMING_SQL: &str = "SELECT follower_id AS target_id
      FROM follows
     WHERE followee_id = $1 AND follower_id = ANY($2) AND state = 'accepted'";

/// [`load_many`](Relationship::load_many)'s domain-block side: which of
/// the requested targets are remote accounts on a domain the viewer
/// blocks — the whole answer behind `domain_blocking`, in one statement.
/// `NOT t.local` states the ticket's rule that a local account is never
/// domain-blocked (its `domain` is `NULL` and joins nowhere anyway); the
/// join matches the canonical stored spelling of the block exactly, with
/// no subdomain widening.
const DOMAIN_BLOCKING_SQL: &str = "SELECT t.id AS target_id FROM users t
      JOIN user_domain_blocks b ON b.user_id = $1 AND b.domain = t.domain
     WHERE t.id = ANY($2) AND NOT t.local";

/// [`load_many`](Relationship::load_many)'s block side: both directions of
/// the viewer's blocks among the requested targets in one statement — the
/// whole `blocking` and `blocked_by` answer. Each arm keys its rows on the
/// account the entity is about (the *other* end of the pair) and carries the
/// direction as a literal, so the two flags need no second query and no
/// post-hoc matching between arms.
const BLOCKS_SQL: &str = "SELECT blocked_id AS target_id, true AS outgoing FROM blocks
      WHERE blocker_id = $1 AND blocked_id = ANY($2)
  UNION ALL
    SELECT blocker_id AS target_id, false AS outgoing FROM blocks
      WHERE blocked_id = $1 AND blocker_id = ANY($2)";

/// One `follows` row of the viewer's, narrowed to the columns that feed
/// relationship flags. The full row type lives in
/// [`crate::models::follow::Follow`]; this is only ever a query result.
#[derive(Debug, sqlx::FromRow)]
pub(super) struct OutgoingFollow {
    /// The followed account: `follows.followee_id`, aliased to the target
    /// role it always plays here.
    pub(super) target_id: Uuid,
    /// Lifecycle state of the row: decides `following` and `requested`.
    pub(super) state: FollowState,
    /// Stored `show_reblogs` column: the `showing_reblogs` flag.
    pub(super) show_reblogs: bool,
    /// Stored `notify` column: the `notifying` flag (and its Pleroma alias
    /// `subscribing`).
    pub(super) notify: bool,
}

/// The loaded answer of every flag source for one batch, one field per
/// source query (see the [module docs](self) for the tickets that extend
/// it). A flag whose source is absent here keeps the default
/// [`Relationship::unconnected`] gives it.
#[derive(Debug)]
pub(super) struct FlagSources {
    /// The viewer's follow rows for the targets, keyed by target id:
    /// decides `following`, `requested`, `showing_reblogs`, `notifying`
    /// and `subscribing`. Loaded by [`outgoing_follows`] — one query.
    pub(super) outgoing: HashMap<Uuid, OutgoingFollow>,
    /// The targets with an accepted follow of the viewer: decides
    /// `followed_by`. Loaded by [`incoming_follows`] — one query.
    pub(super) followed_by: HashSet<Uuid>,
    /// The viewer's stored notes, keyed by target: decides `note`. Loaded
    /// by [`account_note::get_many`] — one query. Targets without a stored
    /// note are absent, and [`assemble`] renders them the empty string.
    pub(super) notes: HashMap<Uuid, String>,
    /// The remote targets on a domain the viewer blocks: decides
    /// `domain_blocking`. Loaded by [`domain_blocked_targets`] — one
    /// query. Local targets and unblocked domains are absent, and
    /// [`assemble`] renders them the flag's `false` default.
    pub(super) domain_blocking: HashSet<Uuid>,
    /// The accounts the viewer endorses: decides `endorsed`. Loaded by
    /// [`endorsement::endorsed_ids`] — one query over the viewer's whole
    /// set. Targets the viewer has not pinned are absent, and
    /// [`assemble`] renders them the flag's `false` default.
    pub(super) endorsed: HashSet<Uuid>,
    /// The blocks between the viewer and the targets, in both directions:
    /// decides `blocking` and `blocked_by`. Loaded by [`block_flags`] —
    /// one query over the `blocks` table. Targets with no block either way
    /// are absent from both sets, and [`assemble`] renders them the flags'
    /// `false` defaults.
    pub(super) blocks: BlockFlags,
    /// The accounts the viewer mutes, and the muted ones whose notifications
    /// they hide: decides `muting` and `muting_notifications`. Loaded by
    /// [`mute_flags`] — the `mutes` table's two flat sets, read through
    /// [`crate::models::mute`], which filters expired mutes out of both.
    /// Targets the viewer does not mute are absent from both sets, and
    /// [`assemble`] renders them the flags' `false` defaults.
    pub(super) mutes: MuteFlags,
}

/// Both flags one viewer's mutes answer, as the two [`FlagSources`] fields of
/// the same name: the muter's muted set and its notification-hidden subset.
///
/// The subset relation is the table's own: an account whose mute hides only
/// the timeline is in [`muting`](Self::muting) and not in
/// [`muting_notifications`](Self::muting_notifications), which is exactly how
/// the two Mastodon flags differ. An expired mute is in neither, because both
/// sets come from reads that treat it as absent.
#[derive(Debug)]
pub(super) struct MuteFlags {
    /// The targets the viewer mutes: the `muting` flag.
    pub(super) muting: HashSet<Uuid>,
    /// The muted targets whose notifications the viewer also hides: the
    /// `muting_notifications` flag.
    pub(super) muting_notifications: HashSet<Uuid>,
}

/// Both directions of one viewer's blocks among the targets of a batch, read
/// off [`BLOCKS_SQL`]'s single statement: the two [`FlagSources`] fields the
/// blocks table answers.
///
/// The two sets are disjoint by construction — a target the viewer blocked is
/// `blocking`, one who blocked the viewer is `blocked_by` — except for a pair
/// that blocked each other both ways, which lights both flags exactly as the
/// two stored rows mean it.
#[derive(Debug)]
pub(super) struct BlockFlags {
    /// The targets the viewer has blocked: the `blocking` flag.
    pub(super) blocking: HashSet<Uuid>,
    /// The targets who have blocked the viewer: the `blocked_by` flag.
    pub(super) blocked_by: HashSet<Uuid>,
}

/// One [`BLOCKS_SQL`] row: a target with a block between them and the viewer
/// and the direction that block runs. Only ever a query result; the row type
/// the table owns is [`Block`](crate::models::block::Block).
#[derive(Debug, sqlx::FromRow)]
pub(super) struct BlockFlag {
    /// The account this relationship is about: the other end of the pair.
    pub(super) target_id: Uuid,
    /// Whether the block runs from the viewer (`true`, the statement's first
    /// arm) or towards them (`false`, its second arm).
    pub(super) outgoing: bool,
}

/// Runs the fixed query set behind [`Relationship::load_many`] and keeps
/// the results together. Every flag source is awaited exactly once here, and
/// is one statement except for the mutes, whose two sets are one statement
/// each; the tickets listed in the [module docs](self) add their one line
/// (and their one [`FlagSources`] field) to this list.
pub(super) async fn flag_sources(
    pool: &PgPool,
    viewer_id: Uuid,
    target_ids: &[Uuid],
) -> AppResult<FlagSources> {
    Ok(FlagSources {
        // The follows table (this ticket): one query per direction.
        outgoing: outgoing_follows(pool, viewer_id, target_ids).await?,
        followed_by: incoming_follows(pool, viewer_id, target_ids).await?,
        // The account_notes table: one batched read of the viewer's notes.
        notes: account_note::get_many(pool, viewer_id, target_ids).await?,
        // The user_domain_blocks table: one join deciding the flag for
        // every target at once.
        domain_blocking: domain_blocked_targets(pool, viewer_id, target_ids).await?,
        // The endorsements table: one read of the viewer's whole pinned
        // set; membership answers the flag for every target at once.
        endorsed: endorsement::endorsed_ids(pool, viewer_id)
            .await?
            .into_iter()
            .collect(),
        // The blocks table: one statement over both directions of the
        // pair; membership answers each of the two flags.
        blocks: block_flags(pool, viewer_id, target_ids).await?,
        // The mutes table: the muter's two flat sets, one read each;
        // membership answers `muting` and `muting_notifications`.
        mutes: mute_flags(pool, viewer_id).await?,
    })
}

/// The viewer's own follow rows for `target_ids`, keyed by target. One
/// statement, whatever the batch size.
async fn outgoing_follows(
    pool: &PgPool,
    viewer_id: Uuid,
    target_ids: &[Uuid],
) -> AppResult<HashMap<Uuid, OutgoingFollow>> {
    let rows = sqlx::query_as::<_, OutgoingFollow>(OUTGOING_SQL)
        .bind(viewer_id)
        .bind(target_ids.to_vec())
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(|row| (row.target_id, row)).collect())
}

/// The ids among `target_ids` with an accepted follow of `viewer_id` —
/// the whole `followed_by` answer, in one statement.
async fn incoming_follows(
    pool: &PgPool,
    viewer_id: Uuid,
    target_ids: &[Uuid],
) -> AppResult<HashSet<Uuid>> {
    let rows = sqlx::query_as::<_, (Uuid,)>(INCOMING_SQL)
        .bind(viewer_id)
        .bind(target_ids.to_vec())
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(|(target_id,)| target_id).collect())
}

/// The ids among `target_ids` of remote accounts on one of `viewer_id`'s
/// blocked domains — the whole `domain_blocking` answer, in one
/// statement (see [`DOMAIN_BLOCKING_SQL`]).
async fn domain_blocked_targets(
    pool: &PgPool,
    viewer_id: Uuid,
    target_ids: &[Uuid],
) -> AppResult<HashSet<Uuid>> {
    let rows = sqlx::query_as::<_, (Uuid,)>(DOMAIN_BLOCKING_SQL)
        .bind(viewer_id)
        .bind(target_ids.to_vec())
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(|(target_id,)| target_id).collect())
}

/// Both directions of `viewer_id`'s blocks among `target_ids` — the whole
/// `blocking` and `blocked_by` answer, in the one statement
/// [`BLOCKS_SQL`] is (see [`BlockFlags`]).
async fn block_flags(pool: &PgPool, viewer_id: Uuid, target_ids: &[Uuid]) -> AppResult<BlockFlags> {
    let rows = sqlx::query_as::<_, BlockFlag>(BLOCKS_SQL)
        .bind(viewer_id)
        .bind(target_ids.to_vec())
        .fetch_all(pool)
        .await?;
    let mut blocking = HashSet::new();
    let mut blocked_by = HashSet::new();
    for row in rows {
        let set = if row.outgoing {
            &mut blocking
        } else {
            &mut blocked_by
        };
        let _added: bool = set.insert(row.target_id);
    }
    Ok(BlockFlags {
        blocking,
        blocked_by,
    })
}

/// `viewer_id`'s mutes, in the two sets the entity's flags key on: who they
/// mute, and who they mute with their notifications hidden. Both come from
/// [`crate::models::mute`], the table's owner, which leaves an expired mute
/// out of either; the batch's targets need no narrowing, because membership
/// in the muter's own set is the whole question (the same shape the
/// `endorsed` flag reads its set from).
async fn mute_flags(pool: &PgPool, viewer_id: Uuid) -> AppResult<MuteFlags> {
    let muting = mute::muted_ids(pool, viewer_id)
        .await?
        .into_iter()
        .collect();
    let muting_notifications = mute::notification_muted_ids(pool, viewer_id)
        .await?
        .into_iter()
        .collect();
    Ok(MuteFlags {
        muting,
        muting_notifications,
    })
}
