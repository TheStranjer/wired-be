-- Status mentions (WBE-73): which accounts a status @-mentions, the edge
-- behind the Status entity's `mentions` array, the notification the mention
-- makes and (later) the mention timeline. Reference: Pleroma derives
-- mentions from the activity's `to` addressing
-- (`lib/pleroma/web/mastodon_api/views/status_view.ex`, `build_mentions`),
-- and `lib/pleroma/formatter.ex` resolves them on write; here the fact is a
-- row per (status, account) pair, so the entity renders from storage and the
-- reverse lookup is an index seek.
CREATE TABLE status_mentions (
    -- The mentioning status. Hard-deleting it (a soft delete keeps the row,
    -- and so its mentions) removes the links with it.
    status_id uuid NOT NULL REFERENCES statuses(id) ON DELETE CASCADE,
    -- The mentioned account, local or a stored remote mirror (decision 7).
    -- Deleting the account removes every mention of it.
    user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- Order the mention was linked in, 0 first: the `mentions` array keeps
    -- the order the accounts were resolved in, not id order, so this column
    -- records it next to the key (like `list_members.position` does for a
    -- list's members, but filled by the model instead of a sequence).
    position integer NOT NULL,
    -- One link per (status, account) pair: linking the same account twice
    -- within a status is what `link` folds and the entity renders once.
    PRIMARY KEY (status_id, user_id)
);

-- The mention timeline (`GET /api/v1/timelines/tag`'s cousin, one account's
-- mentioning statuses): newest status first for one mentioned user — the
-- key's leading column serves the other direction.
CREATE INDEX status_mentions_user_id_idx ON status_mentions (user_id, status_id DESC);
