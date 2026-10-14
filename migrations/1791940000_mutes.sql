-- Mutes: one account hiding another from its timelines, the fact behind
-- `POST /api/v1/accounts/{id}/mute`, `…/unmute` and `GET /api/v1/mutes`, and
-- the `muting` / `muting_notifications` flags of the Relationship entity.
-- Reference: Pleroma's mute edges in `Pleroma.UserRelationship`
-- (`lib/pleroma/user_relationship.ex`) behind `User.mute` / `User.unmute`
-- (`lib/pleroma/user.ex`) and the `mute`/`unmute`/`mutes` actions of the
-- Mastodon `AccountController`.
CREATE TABLE mutes (
    -- UUIDv7 made in Rust (decision 4); no database-side default.
    id uuid PRIMARY KEY,
    -- The muting account. Deleting it deletes the mutes it made.
    muter_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- The muted account. Deleting it removes every mute of it.
    muted_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- Mastodon's `notifications` parameter of the mute: true (the default)
    -- also hides the muted account's notifications, false mutes only the
    -- timeline. The `muting_notifications` flag of the Relationship entity
    -- reads this column.
    hide_notifications boolean NOT NULL DEFAULT true,
    -- When the mute stops applying (Mastodon's `duration`, in seconds from
    -- the write); NULL is an indefinite mute. A row at or past this instant
    -- is treated as absent by every read, and the `mutes.expire` periodic
    -- job deletes it.
    expires_at timestamptz,
    inserted_at timestamptz NOT NULL DEFAULT now(),
    -- One mute per ordered pair: the key mute()/unmute() are idempotent on.
    -- A muter is never its own muted account — the model refuses a self-mute
    -- before it reaches this table (the endpoint answers 422), so no CHECK is
    -- needed to make that true.
    CONSTRAINT mutes_muter_id_muted_id_key UNIQUE (muter_id, muted_id)
);

-- Who muted a given account (the `muted_by` question the timeline and inbox
-- filters ask, and the incoming side of every mute check) is the direction
-- the unique key above cannot serve; the outgoing side — a muter's own list —
-- is what its index already answers.
CREATE INDEX mutes_muted_id_idx ON mutes (muted_id);
