-- Per-version media metadata for status edits (WBE-800).
-- Mastodon snapshots each attachment's alt text per `StatusEdit`; without it
-- the history renders every past version with the attachments' CURRENT alt
-- text. The arrays are aligned element-for-element with `media_ids`; an
-- element is NULL where that attachment had no description or focal point.
-- The columns themselves are NULL on rows recorded before this migration,
-- which the history renders from the attachments' current metadata.
ALTER TABLE status_edits
    ADD COLUMN media_descriptions text[],
    ADD COLUMN media_focus_x real[],
    ADD COLUMN media_focus_y real[];
