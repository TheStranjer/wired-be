//! The fixtures, request helpers and assertions the MFA OAuth tests share.
//! The test modules are its children, so they reach every helper through
//! `use super::*`.

mod attempts;
mod authorize_flow;
mod password_grant;

use crate::common;

use axum::{Router, http::StatusCode};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::DEFAULT_OAUTH_TOKEN_TTL_SECS,
    models::{
        oauth_app::{self, NewOauthApp, OauthApp},
        oauth_token::{self, OauthAuthorization},
        user::{User, mfa},
    },
    util::totp,
};

/// The password every login fixture shares.
const PASSWORD: &str = "correct horse battery staple";
/// The redirect URI the standard app fixture registers.
const REDIRECT: &str = "https://client.example/callback";
/// The out-of-band redirect URI: the code is shown on a page instead of
/// redirected.
const OOB: &str = "urn:ietf:wg:oauth:2.0:oob";
/// The error body every failed challenge shares (Pleroma's
/// `MFAController.challenge`).
const INVALID_CODE: &str = "Invalid code";
/// The flash line above a re-rendered verification form.
const TWO_FACTOR_FAILED: &str = "Two-factor authentication failed.";

/// An MFA-enabled account beside its TOTP secret and issued backup
/// codes.
struct MfaAccount {
    /// The MFA-enabled user.
    user: User,
    /// The account's confirmed TOTP secret.
    secret: String,
    /// The account's issued backup codes in plaintext.
    codes: Vec<String>,
}

/// Inserts the MFA fixtures: `nickname` logging in with [`PASSWORD`],
/// MFA confirmed with five issued backup codes.
///
/// # Panics
///
/// If the fixtures fail to insert.
async fn mfa_account(pool: &PgPool, nickname: &str) -> MfaAccount {
    let (user, secret, codes) =
        common::create_mfa_user_with_password(pool, nickname, PASSWORD, 5).await;
    MfaAccount {
        user,
        secret,
        codes,
    }
}

/// The password-grant body for `app` as `username`, asking `scope`.
fn grant_body(app: &OauthApp, username: &str, scope: &str) -> Value {
    json!({
        "grant_type": "password",
        "username": username,
        "password": PASSWORD,
        "scope": scope,
        "client_id": app.client_id,
        "client_secret": app.client_secret,
    })
}

/// `POST /oauth/token` as JSON.
async fn post_json(http: &Router, body: &Value) -> common::TestResponse {
    common::TestRequest::post("/oauth/token")
        .json(body)
        .send(http)
        .await
}

/// The `error` message of a JSON error response.
fn error_text(response: &common::TestResponse) -> String {
    response.json_value()["error"]
        .as_str()
        .expect("errors carry a string error field")
        .to_owned()
}

/// Assert the no-store cache headers every `/oauth/token`-family answer
/// carries (RFC 6749 §5.1).
fn assert_no_store(response: &common::TestResponse) {
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

/// Runs the password grant for an MFA-enabled account and returns the
/// challenge token the 403 carries, asserting Pleroma's exact body
/// shape on the way.
///
/// # Panics
///
/// If the grant does not answer the pinned 403 challenge.
async fn mfa_grant(http: &Router, app: &OauthApp, username: &str, scope: &str) -> String {
    let refused = post_json(http, &grant_body(app, username, scope)).await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "MFA gates the grant");
    assert_no_store(&refused);
    let body = refused.json_value();
    assert_eq!(body["error"], "mfa_required", "Pleroma's challenge error");
    assert_eq!(
        body["supported_challenge_types"], "totp",
        "totp is the only method this server speaks"
    );
    let token = body["mfa_token"]
        .as_str()
        .expect("mfa_token is a string")
        .to_owned();
    assert_eq!(token.len(), 43, "the 43-char Pleroma credential shape");
    assert_eq!(
        body.as_object().expect("an object").len(),
        3,
        "exactly the three documented keys, and no token: {body}"
    );
    token
}

/// The stored challenge `token` names: its id, linked authorization and
/// expiry.
async fn stored_challenge(
    pool: &PgPool,
    token: &str,
) -> Option<(Uuid, Option<Uuid>, DateTime<Utc>)> {
    sqlx::query_as("SELECT id, authorization_id, valid_until FROM mfa_tokens WHERE token = $1")
        .bind(token)
        .fetch_optional(pool)
        .await
        .expect("the challenge lookup reaches the database")
}

