//! Poll parameters on `POST /api/v1/statuses` over HTTP: the validation
//! and the storage half of the polls tickets (WBE-93).
//!
//! A create body may carry a `poll` — JSON's `"poll": {"options": [...],
//! "expires_in": …, "multiple": …, "hide_totals": …}` or the form's nested
//! `poll[options][]` / `poll[expires_in]` / `poll[multiple]` /
//! `poll[hide_totals]` keys, which the extractor nests into the same
//! shape. `wired_be::services::create_status::check_body` runs Pleroma's
//! `CommonAPI` rules over it: at least two options, at most
//! `MAX_POLL_OPTIONS` (20 by default), no empty option and none past
//! `MAX_POLL_OPTION_CHARS` (200 by default), `poll[expires_in]` inside the
//! `MIN_POLL_EXPIRATION` to `MAX_POLL_EXPIRATION` window (300 seconds to
//! one year by default), and no non-empty `media_ids` beside a poll. Each
//! refusal is a 422 with the message the service pins; duplicates and a
//! `direct` audience are allowed.
//!
//! An accepted poll is stored: the create writes the `polls` row and its
//! `poll_options` rows on the status's own transaction, and the fresh
//! entity answers the whole `Poll` — the stored row's id, the options in
//! submission order at zero votes, the deadline the posted `expires_in`
//! counts out from now, and the posted flags. `hide_totals` withholds the
//! per-option counts (JSON `null`) while the poll is open; the roll-up
//! columns always render. The rest of the storage contract tested here:
//! a `preview` with a poll checks and answers without inserting, a poll
//! create replayed under one `Idempotency-Key` leaves exactly one status
//! and one poll, and a body with no poll stores no polls row at all.
//! `media_ids` keeps its validate-only meaning: beside a poll it is the
//! 422 `Polls cannot have attachments`, on its own the create ignores it
//! (the attachments ticket gives it a storage meaning).

#![cfg(test)]

use axum::{Router, http::StatusCode};
use chrono::{TimeDelta, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::models::{
    poll::{self, Poll},
    status::{self, Visibility},
    user::User,
};

use crate::common;

/// The route under test.
const STATUSES: &str = "/api/v1/statuses";
/// The scope the create endpoint asks a user token for.
const WRITE: &str = "write:statuses";
/// The 401 body `AuthUser` renders when the request carries no token at all.
const NO_TOKEN: &str = "Invalid credentials";
/// 422 body for a poll with fewer than two options.
const TOO_FEW: &str = "Poll must contain at least 2 options";
/// 422 body for a poll past `MAX_POLL_OPTIONS` (the stock 20).
const TOO_MANY: &str = "Poll can't contain more than 20 options";
/// 422 body for an empty option or one past `MAX_POLL_OPTION_CHARS`.
const BROKEN_OPTION: &str = "Poll options cannot be longer than 200 characters";
/// 422 body for `poll[expires_in]` past `MAX_POLL_EXPIRATION`.
const TOO_FAR: &str = "Expiration date is too far in the future";
/// 422 body for `poll[expires_in]` under `MIN_POLL_EXPIRATION`.
const TOO_SOON: &str = "Expiration date is too soon";
/// 422 body for a poll sent beside non-empty `media_ids`.
const WITH_MEDIA: &str = "Polls cannot have attachments";
/// 422 body for a textless post with no poll (or anything else) to carry it.
const EMPTY_STATUS: &str = "Cannot post an empty status without attachments";

/// A local user named `nickname` plus a bearer token granting `scopes`.
async fn user_with_token(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, scopes).await;
    (created, token)
}

/// `POST /api/v1/statuses` with the JSON `body` as `token`.
async fn posted(app: &Router, token: &str, body: Value) -> common::TestResponse {
    common::TestRequest::post(STATUSES)
        .bearer(token)
        .json(&body)
        .send(app)
        .await
}

/// A creation asserted 200 and answered as JSON.
async fn accepted(app: &Router, token: &str, body: Value) -> Value {
    let response = posted(app, token, body).await;
    common::assert_answer(&response, StatusCode::OK, "a body with a valid poll");
    response.json_value()
}

