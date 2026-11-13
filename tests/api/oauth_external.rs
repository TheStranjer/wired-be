//! Integration tests for the external OAuth login endpoints: `GET
//! /oauth/prepare_request`, `GET /oauth/{provider}`, `GET
//! /oauth/{provider}/callback` and the registration form that picks up an
//! unlinked account — `GET /oauth/registration_details` and `POST
//! /oauth/register` (`docs/ARCHITECTURE.md` decision 13).
//!
//! A mock provider runs on a loopback port — its authorize route answers
//! 500, because the flow must only *redirect* a browser there and never
//! fetch it itself — beside its token exchange and userinfo read. The code a
//! test hands the callback is the mock's scenario switch: a plain number is
//! the uid the document answers with, and the reserved words in
//! [`token_endpoint`] and [`userinfo_endpoint`] fail one step of the chain.
//! Each test drives the three endpoints in order, as a browser would, and
//! asserts on what the router answers and what the database ends up
//! holding: the prepared state and its ten minutes, the provider redirect's
//! parameters, a code the owning application can redeem exactly once
//! (`consume_authorization` is the lookup the code exchange runs), the
//! `pending` hand-off of an unlinked account, and every refusal on the way.

#![cfg(test)]

use axum::{
    Router,
    body::Body,
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::{get, post},
};
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::{net::TcpListener, task::JoinHandle};
use url::form_urlencoded;
use wired_be::{
    config::{Config, OauthProvider, Toggle},
    models::{
        oauth_app::{self, NewOauthApp, OauthApp},
        oauth_login_state, oauth_token,
        user::{self, User, register},
        user_identity,
    },
};

use crate::common;

/// The redirect URI the app fixture registers.
const REDIRECT: &str = "https://client.example/callback";
/// The out-of-band redirect URI: the code is shown on a page instead of
/// redirected (Pleroma's `@oob_token_redirect_uri`).
const OOB: &str = "urn:ietf:wg:oauth:2.0:oob";
/// The provider name the fixture configuration answers.
const PROVIDER: &str = "github";
/// A second configured provider, for a state issued to another one.
const OTHER: &str = "keycloak";
/// The provider's own client id, as the fixture configuration sets it.
const PROVIDER_CLIENT_ID: &str = "provider-client-id";
/// The provider's own client secret, as the fixture configuration sets it.
const PROVIDER_CLIENT_SECRET: &str = "provider-client-secret";
/// The scopes the fixture provider asks the provider for.
const PROVIDER_SCOPES: &str = "read:user user:email";
/// The uid the happy path links to a local user.
const LINKED_UID: &str = "4242";
/// A uid nobody has linked.
const UNLINKED_UID: &str = "90210";
/// The callback URL the provider is told to answer to.
const CALLBACK_URI: &str = "https://test.example/oauth/github/callback";

/// The mock provider, on a loopback port of its own.
#[derive(Debug)]
struct MockProvider {
    /// The `http://{addr}` prefix of its routes.
    base_url: String,
    /// The task serving it, aborted when the test is done.
    server: JoinHandle<()>,
}

impl MockProvider {
    /// Binds a port and serves the mock provider until [`stop`](Self::stop).
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("the mock provider binds a port");
        let base_url = format!("http://{}", listener.local_addr().expect("a bound address"));
        let server = tokio::spawn(async move {
            let app = Router::new()
                .route("/authorize", get(authorize_page))
                .route("/token", post(token_endpoint))
                .route("/userinfo", get(userinfo_endpoint));
            if let Err(err) = axum::serve(listener, app).await {
                tracing::debug!("mock provider stopped: {err}");
            }
        });
        Self { base_url, server }
    }

    /// Stops the server.
    fn stop(self) {
        self.server.abort();
    }

    /// The provider definition pointing at this mock, with the field names
    /// the mock's document carries at its top level.
    fn provider(&self, name: &str) -> OauthProvider {
        OauthProvider {
            name: name.to_owned(),
            client_id: PROVIDER_CLIENT_ID.to_owned(),
            client_secret: PROVIDER_CLIENT_SECRET.to_owned(),
            authorize_url: format!("{}/authorize", self.base_url),
            token_url: format!("{}/token", self.base_url),
            userinfo_url: format!("{}/userinfo", self.base_url),
            scopes: PROVIDER_SCOPES.to_owned(),
            uid_field: "id".to_owned(),
            nickname_field: "login".to_owned(),
            email_field: "email".to_owned(),
            name_field: "name".to_owned(),
        }
    }

    /// The same definition with every field read through a dotted path into
    /// the mock's `nested` object.
    fn nested_provider(&self, name: &str) -> OauthProvider {
        OauthProvider {
            uid_field: "nested.id".to_owned(),
            nickname_field: "nested.handle".to_owned(),
            email_field: "nested.missing".to_owned(),
            ..self.provider(name)
        }
    }

    /// [`common::test_config`] with `providers` configured.
    fn config_with(providers: Vec<OauthProvider>) -> Config {
        let mut config = common::test_config();
        config.oauth_providers = providers;
        config
    }

    /// [`common::test_config`] with both fixture providers configured.
    fn config(&self) -> Config {
        Self::config_with(vec![self.provider(PROVIDER), self.provider(OTHER)])
    }
}

/// The mock's authorize page. Nothing here ever fetches it: the flow hands
/// the URL to a browser, so an answer of 500 turns a server-to-server fetch
/// of the authorization endpoint into a failed login a test can see.
async fn authorize_page() -> Response {
    axum::http::Response::builder()
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .body(Body::from("the authorize page is a browser's to visit"))
        .expect("a fixed response")
}

/// The token endpoint: the form exchange is checked field by field, and the
/// `code` names the scenario — `server-error` refuses the exchange,
/// `no-token` answers without an `access_token`, and anything else hands
/// out `at-{code}`.
async fn token_endpoint(headers: HeaderMap, body: String) -> Response {
    let pairs: Vec<(String, String)> = form_urlencoded::parse(body.as_bytes())
        .into_owned()
        .collect();
    let field = |name: &str| {
        pairs
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
    };
    let form_ok = headers
        .get("content-type")
        .is_some_and(|value| value == "application/x-www-form-urlencoded");
    let well_formed = form_ok
        && field("grant_type").as_deref() == Some("authorization_code")
        && field("code").is_some_and(|code| !code.is_empty())
        && field("redirect_uri").as_deref() == Some(CALLBACK_URI)
        && field("client_id").as_deref() == Some(PROVIDER_CLIENT_ID)
        && field("client_secret").as_deref() == Some(PROVIDER_CLIENT_SECRET);
    if !well_formed {
        return json_response(
            StatusCode::BAD_REQUEST,
            &json!({"error": "invalid_request", "sent_fields": pairs.len()}),
        );
    }
    match field("code").unwrap_or_default().as_str() {
        "server-error" => json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &json!({"error": "server_error"}),
        ),
        "no-token" => json_response(StatusCode::OK, &json!({"token_type": "Bearer"})),
        code => json_response(
            StatusCode::OK,
            &json!({"access_token": format!("at-{code}"), "token_type": "Bearer"}),
        ),
    }
}

