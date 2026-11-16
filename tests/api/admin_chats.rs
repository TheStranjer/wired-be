//! The admin chat endpoints over HTTP (`/api/v1/pleroma/admin/…`, WBE-202):
//! Pleroma's `AdminAPI.ChatController` beside `list_user_chats` — the user
//! chat listing, the chat show, the message listing and the moderation
//! withdrawal of a single message, all rendered from the ordinary `Chat`
//! and `ChatMessage` entities WITHOUT ownership checks.
//!
//! What the tests pin, per the ticket:
//!
//! - `GET …/users/{nickname}/chats` answers that account's chats newest
//!   activity first with the `Link` header keyed on chat ids (and the
//!   keyset walks them); an unknown nickname is the 404
//!   `{"error":"Not found"}`, and an account with no chats the empty array
//!   with no `Link` header at all;
//! - `GET …/chats/{id}` shows ANY chat by its id — the admin is neither
//!   participant — while a malformed or unknown id is one 404;
//! - `GET …/chats/{id}/messages` answers reference-id-keyed pages newest
//!   first and leaves withdrawn messages out, exactly as the owner's own
//!   client sees the same chat;
//! - `DELETE …/chats/{id}/messages/{message_id}` tombstones the message for
//!   BOTH participants whoever wrote it (both user-side listings go empty),
//!   answers the entity as it stood before the delete, writes the
//!   `chat_message_delete` moderation entry, queues the federation `Delete`
//!   for a local-origin message whose partner is remote and nothing
//!   otherwise, and 404s a second strike at the same message;
//! - every route sits behind `AdminUser` plus its scope: `admin:read:chats`
//!   for the three reads, `admin:write:chats` for the delete, the class
//!   roots standing for both.

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::TEST_BASE_URL,
    federation::publisher,
    models::{
        chat::{self, ChatMessage, ChatMessageReference, NewChatMessage},
        user::{Flag, NewUser, User},
    },
};

use crate::common;

/// The admin API prefix every admin route here lives under.
const ADMIN: &str = "/api/v1/pleroma/admin";
/// The user-facing message route the two participants use to check what
/// the admin withdrawal hides.
const USER_CHAT_MESSAGES: &str = "/api/v1/pleroma/chats";
/// The 401 body when no token is presented.
const NO_TOKEN: &str = "Invalid credentials";
/// The 401 body when a presented token does not resolve.
const INVALID_TOKEN: &str = "The access token is invalid";
/// The 403 body for an app-only (user-unbound) token.
const USER_REQUIRED: &str = "This API requires an authenticated user";
/// The 403 body for a valid token whose user is not an admin.
const NOT_ADMIN: &str = "User is not an admin.";
/// The 403 body of a token that grants neither `admin:read` nor
/// `admin:read:chats`.
const NO_ADMIN_READ: &str = "Insufficient permissions: admin:read.";
/// The 403 body of a token that grants neither `admin:write` nor
/// `admin:write:chats`.
const NO_ADMIN_WRITE: &str = "Insufficient permissions: admin:write.";
/// The 404 body of the unknown nickname, the admin API's router-shaped miss.
const NOT_FOUND: &str = "Not found";
/// The 404 body of the unknown chat and message ids, the record miss.
const RECORD_NOT_FOUND: &str = "Record not found";
/// The admin scope roots every gate-passing token here carries.
const ALL_ADMIN: &str = "admin:read admin:write";
/// Every JSON key a rendered `Chat` entity must carry, sorted.
const CHAT_KEYS: [&str; 6] = [
    "account",
    "id",
    "last_message",
    "pinned",
    "unread",
    "updated_at",
];
/// Every JSON key a rendered `ChatMessage` entity must carry, sorted.
const MESSAGE_KEYS: [&str; 9] = [
    "account_id",
    "attachment",
    "card",
    "chat_id",
    "content",
    "created_at",
    "emojis",
    "id",
    "unread",
];

// --- fixtures -------------------------------------------------------------------------

/// A local admin named `nickname` with a bearer token granting `scopes`.
async fn admin(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user_with(pool, nickname, |user| NewUser {
        is_admin: Flag::new(true),
        ..user
    })
    .await;
    let token = common::create_app_and_token(pool, &created, scopes).await;
    (created, token)
}

/// [`chat::add_message`] in a transaction of its own — what an endpoint
/// caller does — answering the message with both sides' references (the
/// third element `None` when `recipient` is remote).
async fn send_as(
    pool: &PgPool,
    sender: &User,
    recipient: &User,
    new_message: &NewChatMessage,
) -> (
    ChatMessage,
    ChatMessageReference,
    Option<ChatMessageReference>,
) {
    let mut tx = pool.begin().await.expect("a transaction begins");
    let sent = chat::add_message(&mut tx, sender, recipient, new_message)
        .await
        .expect("the fixture chat message is written");
    tx.commit().await.expect("the transaction commits");
    sent
}

