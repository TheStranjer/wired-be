//! `POST /oauth/authorize` (Pleroma's `create_authorization`): the code
//! redirect with its exact `Location` (state round trip and an existing
//! query string included), the out-of-band code page, the failed-login
//! re-render, the denial redirect and the stored, single-use code row.

use axum::http::StatusCode;
use wired_be::models::oauth_token::{self, OauthAuthorization};

use super::authorize_get::{
    assert_cache_no_store, authorization_count, consent_form, oob_page_code, query_pairs,
    query_param, register_app,
};
use super::support::{OOB, PASSWORD, REDIRECT, error_text, fixtures};
use crate::common;

/// A correct login approves: the reply is a 302 whose `Location` is the
/// registered URI plus `code`, and the code is a fresh, unused,
/// single-use row of `oauth_authorizations`.
#[tokio::test]
async fn post_authorize_redirects_with_a_stored_code() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (user, app) = fixtures(&pool, "tina", "read write").await;
    let http = common::test_app(pool.clone());
    let response = common::TestRequest::post("/oauth/authorize")
        .form(&consent_form(
            &app,
            "tina",
            PASSWORD,
            &["read", "write"],
            None,
        ))
        .send(&http)
        .await;

    assert_eq!(response.status, StatusCode::FOUND, "a 302 redirect");
    assert_cache_no_store(&response);
    let location = response.header("location").expect("a Location header");
    let code = query_param(location, "code").expect("the code parameter");
    assert_eq!(code.len(), 43, "the 43-char Pleroma code shape");
    assert_eq!(
        location,
        format!("{REDIRECT}?code={code}"),
        "the exact Location, nothing else asked for"
    );
    assert!(
        query_param(location, "state").is_none(),
        "no state was sent, none comes back"
    );

    let fresh: OauthAuthorization =
        sqlx::query_as("SELECT * FROM oauth_authorizations WHERE token = $1")
            .bind(&code)
            .fetch_one(&pool)
            .await
            .expect("the code row is stored");
    assert!(!fresh.used, "a fresh code is unused");
    assert_eq!(fresh.user_id, user.id, "granted by the logging-in user");
    assert_eq!(fresh.app_id, app.id, "for the validated app");
    assert_eq!(
        fresh.scopes, "read write",
        "the checked boxes are what got stored"
    );

    let redeemed = oauth_token::consume_authorization(&pool, &code, app.id)
        .await
        .expect("the exchange reaches the database")
        .expect("the owning app can redeem the code");
    assert!(redeemed.used, "redemption flips the flag");
    assert!(
        oauth_token::consume_authorization(&pool, &code, app.id)
            .await
            .expect("the second attempt reaches the database")
            .is_none(),
        "and the code is spent"
    );
}

/// The `state` survives both trips: form field into the re-render,
/// re-render into the redirect, urlencoded characters included.
#[tokio::test]
async fn post_authorize_round_trips_the_state() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_user, app) = fixtures(&pool, "ursula", "read").await;
    let http = common::test_app(pool.clone());
    let response = common::TestRequest::post("/oauth/authorize")
        .form(&consent_form(
            &app,
            "ursula",
            PASSWORD,
            &["read"],
            Some("a b&c=d"),
        ))
        .send(&http)
        .await;

    assert_eq!(response.status, StatusCode::FOUND, "a 302 redirect");
    let location = response.header("location").expect("a Location header");
    let code = query_param(location, "code").expect("the code parameter");
    assert_eq!(
        query_param(location, "state").as_deref(),
        Some("a b&c=d"),
        "the state decodes back verbatim"
    );
    assert_eq!(
        location,
        format!("{REDIRECT}?code={code}&state=a+b%26c%3Dd"),
        "and its exact encoded place in the Location"
    );
}

