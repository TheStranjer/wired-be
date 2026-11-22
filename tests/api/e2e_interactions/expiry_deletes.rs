//! Scenarios 9-10: an expiring status leaving every surface when its job
//! drains, and a delete with its guards and the boost that falls with it.

use axum::{Router, http::StatusCode};
use serde_json::json;
use sqlx::PgPool;
use wired_be::jobs::statuses::KIND_EXPIRE;

use super::support::{
    ACCOUNTS, BAD_TOKEN, HOME, NO_TOKEN, NO_WRITE, NOT_FOUND, PUBLIC, SCOPES, STATUSES, UNKNOWN_ID,
    USER_REQUIRED, acted, actor, app_and_state, assert_gone, created, entity_id, follows, get,
    ids_of, post, user,
};
use crate::common;

// ──────────────────────────  Scenario (9): expire  ──────────────────────────

/// Moves the queued [`KIND_EXPIRE`] fires an hour into the past, so the
/// next claim round takes them — the ticket's exact `UPDATE`, rows only.
///
/// # Panics
///
/// If the update hits anything but exactly one row.
async fn backdate_expire_jobs(pool: &PgPool) {
    let hit = sqlx::query("UPDATE jobs SET run_at = now() - interval '1 minute' WHERE kind = $1")
        .bind(KIND_EXPIRE)
        .execute(pool)
        .await
        .expect("the backdate update runs");
    assert_eq!(
        hit.rows_affected(),
        1,
        "the expiring post queued exactly one fire"
    );
}

/// The states of the [`KIND_EXPIRE`] job rows, in insertion order.
async fn expire_job_states(pool: &PgPool) -> Vec<String> {
    sqlx::query_scalar::<_, String>("SELECT state FROM jobs WHERE kind = $1 ORDER BY id")
        .bind(KIND_EXPIRE)
        .fetch_all(pool)
        .await
        .expect("the job rows read")
}

/// The `statuses_count` the account entity answers for `account`.
async fn statuses_count(app: &Router, account: &str) -> i64 {
    let found = get(app, None, &format!("{ACCOUNTS}/{account}"))
        .await
        .json_value();
    found["statuses_count"]
        .as_i64()
        .expect("the account entity counts its statuses")
}

/// A post carrying `expires_in` renders its expiry, rides every surface
/// until its [`KIND_EXPIRE`] fire drains through the queue harness, and
/// then is gone: the shared 404 for author and stranger alike, off the
/// public timeline and every home that carried it — while its unexpired
/// neighbour stays put.
#[tokio::test]
async fn an_expiring_status_leaves_every_surface_when_its_fire_drains() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = app_and_state(pool.clone());
    let (alice, alice_token) = user(&pool, "alice").await;
    let (_bob, bob_token) = user(&pool, "bob").await;
    follows(&app, &bob_token, &alice.id.to_string()).await;

    let doomed = created(
        &app,
        &alice_token,
        json!({"status": "here today", "expires_in": 3600}),
    )
    .await;
    let doomed_id = entity_id(&doomed);
    assert!(
        doomed["pleroma"]["expires_at"].is_string(),
        "the answer states the expiry: {doomed}"
    );
    let keeper = entity_id(&post(&app, &alice_token, "and stays").await);

    let riding = get(&app, None, PUBLIC).await.json_value();
    assert_eq!(
        ids_of(&riding),
        vec![keeper.clone(), doomed_id.clone()],
        "both posts ride the public timeline"
    );

    backdate_expire_jobs(&pool).await;
    assert_eq!(
        common::run_all_jobs(&state).await,
        6,
        "the drain ran both creates' card fetches and note announcements, \
         the backdated fire, and the expired post's Delete announcement"
    );
    assert_eq!(
        expire_job_states(&pool).await,
        vec!["completed".to_owned()],
        "the fire reached its terminal state"
    );

    assert_gone(&app, Some(&alice_token), &doomed_id, "the expired status").await;
    assert_gone(&app, None, &doomed_id, "the expired status, anonymously").await;
    let public = get(&app, None, PUBLIC).await.json_value();
    assert_eq!(
        ids_of(&public),
        vec![keeper.clone()],
        "the expired post is off the public timeline"
    );
    for viewer in [&alice_token, &bob_token] {
        let home = get(&app, Some(viewer.as_str()), HOME).await.json_value();
        assert_eq!(
            ids_of(&home),
            vec![keeper.clone()],
            "and off every home that carried it"
        );
    }
    assert_eq!(
        statuses_count(&app, &alice.id.to_string()).await,
        1,
        "the author stops counting the expired post"
    );
    common::assert_answer(
        &get(&app, None, &format!("{STATUSES}/{keeper}")).await,
        StatusCode::OK,
        "the unexpired neighbour stands",
    );
}

