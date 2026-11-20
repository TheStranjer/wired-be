//! The fixtures the maintenance sweeps' unit tests share: the test state the
//! handlers run against (whole, config-overridden, and one whose database
//! cannot be reached), the local user a swept row needs to exist, the reader
//! that answers whether a row survived its sweep, and the schedule periods and
//! attempt info the registry tests assert on. Test-only.

use std::{sync::Arc, time::Duration};

use sqlx::{PgPool, postgres::PgPoolOptions, query_scalar};
use uuid::Uuid;

use crate::{
    config::{Config, TEST_BASE_URL},
    http_client::HttpClient,
    jobs::AttemptInfo,
    models::user::{NewUser, User, insert, local_ap_id},
    state::AppState,
    streaming::hub::Hub,
    uploads::UploaderBackend,
};

/// The period the notification sweep is scheduled on: one day.
pub(super) const EVERY: Duration = Duration::from_hours(24);

/// The period the token sweep is scheduled on: one hour.
pub(super) const HOURLY: Duration = Duration::from_secs(3_600);

/// The period the orphaned-media sweep is scheduled on: six hours.
pub(super) const SIX_HOURS: Duration = Duration::from_secs(21_600);

/// One day in the seconds the fixture helpers count ages in.
pub(super) const DAY: i64 = 86_400;

/// Test state over `pool` with the fixed test config.
pub(super) fn app_state(pool: &PgPool) -> AppState {
    state_with_config(pool, Config::for_tests())
}

/// Test state over `pool` with an explicit `config`, for handlers whose
/// work reads the configuration — the token sweep's reset window.
pub(super) fn state_with_config(pool: &PgPool, config: Config) -> AppState {
    let config = Arc::new(config);
    AppState {
        db: pool.clone(),
        hub: Hub::new(pool),
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

/// Test state over a pool that cannot answer: closed before it ever
/// connected, which is how a handler sees a database that went away.
pub(super) async fn broken_state() -> AppState {
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://localhost/unused")
        .expect("a lazy pool needs no database");
    pool.close().await;
    app_state(&pool)
}

/// Inserts a local user named `nickname`.
pub(super) async fn user(pool: &PgPool, nickname: &str) -> User {
    insert(
        pool,
        &NewUser::local(nickname, &local_ap_id(TEST_BASE_URL, nickname)),
    )
    .await
    .expect("the local fixture inserts")
}

/// Whether `table` — one of the swept tables, named by the test — still
/// holds the row with primary key `id`. The table names are test
/// constants, never user input, so the dynamic statement is asserted
/// safe (the same bypass `src/test_db.rs` documents).
pub(super) async fn row_exists(pool: &PgPool, table: &str, id: Uuid) -> bool {
    let rows = query_scalar::<_, i64>(sqlx::AssertSqlSafe(format!(
        "SELECT count(*) FROM {table} WHERE id = $1"
    )))
    .bind(id)
    .fetch_one(pool)
    .await
    .expect("the row count reads");
    rows == 1
}

/// The handler's attempt info: the first of however many the sweep gets,
/// since its work does not depend on which pass it is.
pub(super) const FIRST_ATTEMPT: AttemptInfo = AttemptInfo {
    attempts: 1,
    max_attempts: 5,
};
