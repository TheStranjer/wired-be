//! Poll parameters on `POST /api/v1/statuses` over HTTP: the validation
//! and the storage half of the polls tickets (WBE-93), and the poll's own
//! routes (WBE-106): `GET /api/v1/polls/{id}` answering the [`Poll`]
//! entity for its viewer, and `POST /api/v1/polls/{id}/votes` casting the
//! ballot — tested below after the create-side tests.
//!
//! The read is optionally authenticated (`read:statuses` on a presented
//! user token) and shows the viewer's own `voted`/`own_votes`; the poll's
//! status is the visibility gate, so an unknown, malformed or invisible
//! poll is the shared 404 on both routes. The vote needs
//! `write:statuses`, rides the `statuses_actions` bucket, and refuses
//! with the ticket's pinned 422 bodies, in order: a closed poll "Poll's
//! finished", a second ballot "You have already voted on this poll", the
//! author "Poll's author can't vote", an empty body "Missing parameter:
//! choices", an out-of-range index "Invalid indices", and a second
//! distinct choice on a single-choice poll "Too many choices" — with
//! duplicate indexes ignored. A passing ballot is one transaction
//! writing `poll_votes` and moving every count the entity reports.
//! `hide_totals` lifts at the deadline on this route too.
//!
//! [`Poll`]: wired_be::entities::poll::Poll
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
//! `media_ids` keeps the attachments ticket's meaning: beside a poll it
//! is the 422 `Polls cannot have attachments`, on its own it claims the
//! poster's uploads onto the post — and either way a poll-less body
//! stores no `polls` row.

#![cfg(test)]

