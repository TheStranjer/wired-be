//! The scheduled-status publish jobs: the per-row fire and the periodic
//! safety sweep behind it.
//!
//! `docs/ARCHITECTURE.md` decision 11 keeps background work in Postgres rows;
//! this module is the consumer of the `scheduled_statuses` table
//! [`crate::models::scheduled_status`] produces: a create-status request
//! parked until a future moment. When a row's
//! [`scheduled_at`](ScheduledStatus::scheduled_at) arrives, a [`KIND_PUBLISH`]
//! job on [`QUEUE_SCHEDULED`] replays the stored params through the ordinary
//! create path — [`create_status`] as the stored author — and removes the row:
//! Pleroma's `ScheduledActivityWorker`
//! (`lib/pleroma/workers/scheduled_activity_worker.ex`). The producer side is
//! the endpoint ticket that stores the row and [schedules](schedule) its fire.
//!
//! Both handlers drop a row that can never fire — an author the database no
//! longer has or whose account can no longer post (deactivated, deleted or
//! awaiting approval, the states the API refuses at token resolution), a create the service refuses (a parent since deleted, media
//! claimed by a live status meanwhile, blank text) — and complete: the
//! schedule is spent, and a retry would only drop it again after another
//! round of backoff. Every database or internal failure keeps the row for
//! the next attempt.
//!
//! [`KIND_SWEEP`] is the safety net, the twin of the
//! [statuses expiry sweep](super::statuses::KIND_SWEEP): a lost or failed
//! per-row job must not leave a schedule unfired forever, so every five
//! minutes the sweep walks [`fetch_due_ids`](scheduled_status::fetch_due_ids)
//! ids, [`SWEEP_BATCH`] at a time, and publishes each through the same private
//! `publish_one` the per-row handler runs.
//!
//! Both runners can reach one row at its moment, so `publish_one` claims it
//! first: a transaction locks the row `FOR UPDATE SKIP LOCKED` only once it
//! is due, publishes, deletes the row and commits. The runner that finds the
//! row locked, gone or not yet due does nothing, so a schedule posts at most
//! once and never before its `scheduled_at`.

use chrono::Utc;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{PgConnection, Postgres, Transaction};
use tracing::{debug, warn};
use uuid::Uuid;

use crate::{
    error::AppError,
    jobs::{AttemptInfo, HandlerFuture, JobError, QUEUE_SCHEDULED, scheduler::PeriodicJob},
    models::{
        job::{NewJob, cancel_pending, enqueue},
        scheduled_status::{self, ScheduledStatus},
        user,
    },
    services::create_status::{CreateStatusParams, create_status},
    state::AppState,
};

/// The job kind of a single scheduled status's fire, handled by
/// [`handle_publish`] on [`QUEUE_SCHEDULED`].
pub const KIND_PUBLISH: &str = "scheduled_statuses.publish";

/// The job kind of the periodic safety sweep, handled by [`handle_sweep`]
/// and scheduled by [`sweep_job`] — on
/// [`QUEUE_BACKGROUND`](super::QUEUE_BACKGROUND), once every five minutes.
pub const KIND_SWEEP: &str = "scheduled_statuses.sweep";

/// How many due schedules one sweep pass publishes: small enough to keep
/// every statement short, the ticket's batch size.
pub const SWEEP_BATCH: i64 = 100;

/// The `{scheduled_id}` arguments of the publish job, as [`job_args`] writes
/// and [`handle_publish`] reads them.
#[derive(Debug, Deserialize)]
struct PublishArgs {
    /// The scheduled row the job publishes.
    scheduled_id: Uuid,
}

/// The `{scheduled_id}` job-arguments document for schedule `scheduled_id`:
/// the one shape [`schedule`] enqueues and [`handle_publish`] deserialises.
#[must_use]
pub fn job_args(scheduled_id: Uuid) -> Value {
    json!({ "scheduled_id": scheduled_id })
}

/// The dedupe key of schedule `scheduled_id`'s publish job.
///
/// At most one live fire per schedule, however many times its create is
/// replayed ([`enqueue`]'s `None` swallows the second claim).
#[must_use]
pub fn unique_key(scheduled_id: Uuid) -> String {
    format!("sched:{scheduled_id}")
}

