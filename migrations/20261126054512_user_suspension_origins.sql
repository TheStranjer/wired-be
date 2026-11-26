-- Track who deactivated an account (WBE-873).
--
-- `users.is_active` stays the one flag every reader consults: it is the
-- *effective* state, `NOT local_deactivated AND NOT remote_suspended`
-- (an account whose deletion started stays inactive as well). The two origins
-- are kept apart, the way Mastodon keeps `suspension_origin`, so neither can
-- undo the other:
--   * `local_deactivated`: an admin, MRF or the owner deactivated the
--     account here. Only the local activate/deactivate writes touch it.
--   * `remote_suspended`: the account's origin server marks the actor
--     `suspended` in its document. Only the remote actor upsert writes it.
ALTER TABLE users
    ADD COLUMN local_deactivated boolean NOT NULL DEFAULT false,
    ADD COLUMN remote_suspended boolean NOT NULL DEFAULT false;

-- Every account inactive today was deactivated here: keep it that way, so
-- a remote Update cannot reactivate it.
UPDATE users SET local_deactivated = true WHERE NOT is_active;