use axum::{Router, http::StatusCode};
use chrono::{TimeDelta, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::TEST_BASE_URL,
    models::{
        media_attachment::{self, NewMediaAttachment},
        poll::{self, Poll},
        status::{self, Visibility},
        user::User,
    },
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

/// An unposted upload by `author` — a `media_ids` entry a create may claim.
async fn upload(pool: &PgPool, author: &User) -> Uuid {
    media_attachment::insert(
        pool,
        &NewMediaAttachment {
            user_id: Some(author.id),
            status_id: None,
            url: format!("{TEST_BASE_URL}/media/fixture.png"),
            content_type: "image/png".to_owned(),
            ..NewMediaAttachment::default()
        },
    )
    .await
    .expect("the fixture upload inserts")
    .id
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
/// stored, and answers the same entity. `media_ids` without a poll posts
/// its attachment the way the attachments ticket wired it (see
/// [`media_without_a_poll`]), and still stores no poll.
#[tokio::test]
async fn a_poll_arrives_from_nested_form_keys_too() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = user_with_token(&pool, "alice", WRITE).await;
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

    media_without_a_poll(&app, &pool, &alice, &token).await;
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

/// A `media_ids` body with no poll: the upload claims the post the way
/// the attachments ticket wired it, the entity answers `poll: null`, and
/// no `polls` row appears.
async fn media_without_a_poll(app: &Router, pool: &PgPool, alice: &User, token: &str) {
    let mine = upload(pool, alice).await;
    let media_only = accepted(
        app,
        token,
        json!({"status": "media without a poll", "media_ids": [mine.to_string()]}),
    )
    .await;
    assert_eq!(
        media_only["poll"],
        Value::Null,
        "a poll-less body still answers `poll: null`"
    );
    assert_eq!(
        media_only["media_attachments"]
            .as_array()
            .expect("the entity answers its attachments")
            .len(),
        1,
        "the media rides the post the attachments ticket wired"
    );
    assert!(
        created_row(pool, &media_only)
            .await
            .content
            .contains("without a poll"),
        "media_ids beside no poll posts, attachment and all, no question"
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

// ————————————————— WBE-106: the poll's own routes —————————————————
//
// Everything below drives `GET /api/v1/polls/{id}` and
// `POST /api/v1/polls/{id}/votes` — the read, the ballot, every refusal
// the ticket pins, and the counters a passing vote moves.

/// The path prefix both poll routes mount under.
const POLLS: &str = "/api/v1/polls";
/// The shared 404 body of both routes.
const NOT_FOUND: &str = "Record not found";
/// The 422 body for a poll whose deadline has passed.
const FINISHED: &str = "Poll's finished";
/// The 422 body for a voter who already marked the poll.
const SECOND_BALLOT: &str = "You have already voted on this poll";
/// The 422 body for the author of the poll's status.
const AUTHOR_REFUSAL: &str = "Poll's author can't vote";
/// The 422 body for a vote body that carries no choices.
const NO_CHOICES: &str = "Missing parameter: choices";
/// The 422 body for an index no option of the poll holds.
const BAD_INDEX: &str = "Invalid indices";
/// The 422 body for a second distinct choice on a single-choice poll.
const SECOND_MARK: &str = "Too many choices";
/// The 403 body for an app-only token at a user endpoint.
const NO_USER: &str = "This API requires an authenticated user";

/// The poll `poll_id` names over `GET /api/v1/polls/{id}`, `token` the
/// bearer it presents (`None`: anonymously).
async fn read_poll(app: &Router, poll_id: &str, token: Option<&str>) -> common::TestResponse {
    let request = common::TestRequest::get(&format!("{POLLS}/{poll_id}"));
    let request = match token {
        Some(token) => request.bearer(token),
        None => request,
    };
    request.send(app).await
}

/// `POST /api/v1/polls/{poll_id}/votes` as `token` with the JSON `body`.
async fn vote_on(app: &Router, token: &str, poll_id: &str, body: Value) -> common::TestResponse {
    common::TestRequest::post(&format!("{POLLS}/{poll_id}/votes"))
        .bearer(token)
        .json(&body)
        .send(app)
        .await
}

/// A vote that carries exactly the `choices` array.
async fn marks(app: &Router, token: &str, poll_id: &str, choices: Value) -> common::TestResponse {
    vote_on(app, token, poll_id, json!({ "choices": choices })).await
}

/// A public poll of `["tea", "coffee"]` posted through the create —
/// `extra` merged into the `poll` parameters — answered as the created
/// status's answer, returning the fresh poll's id.
async fn ballot(app: &Router, token: &str, extra: Value) -> String {
    let mut poll = json!({ "options": ["tea", "coffee"] });
    if let (Some(base), Some(extra)) = (poll.as_object_mut(), extra.as_object()) {
        for (key, value) in extra {
            let _over_wins: Option<Value> = base.insert(key.clone(), value.clone());
        }
    }
    let value = accepted(
        app,
        token,
        json!({ "status": "cast a ballot", "poll": poll }),
    )
    .await;
    value["poll"]["id"]
        .as_str()
        .expect("a poll create answers the poll entity")
        .to_owned()
}

/// A vote asserted 422 with exactly the message `refusal.0` as its
/// `{"error": …}` body, for the case named by `refusal.1`.
async fn refused_vote(
    app: &Router,
    token: &str,
    poll_id: &str,
    body: Value,
    refusal: (&str, &str),
) {
    let response = vote_on(app, token, poll_id, body).await;
    common::assert_answer(&response, StatusCode::UNPROCESSABLE_ENTITY, refusal.1);
    common::assert_error(&response, refusal.0, refusal.1);
}

/// The stored `polls` row `poll_id` names, demanded.
async fn poll_row(pool: &PgPool, poll_id: &str) -> Poll {
    poll::get_by_id(pool, parse_marked(poll_id))
        .await
        .expect("the poll reads")
        .expect("the poll is stored")
}

/// The answer's poll id text as a [`Uuid`].
fn parse_marked(text: &str) -> Uuid {
    Uuid::parse_str(text).expect("the answer carries the poll's id text")
}

/// Moves the poll's deadline an hour into the past: the state a finished
/// poll is in, which no create body can post (the window floor is five
/// minutes out).
async fn close_poll(pool: &PgPool, poll_id: &str) {
    let moved =
        sqlx::query("UPDATE polls SET expires_at = now() - interval '1 hour' WHERE id = $1")
            .bind(parse_marked(poll_id))
            .execute(pool)
            .await
            .expect("the deadline moves");
    assert_eq!(moved.rows_affected(), 1, "the fixture poll is stored");
}

/// How many marks `poll_id`'s `poll_votes` rows hold — what a refusal
/// never added to.
async fn mark_rows(pool: &PgPool, poll_id: &str) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM poll_votes WHERE poll_id = $1")
        .bind(parse_marked(poll_id))
        .fetch_one(pool)
        .await
        .expect("the marks count reads")
}

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
        own["voted"], false,
        "and the author is not a voter of her own question"
    );
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
