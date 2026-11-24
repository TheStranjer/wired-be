-- The `publicKey.id` a remote actor's document names for its signing key
-- (WBE-591, FED1-10).
--
-- Inbox signatures carry a `keyId`. Mastodon's is the actor URL plus
-- `#main-key`, which strips back to the actor; GoToSocial's is path-style
-- (`.../users/alice/main-key`) and strips to nothing stored, so every
-- delivery re-fetched the actor. Mirroring the key id lets the signer be
-- found by the id the signature names. NULL for local users and for
-- mirrors not re-fetched since this column appeared.
ALTER TABLE users ADD COLUMN public_key_id TEXT;

-- A key id names one key, hence one actor; the partial index also serves
-- the lookup by key id.
CREATE UNIQUE INDEX users_public_key_id_key ON users (public_key_id)
    WHERE public_key_id IS NOT NULL;
