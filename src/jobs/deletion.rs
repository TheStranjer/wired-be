//! The account-deletion job: clearing a departed account's data.
//!
//! `POST /api/pleroma/delete_account` (WBE-136, `delete_account` in
//! [`crate::api::pleroma_account_settings`]) marks the account deleted and
//! queues this job on [`QUEUE_BACKGROUND`](super::QUEUE_BACKGROUND) — the
//! counterpart of Pleroma's `User.delete` enqueueing its `DeleteWorker`
//! (`lib/pleroma/user.ex`). The endpoint answers immediately; everything
//! slow happens here.
//!
//! The job walks the account's data and takes it with it:
//!
//! 1. every status the account authored becomes a tombstone,
//!    [`soft_delete_all_for_user`](crate::models::status::soft_delete_all_for_user)
//!    batching 100 rows per statement and looping until a pass clears none
//!    (the ticket's soft delete in batches of 100);
//! 2. its follows, favourites, bookmarks, notifications, lists and media
//!    rows go through the per-table `delete_all_for_user` writers, each
//!    taking exactly what a hard delete of the user row would have
//!    cascaded;
//! 3. the user row itself is kept for referential integrity — decision 7
//!    hangs mirrors, activities and relationships off it — but its
//!    personal data (email, password hash, display name) is replaced with
//!    empty values, Pleroma's `purge` step.
//!
//! Every write is idempotent — tombstoning a tombstone clears nothing,
//! deleting a missing row deletes nothing — so a retried pass converges on
//! the same state, which is why a database failure is a
//! [`JobError::Retry`] and only unusable arguments or a vanished user row
//! are a discard.
//!
//! Federation of the actor's `Delete` (the `after_account_deleted` hook in
//! [`crate::api::pleroma_account_settings`]) attaches to the endpoint with
//! the federation ticket that owns the outbox pipeline.

use serde::Deserialize;
use serde_json::{Value, json};
use tracing::debug;
use uuid::Uuid;

use crate::{
    error::AppError,
    jobs::{AttemptInfo, HandlerFuture, JobError},
    models::{bookmark, favourite, follow, list, media_attachment, notification, status, user},
    state::AppState,
};

/// The job kind of an account deletion, handled by [`handle_delete`] on
/// [`QUEUE_BACKGROUND`](super::QUEUE_BACKGROUND).
pub const KIND_DELETE: &str = "users.delete";

/// The `{user_id}` arguments of the delete job, as [`job_args`] writes and
/// [`handle_delete`] reads them.
#[derive(Debug, Deserialize)]
struct DeleteArgs {
    /// The account whose data the job clears.
    user_id: Uuid,
}

/// The `{user_id}` job-arguments document for account `user_id`: the one
/// shape [`crate::api::pleroma_account_settings`] enqueues and
/// [`handle_delete`] deserialises.
#[must_use]
pub fn job_args(user_id: Uuid) -> Value {
    json!({ "user_id": user_id })
}

/// Reads [`DeleteArgs`] out of a claimed job's payload; arguments this
/// shape cannot deserialize are permanent — the job is discarded, never
/// retried.
fn parse_args(args: &Value) -> Result<Uuid, JobError> {
    serde_json::from_value::<DeleteArgs>(args.clone())
        .map(|parsed| parsed.user_id)
        .map_err(|err| {
            JobError::Discard(format!("account delete job arguments are unusable: {err}"))
        })
}

/// Lifts a database failure into the queue's retry arm; every step is
/// idempotent, so another pass costs nothing but the wait.
fn retry(err: &AppError) -> JobError {
    JobError::Retry(format!("account delete pass failed: {err}"))
}

