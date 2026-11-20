//! Listing chat messages over HTTP (WBE-147): `GET …/messages` answering
//! `ChatMessage` entities newest first in a 20-default page with the `Link`
//! header keyed on reference ids, the unread arrival of a posted message, and
//! the `Idempotency-Key` replay within the hour.

use axum::{Router, http::StatusCode};
use serde_json::json;

use super::messages_support::{expire_message, own_chat_id, post_message, post_text, poster};
use super::support::{
    MESSAGE_KEYS, READ_SCOPE, ROOT, count_messages, entity_ids, error_message, expected_link,
    fixture, get_as, json_keys, messages_path, open_chat, parse,
};
use crate::common;

/// Three messages posted into chat `chat` as `token`, oldest first, and
/// their reference ids in that order.
async fn post_three(app: &Router, token: &str, chat: &str) -> Vec<String> {
    let mut ids = Vec::new();
    for index in 0..3 {
        let posted = post_text(app, token, chat, &format!("message {index}"), None).await;
        assert_eq!(
            posted.status,
            StatusCode::OK,
            "the fixture posts: {}",
            posted.text()
        );
        ids.push(
            posted.json_value()["id"]
                .as_str()
                .expect("a reference id")
                .to_owned(),
        );
    }
    ids
}

/// `GET …/chats/{id}/messages` answers `ChatMessage` entities newest first
/// under the `Link` header keyed on the page's reference ids, walks back
/// with `max_id`, forward with `since_id`/`min_id`, defaults to a page of
/// twenty, refuses a malformed cursor, and answers an empty chat with the
/// bare array and no `Link` header.
#[tokio::test]
async fn the_message_list_orders_newest_first_and_pages_by_reference_id() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = poster(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let chat = open_chat(&pool, &alice, &bob).await.id.to_string();
    let app = common::test_app(pool.clone());
    let ids = post_three(&app, &token, &chat).await;
    let path = messages_path(&chat);

    let full = get_as(&app, &token, &path).await;
    let listed = full.json_value();
    assert_eq!(
        entity_ids(&listed),
        vec![ids[2].clone(), ids[1].clone(), ids[0].clone()],
        "newest first"
    );
    assert_eq!(
        listed[0]["content"], "<p>message 2</p>",
        "and it is the text"
    );
    let want = expected_link(&path, &ids[2], &ids[0]);
    assert_eq!(
        full.header("link"),
        Some(want.as_str()),
        "the cursors key on the page's reference ids"
    );

    let tail = get_as(&app, &token, &format!("{path}?limit=2&max_id={}", ids[1])).await;
    assert_eq!(
        entity_ids(&tail.json_value()),
        vec![ids[0].clone()],
        "max_id walks back through the chat"
    );
    let since = get_as(&app, &token, &format!("{path}?since_id={}", ids[0])).await;
    assert_eq!(
        entity_ids(&since.json_value()),
        vec![ids[2].clone(), ids[1].clone()],
        "since_id skips the cursor and everything older"
    );
    let forward = get_as(&app, &token, &format!("{path}?min_id={}", ids[0])).await;
    assert_eq!(
        entity_ids(&forward.json_value()),
        vec![ids[2].clone(), ids[1].clone()],
        "min_id answers the page after the cursor, newest first again"
    );

    let junk = get_as(&app, &token, &format!("{path}?max_id=not-a-uuid")).await;
    assert_eq!(
        junk.status,
        StatusCode::BAD_REQUEST,
        "a malformed cursor 400s"
    );
    assert_eq!(
        error_message(&junk).as_deref(),
        Some("Invalid id"),
        "with the pagination module's wording"
    );

    let carol = common::create_local_user(&pool, "carol").await;
    let quiet = open_chat(&pool, &alice, &carol).await.id.to_string();
    let empty = get_as(&app, &token, &messages_path(&quiet)).await;
    assert_eq!(
        empty.json_value(),
        json!([]),
        "a chatless chat lists the array"
    );
    assert_eq!(empty.header("link"), None, "and sends no Link header");
}

