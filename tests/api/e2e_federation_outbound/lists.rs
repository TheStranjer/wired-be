//! The list-membership scenario: the `Add` and the matching `Remove` a
//! curated list announces of a remote member — `target` the list's curated
//! inbox, the member's personal inbox the only recipient — with the
//! handshake that accepts the local follow and the membership assertion
//! both writes share.

use axum::{Router, http::StatusCode};
use serde_json::json;
use wired_be::{config::TEST_BASE_URL, models::user::User, state::AppState};

use super::expectations::{Expected, assert_delivered, count_at, id_of};
use super::support::{BOB, follow_by_uri, local, personal};
use crate::{common, remote};

/// Accepts `follower`'s pending follow of the mock account `nick`: drains
/// the outbound `Follow`, reads its id off the capture and rides the peer's
/// `Accept` back through the inbox, so the pair stands accepted.
async fn accept_outgoing_follow(
    app: &Router,
    state: &AppState,
    peer: &remote::MockPeer,
    nick: &str,
    name: &str,
) {
    let follows = peer.delivered("Follow");
    let follow_id = follows[0].activity()["id"]
        .as_str()
        .expect("the Follow carries its id")
        .to_owned();
    let accept = peer.activity("Accept", name, &peer.actor(nick), &json!(&follow_id));
    let folded = peer
        .deliver(app, &peer.actor(nick), remote::INBOX, &accept)
        .await;
    assert_eq!(
        folded.status,
        StatusCode::ACCEPTED,
        "the inbox folds the peer's Accept"
    );
    let _drained = common::run_all_jobs(state).await;
}

/// Scenario: list membership. Adding the remote member announces an `Add` —
/// `actor` the list owner, `object` the member, `target` the list's curated
/// inbox — delivered to the member's personal inbox alone; removing them
/// announces the matching `Remove`. Nothing of it touches the shared inbox.
#[tokio::test]
async fn list_membership_adds_and_removes_reach_the_member_inbox() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (alice, alice_token) = local(&pool, "eo-list-alice").await;
    let target = follow_by_uri(&app, &alice_token, &peer.handle(BOB)).await;
    let bob_id = id_of(&target);
    let _sent = common::run_all_jobs(&state).await;
    accept_outgoing_follow(&app, &state, &peer, BOB, "eo-list-accept").await;

    let list = common::TestRequest::post("/api/v1/lists")
        .bearer(&alice_token)
        .json(&json!({"title": "picks"}))
        .send(&app)
        .await;
    common::assert_answer(&list, StatusCode::OK, "the list create");
    let list_id = list.json_value()["id"]
        .as_str()
        .expect("the list id")
        .to_owned();

    let added = common::TestRequest::post(&format!("/api/v1/lists/{list_id}/accounts"))
        .bearer(&alice_token)
        .json(&json!({"account_ids": [bob_id]}))
        .send(&app)
        .await;
    common::assert_answer(&added, StatusCode::OK, "the member add");
    let _drained = common::run_all_jobs(&state).await;

    let curated = format!("{TEST_BASE_URL}/api/v1/lists/{list_id}/inbox");
    let adds = peer.delivered("Add");
    assert_eq!(adds.len(), 1, "the join is announced once");
    assert_list_membership(&adds[0], &alice, &peer, "Add", &curated);

    let removed = common::TestRequest::delete(&format!(
        "/api/v1/lists/{list_id}/accounts?account_ids%5B%5D={bob_id}"
    ))
    .bearer(&alice_token)
    .send(&app)
    .await;
    common::assert_answer(&removed, StatusCode::OK, "the member remove");
    let _drained_again = common::run_all_jobs(&state).await;

    let withdrawals = peer.delivered("Remove");
    assert_eq!(withdrawals.len(), 1, "the leaving is announced once");
    assert_list_membership(&withdrawals[0], &alice, &peer, "Remove", &curated);
    assert_eq!(
        count_at(&peer.received(), remote::INBOX),
        0,
        "the shared inbox holds no membership news"
    );
}

/// Asserts `delivery` is one list-membership activity of `kind` (`Add` or
/// `Remove`): signed by the list owner `alice`, naming the mock's `bob` as
/// object and only addressee, and `target`ing the list's curated inbox.
fn assert_list_membership(
    delivery: &remote::Delivery,
    alice: &User,
    peer: &remote::MockPeer,
    kind: &str,
    curated: &str,
) {
    assert_delivered(
        delivery,
        &Expected {
            path: &personal(peer, BOB),
            kind,
            actor: alice,
            to: Some(json!([peer.actor(BOB)])),
            cc: Some(json!([])),
        },
        &peer.host(),
    );
    assert_eq!(
        delivery.activity()["object"],
        json!(peer.actor(BOB)),
        "the member is the object of their own membership news"
    );
    assert_eq!(
        delivery.activity()["target"],
        json!(curated),
        "the activity points back at the list's curated inbox"
    );
}
