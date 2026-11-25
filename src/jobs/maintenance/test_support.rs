//! The fixtures the maintenance sweeps' unit tests share: the test state the
//! handlers run against (whole, config-overridden, and one whose database
//! cannot be reached), the local user a swept row needs to exist, the reader
//! that answers whether a row survived its sweep, and the schedule periods and
//! attempt info the registry tests assert on. Test-only.

use std::time::Duration;

use sqlx::{PgPool, query_scalar};
use uuid::Uuid;

use crate::{config::Config, jobs::AttemptInfo, state::AppState};

// Inserts a local user named `nickname`.
pub(super) use crate::test_fixtures::local_user as user;

/// The period the notification sweep is scheduled on: one day.
pub(super) const EVERY: Duration = Duration::from_hours(24);

/// The period the token sweep is scheduled on: one hour.
pub(super) const HOURLY: Duration = Duration::from_secs(3_600);

/// The period the orphaned-media sweep is scheduled on: six hours.
pub(super) const SIX_HOURS: Duration = Duration::from_secs(21_600);

/// One day in the seconds the fixture helpers count ages in.
pub(super) const DAY: i64 = 86_400;

/// Test state over `pool` with an explicit `config`, for handlers whose
/// work reads the configuration — the token sweep's reset window.
pub(super) fn state_with_config(pool: &PgPool, config: Config) -> AppState {
    AppState::for_tests(pool.clone(), config)
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
