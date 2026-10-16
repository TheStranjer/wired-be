//! Outbound delivery: the `federator.deliver` job that POSTs one signed
//! activity to one remote inbox, with retries.
//!
//! The reference is Pleroma's publishing pipeline
//! (`lib/pleroma/web/activity_pub/publisher.ex`'s `prepare_one/1` and
//! `publish_one/1`, `lib/pleroma/workers/publisher_worker.ex` on the
//! `:federator_outgoing` queue): one job per `(activity, inbox)` pair, the
//! activity signed as its actor, and the peer's answer deciding whether the
//! delivery is done, doomed or merely not yet.
//!
//! [`enqueue_delivery`] is the producing half and [`handle`] the consuming
//! one, registered under [`KIND_DELIVER`] by [`crate::serve`]. Enqueue never
//! talks to the peer: it refuses an inbox this instance serves itself (the
//! activity is already here), one that is not an `http(s)` URL, and one whose
//! host the instance's own policy rejects, and in every one of those cases it
//! reports `Ok(())` without queueing anything — refusing to federate is not a
//! failure. The same `(activity, inbox)` pair is enqueued once: the job's
//! `unique_key` (`deliver:{activity_id}:{inbox}`) makes the queue's dedup
//! index swallow a repeated call while a delivery for it is still live.
//!
//! A delivery attempt re-reads the signing actor from the database (a local
//! row with its own private key: decision 7), drops the activity's blind
//! recipients (`bto`/`bcc` are for the actor's own followers, never for the
//! wire), serializes it to bytes *once* — the same bytes are digested, signed
//! and sent — and posts it through the SSRF-guarded
//! [`HttpClient`](crate::http_client::HttpClient) with its 20 s total
//! timeout. The answer decides the outcome: `2xx` completes the job, the
//! refusals that never succeed discard it, everything else retries — at the
//! peer's own `Retry-After` moment when it sent one (capped at six hours, so
//! a hostile header cannot park a delivery forever) and on the queue's
//! [`backoff`](crate::jobs::backoff) otherwise, until the
//! [`MAX_ATTEMPTS`] budget is spent.

use std::time::Duration;

use chrono::Utc;
use reqwest::header::{HeaderMap, RETRY_AFTER};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgPool;
use tracing::{debug, warn};
use url::Url;
use uuid::Uuid;

use crate::{
    error::{AppError, AppResult},
    federation::signature::{SignedHeaders, SigningKey, sign_request},
    http_client::HttpError,
    jobs::{AttemptInfo, HandlerFuture, JobError, QUEUE_FEDERATOR_OUTGOING},
    models::{
        instance_policy_domain,
        job::{NewJob, enqueue},
        user::{self, User},
    },
    state::AppState,
};

/// The job kind of one inbox delivery, on
/// [`QUEUE_FEDERATOR_OUTGOING`].
pub const KIND_DELIVER: &str = "federator.deliver";

/// Attempts a delivery gets before it is given up on.
///
/// Pleroma's publisher worker retries a handful of times per inbox and then
/// leaves the pair undelivered; seven is that handful, and the queue's
/// [`backoff`](crate::jobs::backoff) keeps the last attempts hours apart.
pub const MAX_ATTEMPTS: i32 = 7;

/// The media type a delivery POST carries and asks for.
const ACTIVITY_JSON: &str = "application/activity+json";

/// The key fragment appended to an actor id to form its `keyId`, the
/// convention both Mastodon and Pleroma use.
const KEY_ID_SUFFIX: &str = "#main-key";

/// The prefix of a delivery's deduplication key.
const UNIQUE_KEY_PREFIX: &str = "deliver";

/// Ceiling for a peer's `Retry-After`: six hours, the same horizon the
/// queue's own backoff caps at, so no peer can park a delivery longer than
/// the worker's own worst wait.
const MAX_RETRY_AFTER: Duration = Duration::from_hours(6);

/// The only attempt a `401`/`403` is still retried on: the first one, because
/// a key that has just rotated reads as a bad signature once, and a refusal
/// that survives a refresh window is a real refusal.
const FIRST_ATTEMPT: i32 = 1;

/// The JSON arguments of a [`KIND_DELIVER`] job: the four fields
/// [`enqueue_delivery`] writes and [`handle`] reads.
#[derive(Debug, Deserialize)]
struct DeliveryArgs {
    /// The remote inbox to `POST` to.
    inbox: String,
    /// `users.id` of the local actor to sign as.
    actor_id: Uuid,
    /// The activity document, as the producer serialized it.
    body: Value,
    /// The activity's own `ActivityPub` id: what a delivery is for.
    activity_id: String,
}

/// The inbox to deliver to `actor`: their shared (instance-wide) inbox when
/// they published one, their personal inbox otherwise.
///
/// `ActivityPub`'s shared-inbox rule, Pleroma's `maybe_use_sharedinbox/1`;
/// `None` means there is nowhere to send anything at all.
///
/// This only picks the address: whether several recipients on one host share
/// a single POST is the caller's decision, and every caller starts from the
/// same choice.
#[must_use]
pub fn best_inbox(actor: &User) -> Option<String> {
    actor.shared_inbox.clone().or_else(|| actor.inbox.clone())
}

