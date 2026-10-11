//! MFA endpoints (`/api/pleroma/accounts/mfa`, WBE-131): TOTP setup,
//! confirmation, backup codes and disabling over the real router.
//!
//! The lifecycle test walks the whole state machine exactly as a client
//! would (setup → wrong code → confirm → settings → backup codes →
//! disable); the tests beside it pin the ticket's individual rules:
//! re-setup replaces an unconfirmed secret but not a confirmed one, the
//! password gate precedes the code gate, backup codes require MFA, and
//! every route enforces Pleroma's `AuthUser` + scope pipeline. The secret
//! must never reach `Debug` output, tested last against the loaded row.

#![cfg(test)]

pub mod common;

use std::collections::HashSet;

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    models::user::{self, User, mfa::MfaSettings},
    util::totp,
};

/// Password every login-capable fixture shares.
const PASSWORD: &str = "correct horse battery staple";
/// The MFA root path the endpoints live under.
const MFA: &str = "/api/pleroma/accounts/mfa";

/// Inserts a local user named `nickname` that can log in with
/// [`PASSWORD`] plus a bearer token granting `scopes`.
async fn fixture(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_user_with_password(pool, nickname, PASSWORD).await;
    let minted = common::create_app_and_token(pool, &created, scopes).await;
    (created, minted)
}

