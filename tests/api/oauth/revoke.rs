//! `POST /oauth/revoke` (Pleroma's `OAuthController.token_revoke`): the
//! deletion of the app's own token and the bearer dying with it, the form
//! and HTTP Basic request shapes, the shared 400 "Invalid credentials"
//! for foreign tokens and failed client authentication, the RFC 7009 `{}`
//! answers for unknown tokens and second revocations, and the "Missing
//! parameter: `token`" refusal. `token_rows` and `probe_user_endpoint`
//! are `pub(super)` because the refresh submodule's rotation tests probe
//! the same rows.

use axum::{Router, http::StatusCode};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use sqlx::PgPool;
use wired_be::models::oauth_app::OauthApp;

use super::support::{assert_no_store, error_text, fixtures};
use crate::common;

/// A revoke body carrying app `app`'s credentials in the plain body
/// fields, asking to retire `token`.
fn revoke_body(app: &OauthApp, token: &str) -> Value {
    json!({
        "token": token,
        "client_id": app.client_id,
        "client_secret": app.client_secret,
    })
}

/// POSTs `body` as JSON to the revocation endpoint.
async fn post_revoke_json(app: &Router, body: &Value) -> common::TestResponse {
    common::TestRequest::post("/oauth/revoke")
        .json(body)
        .send(app)
        .await
}

/// Rows in `oauth_tokens` whose access token is `token`.
pub(super) async fn token_rows(pool: &PgPool, token: &str) -> i64 {
    let (rows,): (i64,) = sqlx::query_as("SELECT count(*) FROM oauth_tokens WHERE token = $1")
        .bind(token)
        .fetch_one(pool)
        .await
        .expect("the count reaches the database");
    rows
}

/// The authenticated endpoint the revoke tests probe: `GET
/// /api/oauth_tokens` answers 200 with a live user token and 401 once
/// the token is revoked.
pub(super) async fn probe_user_endpoint(http: &Router, token: &str) -> common::TestResponse {
    common::TestRequest::get("/api/oauth_tokens")
        .bearer(token)
        .send(http)
        .await
}

/// Revoking the app's own token over JSON deletes the row, answers the
/// empty object with the no-store headers, and the bearer token stops
/// authenticating an authenticated endpoint.
#[tokio::test]
async fn revoke_deletes_the_apps_own_token() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (user, app) = fixtures(&pool, "ada", "read write").await;
    let token = common::create_token(&pool, &user, &app, "read write").await;
    let http = common::test_app(pool.clone());

    let before = probe_user_endpoint(&http, &token.token).await;
    assert_eq!(before.status, StatusCode::OK, "the token works before");

    let response = post_revoke_json(&http, &revoke_body(&app, &token.token)).await;
    assert_eq!(response.status, StatusCode::OK, "the revocation succeeds");
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "the answer is JSON"
    );
    assert_no_store(&response);
    assert_eq!(
        response.json_value(),
        json!({}),
        "Pleroma's empty-object body"
    );

    assert_eq!(token_rows(&pool, &token.token).await, 0, "the row is gone");
    let after = probe_user_endpoint(&http, &token.token).await;
    assert_eq!(
        after.status,
        StatusCode::UNAUTHORIZED,
        "the revoked bearer stops working"
    );
    assert_eq!(
        error_text(&after),
        "The access token is invalid",
        "the 401 wording"
    );
}

/// The same revocation over urlencoded forms, and through `Authorization:
/// Basic` client credentials that win over poisoned body fields.
#[tokio::test]
async fn revoke_accepts_form_bodies_and_basic_client_credentials() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (user, app) = fixtures(&pool, "ben", "read").await;
    let http = common::test_app(pool.clone());

    let token = common::create_token(&pool, &user, &app, "read").await;
    let form_answer = common::TestRequest::post("/oauth/revoke")
        .form(&[
            ("token", token.token.as_str()),
            ("client_id", app.client_id.as_str()),
            ("client_secret", app.client_secret.as_str()),
        ])
        .send(&http)
        .await;
    assert_eq!(form_answer.status, StatusCode::OK, "the form body revokes");
    assert_no_store(&form_answer);
    assert_eq!(token_rows(&pool, &token.token).await, 0, "the row is gone");

    let fresh = common::create_token(&pool, &user, &app, "read").await;
    let basic = STANDARD.encode(format!("{}:{}", app.client_id, app.client_secret));
    let basic_answer = common::TestRequest::post("/oauth/revoke")
        .header("authorization", &format!("Basic {basic}"))
        .form(&[
            ("token", fresh.token.as_str()),
            ("client_id", "not-this-client"),
            ("client_secret", "not-this-secret"),
        ])
        .send(&http)
        .await;
    assert_eq!(
        basic_answer.status,
        StatusCode::OK,
        "the Basic pair authenticates the app"
    );
    assert_eq!(
        token_rows(&pool, &fresh.token).await,
        0,
        "and the header wins over the poisoned body"
    );
}

