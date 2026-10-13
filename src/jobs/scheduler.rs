//! The periodic scheduler: the cron-like loop that enqueues recurring jobs.
//!
//! `docs/ARCHITECTURE.md` decision 11 keeps background work in Postgres rows;
//! this module is the producer of the rows that fire on a schedule. The
//! [`periodic_jobs`] registry lists every recurring job (empty today; later
//! tickets append one line per entry), and [`run`] ticks every ten seconds,
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
#[must_use]
pub const fn periodic_jobs() -> Vec<PeriodicJob> {
    Vec::new()
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

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn ticks_inside_one_period_enqueue_exactly_one_job(pool: PgPool) {
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

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn a_new_period_enqueues_another_row(pool: PgPool) {
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

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn one_tick_enqueues_every_registry_job(pool: PgPool) {
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

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn run_ticks_the_periodic_schedule_under_a_paused_clock(pool: PgPool) {
        tokio::time::pause();
        // Auto-advance steps the virtual clock to the next armed timer on
        // every park. Left alone that step is the scheduler's own 10 s
        // interval, so the clock races ahead of real Postgres round trips
        // and sqlx's 30 s acquire timeout fires long before a fresh
        // connection's handshake lands in wall-clock time. A 1 ms ticker
        // task keeps every step at one millisecond of virtual time — tens
        // of thousands of steps per real second: real IO always wins its
        // race, and periods still turn over quickly.
        let ticker = tokio::spawn(async move {
            for _step in 0..100_000_u32 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        });
        let jobs = vec![PeriodicJob {
            kind: "heartbeat",
            queue: "scheduled",
            every: Duration::from_secs(1),
        }];

        // The first tick fires at once; the 10 s tick interval spans ten
        // one-second periods, but the dedupe key caps each period at one
        // row, so exactly two rows exist once a new period has been
        // enqueued and the shutdown future resolves.
        let watched = pool.clone();
        run(pool.clone(), jobs, async move {
            wait_for_count(&watched, "heartbeat", 2).await;
        })
        .await;
        ticker.abort();

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

        // `run` returned when the shutdown future resolved; a long virtual
        // wait afterwards proves nothing keeps ticking behind it.
        tokio::time::sleep(Duration::from_secs(60)).await;
        assert_eq!(
            count_jobs(&pool, "heartbeat").await,
            2,
            "the scheduler stopped with its shutdown"
        );
    }

    /// Resolves once `kind` has at least `want` rows.
    ///
    /// The polls park on short sleeps so the auto-advancing paused clock
    /// keeps moving; the deadline is a real-time guard, which is what
    /// watches a paused runtime for a scheduler that never ticks.
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
