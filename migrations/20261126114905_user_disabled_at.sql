-- Keep disable apart from suspension (WBE-883).
--
-- Mastodon's `users.disabled` only blocks sign-in and API use by the owner:
-- the account, its posts and its federation are untouched. `disabled_at` is
-- that flag here. It is written by the admin `disable` action only, reversed
-- by `enable` only, and moves none of `is_active`, `local_deactivated` and
-- `suspended_at`, so a suspend/unsuspend neither sets nor clears it.
ALTER TABLE users
    ADD COLUMN disabled_at timestamptz;

-- A local account whose only local deactivation is a `disable` in the
-- moderation log (no suspension stamped, no later `activate`) moves to the
-- new flag and is reactivated: it was deactivated by WBE-878's disable,
-- which hid the account and its objects from everyone.
UPDATE users
SET disabled_at = disabled.stamped_at,
    local_deactivated = false,
    is_active = NOT users.remote_suspended
FROM (
    SELECT u.id, max(l.inserted_at) AS stamped_at
    FROM users u
    JOIN moderation_log l
      ON l.action = 'disable' AND u.id = ANY (l.subject_ids)
    GROUP BY u.id
) AS disabled
WHERE users.id = disabled.id
  AND users.local
  AND users.local_deactivated
  AND users.suspended_at IS NULL
  AND users.deleted_at IS NULL
  AND NOT EXISTS (
      SELECT 1
      FROM moderation_log later
      WHERE later.action = 'activate'
        AND users.id = ANY (later.subject_ids)
        AND later.inserted_at > disabled.stamped_at
  );
