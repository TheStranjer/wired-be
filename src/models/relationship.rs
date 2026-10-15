//! Batch loading of the [`Relationship`] entity from the tables that answer
//! its flags.
//!
//! [`Relationship::load_many`] answers the relationship flags of one viewer
//! to many targets in a fixed, bounded set of queries: every source of flags
//! is one statement, executed once per call whatever the number of targets —
//! either a `WHERE … = ANY(…)` batch over the requested ids or, for a source
//! whose whole set is smaller than the question (the endorsements and the
//! mutes), one read of the viewer's own set, of which membership then answers
//! every target — so there is no N+1 anywhere in this module. The
//! `FlagSources` struct mirrors that contract — one field per source — and
//! `flag_sources` is the single place the set runs.
//!
//! The `follows` table (owned by [`crate::models::follow`]) supplies the
//! flags this ticket computes: `following`, `requested` and
//! `showing_reblogs` / `notifying` from the viewer's own follow rows, and
//! `followed_by` from the accepted incoming rows. The `account_notes`
//! table (owned by [`crate::models::account_note`]) supplies `note`: the
//! viewer's stored comment, the empty string where they have none. The
//! `user_domain_blocks` table (owned by [`crate::models::domain_block`])
//! supplies `domain_blocking`: true for a remote target whose host the
//! viewer has blocked, never for a local one. The `endorsements` table
//! (owned by [`crate::models::endorsement`]) supplies `endorsed`: one read
//! of the viewer's whole endorsed set, so a target pinned by the viewer
//! lights the flag. The `blocks` table (owned by
//! [`crate::models::block`]) supplies `blocking` and `blocked_by`: one
//! statement over both directions of the pair, so a target the viewer
//! blocked lights the first flag and a target who blocked the viewer the
//! second. The `mutes` table (owned by [`crate::models::mute`]) supplies
//! `muting` and `muting_notifications`: the muter's two flat sets, read
//! through the model that owns them, so a target the viewer mutes lights the
//! first flag and one whose notifications are hidden as well the second —
//! and a mute whose duration has run out is in neither set, which is how
//! every read of that table treats it. Every other flag of the entity is a
//! documented default until its own ticket lands; each one then adds exactly
//! one field to `FlagSources`, one line (one query) to `flag_sources` and one
//! flag line to `assemble`.

use std::collections::{HashMap, HashSet};

use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    entities::{account::JsonBool, relationship::Relationship},
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
struct OutgoingFollow {
    /// The followed account: `follows.followee_id`, aliased to the target
    /// role it always plays here.
    target_id: Uuid,
    /// Lifecycle state of the row: decides `following` and `requested`.
    state: FollowState,
    /// Stored `show_reblogs` column: the `showing_reblogs` flag.
    show_reblogs: bool,
    /// Stored `notify` column: the `notifying` flag (and its Pleroma alias
    /// `subscribing`).
    notify: bool,
}

