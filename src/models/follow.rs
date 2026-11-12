//! The `follows` table: which user follows which, and in which state.
//!
//! Per `docs/ARCHITECTURE.md` decision 6 a follow is one of the relational
//! facts that federate, so a row carries the `ActivityPub` id of the `Follow`
//! activity behind it: `{BASE_URL}/activities/{uuid}` for a follower this
//! instance owns, and whatever a remote instance sent for a mirrored one
//! (NULL until an activity has been seen). The state machine follows Pleroma
//! (`lib/pleroma/following_relationship.ex`, `follow/3` and `unfollow/2` in
//! `lib/pleroma/user.ex`): a follow is granted on the spot when this instance
//! owns an unlocked followee, and otherwise waits in
//! [`FollowState::Pending`] for that followee to [`accept`] or [`reject`] it.
//!
//! `users.followers_count` and `users.following_count` are kept in step with
//! the accepted rows through
//! [`update_counts`]: one step up when a
//! relationship becomes accepted, one step down when an accepted relationship
//! goes away. [`follow`], [`accept`], [`unfollow`], [`reject`],
//! [`remove_follower`] and [`delete_all_for_user_with_counts`] are the only
//! ways either happens, and each moves the counters exactly once per
//! transition.
//!
//! Signatures name the two ends of a relationship `follower` and `target`
//! (the word Pleroma's `subscribe(subscriber, target)` and Mastodon's
//! `target_account` use for the followed account), because `follower` next to
//! `followee` is one Clippy `similar_names` error; the row keeps the table's
//! own `follower_id` and `followee_id` names.
//!
//! The two per-follow toggles the row carries are read by
//! [`crate::models::relationship`] for the
//! [`Relationship`](crate::entities::relationship::Relationship) entity.
//! Pleroma's `subscribe` / `unsubscribe` write `notify` through
//! [`set_notify`] (a granted follow alone answers it); Mastodon's
//! `follow?reblogs` / `follow?notify` write either column through
//! [`set_toggles`], which is the pair-wide counterpart of the same two
//! flags and the only writer of `show_reblogs`.

use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgConnection, PgPool, Postgres, QueryBuilder, postgres::PgRow};
use uuid::Uuid;

use crate::{
    error::{AppError, AppResult},
    models::{
        block, endorsement,
        user::{CountDelta, User, update_counts},
    },
    pagination::PageParams,
    util::id::new_id,
};

/// 422 message [`follow`] answers when an account asks to follow itself,
/// Pleroma's wording for that case.
const SELF_FOLLOW: &str = "Can not follow yourself";

/// [`follow`]'s write when the pair has no row yet. `ON CONFLICT DO NOTHING`
/// is the race guard: a concurrent follow of the same pair wins, this
/// statement writes nothing and returns no row, and the winner's row is read
/// back instead (which is also why it must not move the counters again).
const INSERT_SQL: &str = "INSERT INTO follows (id, follower_id, followee_id, state, ap_id)
     VALUES ($1, $2, $3, $4, $5)
ON CONFLICT (follower_id, followee_id) DO NOTHING
RETURNING *";

/// [`follow`]'s write when the pair has a `rejected` row: reopen it at the
/// state this follow implies, keeping the stored activity id when this
/// follower brings no local one (a remote follower's own `Follow` activity id
/// stands until a new one is minted).
const REOPEN_SQL: &str = "UPDATE follows
      SET state = $2,
          ap_id = COALESCE($3, ap_id),
          updated_at = now()
    WHERE id = $1 AND state = 'rejected'
  RETURNING *";

/// [`unfollow`]: only an open relationship can be taken back, and the removed
/// row is what decides whether the counters step down.
const UNFOLLOW_SQL: &str = "DELETE FROM follows
      WHERE follower_id = $1 AND followee_id = $2
        AND state IN ('pending', 'accepted')
  RETURNING *";

/// [`remove_follower`]: the follower is gone, and only an *accepted* row ever
/// counted, so the removed row is what decides whether the counters step down
/// and anything else in the table stays exactly as it was.
const REMOVE_FOLLOWER_SQL: &str = "DELETE FROM follows
      WHERE follower_id = $1 AND followee_id = $2 AND state = 'accepted'
  RETURNING *";

/// [`accept`]: every not-yet-accepted state becomes accepted. A `rejected`
/// row never counted either, so both transitions are a step up, while an
/// already accepted row matches nothing here and is answered idempotently.
const ACCEPT_SQL: &str = "UPDATE follows
      SET state = 'accepted',
          updated_at = now()
    WHERE id = $1 AND state <> 'accepted'
  RETURNING *";

/// [`sever_between`]: the open relationships of a pair in *both* directions,
/// which is what a block severs — the same `pending`/`accepted` states
/// [`UNFOLLOW_SQL`] takes back (a mirrored `rejected` row is nobody's active
/// relationship and stays where it is), stated once for the ordered pair and
/// once for its reverse. The removed rows come back, because only an accepted
/// one ever counted and the caller fixes the counters from them.
const SEVER_BETWEEN_SQL: &str = "DELETE FROM follows
      WHERE state IN ('pending', 'accepted')
        AND ((follower_id = $1 AND followee_id = $2)
          OR (follower_id = $2 AND followee_id = $1))
   RETURNING *";

/// [`unfollow`]: the memberships the unfollowing account kept of the account
/// they just stopped following. An account in someone's list is an account
/// they follow (the rule [`crate::api::lists`] enforces on the way in), so
/// taking the follow back takes those rows with it: the delete runs over the
/// list's *owner*, which is why the follower's rows in other people's lists
/// are untouched.
const UNFOLLOW_LIST_MEMBERSHIPS_SQL: &str = "DELETE FROM list_members AS lm
     USING lists AS l
     WHERE l.id = lm.list_id AND l.user_id = $1 AND lm.user_id = $2";

/// [`reject`]: the request is gone, and the removed row comes back.
const REJECT_SQL: &str = "DELETE FROM follows WHERE id = $1 RETURNING *";

/// [`set_notify`]: the `notify` toggle, written only onto an *accepted*
/// relationship — Pleroma's `subscribe`/`unsubscribe` and Mastodon's
/// `follow?notify` all act on a granted follow — and refreshing
/// `updated_at` the way [`ACCEPT_SQL`] does.
const SET_NOTIFY_SQL: &str = "UPDATE follows
       SET notify = $3,
           updated_at = now()
     WHERE follower_id = $1 AND followee_id = $2 AND state = 'accepted'
   RETURNING *";

/// [`set_toggles`]: the two per-follow toggles, written onto an *open*
/// relationship and only where the request named them — `COALESCE` keeps the
/// stored flag when its parameter is `NULL`, so a client that sends `reblogs`
/// alone does not reset `notify`. The states are the open ones
/// [`UNFOLLOW_SQL`] takes back: a `pending` request the follow itself just
/// created carries the toggles the client asked for, and a mirrored
/// `rejected` row is nobody's active relationship.
const SET_TOGGLES_SQL: &str = "UPDATE follows
       SET show_reblogs = COALESCE($3, show_reblogs),
           notify = COALESCE($4, notify),
           updated_at = now()
     WHERE follower_id = $1 AND followee_id = $2
       AND state IN ('pending', 'accepted')
   RETURNING *";

/// [`get`]: the one relationship of an ordered pair, whatever its state.
const GET_SQL: &str = "SELECT * FROM follows WHERE follower_id = $1 AND followee_id = $2";

/// [`by_id`]: the one relationship behind a primary key.
const BY_ID_SQL: &str = "SELECT * FROM follows WHERE id = $1";

/// [`following_ids`]: the accepted ones, oldest relationship first (`UUIDv7`
/// text sorts in creation order, decision 4).
const FOLLOWING_IDS_SQL: &str = "SELECT followee_id FROM follows
     WHERE follower_id = $1 AND state = 'accepted'
  ORDER BY id";

/// [`has_accepted_follow`] (and so [`is_following`]): whether the pair has
/// an accepted relationship.
const IS_FOLLOWING_SQL: &str = "SELECT EXISTS (
    SELECT 1 FROM follows
     WHERE follower_id = $1 AND followee_id = $2 AND state = 'accepted'
)";

/// The incoming side of [`followers_page`]: the users whose follow of the
/// bound account was accepted, each entry led by the `follow_id` of the
/// relationship row behind it — the id the page keys on. The text stops
/// where its one placeholder belongs, because that is where [`page_rows`]
/// pushes its bind and the builder is what numbers it `$1`; the keyset it
/// then appends runs on the relationship id `f.id`.
const FOLLOWERS_PAGE_SQL: &str = "SELECT f.id AS follow_id, u.* FROM follows f
    JOIN users u ON u.id = f.follower_id
     WHERE f.state = 'accepted' AND f.followee_id = ";

/// The outgoing side of [`following_page`]: the users the bound account
/// follows, same shape as [`FOLLOWERS_PAGE_SQL`].
const FOLLOWING_PAGE_SQL: &str = "SELECT f.id AS follow_id, u.* FROM follows f
    JOIN users u ON u.id = f.followee_id
     WHERE f.state = 'accepted' AND f.follower_id = ";

/// The incoming side of [`pending_requests_for`]: the users still waiting for
/// the bound account to answer their follow request, each entry led by the
/// `follow_id` of the request row behind it — the id the page keys on, as in
/// [`FOLLOWERS_PAGE_SQL`]. The text stops where its one placeholder belongs,
/// because that is where [`page_rows`] pushes its bind and the builder is what
/// numbers it `$1`; the keyset it then appends runs on the relationship id
/// `f.id`.
const PENDING_REQUESTS_SQL: &str = "SELECT f.id AS follow_id, u.* FROM follows f
    JOIN users u ON u.id = f.follower_id
     WHERE f.state = 'pending' AND f.followee_id = ";

/// The outgoing side of [`outgoing_requests_from`]: the users whose follow of
/// the bound account is still waiting on their answer — the accounts a pending
/// row names as its `followee`, the mirror of [`PENDING_REQUESTS_SQL`] — each
/// entry led by the `follow_id` of the request row behind it, the id the page
/// keys on. The text stops where its one placeholder belongs, because that is
/// where [`page_rows`] pushes its bind and the builder is what numbers it
/// `$1`; the keyset it then appends runs on the relationship id `f.id`.
const PENDING_OUTGOING_SQL: &str = "SELECT f.id AS follow_id, u.* FROM follows f
    JOIN users u ON u.id = f.followee_id
     WHERE f.state = 'pending' AND f.follower_id = ";

/// [`pending_count`]: the same queue [`pending_requests_for`] pages through,
/// counted rather than listed. The `::int` cast is what makes `count(*)`'s
/// `bigint` arrive as the `i32` this layer's counters all are.
const PENDING_COUNT_SQL: &str = "SELECT count(*)::int FROM follows
     WHERE followee_id = $1 AND state = 'pending'";

/// [`move_followers_of`]: the followers whose follow of the moving account
/// an account move may carry over — accepted relationships held by *local*
/// accounts that still allow their following list to move (Pleroma's
/// `FollowingRelationship.move_following` filters `f.local == true` and
/// `f.allow_following_move == true`). Oldest relationship first.
const MOVE_FOLLOWERS_SQL: &str = "SELECT u.* FROM follows f
     JOIN users u ON u.id = f.follower_id
      WHERE f.followee_id = $1 AND f.state = 'accepted'
        AND u.local = true AND u.allow_following_move = true
   ORDER BY f.id";

/// [`remote_followers`]: accepted relationships held by *remote*, active
/// mirrors of the bound account — the followers collection exactly as the
/// sender-side fan-out needs it (Pleroma's `User.get_external_followers`,
/// which its publisher expands into follower inboxes). Local followers are
/// not in it; they receive the activity through their timelines, never
/// through an HTTP delivery, and a deactivated mirror's inbox is nobody's
/// delivery target either. Oldest relationship first.
const REMOTE_FOLLOWERS_SQL: &str = "SELECT u.* FROM follows f
     JOIN users u ON u.id = f.follower_id
      WHERE f.followee_id = $1 AND f.state = 'accepted'
        AND u.local = false AND u.is_active = true
   ORDER BY f.id";

/// [`subscribers`]: the *local*, active followers whose accepted follow of
/// the bound account has its `notify` toggle on — Pleroma's
/// `User.subscriber_users` (`lib/pleroma/user.ex`), the list its
/// `get_notified_subscribers` walks to tell subscribers of every new post.
/// Remote subscribers are deliberately absent: they are told by federation,
/// not by a row (the rule [`crate::services::notify`] states). Oldest
/// relationship first.
const SUBSCRIBERS_SQL: &str = "SELECT u.* FROM follows f
     JOIN users u ON u.id = f.follower_id
      WHERE f.followee_id = $1 AND f.state = 'accepted'
        AND f.notify = true AND u.local = true AND u.is_active = true
   ORDER BY f.id";

/// [`familiar_followers`]: the accounts that follow `target` (`own`, the
/// target's own follower edges) and are themselves followed by the viewer
/// (`theirs`, the viewer's outgoing edges), accepted on both ends. Active
/// only, and without the ones who hide their follows — Pleroma's
/// `get_familiar_followers_query` adds `not u.hide_follows` to its friends
/// subquery for exactly that. `id` excludes the target and the viewer:
/// neither is a follower of the other in this sense. Newest follow of the
/// target first, `LIMIT $3` at a time.
const FAMILIAR_FOLLOWERS_SQL: &str = "SELECT u.* FROM users u
     JOIN follows own ON own.followee_id = $1 AND own.follower_id = u.id
     JOIN follows theirs ON theirs.follower_id = $2 AND theirs.followee_id = u.id
      WHERE own.state = 'accepted' AND theirs.state = 'accepted'
        AND u.is_active AND NOT u.hide_follows
        AND u.id <> $1 AND u.id <> $2
   ORDER BY own.id DESC
      LIMIT $3";

/// Lifecycle state of a [`Follow`] row, stored in the `state` text column as
/// its lowercase variant name (`pending`, `accepted` or `rejected`, which is
/// exactly what the column's `CHECK` constraint allows).
#[derive(Clone, Copy, Debug, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "text", rename_all = "lowercase")]
pub enum FollowState {
    /// Awaiting an answer: the followee is locked or remote and has not
    /// granted the follow yet.
    Pending,
    /// Granted: the relationship counts, feeds timelines and moves the
    /// denormalised counters.
    Accepted,
    /// Answered with a `Reject`. [`reject`] deletes the row rather than
    /// storing this state, so a `rejected` row only appears when a remote
    /// instance's state was mirrored; [`follow`] reopens one.
    Rejected,
}

