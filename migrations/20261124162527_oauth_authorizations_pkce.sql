-- PKCE (RFC 7636): the challenge a code is bound to at authorize time and
-- the method that derived it (only `S256` is accepted). Both stay NULL for
-- a code issued without PKCE.
ALTER TABLE oauth_authorizations
    ADD COLUMN code_challenge text,
    ADD COLUMN code_challenge_method text;