/// The userinfo endpoint: the bearer must be the one the exchange handed
/// out, and its suffix names the document — `html` answers something that is
/// not JSON, `no-uid` a document without the configured id field,
/// `forbidden` refuses the read, and anything else is read as a uid.
async fn userinfo_endpoint(headers: HeaderMap) -> Response {
    let Some(token) = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer at-"))
        .map(str::to_owned)
    else {
        return json_response(StatusCode::UNAUTHORIZED, &json!({"error": "no_bearer"}));
    };
    match token.as_str() {
        "html" => axum::http::Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/html")
            .body(Body::from("<p>not JSON</p>"))
            .expect("a fixed response"),
        "forbidden" => json_response(StatusCode::FORBIDDEN, &json!({"error": "nope"})),
        "no-uid" => json_response(StatusCode::OK, &json!({"login": "nameless"})),
        uid => json_response(
            StatusCode::OK,
            &json!({
                "id": uid,
                "login": format!("user-{uid}"),
                "email": format!("user-{uid}@example.org"),
                "name": format!("User {uid}"),
                "nested": {"id": uid, "handle": format!("user-{uid}")},
            }),
        ),
    }
}

/// A JSON response with `status` and `body`.
fn json_response(status: StatusCode, body: &Value) -> Response {
    axum::http::Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("a fixed response")
}

/// The mock, a router built on its configuration, a registered app and a
/// live user: the fixtures nearly every test runs on.
struct Site {
    /// The provider the callback exchanges a code against.
    provider: MockProvider,
    /// The real router, on the pool `fixture` was given.
    http: Router,
    /// The registered application whose parameters get prepared.
    app: OauthApp,
    /// The local user the happy path links to.
    user: User,
}

/// Starts the mock, builds the router on `pool` with both providers, and
/// registers an app and a user.
async fn fixture(pool: PgPool, nickname: &str, app_scopes: &str) -> Site {
    fixture_with(pool, nickname, app_scopes, |_| ()).await
}

/// The same, with `tweak` applied to the configuration before the router
/// is built: the registration tests flip `registrations_open` and the two
/// registration modes this way.
async fn fixture_with(
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
async fn link(pool: &PgPool, user: &User, uid: &str) {
    let made = user_identity::link(pool, user.id, PROVIDER, uid)
        .await
        .expect("the identity fixture links");
    assert_eq!(made.user_id, user.id, "the fixture links the given user");
}

/// An app registered with the out-of-band redirect URI only.
async fn oob_app(pool: &PgPool, scopes: &str) -> OauthApp {
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
async fn prepare(http: &Router, app: &OauthApp, extra: &str) -> String {
    prepare_to(http, app, REDIRECT, extra).await
}

/// The same, for an app whose registered redirect URI is not the fixture's
/// (the out-of-band client).
async fn prepare_to(http: &Router, app: &OauthApp, redirect_uri: &str, extra: &str) -> String {
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
fn query_param(url: &str, name: &str) -> Option<String> {
    let (_, query) = url.split_once('?')?;
    form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value)
}

/// The pairs of a URL's query string, in order.
fn query_pairs(url: &str) -> Vec<(String, String)> {
    let (_, query) = url.split_once('?').expect("a query string");
    form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect()
}

/// The `Location` of a redirect answer.
fn location(response: &common::TestResponse) -> String {
    response
        .header("location")
        .expect("a redirect carries a Location")
        .to_owned()
}

/// Asserts the `Cache-Control: no-store` every `/oauth` answer carries.
fn assert_no_store(response: &common::TestResponse) {
    assert_eq!(
        response.header("cache-control"),
        Some("no-store"),
        "cache-control: no-store"
    );
}

/// The `error` message of a JSON error response.
fn error_text(response: &common::TestResponse) -> String {
    response.json_value()["error"]
        .as_str()
        .expect("errors carry a string error field")
        .to_owned()
}

/// The parameters stored against `state`, and its provider.
async fn stored_params(pool: &PgPool, state: &str) -> (String, Value) {
    sqlx::query_as("SELECT provider, params FROM oauth_login_states WHERE state = $1")
        .bind(state)
        .fetch_one(pool)
        .await
        .expect("the state row is stored")
}

/// Seconds of validity left on `state`.
async fn seconds_left(pool: &PgPool, state: &str) -> i64 {
    sqlx::query_scalar("SELECT (EXTRACT(EPOCH FROM (valid_until - now())))::int8 FROM oauth_login_states WHERE state = $1")
        .bind(state)
        .fetch_one(pool)
        .await
        .expect("the expiry reads")
}

/// Ages `state` out.
async fn expire(pool: &PgPool, state: &str) {
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
async fn count_states(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM oauth_login_states")
        .fetch_one(pool)
        .await
        .expect("the state count runs")
}

/// Rows of `oauth_authorizations`.
async fn count_codes(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM oauth_authorizations")
        .fetch_one(pool)
        .await
        .expect("the code count runs")
}

/// Rows of `user_identities`.
async fn count_identities(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM user_identities")
        .fetch_one(pool)
        .await
        .expect("the identity count runs")
}

/// A prepared round trip answers 200 `{"url": …}` naming this instance's
/// provider entry point and an issued state, and stores the application's
/// own parameters against it for ten minutes.
#[tokio::test]
async fn prepare_request_stores_the_round_trip() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture(pool.clone(), "alice", "read write").await;
    let response = common::TestRequest::get(&format!(
        "/oauth/prepare_request?provider={PROVIDER}&response_type=code&client_id={}&redirect_uri={REDIRECT}&scope=read+write&state=client-state",
        site.app.client_id
    ))
    .send(&site.http)
    .await;

    assert_eq!(response.status, StatusCode::OK, "it answers");
    assert_no_store(&response);
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "a JSON body"
    );
    let body: Value = response.json();
    let url = body["url"].as_str().expect("the one `url` key");
    assert_eq!(
        body.as_object().map_or(0, serde_json::Map::len),
        1,
        "`url` and nothing else: {body}"
    );
    let state = query_param(url, "state").expect("a state on the url");
    assert_eq!(
        url,
        format!("https://test.example/oauth/{PROVIDER}?state={state}"),
        "the entry point on this instance's own origin"
    );
    assert_eq!(state.len(), 43, "an opaque URL-safe state");

    let (provider, params) = stored_params(&pool, &state).await;
    assert_eq!(provider, PROVIDER, "issued for the named provider");
    assert_eq!(
        params,
        json!({
            "client_id": site.app.client_id,
            "redirect_uri": REDIRECT,
            "scope": "read write",
            "state": "client-state",
        }),
        "the application's own parameters, the granted scopes normalized"
    );
    let left = seconds_left(&pool, &state).await;
    assert!(
        left > 540 && left <= 600,
        "the state lives the ticket's ten minutes, got {left}s"
    );
    site.provider.stop();
}

/// A missing `scope` prepares the default one, and a missing client `state`
/// stores none to echo.
#[tokio::test]
async fn prepare_request_defaults_the_optional_parameters() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture(pool.clone(), "beth", "read").await;
    let state = prepare(&site.http, &site.app, "").await;
    let (_, params) = stored_params(&pool, &state).await;
    assert_eq!(
        params,
        json!({
            "client_id": site.app.client_id,
            "redirect_uri": REDIRECT,
            "scope": "read",
        }),
        "`read` is the scope a client that named none asked for"
    );
    site.provider.stop();
}

