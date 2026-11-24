//! The password-reset and email-confirmation maintenance of WBE-198: the
//! issued reset link that completes through the existing form, the forced
//! reset that blocks login until a new password lands, Pleroma's own
//! forced-reset path, the confirmation that flips the flag and kills the
//! outstanding token, and the resend that mails only the unconfirmed.

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;
use wired_be::{
    config::TEST_BASE_URL,
    models::user::{Flag, NewUser, User},
};

use super::support::{
    CONFIRM, FORCE_RESET, NO_ADMIN_WRITE, RESEND, assert_no_content, flag_of, issued_reset, logged,
    mail_config, mint, one_entry, patch_json, pending, plain, seed, write_token,
};
use crate::common::{self, TestRequest};

/// Pleroma's own path for the forced reset, served by the same handler.
const FORCE_RESET_PLEROMA: &str = "/api/v1/pleroma/admin/users/force_password_reset";

/// The completion form the issued link points at.
const RESET_FORM: &str = "/api/v1/pleroma/password_reset";

/// The 403 `error` of the password grant refused to a forced-reset account.
const RESET_REQUIRED: &str = "Password reset is required";

/// The reset tokens stored for one account.
async fn stored_tokens(
    pool: &PgPool,
    account: &User,
) -> Vec<wired_be::models::password_reset::PasswordResetToken> {
    sqlx::query_as::<_, wired_be::models::password_reset::PasswordResetToken>(
        "SELECT * FROM password_reset_tokens WHERE user_id = $1 ORDER BY inserted_at",
    )
    .bind(account.id)
    .fetch_all(pool)
    .await
    .expect("the token table reads back")
}

/// The issued reset: gated WRITE (the read scope is refused), answered for
/// a local account with `{token, link}` whose link completes through the
/// existing form, and 404 for a mirror or a miss.
#[tokio::test]
async fn the_reset_token_endpoint_issues_a_spendable_link() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;

    let read_only = TestRequest::get(&issued_reset("plain"))
        .bearer(&seeded.token)
        .send(&seeded.app)
        .await;
    common::assert_answer(
        &read_only,
        StatusCode::FORBIDDEN,
        "the read scope must not mint",
    );
    common::assert_error(&read_only, NO_ADMIN_WRITE, "the read scope must not mint");

    let response = TestRequest::get(&issued_reset("plain"))
        .bearer(&token)
        .send(&seeded.app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the mint");
    let body = response.json_value();
    let secret = body["token"].as_str().expect("the issued token text");
    assert_eq!(
        body["link"],
        json!(format!("{TEST_BASE_URL}{RESET_FORM}/{secret}")),
        "the absolute link the answer pins, hung on the instance origin"
    );
    assert_eq!(
        body.as_object().expect("an object").len(),
        2,
        "exactly token and link"
    );
    let stored = stored_tokens(&seeded.pool, &plain(&seeded.pool).await).await;
    assert_eq!(stored.len(), 1, "the mint landed in the table");
    assert_eq!(
        stored[0].token_digest,
        wired_be::util::token::token_digest(secret),
        "and holds the digest of the answered token"
    );
    assert_ne!(stored[0].token_digest, secret, "never the token itself");

    for segment in ["nobody", "faraway@remote.example"] {
        let missing = TestRequest::get(&issued_reset(segment))
            .bearer(&token)
            .send(&seeded.app)
            .await;
        common::assert_answer(&missing, StatusCode::NOT_FOUND, "a reset miss or mirror");
    }

    // The link completes through the existing form, and the new password
    // then signs in: the admin's token is spendable exactly like the
    // emailed one.
    let _resettable =
        common::create_user_with_password(&seeded.pool, "resettable", "first secret pass").await;
    let minted = TestRequest::get(&issued_reset("resettable"))
        .bearer(&token)
        .send(&seeded.app)
        .await;
    let issued = minted.json_value();
    let fresh = issued["token"].as_str().expect("the token text");
    let completed = TestRequest::post(RESET_FORM)
        .form(&[
            ("data[token]", fresh),
            ("data[password]", "second secret pass"),
            ("data[password_confirmation]", "second secret pass"),
        ])
        .send(&seeded.app)
        .await;
    common::assert_answer(&completed, StatusCode::OK, "the completion");
    assert!(
        wired_be::models::user::register::authenticate(
            &seeded.pool,
            "resettable",
            "second secret pass"
        )
        .await
        .is_some(),
        "the reset landed"
    );
    assert!(
        logged(&seeded.pool, "updated_users").await.is_empty(),
        "the user-facing reset completion logs no admin write"
    );
}

