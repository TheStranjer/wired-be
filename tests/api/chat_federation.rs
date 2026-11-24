//! Inbound `Create`/`Delete` of a `ChatMessage` over federation (WBE-175):
//! the dispatcher's own entry point [`handle_incoming`] folding the fixture
//! activities, and the rows and rendered entities the delivery leaves
//! behind (`src/federation/handlers/chat.rs`).
//!
//! Every test delivers a `Create{ChatMessage}` (and, for the withdrawal
//! tests, a `Delete` of it) through the real dispatcher — the same job the
//! inbox routes enqueue, run in-process with the signer's stored mirror as
//! its own row — and reads the result from the database and the client
//! API. What this pins, per the ticket:
//!
//! - a delivered message creates the pair's two chat rows, stores the
//!   sanitized text at the object's `published` time under the object's id,
//!   leaves the recipient's side unread and the sender's read, and raises
//!   exactly one `pleroma:chat_mention` notification whose entity renders
//!   the reader's own `chat_message` and no `status`;
//! - a delivered attachment lands as an unowned remote `media_attachments`
//!   row and renders on that entity;
//! - a blocked sender and a recipient with chats switched off get nothing;
//! - the same object delivered twice — through one activity id and through
//!   a renamed envelope — is one message and one notification;
//! - only the sender's `Delete` withdraws: the row becomes a tombstone and
//!   the notification goes with it, a bare-id `Delete` withdraws it the
//!   same way, and a stranger's `Delete` leaves the message standing.

#![cfg(test)]

use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::models::{
    block, chat, media_attachment,
    notification::TYPE_CHAT_MENTION,
    user::{self, User},
};

use crate::common::{self, deliver, delivering_state};

/// The remote author every fixture delivers as.
const BOB: &str = "https://remote.example/users/bob";

/// A `Create` of a `ChatMessage` from Bob to the account at
/// `recipient_ap`: object id and activity id named by `n`, text
/// `content`, and the object fields `extra` merges over the fixture.
fn chat_create(recipient_ap: &str, n: &str, content: &str, extra: Value) -> Value {
    let object = json!({
        "id": format!("https://remote.example/objects/chat-{n}"),
        "type": "ChatMessage",
        "attributedTo": BOB,
        "to": [recipient_ap],
        "content": content,
        "published": "2024-06-01T10:00:00.000Z",
    });
    let object = if let Value::Object(fields) = extra {
        let mut merged = object.as_object().expect("the fixture is a map").clone();
        for (key, value) in fields {
            let _previous = merged.insert(key, value);
        }
        Value::Object(merged)
    } else {
        object
    };
    json!({
        "id": format!("https://remote.example/activities/chat-{n}"),
        "type": "Create",
        "actor": BOB,
        "to": [recipient_ap],
        "object": object,
    })
}

/// A `Delete` by `actor` naming the embedded object id `object_ap` as a
/// `ChatMessage`, its own ids named by `n`.
fn chat_delete(actor: &str, object_ap: &str, n: &str) -> Value {
    json!({
        "id": format!("https://remote.example/activities/chat-del-{n}"),
        "type": "Delete",
        "actor": actor,
        "object": { "id": object_ap, "type": "ChatMessage" },
    })
}

/// `user_id`'s side of the pair's chat, if the delivery created one.
async fn chat_row(pool: &PgPool, user_id: Uuid, recipient_id: Uuid) -> Option<chat::Chat> {
    sqlx::query_as::<_, chat::Chat>(
        "SELECT id, user_id, recipient_id, pinned, inserted_at, updated_at \
           FROM chats WHERE user_id = $1 AND recipient_id = $2",
    )
    .bind(user_id)
    .bind(recipient_id)
    .fetch_optional(pool)
    .await
    .expect("the chat row reads")
}

/// `user_id`'s notification rows, oldest first, as
/// `(type, status_id, chat_message_id)` triples.
async fn note_rows(pool: &PgPool, user_id: Uuid) -> Vec<(String, Option<Uuid>, Option<Uuid>)> {
    use sqlx::Row as _;
    let rows = sqlx::query(
        "SELECT type, status_id, chat_message_id \
           FROM notifications WHERE user_id = $1 ORDER BY id",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
    .expect("the notification rows read");
    rows.iter()
        .map(|row| {
            (
                row.get::<String, _>("type"),
                row.get::<Option<Uuid>, _>("status_id"),
                row.get::<Option<Uuid>, _>("chat_message_id"),
            )
        })
        .collect()
}

/// How many `chat_messages` rows carry object id `ap_id`, tombstone
/// included.
async fn message_rows(pool: &PgPool, ap_id: &str) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM chat_messages WHERE ap_id = $1")
        .bind(ap_id)
        .fetch_one(pool)
        .await
        .expect("the message count reads")
}

