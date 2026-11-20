//! The two per-follow toggles: [`set_notify`] writes one flag of a granted
//! follow, [`set_toggles`] writes the flags an open relationship carries.

use sqlx::PgPool;
use uuid::Uuid;

use crate::error::AppResult;

use super::types::{Follow, FollowToggles};

/// [`set_notify`]: the `notify` toggle, written only onto an *accepted*
/// relationship — Pleroma's `subscribe`/`unsubscribe` and Mastodon's
/// `follow?notify` all act on a granted follow — and refreshing
/// `updated_at` the way [`ACCEPT_SQL`] does.
const SET_NOTIFY_SQL: &str = "UPDATE follows
       SET notify = $3,
           updated_at = now()
     WHERE follower_id = $1 AND followee_id = $2 AND state = 'accepted'
   RETURNING *";

/// [`set_toggles`]: the two per-follow toggles, written onto an *open*
/// relationship and only where the request named them — `COALESCE` keeps the
/// stored flag when its parameter is `NULL`, so a client that sends `reblogs`
/// alone does not reset `notify`. The states are the open ones
/// [`UNFOLLOW_SQL`] takes back: a `pending` request the follow itself just
/// created carries the toggles the client asked for, and a mirrored
/// `rejected` row is nobody's active relationship.
const SET_TOGGLES_SQL: &str = "UPDATE follows
       SET show_reblogs = COALESCE($3, show_reblogs),
           notify = COALESCE($4, notify),
           updated_at = now()
     WHERE follower_id = $1 AND followee_id = $2
       AND state IN ('pending', 'accepted')
   RETURNING *";

/// Sets the `notify` toggle of the accepted relationship between
/// `follower_id` and `target_id` and returns the updated row.
///
/// The one column the notification flags of
/// [`Relationship`](crate::entities::relationship::Relationship) are read
/// from: Pleroma's `POST /api/v1/pleroma/accounts/{id}/subscribe` and
/// `…/unsubscribe` (`User.subscribe` / `User.unsubscribe`) write it here,
/// while Mastodon's `follow?notify` arrives through [`set_toggles`], the
/// pair-wide write of the same flag plus `show_reblogs`.
///
/// Only a *granted* follow can be subscribed to: a `pending` request nobody
/// answered and a pair with no row at all are answered `None` with nothing
/// written, which is the fact the endpoints turn into their refusal.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn set_notify(
    pool: &PgPool,
    follower_id: Uuid,
    target_id: Uuid,
    notify: bool,
) -> AppResult<Option<Follow>> {
    let updated = sqlx::query_as::<_, Follow>(SET_NOTIFY_SQL)
        .bind(follower_id)
        .bind(target_id)
        .bind(notify)
        .fetch_optional(pool)
        .await?;
    Ok(updated)
}

