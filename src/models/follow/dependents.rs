//! What rides on a follow: the follower's endorsement of the followee and the
//! followee's memberships in the follower's lists. A list member is a followed
//! account and an endorsement is a pinned follow, so whenever a follow row
//! goes, [`clear_dependents`] takes both with it (Mastodon's
//! `remove_endorsements` callback and the `list_accounts.follow_id` cascade).

use sqlx::PgConnection;
use uuid::Uuid;

use crate::{error::AppResult, models::endorsement};

/// [`clear_dependents`]: the memberships the follower kept of the followee.
/// The delete runs over the list's *owner*, so the follower's rows in other
/// people's lists are untouched.
const UNFOLLOW_LIST_MEMBERSHIPS_SQL: &str = "DELETE FROM list_members AS lm
     USING lists AS l
     WHERE l.id = lm.list_id AND l.user_id = $1 AND lm.user_id = $2";

/// [`clear_all_for_user`]: every membership in the account's own lists and
/// every membership of the account in anyone's lists.
const CLEAR_LIST_MEMBERSHIPS_FOR_USER_SQL: &str = "DELETE FROM list_members AS lm
     USING lists AS l
     WHERE l.id = lm.list_id AND (l.user_id = $1 OR lm.user_id = $1)";

/// [`clear_all_for_user`]: every endorsement the account gave or received.
const CLEAR_ENDORSEMENTS_FOR_USER_SQL: &str =
    "DELETE FROM endorsements WHERE user_id = $1 OR target_id = $1";

/// Removes the rows that depend on `follower_id`'s follow of `followee_id`:
/// the follower's endorsement of the followee and the followee's membership in
/// each of the follower's own lists (`target_id` is the followed account), on
/// the open transaction `conn`.
///
/// # Errors
///
/// Any database failure (connection, constraint, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub(super) async fn clear_dependents(
    conn: &mut PgConnection,
    follower_id: Uuid,
    target_id: Uuid,
) -> AppResult<()> {
    endorsement::unendorse(&mut *conn, follower_id, target_id).await?;
    sqlx::query(UNFOLLOW_LIST_MEMBERSHIPS_SQL)
        .bind(follower_id)
        .bind(target_id)
        .execute(&mut *conn)
        .await
        .map(drop)?;
    Ok(())
}

