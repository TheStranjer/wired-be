//! The chat API over HTTP (`/api/v1/pleroma/chats`, `/api/v2/pleroma/chats`,
//! WBE-146, and its read-marker and pinning half, WBE-148).
//!
//! The tests pin what the two tickets promise. On the write side:
//! `POST …/chats/{id}/read` marks every reference of the caller's chat up to
//! `last_read_id` read — up to and including, idempotently, leaving anything
//! newer unread — and answers the `Chat` entity with the new `unread` count;
//! without the marker the request is the 400
//! `{"error":"Missing parameter: last_read_id"}`, and a present non-id
//! marker its `Invalid parameter` sibling. `POST …/messages/{message_id}/read`
//! marks one reference and answers the `ChatMessage` entity; a reference of
//! another chat is not there. `POST …/pin` and `…/unpin` toggle `pinned` and
//! move what the lists' `pinned` filter (both mounted below: `GET
//! /api/v1/pleroma/chats` and `GET /api/v2/pleroma/chats`) sees.
//!
//! On the read side: `POST …/by-account-id/{id}` gets or creates the
//! caller's chat with that account, 200 and idempotent, and refuses the
//! self-chat with a 422, a recipient whose `accepts_chat_messages` flag is
//! down with the 403 naming the flag, and a malformed, unknown or
//! deactivated account with the one 404; the lists answer `Chat` entities
//! newest activity first under the `Link` header keyed on chat ids, with
//! the `pinned` and `with_muted` filters, v1's 30-row page cap against v2's
//! larger one, and keyset walks by `max_id`/`min_id`/`since_id` over the
//! chat ids; `GET …/chats/{id}` answers the chat to its owner alone.
//! Foreign, unknown and malformed ids are one 404 everywhere, and every
//! route demands its scope: `read:chats` to look, `write:chats` to change
//! (the plain `read` and `write` roots cover both sub-scopes).

#![cfg(test)]

// `pub` because Clippy's nursery `redundant_pub_crate` forbids `pub(crate)`
// items inside a private module, while `unreachable_pub` forbids `pub` ones:
// only a `pub` module makes the harness helpers lint-clean.
pub mod common;

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    models::{
        chat::{self, ChatFilter, ChatMessageReference, NewChatMessage},
        user::{Flag, NewUser, User},
    },
    pagination::{MAX_LIMIT, PageParams},
    util::id::new_id,
};

/// The chats root all v1 routes live under.
const ROOT: &str = "/api/v1/pleroma/chats";
/// The v2 chats root, the paged sibling of [`ROOT`].
const V2_ROOT: &str = "/api/v2/pleroma/chats";
/// The scope Pleroma's `OAuthScopesPlug` pipeline demands for the writing actions.
const SCOPE: &str = "write:chats";
/// The scope the same pipeline demands for the reading actions.
const READ_SCOPE: &str = "read:chats";
/// The whole list or page in one fetch.
const ALL: &str = "limit=40";
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

/// The `error` string of a JSON error response, or `None` when the body is
/// not an error object with a string `error` field.
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

/// Id text back to an id, panicking only on fixture bugs.
fn parse(text: &str) -> Uuid {
    text.parse().expect("the fixture ids are id text")
}

/// The whole list in one page, the way the model's tests spell it.
fn all_page() -> PageParams {
    PageParams::from_query(Some(ALL), MAX_LIMIT).expect("the page parses")
}

/// A local [`User`] named `nickname` plus a bearer token granting `scopes`.
async fn fixture(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, scopes).await;
    (created, token)
}

/// Sends `text` from `sender` to `recipient` through the model (what the
/// chat-posting ticket's endpoint will do) and returns the message with both
/// sides' references.
async fn send(
    pool: &PgPool,
    sender: &User,
    recipient: &User,
    text: &str,
) -> (
    chat::ChatMessage,
    ChatMessageReference,
    ChatMessageReference,
) {
    let mut conn = pool.begin().await.expect("a transaction begins");
    let sent = chat::add_message(&mut conn, sender, recipient, &NewChatMessage::text(text))
        .await
        .expect("a plain chat message is written");
    conn.commit().await.expect("the transaction commits");
    (sent.0, sent.1, sent.2.expect("the recipient is local"))
}

/// The `alice → bob` fixture the read tests start from: alice has sent bob
/// three messages, so bob's chat of alice holds three unread references.
struct Thread {
    /// The writer, with a token granting `read`.
    alice: User,
    /// The reader and chat owner.
    bob: User,
    /// The token bob's routes are called with.
    token: String,
    /// bob's own chat row of alice.
    chat: chat::Chat,
    /// The ids of bob's three references, oldest first.
    refs: Vec<Uuid>,
}