/// Alice (local) and Bob (the stored mirror of the fixture's sender).
async fn pair(pool: &PgPool) -> (User, User) {
    let alice = common::create_local_user(pool, "alice").await;
    let bob = common::create_remote_user(pool, "bob", "remote.example").await;
    (alice, bob)
}

/// The stored message delivered under object id `object_ap`, failing the
/// test when nothing is there.
async fn stored_message(pool: &PgPool, object_ap: &str) -> chat::ChatMessage {
    chat::message_by_ap_id(pool, object_ap)
        .await
        .expect("the message read")
        .expect("the message is stored")
}

/// Alice's `pleroma:chat_mention` from the notification API, as rendered
/// JSON, failing the test when the inbox carries none.
async fn rendered_mention(pool: &PgPool, alice: &User) -> Value {
    let token = common::create_app_and_token(pool, alice, "read write").await;
    let response = common::TestRequest::get("/api/v1/notifications")
        .bearer(&token)
        .send(&common::test_app(pool.clone()))
        .await;
    common::assert_answer(
        &response,
        axum::http::StatusCode::OK,
        "the chat mention inbox",
    );
    let list = response.json_value();
    list.as_array()
        .expect("the listing is an array")
        .iter()
        .find(|n| n["type"] == json!(TYPE_CHAT_MENTION))
        .expect("the chat mention renders")
        .clone()
}

/// A delivered chat message becomes the pair's chat, a sanitized message
/// at its published time, one unread mark on the recipient's side, and a
/// `pleroma:chat_mention` the client API renders with the embedded
/// `chat_message` and no `status`.
#[tokio::test]
async fn a_delivered_chat_message_lands_with_a_notification() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, bob) = pair(&pool).await;
    let state = delivering_state(&pool);

    deliver(
        &state,
        &chat_create(
            &alice.ap_id,
            "hello",
            "<p>hello <script>alert(1)</script>alice</p>",
            json!({}),
        ),
        &bob,
    )
    .await;

    // Both sides' chat rows exist and name the other participant.
    let alice_chat = chat_row(&pool, alice.id, bob.id)
        .await
        .expect("alice's chat row");
    let bob_chat = chat_row(&pool, bob.id, alice.id)
        .await
        .expect("bob's chat row");

    // The message: sanitized, stripped, under the object id, at the
    // object's published time, sent by the stored mirror of its actor.
    let message = stored_message(&pool, "https://remote.example/objects/chat-hello").await;
    assert_eq!(message.sender_id, bob.id, "the sender is the stored actor");
    assert_eq!(message.content, "<p>hello alice</p>", "the script is gone");
    assert_eq!(
        message.raw_text, "hello alice\n",
        "and the raw text follows"
    );
    assert!(
        !message.content.contains("alert"),
        "even the script's contents went"
    );
    let published: DateTime<Utc> = "2024-06-01T10:00:00Z"
        .parse()
        .expect("the fixture time parses");
    assert_eq!(
        message.inserted_at, published,
        "the object's published time is the message's creation time"
    );

    // Unread for the recipient, read for the sender — his own words.
    assert_eq!(
        chat::unread_count(&pool, alice_chat.id)
            .await
            .expect("unread count reads"),
        1
    );
    assert_eq!(
        chat::unread_count(&pool, bob_chat.id)
            .await
            .expect("unread count reads"),
        0
    );

    // Exactly one notification, of the chat-mention type, naming the
    // message and no status.
    let notes = note_rows(&pool, alice.id).await;
    assert_eq!(notes.len(), 1, "the recipient is told once");
    assert_eq!(notes[0].0, TYPE_CHAT_MENTION);
    assert_eq!(notes[0].1, None, "a chat mention names no status");
    assert_eq!(notes[0].2, Some(message.id), "and names the message");

    // Nothing leaked to the sender's notification feed.
    assert!(
        note_rows(&pool, bob.id).await.is_empty(),
        "the sender hears nothing of his own message"
    );
}

