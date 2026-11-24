//! Posting a chat message over HTTP (WBE-147): `POST …/messages` taking
//! `content` and/or `media_id`, attaching the sender's own unclaimed media,
//! and refusing bodies with nothing to send, oversize content and
//! unattachable media each with its own 400.

use axum::http::StatusCode;
use serde_json::json;
use wired_be::util::id::new_id;

use super::messages_support::{post_message, poster};
use super::support::{
    MESSAGE_KEYS, claim_media, count_messages, get_as, insert_media, messages_path, open_chat,
};
use crate::common;

/// `POST …/chats/{id}/messages` with `content`: 200 with the sender's
/// [`ChatMessage`] entity — the reference id of the caller's own chat, the
/// rendered text, `unread: false` because the sender has read what they
/// wrote — and a urlencoded form body posts just the same.
#[tokio::test]
async fn posting_a_message_answers_the_senders_entity() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = poster(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let chat = open_chat(&pool, &alice, &bob).await.id.to_string();
    let app = common::test_app(pool.clone());

    let posted = post_message(
        &app,
        &token,
        &chat,
        json!({"content": "hello *there* <b>x</b>"}),
        None,
    )
    .await;
    assert_eq!(
        posted.status,
        StatusCode::OK,
        "posting answers 200: {}",
        posted.text()
    );
    assert_eq!(
        posted.header("content-type"),
        Some("application/json"),
        "the entity is JSON"
    );
    let value = posted.json_value();
    assert_eq!(
        common::sorted_keys(&value),
        MESSAGE_KEYS,
        "exactly the ChatMessage keys"
    );
    assert_eq!(
        value["content"], "<p>hello *there* &lt;b&gt;x&lt;/b&gt;</p>",
        "escaped plain text, linkified, never markdown"
    );
    assert_eq!(value["chat_id"], chat, "the caller's own chat");
    assert_eq!(
        value["account_id"],
        alice.id.to_string(),
        "the sender's account id"
    );
    assert_eq!(
        value["unread"], false,
        "the sender has read what they wrote"
    );
    assert_eq!(value["emojis"], json!([]), "nothing maps chat shortcodes");
    assert!(
        value["attachment"].is_null(),
        "a text message carries nothing"
    );
    assert!(value["card"].is_null(), "and no rich-media card");
    assert!(
        value["created_at"]
            .as_str()
            .is_some_and(|stamp| stamp.ends_with('Z')),
        "a timestamp in the Mastodon format"
    );
    let listed = get_as(&app, &token, &messages_path(&chat))
        .await
        .json_value();
    assert_eq!(
        common::ids_of(&listed),
        vec![value["id"].as_str().expect("a reference id")],
        "the entity carries the reference id the list reads back by"
    );

    let form = common::TestRequest::post(&messages_path(&chat))
        .bearer(&token)
        .form(&[("content", "typed")])
        .send(&app)
        .await;
    assert_eq!(form.status, StatusCode::OK, "form bodies post alike");
    assert_eq!(form.json_value()["content"], "<p>typed</p>");
}

/// `media_id` attaches the sender's own unclaimed image, video or audio row
/// to the message — with or without `content`, and a media-only message
/// keeps both text columns empty — rendered as the full `Attachment`
/// entity beside it.
#[tokio::test]
async fn posted_media_attaches_to_the_message() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = poster(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let chat = open_chat(&pool, &alice, &bob).await.id.to_string();
    let app = common::test_app(pool.clone());

    let image = insert_media(&pool, &alice, "image/png").await;
    let picture = post_message(
        &app,
        &token,
        &chat,
        json!({"media_id": image.to_string()}),
        None,
    )
    .await;
    assert_eq!(
        picture.status,
        StatusCode::OK,
        "media alone is a message: {}",
        picture.text()
    );
    let value = picture.json_value();
    assert_eq!(value["content"], "", "a media-only message stays empty");
    assert_eq!(
        value["attachment"]["id"],
        image.to_string(),
        "the media row is attached"
    );
    assert_eq!(value["attachment"]["type"], "image", "as an Attachment");

    for (mime, want) in [("video/mp4", "video"), ("audio/mpeg", "audio")] {
        let media = insert_media(&pool, &alice, mime).await;
        let both = post_message(
            &app,
            &token,
            &chat,
            json!({"content": format!("watch the {want}"), "media_id": media.to_string()}),
            None,
        )
        .await;
        assert_eq!(both.status, StatusCode::OK, "{mime} is chat media");
        let attached = both.json_value();
        assert_eq!(
            attached["content"],
            format!("<p>watch the {want}</p>"),
            "text and"
        );
        assert_eq!(
            attached["attachment"]["id"],
            media.to_string(),
            "media ride together"
        );
    }
    assert_eq!(count_messages(&pool).await, 3, "three messages, no more");
}

