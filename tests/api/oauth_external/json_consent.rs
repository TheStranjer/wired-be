//! The consent endpoints of a linked login for an untrusted application:
//! `GET /oauth/external/consent` describes the request to a holder of the
//! `confirm_token`, `POST` takes the verdict — a redeemable code, or
//! `access_denied` — and spends the state.

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use wired_be::models::{oauth_app::OauthApp, oauth_token, user::User};

use super::provider::{LINKED_UID, MockProvider, PROVIDER, REDIRECT};
use super::support::{
    assert_no_store, count_codes, count_states, error_text, fixture, link, location, prepare,
    query_param, set_trusted,
};
use crate::common;

/// A linked login parked for consent: the fixtures, the untrusted app, and
/// the state and token the callback redirected with.
struct Parked {
    /// The mock provider, to stop at the end of the test.
    provider: MockProvider,
    /// The router under test.
    http: Router,
    /// The untrusted application the login was prepared for.
    app: OauthApp,
    /// The linked local user.
    user: User,
    /// The state the callback redirected with.
    state: String,
    /// The `confirm_token` the callback redirected with.
    token: String,
}

/// Runs prepare and callback for a linked user and an untrusted app, the
/// way a browser would, and returns what the consent screen would get.
async fn park(pool: &sqlx::PgPool, nickname: &str) -> Parked {
    let site = fixture(pool.clone(), nickname, "read write").await;
    let app = set_trusted(pool, site.app, false).await;
    link(pool, &site.user, LINKED_UID).await;
    let state = prepare(&site.http, &app, "&scope=read&state=client-state").await;
    let response = common::TestRequest::get(&format!(
        "/oauth/{PROVIDER}/callback?code={LINKED_UID}&state={state}"
    ))
    .send(&site.http)
    .await;
    assert_eq!(response.status, StatusCode::FOUND, "the callback redirects");
    let token = query_param(&location(&response), "confirm_token").expect("a confirm_token");
    Parked {
        provider: site.provider,
        http: site.http,
        app,
        user: site.user,
        state,
        token,
    }
}

/// `GET /oauth/external/consent` with the given query.
async fn consent_get(http: &Router, query: &str) -> common::TestResponse {
    common::TestRequest::get(&format!("/oauth/external/consent{query}"))
        .send(http)
        .await
}

/// `POST /oauth/external/consent` with `body` as JSON.
async fn consent_post(http: &Router, body: &Value) -> common::TestResponse {
    common::TestRequest::post("/oauth/external/consent")
        .json(body)
        .send(http)
        .await
}

/// The right state and token describe the request: the application's name
/// and website and the prepared scopes — without spending anything.
#[tokio::test]
async fn consent_get_describes_the_request_for_the_token_holder() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let parked = park(&pool, "iris").await;

    let response = consent_get(
        &parked.http,
        &format!("?state={}&confirm_token={}", parked.state, parked.token),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.text());
    assert_no_store(&response);
    assert_eq!(
        response.json_value(),
        json!({
            "app": {"name": parked.app.client_name, "website": parked.app.website},
            "scopes": ["read"],
        }),
        "the application and the scopes the round trip was prepared with"
    );
    assert_eq!(count_states(&pool).await, 1, "a look spends nothing");
    assert_eq!(count_codes(&pool).await, 0, "and mints nothing");
    parked.provider.stop();
}

/// A wrong, empty or absent token — or a state that is not parked for
/// consent — is the 400 `Invalid state`, and the state is not burned.
#[tokio::test]
async fn consent_get_refuses_a_wrong_token() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let parked = park(&pool, "juno").await;
    let unparked = prepare(&parked.http, &parked.app, "").await;
    let state = &parked.state;

    for (query, why) in [
        (format!("?state={state}&confirm_token=wrong"), "wrong token"),
        (format!("?state={state}&confirm_token="), "empty token"),
        (format!("?state={state}"), "absent token"),
        (
            format!("?state={unparked}&confirm_token={}", parked.token),
            "a state with no confirmation",
        ),
        (
            format!("?state=never-issued&confirm_token={}", parked.token),
            "unknown state",
        ),
        (format!("?confirm_token={}", parked.token), "absent state"),
    ] {
        let response = consent_get(&parked.http, &query).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST, "{why}");
        assert_eq!(error_text(&response), "Invalid state", "for {why}");
    }
    let still = consent_get(
        &parked.http,
        &format!("?state={state}&confirm_token={}", parked.token),
    )
    .await;
    assert_eq!(still.status, StatusCode::OK, "the real token still works");
    parked.provider.stop();
}