/// The stored `mfa_settings` column of user `id`, parsed with the model's
/// lenient reader.
async fn stored(pool: &PgPool, id: Uuid) -> MfaSettings {
    let row: (Value,) = sqlx::query_as("SELECT mfa_settings FROM users WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("the fixture user exists");
    MfaSettings::from_value(&row.0)
}

/// The current 6-digit code for `secret`, as an authenticator app would
/// produce it right now.
fn current_code(secret: &str) -> String {
    totp::code_at(secret, chrono::Utc::now().timestamp()).expect("a generated secret decodes")
}

/// A code that differs from `real` in its first digit.
fn wrong_code(real: &str) -> String {
    let (head, tail) = real.split_at(1);
    let replacement = if head == "0" { "1" } else { "0" };
    format!("{replacement}{tail}")
}

/// The body list of a `backup_codes` response, as strings.
fn codes_in(body: &Value) -> Vec<String> {
    serde_json::from_value(body["backup_codes"].clone()).expect("backup_codes is a list")
}

/// `GET /api/pleroma/accounts/mfa` as `token`, asserting 200, parsed.
async fn get_settings(app: &Router, token: &str) -> Value {
    let fetched = common::TestRequest::get(MFA).bearer(token).send(app).await;
    assert_eq!(fetched.status, StatusCode::OK, "settings status");
    fetched.json_value()
}

/// `GET …/setup/totp` as `token`.
async fn setup(app: &Router, token: &str) -> common::TestResponse {
    common::TestRequest::get(&format!("{MFA}/setup/totp"))
        .bearer(token)
        .send(app)
        .await
}

/// `POST …/confirm/totp` as `token` with `password` and `code` (a form
/// body, the shape Mastodon clients send).
async fn confirm(app: &Router, token: &str, password: &str, code: &str) -> common::TestResponse {
    common::TestRequest::post(&format!("{MFA}/confirm/totp"))
        .bearer(token)
        .form(&[("password", password), ("code", code)])
        .send(app)
        .await
}

/// `DELETE …/totp` as `token` with a JSON `password` parameter.
async fn disable(app: &Router, token: &str, password: &str) -> common::TestResponse {
    common::TestRequest::delete(&format!("{MFA}/totp"))
        .bearer(token)
        .json(&json!({"password": password}))
        .send(app)
        .await
}

/// `GET …/backup_codes` as `token`.
async fn backup_codes(app: &Router, token: &str) -> common::TestResponse {
    common::TestRequest::get(&format!("{MFA}/backup_codes"))
        .bearer(token)
        .send(app)
        .await
}

/// The full lifecycle from the ticket: setup, a rejected wrong code, a
/// successful confirm, settings, backup codes and the disable.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn full_lifecycle_setup_confirm_backup_codes_disable(pool: PgPool) {
    let app = common::test_app(pool.clone());
    let (alice, token) = fixture(&pool, "alice", "read write").await;

    assert_eq!(
        get_settings(&app, &token).await,
        json!({"settings": {"enabled": false, "totp": false}}),
        "a fresh account has MFA off"
    );

    let opened = setup(&app, &token).await;
    assert_eq!(opened.status, StatusCode::OK, "setup answers 200");
    let opened_body = opened.json_value();
    let key = opened_body["key"]
        .as_str()
        .expect("setup returns a key")
        .to_owned();
    assert_eq!(key.len(), 32, "the key is 32 base32 characters");
    assert_eq!(
        opened_body["provisioning_uri"].as_str(),
        Some(totp::provisioning_uri(&key, "alice", "Wired").as_str()),
        "the URI carries issuer INSTANCE_NAME and account nickname"
    );
    let after_setup = stored(&pool, alice.id).await;
    assert_eq!(
        after_setup.totp.secret.as_deref(),
        Some(key.as_str()),
        "the secret is stored as-is so codes verify against it"
    );
    assert!(!after_setup.enabled, "setup alone does not enable MFA");
    assert!(
        !after_setup.totp.confirmed,
        "and the method starts unconfirmed"
    );

    let real = current_code(&key);
    let rejected = confirm(&app, &token, PASSWORD, &wrong_code(&real)).await;
    assert_eq!(rejected.status, StatusCode::UNPROCESSABLE_ENTITY, "422");
    assert_eq!(
        rejected.json_value(),
        json!({"error": "invalid_token"}),
        "the wrong code is Pleroma's invalid_token"
    );
    assert!(
        !stored(&pool, alice.id).await.enabled,
        "a rejected code enables nothing"
    );

    let accepted = confirm(&app, &token, PASSWORD, &real).await;
    assert_eq!(accepted.status, StatusCode::OK, "the right code confirms");
    assert_eq!(
        accepted.json_value(),
        json!({}),
        "confirm answers the empty object"
    );
    let confirmed = stored(&pool, alice.id).await;
    assert!(
        confirmed.enabled && confirmed.totp.confirmed,
        "both flags set"
    );
    assert_eq!(
        confirmed.totp.secret.as_deref(),
        Some(key.as_str()),
        "the secret survives for future logins"
    );
    assert_eq!(
        get_settings(&app, &token).await,
        json!({"settings": {"enabled": true, "totp": true}}),
        "settings now report the method"
    );

    let issued = backup_codes(&app, &token).await;
    assert_eq!(issued.status, StatusCode::OK, "codes are issued");
    let codes = codes_in(&issued.json_value());
    assert_eq!(codes.len(), 5, "Pleroma's count of five codes");
    let unique: HashSet<&String> = codes.iter().collect();
    assert_eq!(unique.len(), 5, "the codes are distinct");
    for code in &codes {
        assert_eq!(code.len(), 8, "codes are 8 characters: `{code}`");
        assert!(
            code.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()),
            "`{code}` is [a-z0-9]"
        );
    }
    let rotated = stored(&pool, alice.id).await;
    let hashes: HashSet<String> = codes
        .iter()
        .map(|code| totp::hash_backup_code(code))
        .collect();
    assert_eq!(
        rotated.backup_codes.iter().collect::<HashSet<_>>(),
        hashes.iter().collect::<HashSet<_>>(),
        "only the SHA-256 hex hashes of the issued codes are stored"
    );
    for hash in &rotated.backup_codes {
        assert_eq!(hash.len(), 64, "a SHA-256 is 32 hex bytes: {hash}");
    }

    let reissued = backup_codes(&app, &token).await;
    let second_codes = codes_in(&reissued.json_value());
    assert_ne!(codes, second_codes, "a rotation replaces the codes");
    let after_rotation = stored(&pool, alice.id).await;
    assert_eq!(after_rotation.backup_codes.len(), 5, "still five hashes");
    let second_hashes: HashSet<String> = second_codes
        .iter()
        .map(|code| totp::hash_backup_code(code))
        .collect();
    assert_eq!(
        after_rotation.backup_codes.iter().collect::<HashSet<_>>(),
        second_hashes.iter().collect::<HashSet<_>>(),
        "and they are the new codes' hashes: the old codes stop working"
    );

    let refused = disable(&app, &token, "not-her-password").await;
    assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY, "422");
    assert_eq!(
        refused.json_value(),
        json!({"error": "Invalid password."}),
        "the disable gate is the password"
    );
    assert!(
        stored(&pool, alice.id).await.enabled,
        "a rejected disable changes nothing"
    );

    let gone = disable(&app, &token, PASSWORD).await;
    assert_eq!(gone.status, StatusCode::OK, "the right password disables");
    assert_eq!(
        gone.json_value(),
        json!({}),
        "disable answers the empty object"
    );
    let cleared = stored(&pool, alice.id).await;
    assert_eq!(
        cleared,
        MfaSettings::default(),
        "secret, codes and the switch are all cleared"
    );
    assert_eq!(
        get_settings(&app, &token).await,
        json!({"settings": {"enabled": false, "totp": false}}),
        "and settings report MFA off again"
    );

    let reopened = setup(&app, &token).await;
    assert_eq!(
        reopened.status,
        StatusCode::OK,
        "disabling makes setup legal again"
    );
    assert_ne!(
        reopened.json_value()["key"].as_str(),
        Some(key.as_str()),
        "and hands out a fresh secret"
    );
}

