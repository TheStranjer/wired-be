-- Blocks: one account refusing another, the fact behind
-- `POST /api/v1/accounts/{id}/block`, `…/unblock` and `GET /api/v1/blocks`,
-- the `blocking` / `blocked_by` flags of the Relationship entity, and (from
-- here on) the follow guard in src/models/follow.rs. Reference: Pleroma's
-- blocking edges in `Pleroma.UserRelationship` (`lib/pleroma/user_relationship.ex`)
-- behind `User.block` / `User.unblock` (`lib/pleroma/user.ex`) and the
-- `block`/`unblock`/`blocks` actions of the Mastodon `AccountController`.
CREATE TABLE blocks (
    -- UUIDv7 made in Rust (decision 4); no database-side default.
    id uuid PRIMARY KEY,
    -- The blocking account. Deleting it deletes the blocks it made.
    blocker_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- The blocked account. Deleting it removes every block of it.
    blocked_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- ActivityPub id of the Block activity behind this row (decision 6): a
    -- local blocker mints {BASE_URL}/activities/{uuid}, a remote one's id
    -- arrives with the federation ticket that processes the activity; NULL
    -- until an activity exists to record.
    ap_id text UNIQUE,
    inserted_at timestamptz NOT NULL DEFAULT now(),
    -- One block per ordered pair: the key block()/unblock() are idempotent
    -- on. A blocker is never its own blocked account — the model refuses a
    -- self-block before it reaches this table (the endpoint answers 422), so
    -- no CHECK is needed to make that true.
    CONSTRAINT blocks_blocker_id_blocked_id_key UNIQUE (blocker_id, blocked_id)
);

-- Who blocks a given account (the `blocked_by` flag, and the incoming side of
-- every block check) is the direction the unique key above cannot serve; the
-- outgoing side — a blocker's own list — is what its index already answers.
CREATE INDEX blocks_blocked_id_idx ON blocks (blocked_id);
