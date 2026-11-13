//! Periodic `instances.probe` job: re-check long-unreachable hosts over
//! their `nodeinfo` discovery document.
//!
//! `docs/ARCHITECTURE.md` decision 11 keeps background work in Postgres rows;
//! this module is the reader of the reachability ledger in
//! [`crate::models::instance`]. The delivery path marks a host whose inbox
//! refused a delivery ([`set_unreachable`]) and the publisher skips marked
//! hosts, so a peer that recovered — but happens to receive nothing, or is
//! only ever probed by deliveries that keep failing — would stay marked and
//! stay skipped forever. This six-hourly pass asks every host marked longer
//! than a day the one question that needs no activity to send: a `GET` on
//! `/.well-known/nodeinfo`, the discovery document every federated instance
//! must serve. Any `2xx` answer is proof of life and clears the mark through
//! [`set_reachable`]; a timeout, a transport failure or any other answer
//! leaves the mark exactly where it was.
//!
//! Marks younger than a day are left alone: the delivery path clears a recent
//! mark by itself as soon as one inbox post works, so probing a host the
//! moment it went quiet only spends requests on a dead peer. And nothing here
//! ever deletes a row: a mark is a fact about when a host went quiet (see the
//! [`instance`] model docs), so a host unreachable for thirty days or thirty
//! years stays marked and stays in the table.
//!
//! [`instance`]: crate::models::instance
//! [`set_reachable`]: crate::models::instance::set_reachable
//! [`set_unreachable`]: crate::models::instance::set_unreachable

use std::time::Duration;

use chrono::{TimeDelta, Utc};
use serde_json::Value;
use tracing::{info, warn};

use crate::{
    jobs::{AttemptInfo, HandlerFuture, JobError},
    models::instance,
    state::AppState,
};

/// The job kind of the unreachable-host probe, run from the
/// [`periodic_jobs`](super::scheduler::periodic_jobs) registry every six
/// hours on [`QUEUE_BACKGROUND`](super::QUEUE_BACKGROUND) through the
/// [`JobRegistry`](super::JobRegistry) handler [`handle_probe`].
pub const KIND_PROBE: &str = "instances.probe";

/// Time one host's `nodeinfo` request may take before the probe gives up on
/// it: five seconds. A host that answers its discovery document slower than
/// that is not a host this instance wants to spend fan-out on anyway, and
/// the sweep must not let one stalled peer hold up the rest of the ledger.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a mark must have stood before its host becomes probe material:
/// one day, the rule this module's [docs][self] state.
const PROBE_AFTER: TimeDelta = TimeDelta::days(1);

/// The body cap the probe reads under: 64 KiB. A `nodeinfo` discovery
/// document is a small JSON list of links, so this is orders of magnitude
/// past any honest answer and far below what a hostile peer could use to
/// make this process buffer.
const NODEINFO_MAX_BYTES: usize = 64 * 1024;

/// The base a production probe hangs the discovery path off:
/// `https://{host}`, the scheme federation endpoints are served under.
fn https_base(host: &str) -> String {
    format!("https://{host}")
}

/// The URL one probe fetches: the host's `nodeinfo` discovery document,
/// `{base}/.well-known/nodeinfo`.
fn probe_nodeinfo_url(base: &str) -> String {
    format!("{base}/.well-known/nodeinfo")
}

/// Whether a peer's status code proves it alive for this probe: any `2xx`.
const fn answers(status: u16) -> bool {
    matches!(status, 200..=299)
}

/// The [`JobRegistry`](super::JobRegistry) handler for [`KIND_PROBE`]: one
/// probe sweep over the unreachable-host ledger with the spec `https://{host}`
/// URLs and the production `PROBE_TIMEOUT` budget.
pub fn handle_probe(state: AppState, _attempt: AttemptInfo, _args: Value) -> HandlerFuture {
    Box::pin(async move { probe_unreachable(&state, None, PROBE_TIMEOUT).await })
}

