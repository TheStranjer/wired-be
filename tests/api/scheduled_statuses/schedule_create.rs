//! The scheduling arm's own answers: a future instant parks the post and
//! answers the whole entity; the window, the unparsable text, the create's
//! refusals and the scope gate refuse without writing; and form and query
//! bodies schedule the very same post JSON does.

use axum::http::StatusCode;
use serde_json::json;
use uuid::Uuid;
use wired_be::{models::scheduled_status, util::time::parse_time};

use super::support::{
    EMPTY_STATUS, INVALID_SCHEDULED_AT, NO_TOKEN, NO_WRITE, NOT_FOUND, SHORT_EXPIRY, STATUSES,
    UNKNOWN_ID, WRITE, assert_entity_answers_the_request, assert_only_the_fire_is_queued,
    assert_row_parks_the_request, in_minutes, post_count, schedule, scheduled_count,
    stored_schedule, writer,
};
use crate::common;

/// A future `scheduled_at` answers 200 with the whole `ScheduledStatus`
/// entity pinned key for key and stores the normalised request — and
/// writes nothing else: no status, no key claim, and one publish job due
/// at the submitted instant.
#[tokio::test]
async fn a_scheduled_post_answers_the_entity_and_posts_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let oauth_app = common::create_app(&pool, WRITE).await;
    let token = common::create_token(&pool, &alice, &oauth_app, WRITE)
        .await
        .token;
    let app = common::test_app(pool.clone());
    let application_id = oauth_app.id.to_string();

    let at = in_minutes(60);
    let response = common::TestRequest::post(STATUSES)
        .bearer(&token)
        .header("idempotency-key", "K-1")
        .json(&json!({
            "status": "  scheduled hello  ",
            "visibility": "unlisted",
            "sensitive": true,
            "spoiler_text": "careful",
            "language": "eo",
            "content_type": "text/plain",
            "expires_in": 3600,
            "scheduled_at": at,
        }))
        .send(&app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "a future scheduled_at");

    let entity = response.json_value();
    assert_entity_answers_the_request(&entity, &at, &application_id);
    let id = Uuid::parse_str(entity["id"].as_str().expect("the entity carries its id"))
        .expect("the id is a uuid");
    assert_row_parks_the_request(&pool, id, &alice, &at, &application_id).await;
    assert_only_the_fire_is_queued(&pool, &stored_schedule(&pool, id).await).await;
}

/// A past instant and one inside the window are the same refusal, whose
/// wording names the instance's configured minutes — the default 300
/// reads "5 minutes", a `min_scheduled_secs` of 600 reads "10 minutes" —
/// and neither stores a schedule or posts anything.
#[tokio::test]
async fn a_moment_inside_the_window_is_refused_with_the_configured_minutes() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = writer(&pool, "alice").await;
    let app = common::test_app(pool.clone());

    for (when, who) in [
        (in_minutes(-60), "an hour ago"),
        (in_minutes(1), "one minute out"),
        (in_minutes(0), "right now"),
    ] {
        let refused = schedule(
            &app,
            &token,
            json!({"status": "too soon", "scheduled_at": when}),
        )
        .await;
        common::assert_answer(&refused, StatusCode::UNPROCESSABLE_ENTITY, who);
        common::assert_error(
            &refused,
            "Scheduled time must be at least 5 minutes in the future",
            &format!("{who} on the default window"),
        );
    }

    let mut config = common::test_config();
    config.min_scheduled_secs = 600;
    let strict = common::test_app_with_config(pool.clone(), config);
    let refused = schedule(
        &strict,
        &token,
        json!({"status": "still too soon", "scheduled_at": in_minutes(5)}),
    )
    .await;
    common::assert_answer(
        &refused,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a minute inside the widened window",
    );
    common::assert_error(
        &refused,
        "Scheduled time must be at least 10 minutes in the future",
        "the refusal tracks the setting",
    );

    assert_eq!(
        scheduled_count(&pool).await,
        0,
        "not one window refusal stored a schedule"
    );
    assert_eq!(post_count(&pool).await, 0, "and none posted");
}

/// `scheduled_at` text that is no RFC3339 instant is 422
/// `Invalid scheduled_at`, before any of the schedule's other gates run,
/// and stores nothing.
#[tokio::test]
async fn unparsable_scheduled_at_text_is_refused() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = writer(&pool, "alice").await;
    let app = common::test_app(pool.clone());

    for text in ["not-a-date", "2026-02-30", "09:00:00", "1706722496"] {
        let refused = schedule(&app, &token, json!({"status": "hi", "scheduled_at": text})).await;
        common::assert_answer(
            &refused,
            StatusCode::UNPROCESSABLE_ENTITY,
            &format!("`{text}`"),
        );
        common::assert_error(&refused, INVALID_SCHEDULED_AT, &format!("`{text}`"));
    }

    assert_eq!(scheduled_count(&pool).await, 0);
    assert_eq!(post_count(&pool).await, 0);
}

