//! The lifecycle activation, toggle and approval of WBE-196: the bulk
//! deactivation that blocks the bearer gate and the login while revoking
//! no token, the activation that frees the account again, the
//! single-account toggle flipping one row by nickname or id, the approval
//! clearing the staff gate and mailing the newly approved, and the gate
//! every lifecycle write — the create and delete included — stands behind.

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use wired_be::models::user::{Flag, NewUser};

use super::support::{
    NO_ADMIN_WRITE, NO_TOKEN, NOT_ADMIN, NOT_FOUND, PASSWORD, USERS, account_named, answered_users,
    create_body, exists, grant_body, jobs_of, lifecycle_flags, logged, mail_config, mint,
    one_entry, patch_json, seed, token_count, write_token,
};
use crate::common::{self, TestRequest};

/// The bulk activation PATCH route.
const ACTIVATE: &str = "/api/v1/pleroma/admin/users/activate";
/// The bulk deactivation PATCH route.
const DEACTIVATE: &str = "/api/v1/pleroma/admin/users/deactivate";
/// The bulk approval PATCH route.
const APPROVE: &str = "/api/v1/pleroma/admin/users/approve";

/// The 403 body of every bearer request a deactivated account makes.
const DISABLED: &str = "Your account is currently disabled";

/// The toggle route of one account.
fn toggle_path(nickname: &str) -> String {
    format!("{USERS}/{nickname}/toggle_activation")
}

/// The `password` grant for `username`, sent.
async fn login(
    app: &Router,
    client_id: &str,
    client_secret: &str,
    username: &str,
) -> common::TestResponse {
    TestRequest::post("/oauth/token")
        .json(&grant_body(client_id, client_secret, username))
        .send(app)
        .await
}

/// Deactivation blocks the account — its bearer requests with the auth
/// gate's own 403, its login with the password grant's disabled refusal —
/// while revoking no token; the caller's own row and the unknown handle
/// are skipped inside the same success, and the moved account answers as
/// the admin entity with Pleroma's `deactivate` entry behind it.
#[tokio::test]
async fn deactivation_blocks_the_account_and_skips_the_caller() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let root = common::create_local_user_with(&pool, "root", |user| NewUser {
        is_admin: Flag::new(true),
        ..user
    })
    .await;
    let sleepy = common::create_user_with_password(&pool, "sleepy", PASSWORD).await;
    let sleepy_token = mint(&pool, &sleepy, "read").await;
    let client = common::create_app(&pool, "read").await;
    let token = mint(&pool, &root, "admin:write:accounts").await;

    let off = patch_json(
        &app,
        &token,
        DEACTIVATE,
        &json!({"nicknames": ["sleepy", "root", "ghost"]}),
    )
    .await;
    common::assert_answer(&off, StatusCode::OK, "the deactivation");
    let body = off.json_value();
    assert_eq!(
        answered_users(&body),
        vec!["sleepy"],
        "the moved account only: the ghost is skipped inside the same 200"
    );
    assert_eq!(
        body["users"][0]["is_active"], false,
        "answered as deactivated"
    );
    let (sleeping, _, _) = lifecycle_flags(&pool, "sleepy").await;
    assert!(!sleeping, "the flag moved");
    let (root_active, _, _) = lifecycle_flags(&pool, "root").await;
    assert!(
        root_active,
        "the caller cannot deactivate itself, even by name"
    );
    assert_eq!(
        token_count(&pool, &sleepy).await,
        1,
        "deactivation revokes no token"
    );

    let denied = TestRequest::get("/api/v1/accounts/verify_credentials")
        .bearer(&sleepy_token)
        .send(&app)
        .await;
    common::assert_answer(&denied, StatusCode::FORBIDDEN, "the bearer gate's own 403");
    common::assert_error(&denied, DISABLED, "in the auth gate's words");

    let refused = login(&app, &client.client_id, &client.client_secret, "sleepy").await;
    common::assert_answer(&refused, StatusCode::FORBIDDEN, "the deactivated login");
    assert_eq!(
        refused.json_value()["identifier"],
        "account_is_disabled",
        "with the password grant's own reason code"
    );

    let (message, subjects) = one_entry(&pool, "deactivate").await;
    assert_eq!(
        message, "@root deactivated users: @sleepy",
        "Pleroma's sentence"
    );
    assert_eq!(
        subjects,
        vec!["sleepy"],
        "the caller's own name left the subject list"
    );
}

