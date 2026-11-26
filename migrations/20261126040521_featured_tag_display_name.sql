-- Featured hashtags keep the casing their owner wrote (WBE-872), as Mastodon's
-- `FeaturedTag#display_name` does. The shared `hashtags` row stays lowercase
-- (it is the case-insensitive key for uniqueness and counting); this column
-- holds the spelling to show. NULL, the state of every row written before
-- this column, means "show the tag's own lowercase name".
ALTER TABLE featured_tags ADD COLUMN display_name text;