/// Pleroma's `PATCH /admin/users/force_password_reset` path reaches the
/// same forced reset as the `password_reset` alias: 204 and the flag raised.
#[tokio::test]
async fn force_password_reset_answers_on_pleromas_path() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;
    let victim =
        common::create_user_with_password(&seeded.pool, "forced", "their secret pass").await;

    let response = patch_json(
        &seeded.app,
        &token,
        FORCE_RESET_PLEROMA,
        &json!({ "nicknames": ["forced"] }),
    )
    .await;
    assert_no_content(&response, "the forced reset on Pleroma's path");
    assert!(pending(&seeded.pool, &victim).await, "the flag is raised");
}

/// The forced reset: 204, the flag raised and the tokens revoked per
/// account, unknown names skipped inside the same success, one log entry
/// naming who was found — and a body with no `nicknames` is the
/// extractor's 422.
#[tokio::test]
async fn force_password_reset_blocks_login_until_a_new_password_lands() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;
    let victim =
        common::create_user_with_password(&seeded.pool, "forced", "their secret pass").await;
    let bearer = mint(&seeded.pool, &victim, "read").await;
    let login_app = common::create_app(&seeded.pool, "read write").await;

    let response = patch_json(
        &seeded.app,
        &token,
        FORCE_RESET,
        &json!({ "nicknames": ["forced", "ghost"] }),
    )
    .await;
    assert_no_content(&response, "the forced reset");

    assert!(pending(&seeded.pool, &victim).await, "the flag is raised");
    let stale = TestRequest::get("/api/v1/accounts/verify_credentials")
        .bearer(&bearer)
        .send(&seeded.app)
        .await;
    common::assert_answer(&stale, StatusCode::UNAUTHORIZED, "the revoked bearer");
    let blocked = TestRequest::post("/oauth/token")
        .json(&json!({
            "grant_type": "password",
            "username": "forced",
            "password": "their secret pass",
            "scope": "read write",
            "client_id": login_app.client_id,
            "client_secret": login_app.client_secret,
        }))
        .send(&seeded.app)
        .await;
    common::assert_answer(&blocked, StatusCode::FORBIDDEN, "the pending grant");
    assert_eq!(
        blocked.json_value(),
        json!({ "error": RESET_REQUIRED, "identifier": "password_reset_required" }),
        "the login refusal names the reason"
    );

    let (message, subjects) = one_entry(&seeded.pool, "force_password_reset").await;
    assert_eq!(
        message, "@root forced password reset for users: @forced",
        "Pleroma's sentence, the skipped ghost unmentioned"
    );
    assert_eq!(subjects, vec!["forced".to_owned()], "one found subject");

    // A form's bracketed spelling reaches the same write, and the missing
    // parameter is the extractor's 422.
    let second =
        common::create_user_with_password(&seeded.pool, "forced2", "their secret pass").await;
    let by_form = TestRequest::patch(FORCE_RESET)
        .bearer(&token)
        .form(&[("nicknames[]", "forced2")])
        .send(&seeded.app)
        .await;
    assert_no_content(&by_form, "the form spelling");
    assert!(pending(&seeded.pool, &second).await, "and it forced too");

    let bare = patch_json(&seeded.app, &token, FORCE_RESET, &json!({})).await;
    common::assert_answer(
        &bare,
        StatusCode::UNPROCESSABLE_ENTITY,
        "the body without nicknames",
    );
}

