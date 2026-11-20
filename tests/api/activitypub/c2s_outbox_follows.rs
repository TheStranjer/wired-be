//! The client-to-server outbox `Follow` and its withdrawal: the follow
//! and its `Undo` by activity id, the restated repeat, the embedded
//! `Follow` undo, and the refusals — an actor this instance has never
//! stored, a deactivated one, and an embedded activity of no withdrawable
//! type.

use axum::http::StatusCode;
use serde_json::json;

use super::support::{c2s_account, post_outbox, relationship};
use crate::common;

/// `Follow` of an actor id through the outbox: the relationship writes
/// and reads back through the client API, a repeat restates the same
/// fact and the same `Location`, and the `Undo` by the activity id the
/// `Follow` answered takes the follow back. A second `Undo` of the same
/// id names nothing any more and is the 404.
#[tokio::test]
async fn the_c2s_outbox_follows_and_undoes_it() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = c2s_account(&pool, "c2s-out-follow-a", "read write").await.1;
    let (bob, _) = c2s_account(&pool, "c2s-out-follow-b", "read").await;

    let follow = post_outbox(
        &app,
        "c2s-out-follow-a",
        Some(&token),
        json!({"type": "Follow", "object": bob.ap_id}),
    )
    .await;
    assert_eq!(
        follow.status,
        StatusCode::CREATED,
        "the follow lands: {}",
        follow.text()
    );
    let follow_id = follow.header("location").expect("a Location").to_owned();
    let rel = relationship(&app, &token, &bob).await;
    assert_eq!(
        rel["following"],
        json!(true),
        "and the relationship says so"
    );

    let repeat = post_outbox(
        &app,
        "c2s-out-follow-a",
        Some(&token),
        json!({"type": "Follow", "object": bob.ap_id}),
    )
    .await;
    assert_eq!(repeat.status, StatusCode::CREATED, "a repeat is restated");
    assert_eq!(
        repeat.header("location"),
        Some(follow_id.as_str()),
        "naming the one Follow the pair holds"
    );

    let undone = post_outbox(
        &app,
        "c2s-out-follow-a",
        Some(&token),
        json!({"type": "Undo", "object": follow_id}),
    )
    .await;
    assert_eq!(undone.status, StatusCode::CREATED, "the undo is accepted");
    assert_ne!(
        undone.header("location"),
        Some(follow_id.as_str()),
        "naming the fresh Undo activity instead"
    );
    let gone = relationship(&app, &token, &bob).await;
    assert_eq!(gone["following"], json!(false), "and the follow is gone");

    let again = post_outbox(
        &app,
        "c2s-out-follow-a",
        Some(&token),
        json!({"type": "Undo", "object": follow_id}),
    )
    .await;
    common::assert_answer(&again, StatusCode::NOT_FOUND, "a second undo of one follow");
}

/// The other `Undo` arms: an embedded `Follow` withdraws by the target
/// actor the inner activity names, a follow of an actor this instance has
/// never stored is the 404 (the C2S door reads stored rows, it fetches
/// nobody), and an embedded activity of no withdrawable type is the 400.
#[tokio::test]
async fn the_c2s_outbox_undoes_an_embedded_follow_and_refuses_the_rest() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = c2s_account(&pool, "c2s-out-follow-c", "read write").await.1;
    let (bob, _) = c2s_account(&pool, "c2s-out-follow-d", "read").await;

    let followed = post_outbox(
        &app,
        "c2s-out-follow-c",
        Some(&token),
        json!({"type": "Follow", "object": bob.ap_id}),
    )
    .await;
    assert_eq!(followed.status, StatusCode::CREATED, "followed");
    let embedded_undo = post_outbox(
        &app,
        "c2s-out-follow-c",
        Some(&token),
        json!({"type": "Undo", "object": {"type": "Follow", "object": bob.ap_id}}),
    )
    .await;
    assert_eq!(
        embedded_undo.status,
        StatusCode::CREATED,
        "the embedded undo lands"
    );
    let withdrawn = relationship(&app, &token, &bob).await;
    assert_eq!(
        withdrawn["following"],
        json!(false),
        "and withdrew the follow"
    );

    let unknown = post_outbox(
        &app,
        "c2s-out-follow-c",
        Some(&token),
        json!({"type": "Follow", "object": "https://nowhere.example/users/eve"}),
    )
    .await;
    common::assert_answer(
        &unknown,
        StatusCode::NOT_FOUND,
        "a follow of an unseen actor",
    );

    let dormant = common::create_deactivated_user_with_password(
        &pool,
        "c2s-out-follow-sleeps",
        "correct horse battery staple",
    )
    .await;
    let gone = post_outbox(
        &app,
        "c2s-out-follow-c",
        Some(&token),
        json!({"type": "Follow", "object": dormant.ap_id}),
    )
    .await;
    common::assert_answer(
        &gone,
        StatusCode::NOT_FOUND,
        "a follow of a deactivated account",
    );

    let odd = post_outbox(
        &app,
        "c2s-out-follow-c",
        Some(&token),
        json!({"type": "Undo", "object": {"type": "Flag", "object": bob.ap_id}}),
    )
    .await;
    common::assert_answer(&odd, StatusCode::BAD_REQUEST, "an undo of an odd type");
    common::assert_error(&odd, "Unsupported activity type", "an undo of an odd type");
}
