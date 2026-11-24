//! The outbound federation of poll votes over HTTP (WBE-181): the answer
//! activities `POST /api/v1/polls/{id}/votes` hands the publisher's queue
//! when the voted poll stands on a REMOTE status
//! (`src/services/federation_out::after_poll_vote`).
//!
//! A ballot on a remote question counts locally exactly as on a local one —
//! the marks, the option counts and the poll's totals all move — and beside
//! that counting this instance sends one `Create` per chosen option: its
//! object a bodyless `Note` naming the option title and answering the
//! question's uri under the `{BASE_URL}/objects/vote-{vote id}-{option
//! position}` id, the activity under the matching `{BASE_URL}/activities/…`
//! id, both addressed to the poll's author alone, and the fan-out delivers
//! to their personal inbox only. A vote on a LOCAL poll sends no answer —
//! its author reads the counters, not the wire — and queues one debounced
//! `Update` of the `Question`. The endpoint's refusals (the
//! invisible question, the author's own ballot) write nothing and announce
//! nothing.
//!
//! The endpoint's own rules are `polls.rs`; the folding of the answers a
//! PEER sends is `poll_votes.rs`; this file is only about what leaves the
//! instance because of the vote.

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::TEST_BASE_URL,
    federation::{delivery, publisher},
    jobs::AttemptInfo,
    models::{
        poll::{self, Poll, PollOption},
        status::{self, NewStatus, Status, Visibility},
        user::User,
    },
    util::id::new_id,
};

use crate::common;

/// The votes route of the poll `id` names.
const VOTES: &str = "/api/v1/polls";
/// The instance origin the fixtures speak under.
const BASE: &str = TEST_BASE_URL;
/// The host every remote fixture lives on.
const HOST: &str = "remote.example";
/// The first attempt a background handler runs on.
const FIRST_ATTEMPT: AttemptInfo = AttemptInfo {
    attempts: 1,
    max_attempts: 5,
};

/// The `@context` every document built here carries, golden-spelled: the
/// `ActivityStreams` vocabulary, this instance's schema and the `und`
/// language map.
fn context() -> Value {
    json!([
        "https://www.w3.org/ns/activitystreams",
        "https://test.example/schemas/litepub-0.1.jsonld",
        { "@language": "und" },
    ])
}

/// A local voter with a token granting `write:statuses`.
async fn voter(pool: &PgPool) -> (User, String) {
    let alice = common::create_local_user(pool, "alice").await;
    let token = common::create_app_and_token(pool, &alice, "write:statuses").await;
    (alice, token)
}

/// A REMOTE status of `author` carrying a poll of `options` — `multiple`
/// lets one voter mark more than one — seen as `visibility`, under a fresh
/// `{HOST}/objects/question-{uuid}` uri.
async fn remote_question(
    pool: &PgPool,
    author: &User,
    options: &[&str],
    multiple: bool,
    visibility: Visibility,
) -> (Status, Poll) {
    let posted = status::insert(
        pool,
        BASE,
        &NewStatus {
            uri: Some(format!("https://{HOST}/objects/question-{}", new_id())),
            user_id: author.id,
            visibility,
            content: "<p>tea or coffee?</p>".to_owned(),
            ..NewStatus::default()
        },
    )
    .await
    .expect("the remote question inserts");
    let mut conn = pool.acquire().await.expect("a slot connection");
    let stand = poll::create(
        &mut conn,
        posted.id,
        &poll::PollParams {
            options: options.iter().map(|title| (*title).to_owned()).collect(),
            multiple,
            ..poll::PollParams::default()
        },
    )
    .await
    .expect("the remote poll creates");
    (posted, stand)
}

/// A LOCAL status of `author` carrying a two-option poll — the fixture the
/// "no answer" vote marks.
async fn local_question(pool: &PgPool, author: &User) -> Poll {
    let posted = common::create_status(pool, author, "tea or coffee?", Visibility::Public).await;
    let mut conn = pool.acquire().await.expect("a slot connection");
    poll::create(
        &mut conn,
        posted.id,
        &poll::PollParams {
            options: vec!["tea".to_owned(), "coffee".to_owned()],
            ..poll::PollParams::default()
        },
    )
    .await
    .expect("the local poll creates")
}

