//! `GET /api/v1/polls/{id}` over HTTP: the entity an anonymous reader and
//! a scoped user alike see on a public poll, the shared 404 the malformed,
//! unknown and invisible polls answer on both poll routes, and the read's
//! own `read:statuses` gate on a presented user token.

use axum::http::StatusCode;
use serde_json::json;
use uuid::Uuid;

use super::support::{WRITE, accepted, poll_rows, two_options, user_with_token};
use super::vote_support::{NOT_FOUND, ballot, mark_rows, marks, read_poll, vote_on};
use crate::common;

/// `GET /api/v1/polls/{id}` — a public poll answers the whole entity to
/// an anonymous reader, and the same unvoted view to a scoped user who
/// said nothing on it.
#[tokio::test]
async fn reading_a_public_poll_answers_the_entity_anonymously() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let (_bob, bob_token) = user_with_token(&pool, "bob", "read:statuses").await;
    let app = common::test_app(pool.clone());

    let poll_id = ballot(&app, &token, json!({"expires_in": 3600, "multiple": true})).await;

    let response = read_poll(&app, &poll_id, None).await;
    common::assert_answer(&response, StatusCode::OK, "an anonymous poll read");
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "the entity is JSON"
    );
    let value = response.json_value();
    assert_eq!(value["id"], poll_id, "the entity answers the poll's own id");
    assert_eq!(value["expired"], false, "an hour out is open");
    assert_eq!(value["multiple"], true, "the posted flag renders");
    assert_eq!(value["votes_count"], 0, "nothing is counted yet");
    assert_eq!(value["voters_count"], 0);
    assert_eq!(value["voted"], false, "the anonymous viewer voted nothing");
    assert_eq!(value["own_votes"], json!([]));
    assert_eq!(
        value["options"],
        json!([
            {"title": "tea", "votes_count": 0},
            {"title": "coffee", "votes_count": 0},
        ]),
        "the options in submission order at zero"
    );
    assert_eq!(value["emojis"], json!([]));
    assert!(
        value["expires_at"].is_string(),
        "the deadline renders as a timestamp"
    );

    let as_bob = read_poll(&app, &poll_id, Some(&bob_token)).await;
    common::assert_answer(&as_bob, StatusCode::OK, "a read on a scoped user token");
    assert_eq!(
        as_bob.json_value()["voted"],
        false,
        "bob said nothing on it either"
    );
}

/// Both routes answer one shared 404 for a poll id that is malformed,
/// that names no poll — even a live status id of a poll-less post — and
/// for a request with any body behind it.
#[tokio::test]
async fn an_unknown_or_malformed_poll_id_is_the_shared_404() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let (_bob, bob_token) = user_with_token(&pool, "bob", "read:statuses write:statuses").await;
    let app = common::test_app(pool.clone());

    let plain = accepted(&app, &token, json!({"status": "no question here"})).await;
    let pollless = plain["id"].as_str().expect("the create answers its id");

    for id in ["not-an-id", &Uuid::now_v7().to_string(), pollless] {
        let who = format!("the id `{id}`");
        let read = read_poll(&app, id, Some(&bob_token)).await;
        common::assert_answer(&read, StatusCode::NOT_FOUND, &who);
        common::assert_error(&read, NOT_FOUND, &who);
        let vote = marks(&app, &bob_token, id, json!([0])).await;
        common::assert_answer(&vote, StatusCode::NOT_FOUND, &who);
        common::assert_error(&vote, NOT_FOUND, &who);
    }

    let anonymous = read_poll(&app, "nonsense", None).await;
    common::assert_answer(
        &anonymous,
        StatusCode::NOT_FOUND,
        "an anonymous malformed id",
    );
    common::assert_error(&anonymous, NOT_FOUND, "an anonymous malformed id");
    assert_eq!(poll_rows(&pool).await, 0, "nothing was ever stored here");
}

