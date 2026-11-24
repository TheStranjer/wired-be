//! The callback of a linked account for an untrusted application: no code
//! yet, a `confirm_token` stored beside the linked user in the state, and
//! the browser sent to the consent screen carrying both.

use axum::http::StatusCode;
use serde_json::json;

use super::provider::{LINKED_UID, PROVIDER, REDIRECT};
use super::support::{
    assert_no_store, count_codes, count_states, error_text, fixture, link, location, prepare,
    query_param, set_trusted, stored_params,
};
use crate::common;

/// A linked account on an untrusted app is not granted a code: the browser
/// is redirected to the consent screen with the state and a fresh
/// `confirm_token`, no authorization row is created, and the state row now
/// carries the token and the linked user.
#[tokio::test]
async fn an_untrusted_app_is_sent_to_consent_for_a_linked_account() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture(pool.clone(), "nora", "read write").await;
    let app = set_trusted(&pool, site.app, false).await;
    link(&pool, &site.user, LINKED_UID).await;
    let state = prepare(&site.http, &app, "&scope=read+write&state=client-state").await;
    assert_eq!(count_states(&pool).await, 1, "the round trip is prepared");

    let response = common::TestRequest::get(&format!(
        "/oauth/{PROVIDER}/callback?code={LINKED_UID}&state={state}"
    ))
    .send(&site.http)
    .await;

    assert_eq!(response.status, StatusCode::FOUND, "the browser moves on");
    assert_no_store(&response);
    let url = location(&response);
    assert!(
        url.starts_with("https://fe.example/oauth/external/consent?"),
        "to the frontend's consent screen: {url}"
    );
    assert!(
        !url.starts_with(REDIRECT),
        "not to the application's redirect URI: {url}"
    );
    assert_eq!(
        query_param(&url, "state").as_deref(),
        Some(state.as_str()),
        "the state rides along"
    );
    let token = query_param(&url, "confirm_token").expect("a confirm_token on the redirect");
    assert_eq!(token.len(), 43, "32 random bytes, base64url");
    assert_eq!(
        url,
        format!("https://fe.example/oauth/external/consent?state={state}&confirm_token={token}"),
        "the state and the token, nothing else"
    );

    assert_eq!(count_codes(&pool).await, 0, "no authorization was created");
    assert_eq!(count_states(&pool).await, 1, "the state waits for consent");
    let (_, params) = stored_params(&pool, &state).await;
    assert_eq!(
        params["confirmation"],
        json!({"token": token, "user_id": site.user.id}),
        "the token and the linked user are parked in the state"
    );
    site.provider.stop();
}

/// A second callback on the same state parks a fresh token: the first one
/// stops opening the consent screen, and still no code exists.
#[tokio::test]
async fn a_second_callback_rotates_the_confirm_token() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture(pool.clone(), "owen", "read").await;
    let app = set_trusted(&pool, site.app, false).await;
    link(&pool, &site.user, LINKED_UID).await;
    let state = prepare(&site.http, &app, "").await;
    let path = format!("/oauth/{PROVIDER}/callback?code={LINKED_UID}&state={state}");

    let first = common::TestRequest::get(&path).send(&site.http).await;
    let second = common::TestRequest::get(&path).send(&site.http).await;
    assert_eq!(second.status, StatusCode::FOUND, "the replay redirects too");
    let old = query_param(&location(&first), "confirm_token").expect("the first token");
    let new = query_param(&location(&second), "confirm_token").expect("the second token");
    assert_ne!(old, new, "each callback parks its own token");

    let old_reply = common::TestRequest::get(&format!(
        "/oauth/external/consent?state={state}&confirm_token={old}"
    ))
    .send(&site.http)
    .await;
    assert_eq!(
        old_reply.status,
        StatusCode::BAD_REQUEST,
        "the old token is dead"
    );
    assert_eq!(error_text(&old_reply), "Invalid state", "the usual refusal");
    assert_eq!(count_codes(&pool).await, 0, "and no code was ever minted");
    site.provider.stop();
}

/// A trusted app still gets its code, and its state is single-use too.
#[tokio::test]
async fn a_trusted_app_gets_a_code_once() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture(pool.clone(), "pia", "read").await;
    assert!(site.app.trusted, "the fixture app is trusted");
    link(&pool, &site.user, LINKED_UID).await;
    let state = prepare(&site.http, &site.app, "").await;
    let path = format!("/oauth/{PROVIDER}/callback?code={LINKED_UID}&state={state}");

    let first = common::TestRequest::get(&path).send(&site.http).await;
    assert_eq!(first.status, StatusCode::FOUND, "the app is redirected");
    assert!(
        location(&first).starts_with(&format!("{REDIRECT}?code=")),
        "with a code: {}",
        location(&first)
    );
    assert_eq!(count_codes(&pool).await, 1, "one authorization exists");
    let second = common::TestRequest::get(&path).send(&site.http).await;
    assert_eq!(
        second.status,
        StatusCode::BAD_REQUEST,
        "the state cannot be replayed"
    );
    assert_eq!(count_codes(&pool).await, 1, "and no second code appears");
    site.provider.stop();
}
