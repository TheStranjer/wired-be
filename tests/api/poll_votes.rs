//! Inbound poll votes over federation (WBE-174): the dispatcher's own entry
//! point [`handle_incoming`] folding a `Create` of a bodyless `Note` that
//! names one option of this instance's own poll, and the counts the
//! delivery leaves behind (`src/federation/handlers/vote.rs`).
//!
//! Every test delivers the fixture activity through the real dispatcher —
//! the same job the inbox routes enqueue, run in-process with the signer's
//! stored mirror as its own row — and reads the answer from the database.
//! What this pins, per the ticket:
//!
//! - a delivered ballot moves the option's count, the poll's totals, and
//!   raises no notification for the author (`poll` is the end-of-poll one);
//! - a [`multiple`](wired_be::models::poll::Poll::multiple) poll takes one
//!   `Create` per chosen option as one voter's ballot: every named option
//!   moves, the voter counts once, and the author is told nothing;
//! - refused ballots mark nothing — the second ballot on a one-option poll,
//!   a vote past the deadline, an option the poll does not carry, the poll's
//!   own author, and a question the voter may not see;
//! - a `Note` that answers a poll-less post is not claimed by the guard at
//!   all: it folds through the ordinary create path and lands as a reply.
//!
//! [`handle_incoming`]: wired_be::federation::dispatch::handle_incoming

#![cfg(test)]

use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    models::{
        poll::{self, Poll, PollOption},
        status::{self, Status, Visibility},
        user::User,
    },
    state::AppState,
};

use crate::common::{self, deliver, delivering_state};

/// The remote voter every fixture delivers as.
const BOB: &str = "https://remote.example/users/bob";
/// The addressing a public answer carries.
const PUBLIC: &str = "https://www.w3.org/ns/activitystreams#Public";

/// The host segment of an actor id, for building activity and object ids
/// that containment will accept from that actor.
fn host_of(id: &str) -> String {
    id.split('/')
        .nth(2)
        .expect("the fixture id is an http(s) url")
        .to_owned()
}

/// A `Create` of the answer this instance folds: delivered by `by`, naming
/// the option `option`, answering the question at `question_uri`, ids for
/// this delivery named by `n`, and the object fields `extra` merged over
/// the fixture.
fn answer_create(by: &str, question_uri: &str, option: &str, n: &str, extra: Value) -> Value {
    let host = host_of(by);
    let object = json!({
        "id": format!("https://{host}/objects/vote-{n}"),
        "type": "Note",
        "attributedTo": by,
        "to": [PUBLIC],
        "inReplyTo": question_uri,
        "name": option,
    });
    let object = if let Value::Object(fields) = extra {
        let mut merged = object.as_object().expect("the fixture is a map").clone();
        for (key, value) in fields {
            let _previous = merged.insert(key, value);
        }
        Value::Object(merged)
    } else {
        object
    };
    json!({
        "id": format!("https://{host}/activities/vote-{n}"),
        "type": "Create",
        "actor": by,
        "object": object,
    })
}

/// Deliver `bob`'s answer naming `option` on the question at `question_uri`,
/// under the delivery-suffix `n`.
async fn bob_answers(state: &AppState, bob: &User, question_uri: &str, option: &str, n: &str) {
    let activity = answer_create(BOB, question_uri, option, n, json!({}));
    deliver(state, &activity, bob).await;
}

/// Alice (the local author) and the poll of `options` standing on her
/// question — `multiple` lets one voter mark more than one option.
async fn local_poll(
    pool: &PgPool,
    options: &[&str],
    multiple: bool,
    visibility: Visibility,
) -> (User, Status, Poll) {
    let alice = common::create_local_user(pool, "alice").await;
    let posted = common::create_status(pool, &alice, "tea or coffee?", visibility).await;
    let ballot = poll::PollParams {
        options: options.iter().map(|title| (*title).to_owned()).collect(),
        multiple,
        ..poll::PollParams::default()
    };
    let mut conn = pool.acquire().await.expect("slot connection");
    let stand = poll::create(&mut conn, posted.id, &ballot)
        .await
        .expect("the fixture poll creates");
    drop(conn);
    (alice, posted, stand)
}

