//! `Idempotency-Key` on the scheduling arm (WBE-669, SVC1-7): a retry that
//! carries the key of a schedule already parked answers that schedule rather
//! than parking a second one, while distinct keys, no key, and another
//! account's identical key each park their own.

use axum::{Router, http::StatusCode};
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{jobs::scheduled_statuses::KIND_PUBLISH, models::scheduled_status};

use super::support::{
    STATUSES, TOO_MANY, in_minutes, jobs_of_kind, post_count, scheduled_count, writer,
};
use crate::common;

/// `POST /api/v1/statuses` scheduling `text` an hour out, under `key` when
/// given, as `token`.
async fn keyed(app: &Router, token: &str, key: Option<&str>, text: &str) -> common::TestResponse {
    let request = common::TestRequest::post(STATUSES).bearer(token);
    let request = match key {
        Some(value) => request.header("idempotency-key", value),
        None => request,
    };
    request
        .json(&json!({"status": text, "scheduled_at": in_minutes(60)}))
        .send(app)
        .await
}

/// The id a scheduled create answered, after checking it answered 200.
fn answered_id(response: &common::TestResponse, who: &str) -> String {
    common::assert_answer(response, StatusCode::OK, who);
    let entity = response.json_value();
    entity["id"]
        .as_str()
        .expect("the entity carries its id")
        .to_owned()
}

/// The stored `idempotency_key` of schedule `id`.
async fn stored_key(pool: &PgPool, id: &str) -> Option<String> {
    let id = Uuid::parse_str(id).expect("the answered id is a uuid");
    scheduled_status::get_by_id(pool, id)
        .await
        .expect("the row reads")
        .expect("the row is stored")
        .idempotency_key
}

/// A replayed key answers the first schedule's id — even for a different
/// body — and stores nothing and queues no second fire.
#[tokio::test]
async fn a_replayed_key_answers_the_first_schedule() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = writer(&pool, "alice").await;
    let app = common::test_app(pool.clone());

    let first = keyed(&app, &token, Some("retry-1"), "first try").await;
    let first_id = answered_id(&first, "the first keyed schedule");
    let second = keyed(&app, &token, Some("retry-1"), "the retry").await;
    let second_id = answered_id(&second, "the replayed keyed schedule");

    assert_eq!(second_id, first_id, "the replay answers the first id");
    assert_eq!(
        second.json_value()["params"]["text"],
        "first try",
        "and the first request's params, not the retry's"
    );
    assert_eq!(scheduled_count(&pool).await, 1, "one row in the table");
    assert_eq!(
        stored_key(&pool, &first_id).await.as_deref(),
        Some("retry-1"),
        "the key is stored on the row"
    );
    assert_eq!(
        jobs_of_kind(&pool, KIND_PUBLISH).await.len(),
        1,
        "one fire for the one schedule"
    );
    assert_eq!(post_count(&pool).await, 0, "nothing was posted");
}

/// Different keys are different schedules.
#[tokio::test]
async fn different_keys_park_separate_schedules() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = writer(&pool, "alice").await;
    let app = common::test_app(pool.clone());

    let first = answered_id(&keyed(&app, &token, Some("k-a"), "same").await, "key a");
    let second = answered_id(&keyed(&app, &token, Some("k-b"), "same").await, "key b");

    assert_ne!(first, second, "two keys, two ids");
    assert_eq!(scheduled_count(&pool).await, 2, "two rows");
}

/// Without a key nothing deduplicates: identical retries park twice.
#[tokio::test]
async fn requests_without_a_key_park_separate_schedules() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = writer(&pool, "alice").await;
    let app = common::test_app(pool.clone());

    let first = answered_id(&keyed(&app, &token, None, "same").await, "first");
    let second = answered_id(&keyed(&app, &token, None, "same").await, "second");

    assert_ne!(first, second, "no key, no replay");
    assert_eq!(scheduled_count(&pool).await, 2, "two rows");
    assert_eq!(stored_key(&pool, &first).await, None, "no key stored");
}

/// A key belongs to its account: another account's identical key parks its
/// own schedule.
#[tokio::test]
async fn the_key_is_scoped_to_the_account() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, alice_token) = writer(&pool, "alice").await;
    let (_bob, bob_token) = writer(&pool, "bob").await;
    let app = common::test_app(pool.clone());

    let alice = answered_id(
        &keyed(&app, &alice_token, Some("shared"), "a").await,
        "alice",
    );
    let bob = answered_id(&keyed(&app, &bob_token, Some("shared"), "b").await, "bob");

    assert_ne!(alice, bob, "each account parks its own");
    assert_eq!(scheduled_count(&pool).await, 2, "two rows");
}

/// A replay is answered before the schedule's gates run, so it costs no
/// quota and is not refused by a cap the first request already used up.
#[tokio::test]
async fn a_replay_is_not_refused_by_the_total_cap() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = writer(&pool, "alice").await;
    let mut config = common::test_config();
    config.max_scheduled_per_user = 1;
    config.max_scheduled_per_user_per_day = 100;
    let app = common::test_app_with_config(pool.clone(), config);

    let first = answered_id(&keyed(&app, &token, Some("cap"), "one").await, "first");
    let replay = answered_id(&keyed(&app, &token, Some("cap"), "one").await, "replay");
    assert_eq!(replay, first, "the replay answers the first schedule");

    let other = keyed(&app, &token, Some("other"), "two").await;
    common::assert_answer(
        &other,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a new key at the cap",
    );
    common::assert_error(
        &other,
        TOO_MANY,
        "the cap still refuses a different schedule",
    );
    assert_eq!(scheduled_count(&pool).await, 1, "one row throughout");
}
