//! Deleting a chat message and the message routes' gates over HTTP
//! (WBE-147): `DELETE …/messages/{message_id}` splitting as the model does,
//! the one-chat 404 of every message route, and the scopes they demand.

use axum::http::StatusCode;
use serde_json::json;
use wired_be::util::id::new_id;

use super::messages_support::{
    assert_all_answer, assert_all_say, assert_answer, assert_delete_404, assert_says, own_chat_id,
    post_message, post_text, poster,
};
use super::support::{
    MESSAGE_KEYS, READ_SCOPE, SCOPE, count_messages, delete_as, entity_ids, error_message, fixture,
    get_anonymous, get_as, json_keys, message_delete_path, messages_path, open_chat,
};
use crate::common;

/// `DELETE …/chats/{id}/messages/{message_id}` as the recipient removes
/// only his own reference: the answer is his view of the message as it was
/// (unread and all), the sender keeps a live copy, and a reference is only
/// deletable through the chat that holds it, once.
#[tokio::test]
async fn a_recipient_delete_only_removes_the_recipients_copy() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = poster(&pool, "alice").await;
    let (bob, bobs_token) = poster(&pool, "bob").await;
    let alices_chat = open_chat(&pool, &alice, &bob).await.id.to_string();
    let app = common::test_app(pool.clone());
    let posted = post_text(&app, &token, &alices_chat, "for both", None).await;
    assert_eq!(posted.status, StatusCode::OK, "{}", posted.text());
    let alice_ref = posted.json_value()["id"]
        .as_str()
        .expect("a reference id")
        .to_owned();
    let bobs_chat = own_chat_id(&pool, bob.id).await.to_string();
    let his = get_as(&app, &bobs_token, &messages_path(&bobs_chat))
        .await
        .json_value();
    let bob_ref = his[0]["id"].as_str().expect("his reference").to_owned();

    let removed = delete_as(
        &app,
        &bobs_token,
        &message_delete_path(&bobs_chat, &bob_ref),
    )
    .await;
    assert_eq!(
        removed.status,
        StatusCode::OK,
        "the recipient deletes: {}",
        removed.text()
    );
    let value = removed.json_value();
    assert_eq!(
        json_keys(&value),
        MESSAGE_KEYS,
        "the full entity, as he saw it"
    );
    assert_eq!(value["id"], bob_ref, "his reference id");
    assert_eq!(value["unread"], true, "still unread when it went");
    assert_eq!(value["content"], "<p>for both</p>", "with the text");

    assert_eq!(
        get_as(&app, &bobs_token, &messages_path(&bobs_chat))
            .await
            .json_value(),
        json!([]),
        "his list no longer holds it"
    );
    let hers = get_as(&app, &token, &messages_path(&alices_chat))
        .await
        .json_value();
    assert_eq!(
        entity_ids(&hers),
        vec![alice_ref.clone()],
        "alice keeps her copy"
    );
    assert_eq!(
        hers[0]["content"], "<p>for both</p>",
        "live: hiding somebody's copy is not withdrawing the message"
    );

    assert_delete_404(&app, &bobs_token, &bobs_chat, &bob_ref, "a second delete").await;
    assert_delete_404(
        &app,
        &bobs_token,
        &alices_chat,
        &alice_ref,
        "alice's chat is not bob's to delete in",
    )
    .await;
    assert_delete_404(
        &app,
        &token,
        &alices_chat,
        &bob_ref,
        "bob's reference is not alice's to delete through hers",
    )
    .await;
    assert_eq!(
        count_messages(&pool).await,
        1,
        "the refusals changed nothing"
    );
}

