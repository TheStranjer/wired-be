//! Fixtures and request helpers the collection tests share.

use axum::Router;
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::TEST_BASE_URL,
    federation::signature::SignedHeaders,
    models::{
        follow::{self, Follow},
        pinned_status,
        status::{self, NewStatus, Status, Visibility},
        user::{self, User},
    },
};

use crate::common;

/// The media type of an `ActivityPub` document, charset included.
pub(super) const ACTIVITY_JSON: &str = "application/activity+json; charset=utf-8";
/// The instance origin of [`common::test_config`].
pub(super) const BASE: &str = TEST_BASE_URL;
/// The `ActivityStreams` public addressing collection.
pub(super) const AS_PUBLIC: &str = "https://www.w3.org/ns/activitystreams#Public";
/// The refusal body of every unsatisfied `AUTHORIZED_FETCH` check.
pub(super) const REFUSAL: &str = "This instance requires authenticated fetches";

/// Fetches `path` as an `ActivityPub` client.
pub(super) async fn fetch_ap(app: &Router, path: &str) -> common::TestResponse {
    common::TestRequest::get(path)
        .header("accept", "application/activity+json")
        .send(app)
        .await
}

/// The sorted key set of a document object.
pub(super) fn key_set(document: &Value) -> Vec<&str> {
    let mut keys: Vec<&str> = document
        .as_object()
        .expect("a document is a JSON object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    keys
}

/// The account named `nickname` as stored now — the row whose denormalised
/// counters the collection roots show.
pub(super) async fn stored_user(pool: &PgPool, nickname: &str) -> User {
    user::get_by_nickname(pool, nickname)
        .await
        .expect("the user row reads back")
        .expect("the fixture user exists")
}

/// `author`'s answer to `parent` as a status of `visibility`.
pub(super) async fn reply_to(
    pool: &PgPool,
    author: &User,
    parent: &Status,
    text: &str,
    visibility: Visibility,
) -> Status {
    status::insert(
        pool,
        BASE,
        &NewStatus {
            in_reply_to_id: Some(parent.id),
            visibility,
            ..NewStatus::local(author, text)
        },
    )
    .await
    .expect("a reply fixture inserts")
}

/// `author`'s public boost of `original`, as its reblog row.
pub(super) async fn boost(pool: &PgPool, author: &User, original: &Status) -> Status {
    let (row, created) = status::create_reblog(pool, BASE, author, original, Visibility::Public)
        .await
        .expect("a reblog fixture writes");
    assert!(created, "the fixture pair had no live reblog yet");
    row
}

/// `follower`'s follow of the open local `target`, accepted on the spot;
/// the returned row is the relationship the follow collections key on.
pub(super) async fn granted(pool: &PgPool, follower: &User, target: &User) -> Follow {
    follow::follow(pool, BASE, follower, target)
        .await
        .expect("the fixture follow records")
}

/// The pinned-status write the profile's pin endpoint performs, run direct.
pub(super) async fn pin(pool: &PgPool, author: &User, status_id: Uuid) {
    let pinned = pinned_status::pin(pool, author.id, status_id)
        .await
        .expect("the fixture pin writes");
    assert!(pinned.is_some(), "the pair had no pin yet");
}

/// Tombstones `row` (the delete's own cascade of counters and pins runs).
pub(super) async fn unpost(pool: &PgPool, row: &Status) {
    let tombstone = status::soft_delete(pool, row.id)
        .await
        .expect("the fixture delete runs");
    assert!(tombstone.is_some(), "the fixture row was live to delete");
}

/// Alice's fixture for the page tests: a public root with a public answer,
/// an unlisted post, a private, a direct and a deleted row, and a public
/// boost of bob's post — created in that order, so the expected page order
/// is the reverse of it.
#[derive(Debug)]
pub(super) struct OutboxFixture {
    /// The account whose outbox the tests read.
    pub(super) alice: User,
    /// The author of the boosted post.
    pub(super) bob: User,
    /// Alice's oldest public root post.
    pub(super) first: Status,
    /// Alice's answer to `first`.
    pub(super) answer: Status,
    /// Alice's unlisted post.
    pub(super) quiet: Status,
    /// Bob's boosted original.
    pub(super) boosted: Status,
    /// Alice's reblog of `boosted`.
    pub(super) reblog: Status,
}

/// Builds [`OutboxFixture`].
pub(super) async fn outbox_fixture(pool: &PgPool) -> OutboxFixture {
    let alice = common::create_local_user(pool, "alice").await;
    let bob = common::create_local_user(pool, "bob").await;
    let first = common::create_status(pool, &alice, "first", Visibility::Public).await;
    let answer = reply_to(pool, &alice, &first, "answering", Visibility::Public).await;
    let _hidden = common::create_status(pool, &alice, "followers only", Visibility::Private).await;
    let quiet = common::create_status(pool, &alice, "quiet", Visibility::Unlisted).await;
    let _dm = common::create_status(pool, &alice, "dms", Visibility::Direct).await;
    let boosted = common::create_status(pool, &bob, "bob speaks", Visibility::Public).await;
    let reblog = boost(pool, &alice, &boosted).await;
    let gone = common::create_status(pool, &alice, "gone", Visibility::Public).await;
    unpost(pool, &gone).await;
    OutboxFixture {
        alice,
        bob,
        first,
        answer,
        quiet,
        boosted,
        reblog,
    }
}

/// A signed `GET` of `path` as an `ActivityPub` client, with `signed`'s
/// headers added.
pub(super) async fn signed_get(
    app: &Router,
    path: &str,
    signed: &SignedHeaders,
) -> common::TestResponse {
    common::TestRequest::get(path)
        .header("accept", "application/activity+json")
        .header("host", &signed.host)
        .header("date", &signed.date)
        .header("signature", &signed.signature)
        .send(app)
        .await
}
