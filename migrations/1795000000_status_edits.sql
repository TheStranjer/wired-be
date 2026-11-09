-- Status edits: one row per superseded version of a status (WBE-89).
-- Reference: Pleroma keeps an edit's history as the `state` list on the
-- object (`Pleroma.Object.Updater.make_update_object_data`,
-- lib/pleroma/object/updater.ex), the newest version last; this table is
-- that history laid out relationally (`docs/ARCHITECTURE.md` decision 6):
-- one row per revision, the CURRENT version stored nowhere here — the
-- `statuses` row is the current version, and the history endpoint renders
-- it as the array's last element.
CREATE TABLE status_edits (
    -- UUIDv7 made in Rust (decision 4); no database-side default.
    id uuid PRIMARY KEY,
    -- The status this revision belongs to; the history goes with the post.
    status_id uuid NOT NULL REFERENCES statuses(id) ON DELETE CASCADE,
    -- The superseded post as sanitized HTML.
    content text NOT NULL,
    -- The superseded post as its source text.
    raw_text text NOT NULL,
    -- The content warning that version carried.
    spoiler_text text NOT NULL DEFAULT '',
    -- Whether that version folded behind its warning.
    sensitive boolean NOT NULL DEFAULT false,
    -- That version's media attachment ids, in order.
    media_ids uuid[] NOT NULL DEFAULT '{}',
    -- When THAT REVISION was authored — the status's creation time for the
    -- first version, the previous edit's time for every later one.
    created_at timestamptz NOT NULL,
    -- The author at the time of the snapshot (denormalised from the status;
    -- a revision names who wrote it even past an account purge).
    account_id uuid NOT NULL
);

-- One status's history in authoring order — the read `GET /api/v1/statuses/
-- {id}/history` walks (oldest first).
CREATE INDEX status_edits_status_id_idx ON status_edits (status_id, created_at);
