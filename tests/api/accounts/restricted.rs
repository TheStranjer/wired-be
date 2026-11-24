//! `RESTRICT_UNAUTHENTICATED` on the account reads: `show`, `lookup`,
//! `followers` and `following` refuse an anonymous request to a closed
//! profile half with the 403 `USER_REQUIRED`, the other half and every
//! presented user token unaffected.

use axum::{Router, http::StatusCode};
use sqlx::PgPool;
use wired_be::{
    config::{self, Config},
    models::user::User,
};

use super::support::USER_REQUIRED;
use crate::common;

/// The four account reads the restriction guards, as request paths for
/// `account` (`lookup` by its stored handle `acct`).
fn read_paths(account: &User, acct: &str) -> [String; 4] {
    let id = &account.id;
    [
        format!("/api/v1/accounts/{id}"),
        format!("/api/v1/accounts/lookup?acct={acct}"),
        format!("/api/v1/accounts/{id}/followers"),
        format!("/api/v1/accounts/{id}/following"),
    ]
}

/// The real router over `pool` with `RESTRICT_UNAUTHENTICATED` set to
/// `areas`, built through [`Config::from_lookup`].
///
/// # Panics
///
/// Never for the fixed area lists the tests pass.
fn restricted_app(pool: PgPool, areas: &str) -> Router {
    let config = Config::from_lookup(|key| match key {
        config::DATABASE_URL_VAR => Some(config::TEST_DATABASE_URL.to_owned()),
        config::BASE_URL_VAR => Some(config::TEST_BASE_URL.to_owned()),
        config::RATE_LIMITS_ENABLED_VAR => Some("false".to_owned()),
        config::RESTRICT_UNAUTHENTICATED_VAR => Some(areas.to_owned()),
        _ => None,
    })
    .expect("the fixed restriction config is valid");
    common::test_app_with_config(pool, config)
}

/// `GET path`, with `token` as bearer when given.
async fn get(app: &Router, path: &str, token: Option<&str>) -> common::TestResponse {
    let request = common::TestRequest::get(path);
    match token {
        Some(token) => request.bearer(token).send(app).await,
        None => request.send(app).await,
    }
}

/// Every path of `paths` answers `expected` to `token`, and a 403 carries
/// the authenticated-user body.
async fn assert_all(
    app: &Router,
    paths: &[String],
    token: Option<&str>,
    expected: StatusCode,
    what: &str,
) {
    for path in paths {
        let response = get(app, path, token).await;
        assert_eq!(response.status, expected, "{what}: {path}");
        if expected == StatusCode::FORBIDDEN {
            assert_eq!(response.text(), USER_REQUIRED, "{what}: {path} body");
        }
    }
}

/// `profiles_local` closes the local account to an anonymous reader on all
/// four reads, leaves the mirror open, and never refuses a user token.
#[tokio::test]
async fn profiles_local_refuses_anonymous_reads_of_a_local_account() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let token = common::create_app_and_token(&pool, &alice, "read:accounts").await;
    let app = restricted_app(pool, "profiles_local");
    let local = read_paths(&alice, "alice");
    let remote = read_paths(&bob, "bob@remote.example");

    assert_all(
        &app,
        &local,
        None,
        StatusCode::FORBIDDEN,
        "anonymous, local",
    )
    .await;
    assert_all(&app, &remote, None, StatusCode::OK, "anonymous, mirror").await;
    assert_all(&app, &local, Some(&token), StatusCode::OK, "user, local").await;
    assert_all(&app, &remote, Some(&token), StatusCode::OK, "user, mirror").await;
}

/// `profiles_remote` closes the mirror and says nothing about the local
/// account.
#[tokio::test]
async fn profiles_remote_refuses_anonymous_reads_of_a_mirror() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let token = common::create_app_and_token(&pool, &alice, "read:accounts").await;
    let app = restricted_app(pool, "profiles_remote");
    let local = read_paths(&alice, "alice");
    let remote = read_paths(&bob, "bob@remote.example");

    assert_all(
        &app,
        &remote,
        None,
        StatusCode::FORBIDDEN,
        "anonymous, mirror",
    )
    .await;
    assert_all(&app, &local, None, StatusCode::OK, "anonymous, local").await;
    assert_all(&app, &remote, Some(&token), StatusCode::OK, "user, mirror").await;
}

/// An open instance answers every anonymous read, and an unknown id is
/// still the 404 (the restriction sits behind the existence check).
#[tokio::test]
async fn an_open_instance_and_a_miss_are_unaffected() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let open = common::test_app(pool.clone());
    let app = restricted_app(pool, "profiles_local");

    assert_all(
        &open,
        &read_paths(&alice, "alice"),
        None,
        StatusCode::OK,
        "open instance",
    )
    .await;
    let missing = get(&app, "/api/v1/accounts/lookup?acct=nobody", None).await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND, "an unknown handle");
}
