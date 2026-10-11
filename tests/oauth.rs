//! Integration tests for the `/oauth` endpoints: the `POST /oauth/token`
//! password grant, and the `GET`/`POST /oauth/authorize`
//! authorization-code flow.
//!
//! Success over JSON, form and HTTP Basic bodies; the 400 "Invalid
//! credentials" shared by wrong secrets, wrong passwords and unknown
//! users; the 400 "Invalid scope" for a request outside the app's
//! registration; the 403 `account_is_disabled` for a deactivated
//! account; the `unsupported_grant_type` and "Missing parameter:
//! `grant_type`" refusals; and the response shape, headers and stored
//! token row (Pleroma's `OAuthController.token_exchange`). The authorize
//! half covers the consent page and its escaping, the "Invalid client",
//! "Invalid `redirect_uri`" and "Invalid scope" refusals, the code
//! redirect with its exact `Location` (state round trip and an existing
//! query string included), the out-of-band code page, the failed-login
//! re-render, the denial redirect and the stored code row (Pleroma's
//! `OAuthController.authorize` and `create_authorization`).

#![cfg(test)]

// `pub` because Clippy's nursery `redundant_pub_crate` forbids `pub(crate)`
// items inside a private module, while `unreachable_pub` forbids `pub` ones:
// only a `pub` module makes the harness helpers lint-clean.
pub mod common;

use axum::{Router, http::StatusCode};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::DEFAULT_OAUTH_TOKEN_TTL_SECS,
    models::{
        oauth_app::{self, NewOauthApp, OauthApp},
        oauth_token::{self, OauthAuthorization},
        user::User,
    },
};

/// The password every user fixture logs in with.
const PASSWORD: &str = "correct horse battery staple";
/// The redirect URI every standard app fixture registers.
const REDIRECT: &str = "https://client.example/callback";
/// The out-of-band redirect URI: the code is shown on a page instead of
/// redirected (Pleroma's `@oob_token_redirect_uri`).
const OOB: &str = "urn:ietf:wg:oauth:2.0:oob";

/// POSTs `body` as JSON to the token endpoint.
async fn post_json(app: &Router, body: &Value) -> common::TestResponse {
    common::TestRequest::post("/oauth/token")
        .json(body)
        .send(app)
        .await
}

/// The `error` message of a JSON error response.
fn error_text(response: &common::TestResponse) -> String {
    response.json_value()["error"]
        .as_str()
        .expect("errors carry a string error field")
        .to_owned()
}

/// Assert the no-store cache headers every answer carries (RFC 6749 §5.1).
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

/// The password-grant body carrying app `app`'s credentials in the plain
/// body fields, asking for `scope` as `username`.
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

/// A named user and a registered app, the two fixtures every test starts
/// from.
async fn fixtures(pool: &PgPool, nickname: &str, app_scopes: &str) -> (User, OauthApp) {
    let user = common::create_user_with_password(pool, nickname, PASSWORD).await;
    let app = common::create_app(pool, app_scopes).await;
    (user, app)
}

