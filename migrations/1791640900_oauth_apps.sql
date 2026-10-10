-- OAuth client applications (docs/ARCHITECTURE.md decision 9): the apps
-- registered through POST /api/v1/apps and used by the later /oauth token
-- endpoints. Reference: Pleroma's apps schema
-- (lib/pleroma/web/o_auth/app.ex).
CREATE TABLE oauth_apps (
    -- UUIDv7 made in Rust (decision 4); no database-side default.
    id uuid PRIMARY KEY,
    -- Human-facing application name, as clients send it.
    client_name text NOT NULL,
    -- Registered redirect URIs, space- or newline-separated exactly as
    -- normalized at registration (Pleroma joins list input with "\n" and
    -- stores single strings verbatim).
    redirect_uris text NOT NULL,
    -- Granted scopes, space-separated (src/oauth/scopes.rs); `read` is
    -- what a registration without `scopes` gets.
    scopes text NOT NULL DEFAULT 'read',
    -- Application homepage, if the client sent one.
    website text,
    -- OAuth credentials: 43-character URL-safe random tokens made in
    -- src/util/token.rs. client_id is the unique lookup key of the
    -- OAuth dance.
    client_id text NOT NULL UNIQUE,
    client_secret text NOT NULL,
    -- Whether the app skips the authorization confirmation screen;
    -- managed by admins, never by registration.
    trusted boolean NOT NULL DEFAULT false,
    -- Owner of the registration. A plain uuid for now: the foreign key
    -- to users arrives with the OAuth token ticket, and user-unbound app
    -- tokens keep this NULL.
    user_id uuid,
    inserted_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);
