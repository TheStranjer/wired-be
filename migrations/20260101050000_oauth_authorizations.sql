-- OAuth authorization codes (docs/ARCHITECTURE.md decision 9): the
-- one-time codes the /oauth/authorize redirect hands the client, which it
-- then exchanges for a token pair. Reference: Pleroma's authorization
-- schema (lib/pleroma/web/o_auth/authorization.ex).
CREATE TABLE oauth_authorizations (
    -- UUIDv7 made in Rust (decision 4); no database-side default.
    id uuid PRIMARY KEY,
    -- The authorization code: a 43-character URL-safe random token made
    -- in src/util/token.rs; the unique lookup key of the exchange.
    token text NOT NULL UNIQUE,
    -- The user who granted the authorization; deleting them revokes it.
    user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- The application the code was issued for; the exchange must present
    -- the same app. Deleting the app revokes pending codes.
    app_id uuid NOT NULL REFERENCES oauth_apps(id) ON DELETE CASCADE,
    -- Scopes the user approved, space-separated (src/oauth/scopes.rs);
    -- copied verbatim onto the token the exchange creates.
    scopes text NOT NULL,
    -- Expiry; short by design (10 minutes, see
    -- src/models/oauth_token.rs).
    valid_until timestamptz NOT NULL,
    -- Whether the code has been exchanged. The exchange flips it in the
    -- same statement that reads the row, so a code is used at most once.
    used boolean NOT NULL DEFAULT false,
    inserted_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);
