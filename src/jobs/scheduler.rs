//! The periodic scheduler: the cron-like loop that enqueues recurring jobs.
//!
//! `docs/ARCHITECTURE.md` decision 11 keeps background work in Postgres rows;
//! this module is the producer of the rows that fire on a schedule. The
//! [`periodic_jobs`] registry lists every recurring job (today the
//! streaming hub's reference-table sweep, the mutes expiry sweep, the daily
//! dismissed-notification sweep, the hourly OAuth/MFA/password-reset
//! token sweep, the daily jobs-table prune, the daily deleted-local-status
//! purge and the six-hourly orphaned-local-media sweep; later tickets
//! append one line per entry), and [`run`] ticks every ten seconds,
//! enqueueing each registry job with the
//! `unique_key` `periodic:<kind>:<bucket>` — where [`bucket`] is
//! `floor(unix_secs / every_secs)`. Every app instance derives the same key
//! for the same period, so the queue's deduplication index (see
//! [`crate::models::job::enqueue`]) lets exactly one row per period exist no
//! matter how many schedulers tick: within one period the repeats are
//! dropped, and a new period enqueues a fresh row. Nothing executes the
//! rows; that is [`Worker`](super::Worker)'s side of the same table.
//!
//! The tick clock is the wall-clock time read once at startup shifted by the
//! monotonic [`tokio::time::Instant`] elapsed since, so a paused tokio test
//! clock drives it: bucket transitions in tests are deterministic instead
//! of racing the real wall clock.

use std::{future::Future, time::Duration};

use chrono::{DateTime, TimeDelta, Utc};
use serde_json::json;
use sqlx::PgPool;
use tokio::time::{Instant, interval};
use tracing::{debug, info, warn};

use crate::models::job::{NewJob, enqueue};

/// How often [`run`] wakes to enqueue whatever period is current.
const TICK_INTERVAL: Duration = Duration::from_secs(10);

/// One recurring job: the [`JobRegistry`](super::JobRegistry) kind to run,
/// the queue to place it on, and how often it runs.
///
/// Entries live in the [`periodic_jobs`] registry; the scheduler only turns
/// each one into queue rows once per [`bucket`], it never executes them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PeriodicJob {
    /// Job kind, as registered in the [`JobRegistry`](super::JobRegistry).
    pub kind: &'static str,
    /// Queue the rows are placed on (one of the [`QUEUE_*`](super::QUEUE_DEFAULT) names).
    pub queue: &'static str,
    /// Period between runs; the job enqueues once per [`bucket`] of this length.
    pub every: Duration,
}

/// Every recurring job this instance schedules.
///
/// The registry is a plain list: a later ticket adds a job by appending one
/// line here (decision 15), and the [`run`] loop starts enqueueing it.
/// Today it holds the streaming hub's sweep of expired reference payloads
/// ([`KIND_CLEANUP`](crate::streaming::hub::KIND_CLEANUP)), run on
/// [`QUEUE_BACKGROUND`](super::QUEUE_BACKGROUND) once a minute so no
/// spill-over row outlives its [`PAYLOAD_TTL`](crate::streaming::hub::PAYLOAD_TTL)
/// by more than a tick, and the sweep of mutes whose duration has run out
/// ([`KIND_EXPIRE`](super::mutes::KIND_EXPIRE)) on the same queue once every ten
/// minutes — every read of that table already treats a lapsed mute as absent, so
/// the period only decides how long its row stays on the books. The daily sweep
/// of notifications dismissed beyond their retention
/// ([`KIND_NOTIFICATIONS`](super::maintenance::KIND_NOTIFICATIONS)) sits on the
/// same queue too: dismissal already hides those rows from every read, so a
/// day is only how long the storage lives on after them. The hourly sweep of
/// dead OAuth, MFA and password-reset tokens
/// ([`KIND_TOKENS`](super::maintenance::KIND_TOKENS)) shares the queue: the
/// validity lookups already refuse those rows, so the period only decides how
/// long the storage lives on after them. The daily prune of terminal queue
/// rows and replay-guard rows
/// ([`KIND_JOBS`](super::maintenance::KIND_JOBS)) shares it too: a job that
/// ended a week ago and an activity redelivered after its window are pure
/// bookkeeping, so the period only decides how long their storage lives on.
/// The daily hard purge of the local statuses soft-deleted beyond the
/// configured age
/// ([`KIND_DELETED_STATUSES`](super::maintenance::KIND_DELETED_STATUSES))
/// shares it too: a tombstone answers no read, so the period only decides
/// how long its row and its cascading dependents live on after it. The
/// six-hourly sweep of unposted local media whose grace window ran out
/// ([`KIND_MEDIA`](super::maintenance::KIND_MEDIA)) shares it too: media
/// no status, and no chat message, claims within the two-day window is
/// storage nothing can read back, so the period only decides how long the
/// file and its row outlive that window.
#[must_use]
pub fn periodic_jobs() -> Vec<PeriodicJob> {
    vec![
        PeriodicJob {
            kind: crate::streaming::hub::KIND_CLEANUP,
            queue: super::QUEUE_BACKGROUND,
            every: Duration::from_secs(60),
        },
        PeriodicJob {
            kind: super::mutes::KIND_EXPIRE,
            queue: super::QUEUE_BACKGROUND,
            every: Duration::from_secs(600),
        },
        PeriodicJob {
            kind: super::maintenance::KIND_NOTIFICATIONS,
            queue: super::QUEUE_BACKGROUND,
            every: Duration::from_hours(24),
        },
        PeriodicJob {
            kind: super::maintenance::KIND_TOKENS,
            queue: super::QUEUE_BACKGROUND,
            every: Duration::from_secs(3_600),
        },
        PeriodicJob {
            kind: super::maintenance::KIND_JOBS,
            queue: super::QUEUE_BACKGROUND,
            every: Duration::from_hours(24),
        },
        PeriodicJob {
            kind: super::maintenance::KIND_DELETED_STATUSES,
            queue: super::QUEUE_BACKGROUND,
            every: Duration::from_hours(24),
        },
        PeriodicJob {
            kind: super::maintenance::KIND_MEDIA,
            queue: super::QUEUE_BACKGROUND,
            every: Duration::from_secs(21_600),
        },
    ]
}

