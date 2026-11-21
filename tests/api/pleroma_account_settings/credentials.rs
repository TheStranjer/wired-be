//! The `change_password` and `change_email` routes over HTTP: the stored
//! results, the refusals and the confirmation letter.

use axum::http::StatusCode;
use serde_json::json;
use wired_be::{mailer::KIND_SEND, util::password::verify_password};

use super::support::{
    CHANGE_EMAIL, CHANGE_PASSWORD, NEW_PASSWORD, PASSWORD, fixture, post_json, queued_jobs,
    signed_in, stored, token_count,
};
use crate::common;

/// `change_password` success: the new secret verifies, the old one does
/// not, the presenting token still authenticates and every other token of
/// the account is gone.
#[tokio::test]
async fn change_password_stores_the_new_secret_and_revokes_the_others() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, current) = fixture(&pool, "alice", "write:accounts read:accounts").await;
    let other = common::create_app_and_token(&pool, &alice, "write:accounts").await;
    let app = common::test_app(pool.clone());

    let response = post_json(
        &app,
        CHANGE_PASSWORD,
        &current,
        json!({
            "password": PASSWORD,
            "new_password": NEW_PASSWORD,
            "new_password_confirmation": NEW_PASSWORD,
        }),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "the change answers");
    assert_eq!(
        response.json_value(),
        json!({"status": "success"}),
        "the documented success body"
    );

    let hash = stored(&pool, alice.id).await.password_hash.expect("a hash");
    assert!(
        verify_password(NEW_PASSWORD, &hash),
        "the new password verifies"
    );
    assert!(!verify_password(PASSWORD, &hash), "the old one is dead");

    assert_eq!(
        token_count(&pool, alice.id).await,
        1,
        "only the presenting token survives"
    );
    assert_eq!(
        signed_in(&app, &current).await,
        StatusCode::OK,
        "and it still authenticates"
    );
    assert_eq!(
        signed_in(&app, &other).await,
        StatusCode::UNAUTHORIZED,
        "the other credential revoked with the old password"
    );
}

/// The three `change_password` refusals are the 200 error bodies Pleroma
/// renders, and none of them touches the hash or the tokens.
#[tokio::test]
async fn change_password_refusals_are_200_bodies_and_change_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "write:accounts").await;
    let app = common::test_app(pool.clone());

    let wrong = post_json(
        &app,
        CHANGE_PASSWORD,
        &token,
        json!({
            "password": "not-my-password",
            "new_password": NEW_PASSWORD,
            "new_password_confirmation": NEW_PASSWORD,
        }),
    )
    .await;
    assert_eq!(wrong.status, StatusCode::OK, "Pleroma answers 200");
    assert_eq!(
        wrong.json_value(),
        json!({"error": "Invalid password."}),
        "the password refusal"
    );

    let mismatched = post_json(
        &app,
        CHANGE_PASSWORD,
        &token,
        json!({
            "password": PASSWORD,
            "new_password": NEW_PASSWORD,
            "new_password_confirmation": "typed-it-differently",
        }),
    )
    .await;
    assert_eq!(mismatched.status, StatusCode::OK, "also a 200");
    assert_eq!(
        mismatched.json_value(),
        json!({"error": "New password does not match confirmation."}),
        "the confirmation refusal"
    );

    let short = post_json(
        &app,
        CHANGE_PASSWORD,
        &token,
        json!({
            "password": PASSWORD,
            "new_password": "short",
            "new_password_confirmation": "short",
        }),
    )
    .await;
    assert_eq!(short.status, StatusCode::OK, "also a 200");
    assert_eq!(
        short.json_value(),
        json!({"error": "Password too short."}),
        "the length refusal (below MIN_PASSWORD_LENGTH's default 8)"
    );

    let hash = stored(&pool, alice.id).await.password_hash.expect("a hash");
    assert!(
        verify_password(PASSWORD, &hash),
        "no refusal replaced the stored hash"
    );
    assert_eq!(
        token_count(&pool, alice.id).await,
        1,
        "no token was revoked"
    );
}

/// `change_email` success: a free address lands, trimmed but as given,
/// confirmed (the instance demands no confirmation by default) — and no
/// token goes with it.
#[tokio::test]
async fn change_email_lands_a_free_address() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "write:accounts").await;
    let app = common::test_app(pool.clone());

    let moved = post_json(
        &app,
        CHANGE_EMAIL,
        &token,
        json!({"password": PASSWORD, "email": "  Alice-New@Example.ORG  "}),
    )
    .await;
    assert_eq!(moved.status, StatusCode::OK, "the change answers");
    assert_eq!(
        moved.json_value(),
        json!({"status": "success"}),
        "the documented success body"
    );
    let alice_now = stored(&pool, alice.id).await;
    assert_eq!(
        alice_now.email.as_deref(),
        Some("Alice-New@Example.ORG"),
        "the trimmed address is stored as given (citext folds the lookups)"
    );
    assert!(
        alice_now.email_confirmed.get(),
        "no confirmation is required by default"
    );
    assert_eq!(
        token_count(&pool, alice.id).await,
        1,
        "an email change revokes nothing"
    );
}