/// The delivered chat mention renders through the notification API as the
/// reader's own view: an embedded `chat_message` keyed by the reader's
/// reference, and no `status`.
#[tokio::test]
async fn the_chat_mention_entity_renders_the_readers_view() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, bob) = pair(&pool).await;
    let state = delivering_state(&pool);

    deliver(
        &state,
        &chat_create(&alice.ap_id, "render", "hello", json!({})),
        &bob,
    )
    .await;
    let message = stored_message(&pool, "https://remote.example/objects/chat-render").await;
    let alice_chat = chat_row(&pool, alice.id, bob.id)
        .await
        .expect("alice's chat row");

    let notice = rendered_mention(&pool, &alice).await;
    assert_eq!(
        notice["group_key"],
        json!(format!(
            "ungrouped-{}",
            notice["id"].as_str().expect("a notification id")
        )),
        "a chat mention stands in its own group"
    );
    assert!(notice.get("status").is_none(), "and renders no status key");
    let rendered = &notice["chat_message"];
    assert_eq!(rendered["content"], json!("hello"), "the stored html");
    assert_eq!(
        rendered["created_at"],
        json!("2024-06-01T10:00:00.000Z"),
        "at the delivered time"
    );
    assert_eq!(
        rendered["chat_id"],
        json!(alice_chat.id.to_string()),
        "under the reader's own chat"
    );
    assert_eq!(
        rendered["account_id"],
        json!(bob.id.to_string()),
        "from the sender's account"
    );
    assert_eq!(
        rendered["unread"],
        json!(true),
        "as the reader's unread message"
    );
    assert_eq!(rendered["card"], json!(null), "no rich-media card");
    assert_eq!(rendered["emojis"], json!([]), "and no mapped emoji yet");
    assert_eq!(
        rendered["attachment"],
        json!(null),
        "a message without media carries a null attachment"
    );
    let reference = chat::message_reference_for_user(&pool, alice.id, message.id)
        .await
        .expect("the reader's reference reads")
        .expect("the reader holds a reference");
    assert_eq!(
        rendered["id"],
        json!(reference.id.to_string()),
        "the entity's id is the reader's reference id, not the message row's"
    );
}

/// A delivered attachment becomes an unowned remote media row and renders
/// on the notification's `chat_message`.
#[tokio::test]
async fn a_delivered_attachment_lands_as_remote_media() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, bob) = pair(&pool).await;
    let state = delivering_state(&pool);

    let activity = chat_create(
        &alice.ap_id,
        "pic",
        "<p>look at this</p>",
        json!({
            "attachment": [{
                "type": "Image",
                "mediaType": "image/png",
                "url": "https://remote.example/media/chat-fix.png",
                "name": "a picture",
                "blurhash": "eDLUkIae0kayE3RQWAae",
                "width": 320,
                "height": 240,
            }],
        }),
    );
    deliver(&state, &activity, &bob).await;

    let message = stored_message(&pool, "https://remote.example/objects/chat-pic").await;
    let media_id = message
        .attachment_media_id
        .expect("the message carries its media");
    let media = media_attachment::get_by_id(&pool, media_id)
        .await
        .expect("the media read")
        .expect("the media row exists");
    assert_eq!(media.user_id, None, "nobody uploaded it, so nobody owns it");
    assert_eq!(media.status_id, None, "and it belongs to no status");
    assert_eq!(
        media.remote_url.as_deref(),
        Some("https://remote.example/media/chat-fix.png"),
        "the document url names its origin"
    );
    assert_eq!(
        media.url, "https://remote.example/media/chat-fix.png",
        "and serves until mirrored"
    );
    assert_eq!(media.content_type, "image/png", "as declared");
    assert_eq!(
        media.description.as_deref(),
        Some("a picture"),
        "with its alt text"
    );
    assert_eq!(
        media.blurhash.as_deref(),
        Some("eDLUkIae0kayE3RQWAae"),
        "and placeholder"
    );
    assert_eq!(media.width, Some(320), "and the stated dimensions");
    assert_eq!(media.height, Some(240), "both");

    // The notification entity carries the attachment as a media entity.
    let notice = rendered_mention(&pool, &alice).await;
    assert_eq!(
        notice["chat_message"]["attachment"]["type"],
        json!("image"),
        "typed from the media type"
    );
    assert_eq!(
        notice["chat_message"]["attachment"]["url"],
        json!("https://remote.example/media/chat-fix.png"),
        "the remote url is served as the attachment url"
    );
}

