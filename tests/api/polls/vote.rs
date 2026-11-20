//! `POST /api/v1/polls/{id}/votes` over HTTP: the passing single- and
//! multiple-choice ballots as one transaction marking the chosen options
//! and moving every count the entity reports, the form spelling of the
//! body, the duplicates a ballot ignores, and the first two refusals in
//! the ticket's order — the finished poll and the voter's second ballot.

use axum::http::StatusCode;
use serde_json::json;
use wired_be::models::poll;

use super::support::{WRITE, stored_options, user_with_token};
use super::vote_support::{
    FINISHED, POLLS, SECOND_BALLOT, ballot, close_poll, mark_rows, marks, poll_row, read_poll,
    refused_vote,
};
use crate::common;

/// A passing single-choice ballot is one transaction over everything the
/// entity reports: the mark row, the option's count, the poll's roll-ups
/// — answered at once and re-read identically, `voted` true and the cast
/// position in `own_votes` for its voter only.
#[tokio::test]
async fn a_single_choice_ballot_marks_the_option_and_moves_the_counts() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let (bob, bob_token) = user_with_token(&pool, "bob", "read:statuses write:statuses").await;
    let app = common::test_app(pool.clone());

    let poll_id = ballot(&app, &token, json!({})).await;
    let response = marks(&app, &bob_token, &poll_id, json!([1])).await;
    common::assert_answer(&response, StatusCode::OK, "a first ballot");
    let value = response.json_value();
    assert_eq!(value["id"], poll_id, "the answer is the poll's entity");
    assert_eq!(value["voted"], true, "the voter sees their ballot at once");
    assert_eq!(value["own_votes"], json!([1]));
    assert_eq!(value["votes_count"], 1, "one mark, one vote");
    assert_eq!(value["voters_count"], 1, "however few marks, one voter");
    assert_eq!(
        value["options"],
        json!([
            {"title": "tea", "votes_count": 0},
            {"title": "coffee", "votes_count": 1},
        ]),
        "only the marked option gained"
    );

    let stand = poll_row(&pool, &poll_id).await;
    assert_eq!(stand.votes_count, 1, "the roll-up columns moved");
    assert_eq!(stand.voters_count, 1);
    assert_eq!(
        stored_options(&pool, stand.id).await,
        vec![(0, "tea".to_owned(), 0), (1, "coffee".to_owned(), 1)],
        "the option rows carry the counts the entity answered"
    );
    assert_eq!(mark_rows(&pool, &poll_id).await, 1, "exactly one mark row");
    assert_eq!(
        poll::voted_option_positions(&pool, stand.id, bob.id)
            .await
            .expect("the marks read"),
        vec![1],
        "the mark sits under its voter at the chosen position"
    );

    let again = read_poll(&app, &poll_id, Some(&bob_token)).await;
    assert_eq!(
        again.json_value()["voted"],
        true,
        "the re-fetch shows the ballot"
    );
    assert_eq!(again.json_value()["own_votes"], json!([1]));
    let as_stranger = read_poll(&app, &poll_id, None).await;
    assert_eq!(
        as_stranger.json_value()["voted"],
        false,
        "and it is nobody else's vote to show"
    );
    assert_eq!(as_stranger.json_value()["own_votes"], json!([]));
}

