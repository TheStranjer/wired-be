-- Dismissal becomes a timestamp, Mastodon's `dismissed_at`: the boolean
-- `dismissed` flag of the notifications table carries the same fact less
-- precisely, and the dismissal endpoints (`src/api/notifications.rs`) record
-- *when* a notification went away. The rows still hide rather than die —
-- every read filters `dismissed_at IS NULL` exactly where it filtered
-- `NOT dismissed` — and a dismissal already stored keeps its moment as close
-- as the old flag allowed: the creation time.
ALTER TABLE notifications
    ADD COLUMN dismissed_at timestamptz;

UPDATE notifications
   SET dismissed_at = inserted_at
 WHERE dismissed;

ALTER TABLE notifications
    DROP COLUMN dismissed;
