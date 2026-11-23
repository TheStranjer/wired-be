//! The registration modes over HTTP: `REGISTRATIONS_APPROVAL_REQUIRED` and
//! `ACCOUNT_ACTIVATION_REQUIRED` end to end.
//!
//! `POST /api/v1/accounts` answers a finished registration one of three ways
//! (Pleroma's `AccountController.create`, which reads the same two config
//! keys off `User.account_status`): the token JSON of a ready account, 200
//! `{"message":"You have been registered. You'll be able to log in once your
//! account is approved.","identifier":"awaiting_approval"}`, or 200
//! `{"message":"You have been registered. Please check your email for further
//! instructions.","identifier":"missing_confirmed_email"}`. Both modes on at
//! once report the approval. Neither the approval nor the confirmation hands
//! out a credential.
//!
//! The waits then close the login door: `check_account_ready` refuses the
//! password grant, the authorization submission and the refresh grant with
//! 403 and the same identifier, and a credential that already exists for an
//! unapproved account fails the bearer extractor with the same refusal.
//!
//! `GET /api/account/confirm_email/{user_id}/{token}` confirms the address and
//! redirects to `{BASE_URL}/` — the same 302 whether or not the pairing was
//! right — and `POST /api/v1/pleroma/accounts/confirmation_resend` re-mails an
//! unconfirmed account and answers 204 whatever it was asked, so it cannot
//! probe the address book. The instance endpoints and `NodeInfo` report both
//! modes.
//!
//! Mail is queued, never sent, inside a request (decision 11 and
//! [`wired_be::mailer`]), so these tests read the `mailer` queue's job rows:
//! what the endpoints actually write.

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::{Config, Toggle},
    models::{
        invite_token::{self, InviteParams},
        oauth_app::OauthApp,
        user::{self, Flag, NewUser, User},
    },
    util::password::hash_password,
};

use crate::common;

/// The instance origin [`common::test_config`] serves on.
const BASE: &str = "https://test.example";
/// The password every registration and login in this file submits.
const PASSWORD: &str = "correct horse battery staple";
/// The scopes the registration fixtures' apps hold: what a Mastodon client
/// ends up with after `/api/v1/apps`.
const FULL_SCOPES: &str = "read write follow push";
/// The registration endpoint.
const REGISTER: &str = "/api/v1/accounts";
/// The confirmation-resend endpoint.
const RESEND: &str = "/api/v1/pleroma/accounts/confirmation_resend";
/// The approval message `POST /api/v1/accounts` answers with.
const APPROVAL_MESSAGE: &str =
    "You have been registered. You'll be able to log in once your account is approved.";
/// The confirmation message `POST /api/v1/accounts` answers with.
const CONFIRMATION_MESSAGE: &str =
    "You have been registered. Please check your email for further instructions.";
/// The approval refusal a login attempt draws.
const APPROVAL_ERROR: &str = "Your account is awaiting approval.";
/// The confirmation refusal a login attempt draws.
const CONFIRMATION_ERROR: &str = "Your login is missing a confirmed e-mail address";
/// The redirect target both halves of the confirmation flow answer with.
const HOME: &str = "https://test.example/";

/// A [`Config`] in registration mode: the two toggles as named, with
/// `SMTP_HOST` set so a confirmation letter really reaches the queue. The
/// modes are `Toggle`s, not `bool`s, because Clippy's
/// `fn_params_excessive_bools` refuses a helper that takes two of them.
fn mode_config(approval: Toggle, activation: Toggle) -> Config {
    Config {
        registrations_approval_required: approval,
        account_activation_required: activation,
        smtp_host: Some("smtp.example.org".to_owned()),
        ..common::test_config()
    }
}

/// The application of an instance in the two modes `approval` and
/// `activation`.
fn mode_app(pool: &PgPool, approval: Toggle, activation: Toggle) -> Router {
    common::test_app_with_config(pool.clone(), mode_config(approval, activation))
}

/// A complete, valid registration body for `username`.
fn body_for(username: &str) -> Value {
    json!({
        "username": username,
        "email": format!("{username}@example.org"),
        "password": PASSWORD,
        "agreement": true,
    })
}

/// Registers `username` on `app` with [`body_for`], over a fresh app-only
/// bearer.
async fn register(app: &Router, pool: &PgPool, username: &str) -> common::TestResponse {
    let bearer = common::create_app_only_token(pool, FULL_SCOPES).await;
    common::TestRequest::post(REGISTER)
        .bearer(&bearer)
        .json(&body_for(username))
        .send(app)
        .await
}