/// The full password grant over JSON: the documented body, the entity
/// fields, and the token row behind them.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn json_password_grant_returns_a_token_pair(pool: PgPool) {
    let (user, app) = fixtures(&pool, "alice", "read write").await;
    let http = common::test_app(pool.clone());
    let response = post_json(&http, &grant_body(&app, "alice", "read write")).await;

    assert_eq!(response.status, StatusCode::OK, "the grant succeeds");
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "the answer is JSON"
    );
    assert_no_store(&response);

    let body = response.json_value();
    assert_eq!(body["token_type"], "Bearer", "bearer tokens");
    let access = body["access_token"]
        .as_str()
        .expect("access_token is a string");
    let refresh = body["refresh_token"]
        .as_str()
        .expect("refresh_token is a string");
    assert_eq!(access.len(), 43, "the 43-char Pleroma token shape");
    assert_eq!(refresh.len(), 43, "both credentials share it");
    assert_ne!(access, refresh, "and they differ");
    assert_eq!(
        body["scope"], "read write",
        "the granted scopes, space-joined"
    );
    let created = body["created_at"]
        .as_i64()
        .expect("created_at is unix seconds");
    let now = chrono::Utc::now().timestamp();
    assert!(
        (now - 5..=now + 5).contains(&created),
        "created_at is issue time, got {created} vs {now}"
    );
    let ttl = i64::try_from(DEFAULT_OAUTH_TOKEN_TTL_SECS).expect("the default fits in i64");
    let expires_in = body["expires_in"].as_i64().expect("expires_in is seconds");
    assert!(
        (ttl - 5..=ttl).contains(&expires_in),
        "expires_in is the configured lifetime, got {expires_in}"
    );
    let id = body["id"]
        .as_str()
        .and_then(|text| Uuid::parse_str(text).ok())
        .expect("id is the hyphenated token uuid");
    assert!(id.get_version_num() == 7, "the id is a UUIDv7 (decision 4)");
    assert_eq!(body["me"], user.ap_id, "me is the user's actor id");
    assert_eq!(
        body.as_object().expect("an object").len(),
        8,
        "exactly the documented keys"
    );

    let (stored, stored_app) = oauth_token::get_with_app(&pool, access)
        .await
        .expect("the lookup reaches the database")
        .expect("the token row was stored");
    assert_eq!(stored.id, id, "the response id is the stored primary key");
    assert_eq!(
        stored.refresh_token, refresh,
        "the refresh secret round trips"
    );
    assert_eq!(stored.user_id, Some(user.id), "the token acts as the user");
    assert_eq!(stored.app_id, app.id, "issued for the validated app");
    assert_eq!(stored.scopes, "read write", "granted what was requested");
    assert_eq!(stored_app.id, app.id, "joined to its app");
}

/// The same grant over an urlencoded form body, the shape Mastodon
/// clients post.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn form_password_grant_returns_a_token_pair(pool: PgPool) {
    let (user, app) = fixtures(&pool, "bob", "read write follow").await;
    let http = common::test_app(pool.clone());
    let response = common::TestRequest::post("/oauth/token")
        .form(&[
            ("grant_type", "password"),
            ("username", "bob"),
            ("password", PASSWORD),
            ("scope", "read follow"),
            ("client_id", app.client_id.as_str()),
            ("client_secret", app.client_secret.as_str()),
        ])
        .send(&http)
        .await;

    assert_eq!(response.status, StatusCode::OK, "the form grant succeeds");
    assert_no_store(&response);
    let body = response.json_value();
    assert_eq!(
        body["scope"], "read follow",
        "the requested subset is granted"
    );
    assert_eq!(body["me"], user.ap_id, "the owner's actor id");
    assert!(
        !body["access_token"].is_null() && !body["refresh_token"].is_null(),
        "both credentials came back"
    );
}

/// Client credentials through `Authorization: Basic`, the RFC 6749
/// preferred presentation, win over the body fields.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn basic_client_credentials_authenticate_the_app(pool: PgPool) {
    let (user, app) = fixtures(&pool, "carol", "read write").await;
    let http = common::test_app(pool.clone());
    let basic = STANDARD.encode(format!("{}:{}", app.client_id, app.client_secret));
    let response = common::TestRequest::post("/oauth/token")
        .header("authorization", &format!("Basic {basic}"))
        .json(&json!({
            "grant_type": "password",
            "username": "carol",
            "password": PASSWORD,
            "scope": "read",
        }))
        .send(&http)
        .await;

    assert_eq!(response.status, StatusCode::OK, "Basic credentials work");
    assert_no_store(&response);
    assert_eq!(
        response.json_value()["me"],
        user.ap_id,
        "the token belongs to the user"
    );

    // The header wins: bogus body credentials cannot poison a good pair.
    let poisoned = json!({
        "grant_type": "password",
        "username": "carol",
        "password": PASSWORD,
        "client_id": "not-this-client",
        "client_secret": "not-this-secret",
    });
    let poisoned_answer = common::TestRequest::post("/oauth/token")
        .header("authorization", &format!("Basic {basic}"))
        .json(&poisoned)
        .send(&http)
        .await;
    assert_eq!(
        poisoned_answer.status,
        StatusCode::OK,
        "the Basic pair takes precedence over the body"
    );
}