/// A multiple-choice ballot marks every chosen option: the vote total
/// counts the marks, the voter total counts the voter — and a second
/// voter's single mark joins both sums without touching the first's.
#[tokio::test]
async fn a_multiple_choice_ballot_marks_every_chosen_option() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let (bob, bob_token) = user_with_token(&pool, "bob", "read:statuses write:statuses").await;
    let (carol, carol_token) =
        user_with_token(&pool, "carol", "read:statuses write:statuses").await;
    let app = common::test_app(pool.clone());

    let poll_id = ballot(
        &app,
        &token,
        json!({"options": ["a", "b", "c"], "multiple": true}),
    )
    .await;
    let value = marks(&app, &bob_token, &poll_id, json!([0, 2]))
        .await
        .json_value();
    assert_eq!(value["votes_count"], 2, "one vote per mark");
    assert_eq!(value["voters_count"], 1, "one ballot, one voter");
    assert_eq!(value["own_votes"], json!([0, 2]));
    assert_eq!(
        value["options"],
        json!([
            {"title": "a", "votes_count": 1},
            {"title": "b", "votes_count": 0},
            {"title": "c", "votes_count": 1},
        ]),
        "every chosen option gained"
    );

    let second = marks(&app, &carol_token, &poll_id, json!([2]))
        .await
        .json_value();
    assert_eq!(second["votes_count"], 3, "the ballots sum");
    assert_eq!(second["voters_count"], 2, "each voter counts once");
    assert_eq!(
        second["own_votes"],
        json!([2]),
        "and each sees only their own marks"
    );
    let stand = poll_row(&pool, &poll_id).await;
    assert_eq!(stand.votes_count, 3);
    assert_eq!(stand.voters_count, 2);
    assert_eq!(
        stored_options(&pool, stand.id).await,
        vec![
            (0, "a".to_owned(), 1),
            (1, "b".to_owned(), 0),
            (2, "c".to_owned(), 2),
        ],
        "the stored option rows agree with both answers"
    );
    assert_eq!(mark_rows(&pool, &poll_id).await, 3, "two marks plus one");
    assert_eq!(
        poll::voted_option_positions(&pool, stand.id, bob.id)
            .await
            .expect("the marks read"),
        vec![0, 2],
        "bob's ballot is untouched by carol's"
    );
    assert_eq!(
        poll::voted_option_positions(&pool, stand.id, carol.id)
            .await
            .expect("the marks read"),
        vec![2]
    );
}

/// The form spelling of the body marks the same ballot: the repeated
/// `choices[]` keys arrive as strings and parse like the integers they
/// spell, and a lone `choices` is wrapped into the one-element list
/// Pleroma's `List.wrap` accepts.
#[tokio::test]
async fn a_form_vote_body_with_string_indexes_marks_the_same() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let (_bob, bob_token) = user_with_token(&pool, "bob", "write:statuses").await;
    let (_carol, carol_token) = user_with_token(&pool, "carol", "write:statuses").await;
    let app = common::test_app(pool.clone());

    let poll_id = ballot(
        &app,
        &token,
        json!({"options": ["a", "b", "c"], "multiple": true}),
    )
    .await;
    let bracketed = common::TestRequest::post(&format!("{POLLS}/{poll_id}/votes"))
        .bearer(&bob_token)
        .form(&[("choices[]", "0"), ("choices[]", "2")])
        .send(&app)
        .await;
    common::assert_answer(&bracketed, StatusCode::OK, "the repeated bracketed keys");
    assert_eq!(bracketed.json_value()["own_votes"], json!([0, 2]));

    let lone = common::TestRequest::post(&format!("{POLLS}/{poll_id}/votes"))
        .bearer(&carol_token)
        .form(&[("choices", "1")])
        .send(&app)
        .await;
    common::assert_answer(&lone, StatusCode::OK, "the lone value");
    assert_eq!(lone.json_value()["own_votes"], json!([1]));
    assert_eq!(
        lone.json_value()["votes_count"],
        3,
        "the two ballots summed"
    );
    assert_eq!(lone.json_value()["voters_count"], 2);
}

/// Duplicates in the body are ignored, not refused: a repeated index —
/// in either spelling, mixed across numbers and strings — marks its
/// option exactly once and passes a single-choice poll.
#[tokio::test]
async fn a_duplicated_choice_marks_the_option_once() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let (_bob, bob_token) = user_with_token(&pool, "bob", "write:statuses").await;
    let (_carol, carol_token) = user_with_token(&pool, "carol", "write:statuses").await;
    let app = common::test_app(pool.clone());

    let poll_id = ballot(&app, &token, json!({})).await;
    let value = marks(&app, &bob_token, &poll_id, json!([1, 1, 1]))
        .await
        .json_value();
    assert_eq!(
        value["votes_count"], 1,
        "three times the same index is one mark"
    );
    assert_eq!(value["voters_count"], 1);
    assert_eq!(value["own_votes"], json!([1]));

    let mixed = marks(&app, &carol_token, &poll_id, json!(["0", 0]))
        .await
        .json_value();
    assert_eq!(
        mixed["votes_count"], 2,
        "the second voter's single mark joins the sum"
    );
    assert_eq!(mixed["voters_count"], 2);
    assert_eq!(mixed["own_votes"], json!([0]));
    assert_eq!(
        mixed["options"],
        json!([
            {"title": "tea", "votes_count": 1},
            {"title": "coffee", "votes_count": 1},
        ]),
        "one row each, none doubled"
    );
    assert_eq!(
        mark_rows(&pool, &poll_id).await,
        2,
        "one mark row per voter"
    );
}

