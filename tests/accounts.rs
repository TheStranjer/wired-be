//! Integration tests for `GET /api/v1/accounts/{id}` and
//! `GET /api/v1/accounts/lookup`: the core `Account` entity with its
//! `pleroma` extension block for local and remote rows without credentials,
//! the privacy rules the request's token decides (hidden follow counts and
//! hidden role flags, and the owner-only keys of the block), the status
//! counters a posted status moves, an invalid token still a 401, every miss
//! (garbage id, unknown id, deactivated account, unresolvable acct) the same
//! 404 "Record not found", `acct` resolved case-insensitively with this
//! instance's own domain stripped, and an absent `acct` a 400 "Missing
//! parameter: acct".

#![cfg(test)]

// `pub` because Clippy's nursery `redundant_pub_crate` forbids `pub(crate)`
// items inside a private module, while `unreachable_pub` forbids `pub` ones:
// only a `pub` module makes the harness helpers lint-clean.
pub mod common;

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::models::{
    status::Visibility,
    user::{Flag, User},
};

/// The instance origin the fixtures live on (`Config::for_tests`).
const BASE: &str = "https://test.example";
/// The body every account miss on these routes carries (decision 8).
const NOT_FOUND: &str = r#"{"error":"Record not found"}"#;
/// The 401 body `MaybeAuthUser` renders for a token that does not resolve.
const INVALID_TOKEN: &str = r#"{"error":"The access token is invalid"}"#;
/// The 400 body when the `acct` parameter is absent or blank.
const MISSING_ACCT: &str = r#"{"error":"Missing parameter: acct"}"#;

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

/// A local account renders its full entity to an anonymous request: auth
/// is optional, the JSON is the core `Account` shape (with its `pleroma`
/// block, which the tests below cover in detail).
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn show_renders_a_local_account_without_auth(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn show_counts_the_statuses_an_account_has_posted(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn show_accepts_a_valid_bearer_token(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn show_renders_the_public_pleroma_block_without_auth(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn show_hides_counts_and_roles_from_strangers(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn show_reveals_the_private_pleroma_state_to_the_owner(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn lookup_renders_for_its_viewer(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn show_rejects_an_invalid_bearer_token(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn show_renders_a_remote_account(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn show_an_unknown_id_is_a_record_not_found(pool: PgPool) {
    let app = common::test_app(pool);
    let response = show(&app, &Uuid::now_v7().to_string()).await;
    assert_eq!(response.status, StatusCode::NOT_FOUND, "no row, no account");
    assert_eq!(response.text(), NOT_FOUND, "decision 8's 404 body");
}

/// An id that is not uuid text answers the same 404 as an unknown one,
/// never a 400, so the route cannot distinguish noise from misses.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn show_a_garbage_id_is_a_record_not_found(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn show_a_deactivated_account_is_a_record_not_found(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn lookup_by_plain_nickname(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn lookup_strips_our_own_domain(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn lookup_is_case_insensitive(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn lookup_finds_a_stored_remote_user(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn lookup_without_a_usable_acct_is_a_bad_request(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn lookup_an_unresolvable_acct_is_a_record_not_found(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn lookup_a_deactivated_account_is_a_record_not_found(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn lookup_rejects_an_invalid_bearer_token(pool: PgPool) {
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
