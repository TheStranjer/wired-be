-- Hashtags (WBE-72): the tags a status carries, the edge behind the Status
-- entity's `tags` array and (later) the tag timelines and tag endpoints.
-- Reference: Pleroma keeps the same shape — a `hashtags` row per normalized
-- name and a join table per object (`lib/pleroma/hashtag.ex`, the
-- `hashtags_objects` join) — normalizing names by lowercasing and trimming.
-- Here the name is stored lowercase and without the leading `#`, so the
-- unique key is a plain one, the entity renders straight from storage, and
-- the reverse lookup — the newest statuses under one tag — is an index seek
-- on `(hashtag_id, status_id DESC)`.
CREATE TABLE hashtags (
    -- UUIDv7 made in Rust (decision 4); no database-side default.
    id uuid PRIMARY KEY,
    -- The tag as written in post text, minus the `#` and lowercased
    -- (src/models/hashtag.rs normalizes before storing). Unique: two posts
    -- using `#Foo` share this one row.
    name text NOT NULL UNIQUE,
    -- When the tag first appeared in a status (or was looked up by name).
    inserted_at timestamptz NOT NULL DEFAULT now()
);

-- Which status carries which tag.
CREATE TABLE status_hashtags (
    -- The tagging status. Hard-deleting it (a soft delete keeps the row,
    -- and so its tags) removes the links with it.
    status_id uuid NOT NULL REFERENCES statuses(id) ON DELETE CASCADE,
    -- The tag. Deleting a tag row (nothing does yet) takes the links of
    -- every status that used it.
    hashtag_id uuid NOT NULL REFERENCES hashtags(id) ON DELETE CASCADE,
    -- One link per (status, tag) pair: a tag used twice in one status is
    -- stored, and rendered, once. No position column — the `tags` array
    -- orders by name, not by first appearance, so the key order is all the
    -- table needs to keep.
    PRIMARY KEY (status_id, hashtag_id)
);

-- The tag timeline (`GET /api/v1/timelines/tag/{hashtag}`): the newest
-- statuses under one tag, newest first off the second key column; the
-- status's own tags read off the primary key's leading column.
CREATE INDEX status_hashtags_hashtag_id_idx ON status_hashtags (hashtag_id, status_id DESC);
