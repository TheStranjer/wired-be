//! The reaction removals: one account's one emoji, and the sweep of
//! everything an account reacted with.

use sqlx::PgPool;
use uuid::Uuid;

use super::EmojiReaction;
use crate::{error::AppResult, util::emoji::canonical};

/// [`unreact_in_domain`]: one account's one emoji on one status; a pair that
/// never reacted (or already withdrew) deletes nothing, so withdrawal is
/// idempotent. The emoji is its text *and* its origin domain (`$4`, `NULL` for
/// a unicode or local emoji). `RETURNING *` hands the caller the row that
/// went, which is what the write hook reports.
const UNREACT_SQL: &str = "DELETE FROM emoji_reactions
      WHERE user_id = $1 AND status_id = $2 AND emoji = $3
        AND emoji_domain IS NOT DISTINCT FROM $4
   RETURNING *";

/// Removes `user_id`'s reaction `emoji` from `status_id`, reporting the row
/// that went.
///
/// Withdrawing a reaction that is not there — never made, or already withdrawn
/// — removes nothing and is not an error, so the write is idempotent; the
/// answer is `None` either way and the caller tells a withdrawal from a no-op
/// by it.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn unreact(
    pool: &PgPool,
    user_id: Uuid,
    status_id: Uuid,
    emoji: &str,
) -> AppResult<Option<EmojiReaction>> {
    unreact_in_domain(pool, user_id, status_id, emoji, None).await
}

/// [`unreact`] for the custom emoji of another instance: removes the reaction
/// `emoji` of origin `domain` (`None`: a unicode or local emoji, which is what
/// [`unreact`] withdraws).
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn unreact_in_domain(
    pool: &PgPool,
    user_id: Uuid,
    status_id: Uuid,
    emoji: &str,
    domain: Option<&str>,
) -> AppResult<Option<EmojiReaction>> {
    let key = canonical(emoji);
    let removed = sqlx::query_as::<_, EmojiReaction>(UNREACT_SQL)
        .bind(user_id)
        .bind(status_id)
        .bind(&*key)
        .bind(domain)
        .fetch_optional(pool)
        .await?;
    Ok(removed)
}

/// The `DELETE` behind [`delete_all_for_user`]: the account's own reactions.
const DELETE_ALL_FOR_USER_SQL: &str = "DELETE FROM emoji_reactions WHERE user_id = $1";

/// Removes every reaction made by user `user_id`: the reaction step of the
/// [`users.delete_remote`](crate::jobs::deletion::KIND_DELETE_REMOTE) job.
///
/// That job keeps the account's row, so it cannot rely on the `user_id`
/// foreign key's cascade the way a hard delete does.
///
/// Reactions *on* the account's statuses are other accounts' rows and stay:
/// their targets are tombstones by the time this runs, and every read filters
/// those out, exactly as [`crate::models::favourite::delete_all_for_user`]
/// leaves the likes of a purged account's posts.
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

#[cfg(test)]
mod tests {
    use super::super::{
        NewReaction,
        read::reactors,
        test_support::{
            BASE, CAT, CAT_URL, HEART, REMOTE_CAT, REMOTE_DOMAIN, THUMBS, post, post_by,
            react_remote, react_with, stored, user,
        },
        write::react,
    };
    use super::*;
    use crate::error::AppError;

    #[tokio::test]
    async fn unreacting_removes_the_row_and_is_idempotent() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let status_id = post_by(&pool, "bob").await;
        react_with(&pool, &alice, status_id, THUMBS, None).await;

        let removed = unreact(&pool, alice.id, status_id, THUMBS)
            .await
            .expect("the withdrawal runs")
            .expect("the reaction was there");
        assert_eq!(removed.user_id, alice.id, "the row that went");
        assert_eq!(removed.emoji, THUMBS, "and its emoji");

