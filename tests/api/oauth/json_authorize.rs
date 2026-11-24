//! The JSON half of `/oauth/authorize` (WBE-666): `GET` with
//! `Accept: application/json` describing the request, and `POST` with a
//! trusted app's bearer token approving or denying it — the successes,
//! the 400/401/403 refusals, the out-of-band answer, the code's redemption
//! at `POST /oauth/token` and the browser redirect that stays apart.

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use wired_be::models::{oauth_app::OauthApp, oauth_token};

use super::authorize_get::{authorization_count, query_param, register_app};
use super::support::{OOB, REDIRECT, fixtures, post_json};
use crate::common;

/// Marks `app` trusted straight in the table.
async fn trust(pool: &PgPool, app: &OauthApp) {
    let trusted = sqlx::query("UPDATE oauth_apps SET trusted = true WHERE id = $1")
        .bind(app.id)
        .execute(pool)
        .await
        .expect("the trusted flag is set");
    assert_eq!(trusted.rows_affected(), 1, "the app row is marked trusted");
}

/// A token with `token_scopes` from a trusted "frontend" app for a fresh
/// user `nickname`, plus a third-party app registered for `app_scopes`.
async fn frontend_token(
    pool: &PgPool,
    nickname: &str,
    token_scopes: &str,
    app_scopes: &str,
) -> (String, OauthApp) {
    let (user, third_party) = fixtures(pool, nickname, app_scopes).await;
    let frontend = common::create_app(pool, "read write follow").await;
    trust(pool, &frontend).await;
    let token = common::create_token(pool, &user, &frontend, token_scopes).await;
    (token.token, third_party)
}

/// `GET /oauth/authorize` as JSON for `client_id` asking `scope`.
async fn get_json(http: &Router, client_id: &str, redirect: &str, scope: &str) -> Value {
    let (status, body) = get_json_status(http, client_id, redirect, scope).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

/// The status and JSON body of [`get_json`]'s request.
async fn get_json_status(
    http: &Router,
    client_id: &str,
    redirect: &str,
    scope: &str,
) -> (StatusCode, Value) {
    let path = format!(
        "/oauth/authorize?response_type=code&client_id={client_id}&redirect_uri={redirect}&scope={scope}&state=st%201"
    );
    let response = common::TestRequest::get(&path)
        .header("accept", "application/json")
        .send(http)
        .await;
    assert_eq!(
        response.header("cache-control"),
        Some("no-store"),
        "never cacheable"
    );
    (response.status, response.json_value())
}

/// POSTs `body` as JSON, with `token` as the bearer when given.
async fn post_verdict(http: &Router, token: Option<&str>, body: &Value) -> common::TestResponse {
    let request = common::TestRequest::post("/oauth/authorize").json(body);
    match token {
        Some(bearer) => request.bearer(bearer).send(http).await,
        None => request.send(http).await,
    }
}

/// The verdict body for `app` asking `scope`, approved or not.
fn verdict(app: &OauthApp, redirect: &str, scope: &str, approve: bool) -> Value {
    json!({
        "client_id": app.client_id,
        "redirect_uri": redirect,
        "scope": scope,
        "state": "st 1",
        "approve": approve,
    })
}

/// A valid request is described with the app's name, scopes, redirect URI
/// and state.
#[tokio::test]
async fn json_get_describes_a_valid_request() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = register_app(&pool, "Third Party", REDIRECT, "read write").await;
    let http = common::test_app(pool);
    let body = get_json(&http, &app.client_id, REDIRECT, "read+write").await;
    assert_eq!(body["app"]["name"], "Third Party", "the app name");
    assert_eq!(body["app"]["website"], Value::Null, "no website registered");
    assert_eq!(body["scopes"], json!(["read", "write"]), "the scopes");
    assert_eq!(body["redirect_uri"], REDIRECT, "the redirect URI");
    assert_eq!(body["state"], "st 1", "the state");
}

