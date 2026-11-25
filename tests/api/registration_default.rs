//! The out-of-the-box registration state: an instance configured without
//! `REGISTRATIONS_OPEN` is closed, and only an explicit `true` opens it.
//!
//! The shared test config opts in to open registration (see
//! [`common::test_config`]), so these tests build their [`Config`] straight
//! from a lookup the way a deployment does.

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::json;
use sqlx::PgPool;
use wired_be::config::{
    BASE_URL_VAR, Config, DATABASE_URL_VAR, RATE_LIMITS_ENABLED_VAR, REGISTRATIONS_OPEN_VAR,
    TEST_BASE_URL, TEST_DATABASE_URL,
};

use crate::common;

/// The scopes the registration fixtures' apps hold.
const FULL_SCOPES: &str = "read write follow push";

/// A [`Config`] read the way a deployment reads it, with
/// `REGISTRATIONS_OPEN` set to `open` when given and unset otherwise.
fn lookup_config(open: Option<&str>) -> Config {
    Config::from_lookup(|key| match key {
        DATABASE_URL_VAR => Some(TEST_DATABASE_URL.to_owned()),
        BASE_URL_VAR => Some(TEST_BASE_URL.to_owned()),
        RATE_LIMITS_ENABLED_VAR => Some("false".to_owned()),
        REGISTRATIONS_OPEN_VAR => open.map(str::to_owned),
        _ => None,
    })
    .expect("the fixed registration default config is valid")
}

/// `POST /api/v1/accounts` for `username` on `app` over a fresh app token.
async fn register(app: &Router, pool: &PgPool, username: &str) -> common::TestResponse {
    let bearer = common::create_app_only_token(pool, FULL_SCOPES).await;
    common::TestRequest::post("/api/v1/accounts")
        .bearer(&bearer)
        .json(&json!({
            "username": username,
            "email": format!("{username}@example.org"),
            "password": "correct horse battery staple",
            "agreement": true,
        }))
        .send(app)
        .await
}

/// With `REGISTRATIONS_OPEN` unset the config is closed.
#[test]
fn unset_registrations_open_is_false() {
    assert!(
        !lookup_config(None).registrations_open.get(),
        "unset means closed"
    );
    assert!(
        lookup_config(Some("true")).registrations_open.get(),
        "an explicit true opens it"
    );
}

/// Without the variable, registration answers the closed 403 and `NodeInfo`
/// reports `openRegistrations: false`.
#[tokio::test]
async fn an_unconfigured_instance_refuses_registration() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app_with_config(pool.clone(), lookup_config(None));

    let response = register(&app, &pool, "lain").await;
    assert_eq!(response.status, StatusCode::FORBIDDEN, "closed by default");
    assert_eq!(
        response.error_text(),
        "Registration is disabled",
        "the closed-registration wording"
    );

    let nodeinfo = common::TestRequest::get("/nodeinfo/2.0").send(&app).await;
    assert_eq!(nodeinfo.status, StatusCode::OK, "NodeInfo answers");
    assert_eq!(
        nodeinfo.json_value()["openRegistrations"],
        json!(false),
        "NodeInfo reports the closed default"
    );
}

/// An explicit `REGISTRATIONS_OPEN=true` still opens registration.
#[tokio::test]
async fn an_explicit_true_opens_registration() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app_with_config(pool.clone(), lookup_config(Some("true")));

    let response = register(&app, &pool, "lain").await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.text());

    let nodeinfo = common::TestRequest::get("/nodeinfo/2.0").send(&app).await;
    assert_eq!(
        nodeinfo.json_value()["openRegistrations"],
        json!(true),
        "NodeInfo reports the opened instance"
    );
}
