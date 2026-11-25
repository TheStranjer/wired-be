//! The delivery: [`send`] encrypts one payload document and POSTs it to a
//! subscription's push service, sorting the answer into delivered, gone or
//! retryable ([`classify_status`]), while [`enqueue_deliveries`] queues the
//! per-subscription jobs that call it.

use sqlx::PgPool;
use tracing::debug;
use uuid::Uuid;

use crate::{
    error::{AppError, AppResult},
    http_client::{HttpClient, HttpError},
    models::web_push_subscription::{self, WebPushSubscription},
};

use super::{PushError, TTL_HEADER, URGENCY_HEADER, encrypt_payload};

/// Most response-body bytes read from a push service: only the status
/// matters, so 64 KiB is ample.
const PUSH_RESPONSE_BYTES: usize = 64 * 1024;

/// Delivers one payload to one subscription: [`encrypt_payload`] the JSON,
/// then the `POST`.
///
/// The request carries `Content-Encoding: aes128gcm`,
/// `Content-Type: application/octet-stream`, `TTL: ` [`TTL_HEADER`],
/// `Urgency: ` [`URGENCY_HEADER`] and the pre-signed `Authorization` header
/// ([`vapid_headers`](super::vapid_headers) signs per delivery; [`send`]
/// just carries the text). A 2xx answer is `Ok`. A 404 or 410 — stored
/// keys that cannot encrypt at all — or an endpoint the SSRF guard blocks
/// is [`PushError::Gone`]: the subscription will never receive anything
/// again and its caller deletes it. A 429, a 5xx or a transport failure is [`PushError::Retryable`]; so
/// is any other status, which the ticket leaves on the safe side of a
/// retry rather than a deletion.
///
/// # Errors
///
/// [`PushError`] per the mapping above: [`Gone`](PushError::Gone) for
/// 404/410, unusable subscription keys and guard-blocked endpoints,
/// [`Retryable`](PushError::Retryable)
/// for every other failure.
pub async fn send(
    http: &HttpClient,
    subscription: &WebPushSubscription,
    payload_json: &str,
    authorization: &str,
) -> Result<(), PushError> {
    let encrypted = encrypt_payload(
        payload_json.as_bytes(),
        &subscription.key_p256dh,
        &subscription.key_auth,
    );
    let body = match encrypted {
        Ok(body) => body,
        Err(AppError::Unprocessable(detail)) => {
            debug!(subscription = %subscription.id, detail, "subscription keys are unusable");
            return Err(PushError::Gone);
        }
        Err(other) => return Err(PushError::Retryable(other.to_string())),
    };
    let response = http
        .post_bytes(
            &subscription.endpoint,
            &[
                ("authorization", authorization),
                ("content-encoding", "aes128gcm"),
                ("content-type", "application/octet-stream"),
                ("ttl", TTL_HEADER),
                ("urgency", URGENCY_HEADER),
            ],
            body,
            PUSH_RESPONSE_BYTES,
        )
        .await
        .map_err(|err| classify_transport(subscription.id, &err))?;
    classify_status(response.status)
}

/// Sorts one transport failure into gone or retryable.
///
/// A target the SSRF guard blocks will be blocked on every retry, so it
/// retires the subscription like a 410; any other failure may pass.
fn classify_transport(subscription_id: Uuid, err: &HttpError) -> PushError {
    if let HttpError::Blocked(reason) = err {
        debug!(
            subscription = %subscription_id,
            reason,
            "push endpoint refused by the SSRF guard"
        );
        PushError::Gone
    } else {
        PushError::Retryable(err.to_string())
    }
}

/// Sorts one push-service answer into delivered, gone or retryable.
///
/// # Errors
///
/// [`PushError::Gone`] for 404/410, [`PushError::Retryable`] for anything
/// else outside the `2xx` range.
fn classify_status(status: u16) -> Result<(), PushError> {
    if (200..300).contains(&status) {
        Ok(())
    } else if matches!(status, 404 | 410) {
        Err(PushError::Gone)
    } else {
        Err(PushError::Retryable(format!(
            "the push service answered {status}"
        )))
    }
}

