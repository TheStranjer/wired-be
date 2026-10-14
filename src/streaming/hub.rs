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

use std::{fmt, future::Future, sync::Arc, time::Duration};

use chrono::{TimeDelta, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sqlx::{PgPool, postgres::PgListener};
use tokio::{runtime::Handle, sync::broadcast, time::sleep};
use tracing::{debug, warn};
use uuid::Uuid;

use crate::{
    jobs::{AttemptInfo, HandlerFuture, JobError},
    state::AppState,
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

/// Who may receive a [`StreamEvent`] once it reaches a connection.
///
/// The hub delivers every event to every subscriber; filtering by audience
/// is the connection's job, exactly as it is the socket process's job in
/// Pleroma's `Streamer`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Audience {
    /// Anyone may see it: the public and hashtag timelines.
    Public,
    /// Only these local users may see it: direct messages, notifications,
    /// user streams, chat messages.
    Users(Vec<Uuid>),
    /// Every authenticated local user may see it: instance-wide events
    /// such as announcements.
    Everyone,
}

/// One event on the streaming bus, in the shape a Mastodon streaming
/// client receives it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StreamEvent {
    /// The stream names this event belongs to (`"public"`,
    /// `"public:local"`, `"user"`, `"hashtag:rust"`, `"list:<id>"`,
    /// `"direct"`, …); a connection subscribed to any of them receives it
    /// — see [`stream_matches`].
    pub streams: Vec<String>,
    /// The Mastodon event name: `update`, `delete`, `notification`,
    /// `status.update`, `conversation`, `filters_changed`, `announcement`,
    /// `pleroma:chat_update`, …
    pub event: String,
    /// The entity, already serialized as a JSON string: Mastodon sends
    /// the entity as a STRING inside the envelope, not as nested JSON,
    /// and clients `JSON.parse` it. `None` for events that carry no body
    /// (`filters_changed` and friends).
    pub payload: Option<String>,
    /// Who the event is for; see [`Audience`].
    pub audience: Audience,
}

/// Whether a connection subscribed to the streams in `subscribed` should
/// receive `event`: true when any of the event's target streams is one
/// the connection holds.
///
/// Audience is deliberately not consulted here — the connection checks
/// that against its own authenticated user; this helper answers the
/// stream-name question only.
#[must_use]
pub fn stream_matches(subscribed: &[String], event: &StreamEvent) -> bool {
    event
        .streams
        .iter()
        .any(|stream| subscribed.contains(stream))
}

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

/// What the Postgres side of the hub can fail on.
#[derive(Debug)]
enum BridgeError {
    /// An envelope could not be (de)serialized.
    Json(serde_json::Error),
    /// A database round trip failed.
    Db(sqlx::Error),
    /// An inbound envelope did not have the hub's shape.
    Malformed(String),
}

impl fmt::Display for BridgeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(err) => write!(f, "envelope JSON failed: {err}"),
            Self::Db(err) => write!(f, "database bridge failed: {err}"),
            Self::Malformed(detail) => write!(f, "malformed envelope: {detail}"),
        }
    }
}

impl From<sqlx::Error> for BridgeError {
    fn from(err: sqlx::Error) -> Self {
        Self::Db(err)
    }
}

/// The JSON envelope `NOTIFY` carries: a [`StreamEvent`] with its payload
/// either inline (the JSON string itself, or `null`), or replaced by a
/// `{"ref": "<uuid>"}` object naming a `stream_event_payloads` row.
#[derive(Debug, Deserialize, Serialize)]
struct Envelope {
    /// The id of the process that published this envelope.
    process: Uuid,
    /// [`StreamEvent::streams`].
    streams: Vec<String>,
    /// [`StreamEvent::event`].
    event: String,
    /// [`StreamEvent::payload`] in its wire form: a JSON string, a
    /// reference object, or `null`.
    payload: Value,
    /// [`StreamEvent::audience`].
    audience: Audience,
}

impl Envelope {
    /// The envelope of `event` with its payload riding inline.
    fn inline(process: Uuid, event: &StreamEvent) -> Self {
        Self {
            process,
            streams: event.streams.clone(),
            event: event.event.clone(),
            payload: event.payload.clone().map_or(Value::Null, Value::String),
            audience: event.audience.clone(),
        }
    }

