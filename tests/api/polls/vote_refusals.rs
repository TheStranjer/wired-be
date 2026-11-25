//! The vote route's refusals over HTTP, in the ticket's order after the
//! deadline and the second ballot tested in [`super::vote`]: the author's
//! own ballot, the body that names no choices, the index no option holds
//! and the second distinct choice on a single-choice poll — each with the
//! nothing-written check. Then the gate the ballot runs behind, the
//! `hide_totals` veil the deadline lifts on this route, and the
//! `statuses_actions` bucket the route rides.

use axum::http::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;
use wired_be::models::{poll, status::Visibility};

use super::support::{NO_TOKEN, WRITE, user_with_token};
use super::vote_support::{
    AUTHOR_REFUSAL, BAD_INDEX, NO_CHOICES, NO_USER, POLLS, SECOND_BALLOT, SECOND_MARK, ballot,
    close_poll, mark_rows, marks, poll_row, read_poll, refused_vote, vote_on,
};
use crate::common;

/// The author's standing is refused before any body rule: their own
/// question takes no ballot from them, empty or full, while everybody
/// else votes freely.
#[tokio::test]
async fn the_author_cannot_vote_on_their_own_poll() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = user_with_token(&pool, "alice", "read:statuses write:statuses").await;
    let app = common::test_app(pool.clone());

    let poll_id = ballot(&app, &token, json!({})).await;
    refused_vote(
        &app,
        &token,
        &poll_id,
        json!({"choices": [0]}),
        (AUTHOR_REFUSAL, "the author's ballot"),
    )
    .await;
    refused_vote(
        &app,
        &token,
        &poll_id,
        json!({}),
        (
            AUTHOR_REFUSAL,
            "the author's refusal outranks the body rules",
        ),
    )
    .await;

    assert_eq!(mark_rows(&pool, &poll_id).await, 0, "nothing was written");
    let stand = poll_row(&pool, &poll_id).await;
    assert_eq!(stand.votes_count, 0);
    assert_eq!(stand.voters_count, 0);
    let own = read_poll(&app, &poll_id, Some(&token)).await.json_value();
    assert_eq!(
        own["voted"], true,
        "the author reads her own poll as voted, as Mastodon and Pleroma do"
    );
    assert_eq!(own["own_votes"], json!([]), "though she marked nothing");
    assert!(
        !poll::has_voted(&pool, stand.id, alice.id)
            .await
            .expect("the flag reads"),
        "no mark hides under her id"
    );
}

/// A vote body that names no choices — the key absent, JSON `null`, or
/// the empty array — is the missing-parameter refusal; a value that is
/// no integer at all is the extractor's own 422. None of them writes.
#[tokio::test]
async fn a_vote_body_without_choices_names_the_missing_parameter() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let (_bob, bob_token) = user_with_token(&pool, "bob", "write:statuses").await;
    let app = common::test_app(pool.clone());

    let poll_id = ballot(&app, &token, json!({})).await;
    for body in [json!({}), json!({"choices": []}), json!({"choices": null})] {
        refused_vote(
            &app,
            &bob_token,
            &poll_id,
            body,
            (NO_CHOICES, "a ballot with no choices"),
        )
        .await;
    }

    let junk = vote_on(&app, &bob_token, &poll_id, json!({"choices": ["abc"]})).await;
    common::assert_answer(
        &junk,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a choice that spells no integer",
    );
    assert!(
        junk.json_value()["error"]
            .as_str()
            .is_some_and(|text| text.starts_with("Invalid parameters")),
        "that refusal is the extractor's, naming the bad parameter"
    );
    assert_eq!(mark_rows(&pool, &poll_id).await, 0, "none of them wrote");
    let stand = poll_row(&pool, &poll_id).await;
    assert_eq!(stand.votes_count, 0, "or counted anything");
}

/// An index no option of the poll holds — past the last one, negative,
/// or mixed in with a good one — is the invalid-indices refusal, and it
/// outranks the single-choice count rule. Nothing is written.
#[tokio::test]
async fn a_choice_index_naming_no_option_is_invalid_indices() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let (_bob, bob_token) = user_with_token(&pool, "bob", "write:statuses").await;
    let app = common::test_app(pool.clone());

    let poll_id = ballot(&app, &token, json!({})).await;
    for choices in [
        json!([2]),
        json!([-1]),
        json!([0, 5]),
        json!([9, -2]),
        json!(["4"]),
    ] {
        refused_vote(
            &app,
            &bob_token,
            &poll_id,
            json!({"choices": choices}),
            (BAD_INDEX, "an out-of-range index"),
        )
        .await;
    }
    assert_eq!(mark_rows(&pool, &poll_id).await, 0, "and nothing landed");
    let stand = poll_row(&pool, &poll_id).await;
    assert_eq!(stand.votes_count, 0);
    assert_eq!(stand.voters_count, 0);
}