/// One row of `follows`: the relationship between two users (decision 6).
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct Follow {
    /// `UUIDv7` primary key, minted by [`follow`] (decision 4).
    pub id: Uuid,
    /// The user who follows.
    pub follower_id: Uuid,
    /// The user who is followed.
    pub followee_id: Uuid,
    /// Whether the follow has been granted, is waiting, or was rejected.
    pub state: FollowState,
    /// `ActivityPub` id of the `Follow` activity behind this relationship: a
    /// `{BASE_URL}/activities/{uuid}` id for a local follower, the remote
    /// id for a mirrored one, `None` until an activity exists to record.
    pub ap_id: Option<String>,
    /// Whether the followee keeps seeing this follower's reblogs (Mastodon's
    /// `reblogs` toggle).
    pub show_reblogs: bool,
    /// Whether the followee is notified of this follower's posts (Mastodon's
    /// `notify` toggle).
    pub notify: bool,
    /// When the row was inserted.
    pub inserted_at: DateTime<Utc>,
    /// When the row last changed; refreshed by every update here.
    pub updated_at: DateTime<Utc>,
}

/// One entry of a paged relationship list: the [`User`] on the other end of
/// the follow plus [`Follow::id`], the id of the row that put them on the
/// page.
///
/// The relationship id — not the user's — is what [`followers_page`]
/// and [`following_page`] keyset on (decision 4), so the account listings'
/// `max_id`/`min_id` cursors and `Link` header carry follow row ids.
#[derive(Debug, sqlx::FromRow)]
pub struct FollowPageEntry {
    /// Primary key of the [`Follow`] row behind this entry.
    pub follow_id: Uuid,
    /// The user on the other end of the relationship.
    #[sqlx(flatten)]
    pub user: User,
}

/// The two per-follow toggles a client may name, as [`set_toggles`] stores
/// them.
///
/// Each side is optional in the sense Mastodon's `follow` endpoint means: a
/// parameter the request carried is written, one it left out keeps whatever the
/// row already holds — which is why these are `Option`s rather than the
/// [`Follow`] row's plain `bool`s.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FollowToggles {
    /// Mastodon's `reblogs`, the [`Follow::show_reblogs`] column; `None`
    /// leaves the stored flag alone.
    pub show_reblogs: Option<bool>,
    /// Mastodon's `notify`, the [`Follow::notify`] column; `None` leaves the
    /// stored flag alone.
    pub notify: Option<bool>,
}

impl FollowToggles {
    /// Whether the request named neither toggle, the case in which
    /// [`set_toggles`] has nothing to write and its caller skips the
    /// statement.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.show_reblogs.is_none() && self.notify.is_none()
    }
}

/// Records that `follower` follows `target` — the followed account, the
/// row's [`Follow::followee_id`] — and returns the relationship.
///
/// The state is [`FollowState::Accepted`] right away when this instance owns
/// an unlocked target — the answer is ours to give — and
/// [`FollowState::Pending`] when the target is locked or remote, where
/// acceptance arrives later through an `Accept` activity ([`accept`]). A
/// local follower's row gets a fresh `ActivityPub` activity id (decision 6);
/// `base_url` is the instance origin, i.e. [`Config::base_url`], which the
/// model layer is handed rather than reading configuration itself.
///
/// Following is idempotent: a pair that already has a `pending` or `accepted`
/// row keeps it as it is and nothing is written, while a `rejected` row is
/// reopened at the state this follow implies. The two denormalised counters
/// step up only when this call turns the relationship accepted. The per-follow
/// toggles are no part of this: a fresh row carries the column defaults, and
/// the endpoint that takes `reblogs` / `notify` writes what it was given
/// through [`set_toggles`] on top of the row this returns.
///
/// [`Config::base_url`]: crate::config::Config::base_url
///
/// # Errors
///
/// [`AppError::Unprocessable`] with `"Can not follow yourself"` when the two
/// users are the same account, and [`AppError::Forbidden`] with Pleroma's
/// `deny_follow_blocked` wording when either has blocked the other: `"Could
/// not follow user: <nick> is blocking you."` when the target blocks the
/// follower, `"Could not follow user: You are blocking <nick>."` when the
/// follower blocks the target (the module's `blocked_between` hook and its
/// `blocked_error`, both reading the `blocks` table through
/// [`block::blocked_between`] and [`block::is_blocking`]). Any database
/// failure (connection, constraint, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn follow(
    pool: &PgPool,
    base_url: &str,
    follower: &User,
    target: &User,
) -> AppResult<Follow> {
    if follower.id == target.id {
        return Err(AppError::Unprocessable(SELF_FOLLOW.to_owned()));
    }
    if blocked_between(pool, follower, target).await? {
        return Err(blocked_error(pool, follower, target).await?);
    }
    let state = initial_state(target);
    let ap_id = local_activity_id(base_url, follower);
    match get(pool, follower.id, target.id).await? {
        // Already open: this follow changes nothing, so the stored row is
        // the answer and no counter moves.
        Some(follow) if follow.state != FollowState::Rejected => Ok(follow),
        // A rejected request is revived at the state of this follow.
        Some(previous) => reopen(pool, previous.id, state, ap_id).await,
        // Nothing yet.
        None => insert_follow(pool, follower.id, target.id, state, ap_id).await,
    }
}

/// Removes `follower`'s follow of `target`, reporting whether an open
/// relationship was there to remove.
///
/// A `pending` or `accepted` row is deleted; a `rejected` row (see
/// [`FollowState::Rejected`]) and an unknown pair are answered `false` and
/// change nothing. Deleting an accepted relationship steps both denormalised
/// counters down again. An endorsement of `target` by `follower` goes with
/// the relationship: an endorsement is a pinned *follow*, and unfollowing
/// clears it (the same pair of facts Pleroma's `unfollow` drops together). So
/// does every membership of `target` in `follower`'s own lists: a list member
/// is a followed account, so unfollowing ends the membership too
/// — the memberships of other owners' lists are nobody else's to touch, and
/// stay.
///
/// # Errors
///
/// Any database failure (connection, constraint, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn unfollow(pool: &PgPool, follower: &User, target: &User) -> AppResult<bool> {
    let removed = sqlx::query_as::<_, Follow>(UNFOLLOW_SQL)
        .bind(follower.id)
        .bind(target.id)
        .fetch_optional(pool)
        .await?;
    let Some(follow) = removed else {
        return Ok(false);
    };
    if follow.state == FollowState::Accepted {
        step_counters(pool, &follow, -1).await?;
    }
    endorsement::unendorse(pool, follower.id, target.id).await?;
    sqlx::query(UNFOLLOW_LIST_MEMBERSHIPS_SQL)
        .bind(follower.id)
        .bind(target.id)
        .execute(pool)
        .await
        .map(drop)?;
    Ok(true)
}

/// The `DELETE` behind [`delete_all_for_user`]: every relationship naming
/// the account, either side.
const DELETE_ALL_FOR_USER_SQL: &str =
    "DELETE FROM follows WHERE follower_id = $1 OR followee_id = $1";

/// Removes every follow relationship that names user `user_id`: the
/// account-deletion job's follow step.
///
/// Both sides go — the rows where the account is the follower and the
/// rows where it is the followee, in any state — Pleroma's per-follower
/// and per-followed `unfollow` loop of `User.perform(:delete)` cut down to
/// the one statement the departing account needs. The denormalised
/// counters of the peers are left alone: Pleroma's purge deactivates the
/// account first, so no listing still reads them, and one clamped step per
/// row would cost a query per relationship to fix a number nobody shows.
///
/// Returns how many rows went.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn delete_all_for_user(pool: &PgPool, user_id: Uuid) -> AppResult<u64> {
    let deleted = sqlx::query(DELETE_ALL_FOR_USER_SQL)
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(deleted.rows_affected())
}

/// [`delete_all_for_user_with_counts`]: every *accepted* relationship naming
/// the account, together with the counters those rows fed, in one statement
/// — a departed account has however many followers it had, so the steps are
/// aggregated per peer instead of one [`update_counts`] call per row.
/// `GREATEST(0, …)` is the same clamp [`update_counts`] applies, and the
/// departing account is its own peer here: it sits at the other end of every
/// row it leaves, so both of its counters step down with them. The statement
/// answers how many relationships went; [`DELETE_REST_FOR_USER_SQL`] takes
/// the ones that never counted.
const DELETE_ACCEPTED_AND_UNCOUNT_SQL: &str = "WITH removed AS (
         DELETE FROM follows
          WHERE (follower_id = $1 OR followee_id = $1) AND state = 'accepted'
       RETURNING follower_id, followee_id
     ), deltas AS (
         SELECT follower_id AS user_id, 0 AS followers, -1 AS following FROM removed
         UNION ALL
         SELECT followee_id AS user_id, -1 AS followers, 0 AS following FROM removed
     ), uncounted AS (
         UPDATE users AS u
            SET followers_count = GREATEST(0, u.followers_count + d.followers),
                following_count = GREATEST(0, u.following_count + d.following),
                updated_at = now()
          FROM (SELECT user_id,
                       SUM(followers)::int AS followers,
                       SUM(following)::int AS following
                  FROM deltas
              GROUP BY user_id) AS d
         WHERE u.id = d.user_id
     )
     SELECT count(*) FROM removed";

/// [`delete_all_for_user_with_counts`]'s second statement: the rows that
/// never fed a counter — a `pending` request and a mirrored `rejected` one,
/// the other two states [`delete_all_for_user`] sweeps up beside the
/// accepted rows — so nothing here owes a step.
const DELETE_REST_FOR_USER_SQL: &str =
    "DELETE FROM follows WHERE (follower_id = $1 OR followee_id = $1) AND state <> 'accepted'";

/// Removes every follow relationship that names user `user_id` *and* steps
/// the denormalised counters the accepted ones fed.
///
/// This is the relationship removal of a remote account's deletion
/// ([`users.delete_remote`](crate::jobs::deletion::KIND_DELETE_REMOTE)),
/// whose peers stay live, so their follower and following counts have to
/// describe what is left of each of them.
///
/// [`delete_all_for_user`] is the same sweep for a *local* deletion, which
/// leaves the numbers alone because the account behind them is purged. Here
/// every accepted row is worth one step, taken in the same transaction as
/// the delete so a crash cannot leave a counted relationship without its row
/// or a row without its counters — the pairing [`remove_follower`] and
/// [`sever_between`] keep for the relationships they take. Both directions
/// go: the account's own follows and the follows *of* it, which is how the
/// local followers of a departed remote account stop following it. A
/// `pending` request and a mirrored `rejected` row go too, moving nothing.
///
/// Returns how many rows went, both states together.
///
/// # Errors
///
/// Any database failure (connection, constraint, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn delete_all_for_user_with_counts(pool: &PgPool, user_id: Uuid) -> AppResult<u64> {
    let mut tx = pool.begin().await?;
    let accepted = sqlx::query_scalar::<_, i64>(DELETE_ACCEPTED_AND_UNCOUNT_SQL)
        .bind(user_id)
        .fetch_one(&mut *tx)
        .await?;
    let rest = sqlx::query(DELETE_REST_FOR_USER_SQL)
        .bind(user_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    tx.commit().await?;
    // `count(*)` over deleted rows is never negative, so this cannot fail.
    let accepted = u64::try_from(accepted).unwrap_or_default();
    Ok(accepted + rest)
}

/// Removes `follower_id`'s accepted follow of `user_id` — the followed
/// account, the row's [`Follow::followee_id`] — and returns the removed
/// relationship, `None` when the account had no such follower to remove.
///
/// The followee's side of [`unfollow`] (Pleroma's `User.remove_follower`,
/// the write behind `remove_from_followers`): the same accepted relationship
/// taken back, but asked for by the account it points at instead of the one
/// that granted it. Only a follower is ever removed: a `pending` request
/// waits for its owner's own answer ([`accept`] / [`reject`], the
/// `/api/v1/follow_requests` routes), a `rejected` row (see
/// [`FollowState::Rejected`]) and an unknown pair are the `None` answer, and
/// none of them changes anything. Removing the row steps both denormalised
/// counters down, in the same transaction as the delete: the row and the
/// counts it fed always move together or not at all.
///
/// # Errors
///
/// Any database failure (connection, constraint, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn remove_follower(
    pool: &PgPool,
    user_id: Uuid,
    follower_id: Uuid,
) -> AppResult<Option<Follow>> {
    let mut tx = pool.begin().await?;
    let removed = sqlx::query_as::<_, Follow>(REMOVE_FOLLOWER_SQL)
        .bind(follower_id)
        .bind(user_id)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(follow) = removed else {
        // Nothing was a follower: nothing was written, and the empty
        // transaction ends with the connection (`chat::delete_message_for`
        // does the same).
        return Ok(None);
    };
    uncount_removed_follower(&mut tx, &follow).await?;
    tx.commit().await?;
    Ok(Some(follow))
}

/// Severs every open relationship between `first` and `second` — in *both*
/// directions — on the open transaction `conn`, and returns nothing but the
/// outcome of the write.
///
/// This is the follow-side half of a block: [`block`] owns the transaction
/// and calls this inside it, so the block row and the relationships it
/// sever land (or vanish) together — Pleroma's `User.block` unfollowing the
/// pair and withdrawing both directions' pending requests before it stores
/// the block. Every removed row that had been *accepted* steps both
/// denormalised counters down through the module's
/// `uncount_removed_follower`, a `pending` request moves nothing, and a
/// mirrored `rejected` row is left exactly where it was, because it is
/// nobody's active relationship (see [`unfollow`]'s same rule).
///
/// # Errors
///
/// Any database failure (connection, constraint, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn sever_between(conn: &mut PgConnection, first: Uuid, second: Uuid) -> AppResult<()> {
    let removed = sqlx::query_as::<_, Follow>(SEVER_BETWEEN_SQL)
        .bind(first)
        .bind(second)
        .fetch_all(&mut *conn)
        .await?;
    for follow in removed
        .iter()
        .filter(|row| row.state == FollowState::Accepted)
    {
        uncount_removed_follower(conn, follow).await?;
    }
    Ok(())
}

/// Grants a follow request and returns the accepted relationship.
///
/// Any not-yet-accepted row becomes [`FollowState::Accepted`] and steps the
/// counters up; calling it again on an accepted row returns that row and
/// moves nothing, because only the transition into accepted counts.
///
/// # Errors
///
/// [`AppError::NotFound`] when no follow has this id (anymore: a concurrent
/// unfollow may have removed it); any other database failure arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn accept(pool: &PgPool, follow_id: Uuid) -> AppResult<Follow> {
    let granted = sqlx::query_as::<_, Follow>(ACCEPT_SQL)
        .bind(follow_id)
        .fetch_optional(pool)
        .await?;
    match granted {
        Some(follow) => {
            step_counters(pool, &follow, 1).await?;
            Ok(follow)
        }
        // Nothing to change: the row was accepted already, or it is gone.
        None => required(by_id(pool, follow_id).await?),
    }
}

/// Withdraws a follow request: the row is deleted and the removed
/// relationship returned.
///
/// A `pending` request leaves no trace and moves no counters, since it never
/// counted; removing an already `accepted` relationship (Pleroma's
/// `follow_reject`, which is an unfollow) steps them down like [`unfollow`]
/// does.
///
/// # Errors
///
/// [`AppError::NotFound`] when no follow has this id; any other database
/// failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn reject(pool: &PgPool, follow_id: Uuid) -> AppResult<Follow> {
    let removed = sqlx::query_as::<_, Follow>(REJECT_SQL)
        .bind(follow_id)
        .fetch_optional(pool)
        .await?;
    let follow = required(removed)?;
    if follow.state == FollowState::Accepted {
        step_counters(pool, &follow, -1).await?;
    }
    Ok(follow)
}

