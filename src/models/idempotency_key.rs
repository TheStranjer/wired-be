//! The `idempotency_keys` table: the `(account, key)` → status mapping the
//! `Idempotency-Key` header of `POST /api/v1/statuses` replays a create
//! through.
//!
//! The table is this instance's durable stand-in for Pleroma's
//! `Pleroma.Web.Plugs.IdempotencyPlug`
//! (`lib/pleroma/web/plugs/idempotency_plug.ex`) Cachex cache
//! (docs/ARCHITECTURE.md decision 11 keeps shared state in Postgres): one
//! row says "this account's create under this key produced this status", and
//! the create path consults it before it writes and claims a row for it
//! inside the status's own transaction. The row's two foreign keys cascade,
//! so a hard-deleted user or status retires the mapping with it; a *soft*-
//! deleted status keeps its row, and [`live_status`] is what hides that
//! tombstone from the replay — the key then falls back to creating afresh,
//! and [`reassign`] moves the row onto the new status instead of leaving it
//! to conflict forever.
//!
//! Nothing here knows about HTTP: the header's syntax (one value, at most
//! 255 characters) is the endpoint's reading, and the 24-hour retention is
//! the [`idempotency.cleanup`](crate::jobs::idempotency::KIND_CLEANUP) job's
//! schedule; this module only stores, claims, moves and deletes the rows.

use chrono::TimeDelta;
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::{error::AppResult, models::status::Status};

/// [`live_status`] and [`live_status_conn`]: the status the key created,
/// joined only when that row is still live. The join is what makes a
/// tombstoned status "no longer exists" for the replay (see [`reassign`]).
const LIVE_SQL: &str = "SELECT s.* FROM idempotency_keys ik
     JOIN statuses s ON s.id = ik.status_id AND s.deleted_at IS NULL
    WHERE ik.user_id = $1 AND ik.key = $2";

/// [`claim`]: insert the mapping, or do nothing when the pair already has
/// one. `RETURNING` answers a row only for the insert that happened, which
/// is what tells the create it owns the key — and a *concurrent* claim of
/// the same pair waits on this statement until the other transaction
/// commits (then skips) or aborts (then inserts).
const CLAIM_SQL: &str = "INSERT INTO idempotency_keys (user_id, key, status_id)
         VALUES ($1, $2, $3)
    ON CONFLICT (user_id, key) DO NOTHING
      RETURNING status_id";

/// [`reassign`]: move a stale mapping — one whose status has been deleted —
/// onto the fresh one, and restart its 24-hour window so the next sweep
/// measures the age of the status the key now names.
const REASSIGN_SQL: &str = "UPDATE idempotency_keys
            SET status_id = $3,
                inserted_at = now()
          WHERE user_id = $1 AND key = $2";

/// [`delete_older_than`]: every mapping whose window has run out. The
/// `inserted_at` index added with the table serves this walk.
const SWEEP_SQL: &str = "DELETE FROM idempotency_keys WHERE inserted_at < now() - $1";

/// [`insert_aged`]: one mapping with an explicit age, which is how a test
/// puts the table into the state the sweep is about without waiting a day
/// out.
#[cfg(test)]
const INSERT_AGED_SQL: &str = "INSERT INTO idempotency_keys (user_id, key, status_id, inserted_at)
     VALUES ($1, $2, $3, now() - make_interval(secs => $4))";

/// The live status the pair `(user_id, key)` created: `Some` for a replay,
/// `None` when the key was never claimed or its status is gone.
///
/// The join keeps only `deleted_at IS NULL` rows, so a tombstone counts as
/// gone.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn live_status(pool: &PgPool, user_id: Uuid, key: &str) -> AppResult<Option<Status>> {
    let status = sqlx::query_as::<_, Status>(LIVE_SQL)
        .bind(user_id)
        .bind(key)
        .fetch_optional(pool)
        .await?;
    Ok(status)
}

