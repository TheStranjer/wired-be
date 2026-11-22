//! The run handler's tests: the stored archive and marked row a full pass
//! leaves, the rerun that converges on the same key, the discards for
//! unusable arguments, a vanished row and a vanished owner, the retry a
//! closed database buys the run and the purge alike, and the wiring of the
//! two kinds into the job and periodic registries.

use std::{fs::File, time::Duration};

use serde_json::json;
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;
use zip::ZipArchive;

use super::*;
use crate::{
    jobs::{
        QUEUE_BACKGROUND,
        backups::{
            archive::{ACTOR_ENTRY, BOOKMARKS_ENTRY, LIKES_ENTRY, OUTBOX_ENTRY},
            test_support::{
                FIRST_ATTEMPT, app_state, like, mark, post, purge, run, uploads_state, user,
            },
        },
        scheduler::periodic_jobs,
    },
    models::status::{Status, Visibility},
    uploads,
};

/// Test state over a pool that cannot answer: closed before it ever
/// connected, which is how a handler sees a database that went away.
async fn broken_state() -> AppState {
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://localhost/unused")
        .expect("a lazy pool needs no database");
    pool.close().await;
    app_state(&pool)
}

/// The entry names of the zip file standing at `path`, in archive
/// order — the read-back of a stored archive, opened by path because
/// the run's temp file is long gone.
fn stored_names(path: &std::path::Path) -> Vec<String> {
    let file = File::open(path).expect("the stored archive is there");
    let zip = ZipArchive::new(file).expect("the stored archive reads back");
    zip.file_names()
        .map(|name| name.expect("entry names read").into_owned())
        .collect()
}

/// Asserts that the last segment of `key` is `archive-alice-<14 digits>-<43
/// url-safe characters>.zip`: the name carries an unguessable suffix.
fn assert_unguessable_archive_key(key: &str) {
    let last = key.rsplit('/').next().expect("a key has a segment");
    let rest = last
        .strip_prefix("archive-alice-")
        .and_then(|rest| rest.strip_suffix(".zip"))
        .expect("the stored name is alice's archive");
    let (stamp, random) = rest
        .split_once('-')
        .expect("a random part follows the stamp");
    assert!(
        stamp.len() == 14 && stamp.chars().all(|c| c.is_ascii_digit()),
        "the stamp is 14 digits, not {stamp}"
    );
    assert!(
        random.len() == 43
            && random
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
        "the random part is 43 url-safe characters, not {random}"
    );
}

#[tokio::test]
async fn the_run_handler_stores_the_archive_and_marks_the_row() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let (dir, state) = uploads_state(&pool);
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    let _alices_post: Status = post(&pool, &alice, Visibility::Public, "alice posts").await;
    let bobs_post: Status = post(&pool, &bob, Visibility::Public, "bob posts").await;
    like(&pool, &alice, bobs_post.id).await;
    mark(&pool, &alice, bobs_post.id).await;
    let row = backup::create(&pool, &alice)
        .await
        .expect("the fixture backup creates");

    // Through the registry's binding, so the pass also proves that the
    // kind the endpoint enqueues stands behind this module's handler.
    let handler = crate::job_registry()
        .get(crate::api::backups::KIND_RUN)
        .expect("the run kind is registered");
    handler(
        state.clone(),
        FIRST_ATTEMPT,
        json!({ "backup_id": row.id.to_string() }),
    )
    .await
    .expect("the run job completes");

    let key = storage_key(alice.id, &row.file_name);
    uploads::check_key(&key).expect("the run's key is legal");
    assert_unguessable_archive_key(&key);
    let marked = backup::get_by_id(&pool, row.id)
        .await
        .expect("the row reads")
        .expect("the request stands");
    assert!(marked.processed, "the run marks the row processed");
    assert_eq!(
        marked.storage_key.as_deref(),
        Some(key.as_str()),
        "and records the key it stored under"
    );
    assert!(marked.file_size > 0, "the archive's byte count stands too");
    assert_eq!(
        marked.file_name, row.file_name,
        "the request's own name is kept"
    );

    let path = dir.path().join(&key);
    assert!(
        path.is_file(),
        "the archive lives at the key in the uploads dir"
    );
    assert_eq!(
        u64::try_from(marked.file_size).expect("a file size fits a u64"),
        std::fs::metadata(&path)
            .expect("the stored file is there")
            .len(),
        "file_size says what the stored file holds"
    );
    assert_eq!(
        stored_names(&path),
        [ACTOR_ENTRY, OUTBOX_ENTRY, LIKES_ENTRY, BOOKMARKS_ENTRY],
        "the stored zip opens with the four entries"
    );
}