/// Queues one signed delivery of `activity` to `inbox`, signed as the local
/// actor `actor`.
///
/// The activity rides in the job's `body` argument as JSON and is serialized
/// to bytes by [`handle`], once per attempt, because the bytes that are sent
/// must be exactly the bytes that were digested and signed. The job lands on
/// [`QUEUE_FEDERATOR_OUTGOING`] with [`MAX_ATTEMPTS`] and the deduplication
/// key `deliver:{activity_id}:{inbox}`, so calling this twice for the same
/// pair while a delivery is live queues nothing the second time.
///
/// Three inboxes are refused without an error and without a job: one this
/// instance serves itself (its authority is the actor's own, so the activity
/// needs no round trip through HTTP), one that is not an `http(s)` URL with a
/// host, and one whose host [`instance_policy_domain`] rejects.
///
/// [`instance_policy_domain`]: crate::models::instance_policy_domain::is_rejected
///
/// # Errors
///
/// [`AppError::Unprocessable`] when `activity` carries no string `id` (every
/// federating document has one, so that is a producer bug); a database
/// failure arrives through `?` as
/// [`AppError::Database`](crate::error::AppError).
pub async fn enqueue_delivery(
    pool: &PgPool,
    inbox: &str,
    actor: &User,
    activity: &Value,
) -> AppResult<()> {
    let Some(target) = parse_inbox(inbox) else {
        warn!(inbox, "delivery refused: the inbox is not an http(s) URL");
        return Ok(());
    };
    if is_local_inbox(&target, &actor.ap_id) {
        debug!(inbox, "delivery skipped: the inbox is this instance's own");
        return Ok(());
    }
    let host = target.host_str().unwrap_or_default();
    if instance_policy_domain::is_rejected(pool, host).await? {
        debug!(inbox, "delivery refused: instance policy rejects {host}");
        return Ok(());
    }
    let activity_id = activity_id_of(activity)?;
    let unique_key = format!("{UNIQUE_KEY_PREFIX}:{activity_id}:{inbox}");
    let queued = enqueue(
        pool,
        NewJob {
            queue: QUEUE_FEDERATOR_OUTGOING,
            kind: KIND_DELIVER,
            args: json!({
                "inbox": inbox,
                "actor_id": actor.id,
                "body": activity,
                "activity_id": activity_id,
            }),
            run_at: None,
            max_attempts: Some(MAX_ATTEMPTS),
            unique_key: Some(&unique_key),
        },
    )
    .await?;
    if queued.is_none() {
        debug!(%unique_key, "delivery already queued for this activity and inbox");
    }
    Ok(())
}

/// The [`KIND_DELIVER`] handler, registered by [`crate::serve`]: runs one
/// delivery attempt and reports its outcome to the worker.
///
/// An attempt reads the signing actor, strips the blind recipients, signs the
/// bytes it sends and classifies the peer's answer, as this module's docs
/// describe.
pub fn handle(state: AppState, attempt: AttemptInfo, args: Value) -> HandlerFuture {
    Box::pin(async move { deliver(state, attempt, args).await })
}

/// Runs one delivery attempt: read the signing actor, prepare the activity,
/// sign, `POST`, and classify the answer.
///
/// Failures are the job's, not the caller's: what cannot ever work (a
/// malformed payload, an actor that is not a local signer, a URL or key that
/// cannot be signed, a peer that refuses for good) is a
/// [`JobError::Discard`], what may work later (a database hiccup, a timeout,
/// a `5xx`, a throttle) a [`JobError::Retry`] or
/// [`JobError::RetryAfter`].
async fn deliver(state: AppState, attempt: AttemptInfo, args: Value) -> Result<(), JobError> {
    let args = serde_json::from_value::<DeliveryArgs>(args)
        .map_err(|err| JobError::Discard(format!("malformed delivery arguments: {err}")))?;
    debug!(
        activity_id = %args.activity_id,
        inbox = %args.inbox,
        attempts = attempt.attempts,
        "delivering activity"
    );
    let actor = signing_actor(&state, args.actor_id).await?;
    let private_pem = actor.private_key.as_deref().ok_or_else(|| {
        JobError::Discard(format!(
            "the local actor {} has no private key to sign with",
            actor.ap_id
        ))
    })?;
    let body = serde_json::to_vec(&strip_bto_bcc(&args.body))
        .map_err(|err| JobError::Discard(format!("the activity is not serializable: {err}")))?;
    let key_id = format!("{}{KEY_ID_SUFFIX}", actor.ap_id);
    let signed = sign_request(
        "POST",
        &args.inbox,
        Some(&body),
        &SigningKey {
            key_id: &key_id,
            private_pem,
        },
        Utc::now(),
    )
    .map_err(|err| {
        JobError::Discard(format!("cannot sign the delivery to {}: {err}", args.inbox))
    })?;
    let response = state
        .http_client
        .post_bytes(&args.inbox, &delivery_headers(&signed), body)
        .await
        .map_err(|err| transport_error(&err))?;
    delivery_outcome(response.status, &response.headers, attempt)
}

/// Parses an inbox into a deliverable URL: `http`/`https` with a host, and
/// nothing else. `None` is [`enqueue_delivery`]'s refusal.
fn parse_inbox(inbox: &str) -> Option<Url> {
    let target = Url::parse(inbox).ok()?;
    let deliverable = matches!(target.scheme(), "http" | "https") && target.host_str().is_some();
    deliverable.then_some(target)
}

