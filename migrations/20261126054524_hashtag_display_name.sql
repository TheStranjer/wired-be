-- Hashtags keep the casing of their first use (WBE-879), as Mastodon's
-- `Tag#display_name` does: the full Tag entity renders it as `name`. The
-- `name` column stays the lowercase unique key (matching, urls, status
-- `tags[]`). NULL, the state of every row written before this column, means
-- "show the lowercase name"; the next use of such a tag fills it in.
ALTER TABLE hashtags ADD COLUMN display_name text;