/// [`live_status`] on an open connection: the claim-conflict re-read runs
/// this statement on the create's own transaction, which is what lets it
/// name the status the winning request committed.
///
/// # Errors
///
/// As [`live_status`].
pub async fn live_status_conn(
    conn: &mut PgConnection,
    user_id: Uuid,
    key: &str,
) -> AppResult<Option<Status>> {
    let status = sqlx::query_as::<_, Status>(LIVE_SQL)
        .bind(user_id)
        .bind(key)
        .fetch_optional(&mut *conn)
        .await?;
    Ok(status)
}

/// Claims `(user_id, key)` for status `status_id`: `true` when this call
/// stored the mapping, `false` when the pair already had one.
///
/// This is the `ON CONFLICT DO NOTHING` arm the create races through, on
/// the caller's transaction: a *concurrent* claim of the same pair waits
/// on this statement until the other transaction commits (then skips) or
/// aborts (then inserts).
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); an unknown `user_id` or
/// `status_id` arrives as the foreign-key violation the database reports —
/// which is why the claim belongs on the transaction that just wrote the
/// status.
pub async fn claim(
    conn: &mut PgConnection,
    user_id: Uuid,
    key: &str,
    status_id: Uuid,
) -> AppResult<bool> {
    let claimed = sqlx::query_scalar::<_, Uuid>(CLAIM_SQL)
        .bind(user_id)
        .bind(key)
        .bind(status_id)
        .fetch_optional(&mut *conn)
        .await?;
    Ok(claimed.is_some())
}

/// Moves the pair's mapping onto `status_id`: the stale-key arm of the
/// create, taken when [`claim`] found the pair already held and its status
/// deleted.
///
/// A pair that names no row moves nothing — the caller only reaches here
/// off a conflict that named exactly the row this restates.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn reassign(
    conn: &mut PgConnection,
    user_id: Uuid,
    key: &str,
    status_id: Uuid,
) -> AppResult<()> {
    sqlx::query(REASSIGN_SQL)
        .bind(user_id)
        .bind(key)
        .bind(status_id)
        .execute(&mut *conn)
        .await
        .map(drop)?;
    Ok(())
}

/// Deletes every mapping older than `max_age`, answering how many rows went.
/// This is the [`idempotency.cleanup`](crate::jobs::idempotency::KIND_CLEANUP)
/// sweep's whole job: past the horizon a key stops replaying and starts
/// creating again.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn delete_older_than(pool: &PgPool, max_age: TimeDelta) -> AppResult<u64> {
    let deleted = sqlx::query(SWEEP_SQL).bind(max_age).execute(pool).await?;
    Ok(deleted.rows_affected())
}

