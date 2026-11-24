//! The read markers over HTTP (WBE-148): `POST …/chats/{id}/read` marking
//! every reference up to `last_read_id` read and answering the `Chat` entity
//! with its new `unread` count, its 400s for a missing or malformed marker,
//! and `POST …/messages/{message_id}/read` marking one reference.

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::TEST_BASE_URL,
    models::chat::{self, NewChatMessage},
    util::id::new_id,
};

use super::support::{
    CHAT_KEYS, MESSAGE_KEYS, SCOPE, all_page, fixture, insert_attachment, marker_body,
    message_read_path, parse, post_empty, post_json, read_path, send, stored_unread, thread,
};
use crate::common;

/// The unread flags of chat `chat_id`, newest reference id first.
async fn unread_flags(pool: &PgPool, chat_id: Uuid) -> Vec<bool> {
    chat::messages_page(pool, chat_id, all_page())
        .await
        .expect("the model reads the chat back")
        .into_iter()
        .map(|reference| reference.unread)
        .collect()
}

/// A `POST …/read` asserted 200 and parsed as a `Chat` entity.
async fn read_chat(app: &Router, token: &str, chat: &str, last_read_id: Uuid) -> Value {
    let response = post_json(app, token, &read_path(chat), marker_body(last_read_id)).await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "a reader's POST /read answers 200: {}",
        response.text()
    );
    response.json_value()
}

/// The whole-chat read over HTTP: the up-to marker clears the references at
/// and below it (nothing else), answers the chat entity with the new unread
/// count, its newest message and the other participant's account, and stays
/// idempotent on a repeat; a newest-marker read empties the chat; a message
/// that arrives afterwards keeps its flag against the old marker.
#[tokio::test]
async fn chat_read_marks_up_to_the_marker_and_counts_what_remains() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let started = thread(&pool).await;
    let app = common::test_app(pool.clone());
    let chat = started.chat_id();

    let value = read_chat(&app, &started.token, &chat, started.refs[1]).await;

    assert_eq!(
        common::sorted_keys(&value),
        CHAT_KEYS,
        "exactly the Chat keys"
    );
    assert_eq!(value["id"], chat, "bob's own chat id");
    assert_eq!(value["unread"], 1, "only the newest message stays unread");
    assert_eq!(value["pinned"], false, "reading does not pin");
    assert_eq!(
        value["account"]["id"],
        started.alice.id.to_string(),
        "the entity's account is the other participant"
    );
    assert_eq!(value["account"]["acct"], "alice", "rendered as an Account");
    assert!(
        value["updated_at"]
            .as_str()
            .is_some_and(|stamp| stamp.ends_with('Z')),
        "the Mastodon time format"
    );
    let last = &value["last_message"];
    assert_eq!(
        common::sorted_keys(last),
        MESSAGE_KEYS,
        "the chat's newest message"
    );
    assert_eq!(last["id"], started.refs[2].to_string(), "by reference id");
    assert_eq!(last["unread"], true, "which the marker did not reach");

    let own = parse(&chat);
    assert_eq!(
        unread_flags(&pool, own).await,
        vec![true, false, false],
        "newest first: the two at and below the marker are read"
    );
    assert_eq!(stored_unread(&pool, own).await, 1);

    let again = read_chat(&app, &started.token, &chat, started.refs[1]).await;
    assert_eq!(again["unread"], 1, "the same marker reads nothing new");

    let all = read_chat(&app, &started.token, &chat, started.refs[2]).await;
    assert_eq!(all["unread"], 0, "the newest marker reads the chat out");
    assert_eq!(
        all["last_message"]["unread"], false,
        "and the entity agrees"
    );
    assert_eq!(
        unread_flags(&pool, own).await,
        vec![false, false, false],
        "every reference is read"
    );

    // A message that arrives after the marker keeps its flag: after a late
    // fourth message the old marker still reaches everything but the newest
    // id, so exactly one stays unread.
    let (_, _, late_ref) = send(&pool, &started.alice, &started.bob, "late").await;
    assert!(late_ref.unread, "the late reference is created unread");
    let late = read_chat(&app, &started.token, &chat, started.refs[2]).await;
    assert_eq!(late["unread"], 1, "the late message is unread");
    let stale = read_chat(&app, &started.token, &chat, started.refs[1]).await;
    assert_eq!(stale["unread"], 1, "and the stale marker does not reach it");
    let flags = unread_flags(&pool, own).await;
    assert_eq!(flags.len(), 4, "the chat lists all four messages");
    assert_eq!(
        flags.iter().filter(|unread| **unread).count(),
        1,
        "and exactly one is unread: {flags:?}"
    );
    assert_eq!(
        stored_unread(&pool, own).await,
        1,
        "which is what both the entity and the count report"
    );
}