/// A registered redirect URI that already carries a query keeps it: the
/// old pairs come first, `code` is appended and a re-sent `state`
/// replaces the stored one (Pleroma's `modify_uri_params`).
#[tokio::test]
async fn post_authorize_respects_an_existing_query_string() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let registered = &format!("{REDIRECT}?foo=1&state=old");
    let app = register_app(&pool, "Query Client", registered, "read").await;
    let _user = common::create_user_with_password(&pool, "vera", PASSWORD).await;
    let http = common::test_app(pool.clone());
    let mut fields = consent_form(&app, "vera", PASSWORD, &["read"], Some("new"));
    fields[1] = ("authorization[redirect_uri]", registered.as_str());
    let response = common::TestRequest::post("/oauth/authorize")
        .form(&fields)
        .send(&http)
        .await;

    assert_eq!(response.status, StatusCode::FOUND, "a 302 redirect");
    let location = response.header("location").expect("a Location header");
    let code = query_param(location, "code").expect("the code parameter");
    let pairs = query_pairs(location);
    let keys: Vec<&str> = pairs.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        keys,
        ["foo", "code", "state"],
        "existing pairs keep their place, new keys append: {location}"
    );
    assert_eq!(
        query_param(location, "state").as_deref(),
        Some("new"),
        "the form's state replaced the stored query's"
    );
    assert_eq!(
        location,
        format!("{REDIRECT}?foo=1&code={code}&state=new"),
        "the exact Location"
    );
}

/// The out-of-band client gets no redirect: its page shows the code,
/// and the code is stored like any other.
#[tokio::test]
async fn post_authorize_renders_the_code_for_oob_clients() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let user = common::create_user_with_password(&pool, "wilma", PASSWORD).await;
    let oob_app = register_app(&pool, "Console Client", OOB, "read").await;
    let http = common::test_app(pool.clone());
    let mut fields = consent_form(&oob_app, "wilma", PASSWORD, &["read"], None);
    fields[1] = ("authorization[redirect_uri]", OOB);
    let response = common::TestRequest::post("/oauth/authorize")
        .form(&fields)
        .send(&http)
        .await;

    assert_eq!(
        response.status,
        StatusCode::OK,
        "the oob client gets a page, not a redirect"
    );
    assert_eq!(
        response.header("content-type"),
        Some("text/html; charset=utf-8"),
        "as HTML"
    );
    assert_cache_no_store(&response);
    assert!(response.header("location").is_none(), "and no Location");
    let body = response.text();
    assert!(
        body.contains("Successfully authorized"),
        "Pleroma's confirmation title"
    );
    let code = oob_page_code(&body);
    assert_eq!(code.len(), 43, "the 43-char code, verbatim on the page");
    let stored: OauthAuthorization =
        sqlx::query_as("SELECT * FROM oauth_authorizations WHERE token = $1")
            .bind(code)
            .fetch_one(&pool)
            .await
            .expect("the shown code is the stored row");
    assert_eq!(stored.user_id, user.id, "granted by the user");
    assert_eq!(stored.app_id, oob_app.id, "for the oob app");
}

/// Wrong credentials do not fail the request: the page re-renders, at
/// 200, with the error line and its round-trip fields intact — and no
/// code is minted.
#[tokio::test]
async fn post_authorize_bad_password_rerenders_the_form() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_user, app) = fixtures(&pool, "xena", "read write").await;
    let http = common::test_app(pool.clone());
    let response = common::TestRequest::post("/oauth/authorize")
        .form(&consent_form(
            &app,
            "xena",
            "wrong password",
            &["read", "write"],
            Some("keep"),
        ))
        .send(&http)
        .await;

    assert_eq!(
        response.status,
        StatusCode::OK,
        "as Pleroma does: a re-render, not an error status"
    );
    assert_eq!(
        response.header("content-type"),
        Some("text/html; charset=utf-8"),
        "the same page again"
    );
    assert_cache_no_store(&response);
    let body = response.text();
    assert!(
        body.contains("Invalid Username/Password"),
        "with Pleroma's error line"
    );
    assert!(
        body.contains(&format!(
            "name=\"authorization[client_id]\" value=\"{}\">",
            app.client_id
        )),
        "the round-trip fields survived the re-render"
    );
    assert!(
        body.contains("name=\"authorization[state]\" value=\"keep\">"),
        "including the state"
    );
    assert_eq!(
        body.matches("name=\"authorization[scope][]\"").count(),
        2,
        "and both scope boxes"
    );
    assert_eq!(
        authorization_count(&pool).await,
        0,
        "a failed login minted no code"
    );
}

