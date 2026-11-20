//! The keyset-paged relationship listings: [`followers_page`] and
//! [`following_page`] walk the accepted pairs, newest relationship first,
//! through the shared [`page_rows`].

use sqlx::{FromRow, PgPool, Postgres, QueryBuilder, postgres::PgRow};
use uuid::Uuid;

use crate::{error::AppResult, pagination::PageParams};

use super::types::FollowPageEntry;

/// The incoming side of [`followers_page`]: the users whose follow of the
/// bound account was accepted, each entry led by the `follow_id` of the
/// relationship row behind it — the id the page keys on. The text stops
/// where its one placeholder belongs, because that is where [`page_rows`]
/// pushes its bind and the builder is what numbers it `$1`; the keyset it
/// then appends runs on the relationship id `f.id`.
pub(super) const FOLLOWERS_PAGE_SQL: &str = "SELECT f.id AS follow_id, u.* FROM follows f
    JOIN users u ON u.id = f.follower_id
     WHERE f.state = 'accepted' AND f.followee_id = ";

/// The outgoing side of [`following_page`]: the users the bound account
/// follows, same shape as [`FOLLOWERS_PAGE_SQL`].
const FOLLOWING_PAGE_SQL: &str = "SELECT f.id AS follow_id, u.* FROM follows f
    JOIN users u ON u.id = f.followee_id
     WHERE f.state = 'accepted' AND f.follower_id = ";

/// The users who follow `user_id`, accepted relationships only, newest
/// relationship first and `page`'s `limit` at a time, each entry carrying
/// the [`Follow`](super::Follow) row's id next to the [`User`](crate::models::user::User) (see [`FollowPageEntry`]).
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
/// the [`Follow`](super::Follow) row's id next to the [`User`](crate::models::user::User) (see [`FollowPageEntry`]).
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

/// Runs one of the paged relationship lists: `sql` is one of the
/// `SELECT … FROM follows f …` prefixes, which stop at the placeholder this
/// binds `user_id` to, so the cursors [`PageParams::apply_keyset`] appends are
/// numbered after it; the page comes back newest relationship first
/// ([`PageParams::finish`] flips a `min_id` fetch). `T` is the row shape the
/// statement selects: a bare [`User`] or a [`FollowPageEntry`] with the
/// relationship id.
pub(super) async fn page_rows<T>(
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
    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        models::{
            follow::{Follow, FollowState, follow, test_support::*},
            user::User,
        },
    };

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
}
