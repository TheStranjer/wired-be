//! Tests for the protocol adapters mounted by `app()`: which routes exist
//! under each `PROTOCOLS` value.
//!
//! The default (`ActivityPub` only) keeps the actor route; a `fixture`-only
//! instance serves `/fixture/ping` and nothing of `ActivityPub`; an
//! `ActivityPub`-only instance has no `/fixture/ping`. A route that is not
//! mounted gets the application fallback, marked `x-wired-fallback: 1`.

#![cfg(test)]

use axum::{Router, http::StatusCode};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::{
        BASE_URL_VAR, Config, DATABASE_URL_VAR, PROTOCOLS_VAR, TEST_BASE_URL, TEST_DATABASE_URL,
    },
    protocols::{
        events::AccountEvent,
        fixture::{Recorded, recorded_for},
    },
};

use crate::{common, remote};

/// The `Accept` value of an `ActivityPub` client.
const ACTIVITY_ACCEPT: &str = "application/activity+json";

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
    .expect("the fixed protocols test config is valid")
}

/// Asks `app` for `path` as an `ActivityPub` client.
async fn fetch_as_activitypub(app: &Router, path: &str) -> common::TestResponse {
    common::TestRequest::get(path)
        .header("accept", ACTIVITY_ACCEPT)
        .send(app)
        .await
}

/// With the default config a local user's actor still answers `ActivityPub` JSON.
#[tokio::test]
async fn the_default_config_serves_the_activitypub_actor() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let app = common::test_app(pool);

    let response = fetch_as_activitypub(&app, &format!("/users/{}", alice.nickname)).await;

    assert_eq!(response.status, StatusCode::OK, "the actor is served");
    assert_eq!(
        response.header("content-type"),
        Some("application/activity+json; charset=utf-8"),
        "as ActivityPub JSON"
    );
    assert_eq!(
        response.header("x-wired-fallback"),
        None,
        "by the route, not the fallback"
    );
}

/// The default config mounts no fixture route.
#[tokio::test]
async fn the_default_config_has_no_fixture_route() {
    let db = common::test_db().await;
    let app = common::test_app(db.pool().clone());

    let response = common::TestRequest::get("/fixture/ping").send(&app).await;

    assert_eq!(response.status, StatusCode::NOT_FOUND, "no fixture route");
    assert_eq!(
        response.header("x-wired-fallback"),
        Some("1"),
        "the fallback answers"
    );
}

/// With only `fixture` enabled the actor route is gone and `/fixture/ping` answers.
#[tokio::test]
async fn a_fixture_only_instance_serves_the_fixture_and_not_activitypub() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let app = common::test_app_with_config(pool, config_with_protocols("fixture"));

    let actor = fetch_as_activitypub(&app, &format!("/users/{}", alice.nickname)).await;
    assert_eq!(
        actor.status,
        StatusCode::NOT_FOUND,
        "the actor route is not mounted"
    );
    assert_eq!(
        actor.header("x-wired-fallback"),
        Some("1"),
        "the fallback answers the actor path"
    );

    let ping = common::TestRequest::get("/fixture/ping").send(&app).await;
    assert_eq!(ping.status, StatusCode::NO_CONTENT, "the fixture answers");
}

/// With only `activitypub` enabled `/fixture/ping` is the fallback 404.
#[tokio::test]
async fn an_activitypub_only_instance_has_no_fixture_route() {
    let db = common::test_db().await;
    let app = common::test_app_with_config(db.pool().clone(), config_with_protocols("activitypub"));

    let ping = common::TestRequest::get("/fixture/ping").send(&app).await;

    assert_eq!(ping.status, StatusCode::NOT_FOUND, "no fixture route");
    assert_eq!(
        ping.header("x-wired-fallback"),
        Some("1"),
        "the fallback answers"
    );
}

