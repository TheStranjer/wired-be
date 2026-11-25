//! The orphaned-media sweep's tests: the stale unposted local rows a
//! six-hourly pass reclaims, the fresh, mirrored and chat-referenced rows it
//! leaves, the stored file that goes with a reclaimed row and the row a
//! refused file delete keeps, the cursor that walks a backlog larger than one
//! batch, the retry a database it cannot reach buys it, and the two
//! registries its kind is wired into.

use bytes::Bytes;
use serde_json::json;
use sqlx::query_scalar;
use uuid::Uuid;

use super::*;
use crate::{
    config::{Config, TEST_BASE_URL},
    jobs::{
        JobError, QUEUE_BACKGROUND,
        maintenance::test_support::{
            FIRST_ATTEMPT, SIX_HOURS, row_exists, state_with_config, user,
        },
        scheduler::periodic_jobs,
    },
    models::user::User,
    test_support::{app_state, broken_state},
    uploads::UploaderBackend,
};

/// Test state over `pool` whose uploads live in a fresh temporary
/// directory, together with that directory's guard and the backend a
/// test puts files through: the handler deletes from that same
/// directory, so the guard must outlive both (the pattern of
/// `src/uploads.rs`'s `backend_delegates_every_operation` test).
fn uploads_state(pool: &PgPool) -> (tempfile::TempDir, AppState, UploaderBackend) {
    let dir = tempfile::TempDir::new().expect("a temp uploads dir");
    let mut config = Config::for_tests();
    config.uploads_dir = dir.path().to_path_buf();
    let backend = UploaderBackend::from(&config);
    let state = state_with_config(pool, config);
    (dir, state, backend)
}

/// Inserts a `media_attachments` row aged `hours_old` into the past,
/// the sweep's inputs set as given: an unposted local upload is
/// `status_id` NULL and `remote_url` NULL with a `storage_key`; a
/// mirrored remote row is one with `remote_url` set. Returns its id.
async fn unposted_media_row(
    pool: &PgPool,
    owner: &User,
    storage_key: Option<&str>,
    remote_url: Option<&str>,
    hours_old: i32,
) -> Uuid {
    let id = Uuid::now_v7();
    let sql = "INSERT INTO media_attachments
                       (id, user_id, status_id, storage_key, remote_url, url, content_type,
                        inserted_at)
                   VALUES ($1, $2, NULL, $3, $4, $5, 'image/png',
                           now() - make_interval(hours => $6))";
    let written = query(sql)
        .bind(id)
        .bind(owner.id)
        .bind(storage_key)
        .bind(remote_url)
        .bind(format!("{TEST_BASE_URL}/media/{id}.png"))
        .bind(hours_old)
        .execute(pool)
        .await
        .expect("the media fixture inserts");
    assert_eq!(written.rows_affected(), 1, "one row was written");
    id
}

/// Inserts a chat between `owner` and `other` and one message from
/// `owner` carrying `media_id` as its attachment: the stored reference
/// that keeps an unposted media row out of the sweep's candidates.
async fn chat_message_with_media(pool: &PgPool, owner: &User, other: &User, media_id: Uuid) {
    let chat = Uuid::now_v7();
    let chat_rows = query("INSERT INTO chats (id, user_id, recipient_id) VALUES ($1, $2, $3)")
        .bind(chat)
        .bind(owner.id)
        .bind(other.id)
        .execute(pool)
        .await
        .expect("the chat fixture inserts");
    assert_eq!(chat_rows.rows_affected(), 1, "the chat row was written");
    let sql = "INSERT INTO chat_messages (id, sender_id, attachment_media_id)
                   VALUES ($1, $2, $3)";
    let message_rows = query(sql)
        .bind(Uuid::now_v7())
        .bind(owner.id)
        .bind(media_id)
        .execute(pool)
        .await
        .expect("the chat message fixture inserts");
    assert_eq!(
        message_rows.rows_affected(),
        1,
        "the message row was written"
    );
}

/// Inserts a `scheduled_statuses` row for `owner` due in a week, its
/// `params` the given JSON: the stored reference that reserves the ids
/// inside `params -> 'media_ids'`. Returns the row's id.
async fn scheduled_row(pool: &PgPool, owner: &User, params: Value) -> Uuid {
    let id = Uuid::now_v7();
    let written = query(
        "INSERT INTO scheduled_statuses (id, user_id, scheduled_at, params)
              VALUES ($1, $2, now() + make_interval(days => 7), $3)",
    )
    .bind(id)
    .bind(owner.id)
    .bind(params)
    .execute(pool)
    .await
    .expect("the scheduled status fixture inserts");
    assert_eq!(written.rows_affected(), 1, "the schedule row was written");
    id
}