/// A refusal asserted 422 with exactly `message` as its `{"error": …}`.
async fn refused(app: &Router, token: &str, body: Value, message: &str, who: &str) {
    let response = posted(app, token, body).await;
    common::assert_answer(&response, StatusCode::UNPROCESSABLE_ENTITY, who);
    common::assert_error(&response, message, who);
}

/// The stored row behind a creation's answer.
async fn created_row(pool: &PgPool, value: &Value) -> status::Status {
    let text = value["id"].as_str().expect("the entity carries its id");
    let id = Uuid::parse_str(text).expect("the entity id is an id");
    status::get_by_id(pool, id)
        .await
        .unwrap()
        .expect("the created status is stored live")
}

/// How many statuses the database holds: what a refusal never adds to.
async fn status_rows(pool: &PgPool) -> i64 {
    sqlx::query_as::<_, (i64,)>("SELECT count(*) FROM statuses")
        .fetch_one(pool)
        .await
        .expect("the status count reads")
        .0
}

/// How many `polls` rows the database holds: one per stored question,
/// none for a plain create, a refused body or a preview.
async fn poll_rows(pool: &PgPool) -> i64 {
    sqlx::query_as::<_, (i64,)>("SELECT count(*) FROM polls")
        .fetch_one(pool)
        .await
        .expect("the poll count reads")
        .0
}

/// How many `poll_options` rows the database holds, whatever poll they
/// belong to.
async fn option_rows(pool: &PgPool) -> i64 {
    sqlx::query_as::<_, (i64,)>("SELECT count(*) FROM poll_options")
        .fetch_one(pool)
        .await
        .expect("the option count reads")
        .0
}

/// The stored `polls` row behind a creation's answer, required to exist —
/// the row the answer's `poll` entity renders from.
async fn stored_poll(pool: &PgPool, value: &Value) -> Poll {
    let row = created_row(pool, value).await;
    poll::get_for_status(pool, row.id)
        .await
        .expect("the poll reads")
        .expect("the create stored the poll beside its status")
}

/// The stored options of `poll_id`, as `(position, title, votes_count)`
/// in the order the model reads them back.
async fn stored_options(pool: &PgPool, poll_id: Uuid) -> Vec<(i32, String, i32)> {
    poll::options(pool, poll_id)
        .await
        .expect("the options read")
        .into_iter()
        .map(|option| (option.position, option.title, option.votes_count))
        .collect()
}

/// Assert the poll row's deadline sits within a few seconds of `secs`
/// after `posted_at` — the instant counted at check time, immediately
/// before the row was written.
fn assert_deadline(stand: &Poll, posted_at: chrono::DateTime<Utc>, secs: i64) {
    let due = stand
        .expires_at
        .unwrap_or_else(|| panic!("the create stored the {secs}-second deadline"));
    let span = due - posted_at;
    assert!(
        (TimeDelta::seconds(secs) - span).num_seconds().abs() <= 5,
        "the poll closes about {secs} seconds out, not {span}"
    );
}

/// The minimum valid poll's options, as JSON.
fn two_options() -> Value {
    json!(["yes", "no"])
}

/// `n` distinct numbered option texts, as JSON.
fn numbered_options(n: usize) -> Value {
    json!(
        (0..n)
            .map(|index| format!("option {index}"))
            .collect::<Vec<String>>()
    )
}

