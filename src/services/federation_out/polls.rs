//! The answers a ballot on a REMOTE status's poll becomes: one `Create` per
//! chosen option, its object a bodyless `Note` naming the option title and
//! answering the question's uri, addressed to the poll's author alone.
//!
//! A ballot on a poll of a local status answers nobody; what it changes is
//! the poll's tally, which [`after_local_poll_changed`] re-announces as a
//! debounced `Update` of the `Question` (after a vote and when the poll
//! closes). The row readers and the publisher hand are the root's.

use chrono::{TimeDelta, Utc};
use serde_json::{Value, json};
use tracing::{debug, warn};
use uuid::Uuid;

use super::{
    load_status, load_user,
    notes::{NoteActivity, note_document, note_publisher},
    queue_activity,
};
use crate::{
    federation::{note, publisher},
    models::{
        poll::{self, Poll as PollRow, PollOption},
        status::Status,
        user::User,
    },
    state::AppState,
    util::id::OBJECTS_PATH,
};

/// How long a local poll's `Update` waits before it goes out: Mastodon's
/// `DistributePollUpdateWorker` delay, so a burst of votes is one fan-out.
const POLL_UPDATE_DELAY_MINUTES: i64 = 3;

/// The deduplication key of a poll's queued `Update`: one live job per
/// question, so every vote inside the delay folds into the one already
/// waiting.
fn poll_update_key(status_id: Uuid) -> String {
    format!("poll-update:{status_id}")
}

/// Re-announce the tally of a LOCAL status' poll as a debounced `Update`.
///
/// The `Question` carries its current `votersCount`, option counts and
/// `closed` state. The job is queued three minutes ahead under the key
/// `poll-update:{status id}`, so votes landing meanwhile collapse into it.
///
/// The document is the edit hook's own (the status' note wrapped in an
/// `Update`) and travels to the audience the `Create` reached. It is
/// rendered when this hook runs, so a debounced job carries the tally of
/// the first change it folded. Silence is the answer when federation is
/// off, and for a row the note publishing gate refuses (a mirror, a
/// reblog, a `local`-visibility post or an author that is gone or switched
/// off).
pub async fn after_local_poll_changed(state: &AppState, status: &Status) {
    if !state.config.federation_enabled.get() {
        return;
    }
    let Some(author) = note_publisher(state, status).await else {
        return;
    };
    let Some(document) = note_document(state, status.id, NoteActivity::Update).await else {
        return;
    };
    let key = poll_update_key(status.id);
    let run_at = Utc::now() + TimeDelta::minutes(POLL_UPDATE_DELAY_MINUTES);
    match publisher::publish_in_background_at(state, author.id, &document, &key, run_at).await {
        Ok(()) => debug!(actor = %author.ap_id, "queued poll Update"),
        Err(err) => warn!(%err, actor = %author.ap_id, "could not queue poll Update"),
    }
}

/// The parts one federated answer document of a remote poll is built from:
/// the ballot as [`after_poll_vote`] resolved it — the poll the marks count
/// on (whose row id names the answer ids), the REMOTE question they answer,
/// and the two actor ends of the telling. Read in as plain borrows so the
/// builder stays pure, the way
/// [`ObjectActivityContext`](crate::federation::activities::ObjectActivityContext)
/// is for the interaction documents.
#[derive(Debug)]
struct Ballot<'a> {
    /// The poll's own row id: the `{poll id}` of the answer ids.
    poll_id: Uuid,
    /// The voting user's row id: the `{voter id}` of the answer ids, so two
    /// local voters on one option never share an id.
    voter_id: Uuid,
    /// The question's object uri — the answers' `inReplyTo`.
    question_uri: &'a str,
    /// The voting account's actor id: `actor` and `attributedTo`.
    voter_ap_id: &'a str,
    /// The poll author's actor id: the answer's only addressee.
    author_ap_id: &'a str,
}