/// A plain-text message from `sender` to LOCAL `recipient`, answered as the
/// message with the SENDER's and the RECIPIENT's own references.
async fn send(
    pool: &PgPool,
    sender: &User,
    recipient: &User,
    text: &str,
) -> (ChatMessage, ChatMessageReference, ChatMessageReference) {
    let (message, sender_reference, recipient_reference) =
        send_as(pool, sender, recipient, &NewChatMessage::text(text)).await;
    (
        message,
        sender_reference,
        recipient_reference.expect("the fixture recipient is local"),
    )
}

/// A chat message from `sender` to `recipient` carrying a local-origin
/// object id — `{TEST_BASE_URL}/objects/{uuid}`, the shape decision 6
/// reserves for this instance's own documents and the only kind the
/// withdrawal hook federates — answered with that id and the sender's
/// reference.
async fn send_local_origin(
    pool: &PgPool,
    sender: &User,
    recipient: &User,
    text: &str,
) -> (String, ChatMessageReference) {
    let ap_id = format!("{TEST_BASE_URL}/objects/{}", Uuid::now_v7());
    let (_, sender_reference, _) = send_as(
        pool,
        sender,
        recipient,
        &NewChatMessage {
            ap_id: Some(ap_id.clone()),
            ..NewChatMessage::text(text)
        },
    )
    .await;
    (ap_id, sender_reference)
}

/// Moves a chat's `updated_at` into the past, so a listing's activity
/// order is settled rather than a race between fixtures (the model's own
/// tests backdate the same way).
async fn backdate_chat(pool: &PgPool, id: Uuid, hours: i64) {
    let _aged = sqlx::query(
        "UPDATE chats SET updated_at = now() - make_interval(hours => $2::int) WHERE id = $1",
    )
    .bind(id)
    .bind(hours)
    .execute(pool)
    .await
    .expect("the stamp is settable");
}

/// The `Link` header value the paged lists answer with for a page whose
/// first and last row carry `newest` and `oldest` as cursors.
fn expected_link(path: &str, newest: &str, oldest: &str) -> String {
    format!(
        "<https://test.example{path}?max_id={oldest}>; rel=\"next\", \
         <https://test.example{path}?min_id={newest}>; rel=\"prev\""
    )
}

