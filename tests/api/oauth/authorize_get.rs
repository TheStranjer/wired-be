//! `GET /oauth/authorize` from a browser (Pleroma's
//! `OAuthController.authorize`): the request is validated and the browser
//! is sent on to the frontend's consent screen at
//! `{FRONTEND_BASE_URL}/oauth/authorize` with its query intact; a request
//! that fails validation is the JSON 400 ("Invalid client", "Invalid
//! `redirect_uri`", "Invalid scope", `unsupported_response_type`) and
//! never a redirect. The app registration, the query readers, the code
//! issuer, the cache assertion and the code counter are `pub(super)`
//! because the other authorize submodules flow through them.

use axum::http::StatusCode;
use sqlx::PgPool;
use wired_be::models::{
    oauth_app::{self, NewOauthApp, OauthApp},
    oauth_token,
    user::User,
};

use super::support::{REDIRECT, fixtures};
use crate::common;

/// The frontend origin the redirect tests configure.
const FRONTEND: &str = "https://fe.example";
/// The `Accept` header a browser navigation sends.
const BROWSER_ACCEPT: &str = "text/html,application/xhtml+xml,*/*;q=0.8";

/// The urlencoded `GET /oauth/authorize` URL for `client_id`: the code
/// flow delivering to `redirect`, asking `scope` and carrying the
/// already-encoded `state`; an empty `scope` or `state` omits the
/// parameter.
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

/// The real router with the frontend at [`FRONTEND`], so the redirect
/// target is not the API's own origin.
fn frontend_app(pool: PgPool) -> axum::Router {
    let mut config = common::test_config();
    config.frontend_base_url = FRONTEND.to_owned();
    common::test_app_with_config(pool, config)
}

/// Registers an app named `name` with `redirect_uris` and `scopes`
/// straight through the model: the shape the standard [`common::create_app`]
/// fixture does not cover (custom names and redirect lists).
///
/// # Panics
///
/// If the app insert fails.
pub(super) async fn register_app(
    pool: &PgPool,
    name: &str,
    redirect_uris: &str,
    scopes: &str,
) -> OauthApp {
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

/// Mints an authorization code for `user` and `app`, approved for
/// `scopes` and bound to `redirect_uri`, the way the frontend's approval
/// does, and returns its text.
///
/// # Panics
///
/// If the insert fails.
pub(super) async fn issue_code(
    pool: &PgPool,
    user: &User,
    app: &OauthApp,
    scopes: &str,
    redirect_uri: &str,
) -> String {
    oauth_token::create_authorization(pool, user.id, app.id, scopes, redirect_uri)
        .await
        .expect("the authorization inserts")
        .token
}

/// The urlencoded query pairs of a redirect target, decoded.
pub(super) fn query_pairs(url: &str) -> Vec<(String, String)> {
    let (_, query) = url.split_once('?').unwrap_or((url, ""));
    url::form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect()
}

/// The decoded `key` parameter of a redirect target's query.
pub(super) fn query_param(url: &str, key: &str) -> Option<String> {
    query_pairs(url)
        .into_iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value)
}

/// Assert the `Cache-Control: no-store` every authorize answer carries,
/// success, redirect or error alike.
pub(super) fn assert_cache_no_store(response: &common::TestResponse) {
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
pub(super) async fn authorization_count(pool: &PgPool) -> i64 {
    let (rows,): (i64,) = sqlx::query_as("SELECT count(*) FROM oauth_authorizations")
        .fetch_one(pool)
        .await
        .expect("the count reaches the database");
    rows
}

/// A valid browser request is a 302 to the frontend's consent screen:
/// `{FRONTEND_BASE_URL}/oauth/authorize` with the original parameters
/// urlencoded again, in order, and no code minted yet.
#[tokio::test]
async fn browser_get_redirects_to_the_frontend() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_user, app) = fixtures(&pool, "nina", "read write").await;
    let http = frontend_app(pool.clone());
    let response = common::TestRequest::get(&authorize_get_url(
        &app.client_id,
        REDIRECT,
        "read+write",
        "xyz",
    ))
    .header("accept", BROWSER_ACCEPT)
    .send(&http)
    .await;

    assert_eq!(response.status, StatusCode::FOUND, "a valid request");
    assert_cache_no_store(&response);
    assert_eq!(
        response.header("location"),
        Some(
            format!(
                "{FRONTEND}/oauth/authorize?response_type=code&client_id={}\
                 &redirect_uri=https%3A%2F%2Fclient.example%2Fcallback\
                 &scope=read+write&state=xyz",
                app.client_id
            )
            .as_str()
        ),
        "the frontend's authorize page with the query preserved"
    );
    assert_eq!(authorization_count(&pool).await, 0, "nothing minted yet");
}

