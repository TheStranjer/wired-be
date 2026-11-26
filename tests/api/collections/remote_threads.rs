//! Threads rooted on another instance: local replies repeat the thread's
//! own context, and this instance serves no context collection for it.

use axum::http::StatusCode;
use sqlx::PgPool;
use wired_be::models::{
    status::{self, NewStatus, Status, Visibility},
    user::User,
};

use super::support::{BASE, fetch_ap, reply_to};
use crate::common;

/// The context the remote thread advertises.
const REMOTE_CONTEXT: &str = "https://mastodon.test/contexts/117672794037874860-117680310363400761";

/// A remote public status by `author` that advertised `context`.
async fn remote_post(pool: &PgPool, author: &User, uri: &str, context: Option<&str>) -> Status {
    status::insert(
        pool,
        BASE,
        &NewStatus {
            uri: Some(uri.to_owned()),
            user_id: author.id,
            content: "<p>remote</p>".to_owned(),
            raw_text: "remote".to_owned(),
            ap_context: context.map(str::to_owned),
            ..NewStatus::default()
        },
    )
    .await
    .expect("the remote status fixture inserts")
}

/// The `context` and `conversation` the served note of `row` names.
async fn named_context(app: &axum::Router, row: &Status) -> (String, String) {
    let note = fetch_ap(app, &format!("/objects/{}", row.id)).await;
    common::assert_answer(&note, StatusCode::OK, "the local note is served");
    let document = note.json_value();
    let text = |key: &str| document[key].as_str().unwrap_or_default().to_owned();
    (text("context"), text("conversation"))
}

/// A reply to a remote status, and a reply to that reply, name the context
/// the remote thread advertised, as `context` and as `conversation`.
#[tokio::test]
async fn replies_in_a_remote_thread_repeat_its_context() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_remote_user(&pool, "alice", "mastodon.test").await;
    let r4w1 = common::create_local_user(&pool, "r4w1").await;
    let root = remote_post(
        &pool,
        &alice,
        "https://mastodon.test/users/alice/statuses/1",
        Some(REMOTE_CONTEXT),
    )
    .await;
    let reply = reply_to(&pool, &r4w1, &root, "hi alice", Visibility::Public).await;
    let deeper = reply_to(&pool, &r4w1, &reply, "and again", Visibility::Public).await;
    let app = common::test_app(pool);

    for row in [&reply, &deeper] {
        let (context, conversation) = named_context(&app, row).await;
        assert_eq!(context, REMOTE_CONTEXT, "the thread's own context");
        assert_eq!(conversation, REMOTE_CONTEXT, "and the same conversation");
    }
}

/// A thread rooted here keeps the local context, replies included.
#[tokio::test]
async fn a_local_thread_keeps_its_local_context() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_remote_user(&pool, "bob", "mastodon.test").await;
    let root = common::create_status(&pool, &alice, "ask", Visibility::Public).await;
    let theirs = remote_post(
        &pool,
        &bob,
        "https://mastodon.test/users/bob/statuses/2",
        Some(REMOTE_CONTEXT),
    )
    .await;
    let reply = reply_to(&pool, &alice, &root, "again", Visibility::Public).await;
    let app = common::test_app(pool);

    let expected = format!("{BASE}/contexts/{}", root.conversation_id);
    for row in [&root, &reply] {
        let (context, conversation) = named_context(&app, row).await;
        assert_eq!(context, expected, "the local context");
        assert_eq!(conversation, expected, "as the conversation too");
    }
    assert_ne!(
        theirs.conversation_id, root.conversation_id,
        "another thread"
    );
}

/// A remote thread whose posts carried no context leaves the local one.
#[tokio::test]
async fn a_remote_thread_without_a_context_falls_back_to_the_local_one() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_remote_user(&pool, "alice", "mastodon.test").await;
    let r4w1 = common::create_local_user(&pool, "r4w1").await;
    let root = remote_post(
        &pool,
        &alice,
        "https://mastodon.test/users/alice/statuses/3",
        None,
    )
    .await;
    let reply = reply_to(&pool, &r4w1, &root, "hi", Visibility::Public).await;
    let app = common::test_app(pool);

    let (context, _conversation) = named_context(&app, &reply).await;
    assert_eq!(
        context,
        format!("{BASE}/contexts/{}", root.conversation_id),
        "no stored context, the local address"
    );
}

/// The context route answers only for threads this instance owns.
#[tokio::test]
async fn the_context_of_a_remote_thread_is_not_served() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_remote_user(&pool, "alice", "mastodon.test").await;
    let r4w1 = common::create_local_user(&pool, "r4w1").await;
    let root = remote_post(
        &pool,
        &alice,
        "https://mastodon.test/users/alice/statuses/4",
        Some(REMOTE_CONTEXT),
    )
    .await;
    let reply = reply_to(&pool, &r4w1, &root, "hi", Visibility::Public).await;
    let app = common::test_app(pool);

    for id in [root.conversation_id, reply.conversation_id] {
        for query in ["", "?page=true"] {
            let path = format!("/contexts/{id}{query}");
            let response = fetch_ap(&app, &path).await;
            common::assert_answer(&response, StatusCode::NOT_FOUND, &path);
        }
    }
}
