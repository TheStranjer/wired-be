//! The whole-column setters: writes that replace or move one stored value
//! (or the three denormalised counters at once) and refresh `updated_at`.
//! Unknown ids match no row and are never an error.

use serde_json::Value;
use sqlx::{Executor, PgPool, Postgres};
use uuid::Uuid;

use super::{CountDelta, User};
use crate::error::AppResult;

/// Adds `delta` to the denormalised counters of user `id` atomically,
/// clamping every count at zero, and refreshes `updated_at`.
///
/// The arithmetic happens in the `UPDATE` statement itself, so two
/// concurrent updates to the same row both take effect and the stored
/// counts are never negative. An unknown `id` matches no row and changes
/// nothing.
///
/// The step runs on any Postgres executor, so callers either fire it
/// against the pool or join it to a transaction of their own (see
/// [`remove_follower`](crate::models::follow::remove_follower), which
/// commits its row removal and its counter steps together).
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn update_counts<'e, E>(executor: E, id: Uuid, delta: CountDelta) -> AppResult<()>
where
    E: Executor<'e, Database = Postgres>,
{
    sqlx::query(
        "UPDATE users
            SET followers_count = GREATEST(0, followers_count + $2),
                following_count = GREATEST(0, following_count + $3),
                statuses_count  = GREATEST(0, statuses_count + $4),
                updated_at = now()
          WHERE id = $1",
    )
    .bind(id)
    .bind(delta.followers)
    .bind(delta.following)
    .bind(delta.statuses)
    .execute(executor)
    .await
    .map(drop)?;
    Ok(())
}

/// The `UPDATE` behind [`set_also_known_as`]: writes the whole alias
/// array in one statement and refreshes `updated_at`.
const SET_ALSO_KNOWN_AS_SQL: &str =
    "UPDATE users SET also_known_as = $2, updated_at = now() WHERE id = $1";

/// Stores `aliases` as user `id`'s `also_known_as` column, replacing what
/// was there.
///
/// The alias endpoints keep the add/remove logic (order-preserving append,
/// set-style removal) in the API layer and hand the whole new array in
/// here, so the column is written exactly as computed. Unknown ids match
/// no row and change nothing.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn set_also_known_as(pool: &PgPool, id: Uuid, aliases: &[String]) -> AppResult<()> {
    sqlx::query(SET_ALSO_KNOWN_AS_SQL)
        .bind(id)
        .bind(aliases.to_vec())
        .execute(pool)
        .await
        .map(drop)?;
    Ok(())
}

/// The `UPDATE` behind [`mark_moved`]: records the move target and stamps
/// the cooldown timestamp in one statement, refreshing `updated_at`.
const MARK_MOVED_SQL: &str = "UPDATE users
      SET moved_to_ap_id = $2,
          last_move_at = now(),
          updated_at = now()
    WHERE id = $1";

/// Records that user `id` moved to the actor `target_ap_id`: writes
/// [`moved_to_ap_id`](super::User::moved_to_ap_id) and stamps
/// [`last_move_at`](super::User::last_move_at) with `now()`, the pair the move
/// endpoint's once-per-30-days cooldown reads back.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn mark_moved(pool: &PgPool, id: Uuid, target_ap_id: &str) -> AppResult<()> {
    sqlx::query(MARK_MOVED_SQL)
        .bind(id)
        .bind(target_ap_id)
        .execute(pool)
        .await
        .map(drop)?;
    Ok(())
}

/// The `UPDATE` behind [`set_moved_to`]: writes the move target and
/// refreshes `updated_at`, and nothing else — the cooldown stamp
/// [`mark_moved`] writes is deliberately out of this statement.
const SET_MOVED_TO_SQL: &str =
    "UPDATE users SET moved_to_ap_id = $2, updated_at = now() WHERE id = $1";