/// An account the recipient has blocked delivers nothing at all.
#[tokio::test]
async fn a_blocked_sender_delivers_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, bob) = pair(&pool).await;
    let _blocked = block::block(&pool, &common::test_config().base_url, &alice, &bob)
        .await
        .expect("alice blocks bob");
    let state = delivering_state(&pool);

    deliver(
        &state,
        &chat_create(&alice.ap_id, "blocked", "hi", json!({})),
        &bob,
    )
    .await;

    assert_eq!(
        message_rows(&pool, "https://remote.example/objects/chat-blocked").await,
        0,
        "the blocked message is never stored"
    );
    assert!(
        chat_row(&pool, alice.id, bob.id).await.is_none(),
        "and no chat opened"
    );
    assert!(
        note_rows(&pool, alice.id).await.is_empty(),
        "and nobody was told"
    );
}

/// A user who switched chat messages off is not a recipient.
#[tokio::test]
async fn a_recipient_with_chats_off_gets_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user_with(&pool, "alice", |u| user::NewUser {
        accepts_chat_messages: user::Flag::new(false),
        ..u
    })
    .await;
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let state = delivering_state(&pool);

    deliver(
        &state,
        &chat_create(&alice.ap_id, "quiet", "hi", json!({})),
        &bob,
    )
    .await;

    assert_eq!(
        message_rows(&pool, "https://remote.example/objects/chat-quiet").await,
        0,
        "the refused message is never stored"
    );
    assert!(
        note_rows(&pool, alice.id).await.is_empty(),
        "and nobody was told"
    );
}

/// The same object delivered three times — the same activity twice (the
/// ledger's swallow) and under a renamed envelope (the handler's own
/// `ap_id` dedupe) — stores one message and raises one notification.
#[tokio::test]
async fn a_delivered_object_redelivered_is_one_message() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, bob) = pair(&pool).await;
    let state = delivering_state(&pool);

    let activity = chat_create(&alice.ap_id, "again", "hello", json!({}));
    deliver(&state, &activity, &bob).await;
    deliver(&state, &activity, &bob).await;

    let mut renamed = activity.clone();
    let _replaced = renamed
        .as_object_mut()
        .expect("the fixture is a map")
        .insert(
            "id".to_owned(),
            json!("https://remote.example/activities/chat-renamed"),
        );
    deliver(&state, &renamed, &bob).await;

    assert_eq!(
        message_rows(&pool, "https://remote.example/objects/chat-again").await,
        1,
        "three deliveries, one message row"
    );
    assert_eq!(
        chat::unread_count(
            &pool,
            chat_row(&pool, alice.id, bob.id)
                .await
                .expect("the chat row")
                .id
        )
        .await
        .expect("unread count reads"),
        1
    );
    assert_eq!(note_rows(&pool, alice.id).await.len(), 1, "and one telling");
}

/// The sender's `Delete` of a delivered message tombstones it, takes it
/// out of both sides' pages, and withdraws the notification with it.
#[tokio::test]
async fn the_sender_can_withdraw_a_delivered_message() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, bob) = pair(&pool).await;
    let state = delivering_state(&pool);

    deliver(
        &state,
        &chat_create(&alice.ap_id, "gone", "hello", json!({})),
        &bob,
    )
    .await;
    let object_ap = "https://remote.example/objects/chat-gone";
    let message = stored_message(&pool, object_ap).await;
    assert_eq!(note_rows(&pool, alice.id).await.len(), 1);

    deliver(&state, &chat_delete(BOB, object_ap, "gone"), &bob).await;

    // The row stands as a tombstone: deleted, text cleared, id reserved.
    let after = stored_message(&pool, object_ap).await;
    assert!(after.deleted_at.is_some(), "the withdrawal stamped it");
    assert!(
        after.content.is_empty() && after.raw_text.is_empty(),
        "the tombstone keeps no text"
    );
    assert!(
        message.deleted_at.is_none() && !message.content.is_empty(),
        "the delivered row was live before the Delete"
    );
    assert!(
        chat::message_reference_for_user(&pool, alice.id, message.id)
            .await
            .expect("the reference read")
            .is_some(),
        "the reader's reference survives and reads the absence through the live filter"
    );

    // Gone from the reader's page of the chat: no live message hangs on
    // the pair's chat any more.
    let alice_chat = chat_row(&pool, alice.id, bob.id)
        .await
        .expect("alice's chat row");
    let live = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM chat_message_references r \
           JOIN chat_messages m ON m.id = r.message_id \
          WHERE r.chat_id = $1 AND m.deleted_at IS NULL",
    )
    .bind(alice_chat.id)
    .fetch_one(&pool)
    .await
    .expect("the live count reads");
    assert_eq!(live, 0, "a withdrawn message leaves the chat's pages");

    // And the notification about it is gone.
    assert!(
        note_rows(&pool, alice.id).await.is_empty(),
        "the telling was withdrawn"
    );

    // The same Delete again, and one for a never-stored id, are clean
    // no-ops.
    deliver(&state, &chat_delete(BOB, object_ap, "gone-again"), &bob).await;
    deliver(
        &state,
        &chat_delete(BOB, "https://remote.example/objects/chat-never", "never"),
        &bob,
    )
    .await;
    assert_eq!(message_rows(&pool, object_ap).await, 1, "nothing doubled");
}