/// Every parameter survives the redirect decoded-equal: awkward `state`
/// characters, a missing `scope` (the frontend applies the default), a
/// valid PKCE pair and parameters this server does not read.
#[tokio::test]
async fn browser_get_keeps_the_whole_query() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_user, app) = fixtures(&pool, "oscar", "read write").await;
    let http = frontend_app(pool.clone());
    let path = format!(
        "{}&state=a%20b%26c%3Dd&code_challenge=E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM\
         &code_challenge_method=S256&force_login=true",
        authorize_get_url(&app.client_id, REDIRECT, "", "")
    );
    let response = common::TestRequest::get(&path).send(&http).await;

    assert_eq!(response.status, StatusCode::FOUND, "read fits anywhere");
    let location = response.header("location").expect("a Location header");
    assert!(
        location.starts_with(&format!("{FRONTEND}/oauth/authorize?")),
        "on the frontend: {location}"
    );
    assert_eq!(
        query_pairs(location),
        query_pairs(&path),
        "the same pairs in the same order"
    );
    assert_eq!(
        query_param(location, "state").as_deref(),
        Some("a b&c=d"),
        "the awkward state decodes to what the client sent"
    );
    assert!(
        query_param(location, "scope").is_none(),
        "no scope invented"
    );
}

/// The redirect does not depend on what the browser says it accepts: no
/// `Accept` header at all redirects too.
#[tokio::test]
async fn browser_get_without_an_accept_header_redirects() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_user, app) = fixtures(&pool, "olga", "read").await;
    let http = frontend_app(pool.clone());
    let response =
        common::TestRequest::get(&authorize_get_url(&app.client_id, REDIRECT, "read", ""))
            .send(&http)
            .await;
    assert_eq!(response.status, StatusCode::FOUND, "redirected");
    assert!(
        response
            .header("location")
            .is_some_and(|value| value.starts_with(FRONTEND)),
        "to the configured frontend"
    );
}

/// An unknown or missing `client_id` is the 400 "Invalid client".
#[tokio::test]
async fn get_authorize_refuses_unknown_clients() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_user, _app) = fixtures(&pool, "peggy", "read").await;
    let http = frontend_app(pool.clone());

    let unknown =
        common::TestRequest::get(&authorize_get_url("no-such-client", REDIRECT, "read", ""))
            .send(&http)
            .await;
    assert_eq!(unknown.status, StatusCode::BAD_REQUEST, "a 400");
    assert_eq!(unknown.error_text(), "Invalid client", "Pleroma's body");
    assert!(unknown.header("location").is_none(), "never a redirect");
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
    assert_eq!(missing.error_text(), "Invalid client", "is refused alike");
    assert!(missing.header("location").is_none(), "never a redirect");
}

/// A `redirect_uri` that is not one of the app's registered URIs — not
/// even a prefix or an extended spelling — is the 400
/// "Invalid `redirect_uri`" as JSON, with no `Location` header: the
/// browser is never bounced to an unvalidated target.
#[tokio::test]
async fn get_authorize_refuses_unregistered_redirect_uris() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_user, app) = fixtures(&pool, "quinn", "read").await;
    let http = frontend_app(pool.clone());
    for unregistered in [
        "https://evil.example/callback",
        "https://client.example",
        &format!("{REDIRECT}/extra"),
        &format!("{REDIRECT}?x=1"),
        "",
    ] {
        let response =
            common::TestRequest::get(&authorize_get_url(&app.client_id, unregistered, "read", ""))
                .header("accept", BROWSER_ACCEPT)
                .send(&http)
                .await;
        assert_eq!(
            response.status,
            StatusCode::BAD_REQUEST,
            "`{unregistered}` is refused"
        );
        assert_eq!(response.error_text(), "Invalid redirect_uri", "by name");
        assert!(
            response.header("location").is_none(),
            "`{unregistered}` is not redirected to"
        );
        assert_cache_no_store(&response);
    }
}

/// A requested scope outside the registration — including the `read`
/// default against a write-only app — is the 400 "Invalid scope".
#[tokio::test]
async fn get_authorize_refuses_scopes_outside_the_registration() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_user, app) = fixtures(&pool, "rosa", "read").await;
    let http = frontend_app(pool.clone());
    let broad = common::TestRequest::get(&authorize_get_url(
        &app.client_id,
        REDIRECT,
        "read+write",
        "",
    ))
    .send(&http)
    .await;
    assert_eq!(broad.status, StatusCode::BAD_REQUEST, "unregistered write");
    assert_eq!(broad.error_text(), "Invalid scope", "is Invalid scope");
    assert!(broad.header("location").is_none(), "never a redirect");
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
    assert_eq!(defaulted.error_text(), "Invalid scope", "alike");
}

/// `response_type` must name the code flow: anything else, absent
/// included, is the 400 `unsupported_response_type`.
#[tokio::test]
async fn get_authorize_requires_the_code_response_type() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_user, app) = fixtures(&pool, "steve", "read").await;
    let http = frontend_app(pool.clone());

    let other = common::TestRequest::get(&format!(
        "/oauth/authorize?response_type=token&client_id={}&redirect_uri={REDIRECT}",
        app.client_id
    ))
    .send(&http)
    .await;
    assert_eq!(other.status, StatusCode::BAD_REQUEST, "a foreign flow");
    assert_eq!(
        other.error_text(),
        "unsupported_response_type",
        "is refused by its RFC name"
    );
    assert!(other.header("location").is_none(), "never a redirect");
    assert_cache_no_store(&other);

    let absent = common::TestRequest::get(&format!(
        "/oauth/authorize?client_id={}&redirect_uri={REDIRECT}",
        app.client_id
    ))
    .send(&http)
    .await;
    assert_eq!(absent.status, StatusCode::BAD_REQUEST, "absent likewise");
    assert_eq!(
        absent.error_text(),
        "unsupported_response_type",
        "with the same body"
    );
}
