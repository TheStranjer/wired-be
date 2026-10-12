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
//! goes away. [`follow`], [`accept`], [`unfollow`] and [`reject`] are the
//! only ways either happens, and each moves the counters exactly once per
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
//! [`Relationship`](crate::entities::relationship::Relationship) entity, and
//! [`set_notify`] is the only writer of `notify`: Mastodon's `follow?notify`
//! and Pleroma's `subscribe` / `unsubscribe` are the same one column.

use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgPool, Postgres, QueryBuilder, postgres::PgRow};
use uuid::Uuid;

use crate::{
    error::{AppError, AppResult},
    models::user::{CountDelta, User, update_counts},
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

/// [`accept`]: every not-yet-accepted state becomes accepted. A `rejected`
/// row never counted either, so both transitions are a step up, while an
/// already accepted row matches nothing here and is answered idempotently.
const ACCEPT_SQL: &str = "UPDATE follows
      SET state = 'accepted',
          updated_at = now()
    WHERE id = $1 AND state <> 'accepted'
  RETURNING *";

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

/// [`get`]: the one relationship of an ordered pair, whatever its state.
const GET_SQL: &str = "SELECT * FROM follows WHERE follower_id = $1 AND followee_id = $2";

/// [`by_id`]: the one relationship behind a primary key.
const BY_ID_SQL: &str = "SELECT * FROM follows WHERE id = $1";

/// [`following_ids`]: the accepted ones, oldest relationship first (`UUIDv7`
/// text sorts in creation order, decision 4).
const FOLLOWING_IDS_SQL: &str = "SELECT followee_id FROM follows
     WHERE follower_id = $1 AND state = 'accepted'
  ORDER BY id";

/// [`is_following`]: whether the pair has an accepted relationship.
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
/// step up only when this call turns the relationship accepted.
///
/// [`Config::base_url`]: crate::config::Config::base_url
///
/// # Errors
///
/// [`AppError::Unprocessable`] with `"Can not follow yourself"` when the two
/// users are the same account, or with Pleroma's "blocked you" message when
/// either has blocked the other (see `blocked_between` below); any database
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
    if blocked_between(follower, target) {
        return Err(blocked_error(target));
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
/// counters down again.
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
    Ok(true)
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
/// `…/unsubscribe` (`User.subscribe` / `User.unsubscribe`) write it here, and
/// Mastodon's `follow?notify` param writes the same flag on the relationship
/// it creates.
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
    let row = sqlx::query_as::<_, (bool,)>(IS_FOLLOWING_SQL)
        .bind(follower.id)
        .bind(target.id)
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
/// No pair is blocked yet: the relationship table that records blocks arrives
/// with the blocks ticket, which wires this hook up to it and keeps
/// [`follow`]'s refusal below from being the dead branch it is now. Until
/// then the answer is `false` for every pair.
const fn blocked_between(_follower: &User, _target: &User) -> bool {
    false
}

/// The refusal [`follow`] answers when [`blocked_between`] says the pair is
/// blocked: Pleroma's `deny_follow_blocked` message
/// (`lib/pleroma/user.ex`, `follow/3`), naming the target who blocked.
fn blocked_error(target: &User) -> AppError {
    AppError::Unprocessable(format!(
        "Could not follow user: {} blocked you.",
        target.nickname
    ))
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
        models::user::{Flag, NewUser, get_by_id, insert, local_ap_id},
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

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn following_an_open_local_user_is_accepted_and_counted(pool: PgPool) {
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

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn following_a_locked_user_waits_and_moves_nothing(pool: PgPool) {
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

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn accepting_grants_a_pending_request_once(pool: PgPool) {
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

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn rejecting_a_pending_request_removes_it(pool: PgPool) {
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

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn rejecting_an_accepted_relationship_uncounts_it(pool: PgPool) {
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

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn unfollowing_reports_what_it_removed(pool: PgPool) {
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

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn following_twice_is_the_same_relationship(pool: PgPool) {
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

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn following_again_reopens_a_rejected_request(pool: PgPool) {
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

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn following_yourself_is_refused(pool: PgPool) {
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

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn a_remote_followee_is_answered_by_its_own_instance(pool: PgPool) {
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

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn a_remote_follower_keeps_its_own_activity_id(pool: PgPool) {
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

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn accept_or_reject_of_an_unknown_id_is_not_found(pool: PgPool) {
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

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn following_ids_lists_accepted_follows_oldest_first(pool: PgPool) {
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

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn follower_lists_are_paged_newest_relationship_first(pool: PgPool) {
        let target = local_user(&pool, "target").await;
        let one = local_user(&pool, "one").await;
        let two = local_user(&pool, "two").await;
        let three = local_user(&pool, "three").await;
        for follower in [&one, &two, &three] {
            let granted = follow(&pool, TEST_BASE_URL, follower, &target)
                .await
                .expect("an open account answers every request");
            assert_eq!(
                granted.state,
                FollowState::Accepted,
                "each of these is granted at once"
            );
        }
        let rows = vec![
            relation(&pool, &one, &target).await,
            relation(&pool, &two, &target).await,
            relation(&pool, &three, &target).await,
        ];
        // Which of the three rows is "newest" is decided by their ids, not by
        // the order they were written in: UUIDv7 is only millisecond precise.
        let newest_first = followers_newest_first(&rows);
        let oldest_id = by_age(&rows).first().expect("three rows were read back").id;

        assert_eq!(
            entry_ids(&followers_page(&pool, target.id, page("")).await.unwrap()),
            newest_first,
            "the whole follower list, newest follow first"
        );
        let first_two = followers_page(&pool, target.id, page("limit=2"))
            .await
            .unwrap();
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
        let next = followers_page(
            &pool,
            target.id,
            page(&format!("limit=2&max_id={}", last_on_page.id)),
        )
        .await
        .unwrap();
        assert_eq!(
            entry_ids(&next),
            newest_first[2..].to_vec(),
            "max_id continues where the page ended"
        );
        let newer = followers_page(
            &pool,
            target.id,
            page(&format!("min_id={}", last_on_page.id)),
        )
        .await
        .unwrap();
        assert_eq!(
            entry_ids(&newer),
            newest_first[..1].to_vec(),
            "min_id reads the half after the cursor and is still answered newest-first"
        );
        assert!(
            followers_page(&pool, target.id, page(&format!("max_id={oldest_id}")))
                .await
                .unwrap()
                .is_empty(),
            "walking past the oldest row yields nothing"
        );
        assert!(
            followers_page(&pool, one.id, page(""))
                .await
                .unwrap()
                .is_empty(),
            "a follower of the list is not followed by anyone"
        );

        let mut outgoing = Vec::new();
        for followed in [&one, &two, &three] {
            outgoing.push(
                follow(&pool, TEST_BASE_URL, &target, followed)
                    .await
                    .expect("the same account follows back"),
            );
        }
        let wanted = followed_newest_first(&outgoing);
        assert_eq!(
            entry_ids(&following_page(&pool, target.id, page("")).await.unwrap()),
            wanted,
            "the following list runs the same way"
        );
        assert_eq!(
            entry_ids(
                &following_page(&pool, target.id, page("limit=2"))
                    .await
                    .unwrap()
            ),
            wanted[..2].to_vec(),
            "and is paginated on the relationship id too"
        );
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn pending_requests_are_paged_and_answered_away(pool: PgPool) {
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

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn pending_counts_the_requests_one_account_has_to_answer(pool: PgPool) {
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

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn the_table_constraints_are_the_ones_the_model_relies_on(pool: PgPool) {
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

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn helpers_classify_the_target_and_mint_activity_ids(pool: PgPool) {
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

        assert!(
            !blocked_between(&alice, &bob) && !blocked_between(&bob, &alice),
            "no pair is blocked until the blocks ticket wires the hook up"
        );
        let err = blocked_error(&bob);
        assert!(
            matches!(err, AppError::Unprocessable(_)),
            "a block is refused with a 422, the way the follow itself is"
        );
        assert_eq!(
            err.to_string(),
            "Could not follow user: bob@remote.example blocked you.",
            "Pleroma's wording, naming who blocked"
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
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn move_followers_of_selects_local_accepted_permitters(pool: PgPool) {
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

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn set_notify_writes_only_an_accepted_relationship(pool: PgPool) {
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

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn outgoing_requests_are_the_callers_own_asks(pool: PgPool) {
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
            entry_ids(
                &outgoing_requests_from(&pool, alice.id, page(""))
                    .await
                    .unwrap()
            ),
            newest_first,
            "both of alice's asks, newest row first; the granted follow and \
             the incoming request stay out"
        );
        let first = outgoing_requests_from(&pool, alice.id, page("limit=1"))
            .await
            .unwrap();
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
            entry_ids(
                &outgoing_requests_from(
                    &pool,
                    alice.id,
                    page(&format!("max_id={}", newest_ids[0]))
                )
                .await
                .unwrap()
            ),
            newest_first[1..].to_vec(),
            "max_id continues where the page ended"
        );
        assert!(
            outgoing_requests_from(&pool, bob.id, page(""))
                .await
                .unwrap()
                .is_empty(),
            "an account that asked nobody has an empty list"
        );
        assert!(
            outgoing_requests_from(&pool, dave.id, page(""))
                .await
                .unwrap()
                .is_empty(),
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
        let rest: Vec<Uuid> = entry_ids(
            &outgoing_requests_from(&pool, alice.id, page(""))
                .await
                .unwrap(),
        );
        assert_eq!(
            rest,
            newest_first[..1].to_vec(),
            "an answered ask leaves the outgoing list"
        );
    }
}
