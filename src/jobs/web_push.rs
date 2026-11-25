//! The Web Push delivery job: one notification to one subscription.
//!
//! `docs/ARCHITECTURE.md` decision 11 keeps background work in Postgres
//! rows; this is the consumer of the `push.deliver` jobs the [`notify`]
//! hook queues when a notification lands for a local account (the
//! counterpart of Pleroma's `Pleroma.Web.Push.Impl.build` + `deliver`
//! fan-out, `lib/pleroma/web/push/impl.ex`). [`handle_deliver`] (job kind
//! [`KIND_DELIVER`], queue [`QUEUE_DEFAULT`](super::QUEUE_DEFAULT),
//! [`MAX_ATTEMPTS`] tries) takes
//! the `{notification_id, subscription_id}` pair, re-reads both rows and
//! runs the same two gates Pleroma's fan-out runs: the subscription's
//! [`alerts`](web_push::alert_enabled) flag for the notification's type,
//! and its policy — `all` wakes for anyone, `followed` only for actors the
//! recipient follows, `follower` only for the recipient's followers,
//! `none` for nobody.
//!
//! Past the gates the payload is the Mastodon-shaped JSON document
//! [`web_push::payload_json`] builds — the subscription's access-token
//! string, the notification id and type, the actor's avatar as `icon`, the
//! `"<actor display name> <verb>"` title, and the status's plain text as
//! `body`, blank when the recipient set
//! [`hide_notification_contents`](crate::models::user::notification_settings::NotificationSettings::hide_notification_contents)
//! — encrypted and VAPID-signed by [`crate::services::web_push`] and
//! `POST`ed.
//!
//! The failure mapping follows the ticket: a
//! [`Gone`](web_push::PushError::Gone) delivery (404/410, keys that can
//! decrypt nothing, or an endpoint the SSRF guard blocks) deletes the subscription and completes the job — there
//! is nothing to retry and nobody to wake again; a
//! [`Retryable`](web_push::PushError::Retryable) one (5xx, 429, transport,
//! or a database hiccup on the reads) hands the job back to the queue,
//! whose third failure discards it. A vanished notification, subscription
//! or token is not an error: whatever deleted them had the delivery's own
//! consent, so the job completes on nothing.
//!
//! [`notify`]: crate::services::notify

use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

mod deliver;
#[cfg(test)]
mod test_support;
mod text;

use self::deliver::deliver;
#[cfg(doc)]
use crate::services::web_push;
use crate::{
    jobs::{AttemptInfo, HandlerFuture, parse_args},
    state::AppState,
};

/// The job kind of one Web Push delivery, handled by [`handle_deliver`] on
/// [`QUEUE_DEFAULT`](super::QUEUE_DEFAULT).
pub const KIND_DELIVER: &str = "push.deliver";
/// The attempt budget the [`notify`](crate::services::notify) hook queues
/// each delivery with: push services flap, and three tries with the
/// queue's backoff is a real chance to land without an infinite chain on
/// a dead peer.
pub const MAX_ATTEMPTS: i32 = 3;

/// The `{notification_id, subscription_id}` arguments of a delivery job,
/// as [`job_args`] writes and [`handle_deliver`] reads them.
#[derive(Debug, Deserialize)]
struct DeliverArgs {
    /// The notification whose facts the payload reports.
    notification_id: Uuid,
    /// The subscription to wake.
    subscription_id: Uuid,
}

/// The job-arguments document for one delivery: the pair [`handle_deliver`]
/// re-reads, so the job survives either row's deletion between enqueue and
/// attempt.
#[must_use]
pub fn job_args(notification_id: Uuid, subscription_id: Uuid) -> Value {
    json!({
        "notification_id": notification_id,
        "subscription_id": subscription_id,
    })
}

/// The job name in the discard text of unusable [`DeliverArgs`].
const DELIVER_JOB: &str = "web push";

/// The job name in the retry text of a failed delivery; every read the job
/// does is idempotent, so another attempt costs nothing but the wait.
const PASS_JOB: &str = "web push delivery";

/// The [`JobRegistry`](super::JobRegistry) handler for [`KIND_DELIVER`].
pub fn handle_deliver(state: AppState, _attempt: AttemptInfo, args: Value) -> HandlerFuture {
    Box::pin(async move {
        let DeliverArgs {
            notification_id,
            subscription_id,
        } = parse_args(&args, DELIVER_JOB)?;
        deliver(&state, notification_id, subscription_id).await
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::JobError;

    #[test]
    fn the_job_shape_round_trips_and_bad_args_discard() {
        let (note_id, subscription_id) = (Uuid::now_v7(), Uuid::now_v7());
        let args = job_args(note_id, subscription_id);
        assert_eq!(
            args["notification_id"],
            note_id.to_string(),
            "ids travel as decision 4's text"
        );
        assert_eq!(args["subscription_id"], subscription_id.to_string());
        let parsed =
            parse_args::<DeliverArgs>(&args, DELIVER_JOB).expect("the written shape reads back");
        assert_eq!(parsed.notification_id, note_id);
        assert_eq!(parsed.subscription_id, subscription_id);
        let err = parse_args::<DeliverArgs>(&json!({}), DELIVER_JOB)
            .expect_err("a pair-less payload cannot run");
        assert!(
            matches!(err, JobError::Discard(_)),
            "unusable arguments are permanent: {err}"
        );
    }

    #[test]
    fn the_kind_is_registered_with_the_ticket_budget() {
        let registry = crate::job_registry(&crate::config::Config::for_tests());
        assert!(
            registry.get(KIND_DELIVER).is_some(),
            "the worker knows `{KIND_DELIVER}`"
        );
        assert_eq!(MAX_ATTEMPTS, 3, "the ticket's three attempts");
    }
}
