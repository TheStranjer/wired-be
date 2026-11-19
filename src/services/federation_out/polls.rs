//! The answers a ballot on a REMOTE status's poll becomes: one `Create` per
//! chosen option, its object a bodyless `Note` naming the option title and
//! answering the question's uri, addressed to the poll's author alone.
//!
//! A poll on a local status is this instance's own fact and announces
//! nothing; the row readers and the publisher hand are the root's.

use serde_json::{Value, json};
use tracing::warn;
use uuid::Uuid;

use super::{load_status, load_user, queue_activity};
use crate::{
    federation::note,
    models::{
        poll::{self, Poll as PollRow, PollOption},
        user::User,
    },
    state::AppState,
};

/// The parts one federated answer document of a remote poll is built from:
/// the ballot as [`after_poll_vote`] resolved it — the poll the marks count
/// on (whose row id names the answer ids), the REMOTE question they answer,
/// and the two actor ends of the telling. Read in as plain borrows so the
/// builder stays pure, the way
/// [`ObjectActivityContext`](crate::federation::activities::ObjectActivityContext)
/// is for the interaction documents.
#[derive(Debug)]
struct Ballot<'a> {
    /// The poll's own row id: the `{vote id}` of the pinned answer ids.
    poll_id: Uuid,
    /// The question's object uri — the answers' `inReplyTo`.
    question_uri: &'a str,
    /// The voting account's actor id: `actor` and `attributedTo`.
    voter_ap_id: &'a str,
    /// The poll author's actor id: the answer's only addressee.
    author_ap_id: &'a str,
}

/// The `Create` of one answer: the bodyless `Note` naming `option`'s title
/// and answering the question behind `ballot`, under the deterministic
/// `{BASE_URL}/objects/vote-{vote id}-{option position}` object id and the
/// matching `{BASE_URL}/activities/vote-{vote id}-{option position}`
/// activity id the ticket pins (`{vote id}` is the poll's own row id, which
/// is exactly what `POST /api/v1/polls/{id}/votes` is keyed on).
///
/// The shape is the one this instance's own inbound fold
/// ([`crate::federation::handlers::vote`]) reads back: a `name`, no
/// `content`, an `inReplyTo` naming the question and an `attributedTo` of
/// the voter. The object's `to` names the poll's author alone and its `cc`
/// is empty — the answer is a mark on somebody's poll, not a post on the
/// voter's timeline — and the envelope repeats that one-address line so the
/// fan-out resolves exactly the author's personal inbox.
fn answer_document(state: &AppState, ballot: &Ballot<'_>, option: &PollOption) -> Value {
    let name = format!("vote-{}-{}", ballot.poll_id, option.position);
    let object = json!({
        "id": format!("{}/objects/{name}", state.config.base_url),
        "type": "Note",
        "attributedTo": ballot.voter_ap_id,
        "inReplyTo": ballot.question_uri,
        "name": option.title,
        "to": [ballot.author_ap_id],
        "cc": [],
    });
    json!({
        "@context": note::json_ld_context(&state.config),
        "id": format!("{}/activities/{name}", state.config.base_url),
        "type": "Create",
        "actor": ballot.voter_ap_id,
        "to": [ballot.author_ap_id],
        "cc": [],
        "object": object,
    })
}

/// Announce the ballot just cast on a REMOTE status's poll: one `Create`
/// per chosen option, all delivered to the poll author's inbox alone.
///
/// Pleroma's `CommonAPI.vote` answers a remote question by running each
/// choice through its pipeline as an `Answer`, and the relational ballot
/// here mints one answer document per mark instead (the documents' own
/// `to` names the author, so the follower of nobody's timeline is
/// expanded). Silence is the answer beside [`crate::services::notify`]'s
/// telling in every other case: a poll on a LOCAL status keeps its ballots
/// inside this instance (its author reads the counters, not the wire), a
/// gone status or author has nobody to answer, a non-local voter's ballot
/// arrived from the wire and goes no further, and an option id the poll
/// does not hold names nothing to announce.
pub async fn after_poll_vote(state: &AppState, poll: &PollRow, voter: &User, option_ids: &[Uuid]) {
    if !state.config.federation_enabled.get() {
        return;
    }
    if option_ids.is_empty() || !voter.local.get() {
        return; // only our own actors sign an answer
    }
    let Some(posted) = load_status(state, poll.status_id).await else {
        return; // a question whose row is gone has no author to answer
    };
    if posted.local {
        return; // a local poll's ballot is this instance's own fact
    }
    let Some(author) = load_user(state, posted.user_id).await else {
        return; // an author whose row is gone is nobody to tell
    };
    let options = match poll::options(&state.db, poll.id).await {
        Ok(options) => options,
        Err(err) => {
            warn!(
                %err,
                poll = %poll.id,
                "federation hook could not read the poll's options"
            );
            return;
        }
    };
    let ballot = Ballot {
        poll_id: poll.id,
        question_uri: &posted.uri,
        voter_ap_id: &voter.ap_id,
        author_ap_id: &author.ap_id,
    };
    for option_id in option_ids {
        let Some(option) = options.iter().find(|row| row.id == *option_id) else {
            continue; // a mark on no stored option federates no answer
        };
        let document = answer_document(state, &ballot, option);
        queue_activity(state, voter, &document, "poll answer").await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::{
        models::{poll::PollParams, status::Visibility},
        services::federation_out::test_support::{
            app_state, job_count, local, local_status, publish_jobs, remote, remote_status,
        },
        util::id::new_id,
    };

    /// A poll on a LOCAL status keeps its ballots inside the database: the
    /// author's counters are the whole telling, and no answer goes out —
    /// beside the other refusals of the hook: a ballot signed by nobody of
    /// ours, and marks naming options the poll does not hold.
    #[tokio::test]
    async fn a_ballot_this_instance_has_no_answer_to_send_publishes_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let state = app_state(&pool);

        let own = local_status(&pool, &alice, "our own question").await;
        let local_stand = {
            let mut conn = pool.acquire().await.expect("a slot connection");
            poll::create(
                &mut conn,
                own.id,
                &PollParams {
                    options: vec!["tea".to_owned()],
                    ..PollParams::default()
                },
            )
            .await
            .expect("the local poll creates")
        };
        let own_options = poll::options(&pool, local_stand.id)
            .await
            .expect("the options read");
        after_poll_vote(&state, &local_stand, &alice, &[own_options[0].id]).await;
        assert!(
            publish_jobs(&pool).await.is_empty(),
            "a local poll's ballot announces nothing"
        );

        let asked = remote_status(&pool, &bob, "question-remote-voter", Visibility::Public).await;
        let stand = {
            let mut conn = pool.acquire().await.expect("a slot connection");
            poll::create(
                &mut conn,
                asked.id,
                &PollParams {
                    options: vec!["tea".to_owned()],
                    ..PollParams::default()
                },
            )
            .await
            .expect("the remote poll creates")
        };
        let mirror = remote(&pool, "voter").await;
        let options = poll::options(&pool, stand.id)
            .await
            .expect("the options read");
        after_poll_vote(&state, &stand, &mirror, &[options[0].id]).await;
        assert!(
            publish_jobs(&pool).await.is_empty(),
            "a mirror's ballot came from the wire and goes nowhere"
        );

        after_poll_vote(&state, &stand, &alice, &[new_id()]).await;
        assert!(
            publish_jobs(&pool).await.is_empty(),
            "an id no option of the poll holds names nothing to announce"
        );
        assert_eq!(
            job_count(&pool).await,
            0,
            "and none of it queued anything at all"
        );
    }
}