/// The `error` string of a JSON error response.
fn error_message(response: &common::TestResponse) -> Option<String> {
    response
        .json_value()
        .get("error")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// The rendered object's keys, sorted, for exact key-set assertions.
fn json_keys(value: &Value) -> Vec<&str> {
    let mut keys: Vec<&str> = value
        .as_object()
        .expect("entities render objects")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    keys
}

/// The `id` of every entry of an entity array, in the answered order.
fn entity_ids(value: &Value) -> Vec<String> {
    value
        .as_array()
        .expect("the lists are arrays")
        .iter()
        .map(|entry| {
            entry["id"]
                .as_str()
                .expect("entities carry id strings")
                .to_owned()
        })
        .collect()
}

/// A GET through the real app.
async fn get(app: &Router, token: &str, path: &str) -> common::TestResponse {
    common::TestRequest::get(path).bearer(token).send(app).await
}

/// A DELETE through the real app.
async fn delete(app: &Router, token: &str, path: &str) -> common::TestResponse {
    common::TestRequest::delete(path)
        .bearer(token)
        .send(app)
        .await
}

/// The admin route listing `nickname`'s chats.
fn user_chats_path(nickname: &str) -> String {
    format!("{ADMIN}/users/{nickname}/chats")
}

/// The admin route showing chat `id`.
fn chat_path(id: &str) -> String {
    format!("{ADMIN}/chats/{id}")
}

/// The admin route listing chat `id`'s messages.
fn messages_path(id: &str) -> String {
    format!("{}/messages", chat_path(id))
}

/// The admin route deleting `message_id` out of chat `id`.
fn message_path(id: &str, message_id: &str) -> String {
    format!("{}/{}", messages_path(id), message_id)
}

/// The USER-facing route listing chat owner's `id`'s messages — what the
/// two participants hit to see what the admin withdrawal hid.
fn own_messages_path(id: &Uuid) -> String {
    format!("{USER_CHAT_MESSAGES}/{id}/messages")
}

/// The args of every queued `federator.publish` job, in insertion order.
async fn publish_jobs(pool: &PgPool) -> Vec<Value> {
    sqlx::query_as::<_, (Value,)>("SELECT args FROM jobs WHERE kind = $1 ORDER BY inserted_at, id")
        .bind(publisher::KIND_PUBLISH)
        .fetch_all(pool)
        .await
        .expect("the jobs read")
        .into_iter()
        .map(|(args,)| args)
        .collect()
}

// --- GET /users/{nickname}/chats ---------------------------------------------------------

/// The user chat listing answers the account's OWN chats — the admin holds
/// neither — newest activity first, full `Chat` entities whose `account`
/// names the other participant, under the `Link` header keyed on the page's
/// chat ids; `max_id` walks back through the rest.
#[tokio::test]
async fn the_admin_lists_a_users_chats_newest_first_under_link_headers() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_root, token) = admin(&pool, "root", "admin:read:chats").await;
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let dave = common::create_local_user(&pool, "dave").await;
    let (_, alice_bob, bobs_copy) = send(&pool, &alice, &bob, "yo bob").await;
    let (_, alice_carol, _) = send(&pool, &alice, &carol, "yo carol").await;
    let (_, alice_dave, _) = send(&pool, &alice, &dave, "yo dave").await;
    backdate_chat(&pool, alice_bob.chat_id, 2).await;
    backdate_chat(&pool, alice_carol.chat_id, 1).await;

    let path = user_chats_path("alice");
    let listed = get(&app, &token, &path).await;
    common::assert_answer(&listed, StatusCode::OK, "listing alice's chats");
    let value = listed.json_value();
    assert_eq!(
        entity_ids(&value),
        vec![
            alice_dave.chat_id.to_string(),
            alice_carol.chat_id.to_string(),
            alice_bob.chat_id.to_string(),
        ],
        "newest activity first, the backdated rows last"
    );
    for entry in value.as_array().expect("the list is an array") {
        assert_eq!(json_keys(entry), CHAT_KEYS, "every entry is a Chat entity");
    }
    assert_eq!(
        value[0]["account"]["id"],
        dave.id.to_string(),
        "the entity names the OTHER participant, from alice's seat"
    );
    assert_eq!(
        value[0]["last_message"]["id"],
        alice_dave.id.to_string(),
        "and carries her newest message"
    );

    let first = get(&app, &token, &format!("{path}?limit=2")).await;
    assert_eq!(
        first.header("link"),
        Some(
            expected_link(
                &path,
                &alice_dave.chat_id.to_string(),
                &alice_carol.chat_id.to_string()
            )
            .as_str()
        ),
        "the cursors key on the page's own chat ids"
    );
    let walked = get(
        &app,
        &token,
        &format!("{path}?limit=2&max_id={}", alice_carol.chat_id),
    )
    .await;
    assert_eq!(
        entity_ids(&walked.json_value()),
        vec![alice_bob.chat_id.to_string()],
        "max_id visits what the first page left out"
    );

    let bobs_side = get(&app, &token, &user_chats_path("bob")).await;
    assert_eq!(
        entity_ids(&bobs_side.json_value()),
        vec![bobs_copy.chat_id.to_string()],
        "bob's listing is HIS own row of the same conversation"
    );
    assert_eq!(
        bobs_side.json_value()[0]["account"]["id"],
        alice.id.to_string(),
        "seen from his seat, where alice is the other side"
    );
    assert_eq!(
        bobs_side.json_value()[0]["unread"],
        1,
        "and alice's message still counts unread there"
    );
}

/// A remote `nick@domain` handle is a stored nickname (decision 7), so the
/// lookup takes it verbatim; an account that owns no chats answers the empty
/// array under no `Link` header; the id spelling resolves like the show
/// route's; an unknown nickname is the admin API's 404
/// `{"error":"Not found"}`; and a malformed cursor is the pagination
/// module's 400, before anything reads.
#[tokio::test]
async fn the_user_lookup_takes_handles_and_refuses_strangers() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_root, token) = admin(&pool, "root", "admin:read:chats").await;
    let alice = common::create_local_user(&pool, "alice").await;
    let _mallory = common::create_remote_user(&pool, "mallory", "remote.example").await;

    let quiet = get(&app, &token, &user_chats_path("mallory@remote.example")).await;
    common::assert_answer(&quiet, StatusCode::OK, "a remote handle resolves");
    assert_eq!(quiet.json_value(), json!([]), "and owns no chat rows here");
    assert_eq!(quiet.header("link"), None, "an empty page links nowhere");

    let stranger = get(&app, &token, &user_chats_path("nobody")).await;
    common::assert_answer(&stranger, StatusCode::NOT_FOUND, "an unknown nickname");
    assert_eq!(
        error_message(&stranger).as_deref(),
        Some(NOT_FOUND),
        "the ticket's wording"
    );

    let by_id = get(&app, &token, &user_chats_path(&alice.id.to_string())).await;
    common::assert_answer(&by_id, StatusCode::OK, "the id spelling resolves too");

    let junk = get(
        &app,
        &token,
        &format!("{}?max_id=nope", user_chats_path("alice")),
    )
    .await;
    common::assert_answer(&junk, StatusCode::BAD_REQUEST, "a broken cursor");
    assert_eq!(
        error_message(&junk).as_deref(),
        Some("Invalid id"),
        "in the pagination module's wording"
    );
}