/// Writes the toggles `toggles` names onto the pair's open relationship and
/// returns the updated row, `None` when the pair has none to write on.
///
/// The write behind Mastodon's `POST /api/v1/accounts/{id}/follow`: the two
/// parameters that endpoint takes ride along with the follow it creates and
/// are re-sent on a follow that already exists, where they update the stored
/// flags. Only the parameters a request actually carried are written; an
/// absent one keeps what the row holds, which is what makes [`FollowToggles`]
/// a pair of `Option`s. The row and its id are otherwise untouched: this
/// changes neither the state nor the denormalised counters (a toggle is not a
/// follow).
///
/// Both open states answer the write — the `pending` request a follow of a
/// locked or remote account just created carries the toggles its client asked
/// for, which is what the row will mean when it is granted — while a mirrored
/// `rejected` row (see [`FollowState::Rejected`](super::FollowState::Rejected)) and an unknown pair are
/// answered `None` with nothing written.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn set_toggles(
    pool: &PgPool,
    follower_id: Uuid,
    target_id: Uuid,
    toggles: FollowToggles,
) -> AppResult<Option<Follow>> {
    let updated = sqlx::query_as::<_, Follow>(SET_TOGGLES_SQL)
        .bind(follower_id)
        .bind(target_id)
        .bind(toggles.show_reblogs)
        .bind(toggles.notify)
        .fetch_optional(pool)
        .await?;
    Ok(updated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        models::follow::{FollowState, follow, test_support::*},
        util::id::new_id,
    };

    #[tokio::test]
    async fn set_notify_writes_only_an_accepted_relationship() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let carol = locked_user(&pool, "carol").await;
        let accepted = follow(&pool, TEST_BASE_URL, &alice, &bob)
            .await
            .expect("the outgoing follow is granted");
        assert_eq!(accepted.state, FollowState::Accepted, "the fixture counts");
        let waiting = follow(&pool, TEST_BASE_URL, &alice, &carol)
            .await
            .expect("the request to the locked account waits");
        assert_eq!(waiting.state, FollowState::Pending, "the fixture waits");

        assert!(
            set_notify(&pool, alice.id, new_id(), true)
                .await
                .unwrap()
                .is_none(),
            "a pair with no row has nothing to toggle"
        );
        assert!(
            set_notify(&pool, alice.id, carol.id, true)
                .await
                .unwrap()
                .is_none(),
            "a request nobody answered cannot be subscribed to"
        );
        assert!(
            set_notify(&pool, bob.id, alice.id, true)
                .await
                .unwrap()
                .is_none(),
            "the other direction of the accepted pair has no row"
        );
        assert!(
            !relation(&pool, &alice, &carol).await.notify,
            "the refused writes stored nothing"
        );

        let subscribed = set_notify(&pool, alice.id, bob.id, true)
            .await
            .unwrap()
            .expect("the accepted row toggles");
        assert_eq!(subscribed.id, accepted.id, "the same row, updated in place");
        assert_eq!(
            subscribed.state,
            FollowState::Accepted,
            "toggling does not unfollow"
        );
        assert!(subscribed.notify, "the row comes back with the flag on");
        assert!(
            relation(&pool, &alice, &bob).await.notify,
            "and the column holds it"
        );
        assert_eq!(
            (
                fresh_user(&pool, bob.id).await.followers_count,
                fresh_user(&pool, alice.id).await.following_count
            ),
            (1, 1),
            "the toggle moves no counters"
        );

        let unsubscribed = set_notify(&pool, alice.id, bob.id, false)
            .await
            .unwrap()
            .expect("the flag goes back the same way");
        assert!(!unsubscribed.notify, "the row comes back with the flag off");
        assert!(
            !relation(&pool, &alice, &bob).await.notify,
            "and so does the stored column"
        );
        assert!(
            relation(&pool, &alice, &bob).await.show_reblogs,
            "the write touches only the notify toggle"
        );
    }

    /// The [`FollowToggles`] of a request that named both parameters, each on
    /// the non-default side of its column: reblogs hidden, notifications on.
    fn flipped() -> FollowToggles {
        FollowToggles {
            show_reblogs: Some(false),
            notify: Some(true),
        }
    }

    /// The [`FollowToggles`] of a request that named only `reblogs`.
    fn reblogs_only(show_reblogs: bool) -> FollowToggles {
        FollowToggles {
            show_reblogs: Some(show_reblogs),
            notify: None,
        }
    }

    /// The [`FollowToggles`] of a request that named only `notify`.
    fn notify_only(notify: bool) -> FollowToggles {
        FollowToggles {
            show_reblogs: None,
            notify: Some(notify),
        }
    }

    #[tokio::test]
    async fn set_toggles_writes_both_flags_on_an_open_relationship() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let carol = locked_user(&pool, "carol").await;
        let accepted = follow(&pool, TEST_BASE_URL, &alice, &bob)
            .await
            .expect("the outgoing follow is granted");
        assert!(accepted.show_reblogs, "the fresh row defaults to showing");
        assert!(!accepted.notify, "and to not notifying");
        let waiting = follow(&pool, TEST_BASE_URL, &alice, &carol)
            .await
            .expect("the request to the locked account waits");
        assert_eq!(waiting.state, FollowState::Pending, "the fixture waits");

        let hidden = set_toggles(&pool, alice.id, bob.id, flipped())
            .await
            .unwrap()
            .expect("the accepted row takes both flags");
        assert_eq!(hidden.id, accepted.id, "the same row, updated in place");
        assert_eq!(
            hidden.state,
            FollowState::Accepted,
            "the toggles do not unfollow"
        );
        assert!(!hidden.show_reblogs, "reblogs are hidden");
        assert!(hidden.notify, "and notifications are on");
        assert!(
            hidden.inserted_at <= hidden.updated_at,
            "the write refreshes `updated_at`"
        );
        let stored = relation(&pool, &alice, &bob).await;
        assert_eq!(
            (stored.show_reblogs, stored.notify),
            (false, true),
            "both columns hold"
        );
        assert_eq!(
            (
                fresh_user(&pool, bob.id).await.followers_count,
                fresh_user(&pool, alice.id).await.following_count
            ),
            (1, 1),
            "a toggle write moves no counters"
        );

        let requested = set_toggles(&pool, alice.id, carol.id, flipped())
            .await
            .unwrap()
            .expect("a pending request takes the toggles its follow asked for");
        assert_eq!(requested.state, FollowState::Pending, "and stays a request");
        assert!(
            !requested.show_reblogs && requested.notify,
            "the waiting row holds both flags"
        );
    }

    #[tokio::test]
    async fn set_toggles_writes_only_the_flags_the_request_named() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let accepted = follow(&pool, TEST_BASE_URL, &alice, &bob)
            .await
            .expect("the outgoing follow is granted");
        assert_eq!(accepted.state, FollowState::Accepted, "the fixture counts");

        let hidden = set_toggles(&pool, alice.id, bob.id, reblogs_only(false))
            .await
            .unwrap()
            .expect("the named flag writes");
        assert!(!hidden.show_reblogs, "reblogs are hidden");
        assert!(!hidden.notify, "the unnamed flag keeps its default");

        let subscribed = set_toggles(&pool, alice.id, bob.id, notify_only(true))
            .await
            .unwrap()
            .expect("the other flag writes");
        assert!(subscribed.notify, "notify is on");
        assert!(
            !subscribed.show_reblogs,
            "and the flag the request left out stands"
        );

        assert!(
            FollowToggles::default().is_empty(),
            "a request that named neither flag is the empty set"
        );
        assert!(
            !reblogs_only(true).is_empty() && !notify_only(false).is_empty(),
            "one named flag is not"
        );
        let untouched = set_toggles(&pool, alice.id, bob.id, FollowToggles::default())
            .await
            .unwrap()
            .expect("even the empty write matches the row");
        assert_eq!(
            (untouched.show_reblogs, untouched.notify),
            (false, true),
            "and it stores nothing"
        );
    }

    #[tokio::test]
    async fn set_toggles_finds_nothing_to_write_outside_an_open_pair() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let carol = locked_user(&pool, "carol").await;
        let dave = local_user(&pool, "dave").await;

        assert!(
            set_toggles(&pool, alice.id, new_id(), flipped())
                .await
                .unwrap()
                .is_none(),
            "a pair with no row has nothing to toggle"
        );
        let rejected = rejected_relation(&pool, &dave, &carol).await;
        assert!(
            set_toggles(&pool, dave.id, carol.id, flipped())
                .await
                .unwrap()
                .is_none(),
            "a mirrored rejection is nobody's open relationship"
        );
        let stored = relation(&pool, &dave, &carol).await;
        assert_eq!(stored.id, rejected.id, "the row is where it was");
        assert!(
            stored.show_reblogs && !stored.notify,
            "and the refused write stored nothing"
        );

        let _accepted = follow(&pool, TEST_BASE_URL, &alice, &bob)
            .await
            .expect("the outgoing follow is granted");
        assert!(
            set_toggles(&pool, bob.id, alice.id, flipped())
                .await
                .unwrap()
                .is_none(),
            "the other direction of the pair has no row of its own"
        );
        assert!(
            set_toggles(&pool, alice.id, new_id(), FollowToggles::default())
                .await
                .unwrap()
                .is_none(),
            "an unknown pair is the `None` answer whatever the toggles"
        );
    }

    #[tokio::test]
    async fn following_a_second_time_keeps_the_row_and_its_toggles() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let first = follow(&pool, TEST_BASE_URL, &alice, &bob)
            .await
            .expect("the first follow is granted");
        let stored = set_toggles(&pool, alice.id, bob.id, flipped())
            .await
            .unwrap()
            .expect("the toggles write");
        assert_eq!(stored.id, first.id, "the toggles ride the same row");

        let again = follow(&pool, TEST_BASE_URL, &alice, &bob)
            .await
            .expect("the repeat follow is idempotent");
        assert_eq!(again, stored, "it hands back the stored row as-is");
        assert_eq!(follow_count(&pool).await, 1, "and writes no second row");
        assert_eq!(
            fresh_user(&pool, bob.id).await.followers_count,
            1,
            "counting it twice is not following it twice"
        );
    }
}
