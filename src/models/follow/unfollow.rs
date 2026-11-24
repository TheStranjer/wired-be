//! Taking a follow back: [`unfollow`] removes the open relationship of a
//! pair, steps the counters it fed and clears the endorsement and list
//! memberships that ride on it.

use sqlx::{PgConnection, PgPool};

use crate::{
    error::AppResult,
    models::{endorsement, user::User},
};

use super::{
    counters::step_counters,
    types::{Follow, FollowState},
};

/// [`unfollow`]: only an open relationship can be taken back, and the removed
/// row is what decides whether the counters step down.
pub(super) const UNFOLLOW_SQL: &str = "DELETE FROM follows
      WHERE follower_id = $1 AND followee_id = $2
        AND state IN ('pending', 'accepted')
  RETURNING *";

/// [`unfollow`]: the memberships the unfollowing account kept of the account
/// they just stopped following. An account in someone's list is an account
/// they follow (the rule [`crate::api::lists`] enforces on the way in), so
/// taking the follow back takes those rows with it: the delete runs over the
/// list's *owner*, which is why the follower's rows in other people's lists
/// are untouched.
const UNFOLLOW_LIST_MEMBERSHIPS_SQL: &str = "DELETE FROM list_members AS lm
     USING lists AS l
     WHERE l.id = lm.list_id AND l.user_id = $1 AND lm.user_id = $2";

/// Removes `follower`'s follow of `target`, reporting whether an open
/// relationship was there to remove.
///
/// A `pending` or `accepted` row is deleted; a `rejected` row (see
/// [`FollowState::Rejected`]) and an unknown pair are answered `false` and
/// change nothing. Deleting an accepted relationship steps both denormalised
/// counters down again. An endorsement of `target` by `follower` goes with
/// the relationship: an endorsement is a pinned *follow*, and unfollowing
/// clears it (the same pair of facts Pleroma's `unfollow` drops together). So
/// does every membership of `target` in `follower`'s own lists: a list member
/// is a followed account, so unfollowing ends the membership too
/// — the memberships of other owners' lists are nobody else's to touch, and
/// stay.
///
/// # Errors
///
/// Any database failure (connection, constraint, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn unfollow(pool: &PgPool, follower: &User, target: &User) -> AppResult<bool> {
    let mut tx = pool.begin().await?;
    let removed = unfollow_conn(&mut tx, follower, target).await?;
    tx.commit().await?;
    Ok(removed)
}