/// The three `change_email` refusals are the endpoint's 200 error bodies
/// — the password's, the format's and the uniqueness' (citext folds the
/// foreign case) — and none of them touches the row.
#[tokio::test]
async fn change_email_refusals_are_200_bodies() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "write:accounts").await;
    let _bob = common::create_user_with_password(&pool, "bob", PASSWORD).await;
    let app = common::test_app(pool.clone());

    let wrong = post_json(
        &app,
        CHANGE_EMAIL,
        &token,
        json!({"password": "nope", "email": "third@example.org"}),
    )
    .await;
    assert_eq!(wrong.status, StatusCode::OK, "the password refusal is 200");
    assert_eq!(
        wrong.json_value(),
        json!({"error": "Invalid password."}),
        "the password body"
    );

    let malformed = post_json(
        &app,
        CHANGE_EMAIL,
        &token,
        json!({"password": PASSWORD, "email": "plainaddress"}),
    )
    .await;
    assert_eq!(
        malformed.status,
        StatusCode::OK,
        "the format refusal is 200"
    );
    assert_eq!(
        malformed.json_value(),
        json!({"error": "Email has invalid format."}),
        "the format refusal"
    );

    let foreign = post_json(
        &app,
        CHANGE_EMAIL,
        &token,
        json!({"password": PASSWORD, "email": "BOB@example.org"}),
    )
    .await;
    assert_eq!(
        foreign.status,
        StatusCode::OK,
        "the uniqueness refusal is 200"
    );
    assert_eq!(
        foreign.json_value(),
        json!({"error": "Email has already been taken"}),
        "the ticket's wording, and citext catches the foreign case"
    );
    assert_eq!(
        stored(&pool, alice.id).await.email,
        Some("alice@example.org".to_owned()),
        "the refusals changed nothing"
    );
}

/// With `ACCOUNT_ACTIVATION_REQUIRED` on and an SMTP relay configured,
/// the address change lands unconfirmed and queues the confirmation
/// letter for the new address.
#[tokio::test]
async fn change_email_queues_a_confirmation_letter_when_required() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "write:accounts").await;
    let mut config = common::test_config();
    config.account_activation_required = wired_be::config::Toggle::new(true);
    config.smtp_host = Some("smtp.test.example".to_owned());
    let app = common::test_app_with_config(pool.clone(), config);

    let response = post_json(
        &app,
        CHANGE_EMAIL,
        &token,
        json!({"password": PASSWORD, "email": "alice-new@example.org"}),
    )
    .await;
    assert_eq!(
        response.json_value(),
        json!({"status": "success"}),
        "the change still succeeds"
    );
    let changed = stored(&pool, alice.id).await;
    assert_eq!(
        changed.email.as_deref(),
        Some("alice-new@example.org"),
        "the address landed"
    );
    assert!(
        !changed.email_confirmed.get(),
        "and the account must confirm it"
    );

    let letters = queued_jobs(&pool, KIND_SEND).await;
    assert_eq!(letters.len(), 1, "one mailer job queued");
    assert_eq!(letters[0].0, "mailer", "on the mail queue");
    assert_eq!(
        letters[0].1["to"],
        json!("alice-new@example.org"),
        "to the new address"
    );
    assert!(
        letters[0].1["subject"]
            .as_str()
            .is_some_and(|s| s.contains("account confirmation")),
        "the confirmation subject: {}",
        letters[0].1["subject"]
    );
    assert!(
        letters[0].1["text_body"]
            .as_str()
            .is_some_and(|body| body.contains(&format!("confirm_email/{}", alice.id))),
        "the letter links to the account's confirmation path"
    );
}

/// Confirmation required without a relay still resets the flag — the
/// dropped letter is a log line, not a failure (see `crate::mailer`).
#[tokio::test]
async fn confirmation_resets_the_flag_even_with_no_relay() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "write:accounts").await;
    let mut config = common::test_config();
    config.account_activation_required = wired_be::config::Toggle::new(true);
    let app = common::test_app_with_config(pool.clone(), config);

    let response = post_json(
        &app,
        CHANGE_EMAIL,
        &token,
        json!({"password": PASSWORD, "email": "alice-new@example.org"}),
    )
    .await;
    assert_eq!(
        response.json_value(),
        json!({"status": "success"}),
        "a disabled mailer never fails the request"
    );
    assert!(
        !stored(&pool, alice.id).await.email_confirmed.get(),
        "the account still awaits confirmation"
    );
    assert!(
        queued_jobs(&pool, KIND_SEND).await.is_empty(),
        "and nothing was queued without SMTP_HOST"
    );
}
