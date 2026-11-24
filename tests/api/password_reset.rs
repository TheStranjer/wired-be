//! Password reset over HTTP (`src/api/password_reset.rs`): the request
//! endpoint `POST /auth/password`, the emailed token page `GET
//! /api/v1/pleroma/password_reset/{token}` and the completion form `POST
//! /api/v1/pleroma/password_reset`.
//!
//! The request endpoint answers 204 with an empty body to everything —
//! known account, unknown account, deactivated or remote account, and the
//! body naming neither `email` nor `nickname` — and only the known local
//! active account gets a token and a queued `mailer.send` letter; the
//! limiter's `password_reset` headers ride on the calls it counts. The
//! mailed link is the frontend's `{FRONTEND_BASE_URL}/password_reset/{token}`.
//! The token check answers 200 `{"nickname"}` for a live token and 404
//! `{"error"}` for unknown, spent and lapsed ones, and completion answers
//! 204 or 422 `{"error"}`, whatever the `Accept` header — no HTML anywhere.
//! Completion validates
//! the token, `MIN_PASSWORD_LENGTH` and the confirmation (each failure is
//! a 422 with its message), then changes the password,
//! spends the token, deletes the account's other tokens, revokes every
//! OAuth token it holds and clears `password_reset_pending` — and a
//! pending account is refused the `/oauth/token` password grant with 403
//! `password_reset_required` until the reset is done.

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use wired_be::{
    config::Config,
    models::{oauth_app::OauthApp, password_reset, user::User},
    util::token::token_digest,
};

use crate::common;

/// The password the fixtures start with (over the 8-character minimum).
const OLD_PASSWORD: &str = "correct horse battery staple";
/// The password the completion tests set (also over the minimum).
const NEW_PASSWORD: &str = "a completely different long password";
/// The request endpoint under test, mounted at the top level.
const REQUEST: &str = "/auth/password";
/// The completion endpoint and the token page's prefix.
const RESET: &str = "/api/v1/pleroma/password_reset";
/// The scopes the login-check fixtures register their apps with.
const SCOPES: &str = "read write";

/// A config with `SMTP_HOST` set: [`deliver_later`] only queues the letter
/// when a relay is configured, which the suite's default config is not.
///
/// [`deliver_later`]: wired_be::mailer::deliver_later
fn mail_config() -> Config {
    let mut config = common::test_config();
    config.smtp_host = Some("smtp.example.org".to_owned());
    config.frontend_base_url = "https://fe.example".to_owned();
    config
}

/// The application with mail enabled, so a request queues its letter.
fn mail_app(pool: PgPool) -> Router {
    common::test_app_with_config(pool, mail_config())
}

/// A local account that can log in with [`OLD_PASSWORD`], plus its derived
/// email (the fixture's own convention).
async fn account(pool: &PgPool, nickname: &str) -> User {
    common::create_user_with_password(pool, nickname, OLD_PASSWORD).await
}

/// The email address of a fixture account.
fn email_of(nickname: &str) -> String {
    format!("{nickname}@example.org")
}

/// `POST /auth/password` as a form naming `key` with `value`.
async fn request_form(app: &Router, key: &str, value: &str) -> common::TestResponse {
    common::TestRequest::post(REQUEST)
        .form(&[(key, value)])
        .send(app)
        .await
}

/// Asserts the 204 with an empty body that every request answer carries.
fn assert_no_content(response: &common::TestResponse, who: &str) {
    common::assert_answer(response, StatusCode::NO_CONTENT, who);
    assert!(response.body.is_empty(), "{who}: the 204 body is empty");
}

