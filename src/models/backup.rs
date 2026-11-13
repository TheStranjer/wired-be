//! The `backups` table: the archive exports an account has asked the instance
//! to build — a request first, a stored file later.
//!
//! Reference: Pleroma's `Pleroma.User.Backup`
//! (`lib/pleroma/user/backup.ex`) over the same-named table. The row follows
//! `docs/ARCHITECTURE.md` decisions 2, 3 and 4: one table, one module, the
//! `UUIDv7` this crate mints as its key, and only runtime-checked
//! `sqlx::query_as` statements against it.
//!
//! A row is created empty of substance: [`create`] records that `user` asked
//! for an export, with [`Backup::file_size`] still `0`,
//! [`Backup::processed`] still `false` and [`Backup::storage_key`] still
//! `None`, because no archive exists yet. A queue job (decision 11, a later
//! ticket) is what builds one, and [`mark_processed`] is the write that job
//! lands when it has put the file away: the size it measured, the key it
//! stored it under, and the flag that turns a download link on.
//!
//! Pleroma's schema also carries a `tempdir` column, the directory its worker
//! writes the archive's parts into and zips from. That is deliberately not
//! copied: the working directory belongs to the job and not to the record, so
//! it is made when the job runs and gone when the job ends, and no stale path
//! needs cleaning out of this table. The same reasoning leaves the archive
//! bytes out — this table names where a file is, never what is in it.
//!
//! [`file_name`] is built here rather than at the call site because the name
//! is the storage key's last segment: it must be one
//! [`crate::uploads::check_key`] accepts, so every character outside the key
//! charset (a remote handle's `@`, above all) is replaced here, once, instead
//! of being re-sanitized by whoever happens to ask for an export.
//!
//! [`get_by_id`] is the queue job's read: the `backups.run` handler is
//! handed the request's id by the endpoint and knows nothing else of it
//! before this fetch.
//!
//! Two reads drive the periodic purge a retention window needs: [`latest`]
//! answers the account's newest request (the one to keep, and the one a
//! rate-limit check looks at), and [`expired`] sweeps every row older than
//! the window — processed or not, so an archive whose job died between the
//! request and the upload cannot stand in the table forever.
//!
//! [`delete`] is unscoped by design: the purge job and the admin-facing
//! delete both know a row by id alone and have no account to compare it
//! against, so the ownership check is the caller's to have already made.
//! Deleting a user takes their rows through the owner's cascade.

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{error::AppResult, models::user::User, util::id::new_id};

/// [`create`]: the id this module mints (decision 4), the owner, and the
/// archive name built here; `file_size`, `processed`, `content_type` and
/// `storage_key` are left to their column defaults, because none of what they
/// describe exists yet. `RETURNING *` brings the row back with its stamps.
const INSERT_SQL: &str = "INSERT INTO backups (id, user_id, file_name)
     VALUES ($1, $2, $3)
   RETURNING *";

/// [`list_for_user`]: one account's requests, newest first on the id whose
/// text sorts in creation order (decision 4) — this table's own order, and
/// the `(user_id, id)` index run backwards.
const LIST_SQL: &str = "SELECT * FROM backups WHERE user_id = $1 ORDER BY id DESC";

/// [`latest`]: the same walk cut to its first row, so the account's newest
/// request is read without fetching the history behind it.
const LATEST_SQL: &str = "SELECT * FROM backups WHERE user_id = $1 ORDER BY id DESC LIMIT 1";

/// [`get_by_id`]: one row by its primary key alone — the id is the whole
/// filter because the queue job names nothing else.
const GET_BY_ID_SQL: &str = "SELECT * FROM backups WHERE id = $1";

/// [`mark_processed`]: the job's write — the archive's size and the key it
/// lives under, `processed` flipped, and the `updated_at` bump every rewrite
/// of a row carries. An id no row carries rewrites nothing, so
/// `RETURNING *` is also the existence check.
const MARK_PROCESSED_SQL: &str = "UPDATE backups
      SET processed = true,
          file_size = $2,
          storage_key = $3,
          updated_at = now()
    WHERE id = $1
 RETURNING *";

/// [`expired`]: every row whose `inserted_at` is strictly older than
/// `now - window`, in creation order. Both states are swept — a processed
/// archive past its shelf life and an unprocessed request whose job died are
/// the same leak if only one of them is collected.
const EXPIRED_SQL: &str = "SELECT * FROM backups
      WHERE inserted_at < $1 - $2::interval
   ORDER BY id";