/// The whole accepted JSON shape — the exact body of the storage ticket —
/// answers 200 with the full `Poll` entity, and the database holds the
/// `polls` row and its options behind it: the answer's `poll.id` is the
/// stored row's id, the options came through in submission order with
/// zero votes, the stored deadline is `now + expires_in`, and the posted
/// `multiple` flag is the row's. The status's own expiry is none of the
/// poll's business.
#[tokio::test]
async fn a_json_poll_create_stores_its_poll_and_renders_the_entity() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());

    let before = Utc::now();
    let value = accepted(
        &app,
        &token,
        json!({
            "status": "ballot",
            "poll": {
                "options": ["a", "b"],
                "expires_in": 3600,
                "multiple": true,
                "hide_totals": false,
            },
        }),
    )
    .await;
    let stand = stored_poll(&pool, &value).await;

    assert_eq!(
        value["poll"]["id"],
        stand.id.to_string(),
        "the entity answers the stored poll's id"
    );
    assert_eq!(
        value["poll"]["options"],
        json!([
            {"title": "a", "votes_count": 0},
            {"title": "b", "votes_count": 0},
        ]),
        "options render in submission order with their fresh zero counts"
    );
    assert_eq!(value["poll"]["multiple"], true, "the posted flag renders");
    assert_eq!(value["poll"]["expired"], false, "an hour out is open");
    assert_eq!(
        value["poll"]["votes_count"], 0,
        "the roll-up columns render, at zero"
    );
    assert_eq!(value["poll"]["voters_count"], 0);
    assert_eq!(value["poll"]["voted"], false, "nobody has voted yet");
    assert_eq!(value["poll"]["own_votes"], json!([]));
    assert!(
        value["poll"]["expires_at"].is_string(),
        "the deadline renders as a timestamp"
    );

    assert!(stand.multiple, "the stored row carries the any-choice flag");
    assert!(!stand.hide_totals, "and the submitted show-totals decision");
    assert_deadline(&stand, before, 3_600);
    assert_eq!(
        stored_options(&pool, stand.id).await,
        vec![(0, "a".to_owned(), 0), (1, "b".to_owned(), 0),],
        "one row per option, in position order, at zero votes"
    );
    let row = created_row(&pool, &value).await;
    assert_eq!(
        row.expires_at, None,
        "the poll's closing time is not the status's deletion time"
    );
    assert_eq!(
        value["pleroma"]["expires_at"],
        Value::Null,
        "and the entity agrees the post never expires"
    );
    assert_eq!(status_rows(&pool).await, 1, "one status was stored");
    assert_eq!(poll_rows(&pool).await, 1, "with exactly one poll row");
}

/// The form spelling of the same shape — nested bracket keys the extractor
/// structures into the poll object — stores exactly what the JSON body
/// stored, and answers the same entity. `media_ids[]` without a poll is
/// still accepted and still stores no poll.
#[tokio::test]
async fn a_poll_arrives_from_nested_form_keys_too() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());

    let json_value = accepted(
        &app,
        &token,
        json!({
            "status": "ballot",
            "poll": {"options": ["a", "b"], "expires_in": 3600, "multiple": true},
        }),
    )
    .await;
    let from_json = stored_poll(&pool, &json_value).await;

    let before = Utc::now();
    let response = common::TestRequest::post(STATUSES)
        .bearer(&token)
        .form(&[
            ("status", "ballot from a form"),
            ("poll[options][]", "a"),
            ("poll[options][]", "b"),
            ("poll[expires_in]", "3600"),
            ("poll[multiple]", "true"),
        ])
        .send(&app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the nested form keys");
    let value = response.json_value();
    assert_eq!(
        value["poll"]["options"], json_value["poll"]["options"],
        "the form's poll renders exactly like the JSON one's"
    );
    assert_eq!(value["poll"]["multiple"], true, "flags and all");
    let from_form = stored_poll(&pool, &value).await;
    assert!(from_form.multiple, "the form's any-choice flag is stored");
    assert_deadline(&from_form, before, 3_600);
    assert_eq!(
        from_json.multiple, from_form.multiple,
        "the form row stores the flags the JSON row stores"
    );
    assert_eq!(
        stored_options(&pool, from_form.id).await,
        vec![(0, "a".to_owned(), 0), (1, "b".to_owned(), 0),],
        "and the same option rows"
    );

    let media_only = accepted(
        &app,
        &token,
        json!({"status": "media without a poll", "media_ids": ["01934567-89ab-7cde-f012-3456789abcde"]}),
    )
    .await;
    assert_eq!(
        media_only["poll"],
        Value::Null,
        "a poll-less body still answers `poll: null`"
    );
    assert!(
        created_row(&pool, &media_only)
            .await
            .content
            .contains("without a poll"),
        "media_ids beside no poll is accepted and otherwise ignored"
    );
    assert_eq!(
        status_rows(&pool).await,
        3,
        "the JSON poll, the form poll and the media body all posted"
    );
    assert_eq!(
        poll_rows(&pool).await,
        2,
        "and only the two bodies that carried a question stored a poll"
    );
}

