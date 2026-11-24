-- Store OAuth access and refresh tokens as SHA-256 digests (audit CORE-6).
-- Until now oauth_tokens.token and oauth_tokens.refresh_token held the
-- bearer credentials verbatim, so any read of the table (a backup, a
-- replica, an SQL console) yielded working credentials. The application
-- now looks a presented token up by lowercase-hex SHA-256 (the
-- sha256_hex helper in src/util/token.rs), and this rewrites the rows that
-- already exist the same way, so live sessions keep authenticating with
-- the raw value their client holds. encode(digest(..., 'sha256'), 'hex')
-- (pgcrypto, enabled in 20260101000000_extensions.sql) is byte-for-byte
-- that helper's output. The UNIQUE constraints carry over: equal digests
-- would need equal tokens.
UPDATE oauth_tokens
   SET token = encode(digest(token, 'sha256'), 'hex'),
       refresh_token = encode(digest(refresh_token, 'sha256'), 'hex');
