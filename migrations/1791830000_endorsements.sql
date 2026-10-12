-- Endorsements: an account pinning another account's profile to its own
-- (Mastodon's "featured profiles"). Reference: Pleroma's endorsement
-- edges in `Pleroma.UserRelationship` (`lib/pleroma/user_relationships.ex`)
-- behind the `endorse`/`unendorse` actions of the Mastodon
-- `AccountController`; the ticket keys the pair (user_id, target_id).
CREATE TABLE endorsements (
    -- UUIDv7 made in Rust (decision 4); no database-side default.
    id uuid PRIMARY KEY,
    -- The endorsing account. Deleting it deletes the endorsements it made.
    user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- The endorsed account. Deleting it removes every endorsement of it.
    target_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    inserted_at timestamptz NOT NULL DEFAULT now(),
    -- One endorsement per ordered pair: the key endorse/unendorse are
    -- idempotent on. A self-endorsement is impossible through the API (it
    -- requires a follow, and follows forbid themselves), so — unlike
    -- follows — no self-check is needed here.
    CONSTRAINT endorsements_user_id_target_id_key UNIQUE (user_id, target_id)
);

-- Every read here starts from the endorsing user (own listing, the
-- relationship loader's batch probe on target ids, the unfollow cleanup),
-- which the unique constraint's index already serves; the target side is
-- only ever reached from a deleted user row's cascade.
