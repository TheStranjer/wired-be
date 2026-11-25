//! The poll telling of [`crate::services::notify`]: the closing the
//! end-notify sweep reports. A ballot tells nobody (Mastodon and Pleroma
//! raise `poll` only when a poll ends). The shared creation plumbing this
//! call lives in the parent module.

use tracing::warn;
use uuid::Uuid;

use crate::{
    error::AppResult,
    models::{
        notification::{self, TYPE_POLL, TYPE_UPDATE, Telling},
        poll::{self, Poll as PollRow},
        status, user,
    },
    state::AppState,
};

use super::{announce, tell, tell_local, told_row};

/// The [`polls.end_notify`](crate::jobs::polls::KIND_END_NOTIFY) sweep's
/// telling: the poll's author and every local voter are told `poll`, with
/// the *author* as the actor, that the question closed.
///
/// Pleroma's `create_poll_notifications` (`lib/pleroma/notification.ex`,
/// run by `lib/pleroma/workers/poll_worker.ex` when a Question's `closed`
/// moment arrives) walks `[actor | voters]` and waives the self skip for
/// the `poll` type — the author is told of their own poll's ending because
/// the *closing*, not the acting, is what the row reports. Here the same
/// pair: the author goes through this module's shared creation arm
/// directly (the local check and the skip rules intact, the self exemption
/// owed to them), and each voter through its self-skipping teller.
///
/// Unlike the endpoint landings this one **returns** its failure: the sweep
/// sets the poll's `ended_notified` flag only after it answers `Ok`, so a
/// retried pass re-runs the tellings — where [`notification::already_told`]
/// keeps each recipient's single row a single row.
///
/// [`polls.end_notify`]: crate::jobs::polls::KIND_END_NOTIFY
///
/// # Errors
///
/// Any database failure behind the status, user, deduplication or insert
/// reads arrives as [`AppError::Database`](crate::error::AppError); the
/// caller owns the retry (see [`handle_end_notify`](crate::jobs::polls::handle_end_notify)).
pub async fn on_poll_ended(state: &AppState, poll: &PollRow) -> AppResult<()> {
    let Some(posted) = status::get_by_id(&state.db, poll.status_id).await? else {
        return Ok(());
    };
    let Some(author) = user::get_by_id(&state.db, posted.user_id).await? else {
        return Ok(());
    };
    let ending = Telling {
        user_id: author.id,
        from_account_id: author.id,
        notification_type: TYPE_POLL,
        status_id: Some(poll.status_id),
        emoji: None,
    };
    if !notification::already_told(&state.db, &ending).await? {
        announce(state, &author, &told_row(&ending)).await?;
    }
    for voter_id in poll::voter_ids(&state.db, poll.id).await? {
        poll_end_voter_telling(state, author.id, poll, voter_id).await?;
    }
    Ok(())
}

/// The voter arm of [`on_poll_ended`]: each *other* local voter is told
/// their ballot's question closed, once however many sweep reruns pass
/// (the author cannot mark their own poll, and is told by the arm above
/// whatever this one decides).
async fn poll_end_voter_telling(
    state: &AppState,
    actor_id: Uuid,
    poll: &PollRow,
    voter_id: Uuid,
) -> AppResult<()> {
    if voter_id == actor_id {
        return Ok(());
    }
    let Some(voter) = user::get_by_id(&state.db, voter_id).await? else {
        return Ok(());
    };
    let telling = Telling {
        user_id: voter.id,
        from_account_id: actor_id,
        notification_type: TYPE_POLL,
        status_id: Some(poll.status_id),
        emoji: None,
    };
    if notification::already_told(&state.db, &telling).await? {
        return Ok(());
    }
    tell(state, &voter, actor_id, TYPE_POLL, Some(poll.status_id)).await
}