/// [`delete`]: one row by id alone, `RETURNING *` so the caller can report
/// what went and tell a real delete from a miss; see the module docs for why
/// no owner is part of the `WHERE` clause.
const DELETE_SQL: &str = "DELETE FROM backups WHERE id = $1 RETURNING *";

/// Whether `c` may stand in an archive name: the characters
/// [`crate::uploads::check_key`] accepts, minus `/`, since the name is one
/// path segment and never a path of its own.
const fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')
}

/// The archive name for `nickname`'s export taken at `at`:
/// `archive-<nickname>-<YYYYMMDDHHMMSS>.zip`, Pleroma's
/// `archive-<nickname>-<basic iso8601>.zip`.
///
/// Every character outside `[A-Za-z0-9._-]` becomes `_`. A remote handle's
/// `@` is the one that matters: the name is also the last segment of the
/// storage key, and [`crate::uploads::check_key`] does not carry `@`, so an
/// unsanitized name would fail the guard on its way into storage. The stamp
/// keeps the name unique per account and second, and it is UTC: `at` is a
/// `DateTime<Utc>`, so the digits are the same whoever reads them.
#[must_use]
pub fn file_name(nickname: &str, at: DateTime<Utc>) -> String {
    let safe: String = nickname
        .chars()
        .map(|c| if is_name_char(c) { c } else { '_' })
        .collect();
    let stamp = at.format("%Y%m%d%H%M%S");
    format!("archive-{safe}-{stamp}.zip")
}

/// One row of `backups`: one account's request for an export archive, and —
/// once the job has run — the record of the file it made.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct Backup {
    /// `UUIDv7` primary key, minted by [`create`] (decision 4); also the
    /// newest-first order [`list_for_user`] and [`latest`] read in.
    pub id: Uuid,
    /// The owning account; its exports are its own, and deleting it deletes
    /// these rows.
    pub user_id: Uuid,
    /// The archive's name, built by [`file_name`] from the owner's nickname
    /// and the moment of the request.
    pub file_name: String,
    /// The archive's size in bytes: `0` until [`mark_processed`] records the
    /// file the job wrote.
    pub file_size: i64,
    /// Whether the archive has been built and stored — the flag a download
    /// link is gated on.
    pub processed: bool,
    /// What the archive is served as; every archive is a zip, so the column
    /// defaults to `application/zip`.
    pub content_type: String,
    /// The storage key the finished archive lives under, `None` while the
    /// request is still waiting for its job.
    pub storage_key: Option<String>,
    /// When the request was made, and the moment the retention window in
    /// [`expired`] is measured from.
    pub inserted_at: DateTime<Utc>,
    /// When the row last changed: [`create`]'s equal until [`mark_processed`]
    /// lands the archive.
    pub updated_at: DateTime<Utc>,
}

/// Records that `user` has asked for an export archive and returns the stored
/// row.
///
/// The primary key is minted here (decision 4) and the name is built here from
/// the account's nickname and the current moment (see [`file_name`]); every
/// other column keeps its default, because the archive does not exist yet —
/// [`Backup::processed`] says so until [`mark_processed`] flips it.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn create(pool: &PgPool, user: &User) -> AppResult<Backup> {
    let row = sqlx::query_as::<_, Backup>(INSERT_SQL)
        .bind(new_id())
        .bind(user.id)
        .bind(file_name(&user.nickname, Utc::now()))
        .fetch_one(pool)
        .await?;
    Ok(row)
}

/// The backups of `user_id`, newest first (decision 4) — the history an
/// account is shown, most recent export on top.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn list_for_user(pool: &PgPool, user_id: Uuid) -> AppResult<Vec<Backup>> {
    let rows = sqlx::query_as::<_, Backup>(LIST_SQL)
        .bind(user_id)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// The newest backup of `user_id`, `None` when the account has never asked
