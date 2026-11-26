//! Threads rooted on another instance: local replies repeat the thread's
//! own context, and this instance serves no context collection for it. A
//! remote thread with no context names a minted address, which is served.

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

/// The context route does not answer for a context the remote thread owns.
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

/// The ids a context page lists, as its `orderedItems`.
fn listed(page: &common::TestResponse) -> Vec<String> {
    let document = page.json_value();
    document["orderedItems"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .map(str::to_owned)
        .collect()
}

/// The minted context a remote thread without a context names is served as
/// the collection of the thread's statuses here.
#[tokio::test]
async fn the_minted_context_of_a_contextless_remote_thread_is_served() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let carol = common::create_remote_user(&pool, "carol", "misskey.test").await;
    let r6a = common::create_local_user(&pool, "r6a").await;
    let root = remote_post(&pool, &carol, "https://misskey.test/notes/carol1", None).await;
    let reply = reply_to(&pool, &r6a, &root, "hi carol", Visibility::Public).await;
    let app = common::test_app(pool);

    let (context, conversation) = named_context(&app, &reply).await;
    let expected = format!("{BASE}/contexts/{}", root.conversation_id);
    assert_eq!(context, expected, "the minted context");
    assert_eq!(conversation, expected, "as the conversation too");

    let path = format!("/contexts/{}", root.conversation_id);
    let rooted = fetch_ap(&app, &path).await;
    common::assert_answer(&rooted, StatusCode::OK, &path);
    assert_eq!(rooted.json_value()["totalItems"], 2, "root and reply");

    let paged = format!("{path}?page=true");
    let page = fetch_ap(&app, &paged).await;
    common::assert_answer(&page, StatusCode::OK, &paged);
    assert_eq!(listed(&page), vec![root.uri.clone(), reply.uri.clone()]);
}

/// Private statuses stay out of the minted context's collection.
#[tokio::test]
async fn the_minted_context_hides_private_statuses() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let carol = common::create_remote_user(&pool, "carol", "misskey.test").await;
    let r6a = common::create_local_user(&pool, "r6a").await;
    let root = remote_post(&pool, &carol, "https://misskey.test/notes/carol2", None).await;
    let open = reply_to(&pool, &r6a, &root, "open", Visibility::Public).await;
    let secret = reply_to(&pool, &r6a, &open, "secret", Visibility::Direct).await;
    let app = common::test_app(pool);

    let path = format!("/contexts/{}?page=true", root.conversation_id);
    let page = fetch_ap(&app, &path).await;
    common::assert_answer(&page, StatusCode::OK, &path);
    let items = listed(&page);
    assert_eq!(items, vec![root.uri.clone(), open.uri.clone()]);
    assert!(!items.contains(&secret.uri), "the direct reply is hidden");
}

/// A minted context with nothing public in it is the plain 404.
#[tokio::test]
async fn a_minted_context_with_only_private_statuses_is_not_found() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let carol = common::create_remote_user(&pool, "carol", "misskey.test").await;
    let r6a = common::create_local_user(&pool, "r6a").await;
    let root = status::insert(
        &pool,
        BASE,
        &NewStatus {
            uri: Some("https://misskey.test/notes/carol3".to_owned()),
            user_id: carol.id,
            content: "<p>dm</p>".to_owned(),
            raw_text: "dm".to_owned(),
            visibility: Visibility::Direct,
            ..NewStatus::default()
        },
    )
    .await
    .expect("the remote direct status inserts");
    let _reply = reply_to(&pool, &r6a, &root, "reply", Visibility::Direct).await;
    let app = common::test_app(pool);

    let path = format!("/contexts/{}", root.conversation_id);
    let response = fetch_ap(&app, &path).await;
    common::assert_answer(&response, StatusCode::NOT_FOUND, &path);
}
