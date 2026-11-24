//! The `authorization_code` grant's `redirect_uri` binding (RFC 6749
//! §4.1.3): a code redeems only with the redirect URI it was issued for,
//! a refused exchange leaves the code usable, and a missing URI is as
//! wrong as a different one.

use axum::http::StatusCode;
use serde_json::{Value, json};
use wired_be::models::oauth_app::OauthApp;

use super::authorize_get::{issue_code, register_app};
use super::support::{PASSWORD, REDIRECT, post_json};
use crate::common;

/// A second URI the same client registers, never the one a code is issued for.
const OTHER_REDIRECT: &str = "https://client.example/other";

/// The exchange body for `code`, presenting `redirect_uri` when given.
fn exchange_body(app: &OauthApp, code: &str, redirect_uri: Option<&str>) -> Value {
    let mut body = json!({
        "grant_type": "authorization_code",
        "code": code,
        "client_id": app.client_id,
        "client_secret": app.client_secret,
    });
    if let Some(uri) = redirect_uri {
        body["redirect_uri"] = json!(uri);
    }
    body
}

/// A code issued for URI A redeems with A, is refused with B and without
/// any URI, and the refusals do not burn it: a retry with A still works.
#[tokio::test]
async fn a_code_redeems_only_with_the_redirect_uri_it_was_issued_for() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let rhea = common::create_user_with_password(&pool, "rhea", PASSWORD).await;
    let app = register_app(
        &pool,
        "Two URIs",
        &format!("{REDIRECT} {OTHER_REDIRECT}"),
        "read",
    )
    .await;
    let http = common::test_app(pool.clone());
    let code = issue_code(&pool, &rhea, &app, "read", REDIRECT).await;

    let other = post_json(&http, &exchange_body(&app, &code, Some(OTHER_REDIRECT))).await;
    assert_eq!(
        other.status,
        StatusCode::BAD_REQUEST,
        "another registered URI is refused"
    );
    assert_eq!(other.error_text(), "Invalid credentials", "the shared 400");

    let missing = post_json(&http, &exchange_body(&app, &code, None)).await;
    assert_eq!(
        missing.status,
        StatusCode::BAD_REQUEST,
        "no redirect_uri at all is refused"
    );
    assert_eq!(
        missing.error_text(),
        "Invalid credentials",
        "the shared 400"
    );

    let matching = post_json(&http, &exchange_body(&app, &code, Some(REDIRECT))).await;
    assert_eq!(
        matching.status,
        StatusCode::OK,
        "the refusals left the code unspent; the issued URI redeems it"
    );
    assert_eq!(matching.json_value()["scope"], "read", "with its scopes");

    let replay = post_json(&http, &exchange_body(&app, &code, Some(REDIRECT))).await;
    assert_eq!(
        replay.status,
        StatusCode::BAD_REQUEST,
        "and it is single-use"
    );
}
