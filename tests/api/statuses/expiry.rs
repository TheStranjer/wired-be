//! WBE-90's `expires_in`: the instant it counts out lands on the row and
//! renders as `pleroma.expires_at`, scheduled as exactly one
//! `statuses.expire` job due at that instant; an under-floor value is 422
//! naming the floor, a non-integer is the extractor's own 422, and the
//! floor itself is whatever the configuration carries.

use axum::http::StatusCode;
use chrono::Utc;
use serde_json::{Value, json};
use sqlx::PgPool;

use super::support::{
    STATUSES, WRITE, created_row, posted, publish, stored_count, user_with_token,
};
use crate::common;

// ──────────────────  POST /api/v1/statuses: `expires_in`  ───────────────────

/// The job kind a post with an `expires_in` schedules for itself.
const EXPIRE_KIND: &str = "statuses.expire";

/// The `jobs` rows of `kind`, in insertion order.
async fn scheduled_jobs(pool: &PgPool, kind: &str) -> Vec<wired_be::models::job::Job> {
    sqlx::query_as::<_, wired_be::models::job::Job>(
        "SELECT * FROM jobs WHERE kind = $1 ORDER BY id",
    )
    .bind(kind)
    .fetch_all(pool)
    .await
    .expect("the job rows read")
}

/// A post carrying Pleroma's `expires_in` stores the instant that span
/// counts out, renders it as `pleroma.expires_at`, and schedules exactly one
/// `statuses.expire` job — on the scheduled queue, due at exactly the stored
/// instant, keyed `expire:<status_id>`. A form body's numeric string is the
/// same parameter; a nested `pleroma.expires_in` is no parameter at all.
#[tokio::test]
async fn a_post_with_expires_in_stores_the_expiry_and_schedules_its_delete() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());

    let value = posted(
        &app,
        &token,
        json!({"status": "here for an hour", "expires_in": 3600}),
    )
    .await;
    let row = created_row(&pool, &value).await;

    let expires = row.expires_at.expect("the row carries its expiry");
    let span = expires - Utc::now();
    assert!(
        (span.num_seconds() - 3600).abs() <= 1,
        "the stored expiry is the hour the body asked for, not {span}"
    );
    assert_eq!(
        value["pleroma"]["expires_at"],
        wired_be::util::time::mastodon_time(&expires),
        "the entity renders the stored instant as `pleroma.expires_at`"
    );

    let jobs = scheduled_jobs(&pool, EXPIRE_KIND).await;
    assert_eq!(jobs.len(), 1, "one scheduled delete for the expiring post");
    let job = &jobs[0];
    assert_eq!(job.queue, "scheduled", "on the scheduled queue");
    assert_eq!(
        job.args,
        json!({"status_id": row.id.to_string()}),
        "the args name exactly the status id"
    );
    assert_eq!(job.run_at, expires, "due at exactly the stored expiry");
    assert_eq!(
        job.unique_key.as_deref(),
        Some(format!("expire:{}", row.id).as_str()),
        "keyed for deduplication as the ticket spells it"
    );

    // The form spelling is the same integer, and the nested key is nothing:
    // `expires_in` is a top-level parameter only.
    let from_form = common::TestRequest::post(STATUSES)
        .bearer(&token)
        .form(&[("status", "form expiry"), ("expires_in", "7200")])
        .send(&app)
        .await;
    common::assert_answer(&from_form, StatusCode::OK, "a form body's expires_in");
    assert!(
        created_row(&pool, &from_form.json_value())
            .await
            .expires_at
            .is_some(),
        "and it expires"
    );
    let nested = posted(
        &app,
        &token,
        json!({"status": "no nested expiry", "pleroma": {"expires_in": 3600}}),
    )
    .await;
    assert_eq!(
        created_row(&pool, &nested).await.expires_at,
        None,
        "`pleroma.expires_in` is not accepted — only the top-level key expires"
    );
    assert_eq!(
        scheduled_jobs(&pool, EXPIRE_KIND).await.len(),
        2,
        "and it scheduled no delete"
    );
}

/// An `expires_in` under the instance's floor — including zero, negatives
/// and the floor's own minute-ward neighbour — is 422 with the message
/// naming that floor, and writes nothing.
#[tokio::test]
async fn an_expires_in_under_the_floor_is_refused_with_the_floor_in_words() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());

    for short in [json!(-1), json!(0), json!(1), json!(599), json!("599")] {
        let refused = publish(
            &app,
            Some(&token),
            json!({"status": "too soon", "expires_in": short}),
        )
        .await;
        common::assert_answer(
            &refused,
            StatusCode::UNPROCESSABLE_ENTITY,
            "an expires_in under the floor",
        );
        common::assert_error(
            &refused,
            "expires_in must be at least 600 seconds",
            "the floor refusal",
        );
    }
    let refused = common::TestRequest::post(STATUSES)
        .bearer(&token)
        .form(&[("status", "too soon"), ("expires_in", "300")])
        .send(&app)
        .await;
    common::assert_error(
        &refused,
        "expires_in must be at least 600 seconds",
        "the floor refusal, from a form body too",
    );

    assert_eq!(stored_count(&pool).await, 0, "and none of it was written");
    assert!(
        scheduled_jobs(&pool, EXPIRE_KIND).await.is_empty(),
        "or scheduled"
    );
}

/// An `expires_in` that is no integer at all is a 422 of its own; `null`
/// is not an expiry but the ordinary never-expiring post.
#[tokio::test]
async fn an_expires_in_that_is_no_integer_is_refused() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());

    for not_a_number in [
        json!("soon"),
        json!(60.5),
        json!(true),
        json!([3600]),
        json!("10m"),
    ] {
        let refused = publish(
            &app,
            Some(&token),
            json!({"status": "not a number", "expires_in": not_a_number}),
        )
        .await;
        common::assert_answer(
            &refused,
            StatusCode::UNPROCESSABLE_ENTITY,
            "a non-integer expires_in",
        );
    }
    assert_eq!(stored_count(&pool).await, 0, "none of them posted");

    let unexpiry = posted(
        &app,
        &token,
        json!({"status": "null is no expiry", "expires_in": null}),
    )
    .await;
    assert_eq!(
        created_row(&pool, &unexpiry).await.expires_at,
        None,
        "null is as absent as no key"
    );
    assert_eq!(
        unexpiry["pleroma"]["expires_at"],
        Value::Null,
        "and renders null"
    );
}

/// The floor is the configured `MIN_EXPIRATION_SECS`: an instance that
/// lowered it to 120 refuses 119 in its words and posts 120.
#[tokio::test]
async fn the_floor_and_its_message_come_from_the_configuration() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let config = wired_be::config::Config {
        min_expiration_secs: 120,
        ..common::test_config()
    };
    let app = common::test_app_with_config(pool.clone(), config);

    let refused = publish(
        &app,
        Some(&token),
        json!({"status": "two minutes", "expires_in": 119}),
    )
    .await;
    common::assert_error(
        &refused,
        "expires_in must be at least 120 seconds",
        "a custom floor names itself",
    );

    let value = posted(
        &app,
        &token,
        json!({"status": "two minutes", "expires_in": 120}),
    )
    .await;
    assert!(
        created_row(&pool, &value).await.expires_at.is_some(),
        "the custom floor itself posts"
    );
}
