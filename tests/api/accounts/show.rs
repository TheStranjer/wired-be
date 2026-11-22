//! `GET /api/v1/accounts/{id}`: the public and owner renderings, privacy
//! rules and the misses.

use axum::http::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;
use wired_be::models::{status::Visibility, user::Flag};

use super::support::{BASE, INVALID_TOKEN, NOT_FOUND, assert_identity, show};
use crate::common;

/// `GET /api/v1/accounts/{id}` for one's own id is still the plain account:
/// the `source` block belongs to the credentials endpoints and no other
/// route renders it.
#[tokio::test]
async fn show_never_renders_the_source_block() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let token = common::create_app_and_token(&pool, &alice, "read:accounts").await;
    let app = common::test_app(pool);
    let body = common::TestRequest::get(&format!("/api/v1/accounts/{}", alice.id))
        .bearer(&token)
        .send(&app)
        .await
        .json_value();
    assert_eq!(body["id"], alice.id.to_string(), "the same account");
    assert!(
        body.get("source").is_none(),
        "the owner view of `show` keeps the editing block out"
    );
}

/// A local account renders its full entity to an anonymous request: auth
/// is optional, the JSON is the core `Account` shape (with its `pleroma`
/// block, which the tests below cover in detail).
#[tokio::test]
async fn show_renders_a_local_account_without_auth() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let app = common::test_app(pool);
    let response = show(&app, &alice.id.to_string()).await;
    assert_eq!(response.status, StatusCode::OK, "auth is optional");
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "the entity is JSON"
    );
    let body = response.json_value();
    assert_identity(
        &body,
        &alice,
        "alice",
        "alice",
        &format!("{BASE}/users/alice"),
    );
    assert_eq!(
        body["fqn"], "alice@test.example",
        "a local fqn gains the config domain"
    );
    assert_eq!(
        body["suspended"], false,
        "an active account is not suspended"
    );
    assert_eq!(body["followers_count"], 0, "counts render from the row");
    assert_eq!(
        body["emojis"],
        json!([]),
        "emojis is the empty list for now"
    );
}

/// A posted status shows up in the account's stored counters: the entity
/// reads `statuses_count` and `last_status_at` off the `users` row, and both
/// are what [`wired_be::models::status`] moved when
/// [`common::create_status`](common::create_status) wrote the status.
#[tokio::test]
async fn show_counts_the_statuses_an_account_has_posted() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let status = common::create_status(&pool, &alice, "hello fediverse", Visibility::Public).await;
    let app = common::test_app(pool);
    let body = show(&app, &alice.id.to_string()).await.json_value();

    assert_eq!(
        status.visibility,
        Visibility::Public,
        "the fixture keeps the visibility it was given"
    );
    assert_eq!(
        status.url.as_deref(),
        Some(format!("{BASE}/notice/{}", status.id).as_str()),
        "and the local post's URL derives from its id"
    );
    assert_eq!(body["statuses_count"], 1, "one posted status counts");
    let posted = status.inserted_at.format("%Y-%m-%d").to_string();
    assert_eq!(
        body["last_status_at"],
        posted.as_str(),
        "last_status_at is the day of that status"
    );
}

/// A valid bearer token may watch: same account, same 200. What the token
/// decides is covered by `show_reveals_the_private_pleroma_state_to_the_owner`.
#[tokio::test]
async fn show_accepts_a_valid_bearer_token() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let token = common::create_app_and_token(&pool, &alice, "read:accounts").await;
    let app = common::test_app(pool);
    let response = common::TestRequest::get(&format!("/api/v1/accounts/{}", alice.id))
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(response.status, StatusCode::OK, "a valid token is accepted");
    assert_eq!(
        response.json_value()["id"],
        alice.id.to_string(),
        "the same account as anonymously"
    );
}

