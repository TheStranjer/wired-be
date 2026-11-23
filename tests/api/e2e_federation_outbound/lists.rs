//! The list-membership scenario: a list is its owner's private bookmark, so
//! adding or removing a remote member announces nothing to anyone — no `Add`,
//! no `Remove`, no job — with the handshake that accepts the local follow
//! (a remote account can only be listed once it is followed).

use axum::{Router, http::StatusCode};
use serde_json::json;

use super::expectations::{count_at, id_of};
use super::support::{BOB, follow_by_uri, local};
use crate::{common, remote};

/// Accepts `follower`'s pending follow of the mock account `nick`: drains
/// the outbound `Follow`, reads its id off the capture and rides the peer's
/// `Accept` back through the inbox, so the pair stands accepted.
async fn accept_outgoing_follow(
    app: &Router,
    state: &wired_be::state::AppState,
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

/// Scenario: list membership. Adding the remote member and removing them
/// again deliver neither an `Add` nor a `Remove` to the peer, and nothing at
/// all reaches either of its inboxes: the listed account is never told.
#[tokio::test]
async fn list_membership_writes_reach_no_remote_inbox() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (_alice, alice_token) = local(&pool, "eo-list-alice").await;
    let target = follow_by_uri(&app, &alice_token, &peer.handle(BOB)).await;
    let bob_id = id_of(&target);
    let _sent = common::run_all_jobs(&state).await;
    accept_outgoing_follow(&app, &state, &peer, BOB, "eo-list-accept").await;
    let before = peer.received().len();

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

    let removed = common::TestRequest::delete(&format!(
        "/api/v1/lists/{list_id}/accounts?account_ids%5B%5D={bob_id}"
    ))
    .bearer(&alice_token)
    .send(&app)
    .await;
    common::assert_answer(&removed, StatusCode::OK, "the member remove");
    let _drained_again = common::run_all_jobs(&state).await;

    assert!(peer.delivered("Add").is_empty(), "no Add was announced");
    assert!(
        peer.delivered("Remove").is_empty(),
        "no Remove was announced"
    );
    assert_eq!(
        peer.received().len(),
        before,
        "the peer received nothing at all after the handshake"
    );
    assert_eq!(
        count_at(&peer.received(), remote::INBOX),
        0,
        "the shared inbox holds no membership news"
    );
}
