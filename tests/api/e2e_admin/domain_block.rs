//! Scenario 2's third part: the remote domain's deletion. A delivered
//! mirror and its post stand until `DELETE /instances/{domain}` marks the
//! domain gone; the staged `users.delete_remote` sweeps then run through
//! the queue and the post leaves the timelines, with the local domain
//! refused its own deletion. The note builders and the signed delivery are
//! [`mrf`](super::mrf)'s `pub(super)` helpers.

use axum::http::StatusCode;
use serde_json::{Value, json};

use super::mrf::{carries_uri, create_note, deliver};
use super::support::{BOB, P_ADMIN, PUBLIC_TIMELINE, World, assert_log, get_array};
use crate::{common, remote};

/// Scenario 2, part 3: the remote domain's deletion. A delivered mirror
/// and its post stand until `DELETE /instances/{domain}` marks the domain
/// gone; the staged `users.delete_remote` sweeps then run through the
/// queue and the post leaves the timelines.
#[tokio::test]
async fn a_blocked_domain_is_refused_at_the_door_and_swept_on_deletion() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let world = World::open(pool).await;
    let peer = remote::MockPeer::start().await;
    let actor = peer.actor(BOB);

    let uri = peer.note("sweep-1");
    let domain_create = create_note(
        &peer,
        &actor,
        "sweep-1",
        "<p>until the sweep</p>",
        json!({}),
    );
    let door = deliver(&world, &peer, &domain_create).await;
    common::assert_answer(
        &door,
        StatusCode::ACCEPTED,
        "the note lands before the sweep",
    );
    assert!(
        carries_uri(&get_array(&world.app, None, PUBLIC_TIMELINE).await, &uri),
        "on the timeline"
    );

    let response = common::api_delete(
        &world.app,
        Some(&world.admin),
        &format!("{P_ADMIN}/instances/{}", peer.host()),
        &Value::Null,
    )
    .await;
    common::assert_answer(&response, StatusCode::OK, "the instance deletion lands");
    assert_eq!(
        response.json_value(),
        json!(peer.host()),
        "the domain answers as a JSON string"
    );
    let ran = world.drain().await;
    assert!(ran >= 1, "the staged sweeps ran: {ran}");
    assert!(
        !carries_uri(&get_array(&world.app, None, PUBLIC_TIMELINE).await, &uri),
        "the swept domain's post left the timeline"
    );
    assert_log(
        &world.app,
        &world.admin,
        "delete_instance",
        &[format!("@root deleted instance: {}", peer.host())],
    )
    .await;

    // The local instance cannot delete itself.
    let local = common::api_delete(
        &world.app,
        Some(&world.admin),
        &format!("{P_ADMIN}/instances/test.example"),
        &Value::Null,
    )
    .await;
    common::assert_answer(
        &local,
        StatusCode::BAD_REQUEST,
        "the local domain is refused",
    );
    common::assert_error(
        &local,
        "Can't delete the local instance",
        "the local domain is refused",
    );
}
