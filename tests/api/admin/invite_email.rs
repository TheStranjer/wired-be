//! The emailed invite and the registration link: `POST
//! /users/email_invite` minting the one-time invite, queueing the
//! `user_invitation` letter carrying the
//! `{FRONTEND_BASE_URL}/registration/{token}` link and logging the create —
//! and refusing Pleroma's 400 without the `invites_enabled` setting or the
//! address — beside the 404 the backend answers on `/registration/{token}`.

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;
use wired_be::config::{Config, Toggle};

use super::invites::{
    INVITE_EMAIL, INVITES_DISABLED, MISSING_EMAIL, REGISTRATION, invite_admin, post_invite,
    stored_invites,
};
use super::support::{LOG, admin};
use crate::common::{self, TestRequest};

/// A [`Config`] with invites on and mail up: the gate opens, and
/// `deliver_later` really queues the letter.
fn invites_open_config() -> Config {
    let mut config = common::test_config();
    config.invites_enabled = Toggle::new(true);
    config.smtp_host = Some("smtp.example.org".to_owned());
    config
}

/// The queued `mailer.send` jobs' arguments, oldest first.
async fn queued_letters(pool: &PgPool) -> Vec<Value> {
    sqlx::query_scalar::<_, Value>(
        "SELECT args FROM jobs WHERE queue = 'mailer' AND kind = 'mailer.send' ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .expect("the mailer queue reads back")
}

/// The `POST /users/email_invite` success: 204 with an empty body, a
/// one-time invite in the table, the `user_invitation` letter on the
/// mailer queue carrying the `{FRONTEND_BASE_URL}/registration/{token}` link —
/// and the `invite_create` log line it records. The optional `name` is
/// accepted (Pleroma's recipient display name; this mailer addresses
/// bare recipients).
#[tokio::test]
async fn email_invite_letters_the_registration_link() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app_with_config(pool.clone(), invites_open_config());
    let (_, token) = admin(&pool, "invitemailer", "admin:read admin:write").await;

    let sent = post_invite(
        &app,
        &token,
        INVITE_EMAIL,
        json!({"email": "friend@example.org", "name": "Friend"}),
    )
    .await;
    common::assert_answer(&sent, StatusCode::NO_CONTENT, "the emailed invite");
    assert!(sent.body.is_empty(), "the 204 body is empty");

    let stored = stored_invites(&pool).await;
    assert_eq!(stored.len(), 1, "the letter mints one invite");
    let (invite_id, invite_token, kind) = &stored[0];
    assert_eq!(kind, "one_time", "and it is a one-time invite");

    let letters = queued_letters(&pool).await;
    assert_eq!(letters.len(), 1, "one queued send");
    assert_eq!(
        letters[0]["to"],
        json!("friend@example.org"),
        "the bare addressed recipient"
    );
    assert_eq!(
        letters[0]["subject"],
        json!("Invitation to Wired"),
        "the instance-named subject"
    );
    assert_eq!(
        letters[0]["text_body"],
        json!(format!(
            "You are invited to Wired.\n\n\
             Open the following link to register:\n\n\
             https://test.example/registration/{invite_token}\n"
        )),
        "the letter with its registration link (the frontend defaults to BASE_URL)"
    );
    assert_eq!(letters[0]["html_body"], Value::Null, "text only");

    let logged = TestRequest::get(&format!("{LOG}?search=created+invite"))
        .bearer(&token)
        .send(&app)
        .await;
    let page = logged.json_value();
    assert_eq!(page["total"], json!(1), "the emailed create is logged");
    assert_eq!(
        page["items"][0]["message"],
        format!("@invitemailer created invite: {invite_id}"),
        "naming the invite it minted"
    );
}

/// The `POST /users/email_invite` refusals: with `INVITES_ENABLED` off —
/// the suite default — the endpoint answers Pleroma's 400 and mints
/// nothing; with it on, a body without an address (or with only blanks)
/// is the 400 the ticket pins, still minting nothing.
#[tokio::test]
async fn email_invite_refuses_without_the_setting_or_the_address() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let closed = common::test_app(pool.clone());
    let token = invite_admin(&pool, "invitegate").await;

    let disabled = post_invite(
        &closed,
        &token,
        INVITE_EMAIL,
        json!({"email": "friend@example.org"}),
    )
    .await;
    common::assert_all_refused(
        &[disabled],
        StatusCode::BAD_REQUEST,
        INVITES_DISABLED,
        "the mail invite with invites_enabled off",
    );
    assert!(
        stored_invites(&pool).await.is_empty(),
        "the refusal minted no invite"
    );

    let open = common::test_app_with_config(pool.clone(), invites_open_config());
    let missing = post_invite(&open, &token, INVITE_EMAIL, json!({"name": "Friend"})).await;
    common::assert_all_refused(
        &[missing],
        StatusCode::BAD_REQUEST,
        MISSING_EMAIL,
        "the mail invite with no email parameter",
    );
    let blank = post_invite(&open, &token, INVITE_EMAIL, json!({"email": "   "})).await;
    common::assert_all_refused(
        &[blank],
        StatusCode::BAD_REQUEST,
        MISSING_EMAIL,
        "the mail invite with a blank address",
    );
    assert!(
        stored_invites(&pool).await.is_empty(),
        "and neither refusal minted an invite"
    );
}

/// With a `FRONTEND_BASE_URL` apart from `BASE_URL` the delivered letter
/// links to the frontend origin — proving which origin is used — and
/// never names the backend's own.
#[tokio::test]
async fn the_letter_links_to_the_frontend_origin() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let mut split = invites_open_config();
    split.frontend_base_url = "https://fe.example".to_owned();
    let app = common::test_app_with_config(pool.clone(), split);
    let (_, token) = admin(&pool, "invitefrontend", "admin:read admin:write").await;

    let sent = post_invite(
        &app,
        &token,
        INVITE_EMAIL,
        json!({"email": "friend@example.org"}),
    )
    .await;
    common::assert_answer(&sent, StatusCode::NO_CONTENT, "the emailed invite");

    let stored = stored_invites(&pool).await;
    let (_, invite_token, _) = &stored[0];
    let letters = queued_letters(&pool).await;
    assert_eq!(letters.len(), 1, "one queued send");
    let body = letters[0]["text_body"]
        .as_str()
        .expect("the letter has a text body");
    assert!(
        body.contains(&format!("https://fe.example/registration/{invite_token}")),
        "the frontend origin carries the link: {body}"
    );
    assert!(
        !body.contains("https://test.example"),
        "and the backend origin does not: {body}"
    );
}

/// The backend serves no `/registration/{token}` route: the request is the
/// app-wide 404, marked with `x-wired-fallback: 1`.
#[tokio::test]
async fn the_registration_path_is_the_app_wide_404() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());

    let response = TestRequest::get(&format!("{REGISTRATION}/x"))
        .send(&app)
        .await;
    common::assert_answer(&response, StatusCode::NOT_FOUND, "the removed link route");
    assert_eq!(
        response.header("x-wired-fallback"),
        Some("1"),
        "no handler is routed here"
    );
}