/// With both enabled, both route sets are mounted.
#[tokio::test]
async fn both_protocols_can_be_mounted_together() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let app = common::test_app_with_config(pool, config_with_protocols("fixture,activitypub"));

    let actor = fetch_as_activitypub(&app, &format!("/users/{}", alice.nickname)).await;
    assert_eq!(actor.status, StatusCode::OK, "the actor is served");

    let ping = common::TestRequest::get("/fixture/ping").send(&app).await;
    assert_eq!(ping.status, StatusCode::NO_CONTENT, "the fixture answers");
}

/// `/.well-known/fixture` is an owned path: served only while the fixture is enabled.
#[tokio::test]
async fn the_fixture_well_known_path_is_served_only_when_the_fixture_is_enabled() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let fixture_app = common::test_app_with_config(pool.clone(), config_with_protocols("fixture"));
    let activitypub_app = common::test_app_with_config(pool, config_with_protocols("activitypub"));

    let served = common::TestRequest::get("/.well-known/fixture")
        .send(&fixture_app)
        .await;
    assert_eq!(served.status, StatusCode::OK, "the fixture owns the path");
    assert_eq!(
        served.json_value(),
        serde_json::json!({ "protocol": "fixture" }),
        "it answers its small JSON object"
    );

    let absent = common::TestRequest::get("/.well-known/fixture")
        .send(&activitypub_app)
        .await;
    assert_eq!(absent.status, StatusCode::NOT_FOUND, "no owner, no path");
    assert_eq!(
        absent.header("x-wired-fallback"),
        Some("1"),
        "the fallback answers"
    );
}

/// Core's shared documents are served whatever `PROTOCOLS` enables.
#[tokio::test]
async fn core_documents_are_served_without_activitypub() {
    let db = common::test_db().await;
    let app = common::test_app_with_config(db.pool().clone(), config_with_protocols("fixture"));

    let nodeinfo = common::TestRequest::get("/.well-known/nodeinfo")
        .send(&app)
        .await;
    assert_eq!(nodeinfo.status, StatusCode::OK, "NodeInfo is core's");

    let host_meta = common::TestRequest::get("/.well-known/host-meta")
        .send(&app)
        .await;
    assert_eq!(host_meta.status, StatusCode::OK, "host-meta is core's");
}

/// The `write:accounts` scope `update_credentials` needs.
const WRITE_ACCOUNTS: &str = "write:accounts";

/// How many `Update` publish jobs the profile debounce key of `user_id` has queued.
async fn queued_profile_updates(pool: &PgPool, user_id: Uuid) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM jobs WHERE kind = 'federator.publish' AND unique_key = $1",
    )
    .bind(format!("profile-update:{user_id}"))
    .fetch_one(pool)
    .await
    .expect("the publish count reads")
}

/// Edits the display name of the account behind `token`, asserting a 200.
async fn rename_through_update_credentials(app: &Router, token: &str) {
    let response = common::TestRequest::patch("/api/v1/accounts/update_credentials")
        .bearer(token)
        .form(&[("display_name", "Renamed")])
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the profile write");
}

/// With `activitypub,fixture` a profile edit fires `ProfileChanged` and still federates the `Update`.
#[tokio::test]
async fn update_credentials_records_the_event_and_federates_the_update() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = remote::keyed_local_user(&pool, "profile-event-both").await;
    let token = common::create_app_and_token(&pool, &alice, WRITE_ACCOUNTS).await;
    let app =
        common::test_app_with_config(pool.clone(), config_with_protocols("activitypub,fixture"));

    rename_through_update_credentials(&app, &token).await;

    assert_eq!(
        recorded_for(alice.id),
        vec![Recorded::Event(AccountEvent::ProfileChanged)],
        "the fixture saw one ProfileChanged"
    );
    assert_eq!(
        queued_profile_updates(&pool, alice.id).await,
        1,
        "ActivityPub queued its Update"
    );
}