// --- GET /chats/{id} -----------------------------------------------------------------------

/// The chat show takes ANY chat by its id and renders the entity its OWNER
/// sees — the `account` is the other participant, `last_message` the
/// owner's newest live reference — which is the whole difference from the
/// client route's owned read: the admin here is neither participant. A
/// malformed id and an unknown one are one 404.
#[tokio::test]
async fn the_admin_shows_any_chat_whatever_its_owner() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_root, token) = admin(&pool, "root", "admin:read:chats").await;
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let (_, alice_ref, bob_ref) = send(&pool, &alice, &bob, "look at me").await;

    let alices_row = get(&app, &token, &chat_path(&alice_ref.chat_id.to_string())).await;
    common::assert_answer(&alices_row, StatusCode::OK, "alice's side of the pair");
    let shown = alices_row.json_value();
    assert_eq!(json_keys(&shown), CHAT_KEYS, "the ordinary Chat entity");
    assert_eq!(
        shown["id"],
        alice_ref.chat_id.to_string(),
        "keyed by her row"
    );
    assert_eq!(
        shown["account"]["id"],
        bob.id.to_string(),
        "rendered from the owner's seat: the account is bob"
    );
    assert_eq!(
        shown["last_message"]["id"],
        alice_ref.id.to_string(),
        "with the owner's newest message attached"
    );

    let bobs_row = get(&app, &token, &chat_path(&bob_ref.chat_id.to_string())).await;
    assert_eq!(
        bobs_row.json_value()["account"]["id"],
        alice.id.to_string(),
        "bob's own row is a second chat, rendered from HIS seat"
    );

    for raw in ["not-a-uuid", &Uuid::now_v7().to_string()] {
        let missing = get(&app, &token, &chat_path(raw)).await;
        common::assert_answer(
            &missing,
            StatusCode::NOT_FOUND,
            &format!("no chat answers to {raw:?}"),
        );
        assert_eq!(
            error_message(&missing).as_deref(),
            Some(RECORD_NOT_FOUND),
            "the record-miss wording, malformed or unknown alike"
        );
    }
}

// --- GET /chats/{id}/messages ------------------------------------------------------------------

/// The message listing answers the chat owner's references newest first
/// under the reference-id `Link` header, walks by cursor, and leaves a
/// withdrawn message out of every read — the model's live filter, the same
/// one the participants' clients see through. A malformed cursor is the 400
/// `Invalid id`, an unknown chat the 404.
#[tokio::test]
async fn the_admin_lists_a_chats_messages_newest_first_without_the_withdrawn() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_root, token) = admin(&pool, "root", ALL_ADMIN).await;
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let mut refs: Vec<ChatMessageReference> = Vec::new();
    for text in ["one", "two", "three"] {
        let (_, _, bob_side) = send(&pool, &alice, &bob, text).await;
        refs.push(bob_side);
    }
    refs.sort_unstable_by_key(|reference| reference.id);
    let chat_id = refs.first().expect("three references").chat_id.to_string();
    let path = messages_path(&chat_id);

    let listed = get(&app, &token, &path).await;
    common::assert_answer(&listed, StatusCode::OK, "listing the chat's messages");
    let newest_first: Vec<String> = refs.iter().map(|r| r.id.to_string()).rev().collect();
    assert_eq!(
        entity_ids(&listed.json_value()),
        newest_first,
        "newest reference id first"
    );
    assert_message_entities(&listed.json_value(), &refs, &alice.id.to_string(), &chat_id);
    assert_eq!(
        listed.header("link"),
        Some(expected_link(&path, &refs[2].id.to_string(), &refs[0].id.to_string()).as_str()),
        "the cursors key on the page's first and last reference ids"
    );

    let tail = get(
        &app,
        &token,
        &format!("{path}?limit=2&max_id={}", refs[1].id),
    )
    .await;
    assert_eq!(
        entity_ids(&tail.json_value()),
        vec![refs[0].id.to_string()],
        "max_id pages back through the chat"
    );
    let junk = get(&app, &token, &format!("{path}?since_id=nope")).await;
    common::assert_answer(&junk, StatusCode::BAD_REQUEST, "a broken cursor");
    assert_eq!(error_message(&junk).as_deref(), Some("Invalid id"));

    // Withdraw the middle message through the admin delete and every read
    // of this chat — this one included — loses it.
    let withdrawn = delete(
        &app,
        &token,
        &message_path(&chat_id, &refs[1].id.to_string()),
    )
    .await;
    common::assert_answer(&withdrawn, StatusCode::OK, "the withdrawal itself");
    let after = get(&app, &token, &path).await;
    assert_eq!(
        entity_ids(&after.json_value()),
        vec![refs[2].id.to_string(), refs[0].id.to_string()],
        "the withdrawn message is gone from the admin's read too"
    );

    let nowhere = get(&app, &token, &messages_path(&Uuid::now_v7().to_string())).await;
    common::assert_answer(
        &nowhere,
        StatusCode::NOT_FOUND,
        "an unknown chat has no messages",
    );
    assert_eq!(
        error_message(&nowhere).as_deref(),
        Some(RECORD_NOT_FOUND),
        "one 404, malformed or unknown alike"
    );
    let malformed = get(&app, &token, &messages_path("not-a-uuid")).await;
    common::assert_answer(&malformed, StatusCode::NOT_FOUND, "a malformed chat id");
}

