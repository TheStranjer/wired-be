//! Shared fixtures and helpers for every submodule of the
//! `oauth_external` area: the [`Site`] bundle of mock, router, app and
//! user with the fixtures that build it, the `prepare_request` round trip
//! and its out-of-band twin, the query-string and redirect readers, the
//! error and no-store assertions, the `oauth_login_states` helpers and
//! the row counters, plus the pending hand-off and form helpers the
//! registration tests share.

use axum::{Router, http::StatusCode};
use serde_json::Value;
use sqlx::PgPool;
use url::form_urlencoded;
use wired_be::{
    config::Config,
    models::{
        oauth_app::{self, NewOauthApp, OauthApp},
        oauth_login_state,
        user::User,
        user_identity,
    },
};

use super::provider::{MockProvider, OOB, PROVIDER, REDIRECT, UNLINKED_UID};
use crate::common;

/// The mock, a router built on its configuration, a registered app and a
/// live user: the fixtures nearly every test runs on.
pub(super) struct Site {
    /// The provider the callback exchanges a code against.
    pub(super) provider: MockProvider,
    /// The real router, on the pool `fixture` was given.
    pub(super) http: Router,
    /// The registered application whose parameters get prepared.
    pub(super) app: OauthApp,
    /// The local user the happy path links to.
    pub(super) user: User,
}

/// Starts the mock, builds the router on `pool` with both providers, and
/// registers an app and a user.
pub(super) async fn fixture(pool: PgPool, nickname: &str, app_scopes: &str) -> Site {
    fixture_with(pool, nickname, app_scopes, |_| ()).await
}

/// The same, with `tweak` applied to the configuration before the router
/// is built: the registration tests flip `registrations_open` and the two
/// registration modes this way.
pub(super) async fn fixture_with(
    pool: PgPool,
    nickname: &str,
    app_scopes: &str,
    tweak: impl FnOnce(&mut Config),
) -> Site {
    let provider = MockProvider::start().await;
    let mut config = provider.config();
    tweak(&mut config);
    let http = common::test_app_with_config(pool.clone(), config);
    let app = common::create_app(&pool, app_scopes).await;
    let user = common::create_local_user(&pool, nickname).await;
    Site {
        provider,
        http,
        app,
        user,
    }
}

/// Links `uid` at [`PROVIDER`] to `user`.
pub(super) async fn link(pool: &PgPool, user: &User, uid: &str) {
    let made = user_identity::link(pool, user.id, PROVIDER, uid)
        .await
        .expect("the identity fixture links");
    assert_eq!(made.user_id, user.id, "the fixture links the given user");
}

/// An app registered with the out-of-band redirect URI only.
pub(super) async fn oob_app(pool: &PgPool, scopes: &str) -> OauthApp {
    oauth_app::create(
        pool,
        &NewOauthApp {
            client_name: "OOB Client".to_owned(),
            redirect_uris: OOB.to_owned(),
            scopes: scopes.to_owned(),
            website: None,
            user_id: None,
        },
    )
    .await
    .expect("the out-of-band app fixture inserts")
}

/// `GET /oauth/prepare_request` for `app` and returns the state out of the
/// URL it answers with. `extra` rides onto the query string.
pub(super) async fn prepare(http: &Router, app: &OauthApp, extra: &str) -> String {
    prepare_to(http, app, REDIRECT, extra).await
}

/// The same, for an app whose registered redirect URI is not the fixture's
/// (the out-of-band client).
pub(super) async fn prepare_to(
    http: &Router,
    app: &OauthApp,
    redirect_uri: &str,
    extra: &str,
) -> String {
    let response = common::TestRequest::get(&format!(
        "/oauth/prepare_request?provider={PROVIDER}&response_type=code&client_id={}&redirect_uri={redirect_uri}{extra}",
        app.client_id
    ))
    .send(http)
    .await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "prepare_request succeeds: {}",
        response.text()
    );
    let url = response.json_value()["url"]
        .as_str()
        .expect("the answer carries a url")
        .to_owned();
    query_param(&url, "state").expect("the url names the issued state")
}

/// The named parameter of a URL's query string.
pub(super) fn query_param(url: &str, name: &str) -> Option<String> {
    let (_, query) = url.split_once('?')?;
    form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value)
}

