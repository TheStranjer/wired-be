//! The write side of mutes: recording, lifting and sweeping.

use sqlx::PgPool;

use super::{Mute, MuteOptions, SELF_MUTE, expiry_of};
use crate::{
    error::{AppError, AppResult},
    models::user::User,
    util::id::new_id,
};

/// [`mute`]: the pair the caller names, with its `expires_at` computed by the
/// database clock from the bound duration (see [`expiry_of`]) — and `NULL` for
/// an indefinite mute, because `now() + NULL::interval` is `NULL`, so this one
/// statement spells both timed and indefinite writes.
/// `ON CONFLICT (muter_id, muted_id) DO UPDATE` is the ticket's upsert: a pair
/// that is already muted keeps its row, its id and its `inserted_at`, and
/// takes the two options from the new write. `RETURNING *` brings whichever row
/// the statement wrote back to the caller.
const UPSERT_SQL: &str =
    "INSERT INTO mutes (id, muter_id, muted_id, hide_notifications, expires_at)
     VALUES ($1, $2, $3, $4, now() + $5::interval)
ON CONFLICT (muter_id, muted_id) DO UPDATE
        SET hide_notifications = EXCLUDED.hide_notifications,
            expires_at = EXCLUDED.expires_at
RETURNING *";

/// [`unmute`]: the pair's row, which comes back so the caller can report
/// whether a mute was there to lift. A never-muted (or already lifted) pair
/// deletes nothing, so unmuting is idempotent. Note that a lapsed mute's row
/// is still there to delete: reads treat it as absent, this statement removes
/// it without waiting for the sweep.
const UNMUTE_SQL: &str = "DELETE FROM mutes WHERE muter_id = $1 AND muted_id = $2
  RETURNING *";

/// [`delete_expired`]: every mute whose `expires_at` has been reached, i.e.
/// every row the reads already treat as absent. Rows at exactly `now()` go
/// too: `expires_at > now()` is what keeps a mute, so a row failing that
/// comparison has expired.
const DELETE_EXPIRED_SQL: &str =
    "DELETE FROM mutes WHERE expires_at IS NOT NULL AND expires_at <= now()";

/// Records that `muter` mutes `target` with `options`, and returns the row.
///
/// The write is an upsert on the pair: a first mute of a pair mints the row's
/// id (decision 4) and stores both options; a repeat of a pair that is already
/// muted — or whose mute has lapsed but is still on the books — keeps the row
/// and its id and rewrites the two options from `options`. That makes re-muting
/// the way a client extends a timed mute, clears one by muting again with no
/// duration, or revives a lapsed mute.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with `"Can not mute yourself"` when the two
/// users are the same account. Any database failure (connection, constraint,
/// encoding) arrives as [`AppError::Database`](crate::error::AppError).
pub async fn mute(
    pool: &PgPool,
    muter: &User,
    target: &User,
    options: MuteOptions,
) -> AppResult<Mute> {
    if muter.id == target.id {
        return Err(AppError::Unprocessable(SELF_MUTE.to_owned()));
    }
    let row = sqlx::query_as::<_, Mute>(UPSERT_SQL)
        .bind(new_id())
        .bind(muter.id)
        .bind(target.id)
        .bind(options.notifications)
        .bind(expiry_of(options))
        .fetch_one(pool)
        .await?;
    Ok(row)
}

/// Lifts `muter`'s mute on `target`, reporting the removed row.
///
/// Unmuting restores nothing and requires nothing: a pair that was never muted
/// (or whose mute was already lifted) is the `None` answer and changes
/// nothing, so the write is idempotent. A lapsed mute whose row the sweep has
/// not yet deleted is a row like any other, so lifting it reports that row even
/// though no read counts it as a mute anymore.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn unmute(pool: &PgPool, muter: &User, target: &User) -> AppResult<Option<Mute>> {
    let removed = sqlx::query_as::<_, Mute>(UNMUTE_SQL)
        .bind(muter.id)
        .bind(target.id)
        .fetch_optional(pool)
        .await?;
    Ok(removed)
}

