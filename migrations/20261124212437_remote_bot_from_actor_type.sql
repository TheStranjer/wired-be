-- Remote actors of type `Service` or `Application` are bots (Mastodon's
-- `Account#bot?`). The upsert now derives `bot` from the actor type on every
-- fetch; this backfills the mirrors stored before it did.
UPDATE users
SET bot = true
WHERE NOT local
  AND actor_type IN ('Service', 'Application')
  AND NOT bot;
