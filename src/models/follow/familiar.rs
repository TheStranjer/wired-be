//! The familiar-follower intersection the
//! `GET /api/v1/accounts/familiar_followers` batch answers: of the accounts
//! a viewer follows, the ones that follow the target too.

use sqlx::PgPool;
use uuid::Uuid;

use crate::{error::AppResult, models::user::User};

/// [`familiar_followers`]: the accounts that follow `target` (`own`, the
/// target's own follower edges) and are themselves followed by the viewer
/// (`theirs`, the viewer's outgoing edges), accepted on both ends. Active
/// only, and without the ones who hide their follows — Pleroma's
/// `get_familiar_followers_query` adds `not u.hide_follows` to its friends
/// subquery for exactly that. `id` excludes the target and the viewer:
/// neither is a follower of the other in this sense. Newest follow of the
/// target first, `LIMIT $3` at a time.
const FAMILIAR_FOLLOWERS_SQL: &str = "SELECT u.* FROM users u
     JOIN follows own ON own.followee_id = $1 AND own.follower_id = u.id
     JOIN follows theirs ON theirs.follower_id = $2 AND theirs.followee_id = u.id
      WHERE own.state = 'accepted' AND theirs.state = 'accepted'
        AND u.is_active AND NOT u.hide_follows
        AND u.id <> $1 AND u.id <> $2
   ORDER BY own.id DESC
      LIMIT $3";
/// The accounts `viewer_id` follows that also follow `target_id` — the
/// intersection the `GET /api/v1/accounts/familiar_followers` batch answers,
/// newest follow of the target first, at most `limit` of them.
///
/// Pleroma's `User.get_familiar_followers(target, current_user)`
/// (`lib/pleroma/user.ex`) is the reference, including its two exclusions
/// built into the statement: the target and the viewer themselves are never
/// familiar followers of the target, and an account that hides its follows
/// ([`hide_follows`](crate::models::user::User::hide_follows)) never shows
/// up as one. That a *hidden follower list* also empties the whole set is
/// the endpoint's check, not this query's (Pleroma's controller asks it of
/// the target row before running the query at all, so the caller decides).
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn familiar_followers(
    pool: &PgPool,
    target_id: Uuid,
    viewer_id: Uuid,
    limit: i64,
) -> AppResult<Vec<User>> {
    let rows = sqlx::query_as::<_, User>(FAMILIAR_FOLLOWERS_SQL)
        .bind(target_id)
        .bind(viewer_id)
        .bind(limit)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        models::{
            follow::{FollowState, follow, test_support::*},
            user::{Flag, NewUser, insert},
        },
    };

    /// Inserts a local user named `nickname` who hides their follow
    /// collection, the flag that keeps them out of a familiar set.
    async fn private_user(pool: &PgPool, nickname: &str) -> User {
        insert(
            pool,
            &NewUser {
                hide_follows: Flag::new(true),
                ..local_new(nickname)
            },
        )
        .await
        .expect("the private fixture inserts")
    }

    /// Inserts a local user named `nickname` and deactivates the row.
    async fn closed_user(pool: &PgPool, nickname: &str) -> User {
        let closed = local_user(pool, nickname).await;
        let gone = sqlx::query("UPDATE users SET is_active = false WHERE id = $1")
            .bind(closed.id)
            .execute(pool)
            .await
            .expect("the fixture deactivates");
        assert_eq!(gone.rows_affected(), 1, "the row was there to close");
        closed
    }

    /// The familiar followers of `target` from `viewer`'s side, uncapped for
    /// the tests that assert on the whole set.
    async fn familiar(pool: &PgPool, target: &User, viewer: &User) -> Vec<User> {
        familiar_followers(pool, target.id, viewer.id, 20)
            .await
            .expect("the familiar set reads")
    }

    /// Records `follower`'s follow of `target` through [`follow`], checking
    /// only that the edge is the pair it was asked for: the fixture form of
    /// the calls that need the relationship to exist and nothing else.
    async fn follow_each(pool: &PgPool, follower: &User, target: &User) {
        let row = follow(pool, TEST_BASE_URL, follower, target)
            .await
            .expect("the fixture follow records");
        assert_eq!(
            (row.follower_id, row.followee_id),
            (follower.id, target.id),
            "the row written is the pair asked for"
        );
    }

    /// Asserts the state `follower`'s edge to `target` is in.
    async fn assert_state(pool: &PgPool, follower: &User, target: &User, expected: FollowState) {
        assert_eq!(
            relation(pool, follower, target).await.state,
            expected,
            "the fixture edge is in the state the test assumes"
        );
    }

    #[tokio::test]
    async fn familiar_followers_is_the_intersection_of_the_two_follow_sets() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let target = local_user(&pool, "target").await;
        // Alice follows bob, and bob follows the target: the one name the
        // batch should answer with.
        let bob = local_user(&pool, "bob").await;
        // Carol follows the target but alice does not follow her.
        let carol = local_user(&pool, "carol").await;
        // Dave is alice's own follow, and of nobody relevant.
        let dave = local_user(&pool, "dave").await;
        for (follower, followed) in [
            (&alice, &bob),
            (&bob, &target),
            (&carol, &target),
            (&alice, &dave),
            (&target, &alice),
            (&alice, &target),
        ] {
            follow_each(&pool, follower, followed).await;
        }

        assert_eq!(
            ids(&familiar(&pool, &target, &alice).await),
            vec![bob.id],
            "only the account both sides follow is familiar"
        );
        assert_eq!(
            ids(&familiar(&pool, &target, &carol).await),
            Vec::<Uuid>::new(),
            "carol's own view answers nothing either: she follows the target, \
             who follows nobody of hers"
        );
        let mutual = ids(&familiar(&pool, &alice, &alice).await);
        assert_eq!(
            mutual,
            vec![target.id],
            "asked about the viewer themself the set is the mutuals — and \
             neither the viewer nor the target of a query is ever its own \
             familiar follower"
        );
    }

    #[tokio::test]
    async fn familiar_followers_skips_hidden_unanswered_and_closed_accounts() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let target = local_user(&pool, "target").await;
        let private = private_user(&pool, "private").await;
        let closed = closed_user(&pool, "closed").await;
        let waiting = locked_user(&pool, "waiting").await;
        let kept = local_user(&pool, "kept").await;

        for friend in [&private, &closed, &waiting, &kept] {
            follow_each(&pool, &alice, friend).await;
            follow_each(&pool, friend, &target).await;
        }
        assert_state(&pool, &alice, &waiting, FollowState::Pending).await;

        let seen = ids(&familiar(&pool, &target, &alice).await);
        assert_eq!(seen, vec![kept.id], "only the open, live, accepted follow");
    }

    #[tokio::test]
    async fn familiar_followers_answers_the_cap_as_the_head_of_the_set() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let target = local_user(&pool, "target").await;
        for nickname in ["one", "two", "three"] {
            let friend = local_user(&pool, nickname).await;
            follow_each(&pool, &alice, &friend).await;
            follow_each(&pool, &friend, &target).await;
        }

        let all = ids(&familiar(&pool, &target, &alice).await);
        assert_eq!(all.len(), 3, "three familiar followers");
        let capped = familiar_followers(&pool, target.id, alice.id, 2)
            .await
            .expect("a capped page reads");
        assert_eq!(
            ids(&capped),
            all[..2].to_vec(),
            "the cap cuts the tail off the same order, newest follow first"
        );
        assert!(
            familiar_followers(&pool, target.id, alice.id, 0)
                .await
                .expect("a zero cap reads")
                .is_empty(),
            "and a cap of nothing answers nothing"
        );
    }
}
