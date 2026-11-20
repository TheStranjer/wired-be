//! Shared consts and helpers for every submodule of the `oauth` area: the
//! fixture password and the redirect URIs, the JSON request to the token
//! endpoint with its error and no-store assertions, the `password` and
//! `client_credentials` grant bodies and the user-and-app fixture pair
//! every test starts from. Everything shared here is `pub(super)`, and a
//! submodule imports exactly the members it calls; a helper only one
//! submodule uses lives there instead.

use axum::Router;
use serde_json::{Value, json};
use sqlx::PgPool;
use wired_be::models::{oauth_app::OauthApp, user::User};

use crate::common;

/// The password every user fixture logs in with.
pub(super) const PASSWORD: &str = "correct horse battery staple";
/// The redirect URI every standard app fixture registers.
pub(super) const REDIRECT: &str = "https://client.example/callback";
/// The out-of-band redirect URI: the code is shown on a page instead of
/// redirected (Pleroma's `@oob_token_redirect_uri`).
pub(super) const OOB: &str = "urn:ietf:wg:oauth:2.0:oob";

/// POSTs `body` as JSON to the token endpoint.
pub(super) async fn post_json(app: &Router, body: &Value) -> common::TestResponse {
    common::TestRequest::post("/oauth/token")
        .json(body)
        .send(app)
        .await
}

/// The `error` message of a JSON error response.
pub(super) fn error_text(response: &common::TestResponse) -> String {
    response.json_value()["error"]
        .as_str()
        .expect("errors carry a string error field")
        .to_owned()
}

/// Assert the no-store cache headers every answer carries (RFC 6749 §5.1).
pub(super) fn assert_no_store(response: &common::TestResponse) {
    assert_eq!(
        response.header("cache-control"),
        Some("no-store"),
        "cache-control: no-store"
    );
    assert_eq!(
        response.header("pragma"),
        Some("no-cache"),
        "pragma: no-cache"
    );
}

/// The password-grant body carrying app `app`'s credentials in the plain
/// body fields, asking for `scope` as `username`.
pub(super) fn grant_body(app: &OauthApp, username: &str, scope: &str) -> Value {
    json!({
        "grant_type": "password",
        "username": username,
        "password": PASSWORD,
        "scope": scope,
        "client_id": app.client_id,
        "client_secret": app.client_secret,
    })
}

/// A named user and a registered app, the two fixtures every test starts
/// from.
pub(super) async fn fixtures(pool: &PgPool, nickname: &str, app_scopes: &str) -> (User, OauthApp) {
    let user = common::create_user_with_password(pool, nickname, PASSWORD).await;
    let app = common::create_app(pool, app_scopes).await;
    (user, app)
}

/// The `client_credentials`-grant body carrying app `app`'s credentials
/// in the plain body fields, asking for `scope`. No `username` or
/// `password` plays a part: the grant authenticates the application.
pub(super) fn client_credentials_body(app: &OauthApp, scope: &str) -> Value {
    json!({
        "grant_type": "client_credentials",
        "scope": scope,
        "client_id": app.client_id,
        "client_secret": app.client_secret,
    })
}
