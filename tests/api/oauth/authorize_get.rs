//! `GET /oauth/authorize` (Pleroma's `OAuthController.authorize`): the
//! consent page's render and its escaping of every dynamic value, and the
//! 400 "Invalid client", "Invalid `redirect_uri`", "Invalid scope" and
//! `unsupported_response_type` refusals. The app registration, the consent
//! form body, the query readers, the oob code extractor, the cache
//! assertion and the code counter are `pub(super)` because the POST
//! submodule's approval tests flow through them.

use axum::http::StatusCode;
use sqlx::PgPool;
use wired_be::models::oauth_app::{self, NewOauthApp, OauthApp};

use super::support::{REDIRECT, error_text, fixtures};
use crate::common;

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

/// The consent form body logging in as `name`/`password` for `app`,
/// approving `scopes` and carrying `state` when given. Callers tweak
/// the entries (the redirect URI) or push more (`authorization[deny]`)
/// as the flow under test needs.
pub(super) fn consent_form<'a>(
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

/// The code shown on the out-of-band page, between its markers.
///
/// # Panics
///
/// If the body holds no `<code id="authorization-code">` element.
pub(super) fn oob_page_code(body: &str) -> &str {
    body.split_once("<code id=\"authorization-code\">")
        .expect("the oob page shows the code")
        .1
        .split_once("</code>")
        .expect("the code element is closed")
        .0
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

/// The full consent page a valid `GET` renders: the form posting to
/// `/oauth/authorize` with the login fields, one checked box per
/// requested scope, and the `client_id`/`redirect_uri`/`state`
/// round-trip fields — plus the no-store header.
#[tokio::test]
async fn get_authorize_renders_the_consent_form() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
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
#[tokio::test]
async fn get_authorize_defaults_the_scope_to_read() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
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
#[tokio::test]
async fn get_authorize_escapes_every_dynamic_value() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
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
#[tokio::test]
async fn get_authorize_refuses_unknown_clients() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
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
#[tokio::test]
async fn get_authorize_refuses_unregistered_redirect_uris() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
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
#[tokio::test]
async fn get_authorize_refuses_scopes_outside_the_registration() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
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
#[tokio::test]
async fn get_authorize_requires_the_code_response_type() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
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