/// The stored row of `username`, for the proofs about what a registration
/// actually wrote.
async fn stored(username: &str, pool: &PgPool) -> User {
    user::get_by_nickname(pool, username)
        .await
        .expect("the lookup reaches the database")
        .unwrap_or_else(|| panic!("no account {username} was registered"))
}

/// The queued letters on the mailer queue, oldest first, as their JSON
/// arguments.
async fn queued_letters(pool: &PgPool) -> Vec<Value> {
    sqlx::query_as::<_, (Value,)>("SELECT args FROM jobs WHERE queue = $1 ORDER BY inserted_at")
        .bind(wired_be::jobs::QUEUE_MAILER)
        .fetch_all(pool)
        .await
        .expect("the job read runs")
        .into_iter()
        .map(|(args,)| args)
        .collect()
}

/// Asserts the 200 `{"message", "identifier"}` body of a pending
/// registration: those two keys and nothing that smells like a credential.
fn assert_pending_answer(response: &common::TestResponse, message: &str, identifier: &str) {
    common::assert_answer(response, StatusCode::OK, "a pending registration");
    let body = response.json_value();
    assert_eq!(body["message"], message, "the human-facing message");
    assert_eq!(
        body["identifier"], identifier,
        "the machine-readable reason"
    );
    assert_eq!(
        body.as_object().expect("an object").len(),
        2,
        "message and identifier only, no token beside them: {body}"
    );
}

/// The OAuth `password` grant body for `username`, over `app`'s credentials.
fn grant_body(app: &OauthApp, username: &str) -> Value {
    json!({
        "grant_type": "password",
        "username": username,
        "password": PASSWORD,
        "scope": "read",
        "client_id": app.client_id,
        "client_secret": app.client_secret,
    })
}

/// A local account named `nickname` that logs in with [`PASSWORD`] and whose
/// account-status flags `customize` sets.
async fn account_with_status<F>(pool: &PgPool, nickname: &str, customize: F) -> User
where
    F: FnOnce(NewUser) -> NewUser,
{
    common::create_local_user_with(pool, nickname, |new| {
        customize(NewUser {
            email: Some(format!("{nickname}@example.org")),
            password_hash: Some(
                hash_password(PASSWORD).expect("password hashing succeeds for fixtures"),
            ),
            ..new
        })
    })
    .await
}

/// Asserts the 403 `{"error", "identifier"}` refusal of a login the account
/// is not ready for.
fn assert_login_refused(response: &common::TestResponse, error: &str, identifier: &str) {
    assert_eq!(
        response.status,
        StatusCode::FORBIDDEN,
        "an unready account is a 403, not the invalid-credentials 400"
    );
    let body = response.json_value();
    assert_eq!(body["error"], error, "Pleroma's message");
    assert_eq!(body["identifier"], identifier, "and its reason code");
    assert_eq!(
        body.as_object().expect("an object").len(),
        2,
        "exactly the two keys: {body}"
    );
}

/// An approval-gated instance registers the account awaiting staff, tells the
/// applicant so, and hands out no credentials: the row is active and
/// confirmed, only approval is pending, and nothing was queued.
#[tokio::test]
async fn approval_mode_registers_a_pending_account_without_a_token() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = mode_app(&pool, Toggle::new(true), Toggle::new(false));

    let response = register(&app, &pool, "awaiting").await;
    assert_pending_answer(&response, APPROVAL_MESSAGE, "awaiting_approval");

    let row = stored("awaiting", &pool).await;
    assert!(row.approval_pending.get(), "the account awaits staff");
    assert!(row.is_active.get(), "approval does not deactivate it");
    assert!(
        row.email_confirmed.get(),
        "an approval-only instance confirms nothing"
    );
    assert!(
        row.confirmation_token.is_none(),
        "and mints no confirmation token"
    );
    assert!(
        queued_letters(&pool).await.is_empty(),
        "approval queues no letter here: the staff notices are the admin ticket"
    );
}

