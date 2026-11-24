//! The publishing half of the Postgres bridge: the [`Envelope`] that rides
//! `NOTIFY`, and the spill of oversized payloads into the
//! `stream_event_payloads` table.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sqlx::PgPool;
use uuid::Uuid;

use super::{
    MAX_NOTIFY_BYTES, NOTIFY_CHANNEL,
    events::{Audience, RenderSpec, StreamEvent},
};

/// What the Postgres side of the hub can fail on.
#[derive(Debug)]
pub(super) enum BridgeError {
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

/// The JSON envelope `NOTIFY` carries: a [`StreamEvent`]'s `streams`,
/// `event`, `audience`, `render` and `local_only`, with its `payload`
/// either inline (the JSON string itself, or `null`) or, in the form older
/// versions wrote and this one still reads, replaced by a
/// `{"ref": "<uuid>"}` object naming a `stream_event_payloads` row. An
/// envelope over the `NOTIFY` cap travels as an [`EnvelopeRef`] instead.
/// `render` and `local_only` default when absent, so envelopes written
/// before those fields existed still parse.
#[derive(Debug, Deserialize, Serialize)]
pub(super) struct Envelope {
    /// The id of the process that published this envelope.
    pub(super) process: Uuid,
    /// [`StreamEvent::streams`].
    pub(super) streams: Vec<String>,
    /// [`StreamEvent::event`].
    pub(super) event: String,
    /// [`StreamEvent::payload`] in its wire form: a JSON string, a
    /// reference object, or `null`.
    pub(super) payload: Value,
    /// [`StreamEvent::audience`].
    pub(super) audience: Audience,
    /// [`StreamEvent::render`]; [`None`] for envelopes that predate it.
    #[serde(default)]
    pub(super) render: Option<RenderSpec>,
    /// [`StreamEvent::local_only`]; `false` for envelopes that predate it.
    #[serde(default)]
    pub(super) local_only: bool,
}

impl Envelope {
    /// The envelope of `event` with its payload riding inline.
    pub(super) fn inline(process: Uuid, event: &StreamEvent) -> Self {
        Self {
            process,
            streams: event.streams.clone(),
            event: event.event.clone(),
            payload: event.payload.clone().map_or(Value::Null, Value::String),
            audience: event.audience.clone(),
            render: event.render.clone(),
            local_only: event.local_only,
        }
    }