/// The `pleroma` block an anonymous request gets: the public keys with the
/// column defaults as stored, and none of the seven the account keeps for
/// itself.
#[tokio::test]
async fn show_renders_the_public_pleroma_block_without_auth() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let app = common::test_app(pool);
    let body = show(&app, &alice.id.to_string()).await.json_value();
    let pleroma = body
        .get("pleroma")
        .expect("every account renders a pleroma block");
    assert_eq!(pleroma["ap_id"], alice.ap_id, "ap_id repeats the actor id");
    assert_eq!(
        pleroma["deactivated"], false,
        "an active account is not deactivated"
    );
    assert_eq!(
        pleroma["confirmation_pending"], false,
        "a stored account is confirmed by default"
    );
    assert_eq!(
        pleroma["hide_favorites"], true,
        "favourites are private by default"
    );
    assert_eq!(
        pleroma["accepts_chat_messages"], true,
        "a local account always answers for its own capability"
    );
    assert_eq!(pleroma["tags"], json!([]), "no tags is the empty array");
    assert_eq!(
        pleroma["background_image"],
        Value::Null,
        "an unset background is null, not the banner default"
    );
    assert_eq!(
        pleroma["relationship"],
        json!({}),
        "the relationship placeholder is the empty object"
    );
    for hidden in [
        "unread_conversation_count",
        "unread_notifications_count",
        "notification_settings",
        "settings_store",
        "chat_token",
        "birthday",
        "show_birthday",
    ] {
        assert!(
            pleroma.get(hidden).is_none(),
            "`{hidden}` is only ever rendered for the account itself"
        );
    }
}

/// An account that hides its follow counts and its role: a stranger —
/// anonymous or authenticated — sees zeroes and no roles.
#[tokio::test]
async fn show_hides_counts_and_roles_from_strangers() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user_with(&pool, "alice", |mut new| {
        new.followers_count = 5;
        new.following_count = 6;
        new.hide_followers_count = Flag::new(true);
        new.hide_follows_count = Flag::new(true);
        new.is_admin = Flag::new(true);
        new.show_role = Flag::new(false);
        new
    })
    .await;
    let mallory = common::create_remote_user(&pool, "mallory", "remote.example").await;
    let stranger_token = common::create_app_and_token(&pool, &mallory, "read:accounts").await;
    let app = common::test_app(pool);

    let stranger = common::TestRequest::get(&format!("/api/v1/accounts/{}", alice.id))
        .bearer(&stranger_token)
        .send(&app)
        .await;
    assert_eq!(
        stranger.status,
        StatusCode::OK,
        "another account's token still gets the account"
    );
    for body in [
        show(&app, &alice.id.to_string()).await.json_value(),
        stranger.json_value(),
    ] {
        assert_eq!(
            body["followers_count"], 0,
            "a hidden follower count is zero for a stranger"
        );
        assert_eq!(
            body["following_count"], 0,
            "a hidden follow count is zero for a stranger"
        );
        assert_eq!(
            body["pleroma"]["is_admin"], false,
            "show_role: false hides the admin flag from a stranger"
        );
        assert_eq!(
            body["pleroma"]["is_moderator"], false,
            "and the moderator flag"
        );
    }
}

/// The same account, seen by its own user: the stored counters, the real
/// roles, and the owner-only keys of the block.
#[tokio::test]
async fn show_reveals_the_private_pleroma_state_to_the_owner() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user_with(&pool, "alice", |mut new| {
        new.followers_count = 5;
        new.following_count = 6;
        new.hide_followers_count = Flag::new(true);
        new.hide_follows_count = Flag::new(true);
        new.is_admin = Flag::new(true);
        new.show_role = Flag::new(false);
        new.settings_store = json!({"pleroma_fe": {"theme": "dark"}});
        new
    })
    .await;
    let token = common::create_app_and_token(&pool, &alice, "read:accounts").await;
    let app = common::test_app(pool);
    let body = common::TestRequest::get(&format!("/api/v1/accounts/{}", alice.id))
        .bearer(&token)
        .send(&app)
        .await
        .json_value();
    assert_eq!(
        body["followers_count"], 5,
        "the owner sees the counter the flag hides from others"
    );
    assert_eq!(
        body["following_count"], 6,
        "the owner sees the counter the flag hides from others"
    );
    assert_eq!(
        body["pleroma"]["is_admin"], true,
        "the owner always sees their own roles"
    );
    assert_eq!(
        body["pleroma"]["settings_store"],
        json!({"pleroma_fe": {"theme": "dark"}}),
        "the stored settings blob comes back to its owner"
    );
    assert_eq!(
        body["pleroma"]["show_birthday"], false,
        "the owner-only switch renders as stored"
    );
    assert_eq!(
        body["pleroma"]["birthday"],
        Value::Null,
        "an account without a birthday sees the key as null"
    );
    assert_eq!(
        body["pleroma"]["chat_token"],
        Value::Null,
        "chat is not implemented, so the owner sees null"
    );
    assert_eq!(
        body["pleroma"]["unread_conversation_count"], 0,
        "the unread counters are placeholders for now"
    );
    assert_eq!(
        body["pleroma"]["unread_notifications_count"], 0,
        "the unread counters are placeholders for now"
    );
    assert_eq!(
        body["pleroma"]["notification_settings"],
        json!({
            "block_from_strangers": false,
            "hide_notification_contents": false,
        }),
        "the owner sees the all-off settings the account starts with"
    );
}

