//! The accepted poll create over HTTP: the whole JSON and form shapes
//! that store a `polls` row and its options and answer the `Poll` entity,
//! the `hide_totals` storage rule, the plain create that stores no poll,
//! the replayed `Idempotency-Key`, the poll-carrying `preview`, and the
//! poll standing in for empty text with duplicates and a `direct`
//! audience allowed.

use axum::{Router, http::StatusCode};
use chrono::Utc;
use serde_json::{Value, json};
use sqlx::PgPool;
use wired_be::models::{poll, user::User};

use super::support::{
    EMPTY_STATUS, STATUSES, WRITE, accepted, assert_deadline, created_row, option_rows, poll_rows,
    posted, status_rows, stored_options, stored_poll, two_options, upload, user_with_token,
};
use crate::common;

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
    assert_eq!(
        value["poll"]["voted"], true,
        "the author reads their own poll as voted"
    );
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
