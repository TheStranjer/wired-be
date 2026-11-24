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
//! minutes the sweep walks [`fetch_due`](scheduled_status::fetch_due) rows,
//! [`SWEEP_BATCH`] at a time, and publishes each through the same private
//! `publish_one` the per-row handler runs.

use chrono::Utc;
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{debug, warn};
use uuid::Uuid;

use crate::{
    error::AppError,
    jobs::{
        AttemptInfo, HandlerFuture, JobError, QUEUE_SCHEDULED, load_user, parse_args, retry,
        scheduler::PeriodicJob,
    },
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
/// is harmless either way: the publish reads the row afresh, and
/// [`sweep_job`]'s pass publishes any schedule whose job did not.
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
            "could not cancel a scheduled status's previous fire; the sweep may publish it twice"
        ),
    }
    schedule(state, row).await;
}

/// The job name in the discard text of unusable [`PublishArgs`].
const PUBLISH_JOB: &str = "scheduled status publish";

/// The job name in the retry text of a failed pass; the row survives every
/// retryable failure, so another pass costs nothing but the wait.
const PASS_JOB: &str = "scheduled status publication";

/// The [`JobRegistry`](super::JobRegistry) handler for [`KIND_PUBLISH`]:
/// publish the scheduled row the arguments name, if it is still stored.
///
/// A row already gone — fired by the sweep, or cancelled — completes the job
/// silently: there is nothing left to fire. Unusable arguments are a
/// [`JobError::Discard`]; a database failure is a [`JobError::Retry`].
pub fn handle_publish(state: AppState, _attempt: AttemptInfo, args: Value) -> HandlerFuture {
    Box::pin(async move {
        let scheduled_id = parse_args::<PublishArgs>(&args, PUBLISH_JOB)?.scheduled_id;
        match scheduled_status::get_by_id(&state.db, scheduled_id).await {
            Ok(Some(row)) => publish_one(&state, &row).await,
            Ok(None) => {
                debug!(scheduled = %scheduled_id, "scheduled status is already gone");
                Ok(())
            }
            Err(err) => Err(retry(PASS_JOB, &err)),
        }
    })
}

/// The publish job's body: replay the row's stored params through the
/// ordinary create path as its author, then take the row out of the table.
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
async fn publish_one(state: &AppState, row: &ScheduledStatus) -> Result<(), JobError> {
    let params = match serde_json::from_value::<CreateStatusParams>(row.params.clone()) {
        Ok(params) => params,
        Err(err) => {
            return drop_row(
                state,
                row,
                &format!("stored create-status params are unusable: {err}"),
            )
            .await;
        }
    };
    // An author who is gone or can no longer post means the row can never
    // fire: the loader's discard is this job's drop-the-row arm.
    let author = match load_user(state, row.user_id, "scheduled status", true).await {
        Ok(author) => author,
        Err(JobError::Discard(reason)) => return drop_row(state, row, &reason).await,
        Err(other) => return Err(other),
    };
    publish_params(state, row, &author, params).await
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
    row: &ScheduledStatus,
    author: &user::User,
    params: CreateStatusParams,
) -> Result<(), JobError> {
    match create_status(state, author, None, params).await {
        Ok(created) => {
            if let Err(err) = scheduled_status::delete(&state.db, row.id).await {
                return Err(retry(PASS_JOB, &err));
            }
            debug!(scheduled = %row.id, status = %created.id, "scheduled status published");
            Ok(())
        }
        Err(
            refusal @ (AppError::Unprocessable(_)
            | AppError::NotFound
            | AppError::Forbidden(_)
            | AppError::Gone),
        ) => drop_row(state, row, &refusal.to_string()).await,
        Err(err) => Err(retry(PASS_JOB, &err)),
    }
}

/// Deletes a schedule that can never fire and logs the reason: a spent
/// schedule leaves the table exactly the way the endpoint's cancel does, so
/// neither the listings nor the sweep see it again. A database failure in
/// the delete is still a [`JobError::Retry`] — the row, and the reason,
/// survive for another attempt.
///
/// # Errors
///
/// [`JobError::Retry`] when the delete itself fails on the database.
async fn drop_row(state: &AppState, row: &ScheduledStatus, reason: &str) -> Result<(), JobError> {
    match scheduled_status::delete(&state.db, row.id).await {
        Ok(deleted) => {
            warn!(scheduled = %row.id, deleted, "scheduled status dropped unpublished: {reason}");
            Ok(())
        }
        Err(err) => Err(retry(PASS_JOB, &err)),
    }
}

/// The [`JobRegistry`](super::JobRegistry) handler for [`KIND_SWEEP`]: walk
/// every schedule due by now — [`fetch_due`](scheduled_status::fetch_due)
/// rows, [`SWEEP_BATCH`] at a time — publishing each through the private
/// `publish_one`, the same helper [`handle_publish`] runs.
///
/// The walk ends on the first short batch: rows the pass published or
/// dropped leave the sweep query, so a full batch only ever means more work,
/// and an instance with nothing due costs one read. A database failure is a
/// [`JobError::Retry`]; the scheduler enqueues the next period regardless,
/// and the per-row jobs still fire whatever the failed pass left behind.
pub fn handle_sweep(state: AppState, _attempt: AttemptInfo, _args: Value) -> HandlerFuture {
    Box::pin(async move {
        let mut total = 0_i64;
        loop {
            let batch = match scheduled_status::fetch_due(&state.db, Utc::now(), SWEEP_BATCH).await
            {
                Ok(rows) => rows,
                Err(err) => return Err(retry(PASS_JOB, &err)),
            };
            let count = i64::try_from(batch.len()).unwrap_or(SWEEP_BATCH);
            publish_batch(&state, &batch).await?;
            total += count;
            if count < SWEEP_BATCH {
                break;
            }
        }
        if total > 0 {
            debug!(total, "scheduled statuses swept");
        }
        Ok(())
    })
}

