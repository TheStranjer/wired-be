//! Scenario 3: the MFA lifecycle — TOTP setup and confirmation, the backup
//! codes, the `mfa_required` challenge the password grant answers with,
//! the challenge pass that hands out the withheld pair, and the disable
//! that opens the plain grant again. The `mfa_*` readers and the challenge
//! posts are this scenario's own steps.

use axum::http::StatusCode;
use serde_json::{Value, json};

use super::support::{
    Account, CHALLENGE, INVALID_CODE, MFA, PASSWORD, VERIFY, World, assert_refused, current_code,
    string_field,
};
use crate::common::{self, TestResponse};

// ──────────────────────────  Scenario 3: MFA  ──────────────────────────

/// The MFA lifecycle end to end: nothing is enabled, setup mints a secret,
/// the authenticator's code confirms it, five backup codes are issued, the
/// password grant then answers the `mfa_required` challenge instead of a
/// token, the challenge with a computed code hands out the pair the grant
/// withheld — a recovery code does the same and spends itself — and
/// removing TOTP opens the plain grant again.
#[tokio::test]
async fn mfa_gates_the_password_grant_until_it_is_disabled() {
    let db = common::test_db().await;
    let world = World::open(db.pool().clone()).await;
    let carol = world.register("carol").await;

    // Step 1: a fresh account has no second factor.
    let before = mfa_settings(&world, &carol).await;
    assert_eq!(
        before["settings"],
        json!({"enabled": false, "totp": false}),
        "MFA starts off and unconfirmed"
    );

    // Step 2: setup mints the secret, and the code the secret gives now
    // confirms it.
    let secret = mfa_setup(&world, &carol).await;
    let confirmed = common::TestRequest::post(&format!("{MFA}/confirm/totp"))
        .bearer(&carol.token)
        .form(&[("password", PASSWORD), ("code", &current_code(&secret))])
        .send(&world.app)
        .await;
    common::assert_answer(&confirmed, StatusCode::OK, "the TOTP confirmation");
    let after = mfa_settings(&world, &carol).await;
    assert_eq!(
        after["settings"],
        json!({"enabled": true, "totp": true}),
        "the method is enabled and confirmed"
    );

    // Step 3: recovery codes, five of them, new to this issuance.
    let codes = backup_codes(&world, &carol).await;
    assert_eq!(codes.len(), 5, "Pleroma issues five backup codes");

    // Step 4: the password grant now answers the challenge, not a token.
    let challenge = mfa_challenge(&world, "carol").await;

    // Step 5: the second factor answers the pair the grant withheld.
    let token = pass_challenge(&world, &challenge, "totp", &current_code(&secret)).await;
    let read = common::api_get(&world.app, Some(&token), VERIFY).await;
    common::assert_answer(&read, StatusCode::OK, "the challenged token");
    assert_eq!(
        string_field(&read.json_value(), "username"),
        "carol",
        "acting as its owner"
    );

    // Step 6: a recovery code carries a challenge too, and is spent by it.
    let second = mfa_challenge(&world, "carol").await;
    let recovered = pass_challenge(&world, &second, "recovery", &codes[0]).await;
    let again = common::api_get(&world.app, Some(&recovered), VERIFY).await;
    common::assert_answer(&again, StatusCode::OK, "the backup-code token");
    let reused = pass_challenge_attempt(&world, &second, "recovery", &codes[0]).await;
    assert_refused(
        &reused,
        StatusCode::BAD_REQUEST,
        INVALID_CODE,
        "a spent backup code",
    );

    // Step 7: removing TOTP closes the challenge and opens the grant.
    let disabled = common::api_delete(
        &world.app,
        Some(&carol.token),
        &format!("{MFA}/totp"),
        &json!({"password": PASSWORD}),
    )
    .await;
    common::assert_answer(&disabled, StatusCode::OK, "the TOTP removal");
    let quiet = mfa_settings(&world, &carol).await;
    assert_eq!(
        quiet["settings"]["enabled"], false,
        "the switch is back off, and no method is confirmed"
    );
    let open = world.login("carol").await;
    let unguarded = common::api_get(&world.app, Some(&open), VERIFY).await;
    common::assert_answer(&unguarded, StatusCode::OK, "the unguarded login");
    assert_eq!(
        string_field(&unguarded.json_value(), "username"),
        "carol",
        "the same account, no challenge"
    );
}

/// `GET /api/pleroma/accounts/mfa` as `account`, answered as its settings.
///
/// # Panics
///
/// If the read is refused.
async fn mfa_settings(world: &World, account: &Account) -> Value {
    let response = common::api_get(&world.app, Some(&account.token), MFA).await;
    common::assert_answer(&response, StatusCode::OK, "the MFA settings read");
    response.json_value()
}

/// `GET …/setup/totp` as `account`, answered as the minted secret key.
///
/// # Panics
///
/// If setup is refused or answers no key.
async fn mfa_setup(world: &World, account: &Account) -> String {
    let response = common::api_get(
        &world.app,
        Some(&account.token),
        &format!("{MFA}/setup/totp"),
    )
    .await;
    common::assert_answer(&response, StatusCode::OK, "the TOTP setup");
    string_field(&response.json_value(), "key")
}

/// `GET …/backup_codes` as `account`, answered as the issued codes.
///
/// # Panics
///
/// If the read is refused or answers no list of strings.
async fn backup_codes(world: &World, account: &Account) -> Vec<String> {
    let response = common::api_get(
        &world.app,
        Some(&account.token),
        &format!("{MFA}/backup_codes"),
    )
    .await;
    common::assert_answer(&response, StatusCode::OK, "the backup-code issue");
    serde_json::from_value(response.json_value()["backup_codes"].clone())
        .expect("backup_codes is a list of strings")
}

/// The `password` grant for `username` while MFA is on: the 403 challenge,
/// answered as its `mfa_token`.
///
/// # Panics
///
/// If the grant answers anything other than the challenge.
async fn mfa_challenge(world: &World, username: &str) -> String {
    let refused = common::login_password(&world.app, &world.client, username, PASSWORD).await;
    assert_refused(
        &refused,
        StatusCode::FORBIDDEN,
        "mfa_required",
        "the password grant of an MFA account",
    );
    let body = refused.json_value();
    assert_eq!(
        body["supported_challenge_types"], "totp",
        "totp is the only method this server speaks: {body}"
    );
    assert!(
        body.get("access_token").is_none(),
        "and no token rides the challenge: {body}"
    );
    string_field(&body, "mfa_token")
}

/// `POST /oauth/mfa/challenge` with `code` of type `kind`, asserted 200 and
/// answered as the token pair.
///
/// # Panics
///
/// If the challenge is refused.
async fn pass_challenge(world: &World, challenge: &str, kind: &str, code: &str) -> String {
    let response = pass_challenge_attempt(world, challenge, kind, code).await;
    common::assert_answer(&response, StatusCode::OK, "the MFA challenge");
    common::access_token_of(&response)
}

/// `POST /oauth/mfa/challenge`, answered raw: the same call, no assertion.
async fn pass_challenge_attempt(
    world: &World,
    challenge: &str,
    kind: &str,
    code: &str,
) -> TestResponse {
    common::TestRequest::post(CHALLENGE)
        .form(&[
            ("mfa_token", challenge),
            ("challenge_type", kind),
            ("code", code),
            ("client_id", world.client.client_id.as_str()),
            ("client_secret", world.client.client_secret.as_str()),
        ])
        .send(&world.app)
        .await
}
