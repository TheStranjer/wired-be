//! The `refresh_token` grant over `POST /oauth/token`: the rotation into
//! a fresh pair with the same user and scopes verbatim, the old access
//! and refresh credentials left dead, expired access tokens still
//! refreshable, and the 400 "Invalid credentials" for a foreign app's, an
//! unknown or a deactivated owner's refresh token, with its own "Missing
//! parameter: `refresh_token`" refusal.

use axum::http::StatusCode;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::DEFAULT_OAUTH_TOKEN_TTL_SECS,
    models::{oauth_app::OauthApp, oauth_token},
};

use super::revoke::{probe_user_endpoint, token_rows};
use super::support::{PASSWORD, assert_no_store, error_text, fixtures, post_json};
use crate::common;

/// A refresh-grant body carrying app `app`'s credentials in the plain
/// body fields, rotating `refresh_token`.
fn refresh_body(app: &OauthApp, refresh_token: &str) -> Value {
    json!({
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
        "client_id": app.client_id,
        "client_secret": app.client_secret,
    })
}

/// Rows in `oauth_tokens` in total.
async fn token_count(pool: &PgPool) -> i64 {
    let (rows,): (i64,) = sqlx::query_as("SELECT count(*) FROM oauth_tokens")
        .fetch_one(pool)
        .await
        .expect("the count reaches the database");
    rows
}

/// The `refresh_token` grant rotates the pair: the standard
/// password-grant answer with fresh credentials, the same user and
/// scopes, and the old row deleted behind it.
#[tokio::test]
async fn refresh_token_grant_rotates_the_pair() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (user, app) = fixtures(&pool, "mel", "read write").await;
    let old = common::create_token(&pool, &user, &app, "read write").await;
    let http = common::test_app(pool.clone());
    let response = post_json(&http, &refresh_body(&app, &old.refresh_token)).await;

    assert_eq!(response.status, StatusCode::OK, "the rotation succeeds");
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "the answer is JSON"
    );
    assert_no_store(&response);

    let body = response.json_value();
    assert_eq!(body["token_type"], "Bearer", "bearer tokens");
    let access = body["access_token"]
        .as_str()
        .expect("access_token is a string");
    let refresh = body["refresh_token"]
        .as_str()
        .expect("refresh_token is a string");
    assert_eq!(access.len(), 43, "the 43-char Pleroma token shape");
    assert_eq!(refresh.len(), 43, "both credentials share it");
    assert_ne!(access, old.token, "a fresh access token");
    assert_ne!(refresh, old.refresh_token, "and a fresh refresh token");
    assert_eq!(body["scope"], "read write", "the same scopes, verbatim");
    let created = body["created_at"]
        .as_i64()
        .expect("created_at is unix seconds");
    let now = chrono::Utc::now().timestamp();
    assert!(
        (now - 5..=now + 5).contains(&created),
        "created_at is issue time, got {created} vs {now}"
    );
    let ttl = i64::try_from(DEFAULT_OAUTH_TOKEN_TTL_SECS).expect("the default fits in i64");
    let expires_in = body["expires_in"].as_i64().expect("expires_in is seconds");
    assert!(
        (ttl - 5..=ttl).contains(&expires_in),
        "expires_in is the configured lifetime afresh, got {expires_in}"
    );
    let id = body["id"]
        .as_str()
        .and_then(|text| Uuid::parse_str(text).ok())
        .expect("id is the hyphenated token uuid");
    assert!(id.get_version_num() == 7, "the id is a UUIDv7 (decision 4)");
    assert_eq!(body["me"], user.ap_id, "me is the user's actor id");
    assert_eq!(
        body.as_object().expect("an object").len(),
        8,
        "exactly the documented keys"
    );

    let (stored, stored_app) = oauth_token::get_with_app(&pool, access)
        .await
        .expect("the lookup reaches the database")
        .expect("the new token row was stored");
    assert_eq!(stored.id, id, "the response id is the stored primary key");
    assert_eq!(stored.refresh_token, refresh, "the new secret");
    assert_eq!(stored.user_id, Some(user.id), "the token keeps its user");
    assert_eq!(stored.app_id, app.id, "and its app");
    assert_eq!(stored.scopes, "read write", "and its scopes");
    assert_eq!(stored_app.id, app.id, "joined to its app");

    assert_eq!(
        token_rows(&pool, &old.token).await,
        0,
        "the old row is gone"
    );
    assert_eq!(token_count(&pool).await, 1, "only the new pair is stored");
}

