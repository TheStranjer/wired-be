//! Which lists hold an account: [`lists_containing`] answers it of one owner,
//! [`lists_following_member`] of the whole owner set at once — the joins the
//! membership table is made for, and the reads the streaming audience and the
//! account's `lists` endpoint run.

use sqlx::PgPool;
use uuid::Uuid;

use crate::error::AppResult;

use super::List;

/// [`lists_containing`]: the owner's lists that hold `member_id`, oldest
/// first — Pleroma's `get_lists_account_belongs`, expressed as the join
/// the membership table is made for.
const LISTS_CONTAINING_SQL: &str = "SELECT lists.* FROM lists
     JOIN list_members ON list_members.list_id = lists.id
     WHERE lists.user_id = $1 AND list_members.user_id = $2
     ORDER BY lists.id";

/// [`lists_following_member`]: every list that holds `member_id` as a
/// member *and* whose owner's follow of them is an accepted fact — the
/// same join [`lists_containing`] states per owner, stated for all owners
/// at once (decision 3's batched-read shape), so the streaming audience
/// names each of the member's lists without one query per owner.
const LISTS_FOLLOWING_MEMBER_SQL: &str = "SELECT lists.* FROM lists
     JOIN list_members ON list_members.list_id = lists.id
     JOIN follows ON follows.follower_id = lists.user_id
          AND follows.followee_id = list_members.user_id
     WHERE list_members.user_id = $1 AND follows.state = 'accepted'
     ORDER BY lists.id";

/// The lists `owner_id` owns that hold `member_id`, oldest first — what
/// `GET /api/v1/accounts/:id/lists` answers for its viewer.
///
/// A member of nobody's list, or of only lists of other owners, yields the
/// empty vector; whether `member_id` names a live user is the API's
/// question, not this query's.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn lists_containing(
    pool: &PgPool,
    owner_id: Uuid,
    member_id: Uuid,
) -> AppResult<Vec<List>> {
    let rows = sqlx::query_as::<_, List>(LISTS_CONTAINING_SQL)
        .bind(owner_id)
        .bind(member_id)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// Every list that holds `member_id` as a member while an **accepted**
/// follow of theirs by the list's owner is on the books.
///
/// The streaming audience's list side: the whole-owner-set statement of
/// the same fact [`lists_containing`] answers for one owner (the API's
/// membership rule is "the owner follows the member"; the join keeps a
/// follow that was since withdrawn from carrying memberships).
///
/// A member of nobody's list, or of only lists whose owners no longer
/// follow them, yields the empty vector; whether `member_id` names a live
/// user is the caller's question, not this query's. Oldest list first,
/// like every read here.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn lists_following_member(pool: &PgPool, member_id: Uuid) -> AppResult<Vec<List>> {
    let rows = sqlx::query_as::<_, List>(LISTS_FOLLOWING_MEMBER_SQL)
        .bind(member_id)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        models::{follow, list::test_support::*},
    };

    #[tokio::test]
    async fn lists_containing_answers_the_owners_membership_in_order() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let first = seed(&pool, &alice, "first").await;
        let _skipped: List = seed(&pool, &alice, "without").await;
        let last = seed(&pool, &alice, "last").await;
        add_member(&pool, &first, &bob).await;
        add_member(&pool, &last, &bob).await;

        let carol = user(&pool, "carol").await;
        let hers = seed(&pool, &carol, "carol's own").await;
        add_member(&pool, &hers, &bob).await;

        let found = lists_containing(&pool, alice.id, bob.id)
            .await
            .expect("the lookup reads");
        assert_eq!(
            titles(&found),
            ["first", "last"],
            "alice's two lists holding bob, oldest first; carol's list of \
             bob is not alice's, and alice's list without bob is no match"
        );

        let nolists = lists_containing(&pool, alice.id, carol.id)
            .await
            .expect("reading nobody's member is not a failure");
        assert!(
            nolists.is_empty(),
            "a member of no list yields the empty vector"
        );
    }

    /// [`lists_following_member`] walks every owner at once, and carries a
    /// list only while its owner's follow of the member is an accepted
    /// fact: a membership whose follow has gone delivers nothing.
    #[tokio::test]
    async fn lists_following_member_walks_every_following_owner() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let carol = user(&pool, "carol").await;

        let _grant = follow::follow(&pool, TEST_BASE_URL, &alice, &bob)
            .await
            .expect("alice follows bob");
        let first = seed(&pool, &alice, "first").await;
        let _without = seed(&pool, &alice, "without").await;
        let last = seed(&pool, &alice, "last").await;
        add_member(&pool, &first, &bob).await;
        add_member(&pool, &last, &bob).await;

        let stale = seed(&pool, &carol, "unfollowed").await;
        add_member(&pool, &stale, &bob).await;

        let found = lists_following_member(&pool, bob.id)
            .await
            .expect("the batched list read runs");
        assert_eq!(
            titles(&found),
            ["first", "last"],
            "both of alice's lists holding bob across the whole owner set, and not \
             carol's unfollowed membership or alice's membership-less list"
        );

        let _removed = follow::remove_follower(&pool, bob.id, alice.id)
            .await
            .expect("the follower removal runs")
            .expect("bob had alice as a follower");
        let after = lists_following_member(&pool, bob.id)
            .await
            .expect("the batched list read runs");
        assert!(
            after.is_empty(),
            "the memberships outlive the withdrawn follow, but no longer stream"
        );
    }
}