/// A confirmation-gated instance registers the account unconfirmed with a
/// token, queues the letter whose link names the account and the token, and
/// hands out no credentials.
#[tokio::test]
async fn activation_mode_registers_an_unconfirmed_account_and_queues_the_letter() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = mode_app(&pool, Toggle::new(false), Toggle::new(true));

    let response = register(&app, &pool, "confirmme").await;
    assert_pending_answer(&response, CONFIRMATION_MESSAGE, "missing_confirmed_email");

    let row = stored("confirmme", &pool).await;
    assert!(!row.email_confirmed.get(), "the address is not confirmed");
    assert!(
        !row.approval_pending.get(),
        "confirmation does not ask staff"
    );
    assert!(row.is_active.get(), "the row is active, merely waiting");
    let token = row.confirmation_token.clone().expect("a token was minted");
    assert_eq!(token.len(), 43, "32 CSPRNG bytes, base64url");
    assert!(row.confirmation_sent_at.is_some(), "the send was stamped");

    let letters = queued_letters(&pool).await;
    assert_eq!(letters.len(), 1, "exactly one letter was queued");
    assert_eq!(
        letters[0]["to"], "confirmme@example.org",
        "addressed to the new account"
    );
    let body = letters[0]["text_body"].as_str().expect("a text body");
    assert!(
        body.contains(&format!(
            "{BASE}/api/account/confirm_email/{}/{}",
            row.id, token
        )),
        "the letter carries the absolute confirmation link: {body}"
    );
}

/// Both modes at once: the account waits on both, the answer reports the
/// approval (the wait that ends last), and the letter still goes out.
#[tokio::test]
async fn both_modes_report_approval_first_and_still_queue_the_letter() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = mode_app(&pool, Toggle::new(true), Toggle::new(true));

    let response = register(&app, &pool, "bothwaits").await;
    assert_pending_answer(&response, APPROVAL_MESSAGE, "awaiting_approval");

    let row = stored("bothwaits", &pool).await;
    assert!(row.approval_pending.get(), "staff must answer");
    assert!(!row.email_confirmed.get(), "and the mailbox too");
    assert!(
        row.confirmation_token.is_some(),
        "the token is minted even though approval speaks first"
    );
    assert_eq!(
        queued_letters(&pool).await.len(),
        1,
        "the confirmation letter was queued all the same"
    );
}

/// An invite is a staff decision made in advance: the invited account skips
/// the approval wait and gets the plain token answer, as before the mode.
#[tokio::test]
async fn an_invited_registration_skips_the_approval_answer() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let mut config = mode_config(Toggle::new(true), Toggle::new(false));
    config.registrations_open = Toggle::new(false);
    config.invites_enabled = Toggle::new(true);
    let app = common::test_app_with_config(pool.clone(), config);
    let invite = invite_token::create(&pool, &InviteParams::one_time())
        .await
        .expect("the invite mints");
    let bearer = common::create_app_only_token(&pool, FULL_SCOPES).await;

    let mut body = body_for("invitedready");
    let _inserted = body
        .as_object_mut()
        .expect("`body_for` builds an object")
        .insert("token".to_owned(), Value::String(invite.token.clone()));

    let response = common::TestRequest::post(REGISTER)
        .bearer(&bearer)
        .json(&body)
        .send(&app)
        .await;

    assert_eq!(response.status, StatusCode::OK, "the invite registers");
    let answer = response.json_value();
    assert!(
        answer["access_token"].is_string(),
        "an invited account gets its credentials: {answer}"
    );
    assert!(
        !stored("invitedready", &pool).await.approval_pending.get(),
        "and does not await approval"
    );
}

/// A confirmation-gated instance that lets registration omit the email
/// cannot hold that account hostage: with no address to confirm it lands
/// confirmed and gets its token.
#[tokio::test]
async fn activation_without_an_address_lands_ready() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let mut config = mode_config(Toggle::new(false), Toggle::new(true));
    config.accounts_email_optional = true;
    let app = common::test_app_with_config(pool.clone(), config);
    let bearer = common::create_app_only_token(&pool, FULL_SCOPES).await;

    let response = common::TestRequest::post(REGISTER)
        .bearer(&bearer)
        .json(&json!({
            "username": "noaddress",
            "password": PASSWORD,
            "agreement": true,
        }))
        .send(&app)
        .await;

    assert_eq!(response.status, StatusCode::OK, "it registers");
    assert!(
        response.json_value()["access_token"].is_string(),
        "with credentials, because nothing waits"
    );
    let row = stored("noaddress", &pool).await;
    assert!(row.email_confirmed.get(), "there was no address to confirm");
    assert!(
        queued_letters(&pool).await.is_empty(),
        "and nothing to send"
    );
}