/// Checks a message-list page holds the full `ChatMessage` entities for
/// `refs`: every key, each entry rendered from the stored reference the
/// model wrote — its content, the writer's `account_id`, the named chat.
fn assert_message_entities(list: &Value, refs: &[ChatMessageReference], sender: &str, chat: &str) {
    for entry in list.as_array().expect("the list is an array") {
        assert_eq!(
            json_keys(entry),
            MESSAGE_KEYS,
            "every entry is a ChatMessage entity"
        );
        let source = refs
            .iter()
            .find(|r| r.id.to_string() == entry["id"].as_str().expect("an id"))
            .expect("only the fixture's own references are listed");
        assert_eq!(
            entry["content"], source.content,
            "rendered from the reference the model wrote"
        );
        assert_eq!(entry["account_id"], sender, "the writer's account id");
        assert_eq!(entry["chat_id"], chat, "all in the named chat");
    }
}

// --- DELETE /chats/{id}/messages/{message_id} ----------------------------------------------------

/// The moderation withdrawal is FOR EVERYONE: deleting through alice's
/// chat and reference takes the message out of bob's listing too — what
/// the client route's self-service delete cannot do when the caller is not
/// the writer (there, each side merely drops its own copy). The answer is
/// the entity as the chat's owner saw it BEFORE the tombstone, and a second
/// strike — through either side's reference — finds nothing left to delete.
#[tokio::test]
async fn the_admin_delete_hides_the_message_from_both_participants() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_root, token) = admin(&pool, "root", ALL_ADMIN).await;
    let alice = common::create_local_user(&pool, "alice").await;
    let alice_token = common::create_app_and_token(&pool, &alice, "read:chats").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let bob_token = common::create_app_and_token(&pool, &bob, "read:chats").await;
    let (message, alice_ref, bob_ref) = send(&pool, &alice, &bob, "the plan").await;

    let deleted = delete(
        &app,
        &token,
        &message_path(&alice_ref.chat_id.to_string(), &alice_ref.id.to_string()),
    )
    .await;
    common::assert_answer(&deleted, StatusCode::OK, "withdrawing through alice's chat");
    let entity = deleted.json_value();
    assert_eq!(json_keys(&entity), MESSAGE_KEYS, "the ordinary entity");
    assert_eq!(
        entity["id"],
        alice_ref.id.to_string(),
        "the reference the route named"
    );
    assert_eq!(
        entity["content"], message.content,
        "rendered BEFORE the delete: the text is what the admin just took away"
    );
    assert_eq!(entity["account_id"], alice.id.to_string(), "alice wrote it");

    let alices_list = get(&app, &alice_token, &own_messages_path(&alice_ref.chat_id)).await;
    common::assert_answer(
        &alices_list,
        StatusCode::OK,
        "alice still lists her own chat",
    );
    assert_eq!(
        alices_list.json_value(),
        json!([]),
        "and the message is gone from it"
    );
    let bobs_list = get(&app, &bob_token, &own_messages_path(&bob_ref.chat_id)).await;
    assert_eq!(
        bobs_list.json_value(),
        json!([]),
        "bob's own listing lost it too, though the delete named alice's chat alone"
    );

    assert_tombstoned(&pool, message.id).await;
    assert_chat_delete_logged(&pool, alice_ref.id).await;

    let repeat = delete(
        &app,
        &token,
        &message_path(&alice_ref.chat_id.to_string(), &alice_ref.id.to_string()),
    )
    .await;
    common::assert_answer(
        &repeat,
        StatusCode::NOT_FOUND,
        "a second strike at the same reference",
    );
    let via_bob = delete(
        &app,
        &token,
        &message_path(&bob_ref.chat_id.to_string(), &bob_ref.id.to_string()),
    )
    .await;
    common::assert_answer(
        &via_bob,
        StatusCode::NOT_FOUND,
        "or the same message through bob's chat",
    );
    assert_eq!(
        error_message(&via_bob).as_deref(),
        Some(RECORD_NOT_FOUND),
        "the withdrawn message leaves nothing to delete either way"
    );

    assert!(
        publish_jobs(&pool).await.is_empty(),
        "a local↔local pair announces nothing to the network"
    );
}

