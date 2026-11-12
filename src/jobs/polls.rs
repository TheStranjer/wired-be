//! The poll-end notification job: the minute sweep that tells a closed
//! poll's author and its local voters that the question ended.
//!
//! `docs/ARCHITECTURE.md` decision 11 keeps background work in Postgres
//! rows; this module is the consumer Pleroma runs from `Oban` instead: its
//! `PollWorker` (`lib/pleroma/workers/poll_worker.ex`) is scheduled at each
//! Question's `closed` instant, runs `Notification.create_poll_notifications`
//! once, and needs no further bookkeeping. Here the telling is a *sweep*:
//! [`KIND_END_NOTIFY`] rides the [`periodic_jobs`] registry once a minute,
//! and [`poll::fetch_ended_unnotified`] hands it the polls whose deadline
//! ran out inside the last [`NOTIFY_WINDOW_MINUTES`] — five minutes, the
//! ticket's look-back, wide enough that a lost run or a restart still tells
//! and narrow enough that a poll closed years ago is not news. The poll's
//! own [`ended_notified`](poll::Poll::ended_notified) column retires a row
//! the sweep has told about, so the telling happens exactly once; a pass
//! that dies mid-poll retries the whole job, and the deduplication of
//! [`notify::on_poll_ended`] keeps the recipients already told from being
//! told twice.
//!
//! [`periodic_jobs`]: super::scheduler::periodic_jobs
//! [`notify::on_poll_ended`]: crate::services::notify::on_poll_ended

use serde_json::Value;
use tracing::debug;

use crate::{
    error::AppError,
    jobs::{AttemptInfo, HandlerFuture, JobError, scheduler::PeriodicJob},
    models::poll,
    services::notify,
    state::AppState,
};

/// The job kind of the poll-end notification sweep, handled by
/// [`handle_end_notify`] and scheduled by [`end_notify_job`] — on
/// [`QUEUE_BACKGROUND`](super::QUEUE_BACKGROUND), once every minute.
pub const KIND_END_NOTIFY: &str = "polls.end_notify";

/// How far back one pass looks for polls that closed without being told
/// about: five minutes, the ticket's window.
///
/// A poll whose deadline ran out before this is left alone — its moment for
/// a "the poll ended" telling has passed, and a sweep that comes back after
/// a long outage should not shout about stale questions.
pub const NOTIFY_WINDOW_MINUTES: i32 = 5;

/// The sweep's [`PeriodicJob`] registry entry: [`KIND_END_NOTIFY`] on
/// [`QUEUE_BACKGROUND`](super::QUEUE_BACKGROUND) every minute.
/// [`periodic_jobs`](super::scheduler::periodic_jobs) appends it; a test
/// reads it so the period and queue are pinned in one place.
#[must_use]
pub const fn end_notify_job() -> PeriodicJob {
    PeriodicJob {
        kind: KIND_END_NOTIFY,
        queue: super::QUEUE_BACKGROUND,
        every: std::time::Duration::from_secs(60),
    }
}

/// Lifts a database failure into the queue's retry arm; the sweep's work is
/// guarded by the poll's `ended_notified` flag and the tellings by the
/// service's own deduplication, so another pass costs nothing but the wait.
fn retry(err: &AppError) -> JobError {
    JobError::Retry(format!("poll end-notify pass failed: {err}"))
}

