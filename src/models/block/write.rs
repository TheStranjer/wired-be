//! The block model's writes: recording a block, which severs the pair's
//! follows, and lifting it.

use sqlx::PgPool;

use super::{
    Block, SELF_BLOCK, local_activity_id,
    read::{get, required},
};
use crate::{
    error::{AppError, AppResult},
    models::{follow, user::User},
    util::id::new_id,
};

/// [`block`]'s write when the pair has no row yet: the id this module mints
/// (decision 4) and the pair the caller names, `inserted_at` left to the
/// column default. `ON CONFLICT (blocker_id, blocked_id) DO NOTHING` is the
/// race guard: a concurrent block of the same pair wins, this statement writes
/// nothing and returns no row, and the winner's row is read back instead —
/// which is also why its severing must not run a second time.
const INSERT_SQL: &str = "INSERT INTO blocks (id, blocker_id, blocked_id, ap_id)
     VALUES ($1, $2, $3, $4)
ON CONFLICT (blocker_id, blocked_id) DO NOTHING
RETURNING *";

/// [`unblock`]: the pair's row, which comes back so the caller can report
/// whether a block was there to lift. A never-blocked (or already unblocked)
/// pair deletes nothing, so unblocking is idempotent.
const UNBLOCK_SQL: &str = "DELETE FROM blocks WHERE blocker_id = $1 AND blocked_id = $2
 RETURNING *";

/// Records that `blocker` blocks `target`, severing the pair's follows, and
/// returns the block.
///
/// The write is the whole fact this ticket stores: the row (idempotent on the
/// pair — an already blocked pair keeps its row, its id and its
/// [`Block::ap_id`]) and everything Pleroma's `User.block` drops with it, both
/// directions' accepted follows and both directions' pending requests, the
/// denormalised counters of [`crate::models::user::User::followers_count`] and
/// [`following_count`](crate::models::user::User::following_count) fixed for
/// every accepted row that goes. All of it is one transaction: the block row
/// and the follows it severs can never be seen apart.
///
/// `base_url` is the instance origin, i.e. [`Config::base_url`], which the
/// model layer is handed rather than reading configuration itself; a blocker
/// this instance owns mints a fresh `{base_url}/activities/{uuid}` for its
/// `Block` activity (decision 6), a remote one stores `None` until the
/// federation ticket mirrors the activity it arrived with.
///
/// [`Config::base_url`]: crate::config::Config::base_url
/// [`following_count`]: crate::models::user::User::following_count
///
/// # Errors
///
/// [`AppError::Unprocessable`] with `"Can not block yourself"` when the two
/// users are the same account; [`AppError::NotFound`] when a concurrent block
/// of the same pair won the key and then rolled back, leaving no row to read
/// from it. Any database failure (connection, constraint, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn block(
    pool: &PgPool,
    base_url: &str,
    blocker: &User,
    target: &User,
) -> AppResult<Block> {
    if blocker.id == target.id {
        return Err(AppError::Unprocessable(SELF_BLOCK.to_owned()));
    }
    match get(pool, blocker.id, target.id).await? {
        // Already blocked: this call changes nothing, and the pair's follows
        // went when the block was first written.
        Some(existing) => Ok(existing),
        None => write_block(pool, base_url, blocker, target).await,
    }
}

/// The write of one new block: the row and the severing it implies, in one
/// transaction, with the row coming back. A concurrent block of the same pair
/// that won the unique key instead is answered its row, unchanged and
/// unsevered twice, because that call owns the whole write.
async fn write_block(
    pool: &PgPool,
    base_url: &str,
    blocker: &User,
    target: &User,
) -> AppResult<Block> {
    let mut tx = pool.begin().await?;
    let created = sqlx::query_as::<_, Block>(INSERT_SQL)
        .bind(new_id())
        .bind(blocker.id)
        .bind(target.id)
        .bind(local_activity_id(base_url, blocker).as_deref())
        .fetch_optional(&mut *tx)
        .await?;
    let Some(block) = created else {
        // Nothing was written here: the winner's row is the answer, read on
        // this transaction's own connection so the fallback never needs a
        // second one, and the empty transaction ends with the connection
        // (`follow::remove_follower` does the same).
        return required(get(&mut *tx, blocker.id, target.id).await?);
    };
    follow::sever_between(&mut tx, blocker.id, target.id).await?;
    tx.commit().await?;
    Ok(block)
}

