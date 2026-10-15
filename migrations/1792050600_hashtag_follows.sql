-- Hashtag follows (WBE-116): which user follows which hashtag, the edge
-- behind `POST /api/v1/tags/{name}/follow`, `…/unfollow` and
-- `GET /api/v1/followed_tags`. Reference: Pleroma's `Pleroma.User.HashtagFollow`
-- (`lib/pleroma/user/hashtag_follow.ex`, the `user_follows_hashtag` table):
-- one row per (user, tag) pair, unique, which is the whole fact the endpoint
-- needs — following twice writes nothing (the API's idempotence) and removing
-- the user or the tag takes the follows with it.
CREATE TABLE hashtag_follows (
    -- The follower. Deleting the user's row takes their follows with it, like
    -- every other user-keyed edge table does.
    user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- The tag. A followed tag that no status ever used still has its
    -- `hashtags` row (`follow` creates one on demand), and deleting that row
    -- (nothing does yet) would take these follows with it.
    hashtag_id uuid NOT NULL REFERENCES hashtags(id) ON DELETE CASCADE,
    -- When the follow was created.
    inserted_at timestamptz NOT NULL DEFAULT now(),
    -- The insertion order of follows, keyed per user by the page below: the
    -- `UUIDv7` ids cannot state it (decision 4 orders rows by mint time, not
    -- by when one user followed this tag), so `GET /api/v1/followed_tags`
    -- pages over this ordinal exactly as `list_members.position` makes the
    -- list-members page (WBE-104).
    position bigserial,
    -- One follow per (user, tag) pair: the idempotence the follow endpoint
    -- relies on is this key refusing the second insert.
    PRIMARY KEY (user_id, hashtag_id)
);

-- The paged read of one user's follows: the primary key leads with `user_id`
-- but cannot serve the `ORDER BY position` the page needs, so this one covers
-- both the user filter and the cursor column.
CREATE INDEX hashtag_follows_user_id_position_idx ON hashtag_follows (user_id, position);
