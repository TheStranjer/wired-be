//! Integration tests for the OAuth MFA challenge flow (WBE-132): the
//! `mfa_required` 403 of the password grant, the
//! `POST /oauth/mfa/challenge` exchange (TOTP codes, recovery codes,
//! wrong/expired/foreign refusals), and the `POST /oauth/authorize`
//! detour through the `/oauth/mfa` page and its
//! `POST /oauth/mfa/verify` completion (Pleroma's
//! `lib/pleroma/web/o_auth/mfa_controller.ex` and the `o_auth/mfa/`
//! templates).
//!
//! The fixture of every test is an account that can log in with
//! [`PASSWORD`] and has MFA confirmed (secret + issued backup codes),
//! plus a registered application. The password grant refuses it the 403
//! challenge; the challenge half then runs the second factor and
//! answers the same token JSON the grant withholds. The authorize half
//! redirects the browser to the code page and delivers the parked
//! authorization code once the verification passes.

#![cfg(test)]

// `pub` because Clippy's nursery `redundant_pub_crate` forbids `pub(crate)`
// items inside a private module, while `unreachable_pub` forbids `pub` ones:
// only a `pub` module makes the harness helpers lint-clean.
pub mod common;

use axum::{Router, http::StatusCode};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::DEFAULT_OAUTH_TOKEN_TTL_SECS,
    models::{
        oauth_app::{self, NewOauthApp, OauthApp},
        oauth_token::{self, OauthAuthorization},
        user::{User, mfa},
    },
    util::totp,
};

/// The password every login fixture shares.
const PASSWORD: &str = "correct horse battery staple";
/// The redirect URI the standard app fixture registers.
const REDIRECT: &str = "https://client.example/callback";
/// The out-of-band redirect URI: the code is shown on a page instead of
/// redirected.
const OOB: &str = "urn:ietf:wg:oauth:2.0:oob";
/// The error body every failed challenge shares (Pleroma's
/// `MFAController.challenge`).
const INVALID_CODE: &str = "Invalid code";
/// The flash line above a re-rendered verification form.
const TWO_FACTOR_FAILED: &str = "Two-factor authentication failed.";

/// An MFA-enabled account beside its TOTP secret and issued backup
/// codes.
struct MfaAccount {
    /// The MFA-enabled user.
    user: User,
    /// The account's confirmed TOTP secret.
    secret: String,
    /// The account's issued backup codes in plaintext.
    codes: Vec<String>,
}

/// Inserts the MFA fixtures: `nickname` logging in with [`PASSWORD`],
/// MFA confirmed with five issued backup codes.
///
/// # Panics
///
/// If the fixtures fail to insert.
async fn mfa_account(pool: &PgPool, nickname: &str) -> MfaAccount {
    let (user, secret, codes) =
        common::create_mfa_user_with_password(pool, nickname, PASSWORD, 5).await;
    MfaAccount {
        user,
        secret,
        codes,
    }
}

/// The password-grant body for `app` as `username`, asking `scope`.
fn grant_body(app: &OauthApp, username: &str, scope: &str) -> Value {
    json!({
        "grant_type": "password",
        "username": username,
        "password": PASSWORD,
        "scope": scope,
        "client_id": app.client_id,
        "client_secret": app.client_secret,
    })
}

/// `POST /oauth/token` as JSON.
async fn post_json(http: &Router, body: &Value) -> common::TestResponse {
    common::TestRequest::post("/oauth/token")
        .json(body)
        .send(http)
        .await
}

/// The `error` message of a JSON error response.
fn error_text(response: &common::TestResponse) -> String {
    response.json_value()["error"]
        .as_str()
        .expect("errors carry a string error field")
        .to_owned()
}

/// Assert the no-store cache headers every `/oauth/token`-family answer
/// carries (RFC 6749 §5.1).
fn assert_no_store(response: &common::TestResponse) {
    assert_eq!(
        response.header("cache-control"),
        Some("no-store"),
        "cache-control: no-store"
    );
    assert_eq!(
        response.header("pragma"),
        Some("no-cache"),
        "pragma: no-cache"
    );
}

