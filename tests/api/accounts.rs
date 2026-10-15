//! Integration tests for `GET /api/v1/accounts/verify_credentials`,
//! `GET /api/v1/accounts/{id}` and
//! `GET /api/v1/accounts/lookup`: the core `Account` entity with its
//! `pleroma` extension block for local and remote rows without credentials,
//! the `source` block and owner view only `verify_credentials` renders
//! (raw note and fields, posting defaults, the pending follow-request
//! count, and the `read:accounts` scope, user-token auth and 401/403
//! rejections in front of them),
//! the privacy rules the request's token decides (hidden follow counts and
//! hidden role flags, and the owner-only keys of the block), the status
//! counters a posted status moves, an invalid token still a 401, every miss
//! (garbage id, unknown id, deactivated account, unresolvable acct) the same
//! 404 "Record not found", `acct` resolved case-insensitively with this
//! instance's own domain stripped, and an absent `acct` a 400 "Missing
//! parameter: acct".

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::TEST_BASE_URL,
    models::{
        follow::{self, FollowState},
        status::Visibility,
        user::{Flag, User},
    },
};

use crate::common;

/// The instance origin the fixtures live on (`Config::for_tests`).
const BASE: &str = "https://test.example";
/// The body every account miss on these routes carries (decision 8).
const NOT_FOUND: &str = r#"{"error":"Record not found"}"#;
/// The 401 body `MaybeAuthUser` renders for a token that does not resolve.
const INVALID_TOKEN: &str = r#"{"error":"The access token is invalid"}"#;
/// The 400 body when the `acct` parameter is absent or blank.
const MISSING_ACCT: &str = r#"{"error":"Missing parameter: acct"}"#;
/// The 401 body `AuthUser` renders when no token was presented at all.
const NO_TOKEN: &str = r#"{"error":"Invalid credentials"}"#;
/// The 403 body `AuthUser` renders for an app-only token.
const USER_REQUIRED: &str = r#"{"error":"This API requires an authenticated user"}"#;
/// The 403 body the scope check renders for a token that lacks the scope.
const NO_SCOPE: &str = r#"{"error":"Insufficient permissions: read:accounts."}"#;

/// `GET /api/v1/accounts/verify_credentials` with `token` as bearer.
async fn verify_credentials(app: &Router, token: &str) -> common::TestResponse {
    common::TestRequest::get("/api/v1/accounts/verify_credentials")
        .bearer(token)
        .send(app)
        .await
}

/// The `source` object of a rendered account, present or panicked over:
/// the tests that ask for it are the ones asserting its shape.
fn source_of(body: &Value) -> Value {
    body.get("source")
        .cloned()
        .expect("verify_credentials renders the source block")
}

/// `GET /api/v1/accounts/{id}` for `id`, without credentials.
async fn show(app: &Router, id: &str) -> common::TestResponse {
    common::TestRequest::get(&format!("/api/v1/accounts/{id}"))
        .send(app)
        .await
}

/// `GET /api/v1/accounts/lookup` with `query` appended verbatim: either
/// empty or starting at the `?`.
async fn lookup(app: &Router, query: &str) -> common::TestResponse {
    common::TestRequest::get(&format!("/api/v1/accounts/lookup{query}"))
        .send(app)
        .await
}

/// The identity fields of a rendered [`Account`]: which row came back.
/// `full` is the handle the entity must show, `username` its nickname
/// part and `url` the human-facing profile link.
fn assert_identity(body: &Value, account: &User, full: &str, username: &str, url: &str) {
    assert_eq!(body["id"], account.id.to_string(), "id is the uuid text");
    assert_eq!(body["acct"], full, "acct is the stored nickname");
    assert_eq!(body["username"], username, "username drops the domain");
    assert_eq!(body["url"], url, "url is the profile page");
    assert_eq!(body["uri"], account.ap_id, "uri is the actor id");
}

