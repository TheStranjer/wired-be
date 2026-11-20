//! Scenario 6's second half: the admin relay lifecycle. `POST /relay`
//! follows the mock peer's relay actor and answers the fresh
//! `{actor, followed_back: false}` ledger entry, the ledger mirrors the
//! subscription, the relay's own follow of the instance actor — staged
//! through the models — flips the `followed_back` flag the listing reads
//! back, the `force` unfollow echoes the address, logs both moves, sends
//! the `Undo`, empties the ledger and answers the second withdrawal with
//! the `400` miss.

use axum::http::StatusCode;
use serde_json::json;

use super::support::{BASE, P_ADMIN, World, assert_log};
use crate::{common, remote};

/// Scenario 6, second half: following a relay mirrors the subscription in
/// the `{actor, followed_back}` ledger — the flag true only once the relay
/// has accepted the instance actor's follow back — delivers the `Follow`
/// outbound, and the `force` unfollow answers the address as a JSON string
/// while it clears the ledger and sends the `Undo`; a second withdrawal of
/// the same relay is the `400` miss.
#[tokio::test]
async fn following_a_relay_mirrors_the_subscription_and_the_ledger() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let world = World::open(pool.clone()).await;
    let peer = remote::MockPeer::start().await;
    let relay_url = peer.relay();

    let followed = common::api_post(
        &world.app,
        Some(&world.admin),
        &format!("{P_ADMIN}/relay"),
        &json!({"relay_url": relay_url}),
    )
    .await;
    common::assert_answer(&followed, StatusCode::OK, "the relay follow");
    assert_eq!(
        followed.json_value(),
        json!({"actor": relay_url, "followed_back": false}),
        "a fresh follow answers its actor, not yet followed back",
    );

    let listed = common::api_get(&world.app, Some(&world.admin), &format!("{P_ADMIN}/relay")).await;
    common::assert_answer(&listed, StatusCode::OK, "the subscription listing");
    assert_eq!(
        listed.json_value(),
        json!({"relays": [{"actor": relay_url, "followed_back": false}]}),
        "the ledger mirrors the subscription",
    );

    relay_read_backs_its_accepted_flag(&world, &peer, &relay_url).await;

    relay_withdrawal_walks_the_ledger_and_the_undo(&world, &peer, &relay_url).await;
}

/// Stage the relay's follow of the instance actor through the model and
/// read the flipped `followed_back` flag back through the ledger.
async fn relay_read_backs_its_accepted_flag(
    world: &World,
    peer: &remote::MockPeer,
    relay_url: &str,
) {
    let pool = &world.state.db;
    let mirror = wired_be::models::user::get_by_ap_id(pool, relay_url)
        .await
        .expect("the mirror reads")
        .expect("the follow mirrored the relay actor");
    let instance =
        wired_be::services::instance_actor::ensure_instance_actor(pool, &world.state.config)
            .await
            .expect("the instance actor stands");
    let row = wired_be::models::follow::follow(pool, BASE, &mirror, &instance)
        .await
        .expect("the relay's follow of the instance actor records");
    let _granted = wired_be::models::follow::accept(pool, row.id)
        .await
        .expect("the instance actor grants it");
    let back = common::api_get(&world.app, Some(&world.admin), &format!("{P_ADMIN}/relay")).await;
    assert_eq!(
        back.json_value()["relays"][0]["followed_back"],
        json!(true),
        "the accepted follow reads back in the ledger",
    );

    let _drain = world.drain().await;
    assert!(
        !peer.delivered("Follow").is_empty(),
        "the relay follow goes out over the federator",
    );
}

/// The `force` unfollow echoes the address as a JSON string, logs both
/// moves, sends the `Undo`, empties the ledger and answers the second
/// withdrawal with the `400` miss.
async fn relay_withdrawal_walks_the_ledger_and_the_undo(
    world: &World,
    peer: &remote::MockPeer,
    relay_url: &str,
) {
    let unfollowed = common::api_delete(
        &world.app,
        Some(&world.admin),
        &format!("{P_ADMIN}/relay"),
        &json!({"relay_url": relay_url, "force": true}),
    )
    .await;
    common::assert_answer(&unfollowed, StatusCode::OK, "the relay unfollow");
    assert_eq!(
        unfollowed.json::<String>(),
        relay_url,
        "the unfollow echoes the address as a JSON string",
    );

    assert_log(
        &world.app,
        &world.admin,
        "relay_follow",
        &[format!("@root followed relay: {relay_url}")],
    )
    .await;
    assert_log(
        &world.app,
        &world.admin,
        "relay_unfollow",
        &[format!("@root unfollowed relay: {relay_url}")],
    )
    .await;

    let _undo_drain = world.drain().await;
    assert!(
        !peer.delivered("Undo").is_empty(),
        "the relay unfollow goes out as an Undo",
    );

    let twice = common::api_delete(
        &world.app,
        Some(&world.admin),
        &format!("{P_ADMIN}/relay"),
        &json!({"relay_url": relay_url, "force": true}),
    )
    .await;
    assert_eq!(
        twice.status,
        StatusCode::BAD_REQUEST,
        "unfollowing twice is a miss"
    );
    common::assert_error(&twice, "Not found", "the unfollow miss");
    let empty = common::api_get(&world.app, Some(&world.admin), &format!("{P_ADMIN}/relay")).await;
    assert_eq!(
        empty.json_value(),
        json!({"relays": []}),
        "the ledger reads empty again"
    );
}