/// Runs the password grant for an MFA-enabled account and returns the
/// challenge token the 403 carries, asserting Pleroma's exact body
/// shape on the way.
///
/// # Panics
///
/// If the grant does not answer the pinned 403 challenge.
async fn mfa_grant(http: &Router, app: &OauthApp, username: &str, scope: &str) -> String {
    let refused = post_json(http, &grant_body(app, username, scope)).await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "MFA gates the grant");
    assert_no_store(&refused);
    let body = refused.json_value();
    assert_eq!(body["error"], "mfa_required", "Pleroma's challenge error");
    assert_eq!(
        body["supported_challenge_types"], "totp",
        "totp is the only method this server speaks"
    );
    let token = body["mfa_token"]
        .as_str()
        .expect("mfa_token is a string")
        .to_owned();
    assert_eq!(token.len(), 43, "the 43-char Pleroma credential shape");
    assert_eq!(
        body.as_object().expect("an object").len(),
        3,
        "exactly the three documented keys, and no token: {body}"
    );
    token
}

/// The stored challenge `token` names: its id, linked authorization and
/// expiry.
async fn stored_challenge(
    pool: &PgPool,
    token: &str,
) -> Option<(Uuid, Option<Uuid>, DateTime<Utc>)> {
    sqlx::query_as("SELECT id, authorization_id, valid_until FROM mfa_tokens WHERE token = $1")
        .bind(token)
        .fetch_optional(pool)
        .await
        .expect("the challenge lookup reaches the database")
}

/// Asserts the stored challenge of `token` belongs to `user`, links a
/// pending authorization and is about five minutes out; returns both
/// ids.
///
/// # Panics
///
/// If the challenge is gone or unlinked.
async fn assert_challenge_stored(pool: &PgPool, token: &str, user: &User) -> (Uuid, Uuid) {
    let (id, authorization_id, valid_until) = stored_challenge(pool, token)
        .await
        .expect("the challenge was stored");
    let owner: (Uuid,) = sqlx::query_as("SELECT user_id FROM mfa_tokens WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("the challenge row answers");
    assert_eq!(owner.0, user.id, "the challenge guards the account");
    let linked = authorization_id.expect("the challenge links the parked authorization");
    let remaining = valid_until.signed_duration_since(Utc::now()).num_seconds();
    assert!(
        (235..=305).contains(&remaining),
        "the challenge is five minutes out, got {remaining}s"
    );
    (id, linked)
}

/// The stored authorization with primary key `id`.
///
/// # Panics
///
/// If the row is gone.
async fn authorization_by_id(pool: &PgPool, id: Uuid) -> OauthAuthorization {
    sqlx::query_as("SELECT * FROM oauth_authorizations WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("the authorization row is stored")
}

/// The current 6-digit TOTP code of `secret`, as an authenticator app
/// would produce it right now.
fn current_code(secret: &str) -> String {
    totp::code_at(secret, Utc::now().timestamp()).expect("a generated secret decodes")
}

/// A code that differs from `real` in its first digit.
fn wrong_code(real: &str) -> String {
    let (head, tail) = real.split_at(1);
    let replacement = if head == "0" { "1" } else { "0" };
    format!("{replacement}{tail}")
}

/// The form fields of a `POST /oauth/mfa/challenge`: the challenge
/// token, type and code beside the application's body credentials.
fn challenge_fields<'a>(
    token: &'a str,
    challenge_type: &'a str,
    code: &'a str,
    app: &'a OauthApp,
) -> Vec<(&'a str, &'a str)> {
    vec![
        ("mfa_token", token),
        ("challenge_type", challenge_type),
        ("code", code),
        ("client_id", app.client_id.as_str()),
        ("client_secret", app.client_secret.as_str()),
    ]
}

/// Posts the challenge form.
async fn post_challenge(http: &Router, fields: &[(&str, &str)]) -> common::TestResponse {
    common::TestRequest::post("/oauth/mfa/challenge")
        .form(fields)
        .send(http)
        .await
}

/// Asserts a challenge answer is Pleroma's 400 "Invalid code" with the
/// no-store headers.
fn assert_invalid_code(response: &common::TestResponse, what: &str) {
    assert_eq!(response.status, StatusCode::BAD_REQUEST, "400 for {what}");
    assert_eq!(
        response.json_value(),
        json!({"error": INVALID_CODE}),
        "the one refusal for {what}"
    );
    assert_no_store(response);
}

/// The urlencoded `POST /oauth/authorize` consent form logging in as
/// `nickname`/[`PASSWORD`], approving `scope` and round-tripping `state`
/// (absent when `None`) to `REDIRECT`.
fn authorize_form<'a>(
    app: &'a OauthApp,
    nickname: &'a str,
    scope: &'a str,
    state: Option<&'a str>,
) -> Vec<(&'a str, &'a str)> {
    let mut fields = vec![
        ("authorization[client_id]", app.client_id.as_str()),
        ("authorization[redirect_uri]", REDIRECT),
        ("authorization[name]", nickname),
        ("authorization[password]", PASSWORD),
        ("authorization[scope][]", scope),
    ];
    if let Some(client_state) = state {
        fields.push(("authorization[state]", client_state));
    }
    fields
}

