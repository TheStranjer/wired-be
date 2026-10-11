-- OAuth access tokens (docs/ARCHITECTURE.md decision 9): the bearer
-- credentials issued by the /oauth token endpoints and checked by the
-- AuthUser extractors. Reference: Pleroma's token schema
-- (lib/pleroma/web/o_auth/token.ex).
CREATE TABLE oauth_tokens (
    -- UUIDv7 made in Rust (decision 4); no database-side default.
    id uuid PRIMARY KEY,
    -- The access token: a 43-character URL-safe random token made in
    -- src/util/token.rs; the unique lookup key of the bearer header.
    token text NOT NULL UNIQUE,
    -- The refresh token, same shape; lets a client mint a fresh token
    -- pair without re-authorizing.
    refresh_token text NOT NULL UNIQUE,
    -- The user this token acts as. NULL means an app-only token issued
    -- through the client_credentials grant (Pleroma's
    -- Token.create(%{user: nil})). Deleting the user revokes their tokens.
    user_id uuid REFERENCES users(id) ON DELETE CASCADE,
    -- The application the token was issued for; deleting the app revokes
    -- its tokens.
    app_id uuid NOT NULL REFERENCES oauth_apps(id) ON DELETE CASCADE,
    -- Granted scopes, space-separated (src/oauth/scopes.rs).
    scopes text NOT NULL,
    -- Expiry; src/models/oauth_token.rs stamps it from OAUTH_TOKEN_TTL_SECS
    -- (default 30 days) and every validity check compares it against now().
    valid_until timestamptz NOT NULL,
    inserted_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

-- Token listings per user: revoke-all screens and the admin audit trail
-- both ask "which tokens does this user hold?". App-only tokens have a
-- NULL user_id and Postgres' B-tree indexes NULLs, so they stay reachable
-- through this index too.
CREATE INDEX oauth_tokens_user_id_idx ON oauth_tokens (user_id);
