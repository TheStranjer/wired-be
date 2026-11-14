//! Pleroma's federation status over HTTP
//! (`GET /api/v1/pleroma/federation_status`, WBE-164).
//!
//! The endpoint serves the unreachable-host ledger of
//! [`wired_be::models::instance`] as exactly
//! `{"unreachable": {"<host>": "<ISO 8601 since>", ...}}`: an empty object
//! while nothing is marked, one entry per marked host with the mark
//! rendered Mastodon-style (decision 5), and no entry for hosts merely seen
//! or recovered. Nothing authenticates — Pleroma routes the action behind
//! no OAuth plug — so every caller gets 200, token or not; only a database
//! failure breaks that answer, and then it is the project `AppError` JSON
//! (`src/error.rs`, decision 8).

#![cfg(test)]

use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use serde_json::json;
use sqlx::PgPool;
use wired_be::{models::instance, util::time::parse_time};

use crate::common;

/// The one route this file requests.
const PATH: &str = "/api/v1/pleroma/federation_status";
/// The 500 body of `AppError::Database` (`src/error.rs`, decision 8).
const INTERNAL_ERROR: &str = "Something went wrong";

/// A fixture timestamp, parsed from its Mastodon wire spelling so each
/// test can name the exact string the endpoint must answer with.
///
/// # Panics
///
/// Never: every literal passed here is a valid timestamp.
fn since(text: &str) -> DateTime<Utc> {
    parse_time(text).expect("the fixture instant is a timestamp")
}

/// Marks `host` unreachable as of `marked_at` through the model the
/// endpoint reads.
///
/// # Panics
///
/// If the write fails.
async fn mark(pool: &PgPool, host: &str, marked_at: DateTime<Utc>) {
    instance::set_unreachable(pool, host, marked_at)
        .await
        .expect("the unreachable mark is written");
}

/// `GET` the endpoint with no `Authorization` header and no body.
async fn anonymous_get(app: &axum::Router) -> common::TestResponse {
    common::TestRequest::get(PATH).send(app).await
}

/// With nothing marked, the endpoint answers 200 with exactly
/// `{"unreachable":{}}`.
#[tokio::test]
async fn an_empty_ledger_answers_the_empty_object() {
    let db = common::test_db().await;
    let app = common::test_app(db.pool().clone());
    let response = anonymous_get(&app).await;
    common::assert_answer(&response, StatusCode::OK, "an empty ledger");
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "the answer is JSON"
    );
    assert_eq!(
        response.json_value(),
        json!({"unreachable": {}}),
        "the whole body is the empty object under `unreachable`: {}",
        response.text()
    );
}

/// After two hosts are marked through [`instance::set_unreachable`], the
/// JSON maps each host to its since timestamp, Mastodon-style with
/// millisecond precision and a `Z`.
#[tokio::test]
async fn marked_hosts_answer_with_their_since_timestamps() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    mark(&pool, "dead.example", since("2025-06-01T10:00:00Z")).await;
    mark(&pool, "gone.example", since("2025-06-02T23:59:59.456Z")).await;
    let app = common::test_app(pool);
    let response = anonymous_get(&app).await;
    common::assert_answer(&response, StatusCode::OK, "a marked ledger");
    assert_eq!(
        response.json_value(),
        json!({
            "unreachable": {
                "dead.example": "2025-06-01T10:00:00.000Z",
                "gone.example": "2025-06-02T23:59:59.456Z",
            }
        }),
        "both hosts map to their marks, whole seconds padded to .000: {}",
        response.text()
    );
}

/// The endpoint demands nothing of its caller: without any bearer token it
/// answers 200 with the ledger, and a presented-but-invalid token changes
/// nothing because no auth extractor guards the route.
#[tokio::test]
async fn any_caller_gets_200_without_a_bearer_token() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let expected = json!({
        "unreachable": {"dark.example": "2025-07-04T05:06:07.008Z"}
    });
    mark(&pool, "dark.example", since("2025-07-04T05:06:07.008Z")).await;
    let app = common::test_app(pool);
    let anonymous = anonymous_get(&app).await;
    common::assert_answer(&anonymous, StatusCode::OK, "no token at all");
    assert_eq!(
        anonymous.json_value(),
        expected,
        "the anonymous read of the ledger: {}",
        anonymous.text()
    );
    let invalid = common::TestRequest::get(PATH)
        .bearer("bogus-token")
        .send(&app)
        .await;
    common::assert_answer(&invalid, StatusCode::OK, "a bogus token");
    assert_eq!(
        invalid.json_value(),
        expected,
        "the token is not even looked at: {}",
        invalid.text()
    );
}

/// With the pool closed underneath the router, the ledger read fails and
/// the endpoint answers the project `AppError` JSON: 500 with
/// `{"error": "Something went wrong"}`.
#[tokio::test]
async fn a_database_failure_answers_the_project_error_json() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    pool.close().await;
    let response = anonymous_get(&app).await;
    common::assert_answer(
        &response,
        StatusCode::INTERNAL_SERVER_ERROR,
        "a closed pool",
    );
    common::assert_error(&response, INTERNAL_ERROR, "a closed pool");
}