/// Every `mailer.send` job's arguments, oldest first.
async fn queued_letters(pool: &PgPool) -> Vec<Value> {
    sqlx::query_scalar::<_, Value>(
        "SELECT args FROM jobs WHERE queue = 'mailer' AND kind = 'mailer.send' ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .expect("the mailer queue reads back")
}

/// The reset tokens stored for `user`, oldest first.
async fn stored_tokens(pool: &PgPool, user: &User) -> Vec<password_reset::PasswordResetToken> {
    sqlx::query_as::<_, password_reset::PasswordResetToken>(
        "SELECT * FROM password_reset_tokens WHERE user_id = $1 ORDER BY inserted_at",
    )
    .bind(user.id)
    .fetch_all(pool)
    .await
    .expect("the token table reads back")
}

/// The frontend's reset-screen prefix the mailed link starts with.
const FRONTEND_RESET: &str = "https://fe.example/password_reset";

/// The raw reset token in the link of the mailed `letter`: the characters
/// after `{FRONTEND_RESET}/` up to the first one a token never carries.
fn mailed_token(letter: &Value) -> String {
    let body = letter["text_body"]
        .as_str()
        .expect("the letter has a text body");
    let (_, after) = body
        .split_once(&format!("{FRONTEND_RESET}/"))
        .expect("the letter carries the reset link");
    after
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect()
}

/// Requests a reset for `user` naming it by `key` and hands back the
/// issued token's text as the mailed link carries it, checking the 204 on
/// the way and that the table stores only that text's digest.
async fn request_reset(app: &Router, pool: &PgPool, user: &User, key: &str) -> String {
    let value = match key {
        "email" => email_of(&user.nickname),
        _ => user.nickname.clone(),
    };
    let response = request_form(app, key, &value).await;
    assert_no_content(&response, "the reset request");
    let letters = queued_letters(pool).await;
    let token = mailed_token(letters.last().expect("the request queued a letter"));
    let stored = stored_tokens(pool, user).await;
    assert!(
        stored
            .iter()
            .any(|row| row.token_digest == token_digest(&token)),
        "the stored row is the digest of the mailed token"
    );
    assert!(
        stored.iter().all(|row| row.token_digest != token),
        "the mailed token itself is not stored"
    );
    token
}

/// POSTs the completion form (Pleroma's nested `data[...]` names).
async fn complete_nested(
    app: &Router,
    token: &str,
    password: &str,
    confirmation: &str,
) -> common::TestResponse {
    common::TestRequest::post(RESET)
        .form(&[
            ("data[token]", token),
            ("data[password]", password),
            ("data[password_confirmation]", confirmation),
        ])
        .send(app)
        .await
}

/// The password grant for `username`, carrying `app`'s credentials.
fn grant_body(app: &OauthApp, username: &str, password: &str) -> Value {
    json!({
        "grant_type": "password",
        "username": username,
        "password": password,
        "scope": SCOPES,
        "client_id": app.client_id,
        "client_secret": app.client_secret,
    })
}

/// Runs the password grant.
async fn grant(app: &Router, body: &Value) -> common::TestResponse {
    common::TestRequest::post("/oauth/token")
        .json(body)
        .send(app)
        .await
}

/// Sets `user`'s `password_reset_pending` flag straight in the row (the
/// admin force-reset flow that writes it is a later ticket).
async fn set_pending(pool: &PgPool, user: &User, pending: bool) {
    let updated = sqlx::query("UPDATE users SET password_reset_pending = $2 WHERE id = $1")
        .bind(user.id)
        .bind(pending)
        .execute(pool)
        .await
        .expect("the flag update runs");
    assert_eq!(updated.rows_affected(), 1, "the account row exists");
}

/// Backdates `token`'s row outside the 24-hour window.
async fn lapse_token(pool: &PgPool, token: &str) {
    let aged = sqlx::query("UPDATE password_reset_tokens SET inserted_at = now() - interval '25 hours' WHERE token_digest = $1")
        .bind(token_digest(token))
        .execute(pool)
        .await
        .expect("the ageing update runs");
    assert_eq!(aged.rows_affected(), 1, "the token row exists");
}

/// A known account answering 204 with an empty body, its token stored
/// unused, and one letter queued whose link carries that token — by email
/// address and by nickname alike.
#[tokio::test]
async fn known_accounts_get_204_and_the_reset_letter() {
    for key in ["email", "nickname"] {
        let db = common::test_db().await;
        let pool = db.pool().clone();
        let user = account(&pool, "alice").await;
        let http = mail_app(pool.clone());
        let response = request_reset(&http, &pool, &user, key).await;
        let tokens = stored_tokens(&pool, &user).await;
        assert_eq!(tokens.len(), 1, "one token minted");
        assert!(!tokens[0].used, "and it starts unused");
        assert_eq!(
            token_digest(&response),
            tokens[0].token_digest,
            "the mailed token's digest is stored"
        );
        let letters = queued_letters(&pool).await;
        assert_eq!(letters.len(), 1, "exactly one letter queued");
        assert_eq!(
            letters[0]["to"],
            json!(email_of("alice")),
            "to the account's email"
        );
        assert_eq!(
            letters[0]["subject"],
            json!("Password reset"),
            "Pleroma's subject"
        );
        let link = format!("{FRONTEND_RESET}/{response}");
        assert!(
            letters[0]["text_body"]
                .as_str()
                .is_some_and(|body| body.contains(&link)),
            "the FRONTEND_BASE_URL-built link is in the body: {}",
            letters[0]["text_body"]
        );
    }
}

/// A JSON request body works exactly like the form: 204 and a letter.
#[tokio::test]
async fn request_body_may_be_json() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let user = account(&pool, "jsonie").await;
    let http = mail_app(pool.clone());
    let response = common::TestRequest::post(REQUEST)
        .json(&json!({"email": email_of(&user.nickname)}))
        .send(&http)
        .await;
    assert_no_content(&response, "the JSON reset request");
    assert_eq!(queued_letters(&pool).await.len(), 1, "the letter queued");
}

