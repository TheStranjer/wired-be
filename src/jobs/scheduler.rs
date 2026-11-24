//! The periodic scheduler: the cron-like loop that enqueues recurring jobs.
//!
//! `docs/ARCHITECTURE.md` decision 11 keeps background work in Postgres rows;
//! this module is the producer of the rows that fire on a schedule. The
//! [`periodic_jobs`] registry lists every recurring job (today the
//! streaming hub's reference-table sweep, the mutes expiry sweep, the
//! idempotency-key sweep, the status-expiry sweep, the scheduled-status
//! fire sweep, the poll-end notification sweep, the daily
//! dismissed-notification sweep, the hourly
//! OAuth/MFA/password-reset token sweep, the daily jobs-table prune, the
//! daily deleted-local-status purge, the six-hourly orphaned-local-media
//! sweep, the six-hourly unreachable-host `nodeinfo` probe, the daily
//! received-activity-ledger prune and the daily backup-expiry purge; later tickets
//! append one line per entry), and [`run`] ticks every
//! ten seconds, enqueueing each registry job with the
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
use tracing::{debug, info};

use super::enqueue_keyed;
use crate::models::job::NewJob;

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
/// by more than a tick, the sweep of mutes whose duration has run out
/// ([`KIND_EXPIRE`](super::mutes::KIND_EXPIRE)) on the same queue once every ten
/// minutes — every read of that table already treats a lapsed mute as absent, so
/// the period only decides how long its row stays on the books — and the sweep
/// of `Idempotency-Key` mappings older than 24 hours
/// ([`KIND_CLEANUP`](super::idempotency::KIND_CLEANUP)), also once every ten
/// minutes on the same queue: a pruned key is simply free to claim again, so
/// the period only decides how long past the horizon a dead mapping keeps
/// replaying — and the safety sweep of statuses whose `expires_at` has run
/// out ([`statuses.expire_sweep`](super::statuses::KIND_SWEEP)),
/// [`super::statuses::sweep_job`], on the same queue once every five minutes:
/// the per-status [`statuses.expire`](super::statuses::KIND_EXPIRE) job is
/// the scheduled delete proper, and this pass catches whatever a lost job
/// row or a spent retry budget would otherwise leave posted forever — and
/// the safety sweep of scheduled statuses whose `scheduled_at` has run out
/// ([`scheduled_statuses.sweep`](super::scheduled_statuses::KIND_SWEEP)),
/// [`super::scheduled_statuses::sweep_job`], on the same queue once every
/// five minutes: the per-row
/// [`scheduled_statuses.publish`](super::scheduled_statuses::KIND_PUBLISH)
/// job is the fire proper, and this pass publishes whatever a lost job row
/// or a spent retry budget would otherwise leave parked forever. The
/// per-minute poll-end notification sweep
/// ([`polls.end_notify`](super::polls::KIND_END_NOTIFY)),
/// [`super::polls::end_notify_job`], shares the queue: where Pleroma
/// schedules one `PollWorker` Oban job per Question, this instance finds
/// closed polls by sweeping, and each poll's own `ended_notified` column
/// keeps the telling to exactly once. The
/// daily sweep of notifications dismissed beyond their retention
/// ([`KIND_NOTIFICATIONS`](super::maintenance::KIND_NOTIFICATIONS)) sits on the
/// same queue too: dismissal already hides those rows from every read, so a
/// day is only how long the storage lives on after them. The hourly sweep of
/// dead OAuth, MFA and password-reset tokens
/// ([`KIND_TOKENS`](super::maintenance::KIND_TOKENS)) shares the queue: the
/// validity lookups already refuse those rows, so the period only decides how
/// long the storage lives on after them. The daily prune of terminal queue
/// rows ([`KIND_JOBS`](super::maintenance::KIND_JOBS)) shares it too: a job
/// that ended a week ago is pure bookkeeping, so the period only decides how
/// long its storage lives on.
/// The daily hard purge of the local statuses soft-deleted beyond the
/// configured age
/// ([`KIND_DELETED_STATUSES`](super::maintenance::KIND_DELETED_STATUSES))
/// shares it too: a tombstone answers no read, so the period only decides
/// how long its row and its cascading dependents live on after it. The
/// six-hourly sweep of unposted local media whose grace window ran out
/// ([`KIND_MEDIA`](super::maintenance::KIND_MEDIA)) shares it too: media
/// no status, and no chat message, claims within the two-day window is
/// storage nothing can read back, so the period only decides how long the
/// file and its row outlive that window. The six-hourly probe of hosts
/// unreachable longer than a day
/// ([`KIND_PROBE`](super::instances::KIND_PROBE)),
/// [`super::instances::handle_probe`], shares it too: a peer whose
/// `/.well-known/nodeinfo` answers again is delivery material again, and the
/// period only decides how long a host that recovered quietly stays skipped
/// by the publisher. The daily prune of the
/// received-activity ledger
/// ([`federation.cleanup_received`](crate::federation::dispatch::KIND_CLEANUP_RECEIVED))
/// shares it too: past two weeks an id is beyond any inbox retry chain
/// still running, so the period only decides how long the bookkeeping row
/// outlives the redeliveries it guards against. The daily purge of the
/// account backups past their validity window
/// ([`backups.purge`](super::backups::KIND_PURGE)) shares it too: past
/// the configured window an archive is its owner's no longer, so the
/// period only decides how long the stored file and its row outlive that
/// window.
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
            kind: super::idempotency::KIND_CLEANUP,
            queue: super::QUEUE_BACKGROUND,
            every: Duration::from_secs(600),
        },
        super::statuses::sweep_job(),
        super::scheduled_statuses::sweep_job(),
        super::polls::end_notify_job(),
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
        PeriodicJob {
            kind: super::instances::KIND_PROBE,
            queue: super::QUEUE_BACKGROUND,
            every: Duration::from_hours(6),
        },
        PeriodicJob {
            kind: crate::federation::dispatch::KIND_CLEANUP_RECEIVED,
            queue: super::QUEUE_BACKGROUND,
            every: Duration::from_hours(24),
        },
        PeriodicJob {
            kind: super::backups::KIND_PURGE,
            queue: super::QUEUE_BACKGROUND,
            every: Duration::from_hours(24),
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
///
/// The shutdown is consulted even while a tick is in flight: one slow
/// database round trip of an enqueue must not deafen the scheduler to the
/// stop signal, so a shutdown that resolves mid-tick ends the loop at
/// once — a half-issued period is exactly what the next tick's retry (or
/// the next app instance) is for.
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
            _ = ticker.tick() => {
                tokio::select! {
                    biased;
                    () = &mut shutdown => ticking = false,
                    () = tick(&pool, &jobs, clock_now(wall, started)) => {}
                }
            }
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
    drop(enqueue_keyed(pool, request, "periodic job").await);
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

    /// Steps the hand-cranked clock a second at a time until two reads of
    /// `kind`'s row count agree: the settled count after a scheduler run
    /// whose last shutdown may have abandoned an in-flight enqueue whose
    /// statement the server still lands.
    async fn settle_count(pool: &PgPool, kind: &str, mut last: i64) -> i64 {
        for _settle in 0..8 {
            tokio::time::advance(Duration::from_secs(1)).await;
            let next = count_jobs(pool, kind).await;
            if next == last {
                break;
            }
            last = next;
        }
        last
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

    /// [`run`] turns the schedule over on its tick and stops with its
    /// shutdown: two periods land as two rows, and nothing more arrives
    /// after the run returns.
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

        // The run's shutdown is the test's to give, not the queue's to
        // report: a oneshot the driving loop fires once it has seen two
        // periods itself. Reading the table inside the shutdown future held
        // `run`'s `select!` inside that round trip on every pass, and one
        // contended poll spanning ten seconds of virtual time is enough to
        // miss a tick outright — `MissedTickBehavior::Skip` drops the period
        // it could not serve and waits for the next — which is how a loaded
        // suite used to push the second row past any budget the 10 s tick
        // and the 20 ms step could leave room for.
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let scheduler = tokio::spawn(run(pool.clone(), jobs, async move {
            let _stopped = stop_rx.await;
        }));

        // The first tick fires at once and the next 10 s of virtual time
        // later, so two periods turn over in ~500 steps. The budget is far
        // larger because one step is one round trip and a busy Postgres
        // needs more of them; the loop stops the run itself as soon as the
        // second row has landed, so a third tick is out of reach either
        // way. And `run` hears the oneshot even with a tick handler in
        // flight — its handler selects on the same shutdown — so the
        // scheduler stops on the signal rather than on a race against the
        // next tick boundary.
        let rows = drive_clock(&pool, 2_500, |rows| rows >= 2).await;
        let _sent = stop_tx.send(());
        let _rows = drive_clock(&pool, 300, |_rows| scheduler.is_finished()).await;
        scheduler
            .await
            .expect("the scheduler task ran to completion");

        // A stopped handler's statement can already be at the server, so
        // one straggler period may still land behind the shutdown; the
        // assertions wait for the count to settle (two reads in a row
        // that agree) rather than pinning it to an instant's snapshot.
        let settled = settle_count(&pool, "heartbeat", rows).await;

        let keys = keys_for(&pool, "heartbeat").await;
        let buckets = keys
            .iter()
            .filter_map(|key| {
                let tail = key.strip_prefix("periodic:heartbeat:")?;
                tail.parse::<i64>().ok()
            })
            .collect::<Vec<i64>>();
        assert_eq!(
            buckets.len(),
            keys.len(),
            "every row carries the ticket's `periodic:heartbeat:<bucket>` key: {keys:?}"
        );
        assert!(
            buckets.len() >= 2,
            "one row for the current period and one for a later one: {keys:?}"
        );
        let mut periods = buckets.clone();
        periods.sort_unstable();
        periods.dedup();
        assert_eq!(
            periods.len(),
            buckets.len(),
            "the schedule turned over {buckets:?}: one row per period, no \
             bucket claimed twice"
        );

        // `run` returned when the shutdown resolved; 60 more seconds of
        // virtual time (six tick periods) prove nothing keeps ticking
        // behind it: a live scheduler adds rows on every boundary and the
        // re-settle outgrows the settled count, while the one straggler a
        // stopped handler could still land is already absorbed by it.
        tokio::time::advance(Duration::from_secs(60)).await;
        assert_eq!(
            settle_count(&pool, "heartbeat", settled).await,
            settled,
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

    /// Steps the paused clock 20 ms at a time until `ready` is satisfied by
    /// the `heartbeat` row count of that step, and fails the test past
    /// `budget` steps: the scheduler has to turn its schedule over (and has
    /// to notice its shutdown) without the suite waiting on it. The count
    /// that satisfied `ready` is the answer — the seed a later
    /// [`settle_count`] waits out.
    ///
    /// The round trip is the point of a step, not the number it reads: a
    /// task parked on Postgres is what lets the runtime drive the
    /// scheduler's own inserts to completion while the virtual clock stands
    /// still between steps.
    async fn drive_clock<R>(pool: &PgPool, budget: u32, ready: R) -> i64
    where
        R: Fn(i64) -> bool,
    {
        let started = std::time::Instant::now();
        let mut steps = 0_u32;
        loop {
            let rows = count_jobs(pool, "heartbeat").await;
            if ready(rows) {
                return rows;
            }
            assert!(
                steps < budget,
                "the schedule stalled at {rows} heartbeat rows after {steps} steps"
            );
            assert!(
                started.elapsed() < Duration::from_secs(60),
                "the scheduler wedged in real time"
            );
            steps += 1;
            tokio::time::advance(Duration::from_millis(20)).await;
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