/// A wrong client secret, an unknown client id and no credentials at all
/// all answer the same 400, Pleroma's `Invalid credentials`.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn bad_client_credentials_are_invalid_credentials(pool: PgPool) {
    let (_user, app) = fixtures(&pool, "dave", "read").await;
    let http = common::test_app(pool.clone());

    let mut wrong_secret = grant_body(&app, "dave", "read");
    wrong_secret["client_secret"] = json!("not-the-secret");
    let secret_answer = post_json(&http, &wrong_secret).await;
    assert_eq!(
        secret_answer.status,
        StatusCode::BAD_REQUEST,
        "a wrong secret is a 400"
    );
    assert_eq!(
        error_text(&secret_answer),
        "Invalid credentials",
        "one message for every credential failure"
    );
    assert_no_store(&secret_answer);

    let mut unknown_client = grant_body(&app, "dave", "read");
    unknown_client["client_id"] = json!("no-such-client");
    let client_answer = post_json(&http, &unknown_client).await;
    assert_eq!(
        client_answer.status,
        StatusCode::BAD_REQUEST,
        "an unknown client id is a 400"
    );
    assert_eq!(
        error_text(&client_answer),
        "Invalid credentials",
        "same message"
    );

    let mut bare = grant_body(&app, "dave", "read");
    bare["client_id"] = json!(null);
    bare["client_secret"] = json!(null);
    let bare_answer = post_json(&http, &bare).await;
    assert_eq!(
        bare_answer.status,
        StatusCode::BAD_REQUEST,
        "absent credentials are a 400"
    );
    assert_eq!(
        error_text(&bare_answer),
        "Invalid credentials",
        "same message"
    );

    let rows: (i64,) = sqlx::query_as("SELECT count(*) FROM oauth_tokens")
        .fetch_one(&pool)
        .await
        .expect("the count reaches the database");
    assert_eq!(rows.0, 0, "no rejection minted a token");
}

/// A wrong password and an unknown user share the 400 "Invalid
/// credentials" answer (Pleroma does not tell them apart).
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn bad_user_credentials_are_invalid_credentials(pool: PgPool) {
    let (_user, app) = fixtures(&pool, "erin", "read").await;
    let http = common::test_app(pool.clone());

    let mut wrong_password = grant_body(&app, "erin", "read");
    wrong_password["password"] = json!("wrong password");
    let password_answer = post_json(&http, &wrong_password).await;
    assert_eq!(
        password_answer.status,
        StatusCode::BAD_REQUEST,
        "a wrong password is a 400"
    );
    assert_eq!(
        error_text(&password_answer),
        "Invalid credentials",
        "its message"
    );

    let unknown_user = grant_body(&app, "nobody", "read");
    let user_answer = post_json(&http, &unknown_user).await;
    assert_eq!(
        user_answer.status,
        StatusCode::BAD_REQUEST,
        "an unknown user is a 400"
    );
    assert_eq!(
        error_text(&user_answer),
        "Invalid credentials",
        "the same message, deliberately"
    );
}