/// Activation is the same write with the flag set: it answers the entity
/// active, logs the `activate` entry, and the account that was blocked
/// signs in and calls the API again — the deactivation's un-revoked token
/// works once more, the flag being the whole gate.
#[tokio::test]
async fn activation_frees_the_account_it_releases() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let root = common::create_local_user_with(&pool, "root", |user| NewUser {
        is_admin: Flag::new(true),
        ..user
    })
    .await;
    let sleepy = common::create_user_with_password(&pool, "sleepy", PASSWORD).await;
    let sleepy_token = mint(&pool, &sleepy, "read").await;
    let client = common::create_app(&pool, "read").await;
    let token = mint(&pool, &root, "admin:write:accounts").await;
    let _off = patch_json(&app, &token, DEACTIVATE, &json!({"nicknames": ["sleepy"]})).await;

    let on = patch_json(&app, &token, ACTIVATE, &json!({"nicknames": ["sleepy"]})).await;
    common::assert_answer(&on, StatusCode::OK, "the activation");
    assert_eq!(
        on.json_value()["users"][0]["is_active"],
        true,
        "answered as active"
    );
    let granted = login(&app, &client.client_id, &client.client_secret, "sleepy").await;
    common::assert_answer(&granted, StatusCode::OK, "the freed account signs in again");

    let verify = TestRequest::get("/api/v1/accounts/verify_credentials")
        .bearer(&sleepy_token)
        .send(&app)
        .await;
    common::assert_answer(
        &verify,
        StatusCode::OK,
        "and the token the deactivation left behind works again",
    );

    let (message, subjects) = one_entry(&pool, "activate").await;
    assert_eq!(
        message, "@root activated users: @sleepy",
        "Pleroma's sentence"
    );
    assert_eq!(subjects, vec!["sleepy"]);
}

/// The single-account toggle flips the flag both ways — by nickname and,
/// like the show route, by id — answers that account's own admin entity
/// carrying the new state, logs the move under the bulk pair's actions,
/// and sends the miss and the wrong-scoped token to 404 and 403 without
/// touching the row.
#[tokio::test]
async fn toggle_flips_one_account_and_logs_the_move() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;
    let suspended = account_named(&seeded.pool, "suspended").await;

    let on = TestRequest::patch(&toggle_path("suspended"))
        .bearer(&token)
        .send(&seeded.app)
        .await;
    common::assert_answer(&on, StatusCode::OK, "the wake-up");
    let entity = on.json_value();
    assert_eq!(
        entity["id"],
        suspended.id.to_string(),
        "the one account's entity"
    );
    assert_eq!(entity["nickname"], "suspended");
    assert_eq!(entity["is_active"], true, "carrying the state it moved to");

    let off = TestRequest::patch(&toggle_path(&suspended.id.to_string()))
        .bearer(&token)
        .send(&seeded.app)
        .await;
    common::assert_answer(&off, StatusCode::OK, "the toggle by id, back to sleep");
    assert_eq!(
        off.json_value()["is_active"],
        false,
        "the id reaches the same row"
    );

    let (on_message, on_subjects) = one_entry(&seeded.pool, "activate").await;
    assert_eq!(
        on_message, "@root activated users: @suspended",
        "Pleroma logs the toggle's activation as an activate"
    );
    assert_eq!(on_subjects, vec!["suspended"]);
    let (off_message, off_subjects) = one_entry(&seeded.pool, "deactivate").await;
    assert_eq!(
        off_message, "@root deactivated users: @suspended",
        "and the deactivation likewise"
    );
    assert_eq!(off_subjects, vec!["suspended"]);

    let miss = TestRequest::patch(&toggle_path("ghost"))
        .bearer(&token)
        .send(&seeded.app)
        .await;
    common::assert_answer(&miss, StatusCode::NOT_FOUND, "no account named ghost");
    common::assert_error(&miss, NOT_FOUND, "the module's 404");

    let wrong_scope = TestRequest::patch(&toggle_path("plain"))
        .bearer(&seeded.token)
        .send(&seeded.app)
        .await;
    common::assert_answer(
        &wrong_scope,
        StatusCode::FORBIDDEN,
        "the read scope must not write",
    );
    common::assert_error(&wrong_scope, NO_ADMIN_WRITE, "in the write class's words");
    let (still_active, _, _) = lifecycle_flags(&seeded.pool, "plain").await;
    assert!(still_active, "and the refused toggle wrote nothing");
}

