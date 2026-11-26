//! The data move of `20261126114905_user_disabled_at.sql` (WBE-883): a
//! local account whose only deactivation is a logged `disable` becomes a
//! disabled, active one.

use sqlx::{AssertSqlSafe, PgPool};
use uuid::Uuid;

use crate::{
    models::{moderation_log, user::User},
    test_fixtures::{local_user, remote_user},
};

/// The migration's own SQL, from its data move on: the schema it adds is
/// already in the slot.
const MIGRATION_SQL: &str =
    include_str!("../../../../migrations/20261126114905_user_disabled_at.sql");

/// One account's flags: `(is_active, local_deactivated, disabled)`.
type Flags = (bool, bool, bool);

/// Replays the migration's data move.
async fn migrate(pool: &PgPool) {
    let (_schema, data) = MIGRATION_SQL
        .split_once("UPDATE users")
        .expect("the migration moves data");
    // The SQL is the migration file's own text, a compile-time constant.
    let _migrated = sqlx::raw_sql(AssertSqlSafe(format!("UPDATE users{data}")))
        .execute(pool)
        .await
        .expect("the data move runs");
}

/// Puts `id` in the state the WBE-878 disable left an account in: inactive,
/// locally deactivated, no suspension stamp.
async fn old_disable(pool: &PgPool, id: Uuid) {
    let touched = sqlx::query(
        "UPDATE users SET is_active = false, local_deactivated = true, suspended_at = NULL
         WHERE id = $1",
    )
    .bind(id)
    .execute(pool)
    .await
    .expect("the old state writes")
    .rows_affected();
    assert_eq!(touched, 1, "the fixture row exists");
}

/// Logs a `disable` of `user` by `admin`, an hour ago.
async fn log_disable(pool: &PgPool, admin: &User, user: &User) {
    let entry = moderation_log::account_action(pool, admin, user, "disable")
        .await
        .expect("the entry records");
    let aged = sqlx::query(
        "UPDATE moderation_log SET inserted_at = now() - interval '1 hour' WHERE id = $1",
    )
    .bind(entry.id)
    .execute(pool)
    .await
    .expect("the entry ages")
    .rows_affected();
    assert_eq!(aged, 1, "the entry exists");
}

/// The flags of `id`.
async fn flags(pool: &PgPool, id: Uuid) -> Flags {
    sqlx::query_as(
        "SELECT is_active, local_deactivated, disabled_at IS NOT NULL FROM users WHERE id = $1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .expect("the flags read")
}

/// Only a local account whose last word is a logged `disable` moves.
#[tokio::test]
async fn only_a_logged_disable_moves_to_the_new_flag() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let admin = local_user(&pool, "admin").await;
    let disabled = local_user(&pool, "disabled").await;
    let reenabled = local_user(&pool, "reenabled").await;
    let suspended = local_user(&pool, "suspended").await;
    let unlogged = local_user(&pool, "unlogged").await;
    let mirror = remote_user(&pool, "mirror").await;
    let deleted = local_user(&pool, "deleted").await;
    for user in [
        &disabled, &reenabled, &suspended, &unlogged, &mirror, &deleted,
    ] {
        old_disable(&pool, user.id).await;
    }
    for user in [&disabled, &reenabled, &suspended, &mirror, &deleted] {
        log_disable(&pool, &admin, user).await;
    }
    let _activated = moderation_log::activate(&pool, &admin, std::slice::from_ref(&reenabled))
        .await
        .expect("the activation records");
    let stamped = sqlx::query("UPDATE users SET suspended_at = now() WHERE id = $1")
        .bind(suspended.id)
        .execute(&pool)
        .await
        .expect("the suspension stamps")
        .rows_affected();
    let marked = sqlx::query("UPDATE users SET deleted_at = now() WHERE id = $1")
        .bind(deleted.id)
        .execute(&pool)
        .await
        .expect("the deletion marks")
        .rows_affected();
    assert_eq!((stamped, marked), (1, 1), "the fixtures exist");

    migrate(&pool).await;

    assert_eq!(
        flags(&pool, disabled.id).await,
        (true, false, true),
        "reactivated, and disabled by the new flag"
    );
    for (user, label) in [
        (&reenabled, "a later `activate` wins"),
        (&suspended, "a suspension stays a suspension"),
        (&unlogged, "no `disable` on record"),
        (&mirror, "a remote mirror"),
        (&deleted, "a deleted account"),
    ] {
        assert_eq!(
            flags(&pool, user.id).await,
            (false, true, false),
            "left alone: {label}"
        );
    }
}
