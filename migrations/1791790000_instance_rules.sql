-- Instance rules (WBE-118): the ordered list of instance rules served as
-- the `rules` block of `GET /api/v1/instance`. Reference: Pleroma's
-- `Pleroma.Rule` (`lib/pleroma/rule.ex`) and its `rules` migration — same
-- columns, with Pleroma's integer id replaced by the crate-wide UUIDv7 key
-- (docs/ARCHITECTURE.md decision 4). `src/models/instance_rule.rs` owns the
-- queries; the admin tickets reuse this table to edit the list.
CREATE TABLE instance_rules (
    -- UUIDv7 made in Rust (decision 4); no database-side default. Its text
    -- sorts in creation order, which breaks `priority` ties stably.
    id uuid PRIMARY KEY,
    -- The rule as clients display it.
    text text NOT NULL,
    -- Guidance shown beside the rule; empty when the instance gave none.
    hint text NOT NULL DEFAULT '',
    -- Sort position: lower shows first.
    priority integer NOT NULL DEFAULT 0,
    inserted_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);