/// The poll's status is the visibility gate: a `direct` question is the
/// one 404 to everyone but its author — anonymously, to a stranger's
/// token, and to that stranger's ballot alike — and the gate runs before
/// any body rule.
#[tokio::test]
async fn a_poll_hiding_behind_an_invisible_status_is_404() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", "read:statuses write:statuses").await;
    let (_bob, bob_token) = user_with_token(&pool, "bob", "read:statuses write:statuses").await;
    let app = common::test_app(pool.clone());

    let value = accepted(
        &app,
        &token,
        json!({"status": "quietly", "visibility": "direct", "poll": {"options": two_options()}}),
    )
    .await;
    let poll_id = value["poll"]["id"].as_str().expect("the entity").to_owned();

    let anonymous = read_poll(&app, &poll_id, None).await;
    common::assert_answer(
        &anonymous,
        StatusCode::NOT_FOUND,
        "an anonymous read of a direct poll",
    );
    common::assert_error(&anonymous, NOT_FOUND, "an anonymous read of a direct poll");
    let nosy = read_poll(&app, &poll_id, Some(&bob_token)).await;
    common::assert_answer(&nosy, StatusCode::NOT_FOUND, "a stranger's read");
    common::assert_error(&nosy, NOT_FOUND, "a stranger's read");
    let ballot = marks(&app, &bob_token, &poll_id, json!([0])).await;
    common::assert_answer(&ballot, StatusCode::NOT_FOUND, "a stranger's ballot");
    common::assert_error(&ballot, NOT_FOUND, "a stranger's ballot");
    let gate_first = vote_on(&app, &bob_token, &poll_id, json!({})).await;
    common::assert_answer(
        &gate_first,
        StatusCode::NOT_FOUND,
        "the invisible gate runs before the body rules",
    );
    common::assert_error(
        &gate_first,
        NOT_FOUND,
        "the invisible gate runs before the body rules",
    );

    let own = read_poll(&app, &poll_id, Some(&token)).await;
    common::assert_answer(&own, StatusCode::OK, "the author reads her own question");
    assert_eq!(
        mark_rows(&pool, &poll_id).await,
        0,
        "and nothing was marked"
    );
}

/// The read is optionally authenticated: a presented user token without
/// `read:statuses` is refused, an app-only token proceeds as the
/// anonymous read, a bogus token is the extractor's 401, and the `read`
/// root covers the sub-scope.
#[tokio::test]
async fn the_poll_read_gates_its_user_token_by_read_statuses() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, alice_token) = user_with_token(&pool, "alice", WRITE).await;
    let (_carol, wrong_scope) = user_with_token(&pool, "carol", "read:accounts").await;
    let (_dave, broad) = user_with_token(&pool, "dave", "read").await;
    let app_only = common::create_app_only_token(&pool, "write:statuses").await;
    let app = common::test_app(pool.clone());

    let poll_id = ballot(&app, &alice_token, json!({})).await;

    let refused = read_poll(&app, &poll_id, Some(&wrong_scope)).await;
    common::assert_answer(
        &refused,
        StatusCode::FORBIDDEN,
        "a user token without read:statuses",
    );
    common::assert_error(
        &refused,
        "Insufficient permissions: read:statuses.",
        "a user token without read:statuses",
    );

    let root = read_poll(&app, &poll_id, Some(&broad)).await;
    common::assert_answer(&root, StatusCode::OK, "the read root covers its sub-scope");

    let app_token = read_poll(&app, &poll_id, Some(&app_only)).await;
    common::assert_answer(
        &app_token,
        StatusCode::OK,
        "an app-only token reads anonymously",
    );
    assert_eq!(
        app_token.json_value()["voted"],
        false,
        "and is rendered as an unmarked viewer"
    );

    let bogus = read_poll(&app, &poll_id, Some("no-such-token")).await;
    common::assert_answer(
        &bogus,
        StatusCode::UNAUTHORIZED,
        "a token that does not resolve",
    );
    common::assert_error(&bogus, "The access token is invalid", "a bogus token");
}

/// The poll's author reads it as `voted` with no `own_votes`, while a
/// user who has not voted reads `voted: false` and, once they vote,
/// `true` with their marks.
#[tokio::test]
async fn the_author_reads_her_own_poll_as_voted_with_no_marks() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", "read:statuses write:statuses").await;
    let (_bob, bob_token) = user_with_token(&pool, "bob", "read:statuses write:statuses").await;
    let app = common::test_app(pool.clone());

    let poll_id = ballot(&app, &token, json!({})).await;

    let own = read_poll(&app, &poll_id, Some(&token)).await.json_value();
    assert_eq!(own["voted"], true, "the author reads her poll as voted");
    assert_eq!(own["own_votes"], json!([]), "though she marked nothing");

    let before = read_poll(&app, &poll_id, Some(&bob_token))
        .await
        .json_value();
    assert_eq!(before["voted"], false, "bob has not voted yet");
    assert_eq!(before["own_votes"], json!([]));

    let cast = marks(&app, &bob_token, &poll_id, json!([0])).await;
    common::assert_answer(&cast, StatusCode::OK, "bob's ballot");
    let after = read_poll(&app, &poll_id, Some(&bob_token))
        .await
        .json_value();
    assert_eq!(after["voted"], true, "bob has voted now");
    assert_eq!(after["own_votes"], json!([0]), "his mark");

    let own_again = read_poll(&app, &poll_id, Some(&token)).await.json_value();
    assert_eq!(own_again["voted"], true, "the author still reads voted");
    assert_eq!(own_again["own_votes"], json!([]), "and still no marks");
}
