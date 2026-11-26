//! The `pinned_statuses` table: one account pinning one of its own posts to
//! its profile — an account-level fact, relationally.
//!
//! Reference: Pleroma's `pin` and `unpin` actions
//! (`lib/pleroma/web/mastodon_api/controllers/status_controller.ex`) over
//! `Pleroma.Web.CommonAPI.pin`/`unpin` (`lib/pleroma/web/common_api.ex`),
//! which keep the fact as the `pinned_objects` map on the user row: object
//! id to the moment it was pinned. This table is that map laid out on its
//! own (`docs/ARCHITECTURE.md` decision 3): one row per
//! `(user_id, status_id)` with the pin time beside it.
//!
//! What a pin may *be* is the endpoint's gate, not the table's: Pleroma
//! refuses another account's post, a post the caller may not see, a reblog
//! and a post past `[:instance, :max_pinned_statuses]`, and here
//! [`crate::api::statuses`] runs those checks before it calls [`pin`]. The
//! model stores what it is handed.
//!
//! Both writes are idempotent on the primary key, the same pair of moves
//! [`crate::models::bookmark`] and [`crate::models::endorsement`] make:
//! pinning twice is one row that keeps its pin time, and unpinning what is
//! not pinned (or no longer is) deletes nothing. Pleroma's own
//! `User.add_pinned_object_id/2` has the same shape — a repeat leaves the
//! stored map, and so the limit check, untouched.
//!
//! The three reads match the three surfaces that need them, all keyed by
//! the pinning account (`docs/ARCHITECTURE.md` decision 13):
//! [`pinned_ids`] answers the viewer's `pinned` flag and `pleroma.pinned_at`
//! for a whole page of statuses in one statement (the status renderer's
//! batch), [`list_for_user`] gives an account's own pinned statuses newest
//! pin first, and [`count`] is the number the pin endpoint weighs against
//! [`max_pinned_statuses`](crate::config::Config::max_pinned_statuses).
//!
//! Deleting a user or a status takes its pins through the foreign key's
//! cascade; a *soft* delete clears the dead post's pins in its own
//! transaction (see [`crate::models::status::soft_delete`]), because a
//! tombstone is pinned by nobody.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{error::AppResult, models::status::Status};

/// [`pin`]: the pair the caller names with `inserted_at` left to the column
/// default. `ON CONFLICT (user_id, status_id) DO NOTHING` is the
/// idempotence, and `RETURNING *` answers exactly the insert that happened,
/// which is what tells the caller a fresh pin landed.
const PIN_SQL: &str = "INSERT INTO pinned_statuses (user_id, status_id)
     VALUES ($1, $2)
ON CONFLICT (user_id, status_id) DO NOTHING
RETURNING *";

/// [`unpin`]: one account's pin of one status; a pair that never pinned
/// (or already withdrew) deletes nothing, so withdrawal is idempotent.
/// `RETURNING *` hands the caller the row that went.
const UNPIN_SQL: &str = "DELETE FROM pinned_statuses
      WHERE user_id = $1 AND status_id = $2
   RETURNING *";

/// [`replace_all`], first half: drops every pin of the account that the new
/// list does not name (an empty list names none, so every pin goes).
const REPLACE_DELETE_SQL: &str = "DELETE FROM pinned_statuses
      WHERE user_id = $1 AND NOT (status_id = ANY($2))";

/// [`replace_all`], second half: pins the listed statuses the account does
/// not pin yet, the first of the list the newest.
const REPLACE_INSERT_SQL: &str = "INSERT INTO pinned_statuses (user_id, status_id, inserted_at)
     SELECT $1, listed.status_id, now() - listed.place::float8 * interval '1 microsecond'
       FROM unnest($2::uuid[]) WITH ORDINALITY AS listed(status_id, place)
ON CONFLICT (user_id, status_id) DO NOTHING";

/// [`pinned_ids`]: which of a batch of statuses the account pinned, and
/// when. Only the ids that *have* a row come back; the caller's flag is
/// membership in the result, its timestamp the map's value.
const FLAGS_SQL: &str = "SELECT status_id, inserted_at FROM pinned_statuses
      WHERE user_id = $1 AND status_id = ANY($2)";