// ─────────────────────────  Scenario (10): delete  ─────────────────────────

/// The delete route's refusal stack on status `id`: no token and a dead
/// token are the 401 arms, an app-only credential and a scope-less user
/// token are the 403 arms, a stranger's 403 keeps the `"Record not
/// found"` body, and an id that names nothing is the shared 404.
async fn assert_delete_guards(
    app: &Router,
    id: &str,
    stranger: &str,
    reader: &str,
    app_only: &str,
) {
    let path = format!("{STATUSES}/{id}");
    let anonymous = common::TestRequest::delete(&path).send(app).await;
    common::assert_answer(
        &anonymous,
        StatusCode::UNAUTHORIZED,
        "a delete with no token",
    );
    common::assert_error(&anonymous, NO_TOKEN, "the delete with no token");

    let ghost = common::TestRequest::delete(&path)
        .bearer("tok3n-that-exists-not")
        .send(app)
        .await;
    common::assert_answer(
        &ghost,
        StatusCode::UNAUTHORIZED,
        "a delete with a dead token",
    );
    common::assert_error(&ghost, BAD_TOKEN, "the delete with a dead token");

    let client = common::TestRequest::delete(&path)
        .bearer(app_only)
        .send(app)
        .await;
    common::assert_answer(&client, StatusCode::FORBIDDEN, "an app-only delete");
    common::assert_error(&client, USER_REQUIRED, "the app-only delete");

    let scopeless = common::TestRequest::delete(&path)
        .bearer(reader)
        .send(app)
        .await;
    common::assert_answer(&scopeless, StatusCode::FORBIDDEN, "a scope-less delete");
    common::assert_error(&scopeless, NO_WRITE, "the token without write:statuses");

    let foreign = common::TestRequest::delete(&path)
        .bearer(stranger)
        .send(app)
        .await;
    common::assert_answer(&foreign, StatusCode::FORBIDDEN, "a stranger's delete");
    common::assert_error(
        &foreign,
        NOT_FOUND,
        "the ownership refusal keeps the shared body",
    );

    let missing = common::TestRequest::delete(&format!("{STATUSES}/{UNKNOWN_ID}"))
        .bearer(stranger)
        .send(app)
        .await;
    common::assert_answer(
        &missing,
        StatusCode::NOT_FOUND,
        "a delete of an id that names nothing",
    );
    common::assert_error(&missing, NOT_FOUND, "the gate's shared 404");
}

/// The delete guards answer before the write does; the author's own
/// delete then answers the gone status with its raw text, takes the
/// boost of it down as a tombstone nobody can read, clears the
/// timelines that carried both, and leaves every counter settled.
#[tokio::test]
async fn a_delete_guards_answers_and_takes_the_boost_with_it() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, alice_token) = user(&pool, "alice").await;
    let (bob, bob_token) = user(&pool, "bob").await;
    let (_carol, carol_token) = user(&pool, "carol").await;
    let reader = actor(&pool, "reader", "read").await.1;
    let app_only = common::create_app_only_token(&pool, SCOPES).await;
    let id = entity_id(&post(&app, &alice_token, "boost me then lose me").await);

    assert_delete_guards(&app, &id, &carol_token, &reader, &app_only).await;

    follows(&app, &carol_token, &bob.id.to_string()).await;
    let boost = entity_id(&acted(&app, &bob_token, &format!("{STATUSES}/{id}/reblog")).await);
    let riding = get(&app, Some(&carol_token), HOME).await.json_value();
    assert_eq!(
        ids_of(&riding),
        vec![boost.clone()],
        "the boost rides carol's home once"
    );

    let response = common::TestRequest::delete(&format!("{STATUSES}/{id}"))
        .bearer(&alice_token)
        .send(&app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the author's delete");
    let body = response.json_value();
    assert_eq!(body["id"], json!(id), "the answer is the status as it was");
    assert_eq!(
        body["text"],
        json!("boost me then lose me"),
        "with its raw source text beside it"
    );

    assert_gone(&app, Some(&alice_token), &id, "the deleted status").await;
    assert_gone(
        &app,
        Some(&bob_token),
        &boost,
        "the boost the delete took down",
    )
    .await;
    let public = get(&app, None, PUBLIC).await.json_value();
    assert!(
        ids_of(&public).is_empty(),
        "both left the public timeline: {public}"
    );
    let settled = get(&app, Some(&carol_token), HOME).await.json_value();
    assert!(
        ids_of(&settled).is_empty(),
        "and the follower's home with them"
    );
    assert_eq!(
        statuses_count(&app, &alice.id.to_string()).await,
        0,
        "the author counts nothing"
    );
    assert_eq!(
        statuses_count(&app, &bob.id.to_string()).await,
        0,
        "and neither does the booster"
    );
}
