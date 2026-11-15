-- MFA challenge tokens (WBE-132): the short-lived credentials a login of
-- an MFA-enabled account gets instead of a token, exchanged afterwards at
-- POST /oauth/mfa/challenge (API clients) or POST /oauth/mfa/verify (the
-- browser page). Reference: Pleroma's MFA token schema
-- (lib/pleroma/mfa/token.ex).
CREATE TABLE mfa_tokens (
    -- UUIDv7 made in Rust (decision 4); no database-side default.
    id uuid PRIMARY KEY,
    -- The mfa_token: a 43-character URL-safe random token made in
    -- src/util/token.rs; the unique lookup key of the challenge.
    token text NOT NULL UNIQUE,
    -- The account that must prove its second factor; deleting them
    -- deletes the challenge.
    user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- The pending authorization carrying the grant's parameters (the
    -- scopes of a password grant, the code of a browser authorization);
    -- NULL for a challenge that stands on its own. Deleting the
    -- authorization takes the challenge with it.
    authorization_id uuid REFERENCES oauth_authorizations(id) ON DELETE CASCADE,
    -- Expiry; five minutes, see src/models/mfa_token.rs.
    valid_until timestamptz NOT NULL,
    inserted_at timestamptz NOT NULL DEFAULT now()
);
