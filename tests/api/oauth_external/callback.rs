//! The callback's successes: the linked user logged in with a
//! single-use code on the app's redirect, the echo of exactly what the
//! client sent, the out-of-band code, and the unlinked account
//! handed to the frontend's registration screen under `pending`.

use axum::http::StatusCode;
use serde_json::json;
use wired_be::models::oauth_token;

use super::provider::{LINKED_UID, OOB, PROVIDER, REDIRECT, UNLINKED_UID};
use super::support::{
    assert_no_store, count_codes, count_states, fixture, link, location, oob_app, prepare,
    prepare_to, query_pairs, query_param, stored_params,
};
use crate::common;

/// A callback whose provider account is linked logs that user in: the answer
/// is the app's redirect with a code, minted for the *original* application
/// and scopes, redeemable by the owning application exactly once.
///
/// The redemption itself is the `authorization_code` grant, walked end to
/// end over HTTP in `e2e_accounts.rs`; here the code's single-use
/// semantics are proven on
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

    let redeemed = oauth_token::consume_authorization(&pool, &code, site.app.id, Some(REDIRECT))
        .await
        .expect("the redemption reaches the database")
        .expect("the owning app can redeem the code");
    assert_eq!(redeemed.user_id, site.user.id, "and it names the login");
    assert!(
        oauth_token::consume_authorization(&pool, &code, site.app.id, Some(REDIRECT))
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

/// An app registered with the out-of-band redirect URI gets the code as
/// JSON instead of a redirect, exactly as the frontend's approval answers
/// it.
#[tokio::test]
async fn the_callback_answers_the_out_of_band_code() {
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
        "an answer, not a redirect: {}",
        response.text()
    );
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "and it is JSON"
    );
    assert_no_store(&response);
    assert!(response.header("location").is_none(), "no redirect target");

    let code: String = sqlx::query_scalar("SELECT token FROM oauth_authorizations")
        .fetch_one(&pool)
        .await
        .expect("the code was minted for the out-of-band client");
    assert_eq!(
        response.json_value(),
        json!({ "code": code }),
        "the body carries the code the client will redeem"
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
/// browser is sent to the frontend's registration screen. No code is minted, and a repeat
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
        format!("https://fe.example/oauth/external/register?state={state}"),
        "the frontend's registration screen, with the state that earned it"
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

/// A state is a one-time token: a linked callback burns it, so a second
/// callback on the same state — even with a fresh provider code — is
/// `Invalid state` and mints no second authorization code.
#[tokio::test]
async fn a_linked_callback_burns_its_state() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture(pool.clone(), "nora", "read").await;
    link(&pool, &site.user, LINKED_UID).await;
    let state = prepare(&site.http, &site.app, "").await;
    let callback = format!("/oauth/{PROVIDER}/callback?code={LINKED_UID}&state={state}");

    let first = common::TestRequest::get(&callback).send(&site.http).await;
    assert_eq!(
        first.status,
        StatusCode::FOUND,
        "the first callback logs in"
    );
    assert_eq!(count_codes(&pool).await, 1, "one code was minted");
    assert_eq!(count_states(&pool).await, 0, "the state is gone");

    let replay = common::TestRequest::get(&callback).send(&site.http).await;
    assert_eq!(replay.status, StatusCode::BAD_REQUEST, "the replay");
    assert_eq!(replay.error_text(), "Invalid state", "is an invalid state");
    let redirect = common::TestRequest::get(&format!("/oauth/{PROVIDER}?state={state}"))
        .send(&site.http)
        .await;
    assert_eq!(
        redirect.status,
        StatusCode::BAD_REQUEST,
        "the provider leg cannot reuse it either"
    );
    assert_eq!(count_codes(&pool).await, 1, "no second code was minted");
    site.provider.stop();
}

/// A denied callback burns the state as well: the same state cannot be
/// replayed afterwards, not even with a good provider code.
#[tokio::test]
async fn a_denied_callback_burns_its_state() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture(pool.clone(), "owen", "read").await;
    link(&pool, &site.user, LINKED_UID).await;
    let state = prepare(&site.http, &site.app, "").await;

    let denied = common::TestRequest::get(&format!(
        "/oauth/{PROVIDER}/callback?error=access_denied&state={state}"
    ))
    .send(&site.http)
    .await;
    assert_eq!(denied.status, StatusCode::FOUND, "the refusal redirects");
    assert_eq!(count_states(&pool).await, 0, "the state is gone");

    let replay = common::TestRequest::get(&format!(
        "/oauth/{PROVIDER}/callback?code={LINKED_UID}&state={state}"
    ))
    .send(&site.http)
    .await;
    assert_eq!(replay.status, StatusCode::BAD_REQUEST, "the replay");
    assert_eq!(replay.error_text(), "Invalid state", "is an invalid state");
    assert_eq!(count_codes(&pool).await, 0, "no code was minted");
    site.provider.stop();
}

/// An unlinked callback leaves the state for the registration screen, whose
/// details endpoint still reads it afterwards.
#[tokio::test]
async fn an_unlinked_callback_keeps_its_state_for_registration() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture(pool.clone(), "piper", "read").await;
    let state = prepare(&site.http, &site.app, "").await;
    let handoff = common::TestRequest::get(&format!(
        "/oauth/{PROVIDER}/callback?code={UNLINKED_UID}&state={state}"
    ))
    .send(&site.http)
    .await;
    assert_eq!(handoff.status, StatusCode::FOUND, "the hand-off redirects");
    assert_eq!(count_states(&pool).await, 1, "the state is kept");

    let form = common::TestRequest::get(&format!("/oauth/registration_details?state={state}"))
        .send(&site.http)
        .await;
    assert_eq!(form.status, StatusCode::OK, "the details still read it");
    site.provider.stop();
}