    /// The event this envelope describes, reading a referenced payload
    /// body out of `db`; `None` when the reference names a row that is
    /// already gone (the [`KIND_CLEANUP`] sweep ran before this process
    /// read it).
    async fn into_stream_event(self, db: &PgPool) -> Result<Option<StreamEvent>, BridgeError> {
        let payload = match self.payload {
            Value::Null => None,
            Value::String(payload) => Some(payload),
            Value::Object(object) => {
                let id = reference_id(&object)?;
                match fetch_payload(db, id).await? {
                    Some(body) => Some(body),
                    None => return Ok(None),
                }
            }
            other @ (Value::Bool(_) | Value::Number(_) | Value::Array(_)) => {
                return Err(BridgeError::Malformed(format!(
                    "payload is a JSON {other} where a string, reference or null belongs"
                )));
            }
        };
        Ok(Some(StreamEvent {
            streams: self.streams,
            event: self.event,
            payload,
            audience: self.audience,
        }))
    }
}

/// The uuid a `{"ref": "<uuid>"}` payload object names.
fn reference_id(object: &Map<String, Value>) -> Result<Uuid, BridgeError> {
    let Some(Value::String(reference)) = object.get("ref") else {
        return Err(BridgeError::Malformed(
            "payload object carries no string \"ref\"".to_owned(),
        ));
    };
    Uuid::parse_str(reference)
        .map_err(|err| BridgeError::Malformed(format!("reference is not a uuid: {err}")))
}

/// Bridges one event to the other instances over `NOTIFY`, spilling an
/// oversized payload into [`store_payload`] first and sending only a
/// reference.
async fn bridge_publish(
    db: &PgPool,
    process: Uuid,
    event: &StreamEvent,
) -> Result<(), BridgeError> {
    let envelope = Envelope::inline(process, event);
    let inline_text = serde_json::to_string(&envelope).map_err(BridgeError::Json)?;
    if inline_text.len() <= MAX_NOTIFY_BYTES {
        return notify(db, &inline_text).await;
    }
    // Past the cap the payload must leave the envelope; an envelope
    // without one cannot shrink, and is this event's only copy of the
    // truth, so it is refused rather than truncated.
    let Some(payload) = event.payload.as_deref() else {
        return Err(BridgeError::Malformed(format!(
            "envelope is {} bytes over with no payload to externalize",
            inline_text.len() - MAX_NOTIFY_BYTES
        )));
    };
    let reference = Uuid::now_v7();
    store_payload(db, reference, payload).await?;
    let referenced = Envelope {
        payload: json!({ "ref": reference }),
        ..envelope
    };
    let text = serde_json::to_string(&referenced).map_err(BridgeError::Json)?;
    notify(db, &text).await
}

/// Fires `NOTIFY wired_events` with `text` as its payload. The
/// `pg_notify` function takes the text as a parameter, so no quoting or
/// escaping of the JSON is needed.
async fn notify(db: &PgPool, text: &str) -> Result<(), BridgeError> {
    let _result = sqlx::query("SELECT pg_notify($1, $2)")
        .bind(NOTIFY_CHANNEL)
        .bind(text)
        .execute(db)
        .await?;
    Ok(())
}

/// Stores a payload body under `id` for listeners to read back; it
/// becomes [`PAYLOAD_TTL`]-old at [`cleanup_payloads`]'s next pass.
async fn store_payload(db: &PgPool, id: Uuid, payload: &str) -> Result<(), BridgeError> {
    let _result = sqlx::query("INSERT INTO stream_event_payloads (id, payload) VALUES ($1, $2)")
        .bind(id)
        .bind(payload)
        .execute(db)
        .await?;
    Ok(())
}

/// The payload body stored under `id`, `None` when no row carries it
/// anymore.
async fn fetch_payload(db: &PgPool, id: Uuid) -> Result<Option<String>, BridgeError> {
    let payload =
        sqlx::query_scalar::<_, String>("SELECT payload FROM stream_event_payloads WHERE id = $1")
            .bind(id)
            .fetch_optional(db)
            .await?;
    Ok(payload)
}