/// `DELETE` as the sender withdraws the message from everyone: their own
/// reference goes, the row becomes a tombstone, and the recipient's
/// listing hides it even though their reference survives.
#[tokio::test]
async fn a_sender_delete_withdraws_the_message_from_both_sides() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = poster(&pool, "alice").await;
    let (bob, bobs_token) = poster(&pool, "bob").await;
    let alices_chat = open_chat(&pool, &alice, &bob).await.id.to_string();
    let app = common::test_app(pool.clone());
    let posted = post_message(
        &app,
        &token,
        &alices_chat,
        json!({"content": "take it back"}),
        None,
    )
    .await;
    let alice_ref = posted.json_value()["id"]
        .as_str()
        .expect("a reference id")
        .to_owned();
    let bobs_chat = own_chat_id(&pool, bob.id).await.to_string();

    let withdrawn = delete_as(&app, &token, &message_delete_path(&alices_chat, &alice_ref)).await;
    assert_eq!(
        withdrawn.status,
        StatusCode::OK,
        "the sender withdraws: {}",
        withdrawn.text()
    );
    assert_eq!(
        withdrawn.json_value()["id"],
        alice_ref,
        "answering what went"
    );
    assert_eq!(
        get_as(&app, &token, &messages_path(&alices_chat))
            .await
            .json_value(),
        json!([]),
        "the sender's list is empty"
    );
    assert_eq!(
        get_as(&app, &bobs_token, &messages_path(&bobs_chat))
            .await
            .json_value(),
        json!([]),
        "and the recipient's no longer shows the tombstone"
    );

    let again = delete_as(&app, &token, &message_delete_path(&alices_chat, &alice_ref)).await;
    assert_eq!(
        again.status,
        StatusCode::NOT_FOUND,
        "withdrawing twice finds nothing"
    );
    let junk = delete_as(
        &app,
        &token,
        &message_delete_path(&alices_chat, "not-a-uuid"),
    )
    .await;
    assert_eq!(
        junk.status,
        StatusCode::NOT_FOUND,
        "and so does a malformed reference id"
    );
}

/// Foreign, unknown and malformed chat ids are one 404 on all three
/// message routes, and a chat that is the caller's own still answers: the
/// gate is ownership of the `{id}`, not the message behind it.
#[tokio::test]
async fn the_message_routes_answer_the_one_chat_404() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = poster(&pool, "alice").await;
    let (bob, bobs_token) = poster(&pool, "bob").await;
    let alices_chat = open_chat(&pool, &alice, &bob).await.id.to_string();
    let app = common::test_app(pool.clone());
    let posted = post_message(
        &app,
        &alice_token,
        &alices_chat,
        json!({"content": "mine"}),
        None,
    )
    .await;
    assert_eq!(posted.status, StatusCode::OK, "the fixture posts");

    for foreign in [
        alices_chat.clone(),
        new_id().to_string(),
        "not-a-uuid".to_owned(),
    ] {
        let listed = get_as(&app, &bobs_token, &messages_path(&foreign)).await;
        assert_eq!(
            listed.status,
            StatusCode::NOT_FOUND,
            "no listing of {foreign}"
        );
        assert_eq!(
            error_message(&listed).as_deref(),
            Some("Record not found"),
            "the plain 404 wording on {foreign}"
        );
        let attempted = post_message(
            &app,
            &bobs_token,
            &foreign,
            json!({"content": "into yours"}),
            None,
        )
        .await;
        assert_eq!(
            attempted.status,
            StatusCode::NOT_FOUND,
            "no posting into {foreign}"
        );
        let deleted = delete_as(
            &app,
            &bobs_token,
            &message_delete_path(&foreign, &new_id().to_string()),
        )
        .await;
        assert_eq!(
            deleted.status,
            StatusCode::NOT_FOUND,
            "no deleting through {foreign}"
        );
    }
    assert_eq!(
        count_messages(&pool).await,
        1,
        "every attempt was a 404 before any write"
    );

    let his_chat = own_chat_id(&pool, bob.id).await.to_string();
    let mine = get_as(&app, &bobs_token, &messages_path(&his_chat)).await;
    assert_eq!(
        mine.status,
        StatusCode::OK,
        "bob's own side of the same conversation answers him"
    );
    assert_eq!(mine.json_value().as_array().expect("array").len(), 1);
}

