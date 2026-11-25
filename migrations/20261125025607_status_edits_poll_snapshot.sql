-- Per-version poll options for status edits (WBE-806).
-- Editing a poll's options replaces them, so a superseded version must keep
-- the options it asked. Mastodon's `StatusEdit` stores them as
-- `poll_options`. The array holds the option titles in order; it is NULL for
-- a version that carried no poll and for revisions recorded before this
-- migration.
ALTER TABLE status_edits
    ADD COLUMN poll_options text[];