/// The `Create` of one answer: the bodyless `Note` naming `option`'s title
/// and answering the question behind `ballot`, under the deterministic
/// `{BASE_URL}/objects/vote-{poll id}-{voter id}-{option position}` object
/// id and the matching
/// `{BASE_URL}/activities/vote-{poll id}-{voter id}-{option position}`
/// activity id (`{poll id}` is the poll's own row id, which is exactly what
/// `POST /api/v1/polls/{id}/votes` is keyed on, and `{voter id}` the local
/// voter's row id, so each voter's answer has an id of its own).
///
/// The shape is the one this instance's own inbound fold
/// ([`crate::federation::handlers::vote`]) reads back: a `name`, no
/// `content`, an `inReplyTo` naming the question and an `attributedTo` of
/// the voter. The object's `to` names the poll's author alone and its `cc`
/// is empty — the answer is a mark on somebody's poll, not a post on the
/// voter's timeline — and the envelope repeats that one-address line so the
/// fan-out resolves exactly the author's personal inbox.
fn answer_document(state: &AppState, ballot: &Ballot<'_>, option: &PollOption) -> Value {
    let name = format!(
        "vote-{}-{}-{}",
        ballot.poll_id, ballot.voter_id, option.position
    );
    let object = json!({
        "id": format!("{}{OBJECTS_PATH}{name}", state.config.base_url),
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
/// expanded). A poll on a LOCAL status sends no answer (its author reads
/// the counters, not the wire) but re-announces its tally through
/// [`after_local_poll_changed`]. Silence is the answer beside
/// [`crate::services::notify`]'s telling in every other case: a
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
        // a local poll's ballot answers nobody; the tally is what changed
        after_local_poll_changed(state, &posted).await;
        return;
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
        voter_id: voter.id,
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

    use sqlx::PgPool;

    use crate::{
        models::{poll::PollParams, status::Visibility},
        services::federation_out::test_support::{
            app_state, job_count, local, local_post, local_status, publish_jobs, remote,
            remote_status, state_with, unfederated,
        },
        util::id::new_id,
    };

    /// A two-option poll on the LOCAL public status `author` asks: the
    /// poll row and its option ids.
    async fn local_poll(pool: &PgPool, author: &User) -> (PollRow, Vec<Uuid>) {
        let asked = local_status(pool, author, "tea or coffee?").await;
        let stand = {
            let mut conn = pool.acquire().await.expect("a slot connection");
            poll::create(
                &mut conn,
                asked.id,
                &PollParams {
                    options: vec!["tea".to_owned(), "coffee".to_owned()],
                    ..PollParams::default()
                },
            )
            .await
            .expect("the local poll creates")
        };
        let ids = poll::options(pool, stand.id)
            .await
            .expect("the options read")
            .into_iter()
            .map(|option| option.id)
            .collect();
        (stand, ids)
    }

    /// Writes `voter`'s mark on `option` and runs the vote hook, the way
    /// the vote endpoint does.
    async fn cast(state: &AppState, stand: &PollRow, voter: &User, option: Uuid) {
        assert!(
            poll::cast_vote(&state.db, stand.id, voter.id, &[option])
                .await
                .expect("the ballot writes"),
            "the fixture ballot counts"
        );
        after_poll_vote(state, stand, voter, &[option]).await;
    }

    /// Whether every queued publish job is due strictly in the future.
    async fn all_publishes_delayed(pool: &PgPool) -> bool {
        sqlx::query_scalar::<_, bool>("SELECT bool_and(run_at > now()) FROM jobs WHERE kind = $1")
            .bind(publisher::KIND_PUBLISH)
            .fetch_one(pool)
            .await
            .expect("the run_at reads")
    }

    /// A vote on a poll of a LOCAL status sends no answer but queues one
    /// delayed `Update` of the `Question` carrying the new count; a second
    /// vote folds into that same job through the unique key.
    #[tokio::test]
    async fn local_votes_queue_one_debounced_question_update() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let carol = local(&pool, "carol").await;
        let alice = local(&pool, "alice").await;
        let dave = local(&pool, "dave").await;
        let state = app_state(&pool);
        let (stand, options) = local_poll(&pool, &carol).await;

        cast(&state, &stand, &alice, options[0]).await;

        let jobs = publish_jobs(&pool).await;
        assert_eq!(jobs.len(), 1, "one job after the first vote");
        let (args, key) = &jobs[0];
        let activity = &args["activity"];
        assert_eq!(activity["type"], "Update", "an Update, not an answer");
        assert_eq!(activity["object"]["type"], "Question", "of the Question");
        assert_eq!(activity["object"]["votersCount"], 1, "with the new count");
        assert_eq!(
            args["actor_id"],
            json!(carol.id),
            "published as the poll's author"
        );
        assert_eq!(
            key.as_deref(),
            Some(format!("poll-update:{}", stand.status_id).as_str()),
            "under the per-status key"
        );
        assert!(all_publishes_delayed(&pool).await, "due in the future");

        cast(&state, &stand, &dave, options[1]).await;
        assert_eq!(
            publish_jobs(&pool).await.len(),
            1,
            "the second vote folds into the queued job"
        );
    }

    /// The Update hook stays silent when federation is off, when the poll's
    /// status is a mirror, and when the status is `local`-visibility.
    #[tokio::test]
    async fn the_poll_update_hook_is_silent_where_it_has_no_business() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let carol = local(&pool, "carol").await;
        let bob = remote(&pool, "bob").await;
        let asked = local_status(&pool, &carol, "tea or coffee?").await;

        after_local_poll_changed(&state_with(&pool, unfederated()), &asked).await;
        assert_eq!(job_count(&pool).await, 0, "federation off: silence");

        let state = app_state(&pool);
        let mirrored = remote_status(&pool, &bob, "question-mirror", Visibility::Public).await;
        after_local_poll_changed(&state, &mirrored).await;
        assert_eq!(job_count(&pool).await, 0, "a mirror is its origin's");

        let hidden = local_post(&pool, &carol, "instance only", Visibility::Local).await;
        after_local_poll_changed(&state, &hidden).await;
        assert_eq!(job_count(&pool).await, 0, "a local-visibility post stays");
    }

    /// A vote on a REMOTE poll is unchanged: one answer `Create` to the
    /// author, no delay, no Update.
    #[tokio::test]
    async fn a_remote_poll_vote_still_sends_only_the_answer() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let state = app_state(&pool);
        let asked = remote_status(&pool, &bob, "question-answered", Visibility::Public).await;
        let stand = {
            let mut conn = pool.acquire().await.expect("a slot connection");
            poll::create(
                &mut conn,
                asked.id,
                &PollParams {
                    options: vec!["tea".to_owned(), "coffee".to_owned()],
                    ..PollParams::default()
                },
            )
            .await
            .expect("the remote poll creates")
        };
        let options = poll::options(&pool, stand.id)
            .await
            .expect("the options read");

        cast(&state, &stand, &alice, options[0].id).await;

        let jobs = publish_jobs(&pool).await;
        assert_eq!(jobs.len(), 1, "the answer alone");
        assert_eq!(jobs[0].0["activity"]["type"], "Create", "a Create");
        assert_eq!(jobs[0].1, None, "not debounced");
        assert!(!all_publishes_delayed(&pool).await, "and not delayed");
    }

    /// A two-option poll on the REMOTE public status `question` of `bob`.
    async fn remote_poll(pool: &PgPool, question: &str) -> (PollRow, Vec<PollOption>) {
        let bob = remote(pool, "bob").await;
        let asked = remote_status(pool, &bob, question, Visibility::Public).await;
        let stand = {
            let mut conn = pool.acquire().await.expect("a slot connection");
            poll::create(
                &mut conn,
                asked.id,
                &PollParams {
                    options: vec!["tea".to_owned(), "coffee".to_owned(), "cocoa".to_owned()],
                    multiple: true,
                    ..PollParams::default()
                },
            )
            .await
            .expect("the remote poll creates")
        };
        let options = poll::options(pool, stand.id)
            .await
            .expect("the options read");
        (stand, options)
    }

    /// The ids of the `Create` and of its object in a queued answer.
    fn answer_ids(args: &Value) -> (String, String) {
        let activity = &args["activity"];
        (
            activity["id"].as_str().expect("an activity id").to_owned(),
            activity["object"]["id"]
                .as_str()
                .expect("an object id")
                .to_owned(),
        )
    }

    /// Two local voters choosing the same option of one remote poll queue
    /// two answers under distinct activity and object ids.
    #[tokio::test]
    async fn two_local_voters_on_one_option_get_distinct_answer_ids() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let wanda = local(&pool, "wanda").await;
        let walt = local(&pool, "walt").await;
        let state = app_state(&pool);
        let (stand, options) = remote_poll(&pool, "question-two-voters").await;

        cast(&state, &stand, &wanda, options[0].id).await;
        cast(&state, &stand, &walt, options[0].id).await;

        let jobs = publish_jobs(&pool).await;
        assert_eq!(jobs.len(), 2, "one answer per voter");
        let (first_activity, first_object) = answer_ids(&jobs[0].0);
        let (second_activity, second_object) = answer_ids(&jobs[1].0);
        assert_ne!(first_activity, second_activity, "distinct activity ids");
        assert_ne!(first_object, second_object, "distinct object ids");
        assert!(
            first_activity.contains(&wanda.id.to_string())
                || second_activity.contains(&wanda.id.to_string()),
            "the voter's id is part of the activity id"
        );
    }

    /// One voter's multiple-choice ballot on options 0 and 2 queues two
    /// answers under distinct ids.
    #[tokio::test]
    async fn a_multiple_choice_ballot_gets_one_id_per_option() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let wanda = local(&pool, "wanda").await;
        let state = app_state(&pool);
        let (stand, options) = remote_poll(&pool, "question-multiple").await;
        let chosen = [options[0].id, options[2].id];

        assert!(
            poll::cast_vote(&state.db, stand.id, wanda.id, &chosen)
                .await
                .expect("the ballot writes"),
            "the fixture ballot counts"
        );
        after_poll_vote(&state, &stand, &wanda, &chosen).await;

        let jobs = publish_jobs(&pool).await;
        assert_eq!(jobs.len(), 2, "one answer per chosen option");
        let (first_activity, first_object) = answer_ids(&jobs[0].0);
        let (second_activity, second_object) = answer_ids(&jobs[1].0);
        assert_ne!(first_activity, second_activity, "distinct activity ids");
        assert_ne!(first_object, second_object, "distinct object ids");
        assert!(
            first_activity.ends_with("-0") && second_activity.ends_with("-2"),
            "each id ends in its option position"
        );
    }

    /// The refusals of the answer hook: a ballot signed by nobody of ours,
    /// and marks naming options the poll does not hold.
    #[tokio::test]
    async fn a_ballot_this_instance_has_no_answer_to_send_publishes_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let state = app_state(&pool);

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