/// The account whose own view is the fullest: a raw note and its rendered
/// form, posting defaults, hidden counters, a role it keeps to itself, a
/// settings blob and one alias.
async fn owner_view(pool: &PgPool) -> User {
    common::create_local_user_with(pool, "alice", |mut new| {
        new.raw_bio = "hi **there**".to_owned();
        new.bio = "<p>hi <strong>there</strong></p>".to_owned();
        new.default_scope = "unlisted".to_owned();
        new.sensitive_default = Flag::new(true);
        new.language = Some("eo".to_owned());
        new.no_rich_text = Flag::new(true);
        new.discoverable = Flag::new(true);
        new.followers_count = 5;
        new.hide_followers_count = Flag::new(true);
        new.is_admin = Flag::new(true);
        new.show_role = Flag::new(false);
        new.settings_store = json!({"pleroma_fe": {"theme": "dark"}});
        new.also_known_as = vec!["https://old.example/users/alice".to_owned()];
        new
    })
    .await
}

/// `verify_credentials` answers the caller's own account as they see it,
/// with the `source` block built from the stored settings: the raw note and
/// posting defaults, the owner-only `pleroma` keys, the top-level
/// `also_known_as` list, and the hidden counters visible.
#[tokio::test]
async fn verify_credentials_renders_the_owner_view_with_its_source() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = owner_view(&pool).await;
    let token = common::create_app_and_token(&pool, &alice, "read:accounts").await;
    let app = common::test_app(pool);
    let response = verify_credentials(&app, &token).await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "the account's own token answers"
    );
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
        body["note"], "<p>hi <strong>there</strong></p>",
        "the public note keeps rendering the sanitized HTML"
    );
    assert_eq!(
        body["followers_count"], 5,
        "a count hidden from strangers is the owner's own"
    );
    assert_eq!(
        body["pleroma"]["is_admin"], true,
        "show_role: false hides the role from others, never from the owner"
    );
    assert_eq!(
        body["pleroma"]["settings_store"],
        json!({"pleroma_fe": {"theme": "dark"}}),
        "the owner-only keys of the pleroma block are in the answer"
    );
    assert_eq!(
        body["pleroma"]["relationship"],
        json!({}),
        "an account has no relationship to itself to embed"
    );
    assert_eq!(
        body["also_known_as"],
        json!(["https://old.example/users/alice"]),
        "the own view carries the alias list as a top-level key"
    );
    assert_eq!(
        source_of(&body),
        json!({
            "privacy": "unlisted",
            "sensitive": true,
            "language": "eo",
            "note": "hi **there**",
            "fields": [],
            "follow_requests_count": 0,
            "pleroma": {
                "show_role": false,
                "no_rich_text": true,
                "discoverable": true,
                "actor_type": "Person",
            },
        }),
        "the source block is the account's own editing state: the raw note, \
         the stored posting defaults and the four Pleroma settings"
    );
}

/// The columns an account never edited answer the block's documented
/// defaults: public, not sensitive, no language, no note, no fields and an
/// empty request queue.
#[tokio::test]
async fn verify_credentials_source_defaults_are_the_column_defaults() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let token = common::create_app_and_token(&pool, &alice, "read:accounts").await;
    let app = common::test_app(pool);
    let body = verify_credentials(&app, &token).await.json_value();
    assert_eq!(
        source_of(&body),
        json!({
            "privacy": "public",
            "sensitive": false,
            "language": null,
            "note": "",
            "fields": [],
            "follow_requests_count": 0,
            "pleroma": {
                "show_role": true,
                "no_rich_text": false,
                "discoverable": false,
                "actor_type": "Person",
            },
        }),
        "a freshly registered account has edited none of it"
    );
}

/// The `source` fields are the text an owner edits, not the markup a
/// stranger reads: the entity renders the `fields` jsonb as stored and the
/// source block round-trips the `raw_fields` jsonb beside it.
#[tokio::test]
async fn verify_credentials_source_fields_are_the_raw_text() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user_with(&pool, "alice", |mut new| {
        new.fields = json!([
            {"name": "site", "value": "<p><a href=\"https://alice.example\" rel=\"ugc nofollow noopener noreferrer\">alice.example</a></p>"},
            {"name": "motto", "value": "<p><b>beep</b></p>"},
        ]);
        new.raw_fields = json!([
            {"name": "site", "value": "https://alice.example"},
            {"name": "motto", "value": "<b>beep</b>"},
        ]);
        new
    })
    .await;
    let token = common::create_app_and_token(&pool, &alice, "read:accounts").await;
    let app = common::test_app(pool);
    let body = verify_credentials(&app, &token).await.json_value();
    assert_eq!(
        body["fields"],
        json!([
            {"name": "site", "value": "<p><a href=\"https://alice.example\" rel=\"ugc nofollow noopener noreferrer\">alice.example</a></p>", "verified_at": null},
            {"name": "motto", "value": "<p><b>beep</b></p>", "verified_at": null},
        ]),
        "the public entity keeps rendering the stored markup"
    );
    assert_eq!(
        source_of(&body)["fields"],
        json!([
            {"name": "site", "value": "https://alice.example", "verified_at": null},
            {"name": "motto", "value": "<b>beep</b>", "verified_at": null},
        ]),
        "the source block hands back the raw text stored beside the markup"
    );
}

