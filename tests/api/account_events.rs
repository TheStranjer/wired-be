//! Tests for the account events that moderation and account-state changes
//! fire: suspend, unsuspend, deactivate and delete.
//!
//! Each path runs against the fixture protocol's recorder. With
//! `activitypub,fixture` the `ActivityPub` side effects stay as they were;
//! with only `fixture` no `ActivityPub` job is queued.

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::json;
use sqlx::PgPool;
use wired_be::{
    config::{
        BASE_URL_VAR, Config, DATABASE_URL_VAR, PROTOCOLS_VAR, TEST_BASE_URL, TEST_DATABASE_URL,
    },
    jobs::{AttemptInfo, deletion},
    models::{follow, user::User},
    protocols::{
        events::AccountEvent,
        fixture::{Recorded, recorded_for},
    },
    state::AppState,
};

use crate::common;

/// Both protocols: the fixture records, `ActivityPub` federates as before.
const BOTH: &str = "activitypub,fixture";
/// The fixture alone: no `ActivityPub` code runs.
const FIXTURE_ONLY: &str = "fixture";
/// The password every fixture account signs in with.
const PASSWORD: &str = "correct horse battery staple";
/// The Pleroma admin console's account collection.
const ADMIN_USERS: &str = "/api/v1/pleroma/admin/users";
/// The Mastodon admin API's account collection.
const ADMIN_ACCOUNTS: &str = "/api/v1/admin/accounts";

/// The test configuration with `PROTOCOLS` set to `protocols`.
///
/// # Panics
///
/// If `protocols` is not a valid `PROTOCOLS` value; the tests pass fixed ones.
fn config_with_protocols(protocols: &str) -> Config {
    Config::from_lookup(|key| match key {
        DATABASE_URL_VAR => Some(TEST_DATABASE_URL.to_owned()),
        BASE_URL_VAR => Some(TEST_BASE_URL.to_owned()),
        PROTOCOLS_VAR => Some(protocols.to_owned()),
        _ => None,
    })
    .expect("the fixed account events test config is valid")
}

/// A state and the router built on it, both running `protocols`.
fn instance(pool: &PgPool, protocols: &str) -> (AppState, Router) {
    let state = common::test_app_state(pool.clone(), config_with_protocols(protocols));
    let app = wired_be::app(state.clone());
    (state, app)
}

/// An `admin:write:accounts` bearer token of a freshly planted admin.
async fn admin_token(pool: &PgPool) -> String {
    let root = common::create_admin_with_password(pool, "root", PASSWORD).await;
    common::create_app_and_token(pool, &root, "admin:write:accounts").await
}

/// The events the fixture recorded for `user`.
fn events_of(user: &User) -> Vec<Recorded> {
    recorded_for(user.id)
}

/// How many `ActivityPub` publish or delivery jobs are queued.
async fn activitypub_jobs(pool: &PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM jobs WHERE kind IN ('federator.publish', 'federator.deliver')",
    )
    .fetch_one(pool)
    .await
    .expect("the job count reads")
}

/// Gives `account` an accepted remote follower, so its `Delete` has an inbox to reach.
async fn remote_follower_of(pool: &PgPool, account: &User) {
    let fan = common::create_remote_user(pool, "fan", "peer.example").await;
    let requested = follow::follow(pool, TEST_BASE_URL, &fan, account)
        .await
        .expect("the follow is recorded");
    let _granted = follow::accept(pool, requested.id)
        .await
        .expect("the follow is accepted");
}

/// Runs the `users.delete` sweep queued for `account`, as the worker would.
async fn run_sweep(state: &AppState, account: &User) {
    let attempt = AttemptInfo {
        attempts: 1,
        max_attempts: 5,
    };
    deletion::handle_delete(state.clone(), attempt, deletion::job_args(account.id))
        .await
        .expect("the sweep runs");
}

/// `POST`s the password to an owner endpoint as `token`, asserting a 200.
async fn owner_post(app: &Router, path: &str, token: &str) {
    let response = common::TestRequest::post(path)
        .bearer(token)
        .json(&json!({"password": PASSWORD}))
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the owner endpoint");
    assert_eq!(
        response.json_value(),
        json!({"status": "success"}),
        "the documented success body"
    );
}

