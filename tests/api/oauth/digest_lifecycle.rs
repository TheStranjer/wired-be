//! A token's life with only digests stored: issued by the password grant,
//! used on `verify_credentials`, rotated by the refresh grant and revoked,
//! while `oauth_tokens` never holds a raw credential (CORE-6).

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use wired_be::{models::oauth_app::OauthApp, util::token::sha256_hex};

use super::support::{fixtures, grant_body, post_json};
use crate::common;

/// `GET /api/v1/accounts/verify_credentials` with `token` as the bearer.
async fn verify_credentials(http: &Router, token: &str) -> common::TestResponse {
    common::TestRequest::get("/api/v1/accounts/verify_credentials")
        .bearer(token)
        .send(http)
        .await
}

/// The refresh-grant body for `app` and `refresh_token`.
fn refresh_body(app: &OauthApp, refresh_token: &str) -> Value {
    json!({
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
        "client_id": app.client_id,
        "client_secret": app.client_secret,
    })
}

/// How many `oauth_tokens` rows hold `value` in their `token` or
/// `refresh_token` column, verbatim.
async fn rows_holding(pool: &PgPool, value: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM oauth_tokens WHERE token = $1 OR refresh_token = $1")
        .bind(value)
        .fetch_one(pool)
        .await
        .expect("the count reaches the database")
}

/// The `access_token` and `refresh_token` strings of a token response.
fn pair_of(response: &common::TestResponse) -> (String, String) {
    let body = response.json_value();
    let access = body["access_token"].as_str().expect("an access token");
    let refresh = body["refresh_token"].as_str().expect("a refresh token");
    (access.to_owned(), refresh.to_owned())
}

/// Password grant, bearer use, refresh rotation and revocation all work on
/// the raw values the client holds, and the table keeps only digests.
#[tokio::test]
async fn the_token_lifecycle_works_on_raw_values_while_only_digests_are_stored() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_user, app) = fixtures(&pool, "digester", "read write").await;
    let http = common::test_app(pool.clone());

    let issued = post_json(&http, &grant_body(&app, "digester", "read write")).await;
    assert_eq!(issued.status, StatusCode::OK, "the password grant issues");
    let (access, refresh) = pair_of(&issued);
    assert_eq!(rows_holding(&pool, &access).await, 0, "no raw access token");
    assert_eq!(
        rows_holding(&pool, &refresh).await,
        0,
        "no raw refresh token"
    );
    assert_eq!(
        rows_holding(&pool, &sha256_hex(&access)).await,
        1,
        "the access digest is stored"
    );
    assert_eq!(
        rows_holding(&pool, &sha256_hex(&refresh)).await,
        1,
        "the refresh digest is stored"
    );

    let verified = verify_credentials(&http, &access).await;
    assert_eq!(
        verified.status,
        StatusCode::OK,
        "the raw token authenticates"
    );
    let by_digest = verify_credentials(&http, &sha256_hex(&access)).await;
    assert_eq!(
        by_digest.status,
        StatusCode::UNAUTHORIZED,
        "the stored digest is not a bearer token"
    );

    let rotated = post_json(&http, &refresh_body(&app, &refresh)).await;
    assert_eq!(rotated.status, StatusCode::OK, "the refresh grant rotates");
    let (new_access, new_refresh) = pair_of(&rotated);
    assert_ne!(new_access, access, "a fresh access token");
    assert_ne!(new_refresh, refresh, "a fresh refresh token");
    assert_eq!(
        rows_holding(&pool, &new_access).await,
        0,
        "the new access token is not stored raw either"
    );
    assert_eq!(
        verify_credentials(&http, &new_access).await.status,
        StatusCode::OK,
        "the rotated token works"
    );
    assert_eq!(
        verify_credentials(&http, &access).await.status,
        StatusCode::UNAUTHORIZED,
        "the old access token died with the rotation"
    );

    let revoked = common::TestRequest::post("/oauth/revoke")
        .json(&json!({
            "token": new_access,
            "client_id": app.client_id,
            "client_secret": app.client_secret,
        }))
        .send(&http)
        .await;
    assert_eq!(revoked.status, StatusCode::OK, "the revocation succeeds");
    assert_eq!(
        verify_credentials(&http, &new_access).await.status,
        StatusCode::UNAUTHORIZED,
        "the revoked token is refused"
    );
    assert_eq!(
        post_json(&http, &refresh_body(&app, &new_refresh))
            .await
            .status,
        StatusCode::BAD_REQUEST,
        "the revoked pair's refresh token is dead too"
    );
}
