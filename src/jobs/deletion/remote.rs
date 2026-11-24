//! The remote account sweep: the same clearing for a mirrored peer account,
//! keeping the row and stepping its live peers' counters.

use serde_json::Value;
use tracing::debug;

#[cfg(doc)]
use super::KIND_DELETE_REMOTE;
use super::{SweepCounts, clear_shared, local::load_account, parse_args};
use crate::{
    jobs::{AttemptInfo, HandlerFuture, JobError},
    models::user,
    state::AppState,
};

/// The [`JobRegistry`](crate::jobs::JobRegistry) handler for
/// [`KIND_DELETE_REMOTE`]: the sweep a peer's `Delete` of its own actor
/// queues ([`crate::federation::handlers::actor`]), for an account this
/// instance only ever mirrored.
///
/// The handler that queued it has already marked the mirror inactive with
/// `deleted_at` stamped, so the row arrives here as a tombstone and stays
/// one: nothing of a remote actor is personal data, and decision 7 keeps
/// the row for the activities and relationships that name it. A vanished
/// row or an unparsable payload is discarded; every database failure
/// retries, the writes being idempotent.
pub fn handle_delete_remote(state: AppState, _attempt: AttemptInfo, args: Value) -> HandlerFuture {
    Box::pin(async move {
        let user_id = parse_args(&args)?;
        let account = load_account(&state, user_id).await?;
        clear_remote_data(&state, &account).await?;
        debug!(account = %account.nickname, "remote account deletion finished");
        Ok(())
    })
}