/// The `POST /oauth/mfa/verify` form: the challenge submission plus the
/// round-tripped delivery fields.
fn verify_form<'a>(
    token: &'a str,
    challenge_type: &'a str,
    code: &'a str,
    redirect_uri: &'a str,
    state: Option<&'a str>,
) -> Vec<(&'a str, &'a str)> {
    let mut fields = vec![
        ("mfa[mfa_token]", token),
        ("mfa[challenge_type]", challenge_type),
        ("mfa[code]", code),
        ("mfa[redirect_uri]", redirect_uri),
    ];
    if let Some(client_state) = state {
        fields.push(("mfa[state]", client_state));
    }
    fields
}

/// Posts the consent form.
async fn post_authorize(http: &Router, fields: &[(&str, &str)]) -> common::TestResponse {
    common::TestRequest::post("/oauth/authorize")
        .form(fields)
        .send(http)
        .await
}

/// Posts the verification form.
async fn post_verify(http: &Router, fields: &[(&str, &str)]) -> common::TestResponse {
    common::TestRequest::post("/oauth/mfa/verify")
        .form(fields)
        .send(http)
        .await
}

/// The decoded `key` parameter of a URL's query.
fn query_param(url: &str, key: &str) -> Option<String> {
    let (_, query) = url.split_once('?').unwrap_or((url, ""));
    url::form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value)
}

/// The code shown on the out-of-band page, between its markers.
///
/// # Panics
///
/// If the body holds no `<code id="authorization-code">` element.
fn oob_page_code(body: &str) -> &str {
    body.split_once("<code id=\"authorization-code\">")
        .expect("the oob page shows the code")
        .1
        .split_once("</code>")
        .expect("the code element is closed")
        .0
}

/// Registers an app with the given `redirect_uris` and `scopes` (the
/// standard [`common::create_app`] fixture registers only
/// [`REDIRECT`]).
///
/// # Panics
///
/// If the app insert fails.
async fn register_app(pool: &PgPool, redirect_uris: &str, scopes: &str) -> OauthApp {
    oauth_app::create(
        pool,
        &NewOauthApp {
            client_name: "MFA Client".to_owned(),
            redirect_uris: redirect_uris.to_owned(),
            scopes: scopes.to_owned(),
            website: None,
            user_id: None,
        },
    )
    .await
    .expect("app registration inserts")
}

/// Rows in `mfa_tokens` in total.
///
/// # Panics
///
/// If the count query fails.
async fn challenge_count(pool: &PgPool) -> i64 {
    let (rows,): (i64,) = sqlx::query_as("SELECT count(*) FROM mfa_tokens")
        .fetch_one(pool)
        .await
        .expect("the count reaches the database");
    rows
}