/// Stores `body` at `key` through the test backend and answers whether
/// the file is on disk under the temporary uploads directory.
async fn put_media_file(backend: &UploaderBackend, key: &str) {
    backend
        .put(key, Bytes::from_static(b"png bytes"), "image/png")
        .await
        .expect("the upload fixture stores");
}

/// Whether `dir` holds a file at relative `key`.
fn file_on_disk(dir: &std::path::Path, key: &str) -> bool {
    dir.join(key).is_file()
}

/// The sweep deletes exactly the stale unposted local orphans: a fresh
/// orphan (47 h), a stale row with a `remote_url`, and a stale row a
/// chat message references all stay.
#[tokio::test]
async fn the_media_sweep_deletes_only_stale_unposted_local_rows() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    let orphan = unposted_media_row(&pool, &alice, None, None, 49).await;
    let fresh = unposted_media_row(&pool, &alice, None, None, 47).await;
    let remote = unposted_media_row(
        &pool,
        &alice,
        None,
        Some("https://remote.example/media/x.png"),
        49,
    )
    .await;
    let in_chat = unposted_media_row(&pool, &alice, None, None, 49).await;
    chat_message_with_media(&pool, &alice, &bob, in_chat).await;

    let outcome = handle_sweep_media(app_state(&pool), FIRST_ATTEMPT, json!({})).await;

    assert_eq!(outcome, Ok(()), "a sweep over orphans and keeps succeeds");
    assert!(
        !row_exists(&pool, "media_attachments", orphan).await,
        "the stale unposted local row is gone"
    );
    assert!(
        row_exists(&pool, "media_attachments", fresh).await,
        "a 47-hour-old orphan is still inside its posting window"
    );
    assert!(
        row_exists(&pool, "media_attachments", remote).await,
        "a mirrored remote row is not this instance's file to delete"
    );
    assert!(
        row_exists(&pool, "media_attachments", in_chat).await,
        "a chat message still carries the row"
    );
}

/// The sweep removes the stored file before it removes the row, so a
/// reclaimed orphan leaves nothing behind on disk.
#[tokio::test]
async fn the_media_sweep_takes_the_stored_file_with_the_row() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let (dir, state, backend) = uploads_state(&pool);
    let alice = user(&pool, "alice").await;
    let key = format!("alice/{}/stale.png", Uuid::now_v7());
    put_media_file(&backend, &key).await;
    assert!(
        file_on_disk(dir.path(), &key),
        "the fixture file is on disk"
    );
    let orphan = unposted_media_row(&pool, &alice, Some(&key), None, 49).await;

    let outcome = handle_sweep_media(state, FIRST_ATTEMPT, json!({})).await;

    assert_eq!(outcome, Ok(()), "the sweep over a real file succeeds");
    assert!(
        !row_exists(&pool, "media_attachments", orphan).await,
        "the orphan row is gone"
    );
    assert!(
        !file_on_disk(dir.path(), &key),
        "and its stored file went with it"
    );
}

/// A file the uploader refuses to delete (the key names nothing on
/// disk) keeps its row and costs the pass nothing but the one skip:
/// the handler still returns `Ok`, and the deleted count covers only
/// the row it could reclaim, never the kept one.
#[tokio::test]
async fn a_row_whose_file_cannot_be_deleted_keeps_its_row() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let (_dir, state, _backend) = uploads_state(&pool);
    let alice = user(&pool, "alice").await;
    let stuck = unposted_media_row(&pool, &alice, Some("alice/missing.png"), None, 49).await;
    let clean = unposted_media_row(&pool, &alice, None, None, 49).await;

    let deleted = sweep_media(&state).await.expect("the sweep runs");

    assert_eq!(
        deleted, 1,
        "the count excludes the row whose file delete failed"
    );
    assert!(
        !row_exists(&pool, "media_attachments", clean).await,
        "the row with nothing to remove went"
    );
    assert!(
        row_exists(&pool, "media_attachments", stuck).await,
        "the stuck row waits for the next run"
    );

    let outcome = handle_sweep_media(state, FIRST_ATTEMPT, json!({})).await;
    assert_eq!(
        outcome,
        Ok(()),
        "an undeletable file is a warn, not a failure"
    );
    assert!(
        row_exists(&pool, "media_attachments", stuck).await,
        "and the job run keeps skipping it without going down"
    );
}