/// A poll past its deadline is refused the ballot with "Poll's finished"
/// before any body rule runs — an empty body gets the same answer —
/// writes nothing, and renders as expired on the read.
#[tokio::test]
async fn voting_on_an_expired_poll_is_refused_as_finished() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", "read:statuses write:statuses").await;
    let (_bob, bob_token) = user_with_token(&pool, "bob", "write:statuses").await;
    let app = common::test_app(pool.clone());

    let poll_id = ballot(&app, &token, json!({"expires_in": 3600})).await;
    close_poll(&pool, &poll_id).await;

    refused_vote(
        &app,
        &bob_token,
        &poll_id,
        json!({"choices": [0]}),
        (FINISHED, "a ballot on a closed poll"),
    )
    .await;
    refused_vote(
        &app,
        &bob_token,
        &poll_id,
        json!({}),
        (FINISHED, "the deadline outranks the body rules"),
    )
    .await;

    let value = read_poll(&app, &poll_id, Some(&token)).await.json_value();
    assert_eq!(value["expired"], true, "the read renders it closed");
    assert_eq!(value["votes_count"], 0, "nothing was written");
    assert_eq!(value["voters_count"], 0);
    assert_eq!(mark_rows(&pool, &poll_id).await, 0);
}

/// The second ballot of one voter is refused with the already-voted
/// wording before the body is even read, and changes nothing; every
/// other voter keeps their own first and only ballot.
#[tokio::test]
async fn a_second_ballot_from_the_same_voter_is_refused() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", "read:statuses write:statuses").await;
    let (_bob, bob_token) = user_with_token(&pool, "bob", "write:statuses").await;
    let (_carol, carol_token) = user_with_token(&pool, "carol", "write:statuses").await;
    let app = common::test_app(pool.clone());

    let poll_id = ballot(&app, &token, json!({})).await;
    let first = marks(&app, &bob_token, &poll_id, json!([0])).await;
    common::assert_answer(&first, StatusCode::OK, "bob's first ballot");

    refused_vote(
        &app,
        &bob_token,
        &poll_id,
        json!({"choices": [1]}),
        (SECOND_BALLOT, "bob's second ballot"),
    )
    .await;
    refused_vote(
        &app,
        &bob_token,
        &poll_id,
        json!({"choices": [0]}),
        (SECOND_BALLOT, "even the same mark cast again"),
    )
    .await;
    refused_vote(
        &app,
        &bob_token,
        &poll_id,
        json!({}),
        (
            SECOND_BALLOT,
            "the already-voted refusal outranks the body rules",
        ),
    )
    .await;

    let stand = poll_row(&pool, &poll_id).await;
    assert_eq!(stand.votes_count, 1, "the refusals counted nothing");
    assert_eq!(stand.voters_count, 1);
    assert_eq!(
        stored_options(&pool, stand.id).await,
        vec![(0, "tea".to_owned(), 1), (1, "coffee".to_owned(), 0)],
        "and moved no option"
    );
    assert_eq!(mark_rows(&pool, &poll_id).await, 1, "one mark row stands");

    let as_carol = marks(&app, &carol_token, &poll_id, json!([1])).await;
    common::assert_answer(
        &as_carol,
        StatusCode::OK,
        "carol's own first ballot is untouched by bob's refusal",
    );
    assert_eq!(as_carol.json_value()["voted"], true);
    assert_eq!(as_carol.json_value()["own_votes"], json!([1]));
}
