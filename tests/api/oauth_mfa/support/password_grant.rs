//! The password grant's MFA challenge: the `mfa_required` answer, the TOTP and
//! recovery codes and the refusals.

use super::*;

/// The password grant of an MFA-enabled account answers the 403
/// `mfa_required` challenge and issues no token: the grant's scopes are
/// parked on a pending authorization the five-minute challenge links
/// (Pleroma's `handle_token_exchange_error({:mfa_required, …})`).
#[tokio::test]
async fn the_password_grant_of_an_mfa_user_answers_mfa_required() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let fix = mfa_account(&pool, "alice").await;
    let app = common::create_app(&pool, "read write").await;
    let http = common::test_app(pool.clone());

    let token = mfa_grant(&http, &app, "alice", "read write").await;
    let (_challenge_id, authorization_id) = assert_challenge_stored(&pool, &token, &fix.user).await;
    let parked = authorization_by_id(&pool, authorization_id).await;
    assert_eq!(parked.user_id, fix.user.id, "the grant is the user's");
    assert_eq!(parked.app_id, app.id, "for the validated app");
    assert_eq!(parked.scopes, "read write", "carrying the requested scopes");
    assert!(!parked.used, "and waiting unused for the second factor");

    assert_eq!(
        token_count(&pool, fix.user.id).await,
        0,
        "no token is issued before the second factor"
    );

    // A second grant mints a second challenge; a wrong password keeps
    // answering the grant's one credential refusal, and a deactivated
    // MFA account keeps the disabled answer that precedes the gate.
    let second = mfa_grant(&http, &app, "alice", "read").await;
    assert_ne!(token, second, "one fresh token per refused login");
    assert_eq!(challenge_count(&pool).await, 2, "one row per refused login");

    let mut bad = grant_body(&app, "alice", "read");
    bad["password"] = json!("not-her-password");
    let denied = post_json(&http, &bad).await;
    assert_eq!(denied.status, StatusCode::BAD_REQUEST, "wrong password");
    assert_eq!(denied.error_text(), "Invalid credentials", "stays the 400");

    let suspended =
        common::create_deactivated_user_with_password(&pool, "suspended", PASSWORD).await;
    let mut settings = mfa::MfaSettings::default();
    settings.setup_totp(&totp::generate_secret());
    settings.confirm_totp();
    mfa::save(&pool, suspended.id, &settings)
        .await
        .expect("MFA settings save for fixtures");
    let disabled = post_json(&http, &grant_body(&app, "suspended", "read")).await;
    assert_eq!(
        disabled.status,
        StatusCode::FORBIDDEN,
        "the deactivated answer precedes the gate"
    );
    assert_eq!(
        disabled.json_value(),
        json!({
            "error": "Your account is currently disabled",
            "identifier": "account_is_disabled",
        }),
        "the account-status 403 stays its own"
    );
}

/// A challenge with the authenticator's current code answers the very
/// token JSON the grant withheld and retires everything it spent; a
/// wrong code answers 400 "Invalid code" and burns nothing, and the
/// spent challenge never works twice.
#[tokio::test]
async fn a_totp_challenge_mints_the_token_pair() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let fix = mfa_account(&pool, "bob").await;
    let app = common::create_app(&pool, "read write").await;
    let http = common::test_app(pool.clone());
    let token = mfa_grant(&http, &app, "bob", "read write").await;

    let real = current_code(&fix.secret);
    let rejected = post_challenge(
        &http,
        &challenge_fields(&token, "totp", &wrong_code(&real), &app),
    )
    .await;
    assert_invalid_code(&rejected, "a wrong code");
    assert!(
        stored_challenge(&pool, &token).await.is_some(),
        "a wrong code does not retire the challenge"
    );
    assert_eq!(
        token_count(&pool, fix.user.id).await,
        0,
        "and issues nothing"
    );

    let accepted = post_challenge(&http, &challenge_fields(&token, "totp", &real, &app)).await;
    assert_eq!(accepted.status, StatusCode::OK, "the right code passes");
    assert_no_store(&accepted);
    let body = accepted.json_value();
    assert_eq!(body["token_type"], "Bearer", "bearer tokens");
    let access = body["access_token"]
        .as_str()
        .expect("access_token is a string");
    assert_eq!(access.len(), 43, "the 43-char Pleroma token shape");
    assert_eq!(body["scope"], "read write", "the parked grant's scopes");
    assert_eq!(body["me"], fix.user.ap_id, "the owner's actor id");
    assert_eq!(
        body.as_object().expect("an object").len(),
        8,
        "exactly the password grant's documented keys"
    );
    let ttl = i64::try_from(DEFAULT_OAUTH_TOKEN_TTL_SECS).expect("the default fits in i64");
    let expires_in = body["expires_in"].as_i64().expect("expires_in is seconds");
    assert!(
        (ttl - 5..=ttl).contains(&expires_in),
        "the configured lifetime, got {expires_in}"
    );

    let (stored, stored_app) = oauth_token::get_with_app(&pool, access)
        .await
        .expect("the lookup reaches the database")
        .expect("the token row was stored");
    assert_eq!(
        stored.user_id,
        Some(fix.user.id),
        "the token acts as the user"
    );
    assert_eq!(stored_app.id, app.id, "issued for the authenticated app");
    assert_eq!(stored.scopes, "read write", "with the parked scopes");

    assert!(
        stored_challenge(&pool, &token).await.is_none(),
        "success retires the challenge"
    );

    let replay = post_challenge(&http, &challenge_fields(&token, "totp", &real, &app)).await;
    assert_invalid_code(&replay, "a replayed challenge");
}