/// The sweep's [`PeriodicJob`] registry entry: [`KIND_SWEEP`] on
/// [`QUEUE_BACKGROUND`](super::QUEUE_BACKGROUND) every five minutes.
///
/// The same period as [`statuses.expire_sweep`](super::statuses::KIND_SWEEP);
/// [`periodic_jobs`](super::scheduler::periodic_jobs) appends it, and a test
/// reads it so the period and queue are pinned in one place.
#[must_use]
pub const fn sweep_job() -> PeriodicJob {
    PeriodicJob {
        kind: KIND_SWEEP,
        queue: super::QUEUE_BACKGROUND,
        every: std::time::Duration::from_secs(300),
    }
}

/// Queues `row`'s fire for its [`scheduled_at`](ScheduledStatus::scheduled_at).
///
/// The row is a [`KIND_PUBLISH`] job on [`QUEUE_SCHEDULED`], due at exactly
/// that instant, keyed [`unique_key`] so a replayed schedule cannot queue the
/// same fire twice.
///
/// A failed enqueue is logged, not returned: the schedule is already stored
/// with its moment, the [`KIND_SWEEP`](sweep_job) pass publishes it when its
/// time comes, and refusing what the client already parked would be the
/// worse answer — the same bargain
/// [`schedule_expiration`](super::statuses::schedule_expiration) strikes for
/// an expiring post.
pub async fn schedule(state: &AppState, row: &ScheduledStatus) {
    let key = unique_key(row.id);
    let request = NewJob {
        queue: QUEUE_SCHEDULED,
        kind: KIND_PUBLISH,
        args: job_args(row.id),
        run_at: Some(row.scheduled_at),
        max_attempts: None,
        unique_key: Some(&key),
    };
    match enqueue(&state.db, request).await {
        Ok(Some(job)) => debug!(
            job = %job.id,
            scheduled = %row.id,
            due = %row.scheduled_at,
            "scheduled status queued"
        ),
        Ok(None) => debug!(scheduled = %row.id, "scheduled status already queued"),
        Err(err) => warn!(
            %err,
            scheduled = %row.id,
            "could not schedule status publication; the sweep will publish it"
        ),
    }
}

/// Re-queues `row`'s fire at the moment it now carries, replacing the one
/// already waiting for it.
///
/// [`schedule`] alone could not do this: the dedupe key is the schedule's id,
/// not its moment, so the still-live job at the old time would swallow the
/// new claim and the post would fire when it was told to stop.
/// [`cancel_pending`] therefore takes that row out first — the one state
/// it can take back being `available`, since a fire already in a worker's
/// hands or awaiting its retry is the queue's to finish. What survives there
/// is harmless either way: the publish claims the row afresh and only while
/// it is due, so a stale fire finds a moved schedule not yet due and does
/// nothing, and [`sweep_job`]'s pass publishes any schedule whose job did not.
///
/// Like [`schedule`], this logs a queue it cannot write rather than failing
/// the caller: the row already holds its new moment, and that is the fact the
/// endpoint answers.
pub async fn reschedule(state: &AppState, row: &ScheduledStatus) {
    let key = unique_key(row.id);
    match cancel_pending(&state.db, &key).await {
        Ok(cancelled) if cancelled > 0 => debug!(
            scheduled = %row.id,
            cancelled,
            "previous scheduled-status fire cancelled"
        ),
        Ok(_) => debug!(
            scheduled = %row.id,
            "scheduled status had no waiting fire to cancel"
        ),
        Err(err) => warn!(
            %err,
            scheduled = %row.id,
            "could not cancel a scheduled status's previous fire; it will find the row not yet due"
        ),
    }
    schedule(state, row).await;
}

/// Reads [`PublishArgs`] out of a claimed job's payload; arguments this
/// shape cannot deserialize are permanent — the job is discarded, never
/// retried.
fn parse_args(args: &Value) -> Result<Uuid, JobError> {
    serde_json::from_value::<PublishArgs>(args.clone())
        .map(|parsed| parsed.scheduled_id)
        .map_err(|err| {
            JobError::Discard(format!(
                "scheduled status publish job arguments are unusable: {err}"
            ))
        })
}

