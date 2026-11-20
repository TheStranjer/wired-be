//! The approve and reject writes over HTTP (WBE-213): the approval that
//! clears the staff gate and letters the account, its unchanged answer for
//! the already approved and its silent one for the addressless, and the
//! rejection that deletes a pending account outright and refuses every
//! account that was never pending.

use axum::http::StatusCode;
use serde_json::json;

use super::write_support::{
    APPROVED_MAIL, AWAITING, NOT_ALLOWED, REJECTED_MAIL, account_action_path, letters, logged,
    login, one_entry, post_json, row_state, seed_writes, sweeps_for, token_count, verify,
};
use crate::common;

/// Approving a pending registration clears the gate, posts its letter and
/// answers the fresh entity — and the account, which the login gate refused
/// as awaiting approval, signs in on the strength of the flag alone: the
/// approval mints nothing.
#[tokio::test]
async fn approving_a_pending_account_letters_it() {
    let db = common::test_db().await;
    let seeded = seed_writes(&db).await;
    let client = common::create_app(&seeded.pool, "read").await;
    let waiting = login(
        &seeded.app,
        &client.client_id,
        &client.client_secret,
        "newbie",
    )
    .await;
    common::assert_answer(
        &waiting,
        StatusCode::FORBIDDEN,
        "the pending account's sign-in",
    );
    common::assert_error(&waiting, AWAITING, "names the gate that holds it");

    let response = post_json(
        &seeded.app,
        &seeded.token,
        &account_action_path(seeded.newbie.id, "approve"),
        &json!({}),
    )
    .await;
    common::assert_answer(&response, StatusCode::OK, "the approval");
    let body = response.json_value();
    assert_eq!(
        body["id"],
        json!(seeded.newbie.id.to_string()),
        "the entity"
    );
    assert_eq!(body["approved"], json!(true), "answered approved");

    let (active, pending, _) = row_state(&seeded.pool, seeded.newbie.id)
        .await
        .expect("the row is there");
    assert!(!pending, "the staff gate is off the row");
    assert!(active, "and the account was active all along");
    assert_eq!(
        token_count(&seeded.pool, &seeded.newbie).await,
        0,
        "approval mints no credential — Pleroma's approval is a flag, not a password"
    );

    let granted = login(
        &seeded.app,
        &client.client_id,
        &client.client_secret,
        "newbie",
    )
    .await;
    common::assert_answer(
        &granted,
        StatusCode::OK,
        "the approved account signs in at last",
    );
    assert!(
        !granted.json_value()["access_token"].is_null(),
        "with a credential of its own now"
    );

    let posted = letters(&seeded.pool).await;
    assert_eq!(
        posted.len(),
        1,
        "one letter, to the account that just passed"
    );
    assert_eq!(
        posted[0]["to"],
        json!("newbie@example.org"),
        "its own address"
    );
    assert_eq!(
        posted[0]["subject"],
        json!(APPROVED_MAIL),
        "the approval letter"
    );

    let (message, subjects) = one_entry(&seeded.pool, "approve").await;
    assert_eq!(
        message, "@root approved users: @newbie",
        "Pleroma's approve"
    );
    assert_eq!(subjects, vec!["newbie"]);
}

/// An account nobody left pending is Pleroma's already-approved arm: the same
/// entity back, no letter, no write — and still a log subject. So is a pending
/// one that never gave an address: approved, and mailed nothing because there
/// is nothing to mail.
#[tokio::test]
async fn approving_answers_an_approved_account_unchanged_and_letters_an_addressless_one() {
    let db = common::test_db().await;
    let seeded = seed_writes(&db).await;

    let again = post_json(
        &seeded.app,
        &seeded.token,
        &account_action_path(seeded.plain.id, "approve"),
        &json!({}),
    )
    .await;
    common::assert_answer(&again, StatusCode::OK, "approval of the already approved");
    assert_eq!(
        again.json_value()["approved"],
        json!(true),
        "answered unchanged"
    );

    let nameless = post_json(
        &seeded.app,
        &seeded.token,
        &account_action_path(seeded.quiet.id, "approve"),
        &json!({}),
    )
    .await;
    common::assert_answer(&nameless, StatusCode::OK, "approval of the addressless one");
    assert_eq!(
        nameless.json_value()["email"],
        json!(null),
        "and its entity carries the absence it stored"
    );
    let (_, pending, _) = row_state(&seeded.pool, seeded.quiet.id)
        .await
        .expect("the row is there");
    assert!(!pending, "the gate is off it");

    assert!(
        letters(&seeded.pool).await.is_empty(),
        "no letter for an account that was not approved now, and none for one with no address"
    );
    let entries = logged(&seeded.pool, "approve").await;
    let messages: Vec<&str> = entries
        .iter()
        .map(|(message, _)| message.as_str())
        .collect();
    assert_eq!(
        messages,
        vec![
            "@root approved users: @plain",
            "@root approved users: @quiet"
        ],
        "one entry per request, moved or not"
    );
}

