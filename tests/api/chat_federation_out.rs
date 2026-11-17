//! The outbound federation of chat messages over HTTP (WBE-181): the
//! activities `POST /api/v1/pleroma/chats/{id}/messages` and its `DELETE`
//! twin hand the publisher's queue when the chat's counterpart lives on
//! another instance (`src/services/federation_out::after_chat_message_created`
//! and `after_chat_message_deleted`).
//!
//! A message posted to a REMOTE account is announced as a
//! `Create{ChatMessage}`: the object names the message row's reserved
//! `{BASE_URL}/objects/{uuid}` id — minted by the posting write itself —
//! the sender as `attributedTo`, the counterpart as the only `to` address,
//! the stored HTML as `content`, the first media (if the message carries
//! one) as `attachment` and the message's own moment as `published`; the
//! `Create` rides under `{BASE_URL}/activities/{message id}`, and the
//! fan-out delivers it to the counterpart's personal inbox alone. The
//! sender's delete of that message follows as a `Delete` of the same
//! object under `{BASE_URL}/activities/delete-{message id}`; a delete by
//! anybody else withdraws nobody's copy.
//!
//! Chat messages to LOCAL recipients never travel over HTTP — the pair's
//! rows are the delivery — and a counterpart whose `accepts_chat_messages`
//! flag is down is not delivered to. The endpoints' own behaviour is
//! `chats.rs`; the folding of the messages a PEER sends is
//! `chat_federation.rs`; this file is only about what leaves the instance.

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::TEST_BASE_URL,
    federation::{delivery, publisher},
    jobs::AttemptInfo,
    models::{chat, user::User},
    pagination::{MAX_LIMIT, PageParams},
    util::{id::new_id, time::mastodon_time},
};

use crate::common;

/// The chats root both routes live under.
const ROOT: &str = "/api/v1/pleroma/chats";
/// The instance origin the fixtures speak under.
const BASE: &str = TEST_BASE_URL;
/// The host every remote fixture lives on.
const HOST: &str = "remote.example";
/// The scope every write here asks.
const SCOPE: &str = "write:chats";
/// The first attempt a background handler runs on.
const FIRST_ATTEMPT: AttemptInfo = AttemptInfo {
    attempts: 1,
    max_attempts: 5,
};

/// The `@context` every document built here carries, golden-spelled.
fn context() -> Value {
    json!([
        "https://www.w3.org/ns/activitystreams",
        "https://test.example/schemas/litepub-0.1.jsonld",
        { "@language": "und" },
    ])
}

/// A local account with a token granting `scopes`.
async fn chatter(pool: &PgPool, nickname: &str) -> (User, String) {
    let user = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &user, SCOPE).await;
    (user, token)
}

/// `POST …/chats/by-account-id/{id}` as `token`, answered with the chat id.
async fn open_chat(app: &Router, token: &str, account: &Uuid) -> String {
    let response = common::TestRequest::post(&format!("{ROOT}/by-account-id/{account}"))
        .bearer(token)
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the pair's chat opens");
    response.json_value()["id"]
        .as_str()
        .expect("the chat entity carries its id")
        .to_owned()
}

/// `POST …/chats/{chat}/messages` as `token` with the JSON `body`,
/// asserted accepted and answered as the sender's message entity.
async fn send_message(app: &Router, token: &str, chat: &str, body: Value) -> Value {
    let response = common::TestRequest::post(&format!("{ROOT}/{chat}/messages"))
        .bearer(token)
        .json(&body)
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the message posts");
    response.json_value()
}

/// `DELETE …/chats/{chat}/messages/{message}` as `token`, asserted
/// accepted.
async fn delete_message(app: &Router, token: &str, chat: &str, message: &str) {
    let response = common::TestRequest::delete(&format!("{ROOT}/{chat}/messages/{message}"))
        .bearer(token)
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the delete runs");
}

/// The one stored `chat_messages` row, newest first.
async fn stored_message(pool: &PgPool) -> chat::ChatMessage {
    sqlx::query_as::<_, chat::ChatMessage>(
        "SELECT * FROM chat_messages ORDER BY inserted_at DESC, id DESC LIMIT 1",
    )
    .fetch_one(pool)
    .await
    .expect("the stored message reads")
}

/// A local media row of `uploader` — image, unclaimed — for the chat
/// attachment gate, with its id and the url it stores.
async fn insert_image(pool: &PgPool, uploader: &User) -> Uuid {
    let row: (Uuid,) = sqlx::query_as(
        "INSERT INTO media_attachments (id, user_id, url, content_type, description)
         VALUES ($1, $2, 'https://test.example/media/cat.png', 'image/png', 'a cat')
         RETURNING id",
    )
    .bind(new_id())
    .bind(uploader.id)
    .fetch_one(pool)
    .await
    .expect("the media fixture inserts");
    row.0
}

/// The queued `federator.publish` activities, in insertion order.
async fn published(pool: &PgPool) -> Vec<Value> {
    sqlx::query_as::<_, (Value,)>(
        "SELECT args->'activity' FROM jobs WHERE kind = $1 ORDER BY inserted_at, id",
    )
    .bind(publisher::KIND_PUBLISH)
    .fetch_all(pool)
    .await
    .expect("the publish jobs read")
    .into_iter()
    .map(|(activity,)| activity)
    .collect()
}

