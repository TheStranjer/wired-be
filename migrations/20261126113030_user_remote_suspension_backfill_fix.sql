-- Undo the over-broad backfill of WBE-873 (WBE-882).
--
-- `20261126054512_user_suspension_origins.sql` set `local_deactivated` on
-- every inactive account, on the premise that each was deactivated here. A
-- remote mirror whose origin had suspended it (`is_active = NOT suspended`
-- since WBE-865) was inactive without any local action, so it was stranded:
-- `20261126060203_user_suspended_at.sql` then stamped `suspended_at` on it
-- too, and when the origin lifted the suspension `is_active` stayed false
-- (`NOT local_deactivated`) forever.
--
-- A remote mirror that is `local_deactivated` but has no local deactivation
-- in `moderation_log` was never deactivated here: hand it back to the remote
-- origin. It stays inactive (`is_active` is untouched) and is marked
-- `remote_suspended`, so the next actor Update or stale-mirror refetch
-- settles the flag from the actor document. Deleted mirrors and local
-- accounts are left alone.
UPDATE users
SET local_deactivated = false,
    suspended_at = NULL,
    remote_suspended = true
WHERE domain IS NOT NULL
  AND NOT local
  AND local_deactivated
  AND deleted_at IS NULL
  AND NOT EXISTS (
      SELECT 1
      FROM moderation_log
      WHERE moderation_log.action IN ('deactivate', 'suspend', 'disable')
        AND users.id = ANY (moderation_log.subject_ids)
  );