/// Deletes the reference rows older than [`PAYLOAD_TTL`], returning how
/// many went. [`KIND_CLEANUP`]'s whole job, run by [`handle_cleanup`].
async fn cleanup_payloads(db: &PgPool) -> Result<u64, BridgeError> {
    let ttl = i64::try_from(PAYLOAD_TTL.as_secs()).unwrap_or(i64::MAX);
    let cutoff = Utc::now() - TimeDelta::try_seconds(ttl).unwrap_or_default();
    let result = sqlx::query("DELETE FROM stream_event_payloads WHERE inserted_at < $1")
        .bind(cutoff)
        .execute(db)
        .await?;
    Ok(result.rows_affected())
}

/// The [`JobRegistry`](crate::jobs::JobRegistry) handler for
/// [`KIND_CLEANUP`]: one sweep of expired reference payloads.
///
/// A failed sweep is a [`JobError::Retry`], and the scheduler enqueues the
/// next period regardless, so a missed pass costs a minute of reference
/// rows, not a lost event.
pub fn handle_cleanup(state: AppState, _attempt: AttemptInfo, _args: Value) -> HandlerFuture {
    Box::pin(async move {
        match cleanup_payloads(&state.db).await {
            Ok(count) => {
                if count > 0 {
                    debug!(
                        count,
                        "streaming cleanup removed expired reference payloads"
                    );
                }
                Ok(())
            }
            Err(err) => Err(JobError::Retry(format!(
                "streaming reference-table sweep failed: {err}"
            ))),
        }
    })
}

/// How one [`listen_session`] ended.
#[derive(Debug)]
enum SessionOutcome {
    /// The shutdown future resolved: the process is stopping.
    Shutdown,
    /// Connecting, `LISTEN`ing or receiving failed, or the connection was
    /// lost; the string is why, and [`run_pg_listener`] retries after
    /// [`RECONNECT_DELAY`].
    Disconnected(String),
}

/// One `LISTEN wired_events` session over its own connection from `pool`:
/// each notification goes to [`Hub::ingest`] until it fails or
/// `shutdown` resolves.
async fn listen_session<F>(pool: &PgPool, hub: &Hub, mut shutdown: F) -> SessionOutcome
where
    F: Future<Output = ()> + Unpin,
{
    let mut listener = match PgListener::connect_with(pool).await {
        Ok(listener) => listener,
        Err(err) => return SessionOutcome::Disconnected(err.to_string()),
    };
    if let Err(err) = listener.listen(NOTIFY_CHANNEL).await {
        return SessionOutcome::Disconnected(err.to_string());
    }
    loop {
        tokio::select! {
            biased;
            () = &mut shutdown => return SessionOutcome::Shutdown,
            notification = listener.try_recv() => match notification {
                Ok(Some(note)) => hub.ingest(note.payload()).await,
                Ok(None) => {
                    return SessionOutcome::Disconnected(
                        "the listener connection was lost".to_owned(),
                    );
                }
                Err(err) => return SessionOutcome::Disconnected(err.to_string()),
            },
        }
    }
}

/// Mirrors every event the other instances publish onto `hub`'s local bus
/// until `shutdown` resolves.
///
/// The loop is a `PgListener` session on [`NOTIFY_CHANNEL`] that
/// reconnects one second after any session fails; events
/// tagged with this process' id are dropped, so this is the bridge's
/// receiving half, started from [`crate::serve`] beside the job worker.
///
/// Delivery here is at-most-once: a notification missed while every
/// connection was down is gone, which is the trade Postgres `NOTIFY`
/// makes for a streaming bus, and what Pleroma's `PubSub` does too.
pub async fn run_pg_listener<F>(pool: PgPool, hub: Hub, shutdown: F)
where
    F: Future<Output = ()>,
{
    tokio::pin!(shutdown);
    loop {
        match listen_session(&pool, &hub, &mut shutdown).await {
            SessionOutcome::Shutdown => break,
            SessionOutcome::Disconnected(reason) => {
                warn!(%reason, "streaming listener session ended; reconnecting");
                tokio::select! {
                    biased;
                    () = &mut shutdown => break,
                    () = sleep(RECONNECT_DELAY) => {}
                }
            }
        }
    }
    debug!("streaming pg listener stopped");
}

#[cfg(test)]
mod tests {
    use sqlx::postgres::PgPoolOptions;
    use tokio::{
        sync::oneshot,
        time::{Instant, timeout},
    };

    use super::*;