/// Lifts a database failure into the queue's retry arm; the row survives
/// every retryable failure, so another pass costs nothing but the wait.
fn retry(err: &AppError) -> JobError {
    JobError::Retry(format!("scheduled status publication failed: {err}"))
}

/// The [`JobRegistry`](super::JobRegistry) handler for [`KIND_PUBLISH`]:
/// publish the scheduled row the arguments name, if this runner can claim it.
///
/// A row already gone — fired by the sweep, or cancelled — or one another
/// runner is publishing right now, or one a reschedule moved past this fire's
/// moment, completes the job silently: there is nothing left for it to fire.
/// Unusable arguments are a [`JobError::Discard`]; a database failure is a
/// [`JobError::Retry`].
pub fn handle_publish(state: AppState, _attempt: AttemptInfo, args: Value) -> HandlerFuture {
    Box::pin(async move {
        let scheduled_id = parse_args(&args)?;
        let _claimed = publish_one(&state, scheduled_id).await?;
        Ok(())
    })
}

/// Ends the claim's transaction without a commit, releasing the row lock.
///
/// Rolled back explicitly rather than by drop, which only queues the rollback
/// until the connection is next used: the next runner must find the row free.
/// A failure to roll back is logged — the connection is discarded by the pool
/// then, which releases the lock all the same.
async fn roll_back(tx: Transaction<'_, Postgres>) {
    if let Err(err) = tx.rollback().await {
        warn!(%err, "could not roll back a scheduled-status claim");
    }
}

/// The publish job's body, shared by the per-row fire and the sweep: claim
/// schedule `id` under a row lock, publish it, and remove the row in the same
/// transaction.
///
/// [`lock_due`](scheduled_status::lock_due) takes the row `FOR UPDATE SKIP
/// LOCKED` only while its moment has passed, so of two runners reaching one
/// row only one gets it, and a stale fire can never post a schedule that was
/// moved into the future. The lock is held on the transaction's connection
/// while [`create_status`] works through the pool — it only reads
/// `scheduled_statuses`, which the lock does not block — and the delete and
/// commit release it. A runner that finds nothing to claim (the row is gone,
/// not due, or held by another runner) rolls back and reports `Ok(false)`.
///
/// Returns `Ok(true)` when this runner claimed the row — published it or
/// dropped it as unpublishable — and `Ok(false)` when it did not.
///
/// # Errors
///
/// [`JobError::Retry`] on a database or internal failure; the transaction is
/// rolled back, so the row stays in place and unlocked.
async fn publish_one(state: &AppState, id: Uuid) -> Result<bool, JobError> {
    let mut tx = state
        .db
        .begin()
        .await
        .map_err(|err| retry(&AppError::from(err)))?;
    let row = match scheduled_status::lock_due(&mut tx, id).await {
        Ok(Some(row)) => row,
        Ok(None) => {
            roll_back(tx).await;
            debug!(scheduled = %id, "scheduled status is gone, not due or held elsewhere");
            return Ok(false);
        }
        Err(err) => {
            roll_back(tx).await;
            return Err(retry(&err));
        }
    };
    match publish_locked(state, &mut tx, &row).await {
        Ok(()) => {
            tx.commit()
                .await
                .map_err(|err| retry(&AppError::from(err)))?;
            Ok(true)
        }
        Err(err) => {
            roll_back(tx).await;
            Err(err)
        }
    }
}