/// The storage half of the `hide_totals` rule: the row keeps the flag and
/// every count column, while the entity withholds the per-option counts
/// as JSON `null` for as long as the poll is open — the roll-up
/// `votes_count` and `voters_count` still render.
#[tokio::test]
async fn a_hidden_poll_withholds_option_counts_while_open() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());

    let value = accepted(
        &app,
        &token,
        json!({
            "status": "secret ballot",
            "poll": {
                "options": ["a", "b"],
                "expires_in": 3600,
                "hide_totals": true,
            },
        }),
    )
    .await;
    assert_eq!(
        value["poll"]["options"],
        json!([
            {"title": "a", "votes_count": null},
            {"title": "b", "votes_count": null},
        ]),
        "the option counts stay hidden while the poll runs"
    );
    let poll_value = &value["poll"];
    assert!(
        poll_value["votes_count"].is_number() && poll_value["voters_count"].is_number(),
        "the roll-up columns are present even when hidden: {poll_value}"
    );
    assert_eq!(poll_value["votes_count"], 0);
    assert_eq!(poll_value["voters_count"], 0);
    assert_eq!(poll_value["expired"], false, "an hour out is open");

    let stand = stored_poll(&pool, &value).await;
    assert!(stand.hide_totals, "the flag is stored, not just rendered");
    assert_eq!(
        stored_options(&pool, stand.id).await,
        vec![(0, "a".to_owned(), 0), (1, "b".to_owned(), 0),],
        "the hidden counts are still stored zeros"
    );
}

/// A create whose body says nothing about a poll keeps the old shape
/// exactly: the status row stands, the answer carries `poll: null`, and
/// the `polls` and `poll_options` tables stay empty — the transaction
/// both create arms run through changes nothing for a plain post.
#[tokio::test]
async fn a_create_without_a_poll_stores_no_poll_row() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());

    let response = posted(&app, &token, json!({"status": "no question here"})).await;
    common::assert_answer(&response, StatusCode::OK, "a plain post");
    let value = response.json_value();

    assert_eq!(value["poll"], Value::Null, "no poll, no render");
    let row = created_row(&pool, &value).await;
    assert_eq!(
        row.raw_text, "no question here",
        "the plain status itself stands"
    );
    assert_eq!(status_rows(&pool).await, 1, "the status itself stands");
    assert_eq!(poll_rows(&pool).await, 0, "and no polls row was written");
    assert_eq!(option_rows(&pool).await, 0, "nor any option rows");
}

/// A poll create under an `Idempotency-Key` replayed twice: the retry
/// answers the same status and the same poll, and the database holds
/// exactly one status, one claim and one poll with its two options —
/// neither the replay nor its early-returning arm writes a second row.
#[tokio::test]
async fn a_replayed_idempotency_key_stores_the_poll_once() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());

    let body = json!({
        "status": "keyed ballot",
        "poll": {"options": ["a", "b"], "expires_in": 3600, "multiple": true},
    });
    let first = common::TestRequest::post(STATUSES)
        .bearer(&token)
        .header("idempotency-key", "K-1")
        .json(&body)
        .send(&app)
        .await;
    common::assert_answer(&first, StatusCode::OK, "the keyed poll create");
    let second = common::TestRequest::post(STATUSES)
        .bearer(&token)
        .header("idempotency-key", "K-1")
        .json(&body)
        .send(&app)
        .await;
    common::assert_answer(&second, StatusCode::OK, "the replay of the keyed create");

    let one = first.json_value();
    let two = second.json_value();
    assert_eq!(two["id"], one["id"], "one key, one status");
    assert_eq!(
        two["poll"]["id"], one["poll"]["id"],
        "and the replay answers the same stored poll"
    );
    assert_eq!(status_rows(&pool).await, 1, "exactly one status row");
    assert_eq!(poll_rows(&pool).await, 1, "exactly one poll row");
    let stand = stored_poll(&pool, &one).await;
    assert_eq!(
        stored_options(&pool, stand.id).await,
        vec![(0, "a".to_owned(), 0), (1, "b".to_owned(), 0),],
        "with exactly its two options"
    );
    let claims = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM idempotency_keys")
        .fetch_one(&pool)
        .await
        .expect("the claim count reads");
    assert_eq!(claims, 1, "one claim for the pair");
}