/// Confirming emails by list: the flag flips, an outstanding token dies,
/// the unknown name is skipped and the entry names the found pair.
#[tokio::test]
async fn confirm_email_confirms_the_named_accounts() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;
    let unminded = wired_be::models::user::get_by_nickname(&seeded.pool, "unminded")
        .await
        .expect("the account reads back")
        .expect("the fixture seeded it");
    let unminded_id = unminded.id;
    let _wild =
        sqlx::query("UPDATE users SET confirmation_token = 'a-letter-in-the-wild' WHERE id = $1")
            .bind(unminded_id)
            .execute(&seeded.pool)
            .await
            .expect("the token fixture writes");

    let response = patch_json(
        &seeded.app,
        &token,
        CONFIRM,
        &json!({ "nicknames": ["unminded", "plain", "ghost"] }),
    )
    .await;
    assert_no_content(&response, "the confirmation");
    assert!(
        flag_of(
            &seeded.pool,
            "SELECT email_confirmed FROM users WHERE id = $1",
            &unminded
        )
        .await,
        "the address is confirmed"
    );
    let token_row = sqlx::query_scalar::<_, Option<String>>(
        "SELECT confirmation_token FROM users WHERE id = $1",
    )
    .bind(unminded_id)
    .fetch_one(&seeded.pool)
    .await
    .expect("the token column reads back");
    assert!(
        token_row.is_none(),
        "the outstanding letter confirms nothing now"
    );

    let (message, subjects) = one_entry(&seeded.pool, "confirm_email").await;
    assert_eq!(
        message, "@root confirmed email for users: @unminded, @plain",
        "Pleroma's sentence"
    );
    assert_eq!(
        subjects,
        vec!["unminded".to_owned(), "plain".to_owned()],
        "the found pair, in the submitted order"
    );
}

/// The resend mails only the unconfirmed: with a relay configured, exactly
/// one letter queues for the unconfirmed named account, the confirmed one
/// is skipped, and the log entry still names both found subjects.
#[tokio::test]
async fn resend_confirmation_emails_only_the_unconfirmed() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let app = common::test_app_with_config(seeded.pool.clone(), mail_config());
    let token = mint(&seeded.pool, &seeded.root, "admin:write:accounts").await;
    let mailed = common::create_local_user_with(&seeded.pool, "postme", |user| NewUser {
        email: Some("postme@example.org".to_owned()),
        email_confirmed: Flag::new(false),
        ..user
    })
    .await;

    let response = patch_json(
        &app,
        &token,
        RESEND,
        &json!({ "nicknames": ["postme", "plain", "ghost"] }),
    )
    .await;
    assert_no_content(&response, "the resend");

    let letters = sqlx::query_scalar::<_, Value>(
        "SELECT args FROM jobs WHERE queue = 'mailer' AND kind = 'mailer.send' ORDER BY id",
    )
    .fetch_all(&seeded.pool)
    .await
    .expect("the mailer queue reads back");
    assert_eq!(
        letters.len(),
        1,
        "one letter, the confirmed account got none"
    );
    assert_eq!(
        letters[0]["to"],
        json!("postme@example.org"),
        "the unconfirmed address"
    );
    assert!(
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT confirmation_token FROM users WHERE id = $1"
        )
        .bind(mailed.id)
        .fetch_one(&seeded.pool)
        .await
        .expect("the token column reads back")
        .is_some(),
        "the resend rotated a live token"
    );

    let (message, subjects) = one_entry(&seeded.pool, "resend_confirmation_email").await;
    assert_eq!(
        message, "@root re-sent confirmation email for users: @postme, @plain",
        "Pleroma's sentence"
    );
    assert_eq!(
        subjects,
        vec!["postme".to_owned(), "plain".to_owned()],
        "every resolved subject, mailed or not"
    );
}
