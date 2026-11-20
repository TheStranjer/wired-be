//! Scenario 7: the CSV imports — the follow, block and mute lists the
//! import endpoints queue, the jobs drained through
//! [`common::run_all_jobs`], and the relationships they promised read back
//! through `GET /api/v1/accounts/relationships`.

use axum::http::StatusCode;
use serde_json::json;

use super::support::{Account, World, assert_flags, relationship};
use crate::common::{self, TestResponse};

// ───────────────────────  Scenario 7: the CSV imports  ───────────────────────

/// The import scenario end to end: three Mastodon-style CSV lists posted
/// to the import endpoints, the jobs they queue drained through
/// [`common::run_all_jobs`], and the relationships they promised read back
/// through `GET /api/v1/accounts/relationships` — including the `Show
/// boosts` column, which lands as the follow's own toggle.
#[tokio::test]
async fn csv_imports_land_as_relationships_when_the_queue_drains() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let world = World::open(pool).await;
    let alice = world.register("alice").await;
    let bob = world.register("bob").await;
    let carol = world.register("carol").await;
    let dave = world.register("dave").await;

    // Step 1: the follow list, posted as an inline `list` parameter: the
    // header line dropped, a leading `@` stripped, one row per account.
    let follows = import(
        &world,
        &alice,
        "/api/pleroma/follow_import",
        "Account address,Show boosts,Notify on new posts,Languages\n\
         @bob,,,\n\
         carol,False,,\n",
    )
    .await;
    common::assert_answer(&follows, StatusCode::OK, "the follow import");
    assert_eq!(
        follows.json_value(),
        "jobs started",
        "Pleroma's JSON-string answer"
    );

    // Step 2: the block and mute lists, and one drain for the whole queue.
    let blocks = import(
        &world,
        &alice,
        "/api/pleroma/blocks_import",
        "Account address\ndave\n",
    )
    .await;
    let mutes = import(
        &world,
        &alice,
        "/api/pleroma/mutes_import",
        "Account address\n@bob\n",
    )
    .await;
    common::assert_answer(&blocks, StatusCode::OK, "the block import");
    common::assert_answer(&mutes, StatusCode::OK, "the mute import");
    let ran = world.drain().await;
    assert!(ran >= 3, "one job per import ran, got {ran}");

    // Step 3: the graph the imports built, read back through the API.
    let followed = relationship(&world, &alice.token, &bob.id).await;
    assert_flags(
        &followed,
        &json!({"following": true, "showing_reblogs": true}),
        "the follow whose boosts column was left blank",
    );
    let hidden = relationship(&world, &alice.token, &carol.id).await;
    assert_flags(
        &hidden,
        &json!({"following": true, "showing_reblogs": false}),
        "the follow whose `Show boosts` said False",
    );
    let blocked = relationship(&world, &alice.token, &dave.id).await;
    assert_flags(&blocked, &json!({"blocking": true}), "the imported block");
    let quiet = relationship(&world, &alice.token, &bob.id).await;
    assert_flags(
        &quiet,
        &json!({"muting": true}),
        "the imported mute, beside the follow it left standing",
    );

    // Step 4: an empty list queues nothing and is still answered.
    let nothing = import(&world, &alice, "/api/pleroma/follow_import", "").await;
    common::assert_answer(&nothing, StatusCode::OK, "the empty import");
}

/// One CSV list posted to `path` as `account`, in the inline JSON spelling
/// of the `list` field.
async fn import(world: &World, account: &Account, path: &str, csv: &str) -> TestResponse {
    common::api_post(
        &world.app,
        Some(&account.token),
        path,
        &json!({"list": csv}),
    )
    .await
}