/// Missing `username` and missing `password` fail exactly like wrong ones.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn missing_user_parameters_are_invalid_credentials(pool: PgPool) {
    let (_user, app) = fixtures(&pool, "frank", "read").await;
    let http = common::test_app(pool.clone());
    for missing in ["username", "password"] {
        let mut body = grant_body(&app, "frank", "read");
        body[missing] = json!(null);
        let response = post_json(&http, &body).await;
        assert_eq!(
            response.status,
            StatusCode::BAD_REQUEST,
            "without `{missing}` the grant fails"
        );
        assert_eq!(
            error_text(&response),
            "Invalid credentials",
            "with Pleroma's answer"
        );
    }
}

/// A scope outside the app's registration is the 400 "Invalid scope".
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn a_scope_outside_the_registration_is_invalid_scope(pool: PgPool) {
    let (_user, app) = fixtures(&pool, "grace", "read").await;
    let http = common::test_app(pool.clone());
    let response = post_json(&http, &grant_body(&app, "grace", "read write")).await;
    assert_eq!(
        response.status,
        StatusCode::BAD_REQUEST,
        "unregistered `write` is refused"
    );
    assert_eq!(error_text(&response), "Invalid scope", "Pleroma's message");
    assert_no_store(&response);
    let rows: (i64,) = sqlx::query_as("SELECT count(*) FROM oauth_tokens")
        .fetch_one(&pool)
        .await
        .expect("the count reaches the database");
    assert_eq!(rows.0, 0, "the refused grant stored no token");
}

/// Sub-scopes fit inside a broader registration and come back verbatim.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn requested_subscopes_are_granted_verbatim(pool: PgPool) {
    let (_user, app) = fixtures(&pool, "heidi", "read write").await;
    let http = common::test_app(pool.clone());
    let response = post_json(
        &http,
        &grant_body(&app, "heidi", "read:accounts write:statuses"),
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "sub-scopes of registered trees pass"
    );
    let body = response.json_value();
    assert_eq!(
        body["scope"], "read:accounts write:statuses",
        "the narrower grant is exactly what was asked for"
    );
    let access = body["access_token"].as_str().expect("a token string");
    let (stored, _) = oauth_token::get_with_app(&pool, access)
        .await
        .expect("the lookup reaches the database")
        .expect("the token row was stored");
    assert_eq!(
        stored.scopes, "read:accounts write:statuses",
        "and that is what got stored"
    );
}

/// Without a `scope` parameter the token gets the default, `read`.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn the_default_scope_is_read(pool: PgPool) {
    let (_user, app) = fixtures(&pool, "ivan", "read write").await;
    let http = common::test_app(pool.clone());
    let mut body = grant_body(&app, "ivan", "read write");
    body["scope"] = json!(null);
    let response = post_json(&http, &body).await;
    assert_eq!(response.status, StatusCode::OK, "an absent scope defaults");
    assert_eq!(
        response.json_value()["scope"],
        "read",
        "to Pleroma's default"
    );

    // And `read` must be registered for the default to pass at all.
    let write_only = common::create_app(&pool, "write").await;
    let mut write_only_body = grant_body(&write_only, "ivan", "read write");
    write_only_body["scope"] = json!(null);
    let refused = post_json(&http, &write_only_body).await;
    assert_eq!(
        refused.status,
        StatusCode::BAD_REQUEST,
        "a `write`-only app cannot receive the default `read`"
    );
    assert_eq!(error_text(&refused), "Invalid scope", "as Invalid scope");
}

/// `username` is a nickname or an email, in any case (citext).
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn an_email_works_as_username(pool: PgPool) {
    let (user, app) = fixtures(&pool, "judy", "read").await;
    let http = common::test_app(pool.clone());
    assert_eq!(
        user.email.as_deref(),
        Some("judy@example.org"),
        "the fixture carries the email"
    );
    let response = post_json(&http, &grant_body(&app, "JUDY@example.org", "read")).await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "the email logs in, case-insensitively"
    );
    assert_eq!(response.json_value()["me"], user.ap_id, "same user");
}

