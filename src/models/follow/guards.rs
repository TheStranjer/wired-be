//! The refusals [`follow`](super::follow) answers before it writes anything:
//! the self-follow message, the block guard and its wording.

use sqlx::PgPool;

use crate::{
    error::{AppError, AppResult},
    models::{block, user::User},
};

/// 422 message [`follow`] answers when an account asks to follow itself,
/// Pleroma's wording for that case.
pub(super) const SELF_FOLLOW: &str = "Can not follow yourself";

/// Whether `follower` and `target` have blocked each other, in either
/// direction.
///
/// The hook [`follow`] refuses through: [`block::blocked_between`] is the
/// `blocks` table's own answer (decision 6), asked once per pair, so a
/// blocked pair never reaches the write below.
pub(super) async fn blocked_between(
    pool: &PgPool,
    follower: &User,
    target: &User,
) -> AppResult<bool> {
    block::blocked_between(pool, follower.id, target.id).await
}

/// The refusal [`follow`] answers when [`blocked_between`] says the pair is
/// blocked: Pleroma's `deny_follow_blocked` message
/// (`lib/pleroma/user.ex`, `follow/3`) in this ticket's wording, naming which
/// way the block runs — the target's block of the follower is stated first, so
/// a pair that blocked each other both ways hears the one that answers them.
pub(super) async fn blocked_error(
    pool: &PgPool,
    follower: &User,
    target: &User,
) -> AppResult<AppError> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        models::follow::{FollowState, follow, get, test_support::*},
        util::id::new_id,
    };

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
}
