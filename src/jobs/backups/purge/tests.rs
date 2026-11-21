//! The purge handler's tests: the daily sweep of expired rows with their
//! files — and of the aged row whose file is already gone — the abandoned
//! request that loses just its row, the young archive the pass leaves
//! untouched, and the disabled stand-down that deletes nothing.

use bytes::Bytes;
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use super::*;
use crate::{
    config::{Config, Toggle},
    jobs::backups::{
        KIND_PURGE,
        run::{ARCHIVE_CONTENT_TYPE, storage_key},
        test_support::{FIRST_ATTEMPT, purge, state_with_config, uploads_state, user},
    },
    models::user::User,
};

/// Days past the test config's 30-day validity window an aged purge
/// fixture is rewound to.
const AGED_DAYS: i64 = 31;

/// Moves the backup `id`'s `inserted_at` `days` into the past, so a
/// purge test is not a race with the clock. Asserts the row was there
/// to move.
async fn age(pool: &PgPool, id: Uuid, days: i64) {
    let moved = sqlx::query("UPDATE backups SET inserted_at = $1 WHERE id = $2")
        .bind(Utc::now() - chrono::TimeDelta::days(days))
        .bind(id)
        .execute(pool)
        .await
        .expect("the fixture row ages")
        .rows_affected();
    assert_eq!(moved, 1, "the row to rewind exists");
}

/// A finished, marked backup row for `owner` with a file of `bytes`
/// standing behind a hand-made key: a purge fixture the way
/// [`handle_run`](crate::jobs::backups::handle_run) would leave one, but
/// with the key independent of the second-granular `file_name` so
/// same-second fixtures cannot collide.
async fn processed_backup(
    state: &AppState,
    owner: &User,
    name: &str,
    bytes: &[u8],
) -> (backup::Backup, String) {
    let row = backup::create(&state.db, owner)
        .await
        .expect("the fixture backup creates");
    let key = storage_key(owner.id, name);
    state
        .uploader
        .put(&key, Bytes::from(bytes.to_vec()), ARCHIVE_CONTENT_TYPE)
        .await
        .expect("the fixture archive stores");
    let size = bytes
        .len()
        .try_into()
        .expect("a fixture's length is an i64 size");
    let _marked = backup::mark_processed(&state.db, row.id, size, &key)
        .await
        .expect("the mark runs")
        .expect("the requested row is there");
    (row, key)
}

#[tokio::test]
async fn the_purge_sweeps_expired_rows_their_files_and_abandoned_requests() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let (dir, state) = uploads_state(&pool);
    let alice = user(&pool, "alice").await;

    // An aged, finished archive with its file on disk...
    let (stored, stored_key) =
        processed_backup(&state, &alice, "stored.zip", b"a finished archive").await;
    age(&pool, stored.id, AGED_DAYS).await;
    // ...an aged finished row whose file is already gone (tolerated,
    // the row is all that is left to sweep)...
    let (lapsed, lapsed_key) = processed_backup(&state, &alice, "lapsed.zip", b"bytes").await;
    state
        .uploader
        .delete(&lapsed_key)
        .await
        .expect("the fixture file goes first");
    age(&pool, lapsed.id, AGED_DAYS).await;
    // ...an aged request whose job never marked anything...
    let abandoned = backup::create(&pool, &alice)
        .await
        .expect("the fixture backup creates");
    age(&pool, abandoned.id, AGED_DAYS).await;
    // ...and a young processed archive the sweep must leave untouched.
    let (fresh, fresh_key) = processed_backup(&state, &alice, "fresh.zip", b"young").await;

    // Through the registry's binding: this module's sweep stands
    // behind the kind the scheduler enqueues.
    let handler = crate::job_registry()
        .get(KIND_PURGE)
        .expect("the purge kind is registered");
    handler(state.clone(), FIRST_ATTEMPT, json!({}))
        .await
        .expect("the purge pass completes");

    for gone in [stored.id, lapsed.id, abandoned.id] {
        assert!(
            backup::get_by_id(&pool, gone)
                .await
                .expect("the swept row reads")
                .is_none(),
            "row {gone} is past the window and gone"
        );
    }
    assert!(
        !dir.path().join(&stored_key).exists(),
        "the aged archive's file went with its row"
    );
    let kept = backup::get_by_id(&pool, fresh.id)
        .await
        .expect("the young row reads")
        .expect("it stands");
    assert!(kept.processed, "a young row is nobody's purge");
    assert!(
        dir.path().join(&fresh_key).is_file(),
        "and its file stays where it is"
    );
}

#[tokio::test]
async fn the_purge_stands_down_when_the_config_says() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let dir = tempfile::TempDir::new().expect("a temp uploads dir");
    let mut config = Config::for_tests();
    config.uploads_dir = dir.path().to_path_buf();
    config.backup_purge = Toggle::new(false);
    let state = state_with_config(&pool, config);
    let alice = user(&pool, "alice").await;

    let (row, key) = processed_backup(&state, &alice, "disabled.zip", b"bytes").await;
    age(&pool, row.id, AGED_DAYS).await;

    purge(&state)
        .await
        .expect("a disabled pass is a clean no-op");

    assert!(
        backup::get_by_id(&pool, row.id)
            .await
            .expect("the row reads")
            .is_some(),
        "the aged row stands: BACKUP_PURGE off deletes nothing"
    );
    assert!(
        dir.path().join(&key).is_file(),
        "and so does its stored file"
    );
}
