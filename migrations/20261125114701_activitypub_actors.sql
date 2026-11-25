-- `activitypub_actors`: ActivityPub's own data about an account, one row
-- per account (local or remote) that exists on ActivityPub (WBE-840).
--
-- Part of the multi-protocol foundation: `users` keeps what every protocol
-- shares, and each protocol keeps its own data in `<protocol id>_...`
-- tables. This table takes the ActivityPub plumbing columns that `users`
-- carries today. `ap_id`, `domain`, `actor_type`, `also_known_as`,
-- `moved_to_ap_id` and `profile_url` stay on `users`: client APIs render
-- them on every account, and `ap_id` is the lookup key across the
-- ActivityPub code.
--
-- This migration only adds and backfills. Nothing writes or reads the table
-- yet; later tickets dual-write, move the readers, then drop the old
-- columns from `users`.
CREATE TABLE activitypub_actors (
    user_id uuid PRIMARY KEY REFERENCES users (id) ON DELETE CASCADE,
    -- Whether the account's ActivityPub identity is claimed (`active`) or
    -- awaiting setup (`pending`).
    status text NOT NULL DEFAULT 'active' CHECK (status IN ('pending', 'active')),
    -- Actor endpoints and collection ids. Remote ones come from the fetched
    -- actor document; local ones derive from `users.ap_id`.
    inbox text,
    shared_inbox text,
    outbox text,
    followers_url text,
    following_url text,
    featured_url text,
    -- Actor signing key pair; `private_key` is local accounts only.
    public_key text,
    private_key text,
    -- The `publicKey.id` the actor's document names (WBE-591): inbox
    -- signatures carry it as `keyId`. NULL for local accounts and for
    -- mirrors not re-fetched since it was recorded.
    public_key_id text,
    -- When this remote actor was last fetched.
    last_fetched_at timestamptz,
    inserted_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

-- Same index `users` has: a key id names one key, hence one actor, and the
-- partial index also serves the lookup by key id.
CREATE UNIQUE INDEX activitypub_actors_public_key_id_key ON activitypub_actors (public_key_id)
    WHERE public_key_id IS NOT NULL;

-- Backfill: every existing account, local and remote, is an ActivityPub
-- actor whose identity is already in use, so it starts `active` (the column
-- default).
INSERT INTO activitypub_actors (
    user_id, inbox, shared_inbox, outbox, followers_url, following_url,
    featured_url, public_key, private_key, public_key_id, last_fetched_at
)
SELECT
    id, inbox, shared_inbox, outbox, followers_url, following_url,
    featured_url, public_key, private_key, public_key_id, last_fetched_at
FROM users;