/// [`list_for_user`]: one account's pinned statuses, newest pin first — the
/// order its profile features them in. The pin time orders, the status id
/// breaks a same-instant tie (Pleroma walks the same map in insertion
/// order), and [`status::get_many`](crate::models::status::get_many)'s
/// tombstone rule is spelled out in the join because this read answers whole
/// [`Status`] rows rather than the pin rows alone.
const LIST_SQL: &str = "SELECT s.* FROM pinned_statuses p
     JOIN statuses s ON s.id = p.status_id
      WHERE p.user_id = $1
        AND s.deleted_at IS NULL
   ORDER BY p.inserted_at DESC, p.status_id DESC";

/// [`count`]: how many statuses one account pins, the number the pin
/// endpoint compares against [`max_pinned_statuses`](crate::config::Config::max_pinned_statuses).
const COUNT_SQL: &str = "SELECT count(*) FROM pinned_statuses WHERE user_id = $1";

/// One row of `pinned_statuses`: `user_id` pins `status_id`, since
/// `inserted_at`.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct PinnedStatus {
    /// The pinning account.
    pub user_id: Uuid,
    /// The pinned status — always a post of `user_id`'s own, which is
    /// the endpoint's rule, not a constraint here.
    pub status_id: Uuid,
    /// When the status was pinned; a repeat keeps this, since it keeps the
    /// row. The profile listing orders by it, newest pin first.
    pub inserted_at: DateTime<Utc>,
}

/// Records `user_id` pinning `status_id` and returns the new row, or `None`
/// when the pair already held the pin.
///
/// The write is idempotent on the table's primary key: a pair that already
/// pinned keeps its row exactly as it is — same `inserted_at`, same place in
/// [`list_for_user`] — and answers `None`, so the caller can tell a pin from
/// a restatement. Nothing else moves: a pin has no stored counter.
///
/// # Errors
///
/// Any database failure (connection, constraint, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError). The two foreign keys are
/// the failures reachable here: an unknown `user_id` or `status_id`.
pub async fn pin(pool: &PgPool, user_id: Uuid, status_id: Uuid) -> AppResult<Option<PinnedStatus>> {
    Ok(sqlx::query_as::<_, PinnedStatus>(PIN_SQL)
        .bind(user_id)
        .bind(status_id)
        .fetch_optional(pool)
        .await?)
}

/// Removes `user_id`'s pin of `status_id`, reporting the row that went.
///
/// Withdrawing a pin that is not there — never made, or already withdrawn —
/// removes nothing and is not an error, so the write is idempotent; the
/// answer is `None` either way and the caller tells a withdrawal from a
/// no-op by it.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn unpin(
    pool: &PgPool,
    user_id: Uuid,
    status_id: Uuid,
) -> AppResult<Option<PinnedStatus>> {
    Ok(sqlx::query_as::<_, PinnedStatus>(UNPIN_SQL)
        .bind(user_id)
        .bind(status_id)
        .fetch_optional(pool)
        .await?)
}

/// Makes `status_ids` exactly the statuses `user_id` pins, in that order.
///
/// `status_ids` lists the newest pin first, the way a remote actor's
/// featured collection does. Pins not in the list are removed; a pin already
/// held keeps its row and its pin time; a new one is stamped a microsecond
/// older per place down the list, so a set that is all new reads back in the
/// list's order. An empty list unpins everything. The delete and the insert
/// run in one transaction.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError);
/// the stored pins are then unchanged.
pub async fn replace_all(pool: &PgPool, user_id: Uuid, status_ids: &[Uuid]) -> AppResult<()> {
    let mut tx = pool.begin().await?;
    sqlx::query(REPLACE_DELETE_SQL)
        .bind(user_id)
        .bind(status_ids)
        .execute(&mut *tx)
        .await
        .map(drop)?;
    sqlx::query(REPLACE_INSERT_SQL)
        .bind(user_id)
        .bind(status_ids)
        .execute(&mut *tx)
        .await
        .map(drop)?;
    tx.commit().await?;
    Ok(())
}

/// Which of `status_ids` `user_id` has pinned, and since when — the batched
/// `pinned` / `pleroma.pinned_at` read the status renderer runs once per
/// page (`docs/ARCHITECTURE.md` decision 13).
///
/// The answer is the pinned subset, not a per-status flag list: an id with
/// no row is simply absent. An empty `status_ids` asks the database nothing
/// at all. Unlike the favourites flag, a pin is keyed to the row's *own*
/// id: only a post of one's own can be pinned, so there is no original to
/// map through, and one account's pin is nobody else's flag.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn pinned_ids(
    pool: &PgPool,
    user_id: Uuid,
    status_ids: &[Uuid],
) -> AppResult<HashMap<Uuid, DateTime<Utc>>> {
    if status_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query_as::<_, (Uuid, DateTime<Utc>)>(FLAGS_SQL)
        .bind(user_id)
        .bind(status_ids)
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().collect())
}