/// Unknown accounts, deactivated accounts, remote mirrors and the empty
/// body all answer the same 204 — with no token minted and no letter
/// queued, so nothing leaks which of them exist.
#[tokio::test]
async fn nothing_found_still_answers_204_with_no_letter() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let gone = common::create_deactivated_user_with_password(&pool, "gone", OLD_PASSWORD).await;
    let _mirror = common::create_remote_user(&pool, "bob", "remote.example").await;
    let http = mail_app(pool.clone());
    for (key, value) in [
        ("email", "nobody@example.org".to_owned()),
        ("nickname", "nobody".to_owned()),
        ("email", email_of(&gone.nickname)),
        ("nickname", "gone".to_owned()),
        ("nickname", "bob@remote.example".to_owned()),
    ] {
        let response = request_form(&http, key, &value).await;
        assert_no_content(&response, &format!("{key}={value}"));
    }
    let response = common::TestRequest::post(REQUEST)
        .form(&[])
        .send(&http)
        .await;
    assert_no_content(&response, "the body naming neither param");
    assert!(
        queued_letters(&pool).await.is_empty(),
        "no letter was queued"
    );
    let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM password_reset_tokens")
        .fetch_one(&pool)
        .await
        .expect("the table counts");
    assert_eq!(stored, 0, "and no token was minted");
}

/// A valid token's check without any `Accept` header is still JSON: 200
/// `{"nickname"}`, never `text/html`.
#[tokio::test]
async fn token_check_without_accept_answers_json_for_a_live_token() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let user = account(&pool, "alice").await;
    let http = mail_app(pool.clone());
    let token = request_reset(&http, &pool, &user, "nickname").await;
    let response = common::TestRequest::get(&format!("{RESET}/{token}"))
        .send(&http)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the live token check");
    assert!(
        response
            .header("content-type")
            .is_some_and(|value| value.starts_with("application/json")),
        "the answer is JSON: {:?}",
        response.header("content-type")
    );
    assert_eq!(
        response.json_value(),
        json!({"nickname": "alice"}),
        "{}",
        response.text()
    );
}

/// Unknown, spent and lapsed tokens all answer the same 404 JSON error
/// without any `Accept` header.
#[tokio::test]
async fn token_check_without_accept_answers_404_for_unknown_spent_and_lapsed() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let user = account(&pool, "alice").await;
    let http = mail_app(pool.clone());
    let spent = request_reset(&http, &pool, &user, "nickname").await;
    let lapsed = {
        let extra = password_reset::create_for_user(&pool, user.id)
            .await
            .expect("the second token mints");
        extra.token
    };
    let record = password_reset::get_valid(&pool, &spent, std::time::Duration::from_secs(60))
        .await
        .expect("the lookup runs")
        .expect("the first token is live");
    assert!(
        password_reset::mark_used(&pool, record.id)
            .await
            .expect("the spend runs"),
        "the first spend lands"
    );
    lapse_token(&pool, &lapsed).await;

    for token in ["never-minted".to_owned(), spent, lapsed] {
        let response = common::TestRequest::get(&format!("{RESET}/{token}"))
            .send(&http)
            .await;
        common::assert_answer(&response, StatusCode::NOT_FOUND, "the invalid-token check");
        common::assert_error(&response, "Invalid Token", "the invalid token");
    }
}