/// The toggle refuses to deactivate the caller: their own nickname, or id,
/// is the 403 and the row stays active with nothing logged, while toggling
/// another account is still the 200 of the unchanged route.
#[tokio::test]
async fn toggle_refuses_to_deactivate_the_caller() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;

    for segment in ["root".to_owned(), seeded.root.id.to_string()] {
        let own = TestRequest::patch(&toggle_path(&segment))
            .bearer(&token)
            .send(&seeded.app)
            .await;
        common::assert_answer(&own, StatusCode::FORBIDDEN, "the caller's own toggle");
        common::assert_error(&own, "This action is not allowed", "in the pinned words");
    }
    let (still_active, _, _) = lifecycle_flags(&seeded.pool, "root").await;
    assert!(still_active, "the refused toggle left the admin active");
    assert!(
        logged(&seeded.pool, "deactivate").await.is_empty(),
        "and logged nothing"
    );

    let other = TestRequest::patch(&toggle_path("plain"))
        .bearer(&token)
        .send(&seeded.app)
        .await;
    common::assert_answer(&other, StatusCode::OK, "another account's toggle");
    assert_eq!(other.json_value()["is_active"], false, "moves as before");
}

/// Approval clears the staff gate for the pending accounts, mails each
/// one that holds an address the approval letter, mints nothing, answers
/// every named account that exists — and leaves the already-approved one
/// unwritten and unmailed when the request runs a second time.
#[tokio::test]
async fn approve_opens_the_pending_accounts_and_mails_them() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let app = common::test_app_with_config(seeded.pool.clone(), mail_config());
    let token = write_token(&seeded).await;
    let _waiting = common::create_local_user_with(&seeded.pool, "waiting", |user| NewUser {
        approval_pending: Flag::new(true),
        email: Some("waiting@example.org".to_owned()),
        ..user
    })
    .await;

    let response = patch_json(
        &app,
        &token,
        APPROVE,
        &json!({"nicknames": ["waiting", "newbie", "plain", "ghost"]}),
    )
    .await;
    common::assert_answer(&response, StatusCode::OK, "the approval");
    let body = response.json_value();
    assert_eq!(
        answered_users(&body),
        vec!["waiting", "newbie", "plain"],
        "every named account that exists, in order, the miss skipped"
    );
    for index in 0..3 {
        assert_eq!(
            body["users"][index]["is_approved"], true,
            "answered as approved, moved now or before"
        );
    }

    for nickname in ["waiting", "newbie"] {
        let (_, _, pending) = lifecycle_flags(&seeded.pool, nickname).await;
        assert!(!pending, "{nickname} is off the approval list");
    }
    let waiting = account_named(&seeded.pool, "waiting").await;
    assert_eq!(
        token_count(&seeded.pool, &waiting).await,
        0,
        "and the approval minted no credential"
    );

    let letters = jobs_of(&seeded.pool, "mailer", "mailer.send").await;
    assert_eq!(
        letters.len(),
        1,
        "one letter: plain was not approved now, and newbie has no address"
    );
    assert_eq!(
        letters[0]["to"],
        json!("waiting@example.org"),
        "to the newly approved"
    );
    assert!(
        letters[0]["subject"]
            .as_str()
            .unwrap_or_default()
            .contains("approved"),
        "the approval letter: {}",
        letters[0]["subject"]
    );

    let (message, subjects) = one_entry(&seeded.pool, "approve").await;
    assert_eq!(
        message, "@root approved users: @waiting, @newbie, @plain",
        "Pleroma's approve sentence, naming every resolved subject"
    );
    assert_eq!(subjects, vec!["waiting", "newbie", "plain"], "moved or not");
}

/// Approving an already-approved account changes nothing and mails
/// nothing — Pleroma's `approve(%User{is_approved: true})` clause hands
/// the user back untouched — yet the answer still carries its entity and
/// the write still logs its entry.
#[tokio::test]
async fn approving_an_already_approved_account_sends_no_letter() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let app = common::test_app_with_config(seeded.pool.clone(), mail_config());
    let token = write_token(&seeded).await;

    let again = patch_json(&app, &token, APPROVE, &json!({"nicknames": ["plain"]})).await;
    common::assert_answer(&again, StatusCode::OK, "approval of the already approved");
    assert_eq!(
        answered_users(&again.json_value()),
        vec!["plain"],
        "it answers unchanged"
    );
    assert!(
        jobs_of(&seeded.pool, "mailer", "mailer.send")
            .await
            .is_empty(),
        "no letter for an account nobody just approved"
    );
    let (message, subjects) = one_entry(&seeded.pool, "approve").await;
    assert_eq!(
        message, "@root approved users: @plain",
        "and still the entry"
    );
    assert_eq!(subjects, vec!["plain"]);
}

