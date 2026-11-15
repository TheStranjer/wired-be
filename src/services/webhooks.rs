//! Signed event deliveries: the webhook fan-out and its delivery job.
//!
//! `docs/ARCHITECTURE.md` decision 11 keeps the work in the `jobs` table:
//! [`emit`] turns one event into one `webhooks.deliver` job per subscribed
//! webhook, and [`handle_deliver`] (job kind [`KIND_DELIVER`], queue
//! [`QUEUE_DEFAULT`], [`MAX_ATTEMPTS`] tries)
//! performs the HTTP `POST`. This is Pleroma's `Pleroma.Webhook.Notify` +
//! `Pleroma.Workers.WebhookWorker` pair
//! (`lib/pleroma/webhook/notify.ex`, `lib/pleroma/workers/webhook_worker.ex`)
//! with two changes the ticket pins: the event's own payload travels in the
//! job's arguments rather than being re-read from the database at attempt
//! time, and the envelope Mastodon's webhooks speak replaces Pleroma's
//! `{event, created_at, object}` — `{"id", "type", "created_at", "data"}`,
//! signed over `"{timestamp}.{body}"` and announced by three `X-Wired-*`
//! headers instead of Pleroma's single `X-Hub-Signature`.
//!
//! [`emit`] filters as Pleroma's `Webhook.get_by_type` does — an
//! [`enabled`](crate::models::webhook::Webhook::enabled) row whose
//! [`events`](crate::models::webhook::Webhook::events) array names the event
//! — and keys each job `webhook:{webhook id}:{event id}`, so one event never
//! queues the same webhook twice while a delivery is still live. The three
//! hooks the ticket names are the thin payload builders beside it:
//! [`report_created`] and [`report_updated`] send the filed report as
//! `{"report": …}`, [`account_created`] the registered account as
//! `{"account": …}`. A payload that cannot be built (a report whose target
//! account is already gone) is logged and skipped: an event nobody can
//! describe is not worth failing the action that produced it.
//!
//! The attempt itself re-reads the webhook, because the row may have changed
//! between enqueue and delivery: gone or disabled means the delivery is off,
//! and the job completes on nothing (Pleroma's `{:cancel, :not_found}` /
//! `{:cancel, :disabled}`). Otherwise the body is HMAC-SHA256'd with the
//! webhook's secret and `POST`ed through the SSRF-guarded
//! [`HttpClient`](crate::http_client::HttpClient), so a webhook can point at
//! any public endpoint and no private one. The answer decides the job: `2xx`
//! completed, a `4xx` other than `408`/`429` discarded (the endpoint refused
//! the event and another try says the same), a `5xx`, a throttle, a timeout
//! or a lost connection retried on the queue's backoff.

use chrono::Utc;
use hmac::{Hmac, KeyInit as _, Mac as _};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2_011::Sha256;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::{
    entities::{admin_account::AdminAccount, report::Report},
    error::{AppError, AppResult},
    http_client::{HttpError, user_agent},
    jobs::{AttemptInfo, HandlerFuture, JobError, QUEUE_DEFAULT},
    models::{
        job::{NewJob, enqueue},
        report::{self, Report as ReportRow},
        user::{self, User},
        webhook::{self, Webhook},
    },
    state::AppState,
    util::{id::new_id, time::mastodon_time, token::to_hex},
};

/// The HMAC flavour a delivery signature uses: HMAC-SHA256 over
/// `"{timestamp}.{body}"`, as Mastodon's webhook signing spells it.
type HmacSha256 = Hmac<Sha256>;

/// The job kind of one webhook delivery, handled by [`handle_deliver`] on
/// [`QUEUE_DEFAULT`].
pub const KIND_DELIVER: &str = "webhooks.deliver";
/// The attempt budget [`emit`] queues every delivery with.
///
/// Five tries on the queue's backoff: an endpoint that is briefly down or
/// slow gets about twelve minutes of patience (15 s doubled per attempt)
/// before the delivery is spent.
pub const MAX_ATTEMPTS: i32 = 5;

/// The event [`report_created`] emits: a report has been filed.
pub const EVENT_REPORT_CREATED: &str = "report.created";
/// The event [`report_updated`] emits: moderation moved a report.
pub const EVENT_REPORT_UPDATED: &str = "report.updated";
/// The event [`account_created`] emits: a local account was registered.
pub const EVENT_ACCOUNT_CREATED: &str = "account.created";

/// Prefix of the `X-Wired-Signature` value, which carries the algorithm
/// beside the digest (Mastodon's `sha256=<hex>` spelling).
const SIGNATURE_PREFIX: &str = "sha256=";
/// The lower bound of the success range a delivery is complete on.
const SUCCESS_STATUS: u16 = 200;
/// The first status above the success range.
const STATUS_CEILING: u16 = 300;
/// The first client-error status.
const CLIENT_ERROR: u16 = 400;
/// The first server-error status, and the end of the discard range: a `4xx`
/// is the endpoint refusing the event, and only the two codes below mean
/// anything else.
const SERVER_ERROR: u16 = 500;
/// Request timeout — the endpoint is not ready yet, so try again.
const STATUS_REQUEST_TIMEOUT: u16 = 408;
/// Too Many Requests — the endpoint is throttling us, so try again.
const STATUS_TOO_MANY: u16 = 429;

/// The `{webhook_id, event, event_id, data}` arguments of a delivery job, as
/// [`job_args`] writes and [`handle_deliver`] reads them. The payload rides
/// with the job rather than being re-derived at attempt time, so a delivery
/// says what the event said.
#[derive(Debug, Clone, Deserialize)]
struct DeliverArgs {
    /// The webhook to deliver to.
    webhook_id: Uuid,
    /// The event type, also sent as `X-Wired-Event`.
    event: String,
    /// The event's own id: the envelope's `id` and the delivery's dedup key.
    event_id: Uuid,
    /// The event's payload, the envelope's `data`.
    data: Value,
}

