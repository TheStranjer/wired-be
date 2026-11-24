//! The callback of a linked account for an untrusted application: no code,
//! an `access_denied` redirect saying consent is required, and a state that
//! cannot be replayed afterwards.

use axum::http::StatusCode;

use super::provider::{LINKED_UID, PROVIDER, REDIRECT};
use super::support::{
    assert_no_store, count_codes, count_states, error_text, fixture, link, location, prepare,
    set_trusted,
};
use crate::common;

/// A linked account on an untrusted app is refused: the app's redirect gets
/// `error=access_denied&error_description=consent_required` and the client's
/// state, no authorization row is created and the state row is gone.
#[tokio::test]
async fn an_untrusted_app_is_refused_a_code_for_a_linked_account() {
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

    assert_eq!(response.status, StatusCode::FOUND, "the app is redirected");
    assert_no_store(&response);
    assert_eq!(
        location(&response),
        format!(
            "{REDIRECT}?error=access_denied&error_description=consent_required&state=client-state"
        ),
        "the refusal names consent, with the client's state and no code"
    );
    assert_eq!(count_codes(&pool).await, 0, "no authorization was created");
    assert_eq!(count_states(&pool).await, 0, "the state row was burned");
    site.provider.stop();
}

/// The refused state cannot be reused: a second callback on it is the 400
/// `Invalid state`, and still no code exists.
#[tokio::test]
async fn a_refused_state_cannot_be_reused() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture(pool.clone(), "owen", "read").await;
    let app = set_trusted(&pool, site.app, false).await;
    link(&pool, &site.user, LINKED_UID).await;
    let state = prepare(&site.http, &app, "").await;
    let path = format!("/oauth/{PROVIDER}/callback?code={LINKED_UID}&state={state}");

    let first = common::TestRequest::get(&path).send(&site.http).await;
    assert_eq!(
        first.status,
        StatusCode::FOUND,
        "the first callback bounces"
    );
    let second = common::TestRequest::get(&path).send(&site.http).await;
    assert_eq!(
        second.status,
        StatusCode::BAD_REQUEST,
        "the burned state is no state"
    );
    assert_eq!(error_text(&second), "Invalid state", "the usual refusal");
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