/// The stored message row is a tombstone: text cleared, `deleted_at` set,
/// the row itself kept (its references survive and read through the
/// listing's live filter).
async fn assert_tombstoned(pool: &PgPool, message_id: Uuid) {
    let row: (String, Option<chrono::DateTime<chrono::Utc>>) =
        sqlx::query_as("SELECT content, deleted_at FROM chat_messages WHERE id = $1")
            .bind(message_id)
            .fetch_one(pool)
            .await
            .expect("the row reads");
    assert_eq!(row.0, "", "the text is cleared");
    assert!(row.1.is_some(), "and the row carries the tombstone stamp");
}

/// One `chat_message_delete` entry stands in the moderation log for the
/// withdrawal of reference `reference_id`: Pleroma's sentence, the id in
/// `subject_id`, the acting admin as `actor`, and no user subjects.
async fn assert_chat_delete_logged(pool: &PgPool, reference_id: Uuid) {
    let entry: (String, Value, Vec<Uuid>) =
        sqlx::query_as("SELECT message, data, subject_ids FROM moderation_log WHERE action = $1")
            .bind("chat_message_delete")
            .fetch_one(pool)
            .await
            .expect("the log entry stands");
    assert_eq!(
        entry.0,
        format!("@root deleted chat message ##{reference_id}"),
        "Pleroma's chat_message_delete sentence, keyed by the reference id"
    );
    assert_eq!(
        entry.1["subject_id"],
        reference_id.to_string(),
        "the subject rides in the data map"
    );
    assert_eq!(
        entry.1["actor"]["nickname"], "root",
        "and the actor is the admin who struck"
    );
    assert!(entry.2.is_empty(), "a chat message is no user subject");
}

/// The delete's participant check is the CHAT, not the caller: a malformed
/// or unknown `message_id`, and a reference that lives in another chat, are
/// one 404 each — and none of them withdraws anything.
#[tokio::test]
async fn the_delete_only_names_references_of_the_named_chat() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_root, token) = admin(&pool, "root", ALL_ADMIN).await;
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let (message, alice_ref, _) = send(&pool, &alice, &bob, "to bob").await;
    let (_, _, carol_ref) = send(&pool, &alice, &carol, "to carol").await;

    let refusals = [
        (
            alice_ref.chat_id.to_string(),
            "not-a-uuid".to_owned(),
            "a malformed reference id",
        ),
        (
            alice_ref.chat_id.to_string(),
            Uuid::now_v7().to_string(),
            "an unknown reference id",
        ),
        (
            alice_ref.chat_id.to_string(),
            carol_ref.id.to_string(),
            "a reference of another chat",
        ),
        (
            carol_ref.id.to_string(),
            alice_ref.id.to_string(),
            "and a live reference named under the wrong chat",
        ),
        (
            Uuid::now_v7().to_string(),
            alice_ref.id.to_string(),
            "an unknown chat holding a live reference",
        ),
    ];
    for (chat_id, message_id, who) in refusals {
        let refused = delete(&app, &token, &message_path(&chat_id, &message_id)).await;
        common::assert_answer(&refused, StatusCode::NOT_FOUND, who);
        assert_eq!(
            error_message(&refused).as_deref(),
            Some(RECORD_NOT_FOUND),
            "{who} is the one 404"
        );
    }
    let still_live =
        sqlx::query_scalar::<_, bool>("SELECT deleted_at IS NULL FROM chat_messages WHERE id = $1")
            .bind(message.id)
            .fetch_one(&pool)
            .await
            .expect("the row reads");
    assert!(still_live, "no refusal touched the message");
    assert!(
        publish_jobs(&pool).await.is_empty()
            && sqlx::query_scalar::<_, i64>("SELECT count(*) FROM moderation_log")
                .fetch_one(&pool)
                .await
                .expect("the log reads")
                == 0,
        "and none of them logged or announced anything"
    );
}

