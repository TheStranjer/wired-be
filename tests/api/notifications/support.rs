//! Shared consts, fixtures and helpers for every submodule of the
//! `notifications` area: the route paths and scopes the tests use, the
//! thin wrappers around the HTTP harness, the readers of the flat and
//! grouped listings, and the seeds the dismissal, read, listing and mute
//! tests start from. Everything here is `pub(super)`, and a submodule
//! imports exactly the members it calls.

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::models::{
    block, conversation_mute, domain_block,
    mute::{self, MuteOptions},
    notification::Notification,
    status::Visibility,
    user::User,
};

use crate::common;

/// The v1 routes' mount point: the listing, the read and the dismissals.
pub(super) const V1: &str = "/api/v1/notifications";
/// Pleroma's read route.
pub(super) const READ_ROUTE: &str = "/api/pleroma/notifications/read";
/// The scope the two reads demand.
pub(super) const READ: &str = "read:notifications";
/// The scope every write route in this file demands.
pub(super) const WRITE: &str = "write:notifications";
/// Both halves, for fixtures that also observe through the v2 listing.
pub(super) const READ_WRITE: &str = "read:notifications write:notifications";
/// The message of the shared `AppError::NotFound` body.
pub(super) const NOT_FOUND: &str = "Record not found";
/// The `Link` header's base, `Config::for_tests`' `BASE_URL`.
pub(super) const BASE: &str = "https://test.example";

/// A local user with a `scopes`-granted bearer token: `(user, token)`.
pub(super) async fn fixture(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let user = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &user, scopes).await;
    (user, token)
}

/// The group keys of the v2 listing, in answer order (newest first).
pub(super) async fn listed_keys(app: &Router, token: &str) -> Vec<String> {
    let response = common::TestRequest::get("/api/v2/notifications")
        .bearer(token)
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the grouped listing");
    response.json_value()["notification_groups"]
        .as_array()
        .expect("notification_groups is a list")
        .iter()
        .map(|group| group["group_key"].as_str().expect("a group key").to_owned())
        .collect()
}

/// The `ungrouped-<id>` key of `note`.
pub(super) fn ungrouped(note: &Notification) -> String {
    format!("ungrouped-{}", note.id)
}

/// `GET /api/v2/notifications/unread_count`'s count for `token`.
pub(super) async fn unread(app: &Router, token: &str) -> i64 {
    let response = common::TestRequest::get("/api/v2/notifications/unread_count")
        .bearer(token)
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the unread count");
    response.json_value()["count"].as_i64().expect("a count")
}