/// Whether `inbox` is served by this very instance: its authority (host,
/// plus the port when it is not the scheme's default) is the authority of
/// the local signing actor's own id, which lives on this instance's origin
/// (`docs/ARCHITECTURE.md` decision 6). Delivering to it would mean asking
/// this process to `POST` an activity to itself.
fn is_local_inbox(inbox: &Url, actor_ap_id: &str) -> bool {
    let Ok(id) = Url::parse(actor_ap_id) else {
        return false;
    };
    id.authority().eq_ignore_ascii_case(inbox.authority())
}

/// The `ActivityPub` id an activity carries: the identity a delivery is for,
/// and the deduplication key's other half.
fn activity_id_of(activity: &Value) -> AppResult<&str> {
    activity
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::Unprocessable("the activity carries no string `id`".to_owned()))
}

/// Reads the actor to sign as, by id. A row that is gone, or that belongs to
/// another instance, can never sign anything for this one, so those are
/// permanent; a database that will not answer is transient.
async fn signing_actor(state: &AppState, actor_id: Uuid) -> Result<User, JobError> {
    let found = user::get_by_id(&state.db, actor_id)
        .await
        .map_err(|err| JobError::Retry(format!("could not read the signing actor: {err}")))?;
    found
        .filter(|actor| actor.local.get())
        .ok_or_else(|| JobError::Discard(format!("actor {actor_id} is not a local signer")))
}

/// The activity as it goes on the wire: [`bto`] and [`bcc`] removed, because
/// blind recipients are addressed by the sender's own followers list and
/// must never be leaked to the instances that receive the activity (the
/// `PublicAddressing` rule both Mastodon and Pleroma apply before sending).
/// Anything else — including `to` and `cc`, which the signature covers too —
/// is untouched.
///
/// [`bto`]: https://www.w3.org/TR/activitystreams-vocabulary/#dfn-bto
/// [`bcc`]: https://www.w3.org/TR/activitystreams-vocabulary/#dfn-bcc
fn strip_bto_bcc(activity: &Value) -> Value {
    let mut stripped = activity.clone();
    if let Some(object) = stripped.as_object_mut() {
        let _bto = object.remove("bto");
        let _bcc = object.remove("bcc");
    }
    stripped
}

/// The headers of a signed delivery: the activity media type in both
/// directions, then the four headers [`sign_request`] signed. `Host` is
/// deliberately not among them: the client fills it from the URL, whose
/// authority is exactly what was signed into `host`.
fn delivery_headers(signed: &SignedHeaders) -> Vec<(&str, &str)> {
    let mut headers = vec![
        ("content-type", ACTIVITY_JSON),
        ("accept", ACTIVITY_JSON),
        ("date", signed.date.as_str()),
    ];
    if let Some(digest) = signed.digest.as_deref() {
        headers.push(("digest", digest));
    }
    headers.push(("signature", signed.signature.as_str()));
    headers
}

/// Classifies a peer's answer into the job's outcome.
fn delivery_outcome(
    status: u16,
    headers: &HeaderMap,
    attempt: AttemptInfo,
) -> Result<(), JobError> {
    let detail = format!("peer answered {status}");
    if (200..=299).contains(&status) {
        return Ok(());
    }
    if is_permanent_refusal(status, attempt) {
        return Err(JobError::Discard(format!("{detail} for good")));
    }
    Err(retryable(&detail, headers))
}

/// Whether the peer's answer says this inbox can never take the activity: a
/// `4xx` that is not a transient one (`408` request timeout, `429` too many
/// requests), and not a first `401`/`403`, which is worth one retry because
/// the actor's key may have rotated since the peer last saw it. Everything
/// else — `3xx`, `408`, `429`, `5xx` — says only *not now*.
fn is_permanent_refusal(status: u16, attempt: AttemptInfo) -> bool {
    let refused = (400..=499).contains(&status) && !matches!(status, 408 | 429);
    let refreshing_key = matches!(status, 401 | 403) && attempt.attempts <= FIRST_ATTEMPT;
    refused && !refreshing_key
}

/// The retryable failure for `detail`: at the peer's own moment when it
/// asked for one, else on the queue's backoff.
fn retryable(detail: &str, headers: &HeaderMap) -> JobError {
    retry_after(headers).map_or_else(
        || JobError::Retry(detail.to_owned()),
        |delay| {
            JobError::RetryAfter(
                delay,
                format!("{detail}, the peer asked to retry in {}s", delay.as_secs()),
            )
        },
    )
}

/// The delay a peer asked for in `Retry-After`, in the delta-seconds spelling
/// only (the HTTP-date form is nowhere in fediverse delivery), capped at
/// [`MAX_RETRY_AFTER`]. `None` when the header is absent, unreadable or not a
/// whole number of seconds.
fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let seconds = headers
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())?
        .trim()
        .parse::<u64>()
        .ok()?;
    Some(Duration::from_secs(seconds.min(MAX_RETRY_AFTER.as_secs())))
}

