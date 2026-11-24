//! OAuth token management over HTTP (`/api/oauth_tokens`,
//! `/api/v1/pleroma/apps`, WBE-49): the caller's valid tokens with their
//! app names, revocation of one's own token, and the apps the caller
//! registered.
//!
//! The tests pin what the ticket promises: the token list shows only the
//! caller's own unexpired tokens, newest first, with exactly the keys
//! `id`, `app_name` and `valid_until` (Mastodon time); a foreign,
//! unknown or malformed revocation id is one and the same 404 "Record
//! not found" while the owner's `DELETE` answers 200 `{}` and really
//! removes the row; `/api/v1/pleroma/apps` lists only owned apps, newest
//! first, under the eight pinned keys (the VAPID public key among them,
//! since push is always configured here), never the secret of an app the
//! caller merely holds a token for, and needs `read` or `follow`; and
//! every route answers 401 to an unauthenticated request; the list needs
//! `read:security` and the revocation `write:security`.

#![cfg(test)]

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;
use wired_be::models::{oauth_token::OauthToken, user::User};

use crate::common;

/// The token list endpoint.
const TOKENS: &str = "/api/oauth_tokens";
/// The Pleroma apps endpoint.
const APPS: &str = "/api/v1/pleroma/apps";
/// Every JSON key a token entry must carry, sorted.
const TOKEN_KEYS: [&str; 3] = ["app_name", "id", "valid_until"];
/// Every JSON key an app entity must carry, sorted.
const APP_KEYS: [&str; 8] = [
    "client_id",
    "client_secret",
    "id",
    "name",
    "redirect_uri",
    "trusted",
    "vapid_key",
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
/// The session token grants `read write`, which covers the
/// `read:security` and `write:security` scopes the token endpoints
/// demand. The session's app is owned by the user, so it also belongs in
/// every fixture math for `/api/v1/pleroma/apps`.
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
    let token = common::create_token(pool, &user, &app, "read write").await;
    Session { user, token }
}

/// A second token for the session's user carrying exactly `scopes`.
async fn token_with(pool: &PgPool, session: &Session, scopes: &str) -> OauthToken {
    let app = common::create_app_named(pool, "Scoped", None).await;
    common::create_token(pool, &session.user, &app, scopes).await
}

/// A second token for the session's user carrying the `read` scope, which
/// `GET /api/v1/pleroma/apps` requires.
async fn read_token(pool: &PgPool, session: &Session) -> OauthToken {
    token_with(pool, session, "read").await
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
#[tokio::test]
async fn token_list_shows_the_callers_valid_tokens_newest_first() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
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
#[tokio::test]
async fn revoke_removes_the_owners_token_and_answers_empty_object() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
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
#[tokio::test]
async fn revoke_of_foreign_unknown_or_malformed_is_404() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
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

/// `GET /api/v1/pleroma/apps` — only the apps the caller registered,
/// newest first, under exactly the eight pinned keys; an app the caller
/// merely holds tokens for, another user's app and an untouched app stay
/// out, so no foreign client secret is ever rendered.
#[tokio::test]
async fn pleroma_apps_are_only_the_ones_the_caller_owns_newest_first() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let vapid_key = wired_be::services::vapid::vapid_public_key(&pool)
        .await
        .expect("the instance VAPID key is readable");
    let alice = session(&pool, "alice").await;
    let alice_read = read_token(&pool, &alice).await;
    let owned = common::create_app_named(&pool, "Owned", Some(&alice.user)).await;
    let tokened = common::create_app_named(&pool, "Tokened", None).await;
    let _first_token = common::create_token(&pool, &alice.user, &tokened, "read").await;
    let _second_token = common::create_token(&pool, &alice.user, &tokened, "write").await;
    let bob = session(&pool, "bob").await;
    let bob_read = read_token(&pool, &bob).await;
    let foreign = common::create_app_named(&pool, "Foreign", Some(&bob.user)).await;
    let _bobs_token = common::create_token(&pool, &bob.user, &foreign, "read").await;
    let stranger = common::create_app_named(&pool, "Stranger", None).await;

    let router = common::test_app(pool);
    let response = common::TestRequest::get(APPS)
        .bearer(&alice_read.token)
        .send(&router)
        .await;
    assert_eq!(response.status, StatusCode::OK, "apps listing works");
    let body = response.json_value();
    assert_eq!(
        ids_of(&body),
        vec![owned.id.to_string(), alice.token.app_id.to_string()],
        "only the owned apps, newest first"
    );
    assert_eq!(names_of(&body), vec!["Owned", "Session"]);
    for entry in body.as_array().expect("array body") {
        assert_eq!(keys_of(entry), APP_KEYS, "exactly the eight keys");
        assert_eq!(
            entry["vapid_key"],
            json!(vapid_key),
            "every entry advertises the instance VAPID key"
        );
    }
    assert_eq!(body[0]["redirect_uri"], REDIRECT_URI, "as stored");
    assert_eq!(body[0]["client_id"], owned.client_id, "the client id");
    assert_eq!(body[0]["client_secret"], owned.client_secret, "and secret");
    assert_eq!(body[0]["trusted"], false, "registrations are untrusted");
    assert!(body[0]["website"].is_null(), "an unset website is null");
    let text = body.to_string();
    for hidden in [&tokened, &foreign, &stranger] {
        assert!(
            !ids_of(&body).contains(&hidden.id.to_string()),
            "{} stays out",
            hidden.client_name
        );
        assert!(
            !text.contains(&hidden.client_secret),
            "the secret of {} never leaks",
            hidden.client_name
        );
    }

    let bob_side = common::TestRequest::get(APPS)
        .bearer(&bob_read.token)
        .send(&router)
        .await;
    assert_eq!(
        names_of(&bob_side.json_value()),
        vec!["Foreign", "Session"],
        "bob sees only his own two apps"
    );
}

