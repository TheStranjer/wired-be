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

use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{debug, warn};
use uuid::Uuid;

use crate::{
    jobs::{JobError, JobErrorExt as _, QUEUE_DEFAULT},
    models::{
        job::{NewJob, enqueue},
        webhook::{self, Webhook},
    },
    state::AppState,
    util::id::new_id,
};

mod deliver;
mod events;
mod signing;
#[cfg(test)]
mod test_support;

pub use deliver::handle_deliver;
pub use events::{account_created, report_created, report_updated};

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
        .job_discard("webhook job arguments are unusable")
}

#[cfg(test)]
mod tests {
    use super::test_support::{app_state, deliveries, fresh_id, hook};
    use super::*;

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
}
