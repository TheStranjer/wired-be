-- Markers: where one account left off reading each of their timelines
-- (`docs/ARCHITECTURE.md` WBE-97). Reference: Pleroma's `Pleroma.Marker`
-- (lib/pleroma/marker.ex), whose `(user_id, timeline)` uniqueness and
-- `on_conflict: {:replace, [:last_read_id]}` upsert this table serves the same
-- way. Pleroma's `lock_version` column is the API's `version`, incremented by
-- the upsert in `src/models/marker.rs` rather than by Ecto's optimistic locking.
CREATE TABLE markers (
    -- UUIDv7 made in Rust (decision 4); no database-side default.
    id uuid PRIMARY KEY,
    -- A marker belongs to exactly one account, and an account's markers go with
    -- it: Pleroma scopes every marker read and write to the user, enforced here
    -- by the cascade.
    user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- Which timeline this is the read position of. The API and
    -- `src/models/marker.rs` only ever write `home` or `notifications`; the
    -- check keeps anything else out of the table.
    timeline text NOT NULL,
    -- Id text of the newest entry the account has read, opaque to this table:
    -- a status `UUIDv7` for `home`, a notification id for `notifications`.
    -- NOT NULL; "nothing read yet" is the empty string, like Pleroma's default.
    last_read_id text NOT NULL,
    -- How many times the row has been written, the client's conflict counter
    -- (Pleroma's `lock_version`): 0 for a first marker, one more per update.
    version integer NOT NULL DEFAULT 0,
    updated_at timestamptz NOT NULL DEFAULT now(),
    -- One marker per account and timeline: the upsert's conflict target, and
    -- the account's own markers' lookup path.
    CONSTRAINT markers_user_id_timeline_key UNIQUE (user_id, timeline),
    CONSTRAINT markers_timeline_check CHECK (timeline IN ('home', 'notifications'))
);