/// Queues one `push.deliver` job ([`crate::jobs::web_push::KIND_DELIVER`])
/// per subscription of `user_id`, for the freshly created notification
/// `notification_id`.
///
/// The [`crate::services::notify`] hook's muscle: a local recipient that
/// just gained a notification gains one delivery job per registered
/// browser, and a recipient with no subscriptions gains none. Returns how
/// many jobs went onto the queue.
///
/// # Errors
///
/// Any database failure (reading subscriptions, writing jobs) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn enqueue_deliveries(
    pool: &PgPool,
    notification_id: Uuid,
    user_id: Uuid,
) -> AppResult<usize> {
    use crate::{
        jobs::{QUEUE_DEFAULT, web_push},
        models::job::{NewJob, enqueue},
    };
    let subscriptions = web_push_subscription::for_user(pool, user_id).await?;
    for subscription in &subscriptions {
        let _job = enqueue(
            pool,
            NewJob {
                queue: QUEUE_DEFAULT,
                kind: web_push::KIND_DELIVER,
                args: web_push::job_args(notification_id, subscription.id),
                run_at: None,
                max_attempts: Some(web_push::MAX_ATTEMPTS),
                unique_key: None,
            },
        )
        .await?;
    }
    Ok(subscriptions.len())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::{
        Router,
        body::Bytes,
        http::{HeaderMap, StatusCode},
        routing::post,
    };
    use chrono::Utc;
    use serde_json::json;
    use tokio::net::TcpListener;

    use super::*;
    use crate::{
        models::web_push_subscription::DEFAULT_POLICY,
        services::web_push::{decrypt_payload, test_support::client_keys},
    };

    /// A stored subscription row aimed at `endpoint`, keys as given.
    fn subscription(endpoint: &str, p256dh: &str, auth: &str) -> WebPushSubscription {
        WebPushSubscription {
            id: Uuid::now_v7(),
            user_id: Some(Uuid::now_v7()),
            token_id: Uuid::now_v7(),
            endpoint: endpoint.to_owned(),
            key_p256dh: p256dh.to_owned(),
            key_auth: auth.to_owned(),
            alerts: json!({}),
            policy: DEFAULT_POLICY.to_owned(),
            inserted_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    /// A local mini push service answering every `POST /push` with
    /// `status`, on a fresh loopback port. Returns its URL and the handle
    /// task to abort when done.
    async fn mock_push_service(status: u16) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a loopback port to serve on");
        let url = format!(
            "http://{}/push",
            listener.local_addr().expect("the bound address")
        );
        let app = Router::new().route(
            "/push",
            post(move || async move {
                StatusCode::from_u16(status).expect("the test statuses are representable")
            }),
        );
        let server = tokio::spawn(async move {
            if let Err(err) = axum::serve(listener, app).await {
                tracing::debug!("test push service stopped: {err}");
            }
        });
        (url, server)
    }

    /// [`send`]s one fixed payload to `subscription` over the test client.
    async fn send_once(subscription: &WebPushSubscription) -> Result<(), PushError> {
        send_once_body(subscription, r#"{"title":"Alice followed you"}"#).await
    }

    #[tokio::test]
    async fn a_2xx_answer_delivers() {
        for status in [200_u16, 201, 204, 299] {
            let (url, server) = mock_push_service(status).await;
            let (p256dh, auth, _private) = client_keys();
            assert_eq!(
                send_once(&subscription(&url, &p256dh, &auth)).await,
                Ok(()),
                "status {status} is a success"
            );
            server.abort();
        }
    }

    #[tokio::test]
    async fn a_404_or_410_reports_the_subscription_gone() {
        for status in [404_u16, 410] {
            let (url, server) = mock_push_service(status).await;
            let (p256dh, auth, _private) = client_keys();
            assert_eq!(
                send_once(&subscription(&url, &p256dh, &auth)).await,
                Err(PushError::Gone),
                "status {status} retires the subscription"
            );
            server.abort();
        }
    }

    #[tokio::test]
    async fn throttling_server_errors_and_other_rejections_retry() {
        for status in [400_u16, 413, 429, 500, 502, 503] {
            let (url, server) = mock_push_service(status).await;
            let (p256dh, auth, _private) = client_keys();
            match send_once(&subscription(&url, &p256dh, &auth)).await {
                Err(PushError::Retryable(detail)) => assert!(
                    detail.contains(&status.to_string()),
                    "the detail names the status: {detail}"
                ),
                other => panic!("status {status} must be retryable, got {other:?}"),
            }
            server.abort();
        }
    }

    #[tokio::test]
    async fn an_endpoint_the_ssrf_guard_blocks_is_gone_not_retryable() {
        let (p256dh, auth, _private) = client_keys();
        let guarded = HttpClient::new("Wired (https://wired.test)");
        for endpoint in [
            "http://127.0.0.1:8080/push",
            "http://10.0.0.5/push",
            "https://192.168.1.1/push",
            "http://[::1]/push",
        ] {
            let outcome = send(
                &guarded,
                &subscription(endpoint, &p256dh, &auth),
                r#"{"title":"Alice followed you"}"#,
                "vapid t=jwt, k=key",
            )
            .await;
            assert_eq!(
                outcome,
                Err(PushError::Gone),
                "a guard block on {endpoint} retires the subscription"
            );
        }
    }

    #[test]
    fn only_a_guard_block_is_gone_among_transport_failures() {
        let id = Uuid::now_v7();
        assert_eq!(
            classify_transport(id, &HttpError::Blocked("private".to_owned())),
            PushError::Gone,
            "Blocked"
        );
        for err in [
            HttpError::Timeout,
            HttpError::Connect("refused".to_owned()),
            HttpError::TooLarge,
        ] {
            assert_eq!(
                classify_transport(id, &err),
                PushError::Retryable(err.to_string()),
                "{err} may pass later"
            );
        }
    }

    #[tokio::test]
    async fn an_unreachable_service_is_retryable_not_gone() {
        let (p256dh, auth, _private) = client_keys();
        match send_once(&subscription("http://127.0.0.1:1/push", &p256dh, &auth)).await {
            Err(PushError::Retryable(_)) => {}
            other => panic!("a dead peer must retry, got {other:?}"),
        }
    }

    /// The five headers a push POST must carry, as the push service sees
    /// them.
    const PUSH_HEADERS: [&str; 5] = [
        "authorization",
        "content-encoding",
        "content-type",
        "ttl",
        "urgency",
    ];

    /// The recording mock's state: the five headers and the body of the
    /// request it served.
    #[derive(Clone, Default)]
    struct Recorder {
        /// `name: value` for each [`PUSH_HEADERS`] entry.
        header_lines: Arc<Mutex<Vec<String>>>,
        /// The request body.
        body: Arc<Mutex<Vec<u8>>>,
    }

    /// The recording handler: store the five push headers and the body,
    /// answer `Created`.
    async fn record_push(
        axum::extract::State(rec): axum::extract::State<Recorder>,
        headers: HeaderMap,
        body: Bytes,
    ) -> StatusCode {
        for name in PUSH_HEADERS {
            let value = headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("<missing>")
                .to_owned();
            rec.header_lines
                .lock()
                .expect("an unpoisoned lock")
                .push(format!("{name}: {value}"));
        }
        *rec.body.lock().expect("an unpoisoned lock") = body.to_vec();
        StatusCode::CREATED
    }

    /// Serves `app` on `listener` until the test aborts the task.
    async fn serve_mock(listener: TcpListener, app: Router) {
        if let Err(err) = axum::serve(listener, app).await {
            tracing::debug!("test push service stopped: {err}");
        }
    }

    #[tokio::test]
    async fn the_request_carries_the_five_push_headers_and_a_reading_coding() {
        let (p256dh, auth, private) = client_keys();
        let rec = Recorder::default();
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a loopback port to serve on");
        let url = format!(
            "http://{}/push",
            listener.local_addr().expect("the bound address")
        );
        let app = Router::new()
            .route("/push", post(record_push))
            .with_state(rec.clone());
        let server = tokio::spawn(serve_mock(listener, app));
        let payload = r#"{"title":"Alice followed you","body":""}"#;
        send_once_body(&subscription(&url, &p256dh, &auth), payload)
            .await
            .expect("the mock accepts");
        server.abort();

        let seen = rec.header_lines.lock().expect("an unpoisoned lock").clone();
        for expected in [
            "authorization: vapid t=jwt, k=key".to_owned(),
            "content-encoding: aes128gcm".to_owned(),
            "content-type: application/octet-stream".to_owned(),
            format!("ttl: {TTL_HEADER}"),
            format!("urgency: {URGENCY_HEADER}"),
        ] {
            assert!(
                seen.contains(&expected),
                "the header `{expected}` is sent, saw {seen:?}"
            );
        }
        let body = rec.body.lock().expect("an unpoisoned lock").clone();
        let decrypted =
            decrypt_payload(&body, &private, &auth).expect("the body is a valid record");
        assert_eq!(
            String::from_utf8(decrypted).expect("the payload is utf-8"),
            payload,
            "the service cannot read the body but the client can"
        );
    }

    /// [`send_once`] with a caller-chosen payload.
    async fn send_once_body(
        subscription: &WebPushSubscription,
        payload: &str,
    ) -> Result<(), PushError> {
        send(
            &HttpClient::new_for_tests("Wired (https://wired.test)"),
            subscription,
            payload,
            "vapid t=jwt, k=key",
        )
        .await
    }

    #[tokio::test]
    async fn unusable_stored_keys_retire_rather_than_retry() {
        let broken = subscription("https://push.invalid/x", "junk-not-base64!!", "also junk!!");
        assert_eq!(
            send_once(&broken).await,
            Err(PushError::Gone),
            "bad keys are a dead subscription, whatever the endpoint"
        );
    }
}
