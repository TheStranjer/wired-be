//! The delete and enable writes over HTTP (WBE-213): `DELETE …/{id}`
//! starting the account's sweep and answering the row as it stood, the
//! refusal of the admin's own account, the shared 404 of an id that names no
//! account, and the enable that frees a deactivated one — a mirror included.

use axum::http::StatusCode;
use serde_json::json;
use uuid::Uuid;

use super::support::{INVALID_TOKEN, NOT_FOUND, V1};
use super::write_support::{
    DISABLED, NOT_ALLOWED, account_action_path, account_path, delete_as, logged, login, one_entry,
    post_json, row_state, seed_writes, sweeps_for, token_count, verify,
};
use crate::common::{self, TestRequest};

/// `DELETE /api/v1/admin/accounts/{id}` starts the same sweep the Pleroma
/// console's `DELETE /users` starts — `deleted_at` stamped, every token
/// revoked, `users.delete` queued — and answers the entity as the row stood,
/// which is the state the client asked to see deleted.
#[tokio::test]
async fn deleting_an_account_starts_its_sweep_and_answers_the_row_as_it_was() {
    let db = common::test_db().await;
    let seeded = seed_writes(&db).await;
    let mine = seeded.token.clone();
    let victim_token = common::create_app_and_token(&seeded.pool, &seeded.plain, "read").await;
    let client = common::create_app(&seeded.pool, "read").await;
    assert_eq!(token_count(&seeded.pool, &seeded.plain).await, 1);

    let response = delete_as(&seeded.app, &mine, &account_path(seeded.plain.id)).await;
    common::assert_answer(&response, StatusCode::OK, "the deletion");
    let body = response.json_value();
    assert_eq!(body["id"], json!(seeded.plain.id.to_string()), "the row");
    assert_eq!(body["username"], json!("plain"), "as the request named it");
    assert_eq!(
        body["disabled"],
        json!(false),
        "as it was: the answer is the pre-deletion snapshot"
    );
    assert_eq!(
        body["approved"],
        json!(true),
        "and not the state it is now in"
    );

    let (active, pending, doomed) = row_state(&seeded.pool, seeded.plain.id)
        .await
        .expect("the row survives its sweep's start");
    assert!(!active, "deleted_at implies the deactivation");
    assert!(!pending, "the approval gate was never the point");
    assert!(doomed, "and the deletion is stamped");
    assert_eq!(
        token_count(&seeded.pool, &seeded.plain).await,
        0,
        "every token went with the start of the deletion"
    );
    assert_eq!(
        sweeps_for(&seeded.pool, &seeded.plain).await.len(),
        1,
        "one sweep queued for the account"
    );

    let denied = verify(&seeded.app, &victim_token).await;
    common::assert_answer(&denied, StatusCode::UNAUTHORIZED, "the revoked credential");
    common::assert_error(&denied, INVALID_TOKEN, "is no credential at all");
    let refused = login(
        &seeded.app,
        &client.client_id,
        &client.client_secret,
        "plain",
    )
    .await;
    common::assert_answer(
        &refused,
        StatusCode::FORBIDDEN,
        "the sign-in of a doomed account",
    );

    let (message, subjects) = one_entry(&seeded.pool, "delete").await;
    assert_eq!(message, "@root deleted users: @plain", "Pleroma's delete");
    assert_eq!(subjects, vec!["plain"], "naming the account");
}

/// The admin's own row is off limits: naming yourself is the 403 this surface
/// answers its refusals with, and nothing is stamped, queued or logged.
#[tokio::test]
async fn deleting_refuses_the_admins_own_account() {
    let db = common::test_db().await;
    let seeded = seed_writes(&db).await;

    let response = delete_as(&seeded.app, &seeded.token, &account_path(seeded.root.id)).await;
    common::assert_answer(&response, StatusCode::FORBIDDEN, "self-deletion");
    common::assert_error(&response, NOT_ALLOWED, "in the pinned words");

    let (active, _, doomed) = row_state(&seeded.pool, seeded.root.id)
        .await
        .expect("the admin is still here");
    assert!(active, "and the refusal is real: the flag did not move");
    assert!(!doomed, "nor was a deletion stamped");
    assert!(
        sweeps_for(&seeded.pool, &seeded.root).await.is_empty(),
        "and no sweep was queued"
    );
    assert!(
        logged(&seeded.pool, "delete").await.is_empty(),
        "and the refused request left no entry behind"
    );
}