/// A `(user_id, key)` → status mapping `secs` seconds old, over a plain
/// local post by `user`: the fixture the sweep's tests age rows with.
/// Answers the fresh status's id.
#[cfg(test)]
pub(crate) async fn insert_aged(
    pool: &PgPool,
    user: &crate::models::user::User,
    key: &str,
    secs: i64,
) -> Uuid {
    let status = crate::models::status::insert(
        pool,
        crate::config::TEST_BASE_URL,
        &crate::models::status::NewStatus::local(user, "aged post"),
    )
    .await
    .expect("the fixture status inserts");
    let written = sqlx::query(INSERT_AGED_SQL)
        .bind(user.id)
        .bind(key)
        .bind(status.id)
        .bind(secs)
        .execute(pool)
        .await
        .expect("the fixture key inserts");
    assert_eq!(written.rows_affected(), 1, "one mapping was written");
    status.id
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::*;
    use crate::{
        models::{
            status::{self, NewStatus},
            user::User,
        },
        test_fixtures::local_user as user,
    };

    /// The instance origin the fixture posts under.
    const BASE: &str = crate::config::TEST_BASE_URL;

    /// A plain live post by `author`, answering its row.
    async fn post(pool: &PgPool, author: &User, text: &str) -> Status {
        status::insert(pool, BASE, &NewStatus::local(author, text))
            .await
            .expect("the fixture status inserts")
    }

    /// Claim `key` for `status_id` on a transaction of its own.
    async fn claimed(pool: &PgPool, user_id: Uuid, key: &str, status_id: Uuid) -> bool {
        let mut tx = pool.begin().await.expect("a transaction opens");
        let outcome = claim(&mut tx, user_id, key, status_id)
            .await
            .expect("the claim runs");
        tx.commit().await.expect("the claim commits");
        outcome
    }

    /// The mapping's stored `status_id` and its age in seconds, read raw.
    async fn stored(pool: &PgPool, user_id: Uuid, key: &str) -> (Uuid, i64) {
        sqlx::query_as::<_, (Uuid, i64)>(
            "SELECT status_id, EXTRACT(EPOCH FROM (now() - inserted_at))::bigint
               FROM idempotency_keys WHERE user_id = $1 AND key = $2",
        )
        .bind(user_id)
        .bind(key)
        .fetch_one(pool)
        .await
        .expect("the mapping reads")
    }

    /// How many mappings the table holds.
    async fn count(pool: &PgPool) -> i64 {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM idempotency_keys")
            .fetch_one(pool)
            .await
            .expect("the count reads")
    }

    /// The live status the key answers, asserted to exist.
    async fn replayed(pool: &PgPool, user_id: Uuid, key: &str) -> Status {
        live_status(pool, user_id, key)
            .await
            .expect("the read runs")
            .expect("the key names a live status")
    }

    /// Move the pair's mapping onto `status_id` on a transaction of its own.
    async fn moved(pool: &PgPool, user_id: Uuid, key: &str, status_id: Uuid) {
        let mut tx = pool.begin().await.expect("a transaction opens");
        reassign(&mut tx, user_id, key, status_id)
            .await
            .expect("the reassign runs");
        tx.commit().await.expect("the reassign commits");
    }

    #[tokio::test]
    async fn a_first_claim_wins_and_a_repeat_does_not() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let alice = user(pool, "alice").await;
        let first = post(pool, &alice, "first").await;
        let second = post(pool, &alice, "second").await;

        assert!(
            claimed(pool, alice.id, "k1", first.id).await,
            "the first claim stores the mapping"
        );
        assert!(
            !claimed(pool, alice.id, "k1", second.id).await,
            "a repeat of the pair claims nothing"
        );
        assert_eq!(
            stored(pool, alice.id, "k1").await.0,
            first.id,
            "and the original mapping stands untouched"
        );
        assert_eq!(
            replayed(pool, alice.id, "k1").await.id,
            first.id,
            "the replay reads the claimed status back"
        );
    }

    #[tokio::test]
    async fn keys_are_scoped_per_account() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let alice = user(pool, "alice").await;
        let bob = user(pool, "bob").await;
        let alice_post = post(pool, &alice, "alice's").await;
        let bob_post = post(pool, &bob, "bob's").await;

        assert!(claimed(pool, alice.id, "shared", alice_post.id).await);
        assert!(
            claimed(pool, bob.id, "shared", bob_post.id).await,
            "the same key text is another account's to claim"
        );
        assert_eq!(
            replayed(pool, bob.id, "shared").await.id,
            bob_post.id,
            "and each account's replay reads its own status"
        );
    }

    #[tokio::test]
    async fn an_unclaimed_key_names_no_status() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let alice = user(pool, "alice").await;

        assert!(
            live_status(pool, alice.id, "never")
                .await
                .expect("the read runs")
                .is_none(),
            "a key that was never claimed replays nothing"
        );
    }

    #[tokio::test]
    async fn a_tombstoned_status_frees_its_key_for_a_new_mapping() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let alice = user(pool, "alice").await;
        let doomed = post(pool, &alice, "doomed").await;
        assert!(claimed(pool, alice.id, "k1", doomed.id).await);

        let tombstone = status::soft_delete(pool, doomed.id)
            .await
            .expect("the delete runs")
            .expect("the row was live");
        assert!(
            tombstone.deleted_at.is_some(),
            "the row stands as a tombstone"
        );

        assert!(
            live_status(pool, alice.id, "k1")
                .await
                .expect("the read runs")
                .is_none(),
            "a deleted status is gone for the replay"
        );
        assert_eq!(
            count(pool).await,
            1,
            "the stale row stays until the create that takes the key moves it"
        );
        let fresh = post(pool, &alice, "fresh").await;
        assert!(
            !claimed(pool, alice.id, "k1", fresh.id).await,
            "the occupied pair still refuses a claim"
        );
        moved(pool, alice.id, "k1", fresh.id).await;
        assert_eq!(
            replayed(pool, alice.id, "k1").await.id,
            fresh.id,
            "and the replay reads the moved mapping"
        );
    }

    #[tokio::test]
    async fn a_hard_delete_takes_the_mapping_with_it() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let alice = user(pool, "alice").await;
        let status = post(pool, &alice, "cascaded").await;
        assert!(claimed(pool, alice.id, "k1", status.id).await);

        let gone = sqlx::query("DELETE FROM statuses WHERE id = $1")
            .bind(status.id)
            .execute(pool)
            .await
            .expect("the status hard-deletes");
        assert_eq!(gone.rows_affected(), 1, "the status row went");
        assert_eq!(
            count(pool).await,
            0,
            "the status_id cascade retired the mapping with the row"
        );

        let again = post(pool, &alice, "after the cascade").await;
        assert!(
            claimed(pool, alice.id, "k1", again.id).await,
            "the cascade freed the pair for a fresh claim"
        );
        let departed = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(alice.id)
            .execute(pool)
            .await
            .expect("the user hard-deletes");
        assert_eq!(departed.rows_affected(), 1, "the account row went");
        assert_eq!(
            count(pool).await,
            0,
            "and the user_id cascade takes the keys with the account"
        );
    }

    #[tokio::test]
    async fn the_sweep_deletes_only_keys_past_the_horizon() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let alice = user(pool, "alice").await;
        let (_old, edge, fresh) = (
            insert_aged(pool, &alice, "old", 24 * 3_600 + 3_600).await,
            insert_aged(pool, &alice, "edge", 24 * 3_600 - 60).await,
            insert_aged(pool, &alice, "fresh", 60).await,
        );

        let deleted = delete_older_than(pool, TimeDelta::hours(24))
            .await
            .expect("the sweep runs");

        assert_eq!(deleted, 1, "exactly the row past 24 hours went");
        assert_eq!(count(pool).await, 2, "the window's edge is still kept");
        assert!(
            live_status(pool, alice.id, "old")
                .await
                .expect("the read runs")
                .is_none(),
            "the aged key no longer names its status"
        );
        assert_eq!(
            replayed(pool, alice.id, "edge").await.id,
            edge,
            "one minute short of the horizon stays replayable"
        );
        assert_eq!(
            replayed(pool, alice.id, "fresh").await.id,
            fresh,
            "and a minute-old claim is untouched"
        );
    }

    #[tokio::test]
    async fn a_reassigned_key_starts_its_window_afresh() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let alice = user(pool, "alice").await;
        let aged = insert_aged(pool, &alice, "k1", 48 * 3_600).await;
        assert_eq!(
            replayed(pool, alice.id, "k1").await.id,
            aged,
            "age alone does not hide a mapping — the sweep deletes it"
        );

        let tombstoned = status::soft_delete(pool, aged)
            .await
            .expect("the delete runs")
            .expect("the aged row was live");
        assert_eq!(tombstoned.id, aged, "the mapped row is a tombstone now");
        let fresh = post(pool, &alice, "the old one got deleted").await;
        moved(pool, alice.id, "k1", fresh.id).await;

        assert_eq!(
            delete_older_than(pool, TimeDelta::hours(24))
                .await
                .expect("the sweep runs"),
            0,
            "the moved row's window restarted, so the sweep passes it over"
        );
    }
}
