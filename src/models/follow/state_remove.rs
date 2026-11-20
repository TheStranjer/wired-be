//! Removing a relationship from the followed account's side:
//! [`remove_follower`] takes one follower back, [`sever_between`] takes the
//! open rows of a newly blocked pair, and [`uncount_removed_follower`] steps
//! down the counters every removed accepted row fed.

use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::{
    error::AppResult,
    models::user::{CountDelta, update_counts},
};

use super::types::{Follow, FollowState};

/// [`remove_follower`]: the follower is gone, and only an *accepted* row ever
/// counted, so the removed row is what decides whether the counters step down
/// and anything else in the table stays exactly as it was.
const REMOVE_FOLLOWER_SQL: &str = "DELETE FROM follows
      WHERE follower_id = $1 AND followee_id = $2 AND state = 'accepted'
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

/// Removes `follower_id`'s accepted follow of `user_id` — the followed
/// account, the row's [`Follow::followee_id`] — and returns the removed
/// relationship, `None` when the account had no such follower to remove.
///
/// The followee's side of [`unfollow`](super::unfollow) (Pleroma's `User.remove_follower`,
/// the write behind `remove_from_followers`): the same accepted relationship
/// taken back, but asked for by the account it points at instead of the one
/// that granted it. Only a follower is ever removed: a `pending` request
/// waits for its owner's own answer ([`accept`](super::accept) / [`reject`](super::reject), the
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
/// This is the follow-side half of a block: [`block`](crate::models::block) owns the transaction
/// and calls this inside it, so the block row and the relationships it
/// sever land (or vanish) together — Pleroma's `User.block` unfollowing the
/// pair and withdrawing both directions' pending requests before it stores
/// the block. Every removed row that had been *accepted* steps both
/// denormalised counters down through the module's
/// `uncount_removed_follower`, a `pending` request moves nothing, and a
/// mirrored `rejected` row is left exactly where it was, because it is
/// nobody's active relationship (see [`unfollow`](super::unfollow)'s same rule).
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        models::follow::{follow, get, test_support::*},
        util::id::new_id,
    };

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
}
