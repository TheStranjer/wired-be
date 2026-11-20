//! The scheduling arm's limits: the total cap and the per-day cap each
//! refuse the next schedule with `Too many scheduled statuses`, and every
//! `media_ids` entry must be the poster's own unclaimed upload — a
//! pending schedule reserves its media without writing any row.

use axum::http::StatusCode;
use chrono::{TimeDelta, Utc};
use serde_json::json;
use uuid::Uuid;
use wired_be::{
    config::TEST_BASE_URL,
    models::{media_attachment, scheduled_status, status::Visibility},
    util::time::mastodon_time,
};

use super::support::{
    MEDIA_NOT_FOUND, TOO_MANY, in_minutes, post_count, schedule, scheduled_count, seed_schedule,
    upload, writer,
};
use crate::common;

/// At `max_scheduled_per_user` pending rows the next schedule is 422
/// `Too many scheduled statuses` — while the same request from an
/// account under the cap still stands (the count is each account's own).
#[tokio::test]
async fn the_total_limit_refuses_the_next_schedule() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = writer(&pool, "alice").await;
    let (bob, bob_token) = writer(&pool, "bob").await;

    let mut config = common::test_config();
    config.max_scheduled_per_user = 2;
    config.max_scheduled_per_user_per_day = 100;
    let app = common::test_app_with_config(pool.clone(), config);

    let due = Utc::now() + TimeDelta::hours(1);
    seed_schedule(&pool, &bob, due).await;
    seed_schedule(&pool, &bob, due + TimeDelta::minutes(5)).await;

    let refused = schedule(
        &app,
        &bob_token,
        json!({"status": "one too many", "scheduled_at": in_minutes(60)}),
    )
    .await;
    common::assert_answer(
        &refused,
        StatusCode::UNPROCESSABLE_ENTITY,
        "at the total cap",
    );
    common::assert_error(&refused, TOO_MANY, "the total limit's refusal");

    let accepted = schedule(
        &app,
        &token,
        json!({"status": "nobody else is at the cap", "scheduled_at": in_minutes(60)}),
    )
    .await;
    common::assert_answer(&accepted, StatusCode::OK, "an account under the cap");

    assert_eq!(
        scheduled_count(&pool).await,
        3,
        "the refusal stored nothing; the acceptance stored one"
    );
    assert_eq!(post_count(&pool).await, 0, "and nothing posted");
}

/// At `max_scheduled_per_user_per_day` pending rows *within the UTC day
/// of the fire moment* the schedule is 422 `Too many scheduled statuses`,
/// while the day beside it stands free.
#[tokio::test]
async fn the_per_day_limit_refuses_the_same_utc_day_only() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = writer(&pool, "alice").await;

    let mut config = common::test_config();
    config.max_scheduled_per_user = 100;
    config.max_scheduled_per_user_per_day = 2;
    let app = common::test_app_with_config(pool.clone(), config);

    let at = Utc::now() + TimeDelta::hours(6);
    let (day_start, day_end) = scheduled_status::utc_day_bounds(at);
    seed_schedule(&pool, &alice, day_start + TimeDelta::seconds(1)).await;
    seed_schedule(&pool, &alice, day_end - TimeDelta::seconds(1)).await;

    let refused = schedule(
        &app,
        &token,
        json!({"status": "full day", "scheduled_at": mastodon_time(&at)}),
    )
    .await;
    common::assert_answer(&refused, StatusCode::UNPROCESSABLE_ENTITY, "a full day");
    common::assert_error(&refused, TOO_MANY, "the per-day limit's refusal");

    let next_day = at + TimeDelta::days(1);
    let accepted = schedule(
        &app,
        &token,
        json!({"status": "the day beside it", "scheduled_at": mastodon_time(&next_day)}),
    )
    .await;
    common::assert_answer(&accepted, StatusCode::OK, "a free day");

    assert_eq!(
        scheduled_count(&pool).await,
        3,
        "the refusal stored nothing; the free day stored one"
    );
    assert_eq!(post_count(&pool).await, 0, "and nothing posted");
}

