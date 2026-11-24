//! The `GET /oauth/prepare_request` endpoint and the provider redirect
//! it feeds: the stored round trip with its ten minutes, the default
//! scope and absent client `state`, the 404 an unconfigured provider
//! earns on every path, the application-parameter refusals shared with
//! `GET /oauth/authorize`, the five parameters of the provider redirect,
//! and the `Invalid state` refusals on both legs.

use axum::http::StatusCode;
use serde_json::{Value, json};

use super::provider::{
    CALLBACK_URI, LINKED_UID, OTHER, PROVIDER, PROVIDER_CLIENT_ID, PROVIDER_SCOPES, REDIRECT,
};
use super::support::{
    assert_no_store, count_identities, count_states, expire, fixture, location, prepare,
    query_pairs, query_param, seconds_left, stored_params,
};
use crate::common;

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
        assert_eq!(response.error_text(), "Unknown provider", "for `{uri}`");
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
        assert_eq!(response.error_text(), want, "for `{query}`");
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
            response.error_text(),
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
