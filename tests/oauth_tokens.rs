//! OAuth token management over HTTP (`/api/oauth_tokens`,
//! `/api/v1/pleroma/apps`, WBE-49): the caller's valid tokens with their
//! app names, revocation of one's own token, and the apps the caller
//! registered or holds tokens for.
//!
//! The tests pin what the ticket promises: the token list shows only the
//! caller's own unexpired tokens, newest first, with exactly the keys
//! `id`, `app_name` and `valid_until` (Mastodon time); a foreign,
//! unknown or malformed revocation id is one and the same 404 "Record
//! not found" while the owner's `DELETE` answers 200 `{}` and really
//! removes the row; `/api/v1/pleroma/apps` merges owned and tokened
//! apps, deduplicated and newest first, under the seven pinned keys;
//! and every route answers 401 to an unauthenticated request.

#![cfg(test)]

// `pub` because Clippy's nursery `redundant_pub_crate` forbids `pub(crate)`
// items inside a private module, while `unreachable_pub` forbids `pub` ones:
// only a `pub` module makes the harness helpers lint-clean.
pub mod common;

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;
use wired_be::models::{oauth_token::OauthToken, user::User};

/// The token list endpoint.
const TOKENS: &str = "/api/oauth_tokens";
/// The Pleroma apps endpoint.
const APPS: &str = "/api/v1/pleroma/apps";
/// Every JSON key a token entry must carry, sorted.
const TOKEN_KEYS: [&str; 3] = ["app_name", "id", "valid_until"];
/// Every JSON key an app entity must carry, sorted.
const APP_KEYS: [&str; 7] = [
    "client_id",
    "client_secret",
    "id",
    "name",
    "redirect_uri",
    "trusted",
    "website",
];
/// The redirect URI every [`common::create_app_named`] fixture stores.
const REDIRECT_URI: &str = "https://client.example/callback";

/// The 404 body of decision 8, as the value a rejection must equal.
fn not_found() -> Value {
    json!({"error": "Record not found"})
}

/// A local user with a live bearer token acting as them.
///
/// The session token deliberately grants the unrelated `push` scope:
/// these endpoints require a user, no particular scope. The session's
/// app is owned by the user and holds their token, so it also belongs
/// in every fixture math for `/api/v1/pleroma/apps`.
struct Session {
    /// The created user.
    user: User,
    /// The stored token row this session authenticates with.
    token: OauthToken,
}

impl Session {
    /// The access token string, ready for `TestRequest::bearer`.
    fn bearer(&self) -> &str {
        &self.token.token
    }
}

/// A local user named `nickname` plus their session token.
async fn session(pool: &PgPool, nickname: &str) -> Session {
    let user = common::create_local_user(pool, nickname).await;
    let app = common::create_app_named(pool, "Session", Some(&user)).await;
    let token = common::create_token(pool, &user, &app, "push").await;
    Session { user, token }
}

/// The sorted JSON keys of `entry`.
fn keys_of(entry: &Value) -> Vec<&str> {
    entry
        .as_object()
        .expect("a JSON object entry")
        .keys()
        .map(String::as_str)
        .collect()
}

/// The string ids of the entries of a JSON array body, in order.
fn ids_of(body: &Value) -> Vec<String> {
    body.as_array()
        .expect("a JSON array body")
        .iter()
        .map(|entry| {
            entry["id"]
                .as_str()
                .expect("entries carry a string id")
                .to_owned()
        })
        .collect()
}

/// The `name` of each entry of a JSON array body, in order.
fn names_of(body: &Value) -> Vec<&str> {
    body.as_array()
        .expect("a JSON array body")
        .iter()
        .map(|entry| entry["name"].as_str().expect("entries carry a name"))
        .collect()
}

