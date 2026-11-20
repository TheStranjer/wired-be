//! Creating and showing a chat over HTTP (WBE-146): `POST
//! …/by-account-id/{id}` getting or creating the caller's chat idempotently,
//! the gates that refuse the self-chat, a silent recipient and an unknown
//! account, and `GET …/chats/{id}` answering the chat to its owner alone.

use axum::http::StatusCode;
use sqlx::PgPool;
use wired_be::{
    models::user::{Flag, NewUser},
    util::id::new_id,
};

use super::support::{
    CHAT_KEYS, READ_SCOPE, ROOT, SCOPE, error_message, fixture, get_as, json_keys, listed_ids,
    open_path, post_empty, send,
};
use crate::common;

/// How many rows the `chats` table holds.
async fn count_chats(pool: &PgPool) -> i64 {
    let row: (i64,) = sqlx::query_as("SELECT count(*) FROM chats")
        .fetch_one(pool)
        .await
        .unwrap();
    row.0
}

/// `POST …/by-account-id/{id}` gets or creates the caller's chat with that
/// account: 200 with the full `Chat` entity, the same row on a repeat (an
/// opening never bumps the list), and the other participant's side is the
/// other row with the other id — which is what their own list and `show`
/// then answer.
#[tokio::test]
async fn creating_a_chat_gets_the_same_row_every_time() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", SCOPE).await;
    let (bob, bobs_token) = fixture(&pool, "bob", SCOPE).await;
    let alice_read = common::create_app_and_token(&pool, &alice, READ_SCOPE).await;
    let app = common::test_app(pool.clone());

    let first = post_empty(&app, &token, &open_path(&bob.id.to_string())).await;
    assert_eq!(
        first.status,
        StatusCode::OK,
        "creating answers 200: {}",
        first.text()
    );
    assert_eq!(
        first.header("content-type"),
        Some("application/json"),
        "the entity is JSON"
    );
    let value = first.json_value();
    assert_eq!(json_keys(&value), CHAT_KEYS, "exactly the Chat keys");
    let chat = value["id"]
        .as_str()
        .expect("the chat id is a string")
        .to_owned();
    assert_eq!(value["account"]["id"], bob.id.to_string(), "about bob");
    assert_eq!(value["account"]["acct"], "bob", "as a full Account entity");
    assert_eq!(value["unread"], 0, "a fresh chat has nothing to read");
    assert!(value["last_message"].is_null(), "and holds no message");
    assert_eq!(value["pinned"], false, "a fresh chat is unpinned");

    let again = post_empty(&app, &token, &open_path(&bob.id.to_string())).await;
    assert_eq!(again.status, StatusCode::OK, "the second call is no error");
    let second = again.json_value();
    assert_eq!(second["id"], chat.as_str(), "and answers the same chat");
    assert_eq!(
        second["updated_at"], value["updated_at"],
        "opening does not bump the chat in its owner's list"
    );

    let mine = post_empty(&app, &bobs_token, &open_path(&alice.id.to_string())).await;
    assert_eq!(mine.status, StatusCode::OK, "bob may open his own side");
    let theirs = mine.json_value()["id"]
        .as_str()
        .expect("a chat id")
        .to_owned();
    assert_ne!(theirs, chat, "which is the other row with the other id");
    assert_eq!(count_chats(&pool).await, 2, "one row per direction");

    let listed = get_as(&app, &alice_read, ROOT).await.json_value();
    assert_eq!(
        listed_ids(&listed),
        vec![chat.clone()],
        "alice lists her own chat, not bob's"
    );
    let shown = get_as(&app, &alice_read, &format!("{ROOT}/{chat}")).await;
    assert_eq!(shown.status, StatusCode::OK, "show answers the owner");
    assert_eq!(shown.json_value()["id"], chat.as_str(), "and that chat");
}