#[tokio::test]
async fn a_second_run_pass_converges_on_the_same_backup() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let (dir, state) = uploads_state(&pool);
    let alice = user(&pool, "alice").await;
    let _post: Status = post(&pool, &alice, Visibility::Public, "one post").await;
    let row = backup::create(&pool, &alice)
        .await
        .expect("the fixture backup creates");

    run(&state, row.id).await.expect("the first pass runs");
    let first = backup::get_by_id(&pool, row.id)
        .await
        .expect("the row reads")
        .expect("the request stands");
    run(&state, row.id).await.expect("the rerun converges");
    let second = backup::get_by_id(&pool, row.id)
        .await
        .expect("the row reads again")
        .expect("it still stands");

    assert!(second.processed, "still processed");
    assert_eq!(
        second.storage_key, first.storage_key,
        "the same key, overwritten in place"
    );
    let path = dir.path().join(
        second
            .storage_key
            .as_deref()
            .expect("the rerun kept the key"),
    );
    assert!(path.is_file(), "one file stands at the key");
    assert_eq!(
        u64::try_from(second.file_size).expect("a file size fits a u64"),
        std::fs::metadata(&path)
            .expect("the stored file is there")
            .len(),
        "the recorded size still describes the stored file"
    );
    assert_eq!(
        stored_names(&path).len(),
        4,
        "and the archive still opens with its four entries"
    );
}

#[tokio::test]
async fn unusable_args_a_missing_row_and_a_missing_user_discard() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let state = app_state(db.pool());

    let bad = handle_run(
        state.clone(),
        FIRST_ATTEMPT,
        json!({ "id": "not-a-backup" }),
    )
    .await;
    assert!(
        matches!(&bad, Err(JobError::Discard(detail)) if detail.contains("unusable")),
        "unusable arguments must discard, not {bad:?}"
    );

    let missing = run(&state, Uuid::now_v7()).await;
    assert!(
        matches!(&missing, Err(JobError::Discard(detail)) if detail.contains("no longer exists")),
        "a vanished row must discard, not {missing:?}"
    );

    // A row cannot outlive its owner in the table — the foreign key's
    // cascade takes it — so the owner arm is what races between the
    // handler's two reads: it is proven on the read itself.
    let orphan = load_owner(&state, Uuid::now_v7()).await;
    assert!(
        matches!(&orphan, Err(JobError::Discard(detail)) if detail.contains("user") && detail.contains("no longer exists")),
        "a vanished owner must discard, not {orphan:?}"
    );
    assert!(
        matches!(&orphan, Err(JobError::Discard(detail)) if detail.contains("no longer exists")),
        "and it names the missing account: {orphan:?}"
    );
}

#[tokio::test]
async fn a_closed_pool_retries_the_run_and_the_purge() {
    let state = broken_state().await;

    let run = run(&state, Uuid::now_v7()).await;
    assert!(
        matches!(&run, Err(JobError::Retry(detail)) if detail.starts_with("backup row read failed")),
        "a closed pool retries the run, not {run:?}"
    );

    let swept = purge(&state).await;
    assert!(
        matches!(&swept, Err(JobError::Retry(detail)) if detail.starts_with("backup purge read failed")),
        "a closed pool retries the purge, not {swept:?}"
    );
}

#[test]
fn the_kinds_are_registered_and_the_purge_is_scheduled_daily() {
    assert_eq!(
        crate::api::backups::KIND_RUN,
        "backups.run",
        "the kind the endpoint enqueues"
    );
    assert_eq!(KIND_PURGE, "backups.purge", "the ticket's purge kind");

    let registry = crate::job_registry();
    assert!(
        registry.get(crate::api::backups::KIND_RUN).is_some(),
        "the worker knows the run kind"
    );
    assert!(
        registry.get(KIND_PURGE).is_some(),
        "the worker knows the purge kind"
    );

    let job = periodic_jobs()
        .into_iter()
        .find(|job| job.kind == KIND_PURGE)
        .expect("the purge is on the periodic schedule");
    assert_eq!(job.queue, QUEUE_BACKGROUND, "on the maintenance queue");
    assert_eq!(job.every, Duration::from_hours(24), "once a day");
}