/// The stored poll's roll-up columns.
async fn stored_poll(pool: &PgPool, poll_id: Uuid) -> Poll {
    poll::get_by_id(pool, poll_id)
        .await
        .expect("the poll reads")
        .expect("the poll stands")
}

/// The poll's options as `(position, votes_count)` pairs.
async fn option_counts(pool: &PgPool, poll_id: Uuid) -> Vec<(i32, i32)> {
    poll::options(pool, poll_id)
        .await
        .expect("the options read")
        .into_iter()
        .map(|option: PollOption| (option.position, option.votes_count))
        .collect()
}

/// `user_id`'s notification rows, oldest first, as
/// `(type, from_account_id, status_id)` triples.
async fn note_rows(pool: &PgPool, user_id: Uuid) -> Vec<(String, Uuid, Option<Uuid>)> {
    use sqlx::Row as _;
    let rows = sqlx::query(
        "SELECT type, from_account_id, status_id \
           FROM notifications WHERE user_id = $1 ORDER BY id",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
    .expect("the notification rows read");
    rows.iter()
        .map(|row| {
            (
                row.get::<String, _>("type"),
                row.get::<Uuid, _>("from_account_id"),
                row.get::<Option<Uuid>, _>("status_id"),
            )
        })
        .collect()
}

/// How many `poll_votes` rows the poll holds.
async fn mark_rows(pool: &PgPool, poll_id: Uuid) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM poll_votes WHERE poll_id = $1")
        .bind(poll_id)
        .fetch_one(pool)
        .await
        .expect("the marks count reads")
}

/// How many `statuses` rows carry object uri `uri`.
async fn rows_for_uri(pool: &PgPool, uri: &str) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM statuses WHERE uri = $1")
        .bind(uri)
        .fetch_one(pool)
        .await
        .expect("the status count reads")
}

/// A delivered ballot moves its option's count, the poll's totals once
/// each, and tells the author nothing — and stores no status, because a
/// claimed answer is a mark, never a post.
#[tokio::test]
async fn a_delivered_ballot_moves_the_counts_and_tells_nobody() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = delivering_state(&pool);
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let (alice, posted, stand) =
        local_poll(&pool, &["tea", "coffee"], false, Visibility::Public).await;

    bob_answers(&state, &bob, &posted.uri, "tea", "one").await;

    let counted = stored_poll(&pool, stand.id).await;
    assert_eq!(counted.votes_count, 1, "the ballot counts once");
    assert_eq!(counted.voters_count, 1, "and the voter is counted once");
    assert_eq!(
        option_counts(&pool, stand.id).await,
        vec![(0, 1), (1, 0)],
        "the named option gained the mark and its rival kept nothing"
    );
    assert_eq!(
        rows_for_uri(&pool, "https://remote.example/objects/vote-one").await,
        0,
        "the claimed answer is no post"
    );

    // A ballot notifies nobody: `poll` is raised when the poll ends.
    assert!(
        note_rows(&pool, alice.id).await.is_empty(),
        "the author is not told of the ballot"
    );
    assert!(
        note_rows(&pool, bob.id).await.is_empty(),
        "and the voter hears nothing of their own ballot"
    );
}

/// A multiple-choice ballot arrives as one `Create` per chosen option; the
/// poll takes each as the same voter's mark, counts the voter once, and
/// tells the author once.
#[tokio::test]
async fn a_multiple_choice_ballot_arrives_as_one_create_per_option() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = delivering_state(&pool);
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let (alice, posted, stand) =
        local_poll(&pool, &["tea", "coffee", "water"], true, Visibility::Public).await;

    bob_answers(&state, &bob, &posted.uri, "tea", "first").await;
    bob_answers(&state, &bob, &posted.uri, "coffee", "second").await;

    let counted = stored_poll(&pool, stand.id).await;
    assert_eq!(counted.votes_count, 2, "two options, two votes");
    assert_eq!(counted.voters_count, 1, "one voter, however many creates");
    assert_eq!(
        option_counts(&pool, stand.id).await,
        vec![(0, 1), (1, 1), (2, 0)],
        "each delivered option moved and the untouched one did not"
    );
    assert_eq!(
        mark_rows(&pool, stand.id).await,
        2,
        "two marks, one ballot-set"
    );

    assert!(
        note_rows(&pool, alice.id).await.is_empty(),
        "no create of the multiple-choice ballot told the author anything"
    );
}

