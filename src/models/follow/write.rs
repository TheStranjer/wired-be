//! Writing a follow: [`follow`] itself and the two statements it runs when
//! the pair has no open row — [`insert_follow`] for a fresh pair, [`reopen`]
//! for a mirrored rejected one.

use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    error::{AppError, AppResult},
    models::user::User,
    util::id::{new_activity_ap_id, new_id},
};

use super::{
    counters::counted,
    guards::{SELF_FOLLOW, blocked_between, blocked_error},
    lookup::{by_id, get, required},
    types::{Follow, FollowState, initial_state},
};

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

/// Records that `follower` follows `target` — the followed account, the
/// row's [`Follow::followee_id`] — and returns the relationship.
///
/// The state is [`FollowState::Accepted`] right away when this instance owns
/// an unlocked target — the answer is ours to give — and
/// [`FollowState::Pending`] when the target is locked or remote, where
/// acceptance arrives later through an `Accept` activity ([`accept`](super::accept)). A
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
/// through [`set_toggles`](super::set_toggles) on top of the row this returns.
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
/// [`block::blocked_between`](crate::models::block::blocked_between) and [`block::is_blocking`](crate::models::block::is_blocking)). Any database
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
    // A remote follower's own `Follow` id is recorded by the federation layer.
    let ap_id = follower.local.get().then(|| new_activity_ap_id(base_url));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        models::follow::{following_ids, is_following, pending_requests_for, test_support::*},
        util::id::parse_id,
    };

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
}