/// Bulk-deactivates or activates `nicknames` as `token`, asserting a 200.
async fn bulk_activation(app: &Router, token: &str, verb: &str, nicknames: &[&str]) {
    let response = common::TestRequest::patch(&format!("{ADMIN_USERS}/{verb}"))
        .bearer(token)
        .json(&json!({"nicknames": nicknames}))
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the bulk activation");
}

/// The owner's disable fires `Deactivated`, and `ActivityPub` announces it as one actor `Update`.
#[tokio::test]
async fn disable_account_records_deactivated_and_federates_the_suspended_update() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_user_with_password(&pool, "alice", PASSWORD).await;
    remote_follower_of(&pool, &alice).await;
    let token = common::create_app_and_token(&pool, &alice, "write:accounts").await;
    let (_state, app) = instance(&pool, BOTH);

    owner_post(&app, "/api/pleroma/disable_account", &token).await;

    assert_eq!(
        events_of(&alice),
        vec![Recorded::Event(AccountEvent::Deactivated)],
        "the fixture saw one Deactivated"
    );
    assert_eq!(activitypub_jobs(&pool).await, 1, "one Update was queued");
    let activity = sqlx::query_scalar::<_, serde_json::Value>(
        "SELECT args->'activity' FROM jobs WHERE kind = 'federator.publish'",
    )
    .fetch_one(&pool)
    .await
    .expect("the publish job reads");
    assert_eq!(activity["type"], "Update", "an Update");
    assert_eq!(
        activity["object"]["suspended"], true,
        "of the suspended actor, though the request held the old row"
    );
}

/// With the fixture alone no `ActivityPub` code runs, so a disable queues no job.
#[tokio::test]
async fn disable_account_federates_nothing_without_activitypub() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_user_with_password(&pool, "alice", PASSWORD).await;
    remote_follower_of(&pool, &alice).await;
    let token = common::create_app_and_token(&pool, &alice, "write:accounts").await;
    let (_state, app) = instance(&pool, FIXTURE_ONLY);

    owner_post(&app, "/api/pleroma/disable_account", &token).await;

    assert_eq!(activitypub_jobs(&pool).await, 0, "nothing was federated");
}

/// The owner's deletion fires `Deleted` from the sweep, and `ActivityPub` still federates the `Delete`.
#[tokio::test]
async fn delete_account_records_deleted_and_federates_the_delete() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_user_with_password(&pool, "alice", PASSWORD).await;
    remote_follower_of(&pool, &alice).await;
    let token = common::create_app_and_token(&pool, &alice, "write:accounts").await;
    let (state, app) = instance(&pool, BOTH);

    owner_post(&app, "/api/pleroma/delete_account", &token).await;
    assert_eq!(
        events_of(&alice),
        Vec::new(),
        "the endpoint only queues the sweep, which fires the event"
    );
    run_sweep(&state, &alice).await;

    assert_eq!(
        events_of(&alice),
        vec![Recorded::Event(AccountEvent::Deleted)],
        "the fixture saw one Deleted"
    );
    assert!(
        activitypub_jobs(&pool).await > 0,
        "ActivityPub queued its Delete"
    );
}

/// With only the fixture enabled a deletion records `Deleted` and queues no `ActivityPub` delivery.
#[tokio::test]
async fn delete_account_without_activitypub_records_the_event_only() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_user_with_password(&pool, "alice", PASSWORD).await;
    remote_follower_of(&pool, &alice).await;
    let token = common::create_app_and_token(&pool, &alice, "write:accounts").await;
    let (state, app) = instance(&pool, FIXTURE_ONLY);

    owner_post(&app, "/api/pleroma/delete_account", &token).await;
    run_sweep(&state, &alice).await;

    assert_eq!(
        events_of(&alice),
        vec![Recorded::Event(AccountEvent::Deleted)],
        "the fixture saw one Deleted"
    );
    assert_eq!(
        activitypub_jobs(&pool).await,
        0,
        "no ActivityPub job of any kind"
    );
}

/// The Pleroma console's `DELETE /users` fires `Deleted` for each account it deletes.
#[tokio::test]
async fn the_pleroma_console_delete_records_deleted() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    remote_follower_of(&pool, &alice).await;
    let token = admin_token(&pool).await;
    let (state, app) = instance(&pool, BOTH);

    let response = common::TestRequest::delete(ADMIN_USERS)
        .bearer(&token)
        .json(&json!({"nicknames": ["alice"]}))
        .send(&app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the console delete");
    run_sweep(&state, &alice).await;

    assert_eq!(
        events_of(&alice),
        vec![Recorded::Event(AccountEvent::Deleted)],
        "the fixture saw one Deleted"
    );
    assert!(
        activitypub_jobs(&pool).await > 0,
        "ActivityPub queued its Delete"
    );
}

