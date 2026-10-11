-- Follow relationships: who follows whom, in which state. Reference:
-- Pleroma's FollowingRelationship schema
-- (lib/pleroma/following_relationship.ex) and the follow/unfollow side of
-- lib/pleroma/user.ex.
CREATE TABLE follows (
    -- UUIDv7 made in Rust (decision 4); no database-side default.
    id uuid PRIMARY KEY,
    -- The follower. Deleting the account deletes their follows.
    follower_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- The followed account. Deleting it removes every follow of it.
    followee_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- Lifecycle state (see src/models/follow.rs): a follow starts
    -- 'pending' and stays there until the followee accepts it.
    state text NOT NULL DEFAULT 'pending'
        CONSTRAINT follows_state_check
        CHECK (state IN ('pending', 'accepted', 'rejected')),
    -- ActivityPub id of the Follow activity behind this relationship
    -- (decision 6): local followers mint {BASE_URL}/activities/{uuid},
    -- remote ones send their own; NULL until an activity exists to record.
    ap_id text UNIQUE,
    -- Whether the followee keeps seeing the follower's reblogs (Mastodon's
    -- `reblogs` toggle of follow/unfollow).
    show_reblogs boolean NOT NULL DEFAULT true,
    -- Whether the followee gets a notification for every follower post
    -- (Mastodon's `notify` toggle).
    notify boolean NOT NULL DEFAULT false,
    inserted_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    -- One relationship per ordered pair: the key follow() is idempotent on.
    CONSTRAINT follows_follower_id_followee_id_key UNIQUE (follower_id, followee_id),
    -- Nobody follows themselves; the model refuses it first, this makes it
    -- impossible anyway.
    CONSTRAINT follows_not_self CHECK (follower_id <> followee_id)
);

-- A user's followers, by state (incoming follow requests, follower lists).
CREATE INDEX follows_followee_id_state_idx ON follows (followee_id, state);
-- Whom a user follows, by state (following lists, timeline delivery).
CREATE INDEX follows_follower_id_state_idx ON follows (follower_id, state);
