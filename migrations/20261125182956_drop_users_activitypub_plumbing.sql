-- Drop the ActivityPub plumbing columns from `users` (WBE-858).
--
-- The last step of the users split (see `activitypub_actors`): `users` keeps
-- what every protocol shares, and ActivityPub keeps its endpoints, keys and
-- fetch stamp in `activitypub_actors`, which every reader and writer already
-- uses. `ap_id`, `domain`, `actor_type`, `also_known_as`, `moved_to_ap_id`
-- and `profile_url` stay on `users`.

-- Safety net: an account that somehow has no actor row yet is copied over
-- before its columns go, so no endpoint or key is lost. The rows the dual
-- write kept in step already exist and are left alone.
INSERT INTO activitypub_actors (
    user_id, inbox, shared_inbox, outbox, followers_url, following_url,
    featured_url, public_key, private_key, public_key_id, last_fetched_at
)
SELECT
    id, inbox, shared_inbox, outbox, followers_url, following_url,
    featured_url, public_key, private_key, public_key_id, last_fetched_at
FROM users
ON CONFLICT DO NOTHING;

-- The key-id index lives on the actor row now.
DROP INDEX users_public_key_id_key;

ALTER TABLE users
    DROP COLUMN inbox,
    DROP COLUMN shared_inbox,
    DROP COLUMN outbox,
    DROP COLUMN followers_url,
    DROP COLUMN following_url,
    DROP COLUMN featured_url,
    DROP COLUMN public_key,
    DROP COLUMN public_key_id,
    DROP COLUMN private_key,
    DROP COLUMN last_fetched_at;