/// A name `OAUTH_PROVIDERS` does not configure is the 404 on all three
/// paths, before any application check and without a query.
#[tokio::test]
async fn an_unconfigured_provider_is_a_404_everywhere() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture(pool.clone(), "carl", "read").await;
    for uri in [
        "/oauth/prepare_request?provider=gitlab&response_type=code",
        "/oauth/prepare_request?response_type=code",
        "/oauth/gitlab?state=whatever",
        "/oauth/gitlab/callback?state=whatever",
    ] {
        let response = common::TestRequest::get(uri).send(&site.http).await;
        assert_eq!(response.status, StatusCode::NOT_FOUND, "`{uri}`");
        assert_eq!(error_text(&response), "Unknown provider", "for `{uri}`");
        assert_no_store(&response);
    }
    site.provider.stop();
}

/// The application half of `prepare_request` is `GET /oauth/authorize`'s:
/// flow, client, redirect URI and scopes are refused with the very same
/// bodies, and none of the refusals stores a state.
#[tokio::test]
async fn prepare_request_validates_the_app_parameters() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture(pool.clone(), "dora", "read").await;
    let client = &site.app.client_id;
    for (query, want) in [
        (
            format!("response_type=token&client_id={client}&redirect_uri={REDIRECT}"),
            "unsupported_response_type",
        ),
        (
            "response_type=code&client_id=nobody&redirect_uri=https://x.example/cb".to_owned(),
            "Invalid client",
        ),
        (
            format!("response_type=code&client_id={client}&redirect_uri=https://evil.example/cb"),
            "Invalid redirect_uri",
        ),
        (
            format!(
                "response_type=code&client_id={client}&redirect_uri={REDIRECT}&scope=read+write"
            ),
            "Invalid scope",
        ),
        (
            format!("client_id={client}&redirect_uri={REDIRECT}"),
            "unsupported_response_type",
        ),
    ] {
        let response = common::TestRequest::get(&format!(
            "/oauth/prepare_request?provider={PROVIDER}&{query}"
        ))
        .send(&site.http)
        .await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST, "`{query}`");
        assert_eq!(error_text(&response), want, "for `{query}`");
        assert_no_store(&response);
    }
    assert_eq!(count_states(&pool).await, 0, "no refusal stored a state");
    site.provider.stop();
}

/// The provider redirect carries the configured client, this instance's
/// callback URL, the code flow, the provider's own scopes and the state —
/// in that order, with nothing of the app's parameters leaked into it.
#[tokio::test]
async fn the_provider_redirect_names_the_flow() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture(pool.clone(), "erin", "read write").await;
    let state = prepare(
        &site.http,
        &site.app,
        "&scope=read+write&state=client-state",
    )
    .await;

    let response = common::TestRequest::get(&format!("/oauth/{PROVIDER}?state={state}"))
        .send(&site.http)
        .await;
    assert_eq!(response.status, StatusCode::FOUND, "a 302 to the provider");
    assert_no_store(&response);
    let url = location(&response);
    let (base, _) = url.split_once('?').expect("a query string");
    assert_eq!(
        base,
        format!("{}/authorize", site.provider.base_url),
        "the configured authorize endpoint, verbatim"
    );
    assert_eq!(
        query_pairs(&url),
        vec![
            ("client_id".to_owned(), PROVIDER_CLIENT_ID.to_owned()),
            ("redirect_uri".to_owned(), CALLBACK_URI.to_owned()),
            ("response_type".to_owned(), "code".to_owned()),
            ("scope".to_owned(), PROVIDER_SCOPES.to_owned()),
            ("state".to_owned(), state),
        ],
        "the round trip's five parameters, in order"
    );
    site.provider.stop();
}

/// An unissued state, an empty one, an absent one, an expired one and one
/// issued to another provider all read as `Invalid state` — on both the
/// redirect and its callback.
#[tokio::test]
async fn a_bad_state_is_refused_on_both_legs() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture(pool.clone(), "fern", "read").await;
    let live = prepare(&site.http, &site.app, "").await;
    let aged = prepare(&site.http, &site.app, "").await;
    expire(&pool, &aged).await;

    for (uri, why) in [
        (
            format!("/oauth/{PROVIDER}?state=never-issued"),
            "never issued",
        ),
        (format!("/oauth/{PROVIDER}?state="), "empty"),
        (format!("/oauth/{PROVIDER}"), "absent"),
        (format!("/oauth/{PROVIDER}?state={aged}"), "expired"),
        (format!("/oauth/{OTHER}?state={live}"), "another provider's"),
        (
            format!("/oauth/{PROVIDER}/callback?code={LINKED_UID}&state=never-issued"),
            "never issued, on the callback",
        ),
        (
            format!("/oauth/{PROVIDER}/callback?code={LINKED_UID}&state={aged}"),
            "expired, on the callback",
        ),
        (
            format!("/oauth/{OTHER}/callback?code={LINKED_UID}&state={live}"),
            "another provider's, on the callback",
        ),
        (
            format!("/oauth/{PROVIDER}/callback?code={LINKED_UID}"),
            "absent, on the callback",
        ),
    ] {
        let response = common::TestRequest::get(&uri).send(&site.http).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST, "{why}");
        assert_eq!(
            error_text(&response),
            "Invalid state",
            "for the state that {why}"
        );
        assert_no_store(&response);
    }
    assert_eq!(
        count_identities(&pool).await,
        0,
        "no refusal linked anything"
    );
    site.provider.stop();
}

