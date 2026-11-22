//! Fixtures the test database unit tests share: the leased fingerprint, the
//! marker rows and the lease-holder lookups.

use std::time::Duration;

use sqlx::{Connection as _, PgConnection, PgPool};
use uuid::Uuid;

use super::{SLOT_LOCK_CLASS, maintenance_options, template::migrator_fingerprint};

/// How long a released lease may take to disappear from `pg_locks`.
pub(super) const LEASE_RELEASE_TIMEOUT: Duration = Duration::from_secs(5);
/// The pause between two looks at `pg_locks`.
pub(super) const LEASE_POLL: Duration = Duration::from_millis(20);

/// The fingerprint the leases in this process are named for: the embedded
/// migrations under the role `DATABASE_URL` connects as.
pub(super) fn leased_fingerprint() -> String {
    let options = maintenance_options().expect("DATABASE_URL is set for the test suite");
    migrator_fingerprint(options.get_username())
}

/// The name of the database `pool` is connected to.
pub(super) async fn database_of(pool: &PgPool) -> String {
    sqlx::query_scalar::<_, String>("SELECT current_database()")
        .fetch_one(pool)
        .await
        .expect("the slot answers its own name")
}

/// The slot number a slot database name ends with.
pub(super) fn slot_of(database: &str) -> i32 {
    database
        .rsplit_once('_')
        .and_then(|(_fp, slot)| slot.parse::<i32>().ok())
        .unwrap_or_else(|| panic!("{database} is not a slot database name"))
}

/// Puts a `hashtags` row named `tag` into `pool`: the marker a reset has
/// to clear.
pub(super) async fn mark(pool: &PgPool, tag: &str) {
    let _inserted = sqlx::query("INSERT INTO hashtags (id, name) VALUES ($1, $2)")
        .bind(Uuid::now_v7())
        .bind(tag)
        .execute(pool)
        .await
        .expect("the marker tag inserts");
}

/// Whether the `hashtags` row named `tag` is still in `pool`.
pub(super) async fn marked(pool: &PgPool, tag: &str) -> bool {
    sqlx::query_scalar::<_, bool>("SELECT EXISTS (SELECT 1 FROM hashtags WHERE name = $1)")
        .bind(tag)
        .fetch_one(pool)
        .await
        .expect("the marker tag is looked up")
}

/// The next `list_members.position` value: one while the identities are at
/// their start, one more every time a test takes a number from them.
pub(super) async fn next_position(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT nextval(pg_get_serial_sequence('public.list_members', 'position'))")
        .fetch_one(pool)
        .await
        .expect("the position sequence answers")
}

/// A connection to the maintenance database, for looking at `pg_locks`.
pub(super) async fn maintenance_conn() -> PgConnection {
    let options = maintenance_options().expect("DATABASE_URL is set for the test suite");
    PgConnection::connect_with(&options)
        .await
        .expect("the maintenance database answers")
}

/// The sessions holding the lease of slot `slot`, as `conn` sees them.
pub(super) async fn holders(conn: &mut PgConnection, slot: i32) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM pg_locks \
          WHERE locktype = 'advisory' \
            AND classid::bigint = $1 AND objid::bigint = $2",
    )
    .bind(i64::from(SLOT_LOCK_CLASS))
    .bind(i64::from(slot))
    .fetch_one(conn)
    .await
    .expect("pg_locks answers")
}

/// Waits until no session holds the lease of slot `slot` any more, which
/// the server does as soon as the connection that took it is closed.
pub(super) async fn lease_released(slot: i32) -> bool {
    let mut conn = maintenance_conn().await;
    let started = std::time::Instant::now();
    loop {
        if holders(&mut conn, slot).await == 0 {
            return true;
        }
        if started.elapsed() >= LEASE_RELEASE_TIMEOUT {
            return false;
        }
        tokio::time::sleep(LEASE_POLL).await;
    }
}