/// The activity of the one `federator.publish` job the queue holds.
async fn the_published_activity(pool: &PgPool) -> Value {
    let jobs = published(pool).await;
    assert_eq!(jobs.len(), 1, "the write is announced once");
    jobs[0].clone()
}

/// The activity of the most recently queued `federator.publish` job: the
/// write under test, once the fixture has announced the posting beside it.
async fn the_latest_activity(pool: &PgPool) -> Value {
    let jobs = published(pool).await;
    let last = jobs.last().expect("a publish job is queued");
    last.clone()
}

/// How many `federator.deliver` jobs the queue holds, of any activity.
async fn delivery_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM jobs WHERE kind = $1")
        .bind(delivery::KIND_DELIVER)
        .fetch_one(pool)
        .await
        .expect("the delivery count reads")
}

/// Runs every queued publish through the publisher's own handler, so the
/// addressing turns into the delivery jobs a worker would send.
async fn fan_out(pool: &PgPool) {
    let state = common::test_app_state(pool.clone(), common::test_config());
    let rows = sqlx::query_as::<_, (Value,)>(
        "SELECT args FROM jobs WHERE kind = $1 ORDER BY inserted_at, id",
    )
    .bind(publisher::KIND_PUBLISH)
    .fetch_all(pool)
    .await
    .expect("the publish jobs read");
    for (args,) in rows {
        publisher::handle(state.clone(), FIRST_ATTEMPT, args)
            .await
            .expect("the fan-out job completes");
    }
}

/// The inboxes the queued `federator.deliver` jobs name, sorted.
async fn delivered(pool: &PgPool) -> Vec<String> {
    sqlx::query_as::<_, (String,)>("SELECT args->>'inbox' FROM jobs WHERE kind = $1 ORDER BY 1")
        .bind(delivery::KIND_DELIVER)
        .fetch_all(pool)
        .await
        .expect("the delivery jobs read")
        .into_iter()
        .map(|(inbox,)| inbox)
        .collect()
}

/// The whole `Create{ChatMessage}` the ticket pins for `message`: the
/// reserved object id, the sender as `attributedTo`, the counterpart as
/// the only `to` address, the stored text and moment — and `attachment`
/// when `attachment` names the media the message carries.
fn expected_create(
    message: &chat::ChatMessage,
    sender: &User,
    recipient: &User,
    attachment: Option<Value>,
) -> Value {
    let object = attachment.map_or_else(
        || {
            json!({
                "id": message.ap_id,
                "type": "ChatMessage",
                "attributedTo": sender.ap_id,
                "to": [recipient.ap_id],
                "content": message.content,
                "published": mastodon_time(&message.inserted_at),
            })
        },
        |document| {
            json!({
                "id": message.ap_id,
                "type": "ChatMessage",
                "attributedTo": sender.ap_id,
                "to": [recipient.ap_id],
                "content": message.content,
                "published": mastodon_time(&message.inserted_at),
                "attachment": document,
            })
        },
    );
    json!({
        "@context": context(),
        "id": format!("{BASE}/activities/{}", message.id),
        "type": "Create",
        "actor": sender.ap_id,
        "to": [recipient.ap_id],
        "cc": [],
        "object": object,
    })
}

/// `alice → erin`: a chat message posted to a REMOTE account mints its
/// object id on the posting write and publishes the whole
/// `Create{ChatMessage}` — text only, no `attachment` key — delivered to
/// the counterpart's personal inbox alone.
#[tokio::test]
async fn a_message_to_a_remote_account_publishes_the_chat_create() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = chatter(&pool, "alice").await;
    let erin = common::create_remote_user(&pool, "erin", HOST).await;

    let chat_id = open_chat(&app, &token, &erin.id).await;
    let _sent = send_message(
        &app,
        &token,
        &chat_id,
        json!({"content": "hello over the wire"}),
    )
    .await;

    let message = stored_message(&pool).await;
    assert_eq!(
        message.ap_id.as_deref(),
        Some(format!("{BASE}/objects/{}", message.id).as_str()),
        "the posting write minted this instance's object id"
    );

    let activity = the_published_activity(&pool).await;
    assert_eq!(
        activity,
        expected_create(&message, &alice, &erin, None),
        "the whole delivery JSON for the text-only message"
    );
    assert!(
        activity["object"].get("attachment").is_none(),
        "a message without media carries no attachment key"
    );

    fan_out(&pool).await;
    assert_eq!(
        delivered(&pool).await,
        vec!["https://remote.example/users/erin/inbox"],
        "the counterpart's personal inbox is the only delivery"
    );
}

