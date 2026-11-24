//! The cap on wrong codes per challenge (WBE-668): five wrong second
//! factors burn the challenge on both halves, and the `authentication`
//! bucket throttles the two code-checking routes.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use super::*;
use wired_be::models::mfa_token::MAX_MFA_ATTEMPTS;

/// How many wrong codes the tests send before the cap bites.
fn cap() -> usize {
    usize::try_from(MAX_MFA_ATTEMPTS).expect("the cap is positive")
}

/// Sends `count` wrong TOTP codes to `/oauth/mfa/challenge` for `token`,
/// asserting each is the one 400 refusal.
async fn wrong_challenges(http: &Router, app: &OauthApp, token: &str, secret: &str, count: usize) {
    let wrong = wrong_code(&current_code(secret));
    for _ in 0..count {
        let refused = post_challenge(http, &challenge_fields(token, "totp", &wrong, app)).await;
        assert_invalid_code(&refused, "a wrong code");
    }
}

/// Starts the browser flow for `nickname` and returns the challenge token
/// the detour redirect carries.
async fn browser_challenge(http: &Router, app: &OauthApp, nickname: &str) -> String {
    let detour = post_authorize(http, &authorize_form(app, nickname, "read", None)).await;
    let location = detour.header("location").expect("a Location header");
    query_param(location, "mfa_token").expect("the challenge token rides along")
}

/// Sends `count` wrong TOTP codes to `/oauth/mfa/verify` for `token`,
/// asserting each re-renders the form at 401.
async fn wrong_verifications(http: &Router, token: &str, secret: &str, count: usize) {
    let wrong = wrong_code(&current_code(secret));
    for _ in 0..count {
        let refused = post_verify(http, &verify_form(token, "totp", &wrong, REDIRECT, None)).await;
        assert_eq!(refused.status, StatusCode::UNAUTHORIZED, "a wrong code");
        assert!(
            refused.text().contains(TWO_FACTOR_FAILED),
            "the form comes back with the flash line"
        );
    }
}

/// Five wrong codes on the API half burn the challenge: the right code
/// with the same `mfa_token` is refused, and the row is gone.
#[tokio::test]
async fn five_wrong_codes_burn_the_api_challenge() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let fix = mfa_account(&pool, "ivan").await;
    let app = common::create_app(&pool, "read").await;
    let http = common::test_app(pool.clone());
    let token = mfa_grant(&http, &app, "ivan", "read").await;

    wrong_challenges(&http, &app, &token, &fix.secret, cap()).await;
    assert!(
        stored_challenge(&pool, &token).await.is_none(),
        "the fifth failure deletes the challenge"
    );

    let late = post_challenge(
        &http,
        &challenge_fields(&token, "totp", &current_code(&fix.secret), &app),
    )
    .await;
    assert_invalid_code(&late, "the right code after the cap");
    assert_eq!(
        token_count(&pool, fix.user.id).await,
        0,
        "and no token was issued"
    );
}

/// Four wrong codes leave the challenge usable: the right one still
/// answers the token JSON.
#[tokio::test]
async fn four_wrong_codes_leave_the_api_challenge_usable() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let fix = mfa_account(&pool, "judy").await;
    let app = common::create_app(&pool, "read").await;
    let http = common::test_app(pool.clone());
    let token = mfa_grant(&http, &app, "judy", "read").await;

    wrong_challenges(&http, &app, &token, &fix.secret, cap() - 1).await;
    assert!(
        stored_challenge(&pool, &token).await.is_some(),
        "under the cap the challenge stays"
    );

    let accepted = post_challenge(
        &http,
        &challenge_fields(&token, "totp", &current_code(&fix.secret), &app),
    )
    .await;
    assert_eq!(accepted.status, StatusCode::OK, "the right code passes");
    assert!(
        accepted.json_value()["access_token"].is_string(),
        "and answers the token JSON"
    );
}