/// Publishes `row`, which the caller holds locked on `conn`, and deletes it on
/// `conn`: replay the row's stored params through the ordinary create path as
/// its author, then take the row out of the table.
///
/// The caller commits (or rolls back on `Err`).
///
/// The params deserialize straight into [`CreateStatusParams`]: unknown keys
/// are ignored by serde, so whatever else the endpoint parked beside them
/// (`idempotency`, `application_id`) is harmless. The arms that can never
/// succeed — params of a shape no body could have had, an author the
/// database no longer has, a create the service refuses with a validation or
/// a gone-resource error — drop the row and complete through [`drop_row`];
/// every [`AppError::Database`] or [`AppError::Internal`] failure is a
/// [`JobError::Retry`], and the [`KIND_SWEEP`](sweep_job) pass stands behind
/// a spent retry budget.
///
/// # Errors
///
/// [`JobError::Retry`] on a database or internal failure, including one that
/// strikes while dropping an unpublishable row.
async fn publish_locked(
    state: &AppState,
    conn: &mut PgConnection,
    row: &ScheduledStatus,
) -> Result<(), JobError> {
    let params = match serde_json::from_value::<CreateStatusParams>(row.params.clone()) {
        Ok(params) => params,
        Err(err) => {
            return drop_row(
                conn,
                row,
                &format!("stored create-status params are unusable: {err}"),
            )
            .await;
        }
    };
    let author = match user::get_by_id(&state.db, row.user_id).await {
        Ok(Some(author)) => author,
        Ok(None) => return drop_row(conn, row, "the scheduling account is gone").await,
        Err(err) => return Err(retry(&err)),
    };
    if author.deleted_at.is_some()
        || matches!(
            author.account_status(),
            user::AccountStatus::Deactivated | user::AccountStatus::ApprovalPending
        )
    {
        return drop_row(conn, row, "the scheduling account can no longer post").await;
    }
    publish_params(state, conn, row, &author, params).await
}

/// Runs the create and sorts its outcome into the ticket's three arms:
/// published, refused-for-good, or retry. A refusal is the row's last word
/// — the same body will be refused again — so it leaves through [`drop_row`]
/// like the author-less arm. Either way the fire runs the plain
/// [`create_status`], never its replayable twin, so an `idempotency` key
/// parked beside the stored params is inert in every arm: it is the
/// [`ScheduledStatus`] entity's echo, and nothing claims it on the account's
/// replay list.
///
/// [`create_status`]: crate::services::create_status::create_status
/// [`ScheduledStatus`]: crate::entities::scheduled_status::ScheduledStatus
///
/// # Errors
///
/// [`JobError::Retry`] on a database or internal failure of the create or of
/// the row delete that follows a published one.
async fn publish_params(
    state: &AppState,
    conn: &mut PgConnection,
    row: &ScheduledStatus,
    author: &user::User,
    params: CreateStatusParams,
) -> Result<(), JobError> {
    match create_status(state, author, None, params).await {
        Ok(created) => {
            if let Err(err) = scheduled_status::delete_on(conn, row.id).await {
                return Err(retry(&err));
            }
            debug!(scheduled = %row.id, status = %created.id, "scheduled status published");
            Ok(())
        }
        Err(
            refusal @ (AppError::Unprocessable(_)
            | AppError::NotFound
            | AppError::Forbidden(_)
            | AppError::Gone),
        ) => drop_row(conn, row, &refusal.to_string()).await,
        Err(err) => Err(retry(&err)),
    }
}

/// Deletes a schedule that can never fire, on the claim's connection, and
/// logs the reason: a spent schedule leaves the table exactly the way the
/// endpoint's cancel does, so neither the listings nor the sweep see it
/// again. A database failure in the delete is still a [`JobError::Retry`] —
/// the row, and the reason, survive for another attempt.
///
/// # Errors
///
/// [`JobError::Retry`] when the delete itself fails on the database.
async fn drop_row(
    conn: &mut PgConnection,
    row: &ScheduledStatus,
    reason: &str,
) -> Result<(), JobError> {
    match scheduled_status::delete_on(conn, row.id).await {
        Ok(deleted) => {
            warn!(scheduled = %row.id, deleted, "scheduled status dropped unpublished: {reason}");
            Ok(())
        }
        Err(err) => Err(retry(&err)),
    }
}