/// Queues one `webhooks.deliver` job per enabled webhook subscribed to
/// `event`, carrying `data` as the event's payload.
///
/// All the jobs of one call share `event_id` — a fresh
/// [`new_id`] (decision 4) — so a receiver sees one event with one id however
/// many endpoints it fans out to, while each job's own `unique_key` keeps a
/// webhook from holding two live deliveries of the same event. Nothing
/// subscribed is nothing queued, and a failure to read the subscriptions or
/// insert a job is logged, not returned: the action that fired the event has
/// already happened, and a webhook is not a reason to undo it.
pub async fn emit(state: &AppState, event: &str, data: Value) {
    let event_id = new_id();
    let hooks = match webhook::enabled_for_event(&state.db, event).await {
        Ok(hooks) => hooks,
        Err(err) => {
            warn!(%err, event, "webhook fan-out could not read its subscriptions");
            return;
        }
    };
    for hook in hooks {
        queue_delivery(state, &hook, event, event_id, &data).await;
    }
}

/// Enqueues the one delivery of `event` to `hook`; see [`emit`]. Every
/// outcome is logged and none of them travels back to the caller.
async fn queue_delivery(
    state: &AppState,
    hook: &Webhook,
    event: &str,
    event_id: Uuid,
    data: &Value,
) {
    let key = unique_key(hook.id, event_id);
    let queued = enqueue(
        &state.db,
        NewJob {
            queue: QUEUE_DEFAULT,
            kind: KIND_DELIVER,
            args: job_args(hook.id, event, event_id, data),
            run_at: None,
            max_attempts: Some(MAX_ATTEMPTS),
            unique_key: Some(key.as_str()),
        },
    )
    .await;
    match queued {
        Ok(Some(job)) => {
            debug!(
                job = %job.id,
                webhook = %hook.id,
                event,
                "webhook delivery queued"
            );
        }
        Ok(None) => {
            debug!(webhook = %hook.id, event, "this event is already queued for the webhook");
        }
        Err(err) => {
            warn!(%err, webhook = %hook.id, event, "webhook delivery could not be queued");
        }
    }
}

/// The deduplication key of one delivery: `webhook:{webhook id}:{event id}`.
fn unique_key(webhook_id: Uuid, event_id: Uuid) -> String {
    format!("webhook:{webhook_id}:{event_id}")
}

/// The job arguments [`handle_deliver`] reads: the pair of ids, the event
/// type it names and the payload it carries.
fn job_args(webhook_id: Uuid, event: &str, event_id: Uuid, data: &Value) -> Value {
    json!({
        "webhook_id": webhook_id.to_string(),
        "event": event,
        "event_id": event_id.to_string(),
        "data": data,
    })
}

/// Reads [`DeliverArgs`] out of a claimed job's payload; arguments this shape
/// cannot deserialize are permanent — the job is discarded.
fn parse_args(args: &Value) -> Result<DeliverArgs, JobError> {
    serde_json::from_value::<DeliverArgs>(args.clone())
        .map_err(|err| JobError::Discard(format!("webhook job arguments are unusable: {err}")))
}

/// The [`JobRegistry`](crate::jobs::JobRegistry) handler for [`KIND_DELIVER`].
pub fn handle_deliver(state: AppState, _attempt: AttemptInfo, args: Value) -> HandlerFuture {
    Box::pin(async move { deliver(&state, &parse_args(&args)?).await })
}

/// One attempt: re-read the webhook, then deliver to it. A webhook the queue
/// still holds a job for but the database no longer has — or that has been
/// disabled since — is not delivered to; the job completes, because the row's
/// owner has said so.
///
/// # Errors
///
/// [`JobError::Retry`] on a database failure, and whatever [`post_event`]
/// reports for the HTTP answer.
async fn deliver(state: &AppState, args: &DeliverArgs) -> Result<(), JobError> {
    let found = webhook::get(&state.db, args.webhook_id)
        .await
        .map_err(|err| retry(&err))?;
    let Some(hook) = found else {
        debug!(webhook = %args.webhook_id, "the webhook is gone; nothing to deliver to");
        return Ok(());
    };
    if !hook.enabled {
        debug!(webhook = %hook.id, "the webhook is disabled; the delivery is off");
        return Ok(());
    }
    post_event(state, &hook, args).await
}

/// Lifts a database failure into the queue's retry arm; every read the job
/// does is idempotent, so another attempt costs nothing but the wait.
fn retry(err: &AppError) -> JobError {
    JobError::Retry(format!("webhook delivery failed: {err}"))
}

/// The signed `POST` of one event and the verdict its answer gives.
///
/// # Errors
///
/// [`JobError::Discard`] when the secret cannot sign or the target is one
/// this instance will never reach (an unparsable URL, or a refused private
/// address — the SSRF guard's answer cannot change between attempts),
/// [`JobError::Retry`] for a transport failure or anything [`classify`]
/// finds retryable.
async fn post_event(state: &AppState, hook: &Webhook, args: &DeliverArgs) -> Result<(), JobError> {
    let now = Utc::now();
    let timestamp = now.timestamp();
    let body = event_body(args, &mastodon_time(&now));
    let signature = signature(&hook.secret, timestamp, &body).ok_or_else(|| {
        JobError::Discard(format!(
            "webhook {} carries a secret that cannot sign",
            hook.id
        ))
    })?;
    let agent = user_agent(
        &state.config.base_url,
        state.config.contact_email.as_deref(),
    );
    let stamp = timestamp.to_string();
    let headers = [
        ("content-type", "application/json"),
        ("user-agent", agent.as_str()),
        ("x-wired-event", args.event.as_str()),
        ("x-wired-timestamp", stamp.as_str()),
        ("x-wired-signature", signature.as_str()),
    ];
    let answer = state
        .http_client
        .post_bytes(&hook.url, &headers, body.into_bytes())
        .await;
    match answer {
        Ok(response) => {
            debug!(
                webhook = %hook.id,
                event = %args.event,
                status = response.status,
                "webhook delivered"
            );
            classify(response.status)
        }
        Err(err) => Err(http_failure(&err)),
    }
}

