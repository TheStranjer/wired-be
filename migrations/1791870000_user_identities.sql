-- External OAuth identities (docs/ARCHITECTURE.md decision 9): which
-- provider account belongs to which local user, the link the `/oauth`
-- provider callback looks a login up by. Reference: Pleroma's Ueberauth
-- registrations (lib/pleroma/registration.ex plus
-- lib/pleroma/web/o_auth/o_auth_controller.ex), whose `(provider, ouid)`
-- pair this table keys the same way.
CREATE TABLE user_identities (
    -- UUIDv7 made in Rust (decision 4); no database-side default.
    id uuid PRIMARY KEY,
    -- The local account this provider identity logs in as. Deleting the
    -- user takes their identities with it.
    user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- Provider name, matching an OAUTH_PROVIDERS entry verbatim.
    provider text NOT NULL,
    -- The provider's own stable account id, read out of its userinfo
    -- document by the configured uid_field and kept as text (providers
    -- number accounts, keycloak uses UUIDs).
    uid text NOT NULL,
    inserted_at timestamptz NOT NULL DEFAULT now(),
    -- One local account per provider account: the callback's lookup key and
    -- the registration ticket's double-link guard.
    UNIQUE (provider, uid)
);
