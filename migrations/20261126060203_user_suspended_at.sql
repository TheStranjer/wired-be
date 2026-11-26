-- Tell a suspension from a plain disable (WBE-878).
--
-- `users.is_active` stays the one flag every reader consults, and
-- `local_deactivated` / `remote_suspended` stay the two origins behind it
-- (WBE-873). `suspended_at` records *which kind* of local deactivation it
-- was: set by a suspension (the Mastodon `suspend` action, the Pleroma admin
-- deactivation, the owner's own `disable_account`), cleared by the
-- activation that reverses it, and left NULL by the Mastodon `disable`
-- action. It drives the `suspended` of the admin and public account
-- entities, the `status=suspended` / `status=disabled` filters and the
-- `"suspended": true` of the federated actor.
ALTER TABLE users
    ADD COLUMN suspended_at timestamptz;

-- Every account deactivated here before this column existed was shown to
-- peers as suspended: keep it that way.
UPDATE users SET suspended_at = updated_at WHERE local_deactivated;