/// Runs one probe sweep: read the [`list_unreachable`] ledger, probe every
/// host whose mark predates [`PROBE_AFTER`], clear the marks of those that
/// answer `2xx`, and log the sweep's counts.
///
/// [`list_unreachable`]: crate::models::instance::list_unreachable
///
/// `base_override` replaces every probed host's [`https_base`] with the
/// caller's origin; production passes [`None`] and speaks the spec, tests
/// pass their loopback mock's `http://` origin — the same shape
/// [`resolve_acct_with_base`] uses, because the mocks cannot serve `https`.
/// `probe_timeout` is the per-host budget: production passes
/// [`PROBE_TIMEOUT`], a timeout test passes a tiny one to watch the guard
/// fire against a mock that never answers.
///
/// Each host is probed on its own and no host's outcome but the ledger read
/// aborts the sweep: a host that stays silent costs one `warn!`.
///
/// [`resolve_acct_with_base`]: crate::federation::webfinger_client::resolve_acct_with_base
///
/// # Errors
///
/// A failed ledger read is [`JobError::Retry`] — with no ledger there is
/// nothing to probe, and the queue should try again on a working database.
/// Every other failure is absorbed per host and the sweep still completes.
pub(crate) async fn probe_unreachable(
    state: &AppState,
    base_override: Option<&str>,
    probe_timeout: Duration,
) -> Result<(), JobError> {
    let ledger = instance::list_unreachable(&state.db)
        .await
        .map_err(|err| JobError::Retry(format!("unreachable-host ledger read failed: {err}")))?;
    let cutoff = Utc::now() - PROBE_AFTER;
    let mut probed = 0_usize;
    let mut cleared = 0_usize;
    for (host, _) in ledger.iter().filter(|(_, since)| *since < cutoff) {
        probed += 1;
        if probe_host(state, host, base_override, probe_timeout).await {
            cleared += 1;
        }
    }
    info!(
        marked = ledger.len(),
        probed, cleared, "unreachable host probe sweep finished"
    );
    Ok(())
}