/// The message page defaults to twenty and takes any `limit` up to
/// [`MAX_LIMIT`]: twenty-one messages answer twenty unasked and all of them
/// on a page of twenty-five.
#[tokio::test]
async fn the_message_list_defaults_to_twenty() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = poster(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let chat = open_chat(&pool, &alice, &bob).await.id.to_string();
    let app = common::test_app(pool.clone());
    for index in 0..21 {
        let posted = post_message(
            &app,
            &token,
            &chat,
            json!({"content": format!("m{index}")}),
            None,
        )
        .await;
        assert_eq!(posted.status, StatusCode::OK, "fixture post {index}");
    }
    let path = messages_path(&chat);

    let default = get_as(&app, &token, &path).await.json_value();
    assert_eq!(entity_ids(&default).len(), 20, "the default page is 20");
    let asked = get_as(&app, &token, &format!("{path}?limit=25"))
        .await
        .json_value();
    assert_eq!(
        entity_ids(&asked).len(),
        21,
        "and 25 takes everything there is"
    );
}

/// What a posted message does to the other side: the recipient gets a
/// reference of their own chat — a different reference id in a different
/// chat — flagged unread, and their `Chat` entity counts it.
#[tokio::test]
async fn a_posted_message_arrives_unread_for_the_recipient() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = poster(&pool, "alice").await;
    let (bob, bobs_token) = fixture(&pool, "bob", READ_SCOPE).await;
    let chat = open_chat(&pool, &alice, &bob).await.id.to_string();
    let app = common::test_app(pool.clone());

    let posted = post_message(&app, &token, &chat, json!({"content": "ping"}), None).await;
    assert_eq!(
        posted.status,
        StatusCode::OK,
        "alice posts: {}",
        posted.text()
    );
    let senders_ref = posted.json_value()["id"]
        .as_str()
        .expect("a reference id")
        .to_owned();

    let bobs_chat = own_chat_id(&pool, bob.id).await.to_string();
    assert_ne!(
        bobs_chat, chat,
        "the recipient's side of the conversation is its own chat"
    );
    let listed = get_as(&app, &bobs_token, &messages_path(&bobs_chat)).await;
    assert_eq!(
        listed.status,
        StatusCode::OK,
        "and he may list it: {}",
        listed.text()
    );
    let messages = listed.json_value();
    assert_eq!(
        json_keys(&messages[0]),
        MESSAGE_KEYS,
        "his own ChatMessage entity"
    );
    assert_ne!(
        messages[0]["id"].as_str().expect("an id"),
        senders_ref,
        "each side holds its own reference to one message"
    );
    assert_eq!(messages[0]["unread"], true, "unread for him");
    assert_eq!(messages[0]["content"], "<p>ping</p>", "the same text");
    assert_eq!(
        messages[0]["account_id"],
        alice.id.to_string(),
        "from alice"
    );
    assert_eq!(messages[0]["chat_id"], bobs_chat, "in his chat");

    let shown = get_as(&app, &bobs_token, &format!("{ROOT}/{bobs_chat}")).await;
    assert_eq!(shown.json_value()["unread"], 1, "his chat counts it unread");
    assert_eq!(
        shown.json_value()["last_message"]["unread"],
        true,
        "and its last message says so too"
    );
}