/// The event envelope the ticket pins: the event's id and type, the moment of
/// the attempt in decision 5's format (an RFC 3339 instant), and the payload
/// as it was queued. The returned text is what both the signature covers and
/// the body bytes are made of, so a receiver can recompute the digest from
/// exactly what it received.
fn event_body(args: &DeliverArgs, created_at: &str) -> String {
    json!({
        "id": args.event_id.to_string(),
        "type": args.event,
        "created_at": created_at,
        "data": args.data,
    })
    .to_string()
}

/// The `X-Wired-Signature` value: `sha256=<hex>` of the HMAC-SHA256 of
/// `"{timestamp}.{body}"` keyed by `secret` (Pleroma signs the bare body with
/// its `X-Hub-Signature`; the ticket pins the Mastodon timestamped form).
fn signature(secret: &str, timestamp: i64, body: &str) -> Option<String> {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).ok()?;
    mac.update(format!("{timestamp}.{body}").as_bytes());
    let digest = mac.finalize().into_bytes();
    Some(format!("{SIGNATURE_PREFIX}{}", to_hex(&digest)))
}

/// Sorts one endpoint's answer into completed, discarded or retried: `2xx`
/// landed, a `4xx` that is neither `408` nor `429` is a refusal no retry
/// changes, and everything else — `3xx`, `5xx`, a throttle, a timeout — the
/// queue's to try again.
fn classify(status: u16) -> Result<(), JobError> {
    if (SUCCESS_STATUS..STATUS_CEILING).contains(&status) {
        return Ok(());
    }
    if matches!(status, STATUS_REQUEST_TIMEOUT | STATUS_TOO_MANY) {
        return Err(JobError::Retry(format!(
            "the webhook endpoint asked to wait ({status})"
        )));
    }
    if (CLIENT_ERROR..SERVER_ERROR).contains(&status) {
        return Err(JobError::Discard(format!(
            "the webhook endpoint refused the delivery ({status})"
        )));
    }
    Err(JobError::Retry(format!(
        "the webhook endpoint answered {status}"
    )))
}

/// A transport failure in job terms: a target this instance will never reach
/// discards, anything else (timeout, closed connection, oversized answer, an
/// answer the caller did not ask to treat as an error) is retried.
fn http_failure(err: &HttpError) -> JobError {
    let detail = err.to_string();
    match err {
        HttpError::InvalidUrl | HttpError::Blocked(_) => JobError::Discard(detail),
        HttpError::Timeout | HttpError::TooLarge | HttpError::Connect(_) | HttpError::Status(_) => {
            JobError::Retry(detail)
        }
    }
}

/// The `report.created` hook: the filed `report` as `{"report": …}`.
///
/// The payload is the Mastodon [`Report`] render — this codebase has no admin
/// report entity yet, so the one report shape there is goes out — with the
/// target account seen as an anonymous reader sees it: the event says what was
/// filed, not what any particular viewer may read.
pub async fn report_created(state: &AppState, report: &ReportRow) {
    emit_report_event(state, EVENT_REPORT_CREATED, report).await;
}

/// The `report.updated` hook: the report moderation moved, in the same
/// envelope as [`report_created`].
pub async fn report_updated(state: &AppState, report: &ReportRow) {
    emit_report_event(state, EVENT_REPORT_UPDATED, report).await;
}

/// The two report events' shared half: build the payload, then [`emit`]. A
/// report that cannot be rendered is logged and skipped — the filing or the
/// state change it would have announced has already been committed.
async fn emit_report_event(state: &AppState, event: &str, row: &ReportRow) {
    match report_payload(state, row).await {
        Ok(data) => emit(state, event, data).await,
        Err(err) => {
            warn!(%err, report = %row.id, event, "the webhook event could not be rendered");
        }
    }
}

/// `{"report": …}`: the row as the [`Report`] entity, with the cited statuses
/// read back off `report_statuses` and the target account behind
/// [`target_id`](ReportRow::target_id).
///
/// # Errors
///
/// [`AppError::NotFound`] when the reported account is already gone (an
/// deleting cascade got there first, so there is nothing to describe),
/// [`AppError::Database`] from either read and [`AppError::Internal`] for an
/// entity that somehow cannot serialize.
async fn report_payload(state: &AppState, row: &ReportRow) -> AppResult<Value> {
    let target = user::get_by_id(&state.db, row.target_id)
        .await?
        .ok_or(AppError::NotFound)?;
    let cited = report::statuses_for(&state.db, row.id).await?;
    let entity = Report::from_row(row, &target, &cited, &state.config, None);
    serde_json::to_value(entity)
        .map(|report| json!({"report": report}))
        .map_err(|err| AppError::Internal(format!("the report entity did not serialize: {err}")))
}