/// Publishes one sweep batch. Each row's fire is its own statements, so a
/// retried pass finds the earlier rows already gone and picks up where the
/// failure stopped.
///
/// # Errors
///
/// [`JobError::Retry`] on the first row whose publish cannot be completed.
async fn publish_batch(state: &AppState, batch: &[ScheduledStatus]) -> Result<(), JobError> {
    for row in batch {
        publish_one(state, row).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, SubsecRound as _, TimeDelta, Utc};
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
            user::User,
        },
        test_fixtures::local_user,
    };

    /// Test state over `pool` with the fixed test config.
    fn app_state(pool: &PgPool) -> AppState {
        AppState::for_tests(pool.clone(), Config::for_tests())
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

    /// The handler's attempt info: the first of however many the job gets.
    const FIRST_ATTEMPT: AttemptInfo = AttemptInfo {
        attempts: 1,
        max_attempts: 5,
    };

    #[tokio::test]
    async fn schedule_queues_one_job_due_exactly_at_the_parked_moment() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let row = parked(
            &pool,
            &alice,
            Utc::now() + TimeDelta::hours(1),
            json!({ "status": "hello tomorrow" }),
        )
        .await;
        let state = app_state(&pool);

        schedule(&state, &row).await;
        // The dedupe key swallows the second claim, so a replayed create
        // cannot queue the same fire twice.
        schedule(&state, &row).await;

        let rows = sqlx::query_as::<_, crate::models::job::Job>(
            "SELECT * FROM jobs WHERE kind = $1 ORDER BY id",
        )
        .bind(KIND_PUBLISH)
        .fetch_all(&pool)
        .await
        .expect("the job rows read");
        assert_eq!(rows.len(), 1, "one job per schedule, second call deduped");
        let job = &rows[0];
        assert_eq!(job.queue, QUEUE_SCHEDULED, "on the scheduled queue");
        assert_eq!(
            job.args,
            job_args(row.id),
            "the args name exactly the schedule id"
        );
        assert_eq!(
            job.args["scheduled_id"],
            row.id.to_string(),
            "the key is scheduled_id"
        );
        assert_eq!(
            job.run_at, row.scheduled_at,
            "due at the parked moment, to the microsecond"
        );
        assert_eq!(
            job.unique_key.as_deref(),
            Some(unique_key(row.id).as_str()),
            "the ticket's dedupe key"
        );
        assert_eq!(job.max_attempts, 5, "default attempts");
    }

    /// The `scheduled_statuses.publish` job rows, oldest id first.
    async fn publish_jobs(pool: &PgPool) -> Vec<crate::models::job::Job> {
        sqlx::query_as::<_, crate::models::job::Job>(
            "SELECT * FROM jobs WHERE kind = $1 ORDER BY id",
        )
        .bind(KIND_PUBLISH)
        .fetch_all(pool)
        .await
        .expect("the job rows read")
    }

    #[tokio::test]
    async fn reschedule_replaces_the_waiting_fire_at_the_new_moment() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let row = parked(
            &pool,
            &alice,
            Utc::now() + TimeDelta::hours(1),
            json!({ "status": "hello tomorrow" }),
        )
        .await;
        let state = app_state(&pool);
        schedule(&state, &row).await;
        let waiting = publish_jobs(&pool).await;
        assert_eq!(waiting.len(), 1, "the create queued one fire");
        assert_eq!(waiting[0].run_at, row.scheduled_at, "due at the old moment");

        // The endpoint's own move, then the re-queue it runs afterwards.
        // Whole microseconds, because that is what a `timestamptz` keeps and
        // the comparison below is against a row read back out of one.
        let later = Utc::now().trunc_subsecs(6) + TimeDelta::hours(6);
        let moved = scheduled_status::reschedule(&pool, row.id, alice.id, later)
            .await
            .expect("the reschedule runs")
            .expect("the owner's row moved");
        reschedule(&state, &moved).await;

        let after = publish_jobs(&pool).await;
        assert_eq!(
            after.len(),
            1,
            "the old fire went and the new one took its place"
        );
        assert_eq!(after[0].run_at, later, "due at the new moment");
        assert_ne!(
            after[0].id, waiting[0].id,
            "a fresh row, not the old one edited"
        );
        assert_eq!(
            after[0].unique_key.as_deref(),
            Some(unique_key(row.id).as_str()),
            "and still exactly one key per schedule"
        );
    }

    #[tokio::test]
    async fn reschedule_queues_a_fire_when_none_was_waiting() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let row = parked(
            &pool,
            &alice,
            Utc::now() + TimeDelta::hours(2),
            json!({ "status": "never queued" }),
        )
        .await;
        let state = app_state(&pool);
        assert!(
            publish_jobs(&pool).await.is_empty(),
            "the fixture row has no fire waiting"
        );

        // A lost or already-claimed job leaves nothing to cancel; the moved
        // schedule still gets its fire, and twice over the key holds one.
        reschedule(&state, &row).await;
        reschedule(&state, &row).await;

        let after = publish_jobs(&pool).await;
        assert_eq!(after.len(), 1, "one fire for the row, deduped by key");
        assert_eq!(
            after[0].run_at, row.scheduled_at,
            "due at the moment the row carries"
        );
    }

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

        let outcome = publish_one(&app_state(&pool), &orphan).await;

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

        let outcome = publish_one(&app_state(&pool), &broken).await;

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