/// for one.
///
/// This is the row a rate-limit check reads — how long ago the last export
/// was — and the one a purge keeps while deleting what is behind it, which is
/// why the ordering is the id's and not the timestamp's: `UUIDv7` text is
/// creation order (decision 4), so the newest request wins outright.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn latest(pool: &PgPool, user_id: Uuid) -> AppResult<Option<Backup>> {
    let row = sqlx::query_as::<_, Backup>(LATEST_SQL)
        .bind(user_id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// The backup `id` names, `None` when no row carries it.
///
/// This is the run job's read: the endpoint enqueues the row's id and the
/// handler knows the request by nothing else, so the fetch doubles as its
/// existence check — `None` is the account's own delete having won the
/// race, not a failure. Like [`delete`], it carries no owner filter: the
/// job was handed this id by the endpoint that recorded it.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_by_id(pool: &PgPool, id: Uuid) -> AppResult<Option<Backup>> {
    let row = sqlx::query_as::<_, Backup>(GET_BY_ID_SQL)
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// Records the archive the job built: `file_size` bytes stored under
/// `storage_key`, with `processed` flipped and `updated_at` bumped — the one
/// write that turns a request into a downloadable backup.
///
/// Returns the rewritten row, `None` when `id` names no row (one deleted
/// under the job's feet, which is the account's own delete winning the race).
/// The write is not owner-scoped, like [`delete`]: the job knows the row it
/// was handed.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn mark_processed(
    pool: &PgPool,
    id: Uuid,
    file_size: i64,
    storage_key: &str,
) -> AppResult<Option<Backup>> {
    let row = sqlx::query_as::<_, Backup>(MARK_PROCESSED_SQL)
        .bind(id)
        .bind(file_size)
        .bind(storage_key)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// Every backup older than `validity_days` days as seen from `now`, in
/// creation order — the purge sweep's input.
///
/// The comparison is `inserted_at < now - window`, so a row exactly
/// `validity_days` old is still within its shelf life and the second after
/// that boundary is not. Both states are collected: an archive that was never
/// processed has no file to delete but a row that must not stand here past
/// the window either, which is what keeps a dead job from leaking a request
/// forever.
///
/// The window is applied in SQL against the caller's `now`, so a sweep run on
/// one process ages rows by the same clock the row was stamped with.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn expired(
    pool: &PgPool,
    now: DateTime<Utc>,
    validity_days: usize,
) -> AppResult<Vec<Backup>> {
    // A count of days too large for the interval type is a window that never
    // closes: nothing is old enough to be swept, which is the safe reading of
    // an absurd retention setting.
    let days = i64::try_from(validity_days).unwrap_or(i64::MAX);
    let window = ChronoDuration::try_days(days).unwrap_or(ChronoDuration::MAX);
    let rows = sqlx::query_as::<_, Backup>(EXPIRED_SQL)
        .bind(now)
        .bind(window)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// Deletes the backup `id` and returns the row that went; `None` when no row
/// carries it.
///
/// Any stored archive the row named is removed by the caller — the storage
/// layer, not this table, holds the file, and a purge that deletes the row
/// first would leave the bytes unreachable. See the module docs for why `id`
/// is the whole filter.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn delete(pool: &PgPool, id: Uuid) -> AppResult<Option<Backup>> {
    let row = sqlx::query_as::<_, Backup>(DELETE_SQL)
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

#[cfg(test)]
mod tests {
    use std::{thread, time::Duration};

    use chrono::{SubsecRound as _, TimeZone as _};

    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        models::user::{NewUser, insert as insert_user, local_ap_id},
        uploads,
    };

    /// The instance origin the fixtures post under.
    const BASE: &str = TEST_BASE_URL;

    /// How far apart two fixtures must be made for their `UUIDv7`s to order
    /// by creation (the ids are stamped to the millisecond, decision 4).
    const ORDER_GAP: Duration = Duration::from_millis(2);

    /// The retention window the expiry tests use, in days.
    const DAYS: i64 = 30;

    /// A local user named `nickname`.
    async fn user(pool: &PgPool, nickname: &str) -> User {
        insert_user(
            pool,
            &NewUser::local(nickname, &local_ap_id(BASE, nickname)),
        )
        .await
        .expect("the fixture user inserts")
    }

    /// A backup requested by `owner`.
    async fn seed(pool: &PgPool, owner: &User) -> Backup {
        create(pool, owner)
            .await
            .expect("the fixture backup creates")
    }

    /// Moves `id`'s `inserted_at` to `at`, so an expiry test is not a race
    /// with the clock. Asserts the row was there to move.
    async fn rewind(pool: &PgPool, id: Uuid, at: DateTime<Utc>) {
        let moved = sqlx::query("UPDATE backups SET inserted_at = $1 WHERE id = $2")
            .bind(at)
            .bind(id)
            .execute(pool)
            .await
            .expect("the fixture row ages")
            .rows_affected();
        assert_eq!(moved, 1, "the row to rewind exists");
    }

    /// The ids of `rows`, for a set-like comparison.
    fn ids(rows: &[Backup]) -> Vec<Uuid> {
        rows.iter().map(|row| row.id).collect()
    }

    /// `validity_days` as the `usize` [`expired`] takes, beside the
    /// [`ChronoDuration`] the same window is rewound by.
    fn window() -> usize {
        usize::try_from(DAYS).expect("thirty days is a usize count")
    }

    #[tokio::test]
    async fn create_stores_a_request_with_nothing_behind_it() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;

        let stored = create(&pool, &alice)
            .await
            .expect("a backup request creates");

        assert!(stored.id.get_version_num() == 7, "the id is a UUIDv7");
        assert_eq!(stored.user_id, alice.id, "the owner is the user");
        assert_eq!(stored.file_size, 0, "no archive has been written");
        assert!(!stored.processed, "and the request is unprocessed");
        assert_eq!(
            stored.content_type, "application/zip",
            "the column default is the type an archive is served as"
        );
        assert_eq!(stored.storage_key, None, "and nothing is stored yet");
        assert_eq!(
            stored.inserted_at, stored.updated_at,
            "a fresh row is stamped once"
        );

        // The name is this account's, stamped to the second: the request's own
        // moment is the database's `now()`, a hair before the row came back,
        // so the stamp is checked by shape rather than rebuilt.
        let stamp = stored
            .file_name
            .strip_prefix("archive-alice-")
            .and_then(|rest| rest.strip_suffix(".zip"))
            .expect("the name is alice's archive");
        assert_eq!(stamp.len(), 14, "the stamp is YYYYMMDDHHMMSS");
        assert!(
            stamp.chars().all(|c| c.is_ascii_digit()),
            "and it is all digits, not {stamp}"
        );
        uploads::check_key(&stored.file_name).expect("a generated name is a legal key");
    }

    #[tokio::test]
    async fn list_is_newest_first_and_the_owners_own() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;

        let first = seed(&pool, &alice).await;
        thread::sleep(ORDER_GAP);
        let second = seed(&pool, &alice).await;
        thread::sleep(ORDER_GAP);
        let theirs = seed(&pool, &bob).await;

        let listed = list_for_user(&pool, alice.id)
            .await
            .expect("the list reads");
        assert_eq!(
            listed,
            vec![second.clone(), first.clone()],
            "alice's two requests, newest first"
        );
        assert_eq!(theirs.user_id, bob.id, "and bob's was there to be missed");
        assert!(
            list_for_user(&pool, user(&pool, "lonely").await.id)
                .await
                .expect("an account with no requests reads")
                .is_empty(),
            "nobody's history is the empty list"
        );
    }

    #[tokio::test]
    async fn latest_answers_the_newest_of_the_owners_own() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let carol = user(&pool, "carol").await;

        assert!(
            latest(&pool, alice.id)
                .await
                .expect("an account with no requests reads")
                .is_none(),
            "never asked is no row"
        );

        let older = seed(&pool, &alice).await;
        thread::sleep(ORDER_GAP);
        let newer = seed(&pool, &alice).await;
        // Bob's is the newest row in the table and still not Alice's answer.
        thread::sleep(ORDER_GAP);
        let theirs = seed(&pool, &bob).await;

        assert_eq!(
            latest(&pool, alice.id).await.expect("alice's newest reads"),
            Some(newer.clone()),
            "her own newest, not the older one"
        );
        // Both rows are alice's; only the newest is her answer.
        assert_eq!(
            list_for_user(&pool, alice.id)
                .await
                .expect("her history reads"),
            vec![newer.clone(), older],
            "the older request stands behind it"
        );
        assert_eq!(
            latest(&pool, bob.id).await.expect("bob's newest reads"),
            Some(theirs),
            "and each owner gets their own"
        );
        assert!(
            latest(&pool, carol.id)
                .await
                .expect("carol reads")
                .is_none(),
            "a third account still has nothing"
        );
        assert_eq!(
            latest(&pool, alice.id)
                .await
                .expect("the answer repeats")
                .expect("alice has a row"),
            newer,
            "and it is stable"
        );
    }

    #[tokio::test]
    async fn get_by_id_answers_the_row_by_its_id_alone() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let row = seed(&pool, &alice).await;

        assert_eq!(
            get_by_id(&pool, row.id).await.expect("the job's read runs"),
            Some(row.clone()),
            "the id alone finds the request"
        );

        let marked = mark_processed(&pool, row.id, 256, "backups/alice.zip")
            .await
            .expect("the mark runs")
            .expect("the row is there");
        assert_eq!(
            get_by_id(&pool, row.id)
                .await
                .expect("the second read runs"),
            Some(marked),
            "and it reads the marked row, not a stale copy"
        );

        assert!(
            get_by_id(&pool, Uuid::now_v7())
                .await
                .expect("an unknown id is not a failure")
                .is_none(),
            "an id nothing carries answers None"
        );
    }

    #[tokio::test]
    async fn mark_processed_stores_the_archive_and_returns_the_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let stored = seed(&pool, &alice).await;
        let key = format!("backups/{}", stored.file_name);

        let marked = mark_processed(&pool, stored.id, 4_096, &key)
            .await
            .expect("the mark runs")
            .expect("the requested row is there");

        assert_eq!(marked.id, stored.id, "the row is rewritten in place");
        assert_eq!(marked.user_id, alice.id, "and stays owned");
        assert!(marked.processed, "the flag flips");
        assert_eq!(marked.file_size, 4_096, "the measured size is stored");
        assert_eq!(
            marked.storage_key.as_deref(),
            Some(key.as_str()),
            "so is the key it lives under"
        );
        assert_eq!(
            marked.content_type, stored.content_type,
            "the type the row was born with stands"
        );
        assert_eq!(marked.file_name, stored.file_name, "and so does the name");
        assert_eq!(marked.inserted_at, stored.inserted_at, "stays original");
        assert!(
            marked.updated_at >= stored.updated_at,
            "the update stamp moves forward"
        );
        uploads::check_key(
            marked
                .storage_key
                .as_deref()
                .expect("the marked row has a key"),
        )
        .expect("and the stored key is legal");

        assert_eq!(
            list_for_user(&pool, alice.id)
                .await
                .expect("the history reads"),
            vec![marked],
            "the read-back row is the marked one"
        );

        assert!(
            mark_processed(&pool, Uuid::now_v7(), 1, "backups/missing.zip")
                .await
                .expect("marking nothing is not a failure")
                .is_none(),
            "an unknown id rewrites nothing"
        );
    }

    #[tokio::test]
    async fn expired_sweeps_only_rows_past_their_validity() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let now = Utc::now().trunc_subsecs(6);
        let span = ChronoDuration::days(DAYS);

        // Exactly at the boundary, one second past it (processed, as a real
        // archive would be), a fresh request, and an old unprocessed one whose
        // job never finished.
        let exact = seed(&pool, &alice).await;
        rewind(&pool, exact.id, now - span).await;

        thread::sleep(ORDER_GAP);
        let lapsed = seed(&pool, &alice).await;
        rewind(&pool, lapsed.id, now - span - ChronoDuration::seconds(1)).await;
        let lapsed = mark_processed(&pool, lapsed.id, 512, "backups/lapsed.zip")
            .await
            .expect("the boundary row marks")
            .expect("it exists");

        thread::sleep(ORDER_GAP);
        let fresh = seed(&pool, &alice).await;

        thread::sleep(ORDER_GAP);
        let abandoned = seed(&pool, &alice).await;
        rewind(&pool, abandoned.id, now - span - ChronoDuration::days(1)).await;

        let found = expired(&pool, now, window())
            .await
            .expect("the sweep reads");
        assert_eq!(ids(&found).len(), 2, "two rows are past the window");
        assert!(
            found.contains(&lapsed),
            "the processed archive, one second over, with its file recorded"
        );
        assert!(
            found.iter().any(|row| row.id == abandoned.id),
            "and the request whose job never ran"
        );
        assert!(
            found.iter().all(|row| row.id != exact.id),
            "exactly {DAYS} days old is still inside its shelf life"
        );
        assert!(
            found.iter().all(|row| row.id != fresh.id),
            "and a fresh request is not the purge's business"
        );
        let swept = ids(&found);
        let mut ascending = swept.clone();
        ascending.sort();
        assert_eq!(swept, ascending, "the sweep is in creation order");

        // The cutoff belongs to the `now` that was passed in, not to the
        // database clock: asked a year later, every row above is expired.
        let later = expired(&pool, now + ChronoDuration::days(365), window())
            .await
            .expect("the second sweep reads");
        assert_eq!(
            ids(&later),
            vec![exact.id, lapsed.id, fresh.id, abandoned.id],
            "all four, oldest first"
        );
    }

    #[tokio::test]
    async fn delete_answers_the_row_once() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let hers = seed(&pool, &alice).await;

        let gone = delete(&pool, hers.id)
            .await
            .expect("the row deletes")
            .expect("the first delete finds it");
        assert_eq!(gone, hers, "the answer is the row that went");
        assert!(
            delete(&pool, hers.id)
                .await
                .expect("deleting nothing is not a failure")
                .is_none(),
            "the second finds nothing"
        );
        assert!(
            list_for_user(&pool, alice.id)
                .await
                .expect("the history reads")
                .is_empty(),
            "and the row is gone from it"
        );
        assert!(
            delete(&pool, Uuid::now_v7())
                .await
                .expect("an unknown id is not a failure")
                .is_none(),
            "an id nothing carries deletes nothing"
        );
    }

    #[tokio::test]
    async fn delete_takes_another_accounts_row_by_id() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let bob = user(&pool, "bob").await;
        let his = seed(&pool, &bob).await;

        // The purge job and the admin's delete know a row by id alone, so
        // `delete` carries no owner check: it is the caller's to have made.
        assert_eq!(
            delete(&pool, his.id)
                .await
                .expect("the id deletes")
                .expect("the row was there"),
            his,
            "regardless of whose row it is"
        );
    }

    #[test]
    fn file_name_is_pleromas_archive_name() {
        let at = Utc.with_ymd_and_hms(2026, 11, 13, 7, 29, 1).unwrap();
        assert_eq!(
            file_name("alice", at),
            "archive-alice-20261113072901.zip",
            "archive-<nickname>-<basic iso8601>.zip"
        );
        assert_eq!(
            file_name("a-Z_0.9", at),
            "archive-a-Z_0.9-20261113072901.zip",
            "the key charset survives untouched"
        );
    }

    #[test]
    fn file_name_replaces_what_the_key_charset_excludes() {
        let at = Utc.with_ymd_and_hms(2026, 12, 31, 23, 59, 59).unwrap();

        assert_eq!(
            file_name("alice@remote.example", at),
            "archive-alice_remote.example-20261231235959.zip",
            "a remote handle's `@` becomes `_`, its dots stay"
        );
        assert_eq!(
            file_name("üni@coe", at),
            "archive-_ni_coe-20261231235959.zip",
            "one underscore per character, not per byte"
        );
        assert_eq!(
            file_name("a b/c?d", at),
            "archive-a_b_c_d-20261231235959.zip",
            "spaces too, and a `/` never survives to make a path"
        );

        for nickname in ["", "alice", "ünïcödé🙂", "..", "x/../y", "quote'\""] {
            let name = file_name(nickname, at);
            uploads::check_key(&name).unwrap_or_else(|err| {
                panic!("the name of {nickname:?} must be a legal key: {name} — {err}")
            });
            assert!(
                !name.contains('/'),
                "and no name carries a separator itself: {name}"
            );
        }
    }

    #[tokio::test]
    async fn deleting_a_user_cascades_their_backups() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let _hers = seed(&pool, &alice).await;
        let _his = seed(&pool, &bob).await;

        let erased = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(alice.id)
            .execute(&pool)
            .await
            .expect("the fixture user deletes")
            .rows_affected();
        assert_eq!(erased, 1, "the owner was there to delete");
        assert!(
            list_for_user(&pool, alice.id)
                .await
                .expect("the erased account reads")
                .is_empty(),
            "and the cascade took her requests"
        );
        assert_eq!(
            list_for_user(&pool, bob.id)
                .await
                .expect("bob's history reads")
                .len(),
            1,
            "while his stands"
        );
    }
}