/// A ballot listing more than 1000 choices is refused as invalid indices
/// before any per-choice work runs, even when every entry is a valid
/// index. Nothing is written.
#[tokio::test]
async fn a_ballot_with_more_than_a_thousand_choices_is_invalid_indices() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let (_bob, bob_token) = user_with_token(&pool, "bob", "write:statuses").await;
    let app = common::test_app(pool.clone());

    let poll_id = ballot(&app, &token, json!({})).await;
    let oversized: Vec<i32> = (0..1001).collect();
    refused_vote(
        &app,
        &bob_token,
        &poll_id,
        json!({"choices": oversized}),
        (BAD_INDEX, "1001 choices"),
    )
    .await;
    let all_valid = vec![0; 1001];
    refused_vote(
        &app,
        &bob_token,
        &poll_id,
        json!({"choices": all_valid}),
        (BAD_INDEX, "1001 copies of a valid index"),
    )
    .await;
    assert_eq!(mark_rows(&pool, &poll_id).await, 0, "nothing landed");
    let stand = poll_row(&pool, &poll_id).await;
    assert_eq!(stand.votes_count, 0);
    assert_eq!(stand.voters_count, 0);
}

/// A second distinct index on a single-choice poll is the too-many
/// refusal — after deduplication, so a doubled index passes while two
/// real choices do not — and the range rule answers first when both
/// would fire. A multiple poll takes what the single one refused.
#[tokio::test]
async fn two_distinct_choices_on_a_single_choice_poll_are_too_many() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let (_bob, bob_token) = user_with_token(&pool, "bob", "write:statuses").await;
    let app = common::test_app(pool.clone());

    let poll_id = ballot(&app, &token, json!({})).await;
    refused_vote(
        &app,
        &bob_token,
        &poll_id,
        json!({"choices": [0, 1]}),
        (SECOND_MARK, "two marks on a single-choice poll"),
    )
    .await;
    refused_vote(
        &app,
        &bob_token,
        &poll_id,
        json!({"choices": ["1", 1, 0]}),
        (SECOND_MARK, "two distinct marks hidden in three values"),
    )
    .await;
    refused_vote(
        &app,
        &bob_token,
        &poll_id,
        json!({"choices": [4, 5]}),
        (BAD_INDEX, "the range rule answers before the count rule"),
    )
    .await;
    assert_eq!(mark_rows(&pool, &poll_id).await, 0, "none of them wrote");

    let many = ballot(
        &app,
        &token,
        json!({"options": ["a", "b"], "multiple": true}),
    )
    .await;
    let both = marks(&app, &bob_token, &many, json!([0, 1])).await;
    common::assert_answer(&both, StatusCode::OK, "the multiple poll admits both");
    assert_eq!(both.json_value()["votes_count"], 2);
}

/// The vote's gate is Pleroma's plug order: no token at all is the 401,
/// a user token without `write:statuses` the scope 403, and an app-only
/// one the user-required 403 — before any poll is looked up, so an
/// unknown id behind them is the 401/403, never a leaked 404 ordering.
#[tokio::test]
async fn voting_needs_a_write_statuses_user_token() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let (_carol, wrong_scope) = user_with_token(&pool, "carol", "read:statuses").await;
    let app_only = common::create_app_only_token(&pool, "write:statuses").await;
    let app = common::test_app(pool.clone());

    let poll_id = ballot(&app, &token, json!({})).await;
    let ghost = Uuid::now_v7().to_string();
    let body = json!({"choices": [0]});

    let anonymous = common::TestRequest::post(&format!("{POLLS}/{poll_id}/votes"))
        .json(&body)
        .send(&app)
        .await;
    common::assert_answer(
        &anonymous,
        StatusCode::UNAUTHORIZED,
        "a ballot with no token",
    );
    common::assert_error(&anonymous, NO_TOKEN, "a ballot with no token");
    let anonymous_ghost = common::TestRequest::post(&format!("{POLLS}/{ghost}/votes"))
        .json(&body)
        .send(&app)
        .await;
    common::assert_answer(
        &anonymous_ghost,
        StatusCode::UNAUTHORIZED,
        "the gate runs before any lookup",
    );

    let scopeless = marks(&app, &wrong_scope, &poll_id, json!([0])).await;
    common::assert_answer(
        &scopeless,
        StatusCode::FORBIDDEN,
        "a token without write:statuses",
    );
    common::assert_error(
        &scopeless,
        "Insufficient permissions: write:statuses.",
        "a token without write:statuses",
    );

    let unuser = marks(&app, &app_only, &poll_id, json!([0])).await;
    common::assert_answer(&unuser, StatusCode::FORBIDDEN, "an app-only token");
    common::assert_error(&unuser, NO_USER, "an app-only token");

    assert_eq!(
        mark_rows(&pool, &poll_id).await,
        0,
        "no gate let a mark through"
    );
    let stand = poll_row(&pool, &poll_id).await;
    assert_eq!(stand.voters_count, 0);
}