/// An account that is not the sender deletes nothing.
#[tokio::test]
async fn a_stranger_cannot_withdraw_anothers_message() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, bob) = pair(&pool).await;
    let mallory = common::create_remote_user(&pool, "mallory", "evil.example").await;
    let state = delivering_state(&pool);

    deliver(
        &state,
        &chat_create(&alice.ap_id, "keep", "hello", json!({})),
        &bob,
    )
    .await;

    deliver(
        &state,
        &chat_delete(
            "https://evil.example/users/mallory",
            "https://remote.example/objects/chat-keep",
            "steal",
        ),
        &mallory,
    )
    .await;

    let message = stored_message(&pool, "https://remote.example/objects/chat-keep").await;
    assert!(message.deleted_at.is_none(), "the message stands");
    assert_eq!(
        note_rows(&pool, alice.id).await.len(),
        1,
        "and its telling stands"
    );
}

/// A `Delete` whose object is only an id (Pleroma's shape) declares no
/// type and routes past the chat entry to the note fallback, which finds
/// the stored chat message under that id and withdraws it — writing no
/// status tombstone for the uri.
#[tokio::test]
async fn a_bare_id_delete_withdraws_the_message() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, bob) = pair(&pool).await;
    let state = delivering_state(&pool);

    deliver(
        &state,
        &chat_create(&alice.ap_id, "bare", "hello", json!({})),
        &bob,
    )
    .await;

    let bare = json!({
        "id": "https://remote.example/activities/chat-bare-del",
        "type": "Delete",
        "actor": BOB,
        "object": "https://remote.example/objects/chat-bare",
    });
    deliver(&state, &bare, &bob).await;

    let message = stored_message(&pool, "https://remote.example/objects/chat-bare").await;
    assert!(
        message.deleted_at.is_some(),
        "the bare-id Delete withdraws the message"
    );
    assert!(
        note_rows(&pool, alice.id).await.is_empty(),
        "and the telling goes with it"
    );
    let statuses = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM statuses WHERE uri = $1")
        .bind("https://remote.example/objects/chat-bare")
        .fetch_one(&pool)
        .await
        .expect("the status count reads");
    assert_eq!(statuses, 0, "and no status tombstone is reserved");
}

/// The delivered sender must be the object's attributed author and a live
/// account: a mismatched `attributedTo` and a deactivated sender's
/// delivery both store nothing.
#[tokio::test]
async fn a_sender_that_does_not_match_the_object_delivers_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, bob) = pair(&pool).await;
    let state = delivering_state(&pool);

    // Bob delivers a message attributed to somebody else: containment of
    // the embedded object against the actor refuses it.
    let misattributed = chat_create(
        &alice.ap_id,
        "liar",
        "hello",
        json!({"attributedTo": "https://remote.example/users/someone-else"}),
    );
    deliver(&state, &misattributed, &bob).await;
    assert_eq!(
        message_rows(&pool, "https://remote.example/objects/chat-liar").await,
        0,
        "the misattributed object is not stored"
    );

    // The same fixture from a deactivated account: nothing.
    let retired = common::create_local_user_with(&pool, "carol", |u| user::NewUser {
        nickname: "carol@remote.example".to_owned(),
        ap_id: "https://remote.example/users/carol".to_owned(),
        domain: Some("remote.example".to_owned()),
        local: user::Flag::new(false),
        is_active: user::Flag::new(false),
        ..u
    })
    .await;
    let from_retired = chat_create(&alice.ap_id, "ghost", "hello", json!({}));
    deliver(&state, &from_retired, &retired).await;
    assert_eq!(
        message_rows(&pool, "https://remote.example/objects/chat-ghost").await,
        0,
        "a deactivated sender stores nothing"
    );
    assert!(
        note_rows(&pool, alice.id).await.is_empty(),
        "and nobody was told"
    );
}