/// A callback whose provider account is linked logs that user in: the answer
/// is the app's redirect with a code, minted for the *original* application
/// and scopes, redeemable by the owning application exactly once.
///
/// `/oauth/token` does not speak the `authorization_code` grant yet (a later
/// ticket), so the code's usability is proven through
/// [`consume_authorization`](wired_be::models::oauth_token::consume_authorization)
/// — the one-statement redemption that exchange runs.
#[tokio::test]
async fn the_callback_logs_in_a_linked_user_with_a_code() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture(pool.clone(), "gabe", "read write").await;
    link(&pool, &site.user, LINKED_UID).await;

    let state = prepare(
        &site.http,
        &site.app,
        "&scope=read+write&state=client-state",
    )
    .await;
    let redirect = common::TestRequest::get(&format!("/oauth/{PROVIDER}?state={state}"))
        .send(&site.http)
        .await;
    assert_eq!(redirect.status, StatusCode::FOUND, "the provider leg");
    let response = common::TestRequest::get(&format!(
        "/oauth/{PROVIDER}/callback?code={LINKED_UID}&state={state}"
    ))
    .send(&site.http)
    .await;

    assert_eq!(response.status, StatusCode::FOUND, "the app is redirected");
    assert_no_store(&response);
    let url = location(&response);
    let code = query_param(&url, "code").expect("a code on the redirect");
    assert_eq!(
        url,
        format!("{REDIRECT}?code={code}&state=client-state"),
        "the registered URI, the code and the client's state — nothing else"
    );
    assert_eq!(code.len(), 43, "the 43-character Pleroma code shape");

    let stored: oauth_token::OauthAuthorization =
        sqlx::query_as("SELECT * FROM oauth_authorizations WHERE token = $1")
            .bind(&code)
            .fetch_one(&pool)
            .await
            .expect("the code row is stored");
    assert_eq!(
        stored.user_id, site.user.id,
        "granted by the linked account"
    );
    assert_eq!(stored.app_id, site.app.id, "for the prepared application");
    assert_eq!(stored.scopes, "read write", "the scopes prepared with");
    assert!(!stored.used, "a fresh code is unused");

    let redeemed = oauth_token::consume_authorization(&pool, &code, site.app.id)
        .await
        .expect("the redemption reaches the database")
        .expect("the owning app can redeem the code");
    assert_eq!(redeemed.user_id, site.user.id, "and it names the login");
    assert!(
        oauth_token::consume_authorization(&pool, &code, site.app.id)
            .await
            .expect("the second attempt reaches the database")
            .is_none(),
        "the code is spent after one redemption"
    );
    site.provider.stop();
}

/// A client that sent no `state` gets only the code back.
#[tokio::test]
async fn the_callback_echoes_only_what_the_client_sent() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture(pool.clone(), "helen", "read").await;
    link(&pool, &site.user, LINKED_UID).await;
    let state = prepare(&site.http, &site.app, "").await;
    let response = common::TestRequest::get(&format!(
        "/oauth/{PROVIDER}/callback?code={LINKED_UID}&state={state}"
    ))
    .send(&site.http)
    .await;

    assert_eq!(response.status, StatusCode::FOUND, "the app is redirected");
    assert_eq!(
        query_pairs(&location(&response)),
        vec![(
            "code".to_owned(),
            query_param(&location(&response), "code").expect("a code")
        )],
        "the code alone"
    );
    site.provider.stop();
}

/// An app registered with the out-of-band redirect URI gets the code page
/// instead of a redirect, exactly as `POST /oauth/authorize` answers it.
#[tokio::test]
async fn the_callback_answers_the_out_of_band_code_page() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture(pool.clone(), "ivan", "read").await;
    let app = oob_app(&pool, "read").await;
    link(&pool, &site.user, LINKED_UID).await;
    let state = prepare_to(&site.http, &app, OOB, "").await;

    let response = common::TestRequest::get(&format!(
        "/oauth/{PROVIDER}/callback?code={LINKED_UID}&state={state}"
    ))
    .send(&site.http)
    .await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "a page, not a redirect: {}",
        response.text()
    );
    assert_eq!(
        response.header("content-type"),
        Some("text/html; charset=utf-8"),
        "and it is HTML"
    );
    assert_no_store(&response);
    assert!(response.header("location").is_none(), "no redirect target");

    let page = response.text();
    let code: String = sqlx::query_scalar("SELECT token FROM oauth_authorizations")
        .fetch_one(&pool)
        .await
        .expect("the code was minted for the out-of-band client");
    assert!(
        page.contains(code.as_str()),
        "the page shows the code the client will read off it: {page}"
    );
    let stored: oauth_token::OauthAuthorization =
        sqlx::query_as("SELECT * FROM oauth_authorizations")
            .fetch_one(&pool)
            .await
            .expect("the code row is stored");
    assert_eq!(
        stored.user_id, site.user.id,
        "granted to the linked account"
    );
    assert_eq!(stored.app_id, app.id, "for the out-of-band application");
    site.provider.stop();
}

/// An unlinked provider account is not a login: the state keeps its authorize
/// parameters, gains the provider's account info under `pending`, and the
/// browser is sent to the registration form. No code is minted, and a repeat
/// callback rewrites one `pending` instead of stacking another.
#[tokio::test]
async fn the_callback_hands_an_unlinked_account_to_registration() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture(pool.clone(), "june", "read").await;
    let state = prepare(&site.http, &site.app, "").await;
    let response = common::TestRequest::get(&format!(
        "/oauth/{PROVIDER}/callback?code={UNLINKED_UID}&state={state}"
    ))
    .send(&site.http)
    .await;

    assert_eq!(response.status, StatusCode::FOUND, "a hand-off redirect");
    assert_no_store(&response);
    assert_eq!(
        location(&response),
        format!("https://test.example/oauth/registration_details?state={state}"),
        "the registration form, with the state that earned it"
    );

    let (_, first) = stored_params(&pool, &state).await;
    assert_eq!(
        first.get("pending"),
        Some(&json!({
            "provider": PROVIDER,
            "uid": UNLINKED_UID,
            "nickname": format!("user-{UNLINKED_UID}"),
            "email": format!("user-{UNLINKED_UID}@example.org"),
            "name": format!("User {UNLINKED_UID}"),
        })),
        "the provider's account info, under `pending`: {first}"
    );
    assert_eq!(
        first.get("client_id"),
        Some(&json!(site.app.client_id)),
        "the authorize parameters are untouched"
    );
    assert_eq!(
        count_codes(&pool).await,
        0,
        "no code was minted for an unlinked account"
    );

    let again = common::TestRequest::get(&format!(
        "/oauth/{PROVIDER}/callback?code={UNLINKED_UID}&state={state}"
    ))
    .send(&site.http)
    .await;
    assert_eq!(
        again.status,
        StatusCode::FOUND,
        "the hand-off is repeatable"
    );
    let (_, params) = stored_params(&pool, &state).await;
    assert_eq!(
        params.get("pending").and_then(|p| p.get("uid")),
        Some(&json!(UNLINKED_UID)),
        "one `pending`, rewritten in place: {params}"
    );
    assert_eq!(
        params.as_object().map_or(0, serde_json::Map::len),
        4,
        "authorize parameters and `pending`, no other keys: {params}"
    );
    site.provider.stop();
}

/// Every provider failure — its own `error` parameter, no code, a refused
/// token exchange, an answer with no token, a userinfo read that failed or
/// answered no JSON, and a document naming no account id — is bounced back to
/// the app's redirect URI as `error=access_denied`, with no code minted.
#[tokio::test]
async fn the_callback_bounces_a_provider_failure_to_the_app() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture(pool.clone(), "kate", "read write").await;
    link(&pool, &site.user, LINKED_UID).await;

    for (query, why) in [
        (String::new(), "no code at all"),
        (
            format!("error=access_denied&code={LINKED_UID}"),
            "the provider's own refusal",
        ),
        (
            "code=server-error".to_owned(),
            "a token endpoint that failed",
        ),
        ("code=no-token".to_owned(), "an answer with no access_token"),
        (
            "code=forbidden".to_owned(),
            "a userinfo read that was refused",
        ),
        ("code=html".to_owned(), "a userinfo answer that is not JSON"),
        ("code=no-uid".to_owned(), "a document naming no account id"),
    ] {
        let state = prepare(&site.http, &site.app, "").await;
        let response = common::TestRequest::get(&format!(
            "/oauth/{PROVIDER}/callback?state={state}{}",
            if query.is_empty() {
                String::new()
            } else {
                format!("&{query}")
            }
        ))
        .send(&site.http)
        .await;
        assert_eq!(response.status, StatusCode::FOUND, "{why} redirects");
        assert_no_store(&response);
        assert_eq!(
            location(&response),
            format!("{REDIRECT}?error=access_denied"),
            "the app is told access was denied, for {why}"
        );
    }
    assert_eq!(
        count_codes(&pool).await,
        0,
        "none of the failures minted a code"
    );
    site.provider.stop();
}

