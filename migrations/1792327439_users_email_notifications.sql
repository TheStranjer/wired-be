-- Email notifications (WBE-220): whether the account still wants email from
-- the instance. Reference: Pleroma's `User.switch_email_notifications`
-- (`lib/pleroma/user.ex`), which flips this same flag on the account's
-- request; the HTML unsubscribe page (a later ticket) is the writer that
-- clears it, and the mailer is the reader that skips opted-out accounts.
ALTER TABLE users ADD COLUMN email_notifications boolean NOT NULL DEFAULT true;
