//! The [`StreamEvent`] hub: in-process fan-out with a Postgres bridge.
//!
//! `docs/ARCHITECTURE.md` decision 11 keeps all cross-process machinery in
//! Postgres, so the [`Hub`] is two halves: a single
//! [`tokio::sync::broadcast`] channel that every local subscriber (a
//! streaming WebSocket connection) reads, and the `wired_events` `NOTIFY`
//! channel that carries events to and from the hubs of the other app
//! instances. [`Hub::publish`] delivers to the local subscribers and
//! bridges outward in one call; [`run_pg_listener`] is the receiving half,
//! started from [`crate::serve`] beside the job worker. Every envelope
//! carries the publishing process' random id, and a process' own listener
//! drops its own envelopes — local subscribers were already served by
//! `publish` — so no event is delivered twice on its origin instance.
//!
//! Postgres caps a `NOTIFY` payload at 8 kB, so an envelope larger than
//! [`MAX_NOTIFY_BYTES`] cannot ride the channel whole: the payload body
//! goes into the `UNLOGGED` `stream_event_payloads` table (see
//! `migrations/1791930000_stream_event_payloads.sql`) and the envelope
//! names it with a `{"ref": "<uuid>"}` object instead. Listener processes
//! read the body back before re-broadcasting; the rows outlive that read
//! on purpose (every instance's listener needs them) and are deleted once
//! older than [`PAYLOAD_TTL`] by the [`KIND_CLEANUP`] scheduled job.
//!
//! Pleroma's equivalent is `Pleroma.Web.Streamer`
//! (`lib/pleroma/web/streamer.ex`): `PubSub` topics per stream, `broadcast`
//! on the write side, a socket process subscribed on the read side.

mod bridge;
mod events;
mod listener;
#[cfg(test)]
mod test_support;

use std::{sync::Arc, time::Duration};

use sqlx::PgPool;
use tokio::{runtime::Handle, sync::broadcast};
use tracing::{debug, warn};
use uuid::Uuid;

use self::bridge::{Envelope, bridge_publish};
pub use self::{
    events::{Audience, RenderSpec, StreamEvent, stream_matches},
    listener::{handle_cleanup, run_pg_listener},
};

/// Capacity of a [`Hub`]'s local broadcast channel.
///
/// Events buffered for a subscriber that is momentarily not reading. Past
/// it the subscriber is lagged — it skips what fell out and keeps
/// receiving — never blocked, and the publisher is never held up by it.
pub const BROADCAST_CAPACITY: usize = 4_096;
/// The Postgres `NOTIFY` channel the hubs bridge events over.
pub const NOTIFY_CHANNEL: &str = "wired_events";
/// Largest serialized envelope [`Hub::publish`] puts in a `NOTIFY`.
/// PostgreSQL's own cap is 8 kB; the margin below it keeps an envelope
/// that grew between measuring and sending inside it anyway.
pub const MAX_NOTIFY_BYTES: usize = 7_500;
/// How long a spilled payload reference must stay readable, measured from
/// the row's insert: long enough for every instance's listener to have
/// read it, short enough that the table never grows.
pub const PAYLOAD_TTL: Duration = Duration::from_secs(60);
/// The job kind of the [`PAYLOAD_TTL`] sweep, run from the
/// [`periodic_jobs`](crate::jobs::scheduler::periodic_jobs) registry
/// through the [`JobRegistry`](crate::jobs::JobRegistry) handler
/// [`handle_cleanup`].
pub const KIND_CLEANUP: &str = "streaming.cleanup";
/// Delay [`run_pg_listener`] waits before re-opening a failed or lost
/// `LISTEN` session.
const RECONNECT_DELAY: Duration = Duration::from_secs(1);

/// The in-process streaming bus, shared by every handler and task of one
/// app instance through [`AppState::hub`].
///
/// [`subscribe`](Hub::subscribe) hands out receivers of the single
/// [`BROADCAST_CAPACITY`]-slot channel; [`publish`](Hub::publish) hands an
/// event to the local subscribers and bridges it to the other instances
/// over `NOTIFY` [`NOTIFY_CHANNEL`] (see the module docs for the
/// oversized-payload reference path). A hub is cheap to clone; every
/// clone is the same bus.
#[derive(Clone, Debug)]
pub struct Hub {
    /// The one local fan-out channel. Its sender half lives as long as
    /// the hub does; subscribers come and go.
    tx: broadcast::Sender<Arc<StreamEvent>>,
    /// The pool the `NOTIFY` bridge writes to and reference payloads are
    /// read back from.
    db: PgPool,
    /// This process' identity, minted once, tagged into every envelope
    /// this hub publishes and dropped by [`run_pg_listener`]'s re-broadcast
    /// so the origin never receives its own events back.
    process: Uuid,
}

