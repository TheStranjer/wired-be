-- Repairs remote poll posts that the status-expiry sweep tombstoned when their
-- poll closed (WBE-821). The parser used to copy a remote Question's `endTime`
-- into the status's `expires_at`, so the sweep treated every remote poll post
-- as an expiring status and deleted it the moment voting ended.
--
-- Victims are remote statuses whose `expires_at` equals their poll's
-- `expires_at`. Those still live only need the bogus expiry cleared. Those
-- already tombstoned are restored when the sweep, not a received `Delete`,
-- tombstoned them: the sweep runs every five minutes, so its tombstone lands
-- within ten minutes after `expires_at`. A `Delete` leaves no per-object
-- record, so a post deleted by its origin inside that window is
-- indistinguishable and comes back too.
--
-- What the tombstone destroyed and this cannot bring back: the text
-- (`content`, `raw_text` and `spoiler_text` were cleared), the notifications
-- and conversation participations about the post, its pins, and the reblog
-- rows tombstoned with it. The poll, its options and its final results were
-- never touched. The counters the tombstone stepped down are stepped back up.
CREATE TEMPORARY TABLE wbe821_victims ON COMMIT DROP AS
SELECT s.id,
       s.user_id,
       s.in_reply_to_id,
       s.quote_of_id,
       s.reblog_of_id,
       s.deleted_at IS NOT NULL AS tombstoned
  FROM statuses s
  JOIN polls p ON p.status_id = s.id
 WHERE NOT s.local
   AND s.expires_at IS NOT NULL
   AND s.expires_at = p.expires_at
   AND (
        s.deleted_at IS NULL
        OR (s.deleted_at >= s.expires_at
            AND s.deleted_at <= s.expires_at + interval '10 minutes')
       );

-- The author's post count, one per restored status.
UPDATE users u
   SET statuses_count = u.statuses_count + c.restored,
       updated_at = now()
  FROM (SELECT user_id, count(*)::integer AS restored
          FROM wbe821_victims
         WHERE tombstoned
         GROUP BY user_id) c
 WHERE u.id = c.user_id;

-- The counters the restored posts feed on other statuses.
UPDATE statuses t
   SET replies_count = t.replies_count + c.restored,
       updated_at = now()
  FROM (SELECT in_reply_to_id AS id, count(*)::integer AS restored
          FROM wbe821_victims
         WHERE tombstoned AND in_reply_to_id IS NOT NULL
         GROUP BY in_reply_to_id) c
 WHERE t.id = c.id;

UPDATE statuses t
   SET quotes_count = t.quotes_count + c.restored,
       updated_at = now()
  FROM (SELECT quote_of_id AS id, count(*)::integer AS restored
          FROM wbe821_victims
         WHERE tombstoned AND quote_of_id IS NOT NULL
         GROUP BY quote_of_id) c
 WHERE t.id = c.id;

UPDATE statuses t
   SET reblogs_count = t.reblogs_count + c.restored,
       updated_at = now()
  FROM (SELECT reblog_of_id AS id, count(*)::integer AS restored
          FROM wbe821_victims
         WHERE tombstoned AND reblog_of_id IS NOT NULL
         GROUP BY reblog_of_id) c
 WHERE t.id = c.id;

-- Un-tombstone and drop the bogus expiry.
UPDATE statuses s
   SET expires_at = NULL,
       deleted_at = NULL,
       updated_at = now()
  FROM wbe821_victims v
 WHERE s.id = v.id;