/// A form body carries the marker just as well: Mastodon clients post
/// urlencoded, and Pleroma reads the parameter from the body either way.
#[tokio::test]
async fn a_form_body_marks_the_chat_read() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let started = thread(&pool).await;
    let app = common::test_app(pool.clone());
    let chat = started.chat_id();

    let response = common::TestRequest::post(&read_path(&chat))
        .bearer(&started.token)
        .form(&[("last_read_id", &started.refs[2].to_string())])
        .send(&app)
        .await;

    assert_eq!(response.status, StatusCode::OK, "form bodies work");
    assert_eq!(response.json_value()["unread"], 0, "and mark the chat");
    assert_eq!(stored_unread(&pool, parse(&chat)).await, 0);
}

/// An empty chat answers the read with `last_message: null`, and a marker
/// naming no reference of it is still a legal up-to point: the comparison,
/// not a lookup, decides.
#[tokio::test]
async fn an_empty_chat_reads_to_null_messages() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (bob, token) = fixture(&pool, "bob", SCOPE).await;
    let carol = common::create_local_user(&pool, "carol").await;
    let opened = chat::get_or_create(&pool, &bob, &carol)
        .await
        .expect("the fixture chat opens");
    let app = common::test_app(pool.clone());

    let value = read_chat(&app, &token, &opened.id.to_string(), new_id()).await;

    assert_eq!(value["unread"], 0, "nothing to read");
    assert!(value["last_message"].is_null(), "no last message");
    assert_eq!(value["account"]["id"], carol.id.to_string());
}

/// Without `last_read_id` the whole-chat read is Pleroma's fallback 400 —
/// and it is that 400 even when the chat is unknown: the body clause is
/// matched before any lookup. An empty body counts as no parameter.
#[tokio::test]
async fn a_read_without_the_marker_is_a_400() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let started = thread(&pool).await;
    let app = common::test_app(pool.clone());
    let chat = started.chat_id();

    for path in [read_path(&chat), read_path(&new_id().to_string())] {
        for response in [
            post_json(&app, &started.token, &path, json!({})).await,
            post_empty(&app, &started.token, &path).await,
            common::TestRequest::post(&path)
                .bearer(&started.token)
                .form(&[("other", "x")])
                .send(&app)
                .await,
        ] {
            assert_eq!(
                response.status,
                StatusCode::BAD_REQUEST,
                "no last_read_id is a 400 on {path}"
            );
            assert_eq!(
                response.error_field().as_deref(),
                Some("Missing parameter: last_read_id"),
                "Pleroma's wording on {path}"
            );
        }
    }
    assert_eq!(
        stored_unread(&pool, parse(&chat)).await,
        3,
        "and the refused reads marked nothing"
    );
}

/// A `last_read_id` that is present but not id text is refused once the chat
/// itself checks out: Pleroma's uuid cast has no answer for it either, so
/// this endpoint names the parameter in a 400.
#[tokio::test]
async fn a_malformed_marker_is_a_400() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let started = thread(&pool).await;
    let app = common::test_app(pool.clone());
    let chat = started.chat_id();

    for junk in ["not-a-uuid", "0189", ""] {
        let response = post_json(
            &app,
            &started.token,
            &read_path(&chat),
            json!({"last_read_id": junk}),
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::BAD_REQUEST,
            "`{junk}` is not an id"
        );
        assert_eq!(
            response.error_field().as_deref(),
            Some("Invalid parameter: last_read_id"),
            "the marker is named in the refusal"
        );
    }
    assert_eq!(
        stored_unread(&pool, parse(&chat)).await,
        3,
        "and the refusals marked nothing"
    );
}

