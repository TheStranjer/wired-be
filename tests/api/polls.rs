//! Poll parameters on `POST /api/v1/statuses` over HTTP: the validation
//! half of the polls ticket (WBE-93).
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
//! Nothing here persists the poll — that is the storage ticket. Every
//! accepted body, with a poll or without one, creates a plain status whose
//! entity answers `"poll": null`, whose `expires_at` the poll's
//! `expires_in` does not touch, and which leaves the `polls` table empty.

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::models::{
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

/// A creation asserted 200, answered as JSON, with the one thing every
/// accepted poll body owes until the storage ticket lands: `poll` is null.
async fn accepted(app: &Router, token: &str, body: Value) -> Value {
    let response = posted(app, token, body).await;
    common::assert_answer(&response, StatusCode::OK, "a body with a valid poll");
    let value = response.json_value();
    assert_eq!(value["poll"], Value::Null, "nothing persists the poll yet");
    value
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

/// How many `polls` rows the database holds: zero for every request in
/// this file, because validation persists nothing.
async fn poll_rows(pool: &PgPool) -> i64 {
    sqlx::query_as::<_, (i64,)>("SELECT count(*) FROM polls")
        .fetch_one(pool)
        .await
        .expect("the poll count reads")
        .0
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

/// A poll body carrying the whole accepted shape is 200: a plain status
/// whose `poll` is null, whose expiry the poll's `expires_in` did not set,
/// and which left no `polls` rows behind.
#[tokio::test]
async fn an_accepted_poll_creates_a_plain_status_answering_poll_null() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());

    let value = accepted(
        &app,
        &token,
        json!({
            "status": "lunch?",
            "poll": {
                "options": two_options(),
                "expires_in": 3600,
                "multiple": true,
                "hide_totals": true,
            },
        }),
    )
    .await;
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
    assert_eq!(
        status_rows(&pool).await,
        1,
        "exactly one plain status was stored"
    );
    assert_eq!(
        poll_rows(&pool).await,
        0,
        "and no poll rows — persistence is a later ticket"
    );
}

/// The form spelling of the same shape — nested bracket keys the extractor
/// structures into the poll object — posts as well as the JSON body, and
/// bare `media_ids[]` without a poll is accepted and otherwise ignored.
#[tokio::test]
async fn a_poll_arrives_from_nested_form_keys_too() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());

    let response = common::TestRequest::post(STATUSES)
        .bearer(&token)
        .form(&[
            ("status", "lunch from a form"),
            ("poll[options][]", "yes"),
            ("poll[options][]", "no"),
            ("poll[expires_in]", "3600"),
            ("poll[multiple]", "true"),
            ("poll[hide_totals]", "on"),
        ])
        .send(&app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the nested form keys");
    assert_eq!(
        response.json_value()["poll"],
        Value::Null,
        "and the same poll: null answer"
    );

    let media_only = accepted(
        &app,
        &token,
        json!({"status": "media without a poll", "media_ids": ["01934567-89ab-7cde-f012-3456789abcde"]}),
    )
    .await;
    assert!(
        created_row(&pool, &media_only)
            .await
            .content
            .contains("without a poll"),
        "media_ids beside no poll is accepted and otherwise ignored"
    );
    assert_eq!(
        status_rows(&pool).await,
        2,
        "the form poll and the media body both posted"
    );
}

/// A poll carries an otherwise empty status (the empty-text refusal is
/// skipped for it), a `direct` audience is a legal private question, and
/// duplicate options post; the same empty text without a poll stays the
/// old 422.
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
    assert_eq!(
        created_row(&pool, &duplicates).await.raw_text,
        "twice",
        "and duplicate options post too"
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
}

/// Twenty-one options are one past the stock `MAX_POLL_OPTIONS` ceiling,
/// twenty exactly are its boundary, and the message names the ceiling.
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
        "the boundary body is a plain public post"
    );
    assert_eq!(status_rows(&pool).await, 1, "only the boundary body wrote");
}

/// An option longer than the stock 200 characters, and an empty option,
/// share the one refusal sentence; 200 characters exactly is within the
/// cap, and the form spelling is checked the same way.
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
        let value = accepted(&app, &token, with_span(inside)).await;
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
}

/// A poll sent beside non-empty `media_ids` is the attachment conflict
/// whatever the ids name; an empty list is no attachment and posts.
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
        "the empty id list posted as the plain status it is"
    );
    assert_eq!(status_rows(&pool).await, 1, "only the empty list posted");
    assert_eq!(poll_rows(&pool).await, 0, "and still no poll was stored");
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
}