/// A token issued to another app is refused the shared 400 "Invalid
/// credentials" and stays alive; wrong secrets, unknown clients and
/// absent credentials answer the same.
#[tokio::test]
async fn revoke_refuses_foreign_tokens_and_bad_credentials() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (user, owner) = fixtures(&pool, "cleo", "read").await;
    let stranger = common::create_app(&pool, "read").await;
    let token = common::create_token(&pool, &user, &owner, "read").await;
    let http = common::test_app(pool.clone());

    let foreign = post_revoke_json(&http, &revoke_body(&stranger, &token.token)).await;
    assert_eq!(
        foreign.status,
        StatusCode::BAD_REQUEST,
        "another app's token is a 400"
    );
    assert_eq!(
        error_text(&foreign),
        "Invalid credentials",
        "Pleroma's shared refusal"
    );
    assert_no_store(&foreign);

    let mut wrong_secret = revoke_body(&owner, &token.token);
    wrong_secret["client_secret"] = json!("not-the-secret");
    let secret_answer = post_revoke_json(&http, &wrong_secret).await;
    assert_eq!(
        secret_answer.status,
        StatusCode::BAD_REQUEST,
        "a wrong secret is a 400"
    );
    assert_eq!(
        error_text(&secret_answer),
        "Invalid credentials",
        "the same message"
    );

    let mut unknown_client = revoke_body(&owner, &token.token);
    unknown_client["client_id"] = json!("no-such-client");
    let client_answer = post_revoke_json(&http, &unknown_client).await;
    assert_eq!(
        client_answer.status,
        StatusCode::BAD_REQUEST,
        "an unknown client id is a 400"
    );
    assert_eq!(
        error_text(&client_answer),
        "Invalid credentials",
        "the same message"
    );

    let bare = post_revoke_json(&http, &json!({"token": token.token})).await;
    assert_eq!(
        bare.status,
        StatusCode::BAD_REQUEST,
        "absent credentials are a 400"
    );
    assert_eq!(error_text(&bare), "Invalid credentials", "the same message");

    assert_eq!(
        token_rows(&pool, &token.token).await,
        1,
        "none of the refusals deleted the token"
    );
    let probe = probe_user_endpoint(&http, &token.token).await;
    assert_eq!(probe.status, StatusCode::OK, "and it still authenticates");
}

/// An unknown token revokes to nothing: 200 `{}`, no error (RFC 7009
/// §2.2 — the client cannot handle such a failure, and the desired end
/// state already holds).
#[tokio::test]
async fn revoking_an_unknown_token_is_not_an_error() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_user, app) = fixtures(&pool, "dan", "read").await;
    let http = common::test_app(pool.clone());
    let response = post_revoke_json(&http, &revoke_body(&app, "no-such-token")).await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "an invalid token is not an error"
    );
    assert_eq!(response.json_value(), json!({}), "the same empty object");
    assert_no_store(&response);
}

/// Without the `token` parameter the request is refused by name before
/// the credentials are looked at.
#[tokio::test]
async fn a_missing_token_parameter_is_named() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_user, app) = fixtures(&pool, "eve", "read").await;
    let http = common::test_app(pool.clone());

    let response = post_revoke_json(
        &http,
        &json!({"client_id": app.client_id, "client_secret": app.client_secret}),
    )
    .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST, "a 400");
    assert_eq!(
        error_text(&response),
        "Missing parameter: token",
        "naming the missing parameter"
    );
    assert_no_store(&response);

    // The parameter gate runs first: broken credentials cannot change
    // the answer.
    let both_bad = post_revoke_json(
        &http,
        &json!({"client_id": "no-such-client", "client_secret": "nope"}),
    )
    .await;
    assert_eq!(both_bad.status, StatusCode::BAD_REQUEST, "still a 400");
    assert_eq!(
        error_text(&both_bad),
        "Missing parameter: token",
        "with the same message"
    );

    // A body without the key at all, form or JSON, is "missing", not a 422.
    let empty = common::TestRequest::post("/oauth/revoke")
        .form(&[("scope", "read")])
        .send(&http)
        .await;
    assert_eq!(
        empty.status,
        StatusCode::BAD_REQUEST,
        "an unrelated form body"
    );
    assert_eq!(
        error_text(&empty),
        "Missing parameter: token",
        "gets the same answer"
    );
}

/// The second revocation of the same token sees an unknown one and
/// answers 200 `{}` again.
#[tokio::test]
async fn revoking_twice_succeeds_both_times() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (user, app) = fixtures(&pool, "gina", "read").await;
    let token = common::create_token(&pool, &user, &app, "read").await;
    let http = common::test_app(pool.clone());
    let body = revoke_body(&app, &token.token);

    let first = post_revoke_json(&http, &body).await;
    assert_eq!(first.status, StatusCode::OK, "the first revocation works");
    let second = post_revoke_json(&http, &body).await;
    assert_eq!(
        second.status,
        StatusCode::OK,
        "the second sees an unknown token"
    );
    assert_eq!(second.json_value(), json!({}), "and answers alike");
    assert_no_store(&second);
}