/// Asserts the stored challenge of `token` belongs to `user`, links a
/// pending authorization and is about five minutes out; returns both
/// ids.
///
/// # Panics
///
/// If the challenge is gone or unlinked.
async fn assert_challenge_stored(pool: &PgPool, token: &str, user: &User) -> (Uuid, Uuid) {
    let (id, authorization_id, valid_until) = stored_challenge(pool, token)
        .await
        .expect("the challenge was stored");
    let owner: (Uuid,) = sqlx::query_as("SELECT user_id FROM mfa_tokens WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("the challenge row answers");
    assert_eq!(owner.0, user.id, "the challenge guards the account");
    let linked = authorization_id.expect("the challenge links the parked authorization");
    let remaining = valid_until.signed_duration_since(Utc::now()).num_seconds();
    assert!(
        (235..=305).contains(&remaining),
        "the challenge is five minutes out, got {remaining}s"
    );
    (id, linked)
}

/// The stored authorization with primary key `id`.
///
/// # Panics
///
/// If the row is gone.
async fn authorization_by_id(pool: &PgPool, id: Uuid) -> OauthAuthorization {
    sqlx::query_as("SELECT * FROM oauth_authorizations WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("the authorization row is stored")
}

/// How many `oauth_tokens` rows user `user_id` owns, expired ones included.
///
/// # Panics
///
/// If the query fails.
async fn token_count(pool: &PgPool, user_id: Uuid) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM oauth_tokens WHERE user_id = $1")
        .bind(user_id)
        .fetch_one(pool)
        .await
        .expect("the token count reaches the database")
}

/// The current 6-digit TOTP code of `secret`, as an authenticator app
/// would produce it right now.
fn current_code(secret: &str) -> String {
    totp::code_at(secret, Utc::now().timestamp()).expect("a generated secret decodes")
}

/// A code that differs from `real` in its first digit.
fn wrong_code(real: &str) -> String {
    let (head, tail) = real.split_at(1);
    let replacement = if head == "0" { "1" } else { "0" };
    format!("{replacement}{tail}")
}

/// The form fields of a `POST /oauth/mfa/challenge`: the challenge
/// token, type and code beside the application's body credentials.
fn challenge_fields<'a>(
    token: &'a str,
    challenge_type: &'a str,
    code: &'a str,
    app: &'a OauthApp,
) -> Vec<(&'a str, &'a str)> {
    vec![
        ("mfa_token", token),
        ("challenge_type", challenge_type),
        ("code", code),
        ("client_id", app.client_id.as_str()),
        ("client_secret", app.client_secret.as_str()),
    ]
}

/// Posts the challenge form.
async fn post_challenge(http: &Router, fields: &[(&str, &str)]) -> common::TestResponse {
    common::TestRequest::post("/oauth/mfa/challenge")
        .form(fields)
        .send(http)
        .await
}

/// Asserts a challenge answer is Pleroma's 400 "Invalid code" with the
/// no-store headers.
fn assert_invalid_code(response: &common::TestResponse, what: &str) {
    assert_eq!(response.status, StatusCode::BAD_REQUEST, "400 for {what}");
    assert_eq!(
        response.json_value(),
        json!({"error": INVALID_CODE}),
        "the one refusal for {what}"
    );
    assert_no_store(response);
}

/// The urlencoded `POST /oauth/authorize` consent form logging in as
/// `nickname`/[`PASSWORD`], approving `scope` and round-tripping `state`
/// (absent when `None`) to `REDIRECT`.
fn authorize_form<'a>(
    app: &'a OauthApp,
    nickname: &'a str,
    scope: &'a str,
    state: Option<&'a str>,
) -> Vec<(&'a str, &'a str)> {
    let mut fields = vec![
        ("authorization[client_id]", app.client_id.as_str()),
        ("authorization[redirect_uri]", REDIRECT),
        ("authorization[name]", nickname),
        ("authorization[password]", PASSWORD),
        ("authorization[scope][]", scope),
    ];
    if let Some(client_state) = state {
        fields.push(("authorization[state]", client_state));
    }
    fields
}

/// The `POST /oauth/mfa/verify` form: the challenge submission plus the
/// round-tripped delivery fields.
fn verify_form<'a>(
    token: &'a str,
    challenge_type: &'a str,
    code: &'a str,
    redirect_uri: &'a str,
    state: Option<&'a str>,
) -> Vec<(&'a str, &'a str)> {
    let mut fields = vec![
        ("mfa[mfa_token]", token),
        ("mfa[challenge_type]", challenge_type),
        ("mfa[code]", code),
        ("mfa[redirect_uri]", redirect_uri),
    ];
    if let Some(client_state) = state {
        fields.push(("mfa[state]", client_state));
    }
    fields
}

/// Posts the consent form.
async fn post_authorize(http: &Router, fields: &[(&str, &str)]) -> common::TestResponse {
    common::TestRequest::post("/oauth/authorize")
        .form(fields)
        .send(http)
        .await
}

/// Posts the verification form.
async fn post_verify(http: &Router, fields: &[(&str, &str)]) -> common::TestResponse {
    common::TestRequest::post("/oauth/mfa/verify")
        .form(fields)
        .send(http)
        .await
}

/// The decoded `key` parameter of a URL's query.
fn query_param(url: &str, key: &str) -> Option<String> {
    let (_, query) = url.split_once('?').unwrap_or((url, ""));
    url::form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value)
}

/// The code shown on the out-of-band page, between its markers.
///
/// # Panics
///
/// If the body holds no `<code id="authorization-code">` element.
fn oob_page_code(body: &str) -> &str {
    body.split_once("<code id=\"authorization-code\">")
        .expect("the oob page shows the code")
        .1
        .split_once("</code>")
        .expect("the code element is closed")
        .0
}

/// Registers an app with the given `redirect_uris` and `scopes` (the
/// standard [`common::create_app`] fixture registers only
/// [`REDIRECT`]).
///
/// # Panics
///
/// If the app insert fails.
async fn register_app(pool: &PgPool, redirect_uris: &str, scopes: &str) -> OauthApp {
    oauth_app::create(
        pool,
        &NewOauthApp {
            client_name: "MFA Client".to_owned(),
            redirect_uris: redirect_uris.to_owned(),
            scopes: scopes.to_owned(),
            website: None,
            user_id: None,
        },
    )
    .await
    .expect("app registration inserts")
}

/// Rows in `mfa_tokens` in total.
///
/// # Panics
///
/// If the count query fails.
async fn challenge_count(pool: &PgPool) -> i64 {
    let (rows,): (i64,) = sqlx::query_as("SELECT count(*) FROM mfa_tokens")
        .fetch_one(pool)
        .await
        .expect("the count reaches the database");
    rows
}