/// `POST /api/v1/polls/{id}/votes` as `token` with the JSON `body`.
async fn vote(app: &Router, token: &str, poll_id: &str, body: Value) -> common::TestResponse {
    common::TestRequest::post(&format!("{VOTES}/{poll_id}/votes"))
        .bearer(token)
        .json(&body)
        .send(app)
        .await
}

/// A ballot asserted accepted, answered as the updated `Poll` entity.
async fn counted(app: &Router, token: &str, poll_id: &str, choices: Value) -> Value {
    let response = vote(app, token, poll_id, json!({ "choices": choices })).await;
    common::assert_answer(&response, StatusCode::OK, "the ballot counts");
    response.json_value()
}

/// The stored poll `poll_id` names, demanded.
async fn stored_poll(pool: &PgPool, poll_id: Uuid) -> Poll {
    poll::get_by_id(pool, poll_id)
        .await
        .expect("the poll reads")
        .expect("the poll stands")
}

/// The stored options of poll `poll_id`, in position order.
async fn stored_options(pool: &PgPool, poll_id: Uuid) -> Vec<PollOption> {
    poll::options(pool, poll_id)
        .await
        .expect("the options read")
}

/// How many marks `poll_id`'s `user_id` holds.
async fn mark_rows(pool: &PgPool, poll_id: Uuid, user_id: Uuid) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM poll_votes WHERE poll_id = $1 AND user_id = $2",
    )
    .bind(poll_id)
    .bind(user_id)
    .fetch_one(pool)
    .await
    .expect("the marks count reads")
}

/// The queued `federator.publish` activities, in insertion order.
async fn published(pool: &PgPool) -> Vec<Value> {
    sqlx::query_as::<_, (Value,)>(
        "SELECT args->'activity' FROM jobs WHERE kind = $1 ORDER BY inserted_at, id",
    )
    .bind(publisher::KIND_PUBLISH)
    .fetch_all(pool)
    .await
    .expect("the publish jobs read")
    .into_iter()
    .map(|(activity,)| activity)
    .collect()
}

/// The activity of the one `federator.publish` job the queue holds.
async fn the_published_activity(pool: &PgPool) -> Value {
    let jobs = published(pool).await;
    assert_eq!(jobs.len(), 1, "the ballot is announced once");
    jobs[0].clone()
}

/// How many `federator.deliver` jobs the queue holds, of any activity.
async fn delivery_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM jobs WHERE kind = $1")
        .bind(delivery::KIND_DELIVER)
        .fetch_one(pool)
        .await
        .expect("the delivery count reads")
}

/// Runs every queued publish through the publisher's own handler, so the
/// addressing turns into the delivery jobs a worker would send.
async fn fan_out(pool: &PgPool) {
    let state = common::test_app_state(pool.clone(), common::test_config());
    let rows = sqlx::query_as::<_, (Value,)>(
        "SELECT args FROM jobs WHERE kind = $1 ORDER BY inserted_at, id",
    )
    .bind(publisher::KIND_PUBLISH)
    .fetch_all(pool)
    .await
    .expect("the publish jobs read");
    for (args,) in rows {
        publisher::handle(state.clone(), FIRST_ATTEMPT, args)
            .await
            .expect("the fan-out job completes");
    }
}

/// The inboxes the queued `federator.deliver` jobs name, sorted.
async fn delivered(pool: &PgPool) -> Vec<String> {
    sqlx::query_as::<_, (String,)>("SELECT args->>'inbox' FROM jobs WHERE kind = $1 ORDER BY 1")
        .bind(delivery::KIND_DELIVER)
        .fetch_all(pool)
        .await
        .expect("the delivery jobs read")
        .into_iter()
        .map(|(inbox,)| inbox)
        .collect()
}