/// The create's own refusals keep their exact answers on this arm: an
/// empty text is the 422 it always was, a bad `in_reply_to_id` is the
/// shared 404, and a short `expires_in` its floor's 422 — every one of
/// them ahead of the schedule's own gates, and none storing anything.
#[tokio::test]
async fn the_schedule_arm_keeps_the_creates_own_refusals() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = writer(&pool, "alice").await;
    let app = common::test_app(pool.clone());
    let at = in_minutes(60);

    let refusals = [
        (
            json!({"status": "   ", "scheduled_at": at}),
            StatusCode::UNPROCESSABLE_ENTITY,
            EMPTY_STATUS,
        ),
        (
            json!({"status": "", "media_ids": [], "scheduled_at": at}),
            StatusCode::UNPROCESSABLE_ENTITY,
            EMPTY_STATUS,
        ),
        (
            json!({"status": "hi", "in_reply_to_id": UNKNOWN_ID, "scheduled_at": at}),
            StatusCode::NOT_FOUND,
            NOT_FOUND,
        ),
        (
            json!({"status": "hi", "in_reply_to_id": "nope", "scheduled_at": at}),
            StatusCode::NOT_FOUND,
            NOT_FOUND,
        ),
        (
            json!({"status": "hi", "expires_in": 0, "scheduled_at": at}),
            StatusCode::UNPROCESSABLE_ENTITY,
            SHORT_EXPIRY,
        ),
        (
            json!({"status": "hi", "visibility": "sideways", "scheduled_at": at}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "Invalid visibility",
        ),
    ];
    for (body, status, message) in refusals {
        let refused = schedule(&app, &token, body.clone()).await;
        common::assert_answer(&refused, status, &format!("the refused body `{body}`"));
        common::assert_error(&refused, message, &format!("the wording for `{body}`"));
    }

    assert_eq!(scheduled_count(&pool).await, 0);
    assert_eq!(post_count(&pool).await, 0);
}

/// The gate is the create's: no token is the extractor's 401, and a user
/// token without `write:statuses` is 403 — a valid future `scheduled_at`
/// changes neither answer, and neither reaches the table.
#[tokio::test]
async fn the_schedule_arm_needs_its_scope() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_bob, read_only) = {
        let user = common::create_local_user(&pool, "bob").await;
        let token = common::create_app_and_token(&pool, &user, "read:statuses").await;
        (user, token)
    };
    let app = common::test_app(pool.clone());
    let body = json!({"status": "someday", "scheduled_at": in_minutes(60)});

    let anonymous = common::TestRequest::post(STATUSES)
        .json(&body)
        .send(&app)
        .await;
    common::assert_answer(&anonymous, StatusCode::UNAUTHORIZED, "no token");
    common::assert_error(&anonymous, NO_TOKEN, "the anonymous refusal");

    let unread = schedule(&app, &read_only, body).await;
    common::assert_answer(
        &unread,
        StatusCode::FORBIDDEN,
        "a token without write:statuses",
    );
    common::assert_error(&unread, NO_WRITE, "the scope refusal");

    assert_eq!(scheduled_count(&pool).await, 0);
    assert_eq!(post_count(&pool).await, 0);
}

/// A form body's urlencoded `scheduled_at` and a query parameter of the
/// same name schedule the very same post a JSON body does.
#[tokio::test]
async fn form_and_query_bodies_schedule_like_json() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = writer(&pool, "alice").await;
    let app = common::test_app(pool.clone());

    let at = in_minutes(60);
    let from_form = common::TestRequest::post(STATUSES)
        .bearer(&token)
        .form(&[("status", "form scheduled"), ("scheduled_at", at.as_str())])
        .send(&app)
        .await;
    common::assert_answer(&from_form, StatusCode::OK, "a form body's scheduled_at");
    assert_eq!(
        from_form.json_value()["params"]["text"],
        "form scheduled",
        "the form body parks the same request"
    );

    let later = in_minutes(120);
    let from_query = common::TestRequest::post(&format!("{STATUSES}?scheduled_at={later}"))
        .bearer(&token)
        .form(&[("status", "query scheduled")])
        .send(&app)
        .await;
    common::assert_answer(
        &from_query,
        StatusCode::OK,
        "a query parameter's scheduled_at",
    );

    let rows = sqlx::query_as::<_, scheduled_status::ScheduledStatus>(
        "SELECT * FROM scheduled_statuses ORDER BY scheduled_at",
    )
    .fetch_all(&pool)
    .await
    .expect("the schedules read");
    assert_eq!(rows.len(), 2, "both bodies parked one row each");
    assert_eq!(
        rows[0].scheduled_at,
        parse_time(&at).expect("the fixture instant is a timestamp"),
        "the form row is due at its instant"
    );
    assert_eq!(
        rows[0].params["status"], "form scheduled",
        "with the form's own normalised text"
    );
    assert_eq!(
        rows[1].scheduled_at,
        parse_time(&later).expect("the fixture instant is a timestamp"),
        "and the query row at its own"
    );
    assert_eq!(rows[1].params["status"], "query scheduled");
    assert_eq!(post_count(&pool).await, 0, "and nothing posted");
}
