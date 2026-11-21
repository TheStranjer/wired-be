//! Credential fixtures: apps, privileged and deactivated users, MFA users
//! and access tokens.

use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    models::{
        oauth_app::{self, NewOauthApp},
        oauth_token::{self, NewToken},
        user::{Flag, NewUser, User, insert, local_ap_id, mfa},
    },
    util::{
        password::hash_password,
        totp::{self, hash_backup_code},
    },
};

use super::{create_user_with_password, test_config};

/// The redirect URI every app fixture registers.
const TEST_REDIRECT_URIS: &str = "https://client.example/callback";
/// Registers a test app named "Test Client" granting `scopes` (one
/// space-separated string) and returns its row: the `/oauth/token` tests
/// need the credential pair, not a minted token.
///
/// # Panics
///
/// If the app insert fails.
pub async fn create_app(pool: &PgPool, scopes: &str) -> oauth_app::OauthApp {
    oauth_app::create(pool, &test_app_registration(None, scopes))
        .await
        .expect("app fixture inserts")
}

/// Inserts a local [`User`] named `nickname` that can log in with
/// `password` and holds the admin role (`is_admin = true`).
///
/// The one account the end-to-end scenarios may plant rather than
/// register: no public endpoint mints an admin, and every admin walk has
/// to start from one. Everything the scenarios then *do* to that account
/// — the logins, the tokens, the moderation — runs over HTTP.
///
/// # Panics
///
/// If hashing or the insert fails.
pub async fn create_admin_with_password(pool: &PgPool, nickname: &str, password: &str) -> User {
    let config = test_config();
    let new = NewUser {
        email: Some(format!("{nickname}@example.org")),
        password_hash: Some(
            hash_password(password).expect("password hashing succeeds for fixtures"),
        ),
        is_admin: Flag::new(true),
        ..NewUser::local(nickname, &local_ap_id(&config.base_url, nickname))
    };
    insert(pool, &new)
        .await
        .expect("admin user fixture inserts")
}

/// Inserts a local [`User`] named `nickname` that can log in with
/// `password` and holds the moderator role (`is_moderator = true`).
///
/// The [`create_admin_with_password`] twin for the staff-only surfaces:
/// like the admin, no public endpoint mints a moderator, so the
/// moderation scenarios plant one and then win its credential over HTTP.
///
/// # Panics
///
/// If hashing or the insert fails.
pub async fn create_moderator_with_password(pool: &PgPool, nickname: &str, password: &str) -> User {
    let config = test_config();
    let new = NewUser {
        email: Some(format!("{nickname}@example.org")),
        password_hash: Some(
            hash_password(password).expect("password hashing succeeds for fixtures"),
        ),
        is_moderator: Flag::new(true),
        ..NewUser::local(nickname, &local_ap_id(&config.base_url, nickname))
    };
    insert(pool, &new)
        .await
        .expect("moderator user fixture inserts")
}

/// Inserts a local [`User`] named `nickname` that can log in with
/// `password` but is deactivated (`is_active = false`): the shape a
/// suspended account has.
///
/// # Panics
///
/// If hashing or the insert fails.
pub async fn create_deactivated_user_with_password(
    pool: &PgPool,
    nickname: &str,
    password: &str,
) -> User {
    let config = test_config();
    let new = NewUser {
        email: Some(format!("{nickname}@example.org")),
        password_hash: Some(
            hash_password(password).expect("password hashing succeeds for fixtures"),
        ),
        is_active: Flag::new(false),
        ..NewUser::local(nickname, &local_ap_id(&config.base_url, nickname))
    };
    insert(pool, &new)
        .await
        .expect("deactivated local user fixture inserts")
}

/// Inserts a local [`User`] named `nickname` that can log in with
/// `password`, has MFA enabled, and returns it beside its TOTP secret
/// and `backup_code_count` issued backup-code plaintexts.
///
/// The state is written straight through [`wired_be::models::user::mfa`]:
/// a confirmed TOTP secret (the switch flipped, exactly what
/// `/api/pleroma/accounts/mfa/confirm/totp` leaves behind) plus the
/// SHA-256 hashes of generated recovery codes. The plaintexts come back
/// because the OAuth MFA tests submit them the way an authenticator app
/// or a printed list would.
///
/// # Panics
///
/// If hashing, the insert or the settings save fails.
pub async fn create_mfa_user_with_password(
    pool: &PgPool,
    nickname: &str,
    password: &str,
    backup_code_count: usize,
) -> (User, String, Vec<String>) {
    let user = create_user_with_password(pool, nickname, password).await;
    let secret = totp::generate_secret();
    let codes = totp::generate_backup_codes(backup_code_count);
    let mut settings = mfa::MfaSettings::default();
    settings.setup_totp(&secret);
    settings.confirm_totp();
    let hashes = codes.iter().map(|code| hash_backup_code(code)).collect();
    settings.rotate_backup_codes(hashes);
    mfa::save(pool, user.id, &settings)
        .await
        .expect("MFA settings save for fixtures");
    (user, secret, codes)
}