impl Hub {
    /// An empty hub bridged over `db`.
    #[must_use]
    pub fn new(db: &PgPool) -> Self {
        let (tx, _receiver) = broadcast::channel(BROADCAST_CAPACITY);
        Self {
            tx,
            db: db.clone(),
            process: Uuid::now_v7(),
        }
    }

    /// A receiver of every event this hub publishes from now on. Lagging
    /// behind just costs skipped events: the next read reports
    /// [`broadcast::error::RecvError::Lagged`] and delivery continues at
    /// the oldest event still buffered.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<StreamEvent>> {
        self.tx.subscribe()
    }

    /// Publishes `event`: hands it to every local subscriber right now
    /// (an [`Arc`] clone per receiver, no serialization), then bridges it
    /// to the other instances' hubs over Postgres.
    ///
    /// The bridge runs on a spawned task because `publish` is called from
    /// synchronous job and handler code: an event whose local delivery
    /// succeeded is never lost to a bridge failure, which costs a log
    /// line. Without a tokio runtime around there is nothing to spawn on,
    /// and the event stays local.
    pub fn publish(&self, event: StreamEvent) {
        let event = Arc::new(event);
        if self.tx.send(Arc::clone(&event)).is_err() {
            debug!("streaming event published with no local subscribers");
        }
        let Ok(handle) = Handle::try_current() else {
            warn!("streaming event published outside a tokio runtime; it was not bridged");
            return;
        };
        let db = self.db.clone();
        let process = self.process;
        let _task = handle.spawn(async move {
            if let Err(err) = bridge_publish(&db, process, &event).await {
                warn!(%err, "streaming bridge publish failed");
            }
        });
    }

    /// Re-broadcasts one inbound `NOTIFY` payload on this hub, called by
    /// [`run_pg_listener`] for every notification its session receives.
    ///
    /// Three kinds of payload never reach the local subscribers: an
    /// envelope tagged with this hub's own process id (its events were
    /// delivered by [`publish`](Self::publish) already), an envelope that
    /// is not JSON or not in the envelope shape, and one whose referenced
    /// payload row the [`KIND_CLEANUP`] sweep took before this instance
    /// read it. Each costs a log line, nothing else.
    async fn ingest(&self, raw: &str) {
        let envelope: Envelope = match serde_json::from_str(raw) {
            Ok(envelope) => envelope,
            Err(err) => {
                warn!(%err, "dropping unparseable streaming envelope");
                return;
            }
        };
        if envelope.process == self.process {
            debug!("dropping envelope published by this process");
            return;
        }
        match envelope.into_stream_event(&self.db).await {
            Ok(Some(event)) => {
                let _sent = self.tx.send(Arc::new(event));
            }
            Ok(None) => {
                warn!("dropping cross-process event whose referenced payload expired");
            }
            Err(err) => warn!(%err, "dropping malformed streaming envelope"),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::streaming::hub::test_support::{event_on, lazy_pool, update_event};

    #[tokio::test]
    async fn publish_reaches_every_subscriber() {
        let hub = Hub::new(&lazy_pool());
        let expected = update_event(r#"{"id":"7"}"#);
        let mut first = hub.subscribe();
        let mut second = hub.subscribe();

        hub.publish(expected.clone());

        let one = first.recv().await.unwrap();
        assert_eq!(&*one, &expected, "the whole event arrives intact");
        let two = second.recv().await.unwrap();
        assert_eq!(
            &*two, &expected,
            "and the same instance at every subscriber"
        );
    }

    #[tokio::test]
    async fn a_late_subscriber_sees_only_events_after_it_subscribed() {
        let hub = Hub::new(&lazy_pool());
        hub.publish(update_event("\"early\""));

        let mut late = hub.subscribe();
        hub.publish(update_event("\"late\""));

        let seen = late.recv().await.unwrap();
        assert_eq!(
            seen.payload.as_deref(),
            Some("\"late\""),
            "nothing is replayed from before the subscription"
        );
        assert!(
            matches!(late.try_recv(), Err(broadcast::error::TryRecvError::Empty)),
            "and the earlier event never shows up"
        );
    }

    #[tokio::test]
    async fn a_lagging_receiver_skips_and_never_blocks_the_publisher() {
        let hub = Hub::new(&lazy_pool());
        let mut slow = hub.subscribe();
        let skipped = 10_usize;

        // Push past the capacity on the channel directly: `publish` would
        // spawn a bridge task per event, and the database is not what
        // this test is about.
        for n in 0..BROADCAST_CAPACITY + skipped {
            let _sent = hub.tx.send(Arc::new(update_event(&n.to_string())));
        }
        // Reaching this line is itself proof the publisher never blocked:
        // a full channel is a lagged reader's problem, not the sender's.
        let receivers = hub.tx.send(Arc::new(update_event("tail"))).unwrap();
        assert_eq!(
            receivers, 1,
            "the send still fanned out to the live receiver"
        );

        let lagged = slow.recv().await;
        assert!(
            matches!(lagged, Err(broadcast::error::RecvError::Lagged(_))),
            "the overflowing receiver is told it skipped: {lagged:?}"
        );
        let resumed = slow.recv().await.unwrap();
        let kept_text = resumed.payload.clone().unwrap();
        let oldest_kept: usize = kept_text.parse().unwrap();
        assert!(
            oldest_kept >= skipped,
            "the receiver resumes at the oldest still-buffered event, not a skipped one: {oldest_kept}"
        );
        // Walk on through the surviving buffer to the tail send above; it
        // is the newest event, so reaching it proves delivery continued
        // live, and a receiver that could not continue would leave `recv`
        // an error to unwrap instead.
        let mut next = slow.recv().await.unwrap();
        while next.payload.as_deref() != Some("tail") {
            next = slow.recv().await.unwrap();
        }
    }

    #[tokio::test]
    async fn ingest_never_rebroadcasts_the_process_own_envelopes() {
        let hub = Hub::new(&lazy_pool());
        let mut rx = hub.subscribe();
        let own =
            serde_json::to_string(&Envelope::inline(hub.process, &update_event("{}"))).unwrap();

        hub.ingest(&own).await;

        assert!(
            matches!(rx.try_recv(), Err(broadcast::error::TryRecvError::Empty)),
            "an envelope tagged with this process id is dropped, not double-delivered"
        );
    }

    #[tokio::test]
    async fn ingest_rebroadcasts_other_processes_envelopes() {
        let hub = Hub::new(&lazy_pool());
        let mut rx = hub.subscribe();
        let foreign = serde_json::to_string(&Envelope::inline(
            Uuid::now_v7(),
            &update_event(r#"{"id":"1"}"#),
        ))
        .unwrap();

        hub.ingest(&foreign).await;

        let event = rx.recv().await.unwrap();
        assert_eq!(event.event, "update", "the event name crosses over");
        assert_eq!(
            event.payload.as_deref(),
            Some(r#"{"id":"1"}"#),
            "the inline payload survives"
        );
        assert_eq!(event.audience, Audience::Public, "and so does the audience");
    }

    #[tokio::test]
    async fn junk_envelopes_are_dropped_with_a_log() {
        let hub = Hub::new(&lazy_pool());
        let mut rx = hub.subscribe();
        // Not JSON at all, then a valid shape with an unusable payload,
        // then a reference the (never-connecting) pool cannot read.
        let number = Envelope {
            process: Uuid::now_v7(),
            streams: vec!["public".to_owned()],
            event: "update".to_owned(),
            payload: Value::from(7),
            audience: Audience::Public,
            render: None,
            local_only: false,
        };
        let reference = Envelope {
            payload: json!({ "ref": Uuid::now_v7().to_string() }),
            ..Envelope::inline(Uuid::now_v7(), &event_on(&["public"]))
        };

        hub.ingest("not json at all").await;
        hub.ingest(&serde_json::to_string(&number).unwrap()).await;
        hub.ingest(&serde_json::to_string(&reference).unwrap())
            .await;

        assert!(
            matches!(rx.try_recv(), Err(broadcast::error::TryRecvError::Empty)),
            "none of the three reaches the subscribers"
        );
    }
}