/// Set-based [`clear_dependents`] for a departing account: every follow naming
/// `user_id` is going, so every endorsement and list membership naming it goes
/// too, in two statements on the open transaction `conn`.
///
/// # Errors
///
/// Any database failure (connection, constraint, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub(super) async fn clear_all_for_user(conn: &mut PgConnection, user_id: Uuid) -> AppResult<()> {
    sqlx::query(CLEAR_LIST_MEMBERSHIPS_FOR_USER_SQL)
        .bind(user_id)
        .execute(&mut *conn)
        .await
        .map(drop)?;
    sqlx::query(CLEAR_ENDORSEMENTS_FOR_USER_SQL)
        .bind(user_id)
        .execute(&mut *conn)
        .await
        .map(drop)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use crate::{
        config::TEST_BASE_URL,
        models::{
            block, endorsement,
            follow::{
                FollowState, delete_all_for_user_with_counts, follow, reject, remove_follower,
                test_support::*,
            },
            list::{self, NewList},
            user::User,
        },
    };

    /// A list titled `title` owned by `owner`.
    async fn list_of(pool: &PgPool, owner: &User, title: &str) -> list::List {
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

    /// `follower` follows `target`, endorses them and lists them; returns the
    /// list holding `target`.
    async fn bound_pair(pool: &PgPool, follower: &User, target: &User) -> list::List {
        let row = follow(pool, TEST_BASE_URL, follower, target)
            .await
            .expect("the follow stores");
        assert_eq!(row.state, FollowState::Accepted, "the fixture is granted");
        endorsement::endorse(pool, follower.id, target.id)
            .await
            .expect("the endorsement stores");
        let owned = list_of(pool, follower, "mine").await;
        let added = list::add_members(pool, owned.id, &[target.id])
            .await
            .expect("the member adds");
        assert_eq!(added, 1, "one membership went in");
        owned
    }

    /// Whether `owner` still endorses `target`.
    async fn endorses(pool: &PgPool, owner: &User, target: &User) -> bool {
        endorsement::endorsed_ids(pool, owner.id)
            .await
            .expect("endorsements read")
            .contains(&target.id)
    }

    /// Whether `target` is still a member of `list`.
    async fn listed(pool: &PgPool, list: &list::List, target: &User) -> bool {
        list::member_ids(pool, list.id)
            .await
            .expect("members read")
            .contains(&target.id)
    }

    #[tokio::test]
    async fn blocking_clears_the_endorsement_and_the_membership() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let mine = bound_pair(&pool, &alice, &bob).await;

        let _row = block::block(&pool, TEST_BASE_URL, &alice, &bob)
            .await
            .expect("alice blocks bob");

        assert!(!endorses(&pool, &alice, &bob).await, "the pin is gone");
        assert!(!listed(&pool, &mine, &bob).await, "and so is the member");
    }

    #[tokio::test]
    async fn removing_a_follower_empties_their_list_of_the_user() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let carol = local_user(&pool, "carol").await;
        let hers = bound_pair(&pool, &alice, &bob).await;
        let other = list_of(&pool, &carol, "carol's").await;
        let added = list::add_members(&pool, other.id, &[bob.id])
            .await
            .expect("carol lists bob");
        assert_eq!(added, 1, "one membership went in");
        endorsement::endorse(&pool, carol.id, bob.id)
            .await
            .expect("carol pins bob");

        let removed = remove_follower(&pool, bob.id, alice.id)
            .await
            .expect("the removal runs");

        assert!(removed.is_some(), "alice was bob's follower");
        assert!(!endorses(&pool, &alice, &bob).await, "alice's pin is gone");
        assert!(!listed(&pool, &hers, &bob).await, "alice's list lost bob");
        assert!(endorses(&pool, &carol, &bob).await, "carol's pin stays");
        assert!(listed(&pool, &other, &bob).await, "carol's list stays");
    }

    #[tokio::test]
    async fn rejecting_an_accepted_follow_clears_its_dependents() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let carol = local_user(&pool, "carol").await;
        let hers = bound_pair(&pool, &alice, &bob).await;
        let other = list_of(&pool, &carol, "carol's").await;
        let added = list::add_members(&pool, other.id, &[bob.id])
            .await
            .expect("carol lists bob");
        assert_eq!(added, 1, "one membership went in");
        let accepted = crate::models::follow::get(&pool, alice.id, bob.id)
            .await
            .unwrap()
            .expect("the follow exists");

        let _removed = reject(&pool, accepted.id).await.expect("the reject runs");

        assert!(!endorses(&pool, &alice, &bob).await, "the pin is gone");
        assert!(!listed(&pool, &hers, &bob).await, "so is the member");
        assert!(listed(&pool, &other, &bob).await, "carol's list stays");
    }

    #[tokio::test]
    async fn deleting_an_account_clears_every_dependent_it_touches() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let carol = local_user(&pool, "carol").await;
        let dave = local_user(&pool, "dave").await;
        let outgoing = bound_pair(&pool, &alice, &bob).await;
        let incoming = bound_pair(&pool, &bob, &alice).await;
        let between = bound_pair(&pool, &carol, &dave).await;

        let removed = delete_all_for_user_with_counts(&pool, alice.id)
            .await
            .expect("the sweep runs");

        assert_eq!(removed, 2, "both of alice's follows went");
        assert!(!endorses(&pool, &alice, &bob).await, "alice's pin is gone");
        assert!(!endorses(&pool, &bob, &alice).await, "bob's pin is gone");
        assert!(
            !listed(&pool, &outgoing, &bob).await,
            "alice's list is empty"
        );
        assert!(
            !listed(&pool, &incoming, &alice).await,
            "bob's list lost alice"
        );
        assert!(endorses(&pool, &carol, &dave).await, "other pins stay");
        assert!(listed(&pool, &between, &dave).await, "other lists stay");
    }
}
