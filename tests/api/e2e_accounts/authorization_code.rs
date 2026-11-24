//! Scenario 2: the authorization-code flow — the browser's redirect to
//! the frontend's consent screen, the frontend's approval and its code,
//! that code's exchange, the scope gate the approved scopes decide, the
//! spent code's refusal, the rotation and the revocation. The consent URL,
//! the grant posts and the approval themselves live here too: no other
//! scenario reaches the `/oauth` pair.

use axum::http::StatusCode;
use serde_json::{Value, json};

use super::support::{
    AUTHORIZE, CLIENT_SCOPES, INVALID_CREDENTIALS, INVALID_TOKEN, PASSWORD, REVOKE, STATUSES,
    TOKEN, VERIFY, World, assert_refused, string_field,
};
use crate::common::{self, TestClient, TestResponse};

// ───────────────────  Scenario 2: the OAuth code flow  ───────────────────

/// The authorization-code flow end to end: the browser's redirect to the
/// frontend's consent screen, the frontend's approval, the code's exchange
/// for a pair carrying the approved scopes, the spent code's refusal, the
/// rotation that replaces the pair, and the revocation that ends it — with
/// the scope gate proved on the way: a `read` token may read and may not
/// post.
#[tokio::test]
async fn the_authorization_code_flow_issues_rotates_and_retires_a_token() {
    let db = common::test_db().await;
    let world = World::open(db.pool().clone()).await;
    let _bob = world.register("bob").await;

    // Step 1: a valid browser request is sent on to the frontend's
    // consent screen, query intact.
    let page = common::api_get(&world.app, None, &consent_url(&world.client, "read+write")).await;
    common::assert_answer(&page, StatusCode::FOUND, "the consent redirect");
    let frontend = format!("{}{AUTHORIZE}?", world.state.config.frontend_base_url);
    let location = page.header("location").expect("a Location header");
    assert!(
        location.starts_with(&frontend),
        "the browser lands on the frontend's authorize page: {location}"
    );
    assert!(
        location.contains(&format!("client_id={}", world.client.client_id)),
        "with the client the request named: {location}"
    );

    // Step 2: the frontend's approval answers the client's redirect with a code.
    let code = consent(&world, "bob", "read").await;

    // Step 3: the code is redeemed for a pair acting as its authorizer,
    // with the *approved* scopes and nothing else.
    let exchanged = exchange_code(&world, &code).await;
    common::assert_answer(&exchanged, StatusCode::OK, "the code exchange");
    let pair = exchanged.json_value();
    assert_eq!(pair["scope"], "read", "only the box that was checked");
    assert!(
        !pair["refresh_token"].is_null(),
        "a user-bound pair carries its refresh token: {pair}"
    );
    let access = common::access_token_of(&exchanged);

    // Step 4: that token reads the account it was granted for.
    let read = common::api_get(&world.app, Some(&access), VERIFY).await;
    common::assert_answer(&read, StatusCode::OK, "the code token reads");
    assert_eq!(
        string_field(&read.json_value(), "username"),
        "bob",
        "as the account that consented"
    );

    // Step 5: and it may not post, because `read` is the whole grant.
    let refused = common::api_post(
        &world.app,
        Some(&access),
        STATUSES,
        &json!({"status": "should not land"}),
    )
    .await;
    assert_refused(
        &refused,
        StatusCode::FORBIDDEN,
        "Insufficient permissions: write:statuses.",
        "a read-only token posting a status",
    );

    // Step 6: the same code is spent.
    let replay = exchange_code(&world, &code).await;
    assert_refused(
        &replay,
        StatusCode::BAD_REQUEST,
        INVALID_CREDENTIALS,
        "a replayed code",
    );

    // Step 7: the refresh rotates the pair, and the token it replaces dies.
    let rotated = grant(
        &world,
        &json!({"grant_type": "refresh_token", "refresh_token": pair["refresh_token"]}),
    )
    .await;
    common::assert_answer(&rotated, StatusCode::OK, "the refresh grant");
    let next = common::access_token_of(&rotated);
    assert_ne!(next, access, "a fresh access token, not the old one");
    let dead = common::api_get(&world.app, Some(&access), VERIFY).await;
    assert_refused(
        &dead,
        StatusCode::UNAUTHORIZED,
        INVALID_TOKEN,
        "the rotated-out access token",
    );

    // Step 8: revoking the live one ends it too.
    let revoked = grant_at(&world, REVOKE, &json!({"token": next})).await;
    common::assert_answer(&revoked, StatusCode::OK, "the revocation");
    assert_eq!(revoked.json_value(), json!({}), "RFC 7009's empty body");
    let gone = common::api_get(&world.app, Some(&next), VERIFY).await;
    assert_refused(
        &gone,
        StatusCode::UNAUTHORIZED,
        INVALID_TOKEN,
        "the revoked access token",
    );
}