/// The client's `state` rides the failure redirect too, so the app can tie
/// the refusal back to the login it started.
#[tokio::test]
async fn the_provider_failure_echoes_the_client_state() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture(pool.clone(), "liam", "read").await;
    let state = prepare(&site.http, &site.app, "&state=client-state").await;
    let response = common::TestRequest::get(&format!(
        "/oauth/{PROVIDER}/callback?error=access_denied&state={state}"
    ))
    .send(&site.http)
    .await;

    assert_eq!(response.status, StatusCode::FOUND, "the refusal redirects");
    assert_eq!(
        location(&response),
        format!("{REDIRECT}?error=access_denied&state=client-state"),
        "with the state the app sent to prepare_request"
    );
    site.provider.stop();
}

/// A linked account that is deactivated is refused its login the way the
/// token endpoint refuses it — 403 with Pleroma's machine-readable reason,
/// not a code and not a registration form.
#[tokio::test]
async fn the_callback_refuses_a_deactivated_linked_user() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture(pool.clone(), "mallory", "read").await;
    let state = prepare(&site.http, &site.app, "").await;
    // The account is suspended after the round trip was prepared: the login
    // gate runs on the callback, not on prepare_request.
    let suspended = sqlx::query("UPDATE users SET is_active = false WHERE id = $1")
        .bind(site.user.id)
        .execute(&pool)
        .await
        .expect("the suspension runs");
    assert_eq!(suspended.rows_affected(), 1, "the fixture was suspended");
    link(&pool, &site.user, LINKED_UID).await;

    let response = common::TestRequest::get(&format!(
        "/oauth/{PROVIDER}/callback?code={LINKED_UID}&state={state}"
    ))
    .send(&site.http)
    .await;

    assert_eq!(
        response.status,
        StatusCode::FORBIDDEN,
        "disabled is no login"
    );
    assert_eq!(
        response.json_value()["error"],
        json!("Your account is currently disabled"),
        "Pleroma's wording: {}",
        response.text()
    );
    assert_eq!(
        response.json_value()["identifier"],
        json!("account_is_disabled"),
        "and its reason code"
    );
    assert_no_store(&response);
    assert_eq!(count_codes(&pool).await, 0, "the refusal minted no code");
    site.provider.stop();
}

/// A login prepared for an application that no longer exists cannot be
/// finished: the same 400 the consent endpoint gives an unknown client, and
/// no code is minted.
#[tokio::test]
async fn the_callback_refuses_a_deleted_application() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture(pool.clone(), "nina", "read").await;
    link(&pool, &site.user, LINKED_UID).await;
    let state = prepare(&site.http, &site.app, "").await;
    let gone = sqlx::query("DELETE FROM oauth_apps WHERE id = $1")
        .bind(site.app.id)
        .execute(&pool)
        .await
        .expect("the app fixture is deleted");
    assert_eq!(gone.rows_affected(), 1, "one application went away");

    let response = common::TestRequest::get(&format!(
        "/oauth/{PROVIDER}/callback?code={LINKED_UID}&state={state}"
    ))
    .send(&site.http)
    .await;
    assert_eq!(
        response.status,
        StatusCode::BAD_REQUEST,
        "no application to mint for"
    );
    assert_eq!(error_text(&response), "Invalid client", "named as such");
    assert_no_store(&response);
    site.provider.stop();
}

/// Both provider calls go through the SSRF-guarded client: with the guard on,
/// a provider on loopback is unreachable, and the callback bounces the
/// app's login back as denied rather than fetching it.
#[tokio::test]
async fn the_provider_calls_pass_the_ssrf_guard() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture(pool.clone(), "omar", "read").await;
    link(&pool, &site.user, LINKED_UID).await;
    let state = prepare(&site.http, &site.app, "").await;

    let mut guarded = site.provider.config();
    guarded.fetch.http_allow_private_hosts = false;
    let guarded_http = common::test_app_with_config(pool.clone(), guarded);
    let response = common::TestRequest::get(&format!(
        "/oauth/{PROVIDER}/callback?code={LINKED_UID}&state={state}"
    ))
    .send(&guarded_http)
    .await;

    assert_eq!(
        response.status,
        StatusCode::FOUND,
        "a blocked provider is a provider failure"
    );
    assert_eq!(
        location(&response),
        format!("{REDIRECT}?error=access_denied"),
        "and the app is told access was denied"
    );
    assert_eq!(
        count_codes(&pool).await,
        0,
        "nothing was minted on a blocked exchange"
    );
    site.provider.stop();
}

/// The configured field names are dotted paths into the provider's document:
/// `nested.id` reads the uid from the nested object, `nested.missing` says
/// the provider does not tell us an email, and the account behind the
/// nested id is logged in.
#[tokio::test]
async fn dotted_field_names_read_a_nested_document() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture(pool.clone(), "pam", "read").await;
    link(&pool, &site.user, LINKED_UID).await;
    let http = common::test_app_with_config(
        pool.clone(),
        MockProvider::config_with(vec![site.provider.nested_provider(PROVIDER)]),
    );

    let state = prepare(&http, &site.app, "").await;
    let response = common::TestRequest::get(&format!(
        "/oauth/{PROVIDER}/callback?code={LINKED_UID}&state={state}"
    ))
    .send(&http)
    .await;
    assert_eq!(
        response.status,
        StatusCode::FOUND,
        "the nested uid identifies the linked account"
    );
    assert!(
        query_param(&location(&response), "code").is_some(),
        "with a code: {}",
        location(&response)
    );

    // The same nested read on an unlinked account hands the registration
    // form what the provider did say, and a null for what it did not.
    let stranger = prepare(&http, &site.app, "").await;
    let unlinked = common::TestRequest::get(&format!(
        "/oauth/{PROVIDER}/callback?code={UNLINKED_UID}&state={stranger}"
    ))
    .send(&http)
    .await;
    assert_eq!(
        location(&unlinked),
        format!("https://test.example/oauth/registration_details?state={stranger}"),
        "the nested-field hand-off goes to the same form"
    );
    let (_, nested) = stored_params(&pool, &stranger).await;
    assert_eq!(
        nested.get("pending"),
        Some(&json!({
            "provider": PROVIDER,
            "uid": UNLINKED_UID,
            "nickname": format!("user-{UNLINKED_UID}"),
            "email": null,
            "name": format!("User {UNLINKED_UID}"),
        })),
        "the optional fields are dug out the same way: {nested}"
    );
    site.provider.stop();
}