/// Setup mints a new secret that replaces an unconfirmed one; the
/// superseded secret stops being able to confirm.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn re_setup_replaces_an_unconfirmed_secret(pool: PgPool) {
    let app = common::test_app(pool.clone());
    let (bob, token) = fixture(&pool, "bob", "write:security").await;

    let first = setup(&app, &token).await.json_value();
    let second = setup(&app, &token).await.json_value();
    let first_key = first["key"].as_str().expect("a key").to_owned();
    let second_key = second["key"].as_str().expect("a key").to_owned();
    assert_ne!(first_key, second_key, "a new secret each call");
    assert_eq!(
        stored(&pool, bob.id).await.totp.secret.as_deref(),
        Some(second_key.as_str()),
        "the row holds only the newest secret"
    );

    let stale = confirm(&app, &token, PASSWORD, &current_code(&first_key)).await;
    assert_eq!(
        stale.json_value(),
        json!({"error": "invalid_token"}),
        "the superseded secret's code is rejected"
    );
    let fresh = confirm(&app, &token, PASSWORD, &current_code(&second_key)).await;
    assert_eq!(
        fresh.status,
        StatusCode::OK,
        "and the replacement's code confirms"
    );
}

/// A confirmed method cannot be set up again, and setup leaves the
/// stored secret alone.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn setup_refuses_a_confirmed_method(pool: PgPool) {
    let app = common::test_app(pool.clone());
    let (carol, token) = fixture(&pool, "carol", "read write").await;
    let key = setup(&app, &token).await.json_value()["key"]
        .as_str()
        .expect("a key")
        .to_owned();
    let accepted = confirm(&app, &token, PASSWORD, &current_code(&key)).await;
    assert_eq!(accepted.status, StatusCode::OK, "confirmed fixture");

    let refused = setup(&app, &token).await;
    assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY, "422");
    assert_eq!(
        refused.json_value(),
        json!({"error": "TOTP already enabled"}),
        "the ticket's wording for a confirmed method"
    );
    assert_eq!(
        stored(&pool, carol.id).await.totp.secret.as_deref(),
        Some(key.as_str()),
        "the refusal changed nothing"
    );
}

/// Confirmation without a prior setup names the missing step, not the
/// code.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn confirm_without_setup_is_setup_required(pool: PgPool) {
    let app = common::test_app(pool.clone());
    let (_dave, token) = fixture(&pool, "dave", "write").await;
    let refused = confirm(&app, &token, PASSWORD, "123456").await;
    assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY, "422");
    assert_eq!(
        refused.json_value(),
        json!({"error": "Setup required"}),
        "the ticket's missing-setup answer"
    );
}

