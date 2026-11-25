//! Fixtures the webhook service's unit tests share: test state, account and
//! report fixtures, and the mock webhook endpoint.

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
use uuid::Uuid;

use super::{KIND_DELIVER, MAX_ATTEMPTS, signing::SIGNATURE_PREFIX};
use crate::{
    config::TEST_BASE_URL,
    jobs::{AttemptInfo, JobError},
    models::{
        job::Job,
        report::{self, Category, NewReport, Report as ReportRow},
        user::{self, NewUser, User, insert},
        webhook::{self, Webhook},
    },
    util::{hmac::hmac_sha256, id::new_id, token::to_hex},
};

/// The first of the [`MAX_ATTEMPTS`] tries the queue hands a delivery.
pub(super) const FIRST_ATTEMPT: AttemptInfo = AttemptInfo {
    attempts: 1,
    max_attempts: MAX_ATTEMPTS,
};

/// A local account named `nickname`.
pub(super) async fn local(pool: &PgPool, nickname: &str) -> User {
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
pub(super) async fn filed(pool: &PgPool, reporter: &User, target: &User) -> ReportRow {
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
pub(super) async fn hook(pool: &PgPool, url: &str, events: &[&str]) -> Webhook {
    let names: Vec<String> = events.iter().map(|event| (*event).to_owned()).collect();
    webhook::create(pool, url, &names, true)
        .await
        .expect("the webhook fixture inserts")
}

/// Every delivery job on the queue, oldest first.
pub(super) async fn deliveries(pool: &PgPool) -> Vec<Job> {
    sqlx::query_as::<_, Job>("SELECT * FROM jobs WHERE kind = $1 ORDER BY id")
        .bind(KIND_DELIVER)
        .fetch_all(pool)
        .await
        .expect("the jobs read")
}

/// A fresh event id, for the tests that build arguments by hand.
pub(super) fn fresh_id() -> Uuid {
    new_id()
}

/// What one mock request looked like: the headers that matter and the
/// body bytes, as the endpoint received them.
#[derive(Clone, Debug)]
pub(super) struct Captured {
    /// The request headers, lowercased names.
    pub(super) headers: Vec<(String, String)>,
    /// The raw request body, the signature's input.
    pub(super) body: Vec<u8>,
}

/// The capture every mock endpoint writes into.
pub(super) type Seen = Arc<Mutex<Vec<Captured>>>;

/// The mock's state: the status it answers with and where requests land.
#[derive(Clone)]
pub(super) struct Mock {
    /// Status code every answer carries.
    pub(super) status: u16,
    /// Where each request is recorded.
    pub(super) seen: Seen,
}

/// The mock's handler: record the request, answer with the state's status.
pub(super) async fn catch_request(
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
pub(super) async fn serve_mock(listener: TcpListener, app: Router) {
    if let Err(err) = axum::serve(listener, app).await {
        tracing::debug!("test webhook endpoint stopped: {err}");
    }
}

/// A mock webhook endpoint answering every `POST` with `status`:
/// `(url, capture, server)`.
pub(super) async fn mock_endpoint(status: u16) -> (String, Seen, JoinHandle<()>) {
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
pub(super) fn header(seen: &Seen, name: &str) -> String {
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
pub(super) fn captured_body(seen: &Seen) -> Vec<u8> {
    let last = seen
        .lock()
        .expect("an unpoisoned lock")
        .last()
        .cloned()
        .expect("the mock served a request");
    last.body
}

/// Whether the mock saw any request at all.
pub(super) fn served(seen: &Seen) -> bool {
    !seen.lock().expect("an unpoisoned lock").is_empty()
}

/// A receiver's own HMAC-SHA256 over `"{timestamp}.{body}"`, hex: the
/// independent recomputation the delivery test checks the delivered
/// signature against.
pub(super) fn expect_signature(secret: &str, timestamp: &str, body: &[u8]) -> String {
    let message = [timestamp.as_bytes(), b".", body].concat();
    let digest = hmac_sha256(secret.as_bytes(), &message).expect("any key length is legal");
    format!("{SIGNATURE_PREFIX}{}", to_hex(&digest))
}

/// The message of a discarded job failure, or a panic naming what the
/// attempt reported instead.
pub(super) fn discarded(outcome: Result<(), JobError>, who: &str) -> String {
    match outcome {
        Err(JobError::Discard(detail)) => detail,
        Err(other) => panic!("{who} must discard, it reported {other:?}"),
        Ok(()) => panic!("{who} must discard, it completed"),
    }
}

/// The message of a retryable job failure, or a panic naming what the
/// attempt reported instead.
pub(super) fn retried(outcome: Result<(), JobError>, who: &str) -> String {
    match outcome {
        Err(JobError::Retry(detail)) => detail,
        Err(other) => panic!("{who} must retry, it reported {other:?}"),
        Ok(()) => panic!("{who} must retry, it completed"),
    }
}