/// The withdrawal federates ONLY the messages this instance minted, and
/// only to the partner who could hold a copy: a local-origin message of a
/// local↔remote chat queues the one `Delete` — the sender as actor and
/// signer, the remote partner alone in `to`, the stored object id embedded
/// under its `ChatMessage` type — while a remote-origin withdrawal messages
/// its own way and sends nothing anywhere.
#[tokio::test]
async fn the_delete_announces_local_withdrawals_to_the_remote_partner() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_root, token) = admin(&pool, "root", ALL_ADMIN).await;
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;

    let borrowed = NewChatMessage {
        ap_id: Some("https://remote.example/objects/chat-1".to_owned()),
        ..NewChatMessage::text("bob said")
    };
    let (_, alices_view, _) = send_as(&pool, &bob, &alice, &borrowed).await;
    let withdrawn = delete(
        &app,
        &token,
        &message_path(
            &alices_view.chat_id.to_string(),
            &alices_view.id.to_string(),
        ),
    )
    .await;
    common::assert_answer(&withdrawn, StatusCode::OK, "the tombstone itself lands");
    assert!(
        publish_jobs(&pool).await.is_empty(),
        "and a remote-origin withdrawal announces nothing"
    );

    let (ap_id, alice_ref) = send_local_origin(&pool, &alice, &bob, "alice said").await;
    let deleted = delete(
        &app,
        &token,
        &message_path(&alice_ref.chat_id.to_string(), &alice_ref.id.to_string()),
    )
    .await;
    common::assert_answer(&deleted, StatusCode::OK, "withdrawing the local message");

    let jobs = publish_jobs(&pool).await;
    assert_eq!(
        jobs.len(),
        1,
        "exactly the local origin queues one announce"
    );
    assert_eq!(
        jobs[0]["actor_id"],
        alice.id.to_string(),
        "the message's own sender signs the withdrawal"
    );
    let activity = &jobs[0]["activity"];
    assert_eq!(activity["type"], "Delete", "the withdrawal activity");
    assert_eq!(activity["actor"], alice.ap_id);
    assert_eq!(
        activity["to"],
        json!([bob.ap_id]),
        "addressed to the chat's remote partner alone"
    );
    assert_eq!(activity["object"]["id"], ap_id, "naming the stored object");
    assert_eq!(
        activity["object"]["type"], "ChatMessage",
        "under the type the receiver's dispatcher routes on"
    );
    let id = activity["id"].as_str().expect("the activity has an id");
    assert!(
        id.starts_with(&format!("{TEST_BASE_URL}/activities/")),
        "and this instance's own activity id shape: {id}"
    );
}

// --- the gate ---------------------------------------------------------------------------------

/// The five roles the gate answers before any record or scope: no
/// credentials and an unknown token are the 401s, an app-only token, a
/// non-admin and a moderator are the 403s — Pleroma's `UserIsAdminPlug`
/// order — walked across all four routes.
#[tokio::test]
async fn every_admin_chat_route_demands_an_admin_user_token() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let (_, alice_ref, _) = send(&pool, &alice, &bob, "gate check").await;
    let chat_id = alice_ref.chat_id.to_string();
    let message_id = alice_ref.id.to_string();

    let pleb = common::create_local_user(&pool, "pleb").await;
    let plain = common::create_app_and_token(&pool, &pleb, ALL_ADMIN).await;
    let mod_user = common::create_local_user_with(&pool, "mod", |user| NewUser {
        is_moderator: Flag::new(true),
        ..user
    })
    .await;
    let staff = common::create_app_and_token(&pool, &mod_user, ALL_ADMIN).await;
    let app_only = common::create_app_only_token(&pool, ALL_ADMIN).await;

    // Each route is walked with the verb it registers: a GET to the
    // DELETE-only route is a router-level 405 ahead of any auth.
    let routes = [
        (false, user_chats_path("alice")),
        (false, chat_path(&chat_id)),
        (false, messages_path(&chat_id)),
        (true, message_path(&chat_id, &message_id)),
    ];
    for (is_delete, path) in &routes {
        let unauthenticated = chat_request(*is_delete, path).send(&app).await;
        common::assert_answer(&unauthenticated, StatusCode::UNAUTHORIZED, "no token");
        assert_eq!(
            error_message(&unauthenticated).as_deref(),
            Some(NO_TOKEN),
            "every route challenges alike"
        );

        let bogus = chat_request(*is_delete, path)
            .bearer("not-a-real-token")
            .send(&app)
            .await;
        common::assert_answer(&bogus, StatusCode::UNAUTHORIZED, "a bogus token");
        assert_eq!(error_message(&bogus).as_deref(), Some(INVALID_TOKEN));

        for (token, reason) in [
            (app_only.as_str(), USER_REQUIRED),
            (plain.as_str(), NOT_ADMIN),
            (staff.as_str(), NOT_ADMIN),
        ] {
            let refused = chat_request(*is_delete, path)
                .bearer(token)
                .send(&app)
                .await;
            common::assert_answer(&refused, StatusCode::FORBIDDEN, reason);
            assert_eq!(error_message(&refused).as_deref(), Some(reason));
        }
    }

    // Nothing the walk refused deleted anything: a scoped admin still
    // lists the message the DELETE-only route guards.
    let (_late, late_token) = admin(&pool, "late-reader", "admin:read:chats").await;
    let still_listed = get(&app, &late_token, &messages_path(&chat_id)).await;
    assert_eq!(
        entity_ids(&still_listed.json_value()),
        vec![message_id],
        "the refusals deleted nothing"
    );
}

