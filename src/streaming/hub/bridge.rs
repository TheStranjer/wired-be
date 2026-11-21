//! The publishing half of the Postgres bridge: the [`Envelope`] that rides
//! `NOTIFY`, and the spill of oversized payloads into the
//! `stream_event_payloads` table.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
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
/// either inline (the JSON string itself, or `null`) or replaced by a
/// `{"ref": "<uuid>"}` object naming a `stream_event_payloads` row.
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

/// Bridges one event to the other instances over `NOTIFY`, spilling an
/// oversized payload into [`store_payload`] first and sending only a
/// reference.
pub(super) async fn bridge_publish(
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

    #[tokio::test]
    async fn an_envelope_that_cannot_shrink_is_refused_not_truncated() {
        // Over the cap, but with no payload to externalize: the only copy
        // of the event's data is the envelope itself.
        let fat = StreamEvent {
            streams: vec!["s".repeat(MAX_NOTIFY_BYTES + 10)],
            event: "update".to_owned(),
            payload: None,
            audience: Audience::Public,
            render: None,
            local_only: false,
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
