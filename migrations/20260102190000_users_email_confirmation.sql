-- Email confirmation (`ACCOUNT_ACTIVATION_REQUIRED`, the registration-modes
-- ticket): the secret token a confirmation link carries, and the moment the
-- confirmation email bearing it was queued. `users.email_confirmed` and
-- `users.approval_pending` already exist (the Pleroma-extension migration)
-- and `users.registration_reason` came with the invite-token ticket, so this
-- adds only the two columns the confirmation flow still needs.
--
-- `confirmation_token` is NULL for every account that never went through the
-- confirmation flow: the column default is NULL, the link handler clears it
-- when it confirms the address, and `POST
-- /api/v1/pleroma/accounts/confirmation_resend` mints a fresh one.
-- Reference: Pleroma's `users.confirmation_token`
-- (`lib/pleroma/user.ex`, `confirmation_changeset`).
ALTER TABLE users
    ADD COLUMN confirmation_token text,
    ADD COLUMN confirmation_sent_at timestamptz;