impl Thread {
    /// bob's chat id as the hyphenated text routes speak it.
    fn chat_id(&self) -> String {
        self.chat.id.to_string()
    }
}

async fn thread(pool: &PgPool) -> Thread {
    let (alice, _) = fixture(pool, "alice", "read").await;
    let (bob, token) = fixture(pool, "bob", SCOPE).await;
    let mut references = Vec::new();
    let mut chat_id = None;
    for text in ["one", "two", "three"] {
        let (_, _, bob_ref) = send(pool, &alice, &bob, text).await;
        references.push(bob_ref.id);
        chat_id = Some(bob_ref.chat_id);
    }
    references.sort_unstable();
    let own = chat::get_owned(pool, bob.id, chat_id.expect("three messages wrote a chat"))
        .await
        .expect("the chat read works")
        .expect("bob owns the chat his references sit in");
    Thread {
        alice,
        bob,
        token,
        chat: own,
        refs: references,
    }
}

/// The unread flags of chat `chat_id`, newest reference id first.
async fn unread_flags(pool: &PgPool, chat_id: Uuid) -> Vec<bool> {
    chat::messages_page(pool, chat_id, all_page())
        .await
        .expect("the model reads the chat back")
        .into_iter()
        .map(|reference| reference.unread)
        .collect()
}

/// The stored unread count of chat `chat_id`, through the model.
async fn stored_unread(pool: &PgPool, chat_id: Uuid) -> i64 {
    chat::unread_count(pool, chat_id)
        .await
        .expect("the count reads")
}

/// The stored `pinned` flag of `user_id`'s chat `chat_id`.
async fn stored_pinned(pool: &PgPool, user_id: Uuid, chat_id: Uuid) -> bool {
    chat::get_owned(pool, user_id, chat_id)
        .await
        .expect("the chat read works")
        .expect("the chat exists")
        .pinned
}

/// The ids of `user_id`'s chats under the `pinned` filter — the list query
/// (`page_for_user` with [`ChatFilter`]) the chats list endpoint runs.
async fn pinned_ids(pool: &PgPool, user_id: Uuid, pinned: Option<bool>) -> Vec<Uuid> {
    chat::page_for_user(
        pool,
        user_id,
        all_page(),
        ChatFilter {
            pinned,
            with_muted: false,
        },
    )
    .await
    .expect("the list reads")
    .into_iter()
    .map(|row| row.id)
    .collect()
}

/// `POST` `path` as `token` with the JSON `body`.
async fn post_json(app: &Router, token: &str, path: &str, body: Value) -> common::TestResponse {
    common::TestRequest::post(path)
        .bearer(token)
        .json(&body)
        .send(app)
        .await
}

/// `POST` `path` as `token` with no body at all.
async fn post_empty(app: &Router, token: &str, path: &str) -> common::TestResponse {
    common::TestRequest::post(path)
        .bearer(token)
        .send(app)
        .await
}

/// The `POST …/chats/{id}/read` body Pleroma demands.
fn marker_body(last_read_id: Uuid) -> Value {
    json!({"last_read_id": last_read_id.to_string()})
}

/// The read path of chat `chat`.
fn read_path(chat: &str) -> String {
    format!("{ROOT}/{chat}/read")
}

