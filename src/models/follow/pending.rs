//! The follow-request queues: [`pending_requests_for`] pages the requests
//! waiting on the bound account's answer, [`outgoing_requests_from`] its own
//! unanswered asks, and [`pending_count`] sizes the incoming queue.

use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    error::AppResult,
    pagination::{PageParams, keyset_page_by},
};

use super::types::FollowPageEntry;

/// The incoming side of [`pending_requests_for`]: the users still waiting for
/// the bound account to answer their follow request, each entry led by the
/// `follow_id` of the request row behind it — the id the page keys on, as in
/// [`FOLLOWERS_PAGE_SQL`]. The text stops where its one placeholder belongs,
/// because that is where [`keyset_page_by`] pushes its bind and the builder is what
/// numbers it `$1`; the keyset it then appends runs on the relationship id
/// `f.id`.
const PENDING_REQUESTS_SQL: &str = "SELECT f.id AS follow_id, u.* FROM follows f
    JOIN users u ON u.id = f.follower_id
     WHERE f.state = 'pending' AND u.is_active AND f.followee_id = ";

/// The outgoing side of [`outgoing_requests_from`]: the users whose follow of
/// the bound account is still waiting on their answer — the accounts a pending
/// row names as its `followee`, the mirror of [`PENDING_REQUESTS_SQL`] — each
/// entry led by the `follow_id` of the request row behind it, the id the page
/// keys on. The text stops where its one placeholder belongs, because that is
/// where [`keyset_page_by`] pushes its bind and the builder is what numbers it
/// `$1`; the keyset it then appends runs on the relationship id `f.id`.
const PENDING_OUTGOING_SQL: &str = "SELECT f.id AS follow_id, u.* FROM follows f
    JOIN users u ON u.id = f.followee_id
     WHERE f.state = 'pending' AND u.is_active AND f.follower_id = ";

/// [`pending_count`]: the same queue [`pending_requests_for`] pages through,
/// counted rather than listed. The `::int` cast is what makes `count(*)`'s
/// `bigint` arrive as the `i32` this layer's counters all are.
const PENDING_COUNT_SQL: &str = "SELECT count(*)::int FROM follows f
    JOIN users u ON u.id = f.follower_id
     WHERE f.followee_id = $1 AND f.state = 'pending' AND u.is_active";

/// The users waiting for `target_id` — the followed account, the row's
/// [`Follow::followee_id`](super::Follow::followee_id) — to answer their follow request, newest request
/// first and `page`'s `limit` at a time.
///
/// Like [`followers_page`](super::followers_page), each entry carries the [`Follow`](super::Follow) row's id next
/// to the [`User`](crate::models::user::User) (see [`FollowPageEntry`]): the relationship id — not the
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
    keyset_page_by(pool, PENDING_REQUESTS_SQL, target_id, "f.id", &page).await
}

/// The users `follower_id` has asked to follow and who have not answered: the
/// caller's own outgoing requests, newest request first and `page`'s `limit`
/// at a time.
///
/// Pleroma's `get_outgoing_follow_requests_query`, the list behind
/// `GET /api/v1/pleroma/outgoing_follow_requests`. It is the mirror of
/// [`pending_requests_for`], the other end of the same rows: here the bound
/// account is [`Follow::follower_id`](super::Follow::follower_id) rather than [`Follow::followee_id`](super::Follow::followee_id), so
/// these are requests to withdraw, not to answer. Like it, each entry carries
/// the [`Follow`](super::Follow) row's id next to the [`User`](crate::models::user::User) (see [`FollowPageEntry`]) and
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
    keyset_page_by(pool, PENDING_OUTGOING_SQL, follower_id, "f.id", &page).await
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        models::{
            follow::{
                Follow, FollowState, accept, follow, followers_page, reject, test_support::*,
            },
            user::{User, set_activation},
        },
    };

    /// A deactivated requester leaves the incoming queue and its count; a
    /// deactivated target leaves the requester's outgoing list.
    #[tokio::test]
    async fn deactivated_accounts_are_left_out_of_the_queues() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let carol = locked_user(&pool, "carol").await;
        let dave = locked_user(&pool, "dave").await;
        let active = local_user(&pool, "active").await;
        let gone = local_user(&pool, "gone").await;
        for requester in [&active, &gone] {
            let _row: Follow = follow(&pool, TEST_BASE_URL, requester, &carol)
                .await
                .unwrap();
        }
        let _ask: Follow = follow(&pool, TEST_BASE_URL, &active, &dave).await.unwrap();
        assert_eq!(
            pending_count(&pool, carol.id).await.unwrap(),
            2,
            "both wait"
        );

        set_activation(&pool, gone.id, false).await.unwrap();
        assert_eq!(
            entry_ids(
                &pending_requests_for(&pool, carol.id, page(""))
                    .await
                    .unwrap()
            ),
            vec![active.id],
            "the deactivated requester is absent from the queue"
        );
        assert_eq!(
            pending_count(&pool, carol.id).await.unwrap(),
            1,
            "and from its count"
        );

        set_activation(&pool, dave.id, false).await.unwrap();
        assert_eq!(
            entry_ids(&asks_from(&pool, &active, "").await),
            vec![carol.id],
            "a deactivated target is absent from the outgoing asks"
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
}
