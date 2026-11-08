//! Conversation (thread) mutes over HTTP (WBE-84): `POST /api/v1/statuses/{id}/mute`
//! and its `…/unmute` twin, plus the `muted` and `pleroma.thread_muted`
//! halves of the [`Status`] entity.
//!
//! The writes are Pleroma's `mute_conversation` and `unmute_conversation`:
//! a user token with `write:mutes`, the status resolved through the shared
//! visibility gate (a malformed, unknown, deleted or invisible id is the
//! one 404), and the answer the [`Status`] entity as the caller sees it —
//! both flags true after a mute, both false after a withdrawal, the row
//! re-read for the render. The mute is keyed to the status's
//! `conversation_id`, so a thread is muted as a whole: every one of its
//! statuses renders the same pair for its viewer, and the batched read in
//! the renderer answers a whole page from one query. Behind the gate stands
//! Mastodon's participation rule: only an account that authored a status in
//! the thread or is mentioned in one may mute it, and a stranger answers
//! the same 404 an invisible status does. Both writes are idempotent — a
//! repeat mute is one row and one 200, and withdrawing a mute that is not
//! there is the unchanged entity. Nothing federates, so these routes ride
//! no throttle bucket, like the bookmarks and the pins.
//!
//! [`Status`]: wired_be::entities::status::Status

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::TEST_BASE_URL,
    models::{
        status::{self, NewStatus, Status as StatusRow, Visibility},
        user::User,
    },
};

use crate::common;

/// The 404 body of the shared status gate — and of the participation rule,
/// which answers it too.
const NOT_FOUND: &str = "Record not found";
/// The 401 body `AuthUser` sends when no usable token was presented.
const NO_TOKEN: &str = "Invalid credentials";
/// The 403 body for an app-only token on a route that needs a user.
const USER_REQUIRED: &str = "This API requires an authenticated user";
/// The 401 body for a token that does not resolve.
const BAD_TOKEN: &str = "The access token is invalid";
/// The 403 wording of the thread-mute write gate.
const NO_WRITE: &str = "Insufficient permissions: write:mutes.";
/// An id that names no stored status.
const UNKNOWN_ID: &str = "01934567-89ab-7cde-f012-3456789abcde";
/// A spelling that is not id text at all.
const MALFORMED_ID: &str = "not-a-uuid";

/// A local user named `nickname` with a bearer token granting `scopes`.
async fn fixture(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, scopes).await;
    (created, token)
}

/// A local user named `nickname` who may mute and unmute threads and read a
/// status through a presented token (`read` covers the tree).
async fn muter(pool: &PgPool, nickname: &str) -> (User, String) {
    fixture(pool, nickname, "write:mutes read").await
}

/// A public post by `author`.
async fn post(pool: &PgPool, author: &User) -> StatusRow {
    common::create_status(pool, author, "a post", Visibility::Public).await
}

/// A public post by `author` that @-mentions `mentioned`.
async fn post_mentioning(pool: &PgPool, author: &User, mentioned: &User) -> StatusRow {
    status::insert(
        pool,
        TEST_BASE_URL,
        &NewStatus {
            mentioned_user_ids: vec![mentioned.id],
            ..NewStatus::local(author, "@someone look at this thread")
        },
    )
    .await
    .expect("the fixture mention inserts")
}

/// A reply `author` left under `parent`.
async fn reply(pool: &PgPool, author: &User, parent: &StatusRow) -> StatusRow {
    status::insert(
        pool,
        TEST_BASE_URL,
        &NewStatus {
            in_reply_to_id: Some(parent.id),
            in_reply_to_account_id: Some(parent.user_id),
            ..NewStatus::local(author, "an answer")
        },
    )
    .await
    .expect("the fixture reply inserts")
}

/// A reblog `reblogger` wrote of `original`, as its row — through the
/// reblog model, so it shares the original's conversation like every real
/// boost does.
async fn reblog(pool: &PgPool, reblogger: &User, original: &StatusRow) -> StatusRow {
    let (boost, created) =
        status::create_reblog(pool, TEST_BASE_URL, reblogger, original, Visibility::Public)
            .await
            .expect("the fixture reblog inserts");
    assert!(created, "the fixture pair had not reblogged yet");
    boost
}