/// The [`JobRegistry`](super::JobRegistry) handler for [`KIND_SWEEP`]: publish
/// every schedule due by now.
///
/// It walks [`fetch_due_ids`](scheduled_status::fetch_due_ids) ids,
/// [`SWEEP_BATCH`] at a time, publishing each through the private
/// `publish_one`, the same claim-and-publish [`handle_publish`] runs.
///
/// The walk ends on the first short batch: rows the pass published or
/// dropped leave the sweep query, so a full batch only ever means more work,
/// and an instance with nothing due costs one read. It also ends on a batch
/// in which this pass claimed nothing — every row was held by another runner
/// or not yet due by the database clock, and reading the same rows again
/// would never end. A database failure is a [`JobError::Retry`]; the
/// scheduler enqueues the next period regardless, and the per-row jobs still
/// fire whatever the failed pass left behind.
pub fn handle_sweep(state: AppState, _attempt: AttemptInfo, _args: Value) -> HandlerFuture {
    Box::pin(async move {
        let mut total = 0_i64;
        loop {
            let ids =
                match scheduled_status::fetch_due_ids(&state.db, Utc::now(), SWEEP_BATCH).await {
                    Ok(ids) => ids,
                    Err(err) => return Err(retry(&err)),
                };
            let count = i64::try_from(ids.len()).unwrap_or(SWEEP_BATCH);
            let claimed = publish_batch(&state, &ids).await?;
            total += claimed;
            if count < SWEEP_BATCH || claimed == 0 {
                break;
            }
        }
        if total > 0 {
            debug!(total, "scheduled statuses swept");
        }
        Ok(())
    })
}