/// `GET /api/oauth_tokens` — only the caller's own, unexpired tokens,
/// newest first, under exactly the three pinned keys.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn token_list_shows_the_callers_valid_tokens_newest_first(pool: PgPool) {
    let alice = session(&pool, "alice").await;
    let bob = session(&pool, "bob").await;
    let app_x = common::create_app_named(&pool, "App X", None).await;
    let app_y = common::create_app_named(&pool, "App Y", None).await;
    let older = common::create_token(&pool, &alice.user, &app_x, "read").await;
    let lapsed = common::create_token(&pool, &alice.user, &app_x, "read").await;
    common::expire_token(&pool, lapsed.id).await;
    let newest = common::create_token(&pool, &alice.user, &app_y, "read").await;
    let foreign = common::create_token(&pool, &bob.user, &app_x, "read").await;

    let response = common::TestRequest::get(TOKENS)
        .bearer(alice.bearer())
        .send(&common::test_app(pool))
        .await;
    assert_eq!(response.status, StatusCode::OK, "listing works");
    let body = response.json_value();
    assert_eq!(
        ids_of(&body),
        vec![
            newest.id.to_string(),
            older.id.to_string(),
            alice.token.id.to_string(),
        ],
        "own unexpired tokens newest first — the presenting one included, \
         the expired and bob's rows excluded"
    );
    for entry in body.as_array().expect("array body") {
        assert_eq!(keys_of(entry), TOKEN_KEYS, "exactly the three keys");
    }
    assert_eq!(body[0]["app_name"], "App Y", "the newest token's app");
    assert_eq!(
        body[0]["valid_until"],
        wired_be::util::time::mastodon_time(&newest.valid_until),
        "the expiry in the Mastodon wire format (decision 5)"
    );
    assert_eq!(body[1]["app_name"], "App X", "the older token's app");
    assert_eq!(body[2]["app_name"], "Session", "and the session's app");
    assert!(
        !ids_of(&body).contains(&foreign.id.to_string()),
        "bob's token never appears in alice's list"
    );
}

/// `DELETE /api/oauth_tokens/{id}` — the owner revokes their token:
/// 200 with `{}`, the row gone from database and listing alike, and a
/// second attempt on the dead id is the plain 404.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn revoke_removes_the_owners_token_and_answers_empty_object(pool: PgPool) {
    let alice = session(&pool, "alice").await;
    let app = common::create_app_named(&pool, "Revoke Me", Some(&alice.user)).await;
    let doomed = common::create_token(&pool, &alice.user, &app, "read").await;

    let response = common::TestRequest::delete(&format!("{TOKENS}/{}", doomed.id))
        .bearer(alice.bearer())
        .send(&common::test_app(pool.clone()))
        .await;
    assert_eq!(response.status, StatusCode::OK, "revocation works");
    assert_eq!(
        response.json_value(),
        json!({}),
        "the body is an empty object"
    );
    assert!(
        wired_be::models::oauth_token::get_with_app(&pool, &doomed.token)
            .await
            .expect("the lookup runs")
            .is_none(),
        "the token row is gone from the database"
    );

    let listing = common::TestRequest::get(TOKENS)
        .bearer(alice.bearer())
        .send(&common::test_app(pool.clone()))
        .await;
    let body = listing.json_value();
    assert!(
        !ids_of(&body).contains(&doomed.id.to_string()),
        "and no longer appears in the list: {body}"
    );

    let again = common::TestRequest::delete(&format!("{TOKENS}/{}", doomed.id))
        .bearer(alice.bearer())
        .send(&common::test_app(pool))
        .await;
    assert_eq!(again.status, StatusCode::NOT_FOUND, "a dead id is 404");
    assert_eq!(again.json_value(), not_found(), "with the standard body");
}

/// A foreign, unknown or malformed revocation id is one and the same
/// 404, and the foreign row survives the attempt.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn revoke_of_foreign_unknown_or_malformed_is_404(pool: PgPool) {
    let alice = session(&pool, "alice").await;
    let bob = session(&pool, "bob").await;
    let app = common::create_app_named(&pool, "Bobs", Some(&bob.user)).await;
    let theirs = common::create_token(&pool, &bob.user, &app, "read").await;

    let router = common::test_app(pool.clone());
    let cases = [
        (theirs.id.to_string(), "another user's token"),
        (wired_be::util::id::new_id().to_string(), "an unknown id"),
        ("not-a-uuid".to_owned(), "a malformed id"),
    ];
    for (id, label) in cases {
        let response = common::TestRequest::delete(&format!("{TOKENS}/{id}"))
            .bearer(alice.bearer())
            .send(&router)
            .await;
        assert_eq!(response.status, StatusCode::NOT_FOUND, "{label} is 404");
        assert_eq!(response.json_value(), not_found(), "{label} wording");
    }
    assert!(
        wired_be::models::oauth_token::get_with_app(&pool, &theirs.token)
            .await
            .expect("the lookup runs")
            .is_some(),
        "the foreign token survived"
    );
}

