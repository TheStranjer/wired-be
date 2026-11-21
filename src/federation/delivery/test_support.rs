//! Fixtures shared by the delivery tests: the mock inboxes — one that
//! authenticates a delivery and ones that answer a fixed status — the test
//! [`AppState`]s over a live database, the signing actor and its remote
//! mirrors, the delivery arguments and attempt budgets, and the queue and
//! reachability-ledger reads the tests assert on. Test-only.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, HeaderValue, Method, StatusCode, Uri},
    response::{IntoResponse as _, Response},
    routing::post,
};
use chrono::{DateTime, TimeZone as _, Utc};
use reqwest::header::RETRY_AFTER;
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::{net::TcpListener, task::JoinHandle};
use uuid::Uuid;

use super::{FIRST_ATTEMPT, MAX_ATTEMPTS, handle};
use crate::{
    config::{Config, TEST_BASE_URL},
    federation::signature::verify::{
        SignedRequestParts, parse_signature_header, verify, verify_digest,
    },
    http_client::HttpClient,
    jobs::{AttemptInfo, JobError, QUEUE_FEDERATOR_OUTGOING},
    models::{
        job::Job,
        user::{NewUser, User, insert, local_ap_id},
    },
    state::AppState,
    uploads::UploaderBackend,
    util::keys::{FIXTURE_PRIVATE_PKCS1_PEM, FIXTURE_PUBLIC_PKCS1_PEM},
};

/// `User-Agent` of the test client (whose SSRF guard is off, so the
/// loopback mocks are reachable).
const TEST_UA: &str = "Wired (https://test.example)";
/// The path every mock inbox serves.
pub(super) const INBOX_PATH: &str = "/inbox";
/// The `keyId` a delivery from the fixture actor must carry.
pub(super) const ACTOR_KEY_ID: &str = "https://test.example/users/alice#main-key";
/// The `headers` parameter a signed delivery POST must name.
pub(super) const SIGNED_LINES: &str = "(request-target) host date digest";
/// The host every loopback mock answers on: the ledger key a delivery to
/// a mock records the host under.
pub(super) const LOOPBACK_HOST: &str = "127.0.0.1";

/// The state under test: `pool`, the test config, and a client that may
/// reach the loopback mocks.
pub(super) fn test_state(pool: &PgPool) -> AppState {
    state_with(pool, Config::for_tests())
}

/// The state under test: `pool` over `config`, with a client that may
/// reach the loopback mocks.
pub(super) fn state_with(pool: &PgPool, config: Config) -> AppState {
    let config = Arc::new(config);
    AppState {
        db: pool.clone(),
        hub: crate::streaming::hub::Hub::new(pool),
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

/// The attempt a first delivery runs on, with this ticket's budget.
pub(super) const fn first_attempt() -> AttemptInfo {
    AttemptInfo {
        attempts: FIRST_ATTEMPT,
        max_attempts: MAX_ATTEMPTS,
    }
}

/// The attempt right after a retry, for the outcomes that change with it.
pub(super) const fn second_attempt() -> AttemptInfo {
    AttemptInfo {
        attempts: 2,
        max_attempts: MAX_ATTEMPTS,
    }
}

/// The attempt that spends the [`MAX_ATTEMPTS`] budget: what a retryable
/// answer on it does is the last word this delivery says about the host.
pub(super) const fn last_attempt() -> AttemptInfo {
    AttemptInfo {
        attempts: MAX_ATTEMPTS,
        max_attempts: MAX_ATTEMPTS,
    }
}

/// The fixture activity: an addressable `Create` carrying both the
/// public recipients a peer may see and the blind ones it may not.
pub(super) fn activity() -> Value {
    json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": "https://test.example/activities/1",
        "type": "Create",
        "actor": "https://test.example/users/alice",
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "cc": ["https://test.example/users/alice/followers"],
        "bto": ["https://peer.example/users/carol"],
        "bcc": ["https://peer.example/users/dave"],
    })
}

/// The delivery arguments pointing at `inbox` and signing as
/// `actor_id`, in the shape [`enqueue_delivery`] writes.
fn delivery_args(inbox: &str, actor_id: Uuid) -> Value {
    json!({
        "inbox": inbox,
        "actor_id": actor_id,
        "body": activity(),
        "activity_id": activity()["id"],
    })
}

/// Binds an unused loopback port.
pub(super) async fn bind() -> TcpListener {
    TcpListener::bind("127.0.0.1:0").await.unwrap()
}