/// A recovery code passes the challenge — and works exactly once: its
/// hash comes off the account, so a spent code refuses even with a
/// fresh challenge.
#[tokio::test]
async fn a_recovery_code_works_exactly_once() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let fix = mfa_account(&pool, "carol").await;
    let app = common::create_app(&pool, "read").await;
    let http = common::test_app(pool.clone());

    let first = mfa_grant(&http, &app, "carol", "read").await;
    let spent = post_challenge(
        &http,
        &challenge_fields(&first, "recovery", &fix.codes[0], &app),
    )
    .await;
    assert_eq!(spent.status, StatusCode::OK, "a backup code passes");
    assert_eq!(spent.json_value()["scope"], "read", "for the parked scopes");

    let (column,): (Value,) = sqlx::query_as("SELECT mfa_settings FROM users WHERE id = $1")
        .bind(fix.user.id)
        .fetch_one(&pool)
        .await
        .expect("the settings column answers");
    let stored = mfa::MfaSettings::from_value(&column).backup_codes;
    assert_eq!(stored.len(), 4, "the spent hash came off the list");
    assert!(
        !stored.contains(&totp::hash_backup_code(&fix.codes[0])),
        "the spent code's hash is gone"
    );
    assert!(
        stored.contains(&totp::hash_backup_code(&fix.codes[1])),
        "the others survive"
    );

    // The spent code never works again, even behind a fresh challenge…
    let second = mfa_grant(&http, &app, "carol", "read").await;
    let reused = post_challenge(
        &http,
        &challenge_fields(&second, "recovery", &fix.codes[0], &app),
    )
    .await;
    assert_invalid_code(&reused, "a spent recovery code");
    // …while a code that was never spent still does.
    let passed = post_challenge(
        &http,
        &challenge_fields(&second, "recovery", &fix.codes[1], &app),
    )
    .await;
    assert_eq!(
        passed.status,
        StatusCode::OK,
        "an unspent recovery code passes"
    );
}

