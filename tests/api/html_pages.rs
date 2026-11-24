//! Tests for the HTML helper pages of `src/api/html_pages.rs` (WBE-220):
//! the retired `GET`/`POST` pair on `/frontend_switcher`, the retired
//! `GET /embed/{id}` card and `GET /notice/{id}/embed_player` player
//! (all answered by the fallback alone), the
//! `GET /mailer/unsubscribe/{token}` page (HMAC-verified tokens, the one
//! 400 `Invalid token` refusal, and the flag it clears in the database)
//! and the module headers every answer carries.
//!
//! The unsubscribe page signs against [`Config::secret_key_base`] and
//! writes the `email_notifications` flag. Every test leases a slot so the
//! full router can be mounted.
//!
//! [`Config::secret_key_base`]: wired_be::config::Config::secret_key_base

#![cfg(test)]

use axum::{Router, http::StatusCode};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    api::html_pages::unsubscribe_token,
    models::{status::Visibility, user},
};

use crate::common;

/// The exact CSP every page in the module must carry.
const PAGE_CSP: &str = "default-src 'none'; style-src 'unsafe-inline'";
/// The media type every page in the module must answer with.
const PAGE_CONTENT_TYPE: &str = "text/html; charset=utf-8";

/// The module headers `response` must carry: HTML media type, the CSP and
/// `X-Frame-Options: DENY` (no page here can be framed).
fn assert_module_headers(response: &common::TestResponse, who: &str) {
    assert_eq!(
        response.header("content-type"),
        Some(PAGE_CONTENT_TYPE),
        "{who} answers as HTML"
    );
    assert_eq!(
        response.header("content-security-policy"),
        Some(PAGE_CSP),
        "{who} carries the module CSP"
    );
    assert_eq!(
        response.header("x-frame-options"),
        Some("DENY"),
        "{who} cannot be framed"
    );
}

/// `GET /frontend_switcher` is not routed: the app-wide fallback answers
/// 404 and marks it with `x-wired-fallback: 1`.
#[tokio::test]
async fn get_frontend_switcher_is_answered_by_the_fallback() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool);
    let response = common::TestRequest::get("/frontend_switcher")
        .send(&app)
        .await;
    assert_eq!(
        response.status,
        StatusCode::NOT_FOUND,
        "the switcher page is gone"
    );
    assert_eq!(
        response.header("x-wired-fallback"),
        Some("1"),
        "only the fallback answers the path"
    );
}

/// `POST /frontend_switcher` is not routed either: 404 from the fallback.
#[tokio::test]
async fn post_frontend_switcher_is_answered_by_the_fallback() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool);
    let response = common::TestRequest::post("/frontend_switcher")
        .form(&[("frontend", "pleroma-fe")])
        .send(&app)
        .await;
    assert_eq!(
        response.status,
        StatusCode::NOT_FOUND,
        "the switcher pick is gone"
    );
    assert_eq!(
        response.header("x-wired-fallback"),
        Some("1"),
        "only the fallback answers the path"
    );
    assert_eq!(
        response.header("set-cookie"),
        None,
        "no frontend cookie is set"
    );
}

/// `GET /embed/{id}` of a public status is not routed: the fallback
/// answers 404 with `x-wired-fallback: 1` and no framable page.
#[tokio::test]
async fn the_embed_card_is_answered_by_the_fallback() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let author = common::create_local_user(&pool, "embedalice").await;
    let posted = common::create_status(&pool, &author, "embed me", Visibility::Public).await;
    let response = common::TestRequest::get(&format!("/embed/{}", posted.id))
        .send(&app)
        .await;
    assert_eq!(
        response.status,
        StatusCode::NOT_FOUND,
        "the embed card is gone"
    );
    assert_eq!(
        response.header("x-wired-fallback"),
        Some("1"),
        "only the fallback answers the path"
    );
    assert_eq!(
        response.header("x-frame-options"),
        None,
        "and no framable page is served"
    );
}

/// `GET /notice/{id}/embed_player` is not routed: the fallback answers
/// 404 with `x-wired-fallback: 1`.
#[tokio::test]
async fn the_embed_player_is_answered_by_the_fallback() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let author = common::create_local_user(&pool, "playeralice").await;
    let posted = common::create_status(&pool, &author, "play me", Visibility::Public).await;
    let _video =
        common::create_status_attachment(&pool, &posted, "video/mp4", "/media/two.mp4", 0).await;
    let response = common::TestRequest::get(&format!("/notice/{}/embed_player?url=x", posted.id))
        .send(&app)
        .await;
    assert_eq!(
        response.status,
        StatusCode::NOT_FOUND,
        "the player page is gone"
    );
    assert_eq!(
        response.header("x-wired-fallback"),
        Some("1"),
        "only the fallback answers the path"
    );
}

/// `GET /notice/{id}` with an `ActivityPub` `Accept` still answers the
/// Note JSON: only the `/embed_player` subpath went.
#[tokio::test]
async fn the_notice_path_still_negotiates_the_note_json() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let author = common::create_local_user(&pool, "noticealice").await;
    let posted = common::create_status(&pool, &author, "still a note", Visibility::Public).await;
    let response = common::TestRequest::get(&format!("/notice/{}", posted.id))
        .header("accept", "application/activity+json")
        .send(&app)
        .await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "the notice still serves ActivityPub"
    );
    assert_eq!(
        response.json_value()["type"],
        "Note",
        "the body is the Note: {}",
        response.text()
    );
}