/// The remote sweep: the mirror's posts tombstone in batches of 100, every
/// relationship naming it goes with its live peers' counters stepped, its
/// own likes, reactions and marks go, and every notification naming it in
/// either role goes — including the ones local accounts received from the
/// now-dead actor. What is *not* here is the local sweep's tail: a mirror
/// owns no list, uploads no media, and has no email, password hash or
/// display name to purge, so the row stays exactly as it was, inactive.
///
/// # Errors
///
/// [`JobError::Retry`] on any database failure (see [`clear_shared`]).
async fn clear_remote_data(state: &AppState, account: &user::User) -> Result<(), JobError> {
    let SweepCounts {
        statuses,
        chat_messages,
        follows,
        favourites,
        reactions,
        bookmarks,
        notifications,
    } = clear_shared(state, account).await?;
    debug!(
        account = %account.nickname,
        statuses, chat_messages, follows, favourites, reactions, bookmarks, notifications,
        "remote account deletion cleared"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use uuid::Uuid;

    use super::super::test_support::{
        Departed, app_state, broken_state, count_where, departed_mirror, fresh_user, has_follow,
        is_blank_tombstone, likes, listed_messages, live_statuses, local_user, post, remote_post,
        remote_user, run_remote, send_chat, send_chat_own, status_deleted_at,
    };
    use super::super::{KIND_DELETE_REMOTE, job_args};
    use super::*;

    /// The remote sweep, past a batch boundary: every post the mirror made is
    /// a tombstone while the rows stay, and the row itself survives as the
    /// inactive, still-named mirror the `Delete` handler left.
    #[tokio::test]
    async fn the_remote_handler_tombstones_statuses_and_keeps_the_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let fixture = departed_mirror(&pool).await;
        let Departed {
            alice,
            alices_post,
            bobs_post,
            ..
        } = &fixture;

        run_remote(&app_state(&pool), alice.id)
            .await
            .expect("the remote delete job runs");

        assert_eq!(
            live_statuses(&pool, alice.id).await,
            0,
            "all 121 of the mirror's statuses tombstoned, across the 100-row batch"
        );
        assert!(
            status_deleted_at(&pool, alices_post.id).await.is_some(),
            "the one post it made over the fixture batch is a tombstone too"
        );
        assert_eq!(
            count_where(&pool, "statuses", "user_id", alice.id).await,
            121,
            "the rows stay, tombstoned"
        );
        assert!(
            status_deleted_at(&pool, bobs_post.id).await.is_none(),
            "bob's post is not alice's to tombstone"
        );

        let kept = fresh_user(&pool, alice.id).await;
        assert!(!kept.is_active.get(), "the row stays inactive");
        assert!(kept.deleted_at.is_some(), "and stays marked deleted");
        assert_eq!(
            kept.nickname, "alice@peer.example",
            "the mirror is intact: the remote sweep purges nothing"
        );
    }

    /// The remote sweep's relationship and interaction half: both directions
    /// of every follow naming the account, its own likes, marks and reactions,
    /// and the notifications that name it, with the live peers' counters
    /// stepped and nobody else's rows touched.
    #[tokio::test]
    async fn the_remote_handler_clears_relationships_and_interactions() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let fixture = departed_mirror(&pool).await;
        let Departed {
            alice,
            bob,
            carol,
            bobs_notification,
            ..
        } = &fixture;

        run_remote(&app_state(&pool), alice.id)
            .await
            .expect("the remote delete job runs");

        assert!(
            !has_follow(&pool, bob, alice).await,
            "bob's follow of the departed account is gone"
        );
        assert!(
            !has_follow(&pool, alice, carol).await,
            "and alice's follow of carol"
        );
        assert!(
            has_follow(&pool, bob, carol).await,
            "bob and carol keep their relationship"
        );
        assert_eq!(
            fresh_user(&pool, bob.id).await.following_count,
            1,
            "bob counts the follow that is left, not the one that went"
        );
        assert_eq!(
            fresh_user(&pool, carol.id).await.followers_count,
            1,
            "and carol counts her remaining follower"
        );
        let departed = fresh_user(&pool, alice.id).await;
        assert_eq!(
            (departed.followers_count, departed.following_count),
            (0, 0),
            "the departed mirror's own counters describe what is left of them"
        );

        assert_eq!(
            count_where(&pool, "favourites", "user_id", alice.id).await,
            0
        );
        assert_eq!(
            count_where(&pool, "bookmarks", "user_id", alice.id).await,
            0
        );
        assert_eq!(
            count_where(&pool, "emoji_reactions", "user_id", alice.id).await,
            0,
            "its kept row would otherwise keep its likes, marks and reactions alive"
        );
        assert_eq!(
            count_where(&pool, "emoji_reactions", "user_id", bob.id).await,
            1,
            "bob's reaction on alice's (tombstoned) status is bob's own row"
        );
        assert_eq!(
            count_where(&pool, "notifications", "from_account_id", alice.id).await,
            0,
            "nobody is told about a departed account any more"
        );
        assert_eq!(
            count_where(&pool, "notifications", "id", bobs_notification.id).await,
            1,
            "carol's notification about bob is nobody else's to clear"
        );
    }

    /// The mirror's chat messages are blank tombstones after the sweep and
    /// drop off the local participant's page, while the local participant's
    /// own message stays.
    #[tokio::test]
    async fn the_remote_handler_withdraws_the_chat_messages_the_mirror_sent() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = remote_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let first = send_chat(&pool, &alice, &bob, "alice says one").await;
        let second = send_chat(&pool, &alice, &bob, "alice says two").await;
        let reply = send_chat_own(&pool, &bob, &alice, "bob answers").await;
        let bobs_chat = first.chat_id;
        assert_eq!(
            listed_messages(&pool, bobs_chat).await.len(),
            3,
            "bob reads all three messages before the sweep"
        );

        run_remote(&app_state(&pool), alice.id)
            .await
            .expect("the remote delete job runs");

        assert_eq!(
            listed_messages(&pool, bobs_chat).await,
            vec![reply.message_id],
            "bob's page keeps only bob's own message"
        );
        for gone in [first.message_id, second.message_id] {
            assert!(
                is_blank_tombstone(&pool, gone).await,
                "the mirror's message {gone} is a blank tombstone"
            );
        }
        assert!(
            !is_blank_tombstone(&pool, reply.message_id).await,
            "bob's reply is not the mirror's to withdraw"
        );
    }

    /// The remote sweep is idempotent and keeps its hands off the local
    /// sweep's writes: a rerun removes nothing further and purges no
    /// personal data, because a mirror has none.
    #[tokio::test]
    async fn a_second_remote_pass_clears_nothing_and_purges_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = remote_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let bobs_post = post(&pool, &bob, "bob posts").await;
        let _liked = likes(&pool, &alice, bobs_post.id).await;
        let state = app_state(&pool);

        run_remote(&state, alice.id)
            .await
            .expect("the first pass runs");
        run_remote(&state, alice.id)
            .await
            .expect("the second pass runs");

        assert_eq!(
            count_where(&pool, "favourites", "user_id", alice.id).await,
            0,
            "the like went on the first pass and nothing more on the second"
        );
        assert_eq!(
            fresh_user(&pool, alice.id).await.nickname,
            "alice@peer.example",
            "the row keeps its handle: the remote sweep has no purge step"
        );
    }

    /// The remote job's unusable payload and vanished row discard, exactly
    /// as the local job's do.
    #[tokio::test]
    async fn the_remote_handler_discards_unusable_args_and_a_gone_user() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let state = app_state(db.pool());

        let bad = handle_delete_remote(
            state.clone(),
            AttemptInfo {
                attempts: 1,
                max_attempts: 5,
            },
            json!({ "user": "not-an-id" }),
        )
        .await;
        assert!(
            matches!(&bad, Err(JobError::Discard(detail)) if detail.contains("unusable")),
            "unusable arguments discard: {bad:?}"
        );

        let missing = run_remote(&state, Uuid::now_v7()).await;
        assert!(
            matches!(&missing, Err(JobError::Discard(detail)) if detail.contains("no longer exists")),
            "a vanished account discards: {missing:?}"
        );
    }

    /// A database that cannot answer retries the remote sweep too.
    #[tokio::test]
    async fn an_unreachable_database_retries_the_remote_sweep() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = remote_user(&pool, "alice").await;

        let outcome = run_remote(&broken_state().await, alice.id).await;
        assert!(
            matches!(&outcome, Err(JobError::Retry(detail)) if detail.starts_with("delete user read failed")),
            "a closed pool retries: {outcome:?}"
        );
    }

    /// The remote kind is wired into the worker's registry, and the handler
    /// behind it is this module's remote sweep.
    #[tokio::test]
    async fn the_remote_kind_is_registered_to_this_handler() {
        let registry = crate::job_registry();
        let handler = registry
            .get(KIND_DELETE_REMOTE)
            .expect("the remote delete kind is registered");
        assert_eq!(
            KIND_DELETE_REMOTE, "users.delete_remote",
            "the ticket's kind"
        );

        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = remote_user(&pool, "alice").await;
        let _posted = remote_post(&pool, &alice, "one post").await;
        handler(
            app_state(&pool),
            AttemptInfo {
                attempts: 1,
                max_attempts: 5,
            },
            job_args(alice.id),
        )
        .await
        .expect("the registered handler runs");
        assert_eq!(
            live_statuses(&pool, alice.id).await,
            0,
            "and it is this module's remote sweep: the mirror's post is a tombstone"
        );
    }
}