/// Mirrors what a remote actor's document says about its move: stores
/// `target_ap_id` as user `id`'s [`moved_to_ap_id`](super::User::moved_to_ap_id),
/// or clears it again when the document no longer names a target.
///
/// This is the mirror writer, for the inbound `Update` of a remote account:
/// unlike [`mark_moved`] it stamps no [`last_move_at`](super::User::last_move_at),
/// because that column is the cooldown of a move *this* instance performed,
/// and a mirror only records what its peer declared. Unknown ids match no
/// row and change nothing.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn set_moved_to(pool: &PgPool, id: Uuid, target_ap_id: Option<&str>) -> AppResult<()> {
    sqlx::query(SET_MOVED_TO_SQL)
        .bind(id)
        .bind(target_ap_id)
        .execute(pool)
        .await
        .map(drop)?;
    Ok(())
}

/// The `UPDATE` behind [`set_settings_store`]: writes the whole client
/// settings blob in one statement and refreshes `updated_at`.
const SET_SETTINGS_STORE_SQL: &str =
    "UPDATE users SET settings_store = $2, updated_at = now() WHERE id = $1";

/// Stores `store` as user `id`'s `settings_store` column, replacing what
/// was there.
///
/// The settings endpoints keep the merge logic (deep merge, `null`
/// removal) in the API layer and hand the whole new blob in here, so the
/// jsonb column is written exactly as computed — the same shape the
/// account entity round-trips. Unknown ids match no row and change
/// nothing.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn set_settings_store(pool: &PgPool, id: Uuid, store: &Value) -> AppResult<()> {
    sqlx::query(SET_SETTINGS_STORE_SQL)
        .bind(id)
        .bind(store.clone())
        .execute(pool)
        .await
        .map(drop)?;
    Ok(())
}

/// The `UPDATE` behind [`set_preferred_frontend`]: writes the chosen
/// frontend and refreshes `updated_at`.
const SET_PREFERRED_FRONTEND_SQL: &str =
    "UPDATE users SET preferred_frontend = $2, updated_at = now() WHERE id = $1";

/// Stores `frontend` as user `id`'s `preferred_frontend` column: the
/// frontend the account picked at `PUT /api/v1/pleroma/preferred_frontend`
/// (a name the endpoint has validated against the configured list).
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn set_preferred_frontend(pool: &PgPool, id: Uuid, frontend: &str) -> AppResult<()> {
    sqlx::query(SET_PREFERRED_FRONTEND_SQL)
        .bind(id)
        .bind(frontend)
        .execute(pool)
        .await
        .map(drop)?;
    Ok(())
}

/// The `UPDATE` behind [`set_email_notifications`]: writes the flag and
/// refreshes `updated_at`.
const SET_EMAIL_NOTIFICATIONS_SQL: &str =
    "UPDATE users SET email_notifications = $2, updated_at = now() WHERE id = $1";

/// Stores `enabled` as user `id`'s [`email_notifications`](super::User::email_notifications)
/// flag, replacing what was there.
///
/// This is the switch Pleroma's `User.switch_email_notifications`
/// (`lib/pleroma/user.ex`) moves. Unknown ids match no row and change
/// nothing.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn set_email_notifications(pool: &PgPool, id: Uuid, enabled: bool) -> AppResult<()> {
    sqlx::query(SET_EMAIL_NOTIFICATIONS_SQL)
        .bind(id)
        .bind(enabled)
        .execute(pool)
        .await
        .map(drop)?;
    Ok(())
}

/// The `UPDATE` behind [`set_nickname`]: renames user `id` unless another
/// row already holds the nickname (the same ownership probe
/// [`upsert_remote_user`](crate::federation::remote_actor::upsert_remote_user)
/// runs), in one statement so the check and the write cannot interleave.
const SET_NICKNAME_SQL: &str = "UPDATE users
      SET nickname = $2, updated_at = now()
    WHERE id = $1
      AND NOT EXISTS (SELECT 1 FROM users WHERE nickname = $2::citext AND id <> $1)
RETURNING *";