/// The password grant refuses both waits with their own identifier, and an
/// account that is waiting for nothing logs in as before.
#[tokio::test]
async fn the_password_grant_refuses_an_unready_account() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let unapproved = account_with_status(&pool, "gate-approve", |new| NewUser {
        approval_pending: Flag::new(true),
        ..new
    })
    .await;
    let unconfirmed = account_with_status(&pool, "gate-confirm", |new| NewUser {
        email_confirmed: Flag::new(false),
        ..new
    })
    .await;
    let ready = common::create_user_with_password(&pool, "gate-ready", PASSWORD).await;
    assert!(
        !unconfirmed.email_confirmed.get() && unconfirmed.is_active.get(),
        "the fixture is an active account with an unconfirmed address"
    );
    let app = common::create_app(&pool, "read").await;
    let http = common::test_app(pool.clone());

    let refused = common::TestRequest::post("/oauth/token")
        .json(&grant_body(&app, "gate-approve"))
        .send(&http)
        .await;
    assert_login_refused(&refused, APPROVAL_ERROR, "awaiting_approval");
    let declined = common::TestRequest::post("/oauth/token")
        .json(&grant_body(&app, "gate-confirm"))
        .send(&http)
        .await;
    assert_login_refused(&declined, CONFIRMATION_ERROR, "missing_confirmed_email");

    let allowed = common::TestRequest::post("/oauth/token")
        .json(&grant_body(&app, "gate-ready"))
        .send(&http)
        .await;
    assert_eq!(
        allowed.status,
        StatusCode::OK,
        "a plain account still logs in"
    );
    assert_eq!(allowed.json_value()["me"], ready.ap_id, "for itself");
    assert_ne!(
        allowed.json_value()["me"],
        unapproved.ap_id,
        "and never for a peer"
    );
}

/// Deactivation outranks both waits, exactly as Pleroma's `account_status`
/// orders its clauses.
#[tokio::test]
async fn deactivation_outranks_the_registration_modes() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let waiting = account_with_status(&pool, "all-three", |new| NewUser {
        is_active: Flag::new(false),
        approval_pending: Flag::new(true),
        email_confirmed: Flag::new(false),
        ..new
    })
    .await;
    assert!(
        !waiting.is_active.get() && waiting.approval_pending.get(),
        "the fixture waits for all three at once"
    );
    let app = common::create_app(&pool, "read").await;
    let http = common::test_app(pool);

    let response = common::TestRequest::post("/oauth/token")
        .json(&grant_body(&app, "all-three"))
        .send(&http)
        .await;
    assert_login_refused(
        &response,
        "Your account is currently disabled",
        "account_is_disabled",
    );
}

/// The authorization submission runs the same gate: a good password for a
/// pending account is a 403 with the identifier, never a code.
#[tokio::test]
async fn the_authorization_endpoint_refuses_an_unready_account() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let pending = account_with_status(&pool, "consent-pending", |new| NewUser {
        approval_pending: Flag::new(true),
        ..new
    })
    .await;
    let unconfirmed = account_with_status(&pool, "consent-unconfirmed", |new| NewUser {
        email_confirmed: Flag::new(false),
        ..new
    })
    .await;
    assert!(
        pending.approval_pending.get() && !unconfirmed.email_confirmed.get(),
        "both consent fixtures are the accounts the gate must stop"
    );
    let app = common::create_app(&pool, "read").await;
    let http = common::test_app(pool);

    let refusals = [
        ("consent-pending", APPROVAL_ERROR, "awaiting_approval"),
        (
            "consent-unconfirmed",
            CONFIRMATION_ERROR,
            "missing_confirmed_email",
        ),
    ];
    for (name, error, identifier) in refusals {
        let response = common::TestRequest::post("/oauth/authorize")
            .form(&[
                ("authorization[client_id]", app.client_id.as_str()),
                (
                    "authorization[redirect_uri]",
                    "https://client.example/callback",
                ),
                ("authorization[name]", name),
                ("authorization[password]", PASSWORD),
                ("authorization[scope][]", "read"),
            ])
            .send(&http)
            .await;
        assert_eq!(
            response.status,
            StatusCode::FORBIDDEN,
            "{name} is refused a code"
        );
        assert_login_refused(&response, error, identifier);
    }
}

