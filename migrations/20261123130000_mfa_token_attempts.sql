-- Count the wrong second-factor codes presented against a challenge
-- (WBE-668): a challenge that has taken the cap of failures is unusable, so
-- the five-minute window cannot be used to brute-force a TOTP code.
ALTER TABLE mfa_tokens ADD COLUMN failed_attempts integer NOT NULL DEFAULT 0;
