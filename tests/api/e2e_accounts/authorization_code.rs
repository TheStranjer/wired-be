//! Scenario 2: the authorization-code flow — the consent page, the
//! approved submission's code, that code's exchange, the scope gate the
//! approved scopes decide, the spent code's refusal, the rotation and the
//! revocation. The consent URL, the grant posts and the consent form
//! themselves live here too: no other scenario reaches the `/oauth` pair.

use axum::http::StatusCode;
use serde_json::{Value, json};

use super::support::{
    AUTHORIZE, INVALID_CREDENTIALS, INVALID_TOKEN, PASSWORD, REVOKE, STATUSES, TOKEN, VERIFY,
    World, assert_refused, string_field,
};
use crate::common::{self, TestClient, TestResponse};

// ───────────────────  Scenario 2: the OAuth code flow  ───────────────────

/// The authorization-code flow end to end: the consent page, the approved
/// submission's redirect, the code's exchange for a pair carrying the
/// approved scopes, the spent code's refusal, the rotation that replaces
/// the pair, and the revocation that ends it — with the scope gate proved
/// on the way: a `read` token may read and may not post.
#[tokio::test]
async fn the_authorization_code_flow_issues_rotates_and_retires_a_token() {
    let db = common::test_db().await;
    let world = World::open(db.pool().clone()).await;
    let _bob = world.register("bob").await;

    // Step 1: the consent page renders for a valid request.
    let page = common::api_get(&world.app, None, &consent_url(&world.client, "read+write")).await;
    common::assert_answer(&page, StatusCode::OK, "the consent page");
    let html = page.text();
    assert!(
        html.contains("<form method=\"post\" action=\"/oauth/authorize\">"),
        "the page posts its consent back to the same route: {html}"
    );

    // Step 2: the approved submission redirects to the client with a code.
    let code = consent(&world, "bob", &["read"]).await;

    // Step 3: the code is redeemed for a pair acting as its authorizer,
    // with the *approved* scopes and nothing else.
    let exchanged = grant(
        &world,
        &json!({"grant_type": "authorization_code", "code": code}),
    )
    .await;
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
    let replay = grant(
        &world,
        &json!({"grant_type": "authorization_code", "code": code}),
    )
    .await;
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

/// `POST /oauth/token` (or [`REVOKE`]) with `body` and the client's own
/// credentials merged in.
async fn grant(world: &World, body: &Value) -> TestResponse {
    grant_at(world, TOKEN, body).await
}

/// `POST`s `path` with the client credentials beside `body` — the shape
/// every `/oauth` endpoint but the consent form takes.
async fn grant_at(world: &World, path: &str, body: &Value) -> TestResponse {
    common::TestRequest::post(path)
        .json(&world.client.with_credentials(body.clone()))
        .send(&world.app)
        .await
}

/// The approved `POST /oauth/authorize` for `username`, checking the boxes
/// in `scopes`, answered as the code its redirect carries.
///
/// The form carries the login, not a bearer: this API has no browser
/// session, so the consent submission authenticates the account it
/// approves — the same [`PASSWORD`] the scenario registered it with.
///
/// # Panics
///
/// If the submission is not the 302, or its redirect carries no `code`.
async fn consent(world: &World, username: &str, scopes: &[&str]) -> String {
    let mut fields = vec![
        ("authorization[client_id]", world.client.client_id.as_str()),
        ("authorization[redirect_uri]", common::TEST_REDIRECT_URI),
        ("authorization[name]", username),
        ("authorization[password]", PASSWORD),
    ];
    for scope in scopes {
        fields.push(("authorization[scope][]", scope));
    }
    let response = common::TestRequest::post(AUTHORIZE)
        .form(&fields)
        .send(&world.app)
        .await;
    common::assert_answer(&response, StatusCode::FOUND, "the consent submission");
    let location = response
        .header("location")
        .expect("the approval redirects to the client")
        .to_owned();
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
