//! `POST /oauth/authorize` without the frontend's bearer token and JSON
//! verdict: the retired consent form post, with its
//! `authorization[name]`/`authorization[password]` login, is a 400 that
//! mints nothing and redirects nowhere, whether it approves or denies.
//! The approvals themselves are `json_authorize`'s.

use axum::http::StatusCode;
use serde_json::json;

use super::authorize_get::{assert_cache_no_store, authorization_count};
use super::support::{PASSWORD, REDIRECT, fixtures};
use crate::common;

/// Asserts `response` is the 400 `invalid_request` of a refused
/// `POST /oauth/authorize`: no redirect, no cache.
fn assert_refused(response: &common::TestResponse, what: &str) {
    assert_eq!(response.status, StatusCode::BAD_REQUEST, "{what} is a 400");
    assert!(
        response.header("location").is_none(),
        "{what} redirects nowhere"
    );
    assert_cache_no_store(response);
    assert_eq!(
        response.json_value()["error"],
        "invalid_request",
        "{what} gets the OAuth error code"
    );
}

/// The old consent form, correct credentials and all, no longer logs in
/// or mints a code.
#[tokio::test]
async fn a_form_post_with_a_login_is_refused() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_user, app) = fixtures(&pool, "tina", "read write").await;
    let http = common::test_app(pool.clone());
    let response = common::TestRequest::post("/oauth/authorize")
        .form(&[
            ("authorization[client_id]", app.client_id.as_str()),
            ("authorization[redirect_uri]", REDIRECT),
            ("authorization[name]", "tina"),
            ("authorization[password]", PASSWORD),
            ("authorization[scope][]", "read"),
            ("authorization[state]", "xyz"),
        ])
        .send(&http)
        .await;

    assert_refused(&response, "the login form");
    assert_eq!(authorization_count(&pool).await, 0, "no code minted");
}

/// The form's Deny button needed no credentials, and is refused like
/// the approval: there is no redirect with `access_denied` from here.
#[tokio::test]
async fn a_form_post_deny_is_refused() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_user, app) = fixtures(&pool, "ursula", "read").await;
    let http = common::test_app(pool.clone());
    let response = common::TestRequest::post("/oauth/authorize")
        .form(&[
            ("authorization[client_id]", app.client_id.as_str()),
            ("authorization[redirect_uri]", REDIRECT),
            ("authorization[deny]", "Deny"),
            ("authorization[state]", "xyz"),
        ])
        .send(&http)
        .await;

    assert_refused(&response, "the deny form");
    assert_eq!(authorization_count(&pool).await, 0, "no code minted");
}

/// The form's own shape in a JSON body without a bearer token is the same
/// 400, as is a post with no body at all.
#[tokio::test]
async fn other_posts_without_a_bearer_verdict_are_refused() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_user, app) = fixtures(&pool, "victor", "read").await;
    let http = common::test_app(pool.clone());

    let json_form = common::TestRequest::post("/oauth/authorize")
        .json(&json!({
            "authorization": {
                "name": "victor",
                "password": PASSWORD,
                "client_id": app.client_id,
                "redirect_uri": REDIRECT,
            }
        }))
        .send(&http)
        .await;
    assert_refused(&json_form, "the form shape as JSON");

    let empty = common::TestRequest::post("/oauth/authorize")
        .send(&http)
        .await;
    assert_refused(&empty, "an empty post");
    assert_eq!(authorization_count(&pool).await, 0, "no code minted");
}
