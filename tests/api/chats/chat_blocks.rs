//! Blocks and the chat gates over HTTP (WBE-609): `POST …/by-account-id/{id}`
//! and `POST …/chats/{id}/messages` both refuse a pair where either side has
//! blocked the other with Pleroma's `Chat not allowed` 403, and the posting
//! route also refuses a recipient who has since stopped accepting chats.

use axum::http::StatusCode;
use sqlx::PgPool;
use wired_be::{
    config::TEST_BASE_URL,
    models::{
        block,
        user::{Flag, NewUser, User},
    },
};

use super::messages_support::{post_message, poster};
use super::support::{
    SCOPE, assert_refused, count_messages, fixture, messages_path, open_chat, open_path, post_empty,
};
use crate::common;

/// `blocker` blocks `target`, through the model.
async fn block_user(pool: &PgPool, blocker: &User, target: &User) {
    let _row = block::block(pool, TEST_BASE_URL, blocker, target)
        .await
        .expect("the block is written");
}

/// How many rows the `chats` table holds.
async fn count_chats(pool: &PgPool) -> i64 {
    let row: (i64,) = sqlx::query_as("SELECT count(*) FROM chats")
        .fetch_one(pool)
        .await
        .unwrap();
    row.0
}

/// Turns `user`'s `accepts_chat_messages` flag off.
async fn stop_accepting_chats(pool: &PgPool, user: &User) {
    let _done: sqlx::postgres::PgQueryResult =
        sqlx::query("UPDATE users SET accepts_chat_messages = false WHERE id = $1")
            .bind(user.id)
            .execute(pool)
            .await
            .expect("the flag update runs");
}

/// A recipient who blocked the caller, and a caller who blocked the
/// recipient, are each the 403 `Chat not allowed` and leave no chat row; an
/// unblocked pair still opens one.
#[tokio::test]
async fn a_blocked_pair_cannot_open_a_chat_in_either_direction() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = fixture(&pool, "alice", SCOPE).await;
    let (bob, bob_token) = fixture(&pool, "bob", SCOPE).await;
    let carol = common::create_local_user(&pool, "carol").await;
    let app = common::test_app(pool.clone());

    block_user(&pool, &bob, &alice).await;

    let alice_opens = post_empty(&app, &alice_token, &open_path(&bob.id.to_string())).await;
    assert_refused(
        &alice_opens,
        "alice, whom bob blocked",
        "by-account-id",
        StatusCode::FORBIDDEN,
        "Chat not allowed",
    );
    let bob_opens = post_empty(&app, &bob_token, &open_path(&alice.id.to_string())).await;
    assert_refused(
        &bob_opens,
        "bob, who blocked alice",
        "by-account-id",
        StatusCode::FORBIDDEN,
        "Chat not allowed",
    );
    assert_eq!(
        count_chats(&pool).await,
        0,
        "no refusal leaves a chat behind"
    );

    let unblocked = post_empty(&app, &alice_token, &open_path(&carol.id.to_string())).await;
    assert_eq!(
        unblocked.status,
        StatusCode::OK,
        "an unblocked pair still opens a chat: {}",
        unblocked.text()
    );
    assert_eq!(
        unblocked.json_value()["account"]["id"],
        carol.id.to_string(),
        "with the account asked for"
    );
}

/// The gates keep their order ahead of the block: yourself is the 422 and a
/// recipient who does not accept chats is the flag's 403, even when the
/// pair is also blocked.
#[tokio::test]
async fn the_self_and_flag_refusals_stand_ahead_of_the_block() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", SCOPE).await;
    let carol = common::create_local_user_with(&pool, "carol", |new| NewUser {
        accepts_chat_messages: Flag::new(false),
        ..new
    })
    .await;
    block_user(&pool, &carol, &alice).await;
    let app = common::test_app(pool.clone());

    let silent = post_empty(&app, &token, &open_path(&carol.id.to_string())).await;
    assert_refused(
        &silent,
        "carol, who is silent and blocked alice",
        "by-account-id",
        StatusCode::FORBIDDEN,
        "Recipient doesn't accept chat messages",
    );
    let yourself = post_empty(&app, &token, &open_path(&alice.id.to_string())).await;
    assert_refused(
        &yourself,
        "alice, naming herself",
        "by-account-id",
        StatusCode::UNPROCESSABLE_ENTITY,
        "Can't chat with yourself",
    );
}

/// A chat opened before a block stops delivering once either side blocks
/// the other: the 403 `Chat not allowed`, no message row, and the chat's
/// other owner is refused the same way. Unblocked, the pair posts.
#[tokio::test]
async fn posting_into_an_old_chat_is_refused_after_a_block() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = poster(&pool, "alice").await;
    let (bob, bob_token) = poster(&pool, "bob").await;
    let alice_chat = open_chat(&pool, &alice, &bob).await.id.to_string();
    let bob_chat = open_chat(&pool, &bob, &alice).await.id.to_string();
    let app = common::test_app(pool.clone());

    let before = post_message(
        &app,
        &alice_token,
        &alice_chat,
        serde_json::json!({"content": "before"}),
        None,
    )
    .await;
    assert_eq!(
        before.status,
        StatusCode::OK,
        "the unblocked pair posts: {}",
        before.text()
    );
    assert_eq!(count_messages(&pool).await, 1, "and the message is stored");

    block_user(&pool, &bob, &alice).await;

    for (who, token, chat) in [
        ("alice, whom bob blocked", &alice_token, &alice_chat),
        ("bob, who blocked alice", &bob_token, &bob_chat),
    ] {
        let refused = post_message(
            &app,
            token,
            chat,
            serde_json::json!({"content": "after"}),
            Some("key-after-block"),
        )
        .await;
        assert_refused(
            &refused,
            who,
            &messages_path(chat),
            StatusCode::FORBIDDEN,
            "Chat not allowed",
        );
    }
    assert_eq!(
        count_messages(&pool).await,
        1,
        "the refused posts wrote no message"
    );
}

/// A recipient who turned `accepts_chat_messages` off after the chat was
/// opened no longer receives: the flag's 403 and no message row.
#[tokio::test]
async fn posting_is_refused_once_the_recipient_stops_accepting_chats() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = poster(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let chat = open_chat(&pool, &alice, &bob).await.id.to_string();
    stop_accepting_chats(&pool, &bob).await;
    let app = common::test_app(pool.clone());

    let refused = post_message(
        &app,
        &token,
        &chat,
        serde_json::json!({"content": "hello?"}),
        None,
    )
    .await;
    assert_refused(
        &refused,
        "alice, posting to a silent bob",
        &messages_path(&chat),
        StatusCode::FORBIDDEN,
        "Recipient doesn't accept chat messages",
    );
    assert_eq!(count_messages(&pool).await, 0, "nothing was written");
}
