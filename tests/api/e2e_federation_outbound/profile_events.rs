//! The profile-event scenarios: the actor-document `Update` behind
//! `PATCH /api/v1/accounts/update_credentials` and its per-account debounce
//! key, the `Move` behind `POST /api/v1/accounts/move` and the re-read
//! actor's `Update`, and the forwarded `Flag` spoken by the instance actor
//! and filed into the admin queue — with the multipart profile write the
//! `Update` scenarios ride.

use axum::{Router, body::Bytes, http::StatusCode};
use serde_json::{Value, json};
use tempfile::TempDir;
use wired_be::models::user::{Flag, NewUser};

use super::expectations::{Expected, assert_delivered, assert_flags};
use super::support::{
    ACCOUNTS, AS_PUBLIC, BOB, SCOPES, followers_of, get_json, instance_actor, keyed_with_password,
    local, lookup, peer_follows, personal, publishes_with_key,
};
use crate::{common, remote};

/// The `avatar` file part of the profile writes: a real, decodable `PNG`.
fn avatar_part() -> common::MultipartPart {
    common::MultipartPart::file(
        "avatar",
        "me.png",
        "image/png",
        Bytes::from(common::images::png()),
    )
}

/// One `PATCH /api/v1/accounts/update_credentials` multipart write with the
/// given text parts and an optional image file, asserted 200 and answered
/// as the `Account` entity.
async fn profile_patch(app: &Router, token: &str, parts: &[common::MultipartPart]) -> Value {
    let response = common::TestRequest::patch("/api/v1/accounts/update_credentials")
        .bearer(token)
        .multipart("wired-e2e-outbound-profile", parts)
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the profile write");
    response.json_value()
}

/// Scenario: the profile `Update`. A `PATCH /api/v1/accounts/update_credentials`
/// puts the freshly rendered actor document on the wire to the follower's
/// shared inbox — display name, bio, profile fields and avatar as the row
/// now holds them, `to` the public and the followers collection in `cc` —
/// and a burst of later edits collapses into the one activity the
/// per-account `profile-update:{id}` key keeps queued.
#[tokio::test]
async fn a_profile_update_federates_the_fresh_actor_document() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let uploads = TempDir::new().expect("temp dir for uploads");
    let config = common::test_config_with_uploads_dir(uploads.path());
    let state = common::test_app_state(pool.clone(), config);
    let app = wired_be::app(state.clone());
    let peer = remote::MockPeer::start().await;
    let (alice, alice_token) = local(&pool, "eo-profile-alice").await;
    peer_follows(&app, &state, &peer, BOB, &alice).await;

    let entity = profile_patch(
        &app,
        &alice_token,
        &[
            common::MultipartPart::text("display_name", "Alice Wonder"),
            common::MultipartPart::text("note", "bio the peers should read"),
            common::MultipartPart::text("fields_attributes[0][name]", "Site"),
            common::MultipartPart::text("fields_attributes[0][value]", "wired.example"),
            avatar_part(),
        ],
    )
    .await;
    let _drained = common::run_all_jobs(&state).await;

    let first_updates = peer.delivered("Update");
    assert_eq!(
        first_updates.len(),
        1,
        "the profile write is announced once"
    );
    assert_delivered(
        &first_updates[0],
        &Expected {
            path: remote::INBOX,
            kind: "Update",
            actor: &alice,
            to: Some(json!([AS_PUBLIC])),
            cc: Some(json!([followers_of(&alice)])),
        },
        &peer.host(),
    );
    let document = &first_updates[0].activity()["object"];
    assert_flags(
        document,
        &[
            ("id", json!(alice.ap_id)),
            ("type", json!("Person")),
            ("name", json!("Alice Wonder")),
            ("summary", json!("<p>bio the peers should read</p>")),
            ("icon", json!({ "type": "Image", "url": entity["avatar"] })),
        ],
    );
    assert_eq!(
        document["attachment"][0]["name"],
        json!("Site"),
        "the profile field travels as a property"
    );

    let burst = [common::MultipartPart::text("note", "second bio")];
    let _second = profile_patch(&app, &alice_token, &burst).await;
    let _third = profile_patch(
        &app,
        &alice_token,
        &[common::MultipartPart::text("display_name", "Alice Twice")],
    )
    .await;
    let _drained_again = common::run_all_jobs(&state).await;
    let updates = peer.delivered("Update");
    assert_eq!(updates.len(), 2, "the burst rides out as one Update");
    assert_eq!(
        publishes_with_key(&pool, &format!("profile-update:{}", alice.id)).await,
        2,
        "two live windows, two keyed publish jobs: the swallowed edit queued none"
    );
    assert_flags(
        &updates[1].activity()["object"],
        &[
            ("name", json!("Alice Wonder")),
            ("summary", json!("<p>second bio</p>")),
        ],
    );
}