    /// Generous ceiling on one `NOTIFY` round trip in these tests.
    const BRIDGE_WAIT: Duration = Duration::from_secs(30);
    /// How long a hub must stay quiet to prove it did not echo an event
    /// back; the round trip the assertions wait on takes milliseconds.
    const QUIET_WAIT: Duration = Duration::from_secs(2);

    /// A lazy, never-connected pool, for tests that never reach Postgres.
    fn lazy_pool() -> PgPool {
        PgPoolOptions::new()
            .acquire_timeout(Duration::from_secs(2))
            .connect_lazy("postgres://localhost/unused")
            .unwrap()
    }

    /// A public `update` event carrying `payload` as its entity.
    fn update_event(payload: &str) -> StreamEvent {
        StreamEvent {
            streams: vec!["public".to_owned()],
            event: "update".to_owned(),
            payload: Some(payload.to_owned()),
            audience: Audience::Public,
        }
    }

    /// An event on exactly the streams in `streams`, with no payload.
    fn event_on(streams: &[&str]) -> StreamEvent {
        StreamEvent {
            streams: streams.iter().copied().map(String::from).collect(),
            event: "update".to_owned(),
            payload: None,
            audience: Audience::Public,
        }
    }

    /// The shutdown future [`run_pg_listener`] accepts: resolves once
    /// `rx` is sent on (or dropped).
    async fn stop_when(rx: oneshot::Receiver<()>) {
        let _signal = rx.await.is_ok();
    }

    /// Spawns a listener task for `hub` over `pool`, returning the handle
    /// and its stopper.
    fn spawn_listener(
        pool: &PgPool,
        hub: &Hub,
    ) -> (oneshot::Sender<()>, tokio::task::JoinHandle<()>) {
        let (stop, ready) = oneshot::channel();
        let task = tokio::spawn(run_pg_listener(pool.clone(), hub.clone(), stop_when(ready)));
        (stop, task)
    }