/// Wrong recovery codes count against the cap like wrong TOTP codes.
#[tokio::test]
async fn wrong_recovery_codes_count_too() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let fix = mfa_account(&pool, "karl").await;
    let app = common::create_app(&pool, "read").await;
    let http = common::test_app(pool.clone());
    let token = mfa_grant(&http, &app, "karl", "read").await;

    for _ in 0..cap() {
        let refused = post_challenge(
            &http,
            &challenge_fields(&token, "recovery", "not-a-backup-code", &app),
        )
        .await;
        assert_invalid_code(&refused, "a wrong recovery code");
    }
    let late = post_challenge(
        &http,
        &challenge_fields(&token, "recovery", &fix.codes[0], &app),
    )
    .await;
    assert_invalid_code(&late, "a real recovery code after the cap");
}

/// Five wrong codes on the browser half burn the challenge: the form
/// re-renders every time, and the right code afterwards is refused too.
#[tokio::test]
async fn five_wrong_codes_burn_the_browser_challenge() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let fix = mfa_account(&pool, "laura").await;
    let app = common::create_app(&pool, "read").await;
    let http = common::test_app(pool.clone());
    let token = browser_challenge(&http, &app, "laura").await;

    wrong_verifications(&http, &token, &fix.secret, cap()).await;
    assert!(
        stored_challenge(&pool, &token).await.is_none(),
        "the fifth failure deletes the challenge"
    );

    let late = post_verify(
        &http,
        &verify_form(&token, "totp", &current_code(&fix.secret), REDIRECT, None),
    )
    .await;
    assert_eq!(
        late.status,
        StatusCode::UNAUTHORIZED,
        "the right code after the cap is refused"
    );
    assert!(
        late.text().contains(TWO_FACTOR_FAILED),
        "with the flash line"
    );
    assert!(
        late.header("location").is_none(),
        "and nothing is delivered"
    );
}

/// Four wrong codes on the browser half leave the challenge usable.
#[tokio::test]
async fn four_wrong_codes_leave_the_browser_challenge_usable() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let fix = mfa_account(&pool, "mallory").await;
    let app = common::create_app(&pool, "read").await;
    let http = common::test_app(pool.clone());
    let token = browser_challenge(&http, &app, "mallory").await;

    wrong_verifications(&http, &token, &fix.secret, cap() - 1).await;
    let finished = post_verify(
        &http,
        &verify_form(&token, "totp", &current_code(&fix.secret), REDIRECT, None),
    )
    .await;
    assert_eq!(
        finished.status,
        StatusCode::FOUND,
        "the right code still finishes the flow"
    );
    let location = finished.header("location").expect("a Location header");
    assert!(
        query_param(location, "code").is_some(),
        "and delivers the code"
    );
}

/// With `authentication` limited to two requests, the third challenge
/// request is refused 429 `Throttled`; the page that only renders the
/// form is outside the bucket.
#[tokio::test]
async fn the_code_checking_routes_are_throttled() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let fix = mfa_account(&pool, "nina").await;
    let app = common::create_app(&pool, "read").await;
    let http = common::test_app_with_config(
        pool.clone(),
        common::test_config_with_rate_limits("authentication=2:600"),
    );
    let client = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)), 40_007);
    let wrong = wrong_code(&current_code(&fix.secret));
    let fields = challenge_fields("no-such-challenge", "totp", &wrong, &app);

    for attempt in 0..2 {
        let refused = common::TestRequest::post("/oauth/mfa/challenge")
            .form(&fields)
            .peer(client)
            .send(&http)
            .await;
        assert_invalid_code(&refused, &format!("request {attempt}"));
    }
    let third = common::TestRequest::post("/oauth/mfa/challenge")
        .form(&fields)
        .peer(client)
        .send(&http)
        .await;
    common::assert_answer(&third, StatusCode::TOO_MANY_REQUESTS, "the third challenge");
    common::assert_error(&third, "Throttled", "the refusal body");

    let verify = common::TestRequest::post("/oauth/mfa/verify")
        .form(&verify_form(
            "no-such-challenge",
            "totp",
            &wrong,
            REDIRECT,
            None,
        ))
        .peer(client)
        .send(&http)
        .await;
    common::assert_answer(
        &verify,
        StatusCode::TOO_MANY_REQUESTS,
        "verify shares the bucket",
    );

    let page = common::TestRequest::get("/oauth/mfa?mfa_token=t")
        .peer(client)
        .send(&http)
        .await;
    common::assert_answer(&page, StatusCode::OK, "the form page is not throttled");
}
