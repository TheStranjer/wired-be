//! Forwarding: a filing about a remote account queues the flag, and the
//! path-keyed flag route does the same by default.

use axum::http::StatusCode;
use serde_json::json;
use uuid::Uuid;
use wired_be::{
    config::TEST_BASE_URL, models::status::Visibility,
    services::instance_actor::instance_actor_ap_id,
};

use super::support::{about, filed, filings, fixture, flag, publish_jobs, reporter, stored};
use crate::common;

/// The forwarding half: a `forward = true` filing about a REMOTE account
/// queues the instance's `Flag` — from the instance actor, the target
/// first in `object` and the only address, the comment as content — and
/// nothing else rides along.
#[tokio::test]
async fn a_forwarded_filing_about_a_remote_account_queues_the_flag() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = reporter(&pool, "alice").await;
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let cited = common::create_status(&pool, &bob, "flag me", Visibility::Public).await;

    let value = filed(
        &app,
        &token,
        json!({
            "account_id": bob.id.to_string(),
            "status_ids": [cited.id.to_string()],
            "comment": "they will not stop",
            "forward": true,
        }),
    )
    .await;
    let row = stored(&pool, &value).await;

    let jobs = publish_jobs(&pool).await;
    assert_eq!(jobs.len(), 1, "the forward is announced once");
    let activity = &jobs[0]["activity"];
    assert_eq!(activity["type"], "Flag");
    assert_eq!(
        activity["id"],
        row.ap_id.expect("a local filing mints its Flag id"),
        "the document is the report row's own activity"
    );
    assert_eq!(
        activity["actor"],
        instance_actor_ap_id(TEST_BASE_URL),
        "the instance speaks for its reporter"
    );
    assert_eq!(
        activity["object"],
        json!([bob.ap_id, cited.uri]),
        "the reported account leads the cited statuses"
    );
    assert_eq!(activity["content"], "they will not stop", "the comment");
    assert_eq!(
        activity["to"],
        json!([bob.ap_id]),
        "the target is the only address"
    );
    assert_eq!(activity["cc"], json!([]), "and nobody is copied");
}

/// The arms that federate nothing: a forwarded filing whose target is a
/// LOCAL account, and an unforwarded filing about a remote one, queue no
/// `Flag` — the report stays with our own moderators.
#[tokio::test]
async fn filings_that_must_not_forward_queue_no_flag() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = reporter(&pool, "alice").await;
    let neighbour = common::create_local_user(&pool, "neighbour").await;
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;

    let _local = filed(
        &app,
        &token,
        json!({"account_id": neighbour.id.to_string(), "forward": true}),
    )
    .await;
    let _quiet = filed(&app, &token, about(&bob)).await;

    assert_eq!(filings(&pool).await.len(), 2, "both reports are stored");
    assert!(
        publish_jobs(&pool).await.is_empty(),
        "neither filing leaves the instance"
    );
}

/// A filing through the path-keyed route files the report and defaults to
/// forwarding: a REMOTE target's flag queues the `Flag` (instance actor,
/// target alone) even with no `forward` in the body, and the entity reads
/// `forwarded: true`.
#[tokio::test]
async fn a_flag_files_the_report_and_forwards_it_by_default() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = reporter(&pool, "alice").await;
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;

    let response = flag(
        &app,
        &token,
        &bob.id.to_string(),
        json!({"comment": "path flag"}),
    )
    .await;
    common::assert_answer(&response, StatusCode::OK, "the flag files");
    let value = response.json_value();
    assert_eq!(
        value["forwarded"],
        json!(true),
        "the flag route forwards by default"
    );
    assert_eq!(value["comment"], json!("path flag"), "the comment verbatim");
    assert_eq!(
        value["category"],
        json!("other"),
        "the shared category default"
    );

    let jobs = publish_jobs(&pool).await;
    assert_eq!(jobs.len(), 1, "the forward is announced once");
    assert_eq!(jobs[0]["activity"]["type"], "Flag");
    assert_eq!(jobs[0]["activity"]["content"], "path flag");
    assert_eq!(
        jobs[0]["activity"]["actor"],
        instance_actor_ap_id(TEST_BASE_URL),
        "the instance speaks for its reporter"
    );
    assert_eq!(
        jobs[0]["activity"]["to"],
        json!([bob.ap_id]),
        "the target alone"
    );
}

/// A flag that explicitly refuses forwarding keeps the report local, and a
/// flag on a LOCAL account is silent whatever the default: the filing
/// stands, nothing queues.
#[tokio::test]
async fn flags_that_stay_home_queue_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = reporter(&pool, "alice").await;
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let neighbour = common::create_local_user(&pool, "neighbour").await;

    let unforwarded = flag(&app, &token, &bob.id.to_string(), json!({"forward": false})).await;
    common::assert_answer(&unforwarded, StatusCode::OK, "the opt-out files");
    assert_eq!(
        unforwarded.json_value()["forwarded"],
        json!(false),
        "and records the opt-out"
    );
    let _local = flag(&app, &token, &neighbour.id.to_string(), json!({})).await;

    assert_eq!(filings(&pool).await.len(), 2, "both reports are stored");
    assert!(
        publish_jobs(&pool).await.is_empty(),
        "neither an opted-out flag nor a local target leaves the instance"
    );
}

/// The path route shares the collection route's gates: an unknown target is
/// the 404, a self-flag the 422, a bad category the 422, a missing token
/// the 401, and the write needs `write:reports`.
#[tokio::test]
async fn the_flag_route_enforces_the_same_gates() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = reporter(&pool, "alice").await;
    let (_other, reader) = fixture(&pool, "reader", "read:reports").await;
    let missing = Uuid::now_v7().to_string();

    let unknown = flag(&app, &token, &missing, json!({})).await;
    common::assert_answer(
        &unknown,
        StatusCode::NOT_FOUND,
        "an unknown target is a 404",
    );
    let self_flag = flag(&app, &token, &alice.id.to_string(), json!({})).await;
    common::assert_answer(
        &self_flag,
        StatusCode::UNPROCESSABLE_ENTITY,
        "no self-flags",
    );
    common::assert_error(&self_flag, "Can not report yourself", "a self-flag");
    let bad_category = flag(
        &app,
        &token,
        &alice.id.to_string(),
        json!({"category": "vibes"}),
    )
    .await;
    common::assert_answer(
        &bad_category,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a bad category",
    );
    common::assert_error(
        &bad_category,
        "Validation failed: Category is not included in the list",
        "a bad category",
    );

    let no_scope = flag(&app, &reader, &alice.id.to_string(), json!({})).await;
    common::assert_answer(&no_scope, StatusCode::FORBIDDEN, "read:reports cannot file");
}

/// A `flag` under a malformed path id is the shared 404, not a parse panic.
#[tokio::test]
async fn a_malformed_flag_id_is_the_404() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = reporter(&pool, "alice").await;

    let response = flag(&app, &token, "not-a-uuid", json!({})).await;
    common::assert_answer(&response, StatusCode::NOT_FOUND, "a malformed id is a 404");
}