/// A `preview` body carrying a poll runs every check — the poll even
/// stands in for the empty text — and stores nothing: the answer carries
/// no poll, and the `statuses`, `polls` and `poll_options` tables stay
/// empty, exactly as the plain preview does.
#[tokio::test]
async fn a_preview_with_a_poll_answers_nothing_and_stores_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());

    let value = accepted(
        &app,
        &token,
        json!({
            "status": "   ",
            "preview": true,
            "poll": {"options": ["a", "b"], "expires_in": 3600},
        }),
    )
    .await;

    assert_eq!(
        value["poll"],
        Value::Null,
        "a preview has stored nothing to render"
    );
    assert_eq!(status_rows(&pool).await, 0, "no status row");
    assert_eq!(poll_rows(&pool).await, 0, "no polls row");
    assert_eq!(option_rows(&pool).await, 0, "and no option rows");
}

/// A poll carries an otherwise empty status (the empty-text refusal is
/// skipped for it), a `direct` audience is a legal private question, and
/// duplicate options post and store; the same empty text without a poll
/// stays the old 422. Every accepted body here leaves its status *and*
/// its poll behind.
#[tokio::test]
async fn a_poll_stands_in_for_the_text_and_needs_no_audience_change() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());

    let blank = accepted(
        &app,
        &token,
        json!({"status": "   ", "poll": {"options": two_options()}}),
    )
    .await;
    assert_eq!(blank["content"], "", "the textless post carries no content");
    let blank_row = created_row(&pool, &blank).await;
    assert_eq!(blank_row.raw_text, "", "and the stored text is empty");
    assert_eq!(
        blank["poll"]["options"],
        json!([
            {"title": "yes", "votes_count": 0},
            {"title": "no", "votes_count": 0},
        ]),
        "while the poll stands in and renders"
    );
    let blank_poll = poll::get_for_status(&pool, blank_row.id)
        .await
        .expect("the poll reads")
        .expect("the empty-text status carries its poll row");
    assert_eq!(
        blank_poll.status_id, blank_row.id,
        "and the poll names that empty-text status"
    );

    let private = accepted(
        &app,
        &token,
        json!({
            "status": "vote quietly",
            "visibility": "direct",
            "poll": {"options": two_options()},
        }),
    )
    .await;
    assert_eq!(private["visibility"], "direct", "a direct poll posts");

    let duplicates = accepted(
        &app,
        &token,
        json!({"status": "twice", "poll": {"options": ["same", "same"]}}),
    )
    .await;
    let duplicates_row = created_row(&pool, &duplicates).await;
    assert_eq!(
        duplicates_row.raw_text, "twice",
        "and duplicate options post too"
    );
    let duplicate_poll = stored_poll(&pool, &duplicates).await;
    assert_eq!(
        stored_options(&pool, duplicate_poll.id).await,
        vec![(0, "same".to_owned(), 0), (1, "same".to_owned(), 0),],
        "each posted copy keeps its own option row and position"
    );

    let refused_response = posted(&app, &token, json!({"status": "   "})).await;
    common::assert_answer(
        &refused_response,
        StatusCode::UNPROCESSABLE_ENTITY,
        "the same blank without a poll",
    );
    common::assert_error(
        &refused_response,
        EMPTY_STATUS,
        "the same blank without a poll",
    );
    assert_eq!(
        status_rows(&pool).await,
        3,
        "only the accepted bodies wrote"
    );
    assert_eq!(poll_rows(&pool).await, 3, "and each carries its own poll");
}

/// A poll with no options at all, or with only one, is Pleroma's minimum
/// refusal — the empty list reaching the service, not the extractor — from
/// a JSON or a form body alike.
#[tokio::test]
async fn a_poll_with_fewer_than_two_options_is_refused() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());

    for poll in [
        json!({}),
        json!({"options": []}),
        json!({"options": ["only"]}),
    ] {
        refused(
            &app,
            &token,
            json!({"status": "hi", "poll": poll}),
            TOO_FEW,
            "a poll short of two options",
        )
        .await;
    }
    let response = common::TestRequest::post(STATUSES)
        .bearer(&token)
        .form(&[("status", "form"), ("poll[options][]", "only")])
        .send(&app)
        .await;
    common::assert_answer(
        &response,
        StatusCode::UNPROCESSABLE_ENTITY,
        "one option from a form body",
    );
    common::assert_error(&response, TOO_FEW, "one option from a form body");
    assert_eq!(status_rows(&pool).await, 0, "and none of them wrote a row");
    assert_eq!(poll_rows(&pool).await, 0, "or a poll");
}