/// `GET /oauth/authorize` and the token endpoints keep answering their own
/// paths: the `{provider}` segment must not swallow them.
#[tokio::test]
async fn the_provider_route_leaves_the_sibling_paths_alone() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture(pool.clone(), "quinn", "read").await;
    let consent = common::TestRequest::get("/oauth/authorize?response_type=token")
        .send(&site.http)
        .await;
    assert_eq!(
        consent.status,
        StatusCode::BAD_REQUEST,
        "the consent endpoint still runs its own flow check"
    );
    assert_eq!(
        error_text(&consent),
        "unsupported_response_type",
        "with its own body"
    );

    let token = common::TestRequest::post("/oauth/token")
        .json(&json!({"grant_type": "password"}))
        .send(&site.http)
        .await;
    assert_eq!(
        token.status,
        StatusCode::BAD_REQUEST,
        "the token endpoint answers"
    );
    assert_eq!(
        error_text(&token),
        "Invalid credentials",
        "with its own body"
    );
    site.provider.stop();
}

/// A user's own row is what the identity link points at: the model write the
/// endpoint reads through is the one the fixture used.
#[tokio::test]
async fn a_link_to_a_remote_actor_is_not_a_login() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture(pool.clone(), "rosa", "read").await;
    let remote = common::create_remote_user(&pool, "octocat", "remote.example").await;
    let made = user_identity::link(&pool, remote.id, PROVIDER, LINKED_UID)
        .await
        .expect("the identity fixture links a remote row");
    assert_eq!(made.user_id, remote.id, "the link names the remote actor");

    let state = prepare(&site.http, &site.app, "").await;
    let response = common::TestRequest::get(&format!(
        "/oauth/{PROVIDER}/callback?code={LINKED_UID}&state={state}"
    ))
    .send(&site.http)
    .await;
    assert_eq!(
        response.status,
        StatusCode::FOUND,
        "the remote link reads as unlinked"
    );
    assert_eq!(
        location(&response),
        format!("https://test.example/oauth/registration_details?state={state}"),
        "so the account goes to the registration form"
    );
    assert_eq!(
        count_codes(&pool).await,
        0,
        "and no code lets a remote actor log in"
    );
    site.provider.stop();
}

/// Runs the flow to the hand-off: prepare for `app`, callback the
/// [`UNLINKED_UID`] account, and return the state that now carries the
/// pending registration. `extra` rides onto the prepare query.
async fn pending_state(http: &Router, app: &OauthApp, extra: &str) -> String {
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
async fn post_form(http: &Router, fields: &[(&str, &str)]) -> common::TestResponse {
    common::TestRequest::post("/oauth/register")
        .form(fields)
        .send(http)
        .await
}

/// The nickname a callback for [`UNLINKED_UID`] reports: the mock's
/// `login` field, and the form's prefilled nickname.
fn pending_nickname() -> String {
    format!("user-{UNLINKED_UID}")
}

/// Rows of `users`.
async fn count_users(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM users")
        .fetch_one(pool)
        .await
        .expect("the user count runs")
}

/// Writes a `pending` document of the test's choosing onto `state`, the
/// way an unlinked callback would.
async fn set_pending(pool: &PgPool, state: &str, pending: &Value) {
    let written = oauth_login_state::set_pending(pool, state, pending)
        .await
        .expect("the pending fixture writes");
    assert!(written, "the state row is live to take the write");
}

/// `GET /oauth/registration_details` renders the form for `state`: 200
/// HTML with the `pending` account's nickname, email and name prefilled,
/// a `bio` field, the hidden state and the post target, under no-store.
#[tokio::test]
async fn registration_details_renders_the_prefilled_form() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture(pool.clone(), "rita", "read").await;
    let state = pending_state(&site.http, &site.app, "").await;

    let response = common::TestRequest::get(&format!("/oauth/registration_details?state={state}"))
        .send(&site.http)
        .await;
    assert_eq!(response.status, StatusCode::OK, "the form renders");
    assert_eq!(
        response.header("content-type"),
        Some("text/html; charset=utf-8"),
        "and it is a page"
    );
    assert_no_store(&response);

    let page = response.text();
    assert!(
        page.contains("<form method=\"post\" action=\"/oauth/register\">"),
        "the form posts to /oauth/register: {page}"
    );
    assert!(
        page.contains(&format!(
            "name=\"nickname\" value=\"{}\"",
            pending_nickname()
        )),
        "the provider's nickname is prefilled: {page}"
    );
    assert!(
        page.contains(&format!(
            "name=\"email\" value=\"user-{UNLINKED_UID}@example.org\""
        )),
        "so is the email: {page}"
    );
    assert!(
        page.contains(&format!("name=\"name\" value=\"User {UNLINKED_UID}\"")),
        "and the name: {page}"
    );
    assert!(page.contains("name=\"bio\""), "and the bio is editable");
    assert!(
        page.contains(&format!(
            "<input type=\"hidden\" name=\"state\" value=\"{state}\">"
        )),
        "the state rides along hidden: {page}"
    );
    site.provider.stop();
}

/// Provider info is escaped on the page: markup in a nickname, email or
/// name cannot open a tag or break out of its attribute.
#[tokio::test]
async fn registration_details_escapes_the_provider_values() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture(pool.clone(), "sara", "read").await;
    let state = prepare(&site.http, &site.app, "").await;
    set_pending(
        &pool,
        &state,
        &json!({
            "provider": PROVIDER,
            "uid": "1",
            "nickname": "\"><script>alert(1)</script>",
            "email": "a&b@example.org",
            "name": "<img src=x onerror=alert(2)>",
        }),
    )
    .await;

    let page = common::TestRequest::get(&format!("/oauth/registration_details?state={state}"))
        .send(&site.http)
        .await
        .text();
    assert!(
        page.contains("&lt;script&gt;alert(1)&lt;/script&gt;"),
        "the nickname shows escaped: {page}"
    );
    assert!(!page.contains("<script>alert(1)"), "and never raw");
    assert!(
        page.contains("value=\"a&amp;b@example.org\""),
        "ampersands escape in the email too"
    );
    assert!(
        page.contains("&lt;img src=x onerror=alert(2)&gt;"),
        "the name cannot embed an image"
    );
    assert!(
        !page.contains("\"><script>"),
        "and no value breaks out of its attribute"
    );
    site.provider.stop();
}