/// The loaded answer of every flag source for one batch, one field per
/// source query (see the [module docs](self) for the tickets that extend
/// it). A flag whose source is absent here keeps the default
/// [`Relationship::unconnected`] gives it.
#[derive(Debug)]
struct FlagSources {
    /// The viewer's follow rows for the targets, keyed by target id:
    /// decides `following`, `requested`, `showing_reblogs`, `notifying`
    /// and `subscribing`. Loaded by [`outgoing_follows`] — one query.
    outgoing: HashMap<Uuid, OutgoingFollow>,
    /// The targets with an accepted follow of the viewer: decides
    /// `followed_by`. Loaded by [`incoming_follows`] — one query.
    followed_by: HashSet<Uuid>,
    /// The viewer's stored notes, keyed by target: decides `note`. Loaded
    /// by [`account_note::get_many`] — one query. Targets without a stored
    /// note are absent, and [`assemble`] renders them the empty string.
    notes: HashMap<Uuid, String>,
    /// The remote targets on a domain the viewer blocks: decides
    /// `domain_blocking`. Loaded by [`domain_blocked_targets`] — one
    /// query. Local targets and unblocked domains are absent, and
    /// [`assemble`] renders them the flag's `false` default.
    domain_blocking: HashSet<Uuid>,
    /// The accounts the viewer endorses: decides `endorsed`. Loaded by
    /// [`endorsement::endorsed_ids`] — one query over the viewer's whole
    /// set. Targets the viewer has not pinned are absent, and
    /// [`assemble`] renders them the flag's `false` default.
    endorsed: HashSet<Uuid>,
    /// The blocks between the viewer and the targets, in both directions:
    /// decides `blocking` and `blocked_by`. Loaded by [`block_flags`] —
    /// one query over the `blocks` table. Targets with no block either way
    /// are absent from both sets, and [`assemble`] renders them the flags'
    /// `false` defaults.
    blocks: BlockFlags,
    /// The accounts the viewer mutes, and the muted ones whose notifications
    /// they hide: decides `muting` and `muting_notifications`. Loaded by
    /// [`mute_flags`] — the `mutes` table's two flat sets, read through
    /// [`crate::models::mute`], which filters expired mutes out of both.
    /// Targets the viewer does not mute are absent from both sets, and
    /// [`assemble`] renders them the flags' `false` defaults.
    mutes: MuteFlags,
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
struct MuteFlags {
    /// The targets the viewer mutes: the `muting` flag.
    muting: HashSet<Uuid>,
    /// The muted targets whose notifications the viewer also hides: the
    /// `muting_notifications` flag.
    muting_notifications: HashSet<Uuid>,
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
struct BlockFlags {
    /// The targets the viewer has blocked: the `blocking` flag.
    blocking: HashSet<Uuid>,
    /// The targets who have blocked the viewer: the `blocked_by` flag.
    blocked_by: HashSet<Uuid>,
}

/// One [`BLOCKS_SQL`] row: a target with a block between them and the viewer
/// and the direction that block runs. Only ever a query result; the row type
/// the table owns is [`Block`](crate::models::block::Block).
#[derive(Debug, sqlx::FromRow)]
struct BlockFlag {
    /// The account this relationship is about: the other end of the pair.
    target_id: Uuid,
    /// Whether the block runs from the viewer (`true`, the statement's first
    /// arm) or towards them (`false`, its second arm).
    outgoing: bool,
}

/// The single inherent implementation of the entity: the constructor that
/// holds its defaults and the batch loader that computes the rest. It
/// lives here rather than beside the struct because every query belongs
/// to the models layer (decision 3), and the crate denies splitting a
/// type's inherent blocks across modules (`multiple_inherent_impl`).
impl Relationship {
    /// The relationship of a viewer with nothing recorded about
    /// `target_id`: every flag `false` — except
    /// [`showing_reblogs`](Self::showing_reblogs), which only a stored
    /// hiding can make `false` — and an empty
    /// [`note`](Self::note). [`load_many`](Self::load_many) starts from
    /// this and overwrites exactly the flags its sources computed.
    #[must_use]
    pub fn unconnected(target_id: Uuid) -> Self {
        Self {
            id: target_id.to_string(),
            following: JsonBool(false),
            requested: JsonBool(false),
            endorsed: JsonBool(false),
            followed_by: JsonBool(false),
            blocking: JsonBool(false),
            blocked_by: JsonBool(false),
            muting: JsonBool(false),
            muting_notifications: JsonBool(false),
            notifying: JsonBool(false),
            showing_reblogs: JsonBool(true),
            domain_blocking: JsonBool(false),
            note: String::new(),
            subscribing: JsonBool(false),
        }
    }