/// The Mastodon admin `DELETE /accounts/{id}` fires `Deleted`.
#[tokio::test]
async fn the_mastodon_admin_delete_records_deleted() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let token = admin_token(&pool).await;
    let (state, app) = instance(&pool, FIXTURE_ONLY);

    let response = common::TestRequest::delete(&format!("{ADMIN_ACCOUNTS}/{}", alice.id))
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the admin delete");
    run_sweep(&state, &alice).await;

    assert_eq!(
        events_of(&alice),
        vec![Recorded::Event(AccountEvent::Deleted)],
        "the fixture saw one Deleted"
    );
}

/// A bulk deactivation of two accounts fires `Suspended` for both and federates nothing.
#[tokio::test]
async fn bulk_deactivate_records_suspended_for_each_account() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let token = admin_token(&pool).await;
    let (_state, app) = instance(&pool, BOTH);

    bulk_activation(&app, &token, "deactivate", &["alice", "bob"]).await;

    for account in [&alice, &bob] {
        assert_eq!(
            events_of(account),
            vec![Recorded::Event(AccountEvent::Suspended)],
            "{} was suspended once",
            account.nickname
        );
    }
    assert_eq!(activitypub_jobs(&pool).await, 0, "nothing was federated");
}

/// A bulk activation fires `Unsuspended` for the account it moved.
#[tokio::test]
async fn bulk_activate_records_unsuspended() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let sleeper = common::create_deactivated_user_with_password(&pool, "sleeper", PASSWORD).await;
    let token = admin_token(&pool).await;
    let (_state, app) = instance(&pool, BOTH);

    bulk_activation(&app, &token, "activate", &["sleeper"]).await;

    assert_eq!(
        events_of(&sleeper),
        vec![Recorded::Event(AccountEvent::Unsuspended)],
        "the fixture saw one Unsuspended"
    );
}

/// The caller's own account and an unknown nickname are skipped, so neither fires.
#[tokio::test]
async fn bulk_deactivate_skips_the_callers_own_account() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let root = common::create_admin_with_password(&pool, "root", PASSWORD).await;
    let token = common::create_app_and_token(&pool, &root, "admin:write:accounts").await;
    let (_state, app) = instance(&pool, FIXTURE_ONLY);

    bulk_activation(&app, &token, "deactivate", &["root", "ghost"]).await;

    assert_eq!(events_of(&root), Vec::new(), "the caller was not moved");
}

/// `toggle_activation` fires `Suspended` on an active account, then `Unsuspended`.
#[tokio::test]
async fn toggle_activation_records_each_direction() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let token = admin_token(&pool).await;
    let (_state, app) = instance(&pool, BOTH);

    for _flip in 0..2 {
        let response =
            common::TestRequest::patch(&format!("{ADMIN_USERS}/alice/toggle_activation"))
                .bearer(&token)
                .send(&app)
                .await;
        common::assert_answer(&response, StatusCode::OK, "the toggle");
    }

    assert_eq!(
        events_of(&alice),
        vec![
            Recorded::Event(AccountEvent::Suspended),
            Recorded::Event(AccountEvent::Unsuspended),
        ],
        "the fixture saw the suspension, then the lifting"
    );
}

/// The Mastodon admin `action` (suspend) and `enable` fire `Suspended` and `Unsuspended`.
#[tokio::test]
async fn the_mastodon_admin_action_and_enable_record_both_events() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let token = admin_token(&pool).await;
    let (_state, app) = instance(&pool, BOTH);

    let suspend = common::TestRequest::post(&format!("{ADMIN_ACCOUNTS}/{}/action", alice.id))
        .bearer(&token)
        .json(&json!({"type": "suspend"}))
        .send(&app)
        .await;
    common::assert_answer(&suspend, StatusCode::OK, "the suspend action");
    let enable = common::TestRequest::post(&format!("{ADMIN_ACCOUNTS}/{}/enable", alice.id))
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&enable, StatusCode::OK, "the enable");

    assert_eq!(
        events_of(&alice),
        vec![
            Recorded::Event(AccountEvent::Suspended),
            Recorded::Event(AccountEvent::Unsuspended),
        ],
        "the fixture saw the suspension, then the lifting"
    );
}
