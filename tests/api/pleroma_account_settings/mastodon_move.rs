//! The Mastodon `POST /api/v1/accounts/move` twin of the Pleroma move
//! route: its answer, its gate order and the shared cooldown.

use axum::http::StatusCode;
use serde_json::json;
use uuid::Uuid;
use wired_be::federation::publisher;

use super::support::{
    PASSWORD, aliased_target, assert_move_queued, fixture, move_account, move_v1, queued_jobs,
    stored,
};
use crate::common;

/// The Mastodon twin moves the same account the Pleroma route moves: the
/// caller's `Relationship` to the destination comes back as the 200 body
/// (where Pleroma answers `{"status": "success"}`), the account is marked
/// moved, and the same `Move` lands on the federation queue.
#[tokio::test]
async fn the_mastodon_move_route_moves_the_account_and_queues_the_move() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "write:accounts").await;
    let bob = aliased_target(&pool, "bob", &alice).await;
    let app = common::test_app(pool.clone());

    let moved = move_v1(
        &app,
        &token,
        json!({"to_account": bob.id.to_string(), "password": PASSWORD}),
    )
    .await;
    assert_eq!(moved.status, StatusCode::OK, "the move succeeds");
    let relationship = moved.json_value();
    assert_eq!(
        relationship["id"],
        json!(bob.id.to_string()),
        "the answer is the relationship entity, not the Pleroma body"
    );
    assert_eq!(
        relationship["following"],
        json!(false),
        "alice holds no follow of her own destination"
    );
    assert!(
        stored(&pool, alice.id).await.last_move_at.is_some(),
        "the account wears the move and its cooldown stamp"
    );
    assert_move_queued(&pool, &alice, &bob).await;
}

/// The twin's refusals are proper statuses, not Pleroma's 200 bodies, in
/// the same gate order: the password first, then the target (404 "Target
/// account not found."), then the alias precondition, then the cooldown —
/// and the 401/403/422 pipeline of the extractor.
#[tokio::test]
async fn the_mastodon_move_route_refuses_in_the_gate_order() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "write:accounts").await;
    let bob = aliased_target(&pool, "bob", &alice).await;
    let stranger = common::create_local_user(&pool, "stranger").await;
    let (_other, read_only) = fixture(&pool, "reader", "read:accounts").await;
    let app = common::test_app(pool.clone());
    let bob_id = bob.id.to_string();

    let wrong_password = move_v1(
        &app,
        &token,
        json!({"to_account": bob_id, "password": "not-my-password"}),
    )
    .await;
    common::assert_answer(
        &wrong_password,
        StatusCode::UNPROCESSABLE_ENTITY,
        "the password",
    );
    common::assert_error(&wrong_password, "Invalid password.", "the password");

    let unknown = move_v1(
        &app,
        &token,
        json!({"to_account": Uuid::now_v7().to_string(), "password": PASSWORD}),
    )
    .await;
    common::assert_answer(&unknown, StatusCode::NOT_FOUND, "an unknown destination");
    common::assert_error(
        &unknown,
        "Target account not found.",
        "an unknown destination",
    );
    let malformed = move_v1(
        &app,
        &token,
        json!({"to_account": "not-a-uuid", "password": PASSWORD}),
    )
    .await;
    common::assert_answer(&malformed, StatusCode::NOT_FOUND, "a malformed id");

    let no_alias = move_v1(
        &app,
        &token,
        json!({"to_account": stranger.id.to_string(), "password": PASSWORD}),
    )
    .await;
    common::assert_answer(
        &no_alias,
        StatusCode::UNPROCESSABLE_ENTITY,
        "the alias gate",
    );
    common::assert_error(
        &no_alias,
        "Target account must have the origin in `alsoKnownAs`",
        "the alias gate",
    );

    let dead = move_v1(
        &app,
        "not-a-stored-token",
        json!({"to_account": bob_id, "password": PASSWORD}),
    )
    .await;
    common::assert_answer(&dead, StatusCode::UNAUTHORIZED, "a dead token");
    let unscoped = move_v1(
        &app,
        &read_only,
        json!({"to_account": bob_id, "password": PASSWORD}),
    )
    .await;
    common::assert_answer(
        &unscoped,
        StatusCode::FORBIDDEN,
        "read:accounts cannot move",
    );
    let short = move_v1(&app, &token, json!({})).await;
    common::assert_answer(&short, StatusCode::UNPROCESSABLE_ENTITY, "a bare body");
    assert!(
        queued_jobs(&pool, publisher::KIND_PUBLISH).await.is_empty(),
        "and no refusal announced a move"
    );
}

/// After the twin moves an account, the cooldown bites the PLEROMA route
/// too — one `last_move_at`, shared gates: a second move inside the window
/// answers Pleroma's 200-body refusal.
#[tokio::test]
async fn the_mastodon_move_route_stands_in_the_shared_cooldown() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "write:accounts").await;
    let first = aliased_target(&pool, "first_target", &alice).await;
    let _second = aliased_target(&pool, "second_target", &alice).await;
    let app = common::test_app(pool.clone());

    let moved = move_v1(
        &app,
        &token,
        json!({"to_account": first.id.to_string(), "password": PASSWORD}),
    )
    .await;
    assert_eq!(moved.status, StatusCode::OK, "the twin moves first");
    let throttled = move_account(&app, &token, PASSWORD, "second_target").await;
    assert_eq!(
        throttled.json_value(),
        json!({"error": "You can't move your account more than once in 30 days"}),
        "the Pleroma route reads the same cooldown stamp"
    );
    assert_eq!(
        stored(&pool, alice.id).await.moved_to_ap_id.as_deref(),
        Some(first.ap_id.as_str()),
        "the refused attempt kept the original target"
    );
}
