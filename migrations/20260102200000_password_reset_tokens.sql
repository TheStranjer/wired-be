-- Password reset (WBE-135): the tokens `POST /auth/password` mints and the
-- `/api/v1/pleroma/password_reset` endpoints consume, plus the `users` flag
-- that gates login until the account's password has been reset (Pleroma's
-- `users.password_reset_pending`, set by the admin force-reset flow).
-- Reference: Pleroma's `Pleroma.PasswordResetToken`
-- (`lib/pleroma/password_reset_token.ex`) and its `password_reset_tokens`
-- table; the validity window is `PASSWORD_RESET_TTL_SECS`, counted from
-- `inserted_at` (see `src/models/password_reset.rs`).
CREATE TABLE password_reset_tokens (
    -- UUIDv7 made in Rust (decision 4); no database-side default.
    id uuid PRIMARY KEY,
    -- The random secret carried in the emailed reset link
    -- (src/util/token.rs); unique, so a lookup by submitted text finds at
    -- most one token.
    token text NOT NULL UNIQUE,
    -- The account this token can reset; deleted with its user.
    user_id uuid NOT NULL REFERENCES users ON DELETE CASCADE,
    -- Whether the token has been consumed: a used token never resets again.
    used boolean NOT NULL DEFAULT false,
    -- Issue time; the model's expiry window counts from here.
    inserted_at timestamptz NOT NULL DEFAULT now()
);

-- The login gate of `check_account_ready` (see `src/api/oauth.rs`): an
-- account whose password an admin (or the reset flow) marked pending cannot
-- authenticate until it has been changed. Every ordinary account starts not
-- pending, hence the default.
ALTER TABLE users
    ADD COLUMN password_reset_pending boolean NOT NULL DEFAULT false;