/// The [`JobRegistry`](super::JobRegistry) handler for [`KIND_END_NOTIFY`]:
/// tell every poll that closed inside [`NOTIFY_WINDOW_MINUTES`] minutes and
/// was never told about, then retire each one's flag.
///
/// That flag is the ticket's exactly-once rule over decision 11's queue:
/// one poll's telling runs before its flag is set, so a failure anywhere in
/// between leaves the row unretired and the whole pass retryable, and the
/// recipients already told are not told twice (the deduplication of
/// [`notify::on_poll_ended`]). A poll whose status is gone tells nobody and
/// is retired like any other — the sweep never revisits it. With nothing
/// due the pass costs one read.
///
/// [`notify::on_poll_ended`]: crate::services::notify::on_poll_ended
pub fn handle_end_notify(state: AppState, _attempt: AttemptInfo, _args: Value) -> HandlerFuture {
    Box::pin(async move {
        let ended = poll::fetch_ended_unnotified(&state.db, NOTIFY_WINDOW_MINUTES)
            .await
            .map_err(|err| retry(&err))?;
        for row in &ended {
            notify::on_poll_ended(&state, row)
                .await
                .map_err(|err| retry(&err))?;
            // `0` would say a concurrent sweep retired the row first,
            // which the per-recipient deduplication has already made
            // harmless; the count itself is the flag write's concern.
            let _retired = poll::mark_ended_notified(&state.db, row.id)
                .await
                .map_err(|err| retry(&err))?;
        }
        let count = ended.len();
        if count > 0 {
            debug!(count, "closed polls notified");
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::{TimeDelta, Utc};
    use serde_json::json;
    use sqlx::PgPool;
    use uuid::Uuid;

    use super::*;
    use crate::{
        config::{Config, TEST_BASE_URL},
        http_client::HttpClient,
        jobs::QUEUE_BACKGROUND,
        models::{
            notification::TYPE_POLL,
            poll::PollParams,
            status::{self, NewStatus},
            user::{NewUser, User, insert, local_ap_id},
        },
        streaming::hub::Hub,
        uploads::UploaderBackend,
    };

    /// Test state over `pool` with the fixed test config.
    fn app_state(pool: &PgPool) -> AppState {
        let config = Arc::new(Config::for_tests());
        AppState {
            db: pool.clone(),
            hub: Hub::new(pool),
            config: Arc::clone(&config),
            http_client: HttpClient::from(config.as_ref()),
            uploader: UploaderBackend::from(config.as_ref()),
            instance_stats_cache: Arc::default(),
            mrf_config_cache: Arc::default(),
            translation_languages_cache: Arc::default(),
            rate_limiter: Arc::new(crate::rate_limit::RateLimiter::from_config(config.as_ref())),
            mailer: Arc::new(crate::mailer::MemoryTransport::new()),
        }
    }

    /// Inserts a local user named `nickname`.
    async fn user(pool: &PgPool, nickname: &str) -> User {
        insert(
            pool,
            &NewUser::local(nickname, &local_ap_id(TEST_BASE_URL, nickname)),
        )
        .await
        .expect("the local fixture inserts")
    }

    /// A poll by `author` closing `mins` minutes from now (negative: one
    /// whose deadline ran out), with each of `voters` marking one option.
    async fn poll_closed_around(
        pool: &PgPool,
        author: &User,
        mins: i64,
        voters: &[&User],
    ) -> poll::Poll {
        let posted = status::insert(
            pool,
            TEST_BASE_URL,
            &NewStatus::local(author, "tea or coffee?"),
        )
        .await
        .expect("the poll status inserts");
        let created = poll::create(
            &mut pool.acquire().await.expect("a slot connection"),
            posted.id,
            &PollParams {
                options: vec!["tea".to_owned(), "coffee".to_owned()],
                expires_at: Some(Utc::now() + TimeDelta::minutes(mins)),
                multiple: true,
                ..PollParams::default()
            },
        )
        .await
        .expect("the poll fixture creates");
        let choices = poll::options(pool, created.id)
            .await
            .expect("the options read");
        for (index, voter) in voters.iter().enumerate() {
            let mark = choices[index % choices.len()].id;
            assert!(
                poll::cast_vote(pool, created.id, voter.id, &[mark])
                    .await
                    .expect("the fixture ballot writes"),
                "each fixture voter marks once"
            );
        }
        poll::get_by_id(pool, created.id)
            .await
            .expect("the poll reads")
            .expect("the fixture poll stands")
    }

    /// The recipient's stored `poll` notifications as `(from, status)`
    /// pairs, newest first.
    async fn poll_facts(pool: &PgPool, recipient: &User) -> Vec<(Uuid, Option<Uuid>)> {
        sqlx::query_as::<_, (Uuid, Option<Uuid>)>(
            "SELECT from_account_id, status_id FROM notifications \
              WHERE user_id = $1 AND type = $2 ORDER BY id DESC",
        )
        .bind(recipient.id)
        .bind(TYPE_POLL)
        .fetch_all(pool)
        .await
        .expect("the rows read")
    }

    /// The stored `ended_notified` flag of poll `id`.
    async fn flag(pool: &PgPool, id: Uuid) -> bool {
        sqlx::query_scalar::<_, bool>("SELECT ended_notified FROM polls WHERE id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .expect("the flag reads")
    }

    /// The handler's attempt info: the first of however many the job gets.
    const FIRST_ATTEMPT: AttemptInfo = AttemptInfo {
        attempts: 1,
        max_attempts: 5,
    };

    #[tokio::test]
    async fn the_sweep_tells_the_author_and_the_voters_exactly_once() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = user(&pool, "pe-alice").await;
        let bob = user(&pool, "pe-bob").await;
        let carol = user(&pool, "pe-carol").await;
        let ended = poll_closed_around(&pool, &alice, -1, &[&bob, &carol]).await;

        let outcome = handle_end_notify(state.clone(), FIRST_ATTEMPT, json!({})).await;
        assert_eq!(outcome, Ok(()), "a sweep of one closed poll succeeds");

        let closing = (alice.id, Some(ended.status_id));
        // The author is told of the closing they are the actor of — the
        // self skip's one exemption — and both voters hear it from them.
        assert_eq!(
            poll_facts(&pool, &alice).await,
            vec![closing],
            "the author is told, once, by nobody but themselves"
        );
        for voter in [&bob, &carol] {
            assert_eq!(
                poll_facts(&pool, voter).await,
                vec![closing],
                "{} hears the closing once",
                voter.nickname
            );
        }
        assert!(flag(&pool, ended.id).await, "and the poll is retired");

        // A second pass — the next minute's row, or a retry — changes
        // nothing: the flag took the poll off the work list.
        let reran = handle_end_notify(state, FIRST_ATTEMPT, json!({})).await;
        assert_eq!(reran, Ok(()), "the rerun succeeds");
        for recipient in [&alice, &bob, &carol] {
            assert_eq!(
                poll_facts(&pool, recipient).await,
                vec![closing],
                "the rerun tells nobody twice ({}'s inbox)",
                recipient.nickname
            );
        }
    }

    #[tokio::test]
    async fn the_sweep_skips_open_stale_and_retired_polls() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = user(&pool, "sw-alice").await;
        let bob = user(&pool, "sw-bob").await;
        let stale_mins = -i64::from(NOTIFY_WINDOW_MINUTES) - 5;
        let still_open = poll_closed_around(&pool, &alice, 10, &[]).await;
        let lapsed = poll_closed_around(&pool, &alice, stale_mins, &[]).await;
        let retired = poll_closed_around(&pool, &alice, -1, &[]).await;
        let _retired = poll::mark_ended_notified(&pool, retired.id)
            .await
            .expect("the retirement runs");
        let due = poll_closed_around(&pool, &alice, -1, &[&bob]).await;

        let outcome = handle_end_notify(state, FIRST_ATTEMPT, json!({})).await;
        assert_eq!(outcome, Ok(()), "the pass runs");

        assert!(
            !flag(&pool, still_open.id).await,
            "an open poll is untouched"
        );
        assert!(!flag(&pool, lapsed.id).await, "a stale close is left alone");
        assert!(
            flag(&pool, retired.id).await,
            "a retired one stays retired, un-told"
        );
        assert_eq!(
            poll_facts(&pool, &alice).await,
            vec![(alice.id, Some(due.status_id))],
            "only the due poll tells the author"
        );
        assert_eq!(
            poll_facts(&pool, &bob).await,
            vec![(alice.id, Some(due.status_id))],
            "and only the due poll's voter is told among them"
        );
        assert!(
            flag(&pool, due.id).await,
            "the due poll is retired by the pass"
        );
    }

    #[tokio::test]
    async fn a_deleted_poll_status_is_retired_without_a_telling() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = user(&pool, "ds-alice").await;
        let ended = poll_closed_around(&pool, &alice, -1, &[]).await;
        let _tombstone = status::soft_delete(&pool, ended.status_id)
            .await
            .expect("the delete runs");

        let outcome = handle_end_notify(state, FIRST_ATTEMPT, json!({})).await;
        assert_eq!(outcome, Ok(()), "a gone status is done work");
        assert!(
            poll_facts(&pool, &alice).await.is_empty(),
            "nobody is told about a deleted question"
        );
        assert!(
            flag(&pool, ended.id).await,
            "and the sweep still retires the row"
        );
    }

    #[tokio::test]
    async fn an_unreachable_database_retries_the_sweep() {
        let unusable = {
            let lazy = sqlx::postgres::PgPoolOptions::new()
                .connect_lazy("postgres://localhost/unused")
                .expect("a lazy pool needs no database");
            lazy.close().await;
            app_state(&lazy)
        };

        let outcome = handle_end_notify(unusable, FIRST_ATTEMPT, json!({})).await;
        match outcome {
            Err(JobError::Retry(detail)) => assert!(
                detail.starts_with("poll end-notify pass failed"),
                "the retry states what failed: {detail}"
            ),
            other => panic!("a closed pool is retryable, not {other:?}"),
        }
    }

    /// The kind is wired into both registries the ticket names: the worker
    /// can run it, and the sweep sits on the periodic schedule every
    /// minute.
    #[test]
    fn the_kind_is_registered_and_scheduled() {
        let registry = crate::job_registry();
        assert!(
            registry.get(KIND_END_NOTIFY).is_some(),
            "the worker knows `{KIND_END_NOTIFY}`"
        );

        let scheduled = crate::jobs::scheduler::periodic_jobs()
            .into_iter()
            .find(|job| job.kind == KIND_END_NOTIFY)
            .expect("the sweep is on the periodic schedule");
        assert_eq!(scheduled, end_notify_job(), "from the registry entry here");
        assert_eq!(scheduled.queue, QUEUE_BACKGROUND, "on the background queue");
        assert_eq!(
            scheduled.every,
            std::time::Duration::from_secs(60),
            "once every minute"
        );
    }
}