/// A row with no `storage_key` has no file step: it goes directly.
#[tokio::test]
async fn a_row_without_a_storage_key_deletes_without_a_file_step() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let (dir, state, _backend) = uploads_state(&pool);
    let alice = user(&pool, "alice").await;
    let keyless = unposted_media_row(&pool, &alice, None, None, 49).await;

    let outcome = handle_sweep_media(state, FIRST_ATTEMPT, json!({})).await;

    assert_eq!(outcome, Ok(()), "the keyless row is deletable bookkeeping");
    assert!(
        !row_exists(&pool, "media_attachments", keyless).await,
        "it goes without any file"
    );
    let leftovers = dir
        .path()
        .read_dir()
        .expect("the fresh uploads dir is readable")
        .count();
    assert_eq!(leftovers, 0, "the sweep created no stray file for it");
}

/// A backlog larger than one batch is swept by a single handler run:
/// the id cursor walks every batch in one pass, and a row kept after a
/// failed file delete does not re-loop inside it.
#[tokio::test]
async fn the_media_sweep_walks_more_than_one_batch_in_one_run() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let want = u64::try_from(MEDIA_BATCH_SIZE + 5).expect("the batch size is positive");
    let inserted = query(
        "INSERT INTO media_attachments (id, user_id, url, content_type, inserted_at)
             SELECT gen_random_uuid(), $1, 'https://test.example/media/bulk.png', 'image/png',
                    now() - make_interval(hours => 49)
               FROM generate_series(1, $2)",
    )
    .bind(alice.id)
    .bind(MEDIA_BATCH_SIZE + 5)
    .execute(&pool)
    .await
    .expect("the bulk media fixture inserts");
    assert_eq!(
        inserted.rows_affected(),
        want,
        "every seeded orphan landed in the table"
    );

    let outcome = handle_sweep_media(app_state(&pool), FIRST_ATTEMPT, json!({})).await;

    assert_eq!(outcome, Ok(()), "the multi-batch pass succeeds");
    let left = query_scalar::<_, i64>(
        "SELECT count(*) FROM media_attachments
              WHERE status_id IS NULL AND remote_url IS NULL",
    )
    .fetch_one(&pool)
    .await
    .expect("the rows read");
    assert_eq!(
        left, 0,
        "one handler run cleared more than {MEDIA_BATCH_SIZE} rows: \
             the cursor walked, not a single batch"
    );
}

/// The media sweep is wired into both registries the ticket names: the
/// worker can run its kind, and the scheduler places it on the
/// maintenance queue every six hours.
#[test]
fn the_media_kind_is_registered_and_scheduled() {
    let registry = crate::job_registry(&Config::for_tests());
    assert!(
        registry.get(KIND_MEDIA).is_some(),
        "the worker knows `{KIND_MEDIA}`"
    );
    assert_eq!(KIND_MEDIA, "maintenance.media", "the ticket's kind name");

    let job = periodic_jobs()
        .into_iter()
        .find(|job| job.kind == KIND_MEDIA)
        .expect("the sweep is on the periodic schedule");
    assert_eq!(job.queue, QUEUE_BACKGROUND, "on the maintenance queue");
    assert_eq!(job.every, SIX_HOURS, "once every six hours");
}

/// The handler the registry binds to the kind is this module's
/// [`handle_sweep_media`], not some other sweep: calling the registered
/// handler reclaims the stale orphan and keeps the fresh one.
#[tokio::test]
async fn the_registered_media_handler_is_this_sweep() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let orphan = unposted_media_row(&pool, &alice, None, None, 49).await;
    let fresh = unposted_media_row(&pool, &alice, None, None, 47).await;
    let registry = crate::job_registry(&Config::for_tests());
    let handler = registry
        .get(KIND_MEDIA)
        .expect("the sweep's kind is registered");

    let outcome = handler(app_state(&pool), FIRST_ATTEMPT, json!({})).await;

    assert_eq!(outcome, Ok(()), "the registered handler runs");
    assert!(
        !row_exists(&pool, "media_attachments", orphan).await,
        "and it is this module's sweep: the stale orphan is gone"
    );
    assert!(
        row_exists(&pool, "media_attachments", fresh).await,
        "and only the stale rows went: the fresh one stays"
    );
}