/// Completing with a live token changes the password (the old one stops
/// logging in, the new one starts), spends the token, deletes the
/// account's other tokens and answers 204.
#[tokio::test]
async fn completion_changes_the_password_and_spends_the_token() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let user = account(&pool, "alice").await;
    let http = mail_app(pool.clone());
    let token = request_reset(&http, &pool, &user, "email").await;
    let rival = password_reset::create_for_user(&pool, user.id)
        .await
        .expect("the rival token mints");

    let response = complete_nested(&http, &token, NEW_PASSWORD, NEW_PASSWORD).await;
    assert_no_content(&response, "the completion");

    let after = wired_be::models::user::get_by_id(&pool, user.id)
        .await
        .expect("the account reads back")
        .expect("the account exists");
    assert_ne!(
        after.password_hash, user.password_hash,
        "the stored hash changed"
    );
    assert!(
        wired_be::models::user::register::authenticate(&pool, "alice", NEW_PASSWORD)
            .await
            .is_some(),
        "the new password logs in"
    );
    assert!(
        wired_be::models::user::register::authenticate(&pool, "alice", OLD_PASSWORD)
            .await
            .is_none(),
        "the old password does not"
    );
    let left = stored_tokens(&pool, &after).await;
    assert_eq!(left.len(), 1, "only the spent token is left");
    assert!(left[0].used, "and it is marked used");
    assert!(
        password_reset::get_valid(&pool, &rival.token, std::time::Duration::from_secs(60))
            .await
            .expect("the lookup runs")
            .is_none(),
        "the account's other token is gone"
    );
    let again = complete_nested(&http, &token, NEW_PASSWORD, NEW_PASSWORD).await;
    common::assert_answer(&again, StatusCode::UNPROCESSABLE_ENTITY, "the replay");
    common::assert_error(&again, "Invalid Token", "spent once, never again");
}

/// Completion accepts the flat parameter spellings and a JSON `data`
/// object exactly like the nested form.
#[tokio::test]
async fn completion_accepts_flat_fields_and_json() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let user = account(&pool, "flat").await;
    let http = mail_app(pool.clone());
    let token = request_reset(&http, &pool, &user, "nickname").await;
    let response = common::TestRequest::post(RESET)
        .form(&[
            ("token", token.as_str()),
            ("password", NEW_PASSWORD),
            ("password_confirmation", NEW_PASSWORD),
        ])
        .send(&http)
        .await;
    assert_no_content(&response, "the flat form completion");

    let second_user = account(&pool, "nestedjson").await;
    let second = request_reset(&http, &pool, &second_user, "nickname").await;
    let json_done = common::TestRequest::post(RESET)
        .json(&json!({
            "data": {
                "token": second,
                "password": NEW_PASSWORD,
                "password_confirmation": NEW_PASSWORD,
            },
        }))
        .send(&http)
        .await;
    assert_no_content(&json_done, "the JSON completion");
}

/// The token page as a JSON client asks for it.
async fn json_page(app: &Router, token: &str) -> common::TestResponse {
    common::TestRequest::get(&format!("{RESET}/{token}"))
        .header("accept", "application/json")
        .send(app)
        .await
}

/// The completion as a JSON client posts it (flat fields).
async fn complete_json(
    app: &Router,
    token: &str,
    password: &str,
    confirmation: &str,
) -> common::TestResponse {
    common::TestRequest::post(RESET)
        .json(&json!({
            "token": token,
            "password": password,
            "password_confirmation": confirmation,
        }))
        .send(app)
        .await
}

/// A live token's JSON page is 200 `{"nickname"}` and nothing else.
#[tokio::test]
async fn json_token_page_names_the_account_for_a_live_token() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let user = account(&pool, "alice").await;
    let http = mail_app(pool.clone());
    let token = request_reset(&http, &pool, &user, "nickname").await;
    let response = json_page(&http, &token).await;
    common::assert_answer(&response, StatusCode::OK, "the live JSON page");
    assert_eq!(
        response.json_value(),
        json!({"nickname": "alice"}),
        "{}",
        response.text()
    );
}