/// The refresh grant runs the gate too: an unapproved account cannot rotate
/// its way back into a live credential, and the old pair survives the
/// refusal. An unknown refresh token keeps the shared 400.
#[tokio::test]
async fn the_refresh_grant_refuses_an_unready_account() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let unapproved = account_with_status(&pool, "rotate-pending", |new| NewUser {
        approval_pending: Flag::new(true),
        ..new
    })
    .await;
    let app = common::create_app(&pool, "read write").await;
    let pair = common::create_token(&pool, &unapproved, &app, "read").await;
    let http = common::test_app(pool.clone());

    let response = common::TestRequest::post("/oauth/token")
        .json(&json!({
            "grant_type": "refresh_token",
            "refresh_token": pair.refresh_token,
            "client_id": app.client_id,
            "client_secret": app.client_secret,
        }))
        .send(&http)
        .await;
    assert_login_refused(&response, APPROVAL_ERROR, "awaiting_approval");
    let standing: i64 =
        sqlx::query_scalar("SELECT count(*) FROM oauth_tokens WHERE refresh_token = $1")
            .bind(&pair.refresh_token)
            .fetch_one(&pool)
            .await
            .expect("the lookup runs");
    assert_eq!(
        standing, 1,
        "a refused rotation rolls back and leaves the old pair standing"
    );

    let unknown = common::TestRequest::post("/oauth/token")
        .json(&json!({
            "grant_type": "refresh_token",
            "refresh_token": "never-minted",
            "client_id": app.client_id,
            "client_secret": app.client_secret,
        }))
        .send(&http)
        .await;
    common::assert_error(&unknown, "Invalid credentials", "an unknown refresh token");
}

/// A credential that already exists is worth nothing to an unapproved
/// account; an unconfirmed one still works, because the mode refuses a
/// *login* and not a bearer that already holds a token.
#[tokio::test]
async fn an_existing_token_of_an_unapproved_account_is_refused() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let unapproved = account_with_status(&pool, "bearer-pending", |new| NewUser {
        approval_pending: Flag::new(true),
        ..new
    })
    .await;
    let still_usable = account_with_status(&pool, "bearer-unconfirmed", |new| NewUser {
        email_confirmed: Flag::new(false),
        ..new
    })
    .await;
    let pending_token = common::create_app_and_token(&pool, &unapproved, FULL_SCOPES).await;
    let unconfirmed_token = common::create_app_and_token(&pool, &still_usable, FULL_SCOPES).await;
    let http = common::test_app(pool);

    let refused = common::TestRequest::get("/api/v1/accounts/verify_credentials")
        .bearer(&pending_token)
        .send(&http)
        .await;
    assert_login_refused(&refused, APPROVAL_ERROR, "awaiting_approval");

    let allowed = common::TestRequest::get("/api/v1/accounts/verify_credentials")
        .bearer(&unconfirmed_token)
        .send(&http)
        .await;
    assert_eq!(
        allowed.status,
        StatusCode::OK,
        "an unconfirmed account keeps its live credential: the mode gates logins"
    );
    assert_eq!(
        allowed.json_value()["username"],
        "bearer-unconfirmed",
        "as itself"
    );
}

/// A valid confirmation link confirms the address, clears the token and the
/// send stamp, and redirects to the instance root — after which the account
/// can log in.
#[tokio::test]
async fn a_valid_confirmation_link_activates_the_account() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = mode_app(&pool, Toggle::new(false), Toggle::new(true));
    let _registered = register(&app, &pool, "linkable").await;
    let row = stored("linkable", &pool).await;
    let token = row
        .confirmation_token
        .clone()
        .expect("the registration minted a token");

    let response =
        common::TestRequest::get(&format!("/api/account/confirm_email/{}/{}", row.id, token))
            .send(&app)
            .await;

    assert_eq!(response.status, StatusCode::FOUND, "the link redirects");
    assert_eq!(
        response.header("location"),
        Some(HOME),
        "to the instance root, where the frontend would be"
    );
    let confirmed = stored("linkable", &pool).await;
    assert!(confirmed.email_confirmed.get(), "the address is confirmed");
    assert!(
        confirmed.confirmation_token.is_none(),
        "the used token is gone from the row"
    );
    assert!(
        confirmed.confirmation_sent_at.is_none(),
        "and so is its send stamp"
    );

    let oauth_app = common::create_app(&pool, "read").await;
    let login = common::TestRequest::post("/oauth/token")
        .json(&grant_body(&oauth_app, "linkable"))
        .send(&common::test_app(pool))
        .await;
    assert_eq!(login.status, StatusCode::OK, "and the account logs in now");
}