/// The `http://` origin a listener answers on.
pub(super) fn origin(listener: &TcpListener) -> String {
    format!("http://{}", listener.local_addr().unwrap())
}

/// Serves `app` on `listener` until the test aborts the task.
pub(super) fn spawn(listener: TcpListener, app: Router) -> JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, app).await {
            tracing::debug!("test server stopped: {err}");
        }
    })
}

/// A mock inbox that authenticates the delivery it is handed, plus the
/// shared record of what it saw and the task serving it.
pub(super) async fn verifying_inbox() -> (String, Seen, JoinHandle<()>) {
    let listener = bind().await;
    let inbox = format!("{}{INBOX_PATH}", origin(&listener));
    let observable = seen();
    let server = spawn(
        listener,
        Router::new()
            .route(INBOX_PATH, post(authenticate))
            .with_state(observable.clone()),
    );
    (inbox, observable, server)
}

/// The local signing actor every delivery test sends as: `alice` on the
/// test origin, holding the key pair the mock inbox verifies against.
pub(super) async fn actor(pool: &PgPool) -> User {
    let ap_id = local_ap_id(TEST_BASE_URL, "alice");
    insert(
        pool,
        &NewUser {
            inbox: Some(format!("{ap_id}/inbox")),
            public_key: Some(FIXTURE_PUBLIC_PKCS1_PEM.to_owned()),
            private_key: Some(FIXTURE_PRIVATE_PKCS1_PEM.to_owned()),
            ..NewUser::local("alice", &ap_id)
        },
    )
    .await
    .unwrap()
}

/// What a mock inbox saw, shared with the test that asserts on it.
#[derive(Clone, Debug, Default)]
pub(super) struct Seen {
    /// Requests the mock answered.
    pub(super) hits: Arc<AtomicUsize>,
    /// The headers of the most recent request.
    pub(super) headers: Arc<Mutex<Option<HeaderMap>>>,
    /// The body of the most recent request.
    pub(super) body: Arc<Mutex<Option<Vec<u8>>>>,
}

/// A mock inbox that has seen nothing yet.
fn seen() -> Seen {
    Seen::default()
}

/// The visible text of header `name`, empty when absent or unreadable.
pub(super) fn header_text(headers: &HeaderMap, name: &str) -> String {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned()
}

/// Records the request, authenticates it the way a real inbox must
/// ([`verify`] over the signed headers against the actor's public key,
/// plus [`verify_digest`] over the bytes received), and answers `202` to
/// a delivery that checks out and `401` to one that does not.
async fn authenticate(
    State(observable): State<Seen>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let _hits = observable.hits.fetch_add(1, Ordering::SeqCst);
    let _captured = observable.headers.lock().unwrap().replace(headers.clone());
    let _recorded = observable.body.lock().unwrap().replace(body.to_vec());
    let signature = header_text(&headers, "signature");
    let Ok(parsed) = parse_signature_header(&signature) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let path_and_query = uri.path_and_query().map_or_else(
        || uri.path().to_owned(),
        |target| target.as_str().to_owned(),
    );
    let request = SignedRequestParts {
        method: method.as_str(),
        path_and_query: &path_and_query,
        headers,
    };
    let authentic = verify(&parsed, &request, FIXTURE_PUBLIC_PKCS1_PEM, Utc::now()).is_ok()
        && verify_digest(&request.headers, &body).is_ok();
    if authentic {
        return StatusCode::ACCEPTED.into_response();
    }
    StatusCode::UNAUTHORIZED.into_response()
}

/// Answers every request with `status` and no `Retry-After`.
pub(super) async fn status_only(State(code): State<StatusCode>) -> StatusCode {
    code
}

/// Answers `429`, asking for `Retry-After: {seconds}`.
pub(super) async fn throttled(State(seconds): State<u32>) -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        [(RETRY_AFTER, seconds.to_string())],
    )
        .into_response()
}

/// A mock inbox that answers every request with `status` and nothing
/// else, plus the task serving it: the host is [`LOOPBACK_HOST`].
pub(super) async fn answering_inbox(status: StatusCode) -> (String, JoinHandle<()>) {
    let listener = bind().await;
    let inbox = format!("{}{INBOX_PATH}", origin(&listener));
    let server = spawn(
        listener,
        Router::new()
            .route(INBOX_PATH, post(status_only))
            .with_state(status),
    );
    (inbox, server)
}