/// The job outcome of a request that never turned into an answer.
///
/// A peer that is down, slow or gone mid-exchange may answer the next
/// attempt, so those failures retry. A failure the address itself causes —
/// an inbox that is not a deliverable URL, or one the SSRF guard refuses
/// because it resolves to a private or reserved address — cannot be fixed by
/// trying again, so it discards.
fn transport_error(err: &HttpError) -> JobError {
    match err {
        HttpError::InvalidUrl | HttpError::Blocked(_) => JobError::Discard(err.to_string()),
        // `Status` and `TooLarge` cannot come out of a delivery `POST`
        // (statuses are classified by `delivery_outcome` and the response
        // body is uncapped), so they are treated like the transport
        // hiccups they would be if they ever did.
        HttpError::Timeout | HttpError::Connect(_) | HttpError::Status(_) | HttpError::TooLarge => {
            JobError::Retry(err.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
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
    use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
    use serde_json::json;
    use sqlx::PgPool;
    use tokio::{net::TcpListener, task::JoinHandle};

    use super::*;
    use crate::{
        config::{Config, TEST_BASE_URL},
        federation::signature::{
            digest_header,
            verify::{SignedRequestParts, parse_signature_header, verify, verify_digest},
        },
        http_client::HttpClient,
        models::job::Job,
        models::user::{NewUser, insert, local_ap_id},
        uploads::UploaderBackend,
        util::keys::{FIXTURE_PRIVATE_PKCS1_PEM, FIXTURE_PUBLIC_PKCS1_PEM},
    };

    /// `User-Agent` of the test client (whose SSRF guard is off, so the
    /// loopback mocks are reachable).
    const TEST_UA: &str = "Wired (https://test.example)";
    /// The path every mock inbox serves.
    const INBOX_PATH: &str = "/inbox";
    /// The `keyId` a delivery from the fixture actor must carry.
    const ACTOR_KEY_ID: &str = "https://test.example/users/alice#main-key";
    /// The `headers` parameter a signed delivery POST must name.
    const SIGNED_LINES: &str = "(request-target) host date digest";

    /// The state under test: `pool`, the test config, and a client that may
    /// reach the loopback mocks.
    fn test_state(pool: &PgPool) -> AppState {
        let config = Arc::new(Config::for_tests());
        AppState {
            db: pool.clone(),
            hub: crate::streaming::hub::Hub::new(pool),
            config: Arc::clone(&config),
            http_client: HttpClient::new_for_tests(TEST_UA),
            uploader: UploaderBackend::from(config.as_ref()),
            instance_stats_cache: Arc::default(),
            mailer: Arc::new(crate::mailer::MemoryTransport::new()),
        }
    }

    /// The attempt a first delivery runs on, with this ticket's budget.
    const fn first_attempt() -> AttemptInfo {
        AttemptInfo {
            attempts: FIRST_ATTEMPT,
            max_attempts: MAX_ATTEMPTS,
        }
    }

    /// The attempt right after a retry, for the outcomes that change with it.
    const fn second_attempt() -> AttemptInfo {
        AttemptInfo {
            attempts: 2,
            max_attempts: MAX_ATTEMPTS,
        }
    }

    /// The fixture activity: an addressable `Create` carrying both the
    /// public recipients a peer may see and the blind ones it may not.
    fn activity() -> Value {
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
    async fn bind() -> TcpListener {
        TcpListener::bind("127.0.0.1:0").await.unwrap()
    }

    /// The `http://` origin a listener answers on.
    fn origin(listener: &TcpListener) -> String {
        format!("http://{}", listener.local_addr().unwrap())
    }

    /// Serves `app` on `listener` until the test aborts the task.
    fn spawn(listener: TcpListener, app: Router) -> JoinHandle<()> {
        tokio::spawn(async move {
            if let Err(err) = axum::serve(listener, app).await {
                tracing::debug!("test server stopped: {err}");
            }
        })
    }

    /// A mock inbox that authenticates the delivery it is handed, plus the
    /// shared record of what it saw and the task serving it.
    async fn verifying_inbox() -> (String, Seen, JoinHandle<()>) {
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
    async fn actor(pool: &PgPool) -> User {
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
    struct Seen {
        /// Requests the mock answered.
        hits: Arc<AtomicUsize>,
        /// The headers of the most recent request.
        headers: Arc<Mutex<Option<HeaderMap>>>,
        /// The body of the most recent request.
        body: Arc<Mutex<Option<Vec<u8>>>>,
    }

    /// A mock inbox that has seen nothing yet.
    fn seen() -> Seen {
        Seen::default()
    }

    /// The visible text of header `name`, empty when absent or unreadable.
    fn header_text(headers: &HeaderMap, name: &str) -> String {
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
    async fn status_only(State(code): State<StatusCode>) -> StatusCode {
        code
    }

    /// Answers `429`, asking for `Retry-After: {seconds}`.
    async fn throttled(State(seconds): State<u32>) -> Response {
        (
            StatusCode::TOO_MANY_REQUESTS,
            [(RETRY_AFTER, seconds.to_string())],
        )
            .into_response()
    }

    /// Runs one [`handle`] attempt as the actor `actor_id`, delivering to
    /// `inbox`.
    async fn attempt_to(
        state: &AppState,
        inbox: &str,
        actor_id: Uuid,
        attempt: AttemptInfo,
    ) -> Result<(), JobError> {
        handle(state.clone(), attempt, delivery_args(inbox, actor_id)).await
    }

    /// How a run of [`handle`] reads in a test message: the handler's own
    /// `Display`, since a `Result` has none.
    fn outcome_text(outcome: &Result<(), JobError>) -> String {
        match outcome {
            Ok(()) => "completed".to_owned(),
            Err(err) => err.to_string(),
        }
    }

    /// The named parameter of a `Signature` header value, unquoted.
    fn signature_param(header: &str, name: &str) -> String {
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
    fn retry_after_headers(seconds: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        let _set: Option<HeaderValue> =
            headers.insert(RETRY_AFTER, HeaderValue::from_str(seconds).unwrap());
        headers
    }

    /// Inserts one instance-policy row.
    async fn policy(pool: &PgPool, domain: &str, action: &str) {
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
    async fn queued_job(pool: &PgPool) -> Job {
        sqlx::query_as::<_, Job>("SELECT * FROM jobs WHERE queue = $1 ORDER BY id LIMIT 1")
            .bind(QUEUE_FEDERATOR_OUTGOING)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// How many jobs the database holds.
    async fn job_count(pool: &PgPool) -> i64 {
        let (count,): (i64,) = sqlx::query_as("SELECT count(*) FROM jobs")
            .fetch_one(pool)
            .await
            .unwrap();
        count
    }

    /// Inserts a remote mirror named `nickname`, carrying the given inbox
    /// endpoints, for the tests that need an actor this instance does not own.
    async fn mirror(
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

    #[tokio::test]
    async fn best_inbox_prefers_the_shared_inbox() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let shared = mirror(
            &pool,
            "shared",
            Some("https://peer.example/users/bob/inbox"),
            Some("https://peer.example/inbox"),
        )
        .await;
        let personal = mirror(
            &pool,
            "personal",
            Some("https://peer.example/users/bob/inbox"),
            None,
        )
        .await;
        let nowhere = mirror(&pool, "nowhere", None, None).await;

        assert_eq!(
            best_inbox(&shared).as_deref(),
            Some("https://peer.example/inbox"),
            "one POST per instance when the actor publishes a shared inbox"
        );
        assert_eq!(
            best_inbox(&personal).as_deref(),
            Some("https://peer.example/users/bob/inbox"),
            "the personal inbox when there is no shared one"
        );
        assert_eq!(best_inbox(&nowhere), None, "and nowhere to send at all");
    }

    #[test]
    fn blind_recipients_are_stripped_and_nothing_else() {
        let stripped = strip_bto_bcc(&activity());
        assert!(
            stripped.get("bto").is_none() && stripped.get("bcc").is_none(),
            "the blind recipient lists are gone: {stripped}"
        );
        assert_eq!(
            stripped["to"],
            json!(["https://www.w3.org/ns/activitystreams#Public"]),
            "the public addressees stay"
        );
        assert_eq!(
            stripped["cc"],
            json!(["https://test.example/users/alice/followers"]),
            "and so do the followers"
        );
        assert_eq!(
            stripped["id"],
            activity()["id"],
            "the document is otherwise untouched"
        );
        let not_an_object = json!("a string is not an object");
        assert_eq!(
            strip_bto_bcc(&not_an_object),
            not_an_object,
            "a body that is not an object passes through"
        );
    }

    #[test]
    fn the_headers_label_the_activity_and_carry_the_signature() {
        let signed = SignedHeaders {
            date: "Tue, 07 Jun 2022 20:51:35 GMT".to_owned(),
            host: "peer.example".to_owned(),
            digest: Some("SHA-256=abc".to_owned()),
            signature: r#"keyId="k",signature="s""#.to_owned(),
        };
        let headers = delivery_headers(&signed);
        assert_eq!(
            headers,
            vec![
                ("content-type", ACTIVITY_JSON),
                ("accept", ACTIVITY_JSON),
                ("date", "Tue, 07 Jun 2022 20:51:35 GMT"),
                ("digest", "SHA-256=abc"),
                ("signature", r#"keyId="k",signature="s""#),
            ],
            "both media types and the signed values, and no Host"
        );
        let bodyless = SignedHeaders {
            digest: None,
            ..signed
        };
        assert!(
            !delivery_headers(&bodyless)
                .iter()
                .any(|(name, _)| *name == "digest"),
            "a signature over no digest sends no Digest header"
        );
    }

    #[test]
    fn answers_are_classified_by_whether_retrying_can_help() {
        let none = HeaderMap::new();
        for status in [200_u16, 201, 204, 299] {
            assert_eq!(
                delivery_outcome(status, &none, first_attempt()),
                Ok(()),
                "a {status} answer completes the delivery"
            );
        }
        for status in [400_u16, 404, 405, 410, 413, 422, 451] {
            assert!(
                matches!(
                    delivery_outcome(status, &none, first_attempt()),
                    Err(JobError::Discard(_))
                ),
                "a permanent {status} refusal discards"
            );
        }
        for status in [401_u16, 403] {
            assert_eq!(
                delivery_outcome(status, &none, first_attempt()),
                Err(JobError::Retry(format!("peer answered {status}"))),
                "a first {status} is retried for the key refresh"
            );
            assert!(
                matches!(
                    delivery_outcome(status, &none, second_attempt()),
                    Err(JobError::Discard(_))
                ),
                "the same {status} again is a real refusal"
            );
        }
        for status in [302_u16, 408, 429, 500, 502, 503] {
            assert!(
                matches!(
                    delivery_outcome(status, &none, second_attempt()),
                    Err(JobError::Retry(_))
                ),
                "a {status} answer only says not now"
            );
        }
    }

    #[test]
    fn the_peer_retry_after_is_honoured_and_capped() {
        assert_eq!(
            retry_after(&retry_after_headers("7")),
            Some(Duration::from_secs(7)),
            "seven delta-seconds"
        );
        assert_eq!(
            retry_after(&retry_after_headers(" 999999 ")),
            Some(MAX_RETRY_AFTER),
            "a request past six hours is capped at six"
        );
        for value in ["", "soon", "-3", "1.5"] {
            assert_eq!(
                retry_after(&retry_after_headers(value)),
                None,
                "`{value}` is no usable delay"
            );
        }
        assert_eq!(
            retry_after(&HeaderMap::new()),
            None,
            "absent means nothing was asked"
        );
        assert_eq!(
            retryable("peer answered 503", &retry_after_headers("120")),
            JobError::RetryAfter(
                Duration::from_secs(120),
                "peer answered 503, the peer asked to retry in 120s".to_owned()
            ),
            "a delayed answer becomes the peer's own schedule"
        );
        assert_eq!(
            retryable("peer answered 503", &HeaderMap::new()),
            JobError::Retry("peer answered 503".to_owned()),
            "without the header the queue's backoff decides"
        );
    }

    #[test]
    fn unreachable_inboxes_are_told_apart_from_undeliverable_ones() {
        for err in [
            HttpError::Timeout,
            HttpError::Connect("refused".to_owned()),
            HttpError::Status(503),
            HttpError::TooLarge,
        ] {
            assert_eq!(
                transport_error(&err),
                JobError::Retry(err.to_string()),
                "a peer that did not answer may answer next time: {err}"
            );
        }
        for err in [
            HttpError::InvalidUrl,
            HttpError::Blocked("private address".to_owned()),
        ] {
            assert_eq!(
                transport_error(&err),
                JobError::Discard(err.to_string()),
                "an inbox this client can never reach is refused for good: {err}"
            );
        }
    }

    #[test]
    fn only_http_inboxes_are_deliverable() {
        for inbox in [
            "ftp://peer.example/inbox",
            "file:///etc/passwd",
            "not a url",
            "",
        ] {
            assert!(
                parse_inbox(inbox).is_none(),
                "`{inbox}` is not an http(s) inbox"
            );
        }
        for inbox in ["http://peer.example/inbox", "https://peer.example/inbox"] {
            assert!(
                parse_inbox(inbox).is_some(),
                "`{inbox}` is an http(s) inbox"
            );
        }
    }

    #[test]
    fn an_inbox_on_the_actors_own_origin_is_local() {
        let alice = local_ap_id(TEST_BASE_URL, "alice");
        let own = Url::parse("https://test.example/users/bob/inbox").unwrap();
        let lookalike = Url::parse("https://test.example.evil.example/inbox").unwrap();
        let other = Url::parse("https://peer.example/inbox").unwrap();
        let ported = Url::parse("https://test.example:443/inbox").unwrap();
        assert!(is_local_inbox(&own, &alice), "this instance's own origin");
        assert!(
            is_local_inbox(&ported, &alice),
            "the default port spelled out is the same authority"
        );
        assert!(
            !is_local_inbox(&lookalike, &alice),
            "a look-alike prefix host is not this instance"
        );
        assert!(!is_local_inbox(&other, &alice), "another host is not local");
        assert!(
            !is_local_inbox(&own, "not an actor id"),
            "an actor id that is no URL vouches for nothing"
        );
    }

    #[tokio::test]
    async fn a_signed_delivery_verifies_at_the_peer_and_completes() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = actor(&pool).await;
        let state = test_state(&pool);
        let (inbox, observable, server) = verifying_inbox().await;

        let outcome = attempt_to(&state, &inbox, alice.id, first_attempt()).await;
        server.abort();

        assert_eq!(
            outcome,
            Ok(()),
            "a delivery the peer authenticates completes"
        );
        assert_eq!(
            observable.hits.load(Ordering::SeqCst),
            1,
            "the peer was asked exactly once"
        );
        let headers = observable.headers.lock().unwrap().clone().unwrap();
        let signature = header_text(&headers, "signature");
        assert_eq!(
            signature_param(&signature, "keyId"),
            ACTOR_KEY_ID,
            "the signature names the actor's own key"
        );
        assert_eq!(
            signature_param(&signature, "headers"),
            SIGNED_LINES,
            "a delivery signs the four canonical lines"
        );
        let raw = BASE64
            .decode(signature_param(&signature, "signature"))
            .unwrap();
        assert_eq!(raw.len(), 128, "an RSA-1024 signature is 128 bytes");
    }

    #[tokio::test]
    async fn blind_recipients_never_reach_the_peer() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = actor(&pool).await;
        let state = test_state(&pool);
        let (inbox, observable, server) = verifying_inbox().await;

        attempt_to(&state, &inbox, alice.id, first_attempt())
            .await
            .unwrap();
        server.abort();

        let body = observable.body.lock().unwrap().clone().unwrap();
        let sent: Value = serde_json::from_slice(&body).unwrap();
        assert!(
            sent.get("bto").is_none() && sent.get("bcc").is_none(),
            "the blind recipient lists were stripped: {sent}"
        );
        assert!(sent.get("to").is_some(), "the public addressees ride along");
        let headers = observable.headers.lock().unwrap().clone().unwrap();
        assert_eq!(
            header_text(&headers, "digest"),
            digest_header(&body),
            "the Digest header describes exactly the bytes that were sent"
        );
    }

    #[tokio::test]
    async fn the_delivery_carries_every_inbox_header() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = actor(&pool).await;
        let state = test_state(&pool);
        let (inbox, observable, server) = verifying_inbox().await;

        attempt_to(&state, &inbox, alice.id, first_attempt())
            .await
            .unwrap();
        server.abort();

        let headers = observable.headers.lock().unwrap().clone().unwrap();
        assert_eq!(
            header_text(&headers, "content-type"),
            ACTIVITY_JSON,
            "the body is labelled an activity"
        );
        assert_eq!(
            header_text(&headers, "accept"),
            ACTIVITY_JSON,
            "and the answer is asked for in the same media type"
        );
        assert!(
            header_text(&headers, "date").ends_with(" GMT"),
            "a signed Date header"
        );
        assert!(
            header_text(&headers, "digest").starts_with("SHA-256="),
            "a SHA-256 Digest header"
        );
        assert!(
            !header_text(&headers, "signature").is_empty(),
            "a Signature header"
        );
        assert!(
            !header_text(&headers, "host").is_empty(),
            "the client fills Host, which is why it is not sent by hand"
        );
    }

    #[tokio::test]
    async fn a_gone_inbox_discards_the_delivery() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = actor(&pool).await;
        let state = test_state(&pool);
        let listener = bind().await;
        let inbox = format!("{}{INBOX_PATH}", origin(&listener));
        let server = spawn(
            listener,
            Router::new()
                .route(INBOX_PATH, post(status_only))
                .with_state(StatusCode::GONE),
        );

        let outcome = attempt_to(&state, &inbox, alice.id, first_attempt()).await;
        server.abort();

        match outcome {
            Err(JobError::Discard(detail)) => {
                assert!(detail.contains("410"), "the discard names it: {detail}");
            }
            other => panic!(
                "a 410 inbox must discard the delivery, got {}",
                outcome_text(&other)
            ),
        }
    }

    #[tokio::test]
    async fn a_server_error_retries_the_delivery() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = actor(&pool).await;
        let state = test_state(&pool);
        let listener = bind().await;
        let inbox = format!("{}{INBOX_PATH}", origin(&listener));
        let server = spawn(
            listener,
            Router::new()
                .route(INBOX_PATH, post(status_only))
                .with_state(StatusCode::INTERNAL_SERVER_ERROR),
        );

        let outcome = attempt_to(&state, &inbox, alice.id, first_attempt()).await;
        server.abort();

        match outcome {
            Err(JobError::Retry(detail)) => {
                assert!(detail.contains("500"), "the retry names it: {detail}");
            }
            other => panic!(
                "a 500 inbox must retry the delivery, got {}",
                outcome_text(&other)
            ),
        }
    }

    #[tokio::test]
    async fn a_first_unauthorized_answer_retries_and_a_second_discards() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = actor(&pool).await;
        let state = test_state(&pool);
        let listener = bind().await;
        let inbox = format!("{}{INBOX_PATH}", origin(&listener));
        let server = spawn(
            listener,
            Router::new()
                .route(INBOX_PATH, post(status_only))
                .with_state(StatusCode::UNAUTHORIZED),
        );

        let first = attempt_to(&state, &inbox, alice.id, first_attempt()).await;
        let second = attempt_to(&state, &inbox, alice.id, second_attempt()).await;
        server.abort();

        assert!(
            matches!(first, Err(JobError::Retry(_))),
            "a 401 the first time may be a key that just rotated, got {}",
            outcome_text(&first)
        );
        assert!(
            matches!(second, Err(JobError::Discard(_))),
            "the same 401 twice is a real refusal, got {}",
            outcome_text(&second)
        );
    }

    #[tokio::test]
    async fn a_throttled_inbox_sets_the_retry_delay() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = actor(&pool).await;
        let state = test_state(&pool);
        let listener = bind().await;
        let inbox = format!("{}{INBOX_PATH}", origin(&listener));
        let server = spawn(
            listener,
            Router::new()
                .route(INBOX_PATH, post(throttled))
                .with_state(7),
        );

        let outcome = attempt_to(&state, &inbox, alice.id, first_attempt()).await;
        server.abort();

        assert_eq!(
            outcome,
            Err(JobError::RetryAfter(
                Duration::from_secs(7),
                "peer answered 429, the peer asked to retry in 7s".to_owned()
            )),
            "the job waits out the peer's own seven seconds"
        );
    }

    #[tokio::test]
    async fn an_unreachable_inbox_retries() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = actor(&pool).await;
        let state = test_state(&pool);
        let listener = bind().await;
        let inbox = format!("{}{INBOX_PATH}", origin(&listener));
        drop(listener);

        let outcome = attempt_to(&state, &inbox, alice.id, first_attempt()).await;

        match outcome {
            Err(JobError::Retry(detail)) => assert!(
                !detail.is_empty(),
                "the retry states the transport failure: {detail}"
            ),
            other => panic!("a dead inbox must retry, got {}", outcome_text(&other)),
        }
    }

    #[tokio::test]
    async fn a_delivery_that_can_never_be_signed_is_discarded() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = actor(&pool).await;
        let state = test_state(&pool);

        let junk_args = handle(state.clone(), first_attempt(), json!({"inbox": "x"})).await;
        assert!(
            matches!(junk_args, Err(JobError::Discard(_))),
            "arguments that do not describe a delivery discard, got {}",
            outcome_text(&junk_args)
        );

        let bad_inbox = attempt_to(&state, "not a url", alice.id, first_attempt()).await;
        assert!(
            matches!(bad_inbox, Err(JobError::Discard(_))),
            "an inbox that cannot be signed discards, got {}",
            outcome_text(&bad_inbox)
        );
    }

    #[tokio::test]
    async fn an_actor_that_cannot_sign_is_discarded() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let remote = mirror(&pool, "bob", None, None).await;
        let keyless = insert(
            &pool,
            &NewUser::local("carol", &local_ap_id(TEST_BASE_URL, "carol")),
        )
        .await
        .unwrap();
        let state = test_state(&pool);
        let inbox = "https://peer.example/inbox";

        for (what, actor_id) in [
            ("a mirror of a remote actor", remote.id),
            ("a local actor with no private key", keyless.id),
            ("a row that is gone", Uuid::now_v7()),
        ] {
            let outcome = attempt_to(&state, inbox, actor_id, first_attempt()).await;
            assert!(
                matches!(outcome, Err(JobError::Discard(_))),
                "{what} can never sign a delivery, got {}",
                outcome_text(&outcome)
            );
        }
    }

    #[tokio::test]
    async fn a_local_inbox_is_refused_without_a_job() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = actor(&pool).await;

        enqueue_delivery(
            &pool,
            "https://test.example/users/bob/inbox",
            &alice,
            &activity(),
        )
        .await
        .unwrap();

        assert_eq!(
            job_count(&pool).await,
            0,
            "delivering into this instance's own inbox is skipped"
        );
    }

    #[tokio::test]
    async fn a_rejected_domain_is_refused_without_a_job() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        policy(&pool, "spam.example", "reject").await;
        policy(&pool, "quiet.example", "silence").await;
        let alice = actor(&pool).await;

        for (inbox, expected) in [
            ("https://spam.example/inbox", 0),
            ("https://quiet.example/inbox", 1),
        ] {
            enqueue_delivery(&pool, inbox, &alice, &activity())
                .await
                .unwrap();
            assert_eq!(
                job_count(&pool).await,
                expected,
                "`{inbox}` leaves {expected} job(s) behind: a rejected host is \
                 refused, a silenced one is still delivered to"
            );
        }
    }

    #[tokio::test]
    async fn a_non_http_inbox_is_refused_without_a_job() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = actor(&pool).await;
        for inbox in ["ftp://peer.example/inbox", "not a url", "https://", ""] {
            enqueue_delivery(&pool, inbox, &alice, &activity())
                .await
                .unwrap_or_else(|err| panic!("`{inbox}` refuses without an error: {err}"));
        }

        assert_eq!(job_count(&pool).await, 0, "nothing was queued");
    }

    #[tokio::test]
    async fn the_unique_key_queues_one_delivery_per_pair() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = actor(&pool).await;
        let inbox = "https://peer.example/inbox";

        enqueue_delivery(&pool, inbox, &alice, &activity())
            .await
            .unwrap();
        enqueue_delivery(&pool, inbox, &alice, &activity())
            .await
            .unwrap();

        let job = queued_job(&pool).await;
        assert_eq!(job_count(&pool).await, 1, "the second call queued nothing");
        assert_eq!(
            job.unique_key.as_deref(),
            Some("deliver:https://test.example/activities/1:https://peer.example/inbox"),
            "the dedup key names the activity and the inbox"
        );
        assert_eq!(job.kind, KIND_DELIVER, "the delivery kind");
        assert_eq!(job.queue, QUEUE_FEDERATOR_OUTGOING, "the outgoing queue");
        assert_eq!(job.max_attempts, MAX_ATTEMPTS, "seven attempts");
        assert_eq!(job.attempts, 0, "nothing has been tried yet");
        assert_eq!(job.args["inbox"], inbox, "the inbox argument");
        assert_eq!(
            job.args["actor_id"],
            alice.id.to_string(),
            "the signing actor"
        );
        assert_eq!(job.args["body"], activity(), "the activity rides along");
        assert_eq!(
            job.args["activity_id"],
            activity()["id"],
            "and its id is spelled out"
        );

        enqueue_delivery(&pool, "https://other.example/inbox", &alice, &activity())
            .await
            .unwrap();
        assert_eq!(job_count(&pool).await, 2, "a second inbox is its own job");
    }

    #[tokio::test]
    async fn an_activity_without_an_id_is_an_error() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = actor(&pool).await;
        let keyless = json!({"type": "Create"});

        let err = enqueue_delivery(&pool, "https://peer.example/inbox", &alice, &keyless)
            .await
            .expect_err("an unnamed activity cannot be deduplicated");

        assert!(
            matches!(err, AppError::Unprocessable(_)),
            "a producer bug is a 422, got {err}"
        );
        assert_eq!(job_count(&pool).await, 0, "and nothing was queued");
    }
}