/// A post by `author` with `visibility`.
async fn post_with(pool: &PgPool, author: &User, visibility: Visibility) -> StatusRow {
    common::create_status(pool, author, "hidden from strangers", visibility).await
}

/// `POST …/mute` for status `id`.
async fn mute_route(app: &Router, token: &str, id: &str) -> common::TestResponse {
    common::TestRequest::post(&format!("/api/v1/statuses/{id}/mute"))
        .bearer(token)
        .send(app)
        .await
}

/// `POST …/unmute` for status `id`.
async fn unmute_route(app: &Router, token: &str, id: &str) -> common::TestResponse {
    common::TestRequest::post(&format!("/api/v1/statuses/{id}/unmute"))
        .bearer(token)
        .send(app)
        .await
}

/// A `GET /api/v1/statuses/{id}` as `token`.
async fn show(app: &Router, token: &str, id: &str) -> common::TestResponse {
    common::TestRequest::get(&format!("/api/v1/statuses/{id}"))
        .bearer(token)
        .send(app)
        .await
}

/// A `GET /api/v1/statuses?id[]=a&id[]=b` batch read as `token`.
async fn show_batch(app: &Router, token: Option<&str>, ids: &[Uuid]) -> common::TestResponse {
    let joined = ids
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<String>>()
        .join("&id[]=");
    let request = common::TestRequest::get(&format!("/api/v1/statuses?id[]={joined}"));
    match token {
        Some(found) => request.bearer(found).send(app).await,
        None => request.send(app).await,
    }
}

/// A mute a test only needs to have happened, answered as JSON.
async fn muted(app: &Router, token: &str, id: &str) -> Value {
    let response = mute_route(app, token, id).await;
    common::assert_answer(&response, StatusCode::OK, "the mute stores");
    response.json_value()
}

/// A mute a test only needs to have happened: the row is stored, and the
/// body it answers is the test's business elsewhere.
async fn mute_ok(app: &Router, token: &str, id: &str) {
    let response = mute_route(app, token, id).await;
    common::assert_answer(&response, StatusCode::OK, "the mute stores");
}

/// The `(muted, pleroma.thread_muted)` pair a body answers with — the two
/// halves of one fact, which is why every test reads them together.
fn flags(body: &Value) -> (Value, Value) {
    (
        body["muted"].clone(),
        body["pleroma"]["thread_muted"].clone(),
    )
}

/// How many thread-mute rows exist in the table, and whose thread each is.
async fn muted_threads(pool: &PgPool, user_id: Uuid) -> Vec<Uuid> {
    sqlx::query_scalar::<_, Uuid>(
        "SELECT conversation_id FROM conversation_mutes WHERE user_id = $1",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
    .expect("the table reads")
}

/// Every mute writes the thread, flags the answer, and the read back agrees:
/// the author's own post mutes through its own id.
#[tokio::test]
async fn the_author_mutes_their_own_thread_and_both_flags_answer() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = muter(&pool, "alice").await;
    let status = post(&pool, &alice).await;

    let body = muted(&app, &token, &status.id.to_string()).await;
    assert_eq!(body["id"], status.id.to_string(), "the named row answers");
    assert_eq!(
        flags(&body),
        (serde_json::json!(true), serde_json::json!(true)),
        "the mute flags both halves of the answer"
    );
    assert_eq!(
        muted_threads(&pool, alice.id).await,
        vec![status.conversation_id],
        "the fact is keyed to the thread, not the named row"
    );

    let read = show(&app, &token, &status.id.to_string()).await;
    common::assert_answer(&read, StatusCode::OK, "the status reads back");
    assert_eq!(
        flags(&read.json_value()),
        (serde_json::json!(true), serde_json::json!(true)),
        "a later read carries the same pair, from the renderer's batch"
    );
}