    /// The event this envelope describes, reading a referenced payload
    /// body out of `db`; `None` when the reference names a row that is
    /// already gone (the [`KIND_CLEANUP`](super::KIND_CLEANUP) sweep ran
    /// before this process read it).
    pub(super) async fn into_stream_event(
        self,
        db: &PgPool,
    ) -> Result<Option<StreamEvent>, BridgeError> {
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
            render: self.render,
            local_only: self.local_only,
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

/// The minimal `NOTIFY` text for an envelope too big to ride the channel:
/// the publishing process and the id of the `stream_event_payloads` row
/// that holds the whole inline [`Envelope`] JSON.
#[derive(Debug, Deserialize, Serialize)]
pub(super) struct EnvelopeRef {
    /// The id of the process that published the envelope.
    pub(super) process: Uuid,
    /// The `stream_event_payloads` row holding the inline envelope JSON.
    pub(super) envelope_ref: Uuid,
}

impl EnvelopeRef {
    /// The envelope stored under this reference; `None` when the row is
    /// already gone (the [`KIND_CLEANUP`](super::KIND_CLEANUP) sweep ran
    /// before this process read it).
    async fn fetch(&self, db: &PgPool) -> Result<Option<Envelope>, BridgeError> {
        let Some(text) = fetch_payload(db, self.envelope_ref).await? else {
            return Ok(None);
        };
        let envelope = serde_json::from_str(&text).map_err(BridgeError::Json)?;
        Ok(Some(envelope))
    }
}

/// What one inbound `NOTIFY` text parses into: the slim reference form
/// that [`bridge_publish`] writes for an oversized envelope, or a whole
/// [`Envelope`] (the inline form, which may itself carry the older
/// `{"ref"}` payload reference).
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(super) enum Inbound {
    /// An envelope stored in `stream_event_payloads`.
    Referenced(EnvelopeRef),
    /// An envelope carried whole in the notification.
    Whole(Envelope),
}

impl Inbound {
    /// The id of the process that published this notification.
    pub(super) const fn process(&self) -> Uuid {
        match self {
            Self::Referenced(reference) => reference.process,
            Self::Whole(envelope) => envelope.process,
        }
    }

    /// The whole envelope, read out of `db` when it was sent by
    /// reference; `None` when that row is already gone.
    pub(super) async fn into_envelope(self, db: &PgPool) -> Result<Option<Envelope>, BridgeError> {
        match self {
            Self::Referenced(reference) => reference.fetch(db).await,
            Self::Whole(envelope) => Ok(Some(envelope)),
        }
    }
}

/// Bridges one event to the other instances over `NOTIFY`.
pub(super) async fn bridge_publish(
    db: &PgPool,
    process: Uuid,
    event: &StreamEvent,
) -> Result<(), BridgeError> {
    let text = notification_text(db, process, event).await?;
    notify(db, &text).await
}

/// The text to `NOTIFY` for `event`: the inline envelope JSON when it
/// fits [`MAX_NOTIFY_BYTES`], otherwise a minimal [`EnvelopeRef`] after
/// storing the whole inline envelope JSON through [`store_payload`].
/// `streams` and `audience` grow with the recipients, so the whole
/// envelope, not just its payload, is what spills.
async fn notification_text(
    db: &PgPool,
    process: Uuid,
    event: &StreamEvent,
) -> Result<String, BridgeError> {
    let inline_text =
        serde_json::to_string(&Envelope::inline(process, event)).map_err(BridgeError::Json)?;
    if inline_text.len() <= MAX_NOTIFY_BYTES {
        return Ok(inline_text);
    }
    let envelope_ref = Uuid::now_v7();
    store_payload(db, envelope_ref, &inline_text).await?;
    serde_json::to_string(&EnvelopeRef {
        process,
        envelope_ref,
    })
    .map_err(BridgeError::Json)
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
/// becomes [`PAYLOAD_TTL`](super::PAYLOAD_TTL)-old at the cleanup sweep's
/// next pass.
pub(super) async fn store_payload(db: &PgPool, id: Uuid, payload: &str) -> Result<(), BridgeError> {
    let _result = sqlx::query("INSERT INTO stream_event_payloads (id, payload) VALUES ($1, $2)")
        .bind(id)
        .bind(payload)
        .execute(db)
        .await?;
    Ok(())
}

/// The payload body stored under `id`, `None` when no row carries it
/// anymore.
pub(super) async fn fetch_payload(db: &PgPool, id: Uuid) -> Result<Option<String>, BridgeError> {
    let payload =
        sqlx::query_scalar::<_, String>("SELECT payload FROM stream_event_payloads WHERE id = $1")
            .bind(id)
            .fetch_optional(db)
            .await?;
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;
    use tokio::sync::broadcast;

    use super::*;
    use crate::streaming::hub::{
        Hub,
        test_support::{event_on, lazy_pool, render_cases, render_event, update_event},
    };

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

    /// Every [`RenderSpec`] variant with `local_only` on and off
    /// round-trips publish to subscriber and through the envelope
    /// serialize/deserialize pair — the inline wire form, the
    /// reference-spill form read back from a leased test-database slot,
    /// and [`Hub::ingest`]'s re-broadcast — and a real two-hub `NOTIFY`
    /// crossing delivers the whole event.
    #[tokio::test]
    async fn render_and_local_only_ride_every_envelope_path() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let hub = Hub::new(&pool);
        let mut rx = hub.subscribe();

        for (render, local_only) in render_cases(Uuid::now_v7()) {
            let event = render_event(render, local_only);

            hub.publish(event.clone());
            let delivered = rx.recv().await.unwrap();
            assert_eq!(
                &*delivered, &event,
                "publish hands the subscriber both new fields"
            );

            assert_envelope_round_trips(&pool, &hub, &mut rx, &event).await;
        }
    }

    /// Asserts `event` survives the envelope pair on every path: inline
    /// serialize/deserialize, re-broadcast through [`Hub::ingest`], and
    /// the reference spill with its body stored in and read back from
    /// `db`.
    async fn assert_envelope_round_trips(
        db: &PgPool,
        hub: &Hub,
        rx: &mut broadcast::Receiver<Arc<StreamEvent>>,
        event: &StreamEvent,
    ) {
        let inline = Envelope::inline(Uuid::now_v7(), event);
        let text = serde_json::to_string(&inline).unwrap();
        assert!(
            text.contains(r#""render":{"#),
            "a render event writes its render key: {text}"
        );
        let parsed: Envelope = serde_json::from_str(&text).unwrap();
        let back = parsed.into_stream_event(db).await.unwrap();
        assert_eq!(
            back.as_ref(),
            Some(event),
            "the inline path round-trips: {text}"
        );

        hub.ingest(&text).await;
        let rebroadcast = rx.recv().await.unwrap();
        assert_eq!(
            &*rebroadcast, event,
            "ingest re-broadcasts both new fields: {text}"
        );

        let reference = Uuid::now_v7();
        store_payload(db, reference, event.payload.as_deref().unwrap())
            .await
            .unwrap();
        let spilled = Envelope {
            payload: json!({ "ref": reference.to_string() }),
            ..inline
        };
        let spill_text = serde_json::to_string(&spilled).unwrap();
        assert!(
            spill_text.contains(r#""payload":{"ref":"#),
            "the body left the envelope for the reference: {spill_text}"
        );
        let spill_parsed: Envelope = serde_json::from_str(&spill_text).unwrap();
        let spill_back = spill_parsed.into_stream_event(db).await.unwrap();
        assert_eq!(
            spill_back.as_ref(),
            Some(event),
            "the reference path round-trips with the body read back: {spill_text}"
        );
    }

    /// An envelope JSON written before the new fields — it simply lacks
    /// the keys — still parses, and the event it describes carries
    /// `render: None` and `local_only: false`.
    #[tokio::test]
    async fn an_envelope_without_the_new_keys_still_parses() {
        // Build the old shape by taking a fresh inline envelope and
        // deleting the two keys from its JSON object.
        let mut legacy = serde_json::to_value(Envelope::inline(
            Uuid::now_v7(),
            &update_event(r#"{"id":"7"}"#),
        ))
        .unwrap();
        let object = legacy.as_object_mut().unwrap();
        assert!(
            object.remove("render").is_some() && object.remove("local_only").is_some(),
            "an inline envelope writes both keys"
        );
        let text = serde_json::to_string(&legacy).unwrap();

        let envelope: Envelope = serde_json::from_str(&text).unwrap();
        let event = envelope
            .into_stream_event(&lazy_pool())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(event.render, None, "an absent render key reads None");
        assert!(!event.local_only, "an absent local_only key reads false");
        assert_eq!(
            event.payload.as_deref(),
            Some(r#"{"id":"7"}"#),
            "and the old members parse as before"
        );
    }

    /// An event may carry `render`, `payload`, or neither: all four
    /// combinations survive the envelope serialize/deserialize pair.
    #[tokio::test]
    async fn render_and_payload_are_independent_envelope_members() {
        let entity = r#"{"id":"7"}"#.to_owned();
        let spec = RenderSpec::Status(Uuid::now_v7());
        for (payload, render) in [
            (Some(entity.clone()), Some(spec.clone())),
            (None, Some(spec.clone())),
            (Some(entity.clone()), None),
            (None, None),
        ] {
            let event = StreamEvent {
                payload,
                render,
                ..event_on(&["public"])
            };

            let text = serde_json::to_string(&Envelope::inline(Uuid::now_v7(), &event)).unwrap();
            let parsed: Envelope = serde_json::from_str(&text).unwrap();
            let back = parsed.into_stream_event(&lazy_pool()).await.unwrap();

            assert_eq!(
                back.as_ref(),
                Some(&event),
                "this payload/render combination round-trips: {text}"
            );
        }
    }

    /// An event addressed to `count` local users: one `user:{uuid}`
    /// stream each plus the same ids as its [`Audience::Users`], with
    /// `payload` as its entity.
    fn many_recipients(count: usize, payload: Option<String>) -> StreamEvent {
        let users: Vec<Uuid> = (0..count).map(|_| Uuid::now_v7()).collect();
        StreamEvent {
            streams: users.iter().map(|id| format!("user:{id}")).collect(),
            event: "update".to_owned(),
            payload,
            audience: Audience::Users(users),
            render: Some(RenderSpec::Status(Uuid::now_v7())),
            local_only: false,
        }
    }

    /// Number of rows in `stream_event_payloads`.
    async fn payload_rows(db: &PgPool) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM stream_event_payloads")
            .fetch_one(db)
            .await
            .unwrap()
    }

    /// Asserts that the `NOTIFY` text for `event` stays under the cap,
    /// spills exactly one row, and that another hub's [`Hub::ingest`] of
    /// the text re-broadcasts an event equal to `event`.
    async fn assert_spilled_event_crosses(event: StreamEvent) {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let inline = serde_json::to_string(&Envelope::inline(Uuid::now_v7(), &event)).unwrap();
        assert!(
            inline.len() > MAX_NOTIFY_BYTES,
            "the fixture really is over the cap: {} bytes",
            inline.len()
        );

        let text = notification_text(&pool, Uuid::now_v7(), &event)
            .await
            .unwrap();

        assert!(
            text.len() <= MAX_NOTIFY_BYTES && text.contains(r#""envelope_ref":"#),
            "the notification is the minimal reference: {text}"
        );
        assert_eq!(payload_rows(&pool).await, 1, "the envelope spilled once");

        let hub = Hub::new(&pool);
        let mut rx = hub.subscribe();
        hub.ingest(&text).await;
        let delivered = rx.recv().await.unwrap();
        assert_eq!(
            &*delivered, &event,
            "the far hub re-broadcasts the whole original event"
        );
    }

    #[tokio::test]
    async fn an_envelope_oversized_by_recipients_spills_whole_and_crosses() {
        assert_spilled_event_crosses(many_recipients(300, None)).await;
    }

    #[tokio::test]
    async fn an_announcement_with_a_large_payload_spills_whole_and_crosses() {
        let payload = format!(r#"{{"content":"{}"}}"#, "x".repeat(2 * MAX_NOTIFY_BYTES));
        assert_spilled_event_crosses(many_recipients(300, Some(payload))).await;
    }

    #[tokio::test]
    async fn bridge_publish_sends_an_oversized_event_without_error() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();

        bridge_publish(&pool, Uuid::now_v7(), &many_recipients(300, None))
            .await
            .expect("an envelope over the cap is bridged, not refused");

        assert_eq!(payload_rows(&pool).await, 1, "the envelope spilled once");
    }

    #[tokio::test]
    async fn a_small_event_goes_inline_without_a_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let process = Uuid::now_v7();
        let event = update_event(r#"{"id":"7"}"#);

        let text = notification_text(&pool, process, &event).await.unwrap();

        assert_eq!(
            text,
            serde_json::to_string(&Envelope::inline(process, &event)).unwrap(),
            "the notification is the inline envelope"
        );
        assert_eq!(payload_rows(&pool).await, 0, "no row is written");
    }

    #[tokio::test]
    async fn an_expired_envelope_reference_drops_the_event() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let hub = Hub::new(&pool);
        let mut rx = hub.subscribe();
        let text = notification_text(&pool, Uuid::now_v7(), &many_recipients(300, None))
            .await
            .unwrap();
        // The cleanup sweep took the row before this instance read it.
        let _result = sqlx::query("DELETE FROM stream_event_payloads")
            .execute(&pool)
            .await
            .unwrap();

        hub.ingest(&text).await;

        assert!(
            matches!(rx.try_recv(), Err(broadcast::error::TryRecvError::Empty)),
            "an envelope that is already gone is dropped without a broadcast"
        );
    }

    #[tokio::test]
    async fn an_envelope_reference_from_this_process_is_dropped() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let hub = Hub::new(&pool);
        let mut rx = hub.subscribe();
        let text = notification_text(&pool, hub.process, &many_recipients(300, None))
            .await
            .unwrap();

        hub.ingest(&text).await;

        assert!(
            matches!(rx.try_recv(), Err(broadcast::error::TryRecvError::Empty)),
            "the origin already served its local subscribers"
        );
    }

    #[tokio::test]
    async fn a_legacy_payload_reference_is_still_ingested() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let hub = Hub::new(&pool);
        let mut rx = hub.subscribe();
        let event = update_event(r#"{"id":"legacy"}"#);
        let reference = Uuid::now_v7();
        store_payload(&pool, reference, event.payload.as_deref().unwrap())
            .await
            .unwrap();
        let legacy = Envelope {
            payload: json!({ "ref": reference.to_string() }),
            ..Envelope::inline(Uuid::now_v7(), &event)
        };

        hub.ingest(&serde_json::to_string(&legacy).unwrap()).await;

        let delivered = rx.recv().await.unwrap();
        assert_eq!(
            &*delivered, &event,
            "the older payload-only reference form still delivers"
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
    async fn the_reference_table_is_unlogged_and_swept_by_index() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
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

    #[tokio::test]
    async fn an_expired_reference_drops_the_event() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
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