/// Both registration halves refuse states alike: never issued, empty,
/// absent, expired, and one that never saw an unlinked callback are the
/// 400 `{"error":"Invalid state"}` — no page, no user, no link.
#[tokio::test]
async fn registration_refuses_states_that_cannot_register() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture(pool.clone(), "tess", "read").await;
    let aged = prepare(&site.http, &site.app, "").await;
    expire(&pool, &aged).await;
    let no_pending = prepare(&site.http, &site.app, "").await;
    let aged_get = format!("/oauth/registration_details?state={aged}");
    let no_pending_get = format!("/oauth/registration_details?state={no_pending}");

    for (uri, why) in [
        (
            "/oauth/registration_details?state=never-issued",
            "never issued",
        ),
        ("/oauth/registration_details?state=", "empty"),
        ("/oauth/registration_details", "absent"),
        (aged_get.as_str(), "expired"),
        (no_pending_get.as_str(), "live but no pending account"),
    ] {
        let response = common::TestRequest::get(uri).send(&site.http).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST, "{why} (GET)");
        assert_eq!(
            response.header("content-type"),
            Some("application/json"),
            "the refusal is JSON, not a page, for the state that {why}"
        );
        assert_eq!(error_text(&response), "Invalid state", "for {why} (GET)");
        assert_no_store(&response);
    }

    for (state, why) in [
        ("never-issued", "never issued"),
        ("", "empty"),
        (aged.as_str(), "expired"),
        (no_pending.as_str(), "no pending account"),
    ] {
        let response = post_form(
            &site.http,
            &[
                ("state", state),
                ("nickname", "whoever"),
                ("email", "w@x.y"),
            ],
        )
        .await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST, "{why} (POST)");
        assert_eq!(error_text(&response), "Invalid state", "for {why} (POST)");
        assert_no_store(&response);
    }
    assert_eq!(count_users(&pool).await, 1, "only the fixture user");
    assert_eq!(count_identities(&pool).await, 0, "nothing was linked");
    assert_eq!(count_codes(&pool).await, 0, "and no code was minted");
    site.provider.stop();
}

/// The full hand-off in one go: an unlinked callback's state comes back
/// from `POST /oauth/register` as the very answer the callback gives a
/// linked user — a 302 to the app's `redirect_uri` with `code` and the
/// client's `state`. The code is redeemable by its application, the
/// identity is linked, the state row is gone, and the account carries a
/// password no one can type: `authenticate` refuses it.
#[tokio::test]
async fn registration_creates_the_user_and_mints_a_working_code() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture(pool.clone(), "uma", "read write").await;
    let state = pending_state(&site.http, &site.app, "&state=client-state").await;

    let response = post_form(
        &site.http,
        &[
            ("state", state.as_str()),
            ("nickname", "octocat"),
            ("email", "octocat@example.org"),
            ("name", "The Octocat"),
            ("bio", "lives in a repo"),
        ],
    )
    .await;
    assert_eq!(response.status, StatusCode::FOUND, "the app is redirected");
    assert_no_store(&response);
    let url = location(&response);
    let code = query_param(&url, "code").expect("a code on the redirect");
    assert_eq!(
        url,
        format!("{REDIRECT}?code={code}&state=client-state"),
        "the registered URI, the code and the client's state — nothing else"
    );
    assert_eq!(code.len(), 43, "the 43-character Pleroma code shape");

    let redeemed = oauth_token::consume_authorization(&pool, &code, site.app.id)
        .await
        .expect("the redemption reaches the database")
        .expect("the owning app can redeem the code");

    let created = user::get_by_nickname(&pool, "octocat")
        .await
        .expect("the lookup runs")
        .expect("the registration created the user");
    assert_eq!(redeemed.user_id, created.id, "the code names the new user");
    assert_eq!(created.email.as_deref(), Some("octocat@example.org"));
    assert_eq!(created.name, "The Octocat", "the submitted display name");
    assert_eq!(created.raw_bio, "lives in a repo", "and the bio");
    assert!(
        created.password_hash.is_some(),
        "the row carries a hash — of a random password no one knows"
    );
    assert!(
        !created.approval_pending.get(),
        "plain instance: nothing waits on it"
    );
    assert!(
        created.email_confirmed.get(),
        "and plain instances do not demand confirmation"
    );
    for guess in ["", "lives in a repo", "octocat", "hunter2"] {
        assert!(
            register::authenticate(&pool, "octocat", guess)
                .await
                .is_none(),
            "no guess logs the external account in by password"
        );
    }

    assert_eq!(
        count_states(&pool).await,
        0,
        "the finished state row is deleted"
    );
    let linked = user_identity::find_user(&pool, PROVIDER, UNLINKED_UID)
        .await
        .expect("the identity lookup runs")
        .expect("the provider account is linked");
    assert_eq!(linked.id, created.id, "to the freshly registered user");
    site.provider.stop();
}

/// The next login through the provider needs no form any more: the
/// callback finds the identity and goes straight to the code.
#[tokio::test]
async fn the_next_callback_goes_straight_to_the_code() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture(pool.clone(), "vic", "read").await;
    let state = pending_state(&site.http, &site.app, "").await;
    let response = post_form(
        &site.http,
        &[
            ("state", state.as_str()),
            ("nickname", "octocat"),
            ("email", "octocat@example.org"),
        ],
    )
    .await;
    assert_eq!(response.status, StatusCode::FOUND, "first login done");

    let again = prepare(&site.http, &site.app, "").await;
    let second = common::TestRequest::get(&format!(
        "/oauth/{PROVIDER}/callback?code={UNLINKED_UID}&state={again}"
    ))
    .send(&site.http)
    .await;
    assert_eq!(second.status, StatusCode::FOUND, "a direct login");
    let url = location(&second);
    assert!(
        !url.contains("registration_details"),
        "no form on the second trip: {url}"
    );
    assert!(query_param(&url, "code").is_some(), "a code instead: {url}");

    let created = user::get_by_nickname(&pool, "octocat")
        .await
        .expect("the lookup runs")
        .expect("the registration stands");
    let stored: oauth_token::OauthAuthorization =
        sqlx::query_as("SELECT * FROM oauth_authorizations WHERE token = $1")
            .bind(query_param(&url, "code").expect("the code"))
            .fetch_one(&pool)
            .await
            .expect("the second code is stored");
    assert_eq!(stored.user_id, created.id, "for the same user");
    site.provider.stop();
}

/// A nickname that is taken re-renders the form at 200 with the field
/// message, and changes nothing on the database side: no user, no link,
/// and the state is still live for another submission.
#[tokio::test]
async fn registration_reports_a_duplicate_nickname_on_the_form() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture(pool.clone(), "wen", "read").await;
    let taken = common::create_local_user(&pool, "octocat").await;
    assert_eq!(taken.nickname, "octocat", "the collision fixture exists");
    let state = pending_state(&site.http, &site.app, "").await;

    let response = post_form(
        &site.http,
        &[
            ("state", state.as_str()),
            ("nickname", "octocat"),
            ("email", "unique@example.org"),
        ],
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "the form comes back at 200"
    );
    assert_eq!(
        response.header("content-type"),
        Some("text/html; charset=utf-8"),
        "as a page"
    );
    assert_no_store(&response);
    let page = response.text();
    assert!(
        page.contains("nickname has already been taken"),
        "with the field message: {page}"
    );
    assert!(
        page.contains(&format!(
            "<input type=\"hidden\" name=\"state\" value=\"{state}\">"
        )),
        "and the same state to retry with"
    );
    assert!(
        page.contains("value=\"octocat\""),
        "keeping what the user typed"
    );
    assert_eq!(count_users(&pool).await, 2, "nothing new was inserted");
    assert_eq!(count_identities(&pool).await, 0, "nothing was linked");
    assert_eq!(count_codes(&pool).await, 0, "and nothing was minted");
    assert_eq!(count_states(&pool).await, 1, "the state is still there");
    site.provider.stop();
}

