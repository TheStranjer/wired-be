//! The answers a ballot on a REMOTE status's poll becomes: one `Create` per
//! chosen option, its object a bodyless `Note` naming the option title and
//! answering the question's uri, addressed to the poll's author alone.
//!
//! A ballot on a poll of a local status answers nobody; what it changes is
//! the poll's tally, which [`after_local_poll_changed`] re-announces as a
//! debounced `Update` of the `Question` (after a vote, and at once through
//! [`after_local_poll_closed`] when the poll closes). The `Update` is
//! rendered by the queued job, not by the hook. The row readers and the
//! publisher hand are the root's.

use chrono::{DateTime, TimeDelta, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{debug, warn};
use uuid::Uuid;

use super::{
    load_status, load_user,
    notes::{NoteActivity, note_document, note_publisher},
    queue_activity,
};
use crate::{
    federation::{
        note,
        publisher::{self, PublishOpts},
    },
    jobs::{AttemptInfo, HandlerFuture, JobError, QUEUE_FEDERATOR_OUTGOING},
    models::{
        job::{NewJob, enqueue},
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

/// The job kind of a local poll's deferred `Update`, on
/// [`QUEUE_FEDERATOR_OUTGOING`], handled by [`handle_publish_update`].
pub const KIND_PUBLISH_UPDATE: &str = "polls.publish_update";

/// The [`KIND_PUBLISH_UPDATE`] job arguments: only the status whose poll
/// changed, so the document is rendered when the job runs.
#[derive(Debug, Deserialize)]
struct UpdateArgs {
    /// `statuses.id` of the local status carrying the poll.
    status_id: Uuid,
}

/// Re-announce the tally of a LOCAL status' poll as a debounced `Update`.
///
/// Queues a [`KIND_PUBLISH_UPDATE`] job three minutes ahead under the key
/// `poll-update:{status id}`, so votes landing meanwhile collapse into it.
///
/// Nothing is rendered here: the job carries the status id and
/// [`handle_publish_update`] renders the `Question` (current `votersCount`,
/// option counts and `closed` state) when it runs, so the one deferred
/// `Update` carries the tally as of the last vote it folded. Silence is the
/// answer when federation is off, and for a row the note publishing gate
/// refuses (a mirror, a reblog, a `local`-visibility post or an author that
/// is gone or switched off).
pub async fn after_local_poll_changed(state: &AppState, status: &Status) {
    let run_at = Utc::now() + TimeDelta::minutes(POLL_UPDATE_DELAY_MINUTES);
    queue_poll_update(state, status, run_at).await;
}

/// Announce that a LOCAL status' poll closed, without the vote delay.
///
/// The same keyed [`KIND_PUBLISH_UPDATE`] job, due now. A job already
/// pending under the key is kept: it renders the closed poll when it runs.
pub async fn after_local_poll_closed(state: &AppState, status: &Status) {
    queue_poll_update(state, status, Utc::now()).await;
}

/// Queues the keyed [`KIND_PUBLISH_UPDATE`] job of `status`, due at `run_at`.
async fn queue_poll_update(state: &AppState, status: &Status, run_at: DateTime<Utc>) {
    if !state.config.federation_enabled.get() {
        return;
    }
    let Some(author) = note_publisher(state, status).await else {
        return;
    };
    let key = poll_update_key(status.id);
    let queued = enqueue(
        &state.db,
        NewJob {
            queue: QUEUE_FEDERATOR_OUTGOING,
            kind: KIND_PUBLISH_UPDATE,
            args: json!({ "status_id": status.id }),
            run_at: Some(run_at),
            max_attempts: None,
            unique_key: Some(&key),
        },
    )
    .await;
    match queued {
        Ok(_job) => debug!(actor = %author.ap_id, "queued poll Update"),
        Err(err) => warn!(%err, actor = %author.ap_id, "could not queue poll Update"),
    }
}

/// The [`KIND_PUBLISH_UPDATE`] handler, registered by [`crate::serve`]:
/// renders the poll's `Update` now and fans it out as the author.
///
/// Malformed arguments discard; a status that is gone, no longer federates
/// or has no author to speak for is nothing to announce; a failed fan-out
/// retries.
pub fn handle_publish_update(state: AppState, _attempt: AttemptInfo, args: Value) -> HandlerFuture {
    Box::pin(async move { run_publish_update(&state, args).await })
}

/// Renders and publishes the `Update` of the status `args` names.
async fn run_publish_update(state: &AppState, args: Value) -> Result<(), JobError> {
    let args = serde_json::from_value::<UpdateArgs>(args)
        .map_err(|err| JobError::Discard(format!("malformed poll update arguments: {err}")))?;
    if !state.config.federation_enabled.get() {
        return Ok(());
    }
    let Some(status) = load_status(state, args.status_id).await else {
        return Ok(());
    };
    let Some(author) = note_publisher(state, &status).await else {
        return Ok(());
    };
    let Some(document) = note_document(state, status.id, NoteActivity::Update).await else {
        return Ok(());
    };
    publisher::publish(state, &author, &document, PublishOpts::default())
        .await
        .map(drop)
        .map_err(|err| JobError::Retry(format!("could not publish a poll Update: {err}")))
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
            accepted_follow, app_state, delivery_jobs, first_attempt, job_count, local, local_post,
            local_status, publish_jobs, remote, remote_status, state_with, unfederated,
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

    /// The `(args, unique key, due in the future)` of every queued
    /// `polls.publish_update` job.
    async fn update_jobs(pool: &PgPool) -> Vec<(Value, Option<String>, bool)> {
        sqlx::query_as::<_, (Value, Option<String>, bool)>(
            "SELECT args, unique_key, run_at > now() FROM jobs WHERE kind = $1 \
              ORDER BY inserted_at",
        )
        .bind(KIND_PUBLISH_UPDATE)
        .fetch_all(pool)
        .await
        .expect("the update jobs read")
    }

    /// Runs the queued `polls.publish_update` job for `status_id` the way
    /// a worker would, then retires its row so the key is free again.
    async fn run_update_job(state: &AppState, status_id: Uuid) {
        let outcome = handle_publish_update(
            state.clone(),
            first_attempt(),
            json!({ "status_id": status_id }),
        )
        .await;
        assert_eq!(outcome, Ok(()), "the update job runs");
        let retired = sqlx::query(
            "UPDATE jobs SET state = 'completed' WHERE kind = $1 AND state = 'available'",
        )
        .bind(KIND_PUBLISH_UPDATE)
        .execute(&state.db)
        .await
        .expect("the job retires")
        .rows_affected();
        assert_eq!(retired, 1, "the one queued job is retired");
    }

    /// The per-option `totalItems` of an `Update`'s `Question`.
    fn option_counts(body: &Value) -> Vec<i64> {
        body["object"]["oneOf"]
            .as_array()
            .expect("a single-choice Question carries oneOf")
            .iter()
            .map(|choice| choice["replies"]["totalItems"].as_i64().expect("a count"))
            .collect()
    }

    /// A vote on a poll of a LOCAL status sends no answer but queues one
    /// delayed job naming the status, with no rendered document; a second
    /// vote folds into that same job through the unique key.
    #[tokio::test]
    async fn local_votes_queue_one_deferred_update_job() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let carol = local(&pool, "carol").await;
        let alice = local(&pool, "alice").await;
        let dave = local(&pool, "dave").await;
        let state = app_state(&pool);
        let (stand, options) = local_poll(&pool, &carol).await;

        cast(&state, &stand, &alice, options[0]).await;

        let jobs = update_jobs(&pool).await;
        assert_eq!(jobs.len(), 1, "one job after the first vote");
        let (args, key, delayed) = &jobs[0];
        assert_eq!(
            *args,
            json!({ "status_id": stand.status_id }),
            "the job names the status and carries no document"
        );
        assert_eq!(
            key.as_deref(),
            Some(format!("poll-update:{}", stand.status_id).as_str()),
            "under the per-status key"
        );
        assert!(delayed, "due in the future");
        assert!(publish_jobs(&pool).await.is_empty(), "nothing rendered yet");

        cast(&state, &stand, &dave, options[1]).await;
        assert_eq!(
            update_jobs(&pool).await.len(),
            1,
            "the second vote folds into the queued job"
        );
    }

    /// Three quick votes queue one job; running it fans out one `Update`
    /// whose counts are the final tally, not the first vote's; a vote after
    /// it ran schedules a fresh job.
    #[tokio::test]
    async fn the_deferred_job_renders_the_tally_when_it_runs() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let carol = local(&pool, "carol").await;
        let alice = local(&pool, "alice").await;
        let dave = local(&pool, "dave").await;
        let erin = local(&pool, "erin").await;
        let fay = local(&pool, "fay").await;
        let bob = remote(&pool, "bob").await;
        accepted_follow(&pool, &bob, &carol).await;
        let state = app_state(&pool);
        let (stand, options) = local_poll(&pool, &carol).await;

        cast(&state, &stand, &alice, options[0]).await;
        cast(&state, &stand, &dave, options[0]).await;
        cast(&state, &stand, &erin, options[1]).await;
        assert_eq!(update_jobs(&pool).await.len(), 1, "one job for three votes");

        run_update_job(&state, stand.status_id).await;
        let deliveries = delivery_jobs(&pool).await;
        assert_eq!(deliveries.len(), 1, "one Update fans out to the follower");
        let body = &deliveries[0]["body"];
        assert_eq!(body["type"], "Update", "an Update");
        assert_eq!(body["object"]["type"], "Question", "of the Question");
        assert_eq!(body["object"]["votersCount"], 3, "with all three voters");
        assert_eq!(option_counts(body), vec![2, 1], "and the final counts");
        assert!(body["object"].get("closed").is_none(), "the poll is open");

        cast(&state, &stand, &fay, options[1]).await;
        let jobs = update_jobs(&pool).await;
        assert_eq!(jobs.len(), 2, "a vote after the run schedules a new job");
        assert!(jobs[1].2, "the new job is delayed");
    }

    /// The close hook queues an immediate job whose run publishes the
    /// `Question` with `closed` set and the final counts; a job already
    /// pending is kept, not doubled.
    #[tokio::test]
    async fn closing_a_poll_publishes_the_closed_question() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let carol = local(&pool, "carol").await;
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        accepted_follow(&pool, &bob, &carol).await;
        let state = app_state(&pool);
        let (stand, options) = local_poll(&pool, &carol).await;
        cast(&state, &stand, &alice, options[1]).await;
        let closing =
            sqlx::query("UPDATE polls SET expires_at = now() - interval '1 minute' WHERE id = $1")
                .bind(stand.id)
                .execute(&pool)
                .await
                .expect("the deadline passes")
                .rows_affected();
        assert_eq!(closing, 1, "the poll row closes");
        let asked = load_status(&state, stand.status_id)
            .await
            .expect("the status stands");

        after_local_poll_closed(&state, &asked).await;
        let pending = update_jobs(&pool).await;
        assert_eq!(pending.len(), 1, "the vote's pending job is kept");

        run_update_job(&state, stand.status_id).await;
        after_local_poll_closed(&state, &asked).await;
        let after_close = update_jobs(&pool).await;
        assert_eq!(after_close.len(), 2, "the close queues its own job");
        assert!(!after_close[1].2, "due at once, without the vote delay");

        run_update_job(&state, stand.status_id).await;
        let deliveries = delivery_jobs(&pool).await;
        let last = &deliveries.last().expect("the close Update is delivered")["body"];
        assert!(last["object"]["closed"].is_string(), "closed is set");
        assert_eq!(option_counts(last), vec![0, 1], "with the final counts");
    }

    /// The job handler discards malformed arguments and is silent for a
    /// status that is gone or an instance with federation off.
    #[tokio::test]
    async fn the_update_handler_refuses_what_it_cannot_publish() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let carol = local(&pool, "carol").await;
        let bob = remote(&pool, "bob").await;
        accepted_follow(&pool, &bob, &carol).await;
        let asked = local_status(&pool, &carol, "tea or coffee?").await;

        let malformed =
            handle_publish_update(app_state(&pool), first_attempt(), json!({ "nope": 1 })).await;
        assert!(
            matches!(malformed, Err(JobError::Discard(_))),
            "malformed arguments discard"
        );

        let gone = handle_publish_update(
            app_state(&pool),
            first_attempt(),
            json!({ "status_id": new_id() }),
        )
        .await;
        assert_eq!(gone, Ok(()), "a gone status is nothing to announce");

        let off = handle_publish_update(
            state_with(&pool, unfederated()),
            first_attempt(),
            json!({ "status_id": asked.id }),
        )
        .await;
        assert_eq!(off, Ok(()), "federation off is silence");
        assert_eq!(job_count(&pool).await, 0, "and nothing was queued");
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