/// Runs one [`handle`] attempt as the actor `actor_id`, delivering to
/// `inbox`.
pub(super) async fn attempt_to(
    state: &AppState,
    inbox: &str,
    actor_id: Uuid,
    attempt: AttemptInfo,
) -> Result<(), JobError> {
    handle(state.clone(), attempt, delivery_args(inbox, actor_id)).await
}

/// How a run of [`handle`] reads in a test message: the handler's own
/// `Display`, since a `Result` has none.
pub(super) fn outcome_text(outcome: &Result<(), JobError>) -> String {
    match outcome {
        Ok(()) => "completed".to_owned(),
        Err(err) => err.to_string(),
    }
}

/// The named parameter of a `Signature` header value, unquoted.
pub(super) fn signature_param(header: &str, name: &str) -> String {
    let prefix = format!("{name}=\"");
    let pair = header
        .split(',')
        .map(str::trim)
        .find(|pair| pair.starts_with(&prefix))
        .unwrap_or_else(|| panic!("signature header carries a {name} parameter: {header}"));
    let value = pair
        .strip_prefix(prefix.as_str())
        .unwrap_or_else(|| panic!("the {name} parameter is quoted: {header}"));
    value.trim_end_matches('"').to_owned()
}

/// A `HeaderMap` carrying only `Retry-After: {seconds}`.
pub(super) fn retry_after_headers(seconds: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    let _set: Option<HeaderValue> =
        headers.insert(RETRY_AFTER, HeaderValue::from_str(seconds).unwrap());
    headers
}

/// Inserts one instance-policy row.
pub(super) async fn policy(pool: &PgPool, domain: &str, action: &str) {
    let inserted =
        sqlx::query("INSERT INTO instance_policy_domains (domain, action) VALUES ($1, $2)")
            .bind(domain)
            .bind(action)
            .execute(pool)
            .await
            .unwrap();
    assert_eq!(inserted.rows_affected(), 1, "the policy row is there");
}

/// The one job the outgoing queue holds.
pub(super) async fn queued_job(pool: &PgPool) -> Job {
    sqlx::query_as::<_, Job>("SELECT * FROM jobs WHERE queue = $1 ORDER BY id LIMIT 1")
        .bind(QUEUE_FEDERATOR_OUTGOING)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// How many jobs the database holds.
pub(super) async fn job_count(pool: &PgPool) -> i64 {
    let (count,): (i64,) = sqlx::query_as("SELECT count(*) FROM jobs")
        .fetch_one(pool)
        .await
        .unwrap();
    count
}

/// The mark `host` carries in the reachability ledger: `None` when it
/// has no row at all, or a row carrying none.
pub(super) async fn mark_of(pool: &PgPool, host: &str) -> Option<DateTime<Utc>> {
    sqlx::query_scalar::<_, Option<DateTime<Utc>>>(
        "SELECT unreachable_since FROM instances WHERE host = $1",
    )
    .bind(host)
    .fetch_optional(pool)
    .await
    .unwrap()
    .flatten()
}

/// Whether the reachability ledger holds a row for `host`.
pub(super) async fn has_row(pool: &PgPool, host: &str) -> bool {
    let (rows,): (i64,) = sqlx::query_as("SELECT count(*) FROM instances WHERE host = $1")
        .bind(host)
        .fetch_one(pool)
        .await
        .unwrap();
    rows > 0
}

/// One fixed minute in June 2025 — whole minutes round-trip through the
/// `timestamptz` columns unchanged, so a test comparing a mark it wrote
/// compares exactly.
pub(super) fn stamp(day: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2025, 6, day, 0, 0, 0)
        .single()
        .expect("a real minute in June 2025")
}

/// Inserts a remote mirror named `nickname`, carrying the given inbox
/// endpoints, for the tests that need an actor this instance does not own.
pub(super) async fn mirror(
    pool: &PgPool,
    nickname: &str,
    inbox: Option<&str>,
    shared: Option<&str>,
) -> User {
    insert(
        pool,
        &NewUser {
            nickname: format!("{nickname}@peer.example"),
            ap_id: format!("https://peer.example/users/{nickname}"),
            domain: Some("peer.example".to_owned()),
            inbox: inbox.map(str::to_owned),
            shared_inbox: shared.map(str::to_owned),
            ..NewUser::default()
        },
    )
    .await
    .unwrap()
}
