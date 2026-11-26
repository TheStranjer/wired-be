//! The corrective migration of the WBE-873 backfill (WBE-882): a remote
//! mirror stranded as locally deactivated is handed back to its origin.

use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use super::upsert_remote_user;
use crate::{
    federation::remote_actor::{
        parse_actor,
        test_support::{MASTODON, fixture},
    },
    models::{moderation_log, user::User},
    test_fixtures::{local_user, remote_user},
};

/// The migration's own SQL, replayed against a slot that already holds the
/// migrated schema.
const MIGRATION_SQL: &str =
    include_str!("../../../../migrations/20261126113030_user_remote_suspension_backfill_fix.sql");

/// One account's flags: `(is_active, local_deactivated, remote_suspended, suspended)`.
type Flags = (bool, bool, bool, bool);

/// Puts `id` in the state the shipped backfills left a deactivated account in.
async fn backfilled(pool: &PgPool, id: Uuid) {
    let touched = sqlx::query(
        "UPDATE users SET is_active = false, local_deactivated = true,
             remote_suspended = true, suspended_at = now()
         WHERE id = $1",
    )
    .bind(id)
    .execute(pool)
    .await
    .expect("the backfilled state writes")
    .rows_affected();
    assert_eq!(touched, 1, "the fixture row exists");
}

/// The flags of `id`.
async fn flags(pool: &PgPool, id: Uuid) -> Flags {
    sqlx::query_as(
        "SELECT is_active, local_deactivated, remote_suspended, suspended_at IS NOT NULL
         FROM users WHERE id = $1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .expect("the flags read")
}

/// Upserts the Mastodon fixture actor, with `suspended` as given.
async fn fetch(pool: &PgPool, suspended: bool) -> User {
    let mut document = fixture(MASTODON);
    if suspended {
        document["suspended"] = json!(true);
    }
    upsert_remote_user(pool, parse_actor(&document).unwrap())
        .await
        .unwrap()
}

/// Runs the corrective migration.
async fn migrate(pool: &PgPool) {
    let _migrated = sqlx::raw_sql(MIGRATION_SQL)
        .execute(pool)
        .await
        .expect("the migration runs");
}

/// Only the remote mirror with no local deactivation on record is handed back.
#[tokio::test]
async fn only_an_unlogged_live_remote_mirror_is_handed_back_to_its_origin() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let admin = local_user(&pool, "admin").await;
    let stranded = remote_user(&pool, "stranded").await;
    let moderated = remote_user(&pool, "moderated").await;
    let deleted = remote_user(&pool, "deleted").await;
    let local = local_user(&pool, "local").await;
    for user in [&stranded, &moderated, &deleted, &local] {
        backfilled(&pool, user.id).await;
    }
    let _logged = moderation_log::deactivate(&pool, &admin, std::slice::from_ref(&moderated))
        .await
        .expect("the log entry records");
    let marked = sqlx::query("UPDATE users SET deleted_at = now() WHERE id = $1")
        .bind(deleted.id)
        .execute(&pool)
        .await
        .expect("the deletion marks")
        .rows_affected();
    assert_eq!(marked, 1, "the deleted fixture exists");

    migrate(&pool).await;

    assert_eq!(
        flags(&pool, stranded.id).await,
        (false, false, true, false),
        "handed back: still inactive, remotely suspended, no local origin"
    );
    assert_eq!(
        flags(&pool, moderated.id).await,
        (false, true, true, true),
        "an admin deactivated it: the local deactivation stands"
    );
    assert_eq!(
        flags(&pool, deleted.id).await,
        (false, true, true, true),
        "a deleted mirror is left alone"
    );
    assert_eq!(
        flags(&pool, local.id).await,
        (false, true, true, true),
        "a local account is untouched"
    );
}

/// Each of the three local deactivation actions keeps a mirror locally deactivated.
#[tokio::test]
async fn a_suspend_or_disable_entry_also_keeps_the_local_deactivation() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let admin = local_user(&pool, "admin").await;
    let mut kept = Vec::new();
    for (nickname, action) in [("one", "suspend"), ("two", "disable")] {
        let user = remote_user(&pool, nickname).await;
        backfilled(&pool, user.id).await;
        let entry = moderation_log::deactivate(&pool, &admin, std::slice::from_ref(&user))
            .await
            .expect("the log entry records");
        let renamed = sqlx::query("UPDATE moderation_log SET action = $2 WHERE id = $1")
            .bind(entry.id)
            .bind(action)
            .execute(&pool)
            .await
            .expect("the action rewrites")
            .rows_affected();
        assert_eq!(renamed, 1, "the entry exists");
        kept.push(user.id);
    }

    migrate(&pool).await;

    for id in kept {
        assert_eq!(
            flags(&pool, id).await,
            (false, true, true, true),
            "a local suspend or disable keeps the deactivation"
        );
    }
}

/// A locally moderated mirror survives an `Update` after the migration (WBE-873).
#[tokio::test]
async fn a_logged_mirror_stays_deactivated_through_updates() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let admin = local_user(&pool, "admin").await;
    let alice = fetch(&pool, false).await;
    backfilled(&pool, alice.id).await;
    let _logged = moderation_log::deactivate(&pool, &admin, std::slice::from_ref(&alice))
        .await
        .expect("the log entry records");

    migrate(&pool).await;
    let updated = fetch(&pool, false).await;

    assert!(!updated.is_active.get(), "the local deactivation survives");
    assert_eq!(flags(&pool, alice.id).await, (false, true, false, true));
}

/// A corrected mirror reactivates once the actor document drops `suspended`.
#[tokio::test]
async fn a_corrected_mirror_reactivates_when_the_actor_drops_suspended() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = fetch(&pool, true).await;
    assert!(!alice.is_active.get(), "the origin suspended the mirror");
    backfilled(&pool, alice.id).await;

    migrate(&pool).await;
    assert_eq!(
        flags(&pool, alice.id).await,
        (false, false, true, false),
        "corrected and still inactive"
    );
    let restored = fetch(&pool, false).await;

    assert_eq!(restored.id, alice.id, "the same mirror");
    assert!(
        restored.is_active.get(),
        "the lifted suspension restores it"
    );
    assert_eq!(
        flags(&pool, alice.id).await,
        (true, false, false, false),
        "no origin keeps it inactive"
    );
}

/// A mirror still suspended by its origin stays inactive after the migration and an `Update`.
#[tokio::test]
async fn a_corrected_mirror_stays_inactive_while_the_actor_is_suspended() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = fetch(&pool, true).await;
    backfilled(&pool, alice.id).await;

    migrate(&pool).await;
    let still = fetch(&pool, true).await;

    assert!(!still.is_active.get(), "the origin still suspends it");
    assert_eq!(flags(&pool, alice.id).await, (false, false, true, false));
}

/// Replaying the migration changes nothing.
#[tokio::test]
async fn the_migration_is_idempotent() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let stranded = remote_user(&pool, "stranded").await;
    backfilled(&pool, stranded.id).await;

    migrate(&pool).await;
    migrate(&pool).await;

    assert_eq!(flags(&pool, stranded.id).await, (false, false, true, false));
}