/// Renames user `id` to `nickname` and answers the updated row.
///
/// Used when a remote mirror is vouched for under a handle other than
/// `preferredUsername@host` (a split-domain account). The rename is refused
/// when a different row already holds the nickname: the answer is then
/// [`None`], the same as for an unknown `id`, and nothing changes.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn set_nickname(pool: &PgPool, id: Uuid, nickname: &str) -> AppResult<Option<User>> {
    let user = sqlx::query_as::<_, User>(SET_NICKNAME_SQL)
        .bind(id)
        .bind(nickname)
        .fetch_optional(pool)
        .await?;
    Ok(user)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{
        models::user::{insert, local_ap_id, test_support::*},
        util::id::new_id,
    };

    #[tokio::test]
    async fn update_counts_adds_atomically_and_clamps_at_zero() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let user = insert(&pool, &local_new("dave")).await.unwrap();
        update_counts(
            &pool,
            user.id,
            CountDelta {
                followers: 4,
                following: 2,
                statuses: 7,
            },
        )
        .await
        .unwrap();

        // Two concurrent +1s must both land: the arithmetic is in the
        // UPDATE itself, not a read-modify-write.
        let plus_one = CountDelta {
            followers: 1,
            following: 0,
            statuses: 0,
        };
        let (left, right) = tokio::join!(
            update_counts(&pool, user.id, plus_one),
            update_counts(&pool, user.id, plus_one)
        );
        left.unwrap();
        right.unwrap();

        let after = fetch(&pool, user.id).await;
        assert_eq!(after.followers_count, 6, "both concurrent steps landed");
        assert_eq!(after.following_count, 2, "the untouched step stays");
        assert_eq!(after.statuses_count, 7, "the untouched step stays");

        update_counts(
            &pool,
            user.id,
            CountDelta {
                followers: -100,
                following: -100,
                statuses: -100,
            },
        )
        .await
        .unwrap();
        let clamped = fetch(&pool, user.id).await;
        assert_eq!(
            (
                clamped.followers_count,
                clamped.following_count,
                clamped.statuses_count
            ),
            (0, 0, 0),
            "counts never go negative"
        );

        // An unknown id is a no-op, not an error.
        update_counts(&pool, new_id(), CountDelta::default())
            .await
            .unwrap();
        let still = fetch(&pool, user.id).await;
        assert_eq!(still.followers_count, 0, "a no-op update changed nothing");
    }

    #[tokio::test]
    async fn set_also_known_as_replaces_the_array() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = insert(&pool, &local_new("alice")).await.unwrap();
        let first = vec![local_ap_id(BASE, "alicia")];
        set_also_known_as(&pool, alice.id, &first).await.unwrap();
        let stored = fetch(&pool, alice.id).await;
        assert_eq!(
            stored.also_known_as, first,
            "the written array is what reads back"
        );

        set_also_known_as(&pool, alice.id, &[]).await.unwrap();
        assert!(
            fetch(&pool, alice.id).await.also_known_as.is_empty(),
            "an empty array clears what was there"
        );

        // An unknown id matches no row and is not an error.
        set_also_known_as(&pool, new_id(), &first).await.unwrap();
    }

    #[tokio::test]
    async fn mark_moved_records_target_and_stamp() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = insert(&pool, &local_new("alice")).await.unwrap();
        assert!(alice.last_move_at.is_none(), "the fixture never moved");
        let target = "https://other.example/users/alice";
        mark_moved(&pool, alice.id, target).await.unwrap();
        let moved = fetch(&pool, alice.id).await;
        assert_eq!(
            moved.moved_to_ap_id.as_deref(),
            Some(target),
            "the target actor id is recorded"
        );
        assert!(
            moved.last_move_at.is_some(),
            "the cooldown stamp is set to the move time"
        );
    }

    #[tokio::test]
    async fn set_moved_to_mirrors_the_target_without_a_stamp() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = insert(&pool, &local_new("alice")).await.unwrap();
        let target = "https://other.example/users/alice";
        set_moved_to(&pool, alice.id, Some(target)).await.unwrap();
        let moved = fetch(&pool, alice.id).await;
        assert_eq!(
            moved.moved_to_ap_id.as_deref(),
            Some(target),
            "the mirrored target is recorded"
        );
        assert!(
            moved.last_move_at.is_none(),
            "a mirror write is not a move this instance performed, so no cooldown stamp"
        );

        set_moved_to(&pool, alice.id, None).await.unwrap();
        assert!(
            fetch(&pool, alice.id).await.moved_to_ap_id.is_none(),
            "None clears the target again"
        );

        // An unknown id matches no row and is not an error.
        set_moved_to(&pool, new_id(), Some(target)).await.unwrap();
    }

    #[tokio::test]
    async fn set_settings_store_replaces_the_blob() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = insert(&pool, &local_new("alice")).await.unwrap();
        assert_eq!(alice.settings_store, json!({}), "the fixture starts empty");
        set_settings_store(&pool, alice.id, &json!({"pleroma_fe": {"theme": "dark"}}))
            .await
            .unwrap();
        let stored = fetch(&pool, alice.id).await;
        assert_eq!(
            stored.settings_store,
            json!({"pleroma_fe": {"theme": "dark"}}),
            "the written blob is what reads back"
        );

        set_settings_store(&pool, alice.id, &json!({}))
            .await
            .unwrap();
        assert_eq!(
            fetch(&pool, alice.id).await.settings_store,
            json!({}),
            "an empty object clears what was there"
        );

        // An unknown id matches no row and is not an error.
        set_settings_store(&pool, new_id(), &json!({"x": 1}))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn set_preferred_frontend_stores_the_choice() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = insert(&pool, &local_new("alice")).await.unwrap();
        assert!(alice.preferred_frontend.is_none(), "no choice yet");
        set_preferred_frontend(&pool, alice.id, "pleroma-fe")
            .await
            .unwrap();
        assert_eq!(
            fetch(&pool, alice.id).await.preferred_frontend.as_deref(),
            Some("pleroma-fe"),
            "the chosen frontend is what reads back"
        );

        set_preferred_frontend(&pool, alice.id, "mochi-pub")
            .await
            .unwrap();
        assert_eq!(
            fetch(&pool, alice.id).await.preferred_frontend.as_deref(),
            Some("mochi-pub"),
            "a new choice replaces the old one"
        );
    }

    #[tokio::test]
    async fn set_email_notifications_stores_the_flag() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = insert(&pool, &local_new("alice")).await.unwrap();
        assert!(
            alice.email_notifications.get(),
            "an account starts opted in"
        );

        set_email_notifications(&pool, alice.id, false)
            .await
            .unwrap();
        assert!(
            !fetch(&pool, alice.id).await.email_notifications.get(),
            "the unsubscribe write is what reads back"
        );

        set_email_notifications(&pool, alice.id, true)
            .await
            .unwrap();
        assert!(
            fetch(&pool, alice.id).await.email_notifications.get(),
            "and opting back in flips it again"
        );

        // An unknown id matches no row and is not an error.
        set_email_notifications(&pool, new_id(), false)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn set_nickname_renames_unless_another_row_holds_it() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = insert(&pool, &local_new("alice")).await.unwrap();
        let bob = insert(&pool, &local_new("bob")).await.unwrap();

        let renamed = set_nickname(&pool, alice.id, "alice@mock.example")
            .await
            .unwrap()
            .expect("a free nickname is taken");
        assert_eq!(renamed.nickname, "alice@mock.example", "the row answers");
        assert_eq!(fetch(&pool, alice.id).await.nickname, "alice@mock.example");

        let refused = set_nickname(&pool, bob.id, "ALICE@mock.example")
            .await
            .unwrap();
        assert!(refused.is_none(), "another row holds it, case folded");
        assert_eq!(fetch(&pool, bob.id).await.nickname, "bob", "bob untouched");

        let same = set_nickname(&pool, alice.id, "alice@mock.example")
            .await
            .unwrap();
        assert!(same.is_some(), "a row may keep its own nickname");
        let unknown = set_nickname(&pool, new_id(), "ghost@mock.example")
            .await
            .unwrap();
        assert!(unknown.is_none(), "an unknown id matches no row");
    }
}