/// [`unfollow`]'s writes on the open transaction `conn`: the row delete, the
/// counter steps, the endorsement and the list memberships, none of them
/// committed here. The caller owns the transaction, so dropping it without a
/// commit undoes all four together.
pub(super) async fn unfollow_conn(
    conn: &mut PgConnection,
    follower: &User,
    target: &User,
) -> AppResult<bool> {
    let removed = sqlx::query_as::<_, Follow>(UNFOLLOW_SQL)
        .bind(follower.id)
        .bind(target.id)
        .fetch_optional(&mut *conn)
        .await?;
    let Some(follow) = removed else {
        return Ok(false);
    };
    if follow.state == FollowState::Accepted {
        step_counters(&mut *conn, &follow, -1).await?;
    }
    endorsement::unendorse(&mut *conn, follower.id, target.id).await?;
    sqlx::query(UNFOLLOW_LIST_MEMBERSHIPS_SQL)
        .bind(follower.id)
        .bind(target.id)
        .execute(&mut *conn)
        .await
        .map(drop)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        models::{
            endorsement,
            follow::{follow, test_support::*},
            list::{self, NewList},
        },
    };

    #[tokio::test]
    async fn unfollowing_reports_what_it_removed() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let carol = locked_user(&pool, "carol").await;
        let dave = local_user(&pool, "dave").await;

        assert!(
            !unfollow(&pool, &alice, &bob).await.unwrap(),
            "a pair that never followed has nothing to take back"
        );

        let accepted = follow(&pool, TEST_BASE_URL, &alice, &bob).await.unwrap();
        assert_eq!(accepted.state, FollowState::Accepted, "the fixture counts");
        assert!(
            unfollow(&pool, &alice, &bob).await.unwrap(),
            "an accepted follow is removed"
        );
        assert_eq!(
            fresh_user(&pool, bob.id).await.followers_count,
            0,
            "removing it steps the followee's count down"
        );
        assert_eq!(
            fresh_user(&pool, alice.id).await.following_count,
            0,
            "and the follower's"
        );
        assert!(
            !unfollow(&pool, &alice, &bob).await.unwrap(),
            "unfollowing twice finds nothing the second time"
        );

        let pending = follow(&pool, TEST_BASE_URL, &alice, &carol).await.unwrap();
        assert_eq!(pending.state, FollowState::Pending, "the fixture waits");
        assert!(
            unfollow(&pool, &alice, &carol).await.unwrap(),
            "a pending request can be withdrawn too"
        );
        assert_eq!(
            fresh_user(&pool, carol.id).await.followers_count,
            0,
            "withdrawing it counts nothing"
        );

        let rejected = rejected_relation(&pool, &dave, &carol).await;
        assert!(
            !unfollow(&pool, &dave, &carol).await.unwrap(),
            "a rejected row is not an open relationship"
        );
        assert_eq!(
            relation(&pool, &dave, &carol).await,
            rejected,
            "and unfollowing leaves it in place"
        );
    }

    /// An endorsement is a pinned *follow*: [`unfollow`] clears the pair's
    /// endorsement together with the relationship — granted follows and
    /// withdrawn requests alike — while the reverse pin and another
    /// account's pin on the same target stay exactly where they were.
    #[tokio::test]
    async fn unfollowing_removes_the_endorsement() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let carol = locked_user(&pool, "carol").await;
        let dave = local_user(&pool, "dave").await;

        let accepted = follow(&pool, TEST_BASE_URL, &alice, &bob)
            .await
            .expect("the fixture follow is granted");
        assert_eq!(accepted.state, FollowState::Accepted, "it counts");
        endorsement::endorse(&pool, alice.id, bob.id)
            .await
            .expect("alice pins bob");
        endorsement::endorse(&pool, bob.id, alice.id)
            .await
            .expect("and bob pins alice back");
        endorsement::endorse(&pool, dave.id, bob.id)
            .await
            .expect("dave pins bob too");
        let waiting = follow(&pool, TEST_BASE_URL, &alice, &carol)
            .await
            .expect("the request to the locked account waits");
        assert_eq!(waiting.state, FollowState::Pending, "it waits");
        endorsement::endorse(&pool, alice.id, carol.id)
            .await
            .expect("a hand-seeded pin rides the request");

        assert!(
            unfollow(&pool, &alice, &bob).await.unwrap(),
            "the accepted follow is removed"
        );
        assert!(
            endorsement::endorsed_ids(&pool, alice.id)
                .await
                .unwrap()
                .contains(&carol.id),
            "alice's withdrawal took only the bob pair: the carol pin stands"
        );
        assert!(
            !endorsement::endorsed_ids(&pool, alice.id)
                .await
                .unwrap()
                .contains(&bob.id),
            "and the pair she unfollowed is unpinned"
        );
        assert_eq!(
            endorsement::endorsed_ids(&pool, bob.id).await.unwrap(),
            vec![alice.id],
            "bob's own pin was not alice's to drop"
        );
        assert_eq!(
            endorsement::endorsed_ids(&pool, dave.id).await.unwrap(),
            vec![bob.id],
            "and dave's pin on bob was nobody else's to drop"
        );

        assert!(
            unfollow(&pool, &alice, &carol).await.unwrap(),
            "a withdrawn request is an open relationship too"
        );
        assert!(
            endorsement::endorsed_ids(&pool, alice.id)
                .await
                .unwrap()
                .is_empty(),
            "and its seed pin went with it"
        );
    }

    /// The cleanup rides the removal: an [`unfollow`] that found nothing to
    /// remove (here a `rejected` row) leaves an existing endorsement alone.
    #[tokio::test]
    async fn an_unfollow_that_removed_nothing_keeps_the_endorsement() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let dave = local_user(&pool, "dave").await;
        let carol = locked_user(&pool, "carol").await;
        let _rejected = rejected_relation(&pool, &dave, &carol).await;
        endorsement::endorse(&pool, dave.id, carol.id)
            .await
            .expect("the seed pin stores");

        assert!(
            !unfollow(&pool, &dave, &carol).await.unwrap(),
            "a rejected row is not an open relationship"
        );
        assert_eq!(
            endorsement::endorsed_ids(&pool, dave.id).await.unwrap(),
            vec![carol.id],
            "and the failed unfollow pinned nothing away"
        );
    }

    /// The row delete, both counter steps, the endorsement and the list
    /// memberships ride one transaction: rolling it back undoes every one.
    #[tokio::test]
    async fn a_rolled_back_unfollow_changes_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let accepted = follow(&pool, TEST_BASE_URL, &alice, &bob).await.unwrap();
        assert_eq!(accepted.state, FollowState::Accepted, "the fixture counts");
        endorsement::endorse(&pool, alice.id, bob.id)
            .await
            .expect("alice pins bob");
        let hers = seeded_list(&pool, &alice, "hers").await;
        let seeded = list::add_members(&pool, hers.id, &[bob.id])
            .await
            .expect("bob joins the list");
        assert_eq!(seeded, 1, "one membership went in");

        let mut tx = pool.begin().await.expect("the transaction opens");
        assert!(
            unfollow_conn(&mut tx, &alice, &bob).await.unwrap(),
            "the accepted follow is removed inside the transaction"
        );
        tx.rollback().await.expect("the transaction rolls back");

        assert_eq!(
            relation(&pool, &alice, &bob).await,
            accepted,
            "the row is back as it stood"
        );
        assert_eq!(
            (
                fresh_user(&pool, bob.id).await.followers_count,
                fresh_user(&pool, alice.id).await.following_count
            ),
            (1, 1),
            "both counters are unchanged"
        );
        assert_eq!(
            endorsement::endorsed_ids(&pool, alice.id).await.unwrap(),
            vec![bob.id],
            "the endorsement stays"
        );
        assert_eq!(
            list::member_ids(&pool, hers.id).await.unwrap(),
            vec![bob.id],
            "and so does the list membership"
        );
    }

    /// A list titled `title` owned by `owner`, for the membership cleanup.
    async fn seeded_list(pool: &PgPool, owner: &User, title: &str) -> list::List {
        list::create(
            pool,
            owner.id,
            &NewList {
                title: title.to_owned(),
                ..NewList::default()
            },
        )
        .await
        .expect("the fixture list creates")
    }

    /// [`unfollow`]'s list cleanup: the account stops being a member of the
    /// unfollowing account's own lists — an accepted relationship and a
    /// withdrawn request alike — while another owner's membership of the same
    /// account is not theirs to prune.
    #[tokio::test]
    async fn unfollowing_takes_the_target_out_of_the_followers_lists() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let carol = local_user(&pool, "carol").await;
        let erin = locked_user(&pool, "erin").await;
        let hers = seeded_list(&pool, &alice, "hers").await;
        let also_hers = seeded_list(&pool, &alice, "also hers").await;
        let carols = seeded_list(&pool, &carol, "carol's own").await;
        for row in [&hers, &also_hers, &carols] {
            let added = list::add_members(&pool, row.id, &[bob.id, erin.id])
                .await
                .expect("the fixture members add");
            assert_eq!(added, 2, "both accounts joined the fixture list");
        }
        let _accepted = follow(&pool, TEST_BASE_URL, &alice, &bob)
            .await
            .expect("alice follows bob");
        let _waiting = follow(&pool, TEST_BASE_URL, &alice, &erin)
            .await
            .expect("and alice's request to erin waits");

        assert!(
            unfollow(&pool, &alice, &bob).await.unwrap(),
            "the granted follow is removed"
        );
        assert!(
            unfollow(&pool, &alice, &erin).await.unwrap(),
            "and the withdrawn request is an open relationship too"
        );
        for list in [&hers, &also_hers] {
            assert!(
                list::member_ids(&pool, list.id)
                    .await
                    .expect("members read")
                    .is_empty(),
                "both of alice's lists are pruned of both accounts"
            );
        }
        assert_eq!(
            list::member_ids(&pool, carols.id).await.unwrap(),
            vec![bob.id, erin.id],
            "while carol's membership of the same two accounts stands"
        );
    }

    /// The cleanup rides the removal: an [`unfollow`] that found nothing to
    /// remove leaves the memberships exactly where they were, too.
    #[tokio::test]
    async fn an_unfollow_that_removed_nothing_keeps_the_memberships() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let dave = local_user(&pool, "dave").await;
        let carol = locked_user(&pool, "carol").await;
        let _rejected = rejected_relation(&pool, &dave, &carol).await;
        let hers = seeded_list(&pool, &dave, "hers").await;
        let seeded = list::add_members(&pool, hers.id, &[carol.id])
            .await
            .expect("the seed membership adds");
        assert_eq!(seeded, 1, "one membership went in");

        assert!(
            !unfollow(&pool, &dave, &carol).await.unwrap(),
            "a rejected row is not an open relationship"
        );
        assert_eq!(
            list::member_ids(&pool, hers.id).await.unwrap(),
            vec![carol.id],
            "and the failed unfollow listed nobody out"
        );
    }
}