/// Publishes one sweep batch and counts the rows this pass claimed. Each
/// row's fire is its own transaction, so a retried pass finds the earlier
/// rows already gone and picks up where the failure stopped.
///
/// # Errors
///
/// [`JobError::Retry`] on the first row whose publish cannot be completed.
async fn publish_batch(state: &AppState, ids: &[Uuid]) -> Result<i64, JobError> {
    let mut claimed = 0_i64;
    for id in ids {
        if publish_one(state, *id).await? {
            claimed += 1;
        }
    }
    Ok(claimed)
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, TimeDelta, Utc};
    use serde_json::json;
    use sqlx::PgPool;

    use super::*;
    use crate::{
        config::{Config, TEST_BASE_URL},
        jobs::QUEUE_BACKGROUND,
        models::{
            media_attachment::{self, NewMediaAttachment},
            scheduled_status::NewScheduledStatus,
            status::{Status, Visibility},
            user::{NewUser, User, insert, local_ap_id},
        },
    };

    /// Test state over `pool` with the fixed test config.
    fn app_state(pool: &PgPool) -> AppState {
        AppState::for_tests(pool.clone(), Config::for_tests())
    }

    /// Inserts a local user named `nickname`.
    async fn local_user(pool: &PgPool, nickname: &str) -> User {
        insert(
            pool,
            &NewUser::local(nickname, &local_ap_id(TEST_BASE_URL, nickname)),
        )
        .await
        .expect("the local fixture inserts")
    }

    /// A schedule for `who`, due `when`, with `params` stored as given.
    async fn parked(
        pool: &PgPool,
        who: &User,
        when: DateTime<Utc>,
        params: Value,
    ) -> ScheduledStatus {
        scheduled_status::insert(
            pool,
            &NewScheduledStatus {
                user_id: who.id,
                scheduled_at: when,
                params,
            },
        )
        .await
        .expect("the schedule fixture inserts")
    }

    /// The scheduled row behind `id`, fired or cancelled rows come back `None`.
    async fn fetched(pool: &PgPool, id: Uuid) -> Option<ScheduledStatus> {
        scheduled_status::get_by_id(pool, id)
            .await
            .expect("the schedule reads")
    }

    /// Every live `statuses` row authored by `who`, oldest id first.
    async fn posts_of(pool: &PgPool, who: &User) -> Vec<Status> {
        sqlx::query_as::<_, Status>("SELECT * FROM statuses WHERE user_id = $1 ORDER BY id")
            .bind(who.id)
            .fetch_all(pool)
            .await
            .expect("the statuses read")
    }

    /// How many `statuses` rows the database holds at all.
    async fn post_count(pool: &PgPool) -> i64 {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM statuses")
            .fetch_one(pool)
            .await
            .expect("the count reads")
    }

    /// How many `idempotency_keys` rows the database holds: a fire must leave
    /// this at zero, since it posts through the plain create rather than its
    /// replayable twin.
    async fn key_count(pool: &PgPool) -> i64 {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM idempotency_keys")
            .fetch_one(pool)
            .await
            .expect("the key count reads")
    }

    /// An unposted `image/png` upload by `owner`, as a `media_ids` entry names it.
    async fn upload(pool: &PgPool, owner: &User) -> media_attachment::MediaAttachment {
        media_attachment::insert(
            pool,
            &NewMediaAttachment {
                user_id: Some(owner.id),
                url: format!("{TEST_BASE_URL}/media/fixture.png"),
                content_type: "image/png".to_owned(),
                ..NewMediaAttachment::default()
            },
        )
        .await
        .expect("a fixture upload inserts")
    }

    mod barred_authors;
    mod claims;
    mod queueing;

    /// The handler's attempt info: the first of however many the job gets.
    const FIRST_ATTEMPT: AttemptInfo = AttemptInfo {
        attempts: 1,
        max_attempts: 5,
    };

    #[tokio::test]
    async fn a_plain_post_publishes_the_stored_params_as_its_author() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let pic = upload(&pool, &alice).await;
        // `idempotency` and `application_id` are the parked-request keys a
        // later endpoint ticket stores beside the body; serde ignores them.
        let row = parked(
            &pool,
            &alice,
            Utc::now() - TimeDelta::minutes(5),
            json!({
                "status": "hello from tomorrow",
                "visibility": "unlisted",
                "sensitive": true,
                "spoiler_text": "careful",
                "language": "fr",
                "content_type": "text/plain",
                "media_ids": [pic.id.to_string()],
                "idempotency": "key-1",
                "application_id": 42,
            }),
        )
        .await;

        let outcome = handle_publish(app_state(&pool), FIRST_ATTEMPT, job_args(row.id)).await;

        assert_eq!(outcome, Ok(()), "a plain parked post publishes");
        assert!(
            fetched(&pool, row.id).await.is_none(),
            "the fired schedule is gone"
        );
        // The parked `idempotency` key is the entity's echo, and the fire
        // runs the plain create: no replay key is claimed on the account.
        assert_eq!(
            key_count(&pool).await,
            0,
            "publishing a parked key claims nothing on the replay list"
        );
        let posts = posts_of(&pool, &alice).await;
        assert_eq!(posts.len(), 1, "exactly the one status was written");
        let post = &posts[0];
        assert_eq!(post.user_id, alice.id, "created as the stored author");
        assert_eq!(post.raw_text, "hello from tomorrow", "the raw text");
        assert_eq!(
            post.content, "<p>hello from tomorrow</p>",
            "rendered through the ordinary create path"
        );
        assert_eq!(
            post.visibility,
            Visibility::Unlisted,
            "the stored visibility"
        );
        assert_eq!(post.spoiler_text, "careful", "the stored content warning");
        assert!(post.sensitive, "the stored sensitive flag");
        assert_eq!(post.language.as_deref(), Some("fr"), "the stored language");
        assert_eq!(post.content_type, "text/plain", "the stored MIME type");
        let claimed = media_attachment::get_by_id(&pool, pic.id)
            .await
            .expect("the media reads")
            .expect("the fixture upload is stored");
        assert_eq!(
            claimed.status_id,
            Some(post.id),
            "the attachment gained its fired status"
        );
    }

    #[tokio::test]
    async fn a_missing_row_completes_with_nothing_written() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let gone = Uuid::now_v7();

        let outcome = handle_publish(app_state(&pool), FIRST_ATTEMPT, job_args(gone)).await;

        assert_eq!(outcome, Ok(()), "an id that names nothing is done work");
        assert!(fetched(&pool, gone).await.is_none(), "and nothing appeared");
        assert_eq!(
            post_count(&pool).await,
            0,
            "the vanished schedule wrote no status"
        );
    }

    #[tokio::test]
    async fn params_the_create_refuses_drop_the_row_and_complete() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let unknown_parent = Uuid::now_v7().to_string();
        let reply = parked(
            &pool,
            &alice,
            Utc::now() - TimeDelta::hours(1),
            json!({ "status": "answering nobody", "in_reply_to_id": unknown_parent }),
        )
        .await;
        let blank = parked(
            &pool,
            &alice,
            Utc::now() - TimeDelta::hours(1),
            json!({ "status": "" }),
        )
        .await;
        let state = app_state(&pool);

        let first = handle_publish(state.clone(), FIRST_ATTEMPT, job_args(reply.id)).await;
        let second = handle_publish(state, FIRST_ATTEMPT, job_args(blank.id)).await;

        assert_eq!(first, Ok(()), "a parent since gone is the row's last word");
        assert_eq!(second, Ok(()), "and so is text that refuses to post");
        assert!(
            fetched(&pool, reply.id).await.is_none(),
            "the reply row went"
        );
        assert!(
            fetched(&pool, blank.id).await.is_none(),
            "the blank row went"
        );
        assert_eq!(
            post_count(&pool).await,
            0,
            "a refused schedule writes no status on any attempt"
        );
    }

    #[tokio::test]
    async fn an_unknown_author_drops_the_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let row = parked(
            &pool,
            &alice,
            Utc::now() - TimeDelta::hours(1),
            json!({ "status": "orphaned" }),
        )
        .await;
        // The table's foreign key cascades a deleted account's schedules
        // away, so only a fire racing an account delete can hand the
        // publisher a stored row whose author is gone; the test plays that
        // race with the row shape the sweep would have walked.
        let orphan = ScheduledStatus {
            id: row.id,
            user_id: Uuid::now_v7(),
            scheduled_at: row.scheduled_at,
            params: row.params.clone(),
            inserted_at: row.inserted_at,
            updated_at: row.updated_at,
        };

        let mut tx = pool.begin().await.expect("the claim transaction opens");
        let outcome = publish_locked(&app_state(&pool), &mut tx, &orphan).await;
        tx.commit().await.expect("the claim commits");

        assert_eq!(
            outcome,
            Ok(()),
            "a schedule nobody owns is dropped, not retried"
        );
        assert!(
            fetched(&pool, row.id).await.is_none(),
            "and the stored row leaves with it"
        );
        assert_eq!(
            post_count(&pool).await,
            0,
            "nothing posted for the gone account"
        );
    }

    #[tokio::test]
    async fn params_no_body_could_have_been_drop_the_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let row = parked(
            &pool,
            &alice,
            Utc::now() - TimeDelta::hours(1),
            json!({ "status": 42 }),
        )
        .await;
        // The same race shape as the author arm: a row whose stored params
        // are of a shape `CreateStatusParams` cannot read.
        let broken = ScheduledStatus {
            id: row.id,
            user_id: row.user_id,
            scheduled_at: row.scheduled_at,
            params: json!({ "status": 42 }),
            inserted_at: row.inserted_at,
            updated_at: row.updated_at,
        };

        let mut tx = pool.begin().await.expect("the claim transaction opens");
        let outcome = publish_locked(&app_state(&pool), &mut tx, &broken).await;
        tx.commit().await.expect("the claim commits");

        assert_eq!(
            outcome,
            Ok(()),
            "unusable params are permanent, not retryable"
        );
        assert!(
            fetched(&pool, row.id).await.is_none(),
            "the unreadable row is dropped"
        );
        assert_eq!(post_count(&pool).await, 0, "and nothing was written");
    }

    #[tokio::test]
    async fn the_sweep_publishes_due_rows_oldest_first_and_leaves_the_future() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let now = Utc::now();
        let pic = upload(&pool, &alice).await;
        // Both due rows claim the same upload, and only the first fire can
        // take it: which text ends up posted proves which row the sweep
        // walked first. The loser is refused it and dropped, itself a
        // publish through the same arm the per-row handler runs.
        let older = parked(
            &pool,
            &alice,
            now - TimeDelta::hours(2),
            json!({ "status": "the older one", "media_ids": [pic.id.to_string()] }),
        )
        .await;
        let newer = parked(
            &pool,
            &alice,
            now - TimeDelta::hours(1),
            json!({ "status": "the newer one", "media_ids": [pic.id.to_string()] }),
        )
        .await;
        let future = parked(
            &pool,
            &alice,
            now + TimeDelta::hours(1),
            json!({ "status": "not due yet" }),
        )
        .await;

        let outcome = handle_sweep(app_state(&pool), FIRST_ATTEMPT, json!({})).await;

        assert_eq!(outcome, Ok(()), "a sweep over due rows succeeds");
        let posts = posts_of(&pool, &alice).await;
        assert_eq!(
            posts.len(),
            1,
            "the second claim of the same upload refused"
        );
        assert_eq!(
            posts[0].raw_text, "the older one",
            "the sweep walked oldest first: {posts:?}"
        );
        assert!(
            fetched(&pool, older.id).await.is_none(),
            "the published row is gone"
        );
        assert!(
            fetched(&pool, newer.id).await.is_none(),
            "the refused row is gone too, not retried forever"
        );
        assert!(
            fetched(&pool, future.id).await.is_some(),
            "a future row waits for its moment"
        );

        let empty = handle_sweep(app_state(&pool), FIRST_ATTEMPT, json!({})).await;
        assert_eq!(empty, Ok(()), "the next pass finds nothing new to fire");
        assert_eq!(post_count(&pool).await, 1, "and fired nothing twice");
    }

    #[tokio::test]
    async fn unusable_arguments_discard_the_job() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();

        let outcome = handle_publish(app_state(&pool), FIRST_ATTEMPT, json!({})).await;

        match outcome {
            Err(JobError::Discard(detail)) => assert!(
                detail.starts_with("scheduled status publish job arguments are unusable"),
                "the discard states why: {detail}"
            ),
            Ok(()) => panic!("bad arguments must not complete the job"),
            Err(other) => panic!("bad arguments are permanent, not {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_unreachable_database_retries_the_schedule_and_both_handlers() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let row = parked(
            &pool,
            &alice,
            Utc::now() - TimeDelta::hours(1),
            json!({ "status": "pending" }),
        )
        .await;
        let broken = {
            let unusable = sqlx::postgres::PgPoolOptions::new()
                .connect_lazy("postgres://localhost/unused")
                .expect("a lazy pool needs no database");
            unusable.close().await;
            app_state(&unusable)
        };

        // A lost enqueue is the sweep's to recover: the row stays parked,
        // so a failed schedule costs a log line, not the post.
        schedule(&broken, &row).await;
        let publish = handle_publish(broken.clone(), FIRST_ATTEMPT, job_args(row.id)).await;
        let sweep = handle_sweep(broken, FIRST_ATTEMPT, json!({})).await;

        for outcome in [publish, sweep] {
            match outcome {
                Err(JobError::Retry(detail)) => assert!(
                    detail.starts_with("scheduled status publication failed"),
                    "the retry states what failed: {detail}"
                ),
                other => panic!("a closed pool is retryable, not {other:?}"),
            }
        }
        assert!(
            fetched(&pool, row.id).await.is_some(),
            "the parked row survived every unreachable attempt"
        );
    }

    /// Both kinds are wired into the registries the ticket names: the worker
    /// can run them, and the sweep sits on the periodic schedule every five
    /// minutes.
    #[test]
    fn the_kinds_are_registered_and_the_sweep_is_scheduled() {
        let registry = crate::job_registry();
        assert!(
            registry.get(KIND_PUBLISH).is_some(),
            "the worker knows `{KIND_PUBLISH}`"
        );
        assert!(
            registry.get(KIND_SWEEP).is_some(),
            "the worker knows `{KIND_SWEEP}`"
        );

        let scheduled = crate::jobs::scheduler::periodic_jobs()
            .into_iter()
            .find(|job| job.kind == KIND_SWEEP)
            .expect("the sweep is on the periodic schedule");
        assert_eq!(scheduled, sweep_job(), "from the registry entry here");
        assert_eq!(
            scheduled.queue, QUEUE_BACKGROUND,
            "on the maintenance queue"
        );
        assert_eq!(
            scheduled.every,
            std::time::Duration::from_secs(300),
            "once every five minutes"
        );
    }
}
