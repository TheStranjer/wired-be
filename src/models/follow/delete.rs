//! Sweeping a departed account's rows: [`delete_all_for_user_with_counts`]
//! takes every relationship naming the account and steps the counters of the
//! peers it leaves behind, for local and remote deletions alike.

use sqlx::PgPool;
use uuid::Uuid;

use crate::error::AppResult;

use super::dependents::clear_all_for_user;

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
/// the other two states [`delete_all_for_user_with_counts`] sweeps up beside
/// the accepted rows — so nothing here owes a step.
const DELETE_REST_FOR_USER_SQL: &str =
    "DELETE FROM follows WHERE (follower_id = $1 OR followee_id = $1) AND state <> 'accepted'";

/// Removes every follow relationship that names user `user_id` *and* steps
/// the denormalised counters the accepted ones fed.
///
/// This is the relationship removal of both account deletions, a local one
/// ([`users.delete`](crate::jobs::deletion::KIND_DELETE)) and a remote one
/// ([`users.delete_remote`](crate::jobs::deletion::KIND_DELETE_REMOTE)).
/// The peers stay live and render their follower and following counts on
/// every account entity, so those counts have to describe what is left of
/// each of them — Pleroma's `unfollow` loop in `User.perform(:delete)`. Every
/// accepted row is worth one step, taken in the same transaction as
/// the delete so a crash cannot leave a counted relationship without its row
/// or a row without its counters — the pairing [`remove_follower`](super::remove_follower) and
/// [`sever_between`](super::sever_between) keep for the relationships they take. Both directions
/// go: the account's own follows and the follows *of* it, which is how the
/// local followers of a departed remote account stop following it. A
/// `pending` request and a mirrored `rejected` row go too, moving nothing.
/// The endorsements and list memberships that ride on those follows (anything
/// the account gave or received, in either direction) go in the same
/// transaction.
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
    clear_all_for_user(&mut tx, user_id).await?;
    tx.commit().await?;
    // `count(*)` over deleted rows is never negative, so this cannot fail.
    let accepted = u64::try_from(accepted).unwrap_or_default();
    Ok(accepted + rest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        models::follow::{follow, get, test_support::*},
    };

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