/// The whole answer activity the ticket pins for `option`: the bodyless
/// `Note` naming its title and answering `question_uri`, under the
/// poll-keyed `vote-{poll id}-{option position}` ids, voter as actor and
/// `attributedTo`, author as the only addressee.
fn expected_answer(
    stand: &Poll,
    option: &PollOption,
    voter: &User,
    author: &User,
    question_uri: &str,
) -> Value {
    let name = format!("vote-{}-{}", stand.id, option.position);
    json!({
        "@context": context(),
        "id": format!("{BASE}/activities/{name}"),
        "type": "Create",
        "actor": voter.ap_id,
        "to": [author.ap_id],
        "cc": [],
        "object": {
            "id": format!("{BASE}/objects/{name}"),
            "type": "Note",
            "attributedTo": voter.ap_id,
            "inReplyTo": question_uri,
            "name": option.title,
            "to": [author.ap_id],
            "cc": [],
        },
    })
}

/// A single-choice vote on a REMOTE poll counts every local number the
/// same way a local ballot does and publishes exactly one answer: the
/// whole `Create` document, field for field, delivered to the poll author's
/// personal inbox alone.
#[tokio::test]
async fn a_single_choice_vote_on_a_remote_poll_publishes_one_answer() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = voter(&pool).await;
    let bob = common::create_remote_user(&pool, "bob", HOST).await;
    let (posted, stand) =
        remote_question(&pool, &bob, &["tea", "coffee"], false, Visibility::Public).await;

    let entity = counted(&app, &token, &stand.id.to_string(), json!([1])).await;
    assert_eq!(entity["voted"], true, "the ballot reads back voted");
    assert_eq!(entity["own_votes"], json!([1]), "the cast position");
    assert_eq!(entity["votes_count"], 1, "the total moved");
    assert_eq!(entity["voters_count"], 1, "and the voter count");
    assert_eq!(entity["options"][1]["votes_count"], 1, "the marked option");
    assert_eq!(entity["options"][0]["votes_count"], 0, "and the plain one");

    let stored = stored_poll(&pool, stand.id).await;
    assert_eq!(
        (stored.votes_count, stored.voters_count),
        (1, 1),
        "rows moved"
    );
    assert_eq!(
        mark_rows(&pool, stand.id, alice.id).await,
        1,
        "one mark stands"
    );

    let options = stored_options(&pool, stand.id).await;
    let activity = the_published_activity(&pool).await;
    assert_eq!(
        activity,
        expected_answer(&stand, &options[1], &alice, &bob, &posted.uri),
        "the whole delivery JSON for the one chosen option"
    );

    fan_out(&pool).await;
    assert_eq!(
        delivered(&pool).await,
        vec!["https://remote.example/users/bob/inbox"],
        "the poll author's personal inbox is the only delivery"
    );
}

/// A multiple-choice ballot on a REMOTE poll publishes one answer per
/// chosen option, in the order the body marked them, and counts the voter
/// exactly once across them.
#[tokio::test]
async fn a_multiple_choice_ballot_publishes_one_answer_per_option() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = voter(&pool).await;
    let bob = common::create_remote_user(&pool, "bob", HOST).await;
    let (posted, stand) = remote_question(
        &pool,
        &bob,
        &["tea", "coffee", "cocoa"],
        true,
        Visibility::Public,
    )
    .await;

    let entity = counted(&app, &token, &stand.id.to_string(), json!([2, 0])).await;
    assert_eq!(
        entity["own_votes"],
        json!([0, 2]),
        "the entity names the marked positions in the poll's own order"
    );
    assert_eq!(entity["votes_count"], 2, "two marks count twice");
    assert_eq!(entity["voters_count"], 1, "and the voter once");

    let stored = stored_poll(&pool, stand.id).await;
    assert_eq!(
        (stored.votes_count, stored.voters_count),
        (2, 1),
        "rows moved"
    );
    assert_eq!(
        mark_rows(&pool, stand.id, alice.id).await,
        2,
        "two marks stand"
    );

    let options = stored_options(&pool, stand.id).await;
    let activities = published(&pool).await;
    assert_eq!(activities.len(), 2, "one Create per chosen option");
    assert_eq!(
        activities[0],
        expected_answer(&stand, &options[2], &alice, &bob, &posted.uri),
        "the whole first answer, for the first marked option"
    );
    assert_eq!(
        activities[1],
        expected_answer(&stand, &options[0], &alice, &bob, &posted.uri),
        "and the whole second, for the second marked one"
    );

    fan_out(&pool).await;
    assert_eq!(
        delivered(&pool).await,
        vec![
            "https://remote.example/users/bob/inbox",
            "https://remote.example/users/bob/inbox",
        ],
        "both answers travel to the author's inbox, as two activities"
    );
}

