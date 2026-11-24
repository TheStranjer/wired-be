//! The cap on wrong codes per challenge (WBE-668): five wrong second
//! factors burn the challenge, and the `authentication` bucket throttles
//! the challenge route.

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

/// Five wrong codes burn the challenge: the right code
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

/// With `authentication` limited to two requests, the third challenge
/// request is refused 429 `Throttled`.
#[tokio::test]
async fn the_challenge_route_is_throttled() {
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
}
