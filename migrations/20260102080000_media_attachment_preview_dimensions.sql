-- Preview (thumbnail) dimensions for media attachments (media-processing
-- ticket): `width`/`height` describe the original file; the generated
-- `<stem>_small` preview needs its own size for the Mastodon entity's
-- `meta.small`, because entities render from the row alone (`width`/
-- `height` were added the same way for the original) and nothing
-- re-decodes the stored preview at render time.
ALTER TABLE media_attachments
    ADD COLUMN preview_width integer,
    ADD COLUMN preview_height integer;
