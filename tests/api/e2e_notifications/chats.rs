//! Scenario (6): chats — the two sides of a chat agreeing on unread counts,
//! and the read that empties them while a stranger stays locked out.

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};

use super::support::{
    CHATS, CHATS_V2, NOT_FOUND, acted, entity_id, got, got_array, ids_of, posted, user,
};
use crate::common;

/// The `POST /api/v1/pleroma/chats/by-account-id/{id}` that gets or
/// creates the caller's chat with `peer`, answered as the `Chat` entity.
pub(super) async fn open_chat(app: &Router, token: &str, peer: &str) -> Value {
    acted(app, token, &format!("{CHATS}/by-account-id/{peer}")).await
}

/// The `POST /api/v1/pleroma/chats/{id}/messages` with `content`, answered
/// as the sender's `ChatMessage` entity.
pub(super) async fn send_message(app: &Router, token: &str, chat: &str, text: &str) -> Value {
    posted(
        app,
        token,
        &format!("{CHATS}/{chat}/messages"),
        &json!({ "content": text }),
    )
    .await
}

/// The `GET /api/v1/pleroma/chats/{id}`, answered as the `Chat` entity.
async fn show_chat(app: &Router, token: &str, chat: &str) -> Value {
    got(app, token, &format!("{CHATS}/{chat}")).await
}

/// The `GET` of `root` (either chat list) as the caller's one listed chat.
async fn listed_chat(app: &Router, token: &str, root: &str) -> Value {
    let mut page = got_array(app, token, root).await;
    assert_eq!(page.len(), 1, "{root} carries the caller's one chat");
    page.remove(0)
}

/// The `GET /api/v1/pleroma/chats/{id}/messages`, newest reference first.
pub(super) async fn chat_messages(app: &Router, token: &str, chat: &str) -> Vec<Value> {
    got_array(app, token, &format!("{CHATS}/{chat}/messages")).await
}

/// The `unread` count of the caller's chat `id`, through its show read.
pub(super) async fn chat_unread(app: &Router, token: &str, chat: &str) -> i64 {
    show_chat(app, token, chat).await["unread"]
        .as_i64()
        .expect("unread is a number")
}

/// Two local users exchange chat messages and every per-peer read — the
/// v1 list, the v2 list and the show — agrees on the same `unread` count
/// as the other side's view: alice's message is bob's one owed and bob's
/// reply is alice's, the sender never owes their own post, and each chat
/// names the other participant.
#[tokio::test]
async fn a_chat_pair_agrees_on_the_peers_unread_counts() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, alice_token) = user(&pool, "alice").await;
    let (bob, bob_token) = user(&pool, "bob").await;
    let alice_id = alice.id.to_string();
    let bob_id = bob.id.to_string();

    let hers = open_chat(&app, &alice_token, &bob_id).await;
    let restated = open_chat(&app, &alice_token, &bob_id).await;
    assert_eq!(restated, hers, "opening the same chat twice restates it");
    let her_chat = entity_id(&hers);
    let his_chat = entity_id(&open_chat(&app, &bob_token, &alice_id).await);
    assert_ne!(her_chat, his_chat, "each participant holds their own row");
    assert_eq!(hers["account"]["id"], json!(bob_id), "hers names bob");
    assert_eq!(hers["unread"], json!(0), "a fresh chat owes nothing");
    assert_eq!(
        show_chat(&app, &bob_token, &his_chat).await["account"]["id"],
        json!(alice_id),
        "and his names alice"
    );

    let first = send_message(&app, &alice_token, &her_chat, "hi from alice").await;
    assert_eq!(
        first["content"],
        json!("<p>hi from alice</p>"),
        "rendered, from her"
    );
    assert_eq!(first["account_id"], json!(alice_id), "under her account");
    assert_eq!(
        first["unread"],
        json!(false),
        "the sender has read what they wrote"
    );
    assert_eq!(
        chat_unread(&app, &bob_token, &his_chat).await,
        1,
        "bob owes it"
    );
    assert_eq!(
        listed_chat(&app, &bob_token, CHATS).await["unread"],
        json!(1),
        "the v1 list agrees"
    );
    assert_eq!(
        listed_chat(&app, &bob_token, CHATS_V2).await["unread"],
        json!(1),
        "and the v2 list"
    );
    assert_eq!(
        chat_unread(&app, &alice_token, &her_chat).await,
        0,
        "alice owes nothing yet"
    );

    let _second = send_message(&app, &bob_token, &his_chat, "hi back").await;
    assert_eq!(
        chat_unread(&app, &alice_token, &her_chat).await,
        1,
        "the reply lands unread for alice"
    );
    assert_eq!(
        chat_unread(&app, &bob_token, &his_chat).await,
        1,
        "and alice's message is still owed"
    );
    assert_eq!(
        chat_messages(&app, &alice_token, &her_chat)
            .await
            .iter()
            .map(|message| message["unread"].clone())
            .collect::<Vec<Value>>(),
        vec![json!(true), json!(false)],
        "newest first: bob's reply owed, her own read"
    );
}