/// The [`JobRegistry`](super::JobRegistry) handler for [`KIND_DELETE`]:
/// clear the account's statuses, relationships, notifications, lists and
/// media, then blank the personal data of the row itself.
///
/// A vanished user row or an unparsable payload is discarded — no retry
/// can fix either; everything else that fails is a database failure over
/// work that is idempotent, and retries.
pub fn handle_delete(state: AppState, _attempt: AttemptInfo, args: Value) -> HandlerFuture {
    Box::pin(async move {
        let user_id = parse_args(&args)?;
        let account = load_account(&state, user_id).await?;
        clear_data(&state, &account).await?;
        user::purge_personal_data(&state.db, account.id)
            .await
            .map_err(|err| retry(&err))?;
        debug!(account = %account.nickname, "account deletion finished");
        Ok(())
    })
}

/// The account the job clears. A vanished row is permanent (discard): the
/// endpoint only queues this job for a row it just marked, and if it is
/// gone by a later attempt, there is nothing left to clear.
///
/// # Errors
///
/// [`JobError::Discard`] when the user row is gone, [`JobError::Retry`]
/// on a database failure.
async fn load_account(state: &AppState, user_id: Uuid) -> Result<user::User, JobError> {
    match user::get_by_id(&state.db, user_id).await {
        Ok(Some(account)) => Ok(account),
        Ok(None) => Err(JobError::Discard(format!(
            "the delete job's user {user_id} no longer exists"
        ))),
        Err(err) => Err(retry(&err)),
    }
}