    /// Loads the relationship of `viewer_id` to each of `target_ids`, one
    /// entity per requested id, in the requested order (a repeated id is
    /// answered once per occurrence).
    ///
    /// The whole answer costs one fixed set of queries — see
    /// `flag_sources` — never a query per target. Ids that name no user
    /// are not an error: a relationship is just the (default) flags, and
    /// deciding what an unknown target means for a caller (skipping it,
    /// 404ing) is the endpoint's job, not the loader's. An empty `target_ids`
    /// answers an empty `Vec` without touching the database.
    ///
    /// # Errors
    ///
    /// Any database failure (connection, encoding) arrives as
    /// [`AppError::Database`](crate::error::AppError).
    pub async fn load_many(
        pool: &PgPool,
        viewer_id: Uuid,
        target_ids: &[Uuid],
    ) -> AppResult<Vec<Self>> {
        if target_ids.is_empty() {
            return Ok(Vec::new());
        }
        let sources = flag_sources(pool, viewer_id, target_ids).await?;
        Ok(target_ids
            .iter()
            .map(|&target_id| assemble(target_id, &sources))
            .collect())
    }
}

/// Runs the fixed query set behind [`Relationship::load_many`] and keeps
/// the results together. Every flag source is awaited exactly once here, and
/// is one statement except for the mutes, whose two sets are one statement
/// each; the tickets listed in the [module docs](self) add their one line
/// (and their one [`FlagSources`] field) to this list.
async fn flag_sources(
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

/// Folds the loaded sources into the entity for one target: the six flags
/// the follows table answers, the [`note`] the notes table holds, the
/// `domain_blocking` the domain-block join decided, the `endorsed` the
/// viewer's endorsement set contains, the two directions of the blocks table
/// and the two sets of the mutes table take their sources' values, every
/// other flag keeps the default [`Relationship::unconnected`] states.
///
/// [`note`]: Relationship::note
fn assemble(target_id: Uuid, sources: &FlagSources) -> Relationship {
    let follow = sources.outgoing.get(&target_id);
    let following = follow.is_some_and(|row| row.state == FollowState::Accepted);
    let requested = !following && follow.is_some_and(|row| row.state == FollowState::Pending);
    let notifying = follow.is_some_and(|row| row.notify);
    Relationship {
        following: JsonBool(following),
        requested: JsonBool(requested),
        followed_by: JsonBool(sources.followed_by.contains(&target_id)),
        notifying: JsonBool(notifying),
        subscribing: JsonBool(notifying),
        showing_reblogs: JsonBool(follow.is_none_or(|row| row.show_reblogs)),
        domain_blocking: JsonBool(sources.domain_blocking.contains(&target_id)),
        endorsed: JsonBool(sources.endorsed.contains(&target_id)),
        blocking: JsonBool(sources.blocks.blocking.contains(&target_id)),
        blocked_by: JsonBool(sources.blocks.blocked_by.contains(&target_id)),
        muting: JsonBool(sources.mutes.muting.contains(&target_id)),
        muting_notifications: JsonBool(sources.mutes.muting_notifications.contains(&target_id)),
        note: sources.notes.get(&target_id).cloned().unwrap_or_default(),
        ..Relationship::unconnected(target_id)
    }
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        models::{
            account_note, block, domain_block, follow,
            user::{Flag, NewUser, User, insert, local_ap_id},
        },
        util::id::new_id,
    };

    /// The two per-follow toggles [`set_toggles`] stores, passed as a
    /// struct so the helper does not read as a pair of switch parameters.
    #[derive(Debug)]
    struct Toggles {
        /// Value to store in `show_reblogs`.
        show_reblogs: bool,
        /// Value to store in `notify`.
        notify: bool,
    }

    /// Inserts a local, unlocked user named `nickname`.
    async fn local_user(pool: &PgPool, nickname: &str) -> User {
        insert(
            pool,
            &NewUser::local(nickname, &local_ap_id(TEST_BASE_URL, nickname)),
        )
        .await
        .expect("the local fixture inserts")
    }

    /// Inserts a local user named `nickname` whose account is locked, so a
    /// follow of it waits for an answer.
    async fn locked_user(pool: &PgPool, nickname: &str) -> User {
        insert(
            pool,
            &NewUser {
                locked: Flag::new(true),
                ..NewUser::local(nickname, &local_ap_id(TEST_BASE_URL, nickname))
            },
        )
        .await
        .expect("the locked fixture inserts")
    }

    /// Inserts a mirror of the remote actor `nickname` on `host`: the
    /// non-local row a fetched actor leaves behind (decision 7), with its
    /// host in `domain` — the column the domain-block join reads.
    async fn remote_user(pool: &PgPool, nickname: &str, host: &str) -> User {
        insert(
            pool,
            &NewUser {
                nickname: format!("{nickname}@{host}"),
                ap_id: format!("https://{host}/users/{nickname}"),
                domain: Some(host.to_owned()),
                name: nickname.to_owned(),
                ..NewUser::default()
            },
        )
        .await
        .expect("the remote fixture inserts")
    }

    /// The relationship [`Relationship::load_many`] answers for `viewer`
    /// to `target`, which the test expects to be the only row out.
    async fn one_relationship(pool: &PgPool, viewer: &User, target: &User) -> Relationship {
        let loaded = Relationship::load_many(pool, viewer.id, &[target.id])
            .await
            .unwrap();
        assert_eq!(loaded.len(), 1, "one target in answers one row out");
        let relationship = loaded.first().expect("the batch answered one row");
        assert_eq!(
            relationship.id,
            target.id.to_string(),
            "the row is about the requested target"
        );
        relationship.clone()
    }

    /// Stores `toggles` on the `viewer → target` row the way the (later)
    /// follow-actions ticket's endpoints will: [`follow::follow`] itself
    /// only ever writes the column defaults.
    async fn set_toggles(pool: &PgPool, viewer: &User, target: &User, toggles: Toggles) {
        let updated = sqlx::query(
            "UPDATE follows SET show_reblogs = $3, notify = $4, updated_at = now()
              WHERE follower_id = $1 AND followee_id = $2",
        )
        .bind(viewer.id)
        .bind(target.id)
        .bind(toggles.show_reblogs)
        .bind(toggles.notify)
        .execute(pool)
        .await
        .unwrap();
        assert_eq!(
            updated.rows_affected(),
            1,
            "the fixture row is there to update"
        );
    }

    /// Hides reblogs and turns notifications on: the non-default side of
    /// both toggles the loader must read off the viewer's row.
    fn flipped() -> Toggles {
        Toggles {
            show_reblogs: false,
            notify: true,
        }
    }

    /// Writes a `rejected` row by hand: [`follow::reject`] deletes the row
    /// rather than storing that state, so only a mirrored remote rejection
    /// produces one (see the [`crate::models::follow`] docs).
    async fn rejected_relation(pool: &PgPool, follower: &User, target: &User) {
        let written = sqlx::query(
            "INSERT INTO follows (id, follower_id, followee_id, state)
             VALUES ($1, $2, $3, 'rejected')",
        )
        .bind(new_id())
        .bind(follower.id)
        .bind(target.id)
        .execute(pool)
        .await
        .unwrap();
        assert_eq!(written.rows_affected(), 1, "the rejected fixture row lands");
    }

    /// Asserts the flags no source has computed (yet) keep their defaults:
    /// the constants the ticket promises until each flag's own ticket
    /// stores its data. `note` is computed now, but these fixtures store
    /// no note, so its source answers the empty default here too;
    /// `domain_blocking` is computed as well, and its dedicated fixtures
    /// below test it — these tests pair viewers with local targets, which
    /// that flag can never light up for; `endorsed` is computed too, and
    /// its dedicated fixture below tests it — these tests record no
    /// endorsement, so the flag keeps its default; `blocking` and
    /// `blocked_by` are computed as well, and their dedicated fixture
    /// below tests them — these tests record no block, so both flags keep
    /// their defaults; `muting` and `muting_notifications` are computed as
    /// well, and their dedicated fixture below tests them — these tests
    /// record no mute, so both flags keep their defaults.
    fn assert_placeholder_flags(relationship: &Relationship) {
        assert!(!relationship.endorsed.0, "no endorsement recorded");
        assert!(!relationship.blocking.0, "no block recorded, outgoing");
        assert!(!relationship.blocked_by.0, "no block recorded, incoming");
        assert!(!relationship.muting.0, "no mute recorded");
        assert!(
            !relationship.muting_notifications.0,
            "no notification mute recorded"
        );
        assert!(
            relationship.note.is_empty(),
            "no stored note reads back empty"
        );
    }

    #[tokio::test]
    async fn nothing_recorded_answers_the_default_flags() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;

        let relationship = one_relationship(&pool, &alice, &bob).await;
        assert!(!relationship.following.0, "no row, no follow");
        assert!(!relationship.requested.0, "no row, no request");
        assert!(!relationship.followed_by.0, "no row, no follower");
        assert!(
            !relationship.notifying.0,
            "notifying defaults to false with no row"
        );
        assert!(
            !relationship.subscribing.0,
            "the alias carries the same default"
        );
        assert!(
            relationship.showing_reblogs.0,
            "with no row nothing was ever hidden"
        );
        assert_placeholder_flags(&relationship);
    }