/// A deactivated account with the right password gets Pleroma's 403
/// `account_is_disabled`, not the invalid-credentials 400.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn a_deactivated_account_is_refused_with_an_identifier(pool: PgPool) {
    let user = common::create_deactivated_user_with_password(&pool, "kate", PASSWORD).await;
    let app = common::create_app(&pool, "read").await;
    let http = common::test_app(pool.clone());
    let response = post_json(&http, &grant_body(&app, "kate", "read")).await;

    assert_eq!(
        response.status,
        StatusCode::FORBIDDEN,
        "a deactivated account is a 403 after authenticating"
    );
    assert_no_store(&response);
    let body = response.json_value();
    assert_eq!(
        body["error"], "Your account is currently disabled",
        "Pleroma's message"
    );
    assert_eq!(
        body["identifier"], "account_is_disabled",
        "and its machine-readable reason code"
    );
    assert_eq!(
        body.as_object().expect("an object").len(),
        2,
        "exactly the two keys"
    );
    assert!(
        !body["error"].as_str().unwrap().contains(&user.nickname),
        "the body leaks nothing beyond the fixed message"
    );

    let rows: (i64,) = sqlx::query_as("SELECT count(*) FROM oauth_tokens")
        .fetch_one(&pool)
        .await
        .expect("the count reaches the database");
    assert_eq!(rows.0, 0, "the refusal minted no token");
}

/// A grant type other than `password` is the 400
/// `unsupported_grant_type`.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn an_unknown_grant_type_is_unsupported(pool: PgPool) {
    let (_user, app) = fixtures(&pool, "luna", "read").await;
    let http = common::test_app(pool.clone());
    for grant in [
        "authorization_code",
        "refresh_token",
        "client_credentials",
        "bogus",
    ] {
        let mut body = grant_body(&app, "luna", "read");
        body["grant_type"] = json!(grant);
        let response = post_json(&http, &body).await;
        assert_eq!(
            response.status,
            StatusCode::BAD_REQUEST,
            "`{grant}` is a 400 here"
        );
        assert_eq!(
            error_text(&response),
            "unsupported_grant_type",
            "for the named reason"
        );
        assert_no_store(&response);
    }
}

/// A request without `grant_type` at all is refused by name, matching
/// Pleroma's message.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn a_missing_grant_type_is_named(pool: PgPool) {
    let (_user, app) = fixtures(&pool, "mike", "read").await;
    let http = common::test_app(pool.clone());
    let mut body = grant_body(&app, "mike", "read");
    body["grant_type"] = json!(null);
    let response = post_json(&http, &body).await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST, "a 400");
    assert_eq!(
        error_text(&response),
        "Missing parameter: grant_type",
        "naming the missing parameter"
    );
    assert_no_store(&response);

    // Nothing at all in the body is still "missing grant_type", not a 422.
    let form_answer = common::TestRequest::post("/oauth/token")
        .form(&[("username", "mike")])
        .send(&http)
        .await;
    assert_eq!(
        form_answer.status,
        StatusCode::BAD_REQUEST,
        "an unrelated form body"
    );
    assert_eq!(
        error_text(&form_answer),
        "Missing parameter: grant_type",
        "gets the same answer"
    );
}

/// The urlencoded `GET /oauth/authorize` consent URL for `client_id`:
/// the code flow delivering to `redirect`, asking `scope` and carrying
/// the already-encoded `state`; an empty `state` omits the parameter.
fn authorize_get_url(client_id: &str, redirect: &str, scope: &str, state: &str) -> String {
    let mut url = format!(
        "/oauth/authorize?response_type=code&client_id={client_id}&redirect_uri={redirect}"
    );
    if !scope.is_empty() {
        url.push('&');
        url.push_str("scope=");
        url.push_str(scope);
    }
    if !state.is_empty() {
        url.push('&');
        url.push_str("state=");
        url.push_str(state);
    }
    url
}

