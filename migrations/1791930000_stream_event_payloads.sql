-- Event hub bridge (WBE-145): the spill-over storage for streaming events
-- too large to ride a Postgres NOTIFY (see src/streaming/hub.rs). NOTIFY
-- payloads cap at 8 kB, so `Hub::publish` inlines at most
-- `MAX_NOTIFY_BYTES` of envelope and, past that, stores the event's payload
-- body here and names it in the NOTIFY with a `{"ref": "<uuid>"}` object.
-- The rows are transient — the `streaming.cleanup` scheduled job deletes
-- them once `PAYLOAD_TTL` has lapsed, long after every instance's listener
-- has read them — so the table is `UNLOGGED`: no WAL, no crash recovery,
-- no reason to pay either for data that is gone within a minute.
CREATE UNLOGGED TABLE stream_event_payloads (
    -- UUIDv7 made in Rust (decision 4); no database-side default. The
    -- publishing process mints it, puts it in the NOTIFY reference, and
    -- inserts this row before the NOTIFY fires.
    id uuid PRIMARY KEY,
    -- The event's payload verbatim: the entity serialized as a JSON
    -- string, exactly as `StreamEvent::payload` holds it.
    payload text NOT NULL,
    -- When the body was stored; the cleanup job expires rows by this.
    inserted_at timestamptz NOT NULL DEFAULT now()
);

-- The cleanup job's sweep: delete everything older than the TTL, a range
-- seek on this index instead of a seq scan.
CREATE INDEX stream_event_payloads_inserted_at_idx ON stream_event_payloads (inserted_at);