/// A message carrying media federates the first one as the note builder's
/// `attachment` document, beside its text.
#[tokio::test]
async fn a_message_with_media_publishes_the_first_attachment() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = chatter(&pool, "alice").await;
    let erin = common::create_remote_user(&pool, "erin", HOST).await;
    let media_id = insert_image(&pool, &alice).await;

    let chat_id = open_chat(&app, &token, &erin.id).await;
    let _sent = send_message(
        &app,
        &token,
        &chat_id,
        json!({"content": "look", "media_id": media_id.to_string()}),
    )
    .await;

    let message = stored_message(&pool).await;
    let activity = the_published_activity(&pool).await;
    let expected = expected_create(
        &message,
        &alice,
        &erin,
        Some(json!({
            "type": "Image",
            "mediaType": "image/png",
            "url": "https://test.example/media/cat.png",
            "name": "a cat",
            "blurhash": Value::Null,
        })),
    );
    assert_eq!(activity, expected, "the whole delivery JSON, media and all");

    fan_out(&pool).await;
    assert_eq!(
        delivered(&pool).await,
        vec!["https://remote.example/users/erin/inbox"],
        "and the one delivery it always had"
    );
}

/// The sender's delete of a delivered message follows as a `Delete` of the
/// same object, under the deterministic withdrawal id and the same one
/// recipient as the `Create` it takes back.
#[tokio::test]
async fn the_senders_delete_publishes_the_chat_delete() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = chatter(&pool, "alice").await;
    let erin = common::create_remote_user(&pool, "erin", HOST).await;

    let chat_id = open_chat(&app, &token, &erin.id).await;
    let entity = send_message(&app, &token, &chat_id, json!({"content": "forget it"})).await;
    let message = stored_message(&pool).await;
    delete_message(
        &app,
        &token,
        &chat_id,
        entity["id"].as_str().expect("the reference id"),
    )
    .await;

    let activity = the_latest_activity(&pool).await;
    assert_eq!(
        activity,
        json!({
            "@context": context(),
            "id": format!("{BASE}/activities/delete-{}", message.id),
            "type": "Delete",
            "actor": alice.ap_id,
            "to": [erin.ap_id],
            "cc": [],
            "object": {
                "id": format!("{BASE}/objects/{}", message.id),
                "type": "ChatMessage",
            },
        }),
        "the whole withdrawal, field for field"
    );

    fan_out(&pool).await;
    assert_eq!(
        delivered(&pool).await,
        vec![
            "https://remote.example/users/erin/inbox",
            "https://remote.example/users/erin/inbox",
        ],
        "create and delete both reach the counterpart's inbox, as two activities"
    );
}

/// Chat messages to LOCAL recipients are never delivered over HTTP: the
/// posting, the sender's delete and a participant's withdrawal of their
/// own reference all announce nothing, local pair or remote pair alike.
#[tokio::test]
async fn a_local_counterpart_is_never_delivered_to() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, alice_token) = chatter(&pool, "alice").await;
    let (bob, bob_token) = chatter(&pool, "bob").await;

    let alice_chat = open_chat(&app, &alice_token, &bob.id).await;
    let entity = send_message(&app, &alice_token, &alice_chat, json!({"content": "hi"})).await;

    // bob's own side of the pair, for the not-the-sender delete.
    let bobs_chat = chat::get_or_create(&pool, &bob, &alice)
        .await
        .expect("bob's side of the pair reads");
    let page = chat::messages_page(
        &pool,
        bobs_chat.id,
        PageParams::from_query(Some("limit=10"), MAX_LIMIT).expect("the page parses"),
    )
    .await
    .expect("bob's messages read");
    let bobs_reference = page.first().expect("bob received the message");

    // The recipient's withdrawal of their own reference is nobody else's news.
    delete_message(
        &app,
        &bob_token,
        &bobs_chat.id.to_string(),
        &bobs_reference.id.to_string(),
    )
    .await;
    // And the sender's delete, for a local pair, stays local too.
    delete_message(
        &app,
        &alice_token,
        &alice_chat,
        entity["id"].as_str().expect("the reference id"),
    )
    .await;

    assert!(
        published(&pool).await.is_empty(),
        "a local pair announces nothing"
    );
    assert_eq!(delivery_count(&pool).await, 0, "and delivers nothing");
}

/// The counterpart's `accepts_chat_messages` flag still gates the wire
/// after the chat stands: with the flag down the message posts — the
/// sender's own rows always take it — but nothing is delivered.
#[tokio::test]
async fn a_remote_counterpart_with_the_flag_down_is_not_delivered_to() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_, token) = chatter(&pool, "alice").await;
    let erin = common::create_remote_user(&pool, "erin", HOST).await;
    let chat_id = open_chat(&app, &token, &erin.id).await;
    let _quieted: sqlx::postgres::PgQueryResult =
        sqlx::query("UPDATE users SET accepts_chat_messages = false WHERE id = $1")
            .bind(erin.id)
            .execute(&pool)
            .await
            .expect("the flag update runs");

    let _sent = send_message(&app, &token, &chat_id, json!({"content": "anybody there?"})).await;

    assert_eq!(
        stored_message(&pool).await.content,
        "<p>anybody there?</p>",
        "the message itself stands"
    );
    assert!(
        published(&pool).await.is_empty(),
        "and the lowered flag keeps it off the wire"
    );
}