/// Unknown, spent and lapsed tokens answer 404 `{"error": "Invalid Token"}`.
#[tokio::test]
async fn json_token_page_answers_404_for_unknown_spent_and_lapsed() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let user = account(&pool, "alice").await;
    let http = mail_app(pool.clone());
    let spent = request_reset(&http, &pool, &user, "nickname").await;
    let lapsed = password_reset::create_for_user(&pool, user.id)
        .await
        .expect("the second token mints")
        .token;
    // Completing deletes the account's other tokens, so the lapsed row is
    // only backdated here; the spend below removes it, and an expired or
    // deleted token must answer the same 404.
    lapse_token(&pool, &lapsed).await;
    let lapsed_answer = json_page(&http, &lapsed).await;
    common::assert_answer(
        &lapsed_answer,
        StatusCode::NOT_FOUND,
        "the lapsed JSON page",
    );
    common::assert_error(&lapsed_answer, "Invalid Token", "the lapsed token");
    let done = complete_json(&http, &spent, NEW_PASSWORD, NEW_PASSWORD).await;
    assert_no_content(&done, "the spending completion");

    for token in ["never-minted".to_owned(), spent, lapsed] {
        let response = json_page(&http, &token).await;
        common::assert_answer(&response, StatusCode::NOT_FOUND, "the invalid JSON page");
        common::assert_error(&response, "Invalid Token", "the invalid token");
    }
}

/// A JSON completion changes the password, spends the token and answers
/// 204; the same token a second time is 422 `Invalid Token`.
#[tokio::test]
async fn json_completion_changes_the_password_and_spends_the_token() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let user = account(&pool, "alice").await;
    let http = mail_app(pool.clone());
    let token = request_reset(&http, &pool, &user, "nickname").await;

    let response = complete_json(&http, &token, NEW_PASSWORD, NEW_PASSWORD).await;
    assert_no_content(&response, "the JSON completion");
    assert!(
        wired_be::models::user::register::authenticate(&pool, "alice", NEW_PASSWORD)
            .await
            .is_some(),
        "the new password logs in"
    );
    assert!(
        wired_be::models::user::register::authenticate(&pool, "alice", OLD_PASSWORD)
            .await
            .is_none(),
        "the old password does not"
    );
    let left = stored_tokens(&pool, &user).await;
    assert!(left.iter().all(|row| row.used), "the token is spent");

    let again = complete_json(
        &http,
        &token,
        "another long password!",
        "another long password!",
    )
    .await;
    common::assert_answer(&again, StatusCode::UNPROCESSABLE_ENTITY, "the replay");
    common::assert_error(&again, "Invalid Token", "the replay");
    assert!(
        wired_be::models::user::register::authenticate(&pool, "alice", NEW_PASSWORD)
            .await
            .is_some(),
        "the replay changed nothing"
    );
}

/// `Accept: application/json` alone (a form body) also gets the JSON
/// answers.
#[tokio::test]
async fn completion_with_a_json_accept_answers_json() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let user = account(&pool, "alice").await;
    let http = mail_app(pool.clone());
    let token = request_reset(&http, &pool, &user, "nickname").await;
    let refused = common::TestRequest::post(RESET)
        .header("accept", "application/json")
        .form(&[
            ("data[token]", token.as_str()),
            ("data[password]", "tiny"),
            ("data[password_confirmation]", "tiny"),
        ])
        .send(&http)
        .await;
    common::assert_answer(&refused, StatusCode::UNPROCESSABLE_ENTITY, "the short form");
    common::assert_error(&refused, "Password is too short", "the short form");
    let done = common::TestRequest::post(RESET)
        .header("accept", "application/json")
        .form(&[
            ("data[token]", token.as_str()),
            ("data[password]", NEW_PASSWORD),
            ("data[password_confirmation]", NEW_PASSWORD),
        ])
        .send(&http)
        .await;
    assert_no_content(&done, "the form with a JSON accept");
}

/// A short password and a mismatch are 422 with their messages; neither
/// changes the password nor spends the token. An unknown token is 422
/// `Invalid Token`.
#[tokio::test]
async fn json_completion_refuses_short_mismatched_and_unknown() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let user = account(&pool, "alice").await;
    let http = mail_app(pool.clone());
    let token = request_reset(&http, &pool, &user, "nickname").await;

    let short = complete_json(&http, &token, "tiny", "tiny").await;
    common::assert_answer(
        &short,
        StatusCode::UNPROCESSABLE_ENTITY,
        "the short password",
    );
    common::assert_error(&short, "Password is too short", "the short password");
    let mismatch = complete_json(&http, &token, NEW_PASSWORD, "something else").await;
    common::assert_answer(&mismatch, StatusCode::UNPROCESSABLE_ENTITY, "the mismatch");
    common::assert_error(&mismatch, "Passwords do not match", "the mismatch");
    let unknown = complete_json(&http, "never-minted", NEW_PASSWORD, NEW_PASSWORD).await;
    common::assert_answer(
        &unknown,
        StatusCode::UNPROCESSABLE_ENTITY,
        "the unknown token",
    );
    common::assert_error(&unknown, "Invalid Token", "the unknown token");

    assert!(
        password_reset::get_valid(&pool, &token, std::time::Duration::from_secs(60))
            .await
            .expect("the lookup runs")
            .is_some(),
        "the refused attempts spent nothing"
    );
    assert!(
        wired_be::models::user::register::authenticate(&pool, "alice", OLD_PASSWORD)
            .await
            .is_some(),
        "the old password still logs in"
    );
}