/// Approving mints a code for the linked user on the redirect URI with the
/// client's state; the application redeems it once, and the state is gone.
#[tokio::test]
async fn consent_post_approve_mints_a_redeemable_code() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let parked = park(&pool, "kai").await;

    let response = consent_post(
        &parked.http,
        &json!({"state": parked.state, "confirm_token": parked.token, "approve": true}),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.text());
    assert_no_store(&response);
    let url = response.json_value()["redirect_uri"]
        .as_str()
        .expect("a redirect_uri")
        .to_owned();
    let code = query_param(&url, "code").expect("a code on the redirect");
    assert_eq!(
        url,
        format!("{REDIRECT}?code={code}&state=client-state"),
        "the registered URI, the code and the client's state"
    );
    let redeemed = oauth_token::consume_authorization(&pool, &code, parked.app.id, Some(REDIRECT))
        .await
        .expect("the redemption reaches the database")
        .expect("the owning app can redeem the code");
    assert_eq!(redeemed.user_id, parked.user.id, "for the linked user");
    assert_eq!(count_states(&pool).await, 0, "the state is spent");
    parked.provider.stop();
}

/// Denying redirects with `error=access_denied` and the client's state; no
/// code is minted and the state is spent.
#[tokio::test]
async fn consent_post_deny_answers_access_denied() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let parked = park(&pool, "lex").await;

    let response = consent_post(
        &parked.http,
        &json!({"state": parked.state, "confirm_token": parked.token, "approve": false}),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.text());
    assert_eq!(
        response.json_value(),
        json!({"redirect_uri": format!("{REDIRECT}?error=access_denied&state=client-state")}),
        "the denial, with the client's state and no code"
    );
    assert_eq!(count_codes(&pool).await, 0, "no code was minted");
    assert_eq!(count_states(&pool).await, 0, "the state is spent");
    parked.provider.stop();
}

/// A verdict is given once: the second post of the same state is the 400
/// `Invalid state`, and only the first minted a code.
#[tokio::test]
async fn consent_post_cannot_be_reused() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let parked = park(&pool, "mia").await;
    let body = json!({"state": parked.state, "confirm_token": parked.token, "approve": true});

    let first = consent_post(&parked.http, &body).await;
    assert_eq!(first.status, StatusCode::OK, "the first verdict stands");
    let second = consent_post(&parked.http, &body).await;
    assert_eq!(
        second.status,
        StatusCode::BAD_REQUEST,
        "the replay is refused"
    );
    assert_eq!(error_text(&second), "Invalid state", "the usual refusal");
    assert_eq!(count_codes(&pool).await, 1, "one code, from the first");
    parked.provider.stop();
}

/// A wrong token, a missing verdict and a bare body mint nothing and burn
/// nothing: the real holder can still decide afterwards.
#[tokio::test]
async fn consent_post_refusals_leave_the_state_alone() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let parked = park(&pool, "ned").await;

    let wrong = consent_post(
        &parked.http,
        &json!({"state": parked.state, "confirm_token": "wrong", "approve": true}),
    )
    .await;
    assert_eq!(wrong.status, StatusCode::BAD_REQUEST, "wrong token");
    assert_eq!(error_text(&wrong), "Invalid state", "the usual refusal");

    let undecided = consent_post(
        &parked.http,
        &json!({"state": parked.state, "confirm_token": parked.token}),
    )
    .await;
    assert_eq!(undecided.status, StatusCode::BAD_REQUEST, "no verdict");
    assert_eq!(
        error_text(&undecided),
        "Missing parameter: approve",
        "the missing field is named"
    );

    let bare = consent_post(&parked.http, &json!({})).await;
    assert_eq!(bare.status, StatusCode::BAD_REQUEST, "an empty body");
    assert_eq!(count_codes(&pool).await, 0, "nothing was minted");
    assert_eq!(count_states(&pool).await, 1, "and the state survived");

    let decided = consent_post(
        &parked.http,
        &json!({"state": parked.state, "confirm_token": parked.token, "approve": true}),
    )
    .await;
    assert_eq!(decided.status, StatusCode::OK, "the holder still decides");
    parked.provider.stop();
}