/// With only `fixture` the same edit records the event and enqueues no `ActivityPub` delivery.
#[tokio::test]
async fn update_credentials_without_activitypub_records_the_event_only() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = remote::keyed_local_user(&pool, "profile-event-fixture").await;
    let token = common::create_app_and_token(&pool, &alice, WRITE_ACCOUNTS).await;
    let app = common::test_app_with_config(pool.clone(), config_with_protocols("fixture"));

    rename_through_update_credentials(&app, &token).await;

    assert_eq!(
        recorded_for(alice.id),
        vec![Recorded::Event(AccountEvent::ProfileChanged)],
        "the fixture saw one ProfileChanged"
    );
    assert_eq!(
        queued_profile_updates(&pool, alice.id).await,
        0,
        "no ActivityPub Update was queued"
    );
    let deliveries = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM jobs WHERE kind IN ('federator.publish', 'federator.deliver')",
    )
    .fetch_one(&pool)
    .await
    .expect("the job count reads");
    assert_eq!(deliveries, 0, "no ActivityPub job of any kind");
}

/// Core client routes answer, and `ActivityPub`-backed ones do not, with only `fixture` enabled.
#[tokio::test]
async fn a_fixture_only_instance_keeps_the_core_client_api_and_drops_the_activitypub_half() {
    let db = common::test_db().await;
    let app = common::test_app_with_config(db.pool().clone(), config_with_protocols("fixture"));

    let timeline = common::TestRequest::get("/api/v1/timelines/public")
        .send(&app)
        .await;
    assert_eq!(
        timeline.status,
        StatusCode::NOT_FOUND,
        "the timeline is ActivityPub-backed"
    );
    assert_eq!(
        timeline.header("x-wired-fallback"),
        Some("1"),
        "the fallback answers the timeline"
    );

    let instance = common::TestRequest::get("/api/v1/instance")
        .send(&app)
        .await;
    common::assert_answer(&instance, StatusCode::OK, "instance info is core");

    let registered = common::TestRequest::post("/api/v1/apps")
        .json(&serde_json::json!({
            "client_name": "Fixture Client",
            "redirect_uris": "urn:ietf:wg:oauth:2.0:oob",
        }))
        .send(&app)
        .await;
    common::assert_answer(&registered, StatusCode::OK, "app registration is core");
}

/// Every `ActivityPub`-backed prefix is unmounted with only `fixture`, and mounted by default.
#[tokio::test]
async fn activitypub_backed_client_routes_follow_the_activitypub_toggle() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let fixture_app = common::test_app_with_config(pool.clone(), config_with_protocols("fixture"));
    let default_app = common::test_app(pool);
    let activitypub_backed = [
        "/api/v1/timelines/public",
        "/api/v2/filters",
        "/api/v2/search",
        "/api/v1/pleroma/chats",
    ];

    for path in activitypub_backed {
        let without = common::TestRequest::get(path).send(&fixture_app).await;
        assert_eq!(
            without.header("x-wired-fallback"),
            Some("1"),
            "{path} is the fallback without ActivityPub"
        );
        let with = common::TestRequest::get(path).send(&default_app).await;
        assert_eq!(
            with.header("x-wired-fallback"),
            None,
            "{path} is a route with ActivityPub"
        );
    }
    let core = common::TestRequest::get("/api/v1/pleroma/emoji/packs")
        .send(&fixture_app)
        .await;
    assert_eq!(
        core.header("x-wired-fallback"),
        None,
        "the emoji packs route is core"
    );
}

/// The fixture's `client_api_router` hook is mounted only while the fixture is enabled.
#[tokio::test]
async fn the_fixture_client_api_route_is_mounted_only_when_the_fixture_is_enabled() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let fixture_app = common::test_app_with_config(pool.clone(), config_with_protocols("fixture"));
    let activitypub_app = common::test_app_with_config(pool, config_with_protocols("activitypub"));

    let served = common::TestRequest::get("/api/v1/fixture/ping")
        .send(&fixture_app)
        .await;
    assert_eq!(served.status, StatusCode::NO_CONTENT, "the fixture answers");

    let absent = common::TestRequest::get("/api/v1/fixture/ping")
        .send(&activitypub_app)
        .await;
    assert_eq!(absent.status, StatusCode::NOT_FOUND, "no owner, no route");
    assert_eq!(
        absent.header("x-wired-fallback"),
        Some("1"),
        "the fallback answers"
    );
}