/// Tells the voters of a restarted poll that its author edited the post.
///
/// Every account in `voter_ids` (the voters the edit just deleted the
/// ballots of) is told `update` naming the post, the author as actor —
/// Mastodon's `UpdateStatusService` notifies the accounts that voted when
/// an edit changes a poll's options.
///
/// A voter who is no local account, or who is the author, is nobody to
/// tell from here (the remote ones' servers get the federated `Update`).
/// A failure is logged, never returned: the edit has already been written.
pub async fn on_poll_restarted(state: &AppState, row: &status::Status, voter_ids: &[Uuid]) {
    for voter_id in voter_ids {
        let told = tell_local(state, *voter_id, row.user_id, TYPE_UPDATE, Some(row.id)).await;
        if let Err(err) = told {
            warn!(
                status = %row.id,
                voter = %voter_id,
                %err,
                "could not tell a voter about the edit of a poll"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::notify::test_support::{
        app_state, ballot, facts, local, poll_on, post, remote, rows,
    };

    #[tokio::test]
    async fn a_restarted_poll_tells_its_local_voters_an_update() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let state = app_state(pool);
        let alice = local(pool, "pu-alice").await;
        let bob = local(pool, "pu-bob").await;
        let mallory = remote(pool, "pu-mallory", "remote.example").await;
        let posted = post(pool, &alice, "which?").await;

        on_poll_restarted(&state, &posted, &[bob.id, mallory.id, alice.id]).await;
        assert_eq!(
            facts(pool, bob.id).await,
            vec![(TYPE_UPDATE.to_owned(), alice.id, Some(posted.id))],
            "the local voter hears the edit, from the author"
        );
        assert!(
            rows(pool, mallory.id).await.is_empty(),
            "a remote voter is told by the federated Update"
        );
        assert!(
            rows(pool, alice.id).await.is_empty(),
            "the author is not told of their own edit"
        );
    }

    #[tokio::test]
    async fn the_end_of_a_remote_poll_tells_its_local_voter_once() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let state = app_state(pool);
        let bob = local(pool, "pr-bob").await;
        let mallory = remote(pool, "pr-mallory", "remote.example").await;

        let theirs = post(pool, &mallory, "their question").await;
        let stand = poll_on(pool, &theirs, -1).await;
        ballot(pool, &stand, &bob).await;

        for _pass in 0..2 {
            on_poll_ended(&state, &stand)
                .await
                .expect("the closing telling runs");
        }
        assert_eq!(
            facts(pool, bob.id).await,
            vec![(TYPE_POLL.to_owned(), mallory.id, Some(theirs.id))],
            "the local voter is told once, from the remote author"
        );
        assert!(
            rows(pool, mallory.id).await.is_empty(),
            "the remote author is told by their own server, not by a row"
        );
    }

    #[tokio::test]
    async fn the_poll_end_tells_the_author_and_the_local_voters_once() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let state = app_state(pool);
        let alice = local(pool, "pe-alice").await;
        let bob = local(pool, "pe-bob").await;
        let carol = local(pool, "pe-carol").await;
        let mallory = remote(pool, "pe-mallory", "remote.example").await;

        let posted = post(pool, &alice, "tea or coffee?").await;
        let stand = poll_on(pool, &posted, -1).await;
        for voter in [&bob, &carol] {
            ballot(pool, &stand, voter).await;
        }
        ballot(pool, &stand, &mallory).await;
        assert!(
            rows(pool, alice.id).await.is_empty(),
            "no ballot tells the author anything"
        );

        on_poll_ended(&state, &stand)
            .await
            .expect("the closing telling runs");
        let closing = (TYPE_POLL.to_owned(), alice.id, Some(posted.id));
        assert_eq!(
            facts(pool, alice.id).await,
            vec![closing.clone()],
            "the author gains their own closing — the self skip's one exemption"
        );
        for voter in [&bob, &carol] {
            assert_eq!(
                facts(pool, voter.id).await,
                vec![closing.clone()],
                "each local voter is told the closing, from the author"
            );
        }
        assert!(
            rows(pool, mallory.id).await.is_empty(),
            "a remote voter is not ours to tell"
        );

        // The sweep's rerun (a retried job pass) retells nobody.
        on_poll_ended(&state, &stand).await.expect("the rerun runs");
        assert_eq!(
            facts(pool, alice.id).await.len(),
            1,
            "the author is not told twice"
        );
        assert_eq!(
            facts(pool, bob.id).await,
            vec![closing],
            "and neither is the voter"
        );
    }

    #[tokio::test]
    async fn a_closing_on_a_gone_poll_status_tells_nobody() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let state = app_state(pool);
        let alice = local(pool, "pd-alice").await;
        let posted = post(pool, &alice, "doomed closing").await;
        let stand = poll_on(pool, &posted, -1).await;
        let _tombstone = status::soft_delete(pool, posted.id)
            .await
            .expect("the delete runs");

        on_poll_ended(&state, &stand)
            .await
            .expect("the gone status is done work, not an error");
        assert!(
            rows(pool, alice.id).await.is_empty(),
            "nobody is told about a deleted question"
        );
    }
}