/// A body with neither `content` nor `media_id` is the 400
/// `{"error":"Cannot send an empty chat message"}` — an empty JSON object,
/// unrelated keys, explicit nulls and a completely absent body all alike —
/// and nothing behind a refusal is written.
#[tokio::test]
async fn posting_refuses_bodies_with_nothing_to_send() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = poster(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let chat = open_chat(&pool, &alice, &bob).await.id.to_string();
    let app = common::test_app(pool.clone());

    for body in [
        json!({}),
        json!({"other": "x"}),
        json!({"content": null, "media_id": null}),
    ] {
        let response = post_message(&app, &token, &chat, body.clone(), None).await;
        assert_eq!(
            response.status,
            StatusCode::BAD_REQUEST,
            "`{body}` is nothing to send: {}",
            response.text()
        );
        assert_eq!(
            response.error_field().as_deref(),
            Some("Cannot send an empty chat message"),
            "the ticket's wording"
        );
    }
    let bare = common::TestRequest::post(&messages_path(&chat))
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(
        bare.status,
        StatusCode::BAD_REQUEST,
        "an empty body is an empty object"
    );
    assert_eq!(
        bare.error_field().as_deref(),
        Some("Cannot send an empty chat message"),
        "and reads as one"
    );
    assert_eq!(count_messages(&pool).await, 0, "the refusals wrote nothing");
}

/// `media_id` naming no media the poster may attach is the one 400
/// `{"error":"Media not found"}` whichever way it fails — unknown id,
/// malformed id text, another user's row, a row a status already claimed,
/// or a file that is none of image, video or audio — and the refusal says
/// nothing about which of them the id was.
#[tokio::test]
async fn unattachable_media_is_media_not_found() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = poster(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let chat = open_chat(&pool, &alice, &bob).await.id.to_string();
    let foreign = insert_media(&pool, &bob, "image/png").await;
    let claimed = insert_media(&pool, &alice, "image/png").await;
    claim_media(&pool, claimed, &alice).await;
    let document = insert_media(&pool, &alice, "application/pdf").await;
    let app = common::test_app(pool.clone());

    for junk in [
        new_id().to_string(),
        "not-a-uuid".to_owned(),
        foreign.to_string(),
        claimed.to_string(),
        document.to_string(),
    ] {
        for body in [
            json!({"media_id": &junk}),
            json!({"content": "look", "media_id": &junk}),
        ] {
            let response = post_message(&app, &token, &chat, body, None).await;
            assert_eq!(
                response.status,
                StatusCode::BAD_REQUEST,
                "`{junk}` is no chat media for alice: {}",
                response.text()
            );
            assert_eq!(
                response.error_field().as_deref(),
                Some("Media not found"),
                "one wording on `{junk}`"
            );
        }
    }
    assert_eq!(count_messages(&pool).await, 0, "and nothing was written");
}

/// `content` longer than the `MAX_CHAT_MESSAGE_CHARS` setting (default
/// 5000, counted in characters and not bytes) is the 400
/// `{"error":"The message is too long"}`; the limit itself posts.
#[tokio::test]
async fn content_over_the_chat_limit_is_too_long() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = poster(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let chat = open_chat(&pool, &alice, &bob).await.id.to_string();
    let app = common::test_app(pool.clone());

    let over = "\u{20ac}".repeat(5_001);
    let refused = post_message(&app, &token, &chat, json!({"content": over}), None).await;
    assert_eq!(
        refused.status,
        StatusCode::BAD_REQUEST,
        "one character over 5000 is refused"
    );
    assert_eq!(
        refused.error_field().as_deref(),
        Some("The message is too long"),
        "the ticket's wording"
    );

    let exactly = "\u{20ac}".repeat(5_000);
    let accepted = post_message(&app, &token, &chat, json!({"content": exactly}), None).await;
    assert_eq!(
        accepted.status,
        StatusCode::OK,
        "the limit itself posts: {}",
        accepted.text()
    );
    assert_eq!(count_messages(&pool).await, 1, "and only that one");
}

/// A lowered `MAX_CHAT_MESSAGE_CHARS` setting is what the endpoint
/// measures: six characters over a limit of five are the same 400, five
/// still post, and a media message's content is bounded the same way.
#[tokio::test]
async fn the_chat_length_setting_gates_the_endpoint() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = poster(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let chat = open_chat(&pool, &alice, &bob).await.id.to_string();
    let mut config = common::test_config();
    config.max_chat_message_chars = 5;
    let app = common::test_app_with_config(pool.clone(), config);

    let refused = post_message(&app, &token, &chat, json!({"content": "abcdef"}), None).await;
    assert_eq!(
        refused.status,
        StatusCode::BAD_REQUEST,
        "six characters over a limit of five: {}",
        refused.text()
    );
    assert_eq!(
        refused.error_field().as_deref(),
        Some("The message is too long"),
        "the same wording, the operator's number"
    );

    let media = insert_media(&pool, &alice, "image/png").await;
    let with_media = post_message(
        &app,
        &token,
        &chat,
        json!({"content": "abcdef", "media_id": media.to_string()}),
        None,
    )
    .await;
    assert_eq!(
        with_media.status,
        StatusCode::BAD_REQUEST,
        "an attachment does not excuse length"
    );

    let accepted = post_message(&app, &token, &chat, json!({"content": "abcde"}), None).await;
    assert_eq!(
        accepted.status,
        StatusCode::OK,
        "five characters fit the lowered setting"
    );
}