/// The `account.created` hook: the freshly registered local `account` as
/// `{"account": …}`.
///
/// The render is [`AdminAccount`]'s — the shape Pleroma's `account.created`
/// delivery carries, since the event is for staff tooling and it names the
/// address and the flags registration set.
pub async fn account_created(state: &AppState, account: &User) {
    match account_payload(state, account) {
        Ok(data) => emit(state, EVENT_ACCOUNT_CREATED, data).await,
        Err(err) => {
            warn!(%err, account = %account.id, "the account.created event could not be rendered");
        }
    }
}

/// `{"account": …}`: the row as the admin account entity.
///
/// # Errors
///
/// [`AppError::Internal`] for an entity that somehow cannot serialize.
fn account_payload(state: &AppState, account: &User) -> AppResult<Value> {
    let entity = AdminAccount::from_user(account, &state.config);
    serde_json::to_value(entity)
        .map(|rendered| json!({"account": rendered}))
        .map_err(|err| AppError::Internal(format!("the account entity did not serialize: {err}")))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::{
        Router,
        body::Bytes,
        extract::State as AxumState,
        http::{HeaderMap, StatusCode},
        routing::post,
    };
    use sqlx::PgPool;
    use tokio::{net::TcpListener, task::JoinHandle};

    use super::*;
    use crate::{
        config::{Config, TEST_BASE_URL},
        http_client::HttpClient,
        models::{
            job::Job,
            report::{Category, NewReport},
            user::{NewUser, insert},
        },
        streaming::hub::Hub,
        uploads::UploaderBackend,
    };

    /// Test state over `pool` on the fixed test config, whose HTTP client
    /// reaches the loopback mocks these tests bind.
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

    /// The lazy pool the payload tests build state on: nothing they call
    /// reaches a database, and a state that does must fail.
    fn dead_state() -> AppState {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://localhost/unused")
            .expect("a lazy pool needs no server");
        app_state(&pool)
    }

    /// The first of the [`MAX_ATTEMPTS`] tries the queue hands a delivery.
    const FIRST_ATTEMPT: AttemptInfo = AttemptInfo {
        attempts: 1,
        max_attempts: MAX_ATTEMPTS,
    };

    /// A local account named `nickname`.
    async fn local(pool: &PgPool, nickname: &str) -> User {
        insert(
            pool,
            &NewUser {
                email: Some(format!("{nickname}@example.org")),
                ..NewUser::local(nickname, &user::local_ap_id(TEST_BASE_URL, nickname))
            },
        )
        .await
        .expect("the local fixture inserts")
    }

    /// `reporter`'s filed report about `target`, with one comment.
    async fn filed(pool: &PgPool, reporter: &User, target: &User) -> ReportRow {
        report::create(
            pool,
            TEST_BASE_URL,
            reporter,
            target,
            &NewReport {
                comment: "they posted that thing".to_owned(),
                category: Category::Spam,
                forwarded: false,
                rule_ids: vec![],
                status_ids: vec![],
            },
        )
        .await
        .expect("the report fixture files")
    }

    /// A webhook subscribed to every [`KNOWN_EVENTS`](webhook::KNOWN_EVENTS)
    /// event, delivered to `url` (or to nowhere when the test never sends).
    async fn hook(pool: &PgPool, url: &str, events: &[&str]) -> Webhook {
        let names: Vec<String> = events.iter().map(|event| (*event).to_owned()).collect();
        webhook::create(pool, url, &names, true)
            .await
            .expect("the webhook fixture inserts")
    }

    /// Every delivery job on the queue, oldest first.
    async fn deliveries(pool: &PgPool) -> Vec<Job> {
        sqlx::query_as::<_, Job>("SELECT * FROM jobs WHERE kind = $1 ORDER BY id")
            .bind(KIND_DELIVER)
            .fetch_all(pool)
            .await
            .expect("the jobs read")
    }

    /// A fresh event id, for the tests that build arguments by hand.
    fn fresh_id() -> Uuid {
        new_id()
    }

    /// What one mock request looked like: the headers that matter and the
    /// body bytes, as the endpoint received them.
    #[derive(Clone, Debug)]
    struct Captured {
        /// The request headers, lowercased names.
        headers: Vec<(String, String)>,
        /// The raw request body, the signature's input.
        body: Vec<u8>,
    }

    /// The capture every mock endpoint writes into.
    type Seen = Arc<Mutex<Vec<Captured>>>;

    /// The mock's state: the status it answers with and where requests land.
    #[derive(Clone)]
    struct Mock {
        /// Status code every answer carries.
        status: u16,
        /// Where each request is recorded.
        seen: Seen,
    }

    /// The mock's handler: record the request, answer with the state's status.
    async fn catch_request(
        AxumState(mock): AxumState<Mock>,
        headers: HeaderMap,
        body: Bytes,
    ) -> StatusCode {
        let pairs = headers
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_owned(),
                    String::from_utf8_lossy(value.as_bytes()).into_owned(),
                )
            })
            .collect();
        mock.seen
            .lock()
            .expect("an unpoisoned lock")
            .push(Captured {
                headers: pairs,
                body: body.to_vec(),
            });
        StatusCode::from_u16(mock.status).expect("the test statuses are representable")
    }

    /// Serves `app` on `listener` until the test aborts the task.
    async fn serve_mock(listener: TcpListener, app: Router) {
        if let Err(err) = axum::serve(listener, app).await {
            tracing::debug!("test webhook endpoint stopped: {err}");
        }
    }

    /// A mock webhook endpoint answering every `POST` with `status`:
    /// `(url, capture, server)`.
    async fn mock_endpoint(status: u16) -> (String, Seen, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a loopback port to serve on");
        let url = format!(
            "http://{}/hook",
            listener.local_addr().expect("the bound address")
        );
        let seen: Seen = Arc::default();
        let app = Router::new()
            .route("/hook", post(catch_request))
            .with_state(Mock {
                status,
                seen: Arc::clone(&seen),
            });
        let server = tokio::spawn(serve_mock(listener, app));
        (url, seen, server)
    }

    /// The named header of the mock's last request.
    fn header(seen: &Seen, name: &str) -> String {
        let last = seen
            .lock()
            .expect("an unpoisoned lock")
            .last()
            .cloned()
            .expect("the mock served a request");
        last.headers
            .into_iter()
            .find(|(key, _value)| key == name)
            .map_or_default(|(_key, value)| value)
    }

    /// The mock's last request body, verbatim.
    fn captured_body(seen: &Seen) -> Vec<u8> {
        let last = seen
            .lock()
            .expect("an unpoisoned lock")
            .last()
            .cloned()
            .expect("the mock served a request");
        last.body
    }

    /// Whether the mock saw any request at all.
    fn served(seen: &Seen) -> bool {
        !seen.lock().expect("an unpoisoned lock").is_empty()
    }

    /// A receiver's own HMAC-SHA256 over `"{timestamp}.{body}"`, hex: the
    /// independent recomputation the delivery test checks the delivered
    /// signature against.
    fn expect_signature(secret: &str, timestamp: &str, body: &[u8]) -> String {
        let mut mac =
            HmacSha256::new_from_slice(secret.as_bytes()).expect("any key length is legal");
        mac.update(timestamp.as_bytes());
        mac.update(b".");
        mac.update(body);
        format!("{SIGNATURE_PREFIX}{}", to_hex(&mac.finalize().into_bytes()))
    }

    /// The message of a discarded job failure, or a panic naming what the
    /// attempt reported instead.
    fn discarded(outcome: Result<(), JobError>, who: &str) -> String {
        match outcome {
            Err(JobError::Discard(detail)) => detail,
            Err(other) => panic!("{who} must discard, it reported {other:?}"),
            Ok(()) => panic!("{who} must discard, it completed"),
        }
    }

    /// The message of a retryable job failure, or a panic naming what the
    /// attempt reported instead.
    fn retried(outcome: Result<(), JobError>, who: &str) -> String {
        match outcome {
            Err(JobError::Retry(detail)) => detail,
            Err(other) => panic!("{who} must retry, it reported {other:?}"),
            Ok(()) => panic!("{who} must retry, it completed"),
        }
    }

    #[test]
    fn the_hmac_hex_of_a_known_vector_is_lowercase_and_full_width() {
        // RFC 4231 case 2: HMAC-SHA256(key "Jefe", data "what do ya want for
        // nothing?"), pinned here so `hex_lower` and the digest are checked
        // against a published value rather than against themselves.
        let mut mac = HmacSha256::new_from_slice(b"Jefe").expect("a key length");
        mac.update(b"what do ya want for nothing?");
        let hex = to_hex(&mac.finalize().into_bytes());
        assert_eq!(
            hex, "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843",
            "the published digest, lowercase hex"
        );
        assert_eq!(hex.len(), 64, "a SHA-256 digest is 32 bytes");
    }

    #[test]
    fn the_signature_covers_the_timestamp_and_the_body_and_says_sha256() {
        let body = r#"{"id":"1","type":"report.created"}"#;
        let signed =
            signature("topsecret", 1_700_000_000, body).expect("a secret of any length signs");
        assert_eq!(
            signed,
            expect_signature("topsecret", "1700000000", body.as_bytes()),
            "the delivered digest is the receiver's recomputation of timestamp dot body"
        );
        assert!(
            signed.starts_with(SIGNATURE_PREFIX),
            "the header names the algorithm: {signed}"
        );
        assert_ne!(
            signed,
            signature("topsecret", 1_700_000_001, body).expect("signs"),
            "a different timestamp signs differently"
        );
        assert_ne!(
            signed,
            signature("othersecret", 1_700_000_000, body).expect("signs"),
            "so does a different secret"
        );
    }

    #[test]
    fn the_envelope_is_the_pinned_document() {
        let event_id = fresh_id();
        let args = DeliverArgs {
            webhook_id: fresh_id(),
            event: EVENT_ACCOUNT_CREATED.to_owned(),
            event_id,
            data: json!({"account": {"id": event_id.to_string()}}),
        };
        let body = event_body(&args, "2024-01-31T12:34:56.789Z");
        let parsed: Value = serde_json::from_str(&body).expect("the envelope is JSON");
        assert_eq!(parsed["id"], event_id.to_string(), "the event id");
        assert_eq!(parsed["type"], EVENT_ACCOUNT_CREATED, "the event type");
        assert_eq!(
            parsed["created_at"], "2024-01-31T12:34:56.789Z",
            "the RFC 3339 stamp of the attempt"
        );
        assert_eq!(parsed["data"], args.data, "and the payload verbatim");
        assert_eq!(
            parsed.as_object().expect("an object").len(),
            4,
            "exactly the four keys the ticket pins"
        );
    }

    #[test]
    fn the_job_shape_round_trips_and_bad_args_discard() {
        let (webhook_id, event_id) = (fresh_id(), fresh_id());
        let args = job_args(webhook_id, EVENT_REPORT_UPDATED, event_id, &json!({"a": 1}));
        assert_eq!(
            args["webhook_id"],
            webhook_id.to_string(),
            "ids travel as decision 4's text"
        );
        assert_eq!(args["event"], EVENT_REPORT_UPDATED);
        assert_eq!(args["event_id"], event_id.to_string());
        assert_eq!(args["data"]["a"], 1, "the payload rides along");
        let parsed = parse_args(&args).expect("the written shape reads back");
        assert_eq!(parsed.webhook_id, webhook_id);
        assert_eq!(parsed.event_id, event_id);
        assert_eq!(parsed.event, EVENT_REPORT_UPDATED);
        assert_eq!(parsed.data, json!({"a": 1}));
        let err = parse_args(&json!({})).expect_err("an empty payload cannot run");
        assert!(
            matches!(err, JobError::Discard(_)),
            "unusable arguments are permanent: {err}"
        );
    }

    #[test]
    fn the_dedup_key_names_the_webhook_and_the_event() {
        let (webhook_id, event_id) = (fresh_id(), fresh_id());
        assert_eq!(
            unique_key(webhook_id, event_id),
            format!("webhook:{webhook_id}:{event_id}"),
            "the ticket's key, so one event never queues one webhook twice"
        );
    }

    #[test]
    fn classification_sorts_the_endpoints_answers() {
        for status in [200_u16, 201, 204, 299] {
            assert_eq!(classify(status), Ok(()), "2xx ({status}) completed");
        }
        for status in [400_u16, 401, 403, 404, 410, 422, 499] {
            let detail = discarded(classify(status), &status.to_string());
            assert!(
                detail.contains(&status.to_string()),
                "the discard names the refusal: {detail}"
            );
        }
        for status in [301_u16, 408, 429, 500, 502, 503] {
            let detail = retried(classify(status), &status.to_string());
            assert!(
                detail.contains(&status.to_string()),
                "the retry names the answer: {detail}"
            );
        }
    }

    #[test]
    fn an_unreachable_target_discards_and_a_broken_one_retries() {
        for err in [
            HttpError::InvalidUrl,
            HttpError::Blocked("private address".to_owned()),
        ] {
            let detail = discarded(Err(http_failure(&err)), &err.to_string());
            assert!(!detail.is_empty(), "the discard states the refusal");
        }
        for err in [
            HttpError::Timeout,
            HttpError::Connect("refused".to_owned()),
            HttpError::TooLarge,
        ] {
            let detail = retried(Err(http_failure(&err)), &err.to_string());
            assert!(!detail.is_empty(), "the retry states the failure");
        }
    }

    #[test]
    fn the_kind_is_registered_with_the_ticket_budget() {
        let registry = crate::job_registry();
        assert!(
            registry.get(KIND_DELIVER).is_some(),
            "the worker knows `{KIND_DELIVER}`"
        );
        assert_eq!(MAX_ATTEMPTS, 5, "the ticket's five attempts");
    }

    #[tokio::test]
    async fn emit_queues_one_job_per_subscribed_enabled_webhook() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let subscribed = hook(&pool, "https://example.org/on", &[EVENT_REPORT_CREATED]).await;
        let off = hook(&pool, "https://example.org/off", &[EVENT_REPORT_CREATED]).await;
        let _disabled = webhook::set_enabled(&pool, off.id, false)
            .await
            .expect("the disable runs")
            .expect("the webhook exists");
        let _other_event = hook(
            &pool,
            "https://example.org/account",
            &[EVENT_ACCOUNT_CREATED],
        )
        .await;
        let _unsubscribed = hook(&pool, "https://example.org/none", &[]).await;

        emit(&state, EVENT_REPORT_CREATED, json!({"report": {"id": "x"}})).await;

        let queued = deliveries(&pool).await;
        assert_eq!(queued.len(), 1, "only the subscribed enabled webhook");
        let job = queued.first().expect("one job");
        assert_eq!(job.queue, QUEUE_DEFAULT, "on the default queue");
        assert_eq!(job.kind, KIND_DELIVER, "the delivery kind");
        assert_eq!(job.max_attempts, MAX_ATTEMPTS, "five tries");
        assert_eq!(
            job.unique_key.as_deref(),
            Some(unique_key(subscribed.id, parse_event_id(&job.args)).as_str()),
            "the key is webhook:<webhook id>:<event id>"
        );
        assert_eq!(job.args["webhook_id"], subscribed.id.to_string());
        assert_eq!(job.args["event"], EVENT_REPORT_CREATED);
        assert_eq!(job.args["data"]["report"]["id"], "x", "the payload");
    }

    /// The `event_id` a queued job carries, as the id it was written as.
    fn parse_event_id(args: &Value) -> Uuid {
        Uuid::parse_str(args["event_id"].as_str().expect("the job names its event"))
            .expect("the event id is an id")
    }

    #[tokio::test]
    async fn one_event_shares_its_id_and_is_queued_once_per_webhook() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let first = hook(&pool, "https://example.org/one", &[EVENT_ACCOUNT_CREATED]).await;
        let second = hook(&pool, "https://example.org/two", &[EVENT_ACCOUNT_CREATED]).await;
        let event_id = fresh_id();
        let data = json!({"account": {"id": "1"}});

        for hook in [&first, &second] {
            queue_delivery(&state, hook, EVENT_ACCOUNT_CREATED, event_id, &data).await;
        }
        // The same event arriving at the same webhook again is deduplicated
        // by the live job's key, exactly as the queue's contract says.
        queue_delivery(&state, &first, EVENT_ACCOUNT_CREATED, event_id, &data).await;

        let queued = deliveries(&pool).await;
        assert_eq!(queued.len(), 2, "one live job per webhook");
        let ids: Vec<Uuid> = queued.iter().map(|job| parse_event_id(&job.args)).collect();
        assert!(
            ids.iter().all(|id| *id == event_id),
            "both deliveries announce one event: {ids:?}"
        );
        let targets: Vec<Uuid> = queued
            .iter()
            .map(|job| {
                Uuid::parse_str(job.args["webhook_id"].as_str().expect("a webhook id"))
                    .expect("an id")
            })
            .collect();
        assert!(
            targets.contains(&first.id) && targets.contains(&second.id),
            "both subscribers are addressed: {targets:?}"
        );
    }

    #[tokio::test]
    async fn a_delivery_posts_the_signed_labeled_envelope() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let (url, seen, server) = mock_endpoint(200).await;
        let hook_row = hook(&pool, &url, &[EVENT_REPORT_CREATED]).await;
        let event_id = fresh_id();
        let args = job_args(
            hook_row.id,
            EVENT_REPORT_CREATED,
            event_id,
            &json!({"report": {"id": "abc"}}),
        );

        let outcome = handle_deliver(state.clone(), FIRST_ATTEMPT, args).await;
        assert_eq!(outcome, Ok(()), "the endpoint accepted the delivery");
        server.abort();

        assert_eq!(
            header(&seen, "content-type"),
            "application/json",
            "the body is labelled"
        );
        assert_eq!(header(&seen, "x-wired-event"), EVENT_REPORT_CREATED);
        assert!(
            header(&seen, "user-agent").starts_with("Wired ("),
            "the instance identifies itself: {}",
            header(&seen, "user-agent")
        );
        let body = captured_body(&seen);
        let parsed: Value = serde_json::from_slice(&body).expect("the endpoint received JSON");
        assert_eq!(parsed["id"], event_id.to_string(), "the event id");
        assert_eq!(parsed["type"], EVENT_REPORT_CREATED, "the event type");
        assert_eq!(parsed["data"]["report"]["id"], "abc", "the payload");
        assert!(
            parse_time_text(parsed["created_at"].as_str().expect("a stamp")),
            "created_at is an RFC 3339 instant: {}",
            parsed["created_at"]
        );
        let stamp = header(&seen, "x-wired-timestamp");
        assert!(
            !stamp.is_empty() && stamp.parse::<i64>().is_ok(),
            "the timestamp header is unix seconds: {stamp}"
        );
        assert_eq!(
            header(&seen, "x-wired-signature"),
            expect_signature(&hook_row.secret, &stamp, &body),
            "the digest verifies over exactly what the endpoint received"
        );
    }

    /// Whether `text` reads as an RFC 3339 instant.
    fn parse_time_text(text: &str) -> bool {
        crate::util::time::parse_time(text).is_some()
    }

    #[tokio::test]
    async fn a_vanished_or_disabled_webhook_completes_without_a_request() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let (url, seen, server) = mock_endpoint(200).await;

        let gone = handle_deliver(
            state.clone(),
            FIRST_ATTEMPT,
            job_args(fresh_id(), EVENT_REPORT_CREATED, fresh_id(), &json!({})),
        )
        .await;
        assert_eq!(gone, Ok(()), "a webhook that is gone retires its job");

        let off = {
            let created = hook(&pool, &url, &[EVENT_REPORT_CREATED]).await;
            webhook::set_enabled(&pool, created.id, false)
                .await
                .expect("the disable runs")
                .expect("the webhook exists")
        };
        let skipped = handle_deliver(
            state.clone(),
            FIRST_ATTEMPT,
            job_args(off.id, EVENT_REPORT_CREATED, fresh_id(), &json!({})),
        )
        .await;
        assert_eq!(skipped, Ok(()), "and so does a disabled one");

        assert!(!served(&seen), "neither delivery touched the endpoint");
        server.abort();
    }

    /// One delivery attempt against a fresh mock endpoint answering
    /// `status`: the job's outcome beside the capture and its server task.
    async fn attempt_against(
        state: &AppState,
        pool: &PgPool,
        status: u16,
    ) -> (Result<(), JobError>, Seen, JoinHandle<()>) {
        let (url, seen, server) = mock_endpoint(status).await;
        let hook_row = hook(pool, &url, &[EVENT_REPORT_CREATED]).await;
        let args = job_args(
            hook_row.id,
            EVENT_REPORT_CREATED,
            fresh_id(),
            &json!({"x": 1}),
        );
        let outcome = handle_deliver(state.clone(), FIRST_ATTEMPT, args).await;
        (outcome, seen, server)
    }

    #[tokio::test]
    async fn an_endpoint_refusal_discards_the_delivery() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        for status in [400_u16, 404, 410, 422] {
            let (outcome, seen, server) = attempt_against(&state, &pool, status).await;
            server.abort();
            assert!(served(&seen), "the {status} endpoint was tried");
            let detail = discarded(outcome, &format!("status {status}"));
            assert!(
                detail.contains(&status.to_string()),
                "the discard names the refusal: {detail}"
            );
        }
    }

    #[tokio::test]
    async fn a_failing_or_throttling_endpoint_retries_the_delivery() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        for status in [408_u16, 429, 500, 503] {
            let (outcome, seen, server) = attempt_against(&state, &pool, status).await;
            server.abort();
            assert!(served(&seen), "the {status} endpoint was tried");
            let detail = retried(outcome, &format!("status {status}"));
            assert!(
                detail.contains(&status.to_string()),
                "the retry names the answer: {detail}"
            );
        }
    }

    #[tokio::test]
    async fn an_unsendable_url_discards_the_delivery() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let mut config = Config::for_tests();
        config.fetch.http_allow_private_hosts = false;
        let state = {
            let config = Arc::new(config);
            AppState {
                db: pool.clone(),
                hub: Hub::new(&pool),
                config: Arc::clone(&config),
                http_client: HttpClient::from(config.as_ref()),
                uploader: UploaderBackend::from(config.as_ref()),
                instance_stats_cache: Arc::default(),
                mrf_config_cache: Arc::default(),
                translation_languages_cache: Arc::default(),
                rate_limiter: Arc::new(crate::rate_limit::RateLimiter::from_config(
                    config.as_ref(),
                )),
                mailer: Arc::new(crate::mailer::MemoryTransport::new()),
            }
        };
        let blocked = hook(&pool, "http://127.0.0.1:9/hook", &[EVENT_REPORT_CREATED]).await;
        let junk = hook(&pool, "not-a-url", &[EVENT_REPORT_CREATED]).await;

        for row in [&blocked, &junk] {
            let outcome = handle_deliver(
                state.clone(),
                FIRST_ATTEMPT,
                job_args(row.id, EVENT_REPORT_CREATED, fresh_id(), &json!({})),
            )
            .await;
            let detail = discarded(outcome, &row.url);
            assert!(!detail.is_empty(), "the refusal says why: {detail}");
        }
    }

    #[tokio::test]
    async fn the_report_hooks_announce_the_filed_report() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let reporter = local(&pool, "whreporter").await;
        let target = local(&pool, "whtarget").await;
        let row = filed(&pool, &reporter, &target).await;
        let _created = hook(&pool, "https://example.org/a", &[EVENT_REPORT_CREATED]).await;
        let _updated = hook(&pool, "https://example.org/b", &[EVENT_REPORT_UPDATED]).await;

        report_created(&state, &row).await;
        report_updated(&state, &row).await;

        let queued = deliveries(&pool).await;
        assert_eq!(queued.len(), 2, "one job per event, one event per hook");
        let events: Vec<&str> = queued
            .iter()
            .map(|job| job.args["event"].as_str().expect("the job names its event"))
            .collect();
        assert!(
            events.contains(&EVENT_REPORT_CREATED) && events.contains(&EVENT_REPORT_UPDATED),
            "both report events fired: {events:?}"
        );
        for job in &queued {
            let data = &job.args["data"];
            assert_eq!(data["report"]["id"], row.id.to_string(), "the report id");
            assert_eq!(data["report"]["comment"], row.comment, "the comment");
            assert_eq!(data["report"]["category"], "spam", "the category");
            assert_eq!(
                data["report"]["target_account"]["acct"], target.nickname,
                "the reported account"
            );
            assert!(
                data["report"].get("action_taken").is_some(),
                "the entity's moderation flag rides along"
            );
        }
    }

    #[tokio::test]
    async fn a_report_that_cannot_be_described_queues_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let reporter = local(&pool, "gone-reporter").await;
        let target = local(&pool, "gone-target").await;
        let row = filed(&pool, &reporter, &target).await;
        let _hook = hook(&pool, "https://example.org/gone", &[EVENT_REPORT_UPDATED]).await;
        // A row whose target account the deletion cascade already took.
        let orphan = ReportRow {
            target_id: fresh_id(),
            ..row.clone()
        };

        report_updated(&state, &orphan).await;

        assert!(
            deliveries(&pool).await.is_empty(),
            "an unrenderable report is logged, not queued"
        );
        assert!(
            report_payload(&state, &orphan).await.is_err(),
            "and the payload read is what fails"
        );
    }

    #[tokio::test]
    async fn an_unreadable_database_queues_nothing_and_never_panics() {
        let state = dead_state();
        let row = ReportRow {
            id: fresh_id(),
            reporter_id: fresh_id(),
            target_id: fresh_id(),
            comment: String::new(),
            category: Category::Other,
            forwarded: false,
            state: report::ReportState::Open,
            assigned_to: None,
            action_taken_at: None,
            rule_ids: vec![],
            ap_id: None,
            inserted_at: Utc::now(),
            updated_at: Utc::now(),
        };
        report_created(&state, &row).await;
        emit(&state, EVENT_ACCOUNT_CREATED, json!({})).await;
        assert!(
            report_payload(&state, &row).await.is_err(),
            "the report read fails against a pool that cannot connect"
        );
    }

    #[tokio::test]
    async fn the_account_hook_announces_the_admin_account_render() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let account = local(&pool, "whnewbie").await;
        let _hook = hook(&pool, "https://example.org/new", &[EVENT_ACCOUNT_CREATED]).await;

        account_created(&state, &account).await;

        let queued = deliveries(&pool).await;
        assert_eq!(queued.len(), 1, "the subscriber got the event");
        let job = queued.first().expect("one job");
        assert_eq!(job.args["event"], EVENT_ACCOUNT_CREATED);
        let data = &job.args["data"];
        assert_eq!(data["account"]["id"], account.id.to_string(), "the id");
        assert_eq!(data["account"]["nickname"], "whnewbie", "the nickname");
        assert_eq!(
            data["account"]["email"],
            json!("whnewbie@example.org"),
            "the admin render carries the address registration stored"
        );
        assert_eq!(
            data["account"]["roles"]["admin"], false,
            "a newbie is no admin"
        );
    }

    #[tokio::test]
    async fn an_unknown_event_queues_nothing_and_the_account_event_does() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let account = local(&pool, "quiet").await;
        let _hook = hook(&pool, "https://example.org/quiet", &[EVENT_ACCOUNT_CREATED]).await;
        // An event no webhook subscribes to (and none can name: it is off
        // `KNOWN_EVENTS`) fans out to nothing at all.
        emit(&state, "report.deleted", json!({"nothing": true})).await;

        account_created(&state, &account).await;
        assert_eq!(
            deliveries(&pool).await.len(),
            1,
            "the unknown event queued nothing, the account event queued one job"
        );
    }
}
