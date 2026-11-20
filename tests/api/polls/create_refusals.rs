//! The create's poll refusals over HTTP: every 422 the checks pin — the
//! two-option minimum, the option ceiling and its boundary, the broken
//! option, the expiration window and its boundaries, the non-integer
//! `poll[expires_in]`, the attachment conflict and the token gate — with
//! the JSON and form spellings checked alike and the refusals writing
//! nothing.

use axum::http::StatusCode;
use chrono::Utc;
use serde_json::json;
use wired_be::models::status::Visibility;

use super::support::{
    BROKEN_OPTION, NO_TOKEN, STATUSES, TOO_FAR, TOO_FEW, TOO_MANY, TOO_SOON, WITH_MEDIA, WRITE,
    accepted, assert_deadline, created_row, numbered_options, poll_rows, posted, refused,
    status_rows, stored_options, stored_poll, two_options, user_with_token,
};
use crate::common;

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
