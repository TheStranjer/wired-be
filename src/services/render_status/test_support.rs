//! The fixtures the `render_status` unit tests share: the instance origin
//! the posts live under, the state under test, a plain local account and
//! a plain local post. Test-only.

use std::sync::Arc;

use sqlx::PgPool;

use crate::{
    config::{Config, TEST_BASE_URL},
    http_client::HttpClient,
    mailer::MemoryTransport,
    models::{
        status::{NewStatus, Status as StatusRow, insert},
        user::{self, NewUser, User, local_ap_id},
    },
    state::AppState,
    uploads::UploaderBackend,
};

/// The instance origin the fixtures post under.
pub(super) const BASE: &str = TEST_BASE_URL;

/// The state under test: `pool` and the test config, with the
/// collaborators the renderer never touches.
pub(super) fn test_state(pool: &PgPool) -> AppState {
    let config = Arc::new(Config::for_tests());
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
        mailer: Arc::new(MemoryTransport::new()),
    }
}

/// Inserts a local user named `nickname`.
pub(super) async fn user(pool: &PgPool, nickname: &str) -> User {
    user::insert(
        pool,
        &NewUser::local(nickname, &local_ap_id(BASE, nickname)),
    )
    .await
    .expect("the fixture user inserts")
}

/// Inserts `text` as a local public status of `author`.
pub(super) async fn post(pool: &PgPool, author: &User, text: &str) -> StatusRow {
    insert(pool, BASE, &NewStatus::local(author, text))
        .await
        .expect("a plain local post inserts")
}