/// A request on the route's own verb: `is_delete` marks the withdrawal
/// route (DELETE), the three reads ride GET — a method the router has no
/// route for would 405 ahead of the auth extractor.
fn chat_request(is_delete: bool, path: &str) -> common::TestRequest {
    if is_delete {
        common::TestRequest::delete(path)
    } else {
        common::TestRequest::get(path)
    }
}

/// The scope half of the gate: the three reads live in `admin:read:chats`
/// and the delete in `admin:write:chats`, the class roots grant both, and
/// the wrong class — in this area or any other — is refused naming the
/// class root, Pleroma's plug wording. A refused delete withdraws nothing.
#[tokio::test]
async fn the_chats_scopes_gate_reads_and_write_apart() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let (_, alice_ref, _) = send(&pool, &alice, &bob, "scoped").await;
    let chat_id = alice_ref.chat_id.to_string();
    let message_id = alice_ref.id.to_string();

    let (_reader, read_only) = admin(&pool, "reader", "admin:read:chats").await;
    let (_writer, write_only) = admin(&pool, "writer", "admin:write:chats").await;
    let (_accountant, wrong_area) = admin(&pool, "accountant", "admin:read:accounts").await;

    for path in [
        user_chats_path("alice"),
        chat_path(&chat_id),
        messages_path(&chat_id),
    ] {
        let granted = get(&app, &read_only, &path).await;
        common::assert_answer(
            &granted,
            StatusCode::OK,
            &format!("admin:read:chats on {path}"),
        );
        let write_on_read = get(&app, &write_only, &path).await;
        common::assert_answer(
            &write_on_read,
            StatusCode::FORBIDDEN,
            &format!("admin:write:chats on {path}"),
        );
        assert_eq!(
            error_message(&write_on_read).as_deref(),
            Some(NO_ADMIN_READ)
        );
        let foreign = get(&app, &wrong_area, &path).await;
        common::assert_answer(
            &foreign,
            StatusCode::FORBIDDEN,
            "the accounts scope reads no chats",
        );
        assert_eq!(error_message(&foreign).as_deref(), Some(NO_ADMIN_READ));
    }

    let read_on_write = delete(&app, &read_only, &message_path(&chat_id, &message_id)).await;
    common::assert_answer(
        &read_on_write,
        StatusCode::FORBIDDEN,
        "a read scope cannot delete",
    );
    assert_eq!(
        error_message(&read_on_write).as_deref(),
        Some(NO_ADMIN_WRITE),
        "naming the write root Pleroma's plug asks for"
    );
    let still_there = get(&app, &read_only, &messages_path(&chat_id)).await;
    assert_eq!(
        entity_ids(&still_there.json_value()),
        vec![message_id.clone()],
        "and the refused delete touched nothing"
    );

    let deleted = delete(&app, &write_only, &message_path(&chat_id, &message_id)).await;
    common::assert_answer(&deleted, StatusCode::OK, "admin:write:chats deletes");
    let empty = get(&app, &read_only, &messages_path(&chat_id)).await;
    assert_eq!(
        empty.json_value(),
        json!([]),
        "and the narrow write scope left the tombstone too"
    );
}
