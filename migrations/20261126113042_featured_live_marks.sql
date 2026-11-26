-- When a live featured-collection `Add`/`Remove` last changed a remote
-- account's pins or featured tags (WBE-881).
--
-- A peer may serve its `featured` and `featuredTags` collections from a cache
-- (Mastodon: three minutes), so a sync that runs shortly after a live
-- `Add`/`Remove` can read the set as it was before it and undo the change.
-- The sync skips a collection whose mark is younger than the grace window.
CREATE TABLE featured_live_marks (
    -- The remote account the `Add`/`Remove` came from.
    user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- Which collection: `pins` or `tags`.
    collection text NOT NULL CHECK (collection IN ('pins', 'tags')),
    -- When the last live change was applied.
    applied_at timestamptz NOT NULL,
    PRIMARY KEY (user_id, collection)
);