/// The message routes demand their scopes before anything else: the list
/// wants `read:chats`, the post and the delete want `write:chats`; no token
/// is the 401 "Invalid credentials", a bad one the 401 naming the token, an
/// app-only token the 403 demanding a user; the plain `read` and `write`
/// roots cover the sub-scopes, and a right-scoped stranger still gets the
/// ownership 404.
#[tokio::test]
async fn the_message_routes_demand_their_scopes() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, _) = poster(&pool, "alice").await;
    let (bob, _) = fixture(&pool, "bob", "read").await;
    let chat = open_chat(&pool, &alice, &bob).await.id.to_string();
    let (_, read_only) = fixture(&pool, "erin", READ_SCOPE).await;
    let (_, write_only) = fixture(&pool, "dave", SCOPE).await;
    let (_, unrelated) = fixture(&pool, "grace", "read:accounts").await;
    let app_only = common::create_app_only_token(&pool, "read:chats write:chats").await;
    let app = common::test_app(pool.clone());
    let path = messages_path(&chat);
    let one = format!("{path}/{}", new_id());

    assert_all_say(
        &[
            get_as(&app, &write_only, &path).await,
            get_as(&app, &unrelated, &path).await,
        ],
        "Insufficient permissions: read:chats.",
        "a writer and an unrelated scope read no messages",
    );
    assert_all_say(
        &[
            get_as(&app, &app_only, &path).await,
            post_text(&app, &app_only, &chat, "x", None).await,
            delete_as(&app, &app_only, &one).await,
        ],
        "This API requires an authenticated user",
        "an app-only token is a userless 403 on every route",
    );
    let reader_writes = [
        post_text(&app, &read_only, &chat, "x", None).await,
        delete_as(&app, &read_only, &one).await,
    ];
    assert_all_answer(
        &reader_writes,
        StatusCode::FORBIDDEN,
        "a reader posts and deletes nothing",
    );
    assert_all_say(
        &reader_writes,
        "Insufficient permissions: write:chats.",
        "the reader denial names the scope it lacks",
    );
    let anonymous = get_anonymous(&app, &path).await;
    assert_answer(
        &anonymous,
        StatusCode::UNAUTHORIZED,
        "no token at all is a 401",
    );
    assert_says(
        &anonymous,
        "Invalid credentials",
        "the missing-token wording",
    );
    let bogus = [
        get_as(&app, "not-a-real-token", &path).await,
        post_text(&app, "not-a-real-token", &chat, "x", None).await,
        delete_as(&app, "not-a-real-token", &one).await,
    ];
    assert_all_answer(&bogus, StatusCode::UNAUTHORIZED, "a bogus token is a 401");
    assert_all_say(
        &bogus,
        "The access token is invalid",
        "and it names the token as the problem",
    );

    let stranger = get_as(&app, &read_only, &path).await;
    assert_answer(
        &stranger,
        StatusCode::NOT_FOUND,
        "the right scope on the wrong chat is the ownership 404",
    );
}

/// The other half of the same hierarchy: the narrow sub-scope on its own
/// account, and the broad `read` and `write` roots that cover it.
#[tokio::test]
async fn the_broad_scope_roots_grant_the_message_routes() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, _) = poster(&pool, "alice").await;
    let (bob, bob_read) = fixture(&pool, "bob", "read").await;
    let bobs_chat = open_chat(&pool, &bob, &alice).await.id.to_string();
    let (erin, read_only) = fixture(&pool, "erin", READ_SCOPE).await;
    let bob_write = common::create_app_and_token(&pool, &bob, "write").await;
    let app = common::test_app(pool.clone());

    let erins_chat = open_chat(&pool, &erin, &alice).await.id.to_string();
    let own = get_as(&app, &read_only, &messages_path(&erins_chat)).await;
    assert_answer(
        &own,
        StatusCode::OK,
        "`read:chats` lists the caller's own messages",
    );
    let broad_read = get_as(&app, &bob_read, &messages_path(&bobs_chat)).await;
    assert_answer(
        &broad_read,
        StatusCode::OK,
        "the plain `read` root covers `read:chats`",
    );
    let broad_write = post_text(&app, &bob_write, &bobs_chat, "plain write root", None).await;
    assert_answer(&broad_write, StatusCode::OK, "the plain `write` root posts");
}