/// `follow_requests_count` is the queue waiting for this account: incoming
/// pending requests only — not an answered one, not a request this account
/// sent somewhere else — and it shrinks as the account answers it.
#[tokio::test]
async fn verify_credentials_counts_the_pending_follow_requests() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let carol = common::create_local_user_with(&pool, "carol", |mut new| {
        new.locked = Flag::new(true);
        new
    })
    .await;
    let erin = common::create_local_user_with(&pool, "erin", |mut new| {
        new.locked = Flag::new(true);
        new
    })
    .await;
    let one = common::create_local_user(&pool, "one").await;
    let two = common::create_local_user(&pool, "two").await;
    let dave = common::create_local_user(&pool, "dave").await;

    let mut waiting = Vec::new();
    for follower in [&one, &two] {
        let queued = follow::follow(&pool, TEST_BASE_URL, follower, &carol)
            .await
            .expect("the request is recorded");
        assert_eq!(
            queued.state,
            FollowState::Pending,
            "a locked account answers its own requests"
        );
        waiting.push(queued.id);
    }
    let granted = follow::follow(&pool, TEST_BASE_URL, &dave, &carol)
        .await
        .expect("the third request is recorded");
    let answered = follow::accept(&pool, granted.id)
        .await
        .expect("one request is answered");
    assert_eq!(
        answered.state,
        FollowState::Accepted,
        "the answered one is out of the queue"
    );
    let outgoing = follow::follow(&pool, TEST_BASE_URL, &carol, &erin)
        .await
        .expect("carol asks to follow erin too");
    assert_eq!(
        outgoing.state,
        FollowState::Pending,
        "and hers waits in erin's queue"
    );

    let token = common::create_app_and_token(&pool, &carol, "read:accounts").await;
    let carol_pool = pool.clone();
    let app = common::test_app(pool);
    let body = verify_credentials(&app, &token).await.json_value();
    assert_eq!(
        source_of(&body)["follow_requests_count"],
        2,
        "the two requests still waiting for carol, neither the answered one \
         nor her own outgoing request"
    );

    let carol_answered = follow::accept(&carol_pool, waiting[0])
        .await
        .expect("carol answers one");
    assert_eq!(
        carol_answered.state,
        FollowState::Accepted,
        "and the answered request leaves the queue the count reads"
    );
    let after = verify_credentials(&app, &token).await.json_value();
    assert_eq!(
        source_of(&after)["follow_requests_count"],
        1,
        "answering a request takes it out of the count"
    );
}

/// A token that does not grant `read:accounts` is a 403 naming the scope,
/// even when it is a valid user token.
#[tokio::test]
async fn verify_credentials_without_the_scope_is_forbidden() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let token = common::create_app_and_token(&pool, &alice, "write:accounts").await;
    let app = common::test_app(pool);
    let response = verify_credentials(&app, &token).await;
    assert_eq!(
        response.status,
        StatusCode::FORBIDDEN,
        "a valid token outside the scope is refused"
    );
    assert_eq!(response.text(), NO_SCOPE, "the denial names the scope");
}

/// The broad `read` scope covers `read:accounts` (decision 9), so a client
/// that never asked for the sub-scope still gets its own account.
#[tokio::test]
async fn verify_credentials_accepts_a_broad_read_token() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let token = common::create_app_and_token(&pool, &alice, "read").await;
    let app = common::test_app(pool);
    let response = verify_credentials(&app, &token).await;
    assert_eq!(response.status, StatusCode::OK, "`read` grants the check");
    let body = response.json_value();
    assert_eq!(
        body["id"],
        alice.id.to_string(),
        "and answers the token's own account"
    );
    assert!(
        source_of(&body).get("privacy").is_some(),
        "with the source block attached"
    );
}