/// The rotation ends the old pair: the old access token stops
/// authenticating, the old refresh token no longer rotates, while the
/// new credentials work — and the new refresh token rotates again in
/// turn.
#[tokio::test]
async fn rotation_ends_the_old_pair_and_the_new_one_works() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (user, app) = fixtures(&pool, "nia", "read").await;
    let old = common::create_token(&pool, &user, &app, "read").await;
    let http = common::test_app(pool.clone());

    let before = probe_user_endpoint(&http, &old.token).await;
    assert_eq!(before.status, StatusCode::OK, "the old token works before");

    let response = post_json(&http, &refresh_body(&app, &old.refresh_token)).await;
    assert_eq!(response.status, StatusCode::OK, "the rotation succeeds");
    let body = response.json_value();
    let access = body["access_token"].as_str().expect("a token string");
    let refresh = body["refresh_token"].as_str().expect("a token string");

    let dead = probe_user_endpoint(&http, &old.token).await;
    assert_eq!(
        dead.status,
        StatusCode::UNAUTHORIZED,
        "the old access token stopped working"
    );
    assert_eq!(
        error_text(&dead),
        "The access token is invalid",
        "its row is gone"
    );

    let reuse = post_json(&http, &refresh_body(&app, &old.refresh_token)).await;
    assert_eq!(
        reuse.status,
        StatusCode::BAD_REQUEST,
        "the old refresh token cannot rotate again"
    );
    assert_eq!(
        error_text(&reuse),
        "Invalid credentials",
        "like any unknown one"
    );
    assert_no_store(&reuse);

    let fresh = probe_user_endpoint(&http, access).await;
    assert_eq!(fresh.status, StatusCode::OK, "the new token authenticates");

    let again = post_json(&http, &refresh_body(&app, refresh)).await;
    assert_eq!(again.status, StatusCode::OK, "and the new pair rotates");
    assert_eq!(token_count(&pool).await, 1, "still just one live row");
}

/// Refresh tokens do not expire on their own: an access token pushed
/// past its `valid_until` still rotates into a fresh, full-lifetime pair.
#[tokio::test]
async fn an_expired_access_token_is_still_refreshable() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (user, app) = fixtures(&pool, "oli", "read write").await;
    let old = common::create_token(&pool, &user, &app, "read write").await;
    common::expire_token(&pool, old.id).await;
    let http = common::test_app(pool.clone());

    let dead = probe_user_endpoint(&http, &old.token).await;
    assert_eq!(dead.status, StatusCode::UNAUTHORIZED, "it lapsed for real");

    let response = post_json(&http, &refresh_body(&app, &old.refresh_token)).await;
    assert_eq!(response.status, StatusCode::OK, "and still rotates");
    let body = response.json_value();
    let ttl = i64::try_from(DEFAULT_OAUTH_TOKEN_TTL_SECS).expect("the default fits in i64");
    let expires_in = body["expires_in"].as_i64().expect("expires_in is seconds");
    assert!(
        (ttl - 5..=ttl).contains(&expires_in),
        "the new token carries the full lifetime, got {expires_in}"
    );
    let access = body["access_token"].as_str().expect("a token string");
    let alive = probe_user_endpoint(&http, access).await;
    assert_eq!(alive.status, StatusCode::OK, "and it authenticates");
}

/// A refresh token issued to another application is the shared 400
/// "Invalid credentials"; the old pair stays alive and keeps working.
#[tokio::test]
async fn the_wrong_app_cannot_rotate_another_apps_token() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (user, owner) = fixtures(&pool, "pam", "read").await;
    let stranger = common::create_app(&pool, "read").await;
    let token = common::create_token(&pool, &user, &owner, "read").await;
    let http = common::test_app(pool.clone());

    let foreign = post_json(&http, &refresh_body(&stranger, &token.refresh_token)).await;
    assert_eq!(
        foreign.status,
        StatusCode::BAD_REQUEST,
        "another app's token is a 400"
    );
    assert_eq!(
        error_text(&foreign),
        "Invalid credentials",
        "Pleroma's shared refusal"
    );
    assert_no_store(&foreign);

    let mut poisoned_owner = refresh_body(&owner, &token.refresh_token);
    poisoned_owner["client_secret"] = json!("not-the-secret");
    let secret_answer = post_json(&http, &poisoned_owner).await;
    assert_eq!(
        secret_answer.status,
        StatusCode::BAD_REQUEST,
        "a wrong secret is a 400"
    );
    assert_eq!(
        error_text(&secret_answer),
        "Invalid credentials",
        "the same message"
    );

    assert_eq!(
        token_rows(&pool, &token.token).await,
        1,
        "neither refusal deleted the token"
    );
    assert_eq!(token_count(&pool).await, 1, "nor minted a new one");
    let probe = probe_user_endpoint(&http, &token.token).await;
    assert_eq!(probe.status, StatusCode::OK, "and it still authenticates");
}

/// An unknown refresh token answers the shared 400 "Invalid
/// credentials", minting nothing.
#[tokio::test]
async fn an_unknown_refresh_token_is_invalid_credentials() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_user, app) = fixtures(&pool, "quim", "read").await;
    let http = common::test_app(pool.clone());
    let response = post_json(&http, &refresh_body(&app, "no-such-refresh-token")).await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST, "a 400");
    assert_eq!(
        error_text(&response),
        "Invalid credentials",
        "not told apart from the rest"
    );
    assert_no_store(&response);
    assert_eq!(token_count(&pool).await, 0, "the refusal minted no token");
}

