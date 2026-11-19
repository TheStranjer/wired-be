//! The `create-admin` bootstrap function (`wired_be::cli::create_admin`,
//! WBE-228): the account it registers carries the admin role in the row,
//! starts confirmed and approved whatever the registration modes say,
//! and logs in with the password the command read from stdin. The
//! argument parser is unit-tested in `src/cli.rs`; this is the database
//! half.

#![cfg(test)]

use wired_be::{
    cli::{self, CreateAdminArgs},
    config::{
        ACCOUNT_ACTIVATION_REQUIRED_VAR, BASE_URL_VAR, Config, DATABASE_URL_VAR,
        REGISTRATIONS_APPROVAL_REQUIRED_VAR, TEST_BASE_URL, TEST_DATABASE_URL,
    },
    error::AppError,
    models::user::{get_by_nickname, register::authenticate},
};

use crate::common;

/// The arguments of the command line the ticket documents, in one place.
fn admin_args() -> CreateAdminArgs {
    CreateAdminArgs {
        nickname: "chief".to_owned(),
        email: "chief@example.org".to_owned(),
    }
}

/// A config over the fixture's fixed values with the two registration
/// gates moved: what the bootstrapped account must shrug off.
fn gated_config() -> Config {
    Config::from_lookup(|key| match key {
        DATABASE_URL_VAR => Some(TEST_DATABASE_URL.to_owned()),
        BASE_URL_VAR => Some(TEST_BASE_URL.to_owned()),
        REGISTRATIONS_APPROVAL_REQUIRED_VAR | ACCOUNT_ACTIVATION_REQUIRED_VAR => {
            Some("true".to_owned())
        }
        _ => None,
    })
    .expect("the fixed gated config is valid")
}

/// The success path: `create_admin` registers through
/// `create_local_user`, writes `is_admin` on the row, and the account
/// reads back as a local, confirmed, active administrator whose password
/// authenticates.
#[tokio::test]
async fn create_admin_grants_the_admin_role() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = common::test_app_state(pool.clone(), common::test_config());
    let user = cli::create_admin(&state, &admin_args(), "bootstrap-password")
        .await
        .expect("create-admin succeeds");
    assert!(user.is_admin.get(), "the returned row carries the role");
    let stored = get_by_nickname(&pool, "chief")
        .await
        .expect("the nickname lookup runs")
        .expect("the admin row exists");
    assert!(stored.is_admin.get(), "the row stores is_admin = true");
    assert!(!stored.is_moderator.get(), "only the admin role moved");
    assert!(stored.local.get(), "the account is local");
    assert_eq!(
        stored.email.as_deref(),
        Some("chief@example.org"),
        "the address the command named"
    );
    assert!(
        stored.email_confirmed.get(),
        "the operator's address is confirmed"
    );
    assert!(
        !stored.approval_pending.get(),
        "an admin does not await approval"
    );
    assert!(stored.is_active.get(), "the account is active");
    let logged_in = authenticate(&pool, "chief", "bootstrap-password").await;
    assert!(logged_in.is_some(), "the admin logs in with its password");
}

/// The bootstrap bypasses the registration gates: on an instance with
/// `REGISTRATIONS_APPROVAL_REQUIRED` and `ACCOUNT_ACTIVATION_REQUIRED`
/// both on — where every `POST /api/v1/accounts` waits for staff and a
/// confirmation link — the account `create_admin` creates is approved
/// and confirmed on arrival, because whoever ran the command already
/// holds database access.
#[tokio::test]
async fn create_admin_bootstraps_through_the_registration_gates() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = common::test_app_state(pool.clone(), gated_config());
    let admin = cli::create_admin(&state, &admin_args(), "bootstrap-password")
        .await
        .expect("create-admin succeeds on a gated instance");
    assert!(admin.is_admin.get(), "the returned row carries the role");
    let stored = get_by_nickname(&pool, "chief")
        .await
        .expect("the nickname lookup runs")
        .expect("the admin row exists");
    assert!(stored.is_admin.get(), "the role is there");
    assert!(
        !stored.approval_pending.get(),
        "the approval the gated registration stores is lifted"
    );
    assert!(
        stored.email_confirmed.get(),
        "the activation gate leaves the operator's address confirmed"
    );
    assert!(
        authenticate(&pool, "chief", "bootstrap-password")
            .await
            .is_some(),
        "and the admin can log in"
    );
}

/// A taken nickname is refused the way the registration path refuses it
/// for every other caller: the field-error map, encoded as the 422 body
/// value it would carry over HTTP.
#[tokio::test]
async fn create_admin_refuses_a_taken_nickname() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = common::test_app_state(pool.clone(), common::test_config());
    let existing = common::create_user_with_password(&pool, "chief", "someone-elses-pw").await;
    assert!(
        !existing.is_admin.get(),
        "the fixture account starts as an ordinary user"
    );
    let Err(err) = cli::create_admin(&state, &admin_args(), "bootstrap-password").await else {
        panic!("a taken nickname must be refused");
    };
    let AppError::Unprocessable(detail) = err else {
        panic!("a refused registration answers 422, got: {err}");
    };
    assert!(
        detail.contains("has already been taken"),
        "the field error names the collision: {detail}"
    );
}

/// A password below `MIN_PASSWORD_LENGTH` is refused by the same
/// registration validation the endpoint enforces, and nothing is stored.
#[tokio::test]
async fn create_admin_refuses_a_short_password() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = common::test_app_state(pool.clone(), common::test_config());
    let Err(err) = cli::create_admin(&state, &admin_args(), "short").await else {
        panic!("a short password must be refused");
    };
    let AppError::Unprocessable(detail) = err else {
        panic!("a refused registration answers 422, got: {err}");
    };
    assert!(
        detail.contains("password"),
        "the field error names the password: {detail}"
    );
    let stored = get_by_nickname(&pool, "chief")
        .await
        .expect("the nickname lookup runs");
    assert!(stored.is_none(), "the refused account stored no row");
}

/// A second call on the same state succeeds: `create_admin` reuses the
/// pool and config without exhausting anything.
#[tokio::test]
async fn create_admin_runs_twice_on_one_state() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = common::test_app_state(pool.clone(), common::test_config());
    let first = cli::create_admin(&state, &admin_args(), "bootstrap-password")
        .await
        .expect("the first admin succeeds");
    assert!(first.is_admin.get(), "the first call granted the role");
    let second = CreateAdminArgs {
        nickname: "deputy".to_owned(),
        email: "deputy@example.org".to_owned(),
    };
    let deputy = cli::create_admin(&state, &second, "deputy-password")
        .await
        .expect("a second admin succeeds on the same state");
    assert!(deputy.is_admin.get(), "the role rides on the row");
}