/// An `Idempotency-Key` stored with a message replays it: the same key from
/// the same sender answers the message that key created (id and all, body
/// be damned) and writes nothing; a different key or none posts as usual;
/// outside the hour the key is fresh again. The entity echoes the key while
/// it was created with one, and only then.
#[tokio::test]
async fn an_idempotency_key_replays_its_message_within_the_hour() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = poster(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let chat = open_chat(&pool, &alice, &bob).await.id.to_string();
    let app = common::test_app(pool.clone());
    let mut wanted = MESSAGE_KEYS.to_vec();
    wanted.push("idempotency_key");
    wanted.sort_unstable();

    let first = post_text(&app, &token, &chat, "once", Some("key-1")).await;
    assert_eq!(first.status, StatusCode::OK, "keyed post: {}", first.text());
    let value = first.json_value();
    assert_eq!(json_keys(&value), wanted, "the entity adds the key");
    assert_eq!(value["idempotency_key"], "key-1", "echoed back verbatim");
    let first_id = value["id"].as_str().expect("a reference id").to_owned();

    let replay = post_text(&app, &token, &chat, "twice", Some("key-1")).await;
    assert_eq!(replay.status, StatusCode::OK, "a replay is no error");
    let again = replay.json_value();
    assert_eq!(again["id"], first_id, "the key answers its first message");
    assert_eq!(again["content"], "<p>once</p>", "and not the new body");

    let listed = get_as(&app, &token, &messages_path(&chat))
        .await
        .json_value();
    assert_eq!(
        entity_ids(&listed),
        vec![first_id.clone()],
        "the chat holds it once"
    );
    assert_eq!(
        listed[0]["idempotency_key"], "key-1",
        "read back with its key"
    );
    assert_eq!(count_messages(&pool).await, 1, "the replay wrote nothing");

    let other_key = post_text(&app, &token, &chat, "other", Some("key-2")).await;
    assert_ne!(
        other_key.json_value()["id"].as_str().expect("an id"),
        first_id,
        "another key is another message"
    );
    let no_key = post_text(&app, &token, &chat, "plain", None).await;
    assert!(
        no_key.json_value().get("idempotency_key").is_none(),
        "and a keyless message renders no key"
    );

    expire_message(&pool, parse(&first_id)).await;
    let late = post_text(&app, &token, &chat, "late", Some("key-1")).await;
    assert_eq!(late.status, StatusCode::OK);
    assert_ne!(
        late.json_value()["id"].as_str().expect("an id"),
        first_id,
        "outside the hour the key posts afresh"
    );
    assert_eq!(count_messages(&pool).await, 4, "once, other, plain, late");
}

/// The key belongs to its sender, not to a chat or an account name: the
/// same key replayed into another of the sender's chats answers the first
/// message and touches nothing else, and the same key from another user is
/// just another message.
#[tokio::test]
async fn an_idempotency_key_belongs_to_its_sender() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = poster(&pool, "alice").await;
    let (bob, bobs_token) = poster(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let with_bob = open_chat(&pool, &alice, &bob).await.id.to_string();
    let with_carol = open_chat(&pool, &alice, &carol).await.id.to_string();
    let app = common::test_app(pool.clone());

    let first = post_message(
        &app,
        &token,
        &with_bob,
        json!({"content": "one message"}),
        Some("shared"),
    )
    .await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.text());
    let first_id = first.json_value()["id"]
        .as_str()
        .expect("a reference id")
        .to_owned();

    let crossed = post_message(
        &app,
        &token,
        &with_carol,
        json!({"content": "elsewhere"}),
        Some("shared"),
    )
    .await;
    let value = crossed.json_value();
    assert_eq!(
        value["id"], first_id,
        "the sender's key replays its message"
    );
    assert_eq!(
        value["chat_id"], with_bob,
        "in the chat it was first posted into"
    );
    let carols_list = get_as(&app, &token, &messages_path(&with_carol)).await;
    assert_eq!(
        carols_list.json_value(),
        json!([]),
        "and the crossed chat received nothing"
    );

    let bobs_chat = own_chat_id(&pool, bob.id).await.to_string();
    let bobs_own = post_message(
        &app,
        &bobs_token,
        &bobs_chat,
        json!({"content": "mine"}),
        Some("shared"),
    )
    .await;
    assert_eq!(
        bobs_own.status,
        StatusCode::OK,
        "the same key is bob's to use: {}",
        bobs_own.text()
    );
    assert_ne!(
        bobs_own.json_value()["id"].as_str().expect("an id"),
        first_id,
        "and posts his own message with it"
    );
    assert_eq!(count_messages(&pool).await, 2, "alice's one, bob's one");
}