/// Unknown clients, unregistered redirects and out-of-registration scopes
/// are 400s with OAuth error codes, never redirects.
#[tokio::test]
async fn json_get_refuses_bad_requests() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = register_app(&pool, "Third Party", REDIRECT, "read").await;
    let http = common::test_app(pool);

    let (client_status, client_body) = get_json_status(&http, "nobody", REDIRECT, "read").await;
    assert_eq!(client_status, StatusCode::BAD_REQUEST, "unknown client");
    assert_eq!(client_body["error"], "invalid_client", "its code");
    assert_eq!(
        client_body["error_description"], "Invalid client",
        "its text"
    );

    let other = "https://elsewhere.example/cb";
    let (redirect_status, redirect_body) =
        get_json_status(&http, &app.client_id, other, "read").await;
    assert_eq!(
        redirect_status,
        StatusCode::BAD_REQUEST,
        "unregistered redirect"
    );
    assert_eq!(redirect_body["error"], "invalid_request", "its code");

    let (scope_status, scope_body) =
        get_json_status(&http, &app.client_id, REDIRECT, "write").await;
    assert_eq!(
        scope_status,
        StatusCode::BAD_REQUEST,
        "scope outside the app"
    );
    assert_eq!(scope_body["error"], "invalid_scope", "its code");

    let response = common::TestRequest::get("/oauth/authorize?client_id=x")
        .header("accept", "application/json")
        .send(&http)
        .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST, "no response_type");
    assert_eq!(
        response.json_value()["error"],
        "invalid_request",
        "its code"
    );
}

/// A browser `GET` (no JSON `Accept`) is redirected to the frontend, not
/// answered with the JSON description or a page.
#[tokio::test]
async fn browser_get_is_redirected_not_described() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = register_app(&pool, "Third Party", REDIRECT, "read").await;
    let http = common::test_app(pool);
    let path = format!(
        "/oauth/authorize?response_type=code&client_id={}&redirect_uri={REDIRECT}",
        app.client_id
    );
    let response = common::TestRequest::get(&path)
        .header("accept", "text/html,application/xhtml+xml,*/*;q=0.8")
        .send(&http)
        .await;
    assert_eq!(response.status, StatusCode::FOUND, "the redirect");
    assert!(
        response
            .header("location")
            .is_some_and(|value| value.contains("/oauth/authorize?")),
        "to the frontend's consent screen"
    );
}

/// Approval by a trusted app's token yields a redirect carrying `code` and
/// `state`, and the code redeems for the requested app.
#[tokio::test]
async fn json_post_approves_and_the_code_redeems() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (token, app) = frontend_token(&pool, "tina", "read write", "read write").await;
    let http = common::test_app(pool.clone());

    let response = post_verdict(&http, Some(&token), &verdict(&app, REDIRECT, "read", true)).await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.text());
    assert_eq!(response.header("cache-control"), Some("no-store"));
    let redirect = response.json_value()["redirect_uri"]
        .as_str()
        .expect("a redirect_uri")
        .to_owned();
    assert!(redirect.starts_with(&format!("{REDIRECT}?")), "{redirect}");
    assert_eq!(query_param(&redirect, "state").as_deref(), Some("st 1"));
    let code = query_param(&redirect, "code").expect("a code");

    let grant = json!({
        "grant_type": "authorization_code",
        "code": code,
        "client_id": app.client_id,
        "client_secret": app.client_secret,
        "redirect_uri": REDIRECT,
    });
    let exchanged = post_json(&http, &grant).await;
    assert_eq!(exchanged.status, StatusCode::OK, "{}", exchanged.text());
    assert_eq!(
        exchanged.json_value()["scope"],
        "read",
        "the approved scope"
    );
}

/// The out-of-band URI answers the bare code.
#[tokio::test]
async fn json_post_oob_answers_the_code() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (token, _) = frontend_token(&pool, "oona", "read", "read").await;
    let oob_app = register_app(&pool, "Oob", &format!("{REDIRECT} {OOB}"), "read").await;
    let http = common::test_app(pool);
    let response = post_verdict(&http, Some(&token), &verdict(&oob_app, OOB, "read", true)).await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.text());
    let body = response.json_value();
    assert_eq!(body["code"].as_str().map(str::len), Some(43), "{body}");
    assert!(body.get("redirect_uri").is_none(), "no redirect for oob");
}