/// The pairs of a URL's query string, in order.
pub(super) fn query_pairs(url: &str) -> Vec<(String, String)> {
    let (_, query) = url.split_once('?').expect("a query string");
    form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect()
}

/// The `Location` of a redirect answer.
pub(super) fn location(response: &common::TestResponse) -> String {
    response
        .header("location")
        .expect("a redirect carries a Location")
        .to_owned()
}

/// Asserts the `Cache-Control: no-store` every `/oauth` answer carries.
pub(super) fn assert_no_store(response: &common::TestResponse) {
    assert_eq!(
        response.header("cache-control"),
        Some("no-store"),
        "cache-control: no-store"
    );
}

/// The `error` message of a JSON error response.
pub(super) fn error_text(response: &common::TestResponse) -> String {
    response.json_value()["error"]
        .as_str()
        .expect("errors carry a string error field")
        .to_owned()
}

/// The parameters stored against `state`, and its provider.
pub(super) async fn stored_params(pool: &PgPool, state: &str) -> (String, Value) {
    sqlx::query_as("SELECT provider, params FROM oauth_login_states WHERE state = $1")
        .bind(state)
        .fetch_one(pool)
        .await
        .expect("the state row is stored")
}

/// Seconds of validity left on `state`.
pub(super) async fn seconds_left(pool: &PgPool, state: &str) -> i64 {
    sqlx::query_scalar("SELECT (EXTRACT(EPOCH FROM (valid_until - now())))::int8 FROM oauth_login_states WHERE state = $1")
        .bind(state)
        .fetch_one(pool)
        .await
        .expect("the expiry reads")
}

/// Ages `state` out.
pub(super) async fn expire(pool: &PgPool, state: &str) {
    let rewound = sqlx::query(
        "UPDATE oauth_login_states SET valid_until = now() - interval '1 second' WHERE state = $1",
    )
    .bind(state)
    .execute(pool)
    .await
    .expect("the rewind runs");
    assert_eq!(rewound.rows_affected(), 1, "the rewind hit the state");
}

/// Rows of `oauth_login_states`.
pub(super) async fn count_states(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM oauth_login_states")
        .fetch_one(pool)
        .await
        .expect("the state count runs")
}

/// Rows of `oauth_authorizations`.
pub(super) async fn count_codes(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM oauth_authorizations")
        .fetch_one(pool)
        .await
        .expect("the code count runs")
}

/// Rows of `user_identities`.
pub(super) async fn count_identities(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM user_identities")
        .fetch_one(pool)
        .await
        .expect("the identity count runs")
}

/// Runs the flow to the hand-off: prepare for `app`, callback the
/// [`UNLINKED_UID`] account, and return the state that now carries the
/// pending registration. `extra` rides onto the prepare query.
pub(super) async fn pending_state(http: &Router, app: &OauthApp, extra: &str) -> String {
    let state = prepare(http, app, extra).await;
    let response = common::TestRequest::get(&format!(
        "/oauth/{PROVIDER}/callback?code={UNLINKED_UID}&state={state}"
    ))
    .send(http)
    .await;
    assert_eq!(
        response.status,
        StatusCode::FOUND,
        "the unlinked callback hands off: {}",
        response.text()
    );
    state
}

/// Posts the registration form with the given fields.
pub(super) async fn post_form(http: &Router, fields: &[(&str, &str)]) -> common::TestResponse {
    common::TestRequest::post("/oauth/register")
        .form(fields)
        .send(http)
        .await
}

/// The nickname a callback for [`UNLINKED_UID`] reports: the mock's
/// `login` field, and the form's prefilled nickname.
pub(super) fn pending_nickname() -> String {
    format!("user-{UNLINKED_UID}")
}

/// Rows of `users`.
pub(super) async fn count_users(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM users")
        .fetch_one(pool)
        .await
        .expect("the user count runs")
}

/// Writes a `pending` document of the test's choosing onto `state`, the
/// way an unlinked callback would.
pub(super) async fn set_pending(pool: &PgPool, state: &str, pending: &Value) {
    let written = oauth_login_state::set_pending(pool, state, pending)
        .await
        .expect("the pending fixture writes");
    assert!(written, "the state row is live to take the write");
}