/// Deletes every mute whose `expires_at` has been reached and reports how many
/// rows went.
///
/// The sweep the `mutes.expire` periodic job runs: the reads already treat
/// these rows as absent, so deleting them is housekeeping, not the moment a
/// mute stops. An indefinite mute has no `expires_at` and is never touched.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn delete_expired(pool: &PgPool) -> AppResult<u64> {
    let removed = sqlx::query(DELETE_EXPIRED_SQL).execute(pool).await?;
    Ok(removed.rows_affected())
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::super::{
        is_muting, muted_ids, muting_page, notification_muted_ids,
        test_support::{
            TIMELINE_ONLY, muted_ids_of, page, row_ids_of, seconds_ahead, stored_mute, user,
            write_mute,
        },
    };
    use super::*;

    #[tokio::test]
    async fn a_mute_is_written_and_read_back() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let row = write_mute(&pool, &alice, &bob, MuteOptions::default()).await;

        assert!(row.id.get_version_num() == 7, "the id is a UUIDv7");
        assert_eq!(row.muter_id, alice.id, "who mutes");
        assert_eq!(row.muted_id, bob.id, "who is muted");
        assert!(
            row.hide_notifications,
            "the default mute hides notifications too"
        );
        assert!(row.expires_at.is_none(), "and defaults to indefinite");
        assert!(row.inserted_at <= Utc::now(), "the database stamps it");
        assert_eq!(
            stored_mute(&pool, &alice, &bob).await,
            row,
            "the row reads back as it was written"
        );

        assert!(
            is_muting(&pool, alice.id, bob.id).await.unwrap(),
            "alice mutes bob"
        );
        assert!(
            !is_muting(&pool, bob.id, alice.id).await.unwrap(),
            "muting is directional"
        );
        assert_eq!(
            muted_ids(&pool, alice.id).await.unwrap(),
            vec![bob.id],
            "bob is on alice's list"
        );
        assert!(
            muted_ids(&pool, bob.id).await.unwrap().is_empty(),
            "and alice is on nobody's"
        );
        assert_eq!(
            notification_muted_ids(&pool, alice.id).await.unwrap(),
            vec![bob.id],
            "and bob's notifications are hidden too"
        );

        let entries = muting_page(&pool, alice.id, page("")).await.unwrap();
        assert_eq!(
            muted_ids_of(&entries),
            vec![bob.id],
            "the page names the muted account"
        );
        assert_eq!(
            row_ids_of(&entries),
            vec![row.id],
            "and carries the mute row beside it"
        );
        assert_eq!(entries[0].0, row, "the row is the stored one");
        assert_eq!(entries[0].1.id, bob.id, "the user is the muted one");
        assert!(
            muting_page(&pool, bob.id, page(""))
                .await
                .unwrap()
                .is_empty(),
            "bob's own page holds nothing"
        );
    }

    /// Mastodon's defaults are the column defaults: a row written without
    /// either option hides notifications and never expires — the bare insert
    /// [`mute`] itself never makes.
    #[tokio::test]
    async fn the_column_defaults_are_mastodon_s() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let sql = "INSERT INTO mutes (id, muter_id, muted_id) VALUES ($1, $2, $3) RETURNING *";
        let row = sqlx::query_as::<_, Mute>(sql)
            .bind(new_id())
            .bind(alice.id)
            .bind(bob.id)
            .fetch_one(&pool)
            .await
            .expect("the bare row inserts");

        assert!(
            row.hide_notifications,
            "the notifications default is true, so a mute hides them"
        );
        assert!(row.expires_at.is_none(), "and the duration default is none");
        assert!(
            is_muting(&pool, alice.id, bob.id).await.unwrap(),
            "and such a row is a mute like any other"
        );
    }

    #[tokio::test]
    async fn notifications_false_mutes_the_timeline_only() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let row = write_mute(&pool, &alice, &bob, TIMELINE_ONLY).await;

        assert!(
            !row.hide_notifications,
            "the option is stored as the caller set it"
        );
        assert!(
            is_muting(&pool, alice.id, bob.id).await.unwrap(),
            "the pair is still muted"
        );
        assert_eq!(
            muted_ids(&pool, alice.id).await.unwrap(),
            vec![bob.id],
            "and on the muted list"
        );
        assert!(
            notification_muted_ids(&pool, alice.id)
                .await
                .unwrap()
                .is_empty(),
            "but not on the notification-muted one"
        );
    }

    /// The timed side of the write: `expires_in` becomes an `expires_at` that
    /// many seconds ahead, and the reads keep answering the mute while that
    /// instant is still in the future.
    #[tokio::test]
    async fn a_duration_stores_its_expiry() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let options = MuteOptions {
            notifications: true,
            expires_in: Some(3_600),
        };

        let row = write_mute(&pool, &alice, &bob, options).await;
        let ahead = seconds_ahead(&row);
        assert!(
            (3_590..=3_600).contains(&ahead),
            "the expiry is the requested hour from now, got {ahead}s: {:?}",
            row.expires_at
        );
        assert!(
            is_muting(&pool, alice.id, bob.id).await.unwrap(),
            "a mute whose expiry is in the future is a mute"
        );
        assert_eq!(
            muted_ids(&pool, alice.id).await.unwrap(),
            vec![bob.id],
            "on the list"
        );
        assert_eq!(
            notification_muted_ids(&pool, alice.id).await.unwrap(),
            vec![bob.id],
            "and on the notification-muted one"
        );
    }

    /// `duration=0` is Mastodon's spelling of "no expiry", and a negative
    /// nonsense value is treated the same way rather than as an already expired
    /// mute.
    #[tokio::test]
    async fn a_non_positive_duration_is_an_indefinite_mute() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;

        for expires_in in [Some(3_600), Some(0), Some(-60), None] {
            let options = MuteOptions {
                notifications: true,
                expires_in,
            };
            let row = mute(&pool, &alice, &bob, options)
                .await
                .expect("re-muting stores");
            let indefinite = expires_in.is_none_or(|secs| secs <= 0);
            assert_eq!(
                row.expires_at.is_none(),
                indefinite,
                "`expires_in` {expires_in:?} means {:?}",
                if indefinite { "indefinite" } else { "timed" }
            );
            assert!(
                is_muting(&pool, alice.id, bob.id).await.unwrap(),
                "and either way the pair is muted"
            );
        }
    }
}