/// The period index of `now` for a job that runs every `every`:
/// `floor(unix_secs(now) / whole_secs(every))`.
///
/// Two instants inside the same period share a bucket, and `now + every` is
/// always exactly one bucket ahead, so the bucket — combined with the job's
/// kind — identifies one scheduled run across every app instance. Division
/// floors towards minus infinity, and a period shorter than a second (or
/// zero) degenerates to per-second buckets.
#[must_use]
pub fn bucket(now: DateTime<Utc>, every: Duration) -> i64 {
    let every_secs = i64::try_from(every.as_secs()).unwrap_or(i64::MAX).max(1);
    now.timestamp().div_euclid(every_secs)
}

/// Enqueues the [`periodic_jobs`] on their schedule until `shutdown`
/// resolves.
///
/// Each ten-second tick (and one immediately at start) enqueues every
/// registry job for the period the tick falls in; the `unique_key` dedupe
/// makes that at most one row per job per period, from any number of app
/// instances. Enqueue failures are logged, never fatal: the next tick
/// retries the same period because no row claimed the key.
pub async fn run<F>(pool: PgPool, jobs: Vec<PeriodicJob>, shutdown: F)
where
    F: Future<Output = ()>,
{
    info!(
        count = jobs.len(),
        interval_secs = TICK_INTERVAL.as_secs(),
        "periodic job scheduler started"
    );
    let wall = Utc::now();
    let started = Instant::now();
    let mut ticker = interval(TICK_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut shutdown = Box::pin(shutdown);
    let mut ticking = true;
    while ticking {
        tokio::select! {
            biased;
            () = &mut shutdown => ticking = false,
            _ = ticker.tick() => tick(&pool, &jobs, clock_now(wall, started)).await,
        }
    }
    debug!("periodic job scheduler stopped");
}

/// The tick's notion of now: `wall` (read at scheduler start) shifted by the
/// whole seconds elapsed on the monotonic runtime clock since `started`.
///
/// Deriving the timestamp from [`tokio::time::Instant`] rather than calling
/// [`Utc::now`] per tick keeps a paused test clock in control of which
/// period the scheduler sees.
#[must_use]
fn clock_now(wall: DateTime<Utc>, started: Instant) -> DateTime<Utc> {
    let elapsed = i64::try_from(started.elapsed().as_secs()).unwrap_or(i64::MAX);
    wall + TimeDelta::try_seconds(elapsed).unwrap_or_default()
}

/// Runs one scheduler tick: enqueues every registry job for the period
/// `now` falls in.
async fn tick(pool: &PgPool, jobs: &[PeriodicJob], now: DateTime<Utc>) {
    for job in jobs {
        enqueue_for_period(pool, *job, now).await;
    }
}

/// Enqueues `job` once for the period `now` falls in, keyed
/// `periodic:<kind>:<bucket>` so repeats within the period and other app
/// instances deduplicate against the same row.
async fn enqueue_for_period(pool: &PgPool, job: PeriodicJob, now: DateTime<Utc>) {
    let key = format!("periodic:{}:{}", job.kind, bucket(now, job.every));
    let request = NewJob {
        queue: job.queue,
        kind: job.kind,
        args: json!({}),
        run_at: None,
        max_attempts: None,
        unique_key: Some(key.as_str()),
    };
    match enqueue(pool, request).await {
        Ok(Some(row)) => {
            debug!(kind = job.kind, id = %row.id, %key, "periodic job enqueued");
        }
        Ok(None) => {
            debug!(kind = job.kind, %key, "period already enqueued");
        }
        Err(err) => {
            warn!(%err, kind = job.kind, %key, "periodic enqueue failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use sqlx::postgres::PgPoolOptions;

    use super::*;
    use crate::models::job::Job;

    /// The instant `secs` seconds after the Unix epoch.
    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(secs, 0).unwrap()
    }

    /// The `unique_key`s of `kind`'s rows, sorted.
    async fn keys_for(pool: &PgPool, kind: &str) -> Vec<String> {
        sqlx::query_scalar::<_, String>(
            "SELECT unique_key FROM jobs WHERE kind = $1 ORDER BY unique_key",
        )
        .bind(kind)
        .fetch_all(pool)
        .await
        .unwrap()
    }

    /// How many rows `kind` has.
    async fn count_jobs(pool: &PgPool, kind: &str) -> i64 {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM jobs WHERE kind = $1")
            .bind(kind)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[test]
    fn bucket_divides_the_unix_seconds_by_the_period() {
        assert_eq!(bucket(at(0), Duration::from_secs(10)), 0, "the epoch");
        assert_eq!(
            bucket(at(9), Duration::from_secs(10)),
            0,
            "inside the first period"
        );
        assert_eq!(
            bucket(at(10), Duration::from_secs(10)),
            1,
            "on the boundary"
        );
        assert_eq!(
            bucket(at(1_700_000_000), Duration::from_hours(1)),
            472_222,
            "an hourly period"
        );
        assert_eq!(
            bucket(at(1_700_000_000), Duration::from_secs(600)),
            2_833_333,
            "a ten-minute period"
        );
    }

    #[test]
    fn bucket_floors_negative_time_and_degenerates_sub_second_periods() {
        assert_eq!(
            bucket(at(-1), Duration::from_secs(10)),
            -1,
            "floor, not truncate"
        );
        assert_eq!(
            bucket(at(-11), Duration::from_secs(10)),
            -2,
            "one step below the boundary"
        );
        assert_eq!(
            bucket(at(41), Duration::from_millis(500)),
            41,
            "half-second periods act as one second"
        );
        assert_eq!(
            bucket(at(7), Duration::ZERO),
            7,
            "a zero period acts as one second"
        );
    }

    #[test]
    fn registry_entries_are_fully_described() {
        for job in periodic_jobs() {
            assert!(!job.kind.is_empty(), "a scheduled job needs a kind");
            assert!(!job.queue.is_empty(), "a scheduled job needs a queue");
            assert!(
                !job.every.is_zero(),
                "a scheduled job needs a positive period"
            );
        }
    }

    #[tokio::test]
    async fn ticks_inside_one_period_enqueue_exactly_one_job() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let jobs = vec![PeriodicJob {
            kind: "purge",
            queue: "scheduled",
            every: Duration::from_hours(1),
        }];
        // 1_699_999_200 = 472222 * 3600, the start of a whole hour.
        let period_start = at(1_699_999_200);

        tick(&pool, &jobs, period_start).await;
        tick(&pool, &jobs, period_start).await;
        tick(&pool, &jobs, at(1_699_999_200 + 3_599)).await;

        assert_eq!(
            count_jobs(&pool, "purge").await,
            1,
            "repeats inside a period deduplicate"
        );
        let row = sqlx::query_as::<_, Job>("SELECT * FROM jobs WHERE kind = 'purge'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            row.unique_key.as_deref(),
            Some("periodic:purge:472222"),
            "the ticket's key"
        );
        assert_eq!(row.queue, "scheduled", "the job's queue is honoured");
        assert_eq!(row.state, "available", "the row waits claimable");
        assert_eq!(row.args, json!({}), "periodic jobs run with empty args");
    }

    #[tokio::test]
    async fn a_new_period_enqueues_another_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let jobs = vec![PeriodicJob {
            kind: "purge",
            queue: "scheduled",
            every: Duration::from_hours(1),
        }];

        tick(&pool, &jobs, at(1_699_999_200)).await;
        tick(&pool, &jobs, at(1_700_002_800)).await;

        assert_eq!(
            keys_for(&pool, "purge").await,
            vec!["periodic:purge:472222", "periodic:purge:472223"],
            "the next period gets its own row, the first keeps its key"
        );
    }

    #[tokio::test]
    async fn one_tick_enqueues_every_registry_job() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let jobs = vec![
            PeriodicJob {
                kind: "purge",
                queue: "scheduled",
                every: Duration::from_hours(1),
            },
            PeriodicJob {
                kind: "digest",
                queue: "background",
                every: Duration::from_secs(600),
            },
        ];

        tick(&pool, &jobs, at(1_699_999_200)).await;

        let rows: Vec<(String, String, String)> =
            sqlx::query_as("SELECT kind, queue, unique_key FROM jobs ORDER BY kind")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(
            rows,
            vec![
                (
                    "digest".to_owned(),
                    "background".to_owned(),
                    "periodic:digest:2833332".to_owned()
                ),
                (
                    "purge".to_owned(),
                    "scheduled".to_owned(),
                    "periodic:purge:472222".to_owned()
                ),
            ],
            "each job lands on its own queue under its own key"
        );
    }

    #[tokio::test]
    async fn run_ticks_the_periodic_schedule_under_a_paused_clock() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        // Drive the paused clock by hand. A running blocking task inhibits
        // tokio's auto-advance, so the virtual clock moves only where this
        // test calls `advance`: every real Postgres round trip lands while
        // the clock stands still. That replaces the old free-running 1 ms
        // ticker, whose hundred-fold virtual speed could burn sqlx's 30 s
        // acquire timeout (and its 1 s idle timeout) before a lazily opened
        // connection's handshake finished — the `PoolTimedOut` flakes a
        // loaded suite produced.
        tokio::time::pause();
        let (hold_tx, hold_rx) = std::sync::mpsc::channel::<()>();
        let inhibitor = tokio::task::spawn_blocking(move || {
            let _held = hold_rx.recv();
        });
        let jobs = vec![PeriodicJob {
            kind: "heartbeat",
            queue: "scheduled",
            every: Duration::from_secs(1),
        }];

        // The first tick fires at once; the next lands only 10 s of virtual
        // time later. Stepping 20 ms at a time wakes the shutdown future's
        // poll on that rhythm — and the poll's own round trip is what hands
        // the scheduler's inserts real time to land — while 900 steps still
        // stop short of a third tick, so the finished scheduler holds
        // exactly the two periods the keys below are asserted on.
        let watched = pool.clone();
        let scheduler = tokio::spawn(run(pool.clone(), jobs, async move {
            wait_for_count(&watched, "heartbeat", 2).await;
        }));
        let started = std::time::Instant::now();
        let mut steps = 0_u32;
        while !scheduler.is_finished() {
            assert!(
                steps < 900,
                "the schedule never turned over a period: {steps} steps"
            );
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "the scheduler wedged in real time"
            );
            steps += 1;
            tokio::time::advance(Duration::from_millis(20)).await;
            let _rows = count_jobs(&pool, "heartbeat").await;
        }
        scheduler
            .await
            .expect("the scheduler task ran to completion");

        let keys = keys_for(&pool, "heartbeat").await;
        assert_eq!(
            keys.len(),
            2,
            "one row for the current period, one for a later one"
        );
        assert!(
            keys[0].starts_with("periodic:heartbeat:")
                && keys[1].starts_with("periodic:heartbeat:"),
            "both rows carry the ticket's key prefix: {keys:?}"
        );
        assert_ne!(keys[0], keys[1], "the two rows come from different buckets");

        // `run` returned when the shutdown future resolved; 60 more seconds
        // of virtual time (six tick periods) prove nothing keeps ticking
        // behind it.
        tokio::time::advance(Duration::from_secs(60)).await;
        assert_eq!(
            count_jobs(&pool, "heartbeat").await,
            2,
            "the scheduler stopped with its shutdown"
        );

        drop(hold_tx);
        inhibitor
            .await
            .expect("the clock inhibitor exits with its channel");

        // Hand the wall clock back before returning: sqlx closes the pool
        // and drops the temporary database after the test body, and that
        // cleanup opens a fresh master connection. Left frozen, the
        // auto-advancing paused clock would burn that connection's real-
        // time handshake against sqlx's virtual 30 s acquire timeout, the
        // cleanup would time out, and the test database would leak.
        tokio::time::resume();
    }

    /// Resolves once `kind` has at least `want` rows.
    ///
    /// The polls park on short virtual sleeps, which the driving test steps
    /// forward; the deadline is a real-time guard, which is what watches a
    /// paused runtime for a scheduler that never ticks.
    async fn wait_for_count(pool: &PgPool, kind: &str, want: i64) {
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        while count_jobs(pool, kind).await < want {
            assert!(
                std::time::Instant::now() < deadline,
                "{kind} rows never reached {want}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn a_broken_database_costs_a_log_not_a_crash() {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://localhost/unused")
            .unwrap();
        let jobs = vec![PeriodicJob {
            kind: "ghost",
            queue: "scheduled",
            every: Duration::from_secs(60),
        }];

        tick(&pool, &jobs, at(60)).await;
    }
}