/// The single-message read path inside chat `chat`.
fn message_read_path(chat: &str, message: &str) -> String {
    format!("{ROOT}/{chat}/messages/{message}/read")
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn chat_read_marks_up_to_the_marker_and_counts_what_remains(pool: PgPool) {
    let started = thread(&pool).await;
    let app = common::test_app(pool.clone());
    let chat = started.chat_id();

    let value = read_chat(&app, &started.token, &chat, started.refs[1]).await;

    assert_eq!(json_keys(&value), CHAT_KEYS, "exactly the Chat keys");
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
    assert_eq!(json_keys(last), MESSAGE_KEYS, "the chat's newest message");
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn a_form_body_marks_the_chat_read(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn an_empty_chat_reads_to_null_messages(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn a_read_without_the_marker_is_a_400(pool: PgPool) {
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
                error_message(&response).as_deref(),
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn a_malformed_marker_is_a_400(pool: PgPool) {
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
            error_message(&response).as_deref(),
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn marking_one_message_read_returns_it(pool: PgPool) {
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
        json_keys(&value),
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

    let unknown = post_empty(
        &app,
        &started.token,
        &message_read_path(&chat, &new_id().to_string()),
    )
    .await;
    assert_eq!(
        unknown.status,
        StatusCode::NOT_FOUND,
        "an unknown reference is 404"
    );
    let malformed = post_empty(
        &app,
        &started.token,
        &message_read_path(&chat, "not-a-uuid"),
    )
    .await;
    assert_eq!(
        malformed.status,
        StatusCode::NOT_FOUND,
        "and so is a malformed one"
    );
}

/// A message's attachment rides along in the entity the read answers with,
/// on both routes that render one.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn a_marked_message_carries_its_attachment(pool: PgPool) {
    let (alice, _) = fixture(&pool, "alice", "read").await;
    let (bob, token) = fixture(&pool, "bob", SCOPE).await;
    let media_id = insert_attachment(&pool, &alice).await;
    let mut conn = pool.begin().await.expect("a transaction begins");
    let sent = chat::add_message(
        &mut conn,
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn a_message_of_another_chat_is_not_there(pool: PgPool) {
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
        error_message(&response).as_deref(),
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

/// Foreign, unknown and malformed chat ids are one 404 on every route, and
/// nothing behind them moves: the other participant's side of the same
/// conversation is a different id that their token cannot use.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn chats_are_only_their_owners_to_read_and_pin(pool: PgPool) {
    let started = thread(&pool).await;
    // alice's own side of the same conversation.
    let page = chat::page_for_user(&pool, started.alice.id, all_page(), ChatFilter::default())
        .await
        .expect("alice's list reads");
    let alices_chat = page.first().expect("alice has one chat").id;
    let app = common::test_app(pool.clone());

    for foreign in [
        alices_chat.to_string(),
        new_id().to_string(),
        "not-a-uuid".to_owned(),
    ] {
        let read = post_json(
            &app,
            &started.token,
            &read_path(&foreign),
            marker_body(started.refs[2]),
        )
        .await;
        assert_eq!(
            read.status,
            StatusCode::NOT_FOUND,
            "chat {foreign} is not bob's to read-mark"
        );
        assert_eq!(
            error_message(&read).as_deref(),
            Some("Record not found"),
            "the 404 wording on {foreign}"
        );
        let pin = post_empty(&app, &started.token, &format!("{ROOT}/{foreign}/pin")).await;
        assert_eq!(pin.status, StatusCode::NOT_FOUND, "nor to pin");
        let message = post_empty(
            &app,
            &started.token,
            &message_read_path(&foreign, &started.refs[0].to_string()),
        )
        .await;
        assert_eq!(
            message.status,
            StatusCode::NOT_FOUND,
            "not even his own messages through it"
        );
    }

    assert!(
        !stored_pinned(&pool, started.alice.id, alices_chat).await,
        "alice's row was never pinned through bob's token"
    );
    assert_eq!(
        stored_unread(&pool, parse(&started.chat_id())).await,
        3,
        "bob's chat stayed unread too — every attempt was a 404"
    );
}

/// `POST …/pin` and `POST …/unpin` toggle the caller's own flag, answer the
/// chat entity with it, and move what the list's `pinned` filter sees; the
/// other side of the conversation keeps its own flag, and an already-pinned
/// chat stays pinned under a second pin.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn pin_and_unpin_toggle_the_flag_and_the_list_filter(pool: PgPool) {
    let started = thread(&pool).await;
    let app = common::test_app(pool.clone());
    let chat = started.chat_id();
    let own = parse(&chat);
    let token = started.token.clone();

    let pinned = post_empty(&app, &token, &format!("{ROOT}/{chat}/pin")).await;
    assert_eq!(
        pinned.status,
        StatusCode::OK,
        "pinning answers 200: {}",
        pinned.text()
    );
    let value = pinned.json_value();
    assert_eq!(json_keys(&value), CHAT_KEYS, "the full Chat entity");
    assert_eq!(value["id"], chat, "bob's chat");
    assert_eq!(value["pinned"], true, "now pinned");
    assert_eq!(value["unread"], 3, "pinning marks nothing read");
    assert_eq!(value["last_message"]["id"], started.refs[2].to_string());
    assert!(
        stored_pinned(&pool, started.bob.id, own).await,
        "the flag is stored"
    );

    let again = post_empty(&app, &token, &format!("{ROOT}/{chat}/pin")).await;
    assert_eq!(again.json_value()["pinned"], true, "pinning twice holds");

    assert_eq!(
        pinned_ids(&pool, started.bob.id, Some(true)).await,
        vec![own],
        "the pinned filter names the chat"
    );
    assert!(
        pinned_ids(&pool, started.bob.id, Some(false))
            .await
            .is_empty(),
        "and the unpinned half is empty"
    );

    let unpinned = post_empty(&app, &token, &format!("{ROOT}/{chat}/unpin")).await;
    assert_eq!(unpinned.status, StatusCode::OK, "unpinning answers 200");
    assert_eq!(
        unpinned.json_value()["pinned"],
        false,
        "and clears the flag"
    );
    assert!(
        pinned_ids(&pool, started.bob.id, Some(true))
            .await
            .is_empty(),
        "the pinned filter lets go"
    );
    assert_eq!(
        pinned_ids(&pool, started.bob.id, Some(false)).await,
        vec![own],
        "the chat is back in the unpinned half"
    );

    // bob's toggle is his own ordering choice: alice's side never changes.
    assert!(
        !stored_pinned(&pool, started.alice.id, {
            let rows =
                chat::page_for_user(&pool, started.alice.id, all_page(), ChatFilter::default())
                    .await
                    .expect("alice lists");
            rows.first().expect("alice has one chat").id
        })
        .await,
        "alice's row is untouched by bob's pin and unpin"
    );
}

/// All four routes demand a user token granting `write:chats`: no token is
/// the 401 "Invalid credentials", a bad one the 401 "The access token is
/// invalid", an app-only token and a `read:chats` user token the 403 — and a
/// plain `write` token covers the sub-scope through the Mastodon hierarchy.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn every_route_demands_write_chats(pool: PgPool) {
    let started = thread(&pool).await;
    let app = common::test_app(pool.clone());
    let chat = started.chat_id();
    let routes = [
        (read_path(&chat), Some(marker_body(started.refs[2]))),
        (message_read_path(&chat, &started.refs[0].to_string()), None),
        (format!("{ROOT}/{chat}/pin"), None),
        (format!("{ROOT}/{chat}/unpin"), None),
    ];
    let (_, narrow) = fixture(&pool, "dave", "read:chats").await;
    let app_only = common::create_app_only_token(&pool, SCOPE).await;

    for (path, body) in routes {
        let anonymous = match body.clone() {
            Some(value) => post_json(&app, "not-a-real-token", &path, value).await,
            None => post_empty(&app, "not-a-real-token", &path).await,
        };
        assert_eq!(
            anonymous.status,
            StatusCode::UNAUTHORIZED,
            "an invalid token is a 401 on {path}"
        );
        assert_eq!(
            error_message(&anonymous).as_deref(),
            Some("The access token is invalid"),
            "the token refusal wording on {path}"
        );

        let no_token = match body.clone() {
            Some(value) => {
                common::TestRequest::post(&path)
                    .json(&value)
                    .send(&app)
                    .await
            }
            None => common::TestRequest::post(&path).send(&app).await,
        };
        assert_eq!(
            no_token.status,
            StatusCode::UNAUTHORIZED,
            "no token at all is a 401 on {path}"
        );
        assert_eq!(
            error_message(&no_token).as_deref(),
            Some("Invalid credentials"),
            "the missing-token wording on {path}"
        );

        let wrong_kind = match body.clone() {
            Some(value) => post_json(&app, &app_only, &path, value).await,
            None => post_empty(&app, &app_only, &path).await,
        };
        assert_eq!(
            wrong_kind.status,
            StatusCode::FORBIDDEN,
            "an app-only token is a 403 on {path}"
        );
        assert_eq!(
            error_message(&wrong_kind).as_deref(),
            Some("This API requires an authenticated user"),
            "the user refusal wording on {path}"
        );

        let wrong_scope = match body.clone() {
            Some(value) => post_json(&app, &narrow, &path, value).await,
            None => post_empty(&app, &narrow, &path).await,
        };
        assert_eq!(
            wrong_scope.status,
            StatusCode::FORBIDDEN,
            "`read:chats` is not enough on {path}"
        );
        assert_eq!(
            error_message(&wrong_scope).as_deref(),
            Some("Insufficient permissions: write:chats."),
            "the scope refusal wording on {path}"
        );
    }

    // The scope hierarchy: a plain `write` grant covers `write:chats` — on
    // the token's own chat (bob's stays out of erin's reach).
    let (erin, _) = fixture(&pool, "erin", "read").await;
    let erin_token = common::create_app_and_token(&pool, &erin, "write").await;
    let erins_chat = chat::get_or_create(&pool, &erin, &started.alice)
        .await
        .expect("erin's chat with alice opens");
    let pinned = post_empty(&app, &erin_token, &format!("{ROOT}/{}/pin", erins_chat.id)).await;
    assert_eq!(
        pinned.status,
        StatusCode::OK,
        "`write` grants `write:chats`: {}",
        pinned.text()
    );
    assert_eq!(pinned.json_value()["pinned"], true, "and the flag is set");
    let stranger = post_empty(&app, &erin_token, &format!("{ROOT}/{chat}/pin")).await;
    assert_eq!(
        stranger.status,
        StatusCode::NOT_FOUND,
        "still only through the caller's own chats"
    );
}

/// A media row of `uploader`, for the attachment column to point at.
async fn insert_attachment(pool: &PgPool, uploader: &User) -> Uuid {
    let row: (Uuid,) = sqlx::query_as(
        "INSERT INTO media_attachments (id, user_id, url, content_type)
         VALUES ($1, $2, 'https://test.example/media/x.png', 'image/png')
         RETURNING id",
    )
    .bind(new_id())
    .bind(uploader.id)
    .fetch_one(pool)
    .await
    .unwrap();
    row.0
}

/// The `POST …/by-account-id/{id}` path for the account `account`.
fn open_path(account: &str) -> String {
    format!("{ROOT}/by-account-id/{account}")
}

/// `GET` `path` as `token`.
async fn get_as(app: &Router, token: &str, path: &str) -> common::TestResponse {
    common::TestRequest::get(path).bearer(token).send(app).await
}

/// The ids of the chats in a rendered list, in the order they came back.
fn listed_ids(list: &Value) -> Vec<String> {
    list.as_array()
        .expect("the list is an array")
        .iter()
        .map(|chat| {
            chat["id"]
                .as_str()
                .expect("chat entities carry their id")
                .to_owned()
        })
        .collect()
}

/// The `Link` header Pleroma's `add_link_headers` puts on a page whose first
/// and last rows carry `newest` and `oldest` as chat ids.
fn expected_link(path: &str, newest: &str, oldest: &str) -> String {
    format!(
        "<https://test.example{path}?max_id={oldest}>; rel=\"next\", \
         <https://test.example{path}?min_id={newest}>; rel=\"prev\""
    )
}

/// The chat of `owner` talking to `partner`, through the model — the row the
/// create endpoint writes.
async fn open_chat(pool: &PgPool, owner: &User, partner: &User) -> chat::Chat {
    chat::get_or_create(pool, owner, partner)
        .await
        .expect("the fixture chat opens")
}

/// Moves chat `chat_id`'s `updated_at` `hours` hours into the past, so a
/// list order is fixed no matter how fast the machine is.
async fn backdate_chat(pool: &PgPool, chat_id: Uuid, hours: i64) {
    let _moved: sqlx::postgres::PgQueryResult = sqlx::query(
        "UPDATE chats SET updated_at = now() - make_interval(hours => $2) WHERE id = $1",
    )
    .bind(chat_id)
    .bind(i32::try_from(hours).expect("fixture hours fit"))
    .execute(pool)
    .await
    .expect("the backdate runs");
}

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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn creating_a_chat_gets_the_same_row_every_time(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn the_chat_gates_refuse_self_a_silent_recipient_and_nobody(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn show_answers_the_chat_to_its_owner_alone(pool: PgPool) {
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

/// `GET /api/v1/pleroma/chats` answers the caller's chats newest activity
/// first — an order no id happens to share — under the `Link` header keyed
/// on the page's first and last chat ids, and the `max_id`/`min_id`/
/// `since_id` cursors walk the chat ids themselves: a `limit=1` walk visits
/// every chat exactly once, descending.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn the_v1_list_orders_by_activity_and_pages_by_id(pool: PgPool) {
    let (alice, token) = fixture(&pool, "alice", READ_SCOPE).await;
    let mut opened = Vec::new();
    for (nickname, hours) in [("bob", 1_i64), ("carol", 2), ("dave", 0)] {
        let partner = common::create_local_user(&pool, nickname).await;
        let chat = open_chat(&pool, &alice, &partner).await;
        if hours > 0 {
            backdate_chat(&pool, chat.id, hours).await;
        }
        opened.push(chat);
    }
    let app = common::test_app(pool.clone());

    let response = get_as(&app, &token, ROOT).await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "the list answers 200: {}",
        response.text()
    );
    let value = response.json_value();
    let ids = listed_ids(&value);
    assert_eq!(
        ids,
        vec![
            opened[2].id.to_string(),
            opened[0].id.to_string(),
            opened[1].id.to_string()
        ],
        "dave is fresh, then the hour-old bob, then the two-hour-old carol"
    );
    for chat in value.as_array().expect("the list is an array") {
        assert_eq!(
            json_keys(chat),
            CHAT_KEYS,
            "every entry is the full Chat entity"
        );
    }
    let want = expected_link(ROOT, &ids[0], &ids[2]);
    assert_eq!(
        response.header("link"),
        Some(want.as_str()),
        "the cursors key on the page's own chat ids"
    );

    let mut walked = Vec::new();
    let mut cursor: Option<Uuid> = None;
    loop {
        let path = cursor.map_or_else(
            || format!("{ROOT}?limit=1"),
            |oldest| format!("{ROOT}?limit=1&max_id={oldest}"),
        );
        let page = get_as(&app, &token, &path).await;
        assert_eq!(
            page.status,
            StatusCode::OK,
            "the walk keeps answering: {}",
            page.text()
        );
        let Some(first) = listed_ids(&page.json_value()).first().cloned() else {
            break;
        };
        let id = parse(&first);
        if let Some(older) = cursor {
            assert!(id < older, "pages walk strictly down the chat ids");
        }
        walked.push(id);
        cursor = Some(id);
    }
    assert_eq!(walked.len(), 3, "the limit-one walk visits every chat");
    let mut own: Vec<Uuid> = opened.iter().map(|chat| chat.id).collect();
    let mut seen = walked.clone();
    own.sort_unstable();
    seen.sort_unstable();
    assert_eq!(seen, own, "each exactly once");

    let smallest = walked.last().expect("three visits");
    let newer = get_as(&app, &token, &format!("{ROOT}?since_id={smallest}")).await;
    assert_eq!(
        listed_ids(&newer.json_value()).len(),
        2,
        "since_id skips the cursor and its past"
    );
    let min = get_as(&app, &token, &format!("{ROOT}?min_id={smallest}")).await;
    assert_eq!(
        listed_ids(&min.json_value()).len(),
        2,
        "min_id answers the newer chats too"
    );

    let junk = get_as(&app, &token, &format!("{ROOT}?max_id=not-a-uuid")).await;
    assert_eq!(
        junk.status,
        StatusCode::BAD_REQUEST,
        "a malformed cursor is a 400"
    );
    assert_eq!(
        error_message(&junk).as_deref(),
        Some("Invalid id"),
        "with the pagination module's wording"
    );
}

/// Both lists take the same filters: `pinned=true`/`false` split the chats
/// (through Pleroma's boolean spellings, and on v2 alike), `with_muted` is
/// accepted and hides nothing while the mutes table waits for its ticket, a
/// non-boolean filter fails validation, and a chatless account gets the bare
/// empty array with no `Link` header.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn the_lists_filter_by_pinned_and_tolerate_with_muted(pool: PgPool) {
    let (alice, token) = fixture(&pool, "alice", READ_SCOPE).await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let with_bob = open_chat(&pool, &alice, &bob).await;
    let with_carol = open_chat(&pool, &alice, &carol).await;
    let _pinned: chat::Chat = chat::set_pinned(&pool, alice.id, with_carol.id, true)
        .await
        .expect("the pin writes")
        .expect("alice owns that chat");
    let app = common::test_app(pool.clone());

    let pinned = get_as(&app, &token, &format!("{ROOT}?pinned=true"))
        .await
        .json_value();
    assert_eq!(
        listed_ids(&pinned),
        vec![with_carol.id.to_string()],
        "the pinned filter names the one chat"
    );
    let pinned_v2 = get_as(&app, &token, &format!("{V2_ROOT}?pinned=1"))
        .await
        .json_value();
    assert_eq!(
        listed_ids(&pinned_v2),
        vec![with_carol.id.to_string()],
        "v2 reads the same filter in the form client's spelling"
    );
    let rest = get_as(&app, &token, &format!("{ROOT}?pinned=false"))
        .await
        .json_value();
    assert_eq!(
        listed_ids(&rest),
        vec![with_bob.id.to_string()],
        "the unpinned half gets the rest"
    );
    let muted = get_as(&app, &token, &format!("{ROOT}?with_muted=true"))
        .await
        .json_value();
    let mut want = vec![with_bob.id.to_string(), with_carol.id.to_string()];
    let mut got = listed_ids(&muted);
    want.sort();
    got.sort();
    assert_eq!(
        got, want,
        "with_muted lists every chat: there are no mutes to hide yet"
    );

    let junk = get_as(&app, &token, &format!("{ROOT}?pinned=maybe")).await;
    assert_eq!(
        junk.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a filter that is not a boolean fails: {}",
        junk.text()
    );

    let (_, daves_token) = fixture(&pool, "dave", READ_SCOPE).await;
    let empty = get_as(&app, &daves_token, V2_ROOT).await;
    assert_eq!(
        empty.status,
        StatusCode::OK,
        "a chatless account lists the empty array"
    );
    assert_eq!(empty.json_value(), json!([]), "and nothing else");
    assert_eq!(
        empty.header("link"),
        None,
        "an empty page sends no Link header"
    );
}

/// `GET /api/v2/pleroma/chats` is Pleroma's `index2`: the chat whose last
/// message is newest leads the page (a message bumps it, opening never
/// does), the `Link` header rides along, and the cursors walk chat ids.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn the_v2_list_orders_by_the_last_message_and_pages_by_id(pool: PgPool) {
    let (alice, _) = fixture(&pool, "alice", READ_SCOPE).await;
    let (bob, token) = fixture(&pool, "bob", READ_SCOPE).await;
    let carol = common::create_local_user(&pool, "carol").await;
    let with_alice = open_chat(&pool, &bob, &alice).await;
    let with_carol = open_chat(&pool, &bob, &carol).await;
    backdate_chat(&pool, with_alice.id, 2).await;
    backdate_chat(&pool, with_carol.id, 1).await;
    // The newest message moves alice's chat back to the front.
    let (_, _, bobs_inbox) = send(&pool, &alice, &bob, "newest").await;
    let app = common::test_app(pool.clone());

    let response = get_as(&app, &token, V2_ROOT).await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "v2 answers 200: {}",
        response.text()
    );
    let value = response.json_value();
    let ids = listed_ids(&value);
    assert_eq!(
        ids,
        vec![with_alice.id.to_string(), with_carol.id.to_string()],
        "the chat whose last message is newest leads the page"
    );
    assert_eq!(
        value[0]["last_message"]["id"],
        bobs_inbox.id.to_string(),
        "and its entity carries that message"
    );
    assert!(
        value[1]["last_message"].is_null(),
        "carol's chat never had a message at all"
    );
    let want = expected_link(V2_ROOT, &ids[0], &ids[1]);
    assert_eq!(
        response.header("link"),
        Some(want.as_str()),
        "the Link header keys on the chat ids"
    );

    let older = with_alice.id.min(with_carol.id);
    let newer = if older == with_alice.id {
        with_carol.id
    } else {
        with_alice.id
    };
    let since = get_as(&app, &token, &format!("{V2_ROOT}?since_id={older}"))
        .await
        .json_value();
    assert_eq!(
        listed_ids(&since),
        vec![newer.to_string()],
        "since_id keeps only the chat with the newer id"
    );
    let tail = get_as(&app, &token, &format!("{V2_ROOT}?max_id={newer}"))
        .await
        .json_value();
    assert_eq!(
        listed_ids(&tail),
        vec![older.to_string()],
        "and max_id walks back to the older one"
    );
}

/// Both lists default to a page of 20; the legacy one caps at 30 while the
/// v2 page goes on to [`MAX_LIMIT`], and every full page links onward.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn the_lists_default_to_twenty_and_v1_caps_at_thirty(pool: PgPool) {
    let (alice, token) = fixture(&pool, "alice", READ_SCOPE).await;
    for index in 0..32 {
        let partner = common::create_local_user(&pool, &format!("partner{index}")).await;
        let _opened: chat::Chat = open_chat(&pool, &alice, &partner).await;
    }
    let app = common::test_app(pool.clone());

    let legacy = get_as(&app, &token, ROOT).await;
    assert_eq!(
        listed_ids(&legacy.json_value()).len(),
        20,
        "the v1 default page is 20"
    );
    assert_eq!(
        listed_ids(&get_as(&app, &token, V2_ROOT).await.json_value()).len(),
        20,
        "and so is v2's"
    );
    let big_legacy = get_as(&app, &token, &format!("{ROOT}?limit=35")).await;
    assert_eq!(
        listed_ids(&big_legacy.json_value()).len(),
        30,
        "the legacy page stops at its cap of 30"
    );
    assert!(
        big_legacy.header("link").is_some(),
        "a full page links onward"
    );
    let big_v2 = get_as(&app, &token, &format!("{V2_ROOT}?limit=35")).await;
    assert_eq!(
        listed_ids(&big_v2.json_value()).len(),
        32,
        "v2 lets a page of 35 through — every chat fits under its cap"
    );
}

/// The three reading routes — both lists and the show — demand a user token
/// granting `read:chats`: no token is the 401 "Invalid credentials", a bad
/// one the 401 "The access token is invalid", an app-only token the 403
/// demanding a user, and a `write:chats` or `read:accounts` token the 403
/// naming the missing scope; the plain `read` root covers the sub-scope.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn every_reading_route_demands_read_chats(pool: PgPool) {
    let started = thread(&pool).await;
    let read_token = common::create_app_and_token(&pool, &started.bob, READ_SCOPE).await;
    let broad_token = common::create_app_and_token(&pool, &started.bob, "read").await;
    let (_, narrow) = fixture(&pool, "erin", "read:accounts").await;
    let app_only = common::create_app_only_token(&pool, READ_SCOPE).await;
    let app = common::test_app(pool.clone());
    let paths = [
        ROOT.to_owned(),
        V2_ROOT.to_owned(),
        format!("{ROOT}/{}", started.chat_id()),
    ];

    for path in paths {
        let anonymous = common::TestRequest::get(&path).send(&app).await;
        assert_eq!(
            anonymous.status,
            StatusCode::UNAUTHORIZED,
            "no token at all is a 401 on {path}"
        );
        assert_eq!(
            error_message(&anonymous).as_deref(),
            Some("Invalid credentials"),
            "the missing-token wording on {path}"
        );

        let invalid = get_as(&app, "not-a-real-token", &path).await;
        assert_eq!(
            invalid.status,
            StatusCode::UNAUTHORIZED,
            "an invalid token is a 401 on {path}"
        );
        assert_eq!(
            error_message(&invalid).as_deref(),
            Some("The access token is invalid"),
            "the token refusal wording on {path}"
        );

        let wrong_kind = get_as(&app, &app_only, &path).await;
        assert_eq!(
            wrong_kind.status,
            StatusCode::FORBIDDEN,
            "an app-only token is a 403 on {path}"
        );
        assert_eq!(
            error_message(&wrong_kind).as_deref(),
            Some("This API requires an authenticated user"),
            "the user refusal wording on {path}"
        );

        let wrong_scope = get_as(&app, &narrow, &path).await;
        assert_eq!(
            wrong_scope.status,
            StatusCode::FORBIDDEN,
            "`read:accounts` is not enough on {path}"
        );
        assert_eq!(
            error_message(&wrong_scope).as_deref(),
            Some("Insufficient permissions: read:chats."),
            "the scope refusal wording on {path}"
        );

        let write_only = get_as(&app, &started.token, &path).await;
        assert_eq!(
            write_only.status,
            StatusCode::FORBIDDEN,
            "`write:chats` reads nothing on {path}"
        );
    }

    let owned = format!("{ROOT}/{}", started.chat_id());
    assert_eq!(
        get_as(&app, &read_token, ROOT).await.status,
        StatusCode::OK,
        "bob lists with `read:chats`"
    );
    assert_eq!(
        get_as(&app, &read_token, V2_ROOT).await.status,
        StatusCode::OK,
        "on v2 alike"
    );
    assert_eq!(
        get_as(&app, &read_token, &owned).await.status,
        StatusCode::OK,
        "and shows his own chat"
    );
    assert_eq!(
        get_as(&app, &broad_token, ROOT).await.status,
        StatusCode::OK,
        "the plain `read` root covers `read:chats`"
    );
}

/// `POST …/by-account-id/{id}` demands `write:chats` the same way — the
/// read scopes cannot open chats — and the plain `write` root covers the
/// sub-scope on the token's own chats.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn creating_a_chat_demands_write_chats(pool: PgPool) {
    let (alice, read_token) = fixture(&pool, "alice", READ_SCOPE).await;
    let (erin, _) = fixture(&pool, "erin", "read").await;
    let erin_write = common::create_app_and_token(&pool, &erin, "write").await;
    let app_only = common::create_app_only_token(&pool, SCOPE).await;
    let app = common::test_app(pool.clone());
    let path = open_path(&alice.id.to_string());

    let reader = post_empty(&app, &read_token, &path).await;
    assert_eq!(
        reader.status,
        StatusCode::FORBIDDEN,
        "a reader may not open chats: {}",
        reader.text()
    );
    assert_eq!(
        error_message(&reader).as_deref(),
        Some("Insufficient permissions: write:chats."),
        "the scope refusal wording"
    );

    let anonymous = common::TestRequest::post(&path).send(&app).await;
    assert_eq!(
        anonymous.status,
        StatusCode::UNAUTHORIZED,
        "no token is a 401 on create too"
    );
    assert_eq!(
        error_message(&anonymous).as_deref(),
        Some("Invalid credentials"),
        "the missing-token wording"
    );

    let wrong_kind = post_empty(&app, &app_only, &path).await;
    assert_eq!(
        wrong_kind.status,
        StatusCode::FORBIDDEN,
        "an app-only token may not open chats"
    );
    assert_eq!(
        error_message(&wrong_kind).as_deref(),
        Some("This API requires an authenticated user"),
        "the user refusal wording"
    );

    let broad = post_empty(&app, &erin_write, &path).await;
    assert_eq!(
        broad.status,
        StatusCode::OK,
        "the plain `write` root covers `write:chats`: {}",
        broad.text()
    );
    assert_eq!(
        broad.json_value()["account"]["id"],
        alice.id.to_string(),
        "and opens erin's own chat with alice"
    );
}