/// The challenge refuses what it cannot honour as one voice: an unknown
/// or expired token, an absent code, an unsupported type, failed client
/// authentication and a foreign application all answer 400 "Invalid
/// code".
#[tokio::test]
async fn the_challenge_refuses_everything_it_cannot_honour() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let fix = mfa_account(&pool, "dave").await;
    let app = common::create_app(&pool, "read").await;
    let http = common::test_app(pool.clone());
    let token = mfa_grant(&http, &app, "dave", "read").await;
    let real = current_code(&fix.secret);

    let unknown = post_challenge(
        &http,
        &challenge_fields(
            "no-such-challenge-token-of-forty-three-chars-x",
            "totp",
            &real,
            &app,
        ),
    )
    .await;
    assert_invalid_code(&unknown, "an unknown token");

    let (stale_id, _) = assert_challenge_stored(&pool, &token, &fix.user).await;
    let outcome =
        sqlx::query("UPDATE mfa_tokens SET valid_until = now() - interval '1 hour' WHERE id = $1")
            .bind(stale_id)
            .execute(&pool)
            .await
            .expect("the expiry update runs");
    assert_eq!(outcome.rows_affected(), 1, "and hit the row");
    let stale = post_challenge(&http, &challenge_fields(&token, "totp", &real, &app)).await;
    assert_invalid_code(&stale, "an expired token");

    let fresh = mfa_grant(&http, &app, "dave", "read").await;
    let no_type = vec![
        ("mfa_token", fresh.as_str()),
        ("code", real.as_str()),
        ("client_id", app.client_id.as_str()),
        ("client_secret", app.client_secret.as_str()),
    ];
    assert_invalid_code(
        &post_challenge(&http, &no_type).await,
        "a missing challenge type",
    );
    let sms = challenge_fields(&fresh, "sms", &real, &app);
    assert_invalid_code(
        &post_challenge(&http, &sms).await,
        "an unsupported challenge type",
    );
    let no_code = vec![
        ("mfa_token", fresh.as_str()),
        ("challenge_type", "totp"),
        ("client_id", app.client_id.as_str()),
        ("client_secret", app.client_secret.as_str()),
    ];
    assert_invalid_code(&post_challenge(&http, &no_code).await, "a missing code");

    let wrong_secret = vec![
        ("mfa_token", fresh.as_str()),
        ("challenge_type", "totp"),
        ("code", real.as_str()),
        ("client_id", app.client_id.as_str()),
        ("client_secret", "not-the-secret"),
    ];
    assert_invalid_code(
        &post_challenge(&http, &wrong_secret).await,
        "a wrong client secret",
    );
    let anonymous = vec![
        ("mfa_token", fresh.as_str()),
        ("challenge_type", "totp"),
        ("code", real.as_str()),
    ];
    assert_invalid_code(
        &post_challenge(&http, &anonymous).await,
        "client credentials left off",
    );

    // The token belongs to the app that started the grant: a different
    // application's valid credentials refuse alike.
    let thief = common::create_app(&pool, "read").await;
    let stolen = challenge_fields(&fresh, "totp", &real, &thief);
    assert_invalid_code(
        &post_challenge(&http, &stolen).await,
        "another app's credentials",
    );
}

/// HTTP Basic is the token endpoint's preferred credential form and
/// authenticates the challenge here too: a correctly keyed request passes
/// and issues the deferred grant.
#[tokio::test]
async fn the_challenge_authenticates_the_client_with_http_basic() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let fix = mfa_account(&pool, "dave").await;
    let app = common::create_app(&pool, "read").await;
    let http = common::test_app(pool.clone());
    let fresh = mfa_grant(&http, &app, "dave", "read").await;
    let real = current_code(&fix.secret);

    let basic = common::TestRequest::post("/oauth/mfa/challenge")
        .header(
            "authorization",
            &format!(
                "Basic {}",
                STANDARD.encode(format!("{}:{}", app.client_id, app.client_secret))
            ),
        )
        .form(&[
            ("mfa_token", fresh.as_str()),
            ("challenge_type", "totp"),
            ("code", real.as_str()),
        ])
        .send(&http)
        .await;
    assert_eq!(
        basic.status,
        StatusCode::OK,
        "the Basic-authenticated challenge passes"
    );
    assert_eq!(basic.json_value()["scope"], "read", "and issues the grant");
}

/// Accounts the gate does not touch keep the plain grant: a fresh
/// account, and one whose TOTP setup was never confirmed (`enabled`
/// still off, Pleroma's `MFA.require?`).
#[tokio::test]
async fn accounts_without_mfa_are_unaffected() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::create_app(&pool, "read write").await;
    let http = common::test_app(pool.clone());

    let plain = common::create_user_with_password(&pool, "erin", PASSWORD).await;
    let granted = post_json(&http, &grant_body(&app, "erin", "read write")).await;
    assert_eq!(
        granted.status,
        StatusCode::OK,
        "an account without MFA gets its token"
    );
    assert!(
        granted.json_value()["access_token"].is_string(),
        "the pair is issued, not a challenge"
    );
    assert_eq!(
        token_count(&pool, plain.id).await,
        1,
        "and the row stands behind it"
    );

    let half_set_up = common::create_user_with_password(&pool, "fern", PASSWORD).await;
    let mut settings = mfa::MfaSettings::default();
    settings.setup_totp(&totp::generate_secret());
    mfa::save(&pool, half_set_up.id, &settings)
        .await
        .expect("MFA settings save for fixtures");
    let finished = post_json(&http, &grant_body(&app, "fern", "read")).await;
    assert_eq!(
        finished.status,
        StatusCode::OK,
        "an unconfirmed secret has not enabled MFA"
    );
    assert!(
        finished.json_value()["access_token"].is_string(),
        "so the grant finishes"
    );

    assert_eq!(
        challenge_count(&pool).await,
        0,
        "none of this stored a challenge"
    );
}