/// Registers an app named `name` with `redirect_uris` and `scopes`
/// straight through the model: the shape the standard [`common::create_app`]
/// fixture does not cover (custom names and redirect lists).
///
/// # Panics
///
/// If the app insert fails.
async fn register_app(pool: &PgPool, name: &str, redirect_uris: &str, scopes: &str) -> OauthApp {
    oauth_app::create(
        pool,
        &NewOauthApp {
            client_name: name.to_owned(),
            redirect_uris: redirect_uris.to_owned(),
            scopes: scopes.to_owned(),
            website: None,
            user_id: None,
        },
    )
    .await
    .expect("app registration inserts")
}

/// The consent form body logging in as `name`/`password` for `app`,
/// approving `scopes` and carrying `state` when given. Callers tweak
/// the entries (the redirect URI) or push more (`authorization[deny]`)
/// as the flow under test needs.
fn consent_form<'a>(
    app: &'a OauthApp,
    name: &'a str,
    password: &'a str,
    scopes: &[&'a str],
    state: Option<&'a str>,
) -> Vec<(&'a str, &'a str)> {
    let mut fields = vec![
        ("authorization[client_id]", app.client_id.as_str()),
        ("authorization[redirect_uri]", REDIRECT),
        ("authorization[name]", name),
        ("authorization[password]", password),
    ];
    for scope in scopes {
        fields.push(("authorization[scope][]", scope));
    }
    if let Some(state) = state {
        fields.push(("authorization[state]", state));
    }
    fields
}

/// The urlencoded query pairs of a redirect target, decoded.
fn query_pairs(url: &str) -> Vec<(String, String)> {
    let (_, query) = url.split_once('?').unwrap_or((url, ""));
    url::form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect()
}

/// The decoded `key` parameter of a redirect target's query.
fn query_param(url: &str, key: &str) -> Option<String> {
    query_pairs(url)
        .into_iter()
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

/// Assert the `Cache-Control: no-store` every authorize answer carries,
/// success, redirect or error alike.
fn assert_cache_no_store(response: &common::TestResponse) {
    assert_eq!(
        response.header("cache-control"),
        Some("no-store"),
        "authorize answers are never cacheable"
    );
}

/// Rows in `oauth_authorizations` in total.
///
/// # Panics
///
/// If the count query fails.
async fn authorization_count(pool: &PgPool) -> i64 {
    let (rows,): (i64,) = sqlx::query_as("SELECT count(*) FROM oauth_authorizations")
        .fetch_one(pool)
        .await
        .expect("the count reaches the database");
    rows
}

/// The full consent page a valid `GET` renders: the form posting to
/// `/oauth/authorize` with the login fields, one checked box per
/// requested scope, and the `client_id`/`redirect_uri`/`state`
/// round-trip fields — plus the no-store header.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn get_authorize_renders_the_consent_form(pool: PgPool) {
    let (_user, app) = fixtures(&pool, "nina", "read write").await;
    let http = common::test_app(pool.clone());
    let response = common::TestRequest::get(&authorize_get_url(
        &app.client_id,
        REDIRECT,
        "read+write",
        "xyz",
    ))
    .send(&http)
    .await;

    assert_eq!(response.status, StatusCode::OK, "a valid request renders");
    assert_eq!(
        response.header("content-type"),
        Some("text/html; charset=utf-8"),
        "as a self-contained HTML page"
    );
    assert_cache_no_store(&response);

    let body = response.text();
    assert!(
        body.contains("<form method=\"post\" action=\"/oauth/authorize\">"),
        "the form posts back to /oauth/authorize"
    );
    assert!(
        body.contains("<strong>Test Client</strong> is requesting access to your account."),
        "the text names the app: {body}"
    );
    for field in [
        "name=\"authorization[name]\"",
        "name=\"authorization[password]\"",
        "name=\"authorization[deny]\"",
        "type=\"password\"",
    ] {
        assert!(body.contains(field), "the form carries `{field}`");
    }
    assert_eq!(
        body.matches("name=\"authorization[scope][]\"").count(),
        2,
        "one checkbox per requested scope"
    );
    assert!(body.contains("value=\"read\" checked"), "read is checked");
    assert!(body.contains("value=\"write\" checked"), "so is write");
    assert!(
        body.contains(&format!(
            "name=\"authorization[client_id]\" value=\"{}\">",
            app.client_id
        )),
        "the client id round-trips"
    );
    assert!(
        body.contains(&format!(
            "name=\"authorization[redirect_uri]\" value=\"{REDIRECT}\">"
        )),
        "so does the redirect uri"
    );
    assert!(
        body.contains("name=\"authorization[state]\" value=\"xyz\">"),
        "and so does the state"
    );
    assert!(body.contains("<html lang=\"en\">"), "the default language");
}