/// The create gates, over HTTP: the self-chat is the 422, a recipient whose
/// `accepts_chat_messages` flag is down — local account or remote mirror —
/// the 403 that names the flag, and a malformed, unknown or deactivated id
/// is the one 404 [`account_or_404`] keeps; a remote mirror still at the
/// flag default may be chatted with, and no refusal leaves a chat row
/// behind. (The block refusal behind these waits for the blocks ticket —
/// see the `blocked_between` hook's unit tests for its wiring.)
///
/// [`account_or_404`]: wired_be::api::helpers::account_or_404
#[tokio::test]
async fn the_chat_gates_refuse_self_a_silent_recipient_and_nobody() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", SCOPE).await;
    let carol = common::create_local_user_with(&pool, "carol", |new| NewUser {
        accepts_chat_messages: Flag::new(false),
        ..new
    })
    .await;
    let mallory = common::create_remote_user(&pool, "mallory", "remote.example").await;
    let _quieted: sqlx::postgres::PgQueryResult =
        sqlx::query("UPDATE users SET accepts_chat_messages = false WHERE id = $1")
            .bind(mallory.id)
            .execute(&pool)
            .await
            .expect("the flag update runs");
    let dave = common::create_local_user_with(&pool, "dave", |new| NewUser {
        is_active: Flag::new(false),
        ..new
    })
    .await;
    let erin = common::create_remote_user(&pool, "erin", "remote.example").await;
    let app = common::test_app(pool.clone());

    let self_chat = post_empty(&app, &token, &open_path(&alice.id.to_string())).await;
    assert_eq!(
        self_chat.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "yourself is no recipient: {}",
        self_chat.text()
    );
    assert_eq!(
        error_message(&self_chat).as_deref(),
        Some("Can't chat with yourself"),
        "the ticket's spelling of Pleroma's refusal"
    );

    for silent in [&carol, &mallory] {
        let response = post_empty(&app, &token, &open_path(&silent.id.to_string())).await;
        assert_eq!(
            response.status,
            StatusCode::FORBIDDEN,
            "{} does not accept chats",
            silent.nickname
        );
        assert_eq!(
            error_message(&response).as_deref(),
            Some("Recipient doesn't accept chat messages"),
            "the refusal names the flag"
        );
    }
    for nobody in [
        new_id().to_string(),
        "not-a-uuid".to_owned(),
        dave.id.to_string(),
    ] {
        let response = post_empty(&app, &token, &open_path(&nobody)).await;
        assert_eq!(
            response.status,
            StatusCode::NOT_FOUND,
            "`{nobody}` is no one to chat with"
        );
        assert_eq!(
            error_message(&response).as_deref(),
            Some("Record not found"),
            "the one 404 wording on `{nobody}`"
        );
    }

    let remote = post_empty(&app, &token, &open_path(&erin.id.to_string())).await;
    assert_eq!(
        remote.status,
        StatusCode::OK,
        "a remote mirror at the flag default accepts chats: {}",
        remote.text()
    );
    assert_eq!(
        count_chats(&pool).await,
        1,
        "and the mirror is the only pair the refusals left unwritten"
    );
}

/// `GET /api/v1/pleroma/chats/{id}` answers the full `Chat` entity — the
/// owner's unread count and newest message included — and one 404 to
/// everyone else: the other participant's row of the same conversation, an
/// unknown id and a malformed id are indistinguishable.
#[tokio::test]
async fn show_answers_the_chat_to_its_owner_alone() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = fixture(&pool, "alice", READ_SCOPE).await;
    let (bob, token) = fixture(&pool, "bob", READ_SCOPE).await;
    let (_, alice_ref, bob_ref) = send(&pool, &alice, &bob, "look").await;
    let app = common::test_app(pool.clone());

    let shown = get_as(&app, &token, &format!("{ROOT}/{}", bob_ref.chat_id)).await;
    assert_eq!(
        shown.status,
        StatusCode::OK,
        "the owner shows their chat: {}",
        shown.text()
    );
    let value = shown.json_value();
    assert_eq!(json_keys(&value), CHAT_KEYS, "exactly the Chat keys");
    assert_eq!(value["id"], bob_ref.chat_id.to_string());
    assert_eq!(
        value["account"]["id"],
        alice.id.to_string(),
        "the entity's account is the other side"
    );
    assert_eq!(value["unread"], 1, "bob has not read alice's message");
    assert_eq!(
        value["last_message"]["id"],
        bob_ref.id.to_string(),
        "the newest reference is the entity's last message"
    );
    assert_eq!(value["last_message"]["content"], "<p>look</p>");

    let own = get_as(&app, &alice_token, &format!("{ROOT}/{}", alice_ref.chat_id)).await;
    assert_eq!(own.status, StatusCode::OK, "alice shows her own row");
    assert_eq!(
        own.json_value()["unread"],
        0,
        "which has read what she wrote"
    );

    let stranger = get_as(&app, &alice_token, &format!("{ROOT}/{}", bob_ref.chat_id)).await;
    assert_eq!(
        stranger.status,
        StatusCode::NOT_FOUND,
        "alice cannot show bob's row of the same conversation"
    );
    assert_eq!(
        error_message(&stranger).as_deref(),
        Some("Record not found"),
        "the plain 404 wording"
    );
    for junk in [new_id().to_string(), "not-a-uuid".to_owned()] {
        let missing = get_as(&app, &token, &format!("{ROOT}/{junk}")).await;
        assert_eq!(
            missing.status,
            StatusCode::NOT_FOUND,
            "`{junk}` names no chat"
        );
    }
}