/// Sets the `notify` toggle of the accepted relationship between
/// `follower_id` and `target_id` and returns the updated row.
///
/// The one column the notification flags of
/// [`Relationship`](crate::entities::relationship::Relationship) are read
/// from: Pleroma's `POST /api/v1/pleroma/accounts/{id}/subscribe` and
/// `…/unsubscribe` (`User.subscribe` / `User.unsubscribe`) write it here,
/// while Mastodon's `follow?notify` arrives through [`set_toggles`], the
/// pair-wide write of the same flag plus `show_reblogs`.
///
/// Only a *granted* follow can be subscribed to: a `pending` request nobody
/// answered and a pair with no row at all are answered `None` with nothing
/// written, which is the fact the endpoints turn into their refusal.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn set_notify(
    pool: &PgPool,
    follower_id: Uuid,
    target_id: Uuid,
    notify: bool,
) -> AppResult<Option<Follow>> {
    let updated = sqlx::query_as::<_, Follow>(SET_NOTIFY_SQL)
        .bind(follower_id)
        .bind(target_id)
        .bind(notify)
        .fetch_optional(pool)
        .await?;
    Ok(updated)
}

/// Writes the toggles `toggles` names onto the pair's open relationship and
/// returns the updated row, `None` when the pair has none to write on.
///
/// The write behind Mastodon's `POST /api/v1/accounts/{id}/follow`: the two
/// parameters that endpoint takes ride along with the follow it creates and
/// are re-sent on a follow that already exists, where they update the stored
/// flags. Only the parameters a request actually carried are written; an
/// absent one keeps what the row holds, which is what makes [`FollowToggles`]
/// a pair of `Option`s. The row and its id are otherwise untouched: this
/// changes neither the state nor the denormalised counters (a toggle is not a
/// follow).
///
/// Both open states answer the write — the `pending` request a follow of a
/// locked or remote account just created carries the toggles its client asked
/// for, which is what the row will mean when it is granted — while a mirrored
/// `rejected` row (see [`FollowState::Rejected`]) and an unknown pair are
/// answered `None` with nothing written.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn set_toggles(
    pool: &PgPool,
    follower_id: Uuid,
    target_id: Uuid,
    toggles: FollowToggles,
) -> AppResult<Option<Follow>> {
    let updated = sqlx::query_as::<_, Follow>(SET_TOGGLES_SQL)
        .bind(follower_id)
        .bind(target_id)
        .bind(toggles.show_reblogs)
        .bind(toggles.notify)
        .fetch_optional(pool)
        .await?;
    Ok(updated)
}

/// The relationship between the ordered pair (`follower_id`,
/// `target_id`), whatever its state, `None` when the pair has none.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get(pool: &PgPool, follower_id: Uuid, target_id: Uuid) -> AppResult<Option<Follow>> {
    let follow = sqlx::query_as::<_, Follow>(GET_SQL)
        .bind(follower_id)
        .bind(target_id)
        .fetch_optional(pool)
        .await?;
    Ok(follow)
}

/// The ids of the users `follower_id` follows, accepted relationships only,
/// oldest follow first (decision 4).
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn following_ids(pool: &PgPool, follower_id: Uuid) -> AppResult<Vec<Uuid>> {
    let rows = sqlx::query_as::<_, (Uuid,)>(FOLLOWING_IDS_SQL)
        .bind(follower_id)
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

/// Whether `follower` has an accepted follow of `target`.
///
/// A pending request is not following, and a rejected or absent one neither.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn is_following(pool: &PgPool, follower: &User, target: &User) -> AppResult<bool> {
    has_accepted_follow(pool, follower.id, target.id).await
}

/// Whether the account `follower_id` has an accepted follow of
/// `target_id` — [`is_following`] keyed on ids, for callers that hold
/// only the primary keys.
///
/// The notification path reads it that way: it knows the recipient and
/// the actor ids without loading either row.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn has_accepted_follow(
    pool: &PgPool,
    follower_id: Uuid,
    target_id: Uuid,
) -> AppResult<bool> {
    let row = sqlx::query_as::<_, (bool,)>(IS_FOLLOWING_SQL)
        .bind(follower_id)
        .bind(target_id)
        .fetch_one(pool)
        .await?;
    Ok(row.0)
}

/// The users who follow `user_id`, accepted relationships only, newest
/// relationship first and `page`'s `limit` at a time, each entry carrying
/// the [`Follow`] row's id next to the [`User`] (see [`FollowPageEntry`]).
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn followers_page(
    pool: &PgPool,
    user_id: Uuid,
    page: PageParams,
) -> AppResult<Vec<FollowPageEntry>> {
    page_rows(pool, FOLLOWERS_PAGE_SQL, user_id, &page).await
}

/// The users `user_id` follows, accepted relationships only, newest
/// relationship first and `page`'s `limit` at a time, each entry carrying
/// the [`Follow`] row's id next to the [`User`] (see [`FollowPageEntry`]).
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn following_page(
    pool: &PgPool,
    user_id: Uuid,
    page: PageParams,
) -> AppResult<Vec<FollowPageEntry>> {
    page_rows(pool, FOLLOWING_PAGE_SQL, user_id, &page).await
}

/// The users waiting for `target_id` — the followed account, the row's
/// [`Follow::followee_id`] — to answer their follow request, newest request
/// first and `page`'s `limit` at a time.
///
/// Like [`followers_page`], each entry carries the [`Follow`] row's id next
/// to the [`User`] (see [`FollowPageEntry`]): the relationship id — not the
/// requester's — is what the page keysets on (decision 4), so the
/// follow-request endpoint's `max_id`/`min_id` cursors and `Link` header
/// carry these ids, as Pleroma's do.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn pending_requests_for(
    pool: &PgPool,
    target_id: Uuid,
    page: PageParams,
) -> AppResult<Vec<FollowPageEntry>> {
    page_rows(pool, PENDING_REQUESTS_SQL, target_id, &page).await
}

/// The users `follower_id` has asked to follow and who have not answered: the
/// caller's own outgoing requests, newest request first and `page`'s `limit`
/// at a time.
///
/// Pleroma's `get_outgoing_follow_requests_query`, the list behind
/// `GET /api/v1/pleroma/outgoing_follow_requests`. It is the mirror of
/// [`pending_requests_for`], the other end of the same rows: here the bound
/// account is [`Follow::follower_id`] rather than [`Follow::followee_id`], so
/// these are requests to withdraw, not to answer. Like it, each entry carries
/// the [`Follow`] row's id next to the [`User`] (see [`FollowPageEntry`]) and
/// keysets on that id (decision 4), so the endpoint's `max_id`/`min_id`
/// cursors and `Link` header carry follow row ids.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn outgoing_requests_from(
    pool: &PgPool,
    follower_id: Uuid,
    page: PageParams,
) -> AppResult<Vec<FollowPageEntry>> {
    page_rows(pool, PENDING_OUTGOING_SQL, follower_id, &page).await
}

/// How many follow requests are waiting for `user_id` to answer them: the
/// `state = 'pending'` rows that name the account as their followee, which is
/// exactly what [`pending_requests_for`] pages through.
///
/// The account's own outgoing requests are not counted (a request the account
/// sent is someone else's to answer), and neither is anything already
/// accepted or rejected: the answer is the size of the queue the account has
/// still to work through, the `source.follow_requests_count` the credentials
/// endpoints render.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn pending_count(pool: &PgPool, user_id: Uuid) -> AppResult<i32> {
    let row = sqlx::query_as::<_, (i32,)>(PENDING_COUNT_SQL)
        .bind(user_id)
        .fetch_one(pool)
        .await?;
    Ok(row.0)
}

/// The local followers of `user_id` whose follow an account move carries
/// over.
///
/// Accepted relationships held by accounts this instance owns that still
/// allow their following list to move
/// ([`allow_following_move`](crate::models::user::User::allow_following_move)),
/// oldest relationship first. This is the candidate set Pleroma's
/// `FollowingRelationship.move_following` walks when `move_account`
/// transfers followers to the move target; the endpoint that consumes it
/// ([`crate::api::pleroma_account_settings`]) does the re-following.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn move_followers_of(pool: &PgPool, user_id: Uuid) -> AppResult<Vec<User>> {
    let rows = sqlx::query_as::<_, User>(MOVE_FOLLOWERS_SQL)
        .bind(user_id)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// The remote users with an accepted follow of `user_id`, active mirrors
/// only, oldest relationship first.
///
/// This is the followers collection read from the *sending* side: when this
/// instance publishes an activity addressed to `user_id`'s followers URL,
/// these are the accounts standing behind it, and their inboxes are what
/// the fan-out addresses
/// ([`crate::federation::publisher`] expands the collection through this
/// query). Pleroma's `User.get_external_followers` is the reference; local
/// followers are deliberately absent — they get the activity through their
/// timelines, not a delivery — and so are deactivated mirrors, which can
/// receive nothing.
///
/// # Errors
///
/// Any database failure (connection, decoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn remote_followers(pool: &PgPool, user_id: Uuid) -> AppResult<Vec<User>> {
    let rows = sqlx::query_as::<_, User>(REMOTE_FOLLOWERS_SQL)
        .bind(user_id)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// The local, active users holding a *subscribed* accepted follow of
/// `user_id`.
///
/// These are the accounts [`crate::services::notify`] tells `status`
/// notifications of every post this account makes — Pleroma's
/// `User.subscriber_users`, which its `get_notified_subscribers` reads the
/// same way: `notify` true on a granted follow.
///
/// A pending request with its toggle on is not a subscriber yet, a plain
/// accepted follow is nobody's subscriber, and a deactivated or remote
/// account cannot be told from here.
///
/// # Errors
///
/// Any database failure (connection, decoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn subscribers(pool: &PgPool, user_id: Uuid) -> AppResult<Vec<User>> {
    let rows = sqlx::query_as::<_, User>(SUBSCRIBERS_SQL)
        .bind(user_id)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// The accounts `viewer_id` follows that also follow `target_id` — the
