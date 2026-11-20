//! The poll tellings of [`crate::services::notify`]: the ballot a voter
//! casts and the closing the end-notify sweep reports. The shared
//! creation plumbing these calls live in the parent module.

use tracing::warn;
use uuid::Uuid;

use crate::{
    error::AppResult,
    models::{
        notification::{self, TYPE_POLL, Telling},
        poll::{self, Poll as PollRow},
        status,
        user::{self, User},
    },
    state::AppState,
};

use super::{announce, tell, told_row};

/// The poll vote endpoint's notification, landed from `after_vote_cast` in
/// [`crate::api::polls`].
///
/// A ballot tells the poll's author `poll`, `from` the voter and about the
/// poll's status (Pleroma's `CommonAPI.vote` runs each `Answer` through its
/// pipeline, which lands the same telling). Deduplicated per voter and
/// poll: the endpoint already refuses a second ballot, so only a re-fired
/// hook can offer the same fact twice, and [`notification::already_told`]
/// keeps that one telling standing.
pub async fn on_poll_vote(state: &AppState, poll: &PollRow, voter: &User) {
    if let Err(err) = poll_vote_telling(state, poll, voter).await {
        warn!(
            poll = %poll.id,
            voter = %voter.id,
            %err,
            "could not tell a poll's author about a ballot"
        );
    }
}

/// [`on_poll_vote`]'s work: the author read off the poll's status, the
/// deduplication on the full telling key, and [`tell`] for the local and
/// self decisions (an author who somehow marked their own poll tells
/// themselves nothing).
async fn poll_vote_telling(state: &AppState, poll: &PollRow, voter: &User) -> AppResult<()> {
    let Some(posted) = status::get_by_id(&state.db, poll.status_id).await? else {
        return Ok(());
    };
    let telling = Telling {
        user_id: posted.user_id,
        from_account_id: voter.id,
        notification_type: TYPE_POLL,
        status_id: Some(poll.status_id),
        emoji: None,
    };
    if notification::already_told(&state.db, &telling).await? {
        return Ok(());
    }
    let Some(author) = user::get_by_id(&state.db, posted.user_id).await? else {
        return Ok(());
    };
    tell(state, &author, voter.id, TYPE_POLL, Some(poll.status_id)).await
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::notify::test_support::{
        app_state, ballot, facts, local, poll_on, post, remote, rows,
    };

    #[tokio::test]
    async fn a_poll_vote_tells_the_author_once_per_voter_and_poll() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let state = app_state(pool);
        let alice = local(pool, "pv-alice").await;
        let bob = local(pool, "pv-bob").await;

        let posted = post(pool, &alice, "vote on me").await;
        let stand = poll_on(pool, &posted, 30).await;
        ballot(pool, &stand, &bob).await;
        on_poll_vote(&state, &stand, &bob).await;
        assert_eq!(
            facts(pool, alice.id).await,
            vec![(TYPE_POLL.to_owned(), bob.id, Some(posted.id))],
            "the ballot is told to the author, about the poll's status"
        );

        // The same fact re-fired (the endpoint refuses a second ballot, so
        // only a re-run of the hook can offer it again) stands deduped.
        on_poll_vote(&state, &stand, &bob).await;
        assert_eq!(
            facts(pool, alice.id).await.len(),
            1,
            "one voter and one poll tell once"
        );

        // The same voter on a second poll is a second fact.
        let second = post(pool, &alice, "another question").await;
        let second_stand = poll_on(pool, &second, 30).await;
        ballot(pool, &second_stand, &bob).await;
        on_poll_vote(&state, &second_stand, &bob).await;
        assert_eq!(
            facts(pool, alice.id).await,
            vec![
                (TYPE_POLL.to_owned(), bob.id, Some(second.id)),
                (TYPE_POLL.to_owned(), bob.id, Some(posted.id)),
            ],
            "the deduplication is per voter *and* poll"
        );

        // The author's own ballot (unreachable through the endpoint's
        // refusal, reachable through a re-fired hook) tells nobody.
        on_poll_vote(&state, &stand, &alice).await;
        assert_eq!(
            facts(pool, alice.id).await.len(),
            2,
            "nobody is told of their own action"
        );
    }

    #[tokio::test]
    async fn a_vote_on_a_remote_or_gone_poll_stays_silent() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let state = app_state(pool);
        let bob = local(pool, "pg-bob").await;

        // A mirrored remote author is told by federation, not by a row.
        let mallory = remote(pool, "pg-mallory", "remote.example").await;
        let theirs = post(pool, &mallory, "their question").await;
        let stand = poll_on(pool, &theirs, 30).await;
        ballot(pool, &stand, &bob).await;
        on_poll_vote(&state, &stand, &bob).await;
        assert!(
            rows(pool, mallory.id).await.is_empty(),
            "the remote author is nobody's telling target here"
        );

        // A status deleted after the vote read: the tell finds no author
        // and errors nowhere.
        let alice = local(pool, "pg-alice").await;
        let doomed = post(pool, &alice, "doomed question").await;
        let doomed_stand = poll_on(pool, &doomed, 30).await;
        let _tombstone = status::soft_delete(pool, doomed.id)
            .await
            .expect("the delete runs");
        on_poll_vote(&state, &doomed_stand, &bob).await;
        assert!(
            rows(pool, alice.id).await.is_empty(),
            "a poll status the reads cannot answer tells nobody"
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
            on_poll_vote(&state, &stand, voter).await;
        }
        ballot(pool, &stand, &mallory).await;

        on_poll_ended(&state, &stand)
            .await
            .expect("the closing telling runs");
        let closing = (TYPE_POLL.to_owned(), alice.id, Some(posted.id));
        assert_eq!(
            facts(pool, alice.id).await,
            vec![
                closing.clone(),
                (TYPE_POLL.to_owned(), carol.id, Some(posted.id)),
                (TYPE_POLL.to_owned(), bob.id, Some(posted.id)),
            ],
            "the author keeps the two ballot tellings and gains their own \
             closing — the self skip's one exemption"
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
            3,
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
