//! PKCE (RFC 7636, `S256` only) on `/oauth/authorize` and the
//! `authorization_code` grant: a code approved with a challenge redeems
//! only with the verifier that hashes to it, a refused exchange leaves the
//! code usable, `plain` and malformed challenges are 400s that mint
//! nothing, and a request without a challenge keeps the old behaviour.

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use wired_be::models::oauth_app::OauthApp;

use super::authorize_get::{authorization_count, query_param};
use super::json_authorize::{frontend_token, post_verdict, verdict};
use super::support::{REDIRECT, post_json};
use crate::common;

/// The RFC 7636 Appendix B verifier.
const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
/// The `S256` challenge of [`VERIFIER`].
const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";

/// The approval verdict for `app` with `extra`'s PKCE fields merged in.
fn pkce_verdict(app: &OauthApp, extra: &Value) -> Value {
    let mut body = verdict(app, REDIRECT, "read", true);
    if let (Some(target), Some(fields)) = (body.as_object_mut(), extra.as_object()) {
        for (key, value) in fields {
            let _previous = target.insert(key.clone(), value.clone());
        }
    }
    body
}

/// Approves with the `S256` [`CHALLENGE`] and returns the delivered code.
async fn approved_code(http: &Router, token: &str, app: &OauthApp) -> String {
    let body = pkce_verdict(
        app,
        &json!({ "code_challenge": CHALLENGE, "code_challenge_method": "S256" }),
    );
    let response = post_verdict(http, Some(token), &body).await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.text());
    let redirect = response.json_value()["redirect_uri"]
        .as_str()
        .expect("a redirect_uri")
        .to_owned();
    query_param(&redirect, "code").expect("a code")
}

/// The exchange body for `code`, presenting `verifier` when given.
fn exchange(app: &OauthApp, code: &str, verifier: Option<&str>) -> Value {
    let mut body = json!({
        "grant_type": "authorization_code",
        "code": code,
        "client_id": app.client_id,
        "client_secret": app.client_secret,
        "redirect_uri": REDIRECT,
    });
    if let Some(text) = verifier {
        body["code_verifier"] = json!(text);
    }
    body
}

/// The right verifier redeems a bound code; a wrong or missing one is the
/// shared 400 and leaves the code usable; the code is single-use.
#[tokio::test]
async fn a_bound_code_redeems_only_with_its_verifier() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (token, app) = frontend_token(&pool, "pia", "read", "read").await;
    let http = common::test_app(pool.clone());
    let code = approved_code(&http, &token, &app).await;

    let stored: (Option<String>, Option<String>) =
        sqlx::query_as("SELECT code_challenge, code_challenge_method FROM oauth_authorizations")
            .fetch_one(&pool)
            .await
            .expect("the stored authorization");
    assert_eq!(stored.0.as_deref(), Some(CHALLENGE), "challenge stored");
    assert_eq!(stored.1.as_deref(), Some("S256"), "method stored");

    let wrong = "b".repeat(43);
    for presented in [None, Some(wrong.as_str()), Some(CHALLENGE), Some("")] {
        let refused = post_json(&http, &exchange(&app, &code, presented)).await;
        assert_eq!(
            refused.status,
            StatusCode::BAD_REQUEST,
            "a missing or wrong verifier is refused"
        );
        assert_eq!(refused.error_text(), "Invalid credentials", "shared 400");
    }

    let redeemed = post_json(&http, &exchange(&app, &code, Some(VERIFIER))).await;
    assert_eq!(
        redeemed.status,
        StatusCode::OK,
        "the refusals left the code unused: {}",
        redeemed.text()
    );
    assert_eq!(redeemed.json_value()["scope"], "read", "the approved scope");

    let replay = post_json(&http, &exchange(&app, &code, Some(VERIFIER))).await;
    assert_eq!(replay.status, StatusCode::BAD_REQUEST, "single-use");
}

/// `plain`, unknown methods, a missing method, a method without a
/// challenge and malformed challenges are 400 `invalid_request`s on the
/// verdict, and nothing is minted.
#[tokio::test]
async fn bad_pkce_parameters_are_refused_and_mint_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (token, app) = frontend_token(&pool, "pat", "read", "read").await;
    let http = common::test_app(pool.clone());
    let short = "a".repeat(42);
    let long = "a".repeat(129);
    let cases = [
        json!({ "code_challenge": CHALLENGE, "code_challenge_method": "plain" }),
        json!({ "code_challenge": CHALLENGE, "code_challenge_method": "s256" }),
        json!({ "code_challenge": CHALLENGE }),
        json!({ "code_challenge_method": "S256" }),
        json!({ "code_challenge": short, "code_challenge_method": "S256" }),
        json!({ "code_challenge": long, "code_challenge_method": "S256" }),
        json!({ "code_challenge": format!("{}+", "a".repeat(42)), "code_challenge_method": "S256" }),
    ];
    for extra in cases {
        let response = post_verdict(&http, Some(&token), &pkce_verdict(&app, &extra)).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST, "{extra}");
        assert_eq!(response.json_value()["error"], "invalid_request", "{extra}");
    }
    assert_eq!(authorization_count(&pool).await, 0, "nothing was minted");
}

/// The `GET` half refuses the same parameters, as the browser redirect and
/// as the JSON description, and passes a valid pair.
#[tokio::test]
async fn the_authorize_get_validates_the_pkce_pair() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_token, app) = frontend_token(&pool, "pru", "read", "read").await;
    let http = common::test_app(pool);
    let base = format!(
        "/oauth/authorize?response_type=code&client_id={}&redirect_uri={REDIRECT}",
        app.client_id
    );

    for accept in ["text/html", "application/json"] {
        let plain = format!("{base}&code_challenge={CHALLENGE}&code_challenge_method=plain");
        let refused = common::TestRequest::get(&plain)
            .header("accept", accept)
            .send(&http)
            .await;
        assert_eq!(
            refused.status,
            StatusCode::BAD_REQUEST,
            "plain via {accept}"
        );

        let valid = format!("{base}&code_challenge={CHALLENGE}&code_challenge_method=S256");
        let accepted = common::TestRequest::get(&valid)
            .header("accept", accept)
            .send(&http)
            .await;
        assert!(
            accepted.status == StatusCode::OK || accepted.status == StatusCode::FOUND,
            "S256 via {accept}: {}",
            accepted.status
        );
    }
}

/// Without a challenge nothing changes: the code redeems with no verifier,
/// and a stray verifier is ignored.
#[tokio::test]
async fn a_code_without_a_challenge_keeps_the_old_behaviour() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (token, app) = frontend_token(&pool, "pam", "read", "read").await;
    let http = common::test_app(pool.clone());

    let first = unbound_code(&http, &token, &app).await;
    let second = unbound_code(&http, &token, &app).await;
    let bare = post_json(&http, &exchange(&app, &first, None)).await;
    assert_eq!(bare.status, StatusCode::OK, "no verifier: {}", bare.text());
    let stray = post_json(&http, &exchange(&app, &second, Some(VERIFIER))).await;
    assert_eq!(stray.status, StatusCode::OK, "stray: {}", stray.text());
}

/// Approves without any PKCE parameter and returns the delivered code.
async fn unbound_code(http: &Router, token: &str, app: &OauthApp) -> String {
    let response = post_verdict(http, Some(token), &verdict(app, REDIRECT, "read", true)).await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.text());
    let redirect = response.json_value()["redirect_uri"]
        .as_str()
        .expect("a redirect_uri")
        .to_owned();
    query_param(&redirect, "code").expect("a code")
}