/// Mastodon's participation rule admits the mentioned account: a caller the
/// thread called by name may mute it.
#[tokio::test]
async fn a_mentioned_account_mutes_the_thread_it_was_called_into() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, _alice_token) = muter(&pool, "alice").await;
    let (bob, bob_token) = muter(&pool, "bob").await;
    let root = post_mentioning(&pool, &alice, &bob).await;

    let body = muted(&app, &bob_token, &root.id.to_string()).await;
    assert_eq!(
        flags(&body),
        (serde_json::json!(true), serde_json::json!(true)),
        "the mentioned account mutes"
    );
    assert_eq!(
        muted_threads(&pool, bob.id).await,
        vec![root.conversation_id],
        "the same thread the root names"
    );
}

/// Participation is the whole conversation, not the named row: a replier
/// mutes through the root it answered, and every row of the thread renders
/// the pair to it afterwards.
#[tokio::test]
async fn a_replier_mutes_the_thread_through_another_status_of_it() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, _alice_token) = muter(&pool, "alice").await;
    let (bob, bob_token) = muter(&pool, "bob").await;
    let root = post(&pool, &alice).await;
    let answer = reply(&pool, &bob, &root).await;

    let body = muted(&app, &bob_token, &root.id.to_string()).await;
    assert_eq!(
        flags(&body),
        (serde_json::json!(true), serde_json::json!(true)),
        "bob answers the thread through the root he replied to"
    );

    let read = show(&app, &bob_token, &answer.id.to_string()).await;
    assert_eq!(
        flags(&read.json_value()),
        (serde_json::json!(true), serde_json::json!(true)),
        "his own reply renders the pair too — one conversation, one fact"
    );
}

/// The other half of the participation rule: an account that authored
/// nothing in the thread and was mentioned in none of it may not mute it —
/// and the refusal is the shared 404, not a confession about the thread.
#[tokio::test]
async fn a_stranger_to_the_thread_cannot_mute_it() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, _alice_token) = muter(&pool, "alice").await;
    let (carol, carol_token) = muter(&pool, "carol").await;
    let status = post(&pool, &alice).await;

    let response = mute_route(&app, &carol_token, &status.id.to_string()).await;
    common::assert_answer(&response, StatusCode::NOT_FOUND, "a stranger mutes");
    common::assert_error(&response, NOT_FOUND, "the participation refusal");
    assert!(
        muted_threads(&pool, carol.id).await.is_empty(),
        "the refusal wrote nothing"
    );
}

/// Both routes idempotent: a repeat mute is one row and the same answer,
/// and withdrawing a mute — twice — is always the unchanged entity with 200.
#[tokio::test]
async fn the_writes_are_idempotent_on_the_account_and_thread() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = muter(&pool, "alice").await;
    let status = post(&pool, &alice).await;
    let id = status.id.to_string();

    let first = muted(&app, &token, &id).await;
    let second = muted(&app, &token, &id).await;
    assert_eq!(
        flags(&second),
        (serde_json::json!(true), serde_json::json!(true)),
        "the repeat says the same thing"
    );
    assert_eq!(
        second.get("id"),
        first.get("id"),
        "and answers the same entity"
    );
    assert_eq!(
        muted_threads(&pool, alice.id).await,
        vec![status.conversation_id],
        "for one row"
    );

    let opened = unmute_route(&app, &token, &id).await;
    common::assert_answer(&opened, StatusCode::OK, "the unmute stores");
    assert_eq!(
        flags(&opened.json_value()),
        (serde_json::json!(false), serde_json::json!(false)),
        "both flags go back to false"
    );
    assert!(
        muted_threads(&pool, alice.id).await.is_empty(),
        "the row is gone"
    );

    let again = unmute_route(&app, &token, &id).await;
    common::assert_answer(&again, StatusCode::OK, "a repeat unmute is peace");
    assert_eq!(
        flags(&again.json_value()),
        (serde_json::json!(false), serde_json::json!(false)),
        "withdrawing a mute that is not there changes nothing"
    );
}