/// Without a `scope` parameter the page offers the default, `read` —
/// as a single checkbox.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn get_authorize_defaults_the_scope_to_read(pool: PgPool) {
    let (_user, app) = fixtures(&pool, "oscar", "read write").await;
    let http = common::test_app(pool.clone());
    let response = common::TestRequest::get(&authorize_get_url(&app.client_id, REDIRECT, "", ""))
        .send(&http)
        .await;

    assert_eq!(response.status, StatusCode::OK, "read fits anywhere");
    let body = response.text();
    assert_eq!(
        body.matches("name=\"authorization[scope][]\"").count(),
        1,
        "just the default"
    );
    assert!(
        body.contains("value=\"read\" checked"),
        "the default is checked"
    );
    assert!(!body.contains("value=\"write\""), "write was not asked for");
}

/// A malicious app name, `state` and `lang` come back HTML-escaped:
/// none of them can open a tag or break out of its attribute.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn get_authorize_escapes_every_dynamic_value(pool: PgPool) {
    let app = register_app(&pool, "<script>evil</script>", REDIRECT, "read").await;
    let http = common::test_app(pool.clone());
    let response = common::TestRequest::get(&format!(
        "/oauth/authorize?response_type=code&client_id={}&redirect_uri={}\
         &state=%22%3E%3Cscript%3Ealert%28%27xss%27%29%3C%2Fscript%3E\
         &lang=de%22onload%3D%22x",
        app.client_id, REDIRECT
    ))
    .send(&http)
    .await;

    assert_eq!(response.status, StatusCode::OK, "the page still renders");
    let body = response.text();
    assert!(
        !body.contains("<script"),
        "nothing in the page opens a script"
    );
    assert!(
        body.contains("&lt;script&gt;evil&lt;/script&gt;"),
        "the app name shows escaped"
    );
    assert!(
        body.contains("value=\"&quot;&gt;&lt;script&gt;alert(&#39;xss&#39;)&lt;/script&gt;\""),
        "the state shows escaped inside its attribute"
    );
    assert!(
        body.contains("<html lang=\"de&quot;onload=&quot;x\">"),
        "even the language attribute cannot be broken out of"
    );
}

/// An unknown or missing `client_id` is the 400 "Invalid client".
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn get_authorize_refuses_unknown_clients(pool: PgPool) {
    let (_user, _app) = fixtures(&pool, "peggy", "read").await;
    let http = common::test_app(pool.clone());

    let unknown =
        common::TestRequest::get(&authorize_get_url("no-such-client", REDIRECT, "read", ""))
            .send(&http)
            .await;
    assert_eq!(unknown.status, StatusCode::BAD_REQUEST, "a 400");
    assert_eq!(error_text(&unknown), "Invalid client", "Pleroma's body");
    assert_cache_no_store(&unknown);

    let missing = common::TestRequest::get(&format!(
        "/oauth/authorize?response_type=code&redirect_uri={REDIRECT}&scope=read"
    ))
    .send(&http)
    .await;
    assert_eq!(
        missing.status,
        StatusCode::BAD_REQUEST,
        "a request without any client_id"
    );
    assert_eq!(error_text(&missing), "Invalid client", "is refused alike");
}