/// A browser's `Accept` no longer buys HTML: the token check and the form
/// post answer the same JSON, and no answer is `text/html`.
#[tokio::test]
async fn a_browser_accept_still_gets_json() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let user = account(&pool, "alice").await;
    let http = mail_app(pool.clone());
    let token = request_reset(&http, &pool, &user, "nickname").await;
    let browser = "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8";

    let page = common::TestRequest::get(&format!("{RESET}/{token}"))
        .header("accept", browser)
        .send(&http)
        .await;
    common::assert_answer(&page, StatusCode::OK, "the browser token check");
    assert_eq!(page.json_value(), json!({"nickname": "alice"}), "the check");
    let missing = common::TestRequest::get(&format!("{RESET}/never-minted"))
        .header("accept", browser)
        .send(&http)
        .await;
    common::assert_answer(&missing, StatusCode::NOT_FOUND, "the browser invalid check");
    common::assert_error(&missing, "Invalid Token", "the browser invalid check");
    for answer in [&page, &missing] {
        assert!(
            answer
                .header("content-type")
                .is_none_or(|value| !value.contains("text/html")),
            "no HTML answer: {:?}",
            answer.header("content-type")
        );
    }

    let refused = common::TestRequest::post(RESET)
        .header("accept", browser)
        .form(&[
            ("data[token]", token.as_str()),
            ("data[password]", "tiny"),
            ("data[password_confirmation]", "tiny"),
        ])
        .send(&http)
        .await;
    common::assert_answer(
        &refused,
        StatusCode::UNPROCESSABLE_ENTITY,
        "the browser refusal",
    );
    common::assert_error(&refused, "Password is too short", "the browser refusal");
    let done = common::TestRequest::post(RESET)
        .header("accept", browser)
        .form(&[
            ("data[token]", token.as_str()),
            ("data[password]", NEW_PASSWORD),
            ("data[password_confirmation]", NEW_PASSWORD),
        ])
        .send(&http)
        .await;
    assert_no_content(&done, "the browser completion");
}

/// A short password and a mismatching confirmation answer 422 with their
/// message, and neither changes the password nor spends the token.
#[tokio::test]
async fn completion_refuses_short_and_mismatched_passwords() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let user = account(&pool, "alice").await;
    let http = mail_app(pool.clone());
    let token = request_reset(&http, &pool, &user, "nickname").await;

    let short = complete_nested(&http, &token, "tiny", "tiny").await;
    common::assert_answer(&short, StatusCode::UNPROCESSABLE_ENTITY, "the short form");
    common::assert_error(&short, "Password is too short", "the short form");
    let mismatch = complete_nested(&http, &token, NEW_PASSWORD, "something else").await;
    common::assert_answer(&mismatch, StatusCode::UNPROCESSABLE_ENTITY, "the mismatch");
    common::assert_error(&mismatch, "Passwords do not match", "the mismatch");

    assert!(
        password_reset::get_valid(&pool, &token, std::time::Duration::from_secs(60))
            .await
            .expect("the lookup runs")
            .is_some(),
        "the refused attempts spent nothing"
    );
    assert!(
        wired_be::models::user::register::authenticate(&pool, "alice", OLD_PASSWORD)
            .await
            .is_some(),
        "the old password still logs in"
    );
}