/// Every wrong pairing redirects just the same and confirms nothing: the
/// link cannot be used to learn whether a token was right.
#[tokio::test]
async fn an_invalid_confirmation_link_redirects_without_confirming() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = mode_app(&pool, Toggle::new(false), Toggle::new(true));
    let _registered = register(&app, &pool, "stubborn").await;
    let row = stored("stubborn", &pool).await;

    let dead_links = [
        (
            "a forged token",
            format!("/api/account/confirm_email/{}/not-the-token", row.id),
        ),
        (
            "another account's link",
            format!(
                "/api/account/confirm_email/{}/{}",
                Uuid::now_v7(),
                row.confirmation_token.as_deref().unwrap_or_default()
            ),
        ),
        (
            "a malformed account id",
            "/api/account/confirm_email/not-a-uuid/anything".to_owned(),
        ),
    ];
    for (label, path) in dead_links {
        let response = common::TestRequest::get(&path).send(&app).await;
        assert_eq!(response.status, StatusCode::FOUND, "{label} redirects");
        assert_eq!(
            response.header("location"),
            Some(HOME),
            "{label} lands on the instance root"
        );
    }

    let still_pending = stored("stubborn", &pool).await;
    assert!(
        !still_pending.email_confirmed.get(),
        "none of them confirmed the address"
    );
    assert!(
        still_pending.confirmation_token.is_some(),
        "and the real token is still there, waiting for its next visit"
    );
}

/// Posts a confirmation resend for `email` and hands back the answer.
async fn resend(app: &Router, email: &str) -> common::TestResponse {
    common::TestRequest::post(RESEND)
        .json(&json!({ "email": email }))
        .send(app)
        .await
}

/// Visits the confirmation link of `user_id` carrying `token`.
async fn visit_link(app: &Router, user_id: Uuid, token: &str) -> common::TestResponse {
    let path = format!("/api/account/confirm_email/{user_id}/{token}");
    common::TestRequest::get(&path).send(app).await
}

/// The resend answers 204 with an empty body for every request it gets: the
/// pending address, its case-folded spelling, a confirmed one, an unknown
/// one and no address at all, so it cannot probe the roster. Only the
/// pending account's own asks queue a letter.
#[tokio::test]
async fn confirmation_resend_always_answers_no_content() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = mode_app(&pool, Toggle::new(false), Toggle::new(true));
    let _resent_me = register(&app, &pool, "resendme").await;
    let _settled_once = register(&app, &pool, "settled").await;
    let settled = stored("settled", &pool).await;
    let settled = confirm_directly(&pool, &settled).await;
    assert!(
        settled.email_confirmed.get() && settled.confirmation_token.is_none(),
        "the settled fixture has already been through the flow"
    );
    let letters_before = queued_letters(&pool).await.len();

    let asks = [
        ("the pending address", "resendme@example.org"),
        ("the case-folded address", "RESENDME@example.org"),
        ("a confirmed address", "settled@example.org"),
        ("an unknown address", "nobody@example.org"),
    ];
    let mut answers = Vec::new();
    for (label, email) in asks {
        let response = resend(&app, email).await;
        assert_eq!(
            response.status,
            StatusCode::NO_CONTENT,
            "{label} gets the same 204"
        );
        assert_eq!(
            response.text(),
            "",
            "the 204 carries an empty body, so it leaks nothing about {label}"
        );
        answers.push(response);
    }
    let silent = common::TestRequest::post(RESEND)
        .json(&json!({}))
        .send(&app)
        .await;
    assert_eq!(
        silent.status,
        StatusCode::NO_CONTENT,
        "no address is still 204"
    );
    answers.push(silent);
    common::assert_all_answered(&answers, StatusCode::NO_CONTENT, "the resends");

    assert_eq!(
        queued_letters(&pool).await.len(),
        letters_before + 2,
        "one letter per spelling of the pending address, none for the other asks"
    );
    assert!(
        !stored("resendme", &pool).await.email_confirmed.get(),
        "and no answer confirms anything"
    );
}