/// The password gate runs before the code gate, in both directions.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn confirm_checks_the_password_before_the_code(pool: PgPool) {
    let app = common::test_app(pool.clone());
    let (erin, token) = fixture(&pool, "erin", "read write").await;
    let key = setup(&app, &token).await.json_value()["key"]
        .as_str()
        .expect("a key")
        .to_owned();

    let denied = confirm(&app, &token, "hunter2 wrong", &current_code(&key)).await;
    assert_eq!(
        denied.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a wrong password is a 422 even with the right code"
    );
    assert_eq!(
        denied.json_value(),
        json!({"error": "Invalid password."}),
        "Pleroma's wording, period included"
    );
    let settings = stored(&pool, erin.id).await;
    assert!(
        !settings.enabled && !settings.totp.confirmed,
        "nothing confirmed"
    );

    // A row with no password hash at all (the shape `create_local_user`
    // fixtures have) fails the same way — it can never pass the gate.
    let hashless = common::create_local_user(&pool, "hashless").await;
    let hashless_token = common::create_app_and_token(&pool, &hashless, "write").await;
    let hashless_denied = confirm(&app, &hashless_token, PASSWORD, "123456").await;
    assert_eq!(
        hashless_denied.json_value(),
        json!({"error": "Invalid password."}),
        "an account without a password cannot confirm TOTP either"
    );

    // A missing required parameter is the body extractor's 422.
    let incomplete = common::TestRequest::post(&format!("{MFA}/confirm/totp"))
        .bearer(&hashless_token)
        .form(&[("password", PASSWORD)])
        .send(&app)
        .await;
    assert_eq!(
        incomplete.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a parameterless confirm is unprocessable"
    );
}

/// Backup codes require MFA; disabling requires the password.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn backup_codes_and_disable_gates(pool: PgPool) {
    let app = common::test_app(pool.clone());
    let (fern, token) = fixture(&pool, "fern", "read write").await;

    let early = backup_codes(&app, &token).await;
    assert_eq!(early.status, StatusCode::UNPROCESSABLE_ENTITY, "422");
    assert_eq!(
        early.json_value(),
        json!({"error": "MFA not enabled"}),
        "backup codes are a post-MFA feature"
    );

    let key = setup(&app, &token).await.json_value()["key"]
        .as_str()
        .expect("a key")
        .to_owned();
    let unconfirmed = backup_codes(&app, &token).await;
    assert_eq!(
        unconfirmed.json_value(),
        json!({"error": "MFA not enabled"}),
        "an unconfirmed secret is not MFA yet"
    );

    let accepted = confirm(&app, &token, PASSWORD, &current_code(&key)).await;
    assert_eq!(accepted.status, StatusCode::OK, "fixture: MFA on");
    let refused = disable(&app, &token, "wrong-password").await;
    assert_eq!(
        refused.json_value(),
        json!({"error": "Invalid password."}),
        "the disable password gate matches the confirm one"
    );
    assert!(
        stored(&pool, fern.id).await.enabled,
        "a refused disable sticks"
    );
}