/// Pressing Deny needs no credentials: the redirect carries
/// `error=access_denied` (and the `state`), and no code exists.
#[tokio::test]
async fn post_authorize_deny_redirects_with_access_denied() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_user, app) = fixtures(&pool, "yuri", "read").await;
    let http = common::test_app(pool.clone());
    let mut fields = consent_form(&app, "yuri", "", &[], Some("st1"));
    fields.push(("authorization[deny]", "Deny"));
    let response = common::TestRequest::post("/oauth/authorize")
        .form(&fields)
        .send(&http)
        .await;

    assert_eq!(response.status, StatusCode::FOUND, "the denial redirects");
    assert_cache_no_store(&response);
    let location = response.header("location").expect("a Location header");
    assert_eq!(
        query_param(location, "error").as_deref(),
        Some("access_denied"),
        "with the RFC 6749 error code"
    );
    assert_eq!(
        query_param(location, "state").as_deref(),
        Some("st1"),
        "and the state"
    );
    assert!(query_param(location, "code").is_none(), "but no code");
    assert_eq!(authorization_count(&pool).await, 0, "nothing was minted");
}

/// The `POST` re-runs the `GET`'s checks on the submitted form: an
/// unknown client, an unregistered redirect and tampered scopes all
/// answer their JSON 400, even with correct credentials.
#[tokio::test]
async fn post_authorize_revalidates_the_form() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_user, app) = fixtures(&pool, "zoe", "read").await;
    let http = common::test_app(pool.clone());

    let mut unknown_client = consent_form(&app, "zoe", PASSWORD, &["read"], None);
    unknown_client[0] = ("authorization[client_id]", "no-such-client");
    let client_answer = common::TestRequest::post("/oauth/authorize")
        .form(&unknown_client)
        .send(&http)
        .await;
    assert_eq!(
        client_answer.status,
        StatusCode::BAD_REQUEST,
        "an unknown client cannot post a login"
    );
    assert_eq!(error_text(&client_answer), "Invalid client", "alike");
    assert_cache_no_store(&client_answer);

    let mut wrong_redirect = consent_form(&app, "zoe", PASSWORD, &["read"], None);
    wrong_redirect[1] = ("authorization[redirect_uri]", "https://evil.example/cb");
    let redirect_answer = common::TestRequest::post("/oauth/authorize")
        .form(&wrong_redirect)
        .send(&http)
        .await;
    assert_eq!(
        redirect_answer.status,
        StatusCode::BAD_REQUEST,
        "an unregistered redirect is refused"
    );
    assert_eq!(
        error_text(&redirect_answer),
        "Invalid redirect_uri",
        "by name"
    );

    let tampered = consent_form(&app, "zoe", PASSWORD, &["admin:read"], None);
    let scope_answer = common::TestRequest::post("/oauth/authorize")
        .form(&tampered)
        .send(&http)
        .await;
    assert_eq!(
        scope_answer.status,
        StatusCode::BAD_REQUEST,
        "even a correct login cannot exceed the registration"
    );
    assert_eq!(
        error_text(&scope_answer),
        "Invalid scope",
        "with its refusal"
    );

    let (count,): (i64,) = sqlx::query_as("SELECT count(*) FROM oauth_authorizations")
        .fetch_one(&pool)
        .await
        .expect("the count reaches the database");
    assert_eq!(count, 0, "none of the refusals minted a code");
}