    #[tokio::test]
    async fn an_accepted_follow_computes_following_and_the_toggles() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let granted = follow::follow(&pool, TEST_BASE_URL, &alice, &bob)
            .await
            .unwrap();
        assert_eq!(
            granted.state,
            FollowState::Accepted,
            "an open local account answers on the spot"
        );

        let defaults = one_relationship(&pool, &alice, &bob).await;
        assert!(defaults.following.0, "the accepted row follows");
        assert!(!defaults.requested.0, "and nothing is being asked");
        assert!(
            defaults.showing_reblogs.0,
            "the row's stored show_reblogs default"
        );
        assert!(!defaults.notifying.0, "the row's stored notify default");

        set_toggles(&pool, &alice, &bob, flipped()).await;
        let toggled = one_relationship(&pool, &alice, &bob).await;
        assert!(toggled.following.0, "toggling does not unfollow");
        assert!(
            !toggled.showing_reblogs.0,
            "show_reblogs rides on the viewer's own row"
        );
        assert!(toggled.notifying.0, "so does notify");
        assert!(
            toggled.subscribing.0,
            "and Pleroma's alias carries the same value"
        );
        assert!(
            !toggled.followed_by.0,
            "following one way does not compute the other"
        );
        assert_placeholder_flags(&toggled);
    }

    #[tokio::test]
    async fn a_pending_request_is_requested_but_not_following() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let carol = locked_user(&pool, "carol").await;
        let waiting = follow::follow(&pool, TEST_BASE_URL, &alice, &carol)
            .await
            .unwrap();
        assert_eq!(waiting.state, FollowState::Pending, "the fixture waits");

