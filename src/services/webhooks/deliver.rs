//! The delivery job: re-read the webhook, `POST` the signed envelope and
//! turn the answer into the job's verdict.

use chrono::Utc;
use serde_json::Value;
use tracing::debug;

#[cfg(doc)]
use super::KIND_DELIVER;
use super::{
    DeliverArgs, parse_args,
    signing::{classify, event_body, http_failure, signature},
};
use crate::{
    error::AppError,
    http_client::user_agent,
    jobs::{AttemptInfo, HandlerFuture, JobError},
    models::webhook::{self, Webhook},
    state::AppState,
    util::time::mastodon_time,
};

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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;
    use sqlx::PgPool;
    use tokio::task::JoinHandle;

    use super::super::{
        EVENT_REPORT_CREATED, job_args,
        test_support::{
            FIRST_ATTEMPT, Seen, app_state, captured_body, discarded, expect_signature, fresh_id,
            header, hook, mock_endpoint, retried, served,
        },
    };
    use super::*;
    use crate::{
        config::Config, http_client::HttpClient, streaming::hub::Hub, uploads::UploaderBackend,
    };

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
}
