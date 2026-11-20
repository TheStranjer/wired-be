//! Answering a follow request: [`accept`] grants it and [`reject`]
//! withdraws it, each stepping the denormalised counters for the transition
//! they make.

use sqlx::PgPool;
use uuid::Uuid;

use crate::error::AppResult;

use super::{
    counters::step_counters,
    lookup::{by_id, required},
    types::{Follow, FollowState},
};

/// [`accept`]: every not-yet-accepted state becomes accepted. A `rejected`
/// row never counted either, so both transitions are a step up, while an
/// already accepted row matches nothing here and is answered idempotently.
pub(super) const ACCEPT_SQL: &str = "UPDATE follows
      SET state = 'accepted',
          updated_at = now()
    WHERE id = $1 AND state <> 'accepted'
  RETURNING *";

/// [`reject`]: the request is gone, and the removed row comes back.
const REJECT_SQL: &str = "DELETE FROM follows WHERE id = $1 RETURNING *";

/// Grants a follow request and returns the accepted relationship.
///
/// Any not-yet-accepted row becomes [`FollowState::Accepted`] and steps the
/// counters up; calling it again on an accepted row returns that row and
/// moves nothing, because only the transition into accepted counts.
///
/// # Errors
///
/// [`AppError::NotFound`](crate::error::AppError::NotFound) when no follow has this id (anymore: a concurrent
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
/// `follow_reject`, which is an unfollow) steps them down like [`unfollow`](super::unfollow)
/// does.
///
/// # Errors
///
/// [`AppError::NotFound`](crate::error::AppError::NotFound) when no follow has this id; any other database
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        error::AppError,
        models::follow::{
            follow, followers_page, following_page, get, is_following, pending_requests_for,
            test_support::*, unfollow,
        },
        util::id::new_id,
    };

    #[tokio::test]
    async fn accepting_grants_a_pending_request_once() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
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

    #[tokio::test]
    async fn rejecting_a_pending_request_removes_it() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
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

    #[tokio::test]
    async fn rejecting_an_accepted_relationship_uncounts_it() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
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

    #[tokio::test]
    async fn accept_or_reject_of_an_unknown_id_is_not_found() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
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
}