/// A vote on a LOCAL poll records itself exactly as the remote arm does —
/// and sends no answer: the one thing queued is the debounced `Update` of
/// the poll's own `Question`, due in the future.
#[tokio::test]
async fn a_vote_on_a_local_poll_queues_only_a_question_update() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_, token) = voter(&pool).await;
    let carol = common::create_local_user(&pool, "carol").await;
    let stand = local_question(&pool, &carol).await;

    let _entity = counted(&app, &token, &stand.id.to_string(), json!([0])).await;

    let stored = stored_poll(&pool, stand.id).await;
    assert_eq!(
        (stored.votes_count, stored.voters_count),
        (1, 1),
        "the ballot counted"
    );
    let queued = published(&pool).await;
    assert_eq!(queued.len(), 1, "one job: the tally's Update");
    assert_eq!(queued[0]["type"], "Update", "an Update, not an answer");
    assert_eq!(queued[0]["object"]["type"], "Question", "of the question");
    assert_eq!(queued[0]["object"]["votersCount"], 1, "with the new tally");
    assert_eq!(delivery_count(&pool).await, 0, "and no delivery job yet");
}

/// A REMOTE private question the voter may not see is the shared 404: no
/// ballot, no answer, no queue — the vote endpoint accepts remote polls as
/// far as the visibility model lets them be seen.
#[tokio::test]
async fn an_invisible_remote_question_is_the_shared_404() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = voter(&pool).await;
    let bob = common::create_remote_user(&pool, "bob", HOST).await;
    let (_posted, stand) =
        remote_question(&pool, &bob, &["tea", "coffee"], false, Visibility::Private).await;

    let response = vote(&app, &token, &stand.id.to_string(), json!({"choices": [0]})).await;
    common::assert_error(&response, "Record not found", "the invisible question");
    assert_eq!(response.status, StatusCode::NOT_FOUND, "one 404");

    assert_eq!(
        mark_rows(&pool, stand.id, alice.id).await,
        0,
        "the refused ballot marked nothing"
    );
    assert!(published(&pool).await.is_empty(), "and announced nothing");
}

/// The author check stands on a remote question too: a local account whose
/// own post this instance mirrors as remote may not vote on its poll, and
/// the refusal queues nothing.
#[tokio::test]
async fn the_author_of_a_remote_question_cannot_vote_on_it() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = voter(&pool).await;
    let mut conn = pool.acquire().await.expect("a slot connection");
    let posted = status::insert(
        &pool,
        BASE,
        &NewStatus {
            uri: Some(format!("https://{HOST}/objects/alices-own")),
            user_id: alice.id,
            content: "<p>my own question, mirrored back</p>".to_owned(),
            ..NewStatus::default()
        },
    )
    .await
    .expect("the mirrored question inserts");
    let stand = poll::create(
        &mut conn,
        posted.id,
        &poll::PollParams {
            options: vec!["tea".to_owned(), "coffee".to_owned()],
            ..poll::PollParams::default()
        },
    )
    .await
    .expect("the mirrored poll creates");
    drop(conn);

    let response = vote(&app, &token, &stand.id.to_string(), json!({"choices": [0]})).await;
    common::assert_answer(
        &response,
        StatusCode::UNPROCESSABLE_ENTITY,
        "the author's ballot is refused",
    );
    common::assert_error(&response, "Poll's author can't vote", "its wording");

    assert!(
        published(&pool).await.is_empty(),
        "and nothing was announced"
    );
}