/// The `SECRET_KEY_BASE` value the mailer tests sign with; never a real
/// secret.
const MAILER_SECRET: &str = "a-mailer-signing-secret-for-tests";

/// The real router with `Config::secret_key_base` set to `secret` (or
/// left unset for `None`), over `pool`.
fn mailer_app(pool: PgPool, secret: Option<&str>) -> Router {
    let mut config = common::test_config();
    config.secret_key_base = secret.map(str::to_owned);
    common::test_app_with_config(pool, config)
}

/// `GET /mailer/unsubscribe/{token}` over the mounted router.
async fn get_unsubscribe(app: &Router, token: &str) -> common::TestResponse {
    common::TestRequest::get(&format!("/mailer/unsubscribe/{token}"))
        .send(app)
        .await
}

/// `user`'s stored `email_notifications` flag.
async fn notifications_on(pool: &PgPool, user_id: Uuid) -> bool {
    let found = user::get_by_id(pool, user_id)
        .await
        .expect("the user lookup runs");
    found
        .expect("the user row is there")
        .email_notifications
        .get()
}

/// `token` with its first base64url character swapped for another (both
/// fully significant, so the decoded signature must change).
fn flipped_signature(token: &str) -> String {
    let mut chars = token.chars();
    let first = chars.next().expect("a minted token is not empty");
    let swap = if first == 'A' { 'B' } else { 'A' };
    let mut flipped = String::new();
    flipped.push(swap);
    flipped.extend(chars);
    flipped
}

/// A token minted with the configured secret answers 200
/// `You have been unsubscribed` as a protected page (`X-Frame-Options:
/// DENY`) and leaves `email_notifications` false in the database.
#[tokio::test]
async fn a_valid_unsubscribe_token_clears_the_flag() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = mailer_app(pool.clone(), Some(MAILER_SECRET));
    let subscriber = common::create_local_user(&pool, "unsubalice").await;
    assert!(
        notifications_on(&pool, subscriber.id).await,
        "the fixture starts subscribed"
    );
    let token = unsubscribe_token(MAILER_SECRET, subscriber.id);
    let response = get_unsubscribe(&app, &token).await;
    assert_eq!(response.status, StatusCode::OK, "a good token unsubscribes");
    assert_module_headers(&response, "the unsubscribe page");
    let html = response.text();
    assert!(
        html.contains("<title>You have been unsubscribed</title>")
            && html.contains("<h1>You have been unsubscribed</h1>"),
        "the page says so: {html}"
    );
    assert!(
        !notifications_on(&pool, subscriber.id).await,
        "the flag is stored cleared"
    );
}

/// A flipped signature byte, a token whose id segment names another
/// user, one signed with a different secret, one with no dot and one
/// whose signature segment is no base64url all answer the 400
/// `Invalid token` page and change no row.
#[tokio::test]
async fn every_bad_unsubscribe_token_answers_the_invalid_page_and_changes_no_row() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = mailer_app(pool.clone(), Some(MAILER_SECRET));
    let alice = common::create_local_user(&pool, "unsubbob").await;
    let bob = common::create_local_user(&pool, "unsubcarol").await;
    let token = unsubscribe_token(MAILER_SECRET, alice.id);
    let (signature, id_text) = token.split_once('.').expect("a minted token has a dot");
    for (bad_token, who) in [
        (flipped_signature(&token), "a flipped signature byte"),
        (
            format!("{signature}.{}", bob.id),
            "an id segment swapped for another user",
        ),
        (
            unsubscribe_token("a-different-secret", alice.id),
            "a token signed with a different secret",
        ),
        (signature.to_owned(), "a token with no dot"),
        (
            format!("!!!.{id_text}"),
            "a signature segment that is no base64url",
        ),
    ] {
        let response = get_unsubscribe(&app, &bad_token).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST, "{who} is 400");
        assert_module_headers(&response, who);
        assert!(
            response.text().contains("Invalid token"),
            "{who} says Invalid token: {}",
            response.text()
        );
    }
    assert!(
        notifications_on(&pool, alice.id).await,
        "no refusal touched alice's row"
    );
    assert!(
        notifications_on(&pool, bob.id).await,
        "and none touched bob's"
    );
}

/// With `secret_key_base` unset no token can verify: even one freshly
/// minted with the secret the tests otherwise use answers 400
/// `Invalid token` and changes no row.
#[tokio::test]
async fn without_a_signing_secret_every_unsubscribe_token_is_invalid() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = mailer_app(pool.clone(), None);
    let subscriber = common::create_local_user(&pool, "unsubdave").await;
    let token = unsubscribe_token(MAILER_SECRET, subscriber.id);
    let response = get_unsubscribe(&app, &token).await;
    assert_eq!(
        response.status,
        StatusCode::BAD_REQUEST,
        "an un-mintable config is 400"
    );
    assert_module_headers(&response, "the unconfigured token");
    assert!(
        response.text().contains("Invalid token"),
        "the page says so: {}",
        response.text()
    );
    assert!(
        notifications_on(&pool, subscriber.id).await,
        "the row is untouched"
    );
}