/// `POST …/chats/{id}/messages/{message_id}/read` marks exactly one
/// reference — the up-to sweep leaves the others alone — and answers it as
/// the full `ChatMessage` entity, idempotently.
#[tokio::test]
async fn marking_one_message_read_returns_it() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let started = thread(&pool).await;
    let app = common::test_app(pool.clone());
    let chat = started.chat_id();
    let own = parse(&chat);

    let response = post_empty(
        &app,
        &started.token,
        &message_read_path(&chat, &started.refs[1].to_string()),
    )
    .await;

    assert_eq!(
        response.status,
        StatusCode::OK,
        "marking one message answers 200: {}",
        response.text()
    );
    let value = response.json_value();
    assert_eq!(
        common::sorted_keys(&value),
        MESSAGE_KEYS,
        "exactly the ChatMessage keys"
    );
    assert_eq!(value["id"], started.refs[1].to_string(), "the reference id");
    assert_eq!(value["chat_id"], chat, "inside bob's chat");
    assert_eq!(
        value["account_id"],
        started.alice.id.to_string(),
        "the writer's account"
    );
    assert_eq!(value["content"], "<p>two</p>", "the rendered text");
    assert!(
        value["created_at"]
            .as_str()
            .is_some_and(|stamp| stamp.ends_with('Z')),
        "a timestamp in the Mastodon format"
    );
    assert_eq!(value["unread"], false, "now read");
    assert_eq!(value["emojis"], json!([]), "nothing maps chat shortcodes");
    assert!(
        value["attachment"].is_null(),
        "and this message carries none"
    );
    assert!(value["card"].is_null(), "and no rich-media card");
    assert_eq!(
        unread_flags(&pool, own).await,
        vec![true, false, true],
        "only that one reference flipped"
    );

    let again = post_empty(
        &app,
        &started.token,
        &message_read_path(&chat, &started.refs[1].to_string()),
    )
    .await;
    assert_eq!(again.status, StatusCode::OK, "marking it twice is fine");
    assert_eq!(again.json_value()["unread"], false, "and changes nothing");
    assert_eq!(stored_unread(&pool, own).await, 2);

    let missing = message_read_path(&chat, &new_id().to_string());
    let unknown = post_empty(&app, &started.token, &missing).await;
    assert_eq!(
        unknown.status,
        StatusCode::NOT_FOUND,
        "an unknown reference is 404"
    );
    let junk = message_read_path(&chat, "not-a-uuid");
    let malformed = post_empty(&app, &started.token, &junk).await;
    assert_eq!(
        malformed.status,
        StatusCode::NOT_FOUND,
        "and so is a malformed one"
    );
}

/// A message's attachment rides along in the entity the read answers with,
/// on both routes that render one.
#[tokio::test]
async fn a_marked_message_carries_its_attachment() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, _) = fixture(&pool, "alice", "read").await;
    let (bob, token) = fixture(&pool, "bob", SCOPE).await;
    let media_id = insert_attachment(&pool, &alice).await;
    let mut conn = pool.begin().await.expect("a transaction begins");
    let sent = chat::add_message(
        &mut conn,
        TEST_BASE_URL,
        &alice,
        &bob,
        &NewChatMessage {
            attachment_media_id: Some(media_id),
            ..NewChatMessage::text("look")
        },
    )
    .await
    .expect("a message with an attachment is storable");
    conn.commit().await.expect("the transaction commits");
    let bob_ref = sent.2.expect("bob is local");
    let chat = bob_ref.chat_id.to_string();
    let app = common::test_app(pool.clone());

    let response = post_empty(
        &app,
        &token,
        &message_read_path(&chat, &bob_ref.id.to_string()),
    )
    .await;

    assert_eq!(
        response.status,
        StatusCode::OK,
        "the route answers: {}",
        response.text()
    );
    let value = response.json_value();
    assert_eq!(value["id"], bob_ref.id.to_string(), "that reference");
    assert_eq!(value["unread"], false, "marked read");
    assert_eq!(
        value["attachment"]["id"],
        media_id.to_string(),
        "with its media row rendered"
    );
    assert_eq!(value["attachment"]["type"], "image", "as an Attachment");
    let read = read_chat(&app, &token, &chat, bob_ref.id).await;
    assert_eq!(
        read["last_message"]["attachment"]["id"],
        media_id.to_string(),
        "and the chat entity carries it as the last message too"
    );
}

/// One reference is only markable through the chat that holds it: bob naming
/// carol's reference inside his own chat is a 404, and nobody's flags move.
#[tokio::test]
async fn a_message_of_another_chat_is_not_there() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let started = thread(&pool).await;
    let carol = common::create_local_user(&pool, "carol").await;
    let (_, _, carol_ref) = send(&pool, &started.alice, &carol, "not yours").await;
    let app = common::test_app(pool.clone());
    let chat = started.chat_id();

    let response = post_empty(
        &app,
        &started.token,
        &message_read_path(&chat, &carol_ref.id.to_string()),
    )
    .await;

    assert_eq!(
        response.status,
        StatusCode::NOT_FOUND,
        "a reference of another chat is 404: {}",
        response.text()
    );
    assert_eq!(
        response.error_field().as_deref(),
        Some("Record not found"),
        "the plain 404 wording"
    );
    assert_eq!(
        stored_unread(&pool, carol_ref.chat_id).await,
        1,
        "and carol's reference stayed unread"
    );
    assert_eq!(
        stored_unread(&pool, parse(&chat)).await,
        3,
        "and so did every one of bob's"
    );
}