/// One gated lifecycle route: its name for the assertions, the HTTP verb,
/// the path, and the JSON body it needs (`None` for the body-less toggle).
type LifecycleRoute<'a> = (&'a str, &'a str, &'a str, Option<&'a Value>);

/// One expected refusal: who presents which credential (no bearer at all
/// for `None`) and the status and message the gate answers.
type Refusal<'a> = (&'a str, Option<&'a str>, StatusCode, &'a str);

/// Sends one lifecycle request, asserting the gate refused it with the
/// pinned status and message before anything ran.
async fn refuse(app: &Router, route: LifecycleRoute<'_>, refusal: Refusal<'_>) {
    let (what, verb, path, body) = route;
    let (who, token, status, message) = refusal;
    let base = match verb {
        "POST" => TestRequest::post(path),
        "DELETE" => TestRequest::delete(path),
        _ => TestRequest::patch(path),
    };
    let with_body = match body {
        Some(value) => base.json(value),
        None => base,
    };
    let with_auth = match token {
        Some(bearer) => with_body.bearer(bearer),
        None => with_body,
    };
    let response = with_auth.send(app).await;
    common::assert_answer(&response, status, &format!("{what} from {who}"));
    common::assert_error(&response, message, &format!("{what} from {who}"));
}

/// Every lifecycle route stands behind the same gate as the rest of the
/// module: no token is the 401, a NON-admin holding `admin:write:accounts`
/// is the 403 "User is not an admin.", and an admin token of the wrong
/// scope or the wrong area is the 403 naming `admin:write` — nobody
/// outside the accounts gate moves an account, and nothing lands while
/// the gate is closed.
#[tokio::test]
async fn the_lifecycle_writes_guard_their_gate() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let member = common::create_local_user_with(&seeded.pool, "member", |user| user).await;
    let member_token = mint(&seeded.pool, &member, "admin:write:accounts").await;
    let other_area = mint(&seeded.pool, &seeded.root, "admin:write:reports").await;
    let names = json!({"nicknames": ["plain"]});
    let gate_body = create_body(&[("gate1", "gate1@example.org", PASSWORD)]);
    let toggle = toggle_path("plain");

    let routes: [LifecycleRoute<'_>; 6] = [
        ("POST create", "POST", USERS, Some(&gate_body)),
        ("DELETE users", "DELETE", USERS, Some(&names)),
        ("PATCH activate", "PATCH", ACTIVATE, Some(&names)),
        ("PATCH deactivate", "PATCH", DEACTIVATE, Some(&names)),
        ("PATCH approve", "PATCH", APPROVE, Some(&names)),
        ("PATCH toggle", "PATCH", &toggle, None),
    ];
    let refusals: [Refusal<'_>; 4] = [
        ("no token at all", None, StatusCode::UNAUTHORIZED, NO_TOKEN),
        (
            "a NON-admin holding the exact scope",
            Some(member_token.as_str()),
            StatusCode::FORBIDDEN,
            NOT_ADMIN,
        ),
        (
            "the admin's read scope",
            Some(seeded.token.as_str()),
            StatusCode::FORBIDDEN,
            NO_ADMIN_WRITE,
        ),
        (
            "the admin's other write area",
            Some(other_area.as_str()),
            StatusCode::FORBIDDEN,
            NO_ADMIN_WRITE,
        ),
    ];

    for route in routes {
        for refusal in refusals {
            refuse(&seeded.app, route, refusal).await;
        }
    }

    assert!(
        !exists(&seeded.pool, "gate1").await,
        "and every refused request wrote nothing"
    );
    let (plain_active, _, plain_pending) = lifecycle_flags(&seeded.pool, "plain").await;
    assert!(
        plain_active && !plain_pending,
        "plain's row stands as the fixture left it"
    );
    for action in ["create", "delete", "activate", "deactivate", "approve"] {
        assert!(
            logged(&seeded.pool, action).await.is_empty(),
            "the closed gate logged no {action} entry"
        );
    }
    assert!(
        jobs_of(&seeded.pool, "background", "users.delete")
            .await
            .is_empty(),
        "and queued no sweep"
    );
}