/// A used or lapsed token cannot complete the reset: the answer is 422
/// "Invalid Token".
#[tokio::test]
async fn completion_refuses_spent_and_lapsed_tokens() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let user = account(&pool, "alice").await;
    let http = mail_app(pool.clone());
    let lapsed = request_reset(&http, &pool, &user, "nickname").await;
    lapse_token(&pool, &lapsed).await;
    let response = complete_nested(&http, &lapsed, NEW_PASSWORD, NEW_PASSWORD).await;
    common::assert_answer(
        &response,
        StatusCode::UNPROCESSABLE_ENTITY,
        "the lapsed token",
    );
    common::assert_error(&response, "Invalid Token", "the lapsed token");
    assert!(
        wired_be::models::user::register::authenticate(&pool, "alice", OLD_PASSWORD)
            .await
            .is_some(),
        "the lapsed token changed nothing"
    );
}

/// The completion revokes every OAuth token the account holds: a bearer
/// that worked before the reset answers 401 after it.
#[tokio::test]
async fn completion_revokes_the_accounts_oauth_tokens() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let user = account(&pool, "alice").await;
    let bearer = common::create_app_and_token(&pool, &user, SCOPES).await;
    let http = mail_app(pool.clone());
    let before = common::TestRequest::get("/api/v1/accounts/verify_credentials")
        .bearer(&bearer)
        .send(&http)
        .await;
    common::assert_answer(&before, StatusCode::OK, "the token works before");

    let token = request_reset(&http, &pool, &user, "nickname").await;
    let done = complete_nested(&http, &token, NEW_PASSWORD, NEW_PASSWORD).await;
    assert_no_content(&done, "the completion");

    let after = common::TestRequest::get("/api/v1/accounts/verify_credentials")
        .bearer(&bearer)
        .send(&http)
        .await;
    common::assert_answer(&after, StatusCode::UNAUTHORIZED, "the token is revoked");
}

/// An account sitting on a forced password reset cannot take the password
/// grant: 403 `password_reset_required` — until completing a reset clears
/// the flag, after which the same grant succeeds.
#[tokio::test]
async fn pending_reset_blocks_login_until_the_reset_completes() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let user = account(&pool, "alice").await;
    let app = common::create_app(&pool, SCOPES).await;
    set_pending(&pool, &user, true).await;
    let http = mail_app(pool.clone());

    let blocked = grant(&http, &grant_body(&app, "alice", OLD_PASSWORD)).await;
    common::assert_answer(&blocked, StatusCode::FORBIDDEN, "the pending grant");
    assert_eq!(
        blocked.json_value(),
        json!({
            "error": "Password reset is required",
            "identifier": "password_reset_required",
        }),
        "the refusal body: {}",
        blocked.text()
    );

    let token = request_reset(&http, &pool, &user, "nickname").await;
    let done = complete_nested(&http, &token, NEW_PASSWORD, NEW_PASSWORD).await;
    assert_no_content(&done, "the completion");
    let after = grant(&http, &grant_body(&app, "alice", NEW_PASSWORD)).await;
    common::assert_answer(&after, StatusCode::OK, "the grant works again");
    let row = wired_be::models::user::get_by_id(&pool, user.id)
        .await
        .expect("the account reads back")
        .expect("the account exists");
    assert!(
        !row.password_reset_pending.get(),
        "the completion cleared the flag"
    );
}

/// The request endpoint counts into the `password_reset` bucket: the
/// limited config decorates the 204s with the `x-ratelimit-*` headers and
/// refuses over the budget with 429.
#[tokio::test]
async fn the_request_endpoint_counts_the_password_reset_bucket() {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    let db = common::test_db().await;
    let pool = db.pool().clone();
    let config = common::test_config_with_rate_limits("password_reset=2:600");
    let app = common::test_app_with_config(pool, config);
    let peer = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)), 40_007);
    let first = common::TestRequest::post(REQUEST)
        .form(&[("email", "anyone@example.org")])
        .peer(peer)
        .send(&app)
        .await;
    assert_no_content(&first, "the counted request");
    assert_eq!(first.header("x-ratelimit-limit"), Some("2"), "the budget");
    assert_eq!(
        first.header("x-ratelimit-remaining"),
        Some("1"),
        "one request spent of two"
    );
    let _second = common::TestRequest::post(REQUEST)
        .form(&[("email", "anyone@example.org")])
        .peer(peer)
        .send(&app)
        .await;
    let third = common::TestRequest::post(REQUEST)
        .form(&[("email", "anyone@example.org")])
        .peer(peer)
        .send(&app)
        .await;
    common::assert_answer(&third, StatusCode::TOO_MANY_REQUESTS, "over the budget");
    common::assert_error(&third, "Throttled", "the throttle body");
}