/// A stranger may still withdraw: the unmute route asks only the shared
/// visibility gate, and there is nothing of anybody else's for it to delete.
#[tokio::test]
async fn an_outsider_unmute_changes_nobody_elses_mute() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, alice_token) = muter(&pool, "alice").await;
    let (_bob, bob_token) = muter(&pool, "bob").await;
    let status = post(&pool, &alice).await;
    mute_ok(&app, &alice_token, &status.id.to_string()).await;

    let response = unmute_route(&app, &bob_token, &status.id.to_string()).await;
    common::assert_answer(&response, StatusCode::OK, "bob unmutes a free thread");
    assert_eq!(
        flags(&response.json_value()),
        (serde_json::json!(false), serde_json::json!(false)),
        "his own view says open, as it always did"
    );
    assert_eq!(
        muted_threads(&pool, alice.id).await,
        vec![status.conversation_id],
        "and alice's mute stands — the write is per account"
    );
}

/// The shared gate in front of both writes: what the caller may not see is
/// the one 404, and the participation rule never gets to say anything else.
#[tokio::test]
async fn an_invisible_status_mutes_to_the_shared_404() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, _alice_token) = muter(&pool, "alice").await;
    let (_carol, carol_token) = muter(&pool, "carol").await;
    let hidden = post_with(&pool, &alice, Visibility::Private).await;
    let id = hidden.id.to_string();

    let response = mute_route(&app, &carol_token, &id).await;
    common::assert_answer(&response, StatusCode::NOT_FOUND, "muting");
    common::assert_error(&response, NOT_FOUND, "the invisible refusal");

    let withdrawn = unmute_route(&app, &carol_token, &id).await;
    common::assert_answer(&withdrawn, StatusCode::NOT_FOUND, "unmuting");
    common::assert_error(&withdrawn, NOT_FOUND, "the invisible refusal");
}

/// The gate's other arms on both routes: an unknown id and a malformed one
/// are the same 404 as everything else the route will not discuss.
#[tokio::test]
async fn unknown_and_malformed_ids_are_the_shared_404() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = muter(&pool, "alice").await;

    for path in [UNKNOWN_ID, MALFORMED_ID] {
        for response in [
            mute_route(&app, &token, path).await,
            unmute_route(&app, &token, path).await,
        ] {
            common::assert_answer(&response, StatusCode::NOT_FOUND, "the gate");
            common::assert_error(&response, NOT_FOUND, "the refusal wording");
        }
    }
}

/// A deleted status mutes to the same 404: the gate drops tombstones before
/// the participation rule is ever asked.
#[tokio::test]
async fn a_deleted_status_mutes_to_the_shared_404() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = muter(&pool, "alice").await;
    let status = post(&pool, &alice).await;
    let _tombstone = status::soft_delete(&pool, status.id)
        .await
        .expect("the fixture delete runs");

    let response = mute_route(&app, &token, &status.id.to_string()).await;
    common::assert_answer(&response, StatusCode::NOT_FOUND, "a deleted status");
    common::assert_error(&response, NOT_FOUND, "the refusal wording");
}

/// The auth and scope gates of both routes: no token and a bad token are the
/// extractors' 401s, an app-only token is the user-required 403, and a user
/// token without `write:mutes` is the scope 403.
#[tokio::test]
async fn both_routes_gate_on_a_user_token_with_write_mutes() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, _token) = muter(&pool, "alice").await;
    let status = post(&pool, &alice).await;
    let reader_token = fixture(&pool, "reader", "read").await.1;
    let app_only = common::create_app_only_token(&pool, "write:mutes").await;

    for verb in ["mute", "unmute"] {
        let path = format!("/api/v1/statuses/{}/{verb}", status.id);

        let unauthenticated = common::TestRequest::post(&path).send(&app).await;
        common::assert_answer(&unauthenticated, StatusCode::UNAUTHORIZED, verb);
        common::assert_error(&unauthenticated, NO_TOKEN, "the missing token");

        let bogus = common::TestRequest::post(&path)
            .bearer("tok3n-that-exists-not")
            .send(&app)
            .await;
        common::assert_answer(&bogus, StatusCode::UNAUTHORIZED, verb);
        common::assert_error(&bogus, BAD_TOKEN, "the unresolvable token");

        let client = common::TestRequest::post(&path)
            .bearer(&app_only)
            .send(&app)
            .await;
        common::assert_answer(&client, StatusCode::FORBIDDEN, verb);
        common::assert_error(&client, USER_REQUIRED, "the app-only token");

        let scopeless = common::TestRequest::post(&path)
            .bearer(&reader_token)
            .send(&app)
            .await;
        common::assert_answer(&scopeless, StatusCode::FORBIDDEN, verb);
        common::assert_error(&scopeless, NO_WRITE, "the token lacking write:mutes");
    }
}