/// An app-only (`client_credentials`) token has no account to describe: 403
/// from the extractor, before the scope is even read.
#[tokio::test]
async fn verify_credentials_requires_a_user_token() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let token = common::create_app_only_token(&pool, "read:accounts").await;
    let app = common::test_app(pool);
    let response = verify_credentials(&app, &token).await;
    assert_eq!(
        response.status,
        StatusCode::FORBIDDEN,
        "an app-only token is not a user"
    );
    assert_eq!(response.text(), USER_REQUIRED, "AuthUser's wording");
}

/// No token at all is the 401 [`AuthUser`] owes, and a token that does not
/// resolve is the invalid-credential 401: the endpoint never answers
/// anonymously.
#[tokio::test]
async fn verify_credentials_without_valid_credentials_is_unauthorized() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let _alice = common::create_local_user(&pool, "alice").await;
    let app = common::test_app(pool);
    let anonymous = common::TestRequest::get("/api/v1/accounts/verify_credentials")
        .send(&app)
        .await;
    assert_eq!(
        anonymous.status,
        StatusCode::UNAUTHORIZED,
        "the endpoint requires auth"
    );
    assert_eq!(anonymous.text(), NO_TOKEN, "AuthUser's no-token body");

    let lapsed = common::TestRequest::get("/api/v1/accounts/verify_credentials")
        .bearer("bogus-token")
        .send(&app)
        .await;
    assert_eq!(
        lapsed.status,
        StatusCode::UNAUTHORIZED,
        "a token that resolves to nothing is refused too"
    );
    assert_eq!(lapsed.text(), INVALID_TOKEN, "and says which");
}

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
        json!({}),
        "the notification settings are a placeholder for now"
    );
}

/// `lookup` renders for its viewer exactly as `show` does: the same token
/// reveals the private state, another account's token does not.
#[tokio::test]
async fn lookup_renders_for_its_viewer() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user_with(&pool, "alice", |mut new| {
        new.followers_count = 5;
        new.hide_followers_count = Flag::new(true);
        new.settings_store = json!({"note": "mine"});
        new
    })
    .await;
    let own_token = common::create_app_and_token(&pool, &alice, "read:accounts").await;
    let mallory = common::create_local_user(&pool, "mallory").await;
    let stranger_token = common::create_app_and_token(&pool, &mallory, "read:accounts").await;
    let app = common::test_app(pool);

    let own = common::TestRequest::get("/api/v1/accounts/lookup?acct=alice")
        .bearer(&own_token)
        .send(&app)
        .await;
    assert_eq!(own.status, StatusCode::OK, "the owner may look up herself");
    let own_body = own.json_value();
    assert_eq!(
        own_body["followers_count"], 5,
        "lookup keeps the owner's counters visible"
    );
    assert_eq!(
        own_body["pleroma"]["settings_store"],
        json!({"note": "mine"}),
        "the owner sees the stored settings blob"
    );

    let stranger = common::TestRequest::get("/api/v1/accounts/lookup?acct=alice")
        .bearer(&stranger_token)
        .send(&app)
        .await;
    assert_eq!(
        stranger.status,
        StatusCode::OK,
        "so may another account look her up"
    );
    let stranger_body = stranger.json_value();
    assert_eq!(
        stranger_body["followers_count"], 0,
        "and the hidden count stays hidden"
    );
    assert!(
        stranger_body["pleroma"].get("settings_store").is_none(),
        "the settings blob is not public"
    );
    assert!(
        stranger_body["pleroma"].get("birthday").is_none(),
        "and neither is a birthday the account does not show"
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

/// `acct` without a domain finds the local account by stored nickname.
#[tokio::test]
async fn lookup_by_plain_nickname() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let app = common::test_app(pool);
    let response = lookup(&app, "?acct=alice").await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "the stored nickname resolves"
    );
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
}