/// `GET /api/v1/pleroma/apps` — the apps the caller owns or holds tokens
/// for, deduplicated (the session app is both, the tokened app has two
/// tokens), newest first, under exactly the seven pinned keys; foreign
/// and untouched apps stay out.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn pleroma_apps_are_owned_and_tokened_deduped_newest_first(pool: PgPool) {
    let alice = session(&pool, "alice").await;
    let owned = common::create_app_named(&pool, "Owned", Some(&alice.user)).await;
    let tokened = common::create_app_named(&pool, "Tokened", None).await;
    let _first_token = common::create_token(&pool, &alice.user, &tokened, "read").await;
    let _second_token = common::create_token(&pool, &alice.user, &tokened, "write").await;
    let bob = session(&pool, "bob").await;
    let foreign = common::create_app_named(&pool, "Foreign", Some(&bob.user)).await;
    let _bobs_token = common::create_token(&pool, &bob.user, &foreign, "read").await;
    let stranger = common::create_app_named(&pool, "Stranger", None).await;

    let router = common::test_app(pool);
    let response = common::TestRequest::get(APPS)
        .bearer(alice.bearer())
        .send(&router)
        .await;
    assert_eq!(response.status, StatusCode::OK, "apps listing works");
    let body = response.json_value();
    assert_eq!(
        ids_of(&body),
        vec![
            tokened.id.to_string(),
            owned.id.to_string(),
            alice.token.app_id.to_string(),
        ],
        "newest first, each app exactly once"
    );
    assert_eq!(names_of(&body), vec!["Tokened", "Owned", "Session"]);
    for entry in body.as_array().expect("array body") {
        assert_eq!(keys_of(entry), APP_KEYS, "exactly the seven keys");
    }
    assert_eq!(body[0]["redirect_uri"], REDIRECT_URI, "as stored");
    assert_eq!(body[0]["client_id"], tokened.client_id, "the client id");
    assert_eq!(
        body[0]["client_secret"], tokened.client_secret,
        "and secret"
    );
    assert_eq!(body[0]["trusted"], false, "registrations are untrusted");
    assert!(body[0]["website"].is_null(), "an unset website is null");
    assert!(
        !ids_of(&body).contains(&foreign.id.to_string()),
        "another user's app stays out even when that user holds its token"
    );
    assert!(
        !ids_of(&body).contains(&stranger.id.to_string()),
        "and so does an app the caller neither owns nor holds a token for"
    );

    let bob_side = common::TestRequest::get(APPS)
        .bearer(bob.bearer())
        .send(&router)
        .await;
    assert_eq!(
        names_of(&bob_side.json_value()),
        vec!["Foreign", "Session"],
        "bob sees only his own two apps"
    );
}

/// Every route answers the no-token 401 of the `AuthUser` pipeline.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn every_route_demands_a_user_token(pool: PgPool) {
    let app = common::test_app(pool);
    let requests = [
        common::TestRequest::get(TOKENS),
        common::TestRequest::delete(&format!("{TOKENS}/{}", wired_be::util::id::new_id())),
        common::TestRequest::get(APPS),
    ];
    let labels = [
        "GET /api/oauth_tokens",
        "DELETE /api/oauth_tokens/{id}",
        "GET /api/v1/pleroma/apps",
    ];
    for (request, label) in requests.into_iter().zip(labels) {
        let response = request.send(&app).await;
        assert_eq!(response.status, StatusCode::UNAUTHORIZED, "{label} 401");
        assert_eq!(
            response.json_value(),
            json!({"error": "Invalid credentials"}),
            "{label} carries the no-token wording"
        );
    }
}
