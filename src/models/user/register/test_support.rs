//! The fixtures more than one `register` child's tests build on: the
//! passwords the registrations submit, the [`AppState`] over a pool with
//! the fixed test configuration, the valid [`RegisterParams`] for a
//! nickname, and the direct insert and reads the login and race tests need
//! to set a row up without going through registration. Test-only.

use std::sync::Arc;

use sqlx::PgPool;

use super::RegisterParams;
use crate::{
    config::{Config, TEST_BASE_URL},
    http_client::HttpClient,
    models::user::{NewUser, User, insert, local_ap_id},
    state::AppState,
    uploads::UploaderBackend,
};

/// The password every registration test submits (24 characters).
pub(super) const PASSWORD: &str = "correct horse battery staple";
/// One character short of the default 8-character minimum.
pub(super) const SHORT_PASSWORD: &str = "short_7";

/// An [`AppState`] over `pool` with the fixed test configuration.
pub(super) fn test_state(pool: &PgPool) -> AppState {
    test_state_with(pool, Config::for_tests())
}

/// An [`AppState`] over `pool` with `config`.
pub(super) fn test_state_with(pool: &PgPool, config: Config) -> AppState {
    let config = Arc::new(config);
    AppState {
        db: pool.clone(),
        hub: crate::streaming::hub::Hub::new(pool),
        config: Arc::clone(&config),
        http_client: HttpClient::from(config.as_ref()),
        uploader: UploaderBackend::from(config.as_ref()),
        instance_stats_cache: Arc::default(),
        mrf_config_cache: Arc::default(),
        translation_languages_cache: Arc::default(),
        rate_limiter: Arc::new(crate::rate_limit::RateLimiter::from_config(config.as_ref())),
        mailer: Arc::new(crate::mailer::MemoryTransport::new()),
    }
}

/// Valid registration params for `nickname` at `nickname@example.org`.
pub(super) fn params_for(nickname: &str) -> RegisterParams {
    RegisterParams {
        nickname: nickname.to_owned(),
        email: Some(format!("{nickname}@example.org")),
        password: PASSWORD.to_owned(),
        name: None,
        bio: None,
        reason: None,
        provider_email_verified: false,
    }
}

/// A local [`NewUser`] named `nickname` on the test instance.
pub(super) fn local_new(nickname: &str) -> NewUser {
    NewUser::local(nickname, &local_ap_id(TEST_BASE_URL, nickname))
}

/// Inserts a local user with `email` and `password_hash` straight
/// through [`insert`] — no keys, no validation, no password work.
pub(super) async fn insert_login(
    pool: &PgPool,
    nickname: &str,
    email: &str,
    password_hash: Option<&str>,
) -> User {
    insert(
        pool,
        &NewUser {
            email: Some(email.to_owned()),
            password_hash: password_hash.map(str::to_owned),
            ..local_new(nickname)
        },
    )
    .await
    .expect("the login fixture inserts")
}

/// The number of rows in `users`.
pub(super) async fn user_count(pool: &PgPool) -> i64 {
    let row: (i64,) = sqlx::query_as("SELECT count(*) FROM users")
        .fetch_one(pool)
        .await
        .expect("users can be counted");
    row.0
}