/// Steps 1 and 2: the account's posts tombstone, its relationships, likes,
/// marks, notifications, lists and media rows go. Each statement is a
/// whole step on its own — a retry after a mid-way failure re-runs the
/// earlier ones as no-ops and picks up where they stopped.
///
/// # Errors
///
/// [`JobError::Retry`] on any database failure (see [`retry`]).
async fn clear_data(state: &AppState, account: &user::User) -> Result<(), JobError> {
    let db = &state.db;
    let id = account.id;
    let statuses = status::soft_delete_all_for_user(db, id)
        .await
        .map_err(|err| retry(&err))?;
    let follows = follow::delete_all_for_user(db, id)
        .await
        .map_err(|err| retry(&err))?;
    let favourites = favourite::delete_all_for_user(db, id)
        .await
        .map_err(|err| retry(&err))?;
    let bookmarks = bookmark::delete_all_for_user(db, id)
        .await
        .map_err(|err| retry(&err))?;
    let notifications = notification::delete_all_for_user(db, id)
        .await
        .map_err(|err| retry(&err))?;
    let lists = list::delete_all_for_user(db, id)
        .await
        .map_err(|err| retry(&err))?;
    let media = media_attachment::delete_all_for_user(db, id)
        .await
        .map_err(|err| retry(&err))?;
    debug!(
        account = %account.nickname,
        statuses, follows, favourites, bookmarks, notifications, lists, media,
        "account deletion cleared"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::{DateTime, Utc};
    use serde_json::json;
    use sqlx::PgPool;
    use sqlx::postgres::PgPoolOptions;

    use super::*;
    use crate::{
        config::{Config, TEST_BASE_URL},
        http_client::HttpClient,
        jobs::QUEUE_BACKGROUND,
        models::{
            favourite::Favourite,
            list::{self, List, NewList},
            notification::{self, NewNotification, Notification},
            status::{NewStatus, Status},
            user::{NewUser, User, insert, local_ap_id},
        },
        uploads::UploaderBackend,
    };

    /// How many statuses the batch test inserts: one full 100-row pass and
    /// a remainder, so the loop must run at least twice.
    const BATCH_OVERSHOOT: i64 = 120;

    /// Test state over `pool` with the fixed test config.
    fn app_state(pool: &PgPool) -> AppState {
        let config = Arc::new(Config::for_tests());
        AppState {
            db: pool.clone(),
            hub: crate::streaming::hub::Hub::new(pool),
            config: Arc::clone(&config),
            http_client: HttpClient::from(config.as_ref()),
            uploader: UploaderBackend::from(config.as_ref()),
            instance_stats_cache: Arc::default(),
            translation_languages_cache: Arc::default(),
            rate_limiter: Arc::new(crate::rate_limit::RateLimiter::from_config(config.as_ref())),
            mailer: Arc::new(crate::mailer::MemoryTransport::new()),
        }
    }

    /// Test state over a pool that cannot answer: closed before it ever
    /// connected, which is how a handler sees a database that went away.
    async fn broken_state() -> AppState {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://localhost/unused")
            .expect("a lazy pool needs no database");
        pool.close().await;
        app_state(&pool)
    }

    /// Inserts a local user named `nickname` carrying the personal data the
    /// purge step must blank: an email, a password hash and a display name.
    async fn local_user(pool: &PgPool, nickname: &str) -> User {
        insert(
            pool,
            &NewUser {
                email: Some(format!("{nickname}@example.org")),
                password_hash: Some("hash-of-a-secret".to_owned()),
                name: nickname.to_owned(),
                ..NewUser::local(nickname, &local_ap_id(TEST_BASE_URL, nickname))
            },
        )
        .await
        .expect("the local fixture inserts")
    }

    /// Posts one local status for `author`.
    async fn post(pool: &PgPool, author: &User, text: &str) -> Status {
        status::insert(pool, TEST_BASE_URL, &NewStatus::local(author, text))
            .await
            .expect("the status fixture inserts")
    }

    /// Posts `count` statuses for `author` in one multi-row statement, for
    /// the batch-boundary fixture.
    async fn post_many(pool: &PgPool, author: &User, count: i64) {
        let mut builder = sqlx::QueryBuilder::<sqlx::Postgres>::new(
            "INSERT INTO statuses (id, uri, url, user_id, visibility, conversation_id, local) ",
        );
        let _values: &mut _ = builder.push_values(0..count, |mut row, _| {
            let id = Uuid::now_v7();
            let _fields: &mut _ = row
                .push_bind(id)
                .push_bind(format!("{TEST_BASE_URL}/objects/{id}"))
                .push_bind(format!("{TEST_BASE_URL}/notice/{id}"))
                .push_bind(author.id)
                .push_bind("public")
                .push_bind(id)
                .push_bind(true);
        });
        let inserted = builder
            .build()
            .execute(pool)
            .await
            .expect("the batch status inserts");
        assert_eq!(
            inserted.rows_affected(),
            u64::try_from(count).unwrap_or(u64::MAX),
            "every batch row lands"
        );
    }

    /// `follower` follows `target` through the model's normal path.
    async fn follows(pool: &PgPool, follower: &User, target: &User) {
        let _granted = follow::follow(pool, TEST_BASE_URL, follower, target)
            .await
            .expect("the follow fixture is recorded");
    }

    /// `who` favourites `status_id`, answering the created row.
    async fn likes(pool: &PgPool, who: &User, status_id: Uuid) -> Favourite {
        favourite::favourite(pool, TEST_BASE_URL, who.id, status_id)
            .await
            .expect("the favourite write")
            .expect("the favourite is created")
    }

    /// `who` bookmarks `status_id`.
    async fn marks(pool: &PgPool, who: &User, status_id: Uuid) {
        let _added = bookmark::add(pool, who.id, status_id)
            .await
            .expect("the bookmark write")
            .expect("the bookmark is created");
    }

    /// A `follow` notification of `recipient` caused by `actor`, answering
    /// the created row.
    async fn notify(pool: &PgPool, recipient: &User, actor: &User) -> Notification {
        notification::create(
            pool,
            &NewNotification {
                user_id: recipient.id,
                from_account_id: actor.id,
                notification_type: "follow",
                status_id: None,
                emoji: None,
                report_id: None,
            },
        )
        .await
        .expect("the notification fixture inserts")
    }

    /// A list owned by `owner` holding `member` as its only member.
    async fn list_of(pool: &PgPool, owner: &User, member: &User) -> List {
        let created = list::create(pool, owner.id, &NewList::default())
            .await
            .expect("the list fixture creates");
        let _added = list::add_members(pool, created.id, &[member.id])
            .await
            .expect("the membership fixture adds");
        created
    }

    /// Inserts a media row owned by `owner`, attached to `status_id` when
    /// one is named, and answers its id.
    async fn media_row(pool: &PgPool, owner: &User, status_id: Option<Uuid>) -> Uuid {
        let id = Uuid::now_v7();
        let written = sqlx::query(
            "INSERT INTO media_attachments (id, user_id, status_id, url, content_type)
             VALUES ($1, $2, $3, $4, 'image/png')",
        )
        .bind(id)
        .bind(owner.id)
        .bind(status_id)
        .bind(format!("{TEST_BASE_URL}/media/{id}.png"))
        .execute(pool)
        .await
        .expect("the media fixture inserts");
        assert_eq!(written.rows_affected(), 1, "one row lands");
        id
    }

    /// The count of live (not tombstoned) statuses authored by `author_id`.
    async fn live_statuses(pool: &PgPool, author_id: Uuid) -> i64 {
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM statuses WHERE user_id = $1 AND deleted_at IS NULL",
        )
        .bind(author_id)
        .fetch_one(pool)
        .await
        .expect("the live statuses count")
    }

    /// The count of rows in `column`-keyed table `table` naming `id`. The
    /// table names are test constants, never user input, so the dynamic
    /// statement is asserted safe (the same bypass `src/test_db.rs`
    /// documents).
    async fn count_where(pool: &PgPool, table: &str, column: &str, id: Uuid) -> i64 {
        sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(format!(
            "SELECT count(*) FROM {table} WHERE {column} = $1"
        )))
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap_or_else(|_| panic!("{table}.{column} rows count"))
    }

    /// The `deleted_at` stamp of status `id`.
    async fn status_deleted_at(pool: &PgPool, id: Uuid) -> Option<DateTime<Utc>> {
        sqlx::query_scalar::<_, Option<DateTime<Utc>>>(
            "SELECT deleted_at FROM statuses WHERE id = $1",
        )
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("the status reads")
    }

    /// Runs [`handle_delete`] to completion on `state` for `user_id`.
    async fn run(state: &AppState, user_id: Uuid) -> Result<(), JobError> {
        let attempt = AttemptInfo {
            attempts: 1,
            max_attempts: 5,
        };
        handle_delete(state.clone(), attempt, job_args(user_id)).await
    }

    /// The sweep's status and media half: past a batch boundary every
    /// authored post is a tombstone, the account's uploads and the media
    /// of its posts go, and the kept row has no personal data left.
    #[tokio::test]
    async fn the_handler_tombstones_statuses_and_purges_the_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        post_many(&pool, &alice, BATCH_OVERSHOOT).await;
        let alices_post = post(&pool, &alice, "alice posts").await;
        let bobs_post = post(&pool, &bob, "bob keeps this").await;
        let alices_upload = media_row(&pool, &alice, None).await;
        let on_alices_status = media_row(&pool, &alice, Some(alices_post.id)).await;
        let bobs_on_alices_status = media_row(&pool, &bob, Some(alices_post.id)).await;
        let bobs_own_media = media_row(&pool, &bob, Some(bobs_post.id)).await;

        run(&app_state(&pool), alice.id)
            .await
            .expect("the delete job runs");

        assert_eq!(
            live_statuses(&pool, alice.id).await,
            0,
            "all 121 of alice's statuses tombstoned, across the 100-row batch"
        );
        assert!(
            status_deleted_at(&pool, alices_post.id).await.is_some(),
            "the single post tombstoned too"
        );
        assert!(
            status_deleted_at(&pool, bobs_post.id).await.is_none(),
            "bob's post is not alice's to tombstone"
        );

        for gone in [alices_upload, on_alices_status, bobs_on_alices_status] {
            assert_eq!(
                count_where(&pool, "media_attachments", "id", gone).await,
                0,
                "media row {gone} is gone"
            );
        }
        assert_eq!(
            count_where(&pool, "media_attachments", "id", bobs_own_media).await,
            1,
            "bob's own file on bob's own post stays"
        );

        let purged = user::get_by_id(&pool, alice.id)
            .await
            .unwrap()
            .expect("the row is kept for referential integrity");
        assert!(purged.email.is_none(), "the email is gone");
        assert!(
            purged.password_hash.is_none(),
            "and the password hash with it"
        );
        assert_eq!(purged.name, "", "and the display name is blanked");
        assert_eq!(purged.nickname, "alice", "the handle stays for the refs");
    }

    /// Whether `follower` → `target` currently has a follow row.
    async fn has_follow(pool: &PgPool, follower: &User, target: &User) -> bool {
        follow::get(pool, follower.id, target.id)
            .await
            .unwrap()
            .is_some()
    }

    /// The sweep's relationship half: both directions of every follow,
    /// the account's own likes and marks, its notifications in both
    /// roles, its lists and its memberships — while every row of other
    /// accounts about the same peers stays.
    #[tokio::test]
    async fn the_handler_clears_relationships_notifications_and_lists() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let carol = local_user(&pool, "carol").await;
        let alices_post = post(&pool, &alice, "alice posts").await;
        let bobs_post = post(&pool, &bob, "bob posts").await;
        follows(&pool, &alice, &bob).await;
        follows(&pool, &bob, &alice).await;
        follows(&pool, &bob, &carol).await;
        let _alices_like = likes(&pool, &alice, bobs_post.id).await;
        marks(&pool, &alice, bobs_post.id).await;
        let bobs_like = likes(&pool, &bob, alices_post.id).await;
        let _to_alice = notify(&pool, &alice, &bob).await;
        let _from_alice = notify(&pool, &bob, &alice).await;
        let bobs_other = notify(&pool, &bob, &carol).await;
        let alices_list = list_of(&pool, &alice, &bob).await;
        let bobs_list = list_of(&pool, &bob, &alice).await;

        run(&app_state(&pool), alice.id)
            .await
            .expect("the delete job runs");

        assert!(
            !has_follow(&pool, &alice, &bob).await,
            "alice's follow of bob is gone"
        );
        assert!(
            !has_follow(&pool, &bob, &alice).await,
            "bob's follow of alice is gone"
        );
        assert!(
            has_follow(&pool, &bob, &carol).await,
            "bob and carol keep their relationship"
        );
        assert_eq!(
            count_where(&pool, "favourites", "user_id", alice.id).await,
            0
        );
        assert_eq!(
            count_where(&pool, "bookmarks", "user_id", alice.id).await,
            0
        );
        assert_eq!(
            count_where(&pool, "favourites", "id", bobs_like.id).await,
            1,
            "bob's like of alice's (tombstoned) status survives"
        );
        assert_eq!(
            count_where(&pool, "notifications", "user_id", alice.id).await,
            0,
            "alice's notification inbox is gone"
        );
        assert_eq!(
            count_where(&pool, "notifications", "from_account_id", alice.id).await,
            0,
            "and the notifications alice caused"
        );
        assert_eq!(
            count_where(&pool, "notifications", "id", bobs_other.id).await,
            1,
            "bob's notification about carol is nobody else's data"
        );
        assert_eq!(count_where(&pool, "lists", "id", alices_list.id).await, 0);
        assert_eq!(count_where(&pool, "lists", "id", bobs_list.id).await, 1);
        assert_eq!(
            count_where(&pool, "list_members", "user_id", alice.id).await,
            0,
            "alice is out of every list"
        );
        assert_eq!(
            count_where(&pool, "list_members", "user_id", bob.id).await,
            0,
            "bob's membership of alice's list went down with her list"
        );
    }

    /// The sweep is idempotent: a second pass over the cleared account
    /// succeeds and changes nothing — which is what makes the retry on a
    /// mid-way database failure safe.
    #[tokio::test]
    async fn a_second_pass_clears_nothing_more() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let _posted = post(&pool, &alice, "one post").await;
        follows(&pool, &alice, &bob).await;

        let state = app_state(&pool);
        run(&state, alice.id).await.expect("the first pass runs");
        assert_eq!(
            live_statuses(&pool, alice.id).await,
            0,
            "first pass cleared"
        );

        run(&state, alice.id).await.expect("the second pass runs");
        assert_eq!(
            count_where(&pool, "statuses", "user_id", alice.id).await,
            1,
            "the rerun wrote no new row and removed no tombstone"
        );
        assert_eq!(
            live_statuses(&pool, alice.id).await,
            0,
            "and the account stays fully tombstoned"
        );
    }

    /// A payload that is not `{user_id}` can never run: discarded, not
    /// retried. So does a user row that no longer exists.
    #[tokio::test]
    async fn unusable_arguments_and_a_vanished_user_discard() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let state = app_state(db.pool());

        let bad = handle_delete(
            state.clone(),
            AttemptInfo {
                attempts: 1,
                max_attempts: 5,
            },
            json!({ "id": "not-a-user" }),
        )
        .await;
        match bad {
            Err(JobError::Discard(detail)) => assert!(
                detail.contains("unusable"),
                "the discard says why: {detail}"
            ),
            other => panic!("unusable arguments must discard, not {other:?}"),
        }

        let missing = run(&state, Uuid::now_v7()).await;
        match missing {
            Err(JobError::Discard(detail)) => assert!(
                detail.contains("no longer exists"),
                "the discard names the missing row: {detail}"
            ),
            other => panic!("a vanished user must discard, not {other:?}"),
        }
    }

    /// A database that cannot answer is a retryable failure: every step is
    /// idempotent, so the queue's next attempt picks the sweep up.
    #[tokio::test]
    async fn an_unreachable_database_retries_the_sweep() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let state = broken_state().await;

        let outcome = run(&state, alice.id).await;
        match outcome {
            Err(JobError::Retry(detail)) => assert!(
                detail.starts_with("account delete pass failed"),
                "the retry states what failed: {detail}"
            ),
            other => panic!("a closed pool must retry, not {other:?}"),
        }
        assert!(
            user::get_by_id(&pool, alice.id)
                .await
                .unwrap()
                .expect("the healthy pool still has the row")
                .email
                .is_some(),
            "the broken pass purged nothing"
        );
    }

    /// The kind is wired into the worker's registry, and the registered
    /// handler is this module's sweep.
    #[tokio::test]
    async fn the_kind_is_registered_to_this_handler() {
        let registry = crate::job_registry();
        let handler = registry
            .get(KIND_DELETE)
            .expect("the account delete kind is registered");
        assert_eq!(KIND_DELETE, "users.delete", "the ticket's kind name");

        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let _posted = post(&pool, &alice, "one post").await;
        handler(
            app_state(&pool),
            AttemptInfo {
                attempts: 1,
                max_attempts: 5,
            },
            job_args(alice.id),
        )
        .await
        .expect("the registered handler runs");
        assert_eq!(
            live_statuses(&pool, alice.id).await,
            0,
            "and it is this module's sweep: the post is a tombstone"
        );
    }

    /// The job-arguments document is the `{user_id}` shape the endpoint
    /// enqueues with, on the `background` queue the ticket names.
    #[test]
    fn the_job_args_carry_the_user_id_alone() {
        let id = Uuid::now_v7();
        assert_eq!(job_args(id), json!({ "user_id": id }));
        assert_eq!(QUEUE_BACKGROUND, "background", "the ticket's queue");
    }
}