/// The resend re-mails the unconfirmed account with a freshly minted token:
/// the old link dies with the rotation, the new one confirms, and the letter
/// carries the account's own address.
#[tokio::test]
async fn confirmation_resend_rotates_the_token_it_remails() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = mode_app(&pool, Toggle::new(false), Toggle::new(true));
    let _registered = register(&app, &pool, "rotator").await;
    let pending = stored("rotator", &pool).await;
    let old_token = pending
        .confirmation_token
        .clone()
        .expect("the registration minted a token");
    let letters_before = queued_letters(&pool).await.len();

    let _pending_ask = resend(&app, "rotator@example.org").await;
    let _folded_ask = resend(&app, "ROTATOR@example.org").await;

    let row = stored("rotator", &pool).await;
    let fresh = row.confirmation_token.clone().expect("a fresh token");
    assert_ne!(fresh, old_token, "the letter's token was rotated");
    assert!(
        row.confirmation_sent_at.is_some(),
        "and its send was stamped"
    );
    assert!(!row.email_confirmed.get(), "a resend confirms nothing");
    let letters = queued_letters(&pool).await;
    assert_eq!(
        letters.len(),
        letters_before + 2,
        "one letter per ask that named the pending account, case aside"
    );
    assert_eq!(
        letters.last().expect("the last letter")["to"],
        "rotator@example.org",
        "and it went to the pending account"
    );

    let stale = visit_link(&app, row.id, &old_token).await;
    assert_eq!(
        stale.status,
        StatusCode::FOUND,
        "the superseded link redirects"
    );
    assert!(
        !stored("rotator", &pool).await.email_confirmed.get(),
        "and confirms nothing"
    );
    let live = visit_link(&app, row.id, &fresh).await;
    assert_eq!(live.status, StatusCode::FOUND, "the fresh link redirects");
    assert!(
        stored("rotator", &pool).await.email_confirmed.get(),
        "and it is the one that confirms"
    );
}

/// Confirms an account's address straight through the database: the state a
/// resend finds when the account already went through the flow.
async fn confirm_directly(pool: &PgPool, account: &User) -> User {
    let _confirmed = sqlx::query(
        "UPDATE users SET email_confirmed = true, confirmation_token = NULL, updated_at = now() \
         WHERE id = $1",
    )
    .bind(account.id)
    .execute(pool)
    .await
    .expect("the confirmation fixture runs");
    user::get_by_id(pool, account.id)
        .await
        .expect("the read runs")
        .expect("the account exists")
}

/// Every instance surface reports both modes, each in its own spelling, and
/// the default instance still reports neither.
#[tokio::test]
async fn the_instance_endpoints_and_nodeinfo_report_the_modes() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let on = common::test_app_with_config(
        pool.clone(),
        mode_config(Toggle::new(true), Toggle::new(true)),
    );
    let v1 = common::TestRequest::get("/api/v1/instance")
        .send(&on)
        .await
        .json_value();
    assert_eq!(
        v1["approval_required"], true,
        "GET /api/v1/instance reports REGISTRATIONS_APPROVAL_REQUIRED"
    );
    assert_eq!(
        v1["pleroma"]["metadata"]["account_activation_required"], true,
        "and the activation mode in its pleroma metadata block"
    );
    let v2 = common::TestRequest::get("/api/v2/instance")
        .send(&on)
        .await
        .json_value();
    assert_eq!(
        v2["registrations"]["approval_required"], true,
        "the v2 registrations block follows"
    );
    assert_eq!(
        v2["pleroma"]["metadata"]["account_activation_required"], true,
        "the shared pleroma block appears on v2 too"
    );
    let node = common::TestRequest::get("/nodeinfo/2.0")
        .send(&on)
        .await
        .json_value();
    assert_eq!(
        node["metadata"]["accountActivationRequired"], true,
        "NodeInfo reports the activation mode"
    );
    assert_eq!(
        node["openRegistrations"], true,
        "the modes do not close registration by themselves"
    );

    let off = common::test_app(pool);
    let plain_v1 = common::TestRequest::get("/api/v1/instance")
        .send(&off)
        .await
        .json_value();
    assert_eq!(
        plain_v1["approval_required"], false,
        "the default asks nothing"
    );
    assert_eq!(
        plain_v1["pleroma"]["metadata"]["account_activation_required"], false,
        "and confirms nothing by default"
    );
    let plain_node = common::TestRequest::get("/nodeinfo/2.0")
        .send(&off)
        .await
        .json_value();
    assert_eq!(
        plain_node["metadata"]["accountActivationRequired"], false,
        "NodeInfo says the same"
    );
}