/// With `REGISTRATIONS_OPEN` off, external registrations are blocked the
/// same way the form reports every refusal: the form again with the line
/// "Registrations are closed", no user and no link.
#[tokio::test]
async fn closed_registrations_refuse_the_form_with_a_line() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture_with(pool.clone(), "xena", "read", |config| {
        config.registrations_open = Toggle::new(false);
    })
    .await;
    let state = pending_state(&site.http, &site.app, "").await;

    let response = post_form(
        &site.http,
        &[
            ("state", state.as_str()),
            ("nickname", "octocat"),
            ("email", "octocat@example.org"),
        ],
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "the form answers, not a redirect"
    );
    let page = response.text();
    assert!(
        page.contains("Registrations are closed"),
        "with the refusal line: {page}"
    );
    assert_eq!(count_users(&pool).await, 1, "only the fixture user exists");
    assert_eq!(count_identities(&pool).await, 0, "nothing was linked");
    site.provider.stop();
}

/// `REGISTRATIONS_APPROVAL_REQUIRED`: the account is created and the
/// identity linked, but the login waits for staff — an HTML page with the
/// approval message replaces the code.
#[tokio::test]
async fn approval_mode_answers_the_message_page() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture_with(pool.clone(), "yuri", "read", |config| {
        config.registrations_approval_required = Toggle::new(true);
    })
    .await;
    let state = pending_state(&site.http, &site.app, "").await;

    let response = post_form(
        &site.http,
        &[
            ("state", state.as_str()),
            ("nickname", "octocat"),
            ("email", "octocat@example.org"),
        ],
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "a page, not a redirect");
    assert_eq!(
        response.header("content-type"),
        Some("text/html; charset=utf-8"),
        "an HTML page"
    );
    assert_no_store(&response);
    let page = response.text();
    assert!(
        page.contains("once your account is approved"),
        "with the approval message: {page}"
    );
    assert!(!page.contains("action=\"/oauth/register\""), "no form left");
    assert!(
        !page.contains("check your email"),
        "approval is the wait that ends last"
    );

    let created = user::get_by_nickname(&pool, "octocat")
        .await
        .expect("the lookup runs")
        .expect("the registration created the user");
    assert!(
        created.approval_pending.get(),
        "the account awaits approval"
    );
    assert_eq!(
        count_codes(&pool).await,
        0,
        "an unapproved account gets no code"
    );
    let linked = user_identity::find_user(&pool, PROVIDER, UNLINKED_UID)
        .await
        .expect("the identity lookup runs")
        .expect("the identity is linked for the login after approval");
    assert_eq!(linked.id, created.id, "to the awaiting account");
    assert_eq!(count_states(&pool).await, 0, "and its state is spent");
    site.provider.stop();
}

/// `ACCOUNT_ACTIVATION_REQUIRED` with an email the provider does not
/// vouch for: the account lands unconfirmed and the confirmation message
/// page replaces the code.
#[tokio::test]
async fn activation_mode_answers_the_confirmation_page() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture_with(pool.clone(), "zoe", "read", |config| {
        config.account_activation_required = Toggle::new(true);
    })
    .await;
    let state = pending_state(&site.http, &site.app, "").await;

    let response = post_form(
        &site.http,
        &[
            ("state", state.as_str()),
            ("nickname", "octocat"),
            ("email", "different@example.org"),
        ],
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "a page, not a redirect");
    let page = response.text();
    assert!(
        page.contains("check your email"),
        "with the confirmation message: {page}"
    );

    let created = user::get_by_nickname(&pool, "octocat")
        .await
        .expect("the lookup runs")
        .expect("the registration created the user");
    assert!(
        !created.email_confirmed.get(),
        "the stranger email is not confirmed"
    );
    assert_eq!(count_codes(&pool).await, 0, "and no code skips the click");
    assert!(
        user_identity::find_user(&pool, PROVIDER, UNLINKED_UID)
            .await
            .expect("the identity lookup runs")
            .is_some(),
        "the identity is linked all the same: the provider account is proven"
    );
    assert_eq!(count_states(&pool).await, 0, "and its state is spent");
    site.provider.stop();
}

/// The provider's own email, submitted verbatim (in any case), is the
/// proof `ACCOUNT_ACTIVATION_REQUIRED` asks for: the account registers
/// confirmed and the login completes with a code.
#[tokio::test]
async fn the_provider_email_counts_as_confirmed() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture_with(pool.clone(), "adam", "read", |config| {
        config.account_activation_required = Toggle::new(true);
    })
    .await;
    let state = pending_state(&site.http, &site.app, "").await;

    let response = post_form(
        &site.http,
        &[
            ("state", state.as_str()),
            ("nickname", "octocat"),
            ("email", &format!("USER-{UNLINKED_UID}@EXAMPLE.ORG")),
        ],
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::FOUND,
        "the provider-verified email logs in: {}",
        response.text()
    );
    let code = query_param(&location(&response), "code").expect("a code");

    let created = user::get_by_nickname(&pool, "octocat")
        .await
        .expect("the lookup runs")
        .expect("the registration created the user");
    assert!(
        created.email_confirmed.get(),
        "the provider said this very address"
    );
    assert!(
        created.confirmation_token.is_none(),
        "so there is nothing to confirm by letter"
    );
    let stored: oauth_token::OauthAuthorization =
        sqlx::query_as("SELECT * FROM oauth_authorizations WHERE token = $1")
            .bind(&code)
            .fetch_one(&pool)
            .await
            .expect("the code is stored");
    assert_eq!(stored.user_id, created.id, "for the confirmed account");
    site.provider.stop();
}

/// An out-of-band client gets the OOB code page from registration, the
/// very answer the callback gives a linked out-of-band user.
#[tokio::test]
async fn registration_answers_the_out_of_band_code_page() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture(pool.clone(), "eve", "read").await;
    let app = oob_app(&pool, "read").await;
    let state = prepare_to(&site.http, &app, OOB, "").await;
    let handed = common::TestRequest::get(&format!(
        "/oauth/{PROVIDER}/callback?code={UNLINKED_UID}&state={state}"
    ))
    .send(&site.http)
    .await;
    assert_eq!(handed.status, StatusCode::FOUND, "the hand-off");

    let response = post_form(
        &site.http,
        &[
            ("state", state.as_str()),
            ("nickname", "octocat"),
            ("email", "octocat@example.org"),
        ],
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "a page, not a redirect");
    assert_eq!(
        response.header("content-type"),
        Some("text/html; charset=utf-8"),
        "the OOB page"
    );
    assert_no_store(&response);
    let code: String = sqlx::query_scalar("SELECT token FROM oauth_authorizations")
        .fetch_one(&pool)
        .await
        .expect("the code was minted for the out-of-band client");
    assert!(
        response.text().contains(code.as_str()),
        "and shows the code the client will read off it"
    );
    site.provider.stop();
}
