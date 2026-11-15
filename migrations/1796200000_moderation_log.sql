-- Moderation log (WBE-194): the record of every staff action the admin API
-- takes, as one row per action with the human-readable sentence Pleroma's
-- `Pleroma.ModerationLog` (`lib/pleroma/moderation_log.ex`) renders for its
-- admin frontend. Pleroma keeps the whole entry in one `data` jsonb map;
-- `docs/ARCHITECTURE.md` decisions 2 and 4 make this one relational row
-- whose listing filters (actor, message search, time window) are plain
-- columns, with the action-specific parts (permissions, tags, report and
-- status subject maps, relay targets) staying in the `data` map. The stored
-- `data` keeps Pleroma's shape — the `actor`, `action` and `message` keys
-- the API echoes ride in it beside those extras — so the entry renders the
-- way Pleroma's `ModerationLogView` renders it (`"show.json"` is exactly
-- `{id, data, time, message}`).
CREATE TABLE moderation_log (
    -- UUIDv7 made in Rust (decision 4); no database-side default.
    id uuid PRIMARY KEY,
    -- The staff account that acted. `ON DELETE SET NULL` (the ticket's
    -- column): deleting an admin keeps the record of what it did, without
    -- a live account to point at. The nickname column is why the sentence
    -- still reads after the actor is gone.
    actor_id uuid REFERENCES users(id) ON DELETE SET NULL,
    -- The actor's nickname as it was when the action was recorded, kept
    -- verbatim so a later rename or deletion cannot rewrite history.
    actor_nickname text NOT NULL,
    -- The affected accounts, in the order the action named them; empty for
    -- actions whose subject is not a user (a report, a status, a relay).
    subject_ids uuid[] NOT NULL DEFAULT '{}',
    -- The subjects' nicknames, as recorded, positionally parallel to
    -- `subject_ids`.
    subject_nicknames text[] NOT NULL DEFAULT '{}',
    -- The machine-readable action name (`deactivate`, `grant`,
    -- `report_update`, ...): the closed set `src/models/moderation_log.rs`
    -- writes, one per helper. No `CHECK` here: the admin tickets that land
    -- later add actions, and a constraint per release would be a migration
    -- per word.
    action text NOT NULL,
    -- The full sentence (`"@admin deactivated users: @alice, @bob"`),
    -- built by the model's per-action helpers and stored, so listing reads
    -- never re-derive it. It is also the search column.
    message text NOT NULL,
    -- The action-specific payload in Pleroma's shape: `actor`, `action`
    -- and `message` plus the subject maps and extras (see
    -- `src/models/moderation_log.rs::record`).
    data jsonb NOT NULL DEFAULT '{}',
    inserted_at timestamptz NOT NULL DEFAULT now()
);

-- The log listing is always newest-first (the `page`/`page_size` window of
-- `GET /api/v1/pleroma/admin/moderation_log`), and its time filters compare
-- this column.
CREATE INDEX moderation_log_inserted_at_idx ON moderation_log (inserted_at DESC);
-- The `user_id` filter: one actor's actions.
CREATE INDEX moderation_log_actor_id_idx ON moderation_log (actor_id);