/// The renderer's batched read over HTTP: one page answers the pair per
/// row's own conversation — muted thread flagged throughout, the open one
/// not, and a stranger's (or an anonymous) read of the same rows is the
/// same statuses unflagged.
#[tokio::test]
async fn one_batched_read_flags_the_page_thread_by_thread() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, alice_token) = muter(&pool, "alice").await;
    let (bob, bob_token) = muter(&pool, "bob").await;
    let root = post(&pool, &alice).await;
    let answer = reply(&pool, &bob, &root).await;
    let other = post(&pool, &alice).await;
    mute_ok(&app, &alice_token, &root.id.to_string()).await;

    let for_alice = show_batch(&app, Some(&alice_token), &[root.id, answer.id, other.id]).await;
    common::assert_answer(&for_alice, StatusCode::OK, "the batch reads");
    let rows = for_alice.json_value();
    let entries = rows.as_array().expect("the batch answers an array");
    assert_eq!(entries.len(), 3, "three statuses asked, three rendered");
    for row in [&entries[0], &entries[1]] {
        assert_eq!(
            flags(row),
            (serde_json::json!(true), serde_json::json!(true)),
            "both statuses of the muted thread flag the pair"
        );
    }
    assert_eq!(
        flags(&entries[2]),
        (serde_json::json!(false), serde_json::json!(false)),
        "the open post does not"
    );

    let for_bob = show_batch(&app, Some(&bob_token), &[root.id, answer.id]).await;
    assert!(
        for_bob
            .json_value()
            .as_array()
            .expect("the batch answers an array")
            .iter()
            .all(|row| row["muted"] == serde_json::json!(false)
                && row["pleroma"]["thread_muted"] == serde_json::json!(false)),
        "alice's mute is nobody else's flag: {}",
        for_bob.text()
    );

    let anonymous = show_batch(&app, None, &[root.id, answer.id]).await;
    assert!(
        anonymous
            .json_value()
            .as_array()
            .expect("the batch answers an array")
            .iter()
            .all(|row| row["muted"] == serde_json::json!(false)),
        "an anonymous page is nobody's mute: {}",
        anonymous.text()
    );
}

/// A reblog shares its original's conversation, so muting through the
/// reblog row's id mutes the thread the boost stands in — and the answer
/// stays the named row's entity.
#[tokio::test]
async fn a_reblogs_id_mutes_the_thread_it_boosts() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, _alice_token) = muter(&pool, "alice").await;
    let (bob, bob_token) = muter(&pool, "bob").await;
    let root = post(&pool, &alice).await;
    let boost = reblog(&pool, &bob, &root).await;

    let body = muted(&app, &bob_token, &boost.id.to_string()).await;
    assert_eq!(
        body["id"],
        boost.id.to_string(),
        "the named reblog row answers"
    );
    assert_eq!(
        flags(&body),
        (serde_json::json!(true), serde_json::json!(true)),
        "and carries the pair"
    );
    assert_eq!(
        muted_threads(&pool, bob.id).await,
        vec![root.conversation_id],
        "the mute names the thread, which the boost shares with its original"
    );

    let read = show(&app, &bob_token, &root.id.to_string()).await;
    assert_eq!(
        flags(&read.json_value()),
        (serde_json::json!(true), serde_json::json!(true)),
        "the original it boosts renders flagged to bob too"
    );
}