/// The same option delivered twice is one mark; and on a poll that takes
/// one ballot per voter, the *next* option's `Create` is refused too — the
/// counts stand where the first ballot left them.
#[tokio::test]
async fn a_second_ballot_on_a_single_choice_poll_counts_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = delivering_state(&pool);
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let (alice, posted, stand) =
        local_poll(&pool, &["tea", "coffee"], false, Visibility::Public).await;

    bob_answers(&state, &bob, &posted.uri, "tea", "one").await;
    bob_answers(&state, &bob, &posted.uri, "tea", "repeat").await;
    bob_answers(&state, &bob, &posted.uri, "coffee", "other").await;

    let counted = stored_poll(&pool, stand.id).await;
    assert_eq!(counted.votes_count, 1, "the first ballot is the only one");
    assert_eq!(counted.voters_count, 1, "and the voter stands counted once");
    assert_eq!(
        option_counts(&pool, stand.id).await,
        vec![(0, 1), (1, 0)],
        "neither the repeat nor the second option moved a count"
    );
    assert_eq!(mark_rows(&pool, stand.id).await, 1, "one mark row");

    assert!(
        note_rows(&pool, alice.id).await.is_empty(),
        "neither the ballot nor the refusals told the author anything"
    );
}

/// A ballot that arrives after the deadline marks nothing — the expiry is
/// back-dated in the fixture, never slept on.
#[tokio::test]
async fn a_vote_on_an_expired_poll_is_ignored() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = delivering_state(&pool);
    let (alice, posted, stand) =
        local_poll(&pool, &["tea", "coffee"], false, Visibility::Public).await;
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let moved =
        sqlx::query("UPDATE polls SET expires_at = now() - interval '1 hour' WHERE id = $1")
            .bind(stand.id)
            .execute(&pool)
            .await
            .expect("the deadline moves");
    assert_eq!(moved.rows_affected(), 1, "the fixture poll is stored");

    bob_answers(&state, &bob, &posted.uri, "tea", "late").await;

    assert_eq!(
        stored_poll(&pool, stand.id).await.votes_count,
        0,
        "the closed poll counted nothing"
    );
    assert_eq!(mark_rows(&pool, stand.id).await, 0, "and holds no marks");
    assert!(
        note_rows(&pool, alice.id).await.is_empty(),
        "and the author was not told"
    );
}

/// An answer naming a title the poll does not carry marks nothing; and
/// because the guard claimed it, it is no post either.
#[tokio::test]
async fn a_vote_naming_an_option_the_poll_does_not_carry_is_ignored() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = delivering_state(&pool);
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let (alice, posted, stand) =
        local_poll(&pool, &["tea", "coffee"], false, Visibility::Public).await;

    bob_answers(&state, &bob, &posted.uri, "sandwich", "none").await;

    assert_eq!(
        stored_poll(&pool, stand.id).await.votes_count,
        0,
        "no option of this poll carries that title"
    );
    assert_eq!(mark_rows(&pool, stand.id).await, 0, "so nothing was marked");
    assert_eq!(
        rows_for_uri(&pool, "https://remote.example/objects/vote-none").await,
        0,
        "and the claimed answer was not folded into a status"
    );
    assert!(
        note_rows(&pool, alice.id).await.is_empty(),
        "and nobody was told"
    );
}