        let relationship = one_relationship(&pool, &alice, &carol).await;
        assert!(relationship.requested.0, "the ask is visible");
        assert!(!relationship.following.0, "but not granted");
        assert!(!relationship.followed_by.0, "and not in reverse");
        assert!(
            relationship.showing_reblogs.0,
            "a waiting row still carries the show_reblogs default"
        );
        assert!(!relationship.notifying.0, "and the notify default");
        assert_placeholder_flags(&relationship);
    }

    #[tokio::test]
    async fn an_accepted_incoming_follow_computes_followed_by() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let incoming = follow::follow(&pool, TEST_BASE_URL, &bob, &alice)
            .await
            .unwrap();
        assert_eq!(
            incoming.state,
            FollowState::Accepted,
            "bob's follow of alice is granted"
        );

        let relationship = one_relationship(&pool, &alice, &bob).await;
        assert!(relationship.followed_by.0, "bob follows alice");
        assert!(
            !relationship.following.0,
            "and alice does not follow bob back from that"
        );
        assert!(
            !relationship.requested.0,
            "an incoming follow is not an outgoing request"
        );
        assert!(
            relationship.showing_reblogs.0,
            "the incoming row cannot set alice's own toggles"
        );
        assert!(!relationship.notifying.0, "nor her notify");
        assert_placeholder_flags(&relationship);
    }

    #[tokio::test]
    async fn a_mutual_follow_computes_both_directions() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let outgoing = follow::follow(&pool, TEST_BASE_URL, &alice, &bob)
            .await
            .unwrap();
        assert_eq!(
            outgoing.state,
            FollowState::Accepted,
            "the outgoing leg is granted"
        );
        let incoming = follow::follow(&pool, TEST_BASE_URL, &bob, &alice)
            .await
            .unwrap();
        assert_eq!(
            incoming.state,
            FollowState::Accepted,
            "and so is the incoming one"
        );
        set_toggles(&pool, &alice, &bob, flipped()).await;

        let relationship = one_relationship(&pool, &alice, &bob).await;
        assert!(relationship.following.0, "alice follows bob");
        assert!(relationship.followed_by.0, "and is followed back");
        assert!(!relationship.requested.0, "nothing waits");
        assert!(
            !relationship.showing_reblogs.0,
            "the viewer's own row sets the toggles"
        );
        assert!(relationship.notifying.0, "on both aliases");
        assert!(relationship.subscribing.0, "on both aliases");
        assert_placeholder_flags(&relationship);
    }

    #[tokio::test]
    async fn rejected_rows_compute_no_following_flags() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        rejected_relation(&pool, &alice, &bob).await;
        rejected_relation(&pool, &bob, &alice).await;

        let relationship = one_relationship(&pool, &alice, &bob).await;
        assert!(
            !relationship.following.0,
            "a rejected row is not an accepted follow"
        );
        assert!(!relationship.requested.0, "nor a pending request");
        assert!(
            !relationship.followed_by.0,
            "and the incoming side is not accepted either"
        );
        assert!(
            relationship.showing_reblogs.0,
            "the rejected row only carries column defaults"
        );
        assert_placeholder_flags(&relationship);
    }

    #[tokio::test]
    async fn a_stored_note_rides_the_relationship() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let carol = local_user(&pool, "carol").await;
        let granted = follow::follow(&pool, TEST_BASE_URL, &alice, &bob)
            .await
            .unwrap();
        assert_eq!(granted.state, FollowState::Accepted, "the fixture follows");
        account_note::set(&pool, alice.id, bob.id, "colleague from work")
            .await
            .unwrap();
        account_note::set(&pool, bob.id, alice.id, "who?")
            .await
            .unwrap();

        let relationship = one_relationship(&pool, &alice, &bob).await;
        assert_eq!(
            relationship.note, "colleague from work",
            "the viewer's own note on the target loads"
        );
        assert!(relationship.following.0, "with the follow flags beside it");

        let loaded = Relationship::load_many(&pool, alice.id, &[bob.id, carol.id])
            .await
            .unwrap();
        assert_eq!(loaded.len(), 2, "one answer per requested id");
        assert_eq!(loaded[0].note, "colleague from work", "noted target");
        assert!(loaded[1].note.is_empty(), "unnoted target keeps empty");

        let bobs_view = one_relationship(&pool, &bob, &alice).await;
        assert_eq!(
            bobs_view.note, "who?",
            "and bob sees his own note, not alice's"
        );
    }

    #[tokio::test]
    async fn a_cleared_note_leaves_the_relationship() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        account_note::set(&pool, alice.id, bob.id, "here for a while")
            .await
            .unwrap();
        assert_eq!(
            one_relationship(&pool, &alice, &bob).await.note,
            "here for a while",
            "the note loads while it is stored"
        );

        account_note::set(&pool, alice.id, bob.id, "")
            .await
            .unwrap();
        let relationship = one_relationship(&pool, &alice, &bob).await;
        assert!(
            relationship.note.is_empty(),
            "the empty write deleted the row, and the loader reads the default"
        );
        assert!(
            !relationship.following.0,
            "and nothing else about the pair changed"
        );
    }

    #[tokio::test]
    async fn results_keep_the_requested_order_in_one_query_set() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let carol = locked_user(&pool, "carol").await;
        let dave = local_user(&pool, "dave").await;
        let accepted = follow::follow(&pool, TEST_BASE_URL, &alice, &bob)
            .await
            .unwrap();
        assert_eq!(accepted.state, FollowState::Accepted, "alice → bob");
        let pending = follow::follow(&pool, TEST_BASE_URL, &alice, &carol)
            .await
            .unwrap();
        assert_eq!(pending.state, FollowState::Pending, "alice → carol waits");
        let incoming = follow::follow(&pool, TEST_BASE_URL, &dave, &alice)
            .await
            .unwrap();
        assert_eq!(incoming.state, FollowState::Accepted, "dave → alice");
        // An id no user holds: the loader is not the one deciding to skip
        // it (that is the endpoint's job), it just answers the defaults.
        let unknown = new_id();

        // The batch of mixed states below — outgoing, pending, incoming,
        // unknown, and a repeated id — is answered by the fixed query set:
        // `flag_sources` runs one statement per source exactly once per
        // call, and this asserts every row came back right from those two
        // fetches. There is no per-target query loop in the module to
        // count: one call, one `FlagSources`, N answers.
        let targets = [bob.id, carol.id, unknown, dave.id, bob.id];
        let loaded = Relationship::load_many(&pool, alice.id, &targets)
            .await
            .unwrap();
        assert_eq!(loaded.len(), targets.len(), "one answer per requested id");
        let order: Vec<String> = loaded.iter().map(|row| row.id.clone()).collect();
        let wanted: Vec<String> = targets.iter().map(Uuid::to_string).collect();
        assert_eq!(order, wanted, "the requested order, repeats included");

        let flags = |target: Uuid| {
            let row = loaded
                .iter()
                .find(|relationship| relationship.id == target.to_string())
                .expect("the batch answered the target");
            (row.following.0, row.requested.0, row.followed_by.0)
        };
        assert_eq!(flags(bob.id), (true, false, false), "the accepted follow");
        assert_eq!(flags(carol.id), (false, true, false), "the pending request");
        assert_eq!(flags(dave.id), (false, false, true), "the incoming follow");
        assert_eq!(flags(unknown), (false, false, false), "an unknown target");
    }

    #[tokio::test]
    async fn an_empty_batch_touches_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let loaded = Relationship::load_many(&pool, alice.id, &[]).await.unwrap();
        assert!(loaded.is_empty(), "no targets, no relationships");
    }

    #[tokio::test]
    async fn a_remote_target_on_a_blocked_domain_is_domain_blocking() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = remote_user(&pool, "bob", "spam.example").await;
        let carol = remote_user(&pool, "carol", "other.example").await;
        domain_block::block_domain(&pool, alice.id, "spam.example")
            .await
            .expect("the fixture block stores");

        let loaded = Relationship::load_many(&pool, alice.id, &[bob.id, carol.id])
            .await
            .expect("the batch reads");
        let flags: Vec<bool> = loaded.iter().map(|row| row.domain_blocking.0).collect();
        assert_eq!(
            flags,
            vec![true, false],
            "the blocked host's remote account is domain-blocking, another host's is not"
        );

        // The flag is the viewer's own: another account that has not
        // blocked anything reads the same target unblocked.
        let dave = local_user(&pool, "dave").await;
        let other_view = one_relationship(&pool, &dave, &bob).await;
        assert!(
            !other_view.domain_blocking.0,
            "dave blocked nothing, so bob's host is open to him"
        );

        domain_block::unblock_domain(&pool, alice.id, "spam.example")
            .await
            .expect("the fixture unblock runs");
        let after = one_relationship(&pool, &alice, &bob).await;
        assert!(!after.domain_blocking.0, "lifting the block lifts the flag");
    }

    #[tokio::test]
    async fn local_accounts_are_never_domain_blocking() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        // Block this very instance's domain: even a "blocked" host cannot
        // make a local target domain-blocking (the join's `NOT local`).
        domain_block::block_domain(&pool, alice.id, "test.example")
            .await
            .expect("the fixture block stores");

        let relationship = one_relationship(&pool, &alice, &bob).await;
        assert!(
            !relationship.domain_blocking.0,
            "the local target of a local viewer stands unblocked"
        );
        assert_placeholder_flags(&relationship);
    }

    #[tokio::test]
    async fn an_endorsement_lights_the_endorsed_flag() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let carol = local_user(&pool, "carol").await;
        let granted = follow::follow(&pool, TEST_BASE_URL, &alice, &bob)
            .await
            .unwrap();
        assert_eq!(granted.state, FollowState::Accepted, "the fixture follows");
        endorsement::endorse(&pool, alice.id, bob.id)
            .await
            .expect("the fixture endorsement stores");

        let relationship = one_relationship(&pool, &alice, &bob).await;
        assert!(
            relationship.endorsed.0,
            "a target the viewer pinned is endorsed"
        );
        assert!(relationship.following.0, "with the follow flags beside it");

        // The flag is the viewer's own: carol, who pinned nothing, reads
        // the same target unendorsed, and within alice's batch only the
        // pinned target lights up.
        let other_view = one_relationship(&pool, &carol, &bob).await;
        assert!(!other_view.endorsed.0, "carol endorsed nothing");
        let loaded = Relationship::load_many(&pool, alice.id, &[bob.id, carol.id])
            .await
            .unwrap();
        let flags: Vec<bool> = loaded.iter().map(|row| row.endorsed.0).collect();
        assert_eq!(flags, vec![true, false], "pinned, then not");

        endorsement::unendorse(&pool, alice.id, bob.id)
            .await
            .expect("the fixture withdrawal runs");
        let after = one_relationship(&pool, &alice, &bob).await;
        assert!(
            !after.endorsed.0,
            "withdrawing the pin falls back to the default"
        );
        assert!(after.following.0, "and nothing else about the pair changed");
    }

    /// The `(blocking, blocked_by)` pair [`Relationship::load_many`] answers
    /// for `viewer` over `targets`, in the order requested.
    async fn blocks_of(pool: &PgPool, viewer: &User, targets: &[&User]) -> Vec<(bool, bool)> {
        let ids: Vec<Uuid> = targets.iter().map(|target| target.id).collect();
        Relationship::load_many(pool, viewer.id, &ids)
            .await
            .expect("the batch reads")
            .iter()
            .map(|row| (row.blocking.0, row.blocked_by.0))
            .collect()
    }

    /// Records `blocker` blocking `target` through the model the table is
    /// owned by — the fixture this loader only ever reads.
    async fn block_pair(pool: &PgPool, blocker: &User, target: &User) {
        let stored = block::block(pool, TEST_BASE_URL, blocker, target)
            .await
            .expect("the fixture block stores");
        assert_eq!(
            stored.blocked_id, target.id,
            "the row is the pair that was asked for"
        );
    }

    /// Lifts `blocker`'s block on `target`, asserting one was there to lift.
    async fn unblock_pair(pool: &PgPool, blocker: &User, target: &User) {
        let removed = block::unblock(pool, blocker, target)
            .await
            .expect("the fixture unblock runs");
        assert!(removed.is_some(), "the pair's block was there to lift");
    }

    /// The two directions of the blocks table ride the same batch: the
    /// viewer's own block lights `blocking`, a target's block of the viewer
    /// lights `blocked_by`, a pair blocked both ways lights both, and a
    /// target with no block either way keeps both defaults — as does every
    /// account once its block is lifted.
    #[tokio::test]
    async fn blocks_light_blocking_and_blocked_by() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let carol = local_user(&pool, "carol").await;
        let dave = local_user(&pool, "dave").await;
        block_pair(&pool, &alice, &bob).await;
        block_pair(&pool, &dave, &alice).await;

        assert_eq!(
            blocks_of(&pool, &alice, &[&bob, &carol, &dave]).await,
            vec![(true, false), (false, false), (false, true)],
            "alice blocks bob, blocks nothing about carol, and is blocked by dave"
        );
        assert_eq!(
            blocks_of(&pool, &bob, &[&alice]).await,
            vec![(false, true)],
            "and bob sees the same row from his own end"
        );

        block_pair(&pool, &bob, &alice).await;
        assert_eq!(
            blocks_of(&pool, &alice, &[&bob]).await,
            vec![(true, true)],
            "a pair blocked both ways lights both flags"
        );

        unblock_pair(&pool, &alice, &bob).await;
        assert_eq!(
            blocks_of(&pool, &alice, &[&bob]).await,
            vec![(false, true)],
            "lifting one direction leaves the other one flag lit"
        );

        unblock_pair(&pool, &bob, &alice).await;
        unblock_pair(&pool, &dave, &alice).await;
        let cleared = one_relationship(&pool, &alice, &bob).await;
        assert!(
            !cleared.blocking.0 && !cleared.blocked_by.0,
            "with nothing blocked the pair falls back to the defaults"
        );
        assert_eq!(
            blocks_of(&pool, &alice, &[&bob, &carol, &dave]).await,
            vec![(false, false); 3],
            "and the whole batch reads clean"
        );
        assert_placeholder_flags(&cleared);
    }

    /// The `(muting, muting_notifications)` pair [`Relationship::load_many`]
    /// answers for `viewer` over `targets`, in the order requested.
    async fn mutes_of(pool: &PgPool, viewer: &User, targets: &[&User]) -> Vec<(bool, bool)> {
        let ids: Vec<Uuid> = targets.iter().map(|target| target.id).collect();
        Relationship::load_many(pool, viewer.id, &ids)
            .await
            .expect("the batch reads")
            .iter()
            .map(|row| (row.muting.0, row.muting_notifications.0))
            .collect()
    }

    /// Mutes `target` for `muter` through the model that owns the table — the
    /// fixture this loader only ever reads.
    async fn mute_pair(pool: &PgPool, muter: &User, target: &User) {
        let stored = mute::mute(pool, muter, target, mute::MuteOptions::default())
            .await
            .expect("the fixture mute stores");
        assert_eq!(
            stored.muted_id, target.id,
            "the row is the pair that was asked for"
        );
    }

    /// Mutes `target` for `muter` with the notifications left visible:
    /// Mastodon's `notifications: false`, the one mute that tells the two
    /// flags apart.
    async fn mute_timeline_only(pool: &PgPool, muter: &User, target: &User) {
        let options = mute::MuteOptions {
            notifications: false,
            expires_in: None,
        };
        let stored = mute::mute(pool, muter, target, options)
            .await
            .expect("the fixture mute stores");
        assert!(
            !stored.hide_notifications,
            "and the option is stored as the caller set it"
        );
    }

    /// Writes `target` into `muter`'s mute list with `expires_at` in the past:
    /// the lapsed row of a mute whose duration ran out, which the sweep has not
    /// deleted yet and every read must still treat as absent.
    async fn mute_lapsed(pool: &PgPool, muter: &User, target: &User) {
        let sql = "INSERT INTO mutes (id, muter_id, muted_id, expires_at)
                    VALUES ($1, $2, $3, now() - make_interval(secs => 60))";
        let written = sqlx::query(sql)
            .bind(new_id())
            .bind(muter.id)
            .bind(target.id)
            .execute(pool)
            .await
            .expect("the lapsed fixture row inserts");
        assert_eq!(written.rows_affected(), 1, "one row was written");
    }

    /// Lifts `muter`'s mute on `target`, reporting whether one was there.
    async fn unmute_pair(pool: &PgPool, muter: &User, target: &User) -> bool {
        let removed = mute::unmute(pool, muter, target)
            .await
            .expect("the fixture unmute runs");
        removed.is_some()
    }

    /// Both mutes flags ride the same batch: the viewer's mute of a target
    /// lights `muting`, the same mute with notifications hidden lights both, a
    /// lapsed mute lights neither, and a target nobody muted keeps both
    /// defaults — as does every account once its mute is lifted.
    #[tokio::test]
    async fn mutes_light_muting_and_muting_notifications() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let carol = local_user(&pool, "carol").await;
        let dave = local_user(&pool, "dave").await;
        mute_timeline_only(&pool, &alice, &bob).await;
        mute_pair(&pool, &alice, &carol).await;
        mute_lapsed(&pool, &alice, &dave).await;

        assert_eq!(
            mutes_of(&pool, &alice, &[&bob, &carol, &dave]).await,
            vec![(true, false), (true, true), (false, false)],
            "alice mutes bob's timeline only, mutes carol outright, and is
             past her lapsed mute of dave"
        );
        assert_eq!(
            mutes_of(&pool, &bob, &[&alice]).await,
            vec![(false, false)],
            "being muted does not mute anybody back"
        );

        let lifted = unmute_pair(&pool, &alice, &carol).await;
        assert!(lifted, "carol's mute was there to lift");
        let cleared = one_relationship(&pool, &alice, &carol).await;
        assert!(
            !cleared.muting.0 && !cleared.muting_notifications.0,
            "with nothing muted the pair falls back to the defaults"
        );
        assert_eq!(
            mutes_of(&pool, &alice, &[&bob, &carol, &dave]).await,
            vec![(true, false), (false, false), (false, false)],
            "and the rest of the batch is untouched by the lift"
        );
        assert!(
            !cleared.following.0 && cleared.showing_reblogs.0,
            "a mute says nothing about the follow flags"
        );
    }
}