        assert!(
            unreact(&pool, alice.id, status_id, THUMBS)
                .await
                .expect("repeating a withdrawal runs")
                .is_none(),
            "withdrawing what is already gone removes nothing"
        );
        assert!(
            unreact(&pool, alice.id, status_id, ":never:")
                .await
                .expect("an unreacted emoji runs")
                .is_none(),
            "and an emoji that was never reacted with is no error either"
        );
        assert!(stored(&pool).await.is_empty(), "the table is empty again");
    }

    #[tokio::test]
    async fn unreacting_a_remote_emoji_needs_its_domain() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let status_id = post_by(&pool, "bob").await;
        let _row = react_remote(&pool, &alice, status_id).await;

        assert!(
            unreact(&pool, alice.id, status_id, REMOTE_CAT)
                .await
                .expect("the local withdrawal runs")
                .is_none(),
            "the local emoji of that text is not the remote one"
        );
        let removed =
            unreact_in_domain(&pool, alice.id, status_id, REMOTE_CAT, Some(REMOTE_DOMAIN))
                .await
                .expect("the remote withdrawal runs")
                .expect("the remote reaction was there");
        assert_eq!(
            removed.emoji_domain.as_deref(),
            Some(REMOTE_DOMAIN),
            "the row that went names its origin"
        );
        assert!(stored(&pool).await.is_empty(), "the table is empty again");
    }

    #[tokio::test]
    async fn an_unknown_status_or_a_deleted_row_refuses_or_cascades() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;

        let err = react(
            &pool,
            BASE,
            &NewReaction {
                user_id: alice.id,
                status_id: Uuid::now_v7(),
                emoji: THUMBS,
                url: None,
            },
        )
        .await
        .expect_err("a status that does not exist cannot be reacted to");
        assert!(
            matches!(err, AppError::Database(_)),
            "the foreign key refuses, not a silent no-op: {err}"
        );

        let status_id = post_by(&pool, "bob").await;
        react_with(&pool, &alice, status_id, THUMBS, None).await;
        let gone = sqlx::query("DELETE FROM statuses WHERE id = $1")
            .bind(status_id)
            .execute(&pool)
            .await
            .expect("the fixture status deletes")
            .rows_affected();
        assert_eq!(gone, 1, "the post was there to delete");
        assert!(
            stored(&pool).await.is_empty(),
            "deleting the status cascaded to its reaction"
        );

        let mine = post_by(&pool, "carol").await;
        let me = user(&pool, "dave").await;
        react_with(&pool, &me, mine, THUMBS, None).await;
        let erased = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(me.id)
            .execute(&pool)
            .await
            .expect("the fixture user deletes")
            .rows_affected();
        assert_eq!(erased, 1, "the reactor was there to delete");
        assert!(
            stored(&pool).await.is_empty(),
            "deleting the reactor cascaded to their reaction too"
        );
    }

    /// The sweep of an account whose row is kept: its own reactions go,
    /// another account's reaction on the same status stays, and an account
    /// that never reacted removes nothing.
    #[tokio::test]
    async fn delete_all_for_user_takes_only_the_accounts_own_reactions() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let status = post(&pool, &alice).await;
        react_with(&pool, &alice, status, THUMBS, None).await;
        react_with(&pool, &alice, status, HEART, None).await;
        react_with(&pool, &bob, status, CAT, Some(CAT_URL)).await;

        let removed = delete_all_for_user(&pool, alice.id)
            .await
            .expect("the sweep runs");

        assert_eq!(removed, 2, "both of alice's reactions went");
        let left = reactors(&pool, status, None, None)
            .await
            .expect("the reactors read");
        assert_eq!(left.len(), 1, "only the custom emoji is left");
        assert_eq!(
            left[0].reactor_ids,
            vec![bob.id],
            "and it is bob's row on alice's status"
        );
        assert_eq!(
            delete_all_for_user(&pool, alice.id)
                .await
                .expect("the rerun runs"),
            0,
            "and a second pass removes nothing"
        );
    }
}