/// Auth and scopes follow Pleroma's plug pipeline: every route demands a
/// user token, `settings` demands `read:security`, everything that
/// changes state (including `backup_codes`) demands `write:security`.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn auth_and_scope_requirements(pool: PgPool) {
    let app = common::test_app(pool.clone());
    let (_kate, no_scopes) = fixture(&pool, "kate", "").await;
    let (_gwen, read_only) = fixture(&pool, "gwen", "read").await;
    let (_hal, write_only) = fixture(&pool, "hal", "write").await;
    let (_ivy, read_security) = fixture(&pool, "ivy", "read:security").await;
    let (_jon, write_security) = fixture(&pool, "jon", "write:security").await;
    let app_only = common::create_app_only_token(&pool, "read write").await;

    // No token at all: 401 "Invalid credentials" on every route.
    let anon_settings = common::TestRequest::get(MFA).send(&app).await;
    assert_eq!(
        anon_settings.status,
        StatusCode::UNAUTHORIZED,
        "401 settings"
    );
    assert_eq!(
        anon_settings.json_value(),
        json!({"error": "Invalid credentials"}),
        "the no-token body"
    );
    let anon_setup = setup(&app, "").await;
    assert_eq!(anon_setup.status, StatusCode::UNAUTHORIZED, "401 setup");
    let anon_confirm = common::TestRequest::post(&format!("{MFA}/confirm/totp"))
        .json(&json!({"password": PASSWORD, "code": "123456"}))
        .send(&app)
        .await;
    assert_eq!(anon_confirm.status, StatusCode::UNAUTHORIZED, "401 confirm");
    let anon_disable = common::TestRequest::delete(&format!("{MFA}/totp"))
        .json(&json!({"password": PASSWORD}))
        .send(&app)
        .await;
    assert_eq!(anon_disable.status, StatusCode::UNAUTHORIZED, "401 disable");
    let anon_codes = backup_codes(&app, "").await;
    assert_eq!(
        anon_codes.status,
        StatusCode::UNAUTHORIZED,
        "401 backup codes"
    );

    // An app-only token never names a user.
    let app_view = common::TestRequest::get(MFA)
        .bearer(&app_only)
        .send(&app)
        .await;
    assert_eq!(
        app_view.status,
        StatusCode::FORBIDDEN,
        "an app-only token is not a user"
    );
    assert_eq!(
        app_view.json_value(),
        json!({"error": "This API requires an authenticated user"}),
        "Pleroma's user-required wording"
    );

    // A `write` token cannot read settings, a `read` token cannot touch
    // any state — exactly the 403s Pleroma's scope plug answers, and an
    // empty scope list grants neither side.
    let write_on_settings = common::TestRequest::get(MFA)
        .bearer(&write_only)
        .send(&app)
        .await;
    assert_eq!(
        write_on_settings.status,
        StatusCode::FORBIDDEN,
        "settings 403"
    );
    assert_eq!(
        write_on_settings.json_value(),
        json!({"error": "Insufficient permissions: read:security."}),
        "the denial names the missing scope"
    );
    let none_on_settings = common::TestRequest::get(MFA)
        .bearer(&no_scopes)
        .send(&app)
        .await;
    assert_eq!(
        none_on_settings.status,
        StatusCode::FORBIDDEN,
        "an empty scope list grants nothing"
    );
    let read_on_setup = setup(&app, &read_only).await;
    assert_eq!(read_on_setup.status, StatusCode::FORBIDDEN, "setup 403");
    assert_eq!(
        read_on_setup.json_value(),
        json!({"error": "Insufficient permissions: write:security."}),
        "Pleroma's own test pins this exact body"
    );
    assert_eq!(
        backup_codes(&app, &read_only).await.status,
        StatusCode::FORBIDDEN,
        "backup_codes is a write action"
    );
    assert_eq!(
        confirm(&app, &read_only, PASSWORD, "123456").await.status,
        StatusCode::FORBIDDEN,
        "confirm is gated by scope before anything else"
    );
    assert_eq!(
        disable(&app, &read_only, PASSWORD).await.status,
        StatusCode::FORBIDDEN,
        "disable too"
    );
    assert_eq!(
        common::TestRequest::get(MFA)
            .bearer(&read_security)
            .send(&app)
            .await
            .status,
        StatusCode::OK,
        "the narrow read scope suffices for settings"
    );
    assert_eq!(
        setup(&app, &write_security).await.status,
        StatusCode::OK,
        "and the narrow write scope for setup"
    );
}

/// The stored TOTP secret must never surface in a `Debug` rendering of
/// the row (or of the typed settings), and issued backup codes neither.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn secrets_never_appear_in_debug(pool: PgPool) {
    let app = common::test_app(pool.clone());
    let (mary, token) = fixture(&pool, "mary", "write").await;
    let key = setup(&app, &token).await.json_value()["key"]
        .as_str()
        .expect("a key")
        .to_owned();

    let loaded = user::get_by_id(&pool, mary.id)
        .await
        .expect("the read works")
        .expect("the row exists");
    let setup_debug = format!("{loaded:?}");
    assert!(
        !setup_debug.contains(&key),
        "the TOTP secret must not leak into User's Debug output"
    );
    assert!(
        !setup_debug.contains("mfa_settings: Object"),
        "the column is redacted, not dumped: {setup_debug}"
    );

    let parsed = MfaSettings::for_user(&loaded);
    assert!(
        !format!("{parsed:?}").contains(&key),
        "and not through the typed settings either"
    );

    // A backup code, once stored, stays hashed everywhere too.
    let accepted = confirm(&app, &token, PASSWORD, &current_code(&key)).await;
    assert_eq!(accepted.status, StatusCode::OK, "fixture: MFA on");
    let codes = codes_in(&backup_codes(&app, &token).await.json_value());
    let reloaded = user::get_by_id(&pool, mary.id)
        .await
        .expect("the read works")
        .expect("the row exists");
    let codes_debug = format!("{reloaded:?}");
    for code in &codes {
        assert!(
            !codes_debug.contains(code.as_str()),
            "an issued backup code must not leak into Debug output"
        );
    }
}
