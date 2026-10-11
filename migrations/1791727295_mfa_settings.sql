-- Multi-factor authentication settings on `users` (WBE-131): the whole
-- per-account MFA state in one jsonb column, shaped like Pleroma's
-- `multi_factor_authentication_settings` embedded schema
-- (lib/pleroma/mfa/settings.ex): the `enabled` flag, the TOTP method
-- (`secret` plus the `confirmed` flag) and the `backup_codes`. Backup
-- codes are stored as SHA-256 hex hashes, never in plaintext; the TOTP
-- secret is stored as-is because confirming and verifying codes must
-- recover it. `src/models/user/mfa.rs` owns the shape, and the secret
-- never appears in `Debug` output or logs.
ALTER TABLE users
    ADD COLUMN mfa_settings jsonb NOT NULL DEFAULT
        '{"enabled": false, "totp": {"secret": null, "confirmed": false}, "backup_codes": []}';
