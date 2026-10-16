-- Invite tokens (docs/ARCHITECTURE.md decision 3: one table per model):
-- the registration tokens `INVITES_ENABLED` gates `POST /api/v1/accounts`
-- with while registrations are closed. Reference: Pleroma's
-- `Pleroma.UserInviteToken` (`lib/pleroma/user_invite_token.ex`) and its
-- `user_invite_tokens` table; the type is derived from the limits the
-- invite was created with, exactly like Pleroma's `assign_type`.
CREATE TABLE invite_tokens (
    -- UUIDv7 made in Rust (decision 4); no database-side default.
    id uuid PRIMARY KEY,
    -- The random secret a registrant submits as `token` (16 bytes from
    -- src/util/token.rs); unique, so a lookup by submitted text finds at
    -- most one invite.
    token text NOT NULL UNIQUE,
    -- Lifecycle type (see src/models/invite_token.rs): which of the two
    -- limits below the invite carries.
    invite_type text NOT NULL
        CONSTRAINT invite_tokens_invite_type_check
        CHECK (invite_type IN ('one_time', 'reusable', 'date_limited', 'reusable_date_limited')),
    -- Last day the invite is valid; NULL for an invite that never expires
    -- (`date_limited` and `reusable_date_limited` carry one).
    expires_at date,
    -- Registrations this invite may serve in total; NULL for an invite
    -- without a use cap (`reusable` and `reusable_date_limited` carry
    -- one).
    max_use integer,
    -- Registrations served so far; raised by every consumption.
    uses integer NOT NULL DEFAULT 0,
    -- Whether the invite is spent: set when a `one_time` invite is
    -- consumed or when `max_use` is reached; `date_limited` invites stay
    -- unused until their date passes.
    used boolean NOT NULL DEFAULT false,
    inserted_at timestamptz NOT NULL DEFAULT now()
);
