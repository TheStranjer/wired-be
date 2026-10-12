-- Membership order (WBE-104): `GET /api/v1/lists/:id/accounts` lists a list's
-- members in the order they were added and pages over exactly that order, so
-- `list_members` needs a monotonic ordinal next to its `(list_id, user_id)`
-- key. Reference: Mastodon's `list_members.id` bigserial, the column its
-- list-accounts pagination keys on; Pleroma keeps the members in the list
-- row's `following` array, whose order is the same fact.
ALTER TABLE list_members
    -- `bigserial` is the sequence plus `DEFAULT nextval(...)` plus `NOT NULL`:
    -- every inserted membership takes the next ordinal, and the rows already
    -- in the table are backfilled from the same sequence as the column is
    -- added. The sequence is owned by the column, so dropping the column (or
    -- the table) drops it too.
    ADD COLUMN position bigserial;

-- The paged read of one list's members: the primary key leads with `list_id`
-- but cannot serve the `ORDER BY position` the page needs, so this one covers
-- both the list filter and the cursor column.
CREATE INDEX list_members_list_id_position_idx ON list_members (list_id, position);