/// A sweep that cannot reach the database is a retryable failure, so
/// the queue keeps the pass while the scheduler enqueues the next
/// period: the stale rows wait for a working sweep instead of being
/// assumed gone.
#[tokio::test]
async fn an_unreachable_database_retries_the_media_sweep() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let orphan = unposted_media_row(&pool, &alice, Some("alice/orphan.png"), None, 49).await;
    let broken = broken_state().await;

    let outcome = handle_sweep_media(broken, FIRST_ATTEMPT, json!({})).await;

    match outcome {
        Err(JobError::Retry(detail)) => assert!(
            detail.starts_with("orphaned local media deleted failed"),
            "the retry states what failed: {detail}"
        ),
        Ok(()) => panic!("a closed pool must not read as a successful sweep"),
        Err(other) => panic!("a closed pool is retryable, not {other:?}"),
    }
    assert!(
        row_exists(&pool, "media_attachments", orphan).await,
        "the healthy pool sees no work the broken one claimed to do"
    );
}

/// A stale unposted row a pending scheduled status names in `media_ids`
/// survives the sweep with its stored file; a stale row named nowhere
/// beside it is still reclaimed.
#[tokio::test]
async fn the_media_sweep_keeps_media_reserved_by_a_scheduled_status() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let (dir, state, backend) = uploads_state(&pool);
    let alice = user(&pool, "alice").await;
    let key = format!("alice/{}/reserved.png", Uuid::now_v7());
    put_media_file(&backend, &key).await;
    let reserved = unposted_media_row(&pool, &alice, Some(&key), None, 49).await;
    let orphan = unposted_media_row(&pool, &alice, None, None, 49).await;
    let _schedule = scheduled_row(
        &pool,
        &alice,
        json!({ "status": "later", "media_ids": [reserved] }),
    )
    .await;

    let outcome = handle_sweep_media(state, FIRST_ATTEMPT, json!({})).await;

    assert_eq!(outcome, Ok(()), "the sweep succeeds");
    assert!(
        row_exists(&pool, "media_attachments", reserved).await,
        "the scheduled status still reserves the row"
    );
    assert!(
        file_on_disk(dir.path(), &key),
        "and its stored file stays on disk"
    );
    assert!(
        !row_exists(&pool, "media_attachments", orphan).await,
        "an unreferenced stale row is still deleted"
    );
}

/// An id a scheduled status carries only as `in_reply_to_id` reserves
/// nothing: the stale row is reclaimed.
#[tokio::test]
async fn a_scheduled_in_reply_to_id_does_not_reserve_media() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let media = unposted_media_row(&pool, &alice, None, None, 49).await;
    let _schedule = scheduled_row(
        &pool,
        &alice,
        json!({ "status": "later", "in_reply_to_id": media, "media_ids": [] }),
    )
    .await;

    let outcome = handle_sweep_media(app_state(&pool), FIRST_ATTEMPT, json!({})).await;

    assert_eq!(outcome, Ok(()), "the sweep succeeds");
    assert!(
        !row_exists(&pool, "media_attachments", media).await,
        "only `media_ids` reserves media, not `in_reply_to_id`"
    );
}

/// Once the scheduled row is gone, the next sweep reclaims the media
/// and its file.
#[tokio::test]
async fn the_next_sweep_reclaims_media_once_its_schedule_is_gone() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let (dir, state, backend) = uploads_state(&pool);
    let alice = user(&pool, "alice").await;
    let key = format!("alice/{}/released.png", Uuid::now_v7());
    put_media_file(&backend, &key).await;
    let media = unposted_media_row(&pool, &alice, Some(&key), None, 49).await;
    let schedule = scheduled_row(&pool, &alice, json!({ "media_ids": [media] })).await;

    let first = handle_sweep_media(state.clone(), FIRST_ATTEMPT, json!({})).await;
    assert_eq!(first, Ok(()), "the first sweep succeeds");
    assert!(
        row_exists(&pool, "media_attachments", media).await,
        "the pending schedule keeps the row through the first sweep"
    );

    let removed = query("DELETE FROM scheduled_statuses WHERE id = $1")
        .bind(schedule)
        .execute(&pool)
        .await
        .expect("the schedule deletes");
    assert_eq!(removed.rows_affected(), 1, "the schedule row was removed");

    let second = handle_sweep_media(state, FIRST_ATTEMPT, json!({})).await;
    assert_eq!(second, Ok(()), "the second sweep succeeds");
    assert!(
        !row_exists(&pool, "media_attachments", media).await,
        "with the schedule gone the row is reclaimed"
    );
    assert!(
        !file_on_disk(dir.path(), &key),
        "and its stored file goes with it"
    );
}