/// `carol` neither lists nor reads the pair's chats nor posts into them:
/// both lists stay empty and every probe of `her_chat` is the shared 404.
async fn assert_stranger_locked_out(app: &Router, carol_token: &str, her_chat: &str) {
    assert!(
        got_array(app, carol_token, CHATS).await.is_empty(),
        "carol lists no chats"
    );
    assert!(
        got_array(app, carol_token, CHATS_V2).await.is_empty(),
        "on either list"
    );
    for path in [
        format!("{CHATS}/{her_chat}"),
        format!("{CHATS}/{her_chat}/messages"),
    ] {
        let refused = common::TestRequest::get(&path)
            .bearer(carol_token)
            .send(app)
            .await;
        common::assert_answer(
            &refused,
            StatusCode::NOT_FOUND,
            "carol's read of their chat",
        );
        common::assert_error(&refused, NOT_FOUND, &path);
    }
    let posting = common::TestRequest::post(&format!("{CHATS}/{her_chat}/messages"))
        .bearer(carol_token)
        .json(&json!({ "content": "let me in" }))
        .send(app)
        .await;
    common::assert_answer(
        &posting,
        StatusCode::NOT_FOUND,
        "carol's post into their chat",
    );
    common::assert_error(&posting, NOT_FOUND, "the foreign chat id");
}

/// The chat read endpoint moves the caller's marker only up to the posted
/// `last_read_id` — the count steps down, then empties — and a third user
/// neither lists nor reads the pair's chats nor posts into them.
#[tokio::test]
async fn the_chat_read_empties_the_count_and_a_stranger_is_locked_out() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, alice_token) = user(&pool, "alice").await;
    let (bob, bob_token) = user(&pool, "bob").await;
    let (_carol, carol_token) = user(&pool, "carol").await;
    let her_chat = entity_id(&open_chat(&app, &alice_token, &bob.id.to_string()).await);
    let his_chat = entity_id(&open_chat(&app, &bob_token, &alice.id.to_string()).await);

    let _first = send_message(&app, &alice_token, &her_chat, "one").await;
    let _second = send_message(&app, &bob_token, &his_chat, "two").await;
    let _third = send_message(&app, &alice_token, &her_chat, "three").await;
    assert_eq!(
        chat_unread(&app, &bob_token, &his_chat).await,
        2,
        "bob owes alice's one and three"
    );

    let refs = ids_of(&chat_messages(&app, &bob_token, &his_chat).await);
    assert_eq!(refs.len(), 3, "three references, newest first");
    let partial = posted(
        &app,
        &bob_token,
        &format!("{CHATS}/{his_chat}/read"),
        &json!({ "last_read_id": refs[2] }),
    )
    .await;
    assert_eq!(
        partial["unread"],
        json!(1),
        "the marker stops at the oldest owed message"
    );
    assert_eq!(
        chat_unread(&app, &bob_token, &his_chat).await,
        1,
        "and the count follows"
    );

    let full = posted(
        &app,
        &bob_token,
        &format!("{CHATS}/{his_chat}/read"),
        &json!({ "last_read_id": refs[0] }),
    )
    .await;
    assert_eq!(
        full["unread"],
        json!(0),
        "walking the marker to the newest empties it"
    );
    assert_eq!(
        chat_messages(&app, &bob_token, &his_chat)
            .await
            .iter()
            .filter(|message| message["unread"].as_bool().expect("a flag"))
            .count(),
        0,
        "no reference is unread anymore"
    );

    assert_stranger_locked_out(&app, &carol_token, &her_chat).await;
}