/// Registers a test app and mints a user-bound bearer token, both
/// granting `scopes` (one space-separated string,
/// [`Scopes::parse`](wired_be::oauth::scopes::Scopes::parse) form);
/// returns the access token string to send as `Authorization: Bearer`.
///
/// The token carries the default lifetime; tests that need another
/// state (expired, deactivated owner) mint through
/// [`wired_be::models::oauth_token`] themselves and adjust the row.
///
/// # Panics
///
/// If the app or token insert fails.
pub async fn create_app_and_token(pool: &PgPool, user: &User, scopes: &str) -> String {
    let app = oauth_app::create(pool, &test_app_registration(Some(user.id), scopes))
        .await
        .expect("app fixture inserts");
    create_token_for(pool, Some(user.id), app.id, scopes)
        .await
        .token
}

/// Registers a test app and mints an app-only (`client_credentials`)
/// bearer token granting `scopes`; returns the access token string.
///
/// The user-unbound counterpart of [`create_app_and_token`], for the
/// app-only behaviour of the extractors.
///
/// # Panics
///
/// If the app or token insert fails.
pub async fn create_app_only_token(pool: &PgPool, scopes: &str) -> String {
    let app = oauth_app::create(pool, &test_app_registration(None, scopes))
        .await
        .expect("app fixture inserts");
    create_token_for(pool, None, app.id, scopes).await.token
}

/// Registers an app named `name` owned by `owner` (`None` for a
/// user-unbound registration) and returns its row.
///
/// The named counterpart of [`create_app`] and
/// [`create_app_and_token`], for tests that key on the client name or
/// the owner column and not on the fixed `"Test Client"` fixture.
///
/// # Panics
///
/// If the app insert fails.
pub async fn create_app_named(
    pool: &PgPool,
    name: &str,
    owner: Option<&User>,
) -> oauth_app::OauthApp {
    oauth_app::create(
        pool,
        &NewOauthApp {
            client_name: name.to_owned(),
            redirect_uris: TEST_REDIRECT_URIS.to_owned(),
            scopes: "read".to_owned(),
            website: None,
            user_id: owner.map(|found| found.id),
        },
    )
    .await
    .expect("named app fixture inserts")
}

/// Mints a token acting as `user` issued by `app` and granting `scopes`,
/// returning the whole stored row (id, access token and expiry included)
/// for tests that revoke it over HTTP or expire it first.
///
/// # Panics
///
/// If the token insert fails.
pub async fn create_token(
    pool: &PgPool,
    user: &User,
    app: &oauth_app::OauthApp,
    scopes: &str,
) -> oauth_token::OauthToken {
    create_token_for(pool, Some(user.id), app.id, scopes).await
}

/// Moves the token row `id`'s `valid_until` an hour into the past: the
/// state a lapsed token has, which the validity checks refuse.
///
/// # Panics
///
/// If the update fails or hits no row.
pub async fn expire_token(pool: &PgPool, id: Uuid) {
    let outcome = sqlx::query(
        "UPDATE oauth_tokens SET valid_until = now() - interval '1 hour' WHERE id = $1",
    )
    .bind(id)
    .execute(pool)
    .await
    .expect("the expiry update runs");
    assert_eq!(outcome.rows_affected(), 1, "the update hit the token row");
}

/// The [`NewOauthApp`] both token fixtures register: a named test
/// client granted `scopes` on behalf of `user_id`.
fn test_app_registration(user_id: Option<Uuid>, scopes: &str) -> NewOauthApp {
    NewOauthApp {
        client_name: "Test Client".to_owned(),
        redirect_uris: TEST_REDIRECT_URIS.to_owned(),
        scopes: scopes.to_owned(),
        website: None,
        user_id,
    }
}

/// Mints a token for `user_id` (`None` app-only) issued by `app_id`
/// with `scopes` and returns the stored row.
async fn create_token_for(
    pool: &PgPool,
    user_id: Option<Uuid>,
    app_id: Uuid,
    scopes: &str,
) -> oauth_token::OauthToken {
    oauth_token::create_token(
        pool,
        &NewToken {
            user_id,
            app_id,
            scopes: scopes.to_owned(),
            ttl: None,
        },
    )
    .await
    .expect("token fixture inserts")
}