/// Auth is optional, but a presented token that does not resolve is a 401,
/// never a silently ignored credential (Pleroma's OAuth plug).
#[tokio::test]
async fn show_rejects_an_invalid_bearer_token() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let app = common::test_app(pool);
    let response = common::TestRequest::get(&format!("/api/v1/accounts/{}", alice.id))
        .bearer("bogus-token")
        .send(&app)
        .await;
    assert_eq!(
        response.status,
        StatusCode::UNAUTHORIZED,
        "an invalid token is not treated as anonymous"
    );
    assert_eq!(response.text(), INVALID_TOKEN, "MaybeAuthUser's 401 body");
}

/// A stored remote mirror renders exactly like a local account: same
/// entity, the handle carrying its host.
#[tokio::test]
async fn show_renders_a_remote_account() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let app = common::test_app(pool);
    let response = show(&app, &bob.id.to_string()).await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "remote mirrors are shown too"
    );
    let body = response.json_value();
    assert_identity(
        &body,
        &bob,
        "bob@remote.example",
        "bob",
        "https://remote.example/users/bob",
    );
    assert_eq!(
        body["fqn"], "bob@remote.example",
        "a remote fqn is the stored handle itself"
    );
    assert_eq!(
        body["pleroma"]["ap_id"], bob.ap_id,
        "the pleroma block repeats the actor id"
    );
    assert_eq!(
        body["pleroma"]["accepts_chat_messages"],
        Value::Null,
        "a mirror federation never saw a chat capability for renders null"
    );
}

/// An id that parses but names no row is the plain record-not-found 404.
#[tokio::test]
async fn show_an_unknown_id_is_a_record_not_found() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool);
    let response = show(&app, &Uuid::now_v7().to_string()).await;
    assert_eq!(response.status, StatusCode::NOT_FOUND, "no row, no account");
    assert_eq!(response.text(), NOT_FOUND, "decision 8's 404 body");
}

/// An id that is not uuid text answers the same 404 as an unknown one,
/// never a 400, so the route cannot distinguish noise from misses.
#[tokio::test]
async fn show_a_garbage_id_is_a_record_not_found() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool);
    for id in [
        "not-an-id",
        "1",
        // The right shape, one non-hex digit.
        "6790f6b8-9c3e-5a3d-9b1f-6790f6b89c3g",
    ] {
        let response = show(&app, id).await;
        assert_eq!(
            response.status,
            StatusCode::NOT_FOUND,
            "`{id}` is not an account id, and must not say more"
        );
        assert_eq!(
            response.text(),
            NOT_FOUND,
            "a garbage id is the same 404 as an unknown one"
        );
    }
}

/// A deactivated account is hidden from show: 404, like any other miss.
#[tokio::test]
async fn show_a_deactivated_account_is_a_record_not_found() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let ghost = common::create_local_user_with(&pool, "ghost", |mut new| {
        new.is_active = Flag::new(false);
        new
    })
    .await;
    let app = common::test_app(pool);
    let response = show(&app, &ghost.id.to_string()).await;
    assert_eq!(
        response.status,
        StatusCode::NOT_FOUND,
        "deactivated accounts are not served"
    );
    assert_eq!(response.text(), NOT_FOUND, "like any other miss");
}
