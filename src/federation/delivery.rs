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
//! host the instance refuses — its own policy table or the
//! [`mrf`] domain lists — and in every one of those
//! cases it reports `Ok(())` without queueing anything — refusing to
//! federate is not a failure. While `FEDERATION_ENABLED` is off it is a plain
//! no-op: an instance that does not federate queues no deliveries at all. The
//! same `(activity, inbox)` pair is enqueued once: the job's `unique_key`
//! (`deliver:{activity_id}:{inbox}`) makes the queue's dedup index swallow a
//! repeated call while a delivery for it is still live.
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
//!
//! The attempt also keeps the [`instances`] reachability ledger, per host =
//! the authority of the inbox: a `2xx` answer is a successful outbound
//! exchange, so [`instance::touch`] first-sights the host and
//! [`instance::set_reachable`] clears any mark there; a [`HttpError::Connect`]
//! or [`HttpError::Timeout`] failure marks the host unreachable on any
//! attempt, and so does a `5xx` answer once this attempt spends the
//! [`MAX_ATTEMPTS`] budget (a `5xx` with retries still left says only *not
//! now*). A `410 Gone` marks it on its first answer too: unlike the other
//! refusals it does not reject one activity, it declares the address itself
//! gone for good, and a host that says so of its inbox has opted out of
//! every later fan-out ([`crate::federation::publisher`] drops candidates
//! whose host the ledger marks, so once a `410` lands no new delivery is
//! even queued there). Everything else stays silent in the ledger: any
//! other permanent
//! `4xx` refusal and an [`HttpError::InvalidUrl`] or [`HttpError::Blocked`]
//! guard rejection tell nothing about the host being down. The marking
//! itself is bookkeeping: [`instance::set_unreachable`] keeps an earlier
//! mark in place, and a ledger write that fails is logged and never changes
//! the job's outcome.
//!
//! [`instances`]: crate::models::instance
//! [`HttpError::Blocked`]: crate::http_client::HttpError::Blocked
//! [`HttpError::Connect`]: crate::http_client::HttpError::Connect
//! [`HttpError::InvalidUrl`]: crate::http_client::HttpError::InvalidUrl
//! [`HttpError::Timeout`]: crate::http_client::HttpError::Timeout
//! [`instance::set_reachable`]: crate::models::instance::set_reachable
//! [`instance::set_unreachable`]: crate::models::instance::set_unreachable
//! [`instance::touch`]: crate::models::instance::touch
//! [`mrf`]: crate::federation::mrf

use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

mod enqueue;
mod outcome;
mod send;
#[cfg(test)]
mod send_signing;
#[cfg(test)]
mod test_support;

pub use enqueue::{best_inbox, best_inboxes, enqueue_checked, enqueue_delivery};
pub use send::{handle, is_local_inbox};

/// The job kind of one inbox delivery, on
/// [`QUEUE_FEDERATOR_OUTGOING`].
///
/// [`QUEUE_FEDERATOR_OUTGOING`]: crate::jobs::QUEUE_FEDERATOR_OUTGOING
pub const KIND_DELIVER: &str = "federator.deliver";

/// Attempts a delivery gets before it is given up on.
///
/// Pleroma's publisher worker retries a handful of times per inbox and then
/// leaves the pair undelivered; seven is that handful, and the queue's
/// [`backoff`](crate::jobs::backoff) keeps the last attempts hours apart.
pub const MAX_ATTEMPTS: i32 = 7;

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

/// The status an inbox answers with to say the address is gone for good.
/// It discards its own delivery like every permanent refusal, and it is the
/// one `4xx` the reachability ledger records: no other refusal indicts the
/// whole host the way "this endpoint is gone forever" does, and the ledger
/// is the mechanism that stops [`crate::federation::publisher`] from
/// queueing later fan-outs against it.
const STATUS_GONE: u16 = 410;

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