/// A local handle qualified with this instance's own domain resolves to
/// the bare stored nickname, the domain compared case-insensitively.
#[tokio::test]
async fn lookup_strips_our_own_domain() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let app = common::test_app(pool);
    for acct in ["alice@test.example", "alice@TEST.example"] {
        let response = lookup(&app, &format!("?acct={acct}")).await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "`{acct}` names the local alice"
        );
        assert_eq!(
            response.json_value()["id"],
            alice.id.to_string(),
            "`{acct}` resolves to alice's row"
        );
    }
}

/// The nickname column is citext: any casing of the handle finds the row,
/// with or without the local domain.
#[tokio::test]
async fn lookup_is_case_insensitive() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "Alice").await;
    let app = common::test_app(pool);
    for acct in ["ALICE", "alice", "AlIce@TEST.example"] {
        let response = lookup(&app, &format!("?acct={acct}")).await;
        assert_eq!(response.status, StatusCode::OK, "`{acct}` folds case");
        assert_eq!(
            response.json_value()["id"],
            alice.id.to_string(),
            "`{acct}` finds the row stored as Alice"
        );
    }
}

/// An already-mirrored remote user resolves by its stored `nick@host`
/// handle (federation to fetch unseen ones is a later ticket).
#[tokio::test]
async fn lookup_finds_a_stored_remote_user() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let app = common::test_app(pool);
    let response = lookup(&app, "?acct=bob@remote.example").await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "a mirrored remote handle resolves"
    );
    let body = response.json_value();
    assert_identity(
        &body,
        &bob,
        "bob@remote.example",
        "bob",
        "https://remote.example/users/bob",
    );
}

/// An absent, empty or blank `acct` is the 400 naming the parameter.
#[tokio::test]
async fn lookup_without_a_usable_acct_is_a_bad_request() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool);
    for query in ["", "?", "?acct=", "?acct=%20%20"] {
        let response = lookup(&app, query).await;
        assert_eq!(
            response.status,
            StatusCode::BAD_REQUEST,
            "`{query:?}` carries no acct"
        );
        assert_eq!(
            response.text(),
            MISSING_ACCT,
            "the body names the missing parameter, `{query:?}`"
        );
    }
}

/// A handle that names no stored local or mirrored account is the plain
/// record-not-found 404.
#[tokio::test]
async fn lookup_an_unresolvable_acct_is_a_record_not_found() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let _alice: User = common::create_local_user(&pool, "alice").await;
    let app = common::test_app(pool);
    for acct in [
        "nobody",
        // Stripped to a bare `nobody`, still absent.
        "nobody@test.example",
        // A foreign domain nobody is mirrored from.
        "alice@other.example",
        // Strips to an empty nickname.
        "@test.example",
    ] {
        let response = lookup(&app, &format!("?acct={acct}")).await;
        assert_eq!(
            response.status,
            StatusCode::NOT_FOUND,
            "`{acct}` names no account"
        );
        assert_eq!(
            response.text(),
            NOT_FOUND,
            "an unknown acct is the plain 404 body, `{acct}`"
        );
    }
}

/// Lookup hides deactivated accounts too (Pleroma's `visible_for` check):
/// the row exists, but no acct finds it.
#[tokio::test]
async fn lookup_a_deactivated_account_is_a_record_not_found() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let ghost = common::create_local_user_with(&pool, "ghost", |mut new| {
        new.is_active = Flag::new(false);
        new
    })
    .await;
    let app = common::test_app(pool);
    for query in ["?acct=ghost", "?acct=ghost@test.example"] {
        let response = lookup(&app, query).await;
        assert_eq!(
            response.status,
            StatusCode::NOT_FOUND,
            "the deactivated {} is not served via {query}",
            ghost.nickname
        );
        assert_eq!(response.text(), NOT_FOUND, "like any other miss, {query}");
    }
}

/// The same optional-auth rule guards lookup: an invalid token is a 401.
#[tokio::test]
async fn lookup_rejects_an_invalid_bearer_token() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool);
    let response = common::TestRequest::get("/api/v1/accounts/lookup?acct=alice")
        .bearer("bogus-token")
        .send(&app)
        .await;
    assert_eq!(
        response.status,
        StatusCode::UNAUTHORIZED,
        "the same extractor guards lookup"
    );
    assert_eq!(response.text(), INVALID_TOKEN, "MaybeAuthUser's 401 body");
}
