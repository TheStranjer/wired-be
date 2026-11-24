-- Store password-reset and MFA challenge tokens as SHA-256 digests (WBE-708).
-- Neither value is shown again after it is minted, so the tables keep only
-- the lowercase-hex digest (src/util/token.rs `token_digest`); a leaked row
-- is no longer a working credential.
--
-- Existing rows hold raw secrets and cannot be converted. They are
-- short-lived (a reset token lives at most the configured TTL, an MFA
-- challenge five minutes), so they are dropped and simply expire.
DELETE FROM password_reset_tokens;
DELETE FROM mfa_tokens;

-- The unique constraint and its index follow the renamed column.
ALTER TABLE password_reset_tokens RENAME COLUMN token TO token_digest;
ALTER TABLE mfa_tokens RENAME COLUMN token TO token_digest;