/// Twenty-one options are one past the stock `MAX_POLL_OPTIONS` ceiling,
/// twenty exactly are its boundary — and the boundary's twenty options
/// store with it; the refusal writes nothing.
#[tokio::test]
async fn a_poll_past_the_option_ceiling_is_refused() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());

    refused(
        &app,
        &token,
        json!({"status": "hi", "poll": {"options": numbered_options(21)}}),
        TOO_MANY,
        "twenty-one options",
    )
    .await;
    let boundary = accepted(
        &app,
        &token,
        json!({"status": "hi", "poll": {"options": numbered_options(20)}}),
    )
    .await;
    assert_eq!(
        created_row(&pool, &boundary).await.visibility,
        Visibility::Public,
        "the boundary body is a public post"
    );
    let stand = stored_poll(&pool, &boundary).await;
    assert_eq!(
        stored_options(&pool, stand.id).await.len(),
        20,
        "all twenty options store, positions counting from zero"
    );
    assert_eq!(status_rows(&pool).await, 1, "only the boundary body wrote");
    assert_eq!(poll_rows(&pool).await, 1, "with its one poll");
}

/// An option longer than the stock 200 characters, and an empty option,
/// share the one refusal sentence; 200 characters exactly is within the
/// cap and stores, and the form spelling is checked the same way.
#[tokio::test]
async fn a_broken_poll_option_is_refused() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());

    refused(
        &app,
        &token,
        json!({"status": "hi", "poll": {"options": ["x".repeat(201), "b"]}}),
        BROKEN_OPTION,
        "a 201-character option",
    )
    .await;
    refused(
        &app,
        &token,
        json!({"status": "hi", "poll": {"options": ["", "b"]}}),
        BROKEN_OPTION,
        "an empty option",
    )
    .await;
    let exact = accepted(
        &app,
        &token,
        json!({"status": "hi", "poll": {"options": ["x".repeat(200), "b"]}}),
    )
    .await;
    assert_eq!(
        created_row(&pool, &exact).await.raw_text,
        "hi",
        "the 200-character boundary posts"
    );
    assert_eq!(poll_rows(&pool).await, 1, "and stores its poll");

    let long = "x".repeat(201);
    let response = common::TestRequest::post(STATUSES)
        .bearer(&token)
        .form(&[
            ("status", "form"),
            ("poll[options][]", long.as_str()),
            ("poll[options][]", "b"),
        ])
        .send(&app)
        .await;
    common::assert_error(
        &response,
        BROKEN_OPTION,
        "a 201-character option from a form",
    );
    assert_eq!(
        status_rows(&pool).await,
        1,
        "the accepted boundary is the only write"
    );
}