/// Probes one marked host and, when its `nodeinfo` answers `2xx`, clears the
/// mark; returns whether the host ends the probe reachable.
///
/// Every negative path — the timeout elapsing, a transport or policy
/// failure, a non-`2xx` answer, a mark write the database refused — is
/// logged and answers `false` with the mark left exactly as it was: one
/// host's silence must never abort the sweep over the rest of the ledger.
async fn probe_host(
    state: &AppState,
    host: &str,
    base_override: Option<&str>,
    probe_timeout: Duration,
) -> bool {
    let base = base_override.map_or_else(|| https_base(host), str::to_owned);
    let url = probe_nodeinfo_url(&base);
    let answer = tokio::time::timeout(
        probe_timeout,
        state.http_client.get_limited(&url, &[], NODEINFO_MAX_BYTES),
    )
    .await;
    let live = match answer {
        Ok(Ok(response)) => answers(response.status),
        Ok(Err(err)) => {
            warn!(%err, host, "nodeinfo probe request failed");
            false
        }
        Err(_elapsed) => {
            warn!(host, "nodeinfo probe timed out");
            false
        }
    };
    if !live {
        return false;
    }
    match instance::set_reachable(&state.db, host).await {
        Ok(()) => true,
        Err(err) => {
            warn!(%err, host, "could not clear the unreachable mark");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        net::SocketAddr,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use axum::{
        Router,
        body::Body,
        extract::State,
        http::{Method, StatusCode, Uri, header::CONTENT_TYPE},
        response::{IntoResponse as _, Response},
        routing::get,
    };
    use chrono::{DateTime, Utc};
    use serde_json::json;
    use sqlx::{PgPool, postgres::PgPoolOptions};
    use tokio::{net::TcpListener, sync::Mutex, task::JoinHandle};

    use super::*;
    use crate::{
        config::{Config, TEST_BASE_URL},
        http_client::HttpClient,
        jobs::{QUEUE_BACKGROUND, scheduler::periodic_jobs},
        models::user::{NewUser, insert, local_ap_id},
        streaming::hub::Hub,
        uploads::UploaderBackend,
    };

    /// `User-Agent` of the probe client the tests run.
    const TEST_UA: &str = "Wired (https://test.example)";

    /// The path every probe is expected to ask for.
    const NODEINFO_PATH: &str = "/.well-known/nodeinfo";

    /// One day, in the seconds the fixtures count ages in.
    const DAY: i64 = 86_400;

    /// Far inside the one-day probe age: a host marked this long ago is
    /// never eligible, and no test race over the cutoff can make it so.
    const YOUNGER: i64 = 60;

    /// The handler's attempt info: the first of however many the sweep gets.
    const FIRST_ATTEMPT: AttemptInfo = AttemptInfo {
        attempts: 1,
        max_attempts: 5,
    };

    /// A tiny probe budget for the timeout test: long enough for the request
    /// to reach a mock that accepts it, far shorter than the mock's stall.
    const TINY_TIMEOUT: Duration = Duration::from_millis(100);

    /// Test state over `pool`: the loopback-tolerant client the mocks answer
    /// (guard disabled), so the probe's requests reach `http://127.0.0.1`
    /// targets.
    fn app_state(pool: &PgPool) -> AppState {
        let config = Arc::new(Config::for_tests());
        AppState {
            db: pool.clone(),
            hub: Hub::new(pool),
            config: Arc::clone(&config),
            http_client: HttpClient::new_for_tests(TEST_UA),
            uploader: UploaderBackend::from(config.as_ref()),
            instance_stats_cache: Arc::default(),
            mrf_config_cache: Arc::default(),
            translation_languages_cache: Arc::default(),
            rate_limiter: Arc::new(crate::rate_limit::RateLimiter::from_config(config.as_ref())),
            mailer: Arc::new(crate::mailer::MemoryTransport::new()),
        }
    }

    /// Test state over a pool that cannot answer: closed before it ever
    /// connected, which is how the sweep sees a database that went away.
    async fn broken_state() -> AppState {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://localhost/unused")
            .expect("a lazy pool needs no database");
        pool.close().await;
        app_state(&pool)
    }

    /// Marks `host` unreachable `secs` seconds in the past — the shape a real
    /// delivery failure leaves behind — and ages its row's stamps to the
    /// same moment, so the next write's `updated_at` bump is an observable
    /// move rather than a race with the clock.
    async fn marked_ago(pool: &PgPool, host: &str, secs: i64) {
        let since =
            Utc::now() - TimeDelta::try_seconds(secs).expect("the fixture ages are representable");
        instance::set_unreachable(pool, host, since)
            .await
            .expect("the mark writes");
        let aged =
            sqlx::query("UPDATE instances SET inserted_at = $2, updated_at = $2 WHERE host = $1")
                .bind(host)
                .bind(since)
                .execute(pool)
                .await
                .expect("the stamps are settable");
        assert_eq!(aged.rows_affected(), 1, "the marked host has a row to age");
    }

    /// `host`'s `instances` row: `inserted_at`, `updated_at` and its mark.
    async fn row(
        pool: &PgPool,
        host: &str,
    ) -> (DateTime<Utc>, DateTime<Utc>, Option<DateTime<Utc>>) {
        sqlx::query_as::<_, (DateTime<Utc>, DateTime<Utc>, Option<DateTime<Utc>>)>(
            "SELECT inserted_at, updated_at, unreachable_since FROM instances WHERE host = $1",
        )
        .bind(host)
        .fetch_one(pool)
        .await
        .expect("the host has a row")
    }

    /// Whether `host` still has a row in `instances`.
    async fn has_row(pool: &PgPool, host: &str) -> bool {
        let rows = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM instances WHERE host = $1")
            .bind(host)
            .fetch_one(pool)
            .await
            .expect("the row count reads");
        rows == 1
    }

    /// The mark `host`'s row carries.
    async fn mark_of(pool: &PgPool, host: &str) -> Option<DateTime<Utc>> {
        row(pool, host).await.2
    }

    /// What a mock nodeinfo server records: how many requests arrived and
    /// what each one asked for.
    #[derive(Clone, Debug, Default)]
    struct Seen {
        /// Requests the mock received (the stall mock counts one even
        /// though the probe never sees an answer).
        hits: Arc<AtomicUsize>,
        /// The `METHOD /path` of each request, in arrival order.
        paths: Arc<Mutex<Vec<String>>>,
    }

    impl Seen {
        /// The hits recorded so far.
        fn count(&self) -> usize {
            self.hits.load(Ordering::SeqCst)
        }

        /// The requests recorded so far, in arrival order.
        async fn requested(&self) -> Vec<String> {
            self.paths.lock().await.clone()
        }
    }

    /// The mock's shared state: the canned answer plus the capture.
    #[derive(Clone, Debug)]
    struct Mock {
        /// The status a responding mock answers with.
        status: StatusCode,
        /// When set, the handler stalls long past any probe budget instead
        /// of answering.
        stall: bool,
        /// Where every request is recorded.
        seen: Seen,
    }

    impl Mock {
        /// A mock that answers `status` right away.
        fn answering(status: StatusCode) -> Self {
            Self {
                status,
                stall: false,
                seen: Seen::default(),
            }
        }

        /// A mock that records the request and then stalls, so the probe's
        /// own timeout always wins the race.
        fn stalling() -> Self {
            Self {
                status: StatusCode::OK,
                stall: true,
                seen: Seen::default(),
            }
        }
    }

    /// How long the stall mock holds a request open: far past any timeout a
    /// test injects.
    const STALL: Duration = Duration::from_secs(30);

    /// The mock's handler: record the request, then answer the canned status
    /// (a small `nodeinfo` discovery document for `200`, text otherwise) or
    /// stall until the test runtime tears the server down.
    async fn catch_request(State(mock): State<Mock>, method: Method, uri: Uri) -> Response {
        let _hits = mock.seen.hits.fetch_add(1, Ordering::SeqCst);
        mock.seen
            .paths
            .lock()
            .await
            .push(format!("{} {}", method.as_str(), uri.path()));
        if mock.stall {
            tokio::time::sleep(STALL).await;
        }
        if mock.status == StatusCode::OK {
            return (
                mock.status,
                [(CONTENT_TYPE, "application/json")],
                r#"{"links":[{"rel":"http://nodeinfo.diaspora.software/ns/schema/2.1","href":"https://mock.example/nodeinfo/2.1"}]}"#,
            )
                .into_response();
        }
        (mock.status, Body::from("not nodeinfo")).into_response()
    }

    /// Serves `mock` on a fresh loopback port; returns its origin, the
    /// capture and the server task.
    async fn spawn_mock(mock: Mock) -> (String, Seen, JoinHandle<()>) {
        let seen = mock.seen.clone();
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a loopback port to serve on");
        let addr = listener.local_addr().expect("the bound address");
        let app = Router::new()
            .route(NODEINFO_PATH, get(catch_request))
            .with_state(mock);
        let server = tokio::spawn(async move {
            if let Err(err) = axum::serve(listener, app).await {
                tracing::debug!("test nodeinfo server stopped: {err}");
            }
        });
        (format!("http://{addr}"), seen, server)
    }

    /// A mock nodeinfo endpoint answering every `GET` with `status`:
    /// `(origin, capture, server)`.
    async fn mock_nodeinfo(status: StatusCode) -> (String, Seen, JoinHandle<()>) {
        spawn_mock(Mock::answering(status)).await
    }

    /// A mock nodeinfo endpoint that takes requests and never answers them.
    async fn mock_stall() -> (String, Seen, JoinHandle<()>) {
        spawn_mock(Mock::stalling()).await
    }

    /// An origin no server listens on: requests to it are refused, the
    /// gone-host shape.
    async fn dead_origin() -> String {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a loopback port to measure");
        let addr: SocketAddr = listener.local_addr().expect("the bound address");
        drop(listener);
        format!("http://{addr}")
    }

    /// Inserts a local user named `nickname`: a row outside the ledger a
    /// test checks the sweep leaves alone.
    async fn user(pool: &PgPool, nickname: &str) {
        let _created = insert(
            pool,
            &NewUser::local(nickname, &local_ap_id(TEST_BASE_URL, nickname)),
        )
        .await
        .expect("the local fixture inserts");
    }

    /// How many `users` rows exist.
    async fn user_count(pool: &PgPool) -> i64 {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM users")
            .fetch_one(pool)
            .await
            .expect("the user count reads")
    }

    #[test]
    fn the_probe_url_is_the_nodeinfo_document_on_the_host_base() {
        assert_eq!(
            probe_nodeinfo_url(&https_base("peer.example")),
            "https://peer.example/.well-known/nodeinfo",
            "the spec URL the production probe fetches"
        );
        assert_eq!(
            probe_nodeinfo_url("http://127.0.0.1:9999"),
            "http://127.0.0.1:9999/.well-known/nodeinfo",
            "a test base gets the same path"
        );
    }

    #[test]
    fn only_2xx_answers_prove_a_host_alive() {
        for status in [200_u16, 201, 204, 299] {
            assert!(answers(status), "{status} is a live answer");
        }
        for status in [100_u16, 301, 404, 500] {
            assert!(!answers(status), "{status} proves nothing");
        }
    }

    /// A host marked beyond the one-day age whose mock answers `200` has its
    /// mark cleared, and the sweep writes nothing else: the row itself stays,
    /// no other `instances` row moves, and no other table is touched.
    #[tokio::test]
    async fn a_host_answered_200_has_its_mark_cleared() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let (origin, seen, server) = mock_nodeinfo(StatusCode::OK).await;
        marked_ago(&pool, "back.example", 2 * DAY).await;
        marked_ago(&pool, "quiet.example", YOUNGER).await;
        instance::touch(&pool, "clean.example").await.unwrap();
        user(&pool, "alice").await;
        let quiet_before = row(&pool, "quiet.example").await;
        let clean_before = row(&pool, "clean.example").await;
        let users_before = user_count(&pool).await;

        let outcome = probe_unreachable(&app_state(&pool), Some(&origin), PROBE_TIMEOUT).await;

        assert_eq!(outcome, Ok(()), "a sweep that recovers one host succeeds");
        assert_eq!(seen.count(), 1, "the eligible host was probed once");
        assert_eq!(
            seen.requested().await,
            vec![format!("GET {NODEINFO_PATH}")],
            "the probe asked the nodeinfo discovery document"
        );
        let (inserted, updated, mark) = row(&pool, "back.example").await;
        assert!(mark.is_none(), "the live host's mark is cleared");
        assert!(
            has_row(&pool, "back.example").await,
            "clearing leaves the row itself in the table"
        );
        assert!(updated > inserted, "the write bumps updated_at");
        assert_eq!(
            row(&pool, "quiet.example").await,
            quiet_before,
            "the unprobed marked row is untouched"
        );
        assert_eq!(
            row(&pool, "clean.example").await,
            clean_before,
            "the unmarked row is untouched"
        );
        assert_eq!(
            user_count(&pool).await,
            users_before,
            "the ledger alone is written"
        );
        server.abort();
    }

    /// A mock answering `500` proves nothing: the mark stands, untouched,
    /// and the sweep still succeeds.
    #[tokio::test]
    async fn a_host_answered_500_keeps_its_mark() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let (origin, seen, server) = mock_nodeinfo(StatusCode::INTERNAL_SERVER_ERROR).await;
        marked_ago(&pool, "grumpy.example", 2 * DAY).await;
        let before = mark_of(&pool, "grumpy.example").await;

        let outcome = probe_unreachable(&app_state(&pool), Some(&origin), PROBE_TIMEOUT).await;

        assert_eq!(outcome, Ok(()), "a non-2xx answer is absorbed, not fatal");
        assert_eq!(seen.count(), 1, "the host was probed once");
        assert_eq!(
            mark_of(&pool, "grumpy.example").await,
            before,
            "a 5xx answer leaves the mark exactly where it was"
        );
        server.abort();
    }

    /// A mock that accepts the connection and never answers loses the race
    /// against the tiny injected timeout: the mark stands and the sweep
    /// finishes anyway.
    #[tokio::test]
    async fn a_host_that_never_answers_times_out_and_keeps_its_mark() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let (origin, seen, server) = mock_stall().await;
        marked_ago(&pool, "stalled.example", 2 * DAY).await;
        let before = mark_of(&pool, "stalled.example").await;

        let outcome = probe_unreachable(&app_state(&pool), Some(&origin), TINY_TIMEOUT).await;

        assert_eq!(outcome, Ok(()), "a stalled peer cannot hang the sweep");
        assert_eq!(seen.count(), 1, "the request reached the mock");
        assert_eq!(
            mark_of(&pool, "stalled.example").await,
            before,
            "the timeout leaves the mark alone"
        );
        server.abort();
    }

    /// A host whose origin refuses every connection keeps its mark, and the
    /// sweep still completes.
    #[tokio::test]
    async fn a_gone_host_keeps_its_mark() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let origin = dead_origin().await;
        marked_ago(&pool, "gone.example", 2 * DAY).await;
        let before = mark_of(&pool, "gone.example").await;

        let outcome = probe_unreachable(&app_state(&pool), Some(&origin), PROBE_TIMEOUT).await;

        assert_eq!(outcome, Ok(()), "a refused connection is absorbed");
        assert_eq!(
            mark_of(&pool, "gone.example").await,
            before,
            "the gone host stays marked"
        );
    }

    /// A host marked less than a day ago is not probe material: the mock
    /// observes zero requests and the mark stands.
    #[tokio::test]
    async fn a_fresh_mark_is_not_probed() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let (origin, seen, server) = mock_nodeinfo(StatusCode::OK).await;
        marked_ago(&pool, "recent.example", YOUNGER).await;
        let before = mark_of(&pool, "recent.example").await;

        let outcome = probe_unreachable(&app_state(&pool), Some(&origin), PROBE_TIMEOUT).await;

        assert_eq!(outcome, Ok(()), "nothing to probe is a clean pass");
        assert_eq!(seen.count(), 0, "the fresh mark was never probed");
        assert_eq!(
            mark_of(&pool, "recent.example").await,
            before,
            "and the young mark stands untouched"
        );
        server.abort();
    }

    /// The probe never deletes a row, however long the host has been quiet:
    /// a mark beyond thirty days that no `2xx` clears keeps both mark and
    /// row.
    #[tokio::test]
    async fn a_host_unreachable_beyond_thirty_days_is_never_deleted() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let (origin, seen, server) = mock_nodeinfo(StatusCode::SERVICE_UNAVAILABLE).await;
        marked_ago(&pool, "dead.example", 40 * DAY).await;
        let before = row(&pool, "dead.example").await;

        let outcome = probe_unreachable(&app_state(&pool), Some(&origin), PROBE_TIMEOUT).await;

        assert_eq!(
            outcome,
            Ok(()),
            "a 5xx past any retention horizon is still absorbed"
        );
        assert_eq!(seen.count(), 1, "the long-quiet host was probed");
        assert!(has_row(&pool, "dead.example").await, "the row stays");
        assert_eq!(
            row(&pool, "dead.example").await,
            before,
            "marked and unmoved: the sweep deletes nothing"
        );
        server.abort();
    }

    /// A mixed ledger: the sweep probes exactly the hosts whose marks predate
    /// the one-day age — the two older ones — and leaves the younger mark
    /// standing.
    #[tokio::test]
    async fn a_mixed_ledger_probes_only_the_eligible_hosts() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let (origin, seen, server) = mock_nodeinfo(StatusCode::OK).await;
        marked_ago(&pool, "ancient.example", 10 * DAY).await;
        marked_ago(&pool, "stale.example", 2 * DAY).await;
        marked_ago(&pool, "recent.example", YOUNGER).await;
        let fresh = mark_of(&pool, "recent.example").await;

        let outcome = probe_unreachable(&app_state(&pool), Some(&origin), PROBE_TIMEOUT).await;

        assert_eq!(outcome, Ok(()), "the mixed sweep completes");
        assert_eq!(
            seen.count(),
            2,
            "exactly the two eligible hosts were probed"
        );
        assert_eq!(
            [
                mark_of(&pool, "ancient.example").await,
                mark_of(&pool, "stale.example").await,
                mark_of(&pool, "recent.example").await,
            ],
            [None, None, fresh],
            "the eligible marks cleared and the fresh one stands"
        );
        server.abort();
    }

    /// A ledger the database cannot read is a retryable failure: with no
    /// ledger there is nothing to probe, and the queue keeps the pass for a
    /// working database.
    #[tokio::test]
    async fn an_unreadable_ledger_retries_the_sweep() {
        let state = broken_state().await;

        let outcome = probe_unreachable(&state, Some("http://127.0.0.1:9"), PROBE_TIMEOUT).await;

        match outcome {
            Err(JobError::Retry(detail)) => assert!(
                detail.starts_with("unreachable-host ledger read failed"),
                "the retry states what failed: {detail}"
            ),
            Ok(()) => panic!("a closed pool must not read as a successful sweep"),
            Err(other) => panic!("a closed ledger read is retryable, not {other:?}"),
        }
    }

    /// The probe is wired into both registries the ticket names: the worker
    /// can run its kind, and the scheduler places it on the maintenance
    /// queue every six hours.
    #[test]
    fn the_kind_is_registered_and_scheduled() {
        let registry = crate::job_registry();
        assert!(
            registry.get(KIND_PROBE).is_some(),
            "the worker knows `{KIND_PROBE}`"
        );

        let job = periodic_jobs()
            .into_iter()
            .find(|job| job.kind == KIND_PROBE)
            .expect("the probe is on the periodic schedule");
        assert_eq!(job.queue, QUEUE_BACKGROUND, "on the maintenance queue");
        assert_eq!(job.every, Duration::from_hours(6), "every six hours");
    }

    /// The handler the registry binds to the kind is this module's sweep:
    /// running it over an empty ledger completes cleanly.
    #[tokio::test]
    async fn the_registered_handler_is_this_sweep() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let registry = crate::job_registry();
        let handler = registry
            .get(KIND_PROBE)
            .expect("the probe's kind is registered");

        let outcome = handler(app_state(&pool), FIRST_ATTEMPT, json!({})).await;

        assert_eq!(
            outcome,
            Ok(()),
            "the registered handler runs the sweep, ledger empty"
        );
    }
}
