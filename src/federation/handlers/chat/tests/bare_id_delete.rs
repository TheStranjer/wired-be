//! Tests of a chat message withdrawn by a `Delete` that declares no
//! `ChatMessage` type — Pleroma's bare object id, or an embedded
//! `Tombstone` — which reaches the note handler's fold rather than this
//! module's typed entry.

use serde_json::{Value, json};

use crate::{
    config::{Config, TEST_BASE_URL},
    federation::{
        dispatch::{ActivityView, Outcome},
        handlers::{chat::chat_mention, note::delete_note},
    },
    models::{
        chat::{self, ChatMessage, NewChatMessage},
        notification,
        user::{self, Flag, NewUser, User},
    },
    state::AppState,
};

/// The object id of the delivered chat message every fixture here deletes.
const CHAT_ID: &str = "https://remote.example/objects/chat-1";

/// A `Delete` of `object` delivered by `actor`, parsed the way the
/// dispatcher parses it.
fn delete_view(actor: &str, object: &Value) -> ActivityView {
    ActivityView::parse(&json!({
        "id": "https://remote.example/activities/delete-1",
        "type": "Delete",
        "actor": actor,
        "object": object,
    }))
    .expect("the fixture is a dispatchable activity")
}

/// A stored remote account `nick` on `remote.example`.
async fn remote_user(pool: &sqlx::PgPool, nick: &str) -> User {
    user::insert(
        pool,
        &NewUser {
            nickname: format!("{nick}@remote.example"),
            ap_id: format!("https://remote.example/users/{nick}"),
            local: Flag::new(false),
            domain: Some("remote.example".to_owned()),
            ..NewUser::default()
        },
    )
    .await
    .expect("the remote account fixture inserts")
}

/// Bob's delivered chat message to a local alice, stored with its
/// `pleroma:chat_mention` notification: the sender, and the message.
async fn delivered_chat(pool: &sqlx::PgPool) -> (User, ChatMessage) {
    let bob = remote_user(pool, "bob").await;
    let alice = user::insert(
        pool,
        &NewUser::local("alice", &user::local_ap_id(TEST_BASE_URL, "alice")),
    )
    .await
    .expect("the local recipient inserts");
    let message = {
        let mut conn = pool.acquire().await.expect("a connection");
        let (message, _sent, _received) = chat::add_message(
            &mut conn,
            TEST_BASE_URL,
            &bob,
            &alice,
            &NewChatMessage {
                content: "<p>hi alice</p>".to_owned(),
                raw_text: "hi alice".to_owned(),
                attachment_media_id: None,
                ap_id: Some(CHAT_ID.to_owned()),
                idempotency_key: None,
                inserted_at: None,
            },
        )
        .await
        .expect("the message is stored");
        message
    };
    let _told = notification::create(pool, &chat_mention(&bob, &alice, message.id))
        .await
        .expect("the mention is stored");
    (bob, message)
}

/// The `Delete` handler outcome of `object` delivered by `signer`, through
/// the note handler the fallback and `Tombstone` routes reach.
async fn note_delete(pool: &sqlx::PgPool, signer: User, object: &Value) -> Outcome {
    let state = AppState::for_tests(pool.clone(), Config::for_tests());
    let view = delete_view(&signer.ap_id, object);
    delete_note(state, view, signer)
        .await
        .expect("the fold completes")
}

/// Whether the message is withdrawn, how many notifications name it, and
/// how many `statuses` rows carry its uri.
async fn chat_state(pool: &sqlx::PgPool, message: &ChatMessage) -> (bool, i64, i64) {
    let stored = chat::message_by_ap_id(pool, CHAT_ID)
        .await
        .expect("the message read")
        .expect("the row stays, at worst as a tombstone");
    let notifications = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM notifications WHERE chat_message_id = $1",
    )
    .bind(message.id)
    .fetch_one(pool)
    .await
    .expect("the notification count reads");
    let statuses = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM statuses WHERE uri = $1")
        .bind(CHAT_ID)
        .fetch_one(pool)
        .await
        .expect("the status count reads");
    (stored.deleted_at.is_some(), notifications, statuses)
}

/// Pleroma's bare-id `Delete` by the sender tombstones the message, drops
/// its notification and reserves no status tombstone.
#[tokio::test]
async fn a_bare_id_delete_withdraws_the_chat_message() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let (bob, message) = delivered_chat(&pool).await;
    assert_eq!(
        chat_state(&pool, &message).await,
        (false, 1, 0),
        "the fixture starts live and told"
    );

    let outcome = note_delete(&pool, bob, &json!(CHAT_ID)).await;

    assert_eq!(outcome, Outcome::Handled, "the withdrawal is folded");
    assert_eq!(
        chat_state(&pool, &message).await,
        (true, 0, 0),
        "the row is a tombstone, the telling is gone, and no status tombstone appears"
    );
}

/// An embedded `Tombstone` `Delete` by the sender withdraws it the same way.
#[tokio::test]
async fn a_tombstone_delete_withdraws_the_chat_message() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let (bob, message) = delivered_chat(&pool).await;

    let outcome = note_delete(&pool, bob, &json!({"id": CHAT_ID, "type": "Tombstone"})).await;

    assert_eq!(outcome, Outcome::Handled, "the withdrawal is folded");
    assert_eq!(
        chat_state(&pool, &message).await,
        (true, 0, 0),
        "an embedded Tombstone withdraws it just the same"
    );
}

/// A bare-id `Delete` by an account other than the sender leaves the
/// message, its notification and the `statuses` table untouched.
#[tokio::test]
async fn a_bare_id_delete_by_someone_else_leaves_the_message() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let (_bob, message) = delivered_chat(&pool).await;
    let erin = remote_user(&pool, "erin").await;

    let outcome = note_delete(&pool, erin, &json!(CHAT_ID)).await;

    assert_eq!(
        outcome,
        Outcome::Ignored("a chat message Delete by an account that is not its sender"),
        "nobody else's Delete withdraws it"
    );
    assert_eq!(
        chat_state(&pool, &message).await,
        (false, 1, 0),
        "the message and its telling stand, and no status tombstone is written"
    );
}