/// The poll expiration window: 299 (and every shorter span, negatives
/// included) is too soon, the 300 floor and the one-year ceiling pass, and
/// `31_536_001` is too far; the form's string number is checked the same
/// way, and the status's own top-level `expires_in` keeps its own floor.
/// The in-window spans store their polls with exactly the deadlines the
/// check computed, and leave the status's own expiry alone.
#[tokio::test]
async fn a_poll_expiration_outside_the_window_is_refused() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());

    let with_span = |secs: i64| {
        json!({
            "status": "hi",
            "poll": {"options": two_options(), "expires_in": secs},
        })
    };
    for short in [-1_i64, 0, 299] {
        refused(
            &app,
            &token,
            with_span(short),
            TOO_SOON,
            "a span under the floor",
        )
        .await;
    }
    refused(
        &app,
        &token,
        with_span(31_536_001),
        TOO_FAR,
        "a span past the ceiling",
    )
    .await;
    for inside in [300_i64, 31_536_000] {
        let before = Utc::now();
        let value = accepted(&app, &token, with_span(inside)).await;
        let stand = stored_poll(&pool, &value).await;
        assert_deadline(&stand, before, inside);
        assert_eq!(
            created_row(&pool, &value).await.expires_at,
            None,
            "an in-window poll span leaves the status's own expiry alone"
        );
    }

    let response = common::TestRequest::post(STATUSES)
        .bearer(&token)
        .form(&[
            ("status", "form"),
            ("poll[options][]", "a"),
            ("poll[options][]", "b"),
            ("poll[expires_in]", "299"),
        ])
        .send(&app)
        .await;
    common::assert_answer(
        &response,
        StatusCode::UNPROCESSABLE_ENTITY,
        "the form's string number",
    );
    common::assert_error(&response, TOO_SOON, "the form's string number");

    // The status's own top-level `expires_in` is a different parameter with
    // a different floor: its message stays Pleroma's `expires_in` wording.
    let status_floor = posted(&app, &token, json!({"status": "hi", "expires_in": 300})).await;
    common::assert_error(
        &status_floor,
        "expires_in must be at least 600 seconds",
        "the status floor is not the poll floor",
    );
    assert_eq!(
        status_rows(&pool).await,
        2,
        "only the two in-window spans wrote"
    );
    assert_eq!(poll_rows(&pool).await, 2, "and both stored their poll");
}

/// `poll[expires_in]` that is no integer at all is the extractor's 422,
/// before any service rule runs.
#[tokio::test]
async fn a_poll_expiration_that_is_no_integer_is_the_extractors_422() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());

    let response = posted(
        &app,
        &token,
        json!({
            "status": "hi",
            "poll": {"options": two_options(), "expires_in": "soon"},
        }),
    )
    .await;
    common::assert_answer(
        &response,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a string that spells no integer",
    );
    assert!(
        response.json_value()["error"]
            .as_str()
            .is_some_and(|text| text.starts_with("Invalid parameters")),
        "the refusal is the extractor's, naming the bad parameter"
    );
    assert_eq!(status_rows(&pool).await, 0, "and it wrote nothing");
    assert_eq!(poll_rows(&pool).await, 0, "poll rows included");
}

/// A poll sent beside non-empty `media_ids` is the attachment conflict
/// whatever the ids name, and writes nothing; an empty list is no
/// attachment, and posts with its poll stored.
#[tokio::test]
async fn a_poll_with_attachments_is_refused() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());

    refused(
        &app,
        &token,
        json!({
            "status": "hi",
            "poll": {"options": two_options()},
            "media_ids": ["01934567-89ab-7cde-f012-3456789abcde"],
        }),
        WITH_MEDIA,
        "a poll with one id",
    )
    .await;
    let empty_list = accepted(
        &app,
        &token,
        json!({
            "status": "hi",
            "poll": {"options": two_options()},
            "media_ids": [],
        }),
    )
    .await;
    assert_eq!(
        created_row(&pool, &empty_list).await.content,
        "<p>hi</p>",
        "the empty id list posted"
    );
    assert_eq!(status_rows(&pool).await, 1, "only the empty list posted");
    assert_eq!(
        poll_rows(&pool).await,
        1,
        "and the body that carried a question stored its poll"
    );
}

/// A poll does not open the create route to anonymous callers or to a
/// token without the write scope: the gate runs before any body check.
#[tokio::test]
async fn a_poll_post_still_needs_a_write_token() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", "read:statuses").await;
    let app = common::test_app(pool.clone());

    let body = json!({"status": "hi", "poll": {"options": two_options()}});
    let anonymous = common::TestRequest::post(STATUSES)
        .json(&body)
        .send(&app)
        .await;
    common::assert_answer(&anonymous, StatusCode::UNAUTHORIZED, "no token at all");
    common::assert_error(&anonymous, NO_TOKEN, "no token at all");

    let wrong_scope = posted(&app, &token, body).await;
    common::assert_answer(
        &wrong_scope,
        StatusCode::FORBIDDEN,
        "a token without write:statuses",
    );
    assert_eq!(
        status_rows(&pool).await,
        0,
        "and neither gate let a poll body through"
    );
    assert_eq!(poll_rows(&pool).await, 0, "poll rows included");
}