/// The 404 of an id that names no account is the reads' own wording, and it
/// answers before any body is read or any state moves.
#[tokio::test]
async fn the_writes_404_for_an_id_that_names_no_account() {
    let db = common::test_db().await;
    let seeded = seed_writes(&db).await;
    let missing = Uuid::now_v7();
    let token = seeded.token.clone();

    let gone = delete_as(&seeded.app, &token, &account_path(missing)).await;
    common::assert_error(&gone, NOT_FOUND, "the deletion of nothing");
    for suffix in ["enable", "approve", "reject"] {
        let response = post_json(
            &seeded.app,
            &token,
            &account_action_path(missing, suffix),
            &json!({}),
        )
        .await;
        common::assert_answer(
            &response,
            StatusCode::NOT_FOUND,
            &format!("the {suffix} of nothing"),
        );
        common::assert_error(&response, NOT_FOUND, "is the shared body");
    }
    let response = post_json(
        &seeded.app,
        &token,
        &account_action_path(missing, "action"),
        &json!({"type": "disable"}),
    )
    .await;
    common::assert_answer(&response, StatusCode::NOT_FOUND, "the action on nothing");
    common::assert_error(
        &response,
        NOT_FOUND,
        "and the account half is checked first",
    );

    let text = TestRequest::delete(&format!("{V1}/not-an-id"))
        .bearer(&token)
        .send(&seeded.app)
        .await;
    common::assert_answer(&text, StatusCode::NOT_FOUND, "text that is no id");
    let enabled = post_json(
        &seeded.app,
        &token,
        &format!("{V1}/not-an-id/enable"),
        &json!({}),
    )
    .await;
    common::assert_answer(
        &enabled,
        StatusCode::NOT_FOUND,
        "on the action suffixes too",
    );
    assert!(
        logged(&seeded.pool, "delete").await.is_empty(),
        "and the misses wrote nothing"
    );
}

/// Enabling is the flag alone: the deactivated account answers active, its
/// untouched credential works again, its password signs in once more, and the
/// `activate` entry Pleroma's `enable` writes names it.
#[tokio::test]
async fn enabling_a_disabled_account_frees_it_again() {
    let db = common::test_db().await;
    let seeded = seed_writes(&db).await;
    let client = common::create_app(&seeded.pool, "read").await;
    let asleep_token = common::create_app_and_token(&seeded.pool, &seeded.asleep, "read").await;
    let blocked = verify(&seeded.app, &asleep_token).await;
    common::assert_answer(&blocked, StatusCode::FORBIDDEN, "the gate before the write");
    common::assert_error(&blocked, DISABLED, "names the flag as the reason");

    let response = post_json(
        &seeded.app,
        &seeded.token,
        &account_action_path(seeded.asleep.id, "enable"),
        &json!({}),
    )
    .await;
    common::assert_answer(&response, StatusCode::OK, "the enable");
    let body = response.json_value();
    assert_eq!(
        body["id"],
        json!(seeded.asleep.id.to_string()),
        "its entity"
    );
    assert_eq!(body["disabled"], json!(false), "answered active");

    let (active, _, _) = row_state(&seeded.pool, seeded.asleep.id)
        .await
        .expect("the row is there");
    assert!(active, "and the flag moved in the row");
    assert_eq!(
        token_count(&seeded.pool, &seeded.asleep).await,
        1,
        "the credential the deactivation left behind is the whole gate"
    );
    let freed = login(
        &seeded.app,
        &client.client_id,
        &client.client_secret,
        "asleep",
    )
    .await;
    common::assert_answer(&freed, StatusCode::OK, "and the account signs in again");
    let allowed = verify(&seeded.app, &asleep_token).await;
    common::assert_answer(&allowed, StatusCode::OK, "with the token it already had");

    let (message, subjects) = one_entry(&seeded.pool, "activate").await;
    assert_eq!(
        message, "@root activated users: @asleep",
        "Pleroma's activate"
    );
    assert_eq!(subjects, vec!["asleep"], "naming the account");
}

/// Enabling an account that was never disabled is not an error: the same
/// `true` is written, the same entity answers and the same entry is logged.
#[tokio::test]
async fn enabling_an_active_account_answers_it_unchanged() {
    let db = common::test_db().await;
    let seeded = seed_writes(&db).await;

    let response = post_json(
        &seeded.app,
        &seeded.token,
        &account_action_path(seeded.plain.id, "enable"),
        &json!({}),
    )
    .await;
    common::assert_answer(&response, StatusCode::OK, "the enable of an active account");
    assert_eq!(
        response.json_value()["disabled"],
        json!(false),
        "answered as it was"
    );
    let (active, _, _) = row_state(&seeded.pool, seeded.plain.id)
        .await
        .expect("the row is there");
    assert!(active, "and it stayed active");

    let (message, subjects) = one_entry(&seeded.pool, "activate").await;
    assert_eq!(message, "@root activated users: @plain", "still the entry");
    assert_eq!(subjects, vec!["plain"]);
}

/// A mirror is a row like any other: the admin who names its id can enable it,
/// because this schema's activation flag is the one column both kinds share.
#[tokio::test]
async fn the_writes_reach_a_remote_mirror_by_id() {
    let db = common::test_db().await;
    let seeded = seed_writes(&db).await;

    let response = post_json(
        &seeded.app,
        &seeded.token,
        &account_action_path(seeded.mirror.id, "enable"),
        &json!({}),
    )
    .await;
    common::assert_answer(&response, StatusCode::OK, "the enable of a mirror");
    assert_eq!(
        response.json_value()["domain"],
        json!("remote.example"),
        "whose entity answers as the reads answer it"
    );
    let (active, _, _) = row_state(&seeded.pool, seeded.mirror.id)
        .await
        .expect("the mirror is still a row");
    assert!(active, "and its flag is written like a local one's");

    let (message, subjects) = one_entry(&seeded.pool, "activate").await;
    assert_eq!(message, "@root activated users: @faraway@remote.example");
    assert_eq!(
        subjects,
        vec!["faraway@remote.example"],
        "by its full handle"
    );
}