/// The deactivated owner's refresh token answers the shared 400 "Invalid
/// credentials" — unlike the password grant there is no 403 to give —
/// and the stored pair stays untouched.
#[tokio::test]
async fn a_deactivated_owner_cannot_rotate() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let user = common::create_deactivated_user_with_password(&pool, "sara", PASSWORD).await;
    let app = common::create_app(&pool, "read").await;
    let token = common::create_token(&pool, &user, &app, "read").await;
    let http = common::test_app(pool.clone());

    let response = post_json(&http, &refresh_body(&app, &token.refresh_token)).await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST, "a 400");
    assert_eq!(
        error_text(&response),
        "Invalid credentials",
        "Pleroma's shared refusal"
    );
    assert_no_store(&response);
    assert_eq!(
        token_rows(&pool, &token.token).await,
        1,
        "the refusal deleted nothing"
    );
    assert_eq!(token_count(&pool).await, 1, "and minted nothing");
}

/// Without the `refresh_token` parameter the request is refused by name
/// before the credentials are looked at, over JSON and form alike.
#[tokio::test]
async fn a_missing_refresh_token_parameter_is_named() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_user, app) = fixtures(&pool, "tess", "read").await;
    let http = common::test_app(pool.clone());

    let mut body = refresh_body(&app, "a-live-token");
    body["refresh_token"] = json!(null);
    let response = post_json(&http, &body).await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST, "a 400");
    assert_eq!(
        error_text(&response),
        "Missing parameter: refresh_token",
        "naming the missing parameter"
    );
    assert_no_store(&response);

    // The parameter gate runs first: broken credentials cannot change
    // the answer.
    let both_bad = post_json(
        &http,
        &json!({"grant_type": "refresh_token", "client_id": "no-such-client"}),
    )
    .await;
    assert_eq!(both_bad.status, StatusCode::BAD_REQUEST, "still a 400");
    assert_eq!(
        error_text(&both_bad),
        "Missing parameter: refresh_token",
        "with the same message"
    );

    // A form body without the key at all is "missing", not a 422.
    let form_answer = common::TestRequest::post("/oauth/token")
        .form(&[("grant_type", "refresh_token"), ("scope", "read")])
        .send(&http)
        .await;
    assert_eq!(form_answer.status, StatusCode::BAD_REQUEST, "alike");
    assert_eq!(
        error_text(&form_answer),
        "Missing parameter: refresh_token",
        "the same refusal"
    );
}

/// Rotation copies the old scopes verbatim: a `scope` parameter in the
/// refresh request plays no part.
#[tokio::test]
async fn rotation_preserves_the_exact_scopes() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (user, app) = fixtures(&pool, "ugo", "read write").await;
    let old = common::create_token(&pool, &user, &app, "read:accounts write:statuses").await;
    let http = common::test_app(pool.clone());

    let mut body = refresh_body(&app, &old.refresh_token);
    body["scope"] = json!("admin:read");
    let response = post_json(&http, &body).await;
    assert_eq!(response.status, StatusCode::OK, "the rotation succeeds");
    let answer = response.json_value();
    assert_eq!(
        answer["scope"], "read:accounts write:statuses",
        "the stored scopes win over anything asked for"
    );
    let access = answer["access_token"].as_str().expect("a token string");
    let (stored, _) = oauth_token::get_with_app(&pool, access)
        .await
        .expect("the lookup reaches the database")
        .expect("the token row was stored");
    assert_eq!(
        stored.scopes, "read:accounts write:statuses",
        "and that is what got stored"
    );
}

/// The grant accepts the Mastodon form body and prefers the
/// `Authorization: Basic` client pair over poisoned body fields, like
/// every other half of the endpoint.
#[tokio::test]
async fn rotation_accepts_form_bodies_and_basic_client_credentials() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (user, app) = fixtures(&pool, "wil", "read").await;
    let token = common::create_token(&pool, &user, &app, "read").await;
    let http = common::test_app(pool.clone());

    let form_answer = common::TestRequest::post("/oauth/token")
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", token.refresh_token.as_str()),
            ("client_id", app.client_id.as_str()),
            ("client_secret", app.client_secret.as_str()),
        ])
        .send(&http)
        .await;
    assert_eq!(form_answer.status, StatusCode::OK, "the form body rotates");
    assert_no_store(&form_answer);
    let body = form_answer.json_value();
    let refresh = body["refresh_token"].as_str().expect("a token string");

    let basic = STANDARD.encode(format!("{}:{}", app.client_id, app.client_secret));
    let poisoned = json!({
        "grant_type": "refresh_token",
        "refresh_token": refresh,
        "client_id": "not-this-client",
        "client_secret": "not-this-secret",
    });
    let basic_answer = common::TestRequest::post("/oauth/token")
        .header("authorization", &format!("Basic {basic}"))
        .json(&poisoned)
        .send(&http)
        .await;
    assert_eq!(
        basic_answer.status,
        StatusCode::OK,
        "the Basic pair authenticates the app and wins over the body"
    );
    assert_eq!(token_count(&pool).await, 1, "two rotations, one live row");
}