/// The poll's own author cannot mark her question: delivered as alice, the
/// ballot moves nothing.
#[tokio::test]
async fn the_poll_author_votes_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = delivering_state(&pool);
    let (alice, posted, stand) =
        local_poll(&pool, &["tea", "coffee"], false, Visibility::Public).await;

    let self_vote = answer_create(&alice.ap_id, &posted.uri, "tea", "self", json!({}));
    deliver(&state, &self_vote, &alice).await;

    assert_eq!(
        stored_poll(&pool, stand.id).await.votes_count,
        0,
        "the author's ballot marks nothing"
    );
    assert_eq!(mark_rows(&pool, stand.id).await, 0);
    assert!(
        note_rows(&pool, alice.id).await.is_empty(),
        "and the author tells herself nothing"
    );
}

/// A voter who cannot see the question cannot mark its poll either: the
/// follower-only post is not visible to a remote account that follows
/// nobody, and the refused ballot marks nothing.
#[tokio::test]
async fn a_voter_who_cannot_see_the_question_marks_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = delivering_state(&pool);
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let (alice, posted, stand) =
        local_poll(&pool, &["tea", "coffee"], false, Visibility::Private).await;

    bob_answers(&state, &bob, &posted.uri, "tea", "blind").await;

    assert_eq!(
        stored_poll(&pool, stand.id).await.votes_count,
        0,
        "the invisible question took no mark"
    );
    assert_eq!(mark_rows(&pool, stand.id).await, 0);
    assert_eq!(
        rows_for_uri(&pool, "https://remote.example/objects/vote-blind").await,
        0,
        "and the claimed answer was not folded into a status"
    );
    assert!(
        note_rows(&pool, alice.id).await.is_empty(),
        "and nobody was told"
    );
}

/// A `Note` with a body of its own is not a ballot even when it carries a
/// `name` and answers a poll: the guard lets it go and the ordinary create
/// path folds it as a reply.
#[tokio::test]
async fn an_answer_with_a_body_falls_through_to_the_note_ingest() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = delivering_state(&pool);
    let (_alice, posted, stand) =
        local_poll(&pool, &["tea", "coffee"], false, Visibility::Public).await;
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;

    let activity = answer_create(
        BOB,
        &posted.uri,
        "tea",
        "posted",
        json!({"content": "<p>tea, definitely</p>"}),
    );
    deliver(&state, &activity, &bob).await;

    let child = status::get_by_uri(&pool, "https://remote.example/objects/vote-posted")
        .await
        .expect("the uri read")
        .expect("a note with a body is stored as a status");
    assert_eq!(
        child.in_reply_to_id,
        Some(posted.id),
        "and it replies to the question it answered"
    );
    assert!(
        !child.content.is_empty(),
        "carrying the body the guard refused to eat as a ballot"
    );
    assert_eq!(
        stored_poll(&pool, stand.id).await.votes_count,
        0,
        "while the poll it named took no mark"
    );
    assert_eq!(mark_rows(&pool, stand.id).await, 0);
}

/// A bodyless answer to a status that carries no poll is nobody's ballot:
/// the guard's question read comes back empty and the note ingest folds
/// the post like any other reply.
#[tokio::test]
async fn an_answer_to_a_poll_less_post_is_a_reply_not_a_ballot() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = delivering_state(&pool);
    let alice = common::create_local_user(&pool, "alice").await;
    let parent = common::create_status(
        &pool,
        &alice,
        "just a post, no poll on it",
        Visibility::Public,
    )
    .await;
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;

    let activity = answer_create(BOB, &parent.uri, "tea", "plain", json!({}));
    deliver(&state, &activity, &bob).await;

    let child = status::get_by_uri(&pool, "https://remote.example/objects/vote-plain")
        .await
        .expect("the uri read")
        .expect("an answer to a poll-less status folds through the note ingest");
    assert_eq!(
        child.in_reply_to_id,
        Some(parent.id),
        "as a reply to the status it answered"
    );
    assert!(
        poll::get_for_status(&pool, parent.id)
            .await
            .expect("the poll read")
            .is_none(),
        "and its parent still carries no poll"
    );
}