/// A user who only logged in through an app nobody owns gets `[]`, not
/// that app's secret.
#[tokio::test]
async fn pleroma_apps_is_empty_for_a_user_who_only_holds_tokens() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let user = common::create_local_user(&pool, "carol").await;
    let app_b = common::create_app_named(&pool, "App B", None).await;
    let token = common::create_token(&pool, &user, &app_b, "read").await;

    let response = common::TestRequest::get(APPS)
        .bearer(&token.token)
        .send(&common::test_app(pool))
        .await;
    assert_eq!(response.status, StatusCode::OK, "the listing works");
    assert_eq!(response.json_value(), json!([]), "and lists no app");
}

/// The listing needs `read` or `follow`: a `push`-only token is a 403
/// naming both, while a `follow` token passes.
#[tokio::test]
async fn pleroma_apps_demand_read_or_follow_scope() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = session(&pool, "alice").await;
    let router = common::test_app(pool.clone());
    let push = token_with(&pool, &alice, "push").await;

    let denied = common::TestRequest::get(APPS)
        .bearer(&push.token)
        .send(&router)
        .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN, "push alone is out");
    assert_eq!(
        denied.json_value(),
        json!({"error": "Insufficient permissions: read | follow."}),
        "the wording names both accepted scopes"
    );

    let app = common::create_app_named(&pool, "Follower", None).await;
    let follow = common::create_token(&pool, &alice.user, &app, "follow").await;
    let allowed = common::TestRequest::get(APPS)
        .bearer(&follow.token)
        .send(&router)
        .await;
    assert_eq!(allowed.status, StatusCode::OK, "follow suffices");
}

/// Every route answers the no-token 401 of the `AuthUser` pipeline.
#[tokio::test]
async fn every_route_demands_a_user_token() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
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

/// `GET /api/oauth_tokens` needs `read:security`: a `read` token (which
/// covers it) lists, a `read:statuses` token is a 403 naming the scope.
#[tokio::test]
async fn token_list_demands_read_security_scope() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = session(&pool, "alice").await;
    let router = common::test_app(pool.clone());

    let read = token_with(&pool, &alice, "read").await;
    let allowed = common::TestRequest::get(TOKENS)
        .bearer(&read.token)
        .send(&router)
        .await;
    assert_eq!(allowed.status, StatusCode::OK, "read covers read:security");

    let narrow = token_with(&pool, &alice, "read:statuses").await;
    let denied = common::TestRequest::get(TOKENS)
        .bearer(&narrow.token)
        .send(&router)
        .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN, "read:statuses is out");
    assert_eq!(
        denied.json_value(),
        json!({"error": "Insufficient permissions: read:security."}),
        "the wording names the missing scope"
    );
}

/// `DELETE /api/oauth_tokens/{id}` needs `write:security`: a `write`
/// token revokes, a `read` token is a 403 and the target keeps working.
#[tokio::test]
async fn revoke_demands_write_security_scope() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = session(&pool, "alice").await;
    let router = common::test_app(pool.clone());
    let target = token_with(&pool, &alice, "read").await;
    let reader = token_with(&pool, &alice, "read").await;
    let writer = token_with(&pool, &alice, "write").await;

    let denied = common::TestRequest::delete(&format!("{TOKENS}/{}", target.id))
        .bearer(&reader.token)
        .send(&router)
        .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN, "read cannot revoke");
    assert_eq!(
        denied.json_value(),
        json!({"error": "Insufficient permissions: write:security."}),
        "the wording names the missing scope"
    );
    let still_works = common::TestRequest::get(TOKENS)
        .bearer(&target.token)
        .send(&router)
        .await;
    assert_eq!(
        still_works.status,
        StatusCode::OK,
        "the target token still authenticates after the refused revoke"
    );

    let allowed = common::TestRequest::delete(&format!("{TOKENS}/{}", target.id))
        .bearer(&writer.token)
        .send(&router)
        .await;
    assert_eq!(
        allowed.status,
        StatusCode::OK,
        "write covers write:security"
    );
    let gone = common::TestRequest::get(TOKENS)
        .bearer(&target.token)
        .send(&router)
        .await;
    assert_eq!(
        gone.status,
        StatusCode::UNAUTHORIZED,
        "the target is revoked"
    );
}