/// The stored row of `id`, whatever its state, for the state assertions.
pub(super) async fn stored_row(pool: &PgPool, id: Uuid) -> Option<Notification> {
    sqlx::query_as::<_, Notification>("SELECT * FROM notifications WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .expect("the state read runs")
}

/// Whether the stored row reads seen.
pub(super) async fn is_seen(pool: &PgPool, id: Uuid) -> bool {
    stored_row(pool, id).await.expect("the row is stored").seen
}

/// The `POST` of `path` with a JSON `body`, as `token`'s bearer.
pub(super) async fn post_json(
    app: &Router,
    path: &str,
    token: &str,
    body: Value,
) -> common::TestResponse {
    common::TestRequest::post(path)
        .bearer(token)
        .json(&body)
        .send(app)
        .await
}

/// The `{}` answer of the dismissal routes.
pub(super) fn assert_empty_object(response: &common::TestResponse, who: &str) {
    common::assert_answer(response, StatusCode::OK, who);
    assert_eq!(response.json_value(), json!({}), "{who} answers `{{}}`");
}

/// Alice's two notifications, oldest first: a favourite of `post` (held on
/// the seed) by bob, and a follow by carol. Returns the viewer token that
/// also reads the v2 listing.
pub(super) struct Pair {
    /// Alice's `read:notifications write:notifications` token.
    pub(super) token: String,
    /// The favourited status.
    pub(super) post: wired_be::models::status::Status,
    /// The favourite (older).
    pub(super) favourite: Notification,
    /// The follow (newer).
    pub(super) follow: Notification,
}

pub(super) async fn seed_pair(pool: &PgPool) -> Pair {
    let (alice, token) = fixture(pool, "alice", READ_WRITE).await;
    let erin = common::create_local_user(pool, "erin").await;
    let bob = common::create_local_user(pool, "bob").await;
    let carol = common::create_local_user(pool, "carol").await;
    let post = common::create_status(pool, &erin, "hello", Visibility::Public).await;
    let favourite = common::create_notification(pool, &alice, &bob, "favourite", Some(&post)).await;
    let follow = common::create_notification(pool, &alice, &carol, "follow", None).await;
    Pair {
        token,
        post,
        favourite,
        follow,
    }
}

/// `GET /api/v1/notifications` as `token`, `query` (a `?…` string or empty)
/// appended verbatim.
pub(super) async fn list(app: &Router, token: &str, query: &str) -> common::TestResponse {
    common::TestRequest::get(&format!("{V1}{query}"))
        .bearer(token)
        .send(app)
        .await
}

/// The entity ids of a flat listing answer, in answer order.
pub(super) fn listed_ids(response: &common::TestResponse) -> Vec<String> {
    response
        .json_value()
        .as_array()
        .expect("the flat listing is an array")
        .iter()
        .map(|entity| entity["id"].as_str().expect("an entity id").to_owned())
        .collect()
}

/// The keys of an entity object, sorted, for exact shape assertions.
pub(super) fn key_set(entity: &Value) -> Vec<String> {
    let mut keys: Vec<String> = entity
        .as_object()
        .expect("entities render objects")
        .keys()
        .cloned()
        .collect();
    keys.sort();
    keys
}

/// Assert the listing response answers exactly the ids of `want`, in
/// order — newest first wherever the listing is the one answering.
pub(super) fn assert_listed(response: &common::TestResponse, want: &[&Notification], who: &str) {
    let ids: Vec<String> = want.iter().map(|note| note.id.to_string()).collect();
    assert_eq!(listed_ids(response), ids, "{who}");
}

/// The `Link` header Pleroma's `add_link_headers` writes for a page whose
/// newest row is `newest` and oldest `oldest`.
pub(super) fn expected_link(newest: Uuid, oldest: Uuid) -> String {
    format!(
        "<{BASE}{V1}?max_id={oldest}>; rel=\"next\", \
         <{BASE}{V1}?min_id={newest}>; rel=\"prev\""
    )
}

/// Alice's five plain follows of `bob`, oldest first — the cursor walk's
/// ladder.
pub(super) async fn ladder(pool: &PgPool, alice: &User, bob: &User) -> Vec<Notification> {
    let mut notes = Vec::new();
    for _ in 0..5 {
        notes.push(common::create_notification(pool, alice, bob, "follow", None).await);
    }
    notes
}

/// The shared listing seed: alice's unseen three — a favourite of
/// `erin`'s post by bob, a mention by carol and a follow by erin, oldest
/// first — plus a foreign row nobody may see through.
pub(super) struct ListingSeed {
    /// Alice's `read:notifications` token.
    pub(super) token: String,
    /// Erin's post, the status the favourites and mentions name.
    pub(super) post_id: Uuid,
    /// Erin's id.
    pub(super) erin_id: Uuid,
    /// Bob's id.
    pub(super) bob_id: Uuid,
    /// Carol's id.
    pub(super) carol_id: Uuid,
    /// Bob's favourite (oldest).
    pub(super) favourite: Notification,
    /// Carol's mention.
    pub(super) mention: Notification,
    /// Erin's follow (newest).
    pub(super) follow: Notification,
}

pub(super) async fn seed_listing(pool: &PgPool) -> ListingSeed {
    let (alice, token) = fixture(pool, "alice", READ).await;
    let erin = common::create_local_user(pool, "erin").await;
    let bob = common::create_local_user(pool, "bob").await;
    let carol = common::create_local_user(pool, "carol").await;
    let dave = common::create_local_user(pool, "dave").await;
    let mallory = common::create_local_user(pool, "mallory").await;
    let post = common::create_status(pool, &erin, "hello", Visibility::Public).await;
    let favourite = common::create_notification(pool, &alice, &bob, "favourite", Some(&post)).await;
    let mention = common::create_notification(pool, &alice, &carol, "mention", Some(&post)).await;
    let follow = common::create_notification(pool, &alice, &erin, "follow", None).await;
    let _foreign = common::create_notification(pool, &dave, &mallory, "follow", None).await;
    ListingSeed {
        token,
        post_id: post.id,
        erin_id: erin.id,
        bob_id: bob.id,
        carol_id: carol.id,
        favourite,
        mention,
        follow,
    }
}

/// The `with_muted` seed: alice's rows by a notification-muted bob, a
/// blocked carol, a domain-blocked remote mallory, a timeline-only-muted
/// dave, and erin's two — a favourite in a thread alice muted and a plain
/// follow — oldest first.
pub(super) struct MuteSeed {
    /// Alice's `read:notifications` token.
    pub(super) token: String,
    /// Bob's row, hidden behind a mute that hides notifications.
    pub(super) from_muted: Notification,
    /// Dave's row, under a mute that hides no notifications.
    pub(super) from_timeline: Notification,
    /// Erin's favourite of the muted thread.
    pub(super) on_muted_thread: Notification,
    /// Erin's plain follow.
    pub(super) plain: Notification,
}

pub(super) async fn seed_mutes(pool: &PgPool) -> MuteSeed {
    let (alice, token) = fixture(pool, "alice", READ).await;
    let bob = common::create_local_user(pool, "bob").await;
    let carol = common::create_local_user(pool, "carol").await;
    let dave = common::create_local_user(pool, "dave").await;
    let mallory = common::create_remote_user(pool, "mallory", "evil.example").await;
    let erin = common::create_local_user(pool, "erin").await;
    let post = common::create_status(pool, &erin, "a thread", Visibility::Public).await;
    let from_muted = common::create_notification(pool, &alice, &bob, "follow", None).await;
    let _from_blocked = common::create_notification(pool, &alice, &carol, "follow", None).await;
    let _from_domain = common::create_notification(pool, &alice, &mallory, "follow", None).await;
    let from_timeline = common::create_notification(pool, &alice, &dave, "mention", None).await;
    let on_muted_thread =
        common::create_notification(pool, &alice, &erin, "favourite", Some(&post)).await;
    let plain = common::create_notification(pool, &alice, &erin, "follow", None).await;
    let _muted = mute::mute(pool, &alice, &bob, MuteOptions::default())
        .await
        .expect("the mute fixture lands");
    let _timeline_only = mute::mute(
        pool,
        &alice,
        &dave,
        MuteOptions {
            notifications: false,
            expires_in: None,
        },
    )
    .await
    .expect("the timeline-only mute lands");
    let _blocked = block::block(pool, BASE, &alice, &carol)
        .await
        .expect("the block fixture lands");
    domain_block::block_domain(pool, alice.id, "evil.example")
        .await
        .expect("the domain block lands");
    conversation_mute::mute(pool, alice.id, post.conversation_id)
        .await
        .expect("the thread mute lands");
    MuteSeed {
        token,
        from_muted,
        from_timeline,
        on_muted_thread,
        plain,
    }
}