/// A denial redirects with `error=access_denied` and mints no code.
#[tokio::test]
async fn json_post_denial_redirects_with_access_denied() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (token, app) = frontend_token(&pool, "dina", "read", "read").await;
    let http = common::test_app(pool.clone());
    let response = post_verdict(&http, Some(&token), &verdict(&app, REDIRECT, "read", false)).await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.text());
    let redirect = response.json_value()["redirect_uri"]
        .as_str()
        .expect("a redirect_uri")
        .to_owned();
    assert_eq!(
        query_param(&redirect, "error").as_deref(),
        Some("access_denied")
    );
    assert_eq!(query_param(&redirect, "state").as_deref(), Some("st 1"));
    assert!(query_param(&redirect, "code").is_none(), "no code");
    assert_eq!(authorization_count(&pool).await, 0, "nothing minted");
}

/// A token of an untrusted app, a token short of a requested scope, a
/// missing token and a revoked token are each refused, minting nothing.
#[tokio::test]
async fn json_post_refuses_untrusted_apps_missing_scopes_and_dead_tokens() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (user, app) = fixtures(&pool, "rhea", "read write").await;
    let untrusted = common::create_app(&pool, "read write").await;
    let untrusted_token = common::create_token(&pool, &user, &untrusted, "read write").await;
    let (narrow, _) = frontend_token(&pool, "nell", "read", "read write").await;
    let rex = common::create_app(&pool, "read").await;
    trust(&pool, &rex).await;
    let dead = common::create_token(&pool, &user, &rex, "read").await;
    oauth_token::delete_token(&pool, dead.id)
        .await
        .expect("the token is revoked");
    let http = common::test_app(pool.clone());
    let ask = verdict(&app, REDIRECT, "read", true);

    let untrusted_response = post_verdict(&http, Some(&untrusted_token.token), &ask).await;
    assert_eq!(
        untrusted_response.status,
        StatusCode::FORBIDDEN,
        "untrusted app"
    );
    assert_eq!(
        untrusted_response.json_value()["error"],
        "unauthorized_client"
    );

    let wants_write = verdict(&app, REDIRECT, "read write", true);
    let narrow_response = post_verdict(&http, Some(&narrow), &wants_write).await;
    assert_eq!(
        narrow_response.status,
        StatusCode::FORBIDDEN,
        "scope escalation"
    );
    assert_eq!(narrow_response.json_value()["error"], "invalid_scope");

    let anonymous = post_verdict(&http, None, &ask).await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED, "no token");

    let revoked = post_verdict(&http, Some(&dead.token), &ask).await;
    assert_eq!(revoked.status, StatusCode::UNAUTHORIZED, "revoked token");

    let unknown = post_verdict(&http, Some("not-a-token"), &ask).await;
    assert_eq!(unknown.status, StatusCode::UNAUTHORIZED, "unknown token");

    assert_eq!(authorization_count(&pool).await, 0, "nothing was minted");
}

/// The shared validation applies to the verdict too: 400s with OAuth codes.
#[tokio::test]
async fn json_post_validates_the_request() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (token, app) = frontend_token(&pool, "vera", "read write", "read").await;
    let http = common::test_app(pool.clone());

    let unknown = json!({ "client_id": "nobody", "redirect_uri": REDIRECT, "approve": true });
    let client_response = post_verdict(&http, Some(&token), &unknown).await;
    assert_eq!(client_response.status, StatusCode::BAD_REQUEST);
    assert_eq!(client_response.json_value()["error"], "invalid_client");

    let bad_redirect = verdict(&app, "https://elsewhere.example/cb", "read", true);
    let redirect_response = post_verdict(&http, Some(&token), &bad_redirect).await;
    assert_eq!(redirect_response.status, StatusCode::BAD_REQUEST);
    assert_eq!(redirect_response.json_value()["error"], "invalid_request");

    let wider = verdict(&app, REDIRECT, "write", true);
    let scope_response = post_verdict(&http, Some(&token), &wider).await;
    assert_eq!(scope_response.status, StatusCode::BAD_REQUEST);
    assert_eq!(scope_response.json_value()["error"], "invalid_scope");

    let no_verdict = json!({ "client_id": app.client_id, "redirect_uri": REDIRECT });
    let verdict_response = post_verdict(&http, Some(&token), &no_verdict).await;
    assert_eq!(verdict_response.status, StatusCode::BAD_REQUEST);
    assert_eq!(verdict_response.json_value()["error"], "invalid_request");
    assert_eq!(authorization_count(&pool).await, 0, "nothing was minted");
}
