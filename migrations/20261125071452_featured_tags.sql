-- Featured hashtags (WBE-827): the tags an account shows on its profile.
-- Reference: Mastodon's `featured_tags` table (`FeaturedTag`), federated as
-- the actor's `featuredTags` collection. A row belongs to any account, local
-- or a mirrored remote one: a local user features tags through the API, and
-- a remote actor's collection is stored here on every actor refresh.
CREATE TABLE featured_tags (
    -- UUIDv7 made in Rust (decision 4); no database-side default.
    id uuid PRIMARY KEY,
    -- The account that features the tag. Deleting it deletes its rows.
    user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- The featured tag. Deleting the tag row deletes the feature with it.
    tag_id uuid NOT NULL REFERENCES hashtags(id) ON DELETE CASCADE,
    -- How many of the account's public and unlisted statuses use the tag,
    -- kept current whenever one of them is written, edited or deleted.
    statuses_count bigint NOT NULL DEFAULT 0,
    -- When the account last posted under the tag (public or unlisted),
    -- NULL while it never has.
    last_status_at timestamptz,
    inserted_at timestamptz NOT NULL DEFAULT now(),
    -- One feature per (account, tag): featuring twice is one row.
    CONSTRAINT featured_tags_user_id_tag_id_key UNIQUE (user_id, tag_id)
);

-- Deleting a hashtag row cascades here by `tag_id`.
CREATE INDEX featured_tags_tag_id_idx ON featured_tags (tag_id);