/// The statuses `user_id` pins, newest pin first.
///
/// This is the account's own featured list: the whole set rather than a
/// keyset page, because an account pins at most
/// [`max_pinned_statuses`](crate::config::Config::max_pinned_statuses) posts
/// and every one of them belongs on the profile. A pin whose status no
/// longer answers — hard-deleted, or a tombstone — is no pin at all:
/// [`crate::models::status::soft_delete`] clears those rows in its own
/// transaction, and the join's `deleted_at` check is what keeps a pin that
/// an older delete left behind off the list.
///
/// Whether the statuses that come back may be *seen* by whoever asks is the
/// caller's [`filter_visible`] step, not this one's.
///
/// [`filter_visible`]: crate::models::status_visibility::filter_visible
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn list_for_user(pool: &PgPool, user_id: Uuid) -> AppResult<Vec<Status>> {
    Ok(sqlx::query_as::<_, Status>(LIST_SQL)
        .bind(user_id)
        .fetch_all(pool)
        .await?)
}

/// How many statuses `user_id` pins — the number the pin endpoint weighs
/// against the limit before it writes one more. An account with no pins
/// counts `0`.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn count(pool: &PgPool, user_id: Uuid) -> AppResult<i64> {
    let row = sqlx::query_scalar::<_, i64>(COUNT_SQL)
        .bind(user_id)
        .fetch_one(pool)
        .await?;
    Ok(row)
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone as _;
    use sqlx::PgPool;

    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        error::AppError,
        models::status::{NewStatus, insert, soft_delete},
        models::user::User,
        test_fixtures::local_user as user,
    };

    /// The instance origin the fixtures post under.
    const BASE: &str = TEST_BASE_URL;

    /// A public post by `author`, as its row.
    async fn post(pool: &PgPool, author: &User) -> Status {
        insert(pool, BASE, &NewStatus::local(author, "hello"))
            .await
            .expect("the fixture post inserts")
    }

    /// Pins `status_id` as `who`, asserting the write created a row (a test
    /// that needs the no-op arm calls [`pin`] itself).
    async fn pin_as(pool: &PgPool, who: &User, status_id: Uuid) {
        let created = pin(pool, who.id, status_id)
            .await
            .expect("the fixture pin stores");
        assert!(created.is_some(), "the fixture pair had not pinned yet");
    }

    /// Stamps one pin with `when`, which the ordering tests need: pins made
    /// back to back can share a `UUIDv7` millisecond, and the list's order
    /// is the pin time's, not the insert order's.
    async fn stamp(pool: &PgPool, who: &User, status_id: Uuid, when: DateTime<Utc>) {
        let stamped = sqlx::query(
            "UPDATE pinned_statuses SET inserted_at = $3 WHERE user_id = $1 AND status_id = $2",
        )
        .bind(who.id)
        .bind(status_id)
        .bind(when)
        .execute(pool)
        .await
        .expect("the fixture stamping runs")
        .rows_affected();
        assert_eq!(stamped, 1, "the pin row was there to stamp");
    }

    /// A fixed instant to stamp pins with, `days` after the start of 2024.
    fn day(days: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2024, 1, 1 + days, 12, 0, 0).unwrap()
    }

    /// Every stored pin as `(user_id, status_id)`, newest pin first — the
    /// order the profile list walks — so an assertion says what the table
    /// holds rather than what a loader chose to show.
    async fn stored(pool: &PgPool) -> Vec<(Uuid, Uuid)> {
        sqlx::query_as::<_, (Uuid, Uuid)>(
            "SELECT user_id, status_id FROM pinned_statuses \
              ORDER BY inserted_at DESC, status_id DESC",
        )
        .fetch_all(pool)
        .await
        .expect("the table reads")
    }

    /// The ids of `statuses`, in order.
    fn ids(statuses: &[Status]) -> Vec<Uuid> {
        statuses.iter().map(|status| status.id).collect()
    }

    #[tokio::test]
    async fn pinning_writes_the_row_and_repeating_is_one_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let status_id = post(&pool, &alice).await.id;

        let created = pin(&pool, alice.id, status_id)
            .await
            .expect("the first pin stores")
            .expect("the pair had nothing to keep");

        assert_eq!(created.user_id, alice.id, "who pinned");
        assert_eq!(created.status_id, status_id, "what they pinned");

        let again = pin(&pool, alice.id, status_id)
            .await
            .expect("repeating a pin is not an error");

        assert!(again.is_none(), "the repeat created no row");
        assert_eq!(
            stored(&pool).await,
            vec![(alice.id, status_id)],
            "still one row in the table"
        );
    }

    #[tokio::test]
    async fn a_repeat_pin_keeps_the_moment_of_the_first_one() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let status_id = post(&pool, &alice).await.id;
        let first = day(1);
        pin_as(&pool, &alice, status_id).await;
        stamp(&pool, &alice, status_id, first).await;

        let repeated = pin(&pool, alice.id, status_id)
            .await
            .expect("the repeat pin runs");
        assert!(repeated.is_none(), "the restatement wrote no row");

        let kept = pinned_ids(&pool, alice.id, &[status_id])
            .await
            .expect("the pin reads")
            .get(&status_id)
            .copied()
            .expect("the status is still pinned");
        assert_eq!(
            kept, first,
            "the restatement kept its row, and so its original pin time"
        );
    }

    #[tokio::test]
    async fn unpinning_takes_the_row_and_is_idempotent() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let status_id = post(&pool, &alice).await.id;
        let created = pin(&pool, alice.id, status_id)
            .await
            .expect("the fixture pin stores")
            .expect("the pair had nothing to keep");

        let removed = unpin(&pool, alice.id, status_id)
            .await
            .expect("the withdrawal runs")
            .expect("the pin was there");
        assert_eq!(removed.user_id, created.user_id, "the row that went");
        assert_eq!(removed.status_id, created.status_id, "and it is this one");

        assert!(
            unpin(&pool, alice.id, status_id)
                .await
                .expect("repeating a withdrawal runs")
                .is_none(),
            "withdrawing what is already gone removes nothing"
        );
        assert!(stored(&pool).await.is_empty(), "the table is empty again");
    }

    #[tokio::test]
    async fn pinned_ids_answers_the_batch_and_asks_nothing_for_an_empty_page() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let first = post(&pool, &alice).await.id;
        let second = post(&pool, &alice).await.id;
        pin_as(&pool, &alice, first).await;
        stamp(&pool, &alice, first, day(2)).await;

        let flagged = pinned_ids(&pool, alice.id, &[first, second])
            .await
            .expect("the flags read");
        assert_eq!(
            flagged,
            HashMap::from([(first, day(2))]),
            "only the pinned id comes back, with the moment it was pinned"
        );
        assert!(
            !flagged.contains_key(&second),
            "an id with no row is simply absent"
        );
        assert_eq!(
            pinned_ids(&pool, alice.id, &[])
                .await
                .expect("the empty read runs"),
            HashMap::new(),
            "an empty page flags nothing, and the loader skips the query"
        );
    }

    #[tokio::test]
    async fn pins_are_each_account_s_own() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let shared = post(&pool, &alice).await.id;
        pin_as(&pool, &alice, shared).await;

        assert!(
            pinned_ids(&pool, bob.id, &[shared])
                .await
                .expect("bob's flags read")
                .is_empty(),
            "alice's pin is nobody else's flag"
        );
        assert_eq!(
            count(&pool, alice.id).await.expect("alice counts"),
            1,
            "the pin is hers to count"
        );
        assert_eq!(
            count(&pool, bob.id).await.expect("bob counts"),
            0,
            "and bob has nothing of his own"
        );
        assert!(
            list_for_user(&pool, bob.id)
                .await
                .expect("bob's list reads")
                .is_empty(),
            "and bob's profile lists nothing"
        );

        let nothing = unpin(&pool, bob.id, shared)
            .await
            .expect("bob's withdrawal runs");
        assert!(
            nothing.is_none(),
            "bob never pinned alice's post, so there was nothing to withdraw"
        );
        assert_eq!(
            count(&pool, alice.id).await.expect("alice counts again"),
            1,
            "and left alice's pin alone"
        );
    }

    #[tokio::test]
    async fn list_for_user_answers_newest_pin_first() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let oldest = post(&pool, &alice).await.id;
        let middle = post(&pool, &alice).await.id;
        let newest = post(&pool, &alice).await.id;
        for status_id in [oldest, middle, newest] {
            pin_as(&pool, &alice, status_id).await;
        }
        stamp(&pool, &alice, oldest, day(1)).await;
        stamp(&pool, &alice, middle, day(3)).await;
        stamp(&pool, &alice, newest, day(2)).await;

        let listed = list_for_user(&pool, alice.id)
            .await
            .expect("the profile list reads");
        assert_eq!(
            ids(&listed),
            vec![middle, newest, oldest],
            "the pin time orders the list, not the status ids"
        );
        assert!(
            listed.iter().all(|status| status.user_id == alice.id),
            "every entry is a post of the pinning account"
        );
    }

    #[tokio::test]
    async fn a_tombstone_is_no_pin_and_a_hard_delete_cascades() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let doomed = post(&pool, &alice).await.id;
        let kept = post(&pool, &alice).await.id;
        pin_as(&pool, &alice, doomed).await;
        pin_as(&pool, &alice, kept).await;

        // The delete that clears the pin is `soft_delete`'s own cleanup: the
        // row stays as a tombstone, the pin does not.
        let tombstone = soft_delete(&pool, doomed)
            .await
            .expect("the delete runs")
            .expect("the status was live");
        assert!(tombstone.deleted_at.is_some(), "the row is a tombstone");
        assert_eq!(
            ids(&list_for_user(&pool, alice.id)
                .await
                .expect("the list reads")),
            vec![kept],
            "the deleted post is off the profile"
        );
        assert_eq!(
            count(&pool, alice.id).await.expect("the count reads"),
            1,
            "and its pin row is gone, not merely hidden"
        );

        // A hard delete takes the pin row with it through the cascade.
        let erased = sqlx::query("DELETE FROM statuses WHERE id = $1")
            .bind(kept)
            .execute(&pool)
            .await
            .expect("the fixture status deletes")
            .rows_affected();
        assert_eq!(erased, 1, "the kept post was there to delete");
        assert!(stored(&pool).await.is_empty(), "and its pin cascaded");
    }

    #[tokio::test]
    async fn deleting_a_user_cascades_their_pins() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let status_id = post(&pool, &alice).await.id;
        pin_as(&pool, &alice, status_id).await;

        let erased = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(alice.id)
            .execute(&pool)
            .await
            .expect("the fixture user deletes")
            .rows_affected();
        assert_eq!(erased, 1, "the pinning account was there to delete");
        assert!(
            stored(&pool).await.is_empty(),
            "and the cascade emptied the table"
        );
    }

    #[tokio::test]
    async fn replacing_pins_a_new_set_in_list_order() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let first = post(&pool, &alice).await.id;
        let second = post(&pool, &alice).await.id;
        let third = post(&pool, &alice).await.id;

        replace_all(&pool, alice.id, &[third, first, second])
            .await
            .expect("the replacement stores");

        let listed = list_for_user(&pool, alice.id)
            .await
            .expect("the list reads");
        assert_eq!(ids(&listed), vec![third, first, second], "newest pin first");
    }

    #[tokio::test]
    async fn replacing_drops_the_pins_the_list_leaves_out_and_keeps_the_rest() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let kept = post(&pool, &alice).await.id;
        let dropped = post(&pool, &alice).await.id;
        let bobs = post(&pool, &bob).await.id;
        pin_as(&pool, &alice, kept).await;
        pin_as(&pool, &alice, dropped).await;
        pin_as(&pool, &bob, bobs).await;
        stamp(&pool, &alice, kept, day(1)).await;

        replace_all(&pool, alice.id, &[kept, kept])
            .await
            .expect("the replacement stores");

        let held = pinned_ids(&pool, alice.id, &[kept, dropped])
            .await
            .expect("the flags read");
        assert_eq!(held.len(), 1, "only the listed status stays pinned");
        assert_eq!(held.get(&kept), Some(&day(1)), "and keeps its pin time");
        assert_eq!(
            count(&pool, bob.id).await.expect("the count reads"),
            1,
            "another account's pins are untouched"
        );
    }

    #[tokio::test]
    async fn replacing_with_an_empty_list_unpins_everything() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let pinned = post(&pool, &alice).await.id;
        pin_as(&pool, &alice, pinned).await;

        replace_all(&pool, alice.id, &[])
            .await
            .expect("the replacement stores");

        assert!(stored(&pool).await.is_empty(), "no pin is left");
    }

    #[tokio::test]
    async fn an_unknown_status_refuses_the_write() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;

        let err = pin(&pool, alice.id, Uuid::now_v7())
            .await
            .expect_err("a status that does not exist cannot be pinned");
        assert!(
            matches!(err, AppError::Database(_)),
            "the foreign key refuses, not a silent no-op: {err}"
        );
        assert!(stored(&pool).await.is_empty(), "nothing was stored");
    }
}