/// The consent URL for `client`, asking `scope` and delivering to the
/// redirect URI it registered.
fn consent_url(client: &TestClient, scope: &str) -> String {
    let redirect = url::form_urlencoded::byte_serialize(common::TEST_REDIRECT_URI.as_bytes())
        .collect::<String>();
    format!(
        "{AUTHORIZE}?response_type=code&client_id={}&redirect_uri={redirect}&scope={scope}",
        client.client_id
    )
}

/// The `authorization_code` exchange of `code`, presenting the redirect URI
/// the code was issued for (RFC 6749 §4.1.3).
async fn exchange_code(world: &World, code: &str) -> TestResponse {
    grant(
        world,
        &json!({
            "grant_type": "authorization_code",
            "code": code,
            "redirect_uri": common::TEST_REDIRECT_URI,
        }),
    )
    .await
}

/// `POST /oauth/token` (or [`REVOKE`]) with `body` and the client's own
/// credentials merged in.
async fn grant(world: &World, body: &Value) -> TestResponse {
    grant_at(world, TOKEN, body).await
}

/// `POST`s `path` with the client credentials beside `body` — the shape
/// every `/oauth` endpoint but the approval takes.
async fn grant_at(world: &World, path: &str, body: &Value) -> TestResponse {
    common::TestRequest::post(path)
        .json(&world.client.with_credentials(body.clone()))
        .send(&world.app)
        .await
}

/// The frontend's approval of `scope` for `username`, answered as the code
/// its redirect carries.
///
/// The frontend is a second client, marked trusted, that logs the account
/// in with the password grant and then `POST`s the verdict to
/// `/oauth/authorize` with that token as its bearer — the consent screen's
/// whole conversation with the API.
///
/// # Panics
///
/// If the login or the verdict is refused, or the answered redirect
/// carries no `code`.
async fn consent(world: &World, username: &str, scope: &str) -> String {
    let frontend = common::register_client(&world.app, "Scenario Frontend", CLIENT_SCOPES).await;
    let trusted = sqlx::query("UPDATE oauth_apps SET trusted = true WHERE client_id = $1")
        .bind(&frontend.client_id)
        .execute(&world.state.db)
        .await
        .expect("the frontend app is marked trusted");
    assert_eq!(trusted.rows_affected(), 1, "the frontend app exists");
    let login = common::login_password(&world.app, &frontend, username, PASSWORD).await;
    common::assert_answer(&login, StatusCode::OK, "the frontend's login");
    let bearer = common::access_token_of(&login);

    let response = common::api_post(
        &world.app,
        Some(&bearer),
        AUTHORIZE,
        &json!({
            "client_id": world.client.client_id,
            "redirect_uri": common::TEST_REDIRECT_URI,
            "scope": scope,
            "approve": true,
        }),
    )
    .await;
    common::assert_answer(&response, StatusCode::OK, "the frontend's approval");
    let location = string_field(&response.json_value(), "redirect_uri");
    let (_, query) = location.split_once('?').unwrap_or((location.as_str(), ""));
    let pairs = url::form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect::<Vec<(String, String)>>();
    let (_key, code) = pairs
        .iter()
        .find(|(key, _)| key == "code")
        .unwrap_or_else(|| panic!("the redirect carries a code: {location}"));
    code.clone()
}