/// Scenario: `POST /api/v1/accounts/move` announces the move over the wire.
/// The origin's remote follower receives the `Move` — `object` the origin,
/// `target` the destination, addressed to the followers collection alone —
/// and the profile `Update` of the re-read actor carrying the new `movedTo`.
#[tokio::test]
async fn a_move_announces_the_new_home_to_the_origin_followers() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let alice = keyed_with_password(&pool, "eo-move-alice", "moving-password").await;
    let alice_token = common::create_app_and_token(&pool, &alice, SCOPES).await;
    let carol = keyed_with_password(&pool, "eo-move-carol", "waiting-password").await;
    let carol_token = common::create_app_and_token(&pool, &carol, SCOPES).await;
    peer_follows(&app, &state, &peer, BOB, &alice).await;

    let aliased = common::TestRequest::put("/api/pleroma/aliases")
        .bearer(&carol_token)
        .json(&json!({"alias": alice.ap_id}))
        .send(&app)
        .await;
    common::assert_answer(&aliased, StatusCode::OK, "the destination lists its alias");
    let moved = common::TestRequest::post(&format!("{ACCOUNTS}/move"))
        .bearer(&alice_token)
        .json(&json!({"to_account": carol.id.to_string(), "password": "moving-password"}))
        .send(&app)
        .await;
    common::assert_answer(&moved, StatusCode::OK, "the move write");
    assert_eq!(
        moved.json_value()["id"],
        json!(carol.id.to_string()),
        "the answer is the caller's relationship to the destination"
    );
    let _drained = common::run_all_jobs(&state).await;

    let movement = peer.delivered("Move");
    assert_eq!(movement.len(), 1, "one Move to the follower's shared inbox");
    assert_delivered(
        &movement[0],
        &Expected {
            path: remote::INBOX,
            kind: "Move",
            actor: &alice,
            to: Some(json!([followers_of(&alice)])),
            cc: Some(json!([])),
        },
        &peer.host(),
    );
    assert_flags(
        movement[0].activity(),
        &[
            ("object", json!(alice.ap_id)),
            ("target", json!(carol.ap_id)),
        ],
    );
    let updates = peer.delivered("Update");
    assert_eq!(updates.len(), 1, "and the re-read actor follows the move");
    assert_eq!(
        updates[0].activity()["object"]["movedTo"],
        json!(carol.ap_id),
        "the delivered document grows the gone-to pointer"
    );
    assert_eq!(
        peer.received().len(),
        3,
        "the Accept, the Move and the Update: nothing else left"
    );
}

/// Scenario: the forwarded `Flag`. `POST /api/v1/accounts/{id}/flag` files
/// the report and asks the target's instance to handle it: the INSTANCE
/// actor speaks a `Flag` whose `object` names the reported account and
/// whose `content` is the reporter's comment, delivered to the target's
/// personal inbox alone — no fan-out beyond it — and the filing stands in
/// the admin queue. The same write against a local account answers the
/// entity and sends nothing: those moderators are our own.
#[tokio::test]
async fn a_flag_reaches_the_remote_inbox_and_the_admin_queue() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (alice, alice_token) = local(&pool, "eo-flag-alice").await;
    peer_follows(&app, &state, &peer, BOB, &alice).await;
    let bob = lookup(&app, &peer.handle(BOB)).await;
    let bob_id = bob["id"].as_str().expect("the mirror id");

    let filed = common::TestRequest::post(&format!("{ACCOUNTS}/{bob_id}/flag"))
        .bearer(&alice_token)
        .json(&json!({"comment": "harassment, please review", "category": "spam"}))
        .send(&app)
        .await;
    common::assert_answer(&filed, StatusCode::OK, "the flag write");
    assert_flags(
        &filed.json_value(),
        &[
            ("forwarded", json!(true)),
            ("comment", json!("harassment, please review")),
        ],
    );
    let _drained = common::run_all_jobs(&state).await;

    let instance = instance_actor(&state).await;
    let flags = peer.delivered("Flag");
    assert_eq!(flags.len(), 1, "the target's instance is told once");
    assert_delivered(
        &flags[0],
        &Expected {
            path: &personal(&peer, BOB),
            kind: "Flag",
            actor: &instance,
            to: Some(json!([peer.actor(BOB)])),
            cc: Some(json!([])),
        },
        &peer.host(),
    );
    assert_flags(
        flags[0].activity(),
        &[
            ("object", json!([peer.actor(BOB)])),
            ("content", json!("harassment, please review")),
        ],
    );

    let admin = common::create_local_user_with(&pool, "eo-flag-admin", |user| NewUser {
        is_admin: Flag::new(true),
        ..user
    })
    .await;
    let admin_token = common::create_app_and_token(&pool, &admin, "admin:read:reports").await;
    let queue = get_json(&app, &admin_token, "/api/v1/pleroma/admin/reports").await;
    assert_eq!(
        queue["total"],
        json!(1),
        "the filing stands in the admin queue"
    );
    assert_eq!(
        queue["reports"][0]["account"]["id"],
        json!(bob_id),
        "naming the reported account with the reporter's comment"
    );

    let before = peer.delivery_count();
    let carol = remote::keyed_local_user(&pool, "eo-flag-carol").await;
    let carol_id = lookup(&app, &format!("{}@test.example", carol.nickname)).await;
    let local_path = format!("{ACCOUNTS}/{}/flag", common::id_of(&carol_id));
    let local_filed = common::TestRequest::post(&local_path)
        .bearer(&alice_token)
        .json(&json!({"comment": "our own moderator problem"}))
        .send(&app)
        .await;
    common::assert_answer(
        &local_filed,
        StatusCode::OK,
        "flagging a local account files",
    );
    let _drained_again = common::run_all_jobs(&state).await;
    assert_eq!(
        peer.delivery_count(),
        before,
        "and a local target's moderators hear nothing on the wire"
    );
}