/// Rejecting a pending registration deletes the account outright — row and
/// tokens gone — posts its rejection letter and answers the snapshot of the
/// row that is no more.
#[tokio::test]
async fn rejecting_a_pending_account_deletes_it() {
    let db = common::test_db().await;
    let seeded = seed_writes(&db).await;
    let their_token = common::create_app_and_token(&seeded.pool, &seeded.newbie, "read").await;

    let response = post_json(
        &seeded.app,
        &seeded.token,
        &account_action_path(seeded.newbie.id, "reject"),
        &json!({}),
    )
    .await;
    common::assert_answer(&response, StatusCode::OK, "the rejection");
    let body = response.json_value();
    assert_eq!(
        body["id"],
        json!(seeded.newbie.id.to_string()),
        "the snapshot"
    );
    assert_eq!(
        body["approved"],
        json!(false),
        "as it stood: pending, which is what was refused"
    );
    assert_eq!(
        body["email"],
        json!("newbie@example.org"),
        "the address it gave"
    );

    assert!(
        row_state(&seeded.pool, seeded.newbie.id).await.is_none(),
        "the row is gone, not purged or deactivated"
    );
    assert_eq!(
        token_count(&seeded.pool, &seeded.newbie).await,
        0,
        "and its credential left on the cascade"
    );
    let denied = verify(&seeded.app, &their_token).await;
    common::assert_answer(
        &denied,
        StatusCode::UNAUTHORIZED,
        "the credential of a deleted account",
    );
    assert!(
        sweeps_for(&seeded.pool, &seeded.newbie).await.is_empty(),
        "rejection deletes the row outright: there is no sweep to wait for"
    );

    let posted = letters(&seeded.pool).await;
    assert_eq!(posted.len(), 1, "the rejected account is written to");
    assert_eq!(
        posted[0]["to"],
        json!("newbie@example.org"),
        "at its address"
    );
    assert_eq!(
        posted[0]["subject"],
        json!(REJECTED_MAIL),
        "the rejection letter"
    );

    let (message, subjects) = one_entry(&seeded.pool, "reject").await;
    assert_eq!(
        message, "@root rejected users: @newbie",
        "Pleroma's reject sentence"
    );
    assert_eq!(
        subjects,
        vec!["newbie"],
        "naming the row the log can no longer join to"
    );
}

/// Rejection is the pending account's refusal: an approved one — active,
/// deactivated, a mirror — is the 403, and survives the request unchanged.
#[tokio::test]
async fn rejecting_an_account_that_was_never_pending_is_refused() {
    let db = common::test_db().await;
    let seeded = seed_writes(&db).await;
    let token = seeded.token.clone();

    for (who, label) in [
        (seeded.plain.id, "an approved account"),
        (seeded.asleep.id, "a deactivated one"),
        (seeded.mirror.id, "a mirror"),
    ] {
        let response = post_json(
            &seeded.app,
            &token,
            &account_action_path(who, "reject"),
            &json!({}),
        )
        .await;
        common::assert_answer(
            &response,
            StatusCode::FORBIDDEN,
            &format!("rejecting {label}"),
        );
        common::assert_error(&response, NOT_ALLOWED, "in the pinned words");
        assert!(
            row_state(&seeded.pool, who).await.is_some(),
            "{label} is still a row"
        );
    }
    assert!(
        letters(&seeded.pool).await.is_empty(),
        "and no refusal posted a letter"
    );
    assert!(
        logged(&seeded.pool, "reject").await.is_empty(),
        "nor left an entry"
    );
}
