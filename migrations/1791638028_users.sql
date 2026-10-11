-- Users (docs/ARCHITECTURE.md decision 7): one table serving both local
-- accounts and mirrors of remote actors. Reference: Pleroma's user schema
-- (lib/pleroma/user.ex) and its users migrations in priv/repo/migrations.
CREATE TABLE users (
    -- UUIDv7 made in Rust (decision 4); no database-side default.
    id uuid PRIMARY KEY,
    -- Handle: `alice` for local accounts, `alice@remote.example` for remote
    -- ones (decision 7). citext makes storage, uniqueness and lookups
    -- case-insensitive.
    nickname citext NOT NULL UNIQUE,
    -- ActivityPub actor id, e.g. https://remote.example/users/alice.
    ap_id text NOT NULL UNIQUE,
    -- True when this instance owns the account.
    local boolean NOT NULL,
    -- Host of a remote actor; NULL for local users.
    domain text,
    -- ActivityPub actor type (Person, Service, Group, ...).
    actor_type text NOT NULL DEFAULT 'Person',
    -- Account email, local users only; citext so lookups fold case.
    email citext UNIQUE,
    -- PHC password hash (src/util/password.rs); local users only.
    password_hash text,
    -- Display name; empty means "same as the nickname".
    name text NOT NULL DEFAULT '',
    -- Profile note: sanitized HTML (src/util/sanitize.rs) and its source.
    bio text NOT NULL DEFAULT '',
    raw_bio text NOT NULL DEFAULT '',
    -- Media URLs; NULL when unset.
    avatar_url text,
    banner_url text,
    -- Human-facing profile URL (the ActivityPub `url`).
    profile_url text,
    -- Follow requests need manual approval.
    locked boolean NOT NULL DEFAULT false,
    -- Flagged as an automated account.
    bot boolean NOT NULL DEFAULT false,
    -- Opt-in to the discovery endpoint.
    discoverable boolean NOT NULL DEFAULT false,
    -- Profile metadata: JSON array of {name, value} objects.
    fields jsonb NOT NULL DEFAULT '[]',
    -- ActivityPub endpoints and collection ids. Remote ones come from the
    -- fetched actor document; local ones derive from ap_id
    -- (helpers in src/models/user.rs).
    inbox text,
    shared_inbox text,
    outbox text,
    followers_url text,
    following_url text,
    featured_url text,
    -- Actor signing key pair; private_key is local users only and is
    -- redacted from Debug output in the model layer.
    public_key text,
    private_key text,
    -- Roles; enforced by later API tickets.
    is_admin boolean NOT NULL DEFAULT false,
    is_moderator boolean NOT NULL DEFAULT false,
    -- Soft flag for deactivated or suspended accounts.
    is_active boolean NOT NULL DEFAULT true,
    -- Denormalised counters, moved only by update_counts() and never below
    -- zero.
    followers_count integer NOT NULL DEFAULT 0,
    following_count integer NOT NULL DEFAULT 0,
    statuses_count integer NOT NULL DEFAULT 0,
    -- Insert time of the newest status this user posted.
    last_status_at timestamptz,
    -- When this remote actor was last fetched.
    last_fetched_at timestamptz,
    inserted_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

-- Federation queries: find a user's peers by origin, or all local ones.
CREATE INDEX users_domain_idx ON users (domain);
CREATE INDEX users_local_idx ON users (local);

-- Trigram search over handles and display names (pg_trgm from the
-- extensions migration). The opclass is registered for text, so the
-- citext nickname is indexed through an explicit cast — parenthesized,
-- because an expression index column needs parentheses before its opclass.
CREATE INDEX users_nickname_trgm_idx ON users USING gin ((nickname::text) gin_trgm_ops);
CREATE INDEX users_name_trgm_idx ON users USING gin (name gin_trgm_ops);