/// The password grant of an MFA-enabled account answers the 403
/// `mfa_required` challenge and issues no token: the grant's scopes are
/// parked on a pending authorization the five-minute challenge links
/// (Pleroma's `handle_token_exchange_error({:mfa_required, …})`).
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn the_password_grant_of_an_mfa_user_answers_mfa_required(pool: PgPool) {
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

    assert!(
        oauth_token::list_for_user(&pool, fix.user.id)
            .await
            .expect("the token lookup reaches the database")
            .is_empty(),
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
    assert_eq!(error_text(&denied), "Invalid credentials", "stays the 400");

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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn a_totp_challenge_mints_the_token_pair(pool: PgPool) {
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
    assert!(
        oauth_token::list_for_user(&pool, fix.user.id)
            .await
            .expect("the lookup reaches the database")
            .is_empty(),
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn a_recovery_code_works_exactly_once(pool: PgPool) {
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
/// code"; HTTP Basic authenticates like on the token endpoint.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn the_challenge_refuses_everything_it_cannot_honour(pool: PgPool) {
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

    // HTTP Basic is the token endpoint's preferred credential form and
    // authenticates here too.
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn accounts_without_mfa_are_unaffected(pool: PgPool) {
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
        oauth_token::list_for_user(&pool, plain.id)
            .await
            .expect("the lookup reaches the database")
            .len(),
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

/// The full browser flow: a valid consent login from an MFA-enabled
/// account redirects to the code page instead of issuing the code; the
/// page round-trips every value escaped; a wrong code re-renders it with
/// the flash line; the right one delivers the parked code — unused, the
/// client's to redeem — exactly as a direct approval would.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn the_authorize_flow_detours_through_the_mfa_page(pool: PgPool) {
    let fix = mfa_account(&pool, "gina").await;
    let app = common::create_app(&pool, "read write").await;
    let http = common::test_app(pool.clone());

    let detour = post_authorize(&http, &authorize_form(&app, "gina", "read", Some("st&ate"))).await;
    assert_eq!(detour.status, StatusCode::FOUND, "the login redirects");
    assert_eq!(
        detour.header("cache-control"),
        Some("no-store"),
        "not even the detour is cacheable"
    );
    let location = detour.header("location").expect("a Location header");
    assert!(
        location.starts_with("/oauth/mfa?"),
        "the redirect aims at the MFA page: {location}"
    );
    let token = query_param(location, "mfa_token").expect("the challenge token rides along");
    assert_eq!(token.len(), 43, "its 43-char shape");
    assert_eq!(
        query_param(location, "redirect_uri").as_deref(),
        Some(REDIRECT),
        "the delivery target round-trips"
    );
    assert_eq!(
        query_param(location, "state").as_deref(),
        Some("st&ate"),
        "the urlencoded state survives the trip"
    );
    assert_eq!(
        query_param(location, "client_id").as_deref(),
        Some(app.client_id.as_str()),
        "so does the client id"
    );
    assert_eq!(
        query_param(location, "scope").as_deref(),
        Some("read"),
        "and the approved scope"
    );
    let (_challenge_id, authorization_id) = assert_challenge_stored(&pool, &token, &fix.user).await;
    assert_eq!(
        oauth_token::list_for_user(&pool, fix.user.id)
            .await
            .expect("the lookup reaches the database")
            .len(),
        0,
        "no token and no client-visible code yet"
    );

    let page = common::TestRequest::get(location).send(&http).await;
    assert_eq!(page.status, StatusCode::OK, "the code page renders");
    assert_eq!(
        page.header("content-type"),
        Some("text/html; charset=utf-8"),
        "as HTML"
    );
    assert_eq!(
        page.header("cache-control"),
        Some("no-store"),
        "and not one byte cacheable"
    );
    let html = page.text();
    assert!(
        html.contains("<h1>Two-factor authentication</h1>"),
        "Pleroma's page title"
    );
    assert!(
        html.contains("<form method=\"post\" action=\"/oauth/mfa/verify\">"),
        "the form posts to the verify endpoint"
    );
    assert!(
        html.contains(&format!("name=\"mfa[mfa_token]\" value=\"{token}\">")),
        "the challenge token rides the form"
    );
    assert!(
        html.contains("name=\"mfa[challenge_type]\" value=\"totp\">"),
        "as does the challenge type"
    );

    let real = current_code(&fix.secret);
    let failed = post_verify(
        &http,
        &verify_form(&token, "totp", &wrong_code(&real), REDIRECT, Some("st&ate")),
    )
    .await;
    assert_eq!(failed.status, StatusCode::UNAUTHORIZED, "Pleroma's 401");
    assert_eq!(
        failed.header("content-type"),
        Some("text/html; charset=utf-8"),
        "the form itself"
    );
    let retried = failed.text();
    assert!(
        retried.contains(TWO_FACTOR_FAILED),
        "with the flash line above it"
    );
    assert!(
        stored_challenge(&pool, &token).await.is_some(),
        "a refused verification retires nothing"
    );

    let finished = post_verify(
        &http,
        &verify_form(&token, "totp", &real, REDIRECT, Some("st&ate")),
    )
    .await;
    assert_eq!(
        finished.status,
        StatusCode::FOUND,
        "the right code finishes the flow"
    );
    assert_eq!(
        finished.header("cache-control"),
        Some("no-store"),
        "like every /oauth answer"
    );
    let redirect = finished.header("location").expect("a Location header");
    let code = query_param(redirect, "code").expect("the code parameter");
    assert_eq!(code.len(), 43, "the 43-char Pleroma code shape");
    assert_eq!(
        redirect,
        format!("{REDIRECT}?code={code}&state=st%26ate"),
        "the exact Location, the state round-tripped"
    );
    assert!(
        stored_challenge(&pool, &token).await.is_none(),
        "the passed challenge retires"
    );

    let parked = authorization_by_id(&pool, authorization_id).await;
    assert_eq!(parked.token, code, "the delivered code is the parked one");
    assert_eq!(parked.scopes, "read", "with the approved scopes");
    assert!(!parked.used, "and still fresh for the client to redeem");
    let redeemed = oauth_token::consume_authorization(&pool, &code, app.id)
        .await
        .expect("the exchange reaches the database");
    assert!(redeemed.is_some(), "the code is a real authorization code");
}

/// The verification honours the out-of-band client: no redirect, the
/// code page instead — Pleroma's `oob_authorization_created.html`
/// through the same door the consent flow uses.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn the_verification_delivers_the_code_to_an_oob_client(pool: PgPool) {
    let fix = mfa_account(&pool, "heidi").await;
    let app = register_app(&pool, &format!("{REDIRECT} {OOB}"), "read").await;
    let http = common::test_app(pool.clone());

    let mut fields = authorize_form(&app, "heidi", "read", None);
    fields[1] = ("authorization[redirect_uri]", OOB);
    let detour = post_authorize(&http, &fields).await;
    let location = detour.header("location").expect("a Location header");
    let token = query_param(location, "mfa_token").expect("the challenge token");
    assert_eq!(
        query_param(location, "redirect_uri").as_deref(),
        Some(OOB),
        "the out-of-band target round-trips"
    );

    let finished = post_verify(
        &http,
        &verify_form(&token, "totp", &current_code(&fix.secret), OOB, None),
    )
    .await;
    assert_eq!(
        finished.status,
        StatusCode::OK,
        "the oob client gets a page"
    );
    assert_eq!(
        finished.header("content-type"),
        Some("text/html; charset=utf-8"),
        "of HTML"
    );
    let body = finished.text();
    assert!(
        body.contains("<h1>Successfully authorized</h1>"),
        "Pleroma's oob page"
    );
    let code = oob_page_code(&body).to_owned();
    assert_eq!(code.len(), 43, "showing the 43-char code");
    let parked: OauthAuthorization =
        sqlx::query_as("SELECT * FROM oauth_authorizations WHERE token = $1")
            .bind(&code)
            .fetch_one(&pool)
            .await
            .expect("the code is the parked authorization");
    assert_eq!(parked.user_id, fix.user.id, "of the logging-in user");
}

/// The page renders every parameter it is handed escaped — a
/// malicious `state` or `mfa_token` cannot open a tag or break out of an
/// attribute — and `challenge_type=recovery` swaps to Pleroma's backup-
/// code form.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn the_mfa_page_escapes_and_switches_forms(pool: PgPool) {
    let http = common::test_app(pool.clone());

    let evil = common::TestRequest::get(
        "/oauth/mfa?mfa_token=%22%3E%3Cscript%3Ealert%281%29%3C%2Fscript%3E\
         &state=%3Cb%3Est%3C%2Fb%3E&redirect_uri=%22onload%3Dx&client_id=ci%26dc&scope=read",
    )
    .send(&http)
    .await;
    assert_eq!(evil.status, StatusCode::OK, "the page always renders");
    let html = evil.text();
    assert!(
        !html.contains("<script>alert"),
        "the token cannot open a tag"
    );
    assert!(!html.contains("<b>st</b>"), "nor the state: {html}");
    assert!(
        html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"),
        "it shows escaped inside its attribute"
    );
    assert!(
        html.contains("value=\"ci&amp;dc\""),
        "ampersands escape in the client id too"
    );

    let recovery =
        common::TestRequest::get("/oauth/mfa?mfa_token=t0ken&challenge_type=recovery&state=st")
            .send(&http)
            .await;
    assert_eq!(recovery.status, StatusCode::OK, "the recovery form renders");
    let swapped = recovery.text();
    assert!(
        swapped.contains("<h1>Two-factor recovery</h1>"),
        "Pleroma's recovery title"
    );
    assert!(
        swapped.contains("name=\"mfa[challenge_type]\" value=\"recovery\">"),
        "and the recovery challenge type"
    );
    assert!(
        swapped.contains("challenge_type=totp"),
        "the page links back to the code form"
    );
}