/// `hide_totals` rides the poll route like it does the status embed: the
/// per-option counts stay null — in the ballot's own answer included —
/// while the poll runs, the roll-ups always render, and the deadline
/// lifts the veil over everything at once.
#[tokio::test]
async fn hidden_totals_surface_when_the_poll_closes() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", "read:statuses write:statuses").await;
    let (_bob, bob_token) = user_with_token(&pool, "bob", "write:statuses").await;
    let app = common::test_app(pool.clone());

    let poll_id = ballot(&app, &token, json!({"hide_totals": true})).await;
    let value = marks(&app, &bob_token, &poll_id, json!([0]))
        .await
        .json_value();
    assert_eq!(
        value["options"],
        json!([
            {"title": "tea", "votes_count": null},
            {"title": "coffee", "votes_count": null},
        ]),
        "the counts stay hidden in the ballot's answer too"
    );
    assert_eq!(value["votes_count"], 1, "the roll-ups show");
    assert_eq!(value["voters_count"], 1);
    assert_eq!(value["voted"], true, "the voter still knows their own mark");
    assert_eq!(value["own_votes"], json!([0]));

    let open = read_poll(&app, &poll_id, None).await.json_value();
    assert_eq!(open["options"][0]["votes_count"], Value::Null);

    close_poll(&pool, &poll_id).await;
    let closed = read_poll(&app, &poll_id, None).await.json_value();
    assert_eq!(closed["expired"], true, "the read says so");
    assert_eq!(
        closed["options"],
        json!([
            {"title": "tea", "votes_count": 1},
            {"title": "coffee", "votes_count": 0},
        ]),
        "the stored counts show once the poll is over"
    );
    assert_eq!(closed["votes_count"], 1);
}

/// The route rides the `statuses_actions` bucket like the other status
/// actions: the third request of the window is refused 429
/// `{"error":"Throttled"}` before the handler runs, counted refusal
/// included, and writes nothing.
#[tokio::test]
async fn the_vote_route_rides_the_statuses_actions_bucket() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app_with_config(
        pool.clone(),
        common::test_config_with_rate_limits("statuses_actions=2:600"),
    );
    let alice = common::create_local_user(&pool, "alice").await;
    let (_bob, bob_token) = user_with_token(&pool, "bob", "write:statuses").await;

    // The poll goes in through the model: the create route spends the
    // same bucket, and this test budgets the vote's own two requests.
    let posted =
        common::create_status(&pool, &alice, "a bucketed ballot", Visibility::Public).await;
    let ballot = poll::PollParams {
        options: vec!["tea".to_owned(), "coffee".to_owned()],
        ..poll::PollParams::default()
    };
    let mut conn = pool.acquire().await.expect("slot connection");
    let stand = poll::create(&mut conn, posted.id, &ballot)
        .await
        .expect("the fixture poll creates");
    drop(conn);
    let poll_id = stand.id.to_string();

    let first = marks(&app, &bob_token, &poll_id, json!([0])).await;
    common::assert_answer(&first, StatusCode::OK, "a ballot inside the budget");
    assert_eq!(
        first.header("x-ratelimit-limit"),
        Some("2"),
        "the bucket's budget"
    );
    assert_eq!(
        first.header("x-ratelimit-remaining"),
        Some("1"),
        "the window counts down"
    );

    let refused = marks(&app, &bob_token, &poll_id, json!([1])).await;
    common::assert_answer(
        &refused,
        StatusCode::UNPROCESSABLE_ENTITY,
        "the counted second request is still a refused ballot",
    );
    common::assert_error(&refused, SECOND_BALLOT, "the counted second request");

    let throttled = marks(&app, &bob_token, &poll_id, json!([0])).await;
    common::assert_answer(&throttled, StatusCode::TOO_MANY_REQUESTS, "over the budget");
    common::assert_error(&throttled, "Throttled", "over the budget");

    let after = poll::get_by_id(&pool, stand.id)
        .await
        .expect("the poll reads")
        .expect("the poll stands");
    assert_eq!(
        after.votes_count, 1,
        "the window let exactly one mark through"
    );
    assert_eq!(after.voters_count, 1);
}
