-- Bind an authorization code to the redirect URI it was issued for (WBE-481,
-- RFC 6749 section 4.1.3). NULL for rows written before this column existed
-- and for codes that are never delivered to a client (the password grant's
-- parked MFA authorization), which the exchange accepts without a URI.
ALTER TABLE oauth_authorizations ADD COLUMN redirect_uri text;