/// intersection the `GET /api/v1/accounts/familiar_followers` batch answers,
/// newest follow of the target first, at most `limit` of them.
///
/// Pleroma's `User.get_familiar_followers(target, current_user)`
/// (`lib/pleroma/user.ex`) is the reference, including its two exclusions
/// built into the statement: the target and the viewer themselves are never
/// familiar followers of the target, and an account that hides its follows
/// ([`hide_follows`](crate::models::user::User::hide_follows)) never shows
/// up as one. That a *hidden follower list* also empties the whole set is
/// the endpoint's check, not this query's (Pleroma's controller asks it of
/// the target row before running the query at all, so the caller decides).
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn familiar_followers(
    pool: &PgPool,
    target_id: Uuid,
    viewer_id: Uuid,
    limit: i64,
) -> AppResult<Vec<User>> {
    let rows = sqlx::query_as::<_, User>(FAMILIAR_FOLLOWERS_SQL)
        .bind(target_id)
        .bind(viewer_id)
        .bind(limit)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// The state [`follow`] starts a new relationship in: this instance can grant
/// a follow of an account it owns unless that account is locked, and anything
/// else has to wait for the remote answer (Pleroma's `maybe_direct_follow`
/// and `locked` check in `lib/pleroma/user.ex`).
const fn initial_state(target: &User) -> FollowState {
    if target.local.get() && !target.locked.get() {
        FollowState::Accepted
    } else {
        FollowState::Pending
    }
}

/// The `ap_id` to store for a new relationship: a freshly minted
/// `{base_url}/activities/{uuid}` (decision 6) for a follower this instance
/// owns, `None` for a remote follower, whose own activity id the federation
/// layer records from the `Follow` activity it processed.
fn local_activity_id(base_url: &str, follower: &User) -> Option<String> {
    follower
        .local
        .get()
        .then(|| format!("{base_url}/activities/{}", new_id()))
}

/// Whether `follower` and `target` have blocked each other, in either
/// direction.
///
/// The hook [`follow`] refuses through: [`block::blocked_between`] is the
/// `blocks` table's own answer (decision 6), asked once per pair, so a
/// blocked pair never reaches the write below.
async fn blocked_between(pool: &PgPool, follower: &User, target: &User) -> AppResult<bool> {
    block::blocked_between(pool, follower.id, target.id).await
}

/// The refusal [`follow`] answers when [`blocked_between`] says the pair is
/// blocked: Pleroma's `deny_follow_blocked` message
/// (`lib/pleroma/user.ex`, `follow/3`) in this ticket's wording, naming which
/// way the block runs — the target's block of the follower is stated first, so
/// a pair that blocked each other both ways hears the one that answers them.
async fn blocked_error(pool: &PgPool, follower: &User, target: &User) -> AppResult<AppError> {
    if block::is_blocking(pool, target.id, follower.id).await? {
        return Ok(AppError::Forbidden(format!(
            "Could not follow user: {} is blocking you.",
            target.nickname
        )));
    }
    Ok(AppError::Forbidden(format!(
        "Could not follow user: You are blocking {}.",
        target.nickname
    )))
}

/// Writes the first relationship of a pair and returns it, stepping the
/// counters when the write is what made the follow accepted. A concurrent
/// follow of the same pair won the unique key instead: its row is returned
/// unchanged, counters untouched, because that call owns the transition.
async fn insert_follow(
    pool: &PgPool,
    follower_id: Uuid,
    target_id: Uuid,
    state: FollowState,
    ap_id: Option<String>,
) -> AppResult<Follow> {
    let created = sqlx::query_as::<_, Follow>(INSERT_SQL)
        .bind(new_id())
        .bind(follower_id)
        .bind(target_id)
        .bind(state)
        .bind(ap_id.as_deref())
        .fetch_optional(pool)
        .await?;
    match created {
        Some(follow) => counted(pool, follow).await,
        None => required(get(pool, follower_id, target_id).await?),
    }
}

/// Reopens the `rejected` row `previous_id` at `state` and returns it,
/// stepping the counters when this call is the one that moved it to accepted.
/// When a concurrent call got there first its row is returned as it stands.
async fn reopen(
    pool: &PgPool,
    previous_id: Uuid,
    state: FollowState,
    ap_id: Option<String>,
) -> AppResult<Follow> {
    let reopened = sqlx::query_as::<_, Follow>(REOPEN_SQL)
        .bind(previous_id)
        .bind(state)
        .bind(ap_id.as_deref())
        .fetch_optional(pool)
        .await?;
    match reopened {
        Some(follow) => counted(pool, follow).await,
        None => required(by_id(pool, previous_id).await?),
    }
}

/// Returns `follow`, having counted it on both users when it is an acceptance
/// this very call brought about.
async fn counted(pool: &PgPool, follow: Follow) -> AppResult<Follow> {
    if follow.state == FollowState::Accepted {
        step_counters(pool, &follow, 1).await?;
    }
    Ok(follow)
}

/// Adds `step` to the two denormalised counters one relationship feeds: the
/// followee's `followers_count` and the follower's `following_count`.
async fn step_counters(pool: &PgPool, follow: &Follow, step: i32) -> AppResult<()> {
    update_counts(
        pool,
        follow.followee_id,
        CountDelta {
            followers: step,
            ..CountDelta::default()
        },
    )
    .await?;
    update_counts(
        pool,
        follow.follower_id,
        CountDelta {
            following: step,
            ..CountDelta::default()
        },
    )
    .await
}

/// Steps the two counters an accepted relationship fed back down on `conn`,
/// inside an open transaction: the same pair [`step_counters`] writes, one
/// step of `-1` each, committed together with the row's `DELETE` so a crash
/// never leaves a counted relationship without its row (or the other way
/// round). Both [`remove_follower`] and [`sever_between`] run every row they
/// deleted through it.
async fn uncount_removed_follower(conn: &mut PgConnection, follow: &Follow) -> AppResult<()> {
    update_counts(
        &mut *conn,
        follow.followee_id,
        CountDelta {
            followers: -1,
            ..CountDelta::default()
        },
    )
    .await?;
    update_counts(
        &mut *conn,
        follow.follower_id,
        CountDelta {
            following: -1,
            ..CountDelta::default()
        },
    )
    .await
}

/// The relationship behind a primary key.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
async fn by_id(pool: &PgPool, follow_id: Uuid) -> AppResult<Option<Follow>> {
    let follow = sqlx::query_as::<_, Follow>(BY_ID_SQL)
        .bind(follow_id)
        .fetch_optional(pool)
        .await?;
    Ok(follow)
}

/// Turns a row a statement did not guarantee into a result, so a relationship
/// a concurrent write removed answers like any other absent record: 404.
fn required(follow: Option<Follow>) -> AppResult<Follow> {
    follow.map_or_else(|| Err(AppError::NotFound), Ok)
}

/// Runs one of the paged relationship lists: `sql` is one of the
/// `SELECT … FROM follows f …` prefixes, which stop at the placeholder this
/// binds `user_id` to, so the cursors [`PageParams::apply_keyset`] appends are
/// numbered after it; the page comes back newest relationship first
/// ([`PageParams::finish`] flips a `min_id` fetch). `T` is the row shape the
/// statement selects: a bare [`User`] or a [`FollowPageEntry`] with the
/// relationship id.
async fn page_rows<T>(
    pool: &PgPool,
    sql: &str,
    user_id: Uuid,
    page: &PageParams,
) -> AppResult<Vec<T>>
where
    T: for<'r> FromRow<'r, PgRow> + Send + Unpin,
{
    let mut builder = QueryBuilder::<Postgres>::new(sql);
    // The builder's push methods hand back `&mut Self` for chaining; binding
    // it keeps the statement from having an unused result (decision 14).
    let _bound: &mut _ = builder.push_bind(user_id);
    page.apply_keyset(&mut builder, "f.id");
    let rows = builder.build_query_as::<T>().fetch_all(pool).await?;
    Ok(page.finish(rows))
}

#[cfg(test)]
mod tests {
    use sqlx::{Error as SqlxError, PgPool};

    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        models::{
            list::{self, NewList},
            user::{Flag, NewUser, get_by_id, insert, local_ap_id},
        },
        pagination::MAX_LIMIT,
        util::id::parse_id,
    };

    /// The `ActivityPub` path segment a local follower's `ap_id` is built
    /// under, as [`follow`] must write it (decision 6).
    const ACTIVITY_PREFIX: &str = "/activities/";

    /// A local [`NewUser`] named `nickname` on the test instance.
    fn local_new(nickname: &str) -> NewUser {
        NewUser::local(nickname, &local_ap_id(TEST_BASE_URL, nickname))
    }

    /// Inserts a local, unlocked user named `nickname`.
    async fn local_user(pool: &PgPool, nickname: &str) -> User {
        insert(pool, &local_new(nickname))
            .await
            .expect("the local fixture inserts")
    }

    /// Inserts a local user named `nickname` whose account is locked, so
    /// follows of it wait for an answer.
    async fn locked_user(pool: &PgPool, nickname: &str) -> User {
        insert(
            pool,
            &NewUser {
                locked: Flag::new(true),
                ..local_new(nickname)
            },
        )
        .await
        .expect("the locked fixture inserts")
    }

    /// Inserts a mirror of `nickname` on `remote.example` whose fetch reported
    /// seven followers, so a test can see whether a counter was touched.
    async fn remote_user(pool: &PgPool, nickname: &str) -> User {
        let ap_id = format!("https://remote.example/users/{nickname}");
        insert(
            pool,
            &NewUser {
                nickname: format!("{nickname}@remote.example"),
                ap_id,
                domain: Some("remote.example".to_owned()),
                followers_count: 7,
                ..NewUser::default()
            },
        )
        .await
        .expect("the remote fixture inserts")
    }

    /// The row of user `id` as it stands now, for reading counters back.
    async fn fresh_user(pool: &PgPool, id: Uuid) -> User {
        get_by_id(pool, id).await.unwrap().expect("the user exists")
    }

    /// The stored relationship of the pair, which the test expects to exist.
    async fn relation(pool: &PgPool, follower: &User, target: &User) -> Follow {
        get(pool, follower.id, target.id)
            .await
            .unwrap()
            .expect("the relationship exists")
    }

    /// Writes a `rejected` row by hand: [`reject`] deletes the row rather than
    /// storing that state, so the reopen and unfollow paths have to set it up
    /// the way a mirrored remote rejection would have.
    async fn rejected_relation(pool: &PgPool, follower: &User, target: &User) -> Follow {
        sqlx::query_as::<_, Follow>(
            "INSERT INTO follows (id, follower_id, followee_id, state)
             VALUES ($1, $2, $3, 'rejected')
             RETURNING *",
        )
        .bind(new_id())
        .bind(follower.id)
        .bind(target.id)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// Records `blocker` blocking `target` through the table's own module:
    /// the fixture the follow guard below reads.
    async fn block_pair(pool: &PgPool, blocker: &User, target: &User) {
        let stored = block::block(pool, TEST_BASE_URL, blocker, target)
            .await
            .expect("the fixture block stores");
        assert_eq!(
            stored.blocked_id, target.id,
            "the row is the pair that was asked for"
        );
    }

    /// The raw `state` text of one row, to check the enum's database mapping.
    async fn state_text(pool: &PgPool, follow_id: Uuid) -> String {
        let row: (String,) = sqlx::query_as("SELECT state FROM follows WHERE id = $1")
            .bind(follow_id)
            .fetch_one(pool)
            .await
            .unwrap();
        row.0
    }

    /// The number of relationship rows in the table.
    async fn follow_count(pool: &PgPool) -> i64 {
        let row: (i64,) = sqlx::query_as("SELECT count(*) FROM follows")
            .fetch_one(pool)
            .await
            .unwrap();
        row.0
    }

    /// Page params built from a query string, the way the extractor makes them.
    fn page(query: &str) -> PageParams {
        PageParams::from_query(Some(query), MAX_LIMIT).unwrap()
    }

    /// The ids of `users`, in the order they came back.
    fn ids(users: &[User]) -> Vec<Uuid> {
        users.iter().map(|user| user.id).collect()
    }

    /// The ids of the users behind `entries`, in the order they came back.
    fn entry_ids(entries: &[FollowPageEntry]) -> Vec<Uuid> {
        entries.iter().map(|entry| entry.user.id).collect()
    }

    /// The relationship row ids of `entries`, in the order they came back.
    fn follow_ids(entries: &[FollowPageEntry]) -> Vec<Uuid> {
        entries.iter().map(|entry| entry.follow_id).collect()
    }

    /// The `{TEST_BASE_URL}/activities/{uuid}` id [`follow`] minted for a
    /// local follower, checked for its shape and reduced to the id part.
    fn activity_id(follow: &Follow) -> String {
        let prefix = format!("{TEST_BASE_URL}{ACTIVITY_PREFIX}");
        let ap_id = follow
            .ap_id
            .clone()
            .expect("a local follower's row carries a minted activity id");
        assert!(
            ap_id.starts_with(&prefix),
            "the activity id is this instance's, got {ap_id}"
        );
        let tail = ap_id
            .strip_prefix(&prefix)
            .expect("the prefix was just checked");
        assert!(
            parse_id(tail).is_some(),
            "the tail of the activity id is an id: {tail}"
        );
        tail.to_owned()
    }

    /// The SQLSTATE a database failure carries, or an empty string when the
    /// failure is not the database reporting a constraint.
    fn sqlstate_of(err: SqlxError) -> String {
        if let SqlxError::Database(db_err) = err {
            return db_err.code().unwrap_or_default().into_owned();
        }
        String::new()
    }

    /// `rows` by relationship id, oldest first. `UUIDv7` decides the order the
    /// lists come back in, and it is only millisecond precise, so an
    /// expectation is sorted from what was stored rather than from the order
    /// the writes happened in.
    fn by_age(rows: &[Follow]) -> Vec<&Follow> {
        let mut sorted = rows.iter().collect::<Vec<_>>();
        sorted.sort_by_key(|follow| follow.id);
        sorted
    }

    /// The followees of `rows`, oldest relationship first.
    fn followed_oldest_first(rows: &[Follow]) -> Vec<Uuid> {
        by_age(rows)
            .iter()
            .map(|follow| follow.followee_id)
            .collect()
    }

    /// The followers of `rows`, newest relationship first.
    fn followers_newest_first(rows: &[Follow]) -> Vec<Uuid> {
        by_age(rows)
            .iter()
            .rev()
            .map(|follow| follow.follower_id)
            .collect()
    }

    /// The followees of `rows`, newest relationship first.
    fn followed_newest_first(rows: &[Follow]) -> Vec<Uuid> {
        by_age(rows)
            .iter()
            .rev()
            .map(|follow| follow.followee_id)
            .collect()
    }

    #[tokio::test]
    async fn following_an_open_local_user_is_accepted_and_counted() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let follow = follow(&pool, TEST_BASE_URL, &alice, &bob).await.unwrap();

        assert!(follow.id.get_version_num() == 7, "the id is a UUIDv7");
        assert_eq!(follow.follower_id, alice.id, "who follows");
        assert_eq!(follow.followee_id, bob.id, "who is followed");
        assert_eq!(
            follow.state,
            FollowState::Accepted,
            "an unlocked local account is answered on the spot"
        );
        assert!(follow.show_reblogs, "reblogs are shown by default");
        assert!(!follow.notify, "notifications are off by default");
        assert!(
            follow.inserted_at <= follow.updated_at,
            "the database stamps both timestamps"
        );
        let tail = activity_id(&follow);
        assert!(
            parse_id(&tail).is_some(),
            "the local activity id is {TEST_BASE_URL}{ACTIVITY_PREFIX}{{uuid}}"
        );
        assert_eq!(
            state_text(&pool, follow.id).await,
            "accepted",
            "the state is stored as its lowercase name"
        );

        assert_eq!(
            relation(&pool, &alice, &bob).await,
            follow,
            "get() reads back the row it wrote"
        );
        assert!(
            is_following(&pool, &alice, &bob).await.unwrap(),
            "the follow is on the books"
        );
        assert!(
            !is_following(&pool, &bob, &alice).await.unwrap(),
            "following is directional"
        );
        assert_eq!(
            following_ids(&pool, alice.id).await.unwrap(),
            vec![bob.id],
            "the accepted follow is listed"
        );
        assert_eq!(
            fresh_user(&pool, bob.id).await.followers_count,
            1,
            "the followee counts a new follower"
        );
        assert_eq!(
            fresh_user(&pool, alice.id).await.following_count,
            1,
            "the follower counts a new follow"
        );
    }

    #[tokio::test]
    async fn following_a_locked_user_waits_and_moves_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let carol = locked_user(&pool, "carol").await;
        let follow = follow(&pool, TEST_BASE_URL, &alice, &carol).await.unwrap();

        assert_eq!(
            follow.state,
            FollowState::Pending,
            "a locked account answers its own requests"
        );
        assert_eq!(
            state_text(&pool, follow.id).await,
            "pending",
            "the enum name is what the column holds"
        );
        assert!(
            !is_following(&pool, &alice, &carol).await.unwrap(),
            "a request nobody answered is not a follow"
        );
        assert!(
            following_ids(&pool, alice.id).await.unwrap().is_empty(),
            "a pending follow is not in the following list"
        );
        assert_eq!(
            entry_ids(
                &pending_requests_for(&pool, carol.id, page(""))
                    .await
                    .unwrap()
            ),
            vec![alice.id],
            "carol has alice's request to answer"
        );
        assert!(
            pending_requests_for(&pool, alice.id, page(""))
                .await
                .unwrap()
                .is_empty(),
            "alice has nothing to answer"
        );
        assert_eq!(
            fresh_user(&pool, carol.id).await.followers_count,
            0,
            "a request is not yet a follower"
        );
        assert_eq!(
            fresh_user(&pool, alice.id).await.following_count,
            0,
            "and not yet a follow"
        );
    }

    #[tokio::test]
    async fn accepting_grants_a_pending_request_once() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let carol = locked_user(&pool, "carol").await;
        let requested = follow(&pool, TEST_BASE_URL, &alice, &carol).await.unwrap();
        let accepted = accept(&pool, requested.id).await.unwrap();

        assert_eq!(accepted.id, requested.id, "the same relationship, granted");
        assert_eq!(accepted.state, FollowState::Accepted, "the state moved");
        assert_eq!(
            state_text(&pool, accepted.id).await,
            "accepted",
            "the write went through as text"
        );
        assert_eq!(
            accepted.ap_id, requested.ap_id,
            "accepting keeps the follow's activity id"
        );
        assert!(
            is_following(&pool, &alice, &carol).await.unwrap(),
            "the answered request follows"
        );
        assert!(
            pending_requests_for(&pool, carol.id, page(""))
                .await
                .unwrap()
                .is_empty(),
            "the request left the queue"
        );
        assert_eq!(
            entry_ids(&followers_page(&pool, carol.id, page("")).await.unwrap()),
            vec![alice.id],
            "alice shows up in carol's followers"
        );
        assert_eq!(
            entry_ids(&following_page(&pool, alice.id, page("")).await.unwrap()),
            vec![carol.id],
            "carol shows up in alice's following"
        );
        assert_eq!(
            (
                fresh_user(&pool, carol.id).await.followers_count,
                fresh_user(&pool, alice.id).await.following_count
            ),
            (1, 1),
            "accepting is what counts"
        );

        let again = accept(&pool, accepted.id).await.unwrap();
        assert_eq!(again, accepted, "accepting twice is the stored row again");
        assert_eq!(
            (
                fresh_user(&pool, carol.id).await.followers_count,
                fresh_user(&pool, alice.id).await.following_count
            ),
            (1, 1),
            "and the second call moves no counters"
        );
    }

    #[tokio::test]
    async fn rejecting_a_pending_request_removes_it() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let carol = locked_user(&pool, "carol").await;
        let requested = follow(&pool, TEST_BASE_URL, &alice, &carol).await.unwrap();

        let removed = reject(&pool, requested.id).await.unwrap();
        assert_eq!(removed, requested, "the removed row comes back as it stood");
        assert_eq!(
            removed.state,
            FollowState::Pending,
            "a rejected request never counted"
        );
        assert!(
            get(&pool, alice.id, carol.id).await.unwrap().is_none(),
            "the answer is a deleted row, not a `rejected` one"
        );
        assert_eq!(follow_count(&pool).await, 0, "nothing is left in the table");
        assert!(
            pending_requests_for(&pool, carol.id, page(""))
                .await
                .unwrap()
                .is_empty(),
            "the queue no longer holds it"
        );
        assert_eq!(
            (
                fresh_user(&pool, carol.id).await.followers_count,
                fresh_user(&pool, alice.id).await.following_count
            ),
            (0, 0),
            "withdrawing a request moves no counters"
        );
    }

    #[tokio::test]
    async fn rejecting_an_accepted_relationship_uncounts_it() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let accepted = follow(&pool, TEST_BASE_URL, &alice, &bob).await.unwrap();

        let removed = reject(&pool, accepted.id).await.unwrap();
        assert_eq!(
            removed.state,
            FollowState::Accepted,
            "an accepted relationship can be withdrawn the same way"
        );
        assert!(
            !is_following(&pool, &alice, &bob).await.unwrap(),
            "and it is gone"
        );
        assert_eq!(
            fresh_user(&pool, bob.id).await.followers_count,
            0,
            "the counters step back down"
        );
        assert_eq!(
            fresh_user(&pool, alice.id).await.following_count,
            0,
            "on both sides"
        );
    }

    #[tokio::test]
    async fn unfollowing_reports_what_it_removed() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let carol = locked_user(&pool, "carol").await;
        let dave = local_user(&pool, "dave").await;

        assert!(
            !unfollow(&pool, &alice, &bob).await.unwrap(),
            "a pair that never followed has nothing to take back"
        );

        let accepted = follow(&pool, TEST_BASE_URL, &alice, &bob).await.unwrap();
        assert_eq!(accepted.state, FollowState::Accepted, "the fixture counts");
        assert!(
            unfollow(&pool, &alice, &bob).await.unwrap(),
            "an accepted follow is removed"
        );
        assert_eq!(
            fresh_user(&pool, bob.id).await.followers_count,
            0,
            "removing it steps the followee's count down"
        );
        assert_eq!(
            fresh_user(&pool, alice.id).await.following_count,
            0,
            "and the follower's"
        );
        assert!(
            !unfollow(&pool, &alice, &bob).await.unwrap(),
            "unfollowing twice finds nothing the second time"
        );

        let pending = follow(&pool, TEST_BASE_URL, &alice, &carol).await.unwrap();
        assert_eq!(pending.state, FollowState::Pending, "the fixture waits");
        assert!(
            unfollow(&pool, &alice, &carol).await.unwrap(),
            "a pending request can be withdrawn too"
        );
        assert_eq!(
            fresh_user(&pool, carol.id).await.followers_count,
            0,
            "withdrawing it counts nothing"
        );

        let rejected = rejected_relation(&pool, &dave, &carol).await;
        assert!(
            !unfollow(&pool, &dave, &carol).await.unwrap(),
            "a rejected row is not an open relationship"
        );
        assert_eq!(
            relation(&pool, &dave, &carol).await,
            rejected,
            "and unfollowing leaves it in place"
        );
    }

    /// An endorsement is a pinned *follow*: [`unfollow`] clears the pair's
    /// endorsement together with the relationship — granted follows and
    /// withdrawn requests alike — while the reverse pin and another
    /// account's pin on the same target stay exactly where they were.
    #[tokio::test]
    async fn unfollowing_removes_the_endorsement() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let carol = locked_user(&pool, "carol").await;
        let dave = local_user(&pool, "dave").await;

        let accepted = follow(&pool, TEST_BASE_URL, &alice, &bob)
            .await
            .expect("the fixture follow is granted");
        assert_eq!(accepted.state, FollowState::Accepted, "it counts");
        endorsement::endorse(&pool, alice.id, bob.id)
            .await
            .expect("alice pins bob");
        endorsement::endorse(&pool, bob.id, alice.id)
            .await
            .expect("and bob pins alice back");
        endorsement::endorse(&pool, dave.id, bob.id)
            .await
            .expect("dave pins bob too");
        let waiting = follow(&pool, TEST_BASE_URL, &alice, &carol)
            .await
            .expect("the request to the locked account waits");
        assert_eq!(waiting.state, FollowState::Pending, "it waits");
        endorsement::endorse(&pool, alice.id, carol.id)
            .await
            .expect("a hand-seeded pin rides the request");

        assert!(
            unfollow(&pool, &alice, &bob).await.unwrap(),
            "the accepted follow is removed"
        );
        assert!(
            endorsement::endorsed_ids(&pool, alice.id)
                .await
                .unwrap()
                .contains(&carol.id),
            "alice's withdrawal took only the bob pair: the carol pin stands"
        );
        assert!(
            !endorsement::endorsed_ids(&pool, alice.id)
                .await
                .unwrap()
                .contains(&bob.id),
            "and the pair she unfollowed is unpinned"
        );
        assert_eq!(
            endorsement::endorsed_ids(&pool, bob.id).await.unwrap(),
            vec![alice.id],
            "bob's own pin was not alice's to drop"
        );
        assert_eq!(
            endorsement::endorsed_ids(&pool, dave.id).await.unwrap(),
            vec![bob.id],
            "and dave's pin on bob was nobody else's to drop"
        );

        assert!(
            unfollow(&pool, &alice, &carol).await.unwrap(),
            "a withdrawn request is an open relationship too"
        );
        assert!(
            endorsement::endorsed_ids(&pool, alice.id)
                .await
                .unwrap()
                .is_empty(),
            "and its seed pin went with it"
        );
    }

    /// The cleanup rides the removal: an [`unfollow`] that found nothing to
    /// remove (here a `rejected` row) leaves an existing endorsement alone.
    #[tokio::test]
    async fn an_unfollow_that_removed_nothing_keeps_the_endorsement() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let dave = local_user(&pool, "dave").await;
        let carol = locked_user(&pool, "carol").await;
        let _rejected = rejected_relation(&pool, &dave, &carol).await;
        endorsement::endorse(&pool, dave.id, carol.id)
            .await
            .expect("the seed pin stores");

        assert!(
            !unfollow(&pool, &dave, &carol).await.unwrap(),
            "a rejected row is not an open relationship"
        );
        assert_eq!(
            endorsement::endorsed_ids(&pool, dave.id).await.unwrap(),
            vec![carol.id],
            "and the failed unfollow pinned nothing away"
        );
    }

    /// A list titled `title` owned by `owner`, for the membership cleanup.
    async fn seeded_list(pool: &PgPool, owner: &User, title: &str) -> list::List {
        list::create(
            pool,
            owner.id,
            &NewList {
                title: title.to_owned(),
                ..NewList::default()
            },
        )
        .await
        .expect("the fixture list creates")
    }

    /// [`unfollow`]'s list cleanup: the account stops being a member of the
    /// unfollowing account's own lists — an accepted relationship and a
    /// withdrawn request alike — while another owner's membership of the same
    /// account is not theirs to prune.
    #[tokio::test]
    async fn unfollowing_takes_the_target_out_of_the_followers_lists() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let carol = local_user(&pool, "carol").await;
        let erin = locked_user(&pool, "erin").await;
        let hers = seeded_list(&pool, &alice, "hers").await;
        let also_hers = seeded_list(&pool, &alice, "also hers").await;
        let carols = seeded_list(&pool, &carol, "carol's own").await;
        for row in [&hers, &also_hers, &carols] {
            let added = list::add_members(&pool, row.id, &[bob.id, erin.id])
                .await
                .expect("the fixture members add");
            assert_eq!(added, 2, "both accounts joined the fixture list");
        }
        let _accepted = follow(&pool, TEST_BASE_URL, &alice, &bob)
            .await
            .expect("alice follows bob");
        let _waiting = follow(&pool, TEST_BASE_URL, &alice, &erin)
            .await
            .expect("and alice's request to erin waits");

        assert!(
            unfollow(&pool, &alice, &bob).await.unwrap(),
            "the granted follow is removed"
        );
        assert!(
            unfollow(&pool, &alice, &erin).await.unwrap(),
            "and the withdrawn request is an open relationship too"
        );
        for list in [&hers, &also_hers] {
            assert!(
                list::member_ids(&pool, list.id)
                    .await
                    .expect("members read")
                    .is_empty(),
                "both of alice's lists are pruned of both accounts"
            );
        }
        assert_eq!(
            list::member_ids(&pool, carols.id).await.unwrap(),
            vec![bob.id, erin.id],
            "while carol's membership of the same two accounts stands"
        );
    }

    /// The cleanup rides the removal: an [`unfollow`] that found nothing to
    /// remove leaves the memberships exactly where they were, too.
    #[tokio::test]
    async fn an_unfollow_that_removed_nothing_keeps_the_memberships() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let dave = local_user(&pool, "dave").await;
        let carol = locked_user(&pool, "carol").await;
        let _rejected = rejected_relation(&pool, &dave, &carol).await;
        let hers = seeded_list(&pool, &dave, "hers").await;
        let seeded = list::add_members(&pool, hers.id, &[carol.id])
            .await
            .expect("the seed membership adds");
        assert_eq!(seeded, 1, "one membership went in");

        assert!(
            !unfollow(&pool, &dave, &carol).await.unwrap(),
            "a rejected row is not an open relationship"
        );
        assert_eq!(
            list::member_ids(&pool, hers.id).await.unwrap(),
            vec![carol.id],
            "and the failed unfollow listed nobody out"
        );
    }

    #[tokio::test]
    async fn remove_follower_takes_an_accepted_follower_and_uncounts_it() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let accepted = follow(&pool, TEST_BASE_URL, &alice, &bob).await.unwrap();

        let removed = remove_follower(&pool, bob.id, alice.id).await.unwrap();
        assert_eq!(
            removed.as_ref(),
            Some(&accepted),
            "the removed row comes back as it stood"
        );
        assert!(
            get(&pool, alice.id, bob.id).await.unwrap().is_none(),
            "the follower is gone from the table"
        );
        assert_eq!(
            fresh_user(&pool, bob.id).await.followers_count,
            0,
            "the followee's count steps down with the row"
        );
        assert_eq!(
            fresh_user(&pool, alice.id).await.following_count,
            0,
            "and the follower's"
        );

        let again = remove_follower(&pool, bob.id, alice.id).await.unwrap();
        assert!(again.is_none(), "a second call finds no follower");
        assert_eq!(follow_count(&pool).await, 0, "and writes nothing");
        assert_eq!(
            fresh_user(&pool, bob.id).await.followers_count,
            0,
            "with no row left, no counter moves a second time"
        );
    }

    #[tokio::test]
    async fn remove_follower_leaves_everything_but_a_follower_alone() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let carol = locked_user(&pool, "carol").await;
        let erin = local_user(&pool, "erin").await;

        let pending = follow(&pool, TEST_BASE_URL, &alice, &carol).await.unwrap();
        assert_eq!(pending.state, FollowState::Pending, "the fixture waits");
        assert!(
            remove_follower(&pool, carol.id, alice.id)
                .await
                .unwrap()
                .is_none(),
            "a request nobody answered is not a follower"
        );
        assert_eq!(
            relation(&pool, &alice, &carol).await,
            pending,
            "the request waits on, untouched"
        );

        let mirrored = rejected_relation(&pool, &erin, &carol).await;
        assert!(
            remove_follower(&pool, carol.id, erin.id)
                .await
                .unwrap()
                .is_none(),
            "a mirrored rejection is not a follower either"
        );
        assert_eq!(
            relation(&pool, &erin, &carol).await,
            mirrored,
            "and its row stays in place"
        );

        // The reverse of the pair the caller follows: that is the caller's
        // own follow to give back through unfollow, not a follower to remove.
        let outgoing = follow(&pool, TEST_BASE_URL, &carol, &bob).await.unwrap();
        assert_eq!(outgoing.state, FollowState::Accepted, "carol follows bob");
        assert!(
            remove_follower(&pool, carol.id, bob.id)
                .await
                .unwrap()
                .is_none(),
            "bob is not a follower of carol"
        );
        assert_eq!(
            relation(&pool, &carol, &bob).await,
            outgoing,
            "the outgoing follow stays"
        );

        assert!(
            remove_follower(&pool, carol.id, new_id())
                .await
                .unwrap()
                .is_none(),
            "an account with no row at all is no follower"
        );

        assert_eq!(
            (
                fresh_user(&pool, carol.id).await.followers_count,
                fresh_user(&pool, alice.id).await.following_count,
                fresh_user(&pool, erin.id).await.following_count
            ),
            (0, 0, 0),
            "none of these removals counted anything"
        );
        assert_eq!(
            fresh_user(&pool, bob.id).await.followers_count,
            1,
            "and the one accepted relationship that exists is still on the books"
        );
    }

    #[tokio::test]
    async fn following_twice_is_the_same_relationship() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let carol = locked_user(&pool, "carol").await;

        let first = follow(&pool, TEST_BASE_URL, &alice, &bob).await.unwrap();
        let again = follow(&pool, TEST_BASE_URL, &alice, &bob).await.unwrap();
        assert_eq!(again, first, "the stored row comes back verbatim");
        assert_eq!(
            activity_id(&again),
            activity_id(&first),
            "the second call mints no new activity id"
        );
        assert_eq!(follow_count(&pool).await, 1, "one row per pair");
        assert_eq!(
            (
                fresh_user(&pool, bob.id).await.followers_count,
                fresh_user(&pool, alice.id).await.following_count
            ),
            (1, 1),
            "and the counters move once"
        );

        let pending = follow(&pool, TEST_BASE_URL, &alice, &carol).await.unwrap();
        let pending_again = follow(&pool, TEST_BASE_URL, &alice, &carol).await.unwrap();
        assert_eq!(
            pending_again, pending,
            "asking twice while nobody has answered changes nothing"
        );
        assert_eq!(follow_count(&pool).await, 2, "still one row per pair");
        assert_eq!(
            fresh_user(&pool, carol.id).await.followers_count,
            0,
            "and a waiting request still counts nothing"
        );
    }

    #[tokio::test]
    async fn following_again_reopens_a_rejected_request() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let carol = locked_user(&pool, "carol").await;

        let rejected = rejected_relation(&pool, &alice, &bob).await;
        assert!(
            rejected.ap_id.is_none(),
            "the mirrored rejection stored no id"
        );
        let reopened = follow(&pool, TEST_BASE_URL, &alice, &bob).await.unwrap();
        assert_eq!(
            reopened.id, rejected.id,
            "the rejected row is reopened in place"
        );
        assert_eq!(
            reopened.state,
            FollowState::Accepted,
            "at the state this follow implies"
        );
        let tail = activity_id(&reopened);
        assert_eq!(
            reopened.ap_id,
            Some(format!("{TEST_BASE_URL}{ACTIVITY_PREFIX}{tail}")),
            "and with a fresh activity id for the new `Follow`"
        );
        assert_eq!(follow_count(&pool).await, 1, "reopening does not add a row");
        assert_eq!(
            (
                fresh_user(&pool, bob.id).await.followers_count,
                fresh_user(&pool, alice.id).await.following_count
            ),
            (1, 1),
            "the reopening counts"
        );

        let rejected_locked = rejected_relation(&pool, &alice, &carol).await;
        let pending = follow(&pool, TEST_BASE_URL, &alice, &carol).await.unwrap();
        assert_eq!(pending.id, rejected_locked.id, "reopened in place here too");
        assert_eq!(
            pending.state,
            FollowState::Pending,
            "but a locked account still has to answer"
        );
        assert_eq!(
            fresh_user(&pool, carol.id).await.followers_count,
            0,
            "and the reopened request counts nothing"
        );
        assert_eq!(
            fresh_user(&pool, alice.id).await.following_count,
            1,
            "alice's own count holds at the one accepted follow"
        );
    }

    #[tokio::test]
    async fn following_yourself_is_refused() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let err = follow(&pool, TEST_BASE_URL, &alice, &alice)
            .await
            .expect_err("a self follow cannot be recorded");
        assert!(
            matches!(err, AppError::Unprocessable(_)),
            "a self follow is a 422, like Pleroma's"
        );
        assert_eq!(err.to_string(), "Can not follow yourself", "the wording");
        assert_eq!(follow_count(&pool).await, 0, "nothing was written");
        assert_eq!(
            fresh_user(&pool, alice.id).await.followers_count,
            0,
            "and nothing counted"
        );
    }

    #[tokio::test]
    async fn a_remote_followee_is_answered_by_its_own_instance() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let bob = local_user(&pool, "bob").await;
        let carol = remote_user(&pool, "carol").await;
        let follow = follow(&pool, TEST_BASE_URL, &bob, &carol).await.unwrap();

        assert_eq!(
            follow.state,
            FollowState::Pending,
            "acceptance can only arrive from the remote instance"
        );
        assert!(
            !is_following(&pool, &bob, &carol).await.unwrap(),
            "a request out is not yet a follow"
        );
        assert_eq!(
            fresh_user(&pool, bob.id).await.following_count,
            0,
            "a pending request out counts nothing"
        );
        assert_eq!(
            fresh_user(&pool, carol.id).await.followers_count,
            7,
            "the mirrored account keeps the count its own instance reported"
        );
    }

    #[tokio::test]
    async fn a_remote_follower_keeps_its_own_activity_id() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let mallory = remote_user(&pool, "mallory").await;
        let bob = local_user(&pool, "bob").await;
        let follow = follow(&pool, TEST_BASE_URL, &mallory, &bob).await.unwrap();

        assert_eq!(
            follow.state,
            FollowState::Accepted,
            "this instance owns the answer, so the follow is granted"
        );
        assert!(
            follow.ap_id.is_none(),
            "a remote follower's activity id is the one its instance sent, not a local mint"
        );
        assert_eq!(
            fresh_user(&pool, bob.id).await.followers_count,
            1,
            "a granted follow counts, whoever started it"
        );
        assert_eq!(
            fresh_user(&pool, mallory.id).await.following_count,
            1,
            "on both sides"
        );
    }

    #[tokio::test]
    async fn accept_or_reject_of_an_unknown_id_is_not_found() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let missing = new_id();
        let accept_err = accept(&pool, missing).await.expect_err("nothing to accept");
        assert!(
            matches!(accept_err, AppError::NotFound),
            "an unknown follow id is a 404"
        );
        let reject_err = reject(&pool, missing).await.expect_err("nothing to reject");
        assert!(
            matches!(reject_err, AppError::NotFound),
            "an unknown follow id is a 404"
        );

        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let accepted = follow(&pool, TEST_BASE_URL, &alice, &bob).await.unwrap();
        assert!(
            unfollow(&pool, &alice, &bob).await.unwrap(),
            "the relationship is removed underneath"
        );
        let stale = accept(&pool, accepted.id)
            .await
            .expect_err("a removed relationship cannot be accepted");
        assert!(
            matches!(stale, AppError::NotFound),
            "and the answer is the same 404"
        );
    }

    #[tokio::test]
    async fn following_ids_lists_accepted_follows_oldest_first() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let carol = locked_user(&pool, "carol").await;
        let dave = local_user(&pool, "dave").await;

        let granted = follow(&pool, TEST_BASE_URL, &alice, &bob)
            .await
            .expect("the first follow is recorded");
        let waiting = follow(&pool, TEST_BASE_URL, &alice, &carol)
            .await
            .expect("the second follow is recorded");
        let other = follow(&pool, TEST_BASE_URL, &alice, &dave)
            .await
            .expect("the third follow is recorded");
        let mut listed = vec![granted, other];
        assert_eq!(
            following_ids(&pool, alice.id).await.unwrap(),
            followed_oldest_first(&listed),
            "the two granted follows, oldest relationship first"
        );
        assert!(
            following_ids(&pool, bob.id).await.unwrap().is_empty(),
            "an account nobody follows has an empty list"
        );

        let answered = accept(&pool, waiting.id)
            .await
            .expect("the request is granted");
        assert_eq!(
            answered.state,
            FollowState::Accepted,
            "answering carol's request is what let it into the list"
        );
        listed.push(relation(&pool, &alice, &carol).await);
        assert_eq!(
            following_ids(&pool, alice.id).await.unwrap(),
            followed_oldest_first(&listed),
            "once answered, all three are listed"
        );
    }

    /// Three accounts that each follow `target`, accepted at once, and the
    /// stored rows of those three relationships.
    async fn three_followers(pool: &PgPool, target: &User) -> (Vec<User>, Vec<Follow>) {
        let mut followers = Vec::new();
        for nickname in ["one", "two", "three"] {
            followers.push(local_user(pool, nickname).await);
        }
        let mut rows = Vec::new();
        for follower in &followers {
            let granted = follow(pool, TEST_BASE_URL, follower, target)
                .await
                .expect("an open account answers every request");
            assert_eq!(
                granted.state,
                FollowState::Accepted,
                "each of these is granted at once"
            );
            rows.push(relation(pool, follower, target).await);
        }
        (followers, rows)
    }

    /// One page of `owner`'s followers, unwrapped.
    async fn followers_of(pool: &PgPool, owner: &User, query: &str) -> Vec<FollowPageEntry> {
        followers_page(pool, owner.id, page(query)).await.unwrap()
    }

    /// One page of `owner`'s following list, unwrapped.
    async fn following_of(pool: &PgPool, owner: &User, query: &str) -> Vec<FollowPageEntry> {
        following_page(pool, owner.id, page(query)).await.unwrap()
    }

    #[tokio::test]
    async fn follower_lists_are_paged_newest_relationship_first() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let target = local_user(&pool, "target").await;
        let (followers, rows) = three_followers(&pool, &target).await;
        // Which of the three rows is "newest" is decided by their ids, not by
        // the order they were written in: UUIDv7 is only millisecond precise.
        let newest_first = followers_newest_first(&rows);
        let oldest_id = by_age(&rows).first().expect("three rows were read back").id;

        let whole = followers_of(&pool, &target, "").await;
        assert_eq!(
            entry_ids(&whole),
            newest_first,
            "the whole follower list, newest follow first"
        );
        let first_two = followers_of(&pool, &target, "limit=2").await;
        assert_eq!(
            entry_ids(&first_two),
            newest_first[..2].to_vec(),
            "the default cap and a limit cut the first page short"
        );
        let newest_follow_ids: Vec<Uuid> =
            by_age(&rows).iter().rev().map(|follow| follow.id).collect();
        assert_eq!(
            follow_ids(&first_two),
            newest_follow_ids[..2].to_vec(),
            "entries carry the relationship id the page is keyed on"
        );

        let last_on_page = relation(&pool, &first_two[1].user, &target).await;
        assert_eq!(
            last_on_page.id, first_two[1].follow_id,
            "the entry's relationship id is the stored row's primary key"
        );
        let next = followers_of(
            &pool,
            &target,
            &format!("limit=2&max_id={}", last_on_page.id),
        )
        .await;
        assert_eq!(
            entry_ids(&next),
            newest_first[2..].to_vec(),
            "max_id continues where the page ended"
        );
        let newer = followers_of(&pool, &target, &format!("min_id={}", last_on_page.id)).await;
        assert_eq!(
            entry_ids(&newer),
            newest_first[..1].to_vec(),
            "min_id reads the half after the cursor and is still answered newest-first"
        );
        assert!(
            followers_of(&pool, &target, &format!("max_id={oldest_id}"))
                .await
                .is_empty(),
            "walking past the oldest row yields nothing"
        );
        assert!(
            followers_of(&pool, &followers[0], "").await.is_empty(),
            "a follower of the list is not followed by anyone"
        );

        let mut outgoing = Vec::new();
        for followed in &followers {
            outgoing.push(
                follow(&pool, TEST_BASE_URL, &target, followed)
                    .await
                    .expect("the same account follows back"),
            );
        }
        let wanted = followed_newest_first(&outgoing);
        assert_eq!(
            entry_ids(&following_of(&pool, &target, "").await),
            wanted,
            "the following list runs the same way"
        );
        assert_eq!(
            entry_ids(&following_of(&pool, &target, "limit=2").await),
            wanted[..2].to_vec(),
            "and is paginated on the relationship id too"
        );
    }

    #[tokio::test]
    async fn pending_requests_are_paged_and_answered_away() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let carol = locked_user(&pool, "carol").await;
        let one = local_user(&pool, "one").await;
        let two = local_user(&pool, "two").await;
        let three = local_user(&pool, "three").await;
        for follower in [&one, &two, &three] {
            let queued = follow(&pool, TEST_BASE_URL, follower, &carol)
                .await
                .expect("a locked account queues the request");
            assert_eq!(
                queued.state,
                FollowState::Pending,
                "every request to a locked account waits"
            );
        }
        let rows = vec![
            relation(&pool, &one, &carol).await,
            relation(&pool, &two, &carol).await,
            relation(&pool, &three, &carol).await,
        ];
        let newest_first = followers_newest_first(&rows);

        assert_eq!(
            entry_ids(
                &pending_requests_for(&pool, carol.id, page(""))
                    .await
                    .unwrap()
            ),
            newest_first,
            "every request waits, newest first"
        );
        let first = pending_requests_for(&pool, carol.id, page("limit=1"))
            .await
            .unwrap();
        assert_eq!(
            entry_ids(&first),
            newest_first[..1].to_vec(),
            "one request per page"
        );
        let cursor = relation(&pool, &first[0].user, &carol).await;
        assert_eq!(
            cursor.id, first[0].follow_id,
            "the entry carries the request row id the page is keyed on"
        );
        assert_eq!(
            entry_ids(
                &pending_requests_for(
                    &pool,
                    carol.id,
                    page(&format!("limit=1&max_id={}", cursor.id))
                )
                .await
                .unwrap()
            ),
            newest_first[1..2].to_vec(),
            "the next page is the next-oldest request"
        );

        let granted = accept(&pool, cursor.id)
            .await
            .expect("the newest request is granted");
        assert_eq!(
            granted.id, cursor.id,
            "the request that was at the head is answered"
        );
        assert_eq!(
            entry_ids(
                &pending_requests_for(&pool, carol.id, page(""))
                    .await
                    .unwrap()
            ),
            newest_first[1..].to_vec(),
            "and leaves the queue"
        );
        assert_eq!(
            entry_ids(&followers_page(&pool, carol.id, page("")).await.unwrap()),
            vec![first[0].user.id],
            "into the follower list"
        );
    }

    #[tokio::test]
    async fn pending_counts_the_requests_one_account_has_to_answer() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let carol = locked_user(&pool, "carol").await;
        let dave = local_user(&pool, "dave").await;
        let erin = local_user(&pool, "erin").await;
        let one = local_user(&pool, "one").await;
        let two = local_user(&pool, "two").await;

        assert_eq!(
            pending_count(&pool, carol.id).await.unwrap(),
            0,
            "an account nobody has asked to follow has an empty queue"
        );

        let first = follow(&pool, TEST_BASE_URL, &one, &carol).await.unwrap();
        let second = follow(&pool, TEST_BASE_URL, &two, &carol).await.unwrap();
        assert_eq!(
            pending_count(&pool, carol.id).await.unwrap(),
            2,
            "both requests wait for carol"
        );
        assert_eq!(
            pending_count(&pool, one.id).await.unwrap(),
            0,
            "the requester's own queue is empty: the request is carol's to answer"
        );
        assert_eq!(
            pending_count(&pool, dave.id).await.unwrap(),
            0,
            "an account with no rows at all counts nothing"
        );

        let granted = accept(&pool, first.id).await.unwrap();
        assert_eq!(granted.state, FollowState::Accepted, "one is answered");
        assert_eq!(
            pending_count(&pool, carol.id).await.unwrap(),
            1,
            "and leaves the count"
        );

        let withdrawn = reject(&pool, second.id).await.unwrap();
        assert_eq!(withdrawn.state, FollowState::Pending, "one is refused");
        assert_eq!(
            pending_count(&pool, carol.id).await.unwrap(),
            0,
            "which empties the queue too"
        );

        let open = follow(&pool, TEST_BASE_URL, &dave, &carol).await.unwrap();
        assert_eq!(
            accept(&pool, open.id).await.unwrap().state,
            FollowState::Accepted,
            "a locked account can grant a request as well"
        );
        assert_eq!(
            pending_count(&pool, carol.id).await.unwrap(),
            0,
            "an accepted relationship never counts as pending"
        );

        let mirrored = rejected_relation(&pool, &erin, &carol).await;
        assert_eq!(
            mirrored.state,
            FollowState::Rejected,
            "a mirrored rejection sits in the table as `rejected`"
        );
        assert_eq!(
            pending_count(&pool, carol.id).await.unwrap(),
            0,
            "and is not a request waiting for an answer"
        );
    }

    #[tokio::test]
    async fn the_table_constraints_are_the_ones_the_model_relies_on() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let recorded = follow(&pool, TEST_BASE_URL, &alice, &bob)
            .await
            .expect("the first relationship is recorded");
        assert_eq!(
            recorded.state,
            FollowState::Accepted,
            "the model's own write goes through the constraints"
        );

        let duplicate =
            sqlx::query("INSERT INTO follows (id, follower_id, followee_id) VALUES ($1, $2, $3)")
                .bind(new_id())
                .bind(alice.id)
                .bind(bob.id)
                .execute(&pool)
                .await
                .expect_err("one row per pair");
        assert_eq!(
            sqlstate_of(duplicate),
            "23505",
            "the pair is unique, which is what follow()'s idempotency builds on"
        );

        let yourself =
            sqlx::query("INSERT INTO follows (id, follower_id, followee_id) VALUES ($1, $2, $2)")
                .bind(new_id())
                .bind(alice.id)
                .execute(&pool)
                .await
                .expect_err("nobody follows themselves");
        assert_eq!(
            sqlstate_of(yourself),
            "23514",
            "and the check constraint makes it impossible in the table too"
        );

        let erin = local_user(&pool, "erin").await;
        let unknown_state = sqlx::query("INSERT INTO follows (id, follower_id, followee_id, state) VALUES ($1, $2, $3, 'maybe')")
            .bind(new_id())
            .bind(alice.id)
            .bind(erin.id)
            .execute(&pool)
            .await
            .expect_err("only the three states exist");
        assert_eq!(
            sqlstate_of(unknown_state),
            "23514",
            "the state check lists exactly the enum's spellings"
        );

        let deleted = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(alice.id)
            .execute(&pool)
            .await
            .expect("the follower is deleted");
        assert_eq!(deleted.rows_affected(), 1, "one account was removed");
        assert!(
            get(&pool, alice.id, bob.id).await.unwrap().is_none(),
            "deleting an account takes its relationships with it"
        );
    }

    #[tokio::test]
    async fn helpers_classify_the_target_and_mint_activity_ids() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let carol = locked_user(&pool, "carol").await;
        let bob = remote_user(&pool, "bob").await;

        assert_eq!(
            initial_state(&alice),
            FollowState::Accepted,
            "an unlocked account this instance owns is answered at once"
        );
        assert_eq!(
            initial_state(&carol),
            FollowState::Pending,
            "a locked account answers in its own time"
        );
        assert_eq!(
            initial_state(&bob),
            FollowState::Pending,
            "a remote account cannot be answered here at all"
        );

        let minted = local_activity_id(TEST_BASE_URL, &alice).expect("a local follower gets an id");
        let other = local_activity_id(TEST_BASE_URL, &alice).expect("and gets one again");
        assert_ne!(minted, other, "every `Follow` activity is its own object");
        assert!(
            minted.starts_with(&format!("{TEST_BASE_URL}{ACTIVITY_PREFIX}")),
            "under this instance's activities path: {minted}"
        );
        assert!(
            local_activity_id(TEST_BASE_URL, &bob).is_none(),
            "a remote follower's id is the one its own instance minted"
        );
    }

    /// [`follow`] refuses a pair with a block between them in either
    /// direction, with the 403 and the wording this ticket pins, and every
    /// other pair's follows are untouched by it.
    #[tokio::test]
    async fn a_blocked_pair_cannot_follow() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let carol = local_user(&pool, "carol").await;

        assert!(
            !blocked_between(&pool, &alice, &bob).await.unwrap(),
            "no pair is blocked until somebody blocks somebody"
        );
        let granted = follow(&pool, TEST_BASE_URL, &alice, &bob)
            .await
            .expect("an unblocked pair still follows");
        assert_eq!(
            granted.state,
            FollowState::Accepted,
            "and the follow goes through"
        );

        block_pair(&pool, &bob, &alice).await;
        let refused = follow(&pool, TEST_BASE_URL, &alice, &carol)
            .await
            .expect("alice may still follow someone else");
        assert_eq!(
            refused.state,
            FollowState::Accepted,
            "the guard is per pair, not global"
        );

        let inbound = follow(&pool, TEST_BASE_URL, &alice, &bob)
            .await
            .expect_err("the target's block refuses the follow");
        assert!(
            matches!(inbound, AppError::Forbidden(_)),
            "a block is a 403, the way Pleroma's fallback renders it"
        );
        assert_eq!(
            inbound.to_string(),
            "Could not follow user: bob is blocking you.",
            "the message names the target who blocks"
        );

        block_pair(&pool, &alice, &carol).await;
        let outbound = follow(&pool, TEST_BASE_URL, &alice, &carol)
            .await
            .expect_err("the follower's own block refuses the follow");
        assert!(
            matches!(outbound, AppError::Forbidden(_)),
            "the other direction is a 403 too"
        );
        assert_eq!(
            outbound.to_string(),
            "Could not follow user: You are blocking carol.",
            "and it is worded for the follower's own block"
        );

        let blockers_own = follow(&pool, TEST_BASE_URL, &bob, &alice)
            .await
            .expect_err("bob's own block of alice refuses his follow of her");
        assert_eq!(
            blockers_own.to_string(),
            "Could not follow user: You are blocking alice.",
            "the same pair's other direction is worded for the blocker"
        );
    }

    /// [`blocked_error`] names the target's block when a pair blocked each
    /// other both ways: the answer the blocked follower gets is the one about
    /// the account that holds the block over them.
    #[tokio::test]
    async fn a_mutually_blocked_pair_hears_the_incoming_block_first() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        block_pair(&pool, &alice, &bob).await;
        block_pair(&pool, &bob, &alice).await;

        let alice_hears = blocked_error(&pool, &alice, &bob).await.unwrap();
        assert_eq!(
            alice_hears.to_string(),
            "Could not follow user: bob is blocking you.",
            "bob's block of alice is the fact that answers her"
        );
        let bob_hears = blocked_error(&pool, &bob, &alice).await.unwrap();
        assert_eq!(
            bob_hears.to_string(),
            "Could not follow user: alice is blocking you.",
            "and alice's block of bob is the fact that answers him"
        );
    }

    #[test]
    fn a_missing_relationship_is_answered_not_found() {
        assert!(
            matches!(required(None), Err(AppError::NotFound)),
            "a row a concurrent write removed is a 404"
        );
        let stored = Follow {
            id: new_id(),
            follower_id: new_id(),
            followee_id: new_id(),
            state: FollowState::Accepted,
            ap_id: None,
            show_reblogs: true,
            notify: false,
            inserted_at: Utc::now(),
            updated_at: Utc::now(),
        };
        assert_eq!(
            required(Some(stored.clone())).unwrap(),
            stored,
            "a row that is there passes through"
        );
    }

    /// The move-follower query keeps exactly the accounts the move may
    /// carry: local, with an accepted follow of the moving account, and
    /// still allowing their following list to move. Remote mirrors,
    /// rejected relationships and opted-out followers stay out of the set.
    #[tokio::test]
    async fn move_followers_of_selects_local_accepted_permitters() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let mover = local_user(&pool, "mover").await;
        let carried = local_user(&pool, "carried").await;
        let opted_out = insert(
            &pool,
            &NewUser {
                allow_following_move: Flag::new(false),
                ..local_new("opted_out")
            },
        )
        .await
        .expect("the opted-out fixture inserts");
        let rejected = local_user(&pool, "rejected").await;
        let remote = remote_user(&pool, "remote_fan").await;

        let _accepted = follow(&pool, TEST_BASE_URL, &carried, &mover)
            .await
            .expect("the carried follow is accepted");
        let _opted_out = follow(&pool, TEST_BASE_URL, &opted_out, &mover)
            .await
            .expect("the opted-out follow is accepted");
        let _stale = rejected_relation(&pool, &rejected, &mover).await;
        let _remote = follow(&pool, TEST_BASE_URL, &remote, &mover)
            .await
            .expect("the remote follow is accepted");
        // An accepted local follow of someone else must not qualify either.
        let other = local_user(&pool, "other").await;
        let _unrelated = follow(&pool, TEST_BASE_URL, &carried, &other)
            .await
            .expect("the unrelated follow is accepted");

        assert_eq!(
            ids(&move_followers_of(&pool, mover.id).await.unwrap()),
            vec![carried.id],
            "only the local, accepted, non-opted-out follower of the mover"
        );
        assert!(
            move_followers_of(&pool, new_id()).await.unwrap().is_empty(),
            "an account nobody follows has an empty move set"
        );
    }

    #[tokio::test]
    async fn set_notify_writes_only_an_accepted_relationship() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let carol = locked_user(&pool, "carol").await;
        let accepted = follow(&pool, TEST_BASE_URL, &alice, &bob)
            .await
            .expect("the outgoing follow is granted");
        assert_eq!(accepted.state, FollowState::Accepted, "the fixture counts");
        let waiting = follow(&pool, TEST_BASE_URL, &alice, &carol)
            .await
            .expect("the request to the locked account waits");
        assert_eq!(waiting.state, FollowState::Pending, "the fixture waits");

        assert!(
            set_notify(&pool, alice.id, new_id(), true)
                .await
                .unwrap()
                .is_none(),
            "a pair with no row has nothing to toggle"
        );
        assert!(
            set_notify(&pool, alice.id, carol.id, true)
                .await
                .unwrap()
                .is_none(),
            "a request nobody answered cannot be subscribed to"
        );
        assert!(
            set_notify(&pool, bob.id, alice.id, true)
                .await
                .unwrap()
                .is_none(),
            "the other direction of the accepted pair has no row"
        );
        assert!(
            !relation(&pool, &alice, &carol).await.notify,
            "the refused writes stored nothing"
        );

        let subscribed = set_notify(&pool, alice.id, bob.id, true)
            .await
            .unwrap()
            .expect("the accepted row toggles");
        assert_eq!(subscribed.id, accepted.id, "the same row, updated in place");
        assert_eq!(
            subscribed.state,
            FollowState::Accepted,
            "toggling does not unfollow"
        );
        assert!(subscribed.notify, "the row comes back with the flag on");
        assert!(
            relation(&pool, &alice, &bob).await.notify,
            "and the column holds it"
        );
        assert_eq!(
            (
                fresh_user(&pool, bob.id).await.followers_count,
                fresh_user(&pool, alice.id).await.following_count
            ),
            (1, 1),
            "the toggle moves no counters"
        );

        let unsubscribed = set_notify(&pool, alice.id, bob.id, false)
            .await
            .unwrap()
            .expect("the flag goes back the same way");
        assert!(!unsubscribed.notify, "the row comes back with the flag off");
        assert!(
            !relation(&pool, &alice, &bob).await.notify,
            "and so does the stored column"
        );
        assert!(
            relation(&pool, &alice, &bob).await.show_reblogs,
            "the write touches only the notify toggle"
        );
    }

    /// The [`FollowToggles`] of a request that named both parameters, each on
    /// the non-default side of its column: reblogs hidden, notifications on.
    fn flipped() -> FollowToggles {
        FollowToggles {
            show_reblogs: Some(false),
            notify: Some(true),
        }
    }

    /// The [`FollowToggles`] of a request that named only `reblogs`.
    fn reblogs_only(show_reblogs: bool) -> FollowToggles {
        FollowToggles {
            show_reblogs: Some(show_reblogs),
            notify: None,
        }
    }

    /// The [`FollowToggles`] of a request that named only `notify`.
    fn notify_only(notify: bool) -> FollowToggles {
        FollowToggles {
            show_reblogs: None,
            notify: Some(notify),
        }
    }

    #[tokio::test]
    async fn set_toggles_writes_both_flags_on_an_open_relationship() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let carol = locked_user(&pool, "carol").await;
        let accepted = follow(&pool, TEST_BASE_URL, &alice, &bob)
            .await
            .expect("the outgoing follow is granted");
        assert!(accepted.show_reblogs, "the fresh row defaults to showing");
        assert!(!accepted.notify, "and to not notifying");
        let waiting = follow(&pool, TEST_BASE_URL, &alice, &carol)
            .await
            .expect("the request to the locked account waits");
        assert_eq!(waiting.state, FollowState::Pending, "the fixture waits");

        let hidden = set_toggles(&pool, alice.id, bob.id, flipped())
            .await
            .unwrap()
            .expect("the accepted row takes both flags");
        assert_eq!(hidden.id, accepted.id, "the same row, updated in place");
        assert_eq!(
            hidden.state,
            FollowState::Accepted,
            "the toggles do not unfollow"
        );
        assert!(!hidden.show_reblogs, "reblogs are hidden");
        assert!(hidden.notify, "and notifications are on");
        assert!(
            hidden.inserted_at <= hidden.updated_at,
            "the write refreshes `updated_at`"
        );
        let stored = relation(&pool, &alice, &bob).await;
        assert_eq!(
            (stored.show_reblogs, stored.notify),
            (false, true),
            "both columns hold"
        );
        assert_eq!(
            (
                fresh_user(&pool, bob.id).await.followers_count,
                fresh_user(&pool, alice.id).await.following_count
            ),
            (1, 1),
            "a toggle write moves no counters"
        );

        let requested = set_toggles(&pool, alice.id, carol.id, flipped())
            .await
            .unwrap()
            .expect("a pending request takes the toggles its follow asked for");
        assert_eq!(requested.state, FollowState::Pending, "and stays a request");
        assert!(
            !requested.show_reblogs && requested.notify,
            "the waiting row holds both flags"
        );
    }

    #[tokio::test]
    async fn set_toggles_writes_only_the_flags_the_request_named() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let accepted = follow(&pool, TEST_BASE_URL, &alice, &bob)
            .await
            .expect("the outgoing follow is granted");
        assert_eq!(accepted.state, FollowState::Accepted, "the fixture counts");

        let hidden = set_toggles(&pool, alice.id, bob.id, reblogs_only(false))
            .await
            .unwrap()
            .expect("the named flag writes");
        assert!(!hidden.show_reblogs, "reblogs are hidden");
        assert!(!hidden.notify, "the unnamed flag keeps its default");

        let subscribed = set_toggles(&pool, alice.id, bob.id, notify_only(true))
            .await
            .unwrap()
            .expect("the other flag writes");
        assert!(subscribed.notify, "notify is on");
        assert!(
            !subscribed.show_reblogs,
            "and the flag the request left out stands"
        );

        assert!(
            FollowToggles::default().is_empty(),
            "a request that named neither flag is the empty set"
        );
        assert!(
            !reblogs_only(true).is_empty() && !notify_only(false).is_empty(),
            "one named flag is not"
        );
        let untouched = set_toggles(&pool, alice.id, bob.id, FollowToggles::default())
            .await
            .unwrap()
            .expect("even the empty write matches the row");
        assert_eq!(
            (untouched.show_reblogs, untouched.notify),
            (false, true),
            "and it stores nothing"
        );
    }

    #[tokio::test]
    async fn set_toggles_finds_nothing_to_write_outside_an_open_pair() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let carol = locked_user(&pool, "carol").await;
        let dave = local_user(&pool, "dave").await;

        assert!(
            set_toggles(&pool, alice.id, new_id(), flipped())
                .await
                .unwrap()
                .is_none(),
            "a pair with no row has nothing to toggle"
        );
        let rejected = rejected_relation(&pool, &dave, &carol).await;
        assert!(
            set_toggles(&pool, dave.id, carol.id, flipped())
                .await
                .unwrap()
                .is_none(),
            "a mirrored rejection is nobody's open relationship"
        );
        let stored = relation(&pool, &dave, &carol).await;
        assert_eq!(stored.id, rejected.id, "the row is where it was");
        assert!(
            stored.show_reblogs && !stored.notify,
            "and the refused write stored nothing"
        );

        let _accepted = follow(&pool, TEST_BASE_URL, &alice, &bob)
            .await
            .expect("the outgoing follow is granted");
        assert!(
            set_toggles(&pool, bob.id, alice.id, flipped())
                .await
                .unwrap()
                .is_none(),
            "the other direction of the pair has no row of its own"
        );
        assert!(
            set_toggles(&pool, alice.id, new_id(), FollowToggles::default())
                .await
                .unwrap()
                .is_none(),
            "an unknown pair is the `None` answer whatever the toggles"
        );
    }

    #[tokio::test]
    async fn following_a_second_time_keeps_the_row_and_its_toggles() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let first = follow(&pool, TEST_BASE_URL, &alice, &bob)
            .await
            .expect("the first follow is granted");
        let stored = set_toggles(&pool, alice.id, bob.id, flipped())
            .await
            .unwrap()
            .expect("the toggles write");
        assert_eq!(stored.id, first.id, "the toggles ride the same row");

        let again = follow(&pool, TEST_BASE_URL, &alice, &bob)
            .await
            .expect("the repeat follow is idempotent");
        assert_eq!(again, stored, "it hands back the stored row as-is");
        assert_eq!(follow_count(&pool).await, 1, "and writes no second row");
        assert_eq!(
            fresh_user(&pool, bob.id).await.followers_count,
            1,
            "counting it twice is not following it twice"
        );
    }

    /// `owner`'s own outgoing follow requests, one page, unwrapped.
    async fn asks_from(pool: &PgPool, owner: &User, query: &str) -> Vec<FollowPageEntry> {
        outgoing_requests_from(pool, owner.id, page(query))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn outgoing_requests_are_the_callers_own_asks() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let carol = locked_user(&pool, "carol").await;
        let dave = locked_user(&pool, "dave").await;

        let to_carol = follow(&pool, TEST_BASE_URL, &alice, &carol)
            .await
            .expect("carol has to answer alice");
        let to_dave = follow(&pool, TEST_BASE_URL, &alice, &dave)
            .await
            .expect("so does dave");
        assert_eq!(to_carol.state, FollowState::Pending, "both wait");
        assert_eq!(to_dave.state, FollowState::Pending, "both wait");
        // Alice's granted follow of bob is a follow, not a request, and
        // carol's own request to dave is dave's queue to answer.
        let granted = follow(&pool, TEST_BASE_URL, &alice, &bob)
            .await
            .expect("bob answers on the spot");
        assert_eq!(granted.state, FollowState::Accepted, "the fixture counts");
        let incoming = follow(&pool, TEST_BASE_URL, &carol, &dave)
            .await
            .expect("carol asks dave too");
        assert_eq!(incoming.state, FollowState::Pending, "and that waits");

        let rows = vec![to_carol, to_dave];
        let newest_first = followed_newest_first(&rows);
        let newest_ids: Vec<Uuid> = by_age(&rows).iter().rev().map(|row| row.id).collect();

        assert_eq!(
            entry_ids(&asks_from(&pool, &alice, "").await),
            newest_first,
            "both of alice's asks, newest row first; the granted follow and \
             the incoming request stay out"
        );
        let first = asks_from(&pool, &alice, "limit=1").await;
        assert_eq!(
            entry_ids(&first),
            newest_first[..1].to_vec(),
            "one ask per page"
        );
        assert_eq!(
            follow_ids(&first),
            newest_ids[..1].to_vec(),
            "the entry carries the request row id the page keys on"
        );
        assert_eq!(
            entry_ids(&asks_from(&pool, &alice, &format!("max_id={}", newest_ids[0])).await),
            newest_first[1..].to_vec(),
            "max_id continues where the page ended"
        );
        assert!(
            asks_from(&pool, &bob, "").await.is_empty(),
            "an account that asked nobody has an empty list"
        );
        assert!(
            asks_from(&pool, &dave, "").await.is_empty(),
            "requests made to an account are not its own outgoing asks"
        );

        let answered = accept(&pool, newest_ids[1])
            .await
            .expect("the older ask is answered");
        assert_eq!(
            answered.state,
            FollowState::Accepted,
            "and the answer grants it"
        );
        let rest: Vec<Uuid> = entry_ids(&asks_from(&pool, &alice, "").await);
        assert_eq!(
            rest,
            newest_first[..1].to_vec(),
            "an answered ask leaves the outgoing list"
        );
    }

    /// Inserts a local user named `nickname` who hides their follow
    /// collection, the flag that keeps them out of a familiar set.
    async fn private_user(pool: &PgPool, nickname: &str) -> User {
        insert(
            pool,
            &NewUser {
                hide_follows: Flag::new(true),
                ..local_new(nickname)
            },
        )
        .await
        .expect("the private fixture inserts")
    }

    /// Inserts a local user named `nickname` and deactivates the row.
    async fn closed_user(pool: &PgPool, nickname: &str) -> User {
        let closed = local_user(pool, nickname).await;
        let gone = sqlx::query("UPDATE users SET is_active = false WHERE id = $1")
            .bind(closed.id)
            .execute(pool)
            .await
            .expect("the fixture deactivates");
        assert_eq!(gone.rows_affected(), 1, "the row was there to close");
        closed
    }

    /// The familiar followers of `target` from `viewer`'s side, uncapped for
    /// the tests that assert on the whole set.
    async fn familiar(pool: &PgPool, target: &User, viewer: &User) -> Vec<User> {
        familiar_followers(pool, target.id, viewer.id, 20)
            .await
            .expect("the familiar set reads")
    }

    /// Records `follower`'s follow of `target` through [`follow`], checking
    /// only that the edge is the pair it was asked for: the fixture form of
    /// the calls that need the relationship to exist and nothing else.
    async fn follow_each(pool: &PgPool, follower: &User, target: &User) {
        let row = follow(pool, TEST_BASE_URL, follower, target)
            .await
            .expect("the fixture follow records");
        assert_eq!(
            (row.follower_id, row.followee_id),
            (follower.id, target.id),
            "the row written is the pair asked for"
        );
    }

    /// Asserts the state `follower`'s edge to `target` is in.
    async fn assert_state(pool: &PgPool, follower: &User, target: &User, expected: FollowState) {
        assert_eq!(
            relation(pool, follower, target).await.state,
            expected,
            "the fixture edge is in the state the test assumes"
        );
    }

    #[tokio::test]
    async fn familiar_followers_is_the_intersection_of_the_two_follow_sets() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let target = local_user(&pool, "target").await;
        // Alice follows bob, and bob follows the target: the one name the
        // batch should answer with.
        let bob = local_user(&pool, "bob").await;
        // Carol follows the target but alice does not follow her.
        let carol = local_user(&pool, "carol").await;
        // Dave is alice's own follow, and of nobody relevant.
        let dave = local_user(&pool, "dave").await;
        for (follower, followed) in [
            (&alice, &bob),
            (&bob, &target),
            (&carol, &target),
            (&alice, &dave),
            (&target, &alice),
            (&alice, &target),
        ] {
            follow_each(&pool, follower, followed).await;
        }

        assert_eq!(
            ids(&familiar(&pool, &target, &alice).await),
            vec![bob.id],
            "only the account both sides follow is familiar"
        );
        assert_eq!(
            ids(&familiar(&pool, &target, &carol).await),
            Vec::<Uuid>::new(),
            "carol's own view answers nothing either: she follows the target, \
             who follows nobody of hers"
        );
        let mutual = ids(&familiar(&pool, &alice, &alice).await);
        assert_eq!(
            mutual,
            vec![target.id],
            "asked about the viewer themself the set is the mutuals — and \
             neither the viewer nor the target of a query is ever its own \
             familiar follower"
        );
    }

    #[tokio::test]
    async fn familiar_followers_skips_hidden_unanswered_and_closed_accounts() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let target = local_user(&pool, "target").await;
        let private = private_user(&pool, "private").await;
        let closed = closed_user(&pool, "closed").await;
        let waiting = locked_user(&pool, "waiting").await;
        let kept = local_user(&pool, "kept").await;

        for friend in [&private, &closed, &waiting, &kept] {
            follow_each(&pool, &alice, friend).await;
            follow_each(&pool, friend, &target).await;
        }
        assert_state(&pool, &alice, &waiting, FollowState::Pending).await;

        let seen = ids(&familiar(&pool, &target, &alice).await);
        assert_eq!(seen, vec![kept.id], "only the open, live, accepted follow");
    }

    #[tokio::test]
    async fn familiar_followers_answers_the_cap_as_the_head_of_the_set() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let target = local_user(&pool, "target").await;
        for nickname in ["one", "two", "three"] {
            let friend = local_user(&pool, nickname).await;
            follow_each(&pool, &alice, &friend).await;
            follow_each(&pool, &friend, &target).await;
        }

        let all = ids(&familiar(&pool, &target, &alice).await);
        assert_eq!(all.len(), 3, "three familiar followers");
        let capped = familiar_followers(&pool, target.id, alice.id, 2)
            .await
            .expect("a capped page reads");
        assert_eq!(
            ids(&capped),
            all[..2].to_vec(),
            "the cap cuts the tail off the same order, newest follow first"
        );
        assert!(
            familiar_followers(&pool, target.id, alice.id, 0)
                .await
                .expect("a zero cap reads")
                .is_empty(),
            "and a cap of nothing answers nothing"
        );
    }

    #[tokio::test]
    async fn subscribers_are_the_local_active_notify_flagged_followers() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let author = local_user(&pool, "author").await;
        let subscriber = local_user(&pool, "subscriber").await;
        let plain = local_user(&pool, "plain").await;

        // The subscribed local follower: the only one the read answers.
        let _open = follow(&pool, TEST_BASE_URL, &subscriber, &author)
            .await
            .expect("the subscriber's follow is granted");
        let _toggled = set_notify(&pool, subscriber.id, author.id, true)
            .await
            .expect("the toggle runs")
            .expect("the accepted pair takes the flag");
        // A plain accepted follow of the same author stays out.
        let _quiet = follow(&pool, TEST_BASE_URL, &plain, &author)
            .await
            .expect("the plain follow is granted");
        // A remote subscriber is told by federation, never by a row.
        let alien = remote_user(&pool, "alien").await;
        let _mirrored = follow(&pool, TEST_BASE_URL, &alien, &author)
            .await
            .expect("the remote follow is granted");
        let _alien_toggled = set_notify(&pool, alien.id, author.id, true)
            .await
            .expect("the remote toggle runs")
            .expect("the remote pair takes the flag");
        // A pending request whose toggle is on has not subscribed yet: the
        // lock refuses `set_notify`, so the toggles write stands in.
        let waiting = local_user(&pool, "waiting").await;
        let gated = locked_user(&pool, "gated").await;
        let _request = follow(&pool, TEST_BASE_URL, &waiting, &gated)
            .await
            .expect("the request waits");
        let _open_toggles = set_toggles(
            &pool,
            waiting.id,
            gated.id,
            FollowToggles {
                show_reblogs: None,
                notify: Some(true),
            },
        )
        .await
        .expect("the toggles run")
        .expect("the open pair takes them");
        assert!(
            relation(&pool, &waiting, &gated).await.notify,
            "the pending row carries the flag the read must still refuse"
        );
        // A deactivated local subscriber cannot be told either.
        let ghost = insert(
            &pool,
            &NewUser {
                is_active: Flag::new(false),
                ..local_new("ghost")
            },
        )
        .await
        .expect("the deactivated fixture inserts");
        let _ghosted = follow(&pool, TEST_BASE_URL, &ghost, &author)
            .await
            .expect("the deactivated follow is granted");
        let _ghost_toggled = set_notify(&pool, ghost.id, author.id, true)
            .await
            .expect("the ghost's toggle runs")
            .expect("its accepted pair takes the flag");

        assert_eq!(
            ids(&subscribers(&pool, author.id).await.expect("the read runs")),
            vec![subscriber.id],
            "only the local, active, granted, notified follower"
        );
        assert!(
            subscribers(&pool, gated.id)
                .await
                .expect("the locked account reads")
                .is_empty(),
            "a pending request has subscribed nobody yet"
        );
        assert!(
            subscribers(&pool, new_id())
                .await
                .expect("an unknown id reads")
                .is_empty(),
            "and an account nobody follows has no subscribers"
        );
    }

    /// The counted sweep, both directions and both states: the accepted
    /// relationships naming the account go and take their peers' counters
    /// down with them — the departing account's own two included, since it
    /// sits at the other end of every row it leaves — while the `pending`
    /// request of theirs goes without moving a number, and a relationship of
    /// two other accounts is nobody else's to touch.
    #[tokio::test]
    async fn delete_all_for_user_with_counts_takes_rows_and_counters() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let carol = local_user(&pool, "carol").await;
        let gated = locked_user(&pool, "gated").await;
        let _followed = follow(&pool, TEST_BASE_URL, &bob, &alice)
            .await
            .expect("bob follows alice");
        let _following = follow(&pool, TEST_BASE_URL, &alice, &carol)
            .await
            .expect("alice follows carol");
        let _between = follow(&pool, TEST_BASE_URL, &bob, &carol)
            .await
            .expect("bob follows carol");
        let _request = follow(&pool, TEST_BASE_URL, &alice, &gated)
            .await
            .expect("alice's request waits");
        assert_eq!(
            fresh_user(&pool, gated.id).await.followers_count,
            0,
            "a pending request counted nothing"
        );

        let removed = delete_all_for_user_with_counts(&pool, alice.id)
            .await
            .expect("the sweep runs");

        assert_eq!(removed, 3, "two accepted rows and one pending request");
        assert!(
            get(&pool, bob.id, alice.id).await.unwrap().is_none(),
            "bob's follow of alice is gone"
        );
        assert!(
            get(&pool, alice.id, carol.id).await.unwrap().is_none(),
            "and alice's of carol"
        );
        assert!(
            get(&pool, alice.id, gated.id).await.unwrap().is_none(),
            "and alice's waiting request"
        );
        assert!(
            get(&pool, bob.id, carol.id).await.unwrap().is_some(),
            "bob and carol keep their relationship"
        );
        assert_eq!(
            fresh_user(&pool, bob.id).await.following_count,
            1,
            "bob loses the count of the follow that went and keeps the one he has of carol"
        );
        assert_eq!(
            fresh_user(&pool, carol.id).await.followers_count,
            1,
            "and carol counts the follower she has left"
        );
        let departed = fresh_user(&pool, alice.id).await;
        assert_eq!(
            (departed.followers_count, departed.following_count),
            (0, 0),
            "the departing account's own counters describe what is left: nothing"
        );
        assert_eq!(
            fresh_user(&pool, gated.id).await.followers_count,
            0,
            "and the account that never granted a follow loses nothing"
        );
    }

    /// The sweep of an account nobody is related to removes nothing and
    /// changes no counter.
    #[tokio::test]
    async fn delete_all_for_user_with_counts_of_a_lonely_account_is_a_no_op() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;

        let removed = delete_all_for_user_with_counts(&pool, alice.id)
            .await
            .expect("the sweep runs");

        assert_eq!(removed, 0, "there was nothing to remove");
        assert_eq!(
            fresh_user(&pool, alice.id).await.followers_count,
            0,
            "and no counter moved"
        );
    }
}