/// A `redirect_uri` that is not one of the app's registered URIs — not
/// even a prefix or an extended spelling — is the 400
/// "Invalid `redirect_uri`".
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn get_authorize_refuses_unregistered_redirect_uris(pool: PgPool) {
    let (_user, app) = fixtures(&pool, "quinn", "read").await;
    let http = common::test_app(pool.clone());
    for unregistered in [
        "https://evil.example/callback",
        "https://client.example",
        &format!("{REDIRECT}/extra"),
        &format!("{REDIRECT}?x=1"),
        "",
    ] {
        let response =
            common::TestRequest::get(&authorize_get_url(&app.client_id, unregistered, "read", ""))
                .send(&http)
                .await;
        assert_eq!(
            response.status,
            StatusCode::BAD_REQUEST,
            "`{unregistered}` is refused"
        );
        assert_eq!(error_text(&response), "Invalid redirect_uri", "by name");
        assert_cache_no_store(&response);
    }
}

/// A requested scope outside the registration — including the `read`
/// default against a write-only app — is the 400 "Invalid scope".
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn get_authorize_refuses_scopes_outside_the_registration(pool: PgPool) {
    let (_user, app) = fixtures(&pool, "rosa", "read").await;
    let http = common::test_app(pool.clone());
    let broad = common::TestRequest::get(&authorize_get_url(
        &app.client_id,
        REDIRECT,
        "read+write",
        "",
    ))
    .send(&http)
    .await;
    assert_eq!(broad.status, StatusCode::BAD_REQUEST, "unregistered write");
    assert_eq!(error_text(&broad), "Invalid scope", "is Invalid scope");
    assert_cache_no_store(&broad);

    let write_only = common::create_app(&pool, "write").await;
    let defaulted =
        common::TestRequest::get(&authorize_get_url(&write_only.client_id, REDIRECT, "", ""))
            .send(&http)
            .await;
    assert_eq!(
        defaulted.status,
        StatusCode::BAD_REQUEST,
        "the default `read` does not fit a `write`-only app"
    );
    assert_eq!(error_text(&defaulted), "Invalid scope", "alike");
}

/// `response_type` must name the code flow: anything else, absent
/// included, is the 400 `unsupported_response_type`.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn get_authorize_requires_the_code_response_type(pool: PgPool) {
    let (_user, app) = fixtures(&pool, "steve", "read").await;
    let http = common::test_app(pool.clone());

    let other = common::TestRequest::get(&format!(
        "/oauth/authorize?response_type=token&client_id={}&redirect_uri={REDIRECT}",
        app.client_id
    ))
    .send(&http)
    .await;
    assert_eq!(other.status, StatusCode::BAD_REQUEST, "a foreign flow");
    assert_eq!(
        error_text(&other),
        "unsupported_response_type",
        "is refused by its RFC name"
    );
    assert_cache_no_store(&other);

    let absent = common::TestRequest::get(&format!(
        "/oauth/authorize?client_id={}&redirect_uri={REDIRECT}",
        app.client_id
    ))
    .send(&http)
    .await;
    assert_eq!(absent.status, StatusCode::BAD_REQUEST, "absent likewise");
    assert_eq!(
        error_text(&absent),
        "unsupported_response_type",
        "with the same body"
    );
}

/// A correct login approves: the reply is a 302 whose `Location` is the
/// registered URI plus `code`, and the code is a fresh, unused,
/// single-use row of `oauth_authorizations`.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn post_authorize_redirects_with_a_stored_code(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn post_authorize_round_trips_the_state(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn post_authorize_respects_an_existing_query_string(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn post_authorize_renders_the_code_for_oob_clients(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn post_authorize_bad_password_rerenders_the_form(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn post_authorize_deny_redirects_with_access_denied(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn post_authorize_revalidates_the_form(pool: PgPool) {
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