/// Lifts `blocker`'s block on `target`, reporting the removed row.
///
/// Unblocking does not restore anything: a severed follow stays severed, the
/// same way Pleroma's `User.unblock` only deletes the edge. A pair with no
/// block (or one already lifted) is the `None` answer and changes nothing, so
/// the write is idempotent.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn unblock(pool: &PgPool, blocker: &User, target: &User) -> AppResult<Option<Block>> {
    let removed = sqlx::query_as::<_, Block>(UNBLOCK_SQL)
        .bind(blocker.id)
        .bind(target.id)
        .fetch_optional(pool)
        .await?;
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::super::{
        read::{blocked_between, is_blocking},
        test_support::{
            block_count, fresh_user, local_user, locked_user, remote_user, seed_block, seed_follow,
            seed_unblock, sqlstate_of, stored_block,
        },
    };
    use super::*;
    use crate::{config::TEST_BASE_URL, models::follow::FollowState};

    #[tokio::test]
    async fn a_remote_blocker_mints_no_activity_id() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let carol = remote_user(&pool, "carol").await;
        let alice = local_user(&pool, "alice").await;

        let row = block(&pool, TEST_BASE_URL, &carol, &alice)
            .await
            .expect("the fixture block stores");
        assert!(
            row.ap_id.is_none(),
            "a remote blocker's id is the one its own instance minted"
        );
        assert!(
            stored_block(&pool, &carol, &alice).await.ap_id.is_none(),
            "and nothing was stored in its place"
        );
        assert!(
            is_blocking(&pool, carol.id, alice.id).await.unwrap(),
            "the fact itself is stored the same way"
        );
    }

    #[tokio::test]
    async fn blocking_twice_is_one_row_that_keeps_its_id() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let first = block(&pool, TEST_BASE_URL, &alice, &bob)
            .await
            .expect("the fixture block stores");

        let again = block(&pool, TEST_BASE_URL, &alice, &bob)
            .await
            .expect("the repeat block is not an error");
        assert_eq!(again, first, "the stored row is the answer");
        assert_eq!(block_count(&pool).await, 1, "and nothing was written twice");

        let erin = local_user(&pool, "erin").await;
        let other = block(&pool, TEST_BASE_URL, &alice, &erin)
            .await
            .expect("the fixture block stores");
        assert_ne!(
            other.ap_id, first.ap_id,
            "a new block mints a new activity id"
        );
    }

    /// The severing [`block`] owes the pair: both directions' accepted follows
    /// are gone and their counters stepped down, while another account's
    /// follow of the blocker and the blocker's own unrelated follow stand.
    #[tokio::test]
    async fn a_block_severs_both_follows_and_fixes_the_counts() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let carol = local_user(&pool, "carol").await;
        seed_follow(&pool, &alice, &bob).await;
        seed_follow(&pool, &bob, &alice).await;
        seed_follow(&pool, &carol, &bob).await;
        seed_follow(&pool, &alice, &carol).await;
        assert_eq!(
            fresh_user(&pool, bob.id).await.followers_count,
            2,
            "the fixtures count two followers"
        );

        seed_block(&pool, &alice, &bob).await;

        assert!(
            follow::get(&pool, alice.id, bob.id)
                .await
                .unwrap()
                .is_none(),
            "alice's follow of bob is gone"
        );
        assert!(
            follow::get(&pool, bob.id, alice.id)
                .await
                .unwrap()
                .is_none(),
            "and bob's follow of alice"
        );
        assert_eq!(
            (
                fresh_user(&pool, bob.id).await.followers_count,
                fresh_user(&pool, alice.id).await.followers_count,
                fresh_user(&pool, alice.id).await.following_count,
                fresh_user(&pool, bob.id).await.following_count
            ),
            (1, 0, 1, 0),
            "carol still follows bob and alice still follows carol: only the \
             severed relationships stepped their counts down"
        );
        assert_eq!(
            follow::following_ids(&pool, carol.id).await.unwrap(),
            vec![bob.id],
            "carol's follow stands"
        );
    }

    /// Both directions' *pending* requests go with the block too, and a
    /// request never counted, so no counter moves for it.
    #[tokio::test]
    async fn a_block_withdraws_pending_requests_both_ways() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = locked_user(&pool, "alice").await;
        let carol = locked_user(&pool, "carol").await;
        let from_alice = follow::follow(&pool, TEST_BASE_URL, &alice, &carol)
            .await
            .expect("alice asks carol");
        let from_carol = follow::follow(&pool, TEST_BASE_URL, &carol, &alice)
            .await
            .expect("and carol asks alice");
        assert_eq!(
            (from_alice.state, from_carol.state),
            (FollowState::Pending, FollowState::Pending),
            "two locked accounts answer their own requests"
        );

        seed_block(&pool, &alice, &carol).await;

        assert!(
            follow::get(&pool, alice.id, carol.id)
                .await
                .unwrap()
                .is_none(),
            "alice's request is withdrawn"
        );
        assert!(
            follow::get(&pool, carol.id, alice.id)
                .await
                .unwrap()
                .is_none(),
            "and carol's"
        );
        assert_eq!(
            (
                fresh_user(&pool, carol.id).await.followers_count,
                fresh_user(&pool, alice.id).await.followers_count
            ),
            (0, 0),
            "a request never counted, so nothing steps down"
        );
    }

    /// A mirrored `rejected` row is nobody's active relationship: the block
    /// leaves it in place, exactly as [`follow::unfollow`] does.
    #[tokio::test]
    async fn a_block_leaves_a_rejected_row_alone() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let dave = local_user(&pool, "dave").await;
        let carol = local_user(&pool, "carol").await;
        let rejected = sqlx::query_as::<_, follow::Follow>(
            "INSERT INTO follows (id, follower_id, followee_id, state)
             VALUES ($1, $2, $3, 'rejected')
             RETURNING *",
        )
        .bind(new_id())
        .bind(dave.id)
        .bind(carol.id)
        .fetch_one(&pool)
        .await
        .unwrap();

        seed_block(&pool, &dave, &carol).await;

        assert_eq!(
            follow::get(&pool, dave.id, carol.id).await.unwrap(),
            Some(rejected),
            "the rejected row is still there, unchanged"
        );
    }

    #[tokio::test]
    async fn unblocking_reports_the_row_it_removed() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let carol = local_user(&pool, "carol").await;

        assert!(
            unblock(&pool, &alice, &bob).await.unwrap().is_none(),
            "a pair that was never blocked has nothing to lift"
        );

        let row = block(&pool, TEST_BASE_URL, &alice, &bob)
            .await
            .expect("the fixture block stores");
        let removed = unblock(&pool, &alice, &bob).await.unwrap();
        assert_eq!(
            removed.as_ref(),
            Some(&row),
            "the removed row comes back as it stood"
        );
        assert!(
            !is_blocking(&pool, alice.id, bob.id).await.unwrap(),
            "and the fact is gone"
        );
        assert!(
            !blocked_between(&pool, alice.id, bob.id).await.unwrap(),
            "in either direction"
        );
        assert!(
            unblock(&pool, &alice, &bob).await.unwrap().is_none(),
            "lifting it twice finds nothing the second time"
        );
        assert_eq!(block_count(&pool).await, 0, "and writes nothing");

        // Unblocking does not rewind the pair: it only lifts the guard, so a
        // follow that the block had severed is free to be granted again.
        seed_block(&pool, &alice, &carol).await;
        seed_unblock(&pool, &alice, &carol).await;
        let renewed = follow::follow(&pool, TEST_BASE_URL, &alice, &carol)
            .await
            .expect("a lifted block lets the follow through again");
        assert_eq!(
            renewed.state,
            FollowState::Accepted,
            "and it is an ordinary follow"
        );
    }

    #[tokio::test]
    async fn blocking_yourself_is_refused() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;

        let err = block(&pool, TEST_BASE_URL, &alice, &alice)
            .await
            .expect_err("nobody blocks themselves");
        assert!(
            matches!(err, AppError::Unprocessable(_)),
            "a self-block is the 422 the endpoint renders: {err}"
        );
        assert_eq!(err.to_string(), "Can not block yourself", "its wording");
        assert_eq!(block_count(&pool).await, 0, "and it stored nothing");
        assert!(
            !blocked_between(&pool, alice.id, alice.id).await.unwrap(),
            "the pair question agrees"
        );
    }

    #[tokio::test]
    async fn blocking_an_unknown_account_fails() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        // The endpoint hands the model rows it just resolved; this is the row
        // that vanished in between. The foreign key is the model's only
        // guard, so a pair that names no live account is a database error,
        // never a silently stored orphan.
        let gone = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(bob.id)
            .execute(&pool)
            .await
            .expect("the fixture target deletes");
        assert_eq!(gone.rows_affected(), 1, "one account was removed");

        let err = block(&pool, TEST_BASE_URL, &alice, &bob)
            .await
            .expect_err("the foreign key refuses");
        assert!(
            matches!(err, AppError::Database(_)),
            "a constraint failure is the database variant: {err}"
        );
        assert_eq!(block_count(&pool).await, 0, "nothing was stored");
    }

    /// The pair key and the unique activity id: the facts the model builds
    /// on, checked against the table that stores them.
    #[tokio::test]
    async fn the_table_constraints_are_the_ones_the_model_relies_on() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let row = block(&pool, TEST_BASE_URL, &alice, &bob)
            .await
            .expect("the fixture block stores");

        let duplicate =
            sqlx::query("INSERT INTO blocks (id, blocker_id, blocked_id) VALUES ($1, $2, $3)")
                .bind(new_id())
                .bind(alice.id)
                .bind(bob.id)
                .execute(&pool)
                .await
                .expect_err("one row per pair");
        assert_eq!(
            sqlstate_of(duplicate),
            "23505",
            "the pair is unique, which is what block()'s idempotency builds on"
        );

        let taken = sqlx::query(
            "INSERT INTO blocks (id, blocker_id, blocked_id, ap_id) VALUES ($1, $2, $3, $4)",
        )
        .bind(new_id())
        .bind(bob.id)
        .bind(alice.id)
        .bind(row.ap_id.as_deref())
        .execute(&pool)
        .await
        .expect_err("an activity id names one block");
        assert_eq!(
            sqlstate_of(taken),
            "23505",
            "the `ap_id` is unique, as decision 6 requires"
        );
    }
}
