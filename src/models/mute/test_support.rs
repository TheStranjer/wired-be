//! Fixtures the mute model tests share: users, seeded mutes and the
//! reads that check what the table holds.

use sqlx::{Error as SqlxError, PgPool};
use uuid::Uuid;

use super::*;
use crate::{
    pagination::{MAX_LIMIT, PageParams},
    util::id::new_id,
};

pub(super) use crate::test_fixtures::local_user as user;

/// The options of the timeline-only mute the notification checks use.
pub(super) const TIMELINE_ONLY: MuteOptions = MuteOptions {
    notifications: false,
    expires_in: None,
};

/// Mutes `target` for `muter` with `options`, checks the row it wrote and
/// hands it back: the fixture a read below starts from and asserts on.
pub(super) async fn write_mute(
    pool: &PgPool,
    muter: &User,
    target: &User,
    options: MuteOptions,
) -> Mute {
    let row = mute(pool, muter, target, options)
        .await
        .expect("the fixture mute stores");
    assert_eq!(
        row.muted_id, target.id,
        "the row is the pair that was asked for"
    );
    row
}

/// Mutes `target` for `muter` with `options` and forgets the row: the
/// fixture a test only needs to have happened.
pub(super) async fn seed_mute(pool: &PgPool, muter: &User, target: &User, options: MuteOptions) {
    let row = write_mute(pool, muter, target, options).await;
    assert!(
        is_muting(pool, muter.id, row.muted_id).await.unwrap(),
        "the fixture mute reads back as a mute"
    );
}

/// The stored mute of the pair, which the test expects to exist.
pub(super) async fn stored_mute(pool: &PgPool, muter: &User, target: &User) -> Mute {
    let sql = "SELECT * FROM mutes WHERE muter_id = $1 AND muted_id = $2";
    sqlx::query_as::<_, Mute>(sql)
        .bind(muter.id)
        .bind(target.id)
        .fetch_optional(pool)
        .await
        .expect("the read works")
        .expect("the mute exists")
}

/// Writes `target` into `muter`'s mute list with `expires_at` set to
/// `seconds` from now (negative for the past), bypassing [`mute`]: the
/// state the sweep and the expiry filters start from, which [`mute`] cannot
/// produce because it always computes the column from the write's `now()`.
pub(super) async fn mute_expiring_in(pool: &PgPool, muter: &User, target: &User, seconds: i64) {
    let sql = "INSERT INTO mutes (id, muter_id, muted_id, expires_at)
                VALUES ($1, $2, $3, now() + make_interval(secs => $4))";
    let written = sqlx::query(sql)
        .bind(new_id())
        .bind(muter.id)
        .bind(target.id)
        .bind(seconds)
        .execute(pool)
        .await
        .expect("the lapsed fixture row inserts");
    assert_eq!(written.rows_affected(), 1, "one row was written");
}

/// Every row of the table, newest mute first — the order a page walks,
/// read straight from storage so same-millisecond `UUIDv7`s cannot make an
/// expectation depend on the order the fixtures were written in.
pub(super) async fn stored_rows(pool: &PgPool) -> Vec<(Uuid, Uuid, Uuid)> {
    sqlx::query_as::<_, (Uuid, Uuid, Uuid)>(
        "SELECT id, muter_id, muted_id FROM mutes ORDER BY id DESC",
    )
    .fetch_all(pool)
    .await
    .expect("the rows read")
}

/// The number of rows in the table.
pub(super) async fn mute_count(pool: &PgPool) -> i64 {
    let row: (i64,) = sqlx::query_as("SELECT count(*) FROM mutes")
        .fetch_one(pool)
        .await
        .unwrap();
    row.0
}

/// Page params built from a query string, the way the extractor makes them.
pub(super) fn page(query: &str) -> PageParams {
    PageParams::from_query(Some(query), MAX_LIMIT).expect("the fixture page parses")
}

/// How many seconds ahead of `now()` a stored `expires_at` lies, refusing
/// to answer for a mute that has none: the write takes its own instant, so
/// a test asserts a window and not an exact timestamp.
pub(super) fn seconds_ahead(row: &Mute) -> i64 {
    let expiry = row.expires_at.expect("a timed mute stores an expiry");
    expiry.signed_duration_since(Utc::now()).num_seconds()
}

/// The muted ids of `entries`, in the order they came back.
pub(super) fn muted_ids_of(entries: &[(Mute, User)]) -> Vec<Uuid> {
    entries.iter().map(|(_, user)| user.id).collect()
}

/// The mute row ids of `entries`, in the order they came back.
pub(super) fn row_ids_of(entries: &[(Mute, User)]) -> Vec<Uuid> {
    entries.iter().map(|(row, _)| row.id).collect()
}

/// The SQLSTATE a database failure carries, or an empty string when the
/// failure is not the database reporting a constraint.
pub(super) fn sqlstate_of(err: SqlxError) -> String {
    if let SqlxError::Database(db_err) = err {
        return db_err.code().unwrap_or_default().into_owned();
    }
    String::new()
}