    /// Waits until `rx` yields an event whose payload is `want`, failing
    /// the test past [`BRIDGE_WAIT`]; lagged reports are skipped over.
    async fn await_payload(rx: &mut broadcast::Receiver<Arc<StreamEvent>>, want: &str) {
        let deadline = Instant::now() + BRIDGE_WAIT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "the wait for payload {want:?} ran out of time"
            );
            match timeout(remaining, rx.recv()).await {
                Ok(Ok(event)) if event.payload.as_deref() == Some(want) => return,
                Ok(Ok(_other)) => {}
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => {}
                Ok(Err(closed)) => panic!("the bus closed while waiting: {closed:?}"),
                Err(elapsed) => panic!("payload {want:?} never arrived: {elapsed}"),
            }
        }
    }

    /// Drops everything `rx` holds right now, without waiting.
    fn drain(rx: &mut broadcast::Receiver<Arc<StreamEvent>>) {
        while rx.try_recv().is_ok() {}
    }

    /// Asserts `rx` already holds `want` and then goes quiet: the hub
    /// delivered its own event exactly once, not also via the bridge.
    async fn assert_single_delivery(
        rx: &mut broadcast::Receiver<Arc<StreamEvent>>,
        want: &str,
        who: &str,
    ) {
        let first = rx.recv().await.unwrap();
        assert_eq!(
            first.payload.as_deref(),
            Some(want),
            "{who} received its own event locally"
        );
        let echo = timeout(QUIET_WAIT, rx.recv()).await;
        assert!(
            echo.is_err(),
            "{who} received a second copy of an event it published itself"
        );
    }

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

    #[test]
    fn stream_matches_requires_one_shared_stream() {
        let subscribed = vec!["public".to_owned(), "hashtag:rust".to_owned()];

        assert!(
            stream_matches(&subscribed, &event_on(&["public"])),
            "an event on a held stream matches"
        );
        assert!(
            stream_matches(&subscribed, &event_on(&["direct", "hashtag:rust"])),
            "one held name among many matches"
        );
        assert!(
            !stream_matches(&subscribed, &event_on(&["user"])),
            "an event on other streams does not"
        );
        assert!(
            !stream_matches(&subscribed, &event_on(&[])),
            "an event on no stream matches nothing"
        );
        assert!(
            !stream_matches(&[], &event_on(&["public"])),
            "an unsubscribed connection receives nothing"
        );
    }

    #[test]
    fn audience_serializes_to_its_wire_forms() {
        assert_eq!(
            serde_json::to_string(&Audience::Public).unwrap(),
            "\"public\"",
            "public is a bare tag"
        );
        assert_eq!(
            serde_json::to_string(&Audience::Everyone).unwrap(),
            "\"everyone\"",
            "everyone is a bare tag"
        );
        let id = Uuid::now_v7();
        let users = serde_json::to_string(&Audience::Users(vec![id])).unwrap();
        assert_eq!(
            users,
            format!("{{\"users\":[\"{id}\"]}}"),
            "users carries the allowed ids"
        );
        let back: Audience = serde_json::from_str(&users).unwrap();
        assert_eq!(back, Audience::Users(vec![id]), "and reads back");
    }

    #[tokio::test]
    async fn an_inline_envelope_round_trips_without_the_database() {
        let hub = Hub::new(&lazy_pool());
        let event = update_event(r#"{"id":"7"}"#);

        let envelope = Envelope::inline(hub.process, &event);
        let text = serde_json::to_string(&envelope).unwrap();
        assert!(
            text.contains(r#""payload":"{\"id\":\"7\"}""#),
            "the payload stays a JSON string inside the envelope: {text}"
        );
        assert!(
            text.contains(r#""audience":"public""#),
            "the audience rides along: {text}"
        );

        let parsed: Envelope = serde_json::from_str(&text).unwrap();
        let back = parsed.into_stream_event(&hub.db).await.unwrap();
        assert_eq!(
            back.as_ref(),
            Some(&event),
            "an inline payload needs no reference-table read"
        );
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

    #[tokio::test]
    async fn an_envelope_that_cannot_shrink_is_refused_not_truncated() {
        // Over the cap, but with no payload to externalize: the only copy
        // of the event's data is the envelope itself.
        let fat = StreamEvent {
            streams: vec!["s".repeat(MAX_NOTIFY_BYTES + 10)],
            event: "update".to_owned(),
            payload: None,
            audience: Audience::Public,
        };

        let err = bridge_publish(&lazy_pool(), Uuid::now_v7(), &fat)
            .await
            .unwrap_err();

        assert!(
            matches!(err, BridgeError::Malformed(_)),
            "the oversized payloadless envelope reports why it cannot bridge: {err}"
        );
    }

    #[tokio::test]
    async fn a_dead_database_costs_the_bridge_not_a_panic() {
        let err = bridge_publish(&lazy_pool(), Uuid::now_v7(), &update_event("{}"))
            .await
            .unwrap_err();

        assert!(
            matches!(err, BridgeError::Db(_)),
            "the notify failure surfaces for the caller's log: {err}"
        );
    }

    #[tokio::test]
    async fn the_listener_loop_ends_with_its_shutdown() {
        let pool = lazy_pool();
        let hub = Hub::new(&pool);
        let (stop, task) = spawn_listener(&pool, &hub);

        drop(stop);

        timeout(BRIDGE_WAIT, task)
            .await
            .expect("the listener returned after its shutdown")
            .expect("the listener task did not panic");
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn the_reference_table_is_unlogged_and_swept_by_index(pool: PgPool) {
        let persistence: String = sqlx::query_scalar(
            "SELECT relpersistence::text FROM pg_class WHERE relname = 'stream_event_payloads'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            persistence, "u",
            "the payload spill-over table skips the WAL"
        );

        let indexes: Vec<String> = sqlx::query_scalar(
            "SELECT indexname FROM pg_indexes WHERE tablename = 'stream_event_payloads'",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert!(
            indexes
                .iter()
                .any(|name| name == "stream_event_payloads_pkey"),
            "the reference id is the key: {indexes:?}"
        );
        assert!(
            indexes
                .iter()
                .any(|name| name == "stream_event_payloads_inserted_at_idx"),
            "the cleanup sweep is an index range scan: {indexes:?}"
        );
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn two_hubs_bridge_small_events_over_postgres(pool: PgPool) {
        let hub_a = Hub::new(&pool);
        let hub_b = Hub::new(&pool);
        let (stop_a, task_a) = spawn_listener(&pool, &hub_a);
        let (stop_b, task_b) = spawn_listener(&pool, &hub_b);
        let mut rx_a = hub_a.subscribe();
        let mut rx_b = hub_b.subscribe();

        // Warm up both directions first, so the real assertions do not
        // race the listeners' initial connect-and-LISTEN.
        hub_a.publish(update_event("\"warm-a\""));
        await_payload(&mut rx_b, "\"warm-a\"").await;
        hub_b.publish(update_event("\"warm-b\""));
        await_payload(&mut rx_a, "\"warm-b\"").await;
        drain(&mut rx_a);
        drain(&mut rx_b);

        hub_a.publish(update_event("\"final\""));

        await_payload(&mut rx_b, "\"final\"").await;
        assert_single_delivery(&mut rx_a, "\"final\"", "the origin").await;

        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM stream_event_payloads")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 0, "an inline event never touches the table");

        drop(stop_a);
        drop(stop_b);
        timeout(BRIDGE_WAIT, task_a)
            .await
            .expect("listener A stopped")
            .expect("listener A did not panic");
        timeout(BRIDGE_WAIT, task_b)
            .await
            .expect("listener B stopped")
            .expect("listener B did not panic");
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn an_oversized_payload_crosses_the_bridge_by_reference(pool: PgPool) {
        let hub_a = Hub::new(&pool);
        let hub_b = Hub::new(&pool);
        let (stop_a, task_a) = spawn_listener(&pool, &hub_a);
        let (stop_b, task_b) = spawn_listener(&pool, &hub_b);
        let mut rx_a = hub_a.subscribe();
        let mut rx_b = hub_b.subscribe();
        hub_a.publish(update_event("\"warm-a\""));
        await_payload(&mut rx_b, "\"warm-a\"").await;
        hub_b.publish(update_event("\"warm-b\""));
        await_payload(&mut rx_a, "\"warm-b\"").await;
        drain(&mut rx_a);
        drain(&mut rx_b);

        let big = format!(r#"{{"content":"{}"}}"#, "x".repeat(2 * MAX_NOTIFY_BYTES));
        hub_a.publish(update_event(&big));

        await_payload(&mut rx_b, &big).await;
        assert_single_delivery(&mut rx_a, &big, "the origin").await;

        let stored: Vec<String> = sqlx::query_scalar("SELECT payload FROM stream_event_payloads")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(
            stored.len(),
            1,
            "the oversized body spilled to exactly one reference row"
        );
        assert_eq!(
            stored.first().unwrap().as_str(),
            big.as_str(),
            "and it holds the payload verbatim for every listener"
        );

        drop(stop_a);
        drop(stop_b);
        timeout(BRIDGE_WAIT, task_a)
            .await
            .expect("listener A stopped")
            .expect("listener A did not panic");
        timeout(BRIDGE_WAIT, task_b)
            .await
            .expect("listener B stopped")
            .expect("listener B did not panic");
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn cleanup_removes_only_expired_reference_rows(pool: PgPool) {
        let fresh = Uuid::now_v7();
        let expired = Uuid::now_v7();
        store_payload(&pool, fresh, "fresh-body").await.unwrap();
        store_payload(&pool, expired, "expired-body").await.unwrap();
        let _result = sqlx::query(
            "UPDATE stream_event_payloads SET inserted_at = now() - make_interval(secs => 120) WHERE id = $1",
        )
        .bind(expired)
        .execute(&pool)
        .await
        .unwrap();

        let removed = cleanup_payloads(&pool).await.unwrap();

        assert_eq!(removed, 1, "exactly the row past its TTL went");
        let kept = fetch_payload(&pool, fresh).await.unwrap();
        assert_eq!(
            kept.as_deref(),
            Some("fresh-body"),
            "a row still inside the TTL keeps serving listeners"
        );
        assert!(
            fetch_payload(&pool, expired).await.unwrap().is_none(),
            "the expired row is gone"
        );
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn an_expired_reference_drops_the_event(pool: PgPool) {
        let hub = Hub::new(&pool);
        let mut rx = hub.subscribe();
        // A reference the sweep took before this instance read it.
        let envelope = Envelope {
            payload: json!({ "ref": Uuid::now_v7().to_string() }),
            ..Envelope::inline(Uuid::now_v7(), &update_event("gone"))
        };

        hub.ingest(&serde_json::to_string(&envelope).unwrap()).await;

        assert!(
            matches!(rx.try_recv(), Err(broadcast::error::TryRecvError::Empty)),
            "a body that is already gone drops the whole event instead of delivering it payload-less"
        );
    }
}