/// A `media_ids` entry that is unknown, somebody else's upload, an upload
/// a status already claims, or text that is no id at all, is the create's
/// own 422 `Media not found` on this arm too — and no refusal stores a
/// schedule or moves a media row.
#[tokio::test]
async fn media_ids_the_poster_may_not_reserve_are_refused() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = writer(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let foreign = upload(&pool, &bob).await;
    let posted = common::create_status(&pool, &alice, "a live post", Visibility::Public).await;
    let attached = common::create_status_attachment(
        &pool,
        &posted,
        "image/png",
        &format!("{TEST_BASE_URL}/media/live.png"),
        0,
    )
    .await;
    let app = common::test_app(pool.clone());

    for (id, who) in [
        (Uuid::now_v7().to_string(), "an unknown id"),
        (foreign.id.to_string(), "somebody else's upload"),
        (attached.id.to_string(), "an already-attached upload"),
        ("not-an-id".to_owned(), "text that is no id"),
    ] {
        let refused = schedule(
            &app,
            &token,
            json!({"status": "pics", "media_ids": [id], "scheduled_at": in_minutes(60)}),
        )
        .await;
        common::assert_answer(&refused, StatusCode::UNPROCESSABLE_ENTITY, who);
        common::assert_error(&refused, MEDIA_NOT_FOUND, who);
    }

    assert_eq!(
        scheduled_count(&pool).await,
        0,
        "not one media refusal stored a schedule"
    );
    let untouched = media_attachment::get_by_id(&pool, attached.id)
        .await
        .expect("the attachment reads")
        .expect("the fixture attachment is stored");
    assert_eq!(
        untouched.status_id,
        Some(posted.id),
        "and the probes left its claim exactly as it was"
    );
}

/// An upload one pending schedule reserves cannot be scheduled again
/// while that row stands — every re-claim is 422 `Media not found` — and
/// the reservation never touches the attachment's row: the schedule's
/// entity answers the uploads in the order the body submitted them, the
/// stored params keep that order, and each row's `status_id` stays null
/// until the publish fires.
#[tokio::test]
async fn a_pending_schedule_reserves_its_media_without_writing_the_rows() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = writer(&pool, "alice").await;
    let app = common::test_app(pool.clone());

    // Both uploads are alice's, created in this order; the body submits
    // them in the other one, and both answer and stored params must keep
    // the submitted order (the batch read answers id order).
    let older = upload(&pool, &alice).await;
    let newer = upload(&pool, &alice).await;

    let response = schedule(
        &app,
        &token,
        json!({
            "status": "pics tomorrow",
            "media_ids": [newer.id.to_string(), older.id.to_string()],
            "scheduled_at": in_minutes(60),
        }),
    )
    .await;
    common::assert_answer(&response, StatusCode::OK, "a schedule claiming two uploads");
    let entity = response.json_value();
    assert_eq!(
        entity["params"]["media_ids"],
        json!([newer.id.to_string(), older.id.to_string()]),
        "the params keep the submitted order"
    );
    let attachments = entity["media_attachments"]
        .as_array()
        .expect("media_attachments is an array");
    assert_eq!(attachments.len(), 2, "both uploads are entities");
    assert_eq!(
        attachments[0]["id"],
        newer.id.to_string(),
        "the body's first id answers first"
    );
    assert_eq!(
        attachments[1]["id"],
        older.id.to_string(),
        "and the second follows it, not the id order"
    );

    // Re-claiming either upload — alone or beside the other — is the same
    // 422 while the first schedule is pending.
    for ids in [
        vec![newer.id.to_string()],
        vec![older.id.to_string()],
        vec![newer.id.to_string(), older.id.to_string()],
    ] {
        let refused = schedule(
            &app,
            &token,
            json!({"status": "again", "media_ids": ids, "scheduled_at": in_minutes(120)}),
        )
        .await;
        common::assert_answer(
            &refused,
            StatusCode::UNPROCESSABLE_ENTITY,
            "a re-claimed upload",
        );
        common::assert_error(&refused, MEDIA_NOT_FOUND, "the reservation's refusal");
    }

    for id in [older.id, newer.id] {
        let row = media_attachment::get_by_id(&pool, id)
            .await
            .expect("the attachment reads")
            .expect("the fixture upload is stored");
        assert_eq!(row.status_id, None, "the schedule wrote no claim");
    }
    assert_eq!(
        scheduled_count(&pool).await,
        1,
        "one schedule stands; every re-claim stored nothing"
    );
    assert_eq!(post_count(&pool).await, 0, "and nothing posted");
}
